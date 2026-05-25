//! Macro expansion pass for `[macro ...]` and legacy `[defmacro ...]` forms.
//!
//! Runs between parse and desugar: `parse -> expand_surface_program -> desugar -> typecheck -> eval`
//!
//! The expansion loop:
//! 1. Walk the AST top-down
//! 2. Register `DefMacro` nodes by evaluating their transformer in a fresh context
//! 3. Expand `Call` nodes if the function name matches a registered macro:
//!    - Quote arguments via `surface_node_to_dict` (through surface bridge)
//!    - Call the macro transformer with the quoted args
//!    - Convert the result back to AST via `dict_to_surface_node` (through surface bridge)
//!    - Replace the Call node with the expansion
//!    - Re-expand the result (fixpoint)
//! 4. Track in-progress expansions to detect infinite recursion
//!
//! ## Hygiene (opt-in via gensym)
//!
//! Hygiene is **opt-in** via the `gensym` builtin. Macros must explicitly call `gensym`
//! to generate fresh variable names. By default, macro-introduced bindings can capture
//! user variables (MH1).
//!
//! Full scope-set hygiene (Flatt 2016) is not yet implemented; the ScopeId
//! infrastructure was removed. A `gensym`-based approach is the only hygiene
//! mechanism available.
//!
//! ## Dual-span provenance (Pombrio & Krishnamurthi 2015)
//!
//! The expander maintains a side map from generated AST node spans to their expansion
//! provenance: `(macro_name, call_site_span)`. Error messages use this to show
//! "in expansion of `<name>` at line N".

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use crate::ast::{Param, Span, Spanned, SurfaceEntry, SurfaceNamedArg, SurfaceNode};
use crate::ast_dict::{dict_to_surface_node, surface_node_to_dict, AstToDictOpts};
use crate::builtins;
use crate::error::{EvalError, EvalResult};
use crate::eval::{self, EvalContext};
use crate::value::{Environment, Thunk, Value};

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
    transformer: Arc<Thunk>,
    /// The params pattern (LetDecl SurfaceNode) for binding argument validation.
    params: Arc<SurfaceNode>,
    /// Optional inject: default name for anaphoric macros.
    inject_default: Option<String>,
}

/// Metadata for a registered syntax class — the pattern and error message.
#[derive(Debug, Clone)]
struct SyntaxClassDef {
    /// The pattern to match (typically a [let ...] binding pattern as a SurfaceNode).
    pattern: Arc<SurfaceNode>,
    /// User-facing error message describing what the syntax class expects.
    message: String,
}

/// Macro expansion context — tracks registered macros and prevents infinite expansion.
#[derive(Debug, Clone)]
pub struct MacroEnv {
    /// Map from macro name to its metadata (transformer, params, inject default).
    macros: HashMap<String, MacroMetadata>,
    /// Map from syntax class name to its definition (pattern, message).
    syntax_classes: HashMap<String, SyntaxClassDef>,
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
    /// Accumulated during expansion.
    pub discovered_macros: Vec<(String, Arc<Thunk>)>,
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

impl Default for MacroEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl MacroEnv {
    pub fn new() -> Self {
        Self {
            macros: HashMap::new(),
            syntax_classes: HashMap::new(),
            depth: 0,
            node_count: 0,
            in_progress: HashSet::new(),
            provenance: HashMap::new(),
            discovered_macros: Vec::new(),
        }
    }

    /// Register a macro transformer with params pattern and inject default.
    fn register_macro(
        &mut self,
        name: String,
        transformer: Arc<Thunk>,
        params: Arc<SurfaceNode>,
        inject_default: Option<String>,
        _span: Span,
    ) -> EvalResult<()> {
        self.macros.insert(
            name,
            MacroMetadata {
                transformer,
                params,
                inject_default,
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

/// Result of surface-program macro expansion.
pub struct ExpandSurfaceResult {
    /// Provenance map: generated-node span → expansion origin (for dual-span error reporting).
    pub provenance: ProvenanceMap,
    /// Macro inject defaults: `macro_name -> inject_default_name`.
    /// Populated from all macros with `inject:` declarations encountered during expansion.
    /// Used by the `macro-injects` builtin for runtime introspection.
    pub macro_injects_map: HashMap<String, String>,
}

/// Register stdlib macros by pre-scanning the stdlib source files.
///
/// Parses `stdlib/macros.llt` and calls `pre_scan_surface_document` on each document.
/// This discovers all `[macro ...]` / `[defmacro ...]` declarations and registers them
/// in `env_macro` — the same registration path used for user-code macros.
///
/// Errors (parse failures, transformer evaluation failures) are silently ignored so that
/// a stdlib failure here surfaces as a missing macro error at call time rather than a
/// hard crash during stdlib loading. The stdlib is already evaluated and its bindings are
/// available in `stdlib_env` which is passed to `pre_scan_surface_document`.
fn register_stdlib_macros_from_env(
    env_macro: &mut MacroEnv,
    stdlib_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) {
    let macros_source = include_str!("../stdlib/macros.llt");
    let parsed = match crate::parser::parse(macros_source) {
        Ok(p) => p,
        Err(_) => return,
    };
    for doc_spanned in &parsed.program.documents {
        let _ = pre_scan_surface_document(&doc_spanned.node, env_macro, ctx, stdlib_env);
    }
}

// Reentrance depth guard for expand_surface_program → create_stdlib_env calls.
// When depth > 0, we're in a re-entrant call and must use create_root_env
// to avoid infinite recursion through the stdlib loading path.
std::thread_local! {
    static EXPAND_MACROS_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    static EXPAND_EXPR_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// RAII guard for EXPAND_MACROS_DEPTH. Restores depth on drop, even if the guarded scope panics.
struct DepthGuard {
    original_depth: u32,
}

impl DepthGuard {
    fn new() -> Self {
        let depth = EXPAND_MACROS_DEPTH.get();
        EXPAND_MACROS_DEPTH.set(depth + 1);
        DepthGuard {
            original_depth: depth,
        }
    }
}

impl Drop for DepthGuard {
    fn drop(&mut self) {
        EXPAND_MACROS_DEPTH.set(self.original_depth);
    }
}

/// Expand macros in a `SurfaceProgram`.
///
/// This is the top-level entry point called from the pipeline.
/// Walks `SurfaceDocument.items` and performs macro expansion operations:
/// - Register `DefMacro`/`MacroDecl`/`SyntaxClass` declarations
/// - Flatten `Splice` declarations into expression items
/// - Expand `Call` nodes whose function name is a registered macro
///
/// The expansion body is fully native SurfaceExpression — no bridge to old `Expr` types needed.
pub fn expand_surface_program(
    program: &mut crate::ast::SurfaceProgram,
    no_fs: bool,
    base_dir: &cap_std::fs::Dir,
) -> EvalResult<ExpandSurfaceResult> {
    use crate::ast::{SurfaceDeclaration, SurfaceItem};

    // Detect infinite recursion
    let em_depth = EXPAND_MACROS_DEPTH.get();
    if em_depth > 10 {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "expand_surface_program: infinite recursion detected (depth={})",
                em_depth
            ),
            crate::ast::Span::origin(),
        )
        .into());
    }

    let mut env_macro = MacroEnv::new();

    // Clone the base directory handle
    let base_dir = base_dir.open_dir(".").map_err(|e| {
        EvalError::internal(
            format!("cannot clone base directory for macro expansion: {e}"),
            crate::ast::Span::origin(),
        )
    })?;

    // Create the stdlib env for macro expansion
    let depth = EXPAND_MACROS_DEPTH.get();
    let (stdlib_env, ctx) = if depth == 0 {
        let _depth_guard = DepthGuard::new();
        match builtins::create_stdlib_env_with_arena() {
            Ok((env, arena)) => {
                let type_stage_env =
                    crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&env));
                let ctx = EvalContext::new_sharing_arena(
                    base_dir,
                    Arc::clone(&env),
                    type_stage_env,
                    no_fs,
                    Arc::clone(&arena),
                    HashMap::new(), // macro_injects_map — will be populated during expansion
                );
                register_stdlib_macros_from_env(&mut env_macro, &env, &ctx);
                (env, ctx)
            }
            Err(e) => return Err(e),
        }
    } else {
        let env = builtins::create_root_env();
        let ctx = EvalContext::new_empty(base_dir, Arc::clone(&env), no_fs);
        (env, ctx)
    };
    let ctx = Rc::new(ctx);

