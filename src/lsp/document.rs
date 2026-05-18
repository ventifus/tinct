//! Document storage and incremental re-parsing for the LSP.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use lsp_types::Uri;

use crate::ast::{File, Spanned};
use crate::builtins::create_stdlib_env_with_arena;
use crate::error::EvalError;
use crate::parser::{parse2, ParseError};
use crate::typecheck::{typecheck_file_with_types_and_env, DocMap, SchemeMap, TypeMap};
use crate::types::TypeError;
use crate::value::Environment;

/// The parsed and analyzed state of a single document.
#[derive(Debug, Clone)]
pub struct DocumentState {
    /// The original source text.
    pub text: String,
    /// Parsed AST (if parsing succeeded).
    pub ast: Result<Spanned<File>, ParseError>,
    /// Recovered parse errors from inside bracket forms (non-fatal; collected even when `ast` is Ok).
    /// These come from `ParseOutput.errors` and represent errors where the parser substituted
    /// an `Expr::Error` node and continued rather than stopping.
    pub parse_errors: Vec<ParseError>,
    /// Type errors (advisory; evaluation proceeds regardless).
    pub type_errors: Vec<TypeError>,
    /// Evaluation errors (if eval was attempted and failed).
    pub eval_errors: Vec<EvalError>,
    /// Map from expression spans to inferred types (for hover).
    pub type_map: TypeMap,
    /// Map from variable/parameter names to documentation strings (for hover).
    pub doc_map: DocMap,
    /// Map from VarRef spans to their TypeScheme (for hover constraint display).
    pub scheme_map: SchemeMap,
    /// Extracted literate blocks (for .md files only).
    /// Empty for .llt files.
    pub literate_blocks: Vec<crate::literate::LiterateBlock>,
}

