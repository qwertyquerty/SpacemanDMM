//! DreamMaker language server.
//!
//! Based on:
//!
//! * https://langserver.org/
//! * https://microsoft.github.io/language-server-protocol/specification
//! * https://github.com/rust-lang-nursery/rls
#![deny(unsafe_code)]
#![cfg_attr(not(target_arch="wasm32"), forbid(unsafe_code))]

extern crate url;
extern crate serde;
extern crate serde_json;
#[macro_use] extern crate serde_derive;
extern crate petgraph;
extern crate interval_tree;
extern crate lsp_types as langserver;
extern crate jsonrpc_core as jsonrpc;
extern crate dreammaker as dm;
extern crate dreamchecker;

#[macro_use] mod macros;
mod io;
mod document;
mod symbol_search;
mod find_references;
mod extras;
mod completion;

use std::path::PathBuf;
use std::collections::{HashMap, HashSet, VecDeque};
use std::collections::hash_map::Entry;
use std::rc::Rc;

use url::Url;
use jsonrpc::{Request, Call, Response, Output};
use langserver::MessageType;
use petgraph::visit::IntoNodeReferences;

use dm::FileId;
use dm::annotation::{Annotation, AnnotationTree};
use dm::objtree::TypeRef;

fn main() {
    std::env::set_var("RUST_BACKTRACE", "1");

    eprintln!(
        "dm-langserver {}  Copyright (C) 2017-2019  Tad Hardesty",
        env!("CARGO_PKG_VERSION")
    );
    eprintln!("This program comes with ABSOLUTELY NO WARRANTY. This is free software,");
    eprintln!("and you are welcome to redistribute it under the conditions of the GNU");
    eprintln!("General Public License version 3.");
    eprintln!();
    match std::env::current_exe() {
        Ok(path) => eprintln!("executable: {}", path.display()),
        Err(e) => eprintln!("exe check failure: {}", e),
    }
    eprint!("{}", include_str!(concat!(env!("OUT_DIR"), "/build-info.txt")));
    match std::env::current_dir() {
        Ok(path) => eprintln!("directory: {}", path.display()),
        Err(e) => eprintln!("dir check failure: {}", e),
    }

    io::io_main();
}

const VERSION: Option<jsonrpc::Version> = Some(jsonrpc::Version::V2);

#[derive(PartialEq)]
enum InitStatus {
    Starting,
    Running,
    ShuttingDown,
}

type Span = interval_tree::RangeInclusive<dm::Location>;

#[derive(Default, Debug)]
struct ClientCaps {
    related_info: bool,
    label_offset_support: bool,
    object_tree: bool,
}

impl ClientCaps {
    fn parse(caps: &langserver::ClientCapabilities) -> ClientCaps {
        let mut this = ClientCaps::default();
        if let Some(ref text_document) = caps.text_document {
            if let Some(ref signature_help) = text_document.signature_help {
                if let Some(ref signature_information) = signature_help.signature_information {
                    if let Some(ref parameter_information) = signature_information.parameter_information {
                        if let Some(label_offset_support) = parameter_information.label_offset_support {
                            this.label_offset_support = label_offset_support;
                        }
                    }
                }
            }

            if let Some(ref publish_diagnostics) = text_document.publish_diagnostics {
                if let Some(related_info) = publish_diagnostics.related_information {
                    this.related_info = related_info;
                }
            }
        }
        if let Some(ref experimental) = caps.experimental {
            if let Some(ref dreammaker) = experimental.get("dreammaker") {
                if let Some(ref object_tree) = dreammaker.get("objectTree") {
                    if let Some(value) = object_tree.as_bool() {
                        this.object_tree = value;
                    }
                }
            }
        }
        this
    }
}

struct Engine<'a, W: 'a> {
    write: &'a W,
    docs: document::DocumentStore,

    status: InitStatus,
    parent_pid: u64,
    root: Option<Url>,

    context: &'a dm::Context,
    preprocessor: Option<dm::preprocessor::Preprocessor<'a>>,
    objtree: dm::objtree::ObjectTree,
    references_table: Option<find_references::ReferencesTable>,

    annotations: HashMap<Url, (FileId, FileId, Rc<AnnotationTree>)>,
    diagnostics_set: HashSet<Url>,

    client_caps: ClientCaps,
}

impl<'a, W: io::ResponseWrite> Engine<'a, W> {
    fn new(write: &'a W, context: &'a dm::Context) -> Self {
        Engine {
            write,
            docs: Default::default(),

            status: InitStatus::Starting,
            parent_pid: 0,
            root: None,

            context,
            preprocessor: None,
            objtree: Default::default(),
            references_table: None,

            annotations: Default::default(),
            diagnostics_set: Default::default(),

            client_caps: Default::default(),
        }
    }

    // ------------------------------------------------------------------------
    // General input and output utilities

    fn issue_notification<T>(&self, params: T::Params)
    where
        T: langserver::notification::Notification,
        T::Params: serde::Serialize,
    {
        issue_notification::<_, T>(self.write, params)
    }

    fn show_message<S>(&mut self, typ: MessageType, message: S) where
        S: Into<String>
    {
        let message = message.into();
        eprintln!("{:?}: {}", typ, message);
        self.issue_notification::<langserver::notification::ShowMessage>(
            langserver::ShowMessageParams { typ, message }
        )
    }

    fn show_status<S>(&mut self, message: S) where
        S: Into<String>
    {
        self.issue_notification::<extras::WindowStatus>(extras::WindowStatusParams {
            environment: None,
            tasks: vec![message.into()],
        });
    }

    fn file_url(&self, file: dm::FileId) -> Result<Url, jsonrpc::Error> {
        if let Some(ref root) = self.root {
            root.join(&self.context.file_path(file).display().to_string())
                .map_err(|e| invalid_request(format!("error in file_url: {}", e)))
        } else {
            Url::parse(&self.context.file_path(file).display().to_string())
                .map_err(|e| invalid_request(format!("error in rootless file_url {}", e)))
        }
    }

    fn location_link(&self, loc: dm::Location) -> Result<String, jsonrpc::Error> {
        if loc.file == dm::FileId::builtins() {
            Ok(String::new())
        } else {
            let mut url = self.file_url(loc.file)?;
            url.set_fragment(Some(&loc.line.to_string()));
            Ok(url.to_string())
        }
    }

    fn convert_location(&self, loc: dm::Location, if_builtin: &[&str]) -> Result<langserver::Location, jsonrpc::Error> {
        Ok(langserver::Location {
            uri: if loc.file == dm::FileId::builtins() {
                Url::parse(&format!("dm://docs/reference.dm#{}", if_builtin.join("")))
                    .map_err(invalid_request)?
            } else {
                self.file_url(loc.file)?
            },
            range: location_to_range(loc),
        })
    }