    // Process each document in the program
    for doc_spanned in &mut program.documents {
        let doc = &mut doc_spanned.node;

        // Pre-scan: register macros from declarations
        pre_scan_surface_document(doc, &mut env_macro, &ctx, &stdlib_env)?;

        // Expand items
        let mut expanded_items = Vec::new();
        for item in std::mem::take(&mut doc.items) {
            match item {
                SurfaceItem::Decl(decl_spanned) => {
                    match decl_spanned.node {
                        SurfaceDeclaration::Splice(forms) => {
                            // Flatten splice: each form becomes a separate Expr item
                            for form in forms {
                                expanded_items.push(SurfaceItem::Expr(form));
                            }
                        }
                        SurfaceDeclaration::DefMacro { .. }
                        | SurfaceDeclaration::MacroDecl { .. }
                        | SurfaceDeclaration::SyntaxClass { .. } => {
                            // Macro declarations are registered during pre-scan; do not emit
                        }
                        _ => {
                            // Other declarations pass through unchanged
                            expanded_items.push(SurfaceItem::Decl(decl_spanned));
                        }
                    }
                }
                SurfaceItem::Expr(node) => {
                    // Expand via the native surface path — expand_surface_expr recurses
                    // into all sub-expressions natively.
                    let expanded_node =
                        expand_surface_expr(&node, &mut env_macro, &ctx, &stdlib_env)?;
                    expanded_items.push(SurfaceItem::Expr(expanded_node));
                }
            }
        }

        doc.items = expanded_items;
    }

    let macro_injects_map = env_macro.get_inject_map();
    Ok(ExpandSurfaceResult {
        provenance: env_macro.provenance,
        macro_injects_map,
    })
}

/// Depth-guarded wrapper for `expand_surface_expr_inner`.
///
/// Prevents stack overflow on pathological inputs (deeply nested ASTs).
/// Mirrors the depth guard in `expand_expr`.
fn expand_surface_expr(
    node: &Arc<SurfaceNode>,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<Arc<SurfaceNode>> {
    let ee_depth = EXPAND_EXPR_DEPTH.get();
    if ee_depth > 10_000 {
        return Err(EvalError::resource_limit_exceeded(
            format!("macro expansion: AST recursion depth {ee_depth} exceeds limit (10000)"),
            node.span,
        )
        .into());
    }
    EXPAND_EXPR_DEPTH.set(ee_depth + 1);
    let result = expand_surface_expr_inner(node, env, ctx, stdlib_env);
    EXPAND_EXPR_DEPTH.set(ee_depth);
    result
}

/// Expand macros in a SurfaceNode (no Expr bridge in the main path).
///
/// For `Call` nodes whose func is a registered macro, delegates to
/// `expand_macro_call_surface` which takes `Arc<SurfaceNode>` args directly.
/// For all other nodes, recurses into children natively and rebuilds the node.
///
/// Declaration nodes (`SurfaceExpression::Decl`) pass through unchanged —
/// they were processed during pre-scan.
fn expand_surface_expr_inner(
    node: &Arc<SurfaceNode>,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<Arc<SurfaceNode>> {
    use crate::ast::SurfaceExpression;

    let span = node.span;
    match &node.expr {
        // Declaration nodes pass through (registered during pre-scan; not emitted post-expansion)
        SurfaceExpression::Decl(_) => Ok(Arc::clone(node)),

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            // Check if this is a macro call
            let macro_name = if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if env.is_macro(name) {
                    Some(name.clone())
                } else {
                    None
                }
            } else {
                None
            };

            if let Some(macro_name) = macro_name {
                // Macro call — expand via native surface path
                let expanded = expand_macro_call_surface(
                    &macro_name,
                    args,
                    named_args,
                    span,
                    None, // expression position — no dict key
                    env,
                    ctx,
                    stdlib_env,
                )?;
                // Splice in expression position is an expansion-time error
                if let SurfaceExpression::Decl(decl) = &expanded.expr {
                    if matches!(decl.as_ref(), crate::ast::SurfaceDeclaration::Splice(_)) {
                        return Err(EvalError::macro_error(
                            format!(
                                "macro '{}' returned splice, which is not valid in expression position",
                                macro_name
                            ),
                            span,
                        )
                        .into());
                    }
                }
                Ok(expanded)
            } else {
                // Not a macro call — recurse into children
                let expanded_func = expand_surface_expr(func, env, ctx, stdlib_env)?;
                let expanded_args = args
                    .iter()
                    .map(|arg| expand_surface_expr(arg, env, ctx, stdlib_env))
                    .collect::<EvalResult<Vec<_>>>()?;
                let expanded_named_args = named_args
                    .iter()
                    .map(|na| {
                        let expanded_value =
                            expand_surface_expr(&na.node.value, env, ctx, stdlib_env)?;
                        Ok(Spanned::new(
                            SurfaceNamedArg {
                                name: na.node.name.clone(),
                                value: expanded_value,
                            },
                            na.span,
                        ))
                    })
                    .collect::<EvalResult<Vec<_>>>()?;
                Ok(Arc::new(SurfaceNode {
                    expr: SurfaceExpression::Call {
                        func: expanded_func,
                        args: expanded_args,
                        named_args: expanded_named_args,
                        implied: *implied,
                    },
                    span,
                }))
            }
        }

        SurfaceExpression::DotAccess { expr, field } => {
            let expanded = expand_surface_expr(expr, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::DotAccess {
                    expr: expanded,
                    field: field.clone(),
                },
                span,
            }))
        }