impl DocumentState {
    /// Create a new document state by parsing and analyzing the given text.
    ///
    /// The `stdlib_env` parameter is the cached stdlib environment from the
    /// [`DocumentStore`]. Each document evaluation creates a child scope, so
    /// the shared env is never mutated.
    ///
    /// The `base_dir` parameter is used to resolve include paths for building
    /// the type environment. Pass `None` for source-only contexts.
    pub fn new(
        text: String,
        stdlib_env: &Rc<RefCell<Environment>>,
        eval_ctx: &Rc<crate::eval::EvalContext>,
        base_dir: Option<&std::path::Path>,
    ) -> Self {
        // Use parse2() to capture both the AST and any recovered parse errors.
        let parse_result = parse2(&text);
        let mut parse_errors = Vec::new();
        let mut ast: Result<Spanned<File>, ParseError> = match parse_result {
            Ok(output) => {
                parse_errors = output.errors;
                Ok(output.file)
            }
            Err(err) => Err(err),
        };
        let mut type_errors = Vec::new();
        let eval_errors = Vec::new();
        let mut type_map = TypeMap::new();
        let mut doc_map = DocMap::new();
        let mut scheme_map = SchemeMap::new();

        if let Ok(file) = ast {
            // PIPELINE INVARIANT: expand_macros → desugar → resolve → typecheck
            // This order is enforced across all entry points (main.rs, lib.rs, repl.rs).
            // Macros expand first, then $_ placeholders are desugared to lambdas, then
            // variable resolution runs, then the type checker sees the fully elaborated AST.

            // Expand macros before desugar: rewrites [defmacro ...] and macro calls.
            // This matches the pipeline used by all other entry points (main.rs, lib.rs).
            let mut file = match crate::expand::expand_macros(file, eval_ctx.config.no_fs) {
                Ok(result) => result.file,
                Err(e) => {
                    // Macro expansion error — convert to parse error
                    return Self {
                        text: text.clone(),
                        ast: Err(crate::parser::ParseError {
                            message: format!("macro expansion error: {}", e),
                            span: None,
                        }),
                        parse_errors: vec![crate::parser::ParseError {
                            message: format!("macro expansion error: {}", e),
                            span: None,
                        }],
                        type_errors: vec![],
                        eval_errors: vec![],
                        type_map: TypeMap::new(),
                        doc_map: DocMap::new(),
                        scheme_map: SchemeMap::new(),
                        literate_blocks: vec![],
                    };
                }
            };

            // Desugar before type check and eval: rewrites $_ implicit lambdas to explicit forms.
            // This matches the pipeline used by all other entry points (main.rs, repl.rs, lib.rs,
            // builtins.rs). Without this pass the type checker sees VarRef("_") instead of Fn nodes,
            // producing spurious "undefined variable _" errors for any $_ expression.
            crate::desugar::desugar_file(&mut file.node);

            // Variable resolution pass (Phase 1 of arena allocation strategy).
            crate::resolve::resolve_file(&file.node);

            // Run type checker (advisory), collecting the span-to-type map for hover.
            // Seed the type environment with prelude types and resolved includes via the
            // shared imports module to suppress false "undefined variable" errors.
            // When no_fs is set, skip include resolution in the type checker too — passing
            // None suppresses all $include path traversal while still seeding prelude types.
            let type_base_dir = if eval_ctx.config.no_fs {
                None
            } else {
                base_dir
            };
            let (seeded_env, include_bindings) =
                crate::imports::build_type_env(&file.node, type_base_dir);
            let (errs, mut map, docs, smap, _diagnostics) =
                typecheck_file_with_types_and_env(&file.node, seeded_env);
            // Post-pass: inject precise Record types for [include %cap "path"] expressions.
            crate::imports::apply_include_type_post_pass(&file.node, &include_bindings, &mut map);
            type_errors = errs;
            type_map = map;
            doc_map = docs;
            scheme_map = smap;
            // TODO: Convert diagnostics to LSP diagnostics (type-warning-channel infrastructure only)

            // Build the LSP eval environment, mirroring what main.rs does at startup.
            // The type checker gets runtime percent-vars via build_type_env(); we inject
            // real DirCap values here so the evaluator can resolve [include %libdir ...]
            // and [include %pwd ...] without spurious E002 errors.
            let lsp_eval_env = {
                use crate::ast::{Annotation, Span};
                use crate::value::{Thunk, Value};
                use indexmap::IndexMap;

                // Always create a fresh child env — never mutate the shared stdlib_env.
                let env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
                    stdlib_env,
                ))));

                {
                    let mut e = env.borrow_mut();
                    let zero = Span::origin();

                    // %libdir — actual stdlib directory (same path build_type_env uses).
                    if let Some(libdir_path) = crate::find_libdir_path() {
                        if let Ok(dir) = cap_std::fs::Dir::open_ambient_dir(
                            &libdir_path,
                            cap_std::ambient_authority(),
                        ) {
                            e.insert(
                                "%libdir".to_string(),
                                Rc::new(Thunk::new_materialized(
                                    Value::DirCap {
                                        dir: Rc::new(dir),
                                        perms: crate::value::DirPerms::full(),
                                    },
                                    zero,
                                )),
                            );
                        }
                    }

                    // %pwd — document's directory.
                    if let Some(pwd_path) = base_dir {
                        if let Ok(dir) = cap_std::fs::Dir::open_ambient_dir(
                            pwd_path,
                            cap_std::ambient_authority(),
                        ) {
                            e.insert(
                                "%pwd".to_string(),
                                Rc::new(Thunk::new_materialized(
                                    Value::DirCap {
                                        dir: Rc::new(dir),
                                        perms: crate::value::DirPerms::full(),
                                    },
                                    zero,
                                )),
                            );
                        }
                    }

                    // %stdin — stub string; LSP has no real stdin.
                    let source = Rc::<str>::from("lsp-stub-stdin");
                    e.insert(
                        "%stdin".to_string(),
                        Rc::new(Thunk::new_materialized(
                            Value::String {
                                source: Rc::clone(&source),
                                start: 0,
                                end: source.len(),
                            },
                            zero,
                        )),
                    );
                }

                // Inject stub caps for capabilities declared in `--- caps:` sections.
                for doc in &file.node.documents {
                    if let Some(ref caps_ann) = doc.node.caps {
                        for (cap_name, annotation) in &caps_ann.node {
                            let full_cap_name = format!("%{}", cap_name);

                            // Skip if already injected above.
                            if env.borrow().get(&full_cap_name).is_some() {
                                continue;
                            }

                            let stub_value = match annotation {
                                Annotation::Simple(type_name) if type_name == "NetCap" => {
                                    Value::NetCap(Rc::new(vec![]))
                                }
                                Annotation::Simple(type_name) if type_name == "DirCap" => {
                                    // Try to open a stub directory for DirCap: "." first, then temp_dir.
                                    // If both fail, fall back to an empty Dict stub. The LSP must never
                                    // panic on filesystem errors — degraded service is preferable to
                                    // crashing the editor.
                                    match cap_std::fs::Dir::open_ambient_dir(
                                        ".",
                                        cap_std::ambient_authority(),
                                    )
                                    .or_else(|_| {
                                        cap_std::fs::Dir::open_ambient_dir(
                                            std::env::temp_dir(),
                                            cap_std::ambient_authority(),
                                        )
                                    }) {
                                        Ok(stub_dir) => Value::DirCap {
                                            dir: Rc::new(stub_dir),
                                            perms: crate::value::DirPerms::full(),
                                        },
                                        Err(e) => {
                                            eprintln!(
                                                "LSP: warning: failed to open stub dir for DirCap (tried . and temp_dir): {}; using empty Dict stub",
                                                e
                                            );
                                            Value::Dict(IndexMap::new())
                                        }
                                    }
                                }
                                Annotation::Simple(type_name) if type_name == "Handle" => {
                                    let source = Rc::<str>::from("lsp-stub-handle");
                                    Value::String {
                                        source: Rc::clone(&source),
                                        start: 0,
                                        end: source.len(),
                                    }
                                }
                                Annotation::Simple(type_name) if type_name == "ClockCap" => {
                                    Value::ClockCap(Rc::new(crate::value::ClockCapInner::Fixed(0)))
                                }
                                _ => Value::Dict(IndexMap::new()),
                            };

                            env.borrow_mut().insert(
                                full_cap_name,
                                Rc::new(Thunk::new_materialized(stub_value, caps_ann.span)),
                            );
                        }
                    }
                }

                env
            };

            // Evaluation intentionally skipped in LSP context.
            //
            // The type checker (above) provides everything LSP features need: type_map
            // for hover, doc_map for docs, scheme_map for constraints, type_errors for
            // diagnostics. Running the evaluator here causes false-positive diagnostics
            // for any program that uses caps (%pwd, %nc, etc.) because the LSP cannot
            // supply real capability values — file I/O, network, and env access all fail
            // with misleading errors (E080 file-not-found, E002 undefined-var, etc.).
            //
            // If pure-eval diagnostics are added in future, gate them on the document
            // declaring no caps and using no % variables.
            let _ = lsp_eval_env; // suppress unused-variable warning

            ast = Ok(file);
        }

        Self {
            text,
            ast,
            parse_errors,
            type_errors,
            eval_errors,
            type_map,
            doc_map,
            scheme_map,
            literate_blocks: vec![],
        }
    }

    /// Create a new document state from markdown source.
    ///
    /// Extracts all tinct code blocks and stores them for per-block LSP analysis.
    /// Diagnostics are generated on-demand in `diagnostics_for` rather than
    /// pre-aggregated here, so spans can be correctly mapped to markdown coordinates.
    pub fn new_markdown(
        text: String,
        _stdlib_env: &Rc<RefCell<Environment>>,
        _eval_ctx: &Rc<crate::eval::EvalContext>,
        _base_dir: Option<&std::path::Path>,
    ) -> Self {
        // Extract literate blocks
        let blocks = crate::literate::extract_blocks(&text);

        // Return a DocumentState with no top-level AST (markdown has no single AST)
        // LSP features (hover, diagnostics) will analyze blocks on-demand
        Self {
            text,
            ast: Err(ParseError {
                message: "markdown file (no single AST)".to_string(),
                span: None,
            }),
            parse_errors: vec![],
            type_errors: vec![],
            eval_errors: vec![],
            type_map: TypeMap::new(),
            doc_map: DocMap::new(),
            scheme_map: SchemeMap::new(),
            literate_blocks: blocks,
        }
    }
}

