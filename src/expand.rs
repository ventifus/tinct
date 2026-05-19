//! Macro expansion pass for `[defmacro ...]` forms.
//!
//! Runs between parse and desugar: `parse -> expand_macros -> desugar -> typecheck -> eval`
//!
//! The expansion loop:
//! 1. Walk the AST top-down
//! 2. Register `DefMacro` nodes by evaluating their transformer in a fresh context
//! 3. Expand `Call` nodes if the function name matches a registered macro:
//!    - Quote arguments via `ast_to_dict_expr`
//!    - Call the macro transformer with the quoted args
//!    - Convert the result back to AST via `dict_to_ast`
//!    - Replace the Call node with the expansion
//!    - Re-expand the result (fixpoint)
//! 4. Track in-progress expansions to detect infinite recursion
//!
//! ## Hygiene (Flatt 2016 — simplified scope sets)
//!
//! Each macro invocation gets a fresh `ScopeId(u32)`. Variables introduced by the
//! macro body (bindings in fn params, dict keys) are renamed to `name:scope:N` where
//! N is the scope ID. This ensures macro-introduced names are structurally distinct
//! from user-code names (`:` is forbidden in bare-word identifiers).
//!
//! This is a simplification of Flatt's full biggest-subset binding resolution rule,
//! sufficient for non-recursive macros. Call-site variables pass through unchanged;
//! only bindings introduced *by the macro template itself* get scope-qualified.
//!
//! ## Dual-span provenance (Pombrio & Krishnamurthi 2015)
//!
//! The expander maintains a side map from generated AST node spans to their expansion
//! provenance: `(macro_name, call_site_span)`. Error messages use this to show
//! "in expansion of `<name>` at line N".

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::ast::{Document, Entry, Expr, File, MatchArm, NamedArg, Param, Span, Spanned};
use crate::ast_dict::{ast_to_dict_expr, dict_to_ast, AstToDictOpts};
use crate::builtins;
use crate::error::{EvalError, EvalResult};
use crate::eval::{self, EvalContext};
use crate::value::{Environment, Key, Thunk, Value};

/// Global scope ID counter — monotonically increasing across all expansions.
/// Each macro invocation gets a fresh scope ID.
static SCOPE_COUNTER: AtomicU32 = AtomicU32::new(1);

/// A scope identifier assigned to each macro invocation.
/// Scope 0 is reserved for user code (never assigned to a macro).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

impl ScopeId {
    /// The user-code scope — variables written by the user, not introduced by macros.
    pub const USER: ScopeId = ScopeId(0);

    /// Allocate a fresh scope ID for a new macro invocation.
    fn fresh() -> Self {
        ScopeId(SCOPE_COUNTER.fetch_add(1, Ordering::SeqCst))
    }
}

/// Provenance information for a macro-generated AST node.
/// Used for dual-span error reporting per Pombrio & Krishnamurthi (2015).
#[derive(Debug, Clone)]
pub struct MacroProvenance {
    /// Name of the macro that generated this node.
    pub macro_name: String,
    /// Span of the macro call site (where `[macro-name ...]` appeared in source).
    pub call_site_span: Span,
}

/// Side map from AST node spans to their macro expansion provenance.
/// Keyed by the span of the generated node (which is the call_span of the expansion).
/// This is a simplified version of Pombrio & Krishnamurthi's "honest tags".
pub type ProvenanceMap = HashMap<SpanKey, MacroProvenance>;

/// A hashable key derived from a Span for use in the provenance map.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SpanKey {
    pub start_offset: usize,
    pub end_offset: usize,
    pub start_line: usize,
}

impl From<Span> for SpanKey {
    fn from(span: Span) -> Self {
        SpanKey {
            start_offset: span.start.offset,
            end_offset: span.end.offset,
            start_line: span.start.line,
        }
    }
}

/// Metadata for a registered macro — the transformer function, params pattern, and inject default.
#[derive(Debug, Clone)]
struct MacroMetadata {
    /// The transformer function (as a Value::Function thunk).
    transformer: Rc<Thunk>,
    /// The params pattern (LetDecl) for binding arguments.
    params: Spanned<Expr>,
    /// Optional inject: default name for anaphoric macros.
    inject_default: Option<String>,
    /// True for `[macro ...]` (macros-v2 pattern-matching style),
    /// false for `[defmacro ...]` (old single-args-dict style).
    new_style: bool,
}

/// Macro expansion context — tracks registered macros and prevents infinite expansion.
#[derive(Debug, Clone)]
pub struct MacroEnv {
    /// Map from macro name to its metadata (transformer, params, inject default).
    macros: HashMap<String, MacroMetadata>,
    /// Expansion depth counter — prevents deeply nested expansions.
    depth: usize,
    /// Total node count expanded — prevents runaway macro generation.
    node_count: usize,
    /// In-progress call sites: (file_id, byte_offset) or synthetic ID for generated nodes.
    /// Used for blackhole detection.
    in_progress: HashSet<CallSiteId>,
    /// Provenance side map: generated-node span -> expansion origin.
    pub provenance: ProvenanceMap,
    /// Macros discovered during expansion via `[defmacro ...]` declarations.
    /// Accumulated during expansion, returned in ExpandResult.
    pub discovered_macros: Vec<(String, Rc<Thunk>)>,
}

/// Unique identifier for a macro call site.
/// Source nodes use (file_id, byte_offset); generated nodes use a synthetic counter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum CallSiteId {
    Source { file_id: usize, offset: usize },
    Synthetic(u64),
}

/// Synthetic node ID counter (for macro-generated code with no source span).
static SYNTHETIC_COUNTER: AtomicU64 = AtomicU64::new(0);

fn next_synthetic_id() -> u64 {
    SYNTHETIC_COUNTER.fetch_add(1, Ordering::SeqCst)
}

impl MacroEnv {
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
            depth: 0,
            node_count: 0,
            in_progress: HashSet::new(),
            provenance: HashMap::new(),
            discovered_macros: Vec::new(),
        }
    }

    /// Register a macro transformer with params pattern and inject default.
    /// Returns an error if the name collides with a registered Rust builtin.
    fn register_macro(
        &mut self,
        name: String,
        transformer: Rc<Thunk>,
        params: Spanned<Expr>,
        inject_default: Option<String>,
        new_style: bool,
        span: Span,
    ) -> EvalResult<()> {
        // Check if the name collides with a registered builtin
        let builtin_names: HashSet<String> = builtins::standard_builtins()
            .iter()
            .map(|def| def.name.to_string())
            .collect();

        if builtin_names.contains(&name) {
            return Err(EvalError::user_error(
                format!(
                    "macro name '{}' collides with registered builtin — macros cannot shadow builtins",
                    name
                ),
                span,
            )
            .into());
        }
        self.macros.insert(
            name,
            MacroMetadata {
                transformer,
                params,
                inject_default,
                new_style,
            },
        );
        Ok(())
    }

    /// Check if a name is a registered macro.
    fn is_macro(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Get the metadata for a macro.
    fn get_macro(&self, name: &str) -> Option<&MacroMetadata> {
        self.macros.get(name)
    }

    /// Enter a macro expansion (depth check).
    fn enter_expansion(&mut self, call_site: CallSiteId, span: Span) -> EvalResult<()> {
        const MAX_DEPTH: usize = 100;
        const MAX_NODE_COUNT: usize = 100_000;

        if self.depth >= MAX_DEPTH {
            return Err(EvalError::user_error(
                format!(
                    "macro expansion depth limit exceeded ({} levels)",
                    MAX_DEPTH
                ),
                span,
            )
            .into());
        }

        if self.node_count >= MAX_NODE_COUNT {
            return Err(EvalError::user_error(
                format!(
                    "macro expansion node count limit exceeded ({} nodes)",
                    MAX_NODE_COUNT
                ),
                span,
            )
            .into());
        }

        if self.in_progress.contains(&call_site) {
            return Err(EvalError::user_error(
                "recursive macro expansion detected (macro expanding itself)".to_string(),
                span,
            )
            .into());
        }

        self.depth += 1;
        self.node_count += 1;
        self.in_progress.insert(call_site);
        Ok(())
    }

    /// Leave a macro expansion.
    fn leave_expansion(&mut self, call_site: CallSiteId) {
        self.depth -= 1;
        self.in_progress.remove(&call_site);
    }

    /// Extract the macro inject defaults map for runtime access.
    /// Returns a HashMap of macro_name -> inject_default_name for all macros
    /// that have an `inject:` declaration. Macros without inject: are omitted.
    fn get_inject_map(&self) -> HashMap<String, String> {
        self.macros
            .iter()
            .filter_map(|(name, meta)| {
                meta.inject_default
                    .as_ref()
                    .map(|inject| (name.clone(), inject.clone()))
            })
            .collect()
    }
}

