//! Iterative materialization machinery: CEK continuation stack and force loop.
//!
//! Includes inline TypeAssert handling in force_step for correct lazy type validation.
//!
//! This module contains the core iterative evaluator (run/force_step/apply_cont)
//! that materializes thunks without recursion. The CEK machine design is documented
//! in doc/08-evaluation.md §Iterative Evaluator.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{Annotation, CoreExpr, Span, Spanned, SurfaceExpression};
use crate::builtins::{flatten_overlay, MAX_COLLECT_SIZE};
use crate::error::{EvalError, EvalResult};
use crate::eval::{
    annotation_has_structural_fields, as_record_row_merged, eval_core_expr_pub, format_field_path,
    format_type_for_assert, materialize, validate_and_wrap_record, value_matches_type, EvalContext,
    DEFAULT_ANNOTATION_KEY,
};
use crate::eval_access::invoke_proxy_handler;
use crate::eval_call::{invoke_function, CallContext};
use crate::types::Type;
use crate::value::{string_val, Environment, Key, Thunk, Value};

/// Type alias for the optional default expression + environment pair carried by guarded thunks.
/// Reduces type_complexity in RestoreState and GuardedValidateData.
type GuardDefault = (
    Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
    Arc<RwLock<Environment>>,
);

/// Maximum continuation stack depth. Prevents resource exhaustion from deeply
/// nested evaluation chains that would otherwise exhaust heap memory.
///
/// This limit is separate from MAX_EVAL_DEPTH (256) and is set higher because:
/// - Each continuation is ~96 bytes, so 2048 frames = ~192 KB stack allocation
/// - Deep materialization chains (e.g., nested function calls, deeply nested
///   record validation) can legitimately exceed parse depth
/// - The CEK machine is iterative, so this limit protects heap, not Rust stack
const MAX_CONTINUATION_STACK: usize = 2048;

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
pub(crate) enum RestoreState {
    #[allow(dead_code)]
    // Used in tests; runtime paths now skip RestoreState for PendingBuiltin (Task 4 optimization)
    PendingBuiltin {
        def: crate::value::BuiltinDef,
        args: Vec<Arc<Thunk>>,
        named: Option<IndexMap<String, Arc<Thunk>>>,
        call_span: Span,
        ctx: Arc<EvalContext>,
    },
    PendingCall {
        func: Arc<Thunk>,
        args: Vec<Arc<Thunk>>,
        named: Option<Box<IndexMap<String, Arc<Thunk>>>>,
        call_span: Span,
        caller_env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
    Guarded {
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<GuardDefault>,
    },
    /// Restore a Surface thunk for non-cacheable errors (e.g., DepthExceeded).
    /// Holds the raw SurfaceNode so the thunk can be re-lowered on retry.
    /// TODO(parts-e): replace with RestoreState::CoreExpr (store already-lowered CoreExpr)
    /// to avoid re-lowering on each DepthExceeded retry.
    Surface {
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        res: std::sync::Arc<crate::ast::ResolutionTable>,
        types: std::sync::Arc<crate::ast::TypeAnnotationTable>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
    /// Restore an AstNodeField thunk for non-cacheable errors.
    /// Not yet constructed: AstNodeField evaluation is synchronous and infallible,
    /// so DepthExceeded cannot occur on that path. Retained for structural completeness
    /// and in case AstNodeField evaluation gains async/recursive work in the future.
    #[allow(dead_code)]
    AstNodeField {
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        field: &'static str,
        ctx: Arc<EvalContext>,
    },
    /// Restore a CoreExpr thunk for non-cacheable errors (e.g., DepthExceeded).
    /// Stores the Arc<Spanned<CoreExpr>> directly — no re-lowering on retry.
    CoreExpr {
        expr: Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
}

impl RestoreState {
    pub(crate) fn restore(self, thunk: &Thunk) {
        use crate::value::UnevaluatedState;

        let unevaled = match self {
            RestoreState::PendingBuiltin {
                def,
                args,
                named,
                call_span,
                ctx,
            } => UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                ctx,
            },
            RestoreState::PendingCall {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            } => UnevaluatedState::Call {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            },
            RestoreState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            } => UnevaluatedState::Guarded {
                inner,
                expected,
                field_path,
                guard_span,
                blame_label,
                default,
            },
            RestoreState::Surface {
                node,
                res,
                types,
                env,
                ctx,
            } => UnevaluatedState::Surface {
                node,
                res,
                types,
                env,
                ctx,
            },
            RestoreState::AstNodeField { node, field, ctx } => {
                UnevaluatedState::AstNodeField { node, field, ctx }
            }
            RestoreState::CoreExpr { expr, env, ctx } => {
                UnevaluatedState::CoreExpr { expr, env, ctx }
            }
        };

        thunk.restore_unevaluated(unevaled);
    }
}

/// Payload for Cont::Memoize. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MemoizeData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    // None for paths where restoration is not possible (e.g., default fallback).
    // Some when the original thunk state can be restored on error.
    pub(crate) restore: Option<RestoreState>,
    pub(crate) ctx: Arc<EvalContext>,
}

/// Payload for Cont::PendingCallDispatch. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct PendingCallDispatchData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) func_thunk: Arc<Thunk>,
    pub(crate) args: Vec<Arc<Thunk>>,
    pub(crate) named: Option<Box<IndexMap<String, Arc<Thunk>>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
}

/// Payload for Cont::GuardedValidate. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct GuardedValidateData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) expected: Type,
    pub(crate) field_path: Vec<String>,
    pub(crate) guard_span: Span,
    pub(crate) inner_span: Span,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    /// EvalContext for flattening Value::Overlay results and allocating guard-wrapped field thunks.
    /// Always populated from force_step's ctx parameter (all thunks share one EvalContext).
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) blame_label: Option<crate::error::BlameLabel>,
    /// Default expression and environment from TypeAssert `default:` annotation.
    pub(crate) default: Option<GuardDefault>,
    /// Restoration state for non-cacheable errors (e.g., DepthExceeded).
    /// Wrapped in Option to enable .take() when passing to default-fallback Memoize continuations.
    pub(crate) restore: Option<RestoreState>,
}

/// Payload for Cont::TypeAssertCheck. Boxed to keep the Cont enum ≤96 bytes.
///
/// TODO(parts-e): `annotation` is Box<Spanned<Annotation>> where Annotation::PropertyDict
/// entries store SurfaceNode values. The default-fallback paths in apply_cont extract
/// the "default:" property as &Arc<SurfaceNode>, convert via surface_node_to_core_expr, and
/// dispatch as Action::EvalCore. When Annotation stores CoreExpr values natively, the
/// conversion becomes a zero-cost Arc<Spanned<CoreExpr>> clone.
pub(crate) struct TypeAssertCheckData {
    pub(crate) annotation: Box<Spanned<Annotation>>,
    pub(crate) resolved: Box<Option<Type>>,
    pub(crate) expr_span: Span,
    pub(crate) thunk_span: Span,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
}

/// Payload for Cont::BuiltinForceArg. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) def: crate::value::BuiltinDef,
    pub(crate) args: Vec<Arc<Thunk>>,
    pub(crate) named: Option<IndexMap<String, Arc<Thunk>>>,
    pub(crate) call_span: Span,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
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
    pub(crate) ctx: Arc<EvalContext>,
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
    /// result thunks from PendingBuiltin/PendingCall/CoreExpr/Surface branches.
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
    ///
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
    /// Pushed by force_step's Expr::TypeAssert inline handler after evaluating the inner
    /// expression thunk; replaces the synchronous materialize() call that was the laziness
    /// violation in the TypeAssert branch.
    ///
    /// TODO(parts-e): TypeAssertCheckData::annotation is Box<Spanned<Annotation>>, and
    /// Annotation::PropertyDict entries are SurfaceEntry whose values are Arc<SurfaceNode>.
    /// The default-fallback paths in apply_cont call annotation.node.get_property("default:")
    /// → &Arc<SurfaceNode> → surface_node_to_core_expr → Action::EvalCore. When annotations
    /// store CoreExpr values natively, the conversion becomes a no-op clone.
    TypeAssertCheck(Box<TypeAssertCheckData>),
}

// Compile-time assertion: Cont must be ≤96 bytes to fit in one cache line.
const _: () = assert!(std::mem::size_of::<Cont>() <= 96);

/// RAII guard that pops one entry from the eval_stack when dropped.
///
/// Created immediately after an `eval_stack.push()` in `force_step` or `apply_cont`.
/// Ensures the push is always paired with a pop, even on early error exits, without
/// manual pop calls at every error site.
///
/// **Disarming:** Call `.disarm()` before returning `Action::Materialize` on paths
/// where a continuation (`Cont::Memoize`, `Cont::BuiltinForceArg`, or
/// `Cont::PendingCallDispatch`) takes ownership of the pop. Without disarming, the
/// guard would double-pop the stack.
///
/// **Inherited guards:** Use `EvalStackGuard::inherited()` in `apply_cont` handlers
/// that receive pop responsibility from a prior push (e.g., `Cont::PendingCallDispatch`
/// inherits from `force_step`'s PendingCall push). The inherited guard does not push
/// but will pop on drop unless disarmed.
struct EvalStackGuard {
    state: Arc<Mutex<crate::eval::EvalState>>,
    armed: bool,
}

impl EvalStackGuard {
    /// Push an entry onto the eval_stack and create a guard that will pop on drop.
    fn push(state: &Arc<Mutex<crate::eval::EvalState>>, entry: (String, Span)) -> Self {
        state.lock().unwrap().eval_stack.push(entry);
        EvalStackGuard {
            state: Arc::clone(state),
            armed: true,
        }
    }

    /// Create a guard for an inherited eval_stack entry (no push, but will pop on drop).
    ///
    /// Used in `apply_cont` handlers where the eval_stack entry was pushed by a prior
    /// `force_step` call (e.g., `PendingCallDispatch` inherits from PendingCall's push,
    /// `BuiltinForceArg` inherits from PendingBuiltin's push, `Memoize` inherits from
    /// any pusher).
    fn inherited(state: &Arc<Mutex<crate::eval::EvalState>>) -> Self {
        EvalStackGuard {
            state: Arc::clone(state),
            armed: true,
        }
    }

    /// Disarm: prevent the guard from popping on drop. Call this when transferring
    /// pop ownership to a continuation (Memoize, BuiltinForceArg, PendingCallDispatch).
    fn disarm(mut self) {
        self.armed = false;
        // self is dropped here, but armed=false prevents the pop in Drop.
    }
}

impl Drop for EvalStackGuard {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock().unwrap().eval_stack.pop();
        }
    }
}

/// Check continuation stack depth before pushing. Returns `Err(DepthExceeded)` if
/// the stack has reached MAX_CONTINUATION_STACK, otherwise returns `Ok(())`.
///
/// This guard prevents resource exhaustion from deeply nested evaluation chains.
/// The error is non-cacheable (restores thunk state) to allow retry at lower depth.
#[inline]
fn check_stack_depth(stack: &[Cont], span: Span) -> EvalResult<()> {
    if stack.len() >= MAX_CONTINUATION_STACK {
        Err(EvalError::depth_exceeded(MAX_CONTINUATION_STACK, span).into())
    } else {
        Ok(())
    }
}