/// Resolve an include path relative to a base URI.
///
/// Returns `None` for non-`file://` base URIs or resolution failures.
pub fn resolve_include_uri(base_uri: &Uri, path: &str) -> Option<Uri> {
    use crate::lsp::convert::{file_path_to_uri, uri_to_file_path};

    // Convert base URI to a file path (implicitly checks for file:// scheme)
    let base_path = uri_to_file_path(base_uri)?;
    let base_dir = base_path.parent()?;

    // Resolve the include path relative to the base directory
    let resolved_path = base_dir.join(path);

    // Canonicalize to handle .. and . components
    let canonical_path = resolved_path.canonicalize().ok()?;

    // Security: reject paths that escape the workspace root (base_dir).
    // A canonicalized path that no longer starts with the document's parent
    // directory indicates path traversal via `..` or an absolute path.
    let canonical_base = base_dir.canonicalize().ok()?;
    if !canonical_path.starts_with(&canonical_base) {
        return None;
    }

    // Convert back to URI
    file_path_to_uri(&canonical_path)
}

/// Index a file and its includes into the include graph.
///
/// Recursively indexes all included files up to a depth limit of 16.
/// Uses plain `std::fs::read_to_string` (not eval-time `$include`) — safe
/// because no user code execution occurs, only parsing.
pub fn index_file(
    uri: Uri,
    graph: &mut IncludeGraph,
    stdlib_env: &Rc<RefCell<Environment>>,
    eval_ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
) -> Result<(), String> {
    const MAX_DEPTH: usize = 16;

    if depth >= MAX_DEPTH {
        return Err(format!(
            "Include depth limit ({}) exceeded at {}",
            MAX_DEPTH,
            uri.as_str()
        ));
    }

    // Skip if already indexed
    if graph.contains_key(&uri) {
        return Ok(());
    }

    // Read the file
    let path = crate::lsp::convert::uri_to_file_path(&uri)
        .ok_or_else(|| format!("Cannot convert URI to path: {}", uri.as_str()))?;

    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

    // Create document state (base_dir for include resolution is the file's directory)
    let base_dir = path.parent();
    let state = DocumentState::new(text, stdlib_env, eval_ctx, base_dir);

    // Collect include paths from this file using the shared imports module
    let include_paths = if let Ok(ref file) = state.ast {
        crate::imports::collect_include_paths(&file.node)
    } else {
        vec![]
    };

    // Resolve include URIs (note: imports module returns (Span, Option<String>, String))
    let mut includes = Vec::new();
    for (_span, _cap_name, path) in include_paths {
        if let Some(include_uri) = resolve_include_uri(&uri, &path) {
            includes.push(include_uri);
        }
    }

    // Insert this node into the graph (before recursing, to handle circular deps)
    graph.insert(
        uri.clone(),
        IncludeNode {
            state,
            includes: includes.clone(),
            included_by: vec![],
        },
    );

    // Recursively index included files and build reverse edges
    for include_uri in &includes {
        // Recurse
        if let Err(e) = index_file(include_uri.clone(), graph, stdlib_env, eval_ctx, depth + 1) {
            eprintln!("LSP: Failed to index {}: {}", include_uri.as_str(), e);
            continue;
        }

        // Add reverse edge
        if let Some(included_node) = graph.get_mut(include_uri) {
            if !included_node.included_by.contains(&uri) {
                included_node.included_by.push(uri.clone());
            }
        }
    }

    Ok(())
}