        SurfaceExpression::Pipe { lhs, rhs } => {
            let expanded_lhs = expand_surface_expr(lhs, env, ctx, stdlib_env)?;
            let expanded_rhs = expand_surface_expr(rhs, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Pipe {
                    lhs: expanded_lhs,
                    rhs: expanded_rhs,
                },
                span,
            }))
        }

        SurfaceExpression::Sequential(exprs) => {
            let expanded = exprs
                .iter()
                .map(|e| expand_surface_expr(e, env, ctx, stdlib_env))
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Sequential(expanded),
                span,
            }))
        }

        SurfaceExpression::Dict(entries) => {
            let mut expanded_entries = Vec::new();
            for entry in entries {
                // Extract dict key string for inject: threading
                let dict_key_str: Option<String> =
                    entry.node.key.as_ref().and_then(|k| match &k.expr {
                        SurfaceExpression::Str(s) => Some(s.clone()),
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        _ => None,
                    });

                // Check if the value is a macro call (for inject: key threading)
                let expanded_value = {
                    let value_node = &entry.node.value;
                    if let SurfaceExpression::Call {
                        func,
                        args,
                        named_args,
                        ..
                    } = &value_node.expr
                    {
                        if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                            if env.is_macro(name) {
                                let macro_name = name.clone();
                                expand_macro_call_surface(
                                    &macro_name,
                                    args,
                                    named_args,
                                    value_node.span,
                                    dict_key_str.as_deref(),
                                    env,
                                    ctx,
                                    stdlib_env,
                                )?
                            } else {
                                expand_surface_expr(value_node, env, ctx, stdlib_env)?
                            }
                        } else {
                            expand_surface_expr(value_node, env, ctx, stdlib_env)?
                        }
                    } else {
                        expand_surface_expr(value_node, env, ctx, stdlib_env)?
                    }
                };

                // Filter out declaration nodes (they've been registered; do not emit)
                if let SurfaceExpression::Decl(decl) = &expanded_value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::DefMacro { .. }
                        | crate::ast::SurfaceDeclaration::MacroDecl { .. }
                        | crate::ast::SurfaceDeclaration::SyntaxClass { .. } => continue,
                        _ => {}
                    }
                }

                // Splice in dict context: inject each form as a separate unkeyed entry
                if let SurfaceExpression::Decl(decl) = &expanded_value.expr {
                    if let crate::ast::SurfaceDeclaration::Splice(forms) = decl.as_ref() {
                        for form in forms {
                            if let SurfaceExpression::Decl(inner_decl) = &form.expr {
                                match inner_decl.as_ref() {
                                    crate::ast::SurfaceDeclaration::MacroDecl { .. }
                                    | crate::ast::SurfaceDeclaration::SyntaxClass { .. }
                                    | crate::ast::SurfaceDeclaration::DefMacro { .. } => {
                                        // Register via native surface path
                                        register_surface_macro_decl(
                                            inner_decl.as_ref(),
                                            form.span,
                                            env,
                                            ctx,
                                            stdlib_env,
                                        )?;
                                    }
                                    _ => {
                                        let re_expanded =
                                            expand_surface_expr(form, env, ctx, stdlib_env)?;
                                        if let SurfaceExpression::Decl(inner) = &re_expanded.expr {
                                            match inner.as_ref() {
                                                crate::ast::SurfaceDeclaration::DefMacro {
                                                    ..
                                                }
                                                | crate::ast::SurfaceDeclaration::MacroDecl {
                                                    ..
                                                }
                                                | crate::ast::SurfaceDeclaration::SyntaxClass {
                                                    ..
                                                } => {}
                                                _ => {
                                                    expanded_entries.push(Spanned::new(
                                                        SurfaceEntry {
                                                            key: None,
                                                            value: re_expanded,
                                                        },
                                                        entry.span,
                                                    ));
                                                }
                                            }
                                        } else {
                                            expanded_entries.push(Spanned::new(
                                                SurfaceEntry {
                                                    key: None,
                                                    value: re_expanded,
                                                },
                                                entry.span,
                                            ));
                                        }
                                    }
                                }
                            } else {
                                let re_expanded = expand_surface_expr(form, env, ctx, stdlib_env)?;
                                expanded_entries.push(Spanned::new(
                                    SurfaceEntry {
                                        key: None,
                                        value: re_expanded,
                                    },
                                    entry.span,
                                ));
                            }
                        }
                        continue;
                    }
                }

                let expanded_key = if let Some(key) = &entry.node.key {
                    Some(expand_surface_expr(key, env, ctx, stdlib_env)?)
                } else {
                    None
                };
                expanded_entries.push(Spanned::new(
                    SurfaceEntry {
                        key: expanded_key,
                        value: expanded_value,
                    },
                    entry.span,
                ));
            }
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Dict(expanded_entries),
                span,
            }))
        }

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            let expanded_body = expand_surface_expr(body, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: expanded_body,
                    desugared: *desugared,
                },
                span,
            }))
        }

        SurfaceExpression::TypeAssert { annotation, expr } => {
            let expanded = expand_surface_expr(expr, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: expanded,
                },
                span,
            }))
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            let expanded_scrutinee = expand_surface_expr(scrutinee, env, ctx, stdlib_env)?;
            let expanded_arms = arms
                .iter()
                .map(|arm| {
                    let expanded_guard = arm
                        .guard
                        .as_ref()
                        .map(|g| expand_surface_expr(g, env, ctx, stdlib_env))
                        .transpose()?;
                    let expanded_body = expand_surface_expr(&arm.body, env, ctx, stdlib_env)?;
                    Ok(crate::ast::SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: expanded_guard,
                        body: expanded_body,
                    })
                })
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Match {
                    scrutinee: expanded_scrutinee,
                    arms: expanded_arms,
                },
                span,
            }))
        }

        SurfaceExpression::Quote(inner) => {
            let expanded = expand_surface_expr(inner, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Quote(expanded),
                span,
            }))
        }

        SurfaceExpression::Unquote(inner) => {
            let expanded = expand_surface_expr(inner, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::Unquote(expanded),
                span,
            }))
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            let expanded = expand_surface_expr(inner, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::UnquoteSplice(expanded),
                span,
            }))
        }

        SurfaceExpression::PatternDecl { bindings } => {
            let expanded = bindings
                .iter()
                .map(|b| expand_surface_expr(b, env, ctx, stdlib_env))
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::PatternDecl { bindings: expanded },
                span,
            }))
        }

        SurfaceExpression::LetDecl { bindings } => {
            let expanded = bindings
                .iter()
                .map(|b| expand_surface_expr(b, env, ctx, stdlib_env))
                .collect::<EvalResult<Vec<_>>>()?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::LetDecl { bindings: expanded },
                span,
            }))
        }

        SurfaceExpression::CaseArm { pattern, body } => {
            let expanded_pattern = expand_surface_expr(pattern, env, ctx, stdlib_env)?;
            let expanded_body = expand_surface_expr(body, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::CaseArm {
                    pattern: expanded_pattern,
                    body: expanded_body,
                },
                span,
            }))
        }

        SurfaceExpression::TypeApp { func, arg } => {
            let expanded_func = expand_surface_expr(func, env, ctx, stdlib_env)?;
            let expanded_arg = expand_surface_expr(arg, env, ctx, stdlib_env)?;
            Ok(Arc::new(SurfaceNode {
                expr: SurfaceExpression::TypeApp {
                    func: expanded_func,
                    arg: expanded_arg,
                },
                span,
            }))
        }

        // Leaf nodes — clone the Arc (shared immutable data)
        SurfaceExpression::Int(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Annotated { .. }
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Error(_) => Ok(Arc::clone(node)),
    }
}

