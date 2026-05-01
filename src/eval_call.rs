//! Function call evaluation: argument binding, default parameters, and variadic support.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Expr, NamedArg, Param, Span, Spanned};
use crate::error::{ArityBound, EvalError, EvalResult};
use crate::value::{Environment, Key, Thunk, Value};

// Import eval function and context from eval module
// Note: this creates a circular dependency, but it's safe because
// eval.rs imports invoke_function/CallContext from this module, while
// this module imports eval/EvalContext from eval.rs. Neither module's
// initialization depends on the other.
use crate::eval::{eval, EvalContext};

const DEFAULT_ANNOTATION_KEY: &str = "default";

/// Extract a human-readable label from a function expression for stack frames.
///
/// When the expression is a desugared `$_` lambda (synthesized by `desugar.rs`),
/// " (auto-generated lambda)" is appended so stack traces distinguish sugar-generated
/// closures from user-written `[fn ...]` forms.
pub(crate) fn func_label(expr: &Expr) -> Cow<'static, str> {
    // Fast path for common VarRef case: build label directly to avoid intermediate format! in func_path
    match expr {
        Expr::VarRef(name) => Cow::Owned(format!("call ${name}")),
        Expr::Fn {
            desugared: true, ..
        } => Cow::Owned(format!("call {} (auto-generated lambda)", func_path(expr))),
        _ => Cow::Owned(format!("call {}", func_path(expr))),
    }
}

pub(crate) fn func_path(expr: &Expr) -> String {
    match expr {
        Expr::VarRef(name) => format!("${name}"),
        Expr::DotAccess { expr: inner, field } => format!("{}.{field}", func_path(&inner.node)),
        _ => "<anonymous>".to_string(),
    }
}

/// Evaluate a call expression: return a PendingCall thunk that defers function dispatch.
///
/// Now returns a PendingCall thunk; function dispatch is deferred to the PendingCallDispatch
/// continuation in `run()`. This diverges from the [FORCE-CALL] rule in doc/08-evaluation.md,
/// which described the old eager dispatch model — the rule has been updated to reflect the
/// current lazy dispatch design that enables unlimited tail-call optimization.
///
/// Neither the function nor any argument is materialized here. The PendingCallDispatch
/// continuation forces `func_thunk` when the result is needed, then dispatches to
/// `invoke_function` (user functions) or the builtin handler.
pub(crate) fn eval_call(
    func_expr: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    call_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    // Evaluate the function as a thunk (lazy — no materialization).
    let func_thunk = eval(func_expr, Rc::clone(env), ctx, depth + 1)?;

    // Wrap arguments as unevaluated thunks (lazy). This ensures expressions
    // like $xs[$i] in unselected $if branches are never evaluated.
    let pos_thunks: Vec<Rc<Thunk>> = args
        .iter()
        .map(|arg| {
            Rc::new(Thunk::new_unevaluated(
                Rc::new((*arg).clone()),
                Rc::clone(env),
                Rc::clone(ctx),
                arg.span,
            ))
        })
        .collect();
    let named_thunks = if named_args.is_empty() {
        IndexMap::new()
    } else {
        let mut m = IndexMap::with_capacity(named_args.len());
        for na in named_args {
            m.insert(
                na.node.name.clone(),
                Rc::new(Thunk::new_unevaluated(
                    Rc::new(na.node.value.clone()),
                    Rc::clone(env),
                    Rc::clone(ctx),
                    na.node.value.span,
                )),
            );
        }
        m
    };

    // Return PendingCall thunk — function dispatch happens iteratively in run().
    // PendingCallDispatch forces func_thunk, matches Function vs Builtin, and invokes.
    // For tail-recursive functions, the loop depth stays constant — prerequisite for unlimited TCO via CEK machine.
    Ok(Rc::new(Thunk::new_pending_call(
        func_thunk,
        pos_thunks,
        named_thunks,
        *call_span,
        Rc::clone(env), // caller_env: used for default param evaluation
        *call_span,
        func_label(&func_expr.node),
        Rc::clone(ctx),
    )))
}

/// Arguments for invoking a user-defined function.
///
/// `default_env` is the environment used to evaluate default expressions for
/// optional params. For normal calls this is the caller's environment; for
/// `apply` it is the closure environment.
pub struct CallContext<'a> {
    pub params: &'a [Param],
    pub body: &'a Rc<Spanned<Expr>>,
    pub closure_env: &'a Rc<RefCell<Environment>>,
    pub positional: &'a [Rc<Thunk>],
    pub named: &'a IndexMap<String, Rc<Thunk>>,
    pub default_env: &'a Rc<RefCell<Environment>>,
    pub call_span: Span,
    pub depth: usize,
    /// Label for stack traces (e.g. "call $f"). Set by `eval_call`
    /// when the function expression has a recognizable name.
    pub origin: Cow<'static, str>,
    pub ctx: &'a Rc<EvalContext>,
}

/// Invoke a user-defined function with pre-evaluated thunks.
///
/// Binds positional and named args to function params (respecting defaults and
/// variadics), then wraps the body as an unevaluated thunk. This is the shared
/// call path for both `eval_call` and `builtin_apply`.
pub fn invoke_function(ctx: &CallContext) -> EvalResult<Rc<Thunk>> {
    let call_env = bind_args_thunks(
        ctx.params,
        ctx.positional,
        ctx.named,
        ctx.default_env,
        ctx.closure_env,
        ctx.ctx,
        &ctx.call_span,
        ctx.depth,
    )?;
    let mut thunk = Thunk::new_unevaluated(
        Rc::clone(ctx.body),
        call_env,
        Rc::clone(ctx.ctx),
        ctx.call_span,
    );
    if !ctx.origin.is_empty() {
        thunk = thunk.with_origin(ctx.origin.clone());
    }
    Ok(Rc::new(thunk))
}