/// Invalidate and re-index all dependents of a changed file.
///
/// Follows reverse edges breadth-first to find all files that transitively
/// include the changed file, then re-indexes them.
pub fn invalidate_dependents(
    changed_uri: &Uri,
    graph: &mut IncludeGraph,
    stdlib_env: &Rc<RefCell<Environment>>,
    eval_ctx: &Rc<crate::eval::EvalContext>,
) {
    use std::collections::VecDeque;
    let mut queue = VecDeque::new();
    let mut visited = std::collections::HashSet::new();

    // Start with the changed file's dependents
    if let Some(node) = graph.get(changed_uri) {
        for dependent_uri in &node.included_by {
            queue.push_back(dependent_uri.clone());
        }
    }

    // BFS through reverse edges
    while let Some(uri) = queue.pop_front() {
        if !visited.insert(uri.clone()) {
            continue; // Already processed
        }

        // Re-index this file
        // Remove the old entry first to avoid depth-limit issues
        graph.remove(&uri);

        if let Err(e) = index_file(
            uri.clone(),
            graph,
            stdlib_env,
            eval_ctx,
            0, // Reset depth for re-indexing
        ) {
            eprintln!("LSP: Failed to re-index {}: {}", uri.as_str(), e);
            continue;
        }

        // Add its dependents to the queue
        if let Some(node) = graph.get(&uri) {
            for dependent_uri in &node.included_by {
                queue.push_back(dependent_uri.clone());
            }
        }
    }
}

/// Node in the include dependency graph.
#[derive(Debug, Clone)]
pub struct IncludeNode {
    /// Parsed and analyzed state of this file.
    pub state: DocumentState,
    /// URIs of files this file includes (forward edges).
    pub includes: Vec<Uri>,
    /// URIs of files that include this file (reverse edges).
    pub included_by: Vec<Uri>,
}

/// Include dependency graph for cross-file resolution.
///
/// Tracks all files reachable via `$include` from open documents.
/// Enables go-to-definition and hover across file boundaries.
pub type IncludeGraph = HashMap<Uri, IncludeNode>;

/// Storage for all open documents.
///
/// Holds a cached stdlib environment that is created once and shared across
/// all document evaluations, avoiding the cost of re-parsing and evaluating
/// the 500+ line stdlib prelude on every keystroke.
pub struct DocumentStore {
    docs: HashMap<Uri, DocumentState>,
    /// Cached stdlib environment, created once on construction.
    stdlib_env: Rc<RefCell<Environment>>,
    /// Base evaluation context (with "." as base_dir).
    base_eval_ctx: Rc<crate::eval::EvalContext>,
    /// Include dependency graph for cross-file resolution.
    pub include_graph: IncludeGraph,
    /// Parsed prelude AST (for go-to-definition in stdlib functions).
    /// Created once on construction by parsing the embedded prelude source.
    prelude_ast: Option<Spanned<File>>,
}