/// Validate an argument (as a SurfaceNode) against a syntax-class annotation.
///
/// Surface-native version of `validate_syntax_class`. Called from
/// `expand_macro_call_surface` to avoid converting SurfaceNode → Expr for validation.
fn validate_syntax_class_surface(
    arg: &Arc<SurfaceNode>,
    annotation: &crate::ast::Annotation,
    param_name: &str,
    macro_name: &str,
    call_span: Span,
    env: &MacroEnv,
) -> EvalResult<()> {
    use crate::ast::{Annotation, SurfaceExpression};

    const SINGLE_VARIANT_NAMES: &[&str] = &[
        "VarRef",
        "Literal",
        "Call",
        "Dict",
        "LetDecl",
        "Fn",
        "Seq",
        "Annotated",
        "Quote",
        "Unquote",
        "UnquoteSplice",
    ];

    match annotation {
        Annotation::Simple(name) => {
            let expected_variant = name.as_str();

            // Named syntax class: check the argument against the pattern natively.
            if let Some(syntax_class) = env.syntax_classes.get(expected_variant) {
                return validate_against_pattern_surface(
                    arg,
                    &syntax_class.pattern,
                    &syntax_class.message,
                    param_name,
                    macro_name,
                    call_span,
                );
            }

            if !SINGLE_VARIANT_NAMES.contains(&expected_variant) {
                return Ok(());
            }

            let got_variant = match &arg.expr {
                SurfaceExpression::VarRef { .. } => "VarRef",
                SurfaceExpression::Int(_)
                | SurfaceExpression::Float(_)
                | SurfaceExpression::Bool(_)
                | SurfaceExpression::Str(_) => "Literal",
                SurfaceExpression::Call { .. } => "Call",
                SurfaceExpression::Dict(_) => "Dict",
                SurfaceExpression::LetDecl { .. } => "LetDecl",
                SurfaceExpression::Fn { .. } => "Fn",
                SurfaceExpression::Sequential(_) => "Seq",
                SurfaceExpression::Annotated { .. } => "Annotated",
                SurfaceExpression::Quote(_) => "Quote",
                SurfaceExpression::Unquote(_) => "Unquote",
                SurfaceExpression::UnquoteSplice(_) => "UnquoteSplice",
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
        _ => Ok(()),
    }
}

/// Validate a SurfaceNode argument against a syntax-class pattern (surface-native).
///
/// The pattern is a SurfaceNode expected to be `LetDecl` with one `Annotated` binding.
/// We extract the expected variant name and check the argument matches.
fn validate_against_pattern_surface(
    arg: &Arc<SurfaceNode>,
    pattern: &Arc<SurfaceNode>,
    message: &str,
    param_name: &str,
    macro_name: &str,
    call_span: Span,
) -> EvalResult<()> {
    use crate::ast::SurfaceExpression;

    // The pattern should be LetDecl with one Annotated binding.
    if let SurfaceExpression::LetDecl { bindings } = &pattern.expr {
        if bindings.len() == 1 {
            if let SurfaceExpression::Annotated { annotation, .. } = &bindings[0].expr {
                if let crate::ast::Annotation::Simple(expected_variant) = &annotation.node {
                    let got_variant = match &arg.expr {
                        SurfaceExpression::VarRef { .. } => "VarRef",
                        SurfaceExpression::Int(_)
                        | SurfaceExpression::Float(_)
                        | SurfaceExpression::Bool(_)
                        | SurfaceExpression::Str(_) => "Literal",
                        SurfaceExpression::Call { .. } => "Call",
                        SurfaceExpression::Dict(_) => "Dict",
                        SurfaceExpression::LetDecl { .. } => "LetDecl",
                        SurfaceExpression::Fn { .. } => "Fn",
                        SurfaceExpression::Sequential(_) => "Seq",
                        SurfaceExpression::Annotated { .. } => "Annotated",
                        SurfaceExpression::Quote(_) => "Quote",
                        SurfaceExpression::Unquote(_) => "Unquote",
                        SurfaceExpression::UnquoteSplice(_) => "UnquoteSplice",
                        _ => "other",
                    };
                    if expected_variant.as_str() != got_variant {
                        return Err(EvalError::macro_error(
                            format!(
                                "macro '{}': argument '{}' — {}, got {}",
                                macro_name, param_name, message, got_variant
                            ),
                            call_span,
                        )
                        .into());
                    }
                    return Ok(());
                }
            }
        }
    }
    // Fallback: accept any argument if pattern is not a simple type constraint.
    Ok(())
}

/// RAII guard for macro expansion — ensures leave_expansion is called on all exit paths.
struct ExpansionGuard<'a> {
    expander: &'a mut MacroEnv,
    call_site_id: CallSiteId,
}

impl Drop for ExpansionGuard<'_> {
    fn drop(&mut self) {
        self.expander.leave_expansion(self.call_site_id);
    }
}

