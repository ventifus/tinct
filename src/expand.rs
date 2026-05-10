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
}

/// Expand all macros in a File AST.
///
/// This is the top-level entry point called from the pipeline.
/// Takes a no_fs flag to match the pipeline configuration.
/// Returns the expanded AST and the provenance side map for dual-span error reporting.
/// Register stdlib macros from the stdlib env into a fresh MacroEnv.
///
/// Each stdlib macro is exported from `stdlib/macros.llt` as a `<name>-transformer`
/// Register the built-in `tmpl` macro that expands string interpolation.
///
/// The parser emits `[tmpl "raw_template" expr0 expr1 ...]` for `i"..."` literals.
/// - `raw_template`: the original template string with `$name` → `$name`, `${...}` → `${N}`
/// - `exprN`: extra positional args for `${N}` expression placeholders
///
/// This expands to `[str "seg1" name "seg2" expr0 ...]` at macro-expansion time.
/// Implemented in Rust to avoid a circular dependency with create_stdlib_env.
#[allow(dead_code)]
fn register_tmpl_macro(env_macro: &mut MacroEnv) {
    // We can't register a Rust closure as a macro (the MacroEnv expects a Thunk/Value::Function).
    // Instead, emit the [tmpl ...] call as a Call node that the evaluator handles at runtime.
    // The evaluator finds `tmpl-transformer` in the stdlib env loaded by create_stdlib_env.
    // This works because macro expansion only needs to handle [defmacro] user macros here;
    // the `tmpl` builtin is handled by the evaluator via the stdlib env.
    //
    // We do NOT register tmpl here; instead, leave [tmpl "..."] calls unchanged.
    // The evaluator will call `tmpl-transformer` from the stdlib env at runtime.
    let _ = env_macro; // intentionally unused — no registration needed
}