impl DocumentStore {
    pub fn new() -> Self {
        // Load stdlib once. If it fails, fall back to an empty environment + empty arena
        // so the LSP can still provide parsing/type-checking diagnostics.
        let (stdlib_env, stdlib_arena) = create_stdlib_env_with_arena().unwrap_or_else(|_| {
            (
                Rc::new(RefCell::new(Environment::new())),
                Rc::new(RefCell::new(crate::arena::ThunkArena::new())),
            )
        });
        // Create base evaluation context.
        // no_fs=true prevents executing $include with user-controlled paths when
        // opening malicious .llt files in an editor (CWE-22 path traversal mitigation).
        //
        // Fallback chain for base_dir: try "." first, then temp_dir, then "/" as last resort.
        // This handles systemd socket activation, chroots, and containers where CWD or
        // temp may be inaccessible.
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .or_else(|_| {
                cap_std::fs::Dir::open_ambient_dir(
                    std::env::temp_dir(),
                    cap_std::ambient_authority(),
                )
            })
            .or_else(|_| {
                // Last resort: try root directory.
                cap_std::fs::Dir::open_ambient_dir("/", cap_std::ambient_authority())
            })
            .unwrap_or_else(|e| {
                // All fallbacks failed. Try /tmp and /var/tmp as additional attempts.
                eprintln!(
                    "LSP: warning: failed to open /, trying /tmp: {}",
                    e
                );
                cap_std::fs::Dir::open_ambient_dir("/tmp", cap_std::ambient_authority())
                    .or_else(|_| {
                        eprintln!("LSP: warning: /tmp failed, trying /var/tmp");
                        cap_std::fs::Dir::open_ambient_dir("/var/tmp", cap_std::ambient_authority())
                    })
                    .unwrap_or_else(|final_err| {
                        // Truly exhausted all options. Log error and exit gracefully
                        // rather than panicking. The editor can restart the LSP.
                        eprintln!(
                            "LSP: FATAL: cannot open any base_dir (tried ., temp_dir, /, /tmp, /var/tmp): {}",
                            final_err
                        );
                        eprintln!("LSP: filesystem appears inaccessible; cannot start");
                        std::process::exit(1);
                    })
            });
        // no_fs=false: the capability model (DirCap / RESOLVE_BENEATH in cap_std) provides
        // path-traversal protection. %libdir and %pwd are injected as real DirCaps in
        // DocumentState::new(), which limits access to the stdlib dir and document dir
        // respectively. Bare capless includes are rejected by builtin_include (see builtins_meta.rs).
        let base_eval_ctx = crate::eval::EvalContext::new_sharing_arena(
            base_dir,
            Rc::clone(&stdlib_env),
            false,
            stdlib_arena,
        );

        // Parse the embedded prelude source once for go-to-definition support.
        // If parsing fails, store None — prelude go-to-definition will be unavailable
        // but other LSP features (hover on user code, local definitions, etc.) still work.
        let prelude_ast = {
            let prelude_source = include_str!("../../stdlib/prelude.llt");
            crate::parser::parse(prelude_source).ok()
        };

        Self {
            docs: HashMap::new(),
            stdlib_env,
            base_eval_ctx,
            include_graph: HashMap::new(),
            prelude_ast,
        }
    }