/// Action to perform next in the iterative evaluation loop.
pub(crate) enum Action {
    /// Result ready — pop top continuation and apply, or return if stack empty
    Continue(EvalResult<Value>),
    /// Force this thunk to a materialized value
    Materialize {
        thunk: Arc<Thunk>,
        mat_span: Option<Span>,
    },
    /// Evaluate a CoreExpr to a thunk (wrapping, not forcing).
    ///
    /// Used by TypeAssert and Guarded default expression evaluation. Calls
    /// `eval_core_expr_pub` and wraps the result as `Action::Continue` (if already
    /// materialized) or `Action::Materialize` (if unevaluated). This variant replaces
    /// the old `Action::Eval { expr: Rc<Spanned<Expr>>, ... }` which required routing
    /// through `eval_step` and the Expr-based dispatch table.
    ///
    /// Default expressions from `Annotation::get_property("default:")` are converted
    /// from `Arc<SurfaceNode>` to `Spanned<CoreExpr>` at emit time via `surface_node_to_core_expr`.
    /// Emit sites: GuardedValidate default-fallback (apply_cont) and TypeAssertCheck
    /// default-fallback (apply_cont).
    EvalCore {
        expr: Arc<Spanned<CoreExpr>>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
}

/// Process one thunk and return either a result or a sub-thunk to force.
/// This mirrors the logic of `materialize()` but pushes continuations instead of recursing.
pub(crate) async fn force_step(
    thunk: &Arc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> Action {
    let thunk_span = thunk.span;

    // Check continuation stack depth before processing. This prevents resource exhaustion
    // from deeply nested evaluation chains. Checked here (before any continuations are
    // pushed) rather than at every push site for simplicity and performance.
    if let Err(depth_err) = check_stack_depth(stack, thunk_span) {
        return Action::Continue(Err(depth_err));
    }

    // Early returns for already-resolved states
    // Check Materialized state first (hot path)
    if let Some(v) = thunk.try_get_materialized() {
        return Action::Continue(Ok(v));
    }

    // Check Failed state — no ThunkStateGuard; reads directly from result cell.
    if let Some(mut cloned) = thunk.get_cached_error() {
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
            thunk.cache_failure_once(&cloned);
        }
        return Action::Continue(Err(cloned));
    }

    // Check InProgress state (cycle detection) — no ThunkStateGuard.
    // NOTE: Placeholder thunks (new_placeholder) are also represented as
    // (unevaluated=None, result=empty) and thus indistinguishable from InProgress
    // at the ThunkInner storage level.  Treating them as InProgress produces a
    // "circular dependency" error, which is acceptable — a forced Placeholder
    // means a letrec construction bug, and the error message identifies the span.
    if thunk.is_in_progress() {
        // Defer origin clone to error path only (hot path already returned at Materialized)
        let origin = thunk.origin.clone();
        let label = origin.as_deref().unwrap_or("thunk");

        // Capture the eval_stack for cycle path reconstruction
        let cycle_path = ctx.state.lock().unwrap().eval_stack.clone();

        let mut err = EvalError::circular_dependency(label, thunk.span, cycle_path);
        if let Some(span) = mat_span {
            err = err.with_materialization_span(span);
        }
        let err_boxed: Box<EvalError> = err.into();
        thunk.cache_failure_once(&err_boxed);
        return Action::Continue(Err(err_boxed));
    }

    // INVARIANTS verified post-iterative-eval-b4 (2026-04-30):
    //
    // 1. SHARING PRESERVATION: Arc<Thunk> identity is preserved through Cont dispatch.
    //    The Cont::Memoize handler (apply_cont, line 724) caches the materialization
    //    result back into the ORIGINAL thunk via thunk.set_materialized(), not a copy.
    //    This ensures `Arc::ptr_eq` holds across all references to the same thunk.
    //
    // 2. MONOTONICITY: State transitions are one-way (Unevaluated/PendingBuiltin/
    //    PendingCall/Guarded → InProgress → Materialized/Failed). Exception: DepthExceeded
    //    errors are non-cacheable and trigger state restoration (e.g., InProgress →
    //    PendingBuiltin) so the computation can be retried.
    //    Failed → Failed self-transition (lines 353-371) refines diagnostic metadata
    //    (materialization spans, stack frames) without changing the error's identity.
    //
    // 3. CYCLE DETECTION: InProgress blackholing works across all states. Each take_*
    //    method (take_pending_builtin, take_pending_call, take_guarded, take_core_expr, etc.
    //    in value.rs) atomically transitions to InProgress via mem::replace BEFORE
    //    extracting data. Re-encountering InProgress during materialization (line 373)
    //    immediately produces CircularDependency error, cached in Failed state (line 387).
    //
    // Process deferred states (hot path has already returned above)
    // Defer origin clone to here — it's only needed for error reporting and Memoize continuations.
    let origin = thunk.origin.clone();

    if let Some((def, args, named, call_span, thunk_ctx)) = thunk.take_pending_builtin() {
        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
        // EvalStackGuard ensures pop on all exit paths; disarmed when delegating to a
        // continuation (BuiltinForceArg, Memoize) that inherits pop responsibility.
        let eval_stack_guard = EvalStackGuard::push(
            &thunk_ctx.state,
            (origin.as_deref().unwrap_or("thunk").to_string(), thunk_span),
        );

        // Wrap args/named in Option so each exclusive match arm can move them
        // without cloning. Taking ownership avoids the pre-clone of Vec/IndexMap
        // that was previously done on every successful builtin call to build RestoreState.
        // Each arm calls .take().expect("...") exactly once to extract the owned value.
        let mut args = Some(args);
        let mut named = Some(named);

        // force_count pre-materialization: unconditionally materialize args[0..force_count].
        // This is checked BEFORE pos_strictness W1 scanning to ensure forced args are
        // always materialized regardless of their strictness annotation.
        if def.force_count > 0 {
            if let Some(arg_idx) = (0..def
                .force_count
                .min(args.as_ref().expect("args set above").len()))
                .find(|&i| {
                    args.as_ref().expect("args set above")[i]
                        .try_get_materialized()
                        .is_none()
                })
            {
                let arg_thunk = Arc::clone(&args.as_ref().expect("args set above")[arg_idx]);
                stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                    thunk: Arc::clone(thunk),
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
                // BuiltinForceArg continuation inherits eval_stack pop responsibility
                eval_stack_guard.disarm();
                return Action::Materialize {
                    thunk: arg_thunk,
                    mat_span: None,
                };
            }
        }

        // W1 dispatch-time materialization: scan pos_strictness for first Seq/Spine position.
        // Pre-materialize strict args iteratively to prevent Rust stack growth and enable
        // the builtin to skip redundant materialize() calls (thunk memoization fast-path).
        // Skip positions [0..force_count) that were already processed above.
        use crate::value::Strictness;
        if let Some((arg_idx, _)) = def
            .pos_strictness
            .iter()
            .enumerate()
            .skip(def.force_count)
            .find(|(i, &s)| {
                *i < args.as_ref().expect("args set above").len()
                    && (s == Strictness::Seq || s == Strictness::Spine)
                    && args.as_ref().expect("args set above")[*i]
                        .try_get_materialized()
                        .is_none()
            })
        {
            let arg_thunk = Arc::clone(&args.as_ref().expect("args set above")[arg_idx]);
            stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                thunk: Arc::clone(thunk),
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
            // BuiltinForceArg continuation inherits eval_stack pop responsibility
            eval_stack_guard.disarm();
            return Action::Materialize {
                thunk: arg_thunk,
                mat_span: None,
            };
        }

        // `named` is None for internally-created thunks (common case); only $apply
        // passes named args through. Take ownership to avoid cloning Vec/IndexMap.
        let builtin_args = crate::value::BuiltinArgs {
            args: args.take().expect("args set above"),
            named: named.take().expect("named set above"),
            call_span,
            ctx: Arc::clone(&thunk_ctx),
        };

        match (def.func)(builtin_args).await {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion
                if let Some(value) = result_thunk.try_get_materialized() {
                    // args/named are no longer needed; drop them implicitly.
                    // eval_stack_guard pops on drop (armed)
                    thunk.set_materialized(value.clone());
                    Action::Continue(Ok(value))
                } else {
                    // args/named were moved into BuiltinArgs (no clone), so we can't create RestoreState.
                    // If result materialization fails with DepthExceeded, the thunk will transition to
                    // Failed rather than being restored to PendingBuiltin. This trades retry-ability for
                    // performance (avoiding Vec/IndexMap clone on every builtin call).
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: None,
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    // Memoize continuation inherits eval_stack pop responsibility
                    eval_stack_guard.disarm();
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
                // eval_stack_guard pops on drop (armed)
                // args/named were moved into BuiltinArgs, so we always cache errors (even DepthExceeded).
                // This means non-cacheable errors from builtins will transition to Failed rather than
                // being restored to PendingBuiltin. This trades retry-ability for performance.
                thunk.cache_failure_once(&decorated);
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((func_thunk, args, named, call_span, caller_env, thunk_ctx)) =
        thunk.take_pending_call()
    {
        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
        // PendingCallDispatch continuation inherits eval_stack pop responsibility.
        let eval_stack_guard = EvalStackGuard::push(
            &thunk_ctx.state,
            (origin.as_deref().unwrap_or("thunk").to_string(), thunk_span),
        );

        stack.push(Cont::PendingCallDispatch(Box::new(
            PendingCallDispatchData {
                thunk: Arc::clone(thunk),
                func_thunk: Arc::clone(&func_thunk),
                args,
                named: named.map(Box::new),
                call_span,
                caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
            },
        )));
        eval_stack_guard.disarm();
        Action::Materialize {
            thunk: Arc::clone(&func_thunk),
            mat_span: Some(call_span),
        }
    } else if let Some((inner, expected, field_path, guard_span, blame_label, default_opt)) =
        thunk.take_guarded()
    {
        let inner_span = inner.span;
        // Always use the outer force_step ctx for GuardedValidate. All thunks in a single
        // evaluation share one EvalContext (same arena/state). The ctx is needed for:
        //   1. Flattening Value::Overlay results (flatten_overlay requires ctx)
        //   2. Allocating guard-wrapped field thunks (ctx.alloc_thunk in validate_and_wrap_record)
        // Previously this sniffed the inner thunk's state to extract ctx, returning None for
        // already-Materialized inner thunks. That caused E099 when a Record guard wrapped a
        // Materialized dict (e.g., the output of $append), because validate_and_wrap_record
        // could not allocate new field-guard thunks without a ctx.
        let guard_ctx: Arc<EvalContext> = Arc::clone(ctx);
        // Create RestoreState before pushing continuation (for non-cacheable error recovery)
        let restore = RestoreState::Guarded {
            inner: Arc::clone(&inner),
            expected: expected.clone(),
            field_path: field_path.clone(),
            guard_span,
            blame_label: blame_label.clone(),
            default: default_opt.clone(),
        };
        stack.push(Cont::GuardedValidate(Box::new(GuardedValidateData {
            thunk: Arc::clone(thunk),
            expected: expected.clone(),
            field_path,
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
            thunk: Arc::clone(&inner),
            mat_span,
        }
    } else if let Some((node, res, types, env, thunk_ctx)) = thunk.take_surface() {
        // Surface thunk handling in the CEK machine.
        //
        // The round-trip here is: SurfaceNode → lower() → Spanned<CoreExpr> → eval_core_expr()
        // → Arc<Thunk>. The lower() call is done here; the result is a CoreExpr thunk.
        // TODO(parts-e): pre-lower Surface thunks at creation time (store as CoreExpr thunk)
        // to avoid re-lowering on each DepthExceeded retry.
        //
        // After lower() we call eval_core_expr() to get a result thunk, then push a Memoize
        // continuation and return Action::Materialize to force the result thunk iteratively.
        // This keeps the Rust call stack flat (no recursive materialize() call).
        //
        // Contrast with the monolithic path in eval.rs::materialize() (line 2580) which calls
        // run() recursively (async stack frame). The CEK machine path below uses the shared
        // continuation stack instead — consistent with the iterative evaluation model.
        let restore = RestoreState::Surface {
            node: Arc::clone(&node),
            res: Arc::clone(&res),
            types: Arc::clone(&types),
            env: Arc::clone(&env),
            ctx: Arc::clone(&thunk_ctx),
        };

        let lowered = crate::lower::lower(&node, &res, &types);

        // Handle CoreExpr::DotAccess inline after lowering, for the same reason as the
        // take_core_expr branch above: avoids the looping extra Memoize continuation that
        // eval_core_expr(DotAccess) would add via new_unevaluated_core(DotAccess).
        if let crate::ast::CoreExpr::DotAccess {
            expr: target,
            field,
        } = &lowered.node
        {
            match crate::eval::eval_core_expr_pub(target, &env, &thunk_ctx).await {
                Ok(target_thunk) => {
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(restore),
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    stack.push(Cont::DotAccessForce(Box::new(DotAccessForceData {
                        field: field.clone(),
                        access_span: lowered.span,
                        target_def_span: target_thunk.span,
                        outer_mat_span: mat_span,
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    return Action::Materialize {
                        thunk: target_thunk,
                        mat_span: Some(lowered.span),
                    };
                }
                Err(mut e) => {
                    e.push_frame(format!("accessing .{field}"), lowered.span);
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            }
        }

        // Handle CoreExpr::TypeAssert inline after lowering — same loop risk as take_core_expr.
        if let crate::ast::CoreExpr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } = &lowered.node
        {
            let inner_thunk = match crate::eval::eval_core_expr_pub(inner, &env, &thunk_ctx).await {
                Ok(t) => t,
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            };
            let inner_span = inner_thunk.span;
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span,
                mat_span,
                restore: Some(restore),
                ctx: Arc::clone(&thunk_ctx),
            })));
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(Some(resolved_type.clone())),
                expr_span: lowered.span,
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(lowered.span),
            };
        }

        // Handle CoreExpr::RuntimeTypeCheck inline after lowering.
        if let crate::ast::CoreExpr::RuntimeTypeCheck {
            annotation,
            expr: inner,
            default,
        } = &lowered.node
        {
            let inner_thunk = match crate::eval::eval_core_expr_pub(inner, &env, &thunk_ctx).await {
                Ok(t) => t,
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        restore.restore(thunk);
                    }
                    return Action::Continue(Err(decorated));
                }
            };
            // Mirror the has_type logic from the CoreExpr::TypeAssert inline handler.
            let has_type = match &annotation.node {
                crate::ast::Annotation::Simple(_) => true,
                crate::ast::Annotation::PropertyDict(_) => {
                    annotation.node.get_property("type").is_some()
                        || annotation
                            .node
                            .get_property(crate::eval::DEFAULT_ANNOTATION_KEY)
                            .is_some()
                        || annotation_has_structural_fields(&annotation.node)
                }
                crate::ast::Annotation::Annotated(_, _) => true,
            };
            let inner_span = inner_thunk.span;
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span,
                mat_span,
                restore: Some(restore),
                ctx: Arc::clone(&thunk_ctx),
            })));
            if !has_type && default.is_none() {
                return Action::Materialize {
                    thunk: inner_thunk,
                    mat_span,
                };
            }
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(None),
                expr_span: lowered.span,
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(lowered.span),
            };
        }

        // TODO(parts-e): eval_core_expr itself may recurse for complex CoreExpr variants
        // (Sequential, Match, etc.) — those will need their own CEK continuation variants
        // to be fully iterative. For now, this at least removes the Surface → force_step
        // panic and integrates Surface handling into the CEK machine.
        match crate::eval::eval_core_expr_pub(&lowered, &env, &thunk_ctx).await {
            Ok(result_thunk) => {
                // Fast path: if eval_core_expr already produced a materialized thunk
                // (e.g., literals), skip the Memoize push entirely.
                if let Some(value) = result_thunk.try_get_materialized() {
                    thunk.set_materialized(value.clone());
                    Action::Continue(Ok(value))
                } else {
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(restore),
                        ctx: Arc::clone(&thunk_ctx),
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
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure_once(&decorated);
                } else {
                    restore.restore(thunk);
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((node, field, thunk_ctx)) = thunk.take_ast_node_field() {
        // AstNodeField thunk: evaluate a single named field from a SurfaceNode.
        // This is a fast synchronous computation (no async, no eval recursion).
        // surface_node_get_field returns a Value directly — no thunk to force.
        let value = crate::surface_fields::surface_node_get_field(&node, field, &thunk_ctx);
        thunk.set_materialized(value.clone());
        Action::Continue(Ok(value))
    } else if let Some((core_expr, env, thunk_ctx)) = thunk.take_core_expr() {
        // CoreExpr thunk — created by invoke_function from Value::Function.body.
        // Calls eval_core_expr_pub directly (no CoreExpr→Expr round-trip).
        //
        // Restore state on DepthExceeded so the thunk can be retried.
        let restore = crate::value::UnevaluatedState::CoreExpr {
            expr: Arc::clone(&core_expr),
            env: Arc::clone(&env),
            ctx: Arc::clone(&thunk_ctx),
        };

        // Handle CoreExpr::DotAccess inline. MUST NOT delegate to eval_core_expr_pub
        // here: eval_core_expr(CoreExpr::DotAccess) returns new_unevaluated_core(DotAccess),
        // which loops back into this branch and adds an extra Memoize continuation per
        // access level, causing DepthExceeded on deeply-nested DotAccess chains.
        if let crate::ast::CoreExpr::DotAccess {
            expr: target,
            field,
        } = &core_expr.node
        {
            match eval_core_expr_pub(target, &env, &thunk_ctx).await {
                Ok(target_thunk) => {
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(RestoreState::CoreExpr {
                            expr: Arc::clone(&core_expr),
                            env: Arc::clone(&env),
                            ctx: Arc::clone(&thunk_ctx),
                        }),
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    stack.push(Cont::DotAccessForce(Box::new(DotAccessForceData {
                        field: field.clone(),
                        access_span: core_expr.span,
                        target_def_span: target_thunk.span,
                        outer_mat_span: mat_span,
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    return Action::Materialize {
                        thunk: target_thunk,
                        mat_span: Some(core_expr.span),
                    };
                }
                Err(mut e) => {
                    e.push_frame(format!("accessing .{field}"), core_expr.span);
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        thunk.restore_unevaluated(restore);
                    }
                    return Action::Continue(Err(decorated));
                }
            }
        }

        // Handle CoreExpr::TypeAssert inline. eval_core_expr(CoreExpr::TypeAssert) wraps
        // in new_unevaluated_core(CoreExpr::TypeAssert), which would loop back into this branch.
        if let crate::ast::CoreExpr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
        } = &core_expr.node
        {
            let inner_thunk = match eval_core_expr_pub(inner, &env, &thunk_ctx).await {
                Ok(t) => t,
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        thunk.restore_unevaluated(restore);
                    }
                    return Action::Continue(Err(decorated));
                }
            };
            let inner_span = inner_thunk.span;
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span,
                mat_span,
                restore: Some(RestoreState::CoreExpr {
                    expr: Arc::clone(&core_expr),
                    env: Arc::clone(&env),
                    ctx: Arc::clone(&thunk_ctx),
                }),
                ctx: Arc::clone(&thunk_ctx),
            })));
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(Some(resolved_type.clone())),
                expr_span: core_expr.span,
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(core_expr.span),
            };
        }

        // Handle CoreExpr::RuntimeTypeCheck inline — same loop risk as TypeAssert.
        // RuntimeTypeCheck is for Expr::TypeAssert nodes with no resolved_type (not typechecked
        // or macro-synthesized). Uses the same has_type logic as the Expr::TypeAssert handler.
        if let crate::ast::CoreExpr::RuntimeTypeCheck {
            annotation,
            expr: inner,
            default,
        } = &core_expr.node
        {
            // Evaluate the inner expression as a CoreExpr thunk.
            let inner_thunk = match eval_core_expr_pub(inner, &env, &thunk_ctx).await {
                Ok(t) => t,
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span,
                    );
                    if decorated.kind.is_cacheable() {
                        thunk.cache_failure_once(&decorated);
                    } else {
                        thunk.restore_unevaluated(restore);
                    }
                    return Action::Continue(Err(decorated));
                }
            };
            // Mirror the has_type logic from the CoreExpr::TypeAssert inline handler.
            let has_type = match &annotation.node {
                crate::ast::Annotation::Simple(_) => true,
                crate::ast::Annotation::PropertyDict(_) => {
                    annotation.node.get_property("type").is_some()
                        || annotation
                            .node
                            .get_property(crate::eval::DEFAULT_ANNOTATION_KEY)
                            .is_some()
                        || annotation_has_structural_fields(&annotation.node)
                }
                crate::ast::Annotation::Annotated(_, _) => true,
            };
            let inner_span = inner_thunk.span;
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span,
                mat_span,
                restore: Some(RestoreState::CoreExpr {
                    expr: Arc::clone(&core_expr),
                    env: Arc::clone(&env),
                    ctx: Arc::clone(&thunk_ctx),
                }),
                ctx: Arc::clone(&thunk_ctx),
            })));
            if !has_type && default.is_none() {
                // No type check and no default — pass through.
                return Action::Materialize {
                    thunk: inner_thunk,
                    mat_span,
                };
            }
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(None),
                expr_span: core_expr.span,
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(core_expr.span),
            };
        }

        match eval_core_expr_pub(&core_expr, &env, &thunk_ctx).await {
            Ok(result_thunk) => {
                // Fast path: literal or already-materialized result.
                if let Some(value) = result_thunk.try_get_materialized() {
                    thunk.set_materialized(value.clone());
                    return Action::Continue(Ok(value));
                }
                // Defer to Memoize continuation.
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Arc::clone(thunk),
                    origin,
                    thunk_span,
                    mat_span,
                    restore: Some(RestoreState::CoreExpr {
                        expr: core_expr,
                        env,
                        ctx: Arc::clone(&thunk_ctx),
                    }),
                    ctx: thunk_ctx,
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
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure_once(&decorated);
                } else {
                    thunk.restore_unevaluated(restore);
                }
                Action::Continue(Err(decorated))
            }
        }
    } else {
        unreachable!(
            "force_step: all ThunkState variants are handled. \
             Materialized/Failed/InProgress are early-returned at lines 474-519, \
             PendingBuiltin/PendingCall/Guarded/Surface/AstNodeField/CoreExpr are processed above. \
             If this fires, a new UnevaluatedState variant was added without updating force_step."
        )
    }
}

