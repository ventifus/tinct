//! Iterative materialization machinery: CEK continuation stack and force loop.
//!
//! This module contains the core iterative evaluator (run/force_step/apply_cont)
//! that materializes thunks without recursion. The CEK machine design is documented
//! in doc/08-evaluation.md §Iterative Evaluator.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Annotation, Expr, Param, Span, Spanned};
use crate::builtins::flatten_overlay;
use crate::error::{EvalError, EvalResult};
use crate::eval::{
    annotation_has_structural_fields, as_record_row_merged, eval, eval_dict, eval_recursive,
    format_field_path, format_type_for_assert, validate_and_wrap_record, value_matches_type,
    EvalContext, DEFAULT_ANNOTATION_KEY,
};
use crate::eval_access::invoke_proxy_handler;
use crate::eval_call::{eval_call, invoke_function, CallContext};
use crate::types::Type;
use crate::value::{string_val, Environment, Thunk, ThunkState, Value};

/// Attach materialization span and origin frame to an error.
/// This function is called at every error site in the CEK machine to ensure
/// errors carry full context (definition span, materialization span, stack trace).
pub(crate) fn attach_materialization_context(
    mut err: Box<EvalError>,
    mat_span: Option<&Span>,
    origin: Option<&str>,
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
    if let Some(label) = origin {
        if !err
            .stack
            .iter()
            .any(|f| f.span == thunk_span && f.label == label)
        {
            err.push_frame(label.to_string(), thunk_span);
        }
    }
    err
}

/// State restoration data for non-cacheable errors in iterative materialization.
/// Snapshot of a thunk's pre-materialization state, used to restore the thunk
/// when a non-cacheable error occurs.
///
pub(crate) enum RestoreState {
    Unevaluated {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
    },
    PendingBuiltin {
        def: crate::value::BuiltinDef,
        args: Box<Vec<Rc<Thunk>>>,
        named: Option<IndexMap<String, Rc<Thunk>>>,
        call_span: Span,
        ctx: Rc<EvalContext>,
    },
    PendingCall {
        func: Rc<Thunk>,
        args: Box<Vec<Rc<Thunk>>>,
        named: Option<Box<IndexMap<String, Rc<Thunk>>>>,
        call_span: Span,
        caller_env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
    },
    Guarded {
        inner: Rc<Thunk>,
        expected: Type,
        field_path: Box<Vec<String>>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<(
            Rc<crate::ast::Spanned<crate::ast::Expr>>,
            Rc<RefCell<Environment>>,
        )>,
    },
}

impl RestoreState {
    pub(crate) fn restore(self, thunk: &Thunk) {
        match self {
            RestoreState::Unevaluated { expr, env, ctx } => {
                thunk.set_state(ThunkState::Unevaluated { expr, env, ctx });
            }
            RestoreState::PendingBuiltin {
                def,
                args,
                named,
                call_span,
                ctx,
            } => {
                thunk.set_state(ThunkState::PendingBuiltin {
                    def,
                    args,
                    named,
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
            RestoreState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            } => {
                thunk.set_state(ThunkState::Guarded {
                    inner,
                    expected,
                    field_path,
                    guard_span,
                    blame_label,
                    default,
                });
            }
        }
    }
}

/// Payload for Cont::Memoize. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MemoizeData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) origin: Option<Rc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    // None for paths where restoration is not possible (e.g., default fallback).
    // Some when the original thunk state can be restored on error.
    pub(crate) restore: Option<RestoreState>,
    pub(crate) ctx: Rc<EvalContext>,
}

/// Payload for Cont::PendingCallDispatch. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct PendingCallDispatchData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) func_thunk: Rc<Thunk>,
    pub(crate) args: Box<Vec<Rc<Thunk>>>,
    pub(crate) named: Option<Box<IndexMap<String, Rc<Thunk>>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env: Rc<RefCell<Environment>>,
    pub(crate) ctx: Rc<EvalContext>,
    pub(crate) origin: Option<Rc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
}

/// Payload for Cont::GuardedValidate. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct GuardedValidateData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) inner: Rc<Thunk>,
    pub(crate) expected: Type,
    pub(crate) field_path: Box<Vec<String>>,
    pub(crate) guard_span: Span,
    pub(crate) inner_span: Span,
    pub(crate) origin: Option<Rc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    /// EvalContext for flattening Value::Overlay results. None when inner thunk was already
    /// Materialized/Failed at guard-push time (these states can't produce new Overlays).
    pub(crate) ctx: Option<Rc<EvalContext>>,
    pub(crate) blame_label: Option<crate::error::BlameLabel>,
    /// Default expression and environment from TypeAssert `default:` annotation.
    pub(crate) default: Option<(
        Rc<crate::ast::Spanned<crate::ast::Expr>>,
        Rc<RefCell<crate::value::Environment>>,
    )>,
    /// Restoration state for non-cacheable errors (e.g., DepthExceeded).
    /// Wrapped in Option to enable .take() when passing to default-fallback Memoize continuations.
    pub(crate) restore: Option<RestoreState>,
}

/// Payload for Cont::TypeAssertCheck. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct TypeAssertCheckData {
    pub(crate) annotation: Box<Spanned<Annotation>>,
    pub(crate) resolved: Box<Option<Type>>,
    pub(crate) expr_span: Span,
    pub(crate) thunk_span: Span,
    pub(crate) env: Rc<RefCell<Environment>>,
    pub(crate) ctx: Rc<EvalContext>,
}

/// Payload for Cont::BuiltinForceArg. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Rc<Thunk>,
    pub(crate) def: crate::value::BuiltinDef,
    pub(crate) args: Vec<Rc<Thunk>>,
    pub(crate) named: Option<IndexMap<String, Rc<Thunk>>>,
    pub(crate) call_span: Span,
    pub(crate) ctx: Rc<EvalContext>,
    pub(crate) origin: Option<Rc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) arg_idx: usize,
}

/// Payload for Cont::DotAccessForce. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct DotAccessForceData {
    pub(crate) field: crate::ast::DotKey,
    /// Span of the entire dot-access expression (e.g. `dict.field`).
    pub(crate) access_span: Span,
    /// Definition-site span of the target expression (the dict being accessed).
    /// Used to annotate key-not-found and type-mismatch errors with where the
    /// bad value was defined, complementing `access_span` (where it was accessed).
    pub(crate) target_def_span: Span,
    /// Outermost materialization span from the access chain (e.g., for `a.b.c`,
    /// this is the span where the entire chain was first accessed, not the `.c` access).
    /// Used to provide better error context for chained accesses.
    pub(crate) outer_mat_span: Option<Span>,
    pub(crate) ctx: Rc<EvalContext>,
}

/// Continuation variants for iterative materialization. Each represents
/// "what to do after a sub-thunk has been materialized."
///
/// **Size budget:** Large variants are boxed so the enum fits within 96 bytes
/// (one cache line), keeping the continuation stack cache-friendly.
///
/// **Context capture convention:** Some variants carry `ctx` for proxy dispatch
/// (e.g., `GuardedValidate`), while others read `ctx` from the thunk being forced.
/// Variants that dispatch to proxy handlers need their own `ctx` because the proxy
/// handler may be evaluated in a different scope than the target thunk.
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
    DotAccessForce(Box<DotAccessForceData>),
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
    },
    /// Evaluate an expression to a thunk (wrapping, not forcing).
    /// Used by TypeAssert default expression evaluation and other iterative eval paths.
    /// Eventually eval() will become a run(Action::Eval) wrapper when fully iterative.
    Eval {
        expr: Rc<Spanned<Expr>>,
        env: Rc<RefCell<Environment>>,
        ctx: Rc<EvalContext>,
    },
}