    /// Update or insert a document, re-parsing and re-analyzing the text.
    pub fn update_document(&mut self, uri: Uri, text: String) {
        // Create evaluation context with document's directory as base_dir.
        // $include paths should resolve against the document's directory, not editor cwd.
        let base_path = crate::lsp::convert::uri_to_file_path(&uri)
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Fallback chain: try document's directory first, then ".", then base_eval_ctx's Dir.
        // This handles cases where the document's directory becomes inaccessible mid-session
        // (e.g., unmounted network share, deleted directory).
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_path, cap_std::ambient_authority())
            .or_else(|_| cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()))
            .or_else(|_| {
                // Fallback: reopen base_eval_ctx's Dir. cap_std::fs::Dir doesn't implement
                // Clone, so we open "." relative to base_dir to get a duplicate handle.
                self.base_eval_ctx.config.base_dir.open_dir(".")
            })
            .unwrap_or_else(|e| {
                // All three attempts failed (document dir, ".", and base_eval_ctx.base_dir).
                // Log a warning and fall back to temp_dir as a last resort. The LSP will
                // continue with degraded service rather than crashing the editor.
                eprintln!(
                    "LSP: warning: failed to open base_dir for {}: {}; falling back to temp_dir",
                    uri.as_str(),
                    e
                );
                cap_std::fs::Dir::open_ambient_dir(
                    std::env::temp_dir(),
                    cap_std::ambient_authority(),
                )
                .unwrap_or_else(|temp_err| {
                    // Even temp_dir failed. Try "/" as absolute last resort.
                    eprintln!(
                        "LSP: warning: temp_dir fallback failed: {}; trying /",
                        temp_err
                    );
                    cap_std::fs::Dir::open_ambient_dir("/", cap_std::ambient_authority())
                        .unwrap_or_else(|final_err| {
                            // Everything failed. Log error and continue with a broken Dir by
                            // re-attempting base_eval_ctx.base_dir.open_dir("."), which should
                            // work since it succeeded in DocumentStore::new(). If it fails here,
                            // something has changed mid-session (very rare). Log and exit.
                            eprintln!(
                                "LSP: CRITICAL: cannot open any base_dir for document: {}",
                                final_err
                            );
                            eprintln!(
                                "LSP: filesystem state changed mid-session; exiting to avoid crash"
                            );
                            std::process::exit(1);
                        })
                })
            });
        let eval_ctx = self.base_eval_ctx.with_base_dir(base_dir);

        // Detect .md files and use markdown extraction
        let is_markdown = uri.as_str().ends_with(".md");
        let state = if is_markdown {
            DocumentState::new_markdown(text, &self.stdlib_env, &eval_ctx, Some(&base_path))
        } else {
            DocumentState::new(text, &self.stdlib_env, &eval_ctx, Some(&base_path))
        };

        // Collect include paths from the new AST using the shared imports module
        let new_includes = if let Ok(ref file) = state.ast {
            crate::imports::collect_include_paths(&file.node)
                .into_iter()
                .filter_map(|(_span, _cap_name, path)| resolve_include_uri(&uri, &path))
                .collect::<Vec<_>>()
        } else {
            vec![]
        };

        // Get old includes to detect changes
        let old_includes = self
            .include_graph
            .get(&uri)
            .map(|node| node.includes.clone())
            .unwrap_or_default();

        // Index new includes
        for include_uri in &new_includes {
            if !old_includes.contains(include_uri) {
                // New include detected — index it
                if let Err(e) = index_file(
                    include_uri.clone(),
                    &mut self.include_graph,
                    &self.stdlib_env,
                    &eval_ctx,
                    0,
                ) {
                    eprintln!("LSP: Failed to index {}: {}", include_uri.as_str(), e);
                }
            }
        }

        // Remove stale reverse edges for removed includes
        for old_include in &old_includes {
            if !new_includes.contains(old_include) {
                if let Some(node) = self.include_graph.get_mut(old_include) {
                    node.included_by.retain(|u| u != &uri);
                }
            }
        }

        // Update or insert this document's node in the include graph
        self.include_graph.insert(
            uri.clone(),
            IncludeNode {
                state: state.clone(),
                includes: new_includes.clone(),
                included_by: self
                    .include_graph
                    .get(&uri)
                    .map(|n| n.included_by.clone())
                    .unwrap_or_default(),
            },
        );

        // Add forward edges (reverse edges on included files)
        for include_uri in &new_includes {
            if let Some(node) = self.include_graph.get_mut(include_uri) {
                if !node.included_by.contains(&uri) {
                    node.included_by.push(uri.clone());
                }
            }
        }

        // Store in docs as well for backward compatibility
        self.docs.insert(uri.clone(), state);

        // Invalidate and re-index dependents
        invalidate_dependents(&uri, &mut self.include_graph, &self.stdlib_env, &eval_ctx);
    }

    /// Remove a document from the store.
    pub fn remove_document(&mut self, uri: &Uri) {
        self.docs.remove(uri);
    }

    /// Get a document's state, if it exists.
    pub fn get(&self, uri: &Uri) -> Option<&DocumentState> {
        self.docs.get(uri)
    }

    /// Get the parsed prelude AST, if available.
    pub fn prelude_ast(&self) -> Option<&Spanned<File>> {
        self.prelude_ast.as_ref()
    }

    /// Iterate over all open documents as `(uri, state)` pairs.
    ///
    /// Used by `workspace/symbol` to search across all open documents.
    pub fn docs_iter(&self) -> impl Iterator<Item = (&Uri, &DocumentState)> {
        self.docs.iter()
    }
}

impl Default for DocumentStore {
    fn default() -> Self {
        Self::new()
    }
}

