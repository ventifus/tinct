//! Iterative materialization machinery: CEK continuation stack and force loop.
//!
//! This module contains the core iterative evaluator (run/force_step/apply_cont)
//! that materializes thunks without recursion. The CEK machine design is documented
//! in doc/08-evaluation.md §Iterative Evaluator.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Annotation, Expr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::eval::{
    eval, eval_dict, eval_key, eval_recursive, format_type_for_assert, validate_and_wrap_record,
    value_matches_type, EvalContext, DEFAULT_ANNOTATION_KEY, MAX_EVAL_DEPTH,
};
use crate::eval_access::{eval_range_access, invoke_proxy_handler};
use crate::eval_call::{eval_call, invoke_function, CallContext};
use crate::types::Type;
use crate::value::{Environment, Key, Thunk, ThunkState, Value};

/// Attach materialization span and origin frame to an error.
/// This function is called at every error site in the CEK machine to ensure
/// errors carry full context (definition span, materialization span, stack trace).
pub(crate) fn attach_materialization_context(
    mut err: Box<EvalError>,
    mat_span: Option<&Span>,
    origin: &str,
    thunk_span: Span,
) -> Box<EvalError> {
    if let Some(span) = mat_span {
        if err.materialization_span.is_none() {
            err.materialization_span = Some(*span);
        } else if err.materialization_span != Some(*span)
            && !err.stack.iter().any(|f| f.span == *span)
        {
            // Only push a frame if the span differs from the existing
            // materialization span and isn't already in the stack (avoids
            // duplicate frames when the same span propagates through
            // nested materialize calls).
            err.push_frame("materialized".to_string(), *span);
        }
    }
    if !origin.is_empty()
        && !err
            .stack
            .iter()
            .any(|f| f.span == thunk_span && f.label == origin)
    {
        err.push_frame(origin.to_string(), thunk_span);
    }
    err
}

/// State restoration data for non-cacheable errors in iterative materialization.
/// When a non-cacheable error (e.g., DepthExceeded) occurs, the thunk's original
/// state must be restored so it can be re-evaluated at a shallower depth.
///
/// TODO(iterative-eval): RestoreState duplicates ThunkState data. The full CEK
/// machine (iterative-eval-b) eliminates this by moving PendingBuiltin/PendingCall
/// state to Cont variants on the stack, making restoration unnecessary.
pub(crate) enum RestoreState {
    Unevaluated {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
    },
    PendingBuiltin {
        name: &'static str,
        func: crate::value::BuiltinFn,
        args: Box<Vec<Rc<Thunk>>>,
        named: Box<IndexMap<String, Rc<Thunk>>>,
        depth: usize,
        call_span: Span,
        ctx: Rc<EvalContext>,
    },
    PendingCall {
        func: Rc<Thunk>,
        args: Box<Vec<Rc<Thunk>>>,
        named: Box<IndexMap<String, Rc<Thunk>>>,
        call_span: Span,
        caller_env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
    },
}

impl RestoreState {
    pub(crate) fn restore(self, thunk: &Thunk) {
        match self {
            RestoreState::Unevaluated { expr, env, ctx } => {
                thunk.set_state(ThunkState::Unevaluated { expr, env, ctx });
            }
            RestoreState::PendingBuiltin {
                name,
                func,
                args,
                named,
                depth,
                call_span,
                ctx,
            } => {
                thunk.set_state(ThunkState::PendingBuiltin {
                    name,
                    func,
                    args,
                    named,
                    depth,
                    call_span,
                    ctx,
                });
            }
            RestoreState::PendingCall {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            } => {
                thunk.set_state(ThunkState::PendingCall {
                    func,
                    args,
                    named,
                    call_span,
                    caller_env,
                    ctx,
                });
            }
        }
    }
}

/// Payload for Cont::Memoize. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MemoizeData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) origin: Cow<'static, str>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) restore: RestoreState,
}

/// Payload for Cont::PendingCallDispatch. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct PendingCallDispatchData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) func_thunk: Rc<Thunk>,
    pub(crate) args: Box<Vec<Rc<Thunk>>>,
    pub(crate) named: Box<IndexMap<String, Rc<Thunk>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env: Rc<RefCell<Environment>>,
    pub(crate) ctx: Rc<EvalContext>,
    pub(crate) origin: Cow<'static, str>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) depth: usize,
}

/// Payload for Cont::GuardedValidate. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct GuardedValidateData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) inner: Rc<Thunk>,
    pub(crate) expected: Type,
    pub(crate) field_path: Box<Vec<String>>,
    pub(crate) guard_span: Span,
    pub(crate) inner_span: Span,
    pub(crate) origin: Cow<'static, str>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
}

/// Payload for Cont::TypeAssertCheck. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct TypeAssertCheckData {
    pub(crate) annotation: Box<Spanned<Annotation>>,
    pub(crate) resolved: Box<Option<Type>>,
    pub(crate) expr_span: Span,
    pub(crate) thunk_span: Span,
    pub(crate) env: Rc<RefCell<Environment>>,
    pub(crate) ctx: Rc<EvalContext>,
    pub(crate) depth: usize,
}

/// Payload for Cont::BuiltinForceArg. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) builtin_name: &'static str,
    pub(crate) func: crate::value::BuiltinFn,
    pub(crate) args: Box<Vec<Rc<Thunk>>>,
    pub(crate) named: Box<IndexMap<String, Rc<Thunk>>>,
    pub(crate) depth: usize,
    pub(crate) call_span: Span,
    pub(crate) ctx: Rc<EvalContext>,
    pub(crate) origin: Cow<'static, str>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) restore: RestoreState,
}