/// Result of macro expansion: the expanded AST plus provenance metadata.
pub struct ExpandResult {
    pub file: Spanned<File>,
    pub provenance: ProvenanceMap,
    /// Macros discovered during expansion via `[defmacro ...]` declarations.
    /// Each entry is `(macro_name, transformer_thunk)`.
    /// Used to propagate stdlib macros from macros.llt to user code expansion.
    pub discovered_macros: Vec<(String, Rc<Thunk>)>,
    /// Macro inject defaults: `macro_name -> inject_default_name`.
    /// Populated from all macros with `inject:` declarations encountered during expansion.
    /// Used by the `macro-injects` builtin for runtime introspection.
    pub macro_injects_map: HashMap<String, String>,
}

/// Register stdlib macros by looking up transformer functions in the stdlib environment.
///
/// Stdlib macros are defined in `stdlib/macros.llt` as `[defmacro name [args] body]`
/// declarations. However, we can't use the normal DefMacro expansion mechanism for
/// stdlib macros because:
/// 1. create_stdlib_env() loads macros.llt BEFORE expand_macros runs on user code
/// 2. The DefMacro mechanism requires expand_macros to be running
///
/// Instead, stdlib/macros.llt exports transformer functions as normal dict bindings,
/// and we register them here by looking them up by name.
fn register_stdlib_macros_from_env(
    env_macro: &mut MacroEnv,
    stdlib_env: &Rc<RefCell<Environment>>,
    span: Span,
) {
    // Known stdlib macros and their transformer function names.
    // Future: could auto-discover by scanning for functions with a special naming pattern.
    const STDLIB_MACROS: &[&str] = &["tmpl", "do", "begin"];

    for macro_name in STDLIB_MACROS {
        let transformer_thunk = {
            let env_ref = stdlib_env.borrow();
            env_ref.get(macro_name)
        };
        if let Some(transformer) = transformer_thunk {
            // Old-style defmacro — uses single "args" parameter, no pattern matching
            let dummy_params = Spanned::new(
                Expr::LetDecl {
                    bindings: vec![Spanned::new(Expr::var_ref("args".to_string()), span)],
                },
                span,
            );
            // register_macro checks for builtin collisions.
            let _ = env_macro.register_macro(
                (*macro_name).to_string(),
                transformer,
                dummy_params,
                None,
                false, // old-style: single args dict
                span,
            );
        }
    }
}

// Reentrance depth guard for expand_macros → create_stdlib_env calls.
// When depth > 0, we're in a re-entrant call and must use create_root_env
// to avoid infinite recursion through the stdlib loading path.
std::thread_local! {
    static EXPAND_MACROS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static EXPAND_EXPR_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Expand all macros in a File AST.
///
/// This is the top-level entry point called from the pipeline.
/// Takes a no_fs flag to match the pipeline configuration.
/// `base_dir` is the already-open capability directory for the file being expanded;
/// it is cloned (via `open_dir(".")`) so no new ambient authority is acquired here.
/// Returns the expanded AST and the provenance side map for dual-span error reporting.
/// Stdlib macros are loaded by expanding `stdlib/macros.llt` and collecting discovered macros.
pub fn expand_macros(
    file: Spanned<File>,
    no_fs: bool,
    base_dir: &cap_std::fs::Dir,
) -> EvalResult<ExpandResult> {
    // Detect infinite recursion
    let em_depth = EXPAND_MACROS_DEPTH.get();
    if em_depth > 10 {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "expand_macros: infinite recursion detected (depth={})",
                em_depth
            ),
            file.span,
        )
        .into());
    }

    let mut env_macro = MacroEnv::new();

    // Clone the caller's already-open Dir handle (open_dir(".") duplicates the fd
    // without acquiring new ambient authority).
    let base_dir = base_dir.open_dir(".").map_err(|e| {
        EvalError::internal(
            format!("cannot clone base directory for macro expansion: {e}"),
            file.span,
        )
    })?;

    // Create the stdlib env for macro expansion. Provides prelude functions for [defmacro]
    // transformer bodies.
    // The depth guard prevents infinite recursion when create_stdlib_env calls expand_macros:
    //   expand_macros(user_code) → create_stdlib_env() → typecheck calls expand_macros(prelude.llt) →
    //   use create_root_env to break the cycle
    let depth = EXPAND_MACROS_DEPTH.get();
    let (stdlib_env, ctx) = if depth == 0 {
        // Only call create_stdlib_env at depth 0 (top-level user code).
        // Increment depth to prevent re-entrance.
        EXPAND_MACROS_DEPTH.set(depth + 1);
        let result = match builtins::create_stdlib_env_with_arena() {
            Ok((env, arena)) => {
                // Load stdlib macros from the fully-evaluated stdlib env.
                // The stdlib defines macros via regular function exports that we
                // register by looking them up by name after the stdlib is loaded.
                register_stdlib_macros_from_env(&mut env_macro, &env, file.span);
                // Share the stdlib arena so ThunkIds from prelude dicts (e.g., `result.bind`)
                // remain valid when transformer functions access them during expansion.
                let ctx = EvalContext::new_sharing_arena(
                    base_dir,
                    Rc::clone(&env),
                    no_fs,
                    arena,
                    HashMap::new(), // No macros registered yet during initial expansion
                );
                Ok((env, ctx))
            }
            Err(e) => Err(EvalError::internal(
                format!("cannot create stdlib env for macro expansion: {e}"),
                file.span,
            )),
        };
        // Reset depth after create_stdlib_env completes
        EXPAND_MACROS_DEPTH.set(depth);
        result?
    } else {
        // Re-entrant call (depth > 0): use bare root env to break the cycle.
        // This happens when create_stdlib_env → build_prelude_env → expand_macros(prelude.llt).
        // The stdlib files don't use [defmacro], so not having stdlib macros registered is fine.
        // Root env has no prelude dicts — no ThunkId cross-context accesses occur here.
        // Use new_empty() to bypass STDLIB_ARENA_CACHE — we're in the middle of building
        // stdlib, so we need a fresh arena, not one seeded with potentially stale cache contents.
        let env = builtins::create_root_env();
        let ctx = EvalContext::new_empty(base_dir, Rc::clone(&env), no_fs);
        (env, ctx)
    };
    let ctx = Rc::new(ctx);

    // Pre-scan: collect MacroDecl and SyntaxClass nodes before main expansion
    pre_scan_file(&file.node, &mut env_macro, &ctx, &stdlib_env)?;

    // Process each document in the file
    let expanded_documents = file
        .node
        .documents
        .into_iter()
        .map(|doc| {
            let expanded_doc = expand_document(doc.node, &mut env_macro, &ctx, &stdlib_env)?;
            Ok(Spanned::new(expanded_doc, doc.span))
        })
        .collect::<EvalResult<Vec<_>>>()?;

    let macro_injects_map = env_macro.get_inject_map();
    Ok(ExpandResult {
        file: Spanned::new(
            File {
                documents: expanded_documents,
            },
            file.span,
        ),
        provenance: env_macro.provenance,
        discovered_macros: env_macro.discovered_macros,
        macro_injects_map,
    })
}