    // ------------------------------------------------------------------------
    // Object tree explorer

    fn update_objtree(&mut self) {
        if self.client_caps.object_tree {
            let root = self.recurse_objtree(self.objtree.root());
            self.issue_notification::<extras::ObjectTree>(extras::ObjectTreeParams {
                root,
            });
        }
    }

    fn recurse_objtree(&self, ty: TypeRef) -> extras::ObjectTreeType {
        let mut entry = extras::ObjectTreeType {
            name: ty.name.to_owned(),
            kind: langserver::SymbolKind::Class,
            location: self.convert_location(ty.location, &[&ty.path]).ok(),
            vars: Vec::new(),
            procs: Vec::new(),
            children: Vec::new(),
        };

        // vars
        for (name, var) in ty.vars.iter() {
            let is_declaration = var.declaration.is_some();
            entry.vars.push(extras::ObjectTreeVar {
                name: name.to_owned(),
                kind: langserver::SymbolKind::Field,
                location: self.convert_location(var.value.location, &[&ty.path, "/var/", name]).ok(),
                is_declaration,
            });
        }
        entry.vars.sort_by(|a, b| a.name.cmp(&b.name));

        // procs
        for (name, proc) in ty.procs.iter() {
            let mut is_verb = proc.declaration.as_ref().map(|d| d.kind.is_verb());
            for value in proc.value.iter() {
                entry.procs.push(extras::ObjectTreeProc {
                    name: name.to_owned(),
                    kind: langserver::SymbolKind::Method,
                    location: self.convert_location(value.location, &[&ty.path, "/proc/", name]).ok(),
                    is_verb,
                });
                is_verb = None;
            }
        }
        entry.procs.sort_by(|a, b| a.name.cmp(&b.name));

        // child types
        for child in ty.children() {
            entry.children.push(self.recurse_objtree(child));
        }
        entry.children.sort_by(|a, b| a.name.cmp(&b.name));

        entry
    }

    // ------------------------------------------------------------------------
    // Environment tracking

    fn parse_environment(&mut self, environment: PathBuf) -> Result<(), jsonrpc::Error> {
        // handle the parsing
        let start = std::time::Instant::now();
        eprintln!("environment: {}", environment.display());
        if let Some(stem) = environment.file_stem() {
            self.issue_notification::<extras::WindowStatus>(extras::WindowStatusParams {
                environment: Some(stem.to_string_lossy().into_owned()),
                tasks: vec!["loading".to_owned()],
            })
        } else {
            self.show_status("loading");
        }

        let ctx = self.context;
        let mut pp = match dm::preprocessor::Preprocessor::new(ctx, environment.clone()) {
            Ok(pp) => pp,
            Err(err) => {
                use std::error::Error;
                self.issue_notification::<langserver::notification::PublishDiagnostics>(
                    langserver::PublishDiagnosticsParams {
                        uri: path_to_url(environment)?,
                        diagnostics: vec![langserver::Diagnostic {
                            message: err.description().to_owned(),
                            .. Default::default()
                        }],
                    },
                );
                eprintln!("{:?}", err);
                return Ok(());
            }
        };

        {
            let mut parser = dm::parser::Parser::new(ctx, dm::indents::IndentProcessor::new(ctx, &mut pp));
            parser.enable_procs();
            self.objtree = parser.parse_object_tree();
        }
        self.update_objtree();
        self.references_table = Some(find_references::ReferencesTable::new(&self.objtree));
        // TODO: run dreamchecker here if enabled
        pp.finalize();
        self.preprocessor = Some(pp);
        self.issue_notification::<extras::WindowStatus>(Default::default());
        let elapsed = start.elapsed();
        eprintln!(
            "parsed in {}.{:03}s",
            elapsed.as_secs(),
            elapsed.subsec_nanos() / 1_000_000
        );

        // initial diagnostics pump
        let mut map: HashMap<_, Vec<_>> = HashMap::new();
        for error in self.context.errors().iter() {
            let loc = error.location();
            let related_information = if !self.client_caps.related_info || error.notes().is_empty() {
                None
            } else {
                let mut notes = Vec::with_capacity(error.notes().len());
                for note in error.notes().iter() {
                    notes.push(langserver::DiagnosticRelatedInformation {
                        location: langserver::Location {
                            uri: self.file_url(note.location().file)?,
                            range: location_to_range(note.location()),
                        },
                        message: note.description().to_owned(),
                    });
                }
                Some(notes)
            };
            let diag = langserver::Diagnostic {
                message: error.description().to_owned(),
                severity: Some(convert_severity(error.severity())),
                range: location_to_range(loc),
                source: error.component().name().map(ToOwned::to_owned),
                related_information,
                .. Default::default()
            };
            map.entry(self.file_url(loc.file)?)
                .or_insert_with(Default::default)
                .push(diag);

            if !self.client_caps.related_info {
                // Fallback in case the client does not support related info
                for note in error.notes().iter() {
                    let diag = langserver::Diagnostic {
                        message: note.description().to_owned(),
                        severity: Some(langserver::DiagnosticSeverity::Information),
                        range: location_to_range(note.location()),
                        source: error.component().name().map(ToOwned::to_owned),
                        .. Default::default()
                    };
                    map.entry(self.file_url(note.location().file)?)
                        .or_insert_with(Default::default)
                        .push(diag);
                }
            }
        }

        let mut new_diagnostics_set = HashSet::new();
        for (url, diagnostics) in map {
            self.diagnostics_set.remove(&url);  // don't erase below
            new_diagnostics_set.insert(url.clone());
            self.issue_notification::<langserver::notification::PublishDiagnostics>(
                langserver::PublishDiagnosticsParams {
                    uri: url,
                    diagnostics,
                },
            );
        }

        // erase diagnostics for files which no longer have any
        for url in std::mem::replace(&mut self.diagnostics_set, new_diagnostics_set) {
            self.issue_notification::<langserver::notification::PublishDiagnostics>(
                langserver::PublishDiagnosticsParams {
                    uri: url.clone(),
                    diagnostics: Vec::new(),
                },
            );
        }

        Ok(())
    }