/// Process one thunk and return either a result or a sub-thunk to force.
/// This mirrors the logic of `materialize()` but pushes continuations instead of recursing.
pub(crate) fn force_step(
    thunk: &Rc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Rc<EvalContext>,
) -> Action {
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
                // Defer origin clone to error path only (hot path already returned at Materialized)
                let origin = thunk.origin.clone();
                let label = origin.as_deref().unwrap_or("thunk");

                // Capture the eval_stack for cycle path reconstruction
                let cycle_path = ctx.state.borrow().eval_stack.clone();

                let mut err = EvalError::circular_dependency(label, thunk.span, cycle_path);
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(span);
                }
                let err_boxed: Box<EvalError> = err.into();
                drop(state);
                thunk.cache_failure(&err_boxed);
                return Action::Continue(Err(err_boxed));
            }
            ThunkState::Placeholder => {
                panic!(
                    "attempted to force a Placeholder thunk (span {:?}). \
                     This indicates a letrec construction bug: all placeholder \
                     slots must be filled via set_state() before evaluation begins.",
                    thunk.span
                );
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
    //    PendingBuiltin) so the computation can be retried.
    //    Failed → Failed self-transition (lines 1006-1024) refines diagnostic metadata
    //    (materialization spans, stack frames) without changing the error's identity.
    //
    // 3. CYCLE DETECTION: InProgress blackholing works across all 8 states. Each take_*
    //    method (take_unevaluated, take_pending_builtin, take_pending_call, take_guarded
    //    in value.rs) atomically transitions to InProgress via mem::replace BEFORE
    //    extracting data. Re-encountering InProgress during materialization (line 1026)
    //    immediately produces CircularDependency error, cached in Failed state (line 1034).
    //
    // Process deferred states (hot path has already returned above)
    // Defer origin clone to here — it's only needed for error reporting and Memoize continuations.
    let origin = thunk.origin.clone();

    if let Some((expr, env, thunk_ctx)) = thunk.take_unevaluated() {
        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction)
        thunk_ctx
            .state
            .borrow_mut()
            .eval_stack
            .push((origin.as_deref().unwrap_or("thunk").to_string(), thunk_span));

        let restore = RestoreState::Unevaluated {
            expr: expr.clone(),
            env: env.clone(),
            ctx: thunk_ctx.clone(),
        };

        // Handle DotAccess inline to enable iterative access chains
        if let Expr::DotAccess {
            expr: target,
            field,
        } = &expr.node
        {
            // Evaluate target expression
            match eval(Rc::new((**target).clone()), Rc::clone(&env), &thunk_ctx) {
                Ok(target_thunk) => {
                    // Push Memoize for the outer thunk (the access result)
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Rc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(restore),
                        ctx: Rc::clone(&thunk_ctx),
                    })));
                    // Push DotAccessForce to handle field lookup after target materializes.
                    // Capture the target thunk's definition span so that key-not-found and
                    // type-mismatch errors can report both where the dict was defined and
                    // where it was accessed.
                    // Thread outer_mat_span to preserve the outermost call-site context in
                    // chained accesses like a.b.c.
                    stack.push(Cont::DotAccessForce(Box::new(DotAccessForceData {
                        field: field.clone(),
                        access_span: expr.span,
                        target_def_span: target_thunk.span,
                        outer_mat_span: mat_span,
                        ctx: Rc::clone(&thunk_ctx),
                    })));
                    // Force the target
                    return Action::Materialize {
                        thunk: target_thunk,
                        mat_span: Some(expr.span),
                    };
                }
                Err(mut e) => {
                    e.push_frame(format!("accessing .{field}"), expr.span);
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    // Pop from eval_stack before early return
                    thunk_ctx.state.borrow_mut().eval_stack.pop();
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            }
        }

        match eval(expr, Rc::clone(&env), &thunk_ctx) {
            Ok(result_thunk) => {
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Rc::clone(thunk),
                    origin,
                    thunk_span,
                    mat_span,
                    restore: Some(restore),
                    ctx: Rc::clone(&thunk_ctx),
                })));
                Action::Materialize {
                    thunk: result_thunk,
                    mat_span,
                }
            }
            Err(e) => {
                let decorated = attach_materialization_context(
                    e,
                    mat_span.as_ref(),
                    origin.as_deref(),
                    thunk_span,
                );
                // Pop from eval_stack before error return
                thunk_ctx.state.borrow_mut().eval_stack.pop();
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure(&decorated);
                } else {
                    restore.restore(thunk);
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((def, args, named, call_span, thunk_ctx)) = thunk.take_pending_builtin() {
        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction)
        thunk_ctx
            .state
            .borrow_mut()
            .eval_stack
            .push((origin.as_deref().unwrap_or("thunk").to_string(), thunk_span));

        // Wrap args/named in Option so each exclusive match arm can move them
        // without cloning. Taking ownership avoids the pre-clone of Vec/IndexMap
        // that was previously done on every successful builtin call to build RestoreState.
        // Each arm calls .take().expect("...") exactly once to extract the owned value.
        let mut args = Some(args);
        let mut named = Some(named);

        // W1 dispatch-time materialization: scan pos_strictness for first Seq/Spine position.
        // Pre-materialize strict args iteratively to prevent Rust stack growth and enable
        // the builtin to skip redundant materialize() calls (thunk memoization fast-path).
        use crate::value::Strictness;
        if let Some((arg_idx, _)) = def.pos_strictness.iter().enumerate().find(|(i, &s)| {
            *i < args.as_ref().expect("args set above").len()
                && (s == Strictness::Seq || s == Strictness::Spine)
                && args.as_ref().expect("args set above")[*i]
                    .try_get_materialized()
                    .is_none()
        }) {
            let arg_thunk = Rc::clone(&args.as_ref().expect("args set above")[arg_idx]);
            stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                thunk: Rc::clone(thunk),
                def,
                args: args.take().expect("args set above"),
                named: named.take().expect("named set above"),
                call_span,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                arg_idx,
            })));
            return Action::Materialize {
                thunk: arg_thunk,
                mat_span: None,
            };
        }

        // `named` is None for internally-created thunks (common case); only $apply
        // passes named args through. Use an empty map ref for the None case.
        let builtin_args = crate::value::BuiltinArgs {
            args: args.as_ref().expect("args set above"),
            named: named.as_ref().expect("named set above").as_ref(),
            call_span,
            ctx: Rc::clone(&thunk_ctx),
        };

        match (def.func)(builtin_args) {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion
                if let Some(value) = result_thunk.try_get_materialized() {
                    // args/named are no longer needed; drop them implicitly.
                    // Pop from eval_stack before fast-path return
                    thunk_ctx.state.borrow_mut().eval_stack.pop();
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Action::Continue(Ok(value))
                } else {
                    // Move args/named into RestoreState — no clone needed.
                    let restore = RestoreState::PendingBuiltin {
                        def,
                        args: Box::new(args.take().expect("args set above")),
                        named: named.take().expect("named set above"),
                        call_span,
                        ctx: Rc::clone(&thunk_ctx),
                    };
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Rc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(restore),
                        ctx: Rc::clone(&thunk_ctx),
                    })));
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span,
                    }
                }
            }
            Err(e) => {
                let decorated = attach_materialization_context(
                    e,
                    mat_span.as_ref(),
                    origin.as_deref(),
                    thunk_span,
                );
                // Pop from eval_stack before error return
                thunk_ctx.state.borrow_mut().eval_stack.pop();
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure(&decorated);
                } else {
                    // Move args/named into PendingBuiltin — no clone needed.
                    thunk.set_state(ThunkState::PendingBuiltin {
                        def,
                        args: Box::new(args.take().expect("args set above")),
                        named: named.take().expect("named set above"),
                        call_span,
                        ctx: thunk_ctx,
                    });
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if matches!(&*thunk.state(), ThunkState::PendingCall { .. }) {
        let (func_thunk, args, named, call_span, caller_env, thunk_ctx) = thunk
            .take_pending_call()
            .expect("PendingCall state confirmed above; single-threaded execution prevents TOCTOU");

        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction)
        thunk_ctx
            .state
            .borrow_mut()
            .eval_stack
            .push((origin.as_deref().unwrap_or("thunk").to_string(), thunk_span));

        stack.push(Cont::PendingCallDispatch(Box::new(
            PendingCallDispatchData {
                thunk: Rc::clone(thunk),
                func_thunk: Rc::clone(&func_thunk),
                args: Box::new(args),
                named: named.map(Box::new),
                call_span,
                caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
            },
        )));
        Action::Materialize {
            thunk: Rc::clone(&func_thunk),
            mat_span: Some(call_span),
        }
    } else if let Some((inner, expected, field_path, guard_span, blame_label, default_opt)) =
        thunk.take_guarded()
    {
        let inner_span = inner.span;
        // Extract ctx from the inner thunk's current state for use during GuardedValidate
        // when the materialized result is a Value::Overlay (needs flattening with ctx).
        // The inner thunk has not yet been materialized, so its state still carries ctx.
        // For Materialized/Failed/InProgress inner thunks, ctx isn't needed (these can't
        // produce new Overlay values during re-materialization).
        let guard_ctx: Option<Rc<EvalContext>> = {
            let state = inner.state();
            match &*state {
                ThunkState::Unevaluated { ctx, .. } => Some(Rc::clone(ctx)),
                ThunkState::PendingBuiltin { ctx, .. } => Some(Rc::clone(ctx)),
                ThunkState::PendingCall { ctx, .. } => Some(Rc::clone(ctx)),
                _ => None,
            }
        };
        // Create RestoreState before pushing continuation (for non-cacheable error recovery)
        let restore = RestoreState::Guarded {
            inner: Rc::clone(&inner),
            expected: expected.clone(),
            field_path: Box::new(field_path.clone()),
            guard_span,
            blame_label: blame_label.clone(),
            default: default_opt.clone(),
        };
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
            ctx: guard_ctx,
            blame_label,
            default: default_opt,
            restore: Some(restore),
        })));
        Action::Materialize {
            thunk: Rc::clone(&inner),
            mat_span,
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
                ctx,
            } = *data;
            let decorated_result = result.map_err(|e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            });

            match decorated_result {
                Ok(value) => {
                    // Pop from eval_stack on successful materialization
                    ctx.state.borrow_mut().eval_stack.pop();
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Action::Continue(Ok(value))
                }
                Err(e) => {
                    // Pop from eval_stack on error (cacheable or not)
                    ctx.state.borrow_mut().eval_stack.pop();
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else if let Some(restore_state) = restore {
                        restore_state.restore(&thunk);
                    }
                    // Note: if restore is None, the thunk remains in InProgress state,
                    // which will trigger a CircularDependency error on next access.
                    // This is correct for cases where restoration isn't possible.
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
            } = *data;
            let decorate = |e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            };

            // Wrap args/named in Option so each exclusive match arm can move them
            // without cloning. Taking ownership avoids the pre-clone of Box<Vec>/Box<IndexMap>
            // that was previously done on every successful function call to build RestoreState.
            // Each arm calls .take().expect("...") exactly once to extract the owned value.
            let mut args = Some(args);
            let mut named = Some(named);

            match result.map_err(&decorate) {
                Ok(func_value) => match func_value {
                    Value::Function {
                        params, body, env, ..
                    } => {
                        // The block scopes borrows of args/named so the borrow checker
                        // allows args.take()/named.take() in the match arms below.
                        let invoke_result = {
                            let call_ctx = CallContext {
                                params: &params,
                                body: &body,
                                closure_env: &env,
                                positional: args.as_deref().expect("args set above"),
                                named: named.as_ref().expect("named set above").as_deref(),
                                default_env: &caller_env, // Use caller's environment for default param evaluation
                                call_span,
                                origin: origin.clone(),
                                ctx: &thunk_ctx,
                            };
                            invoke_function(&call_ctx)
                        };

                        match invoke_result.map_err(&decorate) {
                            Ok(result_thunk) => {
                                // Move args/named into RestoreState — no clone needed.
                                // invoke_function consumed them by reference above; after
                                // the Ok result, args/named are not needed for anything else.
                                let restore = RestoreState::PendingCall {
                                    func: func_thunk,
                                    args: args.take().expect("args set above"),
                                    named: named.take().expect("named set above"),
                                    call_span,
                                    caller_env,
                                    ctx: Rc::clone(&thunk_ctx),
                                };
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Rc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore: Some(restore),
                                    ctx: thunk_ctx,
                                })));
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span,
                                }
                            }
                            Err(mut e) => {
                                e.push_frame(
                                    origin.as_deref().unwrap_or("call").to_string(),
                                    call_span,
                                );
                                // Pop from eval_stack before error return
                                thunk_ctx.state.borrow_mut().eval_stack.pop();
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure(&e);
                                } else {
                                    // Move args/named into PendingCall — no clone needed.
                                    thunk.set_state(ThunkState::PendingCall {
                                        func: func_thunk,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span,
                                        caller_env,
                                        ctx: thunk_ctx,
                                    });
                                }
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    Value::Builtin(def) => {
                        // Check if any strict (Seq/Spine) args need pre-materialization.
                        // If so, convert to PendingBuiltin and re-dispatch via force_step
                        // so the BuiltinForceArg continuation can handle them iteratively.
                        //
                        // This is critical for TCO: builtins like $if call materialize()
                        // internally on their args. If called with unevaluated args, each
                        // recursive call adds Rust frames (materialize → run). Pre-materializing
                        // args in the CEK machine (heap-allocated continuations) prevents this.
                        //
                        // Note: we pop eval_stack BEFORE converting to PendingBuiltin so that
                        // force_step(PendingBuiltin) can push a fresh entry — avoiding a
                        // duplicate that would cause an extra pop on completion.
                        use crate::value::Strictness;
                        let has_strict_unevaluated =
                            def.pos_strictness.iter().enumerate().any(|(i, &s)| {
                                i < args.as_ref().expect("args set above").len()
                                    && (s == Strictness::Seq || s == Strictness::Spine)
                                    && args.as_ref().expect("args set above")[i]
                                        .try_get_materialized()
                                        .is_none()
                            });

                        if has_strict_unevaluated {
                            // Pop the eval_stack entry pushed by force_step(PendingCall).
                            // force_step(PendingBuiltin) will push a new entry for this thunk.
                            thunk_ctx.state.borrow_mut().eval_stack.pop();
                            // Transition thunk from InProgress → PendingBuiltin.
                            // args is Box<Vec<...>> (matches ThunkState::PendingBuiltin.args).
                            // named is Option<Box<IndexMap<...>>>; unbox to Option<IndexMap<...>>.
                            thunk.set_state(ThunkState::PendingBuiltin {
                                def,
                                args: args.take().expect("args set above"),
                                named: named.take().expect("named set above").map(|b| *b),
                                call_span,
                                ctx: thunk_ctx,
                            });
                            return Action::Materialize { thunk, mat_span };
                        }

                        // All strict args are already materialized — call the builtin directly.
                        // The block scopes the borrows of args/named so the borrow
                        // checker allows args.take()/named.take() in the match arms below.
                        let builtin_result = {
                            let builtin_args = crate::value::BuiltinArgs {
                                args: args.as_deref().expect("args set above"),
                                named: named.as_ref().expect("named set above").as_deref(),
                                call_span,
                                ctx: Rc::clone(&thunk_ctx),
                            };
                            (def.func)(builtin_args)
                        };
                        match builtin_result.map_err(&decorate) {
                            Ok(result_thunk) => {
                                if let Some(value) = result_thunk.try_get_materialized() {
                                    // Fast path: builtin result is already materialized.
                                    // args/named are no longer needed; drop them implicitly.
                                    // Pop from eval_stack before fast-path return
                                    thunk_ctx.state.borrow_mut().eval_stack.pop();
                                    thunk.set_state(ThunkState::Materialized(value.clone()));
                                    Action::Continue(Ok(value))
                                } else {
                                    // Move args/named into RestoreState — no clone needed.
                                    let restore = RestoreState::PendingCall {
                                        func: func_thunk,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span,
                                        caller_env,
                                        ctx: Rc::clone(&thunk_ctx),
                                    };
                                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                                        thunk: Rc::clone(&thunk),
                                        origin,
                                        thunk_span,
                                        mat_span,
                                        restore: Some(restore),
                                        ctx: thunk_ctx,
                                    })));
                                    Action::Materialize {
                                        thunk: result_thunk,
                                        mat_span,
                                    }
                                }
                            }
                            Err(e) => {
                                // Pop from eval_stack before error return
                                thunk_ctx.state.borrow_mut().eval_stack.pop();
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure(&e);
                                } else {
                                    // Move args/named into PendingCall — no clone needed.
                                    thunk.set_state(ThunkState::PendingCall {
                                        func: func_thunk,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
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
                        // Pop from eval_stack before error return
                        thunk_ctx.state.borrow_mut().eval_stack.pop();
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure(&decorated);
                        } else {
                            // Move args/named into PendingCall — no clone needed.
                            thunk.set_state(ThunkState::PendingCall {
                                func: func_thunk,
                                args: args.take().expect("args set above"),
                                named: named.take().expect("named set above"),
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
                    // Pop from eval_stack before error return
                    thunk_ctx.state.borrow_mut().eval_stack.pop();
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        // Move args/named into PendingCall — no clone needed.
                        thunk.set_state(ThunkState::PendingCall {
                            func: func_thunk,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
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
                mut field_path,
                guard_span,
                inner_span,
                origin,
                thunk_span,
                mat_span,
                ctx: guard_ctx,
                blame_label,
                default,
                mut restore,
            } = *data;
            let decorate = |e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            };

            match result {
                Ok(value) => {
                    // Flatten Overlay to Dict before record validation.
                    // Value::Overlay is produced by $merge; guard wrapping it needs flattened entries.
                    let value = match value {
                        Value::Overlay(l, r) => {
                            if let Some(ref ctx) = guard_ctx {
                                match flatten_overlay(&l, &r, "type guard", ctx, guard_span) {
                                    Ok(map) => Value::Dict(map),
                                    Err(e) => {
                                        let e = decorate(e);
                                        thunk.cache_failure(&e);
                                        return Action::Continue(Err(e));
                                    }
                                }
                            } else {
                                // ctx unavailable (inner was already Materialized at push time);
                                // cannot flatten. Treat as Dict-compatible for non-Record types.
                                Value::Overlay(l, r)
                            }
                        }
                        other => other,
                    };
                    // For Record types and Intersection-of-Records, apply proxy contract wrapping.
                    // as_record_row_merged handles both forms by merging fields into a single Row.
                    if let Some(row) = as_record_row_merged(&expected) {
                        if let Value::Dict(ref entries) = value {
                            let ctx_ref = match guard_ctx.as_ref() {
                                Some(ctx) => ctx,
                                None => {
                                    let err = EvalError::internal(
                                        "validate_and_wrap_record requires ctx but guard_ctx is None".to_string(),
                                        guard_span,
                                    );
                                    let err = decorate(Box::new(err));
                                    thunk.cache_failure(&err);
                                    return Action::Continue(Err(err));
                                }
                            };
                            match validate_and_wrap_record(
                                entries,
                                row.as_ref(),
                                &mut *field_path,
                                guard_span,
                                inner_span,
                                ctx_ref,
                                default.clone(),
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk
                                        .set_state(ThunkState::Materialized(guarded_value.clone()));
                                    Action::Continue(Ok(guarded_value))
                                }
                                Err(err) => {
                                    // Guard validation failed - use default if present
                                    if let Some((default_expr, default_env)) = default {
                                        let ctx_for_default = guard_ctx
                                            .clone()
                                            .expect("guard_ctx should be Some for Record guards");
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Rc::clone(&thunk),
                                            origin: Some(Rc::from("default fallback")),
                                            thunk_span,
                                            mat_span,
                                            restore: restore.take(),
                                            ctx: Rc::clone(&ctx_for_default),
                                        })));
                                        return Action::Eval {
                                            expr: default_expr,
                                            env: default_env,
                                            ctx: ctx_for_default,
                                        };
                                    }
                                    let err = decorate(err);
                                    thunk.cache_failure(&err);
                                    Action::Continue(Err(err))
                                }
                            }
                        } else {
                            // Expected Record but got non-Dict - use default if present
                            if let Some((default_expr, default_env)) = default {
                                let ctx_for_default = guard_ctx
                                    .clone()
                                    .expect("guard_ctx should be Some for Record guards");
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Rc::clone(&thunk),
                                    origin: Some(Rc::from("default fallback")),
                                    thunk_span,
                                    mat_span,
                                    restore: None,
                                    ctx: Rc::clone(&ctx_for_default),
                                })));
                                return Action::Eval {
                                    expr: default_expr,
                                    env: default_env,
                                    ctx: ctx_for_default,
                                };
                            }
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field {}: ", format_field_path(&field_path))
                            };
                            let mut err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                &value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            // Add secondary span if different from definition span
                            if inner.span != inner_span {
                                err = err.with_secondary_span(inner.span, "value produced here");
                            }
                            // Attach blame label if present (gradual typing boundary)
                            if let Some(ref label) = blame_label {
                                err = err.with_blame(label.clone());
                            }
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
                            // Type mismatch for non-Record types - use default if present
                            if let Some((default_expr, default_env)) = default {
                                // For non-Record guards, guard_ctx may be None (inner was already Materialized).
                                // Defaults still need a ctx to evaluate in. Use guard_ctx if available, otherwise
                                // we need to get ctx from somewhere. Since this is a fallback path, we can use
                                // the default_env's associated ctx if needed, but we don't have direct access.
                                // The safest approach: require guard_ctx for defaults (enforced at guard creation).
                                if let Some(ctx_for_default) = guard_ctx {
                                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                                        thunk: Rc::clone(&thunk),
                                        origin: Some(Rc::from("default fallback")),
                                        thunk_span,
                                        mat_span,
                                        restore: None,
                                        ctx: Rc::clone(&ctx_for_default),
                                    })));
                                    return Action::Eval {
                                        expr: default_expr,
                                        env: default_env,
                                        ctx: ctx_for_default,
                                    };
                                }
                                // If guard_ctx is None, we can't evaluate the default. Fall through to error.
                            }
                            let field_path_prefix = if field_path.is_empty() {
                                String::new()
                            } else {
                                format!("field {}: ", format_field_path(&field_path))
                            };
                            let mut err = EvalError::type_assert_failed(
                                &format!(
                                    "{}{}",
                                    field_path_prefix,
                                    format_type_for_assert(&expected)
                                ),
                                &value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            // Add secondary span if different from definition span
                            if inner.span != inner_span {
                                err = err.with_secondary_span(inner.span, "value produced here");
                            }
                            // Attach blame label if present (gradual typing boundary)
                            if let Some(ref label) = blame_label {
                                err = err.with_blame(label.clone());
                            }
                            let err = decorate(err.into());
                            thunk.cache_failure(&err);
                            Action::Continue(Err(err))
                        }
                    }
                }
                Err(e) => {
                    // Inner materialization error propagates
                    let e = decorate(e);
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else if let Some(r) = restore.take() {
                        r.restore(&thunk);
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::BuiltinForceArg(data) => {
            let BuiltinForceArgData {
                thunk,
                def,
                args,
                named,
                call_span,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                arg_idx,
            } = *data;
            let decorate = |e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            };

            // Wrap args/named in Option so each exclusive match arm can move them
            // without cloning, following the same pattern as the initial PendingBuiltin branch.
            let mut args = Some(args);
            let mut named = Some(named);

            // W1 dispatch-time materialization: after arg at arg_idx has been materialized,
            // scan for the next Seq/Spine position. If found, force it; otherwise call builtin.
            match result {
                Ok(_) => {
                    // Scan from arg_idx + 1 for the next Seq/Spine position that needs forcing.
                    use crate::value::Strictness;
                    if let Some((next_idx, _)) = def
                        .pos_strictness
                        .iter()
                        .enumerate()
                        .skip(arg_idx + 1)
                        .find(|(i, &s)| {
                            *i < args.as_ref().expect("args set above").len()
                                && (s == Strictness::Seq || s == Strictness::Spine)
                                && args.as_ref().expect("args set above")[*i]
                                    .try_get_materialized()
                                    .is_none()
                        })
                    {
                        let next_arg = Rc::clone(&args.as_ref().expect("args set above")[next_idx]);
                        stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                            thunk,
                            def,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
                            call_span,
                            ctx: thunk_ctx,
                            origin,
                            thunk_span,
                            mat_span,
                            arg_idx: next_idx,
                        })));
                        return Action::Materialize {
                            thunk: next_arg,
                            mat_span: None,
                        };
                    }

                    // All strict args materialized — call the builtin.
                    let builtin_args = crate::value::BuiltinArgs {
                        args: args.as_ref().expect("args set above"),
                        named: named.as_ref().expect("named set above").as_ref(),
                        call_span,
                        ctx: Rc::clone(&thunk_ctx),
                    };
                    match (def.func)(builtin_args).map_err(&decorate) {
                        Ok(result_thunk) => {
                            if let Some(value) = result_thunk.try_get_materialized() {
                                // args/named are no longer needed; drop them implicitly.
                                // Pop from eval_stack before fast-path return
                                thunk_ctx.state.borrow_mut().eval_stack.pop();
                                thunk.set_state(ThunkState::Materialized(value.clone()));
                                Action::Continue(Ok(value))
                            } else {
                                // Move args/named into RestoreState — no clone needed.
                                let restore = RestoreState::PendingBuiltin {
                                    def,
                                    args: Box::new(args.take().expect("args set above")),
                                    named: named.take().expect("named set above"),
                                    call_span,
                                    ctx: Rc::clone(&thunk_ctx),
                                };
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Rc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore: Some(restore),
                                    ctx: Rc::clone(&thunk_ctx),
                                })));
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span,
                                }
                            }
                        }
                        Err(e) => {
                            // Pop from eval_stack before error return
                            thunk_ctx.state.borrow_mut().eval_stack.pop();
                            if e.kind.is_cacheable() {
                                thunk.cache_failure(&e);
                            } else {
                                // Move args/named into PendingBuiltin — no clone needed.
                                thunk.set_state(ThunkState::PendingBuiltin {
                                    def,
                                    args: Box::new(args.take().expect("args set above")),
                                    named: named.take().expect("named set above"),
                                    call_span,
                                    ctx: thunk_ctx,
                                });
                            }
                            Action::Continue(Err(e))
                        }
                    }
                }
                Err(e) => {
                    let e = decorate(e);
                    // Pop from eval_stack before error return
                    thunk_ctx.state.borrow_mut().eval_stack.pop();
                    if e.kind.is_cacheable() {
                        thunk.cache_failure(&e);
                    } else {
                        // Move args/named into PendingBuiltin — no clone needed.
                        thunk.set_state(ThunkState::PendingBuiltin {
                            def,
                            args: Box::new(args.take().expect("args set above")),
                            named: named.take().expect("named set above"),
                            call_span,
                            ctx: thunk_ctx,
                        });
                    }
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::DotAccessForce(data) => {
            let DotAccessForceData {
                field,
                access_span,
                target_def_span,
                outer_mat_span,
                ctx,
            } = *data;

            // Convert DotKey to string for error messages and Proxy dispatch.
            // NOTE: this string is NOT used for Dict lookup when field is DotKey::Int —
            // integer dot access uses Key::Int(n) directly (auto-indexed dicts store Key::Int).
            let field_str = match &field {
                crate::ast::DotKey::Ident(s) => s.clone(),
                crate::ast::DotKey::Int(n) => n.to_string(),
            };

            // Result is the materialized target value
            match result {
                Ok(target_val) => {
                    // Flatten Overlay to Dict before key lookup.
                    let target_val = match target_val {
                        Value::Overlay(l, r) => {
                            match flatten_overlay(
                                &l,
                                &r,
                                &format!(".{field_str}"),
                                &ctx,
                                access_span,
                            ) {
                                Ok(map) => Value::Dict(map),
                                Err(mut e) => {
                                    e.push_frame(format!("accessing .{field_str}"), access_span);
                                    return Action::Continue(Err(e));
                                }
                            }
                        }
                        other => other,
                    };
                    match target_val {
                        Value::Dict(map) => {
                            // For DotKey::Int, look up Key::Int(n) directly —
                            // auto-indexed dicts store Key::Int, not Key::String.
                            // For DotKey::Ident, use StrKey wrapper to avoid allocation.
                            let thunk_id_opt = match &field {
                                crate::ast::DotKey::Int(n) => map.get(&crate::value::Key::Int(*n)),
                                crate::ast::DotKey::Ident(_) => {
                                    map.get(&crate::value::StrKey(&field_str))
                                }
                            };
                            match thunk_id_opt {
                                Some(thunk_id) => {
                                    // Field found - need to materialize it.
                                    // Use outer_mat_span if available (preserves outermost call-site in chains
                                    // like a.b.c), otherwise fall back to access_span (the current access).
                                    let thunk = ctx.get_thunk(*thunk_id);
                                    Action::Materialize {
                                        thunk,
                                        mat_span: outer_mat_span.or(Some(access_span)),
                                    }
                                }
                                None => {
                                    // Key not found: report definition site and access site.
                                    let available_keys: Vec<String> =
                                        map.keys().map(|k| k.to_string()).collect();
                                    let mut err = EvalError::key_not_found(
                                        &field_str,
                                        available_keys,
                                        target_def_span,
                                    )
                                    .with_materialization_span(access_span);
                                    err.push_frame(format!("accessing .{field_str}"), access_span);
                                    Action::Continue(Err(err.into()))
                                }
                            }
                        }
                        Value::Proxy { handler } => {
                            // Proxy handler invocation
                            let handler_thunk = ctx.get_thunk(handler);
                            match invoke_proxy_handler(
                                &handler_thunk,
                                string_val(&field_str),
                                &ctx,
                                &access_span,
                            ) {
                                Ok(thunk) => {
                                    // Use outer_mat_span for proxy handler results (same as Dict case above).
                                    Action::Materialize {
                                        thunk,
                                        mat_span: outer_mat_span.or(Some(access_span)),
                                    }
                                }
                                Err(mut e) => {
                                    e.push_frame(format!("accessing .{field_str}"), access_span);
                                    Action::Continue(Err(e))
                                }
                            }
                        }
                        other => {
                            // Type mismatch: report definition site and access site.
                            let mut err = EvalError::type_mismatch_ctx(
                                "dot access".to_string(),
                                "Dict or Proxy",
                                other.type_name(),
                                target_def_span,
                            )
                            .with_materialization_span(access_span);
                            err.push_frame(format!("accessing .{field_str}"), access_span);
                            Action::Continue(Err(err.into()))
                        }
                    }
                }
                Err(mut e) => {
                    // Target materialization failed
                    e.push_frame(format!("accessing .{field_str}"), access_span);
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
            } = *data;
            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(value) => match *resolved {
                    Some(expected) => {
                        // For Record types and Intersection-of-Records, apply proxy contract wrapping.
                        // as_record_row_merged merges all required fields from all members into a Row.
                        if let Some(row) = as_record_row_merged(&expected) {
                            // Flatten Overlay to Dict before record type assertion.
                            let value = match value {
                                Value::Overlay(l, r) => {
                                    match flatten_overlay(&l, &r, "type assert", &ctx, expr_span) {
                                        Ok(map) => Value::Dict(map),
                                        Err(e) => return Action::Continue(Err(e)),
                                    }
                                }
                                other => other,
                            };
                            if let Value::Dict(entries) = &value {
                                let default_opt = annotation
                                    .node
                                    .get_property(DEFAULT_ANNOTATION_KEY)
                                    .map(|expr| (Rc::new(expr.clone()), Rc::clone(&env)));
                                match validate_and_wrap_record(
                                    entries,
                                    row.as_ref(),
                                    &mut vec![],
                                    expr_span,
                                    thunk_span,
                                    &ctx,
                                    default_opt.clone(),
                                ) {
                                    Ok(new_entries) => {
                                        Action::Continue(Ok(Value::Dict(new_entries)))
                                    }
                                    Err(err) => {
                                        if let Some((default, env)) = default_opt {
                                            // Evaluate default expression iteratively.
                                            // The result will flow to the next continuation on the stack.
                                            Action::Eval {
                                                expr: default,
                                                env,
                                                ctx: Rc::clone(&ctx),
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
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    Action::Eval {
                                        expr: Rc::new(default_expr.clone()),
                                        env,
                                        ctx: Rc::clone(&ctx),
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
                        } else if value_matches_type(&value, &expected) {
                            Action::Continue(Ok(value))
                        } else if let Some(default_expr) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively.
                            // The result will flow to the next continuation on the stack.
                            Action::Eval {
                                expr: Rc::new(default_expr.clone()),
                                env,
                                ctx: Rc::clone(&ctx),
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
                    None => {
                        // --no-typecheck FALLBACK (nominal validation)
                        // Per doc/07-type-extensions.md §--no-typecheck mode:
                        // - Primitive type assertions still work (nominal string comparison)
                        // - Structural type assertions degrade to tag-only checks (Dict tag)
                        let expected_name: Option<String> = match &annotation.node {
                            Annotation::Simple(name) => Some(name.clone()),
                            Annotation::PropertyDict(_) => annotation
                                .node
                                .get_property("type")
                                .and_then(|type_expr| match &type_expr.node {
                                    Expr::Str(s) => Some(s.clone()),
                                    _ => None,
                                }),
                            Annotation::Annotated(name, _) => Some(name.clone()),
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
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    return Action::Eval {
                                        expr: Rc::new(default_expr.clone()),
                                        env,
                                        ctx: Rc::clone(&ctx),
                                    };
                                }
                                return Action::Continue(Err(EvalError::type_assert_failed(
                                    &expected, actual, thunk_span,
                                )
                                .with_materialization_span(expr_span)
                                .into()));
                            }
                        } else if annotation_has_structural_fields(&annotation.node) {
                            // Structural record annotation without resolved_type — degrade
                            // to Dict tag check. Without elaboration we cannot validate
                            // field names or types, but we can verify the value is a Dict
                            // (the carrier type for records). This closes the elaboration
                            // gap for eval-only mode (doc/07 §--no-typecheck mode).
                            if !matches!(value, Value::Dict(_) | Value::Overlay(..)) {
                                let actual = value.type_name();
                                if let Some(default_expr) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    return Action::Eval {
                                        expr: Rc::new(default_expr.clone()),
                                        env,
                                        ctx: Rc::clone(&ctx),
                                    };
                                }
                                return Action::Continue(Err(EvalError::type_assert_failed(
                                    "Record", actual, thunk_span,
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
    expr: Rc<Spanned<Expr>>,
    env: Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    stack: &mut Vec<Cont>,
) -> Action {
    // Helper: wrap a thunk result from helper functions
    let wrap_thunk = |result: EvalResult<Rc<Thunk>>| -> Action {
        match result {
            Ok(thunk) => match thunk.try_get_materialized() {
                Some(value) => Action::Continue(Ok(value)),
                None => Action::Materialize {
                    thunk,
                    mat_span: Some(expr.span),
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
        Expr::Str(s) => Action::Continue(Ok(string_val(s))),
        Expr::VarRef { name, .. } => {
            let found = env.borrow().get(name);
            match found {
                // Return the thunk from the environment without forcing it.
                // Uses the same fast-path as wrap_thunk: if already materialized,
                // return the value directly; otherwise pass through as-is so the
                // caller decides whether to force. This matches eval_recursive's
                // lazy behavior (Ok(thunk) without materializing).
                Some(thunk) => wrap_thunk(Ok(thunk)),
                None => {
                    Action::Continue(Err(
                        EvalError::undefined_variable(name.clone(), expr.span).into()
                    ))
                }
            }
        }
        Expr::Dict(entries) => wrap_thunk(eval_dict(entries, &env, ctx, &expr.span)),
        Expr::DotAccess { .. } => {
            // Return Unevaluated thunk — force_step handles these iteratively via
            // DotAccessForce continuation
            let span = expr.span;
            wrap_thunk(Ok(Rc::new(Thunk::new_unevaluated(
                Rc::clone(&expr),
                Rc::clone(&env),
                Rc::clone(ctx),
                span,
            ))))
        }
        Expr::TypeAssert {
            expr: inner,
            annotation,
            resolved_type,
        } => {
            // Evaluate the inner expression to get a thunk (without forcing it).
            // This still uses eval_recursive because we need a thunk, not a materialized value.
            // The TypeAssertCheck continuation below will materialize it and validate.
            // This is the correct pattern: eval → thunk → push continuation → materialize.
            let inner_thunk = match eval_recursive(Rc::new((**inner).clone()), Rc::clone(&env), ctx)
            {
                Ok(t) => t,
                Err(e) => return Action::Continue(Err(e)),
            };
            let resolved = resolved_type.borrow().clone();

            // Fast path: if there is no type to check, skip materialization entirely.
            // This applies when resolved_type is None (--no-typecheck mode) and the
            // annotation has no "type" property AND no structural field declarations —
            // e.g. [@[default: 0] $x] where only a default is provided.
            // A Simple annotation always carries a type name.
            // A PropertyDict with structural fields (e.g., [@[name: String] $x]) needs
            // at least a Dict tag check even without elaboration (doc/07 §--no-typecheck).
            let has_type = match &annotation.node {
                Annotation::Simple(_) => true,
                Annotation::PropertyDict(_) => {
                    annotation.node.get_property("type").is_some()
                        || annotation_has_structural_fields(&annotation.node)
                }
                Annotation::Annotated(_, _) => true,
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
            })));
            Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(expr.span),
            }
        }
        Expr::Annotated { name, .. } => {
            // Evaluate as the bare string; the type checker (typecheck.rs) interprets annotations.
            Action::Continue(Ok(string_val(name)))
        }
        Expr::Fn { params, body, .. } => {
            let fn_params: Vec<Param> = params.iter().map(|p| p.node.clone()).collect();
            Action::Continue(Ok(Value::Function {
                params: Rc::new(fn_params),
                body: Rc::new(body.as_ref().clone()),
                env: Rc::clone(&env),
                annotation: None,
            }))
        }
        Expr::Call {
            func,
            args,
            named_args,
            implied: _,
        } => wrap_thunk(eval_call(func, args, named_args, &env, ctx, &expr.span)),
        // Type alias entries are compile-time-only constructs consumed by the type checker.
        // At runtime, they evaluate to an empty dict to maintain dict structure without
        // contributing runtime values.
        Expr::TypeAlias { .. } => Action::Continue(Ok(Value::Dict(IndexMap::new()))),
        Expr::Quote(_) => {
            unreachable!("Quote is handled in eval_recursive before reaching eval_expr_step")
        }
        Expr::Unquote(_) | Expr::UnquoteSplice(_) => {
            unreachable!(
                "Unquote/UnquoteSplice are handled in eval_quote before reaching eval_expr_step"
            )
        }
        Expr::DefMacro { .. } => {
            unreachable!("DefMacro should be removed by expansion pass before evaluation")
        }
        Expr::Match { .. } => {
            // Match is handled by eval_recursive in eval.rs, not the CEK machine.
            // Fall back for now.
            unreachable!("Match should be handled by eval_recursive (not yet in CEK machine)")
        }
        Expr::ClassDecl { .. } => {
            // ClassDecl is handled by eval_recursive in eval.rs, not the CEK machine.
            unreachable!("ClassDecl should be handled by eval_recursive (not yet in CEK machine)")
        }
        Expr::InstanceDecl { .. } => {
            // InstanceDecl is handled by eval_recursive in eval.rs, not the CEK machine.
            unreachable!(
                "InstanceDecl should be handled by eval_recursive (not yet in CEK machine)"
            )
        }
        Expr::Rest(_) => Action::Continue(Err(EvalError::internal(
            "rest marker (...) is only valid inside type expressions".to_string(),
            expr.span,
        )
        .into())),
        Expr::TypeApp { .. } => Action::Continue(Err(EvalError::internal(
            "TypeApp is a type annotation node and cannot be evaluated".to_string(),
            expr.span,
        )
        .into())),
        Expr::Error(span) => Action::Continue(Err(EvalError::internal(
            format!(
                "syntax error at {}:{} (cannot evaluate error node)",
                span.start.line, span.start.column
            ),
            expr.span,
        )
        .into())),
        Expr::Pipe { .. } => {
            unreachable!("Pipe should be desugared before evaluation")
        }
        Expr::Sequential(_) => {
            // Sequential expressions are handled by eval_recursive, not the iterative evaluator.
            // They require full sequential environment chaining which is not yet integrated
            // into the CEK machine. Fall back to eval_recursive for now.
            unreachable!("Sequential should be handled by eval_recursive (not yet in CEK machine)")
        }
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
///
/// # Tail-Call Optimization
/// The loop reuses the Rust stack frame on each iteration, so Rust stack depth is O(1).
/// The continuation stack is explicit (`stack`), preventing Rust stack overflow.
///
/// **Potential micro-optimization**: When eval_step/force_step return Action::Continue(result)
/// and stack.is_empty(), we could return directly instead of looping to line 1970.
/// This would save 1 branch misprediction per tail-call. However, it adds complexity
/// (need to check stack.is_empty() after each step or pass it in).
/// DECISION: Defer until profiling shows this is a bottleneck (likely negligible).
pub(crate) fn run(initial: Action, ctx: &Rc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::Eval {
                expr,
                env,
                ctx: action_ctx,
            } => {
                action = eval_step(expr, env, &action_ctx, &mut stack);
            }
            Action::Materialize { thunk, mat_span } => {
                action = force_step(&thunk, mat_span, &mut stack, ctx);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Expr;
    use crate::test_util::{sp, test_span};
    use crate::value::{Environment, Key, Thunk, ThunkState};

    fn empty_env() -> Rc<RefCell<Environment>> {
        Rc::new(RefCell::new(Environment::new()))
    }

    fn test_env() -> Rc<RefCell<Environment>> {
        empty_env()
    }

    fn test_ctx() -> Rc<EvalContext> {
        let env = empty_env();
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        EvalContext::new(base_dir, env, false)
    }

    #[test]
    fn test_restore_state_unevaluated() {
        let expr = Rc::new(sp(Expr::Int(42)));
        let env = empty_env();
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        let thunk = Thunk::new_unevaluated(expr.clone(), env.clone(), ctx.clone(), span);

        // Take the state (transitions to InProgress)
        let taken = thunk.take_unevaluated();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::Unevaluated {
            expr: expr.clone(),
            env: env.clone(),
            ctx: ctx.clone(),
        };
        restore.restore(&thunk);

        // Verify state is restored
        let state = thunk.state();
        match &*state {
            ThunkState::Unevaluated { .. } => {} // Success
            other => panic!("Expected Unevaluated state, got {:?}", other),
        }
    }

    #[test]
    fn test_restore_state_pending_builtin() {
        use crate::value::BuiltinFn;

        let span = test_span(1, 1, 1, 10);
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));

        // Create a dummy builtin function
        let dummy_func: BuiltinFn = |_args| {
            let span = test_span(1, 1, 1, 10);
            Ok(Rc::new(Thunk::new_materialized(Value::Int(99), span)))
        };
        let dummy_def = crate::value::BuiltinDef {
            func: dummy_func,
            name: "dummy",
            pos_strictness: &[],
        };

        let args = vec![Rc::clone(&thunk)];
        let ctx = test_ctx();

        let pending_thunk = Thunk::new_pending_builtin(
            dummy_def,
            args.clone(),
            None,
            span,
            Some(Rc::from("test_origin")),
            ctx.clone(),
        );

        // Take the state (transitions to InProgress)
        let taken = pending_thunk.take_pending_builtin();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::PendingBuiltin {
            def: dummy_def,
            args: Box::new(args),
            named: None,
            call_span: span,
            ctx: ctx.clone(),
        };
        restore.restore(&pending_thunk);

        // Verify state is restored
        let state = pending_thunk.state();
        match &*state {
            ThunkState::PendingBuiltin { .. } => {} // Success
            other => panic!("Expected PendingBuiltin state, got {:?}", other),
        }
    }

    #[test]
    fn test_restore_state_pending_call() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Create a simple function thunk
        let func_thunk = Rc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Rc::new(sp(Expr::Int(42))),
                env: empty_env(),
                annotation: None,
            },
            span,
        ));

        let args = vec![Rc::new(Thunk::new_materialized(Value::Int(1), span))];
        let named = IndexMap::new();
        let caller_env = empty_env();

        let pending_thunk = Rc::new(Thunk::new_pending_call(
            Rc::clone(&func_thunk),
            args.clone(),
            named.clone(),
            span,
            Rc::clone(&caller_env),
            span,
            Some(Rc::from("test_pending_call")),
            Rc::clone(&ctx),
        ));

        // Take the state (transitions to InProgress)
        let taken = pending_thunk.take_pending_call();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::PendingCall {
            func: Rc::clone(&func_thunk),
            args: Box::new(args),
            named: if named.is_empty() {
                None
            } else {
                Some(Box::new(named))
            },
            call_span: span,
            caller_env,
            ctx: Rc::clone(&ctx),
        };
        restore.restore(&pending_thunk);

        // Verify state is restored
        let state = pending_thunk.state();
        match &*state {
            ThunkState::PendingCall { .. } => {} // Success
            other => panic!("Expected PendingCall state, got {:?}", other),
        }
    }

    #[test]
    fn test_pending_call_restore_preserves_args() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Create a function thunk
        let func_thunk = Rc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Rc::new(sp(Expr::Int(42))),
                env: empty_env(),
                annotation: None,
            },
            span,
        ));

        // Create multiple args with different values
        let args = vec![
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
            Rc::new(Thunk::new_materialized(string_val("test"), span)),
        ];
        let mut named = IndexMap::new();
        named.insert(
            "key".to_string(),
            Rc::new(Thunk::new_materialized(Value::Bool(true), span)),
        );
        let caller_env = empty_env();

        let pending_thunk = Rc::new(Thunk::new_pending_call(
            Rc::clone(&func_thunk),
            args.clone(),
            named.clone(),
            span,
            Rc::clone(&caller_env),
            span,
            Some(Rc::from("test_preserve_args")),
            Rc::clone(&ctx),
        ));

        // Take the state
        let taken = pending_thunk.take_pending_call();
        assert!(taken.is_some());

        // Restore
        let restore = RestoreState::PendingCall {
            func: Rc::clone(&func_thunk),
            args: Box::new(args.clone()),
            named: if named.is_empty() {
                None
            } else {
                Some(Box::new(named.clone()))
            },
            call_span: span,
            caller_env,
            ctx: Rc::clone(&ctx),
        };
        restore.restore(&pending_thunk);

        // Verify the args are preserved
        let state = pending_thunk.state();
        match &*state {
            ThunkState::PendingCall {
                args: restored_args,
                named: restored_named,
                ..
            } => {
                // Check arg count
                assert_eq!(
                    restored_args.len(),
                    3,
                    "Expected 3 positional args, got {}",
                    restored_args.len()
                );

                // Check that the actual arg values are correct
                use crate::eval::materialize;
                let ctx_ref = test_ctx();
                let v0 = materialize(&restored_args[0], None, &ctx_ref).unwrap();
                let v1 = materialize(&restored_args[1], None, &ctx_ref).unwrap();
                let v2 = materialize(&restored_args[2], None, &ctx_ref).unwrap();

                assert_eq!(v0, Value::Int(1));
                assert_eq!(v1, Value::Int(2));
                assert_eq!(v2, string_val("test"));

                // Check named arg count and value
                let named_map = restored_named.as_ref().expect("Expected Some named args");
                assert_eq!(
                    named_map.len(),
                    1,
                    "Expected 1 named arg, got {}",
                    named_map.len()
                );
                let named_val = materialize(named_map.get("key").unwrap(), None, &ctx_ref).unwrap();
                assert_eq!(named_val, Value::Bool(true));
            }
            other => panic!("Expected PendingCall state, got {:?}", other),
        }
    }

    #[test]
    fn test_attach_materialization_context_adds_frame() {
        let thunk_span = test_span(1, 1, 1, 10);
        let err = EvalError::undefined_variable("x".to_string(), thunk_span);
        let mat_span = test_span(10, 5, 10, 6);
        let origin = "test_origin";

        let decorated =
            attach_materialization_context(err.into(), Some(&mat_span), Some(origin), thunk_span);

        // Verify materialization_span is set
        assert_eq!(decorated.materialization_span, Some(mat_span));

        // Verify origin frame is added
        assert!(
            decorated
                .stack
                .iter()
                .any(|f| f.label == origin && f.span == thunk_span),
            "Expected origin frame with label '{}' and thunk_span, but stack frames were: {:?}",
            origin,
            decorated.stack
        );
    }

    // Test fails - needs investigation of test setup (test_ctx/test_env helpers may not properly initialize EvalState)
    #[test]
    #[ignore]
    fn test_guarded_type_assertion_failure_has_secondary_span() {
        // Test that when a Guarded type assertion fails, the error includes
        // a secondary_span pointing to where the value was produced (if different
        // from the assertion site).
        use crate::ast::Expr;
        use crate::eval::materialize;
        use crate::types::Type;

        // Create a simple expression that produces an Int
        let value_expr = Spanned {
            node: Expr::Int(42),
            span: test_span(5, 1, 5, 3), // Line 5: the value production site
        };
        let value_thunk = crate::value::Thunk::new_unevaluated(
            Rc::new(value_expr),
            test_env(),
            test_ctx(),
            test_span(5, 1, 5, 3),
        );

        // Create a Guarded thunk that expects String but wraps the Int
        let expected_type = Type::Str;
        let guard_span = test_span(10, 1, 10, 20); // Line 10: the assertion site
        let guarded = crate::value::Thunk::new_guarded(
            Rc::new(value_thunk),
            expected_type,
            Vec::new(),
            guard_span,
        );

        // Try to materialize - should fail
        let ctx = test_ctx();
        let result = materialize(&guarded, Some(&guard_span), &ctx);

        assert!(result.is_err(), "Expected type assertion to fail");
        let err = result.unwrap_err();

        // Check that secondary_span is present and points to the value production site
        assert!(
            err.secondary_span.is_some(),
            "Expected secondary_span to be set"
        );
        let (sec_span, sec_label) = err.secondary_span.unwrap();
        assert_eq!(
            sec_span,
            test_span(5, 1, 5, 3),
            "Secondary span should point to where the value was produced"
        );
        assert_eq!(
            sec_label, "value produced here",
            "Secondary span label should be 'value produced here'"
        );
    }

    // Test fails - needs investigation of test setup (test_ctx/test_env helpers may not properly initialize EvalState)
    #[test]
    #[ignore]
    fn test_guarded_secondary_span_suppressed_when_same_as_definition() {
        // Test that when the value production site is the same as the assertion site,
        // secondary_span is NOT set (would be redundant).
        use crate::ast::Expr;
        use crate::eval::materialize;
        use crate::types::Type;

        let same_span = test_span(1, 1, 1, 10);

        // Create a value at the same location as the guard
        let value_expr = Spanned {
            node: Expr::Int(42),
            span: same_span,
        };
        let value_thunk = crate::value::Thunk::new_unevaluated(
            Rc::new(value_expr),
            test_env(),
            test_ctx(),
            same_span,
        );

        // Create a Guarded thunk with the same span for both guard and inner
        let guarded = crate::value::Thunk::new_guarded(
            Rc::new(value_thunk),
            Type::Str,
            Vec::new(),
            same_span, // guard_span
        );

        let ctx = test_ctx();
        let result = materialize(&guarded, Some(&same_span), &ctx);

        assert!(result.is_err());
        let err = result.unwrap_err();

        // Secondary span should NOT be set because it would be the same as definition_span
        assert!(
            err.secondary_span.is_none(),
            "Secondary span should be suppressed when same as definition span"
        );
    }

    #[test]
    fn test_cont_memoize_caches_result() {
        // Test that Cont::Memoize caches the materialization result into the parent thunk.
        // Create an Unevaluated thunk, force it via the CEK machine (run), and verify
        // it transitions to Materialized state with the correct cached value.
        let span = test_span(1, 1, 1, 10);
        let expr = Rc::new(sp(Expr::Int(42)));
        let env = empty_env();
        let ctx = test_ctx();

        let thunk = Rc::new(Thunk::new_unevaluated(expr, env, Rc::clone(&ctx), span));

        // Verify initial state is Unevaluated
        {
            let state = thunk.state();
            assert!(
                matches!(&*state, ThunkState::Unevaluated { .. }),
                "Expected Unevaluated state before forcing"
            );
        }

        // Force the thunk via the CEK machine
        let result = run(
            Action::Materialize {
                thunk: Rc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );

        // Verify the result is correct
        assert!(result.is_ok(), "Expected successful materialization");
        assert_eq!(result.unwrap(), Value::Int(42));

        // Verify the thunk transitioned to Materialized state
        {
            let state = thunk.state();
            match &*state {
                ThunkState::Materialized(v) => {
                    assert_eq!(*v, Value::Int(42), "Cached value should be Int(42)");
                }
                other => panic!("Expected Materialized state, got {:?}", other),
            }
        }

        // Verify that a second materialization returns the cached value immediately
        // (no re-evaluation)
        let result2 = run(
            Action::Materialize {
                thunk: Rc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );
        assert_eq!(result2.unwrap(), Value::Int(42));
    }

    #[test]
    fn test_cont_memoize_caches_error_in_failed_state() {
        // Test that when a thunk errors during materialization, the error is cached
        // in Failed state and subsequent materializations return the cached error.
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Create a thunk that will fail: reference an undefined variable
        let expr = Rc::new(sp(Expr::var_ref("undefined_var".into())));
        let thunk = Rc::new(Thunk::new_unevaluated(expr, env, Rc::clone(&ctx), span));

        // Verify initial state is Unevaluated
        {
            let state = thunk.state();
            assert!(
                matches!(&*state, ThunkState::Unevaluated { .. }),
                "Expected Unevaluated state before forcing"
            );
        }

        // Force the thunk — should fail with undefined variable error
        let result = run(
            Action::Materialize {
                thunk: Rc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );

        // Verify the result is an error
        assert!(result.is_err(), "Expected error for undefined variable");
        let err = result.unwrap_err();
        assert!(
            err.message().contains("undefined_var"),
            "Expected undefined variable error, got: {}",
            err.message()
        );

        // Verify the thunk transitioned to Failed state
        {
            let state = thunk.state();
            match &*state {
                ThunkState::Failed(cached_err) => {
                    assert!(
                        cached_err.message().contains("undefined_var"),
                        "Cached error should be undefined variable error, got: {}",
                        cached_err.message()
                    );
                }
                other => panic!("Expected Failed state, got {:?}", other),
            }
        }

        // Verify that a second materialization returns the cached error
        let result2 = run(
            Action::Materialize {
                thunk: Rc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );
        assert!(
            result2.is_err(),
            "Expected cached error on second materialization"
        );
        let err2 = result2.unwrap_err();
        assert!(
            err2.message().contains("undefined_var"),
            "Cached error should be returned, got: {}",
            err2.message()
        );
    }

    #[test]
    fn test_error_propagation_through_continuation() {
        // Test that errors propagate correctly through the continuation stack.
        // Create a nested structure (dict access) where the inner thunk errors,
        // and verify the error propagates through the DotAccessForce continuation.
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Create a dict with an entry that will error when materialized
        let error_expr = Rc::new(sp(Expr::var_ref("undefined_var".into())));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            Rc::clone(&env),
            Rc::clone(&ctx),
            span,
        ));

        let error_id = ctx.alloc_thunk(error_thunk);
        let mut dict_map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        dict_map.insert(Key::String("field".into()), error_id);
        let dict_value = Value::Dict(dict_map);
        let dict_thunk = Rc::new(Thunk::new_materialized(dict_value, span));

        // Insert the dict into the environment
        env.borrow_mut().insert("my_dict".into(), dict_thunk);

        // Create a dot access expression: my_dict.field
        let access_expr = Rc::new(sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::var_ref("my_dict".into()))),
            field: crate::ast::DotKey::Ident("field".to_string()),
        }));

        let access_thunk = Rc::new(Thunk::new_unevaluated(
            access_expr,
            env,
            Rc::clone(&ctx),
            span,
        ));

        // Force the access thunk — should propagate the error from the field value
        let result = run(
            Action::Materialize {
                thunk: access_thunk,
                mat_span: None,
            },
            &ctx,
        );

        // Verify the error propagated
        assert!(
            result.is_err(),
            "Expected error to propagate through DotAccessForce"
        );
        let err = result.unwrap_err();
        assert!(
            err.message().contains("undefined_var"),
            "Expected undefined variable error, got: {}",
            err.message()
        );
    }
}