/// Continuation variants for iterative materialization. Each represents
/// "what to do after a sub-thunk has been materialized."
///
/// **Size budget:** Large variants are boxed so the enum fits within 96 bytes
/// (one cache line), keeping the continuation stack cache-friendly.
pub(crate) enum Cont {
    /// Memoize the result into the parent thunk. Used after materializing
    /// result thunks from Unevaluated/PendingBuiltin/PendingCall branches.
    Memoize(Box<MemoizeData>),
    /// Defunctionalized continuation for the PendingCall branch (Reynolds, 1972).
    /// After the function thunk is forced, this continuation inspects the
    /// resulting `Value::Function` or `Value::Builtin`, invokes it with the captured
    /// argument thunks, and pushes a `Memoize` continuation for the result thunk.
    PendingCallDispatch(Box<PendingCallDispatchData>),
    /// Defunctionalized continuation for the Guarded branch (Reynolds, 1972).
    /// After the inner thunk is forced, this continuation runs
    /// `validate_and_wrap_record` (for record types) or `value_matches_type` (for
    /// scalar types), then memoizes the validated value into `thunk`.
    GuardedValidate(Box<GuardedValidateData>),
    /// Resume a PendingBuiltin call after iteratively materializing arg[0].
    /// This prevents Rust stack growth from chains like $- → materialize → $- → ...
    /// where each builtin synchronously materializes its first arg. By pre-materializing
    /// arg[0] in the iterative loop, the chain stays on the continuation stack instead
    /// of the Rust call stack.
    BuiltinForceArg(Box<BuiltinForceArgData>),
    /// Access a field from a materialized dict. Pushed after target thunk is materialized.
    DotAccessForce {
        field: String,
        access_span: Span,
        ctx: Rc<EvalContext>,
        depth: usize,
    },
    /// Access a key from a materialized dict via bracket notation. Pushed after target is materialized.
    BracketForceTarget {
        key_expr: Rc<Spanned<Expr>>,
        access_span: Span,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
        depth: usize,
    },
    /// Validate a materialized value against a TypeAssert annotation.
    /// Pushed by eval_step() after evaluating the inner expression thunk; replaces the
    /// synchronous materialize() call that was the laziness violation in the TypeAssert branch.
    TypeAssertCheck(Box<TypeAssertCheckData>),
}

// Compile-time assertion: Cont must be ≤96 bytes to fit in one cache line.
const _: () = assert!(std::mem::size_of::<Cont>() <= 96);

/// Action to perform next in the iterative evaluation loop.
pub(crate) enum Action {
    /// Result ready — pop top continuation and apply, or return if stack empty
    Continue(EvalResult<Value>),
    /// Force this thunk to a materialized value
    Materialize {
        thunk: Rc<Thunk>,
        mat_span: Option<Span>,
        depth: usize,
    },
    /// Evaluate an expression to a thunk (wrapping, not forcing)
    // Infrastructure for CEK loop entry — will be constructed when eval() becomes run(Action::Eval) wrapper
    #[allow(dead_code)]
    Eval {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
        depth: usize,
    },
}

/// Increment the evaluation depth by one unit.
/// Each level of thunk forcing consumes one depth unit: when `force_step` or
/// `apply_cont` transitions from a parent thunk to a child sub-thunk, the
/// depth passed to the child is `next_depth(parent_depth)`. This mirrors the
/// `depth + 1` in the old recursive `materialize()` calls and ensures
/// `MAX_EVAL_DEPTH` is enforced uniformly across all sub-thunk dispatch sites.
#[inline]
pub(crate) fn next_depth(d: usize) -> usize {
    d + 1
}