    fn get_annotations(&mut self, url: &Url) -> Result<(FileId, FileId, Rc<AnnotationTree>), jsonrpc::Error> {
        Ok(match self.annotations.entry(url.to_owned()) {
            Entry::Occupied(o) => o.get().clone(),
            Entry::Vacant(v) => match self.root {
                Some(ref root) => {
                    // normal path, when we have a workspace root & an environment loaded
                    let path = url_to_path(url)?;
                    let root = url_to_path(root)?;
                    let stripped = match path.strip_prefix(&root) {
                        Ok(path) => path,
                        Err(_) => return Err(invalid_request(format!("outside workspace: {}", url))),
                    };

                    let preprocessor = match self.preprocessor {
                        Some(ref pp) => pp,
                        None => return Err(invalid_request("no preprocessor")),
                    };
                    let (real_file_id, mut preprocessor) = match self.context.get_file(&stripped) {
                        Some(id) => (id, preprocessor.branch_at_file(id, &self.context)),
                        None => (FileId::default(), preprocessor.branch(&self.context)),
                    };
                    let contents = self.docs.read(url).map_err(invalid_request)?;
                    let file_id = preprocessor.push_file(stripped.to_owned(), contents);
                    preprocessor.enable_annotations();
                    let mut annotations = AnnotationTree::default();
                    {
                        let indent = dm::indents::IndentProcessor::new(&self.context, &mut preprocessor);
                        let mut parser = dm::parser::Parser::new(&self.context, indent);
                        parser.annotate_to(&mut annotations);
                        parser.run();
                    }
                    annotations.merge(preprocessor.take_annotations().unwrap());
                    v.insert((real_file_id, file_id, Rc::new(annotations))).clone()
                },
                None => {
                    // single-file mode
                    let filename = url.to_string();

                    let contents = self.docs.get_contents(url).map_err(invalid_request)?.into_owned();
                    let mut pp = dm::preprocessor::Preprocessor::from_buffer(&self.context, filename.clone().into(), contents);
                    let file_id = self.context.get_file(filename.as_ref()).expect("file didn't exist?");
                    // Clear old errors for this file. Hacky, but it will work for now.
                    self.context.errors_mut().retain(|error| error.location().file != file_id);

                    pp.enable_annotations();
                    let mut annotations = AnnotationTree::default();
                    {
                        let indent = dm::indents::IndentProcessor::new(&self.context, &mut pp);
                        let mut parser = dm::parser::Parser::new(&self.context, indent);
                        parser.annotate_to(&mut annotations);
                        // Every time anyone types anything the object tree is replaced.
                        // This is probably really inefficient, but it will do until
                        // selective definition deletion/reintroduction is implemented.
                        self.objtree = parser.parse_object_tree();
                    }
                    pp.finalize();
                    dreamchecker::run(&self.context, &self.objtree);

                    // Perform a diagnostics pump on this file only.
                    // Assume all errors are in this file.
                    let mut diagnostics = Vec::new();
                    for error in self.context.errors().iter() {
                        let loc = error.location();
                        if loc.file != file_id {
                            continue;
                        }

                        let related_information = if !self.client_caps.related_info || error.notes().is_empty() {
                            None
                        } else {
                            let mut notes = Vec::with_capacity(error.notes().len());
                            for note in error.notes().iter() {
                                notes.push(langserver::DiagnosticRelatedInformation {
                                    location: langserver::Location {
                                        uri: url.to_owned(),
                                        range: location_to_range(note.location()),
                                    },
                                    message: note.description().to_owned(),
                                });
                            }
                            Some(notes)
                        };
                        let diag = langserver::Diagnostic {
                            message: error.description().to_owned(),
                            severity: Some(convert_severity(error.severity())),
                            range: location_to_range(loc),
                            source: error.component().name().map(ToOwned::to_owned),
                            related_information,
                            .. Default::default()
                        };
                        diagnostics.push(diag);

                        if !self.client_caps.related_info {
                            // Fallback in case the client does not support related info
                            for note in error.notes().iter() {
                                let diag = langserver::Diagnostic {
                                    message: note.description().to_owned(),
                                    severity: Some(langserver::DiagnosticSeverity::Information),
                                    range: location_to_range(note.location()),
                                    source: error.component().name().map(ToOwned::to_owned),
                                    .. Default::default()
                                };
                                diagnostics.push(diag);
                            }
                        }
                    }

                    issue_notification::<_, langserver::notification::PublishDiagnostics>(
                        self.write,
                        langserver::PublishDiagnosticsParams {
                            uri: url.to_owned(),
                            diagnostics,
                        },
                    );

                    (file_id, file_id, Rc::new(annotations))
                }
            }
        })
    }