/// Expand a macro call with SurfaceNode arguments.
///
/// Native-surface version of `expand_macro_call`. Args are `Arc<SurfaceNode>` — no
/// `expr_to_surface_node` conversion needed before quoting. The result is returned
/// as `Arc<SurfaceNode>` — no `surface_node_to_expr` → `expr_to_surface_node` round-trip.
///
/// Re-expansion at the end calls `expand_surface_expr` (not `expand_expr`).
#[allow(clippy::too_many_arguments)]
fn expand_macro_call_surface(
    macro_name: &str,
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    call_span: Span,
    dict_key: Option<&str>,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<Arc<SurfaceNode>> {
    // MH3: error on named args to macros
    if !named_args.is_empty() {
        return Err(EvalError::user_error(
            "macros do not accept named arguments".to_string(),
            call_span,
        )
        .into());
    }

    let call_site_id = if call_span == Span::origin() {
        CallSiteId::Synthetic(next_synthetic_id())
    } else {
        CallSiteId::Source {
            file_id: 0,
            offset: call_span.start.offset,
        }
    };

    let macro_metadata = env
        .get_macro(macro_name)
        .expect("macro name verified before call");
    let transformer = macro_metadata.transformer.clone();
    let params_pattern = macro_metadata.params.clone();
    let inject_default = macro_metadata.inject_default.clone();

    env.enter_expansion(call_site_id, call_span)?;
    let guard = ExpansionGuard {
        expander: env,
        call_site_id,
    };

    let opts = AstToDictOpts::default();

    // Validate annotated params (surface-native variant detection)
    let bindings: Vec<&Arc<SurfaceNode>> = match &params_pattern.expr {
        crate::ast::SurfaceExpression::LetDecl { bindings } => bindings.iter().collect(),
        _ => vec![],
    };

    let mut arg_idx = 0usize;
    for binding in &bindings {
        match &binding.expr {
            crate::ast::SurfaceExpression::Rest(_) => break,
            crate::ast::SurfaceExpression::Annotated {
                name: param_name,
                annotation,
            } => {
                if arg_idx < args.len() {
                    validate_syntax_class_surface(
                        &args[arg_idx],
                        &annotation.node,
                        param_name,
                        macro_name,
                        call_span,
                        guard.expander,
                    )?;
                }
                arg_idx += 1;
            }
            crate::ast::SurfaceExpression::VarRef { .. } => {
                arg_idx += 1;
            }
            _ => {
                arg_idx += 1;
            }
        }
    }

    // Quote each SurfaceNode argument directly (no expr_to_surface_node needed)
    let mut positional_thunks: Vec<Arc<Thunk>> = Vec::with_capacity(args.len());
    for arg in args {
        let dict_thunk = surface_node_to_dict(arg, &opts, ctx)?;
        let arg_val = eval::materialize_sync(&dict_thunk, Some(&call_span), ctx).map_err(|e| {
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
        positional_thunks.push(Arc::new(Thunk::new_materialized(deep_arg_val, call_span)));
    }

    // Thread inject: binding
    if inject_default.is_some() {
        let binding_name = dict_key
            .map(|k| k.to_string())
            .or_else(|| inject_default.clone())
            .unwrap_or_default();
        let binding_node = Arc::new(SurfaceNode {
            expr: crate::ast::SurfaceExpression::VarRef {
                name: binding_name.clone(),
                escaped: false,
            },
            span: call_span,
        });
        let binding_thunk = surface_node_to_dict(&binding_node, &opts, ctx)?;
        let binding_val =
            eval::materialize_sync(&binding_thunk, Some(&call_span), ctx).map_err(|e| {
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
        positional_thunks.push(Arc::new(Thunk::new_materialized(
            deep_binding_val,
            call_span,
        )));
    }

    // Materialize the transformer
    let transformer_val =
        eval::materialize_sync(&transformer, Some(&call_span), ctx).map_err(|e| {
            EvalError::user_error(
                format!(
                    "macro '{}' transformer failed to evaluate: {}",
                    macro_name, e.kind
                ),
                call_span,
            )
        })?;

    let result_thunk = match &transformer_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            use crate::eval_call::{invoke_function_sync as invoke_function, CallContext};
            let call_ctx = CallContext {
                params: params.as_slice(),
                body,
                positional: &positional_thunks,
                named: None,
                closure_env,
                default_env: closure_env,
                ctx,
                call_span,
                origin: Some(Arc::from(format!("macro:{}", macro_name).as_str())),
            };
            invoke_function(&call_ctx).map_err(|e| {
                EvalError::user_error(
                    format!("macro '{}' transformer call failed: {}", macro_name, e.kind),
                    call_span,
                )
            })?
        }
        other => {
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
    };

    let result_val = eval::materialize_sync(&result_thunk, Some(&call_span), ctx).map_err(|e| {
        let mut err = EvalError::user_error(
            format!(
                "macro '{}' expansion result failed to evaluate: {}",
                macro_name, e.kind
            ),
            call_span,
        );
        for frame in &e.stack {
            err.push_frame(frame.label.clone(), frame.span);
        }
        err.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        err
    })?;

    let deep_result = eval::deep_materialize(&result_val, ctx, None).map_err(|mut e| {
        e.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        e
    })?;

    #[cfg(debug_assertions)]
    debug_assert!(
        all_thunks_materialized(&deep_result, ctx),
        "macro expansion boundary violated: output contains lazy thunks"
    );

    // Convert result dict back to SurfaceNode (no bridge round-trip)
    let mut expanded_node = dict_to_surface_node(&deep_result, ctx).map_err(|e| {
        EvalError::user_error(
            format!(
                "macro '{}' returned invalid AST{}: {}",
                macro_name,
                if e.field_path.is_empty() {
                    String::new()
                } else {
                    format!(" (at field {})", e.field_path.join("."))
                },
                e.message
            ),
            call_span,
        )
    })?;

    if expanded_node.span == Span::origin() {
        expanded_node = Arc::new(SurfaceNode {
            expr: expanded_node.expr.clone(),
            span: call_span,
        });
    }

    // Record provenance
    guard.expander.provenance.insert(
        SpanKey::from(call_span),
        MacroProvenance {
            macro_name: macro_name.to_string(),
            call_site_span: call_span,
        },
    );

    // Handle splice — return as Decl(Splice(...)) for caller to handle
    // (SurfaceDeclaration::Splice is the surface equivalent of Expr::Splice)
    // No special check needed — the node will be returned and the caller handles it.

    // Drop the guard before re-expanding
    drop(guard);

    // Re-expand the result (fixpoint) — using native surface path
    expand_surface_expr(&expanded_node, env, ctx, stdlib_env)
}

/// Extract the inject: default name from a macro params SurfaceNode.
///
/// The inject: key appears as a Dict entry in the LetDecl bindings with key "inject"
/// and a VarRef value. Returns Some(name) if found, None otherwise.
fn extract_inject_default_surface(params: &Arc<SurfaceNode>) -> Option<String> {
    use crate::ast::SurfaceExpression;
    if let SurfaceExpression::LetDecl { bindings } = &params.expr {
        for binding in bindings {
            if let SurfaceExpression::Dict(entries) = &binding.expr {
                for entry in entries {
                    if let Some(key_node) = &entry.node.key {
                        if let SurfaceExpression::Str(key_str) = &key_node.expr {
                            if key_str == "inject" {
                                if let SurfaceExpression::VarRef { name, .. } =
                                    &entry.node.value.expr
                                {
                                    return Some(name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

/// Register a macro or syntax-class from a SurfaceDeclaration.
///
/// Handles `DefMacro`, `MacroDecl`, and `SyntaxClass` declarations natively.
/// Builds a `Fn` expression from params/body, evaluates it in the stdlib env
/// to obtain the transformer function, then calls `env.register_macro`.
///
/// This replaces the old `pre_scan_expr_spanned` bridge path.
fn register_surface_macro_decl(
    decl: &crate::ast::SurfaceDeclaration,
    decl_span: Span,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<()> {
    use crate::ast::{SurfaceDeclaration, SurfaceExpression};

    match decl {
        SurfaceDeclaration::MacroDecl { name, params, body } => {
            let inject_default = extract_inject_default_surface(params);

            // Convert LetDecl bindings to Vec<Spanned<Param>> for function construction.
            let fn_params: Vec<Spanned<Param>> = match &params.expr {
                SurfaceExpression::LetDecl { bindings } => {
                    let mut out = Vec::with_capacity(bindings.len());
                    for b in bindings {
                        match &b.expr {
                            SurfaceExpression::VarRef { name: n, .. } => {
                                out.push(Spanned::new(
                                    Param {
                                        name: n.clone(),
                                        annotation: None,
                                        variadic: false,
                                    },
                                    b.span,
                                ));
                            }
                            SurfaceExpression::Annotated {
                                name: n,
                                annotation,
                            } => {
                                out.push(Spanned::new(
                                    Param {
                                        name: n.clone(),
                                        annotation: Some(annotation.clone()),
                                        variadic: false,
                                    },
                                    b.span,
                                ));
                            }
                            SurfaceExpression::Rest(Some(n)) => {
                                out.push(Spanned::new(
                                    Param {
                                        name: n.clone(),
                                        annotation: None,
                                        variadic: true,
                                    },
                                    b.span,
                                ));
                            }
                            SurfaceExpression::Rest(None) => {} // anonymous spread
                            _ => {}                             // other binding forms — skip
                        }
                    }
                    out
                }
                _ => {
                    // Params is not a LetDecl — treat as single "args" param
                    vec![Spanned::new(
                        Param {
                            name: "args".to_string(),
                            annotation: None,
                            variadic: false,
                        },
                        params.span,
                    )]
                }
            };

            // Add implicit `binding` param for inject: macros
            let mut all_fn_params = fn_params;
            if inject_default.is_some() {
                all_fn_params.push(Spanned::new(
                    Param {
                        name: "binding".to_string(),
                        annotation: None,
                        variadic: false,
                    },
                    params.span,
                ));
            }

            // Evaluate the macro transformer body as a function thunk.
            let transformer_value = crate::eval::eval_surface_fn(
                all_fn_params,
                body,
                decl_span,
                Arc::clone(stdlib_env),
                ctx,
            )?;

            env.register_macro(
                name.clone(),
                Arc::clone(&transformer_value),
                params.clone(),
                inject_default,
                decl_span,
            )?;
            env.discovered_macros
                .push((name.clone(), transformer_value));
            Ok(())
        }

        SurfaceDeclaration::DefMacro { name, params, body } => {
            // DefMacro: convert LetDecl bindings directly to Vec<Spanned<Param>>.
            let param_vec: Vec<Spanned<Param>> = match &params.expr {
                SurfaceExpression::LetDecl { bindings } => bindings
                    .iter()
                    .filter_map(|b| match &b.expr {
                        SurfaceExpression::VarRef { name: n, .. } => Some(Spanned::new(
                            Param {
                                name: n.clone(),
                                annotation: None,
                                variadic: false,
                            },
                            b.span,
                        )),
                        SurfaceExpression::Annotated {
                            name: n,
                            annotation,
                        } => Some(Spanned::new(
                            Param {
                                name: n.clone(),
                                annotation: Some(annotation.clone()),
                                variadic: false,
                            },
                            b.span,
                        )),
                        SurfaceExpression::Rest(Some(rest_name)) => Some(Spanned::new(
                            Param {
                                name: rest_name.clone(),
                                annotation: None,
                                variadic: true,
                            },
                            b.span,
                        )),
                        SurfaceExpression::Rest(None) => None,
                        _ => Some(Spanned::new(
                            Param {
                                name: "???".to_string(),
                                annotation: None,
                                variadic: false,
                            },
                            b.span,
                        )),
                    })
                    .collect(),
                _ => vec![],
            };

            // Evaluate the macro transformer body as a function thunk.
            let transformer_value = crate::eval::eval_surface_fn(
                param_vec,
                body,
                decl_span,
                Arc::clone(stdlib_env),
                ctx,
            )?;

            env.register_macro(
                name.clone(),
                Arc::clone(&transformer_value),
                params.clone(),
                None,
                decl_span,
            )?;
            env.discovered_macros
                .push((name.clone(), transformer_value));
            Ok(())
        }

        SurfaceDeclaration::SyntaxClass {
            name,
            pattern,
            message,
        } => {
            env.syntax_classes.insert(
                name.clone(),
                SyntaxClassDef {
                    pattern: pattern.clone(),
                    message: message
                        .clone()
                        .unwrap_or_else(|| format!("argument must match syntax class '{}'", name)),
                },
            );
            Ok(())
        }

        _ => Ok(()),
    }
}

/// Pre-scan a SurfaceDocument to collect MacroDecl, DefMacro, and SyntaxClass declarations.
///
/// Walks `doc.items` directly — no bridge to old `File`/`Document` types needed.
/// Declaration items (DefMacro, MacroDecl, SyntaxClass) are registered directly via
/// `register_surface_macro_decl`. Expression items are recursed into via
/// `pre_scan_surface_expr`.
fn pre_scan_surface_document(
    doc: &crate::ast::SurfaceDocument,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<()> {
    for item in &doc.items {
        match item {
            crate::ast::SurfaceItem::Decl(decl_spanned) => {
                register_surface_macro_decl(
                    &decl_spanned.node,
                    decl_spanned.span,
                    env,
                    ctx,
                    stdlib_env,
                )?;
            }
            crate::ast::SurfaceItem::Expr(node) => {
                pre_scan_surface_expr(node, env, ctx, stdlib_env)?;
            }
        }
    }
    Ok(())
}

/// Pre-scan a SurfaceNode expression to discover nested macro definitions and follow includes.
///
/// Handles:
/// - `SurfaceExpression::Call` to `[include %libdir "file.llt"]` → follow include
/// - `SurfaceExpression::Decl(Box<SurfaceDeclaration>)` → register embedded macro/syntax-class
/// - All compound expression variants → recurse into children
fn pre_scan_surface_expr(
    node: &Arc<crate::ast::SurfaceNode>,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
) -> EvalResult<()> {
    use crate::ast::SurfaceExpression;

    match &node.expr {
        // Follow includes to discover macros declared inside included stdlib files.
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "include" && named_args.is_empty() && args.len() == 2 {
                    if let (
                        SurfaceExpression::VarRef { name: cap_name, .. },
                        SurfaceExpression::Str(file_name),
                    ) = (&args[0].expr, &args[1].expr)
                    {
                        if cap_name == "%libdir" {
                            pre_scan_follow_libdir_include(
                                file_name, env, ctx, stdlib_env, node.span,
                            );
                        }
                    }
                }
            }

            // Recurse into children
            pre_scan_surface_expr(func, env, ctx, stdlib_env)?;
            for arg in args {
                pre_scan_surface_expr(arg, env, ctx, stdlib_env)?;
            }
            for named_arg in named_args {
                pre_scan_surface_expr(&named_arg.node.value, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        // Embedded declaration inside an expression context — register if it is a macro/syntax-class
        SurfaceExpression::Decl(decl) => {
            register_surface_macro_decl(decl.as_ref(), node.span, env, ctx, stdlib_env)
        }

        // Compound expressions — recurse into children
        SurfaceExpression::DotAccess { expr, .. } => {
            pre_scan_surface_expr(expr, env, ctx, stdlib_env)
        }

        SurfaceExpression::Pipe { lhs, rhs } => {
            pre_scan_surface_expr(lhs, env, ctx, stdlib_env)?;
            pre_scan_surface_expr(rhs, env, ctx, stdlib_env)
        }

        SurfaceExpression::Sequential(exprs) => {
            for expr in exprs {
                pre_scan_surface_expr(expr, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                if let Some(key) = &entry.node.key {
                    pre_scan_surface_expr(key, env, ctx, stdlib_env)?;
                }
                pre_scan_surface_expr(&entry.node.value, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        SurfaceExpression::Fn { body, .. } => pre_scan_surface_expr(body, env, ctx, stdlib_env),

        SurfaceExpression::TypeAssert { expr, .. } => {
            pre_scan_surface_expr(expr, env, ctx, stdlib_env)
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            pre_scan_surface_expr(scrutinee, env, ctx, stdlib_env)?;
            for arm in arms {
                if let Some(guard) = &arm.guard {
                    pre_scan_surface_expr(guard, env, ctx, stdlib_env)?;
                }
                pre_scan_surface_expr(&arm.body, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        SurfaceExpression::Quote(inner)
        | SurfaceExpression::Unquote(inner)
        | SurfaceExpression::UnquoteSplice(inner) => {
            pre_scan_surface_expr(inner, env, ctx, stdlib_env)
        }

        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            for binding in bindings {
                pre_scan_surface_expr(binding, env, ctx, stdlib_env)?;
            }
            Ok(())
        }

        SurfaceExpression::CaseArm { pattern, body } => {
            pre_scan_surface_expr(pattern, env, ctx, stdlib_env)?;
            pre_scan_surface_expr(body, env, ctx, stdlib_env)
        }

        SurfaceExpression::TypeApp { func, arg } => {
            pre_scan_surface_expr(func, env, ctx, stdlib_env)?;
            pre_scan_surface_expr(arg, env, ctx, stdlib_env)
        }

        // Leaf nodes — no children to scan
        SurfaceExpression::Int(_)
        | SurfaceExpression::Float(_)
        | SurfaceExpression::Bool(_)
        | SurfaceExpression::Str(_)
        | SurfaceExpression::VarRef { .. }
        | SurfaceExpression::Annotated { .. }
        | SurfaceExpression::Rest(_)
        | SurfaceExpression::Placeholder
        | SurfaceExpression::Error(_) => Ok(()),
    }
}

/// Follow a `[include %libdir "file.llt"]` call during pre-scan to discover macros
/// declared inside the included file.
///
/// Silently ignores errors (file not found, parse failure, etc.) — the actual error
/// will surface during eval when the include is executed.
///
/// # Infinite-recursion guard
///
/// A thread-local set tracks which libdir files are currently being pre-scanned.
/// If a file is re-encountered (e.g., via `[include %libdir "syntax.llt"]` →
/// `[include %libdir "ast.llt"]` → `[include %libdir "syntax.llt"]` cycle), the
/// second encounter is silently skipped.
fn pre_scan_follow_libdir_include(
    file_name: &str,
    env: &mut MacroEnv,
    ctx: &Arc<EvalContext>,
    stdlib_env: &Arc<RwLock<Environment>>,
    _call_span: crate::ast::Span,
) {
    // Guard: do not read files if no_fs is set (LSP security)
    if ctx.config.no_fs {
        return;
    }

    std::thread_local! {
        static PRESCAN_INCLUDE_STACK: RefCell<HashSet<String>> =
            RefCell::new(HashSet::new());
    }

    // Guard against recursive includes
    let already_scanning = PRESCAN_INCLUDE_STACK.with(|s| s.borrow().contains(file_name));
    if already_scanning {
        return;
    }

    // Find libdir and open it as a DirCap (cap-std RESOLVE_BENEATH enforced)
    let libdir_path = match crate::find_libdir_path() {
        Some(p) => p,
        None => return,
    };
    #[allow(clippy::disallowed_methods)]
    let libdir =
        match cap_std::fs::Dir::open_ambient_dir(&libdir_path, cap_std::ambient_authority()) {
            Ok(d) => d,
            Err(_) => return,
        };

    // Open and read the file using cap-std (RESOLVE_BENEATH prevents traversal)
    let source = match libdir.open(file_name).and_then(|mut f| {
        use std::io::Read;
        let mut s = String::new();
        f.read_to_string(&mut s)?;
        Ok(s)
    }) {
        Ok(s) => s,
        Err(_) => return,
    };

    // Parse the file
    let parsed = match crate::parser::parse(&source) {
        Ok(f) => f,
        Err(_) => return,
    };

    // Push this file onto the recursion guard before scanning
    PRESCAN_INCLUDE_STACK.with(|s| s.borrow_mut().insert(file_name.to_string()));

    // Pre-scan all documents in the parsed file — walk SurfaceProgram directly
    for doc in &parsed.program.documents {
        let _ = pre_scan_surface_document(&doc.node, env, ctx, stdlib_env);
    }

    // Pop the recursion guard
    PRESCAN_INCLUDE_STACK.with(|s| s.borrow_mut().remove(file_name));
}

/// Debug assertion helper: check that all thunks in a value tree are materialized.
/// Used to validate the macro expansion boundary invariant: no lazy thunks cross
/// from the stdlib arena to the expansion arena or vice versa.
#[cfg(debug_assertions)]
fn all_thunks_materialized(val: &Value, ctx: &Arc<EvalContext>) -> bool {
    match val {
        Value::Dict(map) => {
            for thunk_id in map.values() {
                let thunk = ctx.get_thunk(*thunk_id);
                // Check if thunk is materialized
                if let Some(inner_val) = thunk.try_get_materialized() {
                    // Recursively check the materialized value
                    if !all_thunks_materialized(&inner_val, ctx) {
                        return false;
                    }
                } else {
                    return false;
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
            if let Some(head_val) = head_thunk.try_get_materialized() {
                if !all_thunks_materialized(&head_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            // Check tail
            let tail_thunk = ctx.get_thunk(*tail_id);
            if let Some(tail_val) = tail_thunk.try_get_materialized() {
                if !all_thunks_materialized(&tail_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            true
        }
        Value::Proxy { handler } => {
            // Check handler
            let handler_thunk = ctx.get_thunk(*handler);
            if let Some(handler_val) = handler_thunk.try_get_materialized() {
                if !all_thunks_materialized(&handler_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            true
        }
        Value::Overlay(left, right) => {
            // Check left
            let left_thunk = ctx.get_thunk(*left);
            if let Some(left_val) = left_thunk.try_get_materialized() {
                if !all_thunks_materialized(&left_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            // Check right
            let right_thunk = ctx.get_thunk(*right);
            if let Some(right_val) = right_thunk.try_get_materialized() {
                if !all_thunks_materialized(&right_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            true
        }
        Value::Variant {
            payload: Some(id), ..
        } => {
            let thunk = ctx.get_thunk(*id);
            if let Some(inner_val) = thunk.try_get_materialized() {
                if !all_thunks_materialized(&inner_val, ctx) {
                    return false;
                }
            } else {
                return false;
            }
            true
        }
        Value::Variant { payload: None, .. } => true,
        // All other values (primitives, functions, capabilities, etc.) have no thunks to check
        _ => true,
    }
}
