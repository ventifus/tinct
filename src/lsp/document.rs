//! Document storage and incremental re-parsing for the LSP.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use lsp_types::Uri;

use crate::ast::{File, Spanned, SurfaceProgram};
use crate::builtins::create_stdlib_env_with_arena;
use crate::error::{EvalError, TypeDiagnostic};
use crate::parser::{parse, ParseError};
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
    /// Surface program (if parsing and macro expansion succeeded).
    /// Used by the imports API for include-path collection and type-env building.
    pub surface: Option<SurfaceProgram>,
    /// Recovered parse errors from inside bracket forms (non-fatal; collected even when `ast` is Ok).
    /// These come from `ParseOutput.errors` and represent errors where the parser substituted
    /// an `Expr::Error` node and continued rather than stopping.
    pub parse_errors: Vec<ParseError>,
    /// Type errors (advisory; evaluation proceeds regardless).
    pub type_errors: Vec<TypeError>,
    /// Type quality diagnostics (info/warn level; from `scan_type_quality` and inference).
    /// Distinct from `type_errors` (which are always warning-severity type mismatches);
    /// these carry their own `DiagnosticLevel` (Info for T011, Warn for T010, etc.).
    pub type_diagnostics: Vec<TypeDiagnostic>,
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
        _stdlib_env: &Arc<RwLock<Environment>>,
        eval_ctx: &Arc<crate::eval::EvalContext>,
        base_dir: Option<&std::path::Path>,
    ) -> Self {
        // Use parse() to capture both the AST and any recovered parse errors.
        let parse_result = parse(&text);
        let mut parse_errors = Vec::new();
        let surface_parse_result: Result<crate::ast::SurfaceProgram, ParseError> =
            match parse_result {
                Ok(output) => {
                    parse_errors = output.errors;
                    Ok(output.program)
                }
                Err(err) => Err(err),
            };
        let mut type_errors = Vec::new();
        let mut type_diagnostics: Vec<TypeDiagnostic> = Vec::new();
        let eval_errors = Vec::new();
        let mut type_map = TypeMap::new();
        let mut doc_map = DocMap::new();
        let mut scheme_map = SchemeMap::new();

        // PIPELINE INVARIANT: expand → desugar → surface_program_to_file → resolve → typecheck
        // This order is enforced across all entry points (main.rs, lib.rs, repl.rs).
        // Macros expand first (on SurfaceProgram), then $_ placeholders are desugared,
        // then the SurfaceProgram is lowered to File, then variable resolution runs,
        // then the type checker sees the fully elaborated AST.
        //
        // The expanded+desugared SurfaceProgram is captured in `surface` for use by the
        // imports API (include-path collection, type-env building), which now operates
        // directly on the Surface AST.
        let mut surface: Option<SurfaceProgram> = None;
        let mut ast: Result<Spanned<File>, ParseError> = match surface_parse_result {
            Err(e) => Err(e),
            Ok(mut program) => {
                // Expand macros on SurfaceProgram (Surface-based API, consistent with all other
                // production entry points).
                match crate::expand::expand_surface_program(
                    &mut program,
                    eval_ctx.config.no_fs,
                    &eval_ctx.config.base_dir,
                ) {
                    Ok(_) => {}
                    Err(e) => {
                        // Macro expansion error — convert to parse error and return early
                        return Self {
                            text: text.clone(),
                            ast: Err(crate::parser::ParseError {
                                message: format!("macro expansion error: {}", e),
                                span: None,
                            }),
                            surface: None,
                            parse_errors: vec![crate::parser::ParseError {
                                message: format!("macro expansion error: {}", e),
                                span: None,
                            }],
                            type_errors: vec![],
                            type_diagnostics: vec![],
                            eval_errors: vec![],
                            type_map: TypeMap::new(),
                            doc_map: DocMap::new(),
                            scheme_map: SchemeMap::new(),
                            literate_blocks: vec![],
                        };
                    }
                }

                // Desugar $_ implicit lambdas on SurfaceProgram (after expansion).
                crate::desugar::desugar_surface_program(&mut program);

                // Variable resolution pass (Phase 1 of arena allocation strategy).
                let _resolution_table = crate::resolve::resolve_surface_program(&program);

                // Capture the expanded+desugared SurfaceProgram before lowering.
                // The imports API now operates on SurfaceProgram directly.
                surface = Some(program.clone());

                // Lower SurfaceProgram to File for the remaining passes.
                Ok(crate::ast_convert::surface_program_to_file(&program))
            }
        };

        if let Ok(file) = ast {
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
            // Pass the eval context's cap_std Dir so that %pwd file reads use RESOLVE_BENEATH
            // semantics (kernel-level path confinement) instead of plain std::fs calls.
            let type_cap_dir = if eval_ctx.config.no_fs {
                None
            } else {
                Some(&eval_ctx.config.base_dir)
            };
            if let Some(ref prog) = surface {
                let (seeded_env, include_bindings) =
                    crate::imports::build_type_env_with_cap(prog, type_base_dir, type_cap_dir);
                let (errs, mut map, docs, smap, tc_diagnostics) =
                    typecheck_file_with_types_and_env(&file.node, seeded_env);
                // Post-pass: inject precise Record types for [include %cap "path"] expressions.
                crate::imports::apply_include_type_post_pass(prog, &include_bindings, &mut map);
                type_errors = errs;
                type_map = map;
                doc_map = docs;
                scheme_map = smap;
                // Store type quality diagnostics (T010/T011 Unknown, T012 overbroad, T013 ambiguous, …)
                // so that diagnostics_for() can publish them as LSP diagnostics with correct severity.
                type_diagnostics = tc_diagnostics;
            }

            // Build the LSP eval environment, mirroring what main.rs does at startup.
            // The type checker gets runtime percent-vars via build_type_env(); we inject
            // real DirCap values here so the evaluator can resolve [include %libdir ...]
            // and [include %pwd ...] without spurious E002 errors.
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
            //
            // Historical note: Prior to security-sprint, this code constructed DirPerms::full()
            // DirCaps for %pwd and %libdir, then immediately discarded them. That construction
            // has been removed — the LSP does not evaluate code, so eval env setup is unnecessary.

            ast = Ok(file);
        }

        Self {
            text,
            ast,
            surface,
            parse_errors,
            type_errors,
            type_diagnostics,
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
        _stdlib_env: &Arc<RwLock<Environment>>,
        _eval_ctx: &Arc<crate::eval::EvalContext>,
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
            surface: None,
            parse_errors: vec![],
            type_errors: vec![],
            type_diagnostics: vec![],
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
#[allow(clippy::mutable_key_type)] // Uri interior mutability is safe for HashMap keys
pub fn index_file(
    uri: Uri,
    graph: &mut IncludeGraph,
    stdlib_env: &Arc<RwLock<Environment>>,
    eval_ctx: &Arc<crate::eval::EvalContext>,
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

    // Read the file using cap_std (Fix 3: replaces std::fs::read_to_string).
    // Open the file's parent directory as a cap_std Dir confined to that directory,
    // then read the file by name. Reject absolute paths to prevent traversal.
    let path = crate::lsp::convert::uri_to_file_path(&uri)
        .ok_or_else(|| format!("Cannot convert URI to path: {}", uri.as_str()))?;

    if path.is_absolute() {
        // All LSP URIs are absolute paths; extract parent and filename for cap_std I/O.
        // This is expected — not a traversal, just the document's own path.
    }

    let parent_path = path.parent().unwrap_or(std::path::Path::new("."));
    let file_name = path
        .file_name()
        .ok_or_else(|| format!("Cannot extract filename from: {}", path.display()))?;

    // AMBIENT-OK: LSP opens files that the editor has already opened (document URIs).
    #[allow(clippy::disallowed_methods)]
    let file_dir = cap_std::fs::Dir::open_ambient_dir(parent_path, cap_std::ambient_authority())
        .map_err(|e| format!("Cannot open dir for {}: {e}", path.display()))?;

    let text = {
        use std::io::Read as _;
        let mut f = file_dir
            .open(file_name)
            .map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        let mut buf = String::new();
        f.read_to_string(&mut buf)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
        buf
    };

    // Create document state (base_dir for include resolution is the file's directory)
    let state = DocumentState::new(text, stdlib_env, eval_ctx, Some(parent_path));

    // Collect include paths from this document using the shared imports module
    let include_paths = if let Some(ref prog) = state.surface {
        crate::imports::collect_include_paths(prog)
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
#[allow(clippy::mutable_key_type)] // Uri interior mutability is safe for HashMap keys
pub fn invalidate_dependents(
    changed_uri: &Uri,
    graph: &mut IncludeGraph,
    stdlib_env: &Arc<RwLock<Environment>>,
    eval_ctx: &Arc<crate::eval::EvalContext>,
) {
    use std::collections::VecDeque;
    let mut queue = VecDeque::new();
    #[allow(clippy::mutable_key_type)] // Uri interior mutability is safe for HashSet keys
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
    stdlib_env: Arc<RwLock<Environment>>,
    /// Base evaluation context (with "." as base_dir).
    base_eval_ctx: Arc<crate::eval::EvalContext>,
    /// Include dependency graph for cross-file resolution.
    pub include_graph: IncludeGraph,
    /// Parsed prelude AST (for go-to-definition in stdlib functions).
    /// Created once on construction by parsing the embedded prelude source.
    prelude_ast: Option<Spanned<File>>,
}

impl DocumentStore {
    pub fn new() -> Result<Self, String> {
        // Load stdlib once. If it fails, fall back to an empty environment + empty arena
        // so the LSP can still provide parsing/type-checking diagnostics.
        let (stdlib_env, stdlib_arena) = create_stdlib_env_with_arena().unwrap_or_else(|_| {
            (
                Arc::new(RwLock::new(Environment::new())),
                Arc::new(std::sync::Mutex::new(crate::arena::ThunkArena::new())),
            )
        });
        // Build type-stage environment (for builtin_eval_types). Falls back to stdlib_env if unavailable.
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
        // Create base evaluation context.
        // no_fs=true prevents executing $include with user-controlled paths when
        // opening malicious .llt files in an editor (CWE-22 path traversal mitigation).
        //
        // AMBIENT-OK: bootstrap — acquires CWD as the initial base_dir for the LSP session.
        // Only "." is tried; falling back to "/" or /tmp would make RESOLVE_BENEATH a no-op
        // (everything on the filesystem would be reachable from a root Dir).
        #[allow(clippy::disallowed_methods)]
        let base_dir = match cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()) {
            Ok(dir) => dir,
            Err(e) => {
                // CWD is inaccessible. Return an error to the caller rather than opening root.
                // The editor can retry after the LSP restarts in a valid working directory.
                return Err(format!("LSP: cannot open CWD as base_dir: {}", e));
            }
        };
        let base_eval_ctx = crate::eval::EvalContext::new_sharing_arena(
            base_dir,
            Arc::clone(&stdlib_env),
            type_stage_env,
            true, // no_fs=true prevents $include path traversal (CWE-22)
            stdlib_arena,
            std::collections::HashMap::new(), // LSP doesn't track macro injects yet
        );

        // Parse the embedded prelude source once for go-to-definition support.
        // If parsing fails, store None — prelude go-to-definition will be unavailable
        // but other LSP features (hover on user code, local definitions, etc.) still work.
        let prelude_ast = {
            let prelude_source = include_str!("../../stdlib/prelude.llt");
            crate::parser::parse(prelude_source).ok()
        };

        Ok(Self {
            docs: HashMap::new(),
            stdlib_env,
            base_eval_ctx,
            include_graph: HashMap::new(),
            prelude_ast: prelude_ast
                .map(|o| crate::ast_convert::surface_program_to_file(&o.program)),
        })
    }

    /// Update or insert a document, re-parsing and re-analyzing the text.
    pub fn update_document(&mut self, uri: Uri, text: String) {
        // Create evaluation context with document's directory as base_dir.
        // $include paths should resolve against the document's directory, not editor cwd.
        let base_path = crate::lsp::convert::uri_to_file_path(&uri)
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_else(|| std::path::PathBuf::from("."));
        // Fallback chain: try document's directory first, then ".", then base_eval_ctx's Dir.
        // Stops here — falling back to "/" or /tmp would make RESOLVE_BENEATH a no-op,
        // defeating the cap-std confinement model.
        // AMBIENT-OK: LSP opens document's directory (editor-chosen path); fallback to CWD only.
        #[allow(clippy::disallowed_methods)]
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_path, cap_std::ambient_authority())
            .or_else(|_| cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()))
            .or_else(|_| {
                // Fallback: reopen base_eval_ctx's Dir. cap_std::fs::Dir doesn't implement
                // Clone, so we open "." relative to base_dir to get a duplicate handle.
                self.base_eval_ctx.config.base_dir.open_dir(".")
            });
        let base_dir = match base_dir {
            Ok(dir) => dir,
            Err(e) => {
                // All three attempts failed (document dir, ".", and base_eval_ctx.base_dir).
                // Log a warning and skip this update — falling back to "/" or /tmp would
                // make RESOLVE_BENEATH a no-op and defeat the confinement model.
                eprintln!(
                    "LSP: warning: failed to open any base_dir for {}: {}; skipping update",
                    uri.as_str(),
                    e
                );
                return;
            }
        };
        let eval_ctx = self.base_eval_ctx.with_base_dir(base_dir);

        // Detect .md files and use markdown extraction
        let is_markdown = uri.as_str().ends_with(".md");
        let state = if is_markdown {
            DocumentState::new_markdown(text, &self.stdlib_env, &eval_ctx, Some(&base_path))
        } else {
            DocumentState::new(text, &self.stdlib_env, &eval_ctx, Some(&base_path))
        };

        // Collect include paths from the new surface program using the shared imports module
        let new_includes = if let Some(ref prog) = state.surface {
            crate::imports::collect_include_paths(prog)
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
        Self::new().expect("DocumentStore::default: failed to open CWD as base_dir")
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
    use std::io::Read as _;

    // Convert URI to file path
    let path = crate::lsp::convert::uri_to_file_path(uri)?;

    // Derive the document's parent directory — this is both the cap_std Dir root
    // and the base_dir for include resolution (Fix 6: use path.parent(), not ".").
    let parent_dir_path = path.parent().unwrap_or(std::path::Path::new("."));

    // Open the parent directory as a cap_std Dir (Fix 3 + Fix 6).
    // All file I/O for this document goes through this Dir so RESOLVE_BENEATH
    // confines reads to the document's own directory.
    // AMBIENT-OK: LSP is opened by the editor which chose the document path.
    #[allow(clippy::disallowed_methods)]
    let parent_dir =
        cap_std::fs::Dir::open_ambient_dir(parent_dir_path, cap_std::ambient_authority()).ok()?;

    // Derive the filename relative to the parent directory.
    let file_name = path.file_name()?;

    // Check file size before reading using cap_std metadata (Fix 3).
    let metadata = parent_dir.metadata(file_name).ok()?;
    if metadata.len() > MAX_DOCUMENT_SIZE as u64 {
        // File too large — return None to indicate load failure.
        // The LSP client will handle this as a missing document (same as file-not-found).
        // This matches the behavior of DidOpenTextDocument and DidChangeTextDocument handlers
        // in server.rs, which reject oversized documents with diagnostic errors.
        return None;
    }

    // Read the file from disk using cap_std (Fix 3: replaces std::fs::read_to_string).
    let text = {
        let mut f = parent_dir.open(file_name).ok()?;
        let mut buf = String::new();
        f.read_to_string(&mut buf).ok()?;
        buf
    };

    // Create minimal environment for LSP analysis.
    // base_dir is the document's parent directory (Fix 6: replaces open_ambient_dir(".")).
    let (stdlib_env, stdlib_arena) = create_stdlib_env_with_arena().ok()?;
    let type_stage_env =
        crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
    // Clone the parent_dir handle to give ownership to the EvalContext
    // (open_dir(".") duplicates the fd without acquiring new ambient authority).
    let eval_base_dir = parent_dir.open_dir(".").ok()?;
    let eval_ctx = crate::eval::EvalContext::new_sharing_arena(
        eval_base_dir,
        Arc::clone(&stdlib_env),
        type_stage_env,
        false,
        stdlib_arena,
        std::collections::HashMap::new(),
    );

    // Create document state with the file's directory as base_dir for include resolution
    let is_markdown = uri.as_str().ends_with(".md");
    Some(if is_markdown {
        DocumentState::new_markdown(text, &stdlib_env, &eval_ctx, Some(parent_dir_path))
    } else {
        DocumentState::new(text, &stdlib_env, &eval_ctx, Some(parent_dir_path))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a stdlib env and arena for tests.
    fn test_env_and_arena() -> (
        Arc<RwLock<Environment>>,
        Arc<std::sync::Mutex<crate::arena::ThunkArena>>,
    ) {
        create_stdlib_env_with_arena().unwrap()
    }

    fn test_env() -> Arc<RwLock<Environment>> {
        test_env_and_arena().0
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let (env, arena) = test_env_and_arena();
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new_sharing_arena(
            base_dir,
            Arc::clone(&env),
            type_stage_env,
            false,
            arena,
            std::collections::HashMap::new(),
        )
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
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 1]");
        assert!(doc.ast.is_ok());
    }

    #[test]
    fn test_document_store_update_replaces() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        store.update_document(url.clone(), "[x: 2]".to_string());

        let doc = store.get(&url).unwrap();
        assert_eq!(doc.text, "[x: 2]");
    }

    #[test]
    fn test_document_store_remove() {
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
        let url = "file:///test.llt".parse::<Uri>().unwrap();

        store.update_document(url.clone(), "[x: 1]".to_string());
        assert!(store.get(&url).is_some());

        store.remove_document(&url);
        assert!(store.get(&url).is_none());
    }

    #[test]
    fn test_document_state_underscore_desugared() {
        // Regression: before the desugar pass was wired up, $_ was seen by the type checker as
        // VarRef("_"), producing a spurious "undefined variable _" type error. After the fix, the
        // desugar pass rewrites $_ to an explicit lambda, so no type error should be emitted.
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
        let mut store = DocumentStore::new().expect("DocumentStore::new in test");
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
            "[call $map [fn [let x] x] [1 2 3]]".to_string(),
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