    fn find_type_context<'b, I, Ign>(&self, iter: &I) -> (Option<TypeRef>, Option<(&'b str, usize)>)
    where
        I: Iterator<Item = (Ign, &'b Annotation)> + Clone,
    {
        let mut found = None;
        let mut proc_name = None;
        if_annotation! { Annotation::ProcBody(ref proc_path, ref idx) in iter; {
            // chop off proc name and 'proc/' or 'verb/' if it's there
            // TODO: factor this logic somewhere
            let mut proc_path = &proc_path[..];
            match proc_path.split_last() {
                Some((name, rest)) => {
                    proc_name = Some((name.as_str(), *idx));
                    proc_path = rest;
                }
                _ => {}
            }
            match proc_path.split_last() {
                Some((kwd, rest)) if kwd == "proc" || kwd == "verb" => proc_path = rest,
                _ => {}
            }
            found = self.objtree.type_by_path(proc_path);
        }}
        if found.is_none() {
            if_annotation! { Annotation::TreeBlock(tree_path) in iter; {
                // cut off if we're in a declaration block
                let mut tree_path = &tree_path[..];
                match tree_path.split_last() {
                    Some((kwd, rest)) if kwd == "proc" || kwd == "verb" || kwd == "var" => tree_path = rest,
                    _ => {}
                }
                found = self.objtree.type_by_path(tree_path);
            }}
        }
        (found, proc_name)
    }

    fn find_unscoped_var<'b, I>(
        &'b self,
        iter: &I,
        ty: Option<TypeRef<'b>>,
        proc_name: Option<(&'b str, usize)>,
        var_name: &str,
    ) -> UnscopedVar<'b>
    where
        I: Iterator<Item = (Span, &'b Annotation)> + Clone,
    {
        // local variables
        for (span, annotation) in iter.clone() {
            if let Annotation::LocalVarScope(var_type, name) = annotation {
                if name == var_name {
                    return UnscopedVar::Local {
                        loc: span.start,
                        var_type,
                    };
                }
            }
        }

        // proc parameters
        let ty = ty.unwrap_or(self.objtree.root());
        if let Some((proc_name, idx)) = proc_name {
            if let Some(proc) = ty.get().procs.get(proc_name) {
                if let Some(value) = proc.value.get(idx) {
                    for param in value.parameters.iter() {
                        if &param.name == var_name {
                            return UnscopedVar::Parameter { ty, proc: proc_name, param };
                        }
                    }
                }
            }
        }

        // type variables (implicit `src.` and `globals.`)
        let mut next = Some(ty);
        while let Some(ty) = next {
            if let Some(var) = ty.get().vars.get(var_name) {
                return UnscopedVar::Variable { ty, var };
            }
            next = ty.parent_type();
        }
        UnscopedVar::None
    }

    fn find_scoped_type<'b, I>(&'b self, iter: &I, priors: &[String]) -> Option<TypeRef<'b>>
        where I: Iterator<Item=(Span, &'b Annotation)> + Clone
    {
        let (mut next, proc_name) = self.find_type_context(iter);
        // find the first; check the global scope, parameters, and "src"
        let mut priors = priors.iter();
        let first = match priors.next() {
            Some(i) => i,
            None => return next,  // empty priors acts like unscoped
        };
        if first == "args" {
            next = self.objtree.find("/list");
        } else if first == "global" {
            next = Some(self.objtree.root());
        } else if first == "src" {
            // nothing
        } else if first == "usr" {
            next = self.objtree.find("/mob");
        } else {
            next = match self.find_unscoped_var(iter, next, proc_name, first) {
                UnscopedVar::Parameter { param, .. } => self.objtree.type_by_path(&param.var_type.type_path),
                UnscopedVar::Variable { ty, .. } => match ty.get_var_declaration(first) {
                    Some(decl) => self.objtree.type_by_path(&decl.var_type.type_path),
                    None => None,
                },
                UnscopedVar::Local { var_type, .. } => self.objtree.type_by_path(&var_type.type_path),
                UnscopedVar::None => None,
            };
        }

        // find the rest; only look on the type we've found
        for var_name in priors {
            if let Some(current) = next.take() {
                if let Some(decl) = current.get_var_declaration(var_name) {
                    next = self.objtree.type_by_path(&decl.var_type.type_path);
                }
            } else {
                break;
            }
        }
        next
    }

    // ------------------------------------------------------------------------
    // Driver

    fn handle_input(&mut self, message: &str) {
        let mut outputs: Vec<Output> = match serde_json::from_str(message) {
            Ok(Request::Single(call)) => self.handle_call(call).into_iter().collect(),
            Ok(Request::Batch(calls)) => calls.into_iter().flat_map(|call| self.handle_call(call)).collect(),
            Err(decode_error) => vec![Output::Failure(jsonrpc::Failure {
                jsonrpc: VERSION,
                error: jsonrpc::Error {
                    code: jsonrpc::ErrorCode::ParseError,
                    message: decode_error.to_string(),
                    data: None,
                },
                id: jsonrpc::Id::Null,
            })],
        };

        let response = match outputs.len() {
            0 => return,  // wait for another input
            1 => Response::Single(outputs.remove(0)),
            _ => Response::Batch(outputs),
        };

        self.write.write(serde_json::to_string(&response).expect("response bad to_string"));
    }

    fn handle_call(&mut self, call: Call) -> Option<Output> {
        match call {
            Call::Invalid { id } => Some(Output::invalid_request(id, VERSION)),
            Call::MethodCall(method_call) => {
                let id = method_call.id.clone();
                Some(Output::from(self.handle_method_call(method_call), id, VERSION))
            },
            Call::Notification(notification) => {
                if let Err(e) = self.handle_notification(notification) {
                    self.show_message(MessageType::Error, e.message);
                }
                None
            },
        }
    }
}

handle_method_call! {
    // ------------------------------------------------------------------------
    // basic setup
    on Initialize(&mut self, init) {
        if self.status != InitStatus::Starting {
            return Err(invalid_request(""))
        }
        self.status = InitStatus::Running;

        if let Some(id) = init.process_id {
            self.parent_pid = id;
        }
        if let Some(mut url) = init.root_uri {
            if !url.path().ends_with("/") {
                let path = format!("{}/", url.path());
                url.set_path(&path);
            }
            self.root = Some(url);
        } else if let Some(path) = init.root_path {
            match Url::from_directory_path(path) {
                Ok(url) => self.root = Some(url),
                Err(()) => return Err(invalid_request("Root path does not correspond to a valid URL")),
            }
        } else {
            eprintln!("preparing single file mode");
        }

        // Extract relevant client capabilities.
        self.client_caps = ClientCaps::parse(&init.capabilities);
        let debug = format!("{:?}", self.client_caps);
        if let (Some(start), Some(end)) = (debug.find('{'), debug.rfind('}')) {
            eprintln!("client capabilities: {}", &debug[start + 2..end - 1]);
        } else {
            eprintln!("client capabilities: {}", debug);
        }

        InitializeResult {
            capabilities: ServerCapabilities {
                definition_provider: Some(true),
                workspace_symbol_provider: Some(true),
                hover_provider: Some(true),
                document_symbol_provider: Some(true),
                references_provider: Some(true),
                type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(TextDocumentSyncOptions {
                    open_close: Some(true),
                    change: Some(TextDocumentSyncKind::Incremental),
                    .. Default::default()
                })),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(vec![".".to_owned(), ":".to_owned(), "/".to_owned()]),
                    resolve_provider: None,
                }),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_owned(), ",".to_owned()]),
                }),
                .. Default::default()
            }
        }
    }

    on Shutdown(&mut self, ()) {
        self.status = InitStatus::ShuttingDown;
    }

    // ------------------------------------------------------------------------
    // actual stuff provision
    on WorkspaceSymbol(&mut self, params) {
        let query = symbol_search::Query::parse(&params.query);
        eprintln!("{:?} -> {:?}", params.query, query);

        let query = match query {
            Some(query) => query,
            None => return Ok(None),
        };

        let mut results = Vec::new();
        let start = std::time::Instant::now();
        if let Some(ref preprocessor) = self.preprocessor {
            for (range, &(ref name, _)) in preprocessor.history().iter() {
                if query.matches_define(name) {
                    results.push(SymbolInformation {
                        name: name.to_owned(),
                        kind: SymbolKind::Constant,
                        location: self.convert_location(range.start, &["/DM/preprocessor/", name])?,
                        container_name: None,
                        deprecated: None,
                    });
                }
            }
        }

        for (idx, ty) in self.objtree.graph.node_references() {
            if query.matches_type(&ty.name, &ty.path) && idx.index() != 0 {
                results.push(SymbolInformation {
                    name: ty.name.clone(),
                    kind: SymbolKind::Class,
                    location: self.convert_location(ty.location, &[&ty.path])?,
                    container_name: Some(ty.path[..ty.path.len() - ty.name.len() - 1].to_owned()),
                    deprecated: None,
                });
            }

            if !query.matches_on_type(&ty.path) {
                continue;
            }
            for (var_name, tv) in ty.vars.iter() {
                if let Some(decl) = tv.declaration.as_ref() {
                    if query.matches_var(&var_name) {
                        results.push(SymbolInformation {
                            name: var_name.clone(),
                            kind: SymbolKind::Field,
                            location: self.convert_location(decl.location, &[&ty.path, "/var/", var_name])?,
                            container_name: Some(ty.path.clone()),
                            deprecated: None,
                        });
                    }
                }
            }

            for (proc_name, pv) in ty.procs.iter() {
                if let Some(decl) = pv.declaration.as_ref() {
                    if query.matches_proc(&proc_name, decl.kind) {
                        results.push(SymbolInformation {
                            name: proc_name.clone(),
                            kind: if idx.index() == 0 {
                                SymbolKind::Function
                            } else if is_constructor_name(proc_name.as_str()) {
                                SymbolKind::Constructor
                            } else {
                                SymbolKind::Method
                            },
                            location: self.convert_location(decl.location, &[&ty.path, "/proc/", proc_name])?,
                            container_name: Some(ty.path.clone()),
                            deprecated: None,
                        });
                    }
                }
            }
        }
        let elapsed = start.elapsed();
        eprintln!("    {} results in {}.{:03}s", results.len(), elapsed.as_secs(), elapsed.subsec_nanos() / 1_000_000);
        Some(results)
    }

    on HoverRequest(&mut self, params) {
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };
        let mut results = Vec::new();

        for (_range, annotation) in annotations.get_location(location) {
            #[cfg(debug_assertions)] {
                results.push(format!("{:?}", annotation));
            }
            match annotation {
                Annotation::Variable(path) if !path.is_empty() => {
                    let objtree = &self.objtree;
                    let mut current = objtree.root();
                    let (last, most) = path.split_last().unwrap();
                    for part in most {
                        if part == "var" { break }
                        if let Some(child) = current.child(part) {
                            current = child;
                        } else {
                            break;
                        }
                    }

                    let mut infos = VecDeque::new();
                    let mut next = Some(current);
                    while let Some(current) = next {
                        if let Some(var) = current.vars.get(last) {
                            let constant = if let Some(ref constant) = var.value.constant {
                                format!("  \n= `{}`", constant)
                            } else {
                                String::new()
                            };
                            let path = if current.path.is_empty() {
                                "(global)"
                            } else {
                                &current.path
                            };
                            infos.push_front(format!("[{}]({}){}", path, self.location_link(var.value.location)?, constant));
                            if let Some(ref decl) = var.declaration {
                                let mut declaration = String::new();
                                declaration.push_str("var");
                                if decl.var_type.is_static {
                                    declaration.push_str("/static");
                                }
                                if decl.var_type.is_const {
                                    declaration.push_str("/const");
                                }
                                if decl.var_type.is_tmp {
                                    declaration.push_str("/tmp");
                                }
                                for bit in decl.var_type.type_path.iter() {
                                    declaration.push('/');
                                    declaration.push_str(&bit);
                                }
                                declaration.push_str("/**");
                                declaration.push_str(last);
                                declaration.push_str("**");
                                infos.push_front(declaration);
                            }
                        }
                        next = current.parent_type();
                    }
                    if !infos.is_empty() {
                        results.push(infos.into_iter().collect::<Vec<_>>().join("\n\n"));
                    }
                }
                Annotation::ProcHeader(path, _idx) if !path.is_empty() => {
                    let objtree = &self.objtree;
                    let mut current = objtree.root();
                    let (last, most) = path.split_last().unwrap();
                    for part in most {
                        if part == "proc" || part == "verb" { break }
                        if let Some(child) = current.child(part) {
                            current = child;
                        } else {
                            break;
                        }
                    }

                    // TODO: use `idx` and show the whole list here rather than
                    // the last proc for each type
                    let mut infos = VecDeque::new();
                    let mut next = Some(current);
                    while let Some(current) = next {
                        if let Some(proc) = current.procs.get(last) {
                            let path = if current.path.is_empty() {
                                "(global)"
                            } else {
                                &current.path
                            };
                            let proc_value = proc.main_value();
                            let mut message = format!("[{}]({})  \n{}(", path, self.location_link(proc_value.location)?, last);
                            let mut first = true;
                            for each in proc_value.parameters.iter() {
                                use std::fmt::Write;
                                if first {
                                    first = false;
                                } else {
                                    message.push_str(", ");
                                }
                                let _ = write!(message, "{}", each);
                            }
                            message.push_str(")");
                            infos.push_front(message);
                            if let Some(ref decl) = proc.declaration {
                                let mut declaration = String::new();
                                declaration.push_str(decl.kind.name());
                                declaration.push_str("/**");
                                declaration.push_str(last);
                                declaration.push_str("**");
                                infos.push_front(declaration);
                            }
                        }
                        next = current.parent_type();
                    }
                    if !infos.is_empty() {
                        results.push(infos.into_iter().collect::<Vec<_>>().join("\n\n"));
                    }
                }
                _ => {}
            }
        }
        if results.is_empty() {
            None
        } else {
            Some(Hover {
                range: None,
                contents: HoverContents::Array(results.into_iter().map(MarkedString::String).collect()),
            })
        }
    }

    on GotoDefinition(&mut self, params) {
        let (real_file_id, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };
        let mut results = Vec::new();

        let iter = annotations.get_location(location);
        match_annotation! { iter;
        Annotation::TreePath(absolute, parts) => {
            let full_path: Vec<&str> = completion::combine_tree_path(&iter, *absolute, parts).collect();

            if let Some(ty) = self.objtree.type_by_path(full_path.iter().cloned()) {
                results.push(self.convert_location(ty.location, &[&ty.path])?);
            } else if let Some((&proc_name, prefix)) = full_path.split_last() {
                // If it's not a type, try to find the proc equivalent. Start
                // at the parent type so that this is a decent shortcut for
                // going to the parent proc.
                // TODO: only do this if we're in a ProcHeader.
                let mut next = self.objtree.type_by_path(prefix);
                if let Some(ty) = next {
                    next = ty.parent_type();
                }
                while let Some(ty) = next {
                    if let Some(proc) = ty.procs.get(proc_name) {
                        results.push(self.convert_location(proc.main_value().location, &[&ty.path, "/proc/", proc_name])?);
                        break;
                    }
                    next = ty.parent_type();
                }
            }
        },
        Annotation::TypePath(parts) => {
            match self.follow_type_path(&iter, parts) {
                // '/datum/proc/foo'
                Some(completion::TypePathResult { ty, decl: _, proc: Some((proc_name, proc)) }) => {
                    results.push(self.convert_location(proc.location, &[&ty.path, "/proc/", proc_name])?);
                },
                // 'datum/bar'
                Some(completion::TypePathResult { ty, decl: None, proc: None }) => {
                    results.push(self.convert_location(ty.location, &[&ty.path])?);
                },
                _ => {}
            }
        },
        Annotation::UnscopedCall(proc_name) => {
            let (ty, _) = self.find_type_context(&iter);
            let mut next = ty.or(Some(self.objtree.root()));
            while let Some(ty) = next {
                if let Some(proc) = ty.procs.get(proc_name) {
                    results.push(self.convert_location(proc.main_value().location, &[&ty.path, "/proc/", proc_name])?);
                    break;
                }
                next = ty.parent_type();
            }
        },
        Annotation::UnscopedVar(var_name) => {
            let (ty, proc_name) = self.find_type_context(&iter);
            match self.find_unscoped_var(&iter, ty, proc_name, var_name) {
                UnscopedVar::Parameter { ty, proc, param } => {
                    results.push(self.convert_location(param.location, &[&ty.path, "/proc/", proc])?);
                },
                UnscopedVar::Variable { ty, var } => {
                    results.push(self.convert_location(var.value.location, &[&ty.path, "/var/", var_name])?);
                },
                UnscopedVar::Local { loc, .. } => {
                    results.push(self.convert_location(dm::Location { file: real_file_id, ..loc }, &[])?);
                },
                UnscopedVar::None => {}
            }
        },
        Annotation::ScopedCall(priors, proc_name) => {
            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(proc) = ty.procs.get(proc_name) {
                    results.push(self.convert_location(proc.main_value().location, &[&ty.path, "/proc/", proc_name])?);
                    break;
                }
                next = ignore_root(ty.parent_type());
            }
        },
        Annotation::ScopedVar(priors, var_name) => {
            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(var) = ty.vars.get(var_name) {
                    results.push(self.convert_location(var.value.location, &[&ty.path, "/var/", var_name])?);
                    break;
                }
                next = ignore_root(ty.parent_type());
            }
        },
        Annotation::ParentCall => {
            if let (Some(ty), Some((proc_name, idx))) = self.find_type_context(&iter) {
                // TODO: idx is always 0 unless there are multiple overrides in
                // the same .dm file, due to annotations operating against a
                // dummy ObjectTree which does not contain any definitions from
                // other files.
                if idx == 0 {
                    // first proc on the type, go to the REAL parent
                    let mut next = ty.parent_type();
                    while let Some(ty) = next {
                        if let Some(proc) = ty.procs.get(proc_name) {
                            results.push(self.convert_location(proc.main_value().location, &[&ty.path, "/proc/", proc_name])?);
                            break;
                        }
                        next = ty.parent_type();
                    }
                } else if let Some(proc) = ty.procs.get(proc_name) {
                    // override, go to the previous version of the proc
                    if let Some(parent) = proc.value.get(idx - 1) {
                        results.push(self.convert_location(parent.location, &[&ty.path, "/proc/", proc_name])?);
                    }
                }
            }
        },
        Annotation::MacroUse(name, location) => {
            results.push(self.convert_location(*location, &["/DM/preprocessor/", name])?);
        },
        }

        if results.is_empty() {
            None
        } else {
            Some(GotoDefinitionResponse::Array(results))
        }
    }

    on GotoTypeDefinition(&mut self, params) {
        // Like GotoDefinition, but only supports vars, then finds their types
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };

        let mut type_path: &[String] = &[];

        let iter = annotations.get_location(location);
        match_annotation! { iter;
        Annotation::UnscopedVar(var_name) => {
            let (ty, proc_name) = self.find_type_context(&iter);
            match self.find_unscoped_var(&iter, ty, proc_name, var_name) {
                UnscopedVar::Parameter { param, .. } => {
                    type_path = &param.var_type.type_path;
                },
                UnscopedVar::Variable { ty, .. } => {
                    if let Some(decl) = ty.get_var_declaration(var_name) {
                        type_path = &decl.var_type.type_path;
                    }
                },
                UnscopedVar::Local { var_type, .. } => {
                    type_path = &var_type.type_path;
                },
                UnscopedVar::None => {}
            }
        },
        Annotation::ScopedVar(priors, var_name) => {
            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(var) = ty.get().vars.get(var_name) {
                    if let Some(ref decl) = var.declaration {
                        type_path = &decl.var_type.type_path;
                        break;
                    }
                }
                next = ignore_root(ty.parent_type());
            }
        },
        }

        if type_path.is_empty() {
            None
        } else if let Some(ty) = self.objtree.type_by_path(type_path) {
            let ty_loc = self.convert_location(ty.location, &[&ty.path])?;
            Some(GotoDefinitionResponse::Scalar(ty_loc))
        } else {
            None
        }
    }

    on References(&mut self, params) {
        // Like GotoDefinition, but looks up references instead
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };

        let mut symbol_id = None;

        let iter = annotations.get_location(location);
        match_annotation! { iter;
        Annotation::Variable(path) => {
            let mut current = self.objtree.root();
            let (var_name, most) = path.split_last().unwrap();
            for part in most {
                if part == "var" { break }
                if let Some(child) = current.child(part) {
                    current = child;
                } else {
                    break;
                }
            }

            if let Some(decl) = current.get_var_declaration(var_name) {
                symbol_id = Some(decl.id);
            }
        },
        Annotation::ProcHeader(parts, _) => {
            let mut current = self.objtree.root();
            let (proc_name, most) = parts.split_last().unwrap();
            for part in most {
                if part == "proc" || part == "verb" { break }
                if let Some(child) = current.child(part) {
                    current = child;
                } else {
                    break;
                }
            }

            if let Some(decl) = current.get_proc_declaration(proc_name) {
                symbol_id = Some(decl.id);
            }
        },
        Annotation::TreePath(absolute, parts) => {
            if let Some(ty) = self.objtree.type_by_path(completion::combine_tree_path(&iter, *absolute, parts)) {
                symbol_id = Some(ty.id);
            }
        },
        Annotation::TypePath(parts) => {
            match self.follow_type_path(&iter, parts) {
                // '/datum/proc/foo'
                Some(completion::TypePathResult { ty, decl: _, proc: Some((proc_name, _)) }) => {
                    if let Some(decl) = ty.get_proc_declaration(proc_name) {
                        symbol_id = Some(decl.id);
                    }
                },
                // 'datum/bar'
                Some(completion::TypePathResult { ty, decl: None, proc: None }) => {
                    symbol_id = Some(ty.id);
                },
                _ => {}
            }
        },
        Annotation::UnscopedCall(proc_name) => {
            let (ty, _) = self.find_type_context(&iter);
            let mut next = ty.or(Some(self.objtree.root()));
            while let Some(ty) = next {
                if let Some(proc) = ty.procs.get(proc_name) {
                    if let Some(ref decl) = proc.declaration {
                        symbol_id = Some(decl.id);
                        break;
                    }
                }
                next = ty.parent_type();
            }
        },
        Annotation::UnscopedVar(var_name) => {
            let (ty, proc_name) = self.find_type_context(&iter);
            match self.find_unscoped_var(&iter, ty, proc_name, var_name) {
                UnscopedVar::Parameter { .. } => {
                    // TODO
                },
                UnscopedVar::Variable { ty, .. } => {
                    if let Some(decl) = ty.get_var_declaration(var_name) {
                        symbol_id = Some(decl.id);
                    }
                },
                UnscopedVar::Local { .. } => {
                    // TODO
                },
                UnscopedVar::None => {}
            }
        },
        Annotation::ScopedCall(priors, proc_name) => {
            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(proc) = ty.procs.get(proc_name) {
                    if let Some(ref decl) = proc.declaration {
                        symbol_id = Some(decl.id);
                        break;
                    }
                }
                next = ignore_root(ty.parent_type());
            }
        },
        Annotation::ScopedVar(priors, var_name) => {
            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(var) = ty.vars.get(var_name) {
                    if let Some(ref decl) = var.declaration {
                        symbol_id = Some(decl.id);
                        break;
                    }
                }
                next = ignore_root(ty.parent_type());
            }
        },
        // TODO: macros
        }

        let mut result = &[][..];
        if let Some(id) = symbol_id {
            if let Some(ref table) = self.references_table {
                result = table.find_references(id, params.context.include_declaration);
            }
        }
        if result.is_empty() {
            None
        } else {
            let mut output = Vec::new();
            for each in result {
                output.push(self.convert_location(*each, &[])?);
            }
            Some(output)
        }
    }

    on Completion(&mut self, params) {
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };
        let iter = annotations.get_location(location);
        let mut results = Vec::new();
        let mut any_annotation = false;

        match_annotation! { iter;
            // happy path annotations
            Annotation::TreePath(absolute, parts) => {
                let (query, parts) = parts.split_last().unwrap();
                let path = completion::combine_tree_path(&iter, *absolute, parts);
                let (exact, ty) = self.objtree.type_by_path_approx(path);
                self.tree_completions(&mut results, exact, ty, query);
                any_annotation = true;
            },
            Annotation::TypePath(parts) => {
                let ((last_op, query), parts) = parts.split_last().unwrap();
                self.path_completions(&mut results, &iter, parts, *last_op, query);
                any_annotation = true;
            },
            Annotation::UnscopedCall(query) |
            Annotation::UnscopedVar(query) => {
                self.unscoped_completions(&mut results, &iter, query);
                any_annotation = true;
            },
            Annotation::ScopedCall(priors, query) |
            Annotation::ScopedVar(priors, query) => {
                self.scoped_completions(&mut results, &iter, priors, query);
                any_annotation = true;
            },
            // error annotations, overrides anything else
            Annotation::ScopedMissingIdent(priors) => {
                results.clear();
                self.scoped_completions(&mut results, &iter, priors, "");
                any_annotation = true;
                break;
            },
            Annotation::IncompleteTypePath(parts, last_op) => {
                results.clear();
                self.path_completions(&mut results, &iter, parts, *last_op, "");
                any_annotation = true;
                break;
            },
            Annotation::IncompleteTreePath(absolute, parts) => {
                results.clear();
                let path = completion::combine_tree_path(&iter, *absolute, parts);
                let (exact, ty) = self.objtree.type_by_path_approx(path);
                self.tree_completions(&mut results, exact, ty, "");
                any_annotation = true;
                break;
            },
        }

        if !any_annotation {
            // Someone hit Ctrl+Space with no usable idents nearby
            let (ty, proc_name) = self.find_type_context(&iter);
            if proc_name.is_some() {
                // TODO: unscoped_completions calls find_type_context again
                self.unscoped_completions(&mut results, &iter, "");
            } else {
                self.tree_completions(&mut results, true, ty.unwrap_or(self.objtree.root()), "");
            }
        }

        if results.is_empty() {
            None
        } else {
            Some(CompletionResponse::Array(results))
        }
    }

    on SignatureHelpRequest(&mut self, params) {
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        let location = dm::Location {
            file: file_id,
            line: params.position.line as u32 + 1,
            column: params.position.character as u16 + 1,
        };
        let iter = annotations.get_location(location);
        let mut result = None;

        if_annotation! { Annotation::ProcArguments(priors, proc_name, mut idx) in iter; {
            // take the specific argument we're working on
            if_annotation! { Annotation::ProcArgument(i) in iter; {
                idx = *i;
            }}

            let mut next = self.find_scoped_type(&iter, priors);
            while let Some(ty) = next {
                if let Some(proc) = ty.procs.get(proc_name) {
                    use std::fmt::Write;

                    let mut params = Vec::new();
                    let mut label = format!("{}/{}(", ty.path, proc_name);
                    let mut sep = "";
                    for param in proc.main_value().parameters.iter() {
                        for each in param.var_type.type_path.iter() {
                            let _ = write!(label, "{}{}", sep, each);
                            sep = "/";
                        }
                        label.push_str(sep);
                        let start = label.len();
                        label.push_str(&param.name);
                        let end = label.len();
                        sep = ", ";

                        if self.client_caps.label_offset_support {
                            params.push(ParameterInformation {
                                label: ParameterLabel::LabelOffsets([start as u64, end as u64]),
                                documentation: None,
                            });
                        } else {
                            params.push(ParameterInformation {
                                label: ParameterLabel::Simple(param.name.clone()),
                                documentation: None,
                            });
                        }
                    }
                    let _ = write!(label, ")");

                    result = Some(SignatureHelp {
                        active_signature: Some(0),
                        active_parameter: Some(idx as i64),
                        signatures: vec![SignatureInformation {
                            label: label,
                            parameters: Some(params),
                            documentation: None,
                        }],
                    });
                    break;
                }
                next = ty.parent_type();
                if let Some(ref n) = next {
                    if n.is_root() && !priors.is_empty() {
                        break;
                    }
                }
            }
        }}

        result
    }

    on DocumentSymbolRequest(&mut self, params) {
        fn name_and_detail(path: &[String]) -> (String, Option<String>) {
            let (name, rest) = path.split_last().unwrap();
            (name.to_owned(), rest.last().map(ToOwned::to_owned))
        }

        // recursive traversal
        fn find_document_symbols(
            iter: &mut std::iter::Peekable<dm::annotation::Iter>,
            section_end: dm::Location,
        ) -> Vec<DocumentSymbol> {
            let mut result = Vec::new();

            loop {
                if let Some((range, _)) = iter.peek() {
                    if range.start >= section_end {
                        break;
                    }
                }

                let (child_range, annotation) = if let Some(x) = iter.next() {
                    x
                } else {
                    break;
                };

                let interval_tree::RangeInclusive { start, end } = child_range;
                let range = span_to_range(start..end);
                let selection_range = location_to_range(start);
                match annotation {
                    Annotation::TreeBlock(ref path) => {
                        let (name, detail) = name_and_detail(path);
                        result.push(DocumentSymbol {
                            name,
                            detail,
                            kind: SymbolKind::Class,
                            deprecated: None,
                            range,
                            selection_range,
                            children: Some(find_document_symbols(iter, end)),
                        });
                    },
                    Annotation::Variable(ref path) => {
                        result.push(DocumentSymbol {
                            name: path.last().unwrap().to_owned(),
                            detail: None,
                            kind: SymbolKind::Field,
                            deprecated: None,
                            range,
                            selection_range,
                            children: None,
                        });
                    },
                    Annotation::ProcBody(ref path, _) => {
                        let (name, detail) = name_and_detail(path);
                        let kind = if path.len() == 1 || (path.len() == 2 && path[0] == "proc") {
                            SymbolKind::Function
                        } else if is_constructor_name(&name) {
                            SymbolKind::Constructor
                        } else {
                            SymbolKind::Method
                        };
                        result.push(DocumentSymbol {
                            name,
                            detail,
                            kind,
                            deprecated: None,
                            range,
                            selection_range,
                            children: Some(find_document_symbols(iter, end)),
                        });
                    },
                    Annotation::LocalVarScope(_, ref name) => {
                        result.push(DocumentSymbol {
                            name: name.to_owned(),
                            detail: None,
                            kind: SymbolKind::Variable,
                            deprecated: None,
                            range,
                            selection_range,
                            children: None,
                        });
                    },
                    Annotation::MacroDefinition(ref name) => {
                        result.push(DocumentSymbol {
                            name: name.to_owned(),
                            detail: None,
                            kind: SymbolKind::Constant,
                            deprecated: None,
                            range,
                            selection_range,
                            children: None,
                        })
                    },
                    _ => {}
                }
            }

            result
        }

        // root
        let (_, file_id, annotations) = self.get_annotations(&params.text_document.uri)?;
        if annotations.is_empty() {
            None
        } else {
            let start = dm::Location { file: file_id, line: 0, column: 0 };
            let end = dm::Location { file: file_id, line: !0, column: !0 };
            let mut iter = annotations.get_range(start..end).peekable();
            Some(DocumentSymbolResponse::Nested(find_document_symbols(&mut iter, end)))
        }
    }
}

