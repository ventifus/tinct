//! Iterative materialization machinery: CEK continuation stack and force loop.
//!
//! Includes inline TypeAssert handling in force_step for correct lazy type validation.
//!
//! This module contains the core iterative evaluator (run/force_step/apply_cont)
//! that materializes thunks without recursion. The CEK machine design is documented
//! in doc/08-evaluation.md §Iterative Evaluator.

use std::collections::HashSet;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::ast::{Annotation, CoreExpr, Span, Spanned, SurfaceExpression};
use crate::builtins::flatten_overlay;
use crate::error::{EvalError, EvalResult};
use crate::eval::{
    annotation_has_structural_fields, as_record_row_merged, eval_core_expr_pub, format_field_path,
    format_type_for_assert, match_pattern, materialize, maybe_wrap_guard, validate_and_wrap_record,
    value_matches_type, EvalContext, DEFAULT_ANNOTATION_KEY, IS_ANNOTATION_KEY,
};
use crate::eval_access::invoke_proxy_handler;
use crate::eval_call::{invoke_function, invoke_function_tco, CallContext};
use crate::types::Type;
use crate::value::{string_val, Environment, Key, Thunk, Value};

/// RAII guard for profiling spans. Automatically closes the span on drop.
struct ProfilingSpanGuard {
    profiling: Option<Arc<Mutex<crate::profiling::ProfilingCollector>>>,
    span_id: Option<u64>,
}

impl ProfilingSpanGuard {
    fn new(ctx: &Arc<EvalContext>, thunk: &Thunk) -> Self {
        let (profiling, span_id) = if let Some(ref prof) = ctx.profiling {
            // Extract span source information.
            // Span has no file field; source_file is not available from the span alone.
            // The include cache (not yet plumbed here) would provide it in a future sprint.
            let source_file: Option<String> = None;
            let (source_start, source_end) = if thunk.span != crate::ast::Span::origin() {
                (
                    Some((thunk.span.start.line, thunk.span.start.column)),
                    Some((thunk.span.end.line, thunk.span.end.column)),
                )
            } else {
                (None, None)
            };

            // Extract source text snippet (TODO: from include cache)
            let source_text = None;

            // Extract builtin name from origin if it looks like a builtin
            let (builtin_name, origin_builtin) = match &thunk.origin {
                Some(origin) if origin.starts_with("builtin-") => (Some(origin.to_string()), None),
                Some(origin) => (None, Some(origin.to_string())),
                None => (None, None),
            };

            let id = prof.lock().unwrap().open_span(
                source_file,
                source_start,
                source_end,
                source_text,
                builtin_name,
                origin_builtin,
                thunk.create_parent,
                thunk.create_time_us,
            );
            (Some(Arc::clone(prof)), Some(id))
        } else {
            (None, None)
        };

        Self { profiling, span_id }
    }
}

impl Drop for ProfilingSpanGuard {
    fn drop(&mut self) {
        if let (Some(ref profiling), Some(span_id)) = (&self.profiling, self.span_id) {
            profiling.lock().unwrap().close_span(span_id);
        }
    }
}

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
    PendingBuiltin {
        def: crate::value::BuiltinDef,
        args: Vec<Arc<Thunk>>,
        named: Option<IndexMap<String, Arc<Thunk>>>,
        call_span: Span,
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
    /// TODO(future): store already-lowered CoreExpr to avoid re-lowering on retry.
    Surface {
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        res: std::sync::Arc<crate::ast::ResolutionTable>,
        types: std::sync::Arc<crate::ast::TypeAnnotationTable>,
        env: Arc<RwLock<Environment>>,
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
    // Some when the original thunk state can be restored on non-cacheable errors.
    // Always Some for all push sites (PendingBuiltin, PendingCall, GuardedValidate default-fallback).
    // GuardedValidate default-fallback builds a fresh RestoreState::Guarded rather than
    // consuming the original restore via take(), ensuring Memoize always has a valid restore.
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
    pub(crate) original_call: Arc<Spanned<CoreExpr>>,
    pub(crate) tail_hint: bool,
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
/// TODO(future): Annotation::PropertyDict entries store SurfaceNode values. Default-fallback
/// paths extract "default:" as &Arc<SurfaceNode>, lower via `crate::lower::lower`, and dispatch
/// as Action::EvalCore. When Annotation stores CoreExpr values natively, this becomes a
/// zero-cost Arc<Spanned<CoreExpr>> clone.
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

/// Payload for Cont::SequentialStep. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct SequentialStepData {
    /// Remaining expressions to evaluate (index into the original Sequential exprs vec).
    /// When idx reaches exprs.len(), the Sequential is complete and we return the last value.
    pub(crate) idx: usize,
    pub(crate) exprs: Arc<Vec<Arc<Spanned<CoreExpr>>>>,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) seq_span: Span,
}

/// Payload for Cont::ForceAndBind. Boxed to keep the Cont enum ≤96 bytes.
///
/// After a dict entry value is forced to WHNF, this continuation inserts it as a
/// materialized thunk into child_env and then either forces the next entry (if any
/// remain) or evaluates the next sequential expression (when all entries are bound).
///
/// This enforces strict let* semantics for sequential-step bindings: every named
/// binding is fully evaluated before the next expression in the sequence runs.
/// Without forcing here, a later expression that accesses a binding can trigger
/// a circular dependency (E070) when the outer sequential is still "in progress".
pub(crate) struct ForceAndBindData {
    /// Name of the entry that was just forced (used to insert into child_env).
    pub(crate) name: String,
    /// Span of the dict entry value (used when wrapping the forced value as a thunk).
    pub(crate) value_span: Span,
    /// Remaining entries still to force-and-bind, in order.
    pub(crate) remaining: Vec<(String, Arc<Thunk>)>,
    /// The child environment being built (shared across all ForceAndBind steps).
    pub(crate) child_env: Arc<RwLock<Environment>>,
    /// The sequential step to push once all entries are bound.
    pub(crate) step: Box<SequentialStepData>,
}