/// Apply a continuation to a materialization result.
pub(crate) async fn apply_cont(
    cont: Cont,
    result: EvalResult<Value>,
    stack: &mut Vec<Cont>,
) -> Action {
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
            // Inherited guard: Memoize always pops the eval_stack entry that was
            // pushed by the originating force_step (PendingBuiltin, PendingCall, or
            // GuardedValidate default fallback). The guard auto-pops on all exit paths.
            let _eval_stack_guard = EvalStackGuard::inherited(&ctx.state);
            let decorated_result = result.map_err(|e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            });

            match decorated_result {
                Ok(value) => {
                    // eval_stack_guard pops on drop (armed)
                    thunk.set_materialized(value.clone());
                    Action::Continue(Ok(value))
                }
                Err(e) => {
                    // eval_stack_guard pops on drop (armed)
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else if let Some(restore_state) = restore {
                        restore_state.restore(&thunk);
                    }
                    // restore is always Some when Memoize is pushed from the three
                    // GuardedValidate default-fallback sites: each calls restore.take()
                    // on a freshly-destructured GuardedValidateData whose restore field
                    // starts as Some (set in force_step). restore is also Some for the
                    // Unevaluated and PendingBuiltin Memoize paths. restore: None does
                    // not currently arise from any push site; the else-if above is a
                    // defensive guard for future Memoize push sites that may lack a
                    // restore state (e.g., top-level eval with no deferred thunk).
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
            // Inherited guard: PendingCallDispatch inherits the eval_stack entry
            // pushed by force_step(PendingCall). Auto-pops on all exit paths;
            // disarmed when delegating to Memoize or re-dispatching via PendingBuiltin.
            let eval_stack_guard = EvalStackGuard::inherited(&thunk_ctx.state);
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
                            invoke_function(&call_ctx).await
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
                                    ctx: Arc::clone(&thunk_ctx),
                                };
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore: Some(restore),
                                    ctx: thunk_ctx,
                                })));
                                // Memoize continuation inherits eval_stack pop responsibility
                                eval_stack_guard.disarm();
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
                                // eval_stack_guard pops on drop (armed)
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure_once(&e);
                                } else {
                                    // Move args/named into PendingCall — no clone needed.
                                    thunk.restore_unevaluated(
                                        crate::value::UnevaluatedState::Call {
                                            func: func_thunk,
                                            args: args.take().expect("args set above"),
                                            named: named.take().expect("named set above"),
                                            call_span,
                                            caller_env,
                                            ctx: thunk_ctx,
                                        },
                                    );
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
                        // Note: eval_stack_guard pops BEFORE converting to PendingBuiltin so that
                        // force_step(PendingBuiltin) can push a fresh entry — avoiding a
                        // duplicate that would cause an extra pop on completion.
                        use crate::value::Strictness;
                        let args_ref = args.as_ref().expect("args set above");
                        // Check if any force_count args need pre-materialization.
                        // force_count specifies how many leading positional args must be
                        // fully materialized (Seq) before the builtin is called.
                        let has_force_count_unevaluated = def.force_count > 0
                            && (0..def.force_count.min(args_ref.len()))
                                .any(|i| args_ref[i].try_get_materialized().is_none());
                        // Check if any W1 Seq/Spine positional args need pre-materialization.
                        let has_strict_unevaluated =
                            def.pos_strictness.iter().enumerate().any(|(i, &s)| {
                                i < args_ref.len()
                                    && (s == Strictness::Seq || s == Strictness::Spine)
                                    && args_ref[i].try_get_materialized().is_none()
                            });

                        if has_force_count_unevaluated || has_strict_unevaluated {
                            // eval_stack_guard pops on drop (armed) before PendingBuiltin re-dispatch.
                            // force_step(PendingBuiltin) will push a fresh entry for this thunk.
                            // Transition thunk from InProgress → PendingBuiltin.
                            // args is Box<Vec<...>> (matches ThunkState::PendingBuiltin.args).
                            // named is Option<Box<IndexMap<...>>>; unbox to Option<IndexMap<...>>.
                            thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
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
                                args: args.as_deref().expect("args set above").to_vec(),
                                named: named.as_ref().expect("named set above").as_deref().cloned(),
                                call_span,
                                ctx: Arc::clone(&thunk_ctx),
                            };
                            (def.func)(builtin_args).await
                        };
                        match builtin_result.map_err(&decorate) {
                            Ok(result_thunk) => {
                                if let Some(value) = result_thunk.try_get_materialized() {
                                    // Fast path: builtin result is already materialized.
                                    // args/named are no longer needed; drop them implicitly.
                                    // eval_stack_guard pops on drop (armed)
                                    thunk.set_materialized(value.clone());
                                    Action::Continue(Ok(value))
                                } else {
                                    // Move args/named into RestoreState — no clone needed.
                                    let restore = RestoreState::PendingCall {
                                        func: func_thunk,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span,
                                        caller_env,
                                        ctx: Arc::clone(&thunk_ctx),
                                    };
                                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                                        thunk: Arc::clone(&thunk),
                                        origin,
                                        thunk_span,
                                        mat_span,
                                        restore: Some(restore),
                                        ctx: thunk_ctx,
                                    })));
                                    // Memoize continuation inherits eval_stack pop responsibility
                                    eval_stack_guard.disarm();
                                    Action::Materialize {
                                        thunk: result_thunk,
                                        mat_span,
                                    }
                                }
                            }
                            Err(e) => {
                                // eval_stack_guard pops on drop (armed)
                                if e.kind.is_cacheable() {
                                    thunk.cache_failure_once(&e);
                                } else {
                                    // Move args/named into PendingCall — no clone needed.
                                    thunk.restore_unevaluated(
                                        crate::value::UnevaluatedState::Call {
                                            func: func_thunk,
                                            args: args.take().expect("args set above"),
                                            named: named.take().expect("named set above"),
                                            call_span,
                                            caller_env,
                                            ctx: thunk_ctx,
                                        },
                                    );
                                }
                                Action::Continue(Err(e))
                            }
                        }
                    }
                    // Unit variant used as a constructor: [Ok payload] where Ok = [variant "Ok"].
                    // When a unit Variant (payload: None) is called with exactly one positional
                    // arg and no named args, treat it as constructing Variant(tag, payload).
                    // This allows `Ok: [variant "Ok"]` in the prelude to be called as `[Ok 42]`.
                    Value::Variant { tag, payload: None }
                        if args.as_ref().is_some_and(|v| v.len() == 1)
                            && named
                                .as_ref()
                                .is_none_or(|m| m.as_ref().is_none_or(|b| b.is_empty())) =>
                    {
                        // Allocate the single positional arg as a ThunkId for the payload.
                        // The arg is already an Arc<Thunk> (unevaluated), so this is lazy.
                        let payload_thunk = args.as_ref().expect("args set above")[0].clone();
                        let payload_id = thunk_ctx.alloc_thunk(payload_thunk);
                        let result_val = Value::Variant {
                            tag,
                            payload: Some(payload_id),
                        };
                        // Fast path: the result is immediately materialized — no need to
                        // push a Memoize continuation. eval_stack_guard pops on drop (armed).
                        thunk.set_materialized(result_val.clone());
                        Action::Continue(Ok(result_val))
                    }
                    // ADT constructor called with a single named arg: [Circle r: 5] where Circle = [variant "Circle"].
                    // When a unit Variant is called with no positional args and exactly one named arg,
                    // use the named arg's value as the payload. This supports single-field ADT constructors
                    // declared via `[type Shape [Circle r: Int] ...]`.
                    Value::Variant { tag, payload: None }
                        if args.as_ref().is_some_and(|v| v.is_empty())
                            && named
                                .as_ref()
                                .is_some_and(|m| m.as_ref().is_some_and(|b| b.len() == 1)) =>
                    {
                        let named_map = named
                            .as_ref()
                            .expect("checked Some above")
                            .as_ref()
                            .expect("checked Some above");
                        let payload_thunk = named_map
                            .values()
                            .next()
                            .expect("1 entry checked above")
                            .clone();
                        let payload_id = thunk_ctx.alloc_thunk(payload_thunk);
                        let result_val = Value::Variant {
                            tag,
                            payload: Some(payload_id),
                        };
                        thunk.set_materialized(result_val.clone());
                        Action::Continue(Ok(result_val))
                    }
                    other => {
                        let err = EvalError::type_mismatch(
                            "Function or Builtin",
                            other.type_name(),
                            call_span,
                        );
                        let decorated = decorate(Box::new(err));
                        // eval_stack_guard pops on drop (armed)
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure_once(&decorated);
                        } else {
                            // Move args/named into PendingCall — no clone needed.
                            thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
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
                    // eval_stack_guard pops on drop (armed)
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Move args/named into PendingCall — no clone needed.
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Call {
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
                    // guard_ctx is Arc<EvalContext> (non-optional); destructured directly from the continuation.
                    let value = match value {
                        Value::Overlay(l, r) => {
                            match flatten_overlay(&l, &r, "type guard", &guard_ctx, guard_span) {
                                Ok(map) => Value::Dict(map),
                                Err(e) => {
                                    let e = decorate(e);
                                    if e.kind.is_cacheable() {
                                        thunk.cache_failure_once(&e);
                                    } else if let Some(r) = restore.take() {
                                        r.restore(&thunk);
                                    }
                                    return Action::Continue(Err(e));
                                }
                            }
                        }
                        other => other,
                    };
                    // For Record types and Intersection-of-Records, apply proxy contract wrapping.
                    // as_record_row_merged handles both forms by merging fields into a single Row.
                    if let Some(row) = as_record_row_merged(&expected) {
                        if let Value::Dict(ref entries) = value {
                            match validate_and_wrap_record(
                                entries,
                                row.as_ref(),
                                &mut field_path,
                                guard_span,
                                inner_span,
                                &guard_ctx,
                                default.clone(),
                                blame_label.clone(),
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk.set_materialized(guarded_value.clone());
                                    Action::Continue(Ok(guarded_value))
                                }
                                Err(err) => {
                                    // Guard validation failed - use default if present
                                    if let Some((default_expr, default_env)) = default {
                                        // Push to eval_stack to match the Memoize pop.
                                        // Guarded thunks don't push at force_step time (unlike
                                        // Unevaluated/PendingBuiltin/PendingCall) because
                                        // GuardedValidate normally exits via Action::Continue
                                        // without a Memoize pop. Only the default-fallback
                                        // paths push Cont::Memoize, so we push here to keep
                                        // eval_stack balanced. Memoize inherits pop responsibility.
                                        let guard_eval_stack = EvalStackGuard::push(
                                            &guard_ctx.state,
                                            (
                                                origin.as_deref().unwrap_or("thunk").to_string(),
                                                thunk_span,
                                            ),
                                        );
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin: Some(Arc::from("default fallback")),
                                            thunk_span,
                                            mat_span,
                                            restore: restore.take(),
                                            ctx: Arc::clone(&guard_ctx),
                                        })));
                                        // Memoize continuation inherits eval_stack pop responsibility
                                        guard_eval_stack.disarm();
                                        return Action::EvalCore {
                                            expr: Arc::clone(&default_expr),
                                            env: default_env,
                                            ctx: guard_ctx,
                                        };
                                    }
                                    let err = decorate(err);
                                    if err.kind.is_cacheable() {
                                        thunk.cache_failure_once(&err);
                                    } else if let Some(r) = restore.take() {
                                        r.restore(&thunk);
                                    }
                                    Action::Continue(Err(err))
                                }
                            }
                        } else {
                            // Expected Record but got non-Dict - use default if present
                            if let Some((default_expr, default_env)) = default {
                                // Push to eval_stack to match the Memoize pop (see
                                // comment at the first default-fallback site above).
                                let guard_eval_stack = EvalStackGuard::push(
                                    &guard_ctx.state,
                                    (origin.as_deref().unwrap_or("thunk").to_string(), thunk_span),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span,
                                    mat_span,
                                    restore: restore.take(),
                                    ctx: Arc::clone(&guard_ctx),
                                })));
                                // Memoize continuation inherits eval_stack pop responsibility
                                guard_eval_stack.disarm();
                                return Action::EvalCore {
                                    expr: Arc::clone(&default_expr),
                                    env: default_env,
                                    ctx: guard_ctx,
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
                                value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err.with_secondary_span(inner_span, "value produced here");
                            }
                            // Attach blame label if present (gradual typing boundary)
                            if let Some(ref label) = blame_label {
                                err = err.with_blame(label.clone());
                            }
                            let err = decorate(err.into());
                            if err.kind.is_cacheable() {
                                thunk.cache_failure_once(&err);
                            } else if let Some(r) = restore.take() {
                                r.restore(&thunk);
                            }
                            Action::Continue(Err(err))
                        }
                    } else {
                        // For non-Record types, simple value check
                        if value_matches_type(&value, &expected) {
                            thunk.set_materialized(value.clone());
                            Action::Continue(Ok(value))
                        } else {
                            // Type mismatch for non-Record types - use default if present
                            if let Some((default_expr, default_env)) = default {
                                // Push to eval_stack to match the Memoize pop (see
                                // comment at the first default-fallback site above).
                                let guard_eval_stack = EvalStackGuard::push(
                                    &guard_ctx.state,
                                    (origin.as_deref().unwrap_or("thunk").to_string(), thunk_span),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span,
                                    mat_span,
                                    restore: restore.take(),
                                    ctx: Arc::clone(&guard_ctx),
                                })));
                                // Memoize continuation inherits eval_stack pop responsibility
                                guard_eval_stack.disarm();
                                return Action::EvalCore {
                                    expr: Arc::clone(&default_expr),
                                    env: default_env,
                                    ctx: guard_ctx,
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
                                value.type_name(),
                                inner_span,
                            )
                            .with_materialization_span(guard_span);
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err.with_secondary_span(inner_span, "value produced here");
                            }
                            // Attach blame label if present (gradual typing boundary)
                            if let Some(ref label) = blame_label {
                                err = err.with_blame(label.clone());
                            }
                            let err = decorate(err.into());
                            if err.kind.is_cacheable() {
                                thunk.cache_failure_once(&err);
                            } else if let Some(r) = restore.take() {
                                r.restore(&thunk);
                            }
                            Action::Continue(Err(err))
                        }
                    }
                }
                Err(e) => {
                    // Inner materialization error propagates
                    let e = decorate(e);
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
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
            // Inherited guard: BuiltinForceArg inherits the eval_stack entry
            // pushed by force_step(PendingBuiltin). Auto-pops on all exit paths;
            // disarmed when delegating to another BuiltinForceArg or Memoize.
            let eval_stack_guard = EvalStackGuard::inherited(&thunk_ctx.state);
            let decorate = |e| {
                attach_materialization_context(e, mat_span.as_ref(), origin.as_deref(), thunk_span)
            };

            // Wrap args/named in Option so each exclusive match arm can move them
            // without cloning, following the same pattern as the initial PendingBuiltin branch.
            let mut args = Some(args);
            let mut named = Some(named);

            // force_count + W1 dispatch-time materialization: after arg at arg_idx has been materialized,
            // first check for the next un-materialized arg in [0..force_count], then scan pos_strictness
            // for the next Seq/Spine position. If found, force it; otherwise call builtin.
            match result {
                Ok(_) => {
                    // First check force_count range for next un-materialized arg
                    if def.force_count > 0 {
                        if let Some(next_idx) = (arg_idx + 1
                            ..def
                                .force_count
                                .min(args.as_ref().expect("args set above").len()))
                            .find(|&i| {
                                args.as_ref().expect("args set above")[i]
                                    .try_get_materialized()
                                    .is_none()
                            })
                        {
                            let next_arg =
                                Arc::clone(&args.as_ref().expect("args set above")[next_idx]);
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
                            // Next BuiltinForceArg inherits eval_stack pop responsibility
                            eval_stack_guard.disarm();
                            return Action::Materialize {
                                thunk: next_arg,
                                mat_span: None,
                            };
                        }
                    }

                    // Invariant: positions 0..=arg_idx have already been materialized — either
                    // by the force_count pass above (unconditional leading args) or by a prior
                    // BuiltinForceArg iteration (the W1 Seq/Spine scan). Skipping them is safe
                    // because try_get_materialized() would return Some(...) for all of them,
                    // so they would never be selected by the .find() predicate anyway. The skip
                    // avoids re-scanning already-processed positions.
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
                        let next_arg =
                            Arc::clone(&args.as_ref().expect("args set above")[next_idx]);
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
                        // Next BuiltinForceArg inherits eval_stack pop responsibility
                        eval_stack_guard.disarm();
                        return Action::Materialize {
                            thunk: next_arg,
                            mat_span: None,
                        };
                    }

                    // All forced and strict args materialized — call the builtin.
                    // Take ownership to avoid cloning Vec/IndexMap.
                    let builtin_args = crate::value::BuiltinArgs {
                        args: args.take().expect("args set above"),
                        named: named.take().expect("named set above"),
                        call_span,
                        ctx: Arc::clone(&thunk_ctx),
                    };
                    match (def.func)(builtin_args).await.map_err(&decorate) {
                        Ok(result_thunk) => {
                            if let Some(value) = result_thunk.try_get_materialized() {
                                // args/named are no longer needed; drop them implicitly.
                                // eval_stack_guard pops on drop (armed)
                                thunk.set_materialized(value.clone());
                                Action::Continue(Ok(value))
                            } else {
                                // args/named were moved into BuiltinArgs (no clone), so we can't create RestoreState.
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore: None,
                                    ctx: Arc::clone(&thunk_ctx),
                                })));
                                // Memoize continuation inherits eval_stack pop responsibility
                                eval_stack_guard.disarm();
                                Action::Materialize {
                                    thunk: result_thunk,
                                    mat_span,
                                }
                            }
                        }
                        Err(e) => {
                            // eval_stack_guard pops on drop (armed)
                            // args/named were moved into BuiltinArgs, so we always cache errors (even DepthExceeded).
                            thunk.cache_failure_once(&e);
                            Action::Continue(Err(e))
                        }
                    }
                }
                Err(e) => {
                    let e = decorate(e);
                    // eval_stack_guard pops on drop (armed)
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Move args/named into PendingBuiltin — no clone needed.
                        thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
                            def,
                            args: args.take().expect("args set above"),
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
                            )
                            .await
                            {
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
                        Value::Variant { tag: _, payload } => {
                            // Variant auto-unpacking: dot-access on a variant accesses the payload.
                            // If payload is None, report an error.
                            // If payload is Some(thunk_id), materialize the payload and retry the access.
                            match payload {
                                Some(payload_id) => {
                                    let payload_thunk = ctx.get_thunk(payload_id);
                                    // Materialize the payload, then re-push a DotAccessForce continuation
                                    // to access the field from the materialized payload value.
                                    stack.push(Cont::DotAccessForce(Box::new(
                                        DotAccessForceData {
                                            field,
                                            access_span,
                                            target_def_span,
                                            outer_mat_span,
                                            ctx: Arc::clone(&ctx),
                                        },
                                    )));
                                    Action::Materialize {
                                        thunk: payload_thunk,
                                        mat_span: Some(access_span),
                                    }
                                }
                                None => {
                                    // Unit variant has no payload — cannot access fields.
                                    let mut err = EvalError::internal(
                                        format!(
                                            "cannot access field .{} on unit variant (no payload)",
                                            field_str
                                        ),
                                        target_def_span,
                                    )
                                    .with_materialization_span(access_span);
                                    err.push_frame(format!("accessing .{field_str}"), access_span);
                                    Action::Continue(Err(err.into()))
                                }
                            }
                        }
                        // runtime-v2: dot-access on native AST value types
                        Value::Expression(node) => {
                            let field_value = crate::surface_fields::surface_node_get_field(
                                &node, &field_str, &ctx,
                            );
                            let thunk = Arc::new(Thunk::new_materialized(field_value, access_span));
                            Action::Materialize {
                                thunk,
                                mat_span: outer_mat_span.or(Some(access_span)),
                            }
                        }
                        Value::Program { program: prog, .. } => {
                            // Program.documents → integer-keyed list of Value::Document
                            let val = match field_str.as_str() {
                                "documents" => {
                                    let mut map = indexmap::IndexMap::new();
                                    for (i, doc_spanned) in prog.documents.iter().enumerate() {
                                        let doc_val = Value::Document(std::sync::Arc::new(
                                            doc_spanned.node.clone(),
                                        ));
                                        let tid = ctx.alloc_thunk(Arc::new(
                                            Thunk::new_materialized(doc_val, access_span),
                                        ));
                                        map.insert(crate::value::Key::Int(i as i64), tid);
                                    }
                                    Value::Dict(map)
                                }
                                _ => Value::Dict(indexmap::IndexMap::new()),
                            };
                            let thunk = Arc::new(Thunk::new_materialized(val, access_span));
                            Action::Materialize {
                                thunk,
                                mat_span: outer_mat_span.or(Some(access_span)),
                            }
                        }
                        Value::Document(doc) => {
                            // Document field access — expressions, declarations, name, etc.
                            let val = match field_str.as_str() {
                                "expressions" => {
                                    // Build a LLT Seq linked list (not integer-keyed Dict)
                                    // because builtin_eval expects [Seq Expression].
                                    let expr_nodes: Vec<std::sync::Arc<crate::ast::SurfaceNode>> =
                                        doc.items
                                            .iter()
                                            .filter_map(|item| {
                                                if let crate::ast::SurfaceItem::Expr(node) = item {
                                                    Some(std::sync::Arc::clone(node))
                                                } else {
                                                    None
                                                }
                                            })
                                            .collect();
                                    if expr_nodes.is_empty() {
                                        Value::Dict(indexmap::IndexMap::new())
                                    } else {
                                        // End-of-Seq sentinel
                                        let mut tail_id =
                                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                                Value::Dict(indexmap::IndexMap::new()),
                                                access_span,
                                            )));
                                        // Wrap elements from second-to-last down to index 1
                                        for node in
                                            expr_nodes.iter().rev().take(expr_nodes.len() - 1)
                                        {
                                            let head_id =
                                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                                    Value::Expression(std::sync::Arc::clone(node)),
                                                    access_span,
                                                )));
                                            tail_id =
                                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                                    Value::Seq {
                                                        head: head_id,
                                                        tail: tail_id,
                                                    },
                                                    access_span,
                                                )));
                                        }
                                        // First expression is the outermost head
                                        let head_id =
                                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                                Value::Expression(std::sync::Arc::clone(
                                                    &expr_nodes[0],
                                                )),
                                                access_span,
                                            )));
                                        Value::Seq {
                                            head: head_id,
                                            tail: tail_id,
                                        }
                                    }
                                }
                                "name" => match &doc.name {
                                    Some(n) => Value::Variant {
                                        tag: "Named".into(),
                                        payload: Some(ctx.alloc_thunk(Arc::new(
                                            Thunk::new_materialized(
                                                crate::value::string_val(n),
                                                access_span,
                                            ),
                                        ))),
                                    },
                                    None => Value::Variant {
                                        tag: "Unnamed".into(),
                                        payload: None,
                                    },
                                },
                                "stage" => {
                                    // stage: [Runtime] | [Type] — nominal variant
                                    // Default to Runtime when stage is None (matches ast_dict.rs behavior)
                                    let stage_tag = match &doc.stage {
                                        Some(crate::ast::Stage::Type) => "Type",
                                        Some(crate::ast::Stage::Runtime) | None => "Runtime",
                                    };
                                    Value::Variant {
                                        tag: stage_tag.to_string(),
                                        payload: None,
                                    }
                                }
                                _ => Value::Dict(indexmap::IndexMap::new()),
                            };
                            let thunk = Arc::new(Thunk::new_materialized(val, access_span));
                            Action::Materialize {
                                thunk,
                                mat_span: outer_mat_span.or(Some(access_span)),
                            }
                        }
                        other => {
                            // Type mismatch: report definition site and access site.
                            let mut err = EvalError::type_mismatch_ctx(
                                "dot access".to_string(),
                                "Dict, Proxy, or Variant",
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
                                    .map(|node| {
                                        (
                                            Arc::new(crate::ast_convert::surface_node_to_core_expr(
                                                node,
                                            )),
                                            Arc::clone(&env),
                                        )
                                    });
                                // Construct BlameLabel for TypeAssert boundary
                                let blame_label = Some(crate::error::BlameLabel {
                                    origin_span: thunk_span,  // where the value was produced
                                    boundary_span: expr_span, // where the TypeAssert annotation is
                                    polarity: crate::error::BlameParity::Positive,
                                });
                                match validate_and_wrap_record(
                                    entries,
                                    row.as_ref(),
                                    &mut vec![],
                                    expr_span,
                                    thunk_span,
                                    &ctx,
                                    default_opt.clone(),
                                    blame_label,
                                ) {
                                    Ok(new_entries) => {
                                        Action::Continue(Ok(Value::Dict(new_entries)))
                                    }
                                    Err(err) => {
                                        if let Some((default, env)) = default_opt {
                                            // Evaluate default expression iteratively.
                                            // The result will flow to the next continuation on the stack.
                                            Action::EvalCore {
                                                expr: default,
                                                env,
                                                ctx: Arc::clone(&ctx),
                                            }
                                        } else {
                                            Action::Continue(Err(err))
                                        }
                                    }
                                }
                            } else {
                                if let Some(default_node) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    Action::EvalCore {
                                        expr: Arc::new(crate::ast_convert::surface_node_to_core_expr(
                                            default_node,
                                        )),
                                        env,
                                        ctx: Arc::clone(&ctx),
                                    }
                                } else {
                                    let mut err = EvalError::type_assert_failed(
                                        &format_type_for_assert(&expected),
                                        value.type_name(),
                                        thunk_span,
                                    )
                                    .with_materialization_span(expr_span);
                                    if thunk_span != expr_span {
                                        err = err
                                            .with_secondary_span(thunk_span, "value produced here");
                                    }
                                    Action::Continue(Err(err.into()))
                                }
                            }
                        } else if value_matches_type(&value, &expected) {
                            // KNOWN ISSUE: `is:` predicate validation not implemented
                            //
                            // After type validation passes, TypeAssert should check if the annotation
                            // has an `is:` property and evaluate it as a predicate. For example:
                            //   [@[type: Int  is: positive?] $x]
                            // should call `positive?($x)` after verifying `$x` is an Int.
                            //
                            // Implementation requirements (mirroring match guard logic at eval.rs:1738-1764):
                            // 1. Check if annotation.node.get_property("is") exists
                            // 2. Evaluate the predicate expression in the current environment
                            // 3. If the result is a Function/Builtin, invoke it with the value as argument
                            // 4. Check if the result is truthy (Bool(true), non-empty Dict, or any non-Bool/non-Dict)
                            // 5. If falsy:
                            //    - If `default:` property exists, evaluate and return the default
                            //    - Otherwise, fail with EvalError::type_assert_failed("_ (is: predicate failed)", ...)
                            // 6. If truthy, return the value unchanged
                            //
                            // Challenges:
                            // - Requires eval_core_expr and materialize calls, which need to be integrated
                            //   into the iterative continuation loop (can't just call .await here)
                            // - Need to create a new continuation type (e.g., PredicateCheck) to defer
                            //   the predicate evaluation and result validation
                            // - Need to handle errors from predicate evaluation (should propagate, not
                            //   treat as "predicate failed")
                            //
                            // The match arm guard implementation (eval.rs:1733-1776) is the reference:
                            // it evaluates the guard, checks if it's callable (then invokes it), and
                            // checks truthiness. TypeAssert needs the same logic but with different
                            // error handling (fail assertion vs skip arm).
                            //
                            // Test expectation: tests/corpus/eval/errors/typeassert_is_predicate_fails.llt-eval
                            // expects [@[type: Int  is: [between 0 255]] 300] to fail with
                            // "type assertion failed: expected _ (is: predicate failed), got Int"
                            //
                            // For now, the `is:` predicate is silently ignored when present.
                            // The type checker does NOT validate `is:` predicates statically (they are
                            // runtime-only contracts), so this gap means predicates have no effect.
                            Action::Continue(Ok(value))
                        } else if let Some(default_node) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively.
                            // The result will flow to the next continuation on the stack.
                            Action::EvalCore {
                                expr: Arc::new(crate::ast_convert::surface_node_to_core_expr(
                                    default_node,
                                )),
                                env,
                                ctx: Arc::clone(&ctx),
                            }
                        } else {
                            let mut err = EvalError::type_assert_failed(
                                &format_type_for_assert(&expected),
                                value.type_name(),
                                thunk_span,
                            )
                            .with_materialization_span(expr_span);
                            if thunk_span != expr_span {
                                err = err.with_secondary_span(thunk_span, "value produced here");
                            }
                            Action::Continue(Err(err.into()))
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
                                .and_then(|type_node| match &type_node.expr {
                                    SurfaceExpression::Str(s) => Some(s.clone()),
                                    // Type names written as bare identifiers (e.g., `type: Number`)
                                    // are parsed as VarRef, not Str. Extract the name directly.
                                    SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                                    _ => None,
                                }),
                            Annotation::Annotated(name, _) => Some(name.clone()),
                        };
                        if let Some(expected) = expected_name {
                            let actual = value.type_name();
                            let matches = if expected == "Number" {
                                actual == "Int"
                                    || actual == "Float"
                                    || actual == "Decimal"
                                    || actual == "BigInt"
                            } else if expected == "Unknown"
                                || expected == "Top"
                                || expected == "Any"
                            {
                                // Unknown, Top, and Any accept all values (gradual typing escape hatch)
                                true
                            } else if expected == "Fn" {
                                // Fn matches both Function and Builtin
                                actual == "Function" || actual == "Builtin"
                            } else if expected == "Handle" {
                                // Handle matches both Handle and WriteHandle
                                actual == "Handle" || actual == "WriteHandle"
                            } else if expected == "Null" {
                                // Null is represented as an empty Dict at runtime
                                actual == "Dict"
                                    && matches!(value, Value::Dict(ref entries) if entries.is_empty())
                            } else {
                                actual == expected.as_str()
                            };
                            if !matches {
                                if let Some(default_node) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    return Action::EvalCore {
                                        expr: Arc::new(crate::ast_convert::surface_node_to_core_expr(
                                            default_node,
                                        )),
                                        env,
                                        ctx: Arc::clone(&ctx),
                                    };
                                }
                                let mut err =
                                    EvalError::type_assert_failed(&expected, actual, thunk_span)
                                        .with_materialization_span(expr_span);
                                if thunk_span != expr_span {
                                    err =
                                        err.with_secondary_span(thunk_span, "value produced here");
                                }
                                return Action::Continue(Err(err.into()));
                            }
                        } else if annotation_has_structural_fields(&annotation.node) {
                            // Structural record annotation without resolved_type — degrade
                            // to Dict tag check. Without elaboration we cannot validate
                            // field names or types, but we can verify the value is a Dict
                            // (the carrier type for records). This closes the elaboration
                            // gap for eval-only mode (doc/07 §--no-typecheck mode).
                            if !matches!(value, Value::Dict(_) | Value::Overlay(..)) {
                                let actual = value.type_name();
                                if let Some(default_node) =
                                    annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                                {
                                    // Evaluate default expression iteratively.
                                    // The result will flow to the next continuation on the stack.
                                    return Action::EvalCore {
                                        expr: Arc::new(crate::ast_convert::surface_node_to_core_expr(
                                            default_node,
                                        )),
                                        env,
                                        ctx: Arc::clone(&ctx),
                                    };
                                }
                                let mut err =
                                    EvalError::type_assert_failed("Record", actual, thunk_span)
                                        .with_materialization_span(expr_span);
                                if thunk_span != expr_span {
                                    err =
                                        err.with_secondary_span(thunk_span, "value produced here");
                                }
                                return Action::Continue(Err(err.into()));
                            }
                        }
                        Action::Continue(Ok(value))
                    }
                },
            }
        }
    }
}