handle_notification! {
    // ------------------------------------------------------------------------
    // basic setup
    on Initialized(&mut self, _) {
        if let Some(ref root) = self.root {
            eprintln!("workspace root: {}", root);
        }

        let mut environment = None;
        if let Some(ref root) = self.root {
            // TODO: support non-files here
            if let Ok(root_path) = url_to_path(root) {
                environment = dm::detect_environment(&root_path, dm::DEFAULT_ENV).map_err(invalid_request)?;
            }
        }

        if let Some(environment) = environment {
            self.parse_environment(environment)?;
        } else if self.root.is_some() {
            self.show_status("no .dme file");
        } else {
            self.show_status("single file mode");
        }
    }

    on Reparse(&mut self, _p) {
        eprintln!("reparsing by request...");
        self.context.errors_mut().clear();
        return self.Initialized(_p);
    }

    // ------------------------------------------------------------------------
    // document content management
    on DidOpenTextDocument(&mut self, params) {
        self.docs.open(params.text_document)?;
    }

    on DidCloseTextDocument(&mut self, params) {
        let url = self.docs.close(params.text_document)?;
        self.annotations.remove(&url);
    }

    on DidChangeTextDocument(&mut self, params) {
        let url = self.docs.change(params.text_document, params.content_changes)?;
        self.annotations.remove(&url);
    }

    on DidChangeConfiguration(&mut self, _) {}
}

