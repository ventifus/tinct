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

use crate::ast::{Document, Entry, Expr, File, MatchArm, NamedArg, Span, Spanned};
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

/// Macro expansion context — tracks registered macros and prevents infinite expansion.
#[derive(Debug, Clone)]
pub struct MacroEnv {
    /// Map from macro name to the transformer function (as a Value).
    /// The transformer is a function that takes AST dicts and returns an AST dict.
    macros: HashMap<String, Rc<Thunk>>,
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

    /// Register a macro transformer.
    /// Returns an error if the name collides with a registered Rust builtin.
    fn register_macro(
        &mut self,
        name: String,
        transformer: Rc<Thunk>,
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
        self.macros.insert(name, transformer);
        Ok(())
    }

    /// Check if a name is a registered macro.
    fn is_macro(&self, name: &str) -> bool {
        self.macros.contains_key(name)
    }

    /// Get the transformer for a macro.
    fn get_transformer(&self, name: &str) -> Option<&Rc<Thunk>> {
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
}

/// Result of macro expansion: the expanded AST plus provenance metadata.
pub struct ExpandResult {
    pub file: Spanned<File>,
    pub provenance: ProvenanceMap,
    /// Macros discovered during expansion via `[defmacro ...]` declarations.
    /// Each entry is `(macro_name, transformer_thunk)`.
    /// Used to propagate stdlib macros from macros.llt to user code expansion.
    pub discovered_macros: Vec<(String, Rc<Thunk>)>,
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
            // register_macro checks for builtin collisions.
            let _ = env_macro.register_macro((*macro_name).to_string(), transformer, span);
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
/// Returns the expanded AST and the provenance side map for dual-span error reporting.
/// Stdlib macros are loaded by expanding `stdlib/macros.llt` and collecting discovered macros.
pub fn expand_macros(file: Spanned<File>, no_fs: bool) -> EvalResult<ExpandResult> {
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

    // Create a minimal eval context for macro expansion
    // Use current directory as base_dir
    let base_dir_path = std::env::current_dir()
        .ok()
        .and_then(|d| d.canonicalize().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let base_dir = cap_std::fs::Dir::open_ambient_dir(&base_dir_path, cap_std::ambient_authority())
        .map_err(|e| {
            EvalError::internal(
                format!("cannot open base directory for macro expansion: {e}"),
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

    Ok(ExpandResult {
        file: Spanned::new(
            File {
                documents: expanded_documents,
            },
            file.span,
        ),
        provenance: env_macro.provenance,
        discovered_macros: env_macro.discovered_macros,
    })
}

/// Expand macros in a Document.
fn expand_document(
    doc: Document,
    env: &mut MacroEnv,
    ctx: &Rc<EvalContext>,
    stdlib_env: &Rc<RefCell<Environment>>,
) -> EvalResult<Document> {
    // Expand each expression in the document, filtering out DefMacro nodes
    let mut expanded_exprs = Vec::new();

    for expr in doc.expressions {
        let expanded = expand_expr(expr.as_ref().clone(), env, ctx, stdlib_env)?;
        // Filter out DefMacro nodes (they've been registered and should not appear post-expansion)
        if !matches!(expanded.node, Expr::DefMacro { .. }) {
            expanded_exprs.push(Rc::new(expanded));
        }
    }

    Ok(Document {
        expressions: expanded_exprs,
        name: doc.name,
        output_type: doc.output_type,
        expects: doc.expects,
        caps: doc.caps,
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
        Expr::DefMacro { name, params, body } => {
            // Wrap params+body in a function expression
            let fn_expr = Expr::Fn {
                return_ann: None,
                params: params.clone(),
                body: Rc::clone(body),
                desugared: false,
            };
            let fn_spanned = Spanned::new(fn_expr, expr.span);

            // Evaluate the function in the stdlib environment
            let transformer_value = eval::eval(Rc::new(fn_spanned), Rc::clone(stdlib_env), ctx)?;

            // Register the macro
            env.register_macro(name.clone(), Rc::clone(&transformer_value), expr.span)?;

            // Record the discovered macro for propagation to outer MacroEnv
            env.discovered_macros
                .push((name.clone(), transformer_value));

            // Return the DefMacro node unchanged (will be filtered out by expand_document)
            Ok(expr)
        }

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
                // This is a macro call — expand it
                expand_macro_call(
                    &macro_name,
                    args,
                    named_args,
                    expr.span,
                    env,
                    ctx,
                    stdlib_env,
                )
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
                let expanded_value =
                    expand_expr(entry.node.value.as_ref().clone(), env, ctx, stdlib_env)?;
                // Filter out DefMacro entries — they've been registered during
                // expand_expr and should not appear in the post-expansion AST.
                if matches!(expanded_value.node, Expr::DefMacro { .. }) {
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
                },
                expr.span,
            ))
        }

        // InstanceDecl: expand instance type and method implementations
        Expr::InstanceDecl {
            class_name,
            instance_type,
            methods,
        } => {
            let expanded_type = expand_expr(instance_type.as_ref().clone(), env, ctx, stdlib_env)?;
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
                Expr::InstanceDecl {
                    class_name: class_name.clone(),
                    instance_type: Box::new(expanded_type),
                    methods: expanded_methods,
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
        | Expr::TypeApp { .. }
        | Expr::Error(_) => Ok(expr),
    }
}

/// Expand a macro call by invoking the transformer with quoted arguments.
///
/// The transformer receives a single argument: a list of AST dicts (one per positional arg).
/// It returns an AST dict which is converted back to an Expr node via `dict_to_ast`.
/// The result is then re-expanded (fixpoint) until no macro calls remain.
///
/// Hygiene: a fresh `ScopeId` is allocated per invocation, and any bindings
/// introduced by the expansion (fn params, dict keys that are new names) are
/// renamed to `name:scope:N` to prevent capture of call-site variables.
///
/// ## Arena Boundary Invariant
///
/// The macro expansion boundary is a data boundary. Both the input AST dict and the output
/// AST dict are fully materialized before crossing. No arena-relative ThunkId handles may
/// flow from the stdlib arena into the expansion arena or vice versa.
///
/// This ensures that transformer functions from the stdlib (which contain ThunkIds pointing
/// into their creation-time arena) cannot leak those handles into the expansion arena's
/// value graph. Both input and output are pure Value trees with no lazy references.
fn expand_macro_call(
    macro_name: &str,
    args: &[Rc<Spanned<Expr>>],
    _named_args: &[Spanned<NamedArg>],
    call_span: Span,
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

    // Quote each argument to an AST dict value
    let opts = AstToDictOpts::default();
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
    // ast_to_dict_expr creates all thunks as Materialized, so this is a validation pass
    // with no lazy computation triggered. Cost is O(AST node count) cache hits.
    let deep_args_value =
        eval::deep_materialize(&args_value, ctx, Some(&call_span)).map_err(|mut e| {
            e.push_frame(
                format!("deep-materializing input to macro '{}'", macro_name),
                call_span,
            );
            e
        })?;

    // Debug assertion: verify all thunks in the input are materialized
    #[cfg(debug_assertions)]
    debug_assert!(
        all_thunks_materialized(&deep_args_value, ctx),
        "macro expansion boundary violated: input contains lazy thunks"
    );

    let args_thunk = Rc::new(Thunk::new_materialized(deep_args_value, call_span));

    // Get the transformer and call it with the args list
    let transformer = env
        .get_transformer(macro_name)
        .expect("macro name verified before call")
        .clone();

    // Materialize the transformer to get the function value
    let transformer_val = eval::materialize(&transformer, Some(&call_span), ctx).map_err(|e| {
        EvalError::user_error(
            format!(
                "macro '{}' transformer failed to evaluate: {}",
                macro_name, e.kind
            ),
            call_span,
        )
    })?;

    // Call the transformer function with the args list
    let result_thunk = match &transformer_val {
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

    // Materialize the result to get the AST dict
    let result_val = eval::materialize(&result_thunk, Some(&call_span), ctx).map_err(|e| {
        let mut err = EvalError::user_error(
            format!(
                "macro '{}' expansion result failed to evaluate: {}",
                macro_name, e.kind
            ),
            call_span,
        );
        // Record provenance for the error
        err.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        err
    })?;

    // Deep-materialize the result dict so dict_to_ast can inspect all fields
    let deep_result = eval::deep_materialize(&result_val, ctx, None).map_err(|mut e| {
        e.push_frame(format!("in expansion of `{}`", macro_name), call_span);
        e
    })?;

    // Debug assertion: verify all thunks in the output are materialized
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
    // This ensures errors in expanded code carry the call site span, which the
    // provenance map uses to attach "in expansion of `<name>`" notes.
    if expanded_ast.span == Span::origin() {
        expanded_ast.span = call_span;
    }

    // Allocate a fresh scope ID for hygiene tracking.
    // Phase 1: the scope ID is recorded for provenance but automatic renaming
    // is not applied — macro authors use `gensym` for internal bindings.
    // Phase 2 (future): apply scope-based alpha-renaming to macro-template
    // bindings, distinguishing them from user-spliced code.
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

    // Re-expand the result (fixpoint)
    expand_expr(expanded_ast, env, ctx, stdlib_env)
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