/// Pre-scan a File AST to collect MacroDecl and SyntaxClass nodes.
///
/// This runs BEFORE the main expansion pass and registers all macro and syntax-class
/// declarations so they're available during expansion. For bare string-literal includes,
/// follows the include and scans the included file too. For computed-path includes
/// (non-string-literal), skips them (macro declarations in computed includes would be
/// an expansion error).
fn pre_scan_file(
    file: &File,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<()> {
    for doc in &file.documents {
        pre_scan_document(&doc.node, env, ctx, stdlib_env)?;
    }
    Ok(())
}

/// Pre-scan a Document to collect MacroDecl and SyntaxClass nodes.
fn pre_scan_document(
    doc: &Document,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<()> {
    for expr in &doc.expressions {
        pre_scan_expr(expr, env, ctx, stdlib_env)?;
    }
    Ok(())
}

/// Pre-scan an expression to collect MacroDecl and SyntaxClass nodes.
///
/// When it finds:
/// - `Expr::MacroDecl`: evaluates the transformer and registers the macro
/// - `Expr::SyntaxClass`: registers the syntax class (not yet implemented)
/// - `Expr::Call` to `include` with a bare string literal path: follows the include
///   and scans the included file recursively
fn pre_scan_expr(
    expr: &Rc<Spanned<Expr>>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<()> {
    match &expr.node {
        Expr::MacroDecl { name, params, body } => {
            // Extract inject: default from params if present
            let inject_default = extract_inject_default(params);

            // Convert LetDecl bindings to Vec<Spanned<Param>> for function creation.
            // LetDecl bindings can be:
            //   VarRef { name }         → Param { name, annotation: None, variadic: false }
            //   Annotated { name, ann } → Param { name, annotation: Some(ann), variadic: false }
            //   Rest(Some(name))        → Param { name, annotation: None, variadic: true }
            //   Rest(None)              → skip (anonymous spread)
            let fn_params: Vec<Spanned<crate::ast::Param>> = match &params.node {
                Expr::LetDecl { bindings } => {
                    let mut fn_params = Vec::with_capacity(bindings.len());
                    for binding in bindings {
                        match &binding.node {
                            Expr::VarRef { name: n, .. } => {
                                fn_params.push(Spanned::new(
                                    crate::ast::Param {
                                        name: n.clone(),
                                        annotation: None,
                                        variadic: false,
                                    },
                                    binding.span,
                                ));
                            }
                            Expr::Annotated {
                                name: n,
                                annotation,
                            } => {
                                fn_params.push(Spanned::new(
                                    crate::ast::Param {
                                        name: n.clone(),
                                        annotation: Some(annotation.clone()),
                                        variadic: false,
                                    },
                                    binding.span,
                                ));
                            }
                            Expr::Rest(Some(n)) => {
                                fn_params.push(Spanned::new(
                                    crate::ast::Param {
                                        name: n.clone(),
                                        annotation: None,
                                        variadic: true,
                                    },
                                    binding.span,
                                ));
                            }
                            Expr::Rest(None) => {
                                // Anonymous spread — no binding, skip
                            }
                            _ => {
                                // Other binding forms not yet supported; skip
                            }
                        }
                    }
                    fn_params
                }
                _ => {
                    // Params is not a LetDecl — treat as single arg named "args"
                    vec![Spanned::new(
                        crate::ast::Param {
                            name: "args".to_string(),
                            annotation: None,
                            variadic: false,
                        },
                        params.span,
                    )]
                }
            };

            // Wrap params+body in a function expression and evaluate to get Value::Function.
            // This is identical to the DefMacro path, but using derived params.
            // If the macro has inject:, add an implicit `binding` param as well.
            let mut all_fn_params = fn_params;
            if inject_default.is_some() {
                all_fn_params.push(Spanned::new(
                    crate::ast::Param {
                        name: "binding".to_string(),
                        annotation: None,
                        variadic: false,
                    },
                    params.span,
                ));
            }

            let fn_expr = Expr::Fn {
                return_ann: None,
                params: all_fn_params,
                body: Rc::new(body.as_ref().clone()),
                desugared: false,
            };
            let fn_spanned = Spanned::new(fn_expr, expr.span);
            let transformer_value = eval::eval(Rc::new(fn_spanned), Rc::clone(stdlib_env), ctx)?;

            // Register the macro with its params pattern and inject default (new-style)
            env.register_macro(
                name.clone(),
                Rc::clone(&transformer_value),
                params.as_ref().clone(),
                inject_default,
                true, // new-style: individual positional args per LetDecl binding
                expr.span,
            )?;

            // Record the discovered macro for propagation
            env.discovered_macros
                .push((name.clone(), transformer_value));

            Ok(())
        }

        Expr::SyntaxClass { .. } => {
            // TODO: register syntax class when macros-v2 syntax class validation is implemented
            Ok(())
        }

        // Follow bare string-literal includes
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            // Check if this is an include call with a bare string literal
            if let Expr::VarRef { name, .. } = &func.node {
                if name == "include" && args.len() == 1 && named_args.is_empty() {
                    if let Expr::Str(_path_str) = &args[0].node {
                        // This is a bare string-literal include — follow it
                        // Use the builtin include mechanism to load and parse the file
                        // TODO: implement include following for pre-scan
                        // For now, skip following includes to avoid complexity
                        // This is a known limitation: macros in included files won't be
                        // available during pre-scan
                    }
                }
            }

            // Recursively scan children
            pre_scan_expr_boxed(func, env, ctx, stdlib_env)?;
            for arg in args {
                pre_scan_expr(arg, env, ctx, stdlib_env)?;
            }
            for named_arg in named_args {
                pre_scan_expr(&named_arg.node.value, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        // Recursively scan other expression types
        Expr::DotAccess { expr: target, .. } => pre_scan_expr_boxed(target, env, ctx, stdlib_env),

        Expr::Pipe { lhs, rhs } => {
            pre_scan_expr_boxed(lhs, env, ctx, stdlib_env)?;
            pre_scan_expr_boxed(rhs, env, ctx, stdlib_env)
        }

        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                pre_scan_expr(seq_expr, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    pre_scan_expr_spanned(key, env, ctx, stdlib_env)?;
                }
                pre_scan_expr(&entry.node.value, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::Fn { body, .. } => pre_scan_expr(body, env, ctx, stdlib_env),

        Expr::TypeAlias { body, .. } => pre_scan_expr_boxed(body, env, ctx, stdlib_env),

        Expr::TypeAssert { expr: asserted, .. } => {
            pre_scan_expr_boxed(asserted, env, ctx, stdlib_env)
        }

        Expr::Quote(quoted) => pre_scan_expr_boxed(quoted, env, ctx, stdlib_env),

        Expr::Unquote(unquoted) => pre_scan_expr_boxed(unquoted, env, ctx, stdlib_env),

        Expr::UnquoteSplice(spliced) => pre_scan_expr_boxed(spliced, env, ctx, stdlib_env),

        Expr::Match { scrutinee, arms } => {
            pre_scan_expr_boxed(scrutinee, env, ctx, stdlib_env)?;
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    pre_scan_expr_boxed(guard, env, ctx, stdlib_env)?;
                }
                pre_scan_expr_boxed(&arm.body, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::ClassDecl { methods, .. } => {
            for method in methods {
                pre_scan_expr(&method.node.value, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::InstanceDecl { arms, .. } => {
            for (pattern_expr, methods) in arms {
                pre_scan_expr_spanned(pattern_expr, env, ctx, stdlib_env)?;
                for method in methods {
                    pre_scan_expr(&method.node.value, env, ctx, stdlib_env)?;
                }
            }
            Ok(())
        }

        Expr::PatternDecl { bindings } => {
            for binding in bindings {
                pre_scan_expr_spanned(binding, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::LetDecl { bindings } => {
            for binding in bindings {
                pre_scan_expr_spanned(binding, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        Expr::CaseArm { pattern, body } => {
            pre_scan_expr_boxed(pattern, env, ctx, stdlib_env)?;
            pre_scan_expr_boxed(body, env, ctx, stdlib_env)
        }

        // DefMacro: also register (for backwards compatibility with existing defmacro syntax)
        Expr::DefMacro { name, params, body } => {
            // Convert [let ...] pattern to Vec<Spanned<Param>> for Fn evaluation
            // Extract bindings from the LetDecl pattern
            let param_vec = if let Expr::LetDecl { bindings } = &params.node {
                bindings
                    .iter()
                    .map(|binding| {
                        match &binding.node {
                            Expr::VarRef { name, .. } => Spanned::new(
                                Param {
                                    name: name.clone(),
                                    annotation: None,
                                    variadic: false,
                                },
                                binding.span,
                            ),
                            Expr::Annotated { name, annotation } => Spanned::new(
                                Param {
                                    name: name.clone(),
                                    annotation: Some(annotation.clone()),
                                    variadic: false,
                                },
                                binding.span,
                            ),
                            Expr::Rest(Some(rest_name)) => Spanned::new(
                                Param {
                                    name: rest_name.clone(),
                                    annotation: None,
                                    variadic: true,
                                },
                                binding.span,
                            ),
                            _ => {
                                // Error case — invalid param; will be caught by type checker
                                Spanned::new(
                                    Param {
                                        name: "???".to_string(),
                                        annotation: None,
                                        variadic: false,
                                    },
                                    binding.span,
                                )
                            }
                        }
                    })
                    .collect()
            } else {
                // Error case — params should be LetDecl; will be caught by type checker
                vec![]
            };

            // Wrap params+body in a function expression
            let fn_expr = Expr::Fn {
                return_ann: None,
                params: param_vec,
                body: Rc::clone(body),
                desugared: false,
            };
            let fn_spanned = Spanned::new(fn_expr, expr.span);

            // Evaluate the function in the stdlib environment
            let transformer_value = eval::eval(Rc::new(fn_spanned), Rc::clone(stdlib_env), ctx)?;

            // Register the macro (params_pattern is the LetDecl directly)
            env.register_macro(
                name.clone(),
                Rc::clone(&transformer_value),
                (**params).clone(),
                None,
                false, // old-style: single args dict
                expr.span,
            )?;

            // Record the discovered macro for propagation
            env.discovered_macros
                .push((name.clone(), transformer_value));

            Ok(())
        }

        Expr::Splice(forms) => {
            for form in forms {
                pre_scan_expr_spanned(form, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        // Leaf nodes — no scanning needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Annotated { .. }
        | Expr::Rest(_)
        | Expr::Placeholder
        | Expr::TypeApp { .. }
        | Expr::Error(_) => Ok(()),
    }
}

/// Helper for scanning Box<Spanned<Expr>>
fn pre_scan_expr_boxed(
    expr: &Box<Spanned<Expr>>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<()> {
    pre_scan_expr(&Rc::new(expr.as_ref().clone()), env, ctx, stdlib_env)
}

/// Helper for scanning Spanned<Expr>
fn pre_scan_expr_spanned(
    expr: &Spanned<Expr>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<()> {
    pre_scan_expr(&Rc::new(expr.clone()), env, ctx, stdlib_env)
}

/// Extract the inject: default name from a MacroDecl params Let node.
///
/// The inject: key appears as a KeyedEntry in the Let bindings. We look for
/// a binding with key "inject" and extract its value (which should be a bare
/// identifier VarRef node).
///
/// Returns Some(name) if inject: is found, None otherwise.
fn extract_inject_default(params: &Spanned<Expr>) -> Option<String> {
    match &params.node {
        Expr::LetDecl { bindings } => {
            for binding in bindings {
                // Check if this binding is a dict entry with key "inject"
                if let Expr::Dict(entries) = &binding.node {
                    for entry in entries {
                        if let Some(key_expr) = &entry.node.key {
                            if let Expr::Str(key_str) = &key_expr.node {
                                if key_str == "inject" {
                                    // Found inject: key — extract the value
                                    if let Expr::VarRef { name, .. } = &entry.node.value.node {
                                        return Some(name.clone());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            None
        }
        _ => None,
    }
}

/// Expand macros in a Document.
///
/// Splice handling: when a macro call returns `Expr::Splice(forms)`, each form is injected
/// as a separate document expression. Any MacroDecl or SyntaxClass in splice output is
/// registered immediately (enabling meta-macros) before processing subsequent forms.
fn expand_document(
    doc: Document,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Document> {
    let mut expanded_exprs = Vec::new();

    for expr in doc.expressions {
        let expanded = expand_expr(expr.as_ref().clone(), env, ctx, stdlib_env)?;
        // Filter out declaration nodes (they've been registered and must not appear post-expansion)
        if matches!(
            expanded.node,
            Expr::DefMacro { .. } | Expr::MacroDecl { .. } | Expr::SyntaxClass { .. }
        ) {
            continue;
        }

        // Splice: inject each form as a separate expression.
        // Register any MacroDecl/SyntaxClass in splice output immediately so subsequent
        // forms in the same document can use the newly registered macros.
        if let Expr::Splice(forms) = expanded.node {
            for form in forms {
                match &form.node {
                    Expr::MacroDecl { .. } | Expr::SyntaxClass { .. } => {
                        // Register the new macro/syntax-class immediately
                        pre_scan_expr_spanned(&form, env, ctx, stdlib_env)?;
                        // Do not emit the declaration into the output
                    }
                    Expr::DefMacro { .. } => {
                        pre_scan_expr_spanned(&form, env, ctx, stdlib_env)?;
                    }
                    _ => {
                        // Re-expand the spliced form (fixpoint)
                        let re_expanded = expand_expr(form.clone(), env, ctx, stdlib_env)?;
                        if !matches!(
                            re_expanded.node,
                            Expr::DefMacro { .. }
                                | Expr::MacroDecl { .. }
                                | Expr::SyntaxClass { .. }
                        ) {
                            expanded_exprs.push(Rc::new(re_expanded));
                        }
                    }
                }
            }
        } else {
            expanded_exprs.push(Rc::new(expanded));
        }
    }

    Ok(Document {
        expressions: expanded_exprs,
        name: doc.name,
        output_type: doc.output_type,
        expects: doc.expects,
        caps: doc.caps,
        stage: doc.stage,
    })
}

/// Expand macros in an expression (fixpoint loop).
fn expand_expr(
    expr: Spanned<Expr>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Spanned<Expr>> {
    let ee_depth = EXPAND_EXPR_DEPTH.get();
    if ee_depth > 10_000 {
        return Err(EvalError::resource_limit_exceeded(
            format!("macro expansion: AST recursion depth {ee_depth} exceeds limit (10000)"),
            expr.span,
        )
        .into());
    }
    EXPAND_EXPR_DEPTH.set(ee_depth + 1);
    let result = expand_expr_inner(expr, env, ctx, stdlib_env);
    EXPAND_EXPR_DEPTH.set(ee_depth);
    result
}

fn expand_expr_inner(
    expr: Spanned<Expr>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Spanned<Expr>> {
    match &expr.node {
        // DefMacro, MacroDecl, Splice, and SyntaxClass are already handled by pre_scan_file
        // Just return them unchanged (will be filtered out by expand_document)
        Expr::DefMacro { .. }
        | Expr::MacroDecl { .. }
        | Expr::Splice(..)
        | Expr::SyntaxClass { .. } => Ok(expr),

        Expr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // Check if this is a macro call (func is a VarRef to a registered macro)
            let macro_name = if let Expr::VarRef { name, .. } = &func.node {
                if env.is_macro(name) {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(macro_name) = macro_name {
                // This is a macro call — expand it (in expression position, no dict key)
                let expanded = expand_macro_call(
                    &macro_name,
                    args,
                    named_args,
                    expr.span,
                    None, // expression position — no dict key
                    env,
                    ctx,
                    stdlib_env,
                )?;
                // Task 2: Splice in expression position is an expansion-time error.
                if matches!(expanded.node, Expr::Splice(_)) {
                    return Err(EvalError::macro_error(
                        format!(
                            "macro '{}' returned splice, which is not valid in expression position",
                            macro_name
                        ),
                        expr.span,
                    )
                    .into());
                }
                Ok(expanded)
            } else {
                // Not a macro call — recursively expand children
                let expanded_func = expand_expr(func.as_ref().clone(), env, ctx, stdlib_env)?;
                let expanded_args = args
                    .iter()
                    .map(|arg| {
                        let expanded = expand_expr(arg.as_ref().clone(), env, ctx, stdlib_env)?;
                        Ok(Rc::new(expanded))
                    })
                    .collect::<EvalResult<Vec<_>>>()?;
                let expanded_named_args = named_args
                    .iter()
                    .map(|named_arg| {
                        let expanded_value = expand_expr(
                            named_arg.node.value.as_ref().clone(),
                            env,
                            ctx,
                            stdlib_env,
                        )?;
                        Ok(Spanned::new(
                            NamedArg {
                                name: named_arg.node.name.clone(),
                                value: Rc::new(expanded_value),
                            },
                            named_arg.span,
                        ))
                    })
                    .collect::<EvalResult<Vec<_>>>()?;

                Ok(Spanned::new(
                    Expr::Call {
                        func: Box::new(expanded_func),
                        args: expanded_args,
                        named_args: expanded_named_args,
                        implied: *implied,
                    },
                    expr.span,
                ))
            }
        }

        // Recursively expand other expression types
        Expr::DotAccess {
            expr: target,
            field,
        } => {
            let expanded_target = expand_expr(target.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::DotAccess {
                    expr: Box::new(expanded_target),
                    field: field.clone(),
                },
                expr.span,
            ))
        }

        Expr::Pipe { lhs, rhs } => {
            let expanded_lhs = expand_expr(lhs.as_ref().clone(), env, ctx, stdlib_env)?;
            let expanded_rhs = expand_expr(rhs.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Pipe {
                    lhs: Box::new(expanded_lhs),
                    rhs: Box::new(expanded_rhs),
                },
                expr.span,
            ))
        }

        Expr::Sequential(exprs) => {
            let mut expanded_exprs = Vec::new();
            for seq_expr in exprs {
                let expanded = expand_expr(seq_expr.as_ref().clone(), env, ctx, stdlib_env)?;
                expanded_exprs.push(Rc::new(expanded));
            }
            Ok(Spanned::new(Expr::Sequential(expanded_exprs), expr.span))
        }

        Expr::Dict(entries) => {
            let mut expanded_entries = Vec::new();
            for entry in entries {
                // Task 4: extract the dict key string for inject: threading.
                // The key is available if this is a string/bareword key expression.
                let dict_key_str: Option<String> =
                    entry.node.key.as_ref().and_then(|k| match &k.node {
                        Expr::Str(s) => Some(s.clone()),
                        Expr::VarRef { name, .. } => Some(name.clone()),
                        _ => None,
                    });

                // Task 4: if the entry value is a macro call, expand with the dict key.
                // This is necessary for inject: threading — the macro needs to know its key.
                let expanded_value = {
                    let value_expr = entry.node.value.as_ref();
                    if let Expr::Call {
                        func,
                        args,
                        named_args,
                        ..
                    } = &value_expr.node
                    {
                        if let Expr::VarRef { name, .. } = &func.node {
                            if env.is_macro(name) {
                                let macro_name_clone = name.clone();
                                let expanded = expand_macro_call(
                                    &macro_name_clone,
                                    args,
                                    named_args,
                                    value_expr.span,
                                    dict_key_str.as_deref(), // Task 4: pass key
                                    env,
                                    ctx,
                                    stdlib_env,
                                )?;
                                expanded
                            } else {
                                expand_expr(value_expr.clone(), env, ctx, stdlib_env)?
                            }
                        } else {
                            expand_expr(value_expr.clone(), env, ctx, stdlib_env)?
                        }
                    } else {
                        expand_expr(value_expr.clone(), env, ctx, stdlib_env)?
                    }
                };

                // Filter out declaration nodes — they've been registered and must not appear post-expansion.
                if matches!(
                    expanded_value.node,
                    Expr::DefMacro { .. } | Expr::MacroDecl { .. } | Expr::SyntaxClass { .. }
                ) {
                    continue;
                }

                // Splice in dict context: inject each spliced form as a separate unkeyed entry.
                // Any MacroDecl/SyntaxClass in the splice output is registered immediately.
                if let Expr::Splice(forms) = expanded_value.node {
                    for form in forms {
                        match &form.node {
                            Expr::MacroDecl { .. }
                            | Expr::SyntaxClass { .. }
                            | Expr::DefMacro { .. } => {
                                pre_scan_expr_spanned(&form, env, ctx, stdlib_env)?;
                            }
                            _ => {
                                let re_expanded = expand_expr(form.clone(), env, ctx, stdlib_env)?;
                                if !matches!(
                                    re_expanded.node,
                                    Expr::DefMacro { .. }
                                        | Expr::MacroDecl { .. }
                                        | Expr::SyntaxClass { .. }
                                ) {
                                    expanded_entries.push(Spanned::new(
                                        Entry {
                                            key: None, // splice output is unkeyed
                                            value: Rc::new(re_expanded),
                                        },
                                        entry.span,
                                    ));
                                }
                            }
                        }
                    }
                    continue;
                }

                let expanded_key = if let Some(key) = &entry.node.key {
                    Some(expand_expr(key.clone(), env, ctx, stdlib_env)?)
                } else {
                    None
                };
                expanded_entries.push(Spanned::new(
                    Entry {
                        key: expanded_key,
                        value: Rc::new(expanded_value),
                    },
                    entry.span,
                ));
            }

            Ok(Spanned::new(Expr::Dict(expanded_entries), expr.span))
        }

        Expr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            let expanded_body = expand_expr(body.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: Rc::new(expanded_body),
                    desugared: *desugared,
                },
                expr.span,
            ))
        }

        Expr::TypeAlias { params, body } => {
            let expanded_body = expand_expr(body.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::TypeAlias {
                    params: params.clone(),
                    body: Box::new(expanded_body),
                },
                expr.span,
            ))
        }

        Expr::TypeAssert {
            annotation,
            expr: asserted_expr,
            resolved_type,
        } => {
            let expanded_expr = expand_expr(asserted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::TypeAssert {
                    annotation: annotation.clone(),
                    expr: Box::new(expanded_expr),
                    resolved_type: resolved_type.clone(),
                },
                expr.span,
            ))
        }

        Expr::Quote(quoted_expr) => {
            let expanded_quoted = expand_expr(quoted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Quote(Box::new(expanded_quoted)),
                expr.span,
            ))
        }

        Expr::Unquote(unquoted_expr) => {
            let expanded_unquoted =
                expand_expr(unquoted_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::Unquote(Box::new(expanded_unquoted)),
                expr.span,
            ))
        }

        Expr::UnquoteSplice(spliced_expr) => {
            let expanded_spliced =
                expand_expr(spliced_expr.as_ref().clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::UnquoteSplice(Box::new(expanded_spliced)),
                expr.span,
            ))
        }

        Expr::Match { scrutinee, arms } => {
            let expanded_scrutinee = expand_expr(scrutinee.as_ref().clone(), env, ctx, stdlib_env)?;
            let expanded_arms = arms
                .iter()
                .map(|arm| {
                    let expanded_guard = if let Some(guard) = &arm.guard {
                        Some(Box::new(expand_expr(
                            guard.as_ref().clone(),
                            env,
                            ctx,
                            stdlib_env,
                        )?))
                    } else {
                        None
                    };
                    let expanded_body =
                        expand_expr(arm.body.as_ref().clone(), env, ctx, stdlib_env)?;
                    Ok(MatchArm {
                        pattern: arm.pattern.clone(),
                        guard: expanded_guard,
                        body: Box::new(expanded_body),
                    })
                })
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Spanned::new(
                Expr::Match {
                    scrutinee: Box::new(expanded_scrutinee),
                    arms: expanded_arms,
                },
                expr.span,
            ))
        }

        // ClassDecl: expand method signatures
        Expr::ClassDecl {
            name,
            params,
            superclasses,
            methods,
            determines,
            resolver,
            resolver_injective,
        } => {
            let expanded_methods = methods
                .iter()
                .map(|method| {
                    let expanded_value =
                        expand_expr((*method.node.value).clone(), env, ctx, stdlib_env)?;
                    Ok(Spanned::new(
                        Entry {
                            key: method.node.key.clone(),
                            value: Rc::new(expanded_value),
                        },
                        method.span,
                    ))
                })
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Spanned::new(
                Expr::ClassDecl {
                    name: name.clone(),
                    params: params.clone(),
                    superclasses: superclasses.clone(),
                    methods: expanded_methods,
                    determines: determines.clone(),
                    resolver: resolver.clone(),
                    resolver_injective: *resolver_injective,
                },
                expr.span,
            ))
        }

        // InstanceDecl: expand pattern expressions and method implementations
        Expr::InstanceDecl { class_name, arms } => {
            let expanded_arms = arms
                .iter()
                .map(|(pattern_expr, methods)| {
                    let expanded_pattern = expand_expr(pattern_expr.clone(), env, ctx, stdlib_env)?;
                    let expanded_methods = methods
                        .iter()
                        .map(|method| {
                            let expanded_value =
                                expand_expr((*method.node.value).clone(), env, ctx, stdlib_env)?;
                            Ok(Spanned::new(
                                Entry {
                                    key: method.node.key.clone(),
                                    value: Rc::new(expanded_value),
                                },
                                method.span,
                            ))
                        })
                        .collect::<EvalResult<Vec<_>>>()?;
                    Ok((expanded_pattern, expanded_methods))
                })
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Spanned::new(
                Expr::InstanceDecl {
                    class_name: class_name.clone(),
                    arms: expanded_arms,
                },
                expr.span,
            ))
        }

        Expr::PatternDecl { bindings } => {
            let expanded_bindings = bindings
                .iter()
                .map(|binding| expand_expr(binding.clone(), env, ctx, stdlib_env))
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Spanned::new(
                Expr::PatternDecl {
                    bindings: expanded_bindings,
                },
                expr.span,
            ))
        }

        Expr::LetDecl { bindings } => {
            let expanded_bindings = bindings
                .iter()
                .map(|binding| expand_expr(binding.clone(), env, ctx, stdlib_env))
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Spanned::new(
                Expr::LetDecl {
                    bindings: expanded_bindings,
                },
                expr.span,
            ))
        }

        Expr::CaseArm { pattern, body } => {
            let expanded_pattern = expand_expr((**pattern).clone(), env, ctx, stdlib_env)?;
            let expanded_body = expand_expr((**body).clone(), env, ctx, stdlib_env)?;
            Ok(Spanned::new(
                Expr::CaseArm {
                    pattern: Box::new(expanded_pattern),
                    body: Box::new(expanded_body),
                },
                expr.span,
            ))
        }

        // Leaf nodes — no expansion needed
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Annotated { .. }
        | Expr::Rest(_)
        | Expr::Placeholder
        | Expr::TypeApp { .. }
        | Expr::Error(_) => Ok(expr),
    }
}

/// Expand a macro call by matching arguments against the macro's params pattern.
///
/// The transformer receives arguments bound according to the [let ...] pattern in the macro's
/// params field. It returns an AST dict (or Splice) which is converted back to an Expr node.
/// The result is then re-expanded (fixpoint) until no macro calls remain.
///
/// Supports:
/// - [let ...] pattern matching: [macro my-if [let cond then else] body]
/// - Syntax-class validation: [macro pragma [let name@VarRef value@Literal] body]
/// - Anaphoric macros with inject: threading
/// - Splice output: returned as Expr::Splice for the caller to handle
///
/// ## Arena Boundary Invariant
///
/// The macro expansion boundary is a data boundary. Both the input AST dict and the output
/// AST dict are fully materialized before crossing. No arena-relative ThunkId handles may
/// flow from the stdlib arena into the expansion arena or vice versa.
fn expand_macro_call(
    macro_name: &str,
    args: &[Rc<Spanned<Expr>>],
    _named_args: &[Spanned<NamedArg>],
    call_span: Span,
    // dict_key: the key under which this macro call appears (for inject: threading).
    // None when in expression position.
    dict_key: Option<&str>,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Spanned<Expr>> {
    // Determine call site ID for blackhole detection
    let call_site_id = if call_span == Span::origin() {
        // Synthetic span (origin) — generated code
        CallSiteId::Synthetic(next_synthetic_id())
    } else {
        // Source span — use file_id=0 (single-file assumption for now)
        CallSiteId::Source {
            file_id: 0,
            offset: call_span.start.offset,
        }
    };

    // Enter expansion (depth check + blackhole detection)
    env.enter_expansion(call_site_id, call_span)?;

    // Clone out what we need from macro_metadata before taking mutable references to env.
    let macro_metadata = env
        .get_macro(macro_name)
        .expect("macro name verified before call");
    let transformer = macro_metadata.transformer.clone();
    let params_pattern = macro_metadata.params.clone();
    let inject_default = macro_metadata.inject_default.clone();
    let new_style = macro_metadata.new_style;

    let opts = AstToDictOpts::default();

    // Build the positional thunks to pass to invoke_function.
    let result_thunk = if new_style {
        // ====================================================================
        // Task 1: New-style [macro ...] with [let ...] pattern matching.
        // Each LetDecl binding corresponds to one positional argument.
        // ====================================================================

        // Extract bindings from the LetDecl pattern
        let bindings: Vec<&Spanned<Expr>> = match &params_pattern.node {
            Expr::LetDecl { bindings } => bindings.iter().collect(),
            _ => vec![],
        };

        // Task 3: Validate annotated params (syntax-class check) BEFORE calling the transformer.
        // Walk through non-variadic params and check their annotations against the args.
        let mut arg_idx = 0usize;
        for binding in &bindings {
            match &binding.node {
                Expr::Rest(_) => {
                    // Variadic — consumes all remaining args; no per-arg validation here
                    break;
                }
                Expr::Annotated {
                    name: param_name,
                    annotation,
                } => {
                    if arg_idx < args.len() {
                        validate_syntax_class(
                            &args[arg_idx],
                            &annotation.node,
                            param_name,
                            macro_name,
                            call_span,
                        )?;
                    }
                    arg_idx += 1;
                }
                Expr::VarRef { .. } => {
                    arg_idx += 1;
                }
                _ => {
                    arg_idx += 1;
                }
            }
        }

        // Quote each argument individually to an AST dict thunk.
        // Each quoted arg becomes a separate positional thunk for invoke_function.
        let mut positional_thunks: Vec<Rc<Thunk>> = Vec::with_capacity(args.len());
        for arg in args {
            let dict_thunk = ast_to_dict_expr(arg, &opts, ctx)?;
            // ARENA BOUNDARY: deep-materialize before crossing
            let arg_val = eval::materialize(&dict_thunk, Some(&call_span), ctx).map_err(|e| {
                EvalError::user_error(
                    format!(
                        "macro '{}': failed to quote argument for expansion: {}",
                        macro_name, e.kind
                    ),
                    call_span,
                )
            })?;
            let deep_arg_val =
                eval::deep_materialize(&arg_val, ctx, Some(&call_span)).map_err(|mut e| {
                    e.push_frame(
                        format!("deep-materializing argument for macro '{}'", macro_name),
                        call_span,
                    );
                    e
                })?;
            positional_thunks.push(Rc::new(Thunk::new_materialized(deep_arg_val, call_span)));
        }

        // Task 4: Thread inject: binding.
        // If the macro declares inject:, add the `binding` argument as the last positional.
        // In dict-key position: use the key name. In expression position: use inject_default.
        if inject_default.is_some() {
            let binding_name = dict_key
                .map(|k| k.to_string())
                .or_else(|| inject_default.clone())
                .unwrap_or_default();
            // Build a VarRef AST node for the binding name
            let binding_expr = Spanned::new(Expr::var_ref(binding_name.clone()), call_span);
            let binding_rc = Rc::new(binding_expr);
            let binding_thunk = ast_to_dict_expr(&binding_rc, &opts, ctx)?;
            let binding_val =
                eval::materialize(&binding_thunk, Some(&call_span), ctx).map_err(|e| {
                    EvalError::user_error(
                        format!(
                            "macro '{}': failed to quote binding name '{}': {}",
                            macro_name, binding_name, e.kind
                        ),
                        call_span,
                    )
                })?;
            let deep_binding_val = eval::deep_materialize(&binding_val, ctx, Some(&call_span))
                .map_err(|mut e| {
                    e.push_frame(
                        format!("deep-materializing binding arg for macro '{}'", macro_name),
                        call_span,
                    );
                    e
                })?;
            positional_thunks.push(Rc::new(Thunk::new_materialized(
                deep_binding_val,
                call_span,
            )));
        }

        // Materialize the transformer to get the function value
        let transformer_val =
            eval::materialize(&transformer, Some(&call_span), ctx).map_err(|e| {
                EvalError::user_error(
                    format!(
                        "macro '{}' transformer failed to evaluate: {}",
                        macro_name, e.kind
                    ),
                    call_span,
                )
            })?;

        match &transformer_val {
            Value::Function {
                params,
                body,
                env: closure_env,
                ..
            } => {
                use crate::eval_call::{invoke_function, CallContext};
                let call_ctx = CallContext {
                    params: params.as_slice(),
                    body,
                    positional: &positional_thunks,
                    named: None,
                    closure_env,
                    default_env: closure_env,
                    ctx,
                    call_span,
                    origin: Some(Rc::from(format!("macro:{}", macro_name))),
                };
                invoke_function(&call_ctx).map_err(|e| {
                    EvalError::user_error(
                        format!("macro '{}' transformer call failed: {}", macro_name, e.kind),
                        call_span,
                    )
                })?
            }
            other => {
                env.leave_expansion(call_site_id);
                return Err(EvalError::user_error(
                    format!(
                        "macro '{}' transformer must be a function, got {}",
                        macro_name,
                        other.type_name()
                    ),
                    call_span,
                )
                .into());
            }
        }
    } else {
        // ====================================================================
        // Old-style [defmacro ...]: single args dict passed as one positional arg.
        // ====================================================================
        let mut quoted_args = Vec::with_capacity(args.len());
        for arg in args {
            let dict_thunk = ast_to_dict_expr(arg, &opts, ctx)?;
            quoted_args.push(dict_thunk);
        }

        // Build the args list as a Value::Dict with integer keys (tinct list)
        let mut args_dict = indexmap::IndexMap::new();
        for (i, thunk) in quoted_args.into_iter().enumerate() {
            let thunk_id = ctx.alloc_thunk(thunk);
            args_dict.insert(Key::Int(i as i64), thunk_id);
        }
        let args_value = Value::Dict(args_dict);

        // ARENA BOUNDARY: Deep-materialize the input AST dict before passing to the transformer.
        let deep_args_value =
            eval::deep_materialize(&args_value, ctx, Some(&call_span)).map_err(|mut e| {
                e.push_frame(
                    format!("deep-materializing input to macro '{}'", macro_name),
                    call_span,
                );
                e
            })?;

        #[cfg(debug_assertions)]
        debug_assert!(
            all_thunks_materialized(&deep_args_value, ctx),
            "macro expansion boundary violated: input contains lazy thunks"
        );

        let args_thunk = Rc::new(Thunk::new_materialized(deep_args_value, call_span));

        // Materialize the transformer to get the function value
        let transformer_val =
            eval::materialize(&transformer, Some(&call_span), ctx).map_err(|e| {
                EvalError::user_error(
                    format!(
                        "macro '{}' transformer failed to evaluate: {}",
                        macro_name, e.kind
                    ),
                    call_span,
                )
            })?;

        match &transformer_val {
            Value::Function {
                params,
                body,
                env: closure_env,
                ..
            } => {
                use crate::eval_call::{invoke_function, CallContext};
                let call_ctx = CallContext {
                    params: params.as_slice(),
                    body,
                    positional: &[args_thunk],
                    named: None,
                    closure_env,
                    default_env: closure_env,
                    ctx,
                    call_span,
                    origin: Some(Rc::from(format!("macro:{}", macro_name))),
                };
                invoke_function(&call_ctx).map_err(|e| {
                    EvalError::user_error(
                        format!("macro '{}' transformer call failed: {}", macro_name, e.kind),
                        call_span,
                    )
                })?
            }
            other => {
                env.leave_expansion(call_site_id);
                return Err(EvalError::user_error(
                    format!(
                        "macro '{}' transformer must be a function, got {}",
                        macro_name,
                        other.type_name()
                    ),
                    call_span,
                )
                .into());
            }
        }
    };

    // Materialize the result to get the AST dict.
    // Preserve the full inner error stack so macro expansion bugs in do-fold,
    // tmpl helpers, etc. are diagnosable without losing the macro wrapper context.
    let result_val = eval::materialize(&result_thunk, Some(&call_span), ctx).map_err(|e| {
        // Build E080 wrapper that names the macro and error kind, then copy all
        // stack frames from the inner error so the full call stack is preserved.
        let mut err = EvalError::user_error(
            format!(
                "macro '{}' expansion result failed to evaluate: {}",
                macro_name, e.kind
            ),
            call_span,
        );
        // Copy inner stack frames into the wrapper so the call path is visible.
        for frame in &e.stack {
            err.push_frame(frame.label.clone(), frame.span);
        }
        err.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        err
    })?;

    // Deep-materialize the result dict so dict_to_ast can inspect all fields
    let deep_result = eval::deep_materialize(&result_val, ctx, None).map_err(|mut e| {
        e.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        e
    })?;

    #[cfg(debug_assertions)]
    debug_assert!(
        all_thunks_materialized(&deep_result, ctx),
        "macro expansion boundary violated: output contains lazy thunks"
    );

    // Convert the result dict back to AST
    let mut expanded_ast = dict_to_ast(&deep_result, ctx).map_err(|e| {
        EvalError::user_error(
            format!("macro '{}' returned invalid AST dict: {}", macro_name, e),
            call_span,
        )
    })?;

    // Set the expanded AST's top-level span to the call site span.
    if expanded_ast.span == Span::origin() {
        expanded_ast.span = call_span;
    }

    // Phase 1: allocate a fresh scope ID for hygiene provenance tracking.
    // Phase 2 (future): apply scope-based alpha-renaming to macro-template bindings.
    let _scope_id = ScopeId::fresh();

    // Record provenance for this expansion (dual-span tracking)
    env.provenance.insert(
        SpanKey::from(call_span),
        MacroProvenance {
            macro_name: macro_name.to_string(),
            call_site_span: call_span,
        },
    );

    // Leave expansion
    env.leave_expansion(call_site_id);

    // Task 2: Splice handling — if the expansion returns Expr::Splice, return it as-is.
    // The caller (expand_document or expand_expr_inner for Dict context) will handle injection.
    // In expression position, Expr::Splice is an expansion-time error (checked in expand_expr_inner).
    if matches!(expanded_ast.node, Expr::Splice(_)) {
        return Ok(expanded_ast);
    }

    // Re-expand the result (fixpoint)
    expand_expr(expanded_ast, env, ctx, stdlib_env)
}

/// Validate an argument against a syntax-class annotation (Task 3).
///
/// For now, only supports @VariantName syntax (e.g., @VarRef, @Literal).
/// Full named syntax-class support is TODO.
#[allow(dead_code)] // TODO: will be used when Task 1 pattern matching is fully implemented
fn validate_syntax_class(
    arg: &Rc<Spanned<Expr>>,
    annotation: &crate::ast::Annotation,
    param_name: &str,
    macro_name: &str,
    call_span: Span,
) -> EvalResult<()> {
    use crate::ast::Annotation;

    match annotation {
        Annotation::Simple(name) => {
            // Simple annotation like @VarRef, @Literal
            // Check if the arg matches the expected variant
            let expected_variant = name.as_str();
            let got_variant = match &arg.node {
                Expr::VarRef { .. } => "VarRef",
                Expr::Int(_) | Expr::Float(_) | Expr::Bool(_) | Expr::Str(_) => "Literal",
                Expr::Call { .. } => "Call",
                Expr::Dict(_) => "Dict",
                Expr::LetDecl { .. } => "Let",
                Expr::Fn { .. } => "Fn",
                Expr::Sequential(_) => "Seq",
                Expr::Annotated { .. } => "Annotated",
                Expr::Quote(_) => "Quote",
                Expr::Unquote(_) => "Unquote",
                Expr::UnquoteSplice(_) => "UnquoteSplice",
                _ => "other",
            };

            if expected_variant != got_variant {
                return Err(EvalError::macro_error(
                    format!(
                        "macro '{}': argument '{}' expected {}, got {}",
                        macro_name, param_name, expected_variant, got_variant
                    ),
                    call_span,
                )
                .into());
            }
            Ok(())
        }
        _ => {
            // Other annotation types not yet supported
            Ok(())
        }
    }
}

/// Look up provenance for a span and format an "in expansion of" note.
pub fn format_provenance(provenance: &ProvenanceMap, span: Span) -> Option<String> {
    let key = SpanKey::from(span);
    provenance.get(&key).map(|prov| {
        format!(
            "in expansion of `{}` at {}:{}",
            prov.macro_name, prov.call_site_span.start.line, prov.call_site_span.start.column,
        )
    })
}

/// Debug assertion helper: check that all thunks in a value tree are materialized.
/// Used to validate the macro expansion boundary invariant: no lazy thunks cross
/// from the stdlib arena to the expansion arena or vice versa.
#[cfg(debug_assertions)]
fn all_thunks_materialized(val: &Value, ctx: &Rc<EvalContext>) -> bool {
    match val {
        Value::Dict(map) => {
            for thunk_id in map.values() {
                let thunk = ctx.get_thunk(*thunk_id);
                let state = thunk.state();
                if !matches!(&*state, crate::value::ThunkState::Materialized(_)) {
                    return false;
                }
                // Recursively check the materialized value
                if let crate::value::ThunkState::Materialized(ref inner_val) = &*state {
                    if !all_thunks_materialized(inner_val, ctx) {
                        return false;
                    }
                }
            }
            true
        }
        Value::Seq {
            head: head_id,
            tail: tail_id,
        } => {
            // Check head
            let head_thunk = ctx.get_thunk(*head_id);
            let head_state = head_thunk.state();
            if !matches!(&*head_state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref head_val) = &*head_state {
                if !all_thunks_materialized(head_val, ctx) {
                    return false;
                }
            }
            // Check tail
            let tail_thunk = ctx.get_thunk(*tail_id);
            let tail_state = tail_thunk.state();
            if !matches!(&*tail_state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref tail_val) = &*tail_state {
                if !all_thunks_materialized(tail_val, ctx) {
                    return false;
                }
            }
            true
        }
        Value::Proxy { handler } => {
            // Check handler
            let handler_thunk = ctx.get_thunk(*handler);
            let handler_state = handler_thunk.state();
            if !matches!(&*handler_state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref handler_val) = &*handler_state {
                if !all_thunks_materialized(handler_val, ctx) {
                    return false;
                }
            }
            true
        }
        Value::Overlay(left, right) => {
            // Check left
            let left_thunk = ctx.get_thunk(*left);
            let left_state = left_thunk.state();
            if !matches!(&*left_state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref left_val) = &*left_state {
                if !all_thunks_materialized(left_val, ctx) {
                    return false;
                }
            }
            // Check right
            let right_thunk = ctx.get_thunk(*right);
            let right_state = right_thunk.state();
            if !matches!(&*right_state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref right_val) = &*right_state {
                if !all_thunks_materialized(right_val, ctx) {
                    return false;
                }
            }
            true
        }
        Value::Variant {
            payload: Some(id), ..
        } => {
            let thunk = ctx.get_thunk(*id);
            let state = thunk.state();
            if !matches!(&*state, crate::value::ThunkState::Materialized(_)) {
                return false;
            }
            if let crate::value::ThunkState::Materialized(ref inner_val) = &*state {
                if !all_thunks_materialized(inner_val, ctx) {
                    return false;
                }
            }
            true
        }
        Value::Variant { payload: None, .. } => true,
        // All other values (primitives, functions, capabilities, etc.) have no thunks to check
        _ => true,
    }
}