/// Bind pre-evaluated thunks to function parameters. Returns the new call environment.
///
/// Handles positional args, named args (params with `default:` annotation),
/// and variadic params (`...name`).
pub(crate) fn bind_args_thunks(
    params: &[Param],
    positional: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    default_env: &Rc<RefCell<Environment>>,
    closure_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    call_span: &Span,
    depth: usize,
) -> EvalResult<Rc<RefCell<Environment>>> {
    // TODO(iterative-eval): frame reuse is unsafe with shared Rc<RefCell<Environment>>
    // (closure_env mutations visible to re-entrant callers via shared Rc); safe post-flat-env.
    let call_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        closure_env,
    ))));

    // BIND-SPLIT: Separate the variadic param (if any) from regular params
    let (regular_params, variadic_param) = split_variadic(params);
    let max_positional = regular_params.len();

    // BIND-ARITY: Per-parameter coverage check (Kotlin model)
    // Each required parameter must be reachable via positional index OR named argument
    let mut required_count = 0;
    for (i, param) in regular_params.iter().enumerate() {
        let is_required = get_default(param).is_none();
        if is_required {
            required_count += 1;
            let covered_positionally = i < positional.len();
            let covered_by_name = named.contains_key(&param.name);
            if !covered_positionally && !covered_by_name {
                return Err(
                    EvalError::missing_required_param(param.name.clone(), *call_span).into(),
                );
            }
        }
    }

    // Without variadic: positional args must not exceed max_positional
    if variadic_param.is_none() && positional.len() > max_positional {
        // Use Range when there are optional params, Exact when all params are required
        let expected = if required_count < max_positional {
            ArityBound::Range(required_count, max_positional)
        } else {
            ArityBound::Exact(max_positional)
        };
        return Err(EvalError::arity_mismatch_bound(expected, positional.len(), *call_span).into());
    }

    // BIND-POSITIONAL: Bind args to params following C-PRIORITY chain
    for (i, param) in regular_params.iter().enumerate() {
        let thunk = if i < positional.len() {
            // Case (i): positional arg at index i
            Rc::clone(&positional[i])
        } else if let Some(named_thunk) = named.get(&param.name) {
            // Case (ii): named arg fills gap beyond positional args
            // (Kotlin model: ANY param can be named, not just optional)
            Rc::clone(named_thunk)
        } else if let Some(default_val) = get_default(param) {
            // Case (iii): use default value
            eval(&default_val, Rc::clone(default_env), ctx, depth + 1)?
        } else {
            // Unreachable: BIND-ARITY guarantees every required param is covered
            unreachable!(
                "BIND-ARITY should have caught missing required param '{}'",
                param.name
            );
        };
        call_env.borrow_mut().insert(param.name.clone(), thunk);
    }

    // BIND-NAMED: Validation only (all bindings were already done in BIND-POSITIONAL)
    for (name, _) in named {
        // Single scan: C-NO-OVERLAP and C-NAMED-VALID in one position() call
        match regular_params.iter().position(|p| &p.name == name) {
            Some(idx) if idx < positional.len() => {
                // C-NO-OVERLAP: named arg targets a positionally-bound parameter
                return Err(Box::new(EvalError::named_arg_conflict(
                    name.clone(),
                    *call_span,
                )));
            }
            None => {
                // C-NAMED-VALID: named arg must target an existing parameter
                // (Kotlin model: ANY param can be named, not just optional params)
                let valid_params: Vec<String> =
                    regular_params.iter().map(|p| p.name.clone()).collect();
                return Err(Box::new(EvalError::unknown_named_arg(
                    name.clone(),
                    valid_params,
                    *call_span,
                )));
            }
            Some(_) => {
                // Valid: named arg targets an existing param that wasn't positionally bound
            }
        }
    }

    // BIND-VARIADIC: Collect excess positional args into a dict with int keys
    if let Some(var_param) = variadic_param {
        let mut var_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
        for (i, thunk) in positional.iter().enumerate().skip(max_positional) {
            var_map.insert(
                Key::Int(i64::try_from(i - max_positional).expect("collection too large")),
                Rc::clone(thunk),
            );
        }
        let var_thunk = Rc::new(Thunk::new_materialized(Value::Dict(var_map), *call_span));
        call_env
            .borrow_mut()
            .insert(var_param.name.clone(), var_thunk);
    }

    Ok(call_env)
}

/// Split params into (regular, optional variadic).
pub(crate) fn split_variadic(params: &[Param]) -> (&[Param], Option<&Param>) {
    match params.last() {
        Some(p) if p.variadic => (&params[..params.len() - 1], Some(p)),
        _ => (params, None),
    }
}

/// Extract the default value expression from a param's annotation, if present.
/// default: is specified via PropertyDict annotation with a "default" key.
pub(crate) fn get_default(param: &Param) -> Option<Spanned<Expr>> {
    param
        .annotation
        .as_ref()
        .and_then(|ann| ann.node.get_property(DEFAULT_ANNOTATION_KEY))
        .cloned()
}