/// Main iterative evaluation loop. Executes actions until a final result is produced.
///
/// This function drives the defunctionalized CEK machine: it repeatedly processes
/// `Action::EvalCore` steps (evaluating CoreExpr to a thunk via eval_core_expr_pub),
/// `Action::Materialize` steps (forcing thunks), and `Action::Continue` steps (applying
/// continuations) until the continuation stack is empty and a result is available.
///
/// # Arguments
/// - `initial`: The first action to execute (typically `Action::Materialize`)
/// - `ctx`: Evaluation context (needed for force_step's cycle detection and eval_stack)
///
/// # Returns
/// The final materialized value or error after all continuations have been applied.
///
/// # Tail-Call Optimization
/// The loop reuses the Rust stack frame on each iteration, so Rust stack depth is O(1).
/// The continuation stack is explicit (`stack`), preventing Rust stack overflow.
///
/// **Potential micro-optimization**: When force_step returns Action::Continue(result)
/// and stack.is_empty(), we could return directly instead of looping.
/// This would save 1 branch misprediction per tail-call. However, it adds complexity
/// (need to check stack.is_empty() after each step or pass it in).
/// DECISION: Defer until profiling shows this is a bottleneck (likely negligible).
pub(crate) async fn run(initial: Action, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::EvalCore {
                expr,
                env,
                ctx: action_ctx,
            } => {
                // Evaluate the CoreExpr to a thunk (without forcing).
                // If the result is already materialized (e.g., literals), take the
                // fast path and return Continue(Ok(value)) without pushing to the
                // continuation stack. Otherwise return Materialize to force iteratively.
                action = match eval_core_expr_pub(&expr, &env, &action_ctx).await {
                    Ok(thunk) => match thunk.try_get_materialized() {
                        Some(value) => Action::Continue(Ok(value)),
                        None => Action::Materialize {
                            thunk,
                            mat_span: Some(expr.span),
                        },
                    },
                    Err(e) => Action::Continue(Err(e)),
                };
            }
            Action::Materialize { thunk, mat_span } => {
                action = force_step(&thunk, mat_span, &mut stack, ctx).await;
            }
            Action::Continue(result) => match stack.pop() {
                None => return result,
                Some(cont) => {
                    action = apply_cont(cont, result, &mut stack).await;
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::CoreExpr;
    use crate::test_util::{sp, test_span};
    use crate::value::{Environment, Key, Thunk};

    fn empty_env() -> Arc<RwLock<Environment>> {
        Arc::new(RwLock::new(Environment::new()))
    }

    fn test_env() -> Arc<RwLock<Environment>> {
        empty_env()
    }

    fn test_ctx() -> Arc<EvalContext> {
        let env = empty_env();
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Synchronous shadow of `materialize()` for test contexts.
    fn materialize(
        thunk: &crate::value::Thunk,
        mat_span: Option<&crate::ast::Span>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::async_rt::block_on_anywhere(crate::eval::materialize(thunk, mat_span, ctx))
    }

    /// Synchronous shadow of `run()` for test contexts.
    fn run(initial: Action, ctx: &Arc<EvalContext>) -> crate::error::EvalResult<Value> {
        crate::async_rt::block_on_anywhere(super::run(initial, ctx))
    }

    #[test]
    fn test_restore_state_core_expr() {
        let expr = Arc::new(sp(CoreExpr::Int(42)));
        let env = empty_env();
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 10);

        let thunk = Thunk::new_unevaluated_core(expr.clone(), env.clone(), ctx.clone(), span);

        // Take the state (transitions to InProgress)
        let taken = thunk.take_core_expr();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::CoreExpr {
            expr: expr.clone(),
            env: env.clone(),
            ctx: ctx.clone(),
        };
        restore.restore(&thunk);

        // Verify state is restored (not InProgress, not materialized)
        assert!(
            !thunk.is_in_progress() && !thunk.is_materialized(),
            "Expected CoreExpr unevaluated state after restore"
        );
    }

    #[test]
    fn test_restore_state_pending_builtin() {
        use crate::value::BuiltinFn;

        let span = test_span(1, 1, 1, 10);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));

        // Create a dummy builtin function
        let dummy_func: BuiltinFn = |_args| {
            let span = test_span(1, 1, 1, 10);
            Box::pin(async move { Ok(Arc::new(Thunk::new_materialized(Value::Int(99), span))) })
        };
        let dummy_def = crate::value::BuiltinDef {
            func: dummy_func,
            name: "dummy",
            pos_strictness: &[],
            force_count: 0,
        };

        let args = vec![Arc::clone(&thunk)];
        let ctx = test_ctx();

        let pending_thunk = Thunk::new_pending_builtin(
            dummy_def,
            args.clone(),
            None,
            span,
            Some(Arc::from("test_origin")),
            ctx.clone(),
        );

        // Take the state (transitions to InProgress)
        let taken = pending_thunk.take_pending_builtin();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::PendingBuiltin {
            def: dummy_def,
            args,
            named: None,
            call_span: span,
            ctx: ctx.clone(),
        };
        restore.restore(&pending_thunk);

        // Verify state is restored
        assert!(
            pending_thunk.peek_builtin_def().is_some(),
            "Expected PendingBuiltin state (peek_builtin_def should return Some)"
        );
    }

    #[test]
    fn test_restore_state_pending_call() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Create a simple function thunk
        let func_thunk = Arc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Arc::new(sp(CoreExpr::Int(42))),
                env: empty_env(),
                annotation: None,
            },
            span,
        ));

        let args = vec![Arc::new(Thunk::new_materialized(Value::Int(1), span))];
        let named = IndexMap::new();
        let caller_env = empty_env();

        let pending_thunk = Arc::new(Thunk::new_pending_call(
            Arc::clone(&func_thunk),
            args.clone(),
            named.clone(),
            span,
            Arc::clone(&caller_env),
            span,
            Some(Arc::from("test_pending_call")),
            Arc::clone(&ctx),
        ));

        // Take the state (transitions to InProgress)
        let taken = pending_thunk.take_pending_call();
        assert!(taken.is_some());

        // Create RestoreState and restore
        let restore = RestoreState::PendingCall {
            func: Arc::clone(&func_thunk),
            args,
            named: if named.is_empty() {
                None
            } else {
                Some(Box::new(named))
            },
            call_span: span,
            caller_env,
            ctx: Arc::clone(&ctx),
        };
        restore.restore(&pending_thunk);

        // Verify state is restored
        assert!(
            pending_thunk.is_pending_call(),
            "Expected PendingCall state (is_pending_call should return true)"
        );
    }

    #[test]
    fn test_pending_call_restore_preserves_args() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Create a function thunk
        let func_thunk = Arc::new(Thunk::new_materialized(
            Value::Function {
                params: Rc::new(vec![]),
                body: Arc::new(sp(CoreExpr::Int(42))),
                env: empty_env(),
                annotation: None,
            },
            span,
        ));

        // Create multiple args with different values
        let args = vec![
            Arc::new(Thunk::new_materialized(Value::Int(1), span)),
            Arc::new(Thunk::new_materialized(Value::Int(2), span)),
            Arc::new(Thunk::new_materialized(string_val("test"), span)),
        ];
        let mut named = IndexMap::new();
        named.insert(
            "key".to_string(),
            Arc::new(Thunk::new_materialized(Value::Bool(true), span)),
        );
        let caller_env = empty_env();

        let pending_thunk = Arc::new(Thunk::new_pending_call(
            Arc::clone(&func_thunk),
            args.clone(),
            named.clone(),
            span,
            Arc::clone(&caller_env),
            span,
            Some(Arc::from("test_preserve_args")),
            Arc::clone(&ctx),
        ));

        // Take the state
        let taken = pending_thunk.take_pending_call();
        assert!(taken.is_some());

        // Restore
        let restore = RestoreState::PendingCall {
            func: Arc::clone(&func_thunk),
            args: args.clone(),
            named: if named.is_empty() {
                None
            } else {
                Some(Box::new(named.clone()))
            },
            call_span: span,
            caller_env,
            ctx: Arc::clone(&ctx),
        };
        restore.restore(&pending_thunk);

        // Verify the args are preserved
        let taken = pending_thunk.take_pending_call();
        assert!(
            taken.is_some(),
            "Expected PendingCall state (take_pending_call should return Some)"
        );
        let (_func, restored_args, restored_named, _call_span, _caller_env, _ctx) = taken.unwrap();

        // Check arg count
        assert_eq!(
            restored_args.len(),
            3,
            "Expected 3 positional args, got {}",
            restored_args.len()
        );

        // Check that the actual arg values are correct
        // materialize is the local sync shadow defined at the top of this test module
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

    #[test]
    fn test_guarded_type_assertion_failure_has_secondary_span() {
        // Test that when a Guarded type assertion fails, the error includes
        // a secondary_span pointing to where the value was produced (if different
        // from the assertion site).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        // Create a simple expression that produces an Int
        let value_span = test_span(5, 1, 5, 3); // Line 5: the value production site
        let value_thunk = crate::value::Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), value_span)),
            test_env(),
            test_ctx(),
            value_span,
        );

        // Create a Guarded thunk that expects String but wraps the Int
        let expected_type = Type::Str;
        let guard_span = test_span(10, 1, 10, 20); // Line 10: the assertion site
        let guarded = crate::value::Thunk::new_guarded(
            Arc::new(value_thunk),
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
            sec_span, value_span,
            "Secondary span should point to where the value was produced"
        );
        assert_eq!(
            sec_label, "value produced here",
            "Secondary span label should be 'value produced here'"
        );
    }

    #[test]
    fn test_guarded_secondary_span_suppressed_when_same_as_definition() {
        // Test that when the value production site is the same as the assertion site,
        // secondary_span is NOT set (would be redundant).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let same_span = test_span(1, 1, 1, 10);

        // Create a value at the same location as the guard
        let value_thunk = crate::value::Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), same_span)),
            test_env(),
            test_ctx(),
            same_span,
        );

        // Create a Guarded thunk with the same span for both guard and inner
        let guarded = crate::value::Thunk::new_guarded(
            Arc::new(value_thunk),
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
        let env = empty_env();
        let ctx = test_ctx();

        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), span)),
            env,
            Arc::clone(&ctx),
            span,
        ));

        // Verify initial state is Unevaluated (not yet materialized)
        assert!(
            thunk.try_get_materialized().is_none(),
            "Expected Unevaluated state before forcing"
        );

        // Force the thunk via the CEK machine
        let result = run(
            Action::Materialize {
                thunk: Arc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );

        // Verify the result is correct
        assert!(result.is_ok(), "Expected successful materialization");
        assert_eq!(result.unwrap(), Value::Int(42));

        // Verify the thunk transitioned to Materialized state
        assert_eq!(
            thunk.try_get_materialized(),
            Some(Value::Int(42)),
            "Cached value should be Int(42)"
        );

        // Verify that a second materialization returns the cached value immediately
        // (no re-evaluation)
        let result2 = run(
            Action::Materialize {
                thunk: Arc::clone(&thunk),
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
        // FreeVar performs name-based env lookup and fails when not found.
        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::FreeVar("undefined_var".into()),
                span,
            )),
            env,
            Arc::clone(&ctx),
            span,
        ));

        // Verify initial state is Unevaluated (not yet materialized)
        assert!(
            thunk.try_get_materialized().is_none(),
            "Expected Unevaluated state before forcing"
        );

        // Force the thunk — should fail with undefined variable error
        let result = run(
            Action::Materialize {
                thunk: Arc::clone(&thunk),
                mat_span: None,
            },
            &ctx,
        );

        // Verify the result is an error
        assert!(result.is_err(), "Expected error for undefined variable");
        let err = result.unwrap_err();
        assert!(
            err.kind.to_string().contains("undefined_var"),
            "Expected undefined variable error, got: {}",
            err.kind
        );

        // Verify the thunk transitioned to Failed state
        let cached_err = thunk.get_cached_error();
        assert!(cached_err.is_some(), "Expected Failed state");
        assert!(
            cached_err
                .unwrap()
                .kind
                .to_string()
                .contains("undefined_var"),
            "Cached error should be undefined variable error"
        );

        // Verify that a second materialization returns the cached error
        let result2 = run(
            Action::Materialize {
                thunk: Arc::clone(&thunk),
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
            err2.kind.to_string().contains("undefined_var"),
            "Cached error should be returned, got: {}",
            err2.kind
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
        let error_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::FreeVar("undefined_var".into()),
                span,
            )),
            Arc::clone(&env),
            Arc::clone(&ctx),
            span,
        ));

        let error_id = ctx.alloc_thunk(error_thunk);
        let mut dict_map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        dict_map.insert(Key::String("field".into()), error_id);
        let dict_value = Value::Dict(dict_map);
        let dict_thunk = Arc::new(Thunk::new_materialized(dict_value, span));

        // Insert the dict into the environment
        env.write().unwrap().insert("my_dict".into(), dict_thunk);

        // Create a dot access expression: my_dict.field
        let access_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::DotAccess {
                    expr: Arc::new(Spanned::new(CoreExpr::FreeVar("my_dict".into()), span)),
                    field: crate::ast::DotKey::Ident("field".to_string()),
                },
                span,
            )),
            env,
            Arc::clone(&ctx),
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
            err.kind.to_string().contains("undefined_var"),
            "Expected undefined variable error, got: {}",
            err.kind
        );
    }

    // ── GuardedValidate lifecycle tests ────────────────────────────────────────
    //
    // These three tests verify the three branches in Cont::GuardedValidate:
    //   Branch 1 (success): inner value matches expected type → thunk Materializes
    //   Branch 2 (failure + default): inner value fails type check AND default
    //             expression is present → default evaluated in caller's env
    //   Branch 3 (failure without default): inner value fails type check, no default
    //             → error propagates and thunk caches the failure (Failed state)
    //
    // Each test drives the full CEK machine through `run()` so the entire
    // force_step → push Cont::GuardedValidate → apply_cont path executes.

    #[test]
    fn test_guarded_validate_success_materializes_thunk() {
        // Branch 1: inner value matches expected type.
        // A Guarded thunk wrapping an Int value with an Int type expectation
        // should succeed and leave the thunk in Materialized state.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Inner thunk: an Int value that satisfies the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span));

        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
        ));

        let result = materialize(&guarded, None, &ctx);
        assert!(
            result.is_ok(),
            "Int value should pass Int guard, got: {:?}",
            result.unwrap_err()
        );
        assert_eq!(result.unwrap(), Value::Int(42));

        // After success, thunk must be in Materialized state (memoized).
        assert_eq!(
            guarded.try_get_materialized(),
            Some(Value::Int(42)),
            "after successful validation, thunk should be Materialized(Int(42))"
        );
    }

    #[test]
    fn test_guarded_validate_failure_with_default_evaluates_default_in_caller_env() {
        // Branch 2: inner value fails type check but a default expression is present.
        // The default expression should be evaluated in the caller's environment,
        // and the thunk should memoize the default result.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Bind a variable in caller's env so the default expr can reference it.
        let fallback_thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span));
        env.write()
            .unwrap()
            .insert("fallback_val".into(), fallback_thunk);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(
            crate::value::string_val("not an int"),
            span,
        ));

        // Default expression: a variable reference to `fallback_val` in caller's env.
        // Uses CoreExpr::FreeVar for name-based env lookup at runtime.
        let default_expr = Arc::new(sp(CoreExpr::FreeVar("fallback_val".into())));

        let guarded = Arc::new(Thunk::new_guarded_full(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
            None,
            Some((default_expr, Arc::clone(&env))),
        ));

        // Should succeed, returning the default value (99) evaluated in caller's env.
        let result = materialize(&guarded, None, &ctx);
        assert!(
            result.is_ok(),
            "validation failure with default should succeed, got: {:?}",
            result.unwrap_err()
        );
        assert_eq!(
            result.unwrap(),
            Value::Int(99),
            "default expression should yield 99 (from caller's env)"
        );

        // Thunk should be Materialized with the default value, not Failed.
        assert_eq!(
            guarded.try_get_materialized(),
            Some(Value::Int(99)),
            "after default fallback, thunk should be Materialized(Int(99))"
        );
    }

    #[test]
    fn test_guarded_validate_failure_without_default_propagates_error() {
        // Branch 3: inner value fails type check and no default is present.
        // The error should propagate to the caller and the thunk should cache the
        // failure (transition to Failed state) so subsequent access returns the
        // cached error without re-running the guard.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Inner thunk: a Bool value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Bool(true), span));

        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
        ));

        // First materialization: guard fires, Bool ≠ Int → type assertion failure.
        let result1 = materialize(&guarded, None, &ctx);
        assert!(
            result1.is_err(),
            "Bool value should fail Int guard (no default), but got success"
        );
        let err1 = result1.unwrap_err();
        assert!(
            err1.kind.to_string().contains("type assertion failed"),
            "error should report 'type assertion failed', got: {}",
            err1.kind
        );

        // After failure, thunk must be in Failed state (cacheable error).
        assert!(
            guarded.get_cached_error().is_some(),
            "after validation failure (no default) thunk should be Failed"
        );

        // Second materialization: returns cached error, does not re-run guard.
        let result2 = materialize(&guarded, None, &ctx);
        assert!(
            result2.is_err(),
            "second materialization should also fail (cached error)"
        );
        assert!(
            result2
                .unwrap_err()
                .kind
                .to_string()
                .contains("type assertion failed"),
            "cached error should still report 'type assertion failed'"
        );
    }

    // === Cont::BuiltinForceArg CEK tests ===

    /// BuiltinForceArg CEK test: force_count=1 pre-materializes arg[0] via CEK machine.
    ///
    /// Creates a PendingBuiltin thunk for `builtin_keys` (force_count=1) with an
    /// *unevaluated* dict argument. Forces the outer thunk through the CEK machine
    /// via `run(Action::Materialize { ... })`. The CEK machine must push a
    /// `Cont::BuiltinForceArg` continuation, materialize args[0], and then dispatch
    /// to `builtin_keys` with a pre-materialized dict.
    ///
    /// If `Cont::BuiltinForceArg` is not reached or does not properly force the arg,
    /// `builtin_keys` will panic at `try_get_materialized().expect(...)`.
    #[test]
    fn test_builtin_force_arg_cek_forces_arg_before_dispatch() {
        use crate::value::{BuiltinDef, BuiltinFn, Strictness};

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Create an unevaluated arg thunk: evaluates to an empty dict.
        // `CoreExpr::Dict(vec![])` produces `Value::Dict(IndexMap::new())`.
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span)),
            empty_env(),
            Arc::clone(&ctx),
            span,
        ));

        // Verify the arg is NOT yet materialized.
        assert!(
            unevaluated_arg.try_get_materialized().is_none(),
            "arg must be unevaluated before the PendingBuiltin is forced via CEK"
        );

        // Construct a BuiltinDef for `builtin_keys` with force_count=1.
        const KEYS_STRICTNESS: &[Strictness] = &[];
        let keys_def = BuiltinDef {
            func: crate::builtins::builtin_keys as BuiltinFn,
            name: "keys",
            pos_strictness: KEYS_STRICTNESS,
            force_count: 1,
        };

        // Create a PendingBuiltin thunk for the CEK machine to force.
        let outer_thunk = Arc::new(Thunk::new_pending_builtin(
            keys_def,
            vec![Arc::clone(&unevaluated_arg)],
            None,
            span,
            None,
            Arc::clone(&ctx),
        ));

        // Force via the CEK machine (not via materialize() recursive path).
        // This exercises the force_step(PendingBuiltin) path → Cont::BuiltinForceArg push.
        let result = run(
            Action::Materialize {
                thunk: Arc::clone(&outer_thunk),
                mat_span: None,
            },
            &ctx,
        );

        assert!(
            result.is_ok(),
            "Cont::BuiltinForceArg must pre-materialize force_count args via CEK; got: {:?}",
            result.unwrap_err()
        );

        // builtin_keys on an empty dict returns an empty dict.
        let val = result.unwrap();
        assert!(
            matches!(val, Value::Dict(ref m) if m.is_empty()),
            "expected empty dict from builtin_keys on empty dict, got {:?}",
            val
        );

        // After successful CEK dispatch, outer thunk must be in Materialized state.
        assert!(
            outer_thunk.try_get_materialized().is_some(),
            "outer PendingBuiltin thunk must be Materialized after CEK dispatch"
        );
    }

    /// BuiltinForceArg CEK test: force_count=2 forces two args before dispatch.
    ///
    /// Uses `builtin_keys` with force_count=2 to verify the continuation loops
    /// correctly through both arg positions (even though builtin_keys only uses args[0];
    /// the force_count=2 registration forces the test to exercise the multi-arg path).
    ///
    /// This ensures the BuiltinForceArg continuation correctly iterates through all
    /// force_count positions before dispatching.
    #[test]
    fn test_builtin_force_arg_cek_force_count_two() {
        use crate::value::{BuiltinDef, BuiltinFn, Strictness};

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Arg0: unevaluated dict (will be forced and used by builtin_keys).
        let unevaluated_arg0 = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span)),
            empty_env(),
            Arc::clone(&ctx),
            span,
        ));

        // Arg1: unevaluated int (will be force-materialized but not used by builtin_keys).
        let unevaluated_arg1 = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), span)),
            empty_env(),
            Arc::clone(&ctx),
            span,
        ));

        assert!(
            unevaluated_arg0.try_get_materialized().is_none(),
            "arg0 must be unevaluated"
        );
        assert!(
            unevaluated_arg1.try_get_materialized().is_none(),
            "arg1 must be unevaluated"
        );

        // force_count=2 — both args pre-materialized before dispatch.
        // builtin_keys only checks arity=1 and uses args[0], so arg1 being present
        // will cause an arity error. Use a custom 2-arg builtin that succeeds.
        // Instead, keep force_count=1 for the 2nd arg's CEK loop test via
        // checking that arg1 IS materialized after forcing the outer thunk.
        //
        // Actually: use a custom dummy builtin that accepts any arity and checks
        // that both args were pre-materialized.
        let dummy_func: BuiltinFn = |args| {
            // Both args must be materialized by force_count=2 before this is called.
            let _ = args.args[0]
                .try_get_materialized()
                .expect("pre-materialized by force_count/pos_strictness");
            let _ = args.args[1]
                .try_get_materialized()
                .expect("pre-materialized by force_count/pos_strictness");
            let span = args.call_span;
            Box::pin(async move { Ok(Arc::new(Thunk::new_materialized(Value::Bool(true), span))) })
        };

        const DUMMY_STRICTNESS: &[Strictness] = &[];
        let dummy_def = BuiltinDef {
            func: dummy_func,
            name: "dummy-force2",
            pos_strictness: DUMMY_STRICTNESS,
            force_count: 2,
        };

        let outer_thunk = Arc::new(Thunk::new_pending_builtin(
            dummy_def,
            vec![Arc::clone(&unevaluated_arg0), Arc::clone(&unevaluated_arg1)],
            None,
            span,
            None,
            Arc::clone(&ctx),
        ));

        // Force via CEK — exercises BuiltinForceArg loop for both positions.
        let result = run(
            Action::Materialize {
                thunk: Arc::clone(&outer_thunk),
                mat_span: None,
            },
            &ctx,
        );

        assert!(
            result.is_ok(),
            "BuiltinForceArg CEK must force both args when force_count=2; got: {:?}",
            result.unwrap_err()
        );
        assert_eq!(
            result.unwrap(),
            Value::Bool(true),
            "dummy builtin must succeed with both args pre-materialized"
        );
    }
}