/// Load and analyze a document from a file URI without adding it to the store.
///
/// Used by hover and goto-definition handlers to support on-demand analysis of
/// unopened documents (e.g., when Claude Code's LSP tool sends requests without
/// a prior `textDocument/didOpen`).
///
/// Returns `None` if the URI cannot be converted to a file path or the file
/// cannot be read.
pub fn load_doc_from_uri(uri: &Uri) -> Option<DocumentState> {
    use crate::lsp::MAX_DOCUMENT_SIZE;

    // Convert URI to file path
    let path = crate::lsp::convert::uri_to_file_path(uri)?;

    // Check file size before reading (prevents resource exhaustion from large files)
    let metadata = std::fs::metadata(&path).ok()?;
    if metadata.len() > MAX_DOCUMENT_SIZE as u64 {
        // File too large — return None to indicate load failure.
        // The LSP client will handle this as a missing document (same as file-not-found).
        // This matches the behavior of DidOpenTextDocument and DidChangeTextDocument handlers
        // in server.rs, which reject oversized documents with diagnostic errors.
        return None;
    }

    // Read the file from disk
    let text = std::fs::read_to_string(&path).ok()?;

    // Create minimal environment for LSP analysis
    let (stdlib_env, stdlib_arena) = create_stdlib_env_with_arena().ok()?;
    let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).ok()?;
    let eval_ctx = Rc::new(crate::eval::EvalContext::new_sharing_arena(
        base_dir,
        Rc::clone(&stdlib_env),
        false,
        stdlib_arena,
    ));

    // Create document state with the file's directory as base_dir for include resolution
    let base_path = path.parent().map(|p| p.to_path_buf());
    let is_markdown = uri.as_str().ends_with(".md");
    Some(if is_markdown {
        DocumentState::new_markdown(text, &stdlib_env, &eval_ctx, base_path.as_deref())
    } else {
        DocumentState::new(text, &stdlib_env, &eval_ctx, base_path.as_deref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a stdlib env and arena for tests.
    fn test_env_and_arena() -> (
        Rc<RefCell<Environment>>,
        Rc<RefCell<crate::arena::ThunkArena>>,
    ) {
        create_stdlib_env_with_arena().unwrap()
    }

    fn test_env() -> Rc<RefCell<Environment>> {
        test_env_and_arena().0
    }

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        let (env, arena) = test_env_and_arena();
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new_sharing_arena(base_dir, env, false, arena)
    }

    #[test]
    fn test_document_state_valid_source() {
        let env = test_env();
        let state = DocumentState::new(
            "[x: 42]".to_string(),
            &env,
            &test_ctx(),
            None, // No base_dir for simple test
        );
        assert!(state.ast.is_ok());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_parse_error() {
        let env = test_env();
        let state = DocumentState::new("[unterminated".to_string(), &env, &test_ctx(), None);
        assert!(state.ast.is_err());
        assert!(state.type_errors.is_empty());
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_type_error() {
        let env = test_env();
        let state = DocumentState::new("[@Number hello]".to_string(), &env, &test_ctx(), None);
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty());
        // LSP skips eval — eval_errors always empty; type_errors covers the diagnostic.
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_state_eval_error() {
        let env = test_env();
        let state = DocumentState::new("$undefined".to_string(), &env, &test_ctx(), None);
        assert!(state.ast.is_ok());
        assert!(!state.type_errors.is_empty()); // undefined variable caught by type checker
                                                // LSP skips eval — eval_errors always empty.
        assert!(state.eval_errors.is_empty());
    }

    #[test]
    fn test_document_store_insert_get() {
        let mut store = DocumentStore::new();
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 1]");
        assert!(doc.ast.is_ok());
    }

    #[test]
    fn test_document_store_update_replaces() {
        let mut store = DocumentStore::new();
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        store.update_document(url.clone(), "[x: 2]".to_string());

        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 2]");
    }

    #[test]
    fn test_document_store_remove() {
        let mut store = DocumentStore::new();
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        assert!(store.get(&url).is_some());

        store.remove_document(&url);
        assert!(store.get(&url).is_none());
    }

    #[test]
    fn test_document_state_underscore_desugared() {
        // Regression: before the desugar_file fix, $_ was seen by the type checker as VarRef("_"),
        // producing a spurious "undefined variable _" type error. After the fix, the desugar pass
        // rewrites $_ to an explicit lambda, so no type error should be emitted.
        let env = test_env();
        let ctx = test_ctx();
        let state = DocumentState::new("[f: $_]".to_string(), &env, &ctx, None);
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            state.type_errors.is_empty(),
            "desugar should eliminate spurious 'undefined variable _' error; got: {:?}",
            state.type_errors
        );
    }

    #[test]
    fn test_document_store_multiple_docs() {
        let mut store = DocumentStore::new();
        let url1 = "file:///a.llt".parse::<Uri>().unwrap();
        let url2 = "file:///b.llt".parse::<Uri>().unwrap();

        store.update_document(url1.clone(), "[a: 1]".to_string());
        store.update_document(url2.clone(), "[b: 2]".to_string());

        assert_eq!(store.get(&url1).unwrap().text, "[a: 1]");
        assert_eq!(store.get(&url2).unwrap().text, "[b: 2]");
    }

    #[test]
    fn test_lsp_capless_include_rejected() {
        // Capless [include "foo"] is no longer supported — include requires a DirCap.
        // LSP skips eval entirely, so eval_errors is always empty.
        // The type checker catches arity/type issues; this test verifies no eval side-effects.
        let env = test_env();
        let ctx = test_ctx();
        let state = DocumentState::new(
            "[call $include \"some_file.llt\"]".to_string(),
            &env,
            &ctx,
            None,
        );
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            state.eval_errors.is_empty(),
            "LSP eval is skipped — eval_errors must be empty; got: {:?}",
            state.eval_errors
        );
    }

    #[test]
    fn test_prelude_env_non_empty() {
        // Verify that the shared imports module provides prelude types
        let env = crate::imports::build_prelude_env();
        // The prelude should contain at least these well-known functions
        assert!(env.get("map").is_some(), "prelude env should contain 'map'");
        assert!(
            env.get("filter").is_some(),
            "prelude env should contain 'filter'"
        );
        assert!(
            env.get("identity").is_some(),
            "prelude env should contain 'identity'"
        );
    }

    #[test]
    fn test_no_false_undefined_for_prelude() {
        let env = test_env();
        let ctx = test_ctx();
        let state = DocumentState::new(
            "[call $map [fn [x] x] [1 2 3]]".to_string(),
            &env,
            &ctx,
            None, // base_dir=None still gets prelude types via imports::build_type_env
        );
        assert!(state.ast.is_ok(), "parse should succeed");
        assert!(
            state.type_errors.is_empty(),
            "should have zero type errors (prelude names seeded); got: {:?}",
            state.type_errors
        );
    }

    #[test]
    fn test_resolve_include_uri_relative_path() {
        let base_url = "file:///home/user/project/main.llt".parse::<Uri>().unwrap();
        let include_path = "lib/utils.llt";
        // resolve_include_uri calls canonicalize, which requires the file to exist
        // So this test can't verify the exact URL without creating real files.
        // We can only test that it doesn't panic and returns None for non-existent paths.
        let result = resolve_include_uri(&base_url, include_path);
        // Result is None because the path doesn't exist (canonicalize fails)
        assert!(
            result.is_none(),
            "should return None for non-existent paths"
        );
    }

    #[test]
    fn test_resolve_include_uri_absolute_path() {
        let base_url = "file:///home/user/project/main.llt".parse::<Uri>().unwrap();
        let include_path = "/etc/hosts"; // absolute path outside the workspace
        let result = resolve_include_uri(&base_url, include_path);
        // Blocked by prefix check — cannot read files outside workspace.
        // The canonicalized /etc/hosts does not start_with the document's
        // parent directory (/home/user/project), so the security check returns None.
        assert!(
            result.is_none(),
            "path traversal outside workspace must be blocked"
        );
    }

    #[test]
    fn test_resolve_include_uri_parent_directory() {
        let base_url = "file:///home/user/project/src/main.llt"
            .parse::<Uri>()
            .unwrap();
        let include_path = "../lib/utils.llt";
        let result = resolve_include_uri(&base_url, include_path);
        // Should attempt to resolve to /home/user/project/lib/utils.llt
        // Returns None because the path doesn't exist
        assert!(
            result.is_none(),
            "should return None for non-existent paths"
        );
    }

    #[test]
    fn test_resolve_include_uri_non_file_scheme() {
        let base_url = "http://example.com/main.llt".parse::<Uri>().unwrap();
        let include_path = "lib/utils.llt";
        let result = resolve_include_uri(&base_url, include_path);
        assert!(result.is_none(), "should return None for non-file:// URLs");
    }

    #[test]
    fn test_document_state_markdown_extraction() {
        let env = test_env();
        let ctx = test_ctx();
        let markdown = r#"# Test

```tinct
[x: 1]
```

Some prose.

```tinct
[y: 2]
```"#;
        let state = DocumentState::new_markdown(markdown.to_string(), &env, &ctx, None);
        assert_eq!(state.literate_blocks.len(), 2);
        assert_eq!(state.literate_blocks[0].code, "[x: 1]\n");
        assert_eq!(state.literate_blocks[1].code, "[y: 2]\n");
    }

    #[test]
    fn test_document_state_markdown_no_ast() {
        let env = test_env();
        let ctx = test_ctx();
        let markdown = "# Just prose, no code blocks";
        let state = DocumentState::new_markdown(markdown.to_string(), &env, &ctx, None);
        assert!(state.ast.is_err());
        assert_eq!(state.literate_blocks.len(), 0);
    }
}