/// Process one thunk and return either a result or a sub-thunk to force.
/// This mirrors the logic of `materialize()` but pushes continuations instead of recursing.
pub(crate) fn force_step(
    thunk: &Rc<Thunk>,
    mat_span: Option<Span>,
    depth: usize,
    stack: &mut Vec<Cont>,
) -> Action {
    let origin = thunk.origin.clone();
    let thunk_span = thunk.span;

    // Early returns for already-resolved states
    {
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => return Action::Continue(Ok(v.clone())),
            ThunkState::Failed(ref err) => {
                let mut cloned = (**err).clone();
                let mut should_update_cache = false;
                if let Some(span) = mat_span {
                    if cloned.materialization_span.is_none() {
                        cloned.materialization_span = Some(span);
                        should_update_cache = true;
                    } else if cloned.materialization_span != Some(span)
                        && !cloned.stack.iter().any(|f| f.span == span)
                    {
                        cloned.push_frame("materialized".to_string(), span);
                        should_update_cache = true;
                    }
                }
                if should_update_cache && cloned.kind.is_cacheable() {
                    drop(state);
                    thunk.set_state(ThunkState::Failed(Box::new(cloned.clone())));
                }
                return Action::Continue(Err(Box::new(cloned)));
            }
            ThunkState::InProgress => {
                let label = if origin.is_empty() { "thunk" } else { &origin };
                let mut err = EvalError::circular_dependency(label, thunk.span);
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(span);
                }
                let err_boxed: Box<EvalError> = err.into();
                drop(state);
                thunk.cache_failure(&err_boxed);
                return Action::Continue(Err(err_boxed));
            }
            ThunkState::Unevaluated { .. }
            | ThunkState::PendingBuiltin { .. }
            | ThunkState::PendingCall { .. }
            | ThunkState::Guarded { .. } => {}
        }
    }

    // INVARIANTS verified post-iterative-eval-b4 (2026-04-30):
    //
    // 1. SHARING PRESERVATION: Rc<Thunk> identity is preserved through Cont dispatch.
    //    The Cont::Memoize handler (apply_cont, line 1380) caches the materialization
    //    result back into the ORIGINAL thunk via thunk.set_state(), not a copy.
    //    This ensures `Rc::ptr_eq` holds across all references to the same thunk.
    //
    // 2. MONOTONICITY: State transitions are one-way (Unevaluated/PendingBuiltin/
    //    PendingCall/Guarded → InProgress → Materialized/Failed). Exception: DepthExceeded
    //    errors are non-cacheable and trigger state restoration (e.g., InProgress →
    //    PendingBuiltin) so the computation can be retried at a shallower depth.
    //    Failed → Failed self-transition (lines 1006-1024) refines diagnostic metadata
    //    (materialization spans, stack frames) without changing the error's identity.
    //
    // 3. CYCLE DETECTION: InProgress blackholing works across all 7 states. Each take_*
    //    method (take_unevaluated, take_pending_builtin, take_pending_call, take_guarded
    //    in value.rs) atomically transitions to InProgress via mem::replace BEFORE
    //    extracting data. Re-encountering InProgress during materialization (line 1026)
    //    immediately produces CircularDependency error, cached in Failed state (line 1034).
    //
    // Process deferred states
    if let Some((expr, env, thunk_ctx)) = thunk.take_unevaluated() {
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(span);
            }
            thunk.set_state(ThunkState::Unevaluated {
                expr,
                env,
                ctx: thunk_ctx,
            });
            return Action::Continue(Err(err.into()));
        }

        let restore = RestoreState::Unevaluated {
            expr: expr.clone(),
            env: env.clone(),
            ctx: thunk_ctx.clone(),
        };

        // Handle DotAccess and BracketAccess inline to enable iterative access chains
        if let Expr::DotAccess {
            expr: target,
            field,
        } = &expr.node
        {
            // Evaluate target expression
            match eval(target, Rc::clone(&env), &thunk_ctx, next_depth(depth)) {
                Ok(target_thunk) => {
                    // Push Memoize for the outer thunk (the access result)
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Rc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore,
                    })));
                    // Push DotAccessForce to handle field lookup after target materializes
                    stack.push(Cont::DotAccessForce {
                        field: field.clone(),
                        access_span: expr.span,
                        ctx: Rc::clone(&thunk_ctx),
                        depth: next_depth(depth),
                    });
                    // Force the target
                    return Action::Materialize {
                        thunk: target_thunk,
                        mat_span: Some(expr.span),
                        depth: next_depth(depth),
                    };
                }
                Err(mut e) => {
                    e.push_frame(format!("accessing .{field}"), expr.span);
                    let decorated =
                        attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            }
        }

        if let Expr::BracketAccess { expr: target, key } = &expr.node {
            // Evaluate target expression
            match eval(target, Rc::clone(&env), &thunk_ctx, next_depth(depth)) {
                Ok(target_thunk) => {
                    // Push Memoize for the outer thunk (the access result)
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Rc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore,
                    })));
                    // Push BracketForceTarget to handle key lookup after target materializes
                    stack.push(Cont::BracketForceTarget {
                        key_expr: Rc::from(key.as_ref().clone()),
                        access_span: expr.span,
                        env: Rc::clone(&env),
                        ctx: Rc::clone(&thunk_ctx),
                        depth: next_depth(depth),
                    });
                    // Force the target
                    return Action::Materialize {
                        thunk: target_thunk,
                        mat_span: Some(expr.span),
                        depth: next_depth(depth),
                    };
                }
                Err(mut e) => {
                    e.push_frame("accessing [..]".to_string(), expr.span);
                    let decorated =
                        attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            }
        }

        match eval(&expr, Rc::clone(&env), &thunk_ctx, next_depth(depth)) {
            Ok(result_thunk) => {
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Rc::clone(thunk),
                    origin,
                    thunk_span,
                    mat_span,
                    restore,
                })));
                Action::Materialize {
                    thunk: result_thunk,
                    mat_span,
                    depth: next_depth(depth),
                }
            }
            Err(e) => {
                let decorated =
                    attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure(&decorated);
                } else {
                    restore.restore(thunk);
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((name, func, args, named, pending_depth, call_span, thunk_ctx)) =
        thunk.take_pending_builtin()
    {
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(span);
            }
            thunk.set_state(ThunkState::PendingBuiltin {
                name,
                func,
                args: Box::new(args),
                named: Box::new(named),
                depth: pending_depth,
                call_span,
                ctx: thunk_ctx,
            });
            return Action::Continue(Err(err.into()));
        }

        let restore = RestoreState::PendingBuiltin {
            name,
            func,
            args: Box::new(args.clone()),
            named: Box::new(named.clone()),
            depth: pending_depth,
            call_span,
            ctx: thunk_ctx.clone(),
        };

        // TCO: pre-materialize arg[0] iteratively to prevent Rust stack growth.
        // Without this, chains like $-(n) → materialize($n_prev) → $-(n_prev) → ...
        // create nested PUBLIC materialize calls on the Rust stack. By forcing
        // arg[0] through the continuation stack, the chain stays iterative.
        if !args.is_empty() && args[0].try_get_materialized().is_none() {
            let arg0 = Rc::clone(&args[0]);
            stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                thunk: Rc::clone(thunk),
                builtin_name: name,
                func,
                args: Box::new(args),
                named: Box::new(named),
                depth,
                call_span,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                restore,
            })));
            return Action::Materialize {
                thunk: arg0,
                mat_span: None,
                depth,
            };
        }

        let builtin_args = crate::value::BuiltinArgs {
            args: &args,
            named: &named,
            depth,
            call_span,
            ctx: Rc::clone(&thunk_ctx),
        };

        match func(builtin_args) {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion
                if let Some(value) = result_thunk.try_get_materialized() {
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Action::Continue(Ok(value))
                } else {
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Rc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore,
                    })));
                    // TCO: force result at same depth
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span,
                        depth,
                    }
                }
            }
            Err(e) => {
                let decorated =
                    attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure(&decorated);
                } else {
                    restore.restore(thunk);
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((func_thunk, args, named, call_span, caller_env, thunk_ctx)) =
        thunk.take_pending_call()
    {
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(span);
            }
            thunk.set_state(ThunkState::PendingCall {
                func: func_thunk.clone(),
                args: Box::new(args.clone()),
                named: Box::new(named.clone()),
                call_span,
                caller_env,
                ctx: thunk_ctx,
            });
            return Action::Continue(Err(err.into()));
        }

        stack.push(Cont::PendingCallDispatch(Box::new(
            PendingCallDispatchData {
                thunk: Rc::clone(thunk),
                func_thunk: Rc::clone(&func_thunk),
                args: Box::new(args),
                named: Box::new(named),
                call_span,
                caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                depth,
            },
        )));
        // TCO: if function is already materialized (cached from prior call),
        // don't increment depth — enables tail recursion to same function.
        let func_depth = if func_thunk.try_get_materialized().is_some() {
            depth
        } else {
            next_depth(depth)
        };
        Action::Materialize {
            thunk: Rc::clone(&func_thunk),
            mat_span: Some(call_span),
            depth: func_depth,
        }
    } else if let Some((inner, expected, field_path, guard_span)) = thunk.take_guarded() {
        if depth > MAX_EVAL_DEPTH {
            let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk_span);
            if let Some(span) = mat_span {
                err = err.with_materialization_span(span);
            }
            thunk.set_state(ThunkState::Guarded {
                inner: inner.clone(),
                expected: expected.clone(),
                field_path: Box::new(field_path.clone()),
                guard_span,
            });
            return Action::Continue(Err(err.into()));
        }

        let inner_span = inner.span;
        stack.push(Cont::GuardedValidate(Box::new(GuardedValidateData {
            thunk: Rc::clone(thunk),
            inner: Rc::clone(&inner),
            expected: expected.clone(),
            field_path: Box::new(field_path),
            guard_span,
            inner_span,
            origin,
            thunk_span,
            mat_span,
        })));
        Action::Materialize {
            thunk: Rc::clone(&inner),
            mat_span,
            depth: next_depth(depth),
        }
    } else {
        unreachable!(
            "force_step: all ThunkState variants are handled. \
             Materialized/Failed/InProgress are early-returned at lines 1416-1453, \
             Unevaluated/PendingBuiltin/PendingCall/Guarded are processed above. \
             If this fires, a new ThunkState variant was added without updating force_step."
        )
    }
}