// =============================================================================
// Deep materialization: recursively force all thunks in a value tree.
//
// This section implements the deep materialization algorithm with cycle detection
// and sharing preservation. Deep materialization is ONLY called at output boundaries
// (CLI JSON output, REPL display) and is NEVER part of normal evaluation.
// =============================================================================

/// Recursively force all thunks in a value tree.
///
/// - Primitives (Int, Float, String, Bool) are returned as-is.
/// - Dict values are fully materialized: each thunk entry is forced via
///   [`materialize`], then deep-materialized recursively. The returned Dict
///   wraps every value as [`Thunk::new_materialized`].
/// - Seq values are fully materialized: both head and tail thunks are forced
///   and recursively deep-materialized.
/// - Functions (user-defined and builtins) are returned as-is -- they are
///   opaque values, not collections to traverse.
///
/// Cycle detection and sharing preservation are handled via a
/// `HashMap<*const Thunk, Option<Arc<Thunk>>>` cache; see the
/// dual-purpose cache semantics in `force_thunk`.
///
/// `call_site_span` is the span of the call site (e.g., a builtin call) that
/// triggered deep materialization. If provided, it is attached to errors as the
/// materialization-site span. If `None`, the thunk's own span is used.
pub fn deep_materialize(
    val: &Value,
    ctx: &Arc<EvalContext>,
    call_site_span: Option<&Span>,
) -> EvalResult<Value> {
    // Fast path: primitives and functions need no traversal and no cache allocation.
    // This avoids a HashMap heap allocation for the common case where the top-level
    // value is already a scalar (Int, Float, Bool, String) or a function.
    match val {
        Value::Int(_)
        | Value::Float(_)
        | Value::String { .. }
        | Value::Bool(_)
        | Value::Bytes { .. }
        | Value::Function { .. }
        | Value::Builtin(_)
        | Value::Variant { payload: None, .. } => return Ok(val.clone()),
        _ => {}
    }
    let mut cache: HashMap<*const Thunk, Option<Arc<Thunk>>> = HashMap::new();
    let initial_span = call_site_span.copied().unwrap_or_else(Span::origin);
    deep_materialize_impl(val, ctx, &mut cache, 0, initial_span)
}