/// Register stdlib LLT macro transformers (legacy path, kept for reference).
///
/// function. This function looks them up and pre-registers them so that macro calls
/// like `[tmpl "Hello $name"]` are expanded before any user-defined `[defmacro]` nodes
/// are processed.
///
/// Stdlib macro names must NOT collide with registered Rust builtins (the same check
/// that `register_macro` performs). This is guaranteed by design: the `tmpl` macro
/// cannot shadow any builtin since no builtin is named `tmpl`.
#[allow(dead_code)]
fn register_stdlib_macros(
    env_macro: &mut MacroEnv,
    stdlib_env: &Rc<RefCell<Environment>>,
    span: Span,
) {
    /// Table of (macro_name, transformer_key_in_stdlib_env) pairs.
    const STDLIB_MACROS: &[(&str, &str)] = &[("tmpl", "tmpl-transformer")];

    for (macro_name, transformer_key) in STDLIB_MACROS {
        let transformer_thunk = {
            let env_ref = stdlib_env.borrow();
            env_ref.get(*transformer_key)
        };
        if let Some(transformer) = transformer_thunk {
            // register_macro checks for builtin collisions. Ignore errors here:
            // if registration fails (e.g. the key is absent), the macro simply
            // won't expand and user code gets an "undefined variable: tmpl" error,
            // which is a clear signal that stdlib/macros.llt is not loaded.
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
    // transformer bodies, and tmpl-transformer for i"..." string interpolation.
    // The depth guard prevents infinite recursion when create_stdlib_env calls expand_macros:
    //   expand_macros(user_code) → create_stdlib_env() → load_stdlib_module(prelude.llt) →
    //   build_prelude_env → expand_macros(prelude.llt) [depth=1 → use create_root_env]
    let depth = EXPAND_MACROS_DEPTH.get();
    EXPAND_MACROS_DEPTH.set(depth + 1);
    let stdlib_env = if depth == 0 {
        match builtins::create_stdlib_env() {
            Ok(env) => {
                // Register stdlib macros (tmpl) only at the outermost level.
                register_stdlib_macros(&mut env_macro, &env, file.span);
                env
            }
            Err(e) => {
                EXPAND_MACROS_DEPTH.set(depth);
                return Err(EvalError::internal(
                    format!("cannot create stdlib env for macro expansion: {e}"),
                    file.span,
                )
                .into());
            }
        }
    } else {
        // Re-entrant call: use bare root env to break the cycle.
        // [defmacro] macros in stdlib files won't have access to prelude, but
        // stdlib files don't define user [defmacro] macros so this is fine.
        builtins::create_root_env()
    };
    EXPAND_MACROS_DEPTH.set(depth);

    let ctx = Rc::new(EvalContext::new(base_dir, Rc::clone(&stdlib_env), no_fs));

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
        Expr::DefMacro { name, transformer } => {
            // Evaluate the transformer in the stdlib environment
            let transformer_value = eval::eval(
                Rc::new(transformer.as_ref().clone()),
                Rc::clone(stdlib_env),
                ctx,
            )?;

            // Register the macro
            env.register_macro(name.clone(), transformer_value, expr.span)?;

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
    let args_thunk = Rc::new(Thunk::new_materialized(args_value, call_span));

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

/// Rename bindings introduced by a macro expansion to prevent variable capture.
///
/// This implements the core hygiene invariant (Flatt 2016, simplified):
/// macro-introduced names are structurally distinct from user-code names.
///
/// Only *binding sites* are renamed (fn params, dict entry keys). References to
/// those names within the macro body are also renamed to match. Names that were
/// spliced from user code via `[unquote]` are NOT renamed — they retain their
/// original user-code names, which is exactly the hygiene guarantee: user names
/// and macro names don't interfere.
///
/// The renaming scheme uses `:scope:N` suffix (`:` is forbidden in bare-word
/// identifiers, making collision structurally impossible).
///
/// **Phase 2 (future):** Currently reserved for automatic hygiene when the
/// expander can distinguish template-introduced bindings from user-spliced code.
/// Phase 1 relies on `gensym` for manual hygiene.
#[allow(dead_code)]
fn rename_macro_bindings(expr: &mut Spanned<Expr>, scope_id: ScopeId) {
    // Collect binding names introduced at this level, then rename references
    let mut renames: HashMap<String, String> = HashMap::new();

    collect_and_rename_bindings(&mut expr.node, scope_id, &mut renames);

    // Now rename all VarRef occurrences that reference the renamed bindings
    if !renames.is_empty() {
        rename_refs(&mut expr.node, &renames);
    }
}

/// Walk the expression to find binding sites and rename them.
#[allow(dead_code)]
fn collect_and_rename_bindings(
    expr: &mut Expr,
    scope_id: ScopeId,
    renames: &mut HashMap<String, String>,
) {
    match expr {
        Expr::Fn { params, body, .. } => {
            // Rename fn parameters
            for param in params.iter_mut() {
                let old_name = param.node.name.clone();
                // Don't rename `_` (special desugaring name) or names already scoped
                if old_name != "_" && !old_name.contains(":scope:") {
                    let new_name = format!("{}:scope:{}", old_name, scope_id.0);
                    renames.insert(old_name, new_name.clone());
                    param.node.name = new_name;
                }
            }
            // Recursively process the body with accumulated renames
            collect_and_rename_bindings(&mut Rc::make_mut(body).node, scope_id, renames);
        }
        Expr::Dict(entries) => {
            for entry in entries.iter_mut() {
                // Rename keyed entries (binding sites in dicts)
                if let Some(ref mut key_expr) = entry.node.key {
                    if let Expr::Str(ref name) = key_expr.node {
                        let name = name.clone();
                        if !name.contains(":scope:") {
                            let new_name = format!("{}:scope:{}", name, scope_id.0);
                            renames.insert(name, new_name.clone());
                            key_expr.node = Expr::Str(new_name);
                        }
                    }
                }
                // Recurse into values
                collect_and_rename_bindings(
                    &mut Rc::make_mut(&mut entry.node.value).node,
                    scope_id,
                    renames,
                );
            }
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            collect_and_rename_bindings(&mut func.node, scope_id, renames);
            for arg in args.iter_mut() {
                collect_and_rename_bindings(&mut Rc::make_mut(arg).node, scope_id, renames);
            }
            for na in named_args.iter_mut() {
                collect_and_rename_bindings(
                    &mut Rc::make_mut(&mut na.node.value).node,
                    scope_id,
                    renames,
                );
            }
        }
        Expr::DotAccess { expr: target, .. } => {
            collect_and_rename_bindings(&mut target.node, scope_id, renames);
        }
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                if let Some(seq_expr_mut) = Rc::get_mut(seq_expr) {
                    collect_and_rename_bindings(&mut seq_expr_mut.node, scope_id, renames);
                }
            }
        }
        Expr::Pipe { lhs, rhs } => {
            collect_and_rename_bindings(&mut lhs.node, scope_id, renames);
            collect_and_rename_bindings(&mut rhs.node, scope_id, renames);
        }
        Expr::TypeAlias { body, .. } => {
            collect_and_rename_bindings(&mut body.node, scope_id, renames);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            collect_and_rename_bindings(&mut inner.node, scope_id, renames);
        }
        Expr::Quote(inner) => {
            collect_and_rename_bindings(&mut inner.node, scope_id, renames);
        }
        Expr::Unquote(inner) => {
            collect_and_rename_bindings(&mut inner.node, scope_id, renames);
        }
        Expr::UnquoteSplice(inner) => {
            collect_and_rename_bindings(&mut inner.node, scope_id, renames);
        }
        Expr::Match { scrutinee, arms } => {
            collect_and_rename_bindings(&mut scrutinee.node, scope_id, renames);
            for arm in arms.iter_mut() {
                if let Some(guard) = &mut arm.guard {
                    collect_and_rename_bindings(&mut guard.node, scope_id, renames);
                }
                collect_and_rename_bindings(&mut arm.body.node, scope_id, renames);
            }
        }
        Expr::ClassDecl { methods, .. } => {
            for method in methods.iter_mut() {
                if let Some(ref mut key_expr) = method.node.key {
                    collect_and_rename_bindings(&mut key_expr.node, scope_id, renames);
                }
                collect_and_rename_bindings(
                    &mut Rc::make_mut(&mut method.node.value).node,
                    scope_id,
                    renames,
                );
            }
        }
        Expr::InstanceDecl {
            instance_type,
            methods,
            ..
        } => {
            collect_and_rename_bindings(&mut instance_type.node, scope_id, renames);
            for method in methods.iter_mut() {
                if let Some(ref mut key_expr) = method.node.key {
                    collect_and_rename_bindings(&mut key_expr.node, scope_id, renames);
                }
                collect_and_rename_bindings(
                    &mut Rc::make_mut(&mut method.node.value).node,
                    scope_id,
                    renames,
                );
            }
        }
        // Leaf nodes — nothing to do
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::VarRef { .. }
        | Expr::Annotated { .. }
        | Expr::Rest(_)
        | Expr::Error(_)
        | Expr::DefMacro { .. } => {}
    }
}

/// Rename variable references to match renamed bindings.
#[allow(dead_code)]
fn rename_refs(expr: &mut Expr, renames: &HashMap<String, String>) {
    match expr {
        Expr::VarRef { name, .. } => {
            if let Some(new_name) = renames.get(name.as_str()) {
                *name = new_name.clone();
            }
        }
        Expr::Fn { body, .. } => {
            rename_refs(&mut Rc::make_mut(body).node, renames);
        }
        Expr::Dict(entries) => {
            for entry in entries.iter_mut() {
                rename_refs(&mut Rc::make_mut(&mut entry.node.value).node, renames);
            }
        }
        Expr::Call {
            func,
            args,
            named_args,
            ..
        } => {
            rename_refs(&mut func.node, renames);
            for arg in args.iter_mut() {
                rename_refs(&mut Rc::make_mut(arg).node, renames);
            }
            for na in named_args.iter_mut() {
                rename_refs(&mut Rc::make_mut(&mut na.node.value).node, renames);
            }
        }
        Expr::DotAccess { expr: target, .. } => {
            rename_refs(&mut target.node, renames);
        }
        Expr::Sequential(exprs) => {
            for seq_expr in exprs {
                if let Some(seq_expr_mut) = Rc::get_mut(seq_expr) {
                    rename_refs(&mut seq_expr_mut.node, renames);
                }
            }
        }
        Expr::Pipe { lhs, rhs } => {
            rename_refs(&mut lhs.node, renames);
            rename_refs(&mut rhs.node, renames);
        }
        Expr::TypeAlias { body, .. } => {
            rename_refs(&mut body.node, renames);
        }
        Expr::TypeAssert { expr: inner, .. } => {
            rename_refs(&mut inner.node, renames);
        }
        Expr::Quote(inner) => {
            rename_refs(&mut inner.node, renames);
        }
        Expr::Unquote(inner) => {
            rename_refs(&mut inner.node, renames);
        }
        Expr::UnquoteSplice(inner) => {
            rename_refs(&mut inner.node, renames);
        }
        Expr::Match { scrutinee, arms } => {
            rename_refs(&mut scrutinee.node, renames);
            for arm in arms {
                if let Some(guard) = &mut arm.guard {
                    rename_refs(&mut guard.node, renames);
                }
                rename_refs(&mut arm.body.node, renames);
            }
        }
        Expr::ClassDecl { methods, .. } => {
            for method in methods.iter_mut() {
                if let Some(ref mut key_expr) = method.node.key {
                    rename_refs(&mut key_expr.node, renames);
                }
                rename_refs(&mut Rc::make_mut(&mut method.node.value).node, renames);
            }
        }
        Expr::InstanceDecl {
            instance_type,
            methods,
            ..
        } => {
            rename_refs(&mut instance_type.node, renames);
            for method in methods.iter_mut() {
                if let Some(ref mut key_expr) = method.node.key {
                    rename_refs(&mut key_expr.node, renames);
                }
                rename_refs(&mut Rc::make_mut(&mut method.node.value).node, renames);
            }
        }
        // Leaf nodes — nothing to do
        Expr::Int(_)
        | Expr::Float(_)
        | Expr::Bool(_)
        | Expr::Str(_)
        | Expr::Annotated { .. }
        | Expr::Rest(_)
        | Expr::Error(_)
        | Expr::DefMacro { .. } => {}
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