/// Apply a continuation to a materialization result.
pub(crate) fn apply_cont(cont: Cont, result: EvalResult<Value>, stack: &mut Vec<Cont>) -> Action {
    match cont {
        Cont::Memoize(data) => {
            let MemoizeData {
                thunk,
                origin,
                thunk_span,
                mat_span,
                restore,
            } = *data;
            let decorated_result = result.map_err(|e| {
                attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span)
            });

            match decorated_result {
                Ok(value) => {
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Action::Continue(Ok(value))
                }
                Err(e) => {
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        restore.restore(&thunk);
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::PendingCallDispatch(data) => {
            let PendingCallDispatchData {
                thunk,
                func_thunk,
                args,
                named,
                call_span,
                caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                depth,
            } = *data;
            let decorate =
                |e| attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);

            match result.map_err(&decorate) {
                Ok(func_value) => match func_value {
                    Value::Function { params, body, env } => {
                        let call_ctx = CallContext {
                            params: &params,
                            body: &body,
                            closure_env: &env,
                            positional: &args,
                            named: &named,
                            default_env: &caller_env, // Use caller's environment for default param evaluation
                            call_span,
                            depth,
                            origin: origin.clone(),
                            ctx: &thunk_ctx,
                        };

                        match invoke_function(&call_ctx).map_err(&decorate) {
                            Ok(result_thunk) => {
                                let restore = RestoreState::PendingCall {
                                    func: func_thunk.clone(),
                                    args: args.clone(),
                                    named: named.clone(),
                                    call_span,
                                    caller_env: caller_env.clone(),
                                    ctx: thunk_ctx.clone(),
                                };
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Rc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore,
                                })));
                                // TCO: function return value forced at caller's depth
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span,
                                    depth,
                                }
                            }
                            Err(mut e) => {
                                e.push_frame(origin.to_string(), call_span);
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure(&e);
                                } else {
                                    thunk.set_state(ThunkState::PendingCall {
                                        func: func_thunk,
                                        args: Box::new((*args).clone()),
                                        named: Box::new((*named).clone()),
                                        call_span,
                                        caller_env,
                                        ctx: thunk_ctx,
                                    });
                                }
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    Value::Builtin { func, .. } => {
                        // TCO: use loop depth for builtin arg materialization
                        let builtin_args = crate::value::BuiltinArgs {
                            args: &args,
                            named: &named,
                            depth,
                            call_span,
                            ctx: Rc::clone(&thunk_ctx),
                        };
                        match func(builtin_args).map_err(&decorate) {
                            Ok(result_thunk) => {
                                if let Some(value) = result_thunk.try_get_materialized() {
                                    thunk.set_state(ThunkState::Materialized(value.clone()));
                                    Action::Continue(Ok(value))
                                } else {
                                    let restore = RestoreState::PendingCall {
                                        func: func_thunk.clone(),
                                        args: args.clone(),
                                        named: named.clone(),
                                        call_span,
                                        caller_env: caller_env.clone(),
                                        ctx: thunk_ctx.clone(),
                                    };
                                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                                        thunk: Rc::clone(&thunk),
                                        origin,
                                        thunk_span,
                                        mat_span,
                                        restore,
                                    })));
                                    // TCO: builtin return value forced at caller's depth
                                    Action::Materialize {
                                        thunk: result_thunk,
                                        mat_span,
                                        depth,
                                    }
                                }
                            }
                            Err(e) => {
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure(&e);
                                } else {
                                    thunk.set_state(ThunkState::PendingCall {
                                        func: func_thunk,
                                        args: Box::new((*args).clone()),
                                        named: Box::new((*named).clone()),
                                        call_span,
                                        caller_env,
                                        ctx: thunk_ctx,
                                    });
                                }
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    other => {
                        let err = EvalError::type_mismatch(
                            "Function or Builtin",
                            other.type_name(),
                            call_span,
                        );
                        let decorated = decorate(Box::new(err));
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure(&decorated);
                        } else {
                            thunk.set_state(ThunkState::PendingCall {
                                func: func_thunk,
                                args: Box::new((*args).clone()),
                                named: Box::new((*named).clone()),
                                call_span,
                                caller_env,
                                ctx: thunk_ctx,
                            });
                        }
                        Action::Continue(Err(decorated))
                    }
                },
                Err(e) => {
                    // Function materialization failed
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        thunk.set_state(ThunkState::PendingCall {
                            func: func_thunk,
                            args: Box::new((*args).clone()),
                            named: Box::new((*named).clone()),
                            call_span,
                            caller_env,
                            ctx: thunk_ctx,
                        });
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::GuardedValidate(data) => {
            let GuardedValidateData {
                thunk,
                inner,
                expected,
                field_path,
                guard_span,
                inner_span,
                origin,
                thunk_span,
                mat_span,
            } = *data;
            let decorate =
                |e| attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);

            match result {
                Ok(value) => {
                    // For Record types, apply proxy contract wrapping
                    if let Type::Record(ref row) = expected {
                        if let Value::Dict(ref entries) = value {
                            match validate_and_wrap_record(
                                entries,
                                row,
                                *field_path,
                                guard_span,
                                inner_span,
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk
                                        .set_state(ThunkState::Materialized(guarded_value.clone()));
                                    Action::Continue(Ok(guarded_value))
                                }
                                Err(err) => {
                                    let err = decorate(err);
                                    thunk.cache_failure(&err);
                                    Action::Continue(Err(err))
                                }
                            }
                        } else {
                            // Expected Record but got non-Dict
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field \"{}\": ", field_path.join("."))
                            };
                            let err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                &value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            let err = decorate(err.into());
                            thunk.cache_failure(&err);
                            Action::Continue(Err(err))
                        }
                    } else {
                        // For non-Record types, simple value check
                        if value_matches_type(&value, &expected) {
                            thunk.set_state(ThunkState::Materialized(value.clone()));
                            Action::Continue(Ok(value))
                        } else {
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field \"{}\": ", field_path.join("."))
                            };
                            let err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                &value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            let err = decorate(err.into());
                            thunk.cache_failure(&err);
                            Action::Continue(Err(err))
                        }
                    }
                }
                Err(e) => {
                    // Inner materialization error propagates
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        thunk.set_state(ThunkState::Guarded {
                            inner,
                            expected,
                            field_path: Box::new((*field_path).clone()),
                            guard_span,
                        });
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::BuiltinForceArg(data) => {
            let BuiltinForceArgData {
                thunk,
                builtin_name,
                func,
                args,
                named,
                depth,
                call_span,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                restore,
            } = *data;
            let decorate =
                |e| attach_materialization_context(e, mat_span.as_ref(), &origin, thunk_span);

            // arg[0] has been materialized by the iterative loop (thunk memoization).
            // For $apply specifically, also check if arg[1] needs materialization.
            match result {
                Ok(_) => {
                    // Special case: $apply needs both args[0] (function) and args[1] (args dict)
                    // pre-materialized to avoid Rust stack growth.
                    if builtin_name == "apply"
                        && args.len() >= 2
                        && args[1].try_get_materialized().is_none()
                    {
                        let arg1 = Rc::clone(&args[1]);
                        stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                            thunk,
                            builtin_name,
                            func,
                            args,
                            named,
                            depth,
                            call_span,
                            ctx: thunk_ctx,
                            origin,
                            thunk_span,
                            mat_span,
                            restore,
                        })));
                        return Action::Materialize {
                            thunk: arg1,
                            mat_span: None,
                            depth,
                        };
                    }

                    // Use loop depth for builtin arg materialization (TCO)
                    let builtin_args = crate::value::BuiltinArgs {
                        args: &args,
                        named: &named,
                        depth,
                        call_span,
                        ctx: Rc::clone(&thunk_ctx),
                    };
                    match func(builtin_args).map_err(&decorate) {
                        Ok(result_thunk) => {
                            if let Some(value) = result_thunk.try_get_materialized() {
                                thunk.set_state(ThunkState::Materialized(value.clone()));
                                Action::Continue(Ok(value))
                            } else {
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Rc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore,
                                })));
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span,
                                    depth,
                                }
                            }
                        }
                        Err(e) => {
                            if e.kind.is_cacheable() {
                                thunk.cache_failure(&e);
                            } else {
                                restore.restore(&thunk);
                            }
                            Action::Continue(Err(e))
                        }
                    }
                }
                Err(e) => {
                    let e = decorate(e);
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        restore.restore(&thunk);
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::DotAccessForce {
            field,
            access_span,
            ctx,
            depth,
        } => {
            // Result is the materialized target value
            match result {
                Ok(target_val) => match target_val {
                    Value::Dict(map) => {
                        // Use StrKey wrapper to avoid allocating Key::String
                        match map.get(&crate::value::StrKey(&field)) {
                            Some(thunk) => {
                                // Field found - need to materialize it
                                Action::Materialize {
                                    thunk: Rc::clone(thunk),
                                    mat_span: Some(access_span),
                                    depth,
                                }
                            }
                            None => {
                                // Key not found
                                let available_keys: Vec<String> =
                                    map.keys().map(|k| k.to_string()).collect();
                                let mut err = EvalError::key_not_found(
                                    &field,
                                    available_keys,
                                    access_span, // Use access_span as thunk span
                                )
                                .with_materialization_span(access_span);
                                err.push_frame(format!("accessing .{field}"), access_span);
                                Action::Continue(Err(err.into()))
                            }
                        }
                    }
                    Value::Proxy { handler } => {
                        // Proxy handler invocation
                        match invoke_proxy_handler(
                            &handler,
                            Value::String(field.clone()),
                            &ctx,
                            &access_span,
                            depth,
                        ) {
                            Ok(thunk) => Action::Materialize {
                                thunk,
                                mat_span: Some(access_span),
                                depth,
                            },
                            Err(mut e) => {
                                e.push_frame(format!("accessing .{field}"), access_span);
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    other => {
                        // Type mismatch
                        let mut err = EvalError::type_mismatch_ctx(
                            "dot access".to_string(),
                            "Dict or Proxy",
                            other.type_name(),
                            access_span, // Use access_span as thunk span
                        )
                        .with_materialization_span(access_span);
                        err.push_frame(format!("accessing .{field}"), access_span);
                        Action::Continue(Err(err.into()))
                    }
                },
                Err(mut e) => {
                    // Target materialization failed
                    e.push_frame(format!("accessing .{field}"), access_span);
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::BracketForceTarget {
            key_expr,
            access_span,
            env,
            ctx,
            depth,
        } => {
            // Result is the materialized target value
            match result {
                Ok(target_val) => match target_val {
                    Value::Dict(map) => {
                        // Evaluate the key expression (synchronous for now)
                        match eval_key(&key_expr, &env, &ctx, depth) {
                            Ok(key) => match map.get(&key) {
                                Some(thunk) => {
                                    // Field found - return it to be forced
                                    Action::Materialize {
                                        thunk: Rc::clone(thunk),
                                        mat_span: Some(access_span),
                                        depth,
                                    }
                                }
                                None => {
                                    let available_keys: Vec<String> =
                                        map.keys().map(|k| k.to_string()).collect();
                                    let mut err = EvalError::key_not_found(
                                        &key.to_string(),
                                        available_keys,
                                        access_span,
                                    )
                                    .with_materialization_span(access_span);
                                    err.push_frame("accessing [..]".to_string(), access_span);
                                    Action::Continue(Err(err.into()))
                                }
                            },
                            Err(mut e) => {
                                e.push_frame("accessing [..]".to_string(), access_span);
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    Value::Proxy { handler } => {
                        // Evaluate the key and call proxy handler
                        match eval_key(&key_expr, &env, &ctx, depth) {
                            Ok(key) => {
                                let key_val = match key {
                                    Key::Int(n) => Value::Int(n),
                                    Key::String(s) => Value::String(s),
                                };
                                match invoke_proxy_handler(
                                    &handler,
                                    key_val,
                                    &ctx,
                                    &access_span,
                                    depth,
                                ) {
                                    Ok(thunk) => Action::Materialize {
                                        thunk,
                                        mat_span: Some(access_span),
                                        depth,
                                    },
                                    Err(mut e) => {
                                        e.push_frame("accessing [..]".to_string(), access_span);
                                        Action::Continue(Err(e))
                                    }
                                }
                            }
                            Err(mut e) => {
                                e.push_frame("accessing [..]".to_string(), access_span);
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    other => {
                        let mut err = EvalError::type_mismatch_ctx(
                            "bracket access".to_string(),
                            "Dict or Proxy",
                            other.type_name(),
                            access_span,
                        )
                        .with_materialization_span(access_span);
                        err.push_frame("accessing [..]".to_string(), access_span);
                        Action::Continue(Err(err.into()))
                    }
                },
                Err(mut e) => {
                    // Target materialization failed
                    e.push_frame("accessing [..]".to_string(), access_span);
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::TypeAssertCheck(data) => {
            let TypeAssertCheckData {
                annotation,
                resolved,
                expr_span,
                thunk_span,
                env,
                ctx,
                depth,
            } = *data;
            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(value) => match *resolved {
                    Some(expected) => match &expected {
                        Type::Record(row) => {
                            if let Value::Dict(entries) = &value {
                                let default_opt = annotation
                                    .node
                                    .get_property(DEFAULT_ANNOTATION_KEY)
                                    .map(|expr| (expr.clone(), Rc::clone(&env)));
                                match validate_and_wrap_record(
                                    entries,
                                    row,
                                    vec![],
                                    expr_span,
                                    thunk_span,
                                ) {
                                    Ok(new_entries) => {
                                        Action::Continue(Ok(Value::Dict(new_entries)))
                                    }
                                    Err(err) => {
                                        if let Some((default, env)) = default_opt {
                                            match eval_recursive(&default, env, &ctx, depth + 1) {
                                                Ok(t) => Action::Materialize {
                                                    thunk: t,
                                                    mat_span: None,
                                                    depth,
                                                },
                                                Err(e) => Action::Continue(Err(e)),
                                            }
                                        } else {
                                            Action::Continue(Err(err))
                                        }
                                    }
                                }
                            } else {
                                if let Some(default_expr) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    match eval_recursive(default_expr, env, &ctx, depth + 1) {
                                        Ok(t) => Action::Materialize {
                                            thunk: t,
                                            mat_span: None,
                                            depth,
                                        },
                                        Err(e) => Action::Continue(Err(e)),
                                    }
                                } else {
                                    Action::Continue(Err(EvalError::type_assert_failed(
                                        &format_type_for_assert(&expected),
                                        &value.type_name(),
                                        thunk_span,
                                    )
                                    .with_materialization_span(expr_span)
                                    .into()))
                                }
                            }
                        }
                        _ => {
                            if value_matches_type(&value, &expected) {
                                Action::Continue(Ok(value))
                            } else if let Some(default_expr) =
                                annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                            {
                                match eval_recursive(default_expr, env, &ctx, depth + 1) {
                                    Ok(t) => Action::Materialize {
                                        thunk: t,
                                        mat_span: None,
                                        depth,
                                    },
                                    Err(e) => Action::Continue(Err(e)),
                                }
                            } else {
                                Action::Continue(Err(EvalError::type_assert_failed(
                                    &format_type_for_assert(&expected),
                                    &value.type_name(),
                                    thunk_span,
                                )
                                .with_materialization_span(expr_span)
                                .into()))
                            }
                        }
                    },
                    None => {
                        // --no-typecheck FALLBACK (nominal validation)
                        let expected_name: Option<String> = match &annotation.node {
                            Annotation::Simple(name) => Some(name.clone()),
                            Annotation::PropertyDict(_) => annotation
                                .node
                                .get_property("type")
                                .and_then(|type_expr| match &type_expr.node {
                                    Expr::Str(s) => Some(s.clone()),
                                    _ => None,
                                }),
                        };
                        if let Some(expected) = expected_name {
                            let actual = value.type_name();
                            let matches = if expected == "Number" {
                                actual == "Int" || actual == "Float"
                            } else {
                                actual == expected.as_str()
                            };
                            if !matches {
                                if let Some(default_expr) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    return match eval_recursive(default_expr, env, &ctx, depth + 1)
                                    {
                                        Ok(t) => Action::Materialize {
                                            thunk: t,
                                            mat_span: None,
                                            depth,
                                        },
                                        Err(e) => Action::Continue(Err(e)),
                                    };
                                }
                                return Action::Continue(Err(EvalError::type_assert_failed(
                                    &expected, actual, thunk_span,
                                )
                                .with_materialization_span(expr_span)
                                .into()));
                            }
                        }
                        Action::Continue(Ok(value))
                    }
                },
            }
        }
    }
}

/// Evaluate an expression and return an action for the next step.
///
/// This is the entry point for the iterative evaluator. For the incremental implementation,
/// it delegates all work to the existing recursive `eval()` function and converts the
/// resulting thunk into an appropriate action.
///
/// Future sprints will move individual expression handlers from `eval()` into this function,
/// converting them to push continuations instead of recursing.
pub(crate) fn eval_step(
    expr: &Spanned<Expr>,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    depth: usize,
    stack: &mut Vec<Cont>,
) -> Action {
    // Depth check at entry point
    if depth > MAX_EVAL_DEPTH {
        return Action::Continue(Err(
            EvalError::depth_exceeded(MAX_EVAL_DEPTH, expr.span).into()
        ));
    }

    // Helper: wrap a thunk result from helper functions
    let wrap_thunk = |result: EvalResult<Rc<Thunk>>| -> Action {
        match result {
            Ok(thunk) => match thunk.try_get_materialized() {
                Some(value) => Action::Continue(Ok(value)),
                None => Action::Materialize {
                    thunk,
                    mat_span: Some(expr.span),
                    depth,
                },
            },
            Err(e) => Action::Continue(Err(e)),
        }
    };

    match &expr.node {
        // Literals and closures are already computed values, so we return them directly
        // as materialized values. This avoids the overhead of wrapping, then unwrapping,
        // then re-evaluating on first access.
        Expr::Int(n) => Action::Continue(Ok(Value::Int(*n))),
        Expr::Float(f) => Action::Continue(Ok(Value::Float(*f))),
        Expr::Bool(b) => Action::Continue(Ok(Value::Bool(*b))),
        Expr::Str(s) => Action::Continue(Ok(Value::String(s.clone()))),
        Expr::VarRef(name) => {
            let found = env.borrow().get(name);
            match found {
                Some(thunk) => Action::Materialize {
                    thunk,
                    mat_span: Some(expr.span),
                    depth,
                },
                None => {
                    Action::Continue(Err(
                        EvalError::undefined_variable(name.clone(), expr.span).into()
                    ))
                }
            }
        }
        Expr::Dict(entries) => wrap_thunk(eval_dict(entries, &env, ctx, &expr.span, depth + 1)),
        Expr::DotAccess { .. } | Expr::BracketAccess { .. } => {
            // Return Unevaluated thunk — force_step handles these iteratively via
            // DotAccessForce/BracketForceTarget continuations
            wrap_thunk(Ok(Rc::new(Thunk::new_unevaluated(
                Rc::new((*expr).clone()),
                Rc::clone(&env),
                Rc::clone(ctx),
                expr.span,
            ))))
        }
        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => wrap_thunk(eval_range_access(
            target,
            start.as_deref(),
            end.as_deref(),
            &env,
            ctx,
            &expr.span,
            depth,
        )),
        Expr::TypeAssert {
            expr: inner,
            annotation,
            resolved_type,
        } => {
            let inner_thunk = match eval_recursive(inner, Rc::clone(&env), ctx, depth + 1) {
                Ok(t) => t,
                Err(e) => return Action::Continue(Err(e)),
            };
            let resolved = resolved_type.borrow().clone();

            // Fast path: if there is no type to check, skip materialization entirely.
            // This applies when resolved_type is None (--no-typecheck mode) and the
            // annotation has no "type" property — e.g. [@[default: 0] $x] where only
            // a default is provided. A Simple annotation always carries a type name;
            // a PropertyDict without a "type" key has nothing to validate against.
            let has_type = match &annotation.node {
                Annotation::Simple(_) => true,
                Annotation::PropertyDict(_) => annotation.node.get_property("type").is_some(),
            };
            if resolved.is_none() && !has_type {
                return wrap_thunk(Ok(inner_thunk));
            }

            let thunk_span = inner_thunk.span;
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(resolved),
                expr_span: expr.span,
                thunk_span,
                env,
                ctx: Rc::clone(ctx),
                depth,
            })));
            Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(expr.span),
                depth: depth + 1,
            }
        }
        Expr::Annotated { name, .. } => {
            // Evaluate as the bare string; the type checker (typecheck.rs) interprets annotations.
            Action::Continue(Ok(Value::String(name.clone())))
        }
        Expr::Fn { params, body, .. } => {
            let fn_params: Vec<Param> = params.iter().map(|p| p.node.clone()).collect();
            Action::Continue(Ok(Value::Function {
                params: Rc::new(fn_params),
                body: Rc::new(body.as_ref().clone()),
                env: Rc::clone(&env),
            }))
        }
        Expr::Call {
            func,
            args,
            named_args,
        } => wrap_thunk(eval_call(
            func, args, named_args, &env, ctx, &expr.span, depth,
        )),
        // Type alias entries are compile-time-only constructs consumed by the type checker.
        // At runtime, they evaluate to an empty dict to maintain dict structure without
        // contributing runtime values.
        Expr::TypeAlias(_inner) => Action::Continue(Ok(Value::Dict(IndexMap::new()))),
        Expr::Rest(_) => Action::Continue(Err(EvalError::internal(
            "rest marker (...) is only valid inside type expressions".to_string(),
            expr.span,
        )
        .into())),
    }
}

/// Main iterative evaluation loop. Executes actions until a final result is produced.
///
/// This function drives the defunctionalized CEK machine: it repeatedly processes
/// `Action::Eval` steps (wrapping expressions as thunks), `Action::Materialize` steps
/// (forcing thunks), and `Action::Continue` steps (applying continuations) until the
/// continuation stack is empty and a result is available.
///
/// # Arguments
/// - `initial`: The first action to execute (typically `Action::Materialize`)
/// - `ctx`: Evaluation context (unused currently, but will be needed for full CEK machine)
///
/// # Returns
/// The final materialized value or error after all continuations have been applied.
pub(crate) fn run(initial: Action, _ctx: &Rc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::Eval {
                expr,
                env,
                ctx,
                depth,
            } => {
                action = eval_step(&expr, env, &ctx, depth, &mut stack);
            }
            Action::Materialize {
                thunk,
                mat_span,
                depth,
            } => {
                action = force_step(&thunk, mat_span, depth, &mut stack);
            }
            Action::Continue(result) => match stack.pop() {
                None => return result,
                Some(cont) => {
                    action = apply_cont(cont, result, &mut stack);
                }
            },
        }
    }
}