// Implementation note (iterative-eval-d): The iterative work-stack deep_materialize_impl
// eliminates O(nesting) Rust stack frames. Before: 100-deep dict → 100 recursive calls.
// After: 100-deep dict → 100 work items processed in a loop (constant Rust stack depth).
// Avoids O(n) repeated key collection by storing Rc<IndexMap> directly in BuildDict.
// (Formal benchmarks via criterion are tracked as future work in perf-foundations.)

// ---------------------------------------------------------------------------
// Iterative work-stack items
// ---------------------------------------------------------------------------

/// An item on the iterative work stack.
///
/// The traversal uses two stacks:
///
/// - `work_stack`: items to process (LIFO). Items that need results from
///   sub-items appear BELOW those sub-items on the stack.
/// - `value_stack`: completed `Arc<Thunk>` results (LIFO). Each `Force` item
///   pushes exactly one result. Each `Build*` collector pops N results and
///   pushes one assembled result.
///
/// The protocol for structural values (Dict / Seq / Proxy):
/// 1. Push `Build*` collector first (deepest on work stack → runs last).
/// 2. Push one `Force` item per child in reverse order so the first child
///    lands on top of the work stack → processed first → result deepest on
///    value stack (correct order for the collector).
enum DeepWorkItem {
    /// Force `thunk`, materialize it one level, then:
    /// - For primitives / functions: push the wrapped result onto `value_stack`.
    /// - For Dict / Seq / Proxy: push child `Force` items and a `Build*`
    ///   collector onto `work_stack`.  Nothing is pushed to `value_stack`
    ///   immediately; the collector does that after assembling children.
    Force {
        thunk: Arc<Thunk>,
        seq_depth: usize,
        /// The span to use for materialization errors. Either the original
        /// call-site span from `deep_materialize` or the thunk's own span.
        mat_span: Span,
    },
    /// Collect entries from `value_stack`, assemble a `Value::Dict`,
    /// wrap as a `Materialized` thunk, and push onto `value_stack`.
    /// `thunk_ptr` is the original thunk pointer — used to update the sharing
    /// cache after the dict is assembled.
    /// `dict_map` provides the original IndexMap to extract keys during assembly.
    BuildDict {
        dict_map: Rc<IndexMap<Key, ThunkId>>,
        span: Span,
        /// Original thunk pointer — updated in cache after assembly.
        /// `None` if the dict is a root value (no thunk to cache).
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop two thunks from `value_stack` (tail on top, head below), assemble
    /// a `Value::Seq`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildSeq {
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop one thunk from `value_stack` (the handler), assemble a
    /// `Value::Proxy`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildProxy {
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop one thunk from `value_stack` (the payload), assemble a
    /// `Value::Variant`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildVariant {
        tag: String,
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
}

/// Deep-force a value, using an explicit work stack to avoid Rust call-stack
/// recursion for deeply nested dicts and seq spines.
fn deep_materialize_impl(
    root_val: &Value,
    ctx: &Arc<EvalContext>,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    current_span: Span,
) -> EvalResult<Value> {
    // Fast path: primitives and functions need no traversal.
    match root_val {
        Value::Int(_)
        | Value::Float(_)
        | Value::String { .. }
        | Value::Bool(_)
        | Value::Bytes { .. }
        | Value::Function { .. }
        | Value::Builtin(_)
        | Value::Variant { payload: None, .. } => return Ok(root_val.clone()),
        _ => {}
    }

    // For structural values we need the work stack.  Seed it by expanding the
    // root value's immediate children.  The root has no thunk pointer in the
    // cache (it was already materialized by the caller).
    let mut work_stack: Vec<DeepWorkItem> = Vec::new();
    let mut value_stack: Vec<Arc<Thunk>> = Vec::new();

    deep_push_structural(
        root_val,
        cache,
        seq_depth,
        current_span,
        None,         // root has no thunk pointer
        current_span, // propagate call-site span
        &mut work_stack,
        &mut value_stack,
        ctx,
    )?;

    // Main work loop.
    while let Some(item) = work_stack.pop() {
        match item {
            DeepWorkItem::Force {
                thunk,
                seq_depth: item_seq_depth,
                mat_span,
            } => {
                deep_process_force(
                    &thunk,
                    ctx,
                    cache,
                    item_seq_depth,
                    mat_span,
                    &mut work_stack,
                    &mut value_stack,
                )?;
            }
            DeepWorkItem::BuildDict {
                dict_map,
                span,
                thunk_ptr,
            } => {
                let key_count = dict_map.len();
                let stack_len = value_stack.len();
                debug_assert!(
                    stack_len >= key_count,
                    "BuildDict: expected {key_count} values on stack, have {stack_len}"
                );
                let start = stack_len - key_count;
                let mut result: IndexMap<Key, ThunkId> = IndexMap::with_capacity(key_count);
                // Iterate keys from dict_map (which preserves insertion order)
                for (key, thunk) in dict_map.keys().cloned().zip(value_stack.drain(start..)) {
                    result.insert(key, ctx.alloc_thunk(thunk));
                }
                let assembled = Arc::new(Thunk::new_materialized(Value::Dict(result), span));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            DeepWorkItem::BuildSeq { span, thunk_ptr } => {
                let tail = value_stack
                    .pop()
                    .expect("BuildSeq: missing tail on value_stack");
                let head = value_stack
                    .pop()
                    .expect("BuildSeq: missing head on value_stack");
                let head_id = ctx.alloc_thunk(head);
                let tail_id = ctx.alloc_thunk(tail);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Seq {
                        head: head_id,
                        tail: tail_id,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            DeepWorkItem::BuildProxy { span, thunk_ptr } => {
                let handler = value_stack
                    .pop()
                    .expect("BuildProxy: missing handler on value_stack");
                let handler_id = ctx.alloc_thunk(handler);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Proxy {
                        handler: handler_id,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            DeepWorkItem::BuildVariant {
                tag,
                span,
                thunk_ptr,
            } => {
                let payload_thunk = value_stack
                    .pop()
                    .expect("BuildVariant: missing payload on value_stack");
                let payload_id = ctx.alloc_thunk(payload_thunk);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag,
                        payload: Some(payload_id),
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
        }
    }

    // The work loop should leave exactly one result on the value stack.
    debug_assert_eq!(
        value_stack.len(),
        1,
        "deep_materialize_impl: expected 1 result on value_stack, got {}",
        value_stack.len()
    );
    let result_thunk = value_stack
        .pop()
        .expect("deep_materialize_impl: empty value_stack after work loop");

    // Extract the materialized value from the result thunk.
    match result_thunk.try_get_materialized() {
        Some(v) => Ok(v),
        None => {
            unreachable!("deep_materialize_impl: result thunk is not Materialized after work loop")
        }
    }
}

/// Push work items to process the children of a structural `Value`
/// (Dict / Seq / Proxy) onto the work and value stacks.
///
/// For primitives and functions, push a pre-materialized thunk directly onto
/// `value_stack` (no child work needed).
///
/// `thunk_ptr` is the cache key for the parent thunk (if any), forwarded to
/// the `Build*` collector so it can update the sharing cache after assembly.
///
/// `mat_span` is the materialization-site span to thread through nested materializations.
///
/// Returns `Err` if the Seq spine guard fires (seq_depth >= MAX_COLLECT_SIZE).
#[allow(clippy::too_many_arguments)] // Internal helper for deep_materialize work queue
fn deep_push_structural(
    val: &Value,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    span: Span,
    thunk_ptr: Option<*const Thunk>,
    mat_span: Span,
    work_stack: &mut Vec<DeepWorkItem>,
    value_stack: &mut Vec<Arc<Thunk>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<()> {
    match val {
        Value::Overlay(l, r) => {
            // Flatten overlay to dict, then recurse as Dict.
            let map = flatten_overlay(l, r, "deep-materialize", ctx, span)?;
            return deep_push_structural(
                &Value::Dict(map),
                cache,
                seq_depth,
                span,
                thunk_ptr,
                mat_span,
                work_stack,
                value_stack,
                ctx,
            );
        }
        Value::Dict(map) => {
            if map.is_empty() {
                // Empty dict: assemble immediately, no children.
                let t = Arc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&t)));
                }
                value_stack.push(t);
                return Ok(());
            }
            // Collector runs last: push first. Store an Rc to the original dict_map
            // so we can iterate its keys during BuildDict without allocating a Vec.
            let dict_map_rc = Rc::new(map.clone());
            work_stack.push(DeepWorkItem::BuildDict {
                dict_map: Rc::clone(&dict_map_rc),
                span,
                thunk_ptr,
            });
            // Push Force items in reverse: first key ends on top → processed
            // first → result deepest on value_stack → collected in order.
            for key in dict_map_rc.keys().rev() {
                let entry_thunk = ctx.get_thunk(map[key]);
                work_stack.push(DeepWorkItem::Force {
                    thunk: entry_thunk,
                    seq_depth: 0, // dict entries reset seq_depth
                    mat_span,     // propagate call-site span through nested materializations
                });
            }
        }
        Value::Seq { head, tail } => {
            // Seq spine guard: prevents unbounded traversal of infinite sequences.
            if seq_depth >= MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    "cannot deep-materialize an infinite Seq: use $collect with $take first"
                        .to_string(),
                    span,
                )
                .into());
            }
            // Collector runs last: push first.
            work_stack.push(DeepWorkItem::BuildSeq { span, thunk_ptr });
            // Push tail SECOND on work_stack → processed second → lands on TOP
            // of value_stack → BuildSeq pops tail first.
            let tail_thunk = ctx.get_thunk(*tail);
            work_stack.push(DeepWorkItem::Force {
                thunk: tail_thunk,
                seq_depth: seq_depth + 1,
                mat_span, // propagate call-site span through nested materializations
            });
            // Push head LAST on work_stack → processed first → result BELOW
            // tail on value_stack → BuildSeq pops head after tail.
            let head_thunk = ctx.get_thunk(*head);
            work_stack.push(DeepWorkItem::Force {
                thunk: head_thunk,
                seq_depth: 0, // head resets seq_depth
                mat_span,     // propagate call-site span through nested materializations
            });
        }
        Value::Proxy { handler } => {
            work_stack.push(DeepWorkItem::BuildProxy { span, thunk_ptr });
            let handler_thunk = ctx.get_thunk(*handler);
            work_stack.push(DeepWorkItem::Force {
                thunk: handler_thunk,
                seq_depth: 0,
                mat_span, // propagate call-site span through nested materializations
            });
        }
        Value::Variant { tag, payload } => {
            if let Some(payload_id) = payload {
                // Variant with payload: force the payload recursively
                work_stack.push(DeepWorkItem::BuildVariant {
                    tag: tag.clone(),
                    span,
                    thunk_ptr,
                });
                let payload_thunk = ctx.get_thunk(*payload_id);
                work_stack.push(DeepWorkItem::Force {
                    thunk: payload_thunk,
                    seq_depth: 0, // variant payload resets seq_depth
                    mat_span,     // propagate call-site span through nested materializations
                });
            } else {
                // Variant without payload: leaf value, no children to traverse
                let t = Arc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag: tag.clone(),
                        payload: None,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&t)));
                }
                value_stack.push(t);
            }
        }
        // Primitives and functions: no children.
        other => {
            let t = Arc::new(Thunk::new_materialized(other.clone(), span));
            if let Some(ptr) = thunk_ptr {
                cache.insert(ptr, Some(Arc::clone(&t)));
            }
            value_stack.push(t);
        }
    }
    Ok(())
}