// ----------------------------------------------------------------------------
// Helper functions

fn params_to_value(params: jsonrpc::Params) -> serde_json::Value {
    match params {
        jsonrpc::Params::None => serde_json::Value::Null,
        jsonrpc::Params::Array(x) => serde_json::Value::Array(x),
        jsonrpc::Params::Map(x) => serde_json::Value::Object(x),
    }
}

fn value_to_params(value: serde_json::Value) -> jsonrpc::Params {
    match value {
        serde_json::Value::Null => jsonrpc::Params::None,
        serde_json::Value::Array(x) => jsonrpc::Params::Array(x),
        serde_json::Value::Object(x) => jsonrpc::Params::Map(x),
        _ => panic!("bad value to params conversion"),
    }
}

fn invalid_request<S: ToString>(message: S) -> jsonrpc::Error {
    jsonrpc::Error {
        code: jsonrpc::ErrorCode::InvalidRequest,
        message: message.to_string(),
        data: None,
    }
}

fn url_to_path(url: &Url) -> Result<PathBuf, jsonrpc::Error> {
    if url.scheme() != "file" {
        return Err(invalid_request("URI must have 'file' scheme"));
    }
    url.to_file_path().map_err(|_| invalid_request("URI must be a valid path"))
}

fn path_to_url(path: PathBuf) -> Result<Url, jsonrpc::Error> {
    let formatted = path.display().to_string();
    Url::from_file_path(path).map_err(|_| invalid_request(format!(
        "bad file path: {}", formatted,
    )))
}