/// Payload for Cont::MatchDispatch. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MatchDispatchData {
    /// The arms to try matching. Index starts at 0.
    pub(crate) arm_idx: usize,
    pub(crate) arms: Arc<Vec<crate::ast::CoreMatchArm>>,
    /// The original environment for fallback matching
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) match_span: Span,
}

/// Payload for Cont::MatchGuardCheck. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MatchGuardCheckData {
    /// Current arm index (for continuing to next arm if guard fails)
    pub(crate) arm_idx: usize,
    pub(crate) arms: Arc<Vec<crate::ast::CoreMatchArm>>,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) match_span: Span,
    /// Environment with pattern bindings from the matched arm
    pub(crate) arm_env: Arc<RwLock<Environment>>,
    /// The scrutinee value (needed for predicate invocation and fallback)
    pub(crate) scrutinee_value: Value,
    /// The arm body to evaluate if guard passes
    pub(crate) body: Arc<Spanned<CoreExpr>>,
}

/// Payload for Cont::PredicateCheck. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct PredicateCheckData {
    /// The value being checked (already materialized and type-validated)
    pub(crate) value: Value,
    /// The annotation (needed for extracting default: property on failure)
    pub(crate) annotation: Box<Spanned<Annotation>>,
    /// Span of the TypeAssert expression (for error reporting)
    pub(crate) expr_span: Span,
    /// Span where the value was produced (for error reporting)
    pub(crate) thunk_span: Span,
    /// Environment for evaluating the default expression if predicate fails
    pub(crate) env: Arc<RwLock<Environment>>,
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
    /// TODO(future): Annotation::PropertyDict entries are SurfaceEntry whose values are
    /// Arc<SurfaceNode>. Default-fallback paths call annotation.node.get_property("default:")
    /// → &Arc<SurfaceNode> → lower::lower → Action::EvalCore. When annotations store CoreExpr
    /// values natively, this becomes a no-op clone.
    TypeAssertCheck(Box<TypeAssertCheckData>),
    /// Process the next step in a Sequential expression chain.
    /// After an intermediate expression is materialized (and its dict bindings extracted),
    /// this continuation evaluates the next expression in the sequence.
    SequentialStep(Box<SequentialStepData>),
    /// Force a dict entry value to WHNF and insert it (materialized) into child_env.
    /// Pushed by the SequentialStep handler for each static-key entry so that all
    /// bindings are resolved before the next sequential expression is evaluated.
    /// This prevents E070 circular dependencies from lazy binding insertion.
    ForceAndBind(Box<ForceAndBindData>),
    /// Dispatch to the next arm after materializing the scrutinee in a Match expression.
    /// Tries each arm pattern in order until one matches, then evaluates that arm's body.
    MatchDispatch(Box<MatchDispatchData>),
    /// Check the guard result for a matched arm and either evaluate the body (guard passed)
    /// or continue to the next arm (guard failed).
    MatchGuardCheck(Box<MatchGuardCheckData>),
    /// Check the result of an is: predicate evaluation for a TypeAssert.
    /// After materializing the predicate result, checks truthiness and either returns
    /// the original value (predicate passed) or evaluates the default expression / fails.
    PredicateCheck(Box<PredicateCheckData>),
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
    fn push(state: &Arc<Mutex<crate::eval::EvalState>>, entry: (Arc<str>, Span)) -> Self {
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
    /// from `Arc<SurfaceNode>` to `Spanned<CoreExpr>` at emit time via `lower::lower`.
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

    // Open profiling span if profiling is enabled. The guard closes the span on drop.
    let _profile_guard = ProfilingSpanGuard::new(ctx, thunk);

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
            (
                origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                thunk_span,
            ),
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

        // Clone args/named for the builtin call; keep originals in the Option slots for
        // state restoration on the slow path (non-pre-materialized result) or on
        // non-cacheable errors (e.g. DepthExceeded). This defers Vec/IndexMap container
        // allocs to after the fast-path check — when the builtin returns a pre-materialized
        // thunk, the originals are simply dropped with no restore clone needed.
        let builtin_args = crate::value::BuiltinArgs {
            args: args.as_ref().expect("args set above").clone(),
            named: named.as_ref().expect("named set above").clone(),
            call_span,
            ctx: Arc::clone(&thunk_ctx),
        };

        match (def.func)(builtin_args).await {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion.
                // Originals in args/named are dropped here — no restore clone needed.
                if let Some(value) = result_thunk.try_get_materialized() {
                    // eval_stack_guard pops on drop (armed)
                    thunk.set_materialized(value.clone());
                    Action::Continue(Ok(value))
                } else {
                    // Slow path: move originals into the Memoize restore payload.
                    // Arc<Thunk> clones are cheap (atomic ref count only); the Vec/IndexMap
                    // containers are moved here (not cloned) — no extra alloc on this path.
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span,
                        mat_span,
                        restore: Some(RestoreState::PendingBuiltin {
                            def,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
                            call_span,
                            ctx: Arc::clone(&thunk_ctx),
                        }),
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
                // Restore to PendingBuiltin for non-cacheable errors (e.g. DepthExceeded) so
                // the thunk can be retried. Cache as Failed only for cacheable errors.
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure_once(&decorated);
                } else {
                    thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
                        def,
                        args: args.take().expect("args set above"),
                        named: named.take().expect("named set above"),
                        call_span,
                        ctx: thunk_ctx,
                    });
                }
                Action::Continue(Err(decorated))
            }
        }
    } else if let Some((func_thunk, args, named, call_span, caller_env, thunk_ctx, original_call)) =
        thunk.take_pending_call()
    {
        // TCO eligibility check: If Arc::strong_count == 1, nobody else holds this thunk.
        // Memoization is unnecessary, so we can skip the Memoize continuation push.
        // This achieves O(1) tail-call optimization by reusing the current frame.
        //
        // Race condition safety: Arc::strong_count() and take_pending_call() are both
        // synchronous (no .await between them). In tokio's LocalSet (cooperative,
        // single-threaded), the count is stable across this check.
        let tail_hint = Arc::strong_count(thunk) == 1;

        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
        // PendingCallDispatch continuation inherits eval_stack pop responsibility.
        // TCO: When tail_hint=true, eval_stack guard drops without disarm (no Memoize pushed).
        let eval_stack_guard = EvalStackGuard::push(
            &thunk_ctx.state,
            (
                origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                thunk_span,
            ),
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
                original_call,
                tail_hint,
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
        // TODO(future): pre-lower Surface thunks at creation time (store as CoreExpr) to avoid
        // re-lowering on each DepthExceeded retry.
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

        // TODO(future): eval_core_expr may recurse for complex CoreExpr variants (Sequential,
        // Match) — those need their own CEK continuation variants to be fully iterative.
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

        // Handle CoreExpr::Sequential inline — prevents loop through eval_core_expr.
        // The CEK machine evaluates expressions iteratively via SequentialStep continuations.
        if let crate::ast::CoreExpr::Sequential(exprs) = &core_expr.node {
            if exprs.is_empty() {
                // Empty sequential: return empty dict
                thunk.set_materialized(Value::Dict(IndexMap::new()));
                return Action::Continue(Ok(Value::Dict(IndexMap::new())));
            }

            // Memoize the final result
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

            // Evaluate the first expression and push a SequentialStep to handle the result
            let first_expr = &exprs[0];
            stack.push(Cont::SequentialStep(Box::new(
                crate::eval_materialize::SequentialStepData {
                    idx: 0,
                    exprs: Arc::new(exprs.clone()),
                    env: Arc::clone(&env),
                    ctx: Arc::clone(&thunk_ctx),
                    seq_span: core_expr.span,
                },
            )));

            // Evaluate the first expression
            return Action::EvalCore {
                expr: Arc::clone(first_expr),
                env,
                ctx: thunk_ctx,
            };
        }

        // Handle CoreExpr::Match inline — prevents loop through eval_core_expr.
        // The CEK machine evaluates arms iteratively via MatchDispatch continuations.
        if let crate::ast::CoreExpr::Match { scrutinee, arms } = &core_expr.node {
            // Evaluate the scrutinee first
            let scrutinee_thunk = match eval_core_expr_pub(scrutinee, &env, &thunk_ctx).await {
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

            // Push Memoize to cache the final match result
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

            // Push MatchDispatch to try arms after scrutinee is materialized
            stack.push(Cont::MatchDispatch(Box::new(
                crate::eval_materialize::MatchDispatchData {
                    arm_idx: 0,
                    arms: Arc::new(arms.clone()),
                    env: Arc::clone(&env),
                    ctx: Arc::clone(&thunk_ctx),
                    match_span: core_expr.span,
                },
            )));

            // Materialize the scrutinee
            return Action::Materialize {
                thunk: scrutinee_thunk,
                mat_span: Some(scrutinee.span),
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
                    // restore is Some for all Memoize push sites. GuardedValidate
                    // default-fallback paths build a fresh RestoreState::Guarded rather than
                    // consuming the original via take(), so Memoize always receives Some(restore)
                    // when the default expression hits a non-cacheable error (e.g., DepthExceeded).
                    // PendingBuiltin and PendingCall paths always provide Some(restore).
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
                original_call,
                tail_hint,
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
                        // TCO path: when tail_hint=true, skip Memoize and return EvalCore directly.
                        if tail_hint {
                            let invoke_result = {
                                let call_ctx = CallContext {
                                    params: &params,
                                    body: &body,
                                    closure_env: &env,
                                    positional: args.as_deref().expect("args set above"),
                                    named: named.as_ref().expect("named set above").as_deref(),
                                    default_env: &caller_env,
                                    call_span,
                                    origin: origin.clone(),
                                    ctx: &thunk_ctx,
                                };
                                invoke_function_tco(&call_ctx).await
                            };

                            match invoke_result.map_err(&decorate) {
                                Ok((body_expr, new_env)) => {
                                    // TCO: No Memoize push. The outer thunk's result will be
                                    // set by whatever the body evaluates to. The eval_stack
                                    // guard drops naturally (armed), maintaining the stack frame.
                                    Action::EvalCore {
                                        expr: body_expr,
                                        env: new_env,
                                        ctx: thunk_ctx,
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
                                                original_call: original_call.clone(),
                                            },
                                        );
                                    }
                                    Action::Continue(Err(e))
                                }
                            }
                        } else {
                            // Non-TCO path: create thunk and push Memoize continuation.
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
                                    let restore = RestoreState::CoreExpr {
                                        expr: original_call.clone(),
                                        env: caller_env,
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
                                                original_call: original_call.clone(),
                                            },
                                        );
                                    }
                                    Action::Continue(Err(e))
                                }
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
                                    if tail_hint {
                                        // TCO path: Don't set this thunk — it's being abandoned.
                                        // Result goes directly to the caller's Memoize (or top-level return).
                                        Action::Continue(Ok(value))
                                    } else {
                                        // Non-TCO path: set this thunk and return.
                                        thunk.set_materialized(value.clone());
                                        Action::Continue(Ok(value))
                                    }
                                } else if tail_hint {
                                    // TCO path: Skip Memoize push, return Materialize directly.
                                    // The result_thunk will itself be TCO-eligible on next force_step
                                    // if its strong_count == 1.
                                    // eval_stack_guard pops on drop (armed)
                                    Action::Materialize {
                                        thunk: result_thunk,
                                        mat_span,
                                    }
                                } else {
                                    // Non-TCO path: push Memoize continuation.
                                    let restore = RestoreState::CoreExpr {
                                        expr: original_call.clone(),
                                        env: caller_env,
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
                                            original_call: original_call.clone(),
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
                                original_call: original_call.clone(),
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
                            original_call: original_call.clone(),
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
                                        //
                                        // Build a fresh RestoreState for Memoize rather than
                                        // consuming `restore` via take(). If the default expression
                                        // hits DepthExceeded, Memoize must be able to restore the
                                        // thunk to Guarded state — including the original default
                                        // so a retry at shallower depth can attempt it again.
                                        // Consuming restore here would leave Memoize with None on a
                                        // second call (if restore was already taken by another path).
                                        let memoize_restore = if let Some(RestoreState::Guarded {
                                            ref inner,
                                            ..
                                        }) = restore
                                        {
                                            Some(RestoreState::Guarded {
                                                inner: Arc::clone(inner),
                                                expected: expected.clone(),
                                                field_path: field_path.clone(),
                                                guard_span,
                                                blame_label: blame_label.clone(),
                                                default: Some((
                                                    Arc::clone(&default_expr),
                                                    Arc::clone(&default_env),
                                                )),
                                            })
                                        } else {
                                            None
                                        };
                                        let guard_eval_stack = EvalStackGuard::push(
                                            &guard_ctx.state,
                                            (
                                                origin
                                                    .clone()
                                                    .unwrap_or_else(|| Arc::from("thunk")),
                                                thunk_span,
                                            ),
                                        );
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin: Some(Arc::from("default fallback")),
                                            thunk_span,
                                            mat_span,
                                            restore: memoize_restore,
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
                                // Build a fresh RestoreState rather than consuming restore
                                // (see comment at the first default-fallback site above).
                                let memoize_restore = if let Some(RestoreState::Guarded {
                                    ref inner,
                                    ..
                                }) = restore
                                {
                                    Some(RestoreState::Guarded {
                                        inner: Arc::clone(inner),
                                        expected: expected.clone(),
                                        field_path: field_path.clone(),
                                        guard_span,
                                        blame_label: blame_label.clone(),
                                        default: Some((
                                            Arc::clone(&default_expr),
                                            Arc::clone(&default_env),
                                        )),
                                    })
                                } else {
                                    None
                                };
                                let guard_eval_stack = EvalStackGuard::push(
                                    &guard_ctx.state,
                                    (
                                        origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                                        thunk_span,
                                    ),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span,
                                    mat_span,
                                    restore: memoize_restore,
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
                                // Build a fresh RestoreState rather than consuming restore
                                // (see comment at the first default-fallback site above).
                                let memoize_restore = if let Some(RestoreState::Guarded {
                                    ref inner,
                                    ..
                                }) = restore
                                {
                                    Some(RestoreState::Guarded {
                                        inner: Arc::clone(inner),
                                        expected: expected.clone(),
                                        field_path: field_path.clone(),
                                        guard_span,
                                        blame_label: blame_label.clone(),
                                        default: Some((
                                            Arc::clone(&default_expr),
                                            Arc::clone(&default_env),
                                        )),
                                    })
                                } else {
                                    None
                                };
                                let guard_eval_stack = EvalStackGuard::push(
                                    &guard_ctx.state,
                                    (
                                        origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                                        thunk_span,
                                    ),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span,
                                    mat_span,
                                    restore: memoize_restore,
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
                    // Clone args/named for the builtin call; keep originals in the Option
                    // slots for restore on the slow path or on non-cacheable errors.
                    // This defers Vec/IndexMap allocs to after the fast-path check.
                    let builtin_args = crate::value::BuiltinArgs {
                        args: args.as_ref().expect("args set above").clone(),
                        named: named.as_ref().expect("named set above").clone(),
                        call_span,
                        ctx: Arc::clone(&thunk_ctx),
                    };
                    match (def.func)(builtin_args).await.map_err(&decorate) {
                        Ok(result_thunk) => {
                            // Fast path: originals dropped here — no restore clone needed.
                            if let Some(value) = result_thunk.try_get_materialized() {
                                // eval_stack_guard pops on drop (armed)
                                thunk.set_materialized(value.clone());
                                Action::Continue(Ok(value))
                            } else {
                                // Slow path: move originals into the Memoize restore payload.
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin,
                                    thunk_span,
                                    mat_span,
                                    restore: Some(RestoreState::PendingBuiltin {
                                        def,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span,
                                        ctx: Arc::clone(&thunk_ctx),
                                    }),
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
                            // Restore to PendingBuiltin for non-cacheable errors (e.g. DepthExceeded).
                            if e.kind.is_cacheable() {
                                thunk.cache_failure_once(&e);
                            } else {
                                thunk.restore_unevaluated(
                                    crate::value::UnevaluatedState::Builtin {
                                        def,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span,
                                        ctx: thunk_ctx,
                                    },
                                );
                            }
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
                                    // Apply boundary guards if this access site has a guard registered.
                                    let thunk = maybe_wrap_guard(thunk, access_span, &ctx);
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
                                    // Apply boundary guards if this access site has a guard registered.
                                    let thunk = maybe_wrap_guard(thunk, access_span, &ctx);
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
                            // Apply boundary guards if this access site has a guard registered.
                            let thunk = maybe_wrap_guard(thunk, access_span, &ctx);
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
                            // Apply boundary guards if this access site has a guard registered.
                            let thunk = maybe_wrap_guard(thunk, access_span, &ctx);
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
                            // Apply boundary guards if this access site has a guard registered.
                            let thunk = maybe_wrap_guard(thunk, access_span, &ctx);
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
                                            Arc::new(crate::lower::lower(
                                                node,
                                                crate::ast::empty_resolution_table(),
                                                crate::ast::empty_type_annotation_table(),
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
                                        expr: Arc::new(crate::lower::lower(
                                            default_node,
                                            crate::ast::empty_resolution_table(),
                                            crate::ast::empty_type_annotation_table(),
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
                            // Type matches — check if there's an is: predicate to evaluate
                            let is_predicate =
                                annotation.node.get_property(IS_ANNOTATION_KEY).cloned();
                            if let Some(predicate_node) = is_predicate {
                                // Push a PredicateCheck continuation to handle the predicate result
                                stack.push(Cont::PredicateCheck(Box::new(PredicateCheckData {
                                    value: value.clone(),
                                    annotation,
                                    expr_span,
                                    thunk_span,
                                    env: Arc::clone(&env),
                                    ctx: Arc::clone(&ctx),
                                })));
                                // Evaluate the predicate expression
                                Action::EvalCore {
                                    expr: Arc::new(crate::lower::lower(
                                        &predicate_node,
                                        crate::ast::empty_resolution_table(),
                                        crate::ast::empty_type_annotation_table(),
                                    )),
                                    env,
                                    ctx: Arc::clone(&ctx),
                                }
                            } else {
                                // No is: predicate — type check passed, return value
                                Action::Continue(Ok(value))
                            }
                        } else if let Some(default_node) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively.
                            // The result will flow to the next continuation on the stack.
                            Action::EvalCore {
                                expr: Arc::new(crate::lower::lower(
                                    default_node,
                                    crate::ast::empty_resolution_table(),
                                    crate::ast::empty_type_annotation_table(),
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
                                        expr: Arc::new(crate::lower::lower(
                                            default_node,
                                            crate::ast::empty_resolution_table(),
                                            crate::ast::empty_type_annotation_table(),
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
                                        expr: Arc::new(crate::lower::lower(
                                            default_node,
                                            crate::ast::empty_resolution_table(),
                                            crate::ast::empty_type_annotation_table(),
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
        Cont::SequentialStep(data) => {
            let SequentialStepData {
                idx,
                exprs,
                env,
                ctx,
                seq_span,
            } = *data;

            // Result is the materialized value from the previous expression
            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(intermediate_value) => {
                    // Process the value to extract bindings if needed
                    let next_idx = idx + 1;
                    if next_idx >= exprs.len() {
                        // This was the last expression — return its value
                        return Action::Continue(Ok(intermediate_value));
                    }

                    // Extract static keys from the CURRENT expression (at idx) for scope creation
                    let current_expr = &exprs[idx];
                    let static_keys: Option<HashSet<String>> = match &current_expr.node {
                        CoreExpr::Dict(entries) => {
                            use crate::eval::core_expr_is_static_key;
                            let keys: Vec<String> = entries
                                .iter()
                                .filter_map(|entry| {
                                    entry.node.key.as_ref().and_then(|k| {
                                        if core_expr_is_static_key(&k.node) {
                                            match &k.node {
                                                CoreExpr::Str(s) => Some(s.clone()),
                                                CoreExpr::Annotated { name, .. } => {
                                                    Some(name.clone())
                                                }
                                                _ => None,
                                            }
                                        } else {
                                            None
                                        }
                                    })
                                })
                                .collect();
                            if keys.is_empty() {
                                None
                            } else {
                                Some(keys.into_iter().collect())
                            }
                        }
                        _ => None,
                    };

                    // Build the SequentialStepData for the NEXT expression.
                    // This is shared regardless of whether we have entries to force.
                    let next_step = Box::new(SequentialStepData {
                        idx: next_idx,
                        exprs: Arc::clone(&exprs),
                        // env will be updated to child_env below if we have static keys
                        env: Arc::clone(&env),
                        ctx: Arc::clone(&ctx),
                        seq_span,
                    });

                    if let Some(ref static_key_set) = static_keys {
                        // Flatten Overlay to Dict for scope chain binding
                        let map = match intermediate_value {
                            Value::Dict(map) => map,
                            Value::Overlay(l, r) => match crate::builtins::flatten_overlay(
                                &l,
                                &r,
                                "sequential expression",
                                &ctx,
                                current_expr.span,
                            ) {
                                Ok(map) => map,
                                Err(e) => return Action::Continue(Err(e)),
                            },
                            _ => {
                                return Action::Continue(Err(Box::new(
                                    EvalError::type_mismatch_ctx(
                                        format!("sequential expression #{}", idx + 1),
                                        "Dict",
                                        intermediate_value.type_name(),
                                        current_expr.span,
                                    ),
                                )));
                            }
                        };

                        // Collect all static-key entries that need to be forced.
                        // We process them in order: first entry is forced immediately,
                        // remaining entries are processed by chained ForceAndBind continuations.
                        let child_env =
                            Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

                        let mut entries_to_force: Vec<(String, Arc<Thunk>)> = map
                            .into_iter()
                            .filter_map(|(key, thunk_id)| {
                                if let Key::String(name) = key {
                                    if static_key_set.contains(name.as_ref()) {
                                        let val_thunk = ctx.get_thunk(thunk_id);
                                        return Some((name.to_string(), val_thunk));
                                    }
                                }
                                None
                            })
                            .collect();

                        if entries_to_force.is_empty() {
                            // No entries to force — push SequentialStep directly with child_env
                            // (child_env is empty but still establishes a child scope).
                            let next_expr = &exprs[next_idx];
                            stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                                env: Arc::clone(&child_env),
                                ..*next_step
                            })));
                            Action::EvalCore {
                                expr: Arc::clone(next_expr),
                                env: child_env,
                                ctx,
                            }
                        } else {
                            // Force entries one at a time via ForceAndBind continuations.
                            // Pop the first entry; the rest become `remaining` in ForceAndBind.
                            let (first_name, first_thunk) = entries_to_force.remove(0);
                            let first_span = first_thunk.span;

                            // The step.env field is a placeholder (parent env) that is never
                            // read — ForceAndBind always reconstructs SequentialStepData with
                            // child_env once all entries are bound.
                            stack.push(Cont::ForceAndBind(Box::new(ForceAndBindData {
                                name: first_name,
                                value_span: first_span,
                                remaining: entries_to_force,
                                child_env,
                                step: next_step,
                            })));

                            Action::Materialize {
                                thunk: first_thunk,
                                mat_span: Some(current_expr.span),
                            }
                        }
                    } else {
                        // No static keys — no scope created, continue with same env.
                        let next_expr = &exprs[next_idx];
                        stack.push(Cont::SequentialStep(next_step));
                        Action::EvalCore {
                            expr: Arc::clone(next_expr),
                            env,
                            ctx,
                        }
                    }
                }
            }
        }
        Cont::ForceAndBind(data) => {
            let ForceAndBindData {
                name,
                value_span,
                remaining,
                child_env,
                step,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(forced_value) => {
                    // Insert the forced value as a materialized thunk into child_env.
                    let materialized_thunk =
                        Arc::new(Thunk::new_materialized(forced_value, value_span));
                    child_env.write().unwrap().insert(name, materialized_thunk);

                    if remaining.is_empty() {
                        // All entries have been forced and bound — now evaluate the next
                        // sequential expression with the fully-populated child_env.
                        let next_idx = step.idx;
                        let next_expr = Arc::clone(&step.exprs[next_idx]);
                        let step_ctx = Arc::clone(&step.ctx);
                        let seq_span = step.seq_span;
                        let exprs = Arc::clone(&step.exprs);
                        stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                            env: Arc::clone(&child_env),
                            idx: next_idx,
                            exprs,
                            ctx: Arc::clone(&step_ctx),
                            seq_span,
                        })));
                        Action::EvalCore {
                            expr: next_expr,
                            env: child_env,
                            ctx: step_ctx,
                        }
                    } else {
                        // More entries remain — force the next one.
                        let mut remaining = remaining;
                        let (next_name, next_thunk) = remaining.remove(0);
                        let next_span = next_thunk.span;

                        stack.push(Cont::ForceAndBind(Box::new(ForceAndBindData {
                            name: next_name,
                            value_span: next_span,
                            remaining,
                            child_env,
                            step,
                        })));

                        Action::Materialize {
                            thunk: next_thunk,
                            mat_span: None,
                        }
                    }
                }
            }
        }
        Cont::MatchDispatch(data) => {
            let MatchDispatchData {
                arm_idx,
                arms,
                env,
                ctx,
                match_span,
            } = *data;

            // Result is the materialized scrutinee value
            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(scrutinee_value) => {
                    // Try each arm starting from arm_idx
                    for i in arm_idx..arms.len() {
                        let arm = &arms[i];

                        // Try the pattern (this is async, so we need to spawn a sub-action)
                        // For now, we'll block on pattern matching since it's typically fast
                        // TODO: Make pattern matching iterative too
                        let matched_env_result = crate::async_rt::block_on_anywhere(match_pattern(
                            &arm.pattern.node,
                            &scrutinee_value,
                            &env,
                            &arm.pattern.span,
                            &ctx,
                        ));

                        let matched_env = match matched_env_result {
                            Ok(opt) => opt,
                            Err(e) => return Action::Continue(Err(e)),
                        };

                        if let Some(arm_env) = matched_env {
                            // Pattern matched. If there is a guard, evaluate it.
                            if let Some(guard_expr) = &arm.guard {
                                // Push a continuation to check the guard result
                                stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                    arm_idx: i,
                                    arms: Arc::clone(&arms),
                                    env: Arc::clone(&env),
                                    ctx: Arc::clone(&ctx),
                                    match_span,
                                    arm_env: Arc::clone(&arm_env),
                                    scrutinee_value: scrutinee_value.clone(),
                                    body: Arc::clone(&arm.body),
                                })));

                                // Evaluate the guard
                                return Action::EvalCore {
                                    expr: Arc::clone(guard_expr),
                                    env: arm_env,
                                    ctx,
                                };
                            }

                            // No guard — arm matched, evaluate body
                            return Action::EvalCore {
                                expr: Arc::clone(&arm.body),
                                env: arm_env,
                                ctx,
                            };
                        }
                        // Pattern did not match — continue to next arm
                    }

                    // No arm matched: non-exhaustive match
                    Action::Continue(Err(Box::new(EvalError::match_exhaustion(
                        scrutinee_value.type_name(),
                        match_span,
                    ))))
                }
            }
        }
        Cont::MatchGuardCheck(data) => {
            let MatchGuardCheckData {
                arm_idx,
                arms,
                env,
                ctx,
                match_span,
                arm_env,
                scrutinee_value,
                body,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(guard_value) => {
                    // PM1: If the guard is callable, invoke it with the scrutinee
                    let guard_value = match guard_value {
                        Value::Function { .. } | Value::Builtin(_) => {
                            // Create a thunk for the scrutinee
                            let scrutinee_thunk = Arc::new(Thunk::new_materialized(
                                scrutinee_value.clone(),
                                match_span,
                            ));
                            // Create a thunk for the predicate
                            let pred_thunk =
                                Arc::new(Thunk::new_materialized(guard_value, match_span));
                            // Create a PendingCall thunk
                            let call_thunk = Arc::new(Thunk::new_pending_call(
                                pred_thunk,
                                vec![scrutinee_thunk],
                                IndexMap::new(),
                                match_span,
                                Arc::clone(&arm_env),
                                match_span,
                                None,
                                Arc::clone(&ctx),
                                Arc::new(Spanned {
                                    node: CoreExpr::Int(0),
                                    span: match_span,
                                }),
                            ));
                            // Force the call
                            match crate::async_rt::block_on_anywhere(materialize(
                                &call_thunk,
                                Some(&match_span),
                                &ctx,
                            )) {
                                Ok(v) => v,
                                Err(e) => return Action::Continue(Err(e)),
                            }
                        }
                        other => other,
                    };

                    // Check if the guard is truthy
                    let is_truthy = match &guard_value {
                        Value::Bool(b) => *b,
                        Value::Dict(map) => !map.is_empty(),
                        _ => true,
                    };

                    if is_truthy {
                        // Guard passed — evaluate the body
                        Action::EvalCore {
                            expr: body,
                            env: arm_env,
                            ctx,
                        }
                    } else {
                        // Guard failed — try the next arm
                        stack.push(Cont::MatchDispatch(Box::new(MatchDispatchData {
                            arm_idx: arm_idx + 1,
                            arms,
                            env,
                            ctx: Arc::clone(&ctx),
                            match_span,
                        })));
                        Action::Continue(Ok(scrutinee_value))
                    }
                }
            }
        }

        Cont::PredicateCheck(data) => {
            let PredicateCheckData {
                value,
                annotation,
                expr_span,
                thunk_span,
                env,
                ctx,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(predicate_value) => {
                    // If the predicate is callable, invoke it with the value as argument
                    // (mirroring match guard logic at MatchGuardCheck)
                    let predicate_result_value = match predicate_value {
                        Value::Function { .. } | Value::Builtin(_) => {
                            // Create a thunk for the value
                            let value_thunk =
                                Arc::new(Thunk::new_materialized(value.clone(), thunk_span));
                            // Create a thunk for the predicate
                            let pred_thunk =
                                Arc::new(Thunk::new_materialized(predicate_value, expr_span));
                            // Create a PendingCall thunk
                            let call_thunk = Arc::new(Thunk::new_pending_call(
                                pred_thunk,
                                vec![value_thunk],
                                IndexMap::new(),
                                expr_span,
                                Arc::clone(&env),
                                expr_span,
                                None,
                                Arc::clone(&ctx),
                                Arc::new(Spanned {
                                    node: CoreExpr::Int(0),
                                    span: expr_span,
                                }),
                            ));
                            // Force the call
                            match crate::async_rt::block_on_anywhere(materialize(
                                &call_thunk,
                                Some(&expr_span),
                                &ctx,
                            )) {
                                Ok(v) => v,
                                Err(e) => return Action::Continue(Err(e)),
                            }
                        }
                        other => other,
                    };

                    // Check if the predicate result is truthy
                    let is_truthy = match &predicate_result_value {
                        Value::Bool(b) => *b,
                        Value::Dict(map) => !map.is_empty(),
                        _ => true,
                    };

                    if is_truthy {
                        // Predicate passed — return the original value
                        Action::Continue(Ok(value))
                    } else {
                        // Predicate failed — check for default: or fail
                        if let Some(default_node) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively
                            Action::EvalCore {
                                expr: Arc::new(crate::lower::lower(
                                    default_node,
                                    crate::ast::empty_resolution_table(),
                                    crate::ast::empty_type_annotation_table(),
                                )),
                                env,
                                ctx: Arc::clone(&ctx),
                            }
                        } else {
                            // No default — fail with predicate failed error
                            let mut err = EvalError::type_assert_failed(
                                "_ (is: predicate failed)",
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
                }
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
    fn test_restore_state_core_expr() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        let original_call = Arc::new(sp(CoreExpr::Call {
            func: Arc::new(sp(CoreExpr::Int(42))),
            args: vec![],
            named_args: vec![],
            implied: false,
        }));
        let caller_env = empty_env();

        // Create a PendingCall thunk, then take it to InProgress
        let func_thunk = Arc::new(Thunk::new_materialized(Value::Int(1), span));
        let thunk = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            span,
            empty_env(),
            span,
            None,
            Arc::clone(&ctx),
            Arc::clone(&original_call),
        ));
        let _ = thunk.take_pending_call();
        assert!(thunk.is_in_progress());

        let restore = RestoreState::CoreExpr {
            expr: Arc::clone(&original_call),
            env: Arc::clone(&caller_env),
            ctx: Arc::clone(&ctx),
        };
        restore.restore(&thunk);

        assert!(!thunk.is_pending_call());
        assert!(!thunk.is_in_progress());
        assert!(!thunk.is_materialized());
    }

    #[test]
    fn test_core_expr_restore_preserves_state() {
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        let original_call = Arc::new(sp(CoreExpr::Call {
            func: Arc::new(sp(CoreExpr::Int(100))),
            args: vec![
                Arc::new(sp(CoreExpr::Int(1))),
                Arc::new(sp(CoreExpr::Int(2))),
            ],
            named_args: vec![],
            implied: false,
        }));
        let caller_env = empty_env();

        let func_thunk = Arc::new(Thunk::new_materialized(Value::Int(1), span));
        let thunk = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            span,
            empty_env(),
            span,
            None,
            Arc::clone(&ctx),
            Arc::clone(&original_call),
        ));
        let _ = thunk.take_pending_call();
        assert!(thunk.is_in_progress());

        let restore = RestoreState::CoreExpr {
            expr: Arc::clone(&original_call),
            env: Arc::clone(&caller_env),
            ctx: Arc::clone(&ctx),
        };
        restore.restore(&thunk);

        // Verify state is restored to CoreExpr (NOT Call)
        assert!(
            !thunk.is_pending_call(),
            "Expected CoreExpr state, not Call state"
        );
        assert!(
            !thunk.is_in_progress(),
            "Thunk should not be InProgress after restore"
        );
        assert!(
            !thunk.is_materialized(),
            "Thunk should not be materialized after restore"
        );

        // Verify take_pending_call returns None (because it's CoreExpr, not Call)
        let taken = thunk.take_pending_call();
        assert!(
            taken.is_none(),
            "take_pending_call should return None for CoreExpr state"
        );

        // The CoreExpr restoration is correct: the entire Call expression (with args)
        // is preserved in the expr field. When re-evaluated, eval_core_expr will
        // process the Call expression and create fresh arg thunks from the CoreExpr.
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

#[cfg(test)]
mod deep_tests {
    use super::*;
    use crate::test_util::test_span;

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let stdlib_env = crate::builtins::create_stdlib_env().expect("stdlib failed");
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
        EvalContext::new(base_dir, stdlib_env, type_stage_env, false)
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
                        builtin_def,
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
                            *def,
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

// ============================================================================
// Utility functions for macro expansion
// ============================================================================

/// Recursively materialize all dict values and variant payloads in a value tree.
/// Used to ensure macro expansion results are fully materialized before conversion
/// back to AST via `dict_to_surface_node`, which expects all nested values to be
/// pre-materialized (uses `try_get_materialized`).
///
/// Unlike `deep_materialize` from earlier runtime versions, this function:
/// - Only forces Dict values and Variant payloads (not Seq elements or other types)
/// - Does NOT preserve sharing (may duplicate shared structures)
/// - Uses cycle detection to avoid infinite loops
///
/// If a non-Dict/Variant value is encountered at any level (Int, String, Function, etc.),
/// the value is returned as-is without further recursion.
///
/// Sharing preservation is not guaranteed (may duplicate shared structures).
///
/// Exported for use by:
/// - `expand_macro_call_surface` in expand.rs (fallback path for Dict/Variant macro results)
/// - `builtin_variant` in builtins_meta.rs (deep-materialize AST variant payloads)
pub(crate) async fn force_dict_tree(val: &Value, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
    force_dict_tree_impl(val, ctx, &mut HashSet::new()).await
}

fn force_dict_tree_impl<'a>(
    val: &'a Value,
    ctx: &'a Arc<EvalContext>,
    visited: &'a mut HashSet<*const Thunk>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Value>> + 'a>> {
    Box::pin(async move {
        match val {
            Value::Dict(map) => {
                let mut new_map = IndexMap::new();
                for (key, thunk_id) in map {
                    let thunk = ctx.get_thunk(*thunk_id);
                    let thunk_ptr = Arc::as_ptr(&thunk);

                    // Cycle detection: if we've already visited this thunk, return it as-is
                    if !visited.insert(thunk_ptr) {
                        // Cycle detected — stop recursing
                        new_map.insert(key.clone(), *thunk_id);
                        continue;
                    }

                    let forced_val = materialize(&thunk, None, ctx).await?;
                    let deep_val = force_dict_tree_impl(&forced_val, ctx, visited).await?;
                    let deep_thunk = Arc::new(Thunk::new_materialized(deep_val, thunk.span));
                    let deep_id = ctx.alloc_thunk(deep_thunk);
                    new_map.insert(key.clone(), deep_id);
                }
                Ok(Value::Dict(new_map))
            }
            Value::Variant { tag, payload } => {
                if let Some(payload_id) = payload {
                    let payload_thunk = ctx.get_thunk(*payload_id);
                    let thunk_ptr = Arc::as_ptr(&payload_thunk);

                    // Cycle detection: if we've already visited this thunk, return variant as-is
                    if !visited.insert(thunk_ptr) {
                        return Ok(val.clone());
                    }

                    let forced_payload = materialize(&payload_thunk, None, ctx).await?;
                    let deep_payload = force_dict_tree_impl(&forced_payload, ctx, visited).await?;
                    let deep_thunk =
                        Arc::new(Thunk::new_materialized(deep_payload, payload_thunk.span));
                    let deep_id = ctx.alloc_thunk(deep_thunk);
                    Ok(Value::Variant {
                        tag: tag.clone(),
                        payload: Some(deep_id),
                    })
                } else {
                    Ok(val.clone())
                }
            }
            Value::Seq { head, tail } => {
                let head_thunk = ctx.get_thunk(*head);
                let head_ptr = Arc::as_ptr(&head_thunk);

                let new_head = if !visited.insert(head_ptr) {
                    // Cycle in head — keep original thunk
                    *head
                } else {
                    let forced_head = materialize(&head_thunk, None, ctx).await?;
                    let deep_head = force_dict_tree_impl(&forced_head, ctx, visited).await?;
                    let deep_thunk = Arc::new(Thunk::new_materialized(deep_head, head_thunk.span));
                    ctx.alloc_thunk(deep_thunk)
                };

                let tail_thunk = ctx.get_thunk(*tail);
                let tail_ptr = Arc::as_ptr(&tail_thunk);

                let new_tail = if !visited.insert(tail_ptr) {
                    // Cycle in tail — keep original thunk
                    *tail
                } else {
                    let forced_tail = materialize(&tail_thunk, None, ctx).await?;
                    let deep_tail = force_dict_tree_impl(&forced_tail, ctx, visited).await?;
                    let deep_thunk = Arc::new(Thunk::new_materialized(deep_tail, tail_thunk.span));
                    ctx.alloc_thunk(deep_thunk)
                };

                Ok(Value::Seq {
                    head: new_head,
                    tail: new_tail,
                })
            }
            Value::Overlay(left, right) => {
                // Flatten the overlay to a dict, then recurse on the result
                let flattened_map = flatten_overlay(
                    left,
                    right,
                    "force_dict_tree",
                    ctx,
                    crate::ast::Span::origin(),
                )?;
                let dict_val = Value::Dict(flattened_map);
                force_dict_tree_impl(&dict_val, ctx, visited).await
            }
            // Explicit passthrough for Expression values — these are already fully formed AST nodes
            Value::Expression(_) => Ok(val.clone()),
            // Primitives and other types are already fully materialized
            // Includes: Int, Float, Bool, String, Function, Builtin, DirCap, NetCap, Handle,
            // WriteHandle, RevocableDirCap, Decimal, BigInt, Bytes, Uri, Timestamp, Duration,
            // ClockCap, Timezone, QuicSession, Http2Session, Http3Session, QuicDatagramHandle,
            // DatagramHandle, Program, Document, Builder, Proxy
            _ => Ok(val.clone()),
        }
    })
}