/// Process a single `DeepWorkItem::Force`: check the sharing/cycle cache, call
/// [`materialize`], then expand the materialized value's structure.
///
/// On success, exactly one new result is eventually pushed onto `value_stack`
/// (either immediately for cached/leaf values, or after the children are
/// processed by a `Build*` collector).
///
/// On error, propagates the error.  The cache sentinel (`None`) is removed
/// before propagating to prevent cache poisoning (same as the old
/// `deep_materialize_thunk`).
fn deep_process_force(
    thunk: &Arc<Thunk>,
    ctx: &Arc<EvalContext>,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    mat_span: Span,
    work_stack: &mut Vec<DeepWorkItem>,
    value_stack: &mut Vec<Arc<Thunk>>,
) -> EvalResult<()> {
    let thunk_ptr = Arc::as_ptr(thunk);

    // Cache lookup: sharing hit or cycle sentinel.
    match cache.get(&thunk_ptr) {
        Some(Some(cached)) => {
            value_stack.push(Arc::clone(cached));
            return Ok(());
        }
        Some(None) => {
            // Cycle sentinel: return the original thunk unchanged.
            // Returns Arc::clone(thunk) safely because materialize() has already transitioned
            // the thunk to Materialized; sub-structure of the returned thunk is not deep-forced
            // (documented behavior for cycles).
            value_stack.push(Arc::clone(thunk));
            return Ok(());
        }
        None => {}
    }

    let thunk_span = thunk.span;

    // Insert the in-progress (cycle) sentinel.
    cache.insert(thunk_ptr, None);

    // Materialize the thunk one level.
    // Use the mat_span from the DeepWorkItem::Force, which is either the original
    // call-site span from deep_materialize or the thunk's own span.
    let v = match crate::async_rt::block_on_anywhere(materialize(thunk, Some(&mat_span), ctx)) {
        Ok(v) => v,
        Err(e) => {
            // Clean up sentinel on error (same as old deep_materialize_thunk).
            cache.remove(&thunk_ptr);
            return Err(e);
        }
    };

    // Expand the materialized value.  For leaf values, push directly to
    // value_stack and update cache.  For structural values, deep_push_structural
    // queues child work items and a Build* collector; the collector updates
    // the cache with `thunk_ptr` when it assembles the final result.
    deep_push_structural(
        &v,
        cache,
        seq_depth,
        thunk_span,
        Some(thunk_ptr),
        mat_span, // propagate call-site span through nested materializations
        work_stack,
        value_stack,
        ctx,
    )
    .map_err(|mut e| {
        // Depth / infinite-Seq error from a child: attach the source thunk's
        // span as a frame so depth-exceeded errors show where in the structure
        // the recursion limit was hit.
        if thunk_span != Span::origin() {
            e.push_frame("deep-materializing".to_string(), thunk_span);
        }
        // Remove the sentinel for this thunk since we failed.
        // (deep_push_structural already cleaned up any sentinels it inserted.)
        cache.remove(&thunk_ptr);
        // Clear work_stack to avoid leaking DeepWorkItem::Force Arc<Thunk> references.
        // When deep_push_structural fails mid-traversal, it may have pushed Build* and
        // Force items that will never be processed. Clearing prevents Rc leak.
        work_stack.clear();
        e
    })
}