fn convert_severity(severity: dm::Severity) -> langserver::DiagnosticSeverity {
    match severity {
        dm::Severity::Error => langserver::DiagnosticSeverity::Error,
        dm::Severity::Warning => langserver::DiagnosticSeverity::Warning,
        dm::Severity::Info => langserver::DiagnosticSeverity::Information,
        dm::Severity::Hint => langserver::DiagnosticSeverity::Hint,
    }
}

enum UnscopedVar<'a> {
    Parameter {
        ty: TypeRef<'a>,
        proc: &'a str,
        param: &'a dm::ast::Parameter,
    },
    Variable {
        ty: TypeRef<'a>,
        var: &'a dm::objtree::TypeVar,
    },
    Local {
        loc: dm::Location,
        var_type: &'a dm::ast::VarType,
    },
    None,
}

fn is_constructor_name(name: &str) -> bool {
    name == "New" || name == "init" || name == "Initialize"
}

fn ignore_root(t: Option<TypeRef>) -> Option<TypeRef> {
    match t {
        Some(t) if t.is_root() => None,
        other => other,
    }
}

fn location_to_position(loc: dm::Location) -> langserver::Position  {
    langserver::Position {
        line: loc.line.saturating_sub(1) as u64,
        character: loc.column.saturating_sub(1) as u64,
    }
}

fn location_to_range(loc: dm::Location) -> langserver::Range {
    let pos = location_to_position(loc);
    langserver::Range::new(pos, pos)
}

fn span_to_range(range: ::std::ops::Range<dm::Location>) -> langserver::Range {
    langserver::Range::new(location_to_position(range.start), location_to_position(range.end))
}

fn issue_notification<W: io::ResponseWrite, T>(write: &W, params: T::Params)
where
    T: langserver::notification::Notification,
    T::Params: serde::Serialize,
{
    let params = serde_json::to_value(params).expect("notification bad to_value");
    let request = Request::Single(Call::Notification(jsonrpc::Notification {
        jsonrpc: VERSION,
        method: T::METHOD.to_owned(),
        params: value_to_params(params),
    }));
    write.write(serde_json::to_string(&request).expect("notification bad to_string"))
}