#[cfg(test)]
mod deep_tests {
    use super::*;
    use crate::ast::{CoreExpr, Spanned};
    use crate::test_util::test_span;
    use crate::value::Environment;
    use std::sync::RwLock;

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let stdlib_env = crate::builtins::create_stdlib_env().expect("stdlib failed");
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
        EvalContext::new(base_dir, stdlib_env, type_stage_env, false)
    }

    #[test]
    fn test_deep_materialize_cycle_sentinel() {
        // Test the cycle detection path.
        // When a thunk pointer is already in the cache with None value
        // (the cycle sentinel), deep_process_force should return the original thunk unchanged.
        //
        // Uses `Thunk::new_materialized` to isolate cache-lookup logic from evaluation;
        // real cycles are encountered after `materialize()` has already transitioned the thunk.
        let span = test_span(1, 1, 1, 5);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));

        // Create a cache and pre-populate it with a None entry for this thunk
        let mut cache = std::collections::HashMap::new();
        let thunk_ptr = Arc::as_ptr(&thunk);
        cache.insert(thunk_ptr, None);

        // Call deep_process_force with the pre-populated cache
        let mut work_stack = Vec::new();
        let mut value_stack = Vec::new();
        let ctx = test_ctx();
        deep_process_force(
            &thunk,
            &ctx,
            &mut cache,
            0,
            span, // mat_span
            &mut work_stack,
            &mut value_stack,
        )
        .unwrap();

        // The original thunk should have been pushed onto value_stack
        assert_eq!(value_stack.len(), 1);
        assert!(
            Arc::ptr_eq(&thunk, &value_stack[0]),
            "deep_process_force must push the original thunk when cycle sentinel (None) is found in cache"
        );
    }

    #[test]
    fn test_deep_materialize_preserves_sharing_through_eval() {
        // Test that sharing is preserved when the shared thunk is unevaluated,
        // exercising the actual cache-population path where:
        // 1. First encounter forces the thunk and caches the result
        // 2. Second encounter returns the cached result
        let span = test_span(1, 1, 1, 5);
        let expr = Arc::new(Spanned::new(CoreExpr::Int(42), span));
        let env = Arc::new(RwLock::new(Environment::new()));
        let ctx = test_ctx();

        // Create an unevaluated thunk and allocate it once — same ThunkId for both positions
        let shared_thunk_rc = Arc::new(Thunk::new_unevaluated_core(
            expr,
            env,
            Arc::clone(&ctx),
            span,
        ));
        let shared_id = ctx.alloc_thunk(shared_thunk_rc);

        // Place the same ThunkId in two positions of a dict
        let mut map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        map.insert(Key::String("a".into()), shared_id);
        map.insert(Key::String("b".into()), shared_id);
        let val = Value::Dict(map);

        // Deep materialize the container
        let result = deep_materialize(&val, &ctx, None).unwrap();

        match result {
            Value::Dict(map) => {
                let a = &map[&Key::String("a".into())];
                let b = &map[&Key::String("b".into())];

                // Verify both entries resolve to the same value (ThunkId equality not guaranteed).
                let va = crate::async_rt::block_on_anywhere(crate::eval::materialize(
                    &ctx.get_thunk(*a),
                    None,
                    &ctx,
                ))
                .unwrap();
                let vb = crate::async_rt::block_on_anywhere(crate::eval::materialize(
                    &ctx.get_thunk(*b),
                    None,
                    &ctx,
                ))
                .unwrap();
                assert_eq!(va, Value::Int(42), "entry a should be Int(42)");
                assert_eq!(vb, Value::Int(42), "entry b should be Int(42)");
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_cache_cleanup_on_materialize_error() {
        // Test that cacheable errors (undefined variable) leave the thunk in ThunkState::Failed
        // and are memoized for retry — a second deep_materialize call returns the same cached error
        // rather than re-evaluating. This complements test_deep_materialize_cache_cleanup_on_error
        // which tests DepthExceeded (non-cacheable, sentinel removed on error).
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Create a thunk that will fail with a cacheable error (undefined variable)
        let env = Arc::new(RwLock::new(Environment::new()));
        let error_expr = Arc::new(Spanned::new(CoreExpr::FreeVar("undefined".into()), span));
        let error_thunk = Arc::new(Thunk::new_unevaluated_core(
            error_expr,
            env,
            Arc::clone(&ctx),
            span,
        ));

        // Place the error thunk in a dict
        let error_id = ctx.alloc_thunk(Arc::clone(&error_thunk));
        let mut map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        map.insert(Key::String("x".into()), error_id);
        let dict_val = Value::Dict(map);

        // Attempt to deep materialize — should fail
        let err = deep_materialize(&dict_val, &ctx, None).unwrap_err();
        assert!(
            err.kind.to_string().contains("undefined"),
            "Expected undefined variable error, got: {}",
            err.kind
        );

        // Verify the error_thunk is in Failed state (cacheable error was cached)
        {
            // Check that the thunk has a failed result
            assert!(
                error_thunk.try_get_materialized().is_none(),
                "Expected thunk to not be materialized (should be Failed)"
            );
            // The error is cached - we can't easily inspect it without .state(),
            // but we can verify a second materialization attempt fails with the same error
        }

        // A second deep_materialize should also fail (error is cached in thunk)
        let err2 = deep_materialize(&dict_val, &ctx, None).unwrap_err();
        assert!(
            err2.kind.to_string().contains("undefined"),
            "Expected cached error on retry, got: {}",
            err2.kind
        );
    }

    // ========== test-coverage-cycle311 tests ==========

    #[test]
    fn test_max_continuation_stack_enforced() {
        // Test that MAX_CONTINUATION_STACK=2048 is enforced.
        //
        // Build a chain of PendingBuiltin thunks: each forces the next as an arg,
        // creating ~depth Memoize+BuiltinForceArg continuations. At 2100 levels
        // the continuation stack exceeds the 2048 limit.
        //
        // Direct source parsing can't build this depth (lexer limits to 256 brackets),
        // so we construct thunks programmatically.
        //
        // Uses the iterative CEK `run()` function (not the recursive `materialize()`)
        // to exercise the continuation stack limit without Rust stack overflow.
        //
        // Runs in a large-stack thread because 2100-deep Arc<Thunk> drop chains
        // overflow the default test thread stack.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use crate::ast::Span;
                use crate::value::{Thunk, Value};
                use std::sync::Arc;

                let ctx = test_ctx();
                let origin = Span::origin();

                // Get the $+ builtin definition directly from the standard builtins list
                // (not from stdlib_env, which wraps arithmetic ops with operator dispatch).
                let builtin_def = crate::builtins::standard_builtins()
                    .into_iter()
                    .find(|b| b.name == "+")
                    .expect("$+ must exist in standard_builtins()");

                // Build chain: thunk_0 = Materialized(Int(0))
                // thunk_i = PendingBuiltin($+, [thunk_{i-1}, 1])
                let mut prev = Arc::new(Thunk::new_materialized(Value::Int(0), origin));

                for _ in 0..2100 {
                    let one = Arc::new(Thunk::new_materialized(Value::Int(1), origin));
                    let thunk = Arc::new(Thunk::new_pending_builtin(
                        builtin_def.clone(),
                        vec![Arc::clone(&prev), one],
                        None,
                        origin,
                        None,
                        Arc::clone(&ctx),
                    ));
                    prev = thunk;
                }

                // Force `prev` via the iterative CEK machine to exercise the continuation
                // stack limit (check_stack_depth in force_step).
                let result = crate::async_rt::block_on_anywhere(super::run(
                    super::Action::Materialize {
                        thunk: Arc::clone(&prev),
                        mat_span: None,
                    },
                    &ctx,
                ));

                assert!(
                    result.is_err(),
                    "Expected depth-exceeded error for 2100-deep PendingBuiltin chain"
                );
                let err = format!("{}", result.unwrap_err());
                assert!(
                    err.contains("maximum evaluation depth exceeded") || err.contains("[E040]"),
                    "Error should be E040 (maximum evaluation depth exceeded), got: {}",
                    err
                );
            })
            .expect("thread spawn failed")
            .join()
            .expect("test thread panicked");
    }

    #[test]
    fn test_restore_state_pending_builtin_non_cacheable_error() {
        // Test that a non-cacheable error (DepthExceeded / E040) is consistently
        // raised on repeated evaluation attempts.
        //
        // DepthExceeded is the canonical non-cacheable error: the CEK machine cannot
        // memoize it on the thunk because a retry at shallower depth might succeed.
        // Both calls to `run` here force a fresh thunk chain, verifying the error
        // surface is stable: both calls report a depth/limit error, not a crash or
        // silent success.
        //
        // The existing test_restore_state_pending_builtin (line 2569) covers the
        // RestoreState::PendingBuiltin internal restore mechanism directly.
        //
        // Uses a large-stack thread because 2100-deep Arc<Thunk> drop chains
        // overflow the default test thread stack.
        std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                use crate::ast::Span;
                use crate::value::{Thunk, Value};
                use std::sync::Arc;

                let ctx = test_ctx();
                let origin = Span::origin();

                let builtin_def = crate::builtins::standard_builtins()
                    .into_iter()
                    .find(|b| b.name == "+")
                    .expect("$+ must exist in standard_builtins()");

                // Helper: build a 2100-deep PendingBuiltin thunk chain.
                let build_chain = |def: &crate::value::BuiltinDef,
                                   ctx: &Arc<crate::eval::EvalContext>|
                 -> Arc<Thunk> {
                    let mut prev = Arc::new(Thunk::new_materialized(Value::Int(0), origin));
                    for _ in 0..2100 {
                        let one = Arc::new(Thunk::new_materialized(Value::Int(1), origin));
                        let thunk = Arc::new(Thunk::new_pending_builtin(
                            def.clone(),
                            vec![Arc::clone(&prev), one],
                            None,
                            origin,
                            None,
                            Arc::clone(ctx),
                        ));
                        prev = thunk;
                    }
                    prev
                };

                // First call — should hit depth limit
                let chain1 = build_chain(&builtin_def, &ctx);
                let result1 = crate::async_rt::block_on_anywhere(super::run(
                    super::Action::Materialize {
                        thunk: Arc::clone(&chain1),
                        mat_span: None,
                    },
                    &ctx,
                ));
                assert!(
                    result1.is_err(),
                    "First call: expected depth-exceeded error"
                );
                let err1 = format!("{}", result1.unwrap_err());
                assert!(
                    err1.contains("maximum evaluation depth exceeded") || err1.contains("[E040]"),
                    "First call: error should mention depth limit, got: {}",
                    err1
                );

                // Second call — fresh chain, same pattern. Should produce the same class
                // of error, confirming the non-cacheable error path is stable.
                let chain2 = build_chain(&builtin_def, &ctx);
                let result2 = crate::async_rt::block_on_anywhere(super::run(
                    super::Action::Materialize {
                        thunk: Arc::clone(&chain2),
                        mat_span: None,
                    },
                    &ctx,
                ));
                assert!(
                    result2.is_err(),
                    "Second call: expected depth-exceeded error"
                );
                let err2 = format!("{}", result2.unwrap_err());
                assert!(
                    err2.contains("maximum evaluation depth exceeded") || err2.contains("[E040]"),
                    "Second call: error should mention depth limit, got: {}",
                    err2
                );
            })
            .expect("thread spawn failed")
            .join()
            .expect("test thread panicked");
    }

    #[test]
    fn test_attach_materialization_context_preserves_spans() {
        // Test that attach_materialization_context correctly adds materialization
        // span and origin frame to errors.
        //
        // This is already tested by test_attach_materialization_context_adds_frame,
        // but we add a variant that tests the preservation of existing spans
        // (the "if err.materialization_span.is_none()" branch).

        let thunk_span = test_span(1, 1, 1, 10);
        let err = EvalError::undefined_variable("x".to_string(), thunk_span);
        let mat_span = test_span(10, 5, 10, 6);
        let origin = "test_origin";

        // First attachment — should set materialization_span
        let decorated = attach_materialization_context(
            Box::new(err),
            Some(&mat_span),
            Some(origin),
            thunk_span,
        );

        assert_eq!(
            decorated.materialization_span,
            Some(mat_span),
            "materialization_span should be set"
        );

        // Second attachment with a different mat_span — should preserve the first
        let second_mat_span = test_span(20, 1, 20, 5);
        let decorated2 = attach_materialization_context(
            decorated,
            Some(&second_mat_span),
            Some("second_origin"),
            thunk_span,
        );

        assert_eq!(
            decorated2.materialization_span,
            Some(mat_span),
            "materialization_span should preserve the first value, not overwrite"
        );
    }

    #[test]
    fn test_type_assert_inline_in_force_step() {
        // Test that TypeAssert nodes are handled correctly during materialization
        // (inline in force_step).
        //
        // This exercises the CoreExpr::TypeAssert arm in force_step, which pushes
        // a Cont::TypeAssertCheck continuation and evaluates the inner expression.
        //
        // Use eval_source to test end-to-end behavior.

        // Simple case: TypeAssert with a matching type
        let input = "[x@Int: 42]";
        let result = crate::eval_source(input);
        assert!(result.is_ok(), "TypeAssert should succeed: {:?}", result);
        assert_eq!(result.unwrap(), r#"Dict({"x": Int(42)})"#);

        // Case with type mismatch: `[@Int "hello"]` — standalone TypeAssert says value is Int,
        // but the value is a String. At runtime this triggers a TypeAssert check (E011).
        let input_mismatch = r#"[@Int "hello"]"#;
        let result_mismatch = crate::eval_source(input_mismatch);
        assert!(
            result_mismatch.is_err(),
            "TypeAssert [@Int \"hello\"] should fail with E011: {:?}",
            result_mismatch
        );
        let err_msg = result_mismatch.unwrap_err();
        assert!(
            err_msg.contains("E011") || err_msg.contains("type assertion failed"),
            "Expected E011 type assertion error, got: {}",
            err_msg
        );

        // Case with structural type annotation (record)
        let input_record = r#"[@[name: String] [name: "Alice"]]"#;
        let result_record = crate::eval_source(input_record);
        assert!(
            result_record.is_ok(),
            "TypeAssert with record should succeed: {:?}",
            result_record
        );
    }
}
