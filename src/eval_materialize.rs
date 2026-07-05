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

use crate::ast::{Annotation, CoreExpr, Span, Spanned};
use crate::builtins::flatten_overlay;
use crate::error::{EvalError, EvalResult};
use crate::eval::{
    as_record_row_merged, format_field_path, format_type_for_assert, match_pattern, materialize,
    primitive_eq, validate_and_wrap_record, value_matches_type, EvalContext,
    DEFAULT_ANNOTATION_KEY, IS_ANNOTATION_KEY,
};
use crate::eval_call::{invoke_function, invoke_function_tco, CallContext};
use crate::eval_core::eval_core_expr;
use crate::rust_span;
use crate::types::Type;
use crate::value::{string_val, Environment, HashableValue, Thunk, ThunkId, Value};

/// RAII guard for profiling spans. Automatically closes the span on drop.
struct ProfilingSpanGuard {
    profiling: Option<Arc<Mutex<crate::profiling::ProfilingCollector>>>,
    span_id: Option<u64>,
}

impl ProfilingSpanGuard {
    fn new(ctx: &Arc<EvalContext>, thunk: &Thunk) -> Self {
        let (profiling, span_id) = if let Some(ref prof) = ctx.profiling {
            // Extract span source information.
            // Span carries file: Option<Arc<SourceFile>> populated by parse_with_file.
            // Use the embedded path when present; None for synthetic/builtin spans.
            let source_file: Option<String> = thunk
                .span
                .file
                .as_ref()
                .map(|sf| sf.path.as_ref().to_string());
            let (source_start, source_end) = if thunk.span != rust_span!() {
                (
                    Some((thunk.span.start.line, thunk.span.start.column)),
                    Some((thunk.span.end.line, thunk.span.end.column)),
                )
            } else {
                (None, None)
            };

            // Extract source text snippet (TODO(eval-cleanup): from include cache)
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
            err.materialization_span = Some(span.clone());
        } else if err.materialization_span != Some(span.clone())
            && !err.stack.iter().any(|f| f.definition_span == *span)
        {
            // Only push a frame if the span differs from the existing
            // materialization span and isn't already in the stack (avoids
            // duplicate frames when the same span propagates through
            // nested materialize calls).
            err.push_frame("materialized".to_string(), span.clone());
        }
    }
    if let Some(label) = origin {
        if !err
            .stack
            .iter()
            .any(|f| f.definition_span == thunk_span && f.label == label)
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
        caller_env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
    /// Restore a Call (PendingCall) thunk for non-cacheable errors.
    /// Captures the deferred function call state so it can be retried.
    Call {
        func: Arc<Thunk>,
        args: Vec<Arc<Thunk>>,
        named: Option<Box<IndexMap<String, Arc<Thunk>>>>,
        call_span: Span,
        caller_env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
        original_call: Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
    },
    Guarded {
        inner: Arc<Thunk>,
        expected: Type,
        field_path: Vec<String>,
        guard_span: Span,
        blame_label: Option<crate::error::BlameLabel>,
        default: Option<GuardDefault>,
    },
    /// Restore a Surface thunk for non-cacheable errors.
    /// Holds the raw SurfaceNode so the thunk can be re-lowered on retry.
    /// All cross-phase data is stored inline on AST nodes.
    Surface {
        node: std::sync::Arc<crate::ast::SurfaceNode>,
        env: Arc<RwLock<Environment>>,
        ctx: Arc<EvalContext>,
    },
    /// Restore a CoreExpr thunk for non-cacheable errors.
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
                caller_env,
                ctx,
            } => UnevaluatedState::Builtin {
                def,
                args,
                named,
                call_span,
                caller_env,
                ctx,
            },
            RestoreState::Call {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
                original_call,
            } => UnevaluatedState::Call {
                func,
                args,
                named,
                call_span,
                caller_env,
                ctx,
                original_call,
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
            RestoreState::Surface { node, env, ctx } => {
                UnevaluatedState::Surface { node, env, ctx }
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
    /// Restoration state for non-cacheable errors.
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
    pub(crate) resolved: Box<Type>,
    pub(crate) expr_span: Span,
    pub(crate) thunk_span: Span,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    /// Pipeline blame for `--- expects: @Type` contract assertions.
    /// Carried from `CoreExpr::TypeAssert::pipeline_blame` (set by `wrap_with_nominal_validation`).
    /// None for user-written `[@Type expr]` annotations.
    pub(crate) pipeline_blame: Option<crate::error::PipelineBlame>,
}

/// Payload for Cont::BuiltinForceArg. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) def: crate::value::BuiltinDef,
    pub(crate) args: Vec<Arc<Thunk>>,
    pub(crate) named: Option<IndexMap<String, Arc<Thunk>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) arg_idx: usize,
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

/// Payload for Cont::VariantUnpackForSeq. Boxed to keep the Cont enum ≤96 bytes.
///
/// After materializing a Variant's payload in a SequentialStep context, this continuation
/// unpacks the payload dict and proceeds with the sequential expression evaluation.
pub(crate) struct VariantUnpackForSeqData {
    /// Static keys extracted from the current sequential expression
    pub(crate) static_key_set: HashSet<String>,
    /// Index of the next expression to evaluate
    pub(crate) next_idx: usize,
    pub(crate) exprs: Arc<Vec<Arc<Spanned<CoreExpr>>>>,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) seq_span: Span,
    /// Span of the current expression (for error reporting)
    pub(crate) current_expr_span: Span,
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
    /// True when the guard was a callable and has already been invoked via the CEK machine.
    /// On the second entry into MatchGuardCheck, `result` is the call's return value
    /// and truthiness is checked directly without re-invoking.
    pub(crate) callable_invoked: bool,
    /// Pre-resolved Matchable instance binding name for the guard's return type.
    /// When `Some`, the evaluator uses call_to_match_resolved for direct dispatch.
    /// When `None` (type checking skipped), falls back to call_to_match dynamic dispatch.
    pub(crate) guard_matchable_binding: Option<String>,
}

/// Payload for Cont::MatchPredicateCheck (T-1140). Boxed to keep the Cont enum ≤96 bytes.
///
/// After the predicate expression evaluates to a Value::Fn or Value::Builtin,
/// `apply_predicate_to_subject` constructs a PendingCall thunk and we materialize it.
/// On the second entry (callable_invoked=true), the result is the predicate call's
/// return value, and we check truthiness.
pub(crate) struct MatchPredicateCheckData {
    /// Current arm index (for continuing to next arm if predicate fails)
    pub(crate) arm_idx: usize,
    pub(crate) arms: Arc<Vec<crate::ast::CoreMatchArm>>,
    pub(crate) env: Arc<RwLock<Environment>>,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) match_span: Span,
    /// The scrutinee value (passed as last arg to the predicate, and re-used on success)
    pub(crate) scrutinee_value: Value,
    /// The arm body to evaluate if predicate returns true
    pub(crate) body: Arc<Spanned<CoreExpr>>,
    /// True when the predicate was a callable and has already been invoked via the CEK machine.
    /// On the second entry, `result` is the call's return value; check Bool(true) directly.
    pub(crate) callable_invoked: bool,
    /// Pre-resolved Matchable instance binding name for `to-match`, set by the type checker.
    /// When `Some`, the evaluator uses this to call the correct instance without dynamic dispatch.
    /// When `None` (type checking skipped), falls back to `call_to_match` for dynamic resolution.
    pub(crate) to_match_binding: Option<String>, // extracted from MatchableBinding at eval time
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
    /// True when the predicate was a callable and has already been invoked via the CEK machine.
    /// On the second entry into PredicateCheck, `result` is the call's return value
    /// and truthiness is checked directly without re-invoking.
    pub(crate) callable_invoked: bool,
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
    /// Unpack a Variant payload for sequential expression scope binding.
    /// After materializing a Variant's payload thunk, this continuation calls require_dict
    /// to extract the payload's dict entries and proceeds with SequentialStep logic.
    VariantUnpackForSeq(Box<VariantUnpackForSeqData>),
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
    /// T-1140: Check the result of a predicate pattern evaluation in a Match arm.
    /// After materializing the predicate call result, checks if it equals Bool(true).
    /// If true, evaluates the arm body; otherwise continues to the next arm.
    MatchPredicateCheck(Box<MatchPredicateCheckData>),
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

/// Apply a predicate function to a subject value.
///
/// Wraps both `predicate` and `subject` as materialized thunks, then constructs a
/// `PendingCall` thunk that will invoke `predicate(subject)` when forced.  The
/// returned thunk is **not** yet materialized — callers must drive it through the
/// CEK machine (e.g., by emitting `Action::Materialize`).
///
/// Build a tinct span dict: `{file, start-line, start-col, end-line, end-col}`.
///
/// Used by unified error dicts across pipeline builtins (builtin-parse,
/// builtin-typecheck). The `call_span` argument is used as the creation span for the
/// materialized thunks that hold each field value.
///
/// `file` is the path string if present, or `[]` (empty dict) when no source file is known.
pub(crate) fn make_span_dict(
    span: &crate::ast::Span,
    ctx: &Arc<EvalContext>,
    call_span: &crate::ast::Span,
) -> ThunkId {
    let alloc = |v: Value| ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())));
    let mut w = indexmap::IndexMap::new();
    w.insert(
        HashableValue::Str("file".into()),
        alloc(match &span.file {
            Some(sf) => string_val(sf.path.as_ref()),
            None => Value::Dict(indexmap::IndexMap::new()),
        }),
    );
    w.insert(
        HashableValue::Str("start-line".into()),
        alloc(Value::Int(span.start.line as i64)),
    );
    w.insert(
        HashableValue::Str("start-col".into()),
        alloc(Value::Int(span.start.column as i64)),
    );
    w.insert(
        HashableValue::Str("end-line".into()),
        alloc(Value::Int(span.end.line as i64)),
    );
    w.insert(
        HashableValue::Str("end-col".into()),
        alloc(Value::Int(span.end.column as i64)),
    );
    alloc(Value::Dict(w))
}

/// Returns true if the annotation is `@Expr`, meaning the parameter receives a quoted AST
/// value instead of an evaluated argument. Delegates to `crate::ast::is_expr_annotation`.
fn is_expr_annotation(ann: &crate::ast::Annotation) -> bool {
    crate::ast::is_expr_annotation(ann)
}

/// Used by:
/// - `Cont::PredicateCheck` — `is:` predicate checks in TypeAssert annotations.
/// - Predicate match patterns (T-1140).
///
/// The helper is intentionally synchronous: it only constructs thunks, never
/// forces evaluation.
pub(crate) fn apply_predicate_to_subject(
    predicate: Value,
    subject: Value,
    pred_span: Span,
    subj_span: Span,
    env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    let subject_thunk = Arc::new(Thunk::new_materialized(subject, subj_span));
    let pred_thunk = Arc::new(Thunk::new_materialized(predicate, pred_span.clone()));
    Arc::new(Thunk::new_pending_call(
        pred_thunk,
        vec![subject_thunk],
        IndexMap::new(),
        pred_span.clone(),
        Arc::clone(env),
        pred_span.clone(),
        None,
        Arc::clone(ctx),
        Arc::new(Spanned {
            node: CoreExpr::Int(0),
            span: pred_span,
        }),
    ))
}

/// Process one thunk and return either a result or a sub-thunk to force.
/// This mirrors the logic of `materialize()` but pushes continuations instead of recursing.
pub(crate) async fn force_step(
    thunk: &Arc<Thunk>,
    mat_span: Option<Span>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> Action {
    let thunk_span = thunk.span.clone();

    // Open profiling span if profiling is enabled. The guard closes the span on drop.
    let _profile_guard = ProfilingSpanGuard::new(ctx, thunk);

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
                cloned.materialization_span = Some(span.clone());
                should_update_cache = true;
            } else if cloned.materialization_span != Some(span.clone())
                && !cloned
                    .stack
                    .iter()
                    .any(|f| f.definition_span == span.clone())
            {
                cloned.push_frame("materialized".to_string(), span.clone());
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

        let mut err = EvalError::circular_dependency(label, thunk.span.clone(), cycle_path);
        if let Some(span) = mat_span {
            err = err.with_materialization_span(span.clone());
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
    //    PendingCall/Guarded → InProgress → Materialized/Failed). Exception: non-cacheable
    //    errors (ResourceLimitExceeded) trigger state restoration (e.g., InProgress →
    //    PendingBuiltin) so the computation can be retried if conditions change.
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

    if let Some((def, args, named, call_span, builtin_caller_env, thunk_ctx)) =
        thunk.take_pending_builtin()
    {
        // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
        // EvalStackGuard ensures pop on all exit paths; disarmed when delegating to a
        // continuation (BuiltinForceArg, Memoize) that inherits pop responsibility.
        let eval_stack_guard = EvalStackGuard::push(
            &thunk_ctx.state,
            (
                origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                thunk_span.clone(),
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
                    call_span: call_span.clone(),
                    caller_env: builtin_caller_env,
                    ctx: thunk_ctx,
                    origin,
                    thunk_span: thunk_span.clone(),
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
                call_span: call_span.clone(),
                caller_env: builtin_caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span: thunk_span.clone(),
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
        // non-cacheable errors. This defers Vec/IndexMap container
        // allocs to after the fast-path check — when the builtin returns a pre-materialized
        // thunk, the originals are simply dropped with no restore clone needed.
        // Builtin primitive type enforcement: builtins accept only Dict, String,
        // Int, Float, Bytes. Reject Value::Annotated — prelude wrappers are
        // responsible for peeling annotations before calling a builtin.
        // Only check already-materialized (forced) args; lazy args are checked
        // when the builtin materializes them.
        for arg_thunk in args.as_ref().expect("args set above").iter() {
            if let Some(crate::value::Value::Annotated { .. }) = arg_thunk.try_get_materialized() {
                let err = crate::error::EvalError::type_mismatch_ctx(
                    def.name.to_string(),
                    "primitive type (Dict, String, Int, Float, Bytes)",
                    "Annotated",
                    call_span.clone(),
                );
                // eval_stack_guard pops on drop
                return Action::Continue(Err(err.into()));
            }
        }

        let builtin_args = crate::value::BuiltinArgs {
            args: args.as_ref().expect("args set above").clone(),
            named: named.as_ref().expect("named set above").clone(),
            call_span: call_span.clone(),
            caller_env: Arc::clone(&builtin_caller_env),
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
                        thunk_span: thunk_span.clone(),
                        mat_span: mat_span.clone(),
                        restore: Some(RestoreState::PendingBuiltin {
                            def,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
                            call_span: call_span.clone(),
                            caller_env: Arc::clone(&builtin_caller_env),
                            ctx: Arc::clone(&thunk_ctx),
                        }),
                        ctx: Arc::clone(&thunk_ctx),
                    })));
                    // Memoize continuation inherits eval_stack pop responsibility
                    eval_stack_guard.disarm();
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: mat_span.clone(),
                    }
                }
            }
            Err(e) => {
                let decorated = attach_materialization_context(
                    e,
                    mat_span.as_ref(),
                    origin.as_deref(),
                    thunk_span.clone(),
                );
                // eval_stack_guard pops on drop (armed)
                // Restore to PendingBuiltin for non-cacheable errors so
                // the thunk can be retried. Cache as Failed only for cacheable errors.
                if decorated.kind.is_cacheable() {
                    thunk.cache_failure_once(&decorated);
                } else {
                    thunk.restore_unevaluated(crate::value::UnevaluatedState::Builtin {
                        def,
                        args: args.take().expect("args set above"),
                        named: named.take().expect("named set above"),
                        call_span,
                        caller_env: builtin_caller_env,
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
                thunk_span.clone(),
            ),
        );

        stack.push(Cont::PendingCallDispatch(Box::new(
            PendingCallDispatchData {
                thunk: Arc::clone(thunk),
                func_thunk: Arc::clone(&func_thunk),
                args,
                named: named.map(Box::new),
                call_span: call_span.clone(),
                caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span: thunk_span.clone(),
                mat_span,
                original_call,
                tail_hint,
            },
        )));
        eval_stack_guard.disarm();
        Action::Materialize {
            thunk: Arc::clone(&func_thunk),
            mat_span: Some(call_span.clone()),
        }
    } else if let Some((inner, expected, field_path, guard_span, blame_label, default_opt)) =
        thunk.take_guarded()
    {
        let inner_span = inner.span.clone();
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
            guard_span: guard_span.clone(),
            blame_label: blame_label.clone(),
            default: default_opt.clone(),
        };
        stack.push(Cont::GuardedValidate(Box::new(GuardedValidateData {
            thunk: Arc::clone(thunk),
            expected: expected.clone(),
            field_path,
            guard_span: guard_span.clone(),
            inner_span,
            origin,
            thunk_span: thunk_span.clone(),
            mat_span: mat_span.clone(),
            ctx: guard_ctx,
            blame_label,
            default: default_opt,
            restore: Some(restore),
        })));
        Action::Materialize {
            thunk: Arc::clone(&inner),
            mat_span,
        }
    } else if let Some((node, env, thunk_ctx)) = thunk.take_surface() {
        // Surface thunk handling in the CEK machine.
        //
        // The round-trip here is: SurfaceNode → lower() → Spanned<CoreExpr> → eval_core_expr()
        // → Arc<Thunk>. All cross-phase data (type annotations, field slots) is inline on nodes.
        // The lower() call reads inline fields directly — no external tables.
        //
        // After lower() we call eval_core_expr() to get a result thunk, then push a Memoize
        // continuation and return Action::Materialize to force the result thunk iteratively.
        // This keeps the Rust call stack flat (no recursive materialize() call).
        let restore = RestoreState::Surface {
            node: Arc::clone(&node),
            env: Arc::clone(&env),
            ctx: Arc::clone(&thunk_ctx),
        };

        let lowered = crate::lower::lower(&node);

        // Handle CoreExpr::TypeAssert inline after lowering — same loop risk as take_core_expr.
        if let crate::ast::CoreExpr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
            pipeline_blame,
        } = &lowered.node
        {
            // B-433/B-429: If inner is a literal error node and annotation has default:, use default.
            // Only applies to CoreExpr::Error (parse-time errors), not runtime evaluation failures.
            let inner_thunk = if let (crate::ast::CoreExpr::Error(_), Some(default_node)) =
                (&inner.node, annotation.node.get_property("default"))
            {
                let lowered_default = crate::lower::lower(default_node);
                match eval_core_expr(&lowered_default, &env, &thunk_ctx).await {
                    Ok(default_thunk) => default_thunk,
                    Err(e) => {
                        let decorated = attach_materialization_context(
                            e,
                            mat_span.as_ref(),
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure_once(&decorated);
                        } else {
                            restore.restore(thunk);
                        }
                        return Action::Continue(Err(decorated));
                    }
                }
            } else {
                match eval_core_expr(inner, &env, &thunk_ctx).await {
                    Ok(t) => t,
                    Err(e) => {
                        let decorated = attach_materialization_context(
                            e,
                            mat_span.as_ref(),
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure_once(&decorated);
                        } else {
                            restore.restore(thunk);
                        }
                        return Action::Continue(Err(decorated));
                    }
                }
            };
            let inner_span = inner_thunk.span.clone();
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span: thunk_span.clone(),
                mat_span: mat_span.clone(),
                restore: Some(restore),
                ctx: Arc::clone(&thunk_ctx),
            })));
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(resolved_type.clone()),
                expr_span: lowered.span.clone(),
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
                pipeline_blame: pipeline_blame.clone(),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(lowered.span.clone()),
            };
        }

        // Remaining CoreExpr variants (Call, Dict, Quote, etc.) fall through to eval_core_expr_pub.
        // Sequential and Match are handled inline above (lines ~1214 and ~1257) via CEK continuations.
        match eval_core_expr(&lowered, &env, &thunk_ctx).await {
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
                        thunk_span: thunk_span.clone(),
                        mat_span: mat_span.clone(),
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
                    thunk_span.clone(),
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
        //
        // No RestoreState needed: field extraction is pure/deterministic and cannot
        // raise non-cacheable errors (no I/O, no resource limits). All errors would be
        // cacheable, so no state restoration is required on error paths.
        let value = crate::surface_fields::surface_node_get_field(&node, field, &thunk_ctx);
        thunk.set_materialized(value.clone());
        Action::Continue(Ok(value))
    } else if let Some((core_expr, env, thunk_ctx)) = thunk.take_core_expr() {
        // CoreExpr thunk — created by invoke_function from Value::Function.body.
        // Calls eval_core_expr_pub directly (no CoreExpr→Expr round-trip).
        //
        // Restore state on non-cacheable error so the thunk can be retried.
        let restore = crate::value::UnevaluatedState::CoreExpr {
            expr: Arc::clone(&core_expr),
            env: Arc::clone(&env),
            ctx: Arc::clone(&thunk_ctx),
        };

        // Handle CoreExpr::TypeAssert inline. eval_core_expr(CoreExpr::TypeAssert) wraps
        // in new_unevaluated_core(CoreExpr::TypeAssert), which would loop back into this branch.
        if let crate::ast::CoreExpr::TypeAssert {
            annotation,
            expr: inner,
            resolved_type,
            pipeline_blame,
        } = &core_expr.node
        {
            // B-433/B-429: If inner is a literal error node and annotation has default:, use default.
            // Only applies to CoreExpr::Error (parse-time errors), not runtime evaluation failures.
            let inner_thunk = if let (crate::ast::CoreExpr::Error(_), Some(default_node)) =
                (&inner.node, annotation.node.get_property("default"))
            {
                let lowered_default = crate::lower::lower(default_node);
                match eval_core_expr(&lowered_default, &env, &thunk_ctx).await {
                    Ok(default_thunk) => default_thunk,
                    Err(e) => {
                        let decorated = attach_materialization_context(
                            e,
                            mat_span.as_ref(),
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure_once(&decorated);
                        } else {
                            thunk.restore_unevaluated(restore);
                        }
                        return Action::Continue(Err(decorated));
                    }
                }
            } else {
                match eval_core_expr(inner, &env, &thunk_ctx).await {
                    Ok(t) => t,
                    Err(e) => {
                        let decorated = attach_materialization_context(
                            e,
                            mat_span.as_ref(),
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        if decorated.kind.is_cacheable() {
                            thunk.cache_failure_once(&decorated);
                        } else {
                            thunk.restore_unevaluated(restore);
                        }
                        return Action::Continue(Err(decorated));
                    }
                }
            };
            let inner_span = inner_thunk.span.clone();
            stack.push(Cont::Memoize(Box::new(MemoizeData {
                thunk: Arc::clone(thunk),
                origin,
                thunk_span: thunk_span.clone(),
                mat_span: mat_span.clone(),
                restore: Some(RestoreState::CoreExpr {
                    expr: Arc::clone(&core_expr),
                    env: Arc::clone(&env),
                    ctx: Arc::clone(&thunk_ctx),
                }),
                ctx: Arc::clone(&thunk_ctx),
            })));
            stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                annotation: Box::new(annotation.clone()),
                resolved: Box::new(resolved_type.clone()),
                expr_span: core_expr.span.clone(),
                thunk_span: inner_span,
                env,
                ctx: Arc::clone(&thunk_ctx),
                pipeline_blame: pipeline_blame.clone(),
            })));
            return Action::Materialize {
                thunk: inner_thunk,
                mat_span: Some(core_expr.span.clone()),
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
                thunk_span: thunk_span.clone(),
                mat_span: mat_span.clone(),
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
                    seq_span: core_expr.span.clone(),
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
            let scrutinee_thunk = match eval_core_expr(scrutinee, &env, &thunk_ctx).await {
                Ok(t) => t,
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        mat_span.as_ref(),
                        origin.as_deref(),
                        thunk_span.clone(),
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
                thunk_span: thunk_span.clone(),
                mat_span: mat_span.clone(),
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
                    match_span: core_expr.span.clone(),
                },
            )));

            // Materialize the scrutinee
            return Action::Materialize {
                thunk: scrutinee_thunk,
                mat_span: Some(scrutinee.span.clone()),
            };
        }

        match eval_core_expr(&core_expr, &env, &thunk_ctx).await {
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
                    thunk_span: thunk_span.clone(),
                    mat_span: mat_span.clone(),
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
                    thunk_span.clone(),
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
                attach_materialization_context(
                    e,
                    mat_span.as_ref(),
                    origin.as_deref(),
                    thunk_span.clone(),
                )
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
                    // when the default expression hits a non-cacheable error.
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
            let origin_for_decorate = origin.clone();
            let thunk_span_for_decorate = thunk_span.clone();
            let mat_span_for_decorate = mat_span.clone();
            let decorate = move |e| {
                attach_materialization_context(
                    e,
                    mat_span_for_decorate.as_ref(),
                    origin_for_decorate.as_deref(),
                    thunk_span_for_decorate.clone(),
                )
            };

            // Wrap args/named in Option so each exclusive match arm can move them
            // without cloning. Taking ownership avoids the pre-clone of Box<Vec>/Box<IndexMap>
            // that was previously done on every successful function call to build RestoreState.
            // Each arm calls .take().expect("...") exactly once to extract the owned value.
            let mut args = Some(args);
            let mut named = Some(named);

            match result.map_err(&decorate) {
                Ok(func_value) => {
                    // Value::Annotated is transparent for call dispatch — peel annotation layers
                    // before dispatching. Annotated constructors (e.g., TypeNode.Int@[...]) must
                    // be callable just like their un-annotated inner values. Annotations are purely
                    // metadata and never affect callable semantics (mirrors type_name(), Display,
                    // Debug, which all delegate to the inner value).
                    let func_value = {
                        let mut v = func_value;
                        while let Value::Annotated { inner, .. } = v {
                            v = *inner;
                        }
                        v
                    };
                    match func_value {
                        Value::Function {
                            params, body, env, ..
                        } => {
                            // TCO path: when tail_hint=true, skip Memoize and return EvalCore directly.
                            //
                            // Profiling span note: the non-TCO path pushes a Memoize continuation that
                            // holds a ProfilingSpanGuard, so each call frame gets a distinct span in
                            // profiling output. In the TCO path we skip Memoize entirely — no
                            // ProfilingSpanGuard is created for the tail call, and the parent frame's
                            // guard (if any) stays alive through the tail call body. This means tail
                            // calls appear as part of the parent span in profiling output rather than
                            // as separate child spans. This is a known limitation of O(1) TCO with the
                            // current profiling infrastructure: adding a guard here would require either
                            // a synthetic Memoize-like continuation solely to drop it, or a span-handoff
                            // protocol between the abandoned thunk and the new EvalCore frame.

                            // @Expr param detection: when any parameter is annotated @Expr, replace
                            // that positional arg with the quoted AST of the original call-site
                            // expression, and inject two implicit named args:
                            //   ᴍᴀᴄʀᴏ∷env  — the call-site environment (Value::Environment)
                            //   ᴍᴀᴄʀᴏ∷span — the call-site span (a span dict)
                            //
                            // This implements the new macro convention (S-902): macros are ordinary
                            // `fn` functions whose @Expr params receive raw syntax rather than
                            // evaluated values.  Normal (non-@Expr) calls are completely unaffected —
                            // the branch below only fires when `has_expr_params` is true.
                            //
                            // The implicit args are injected as named args (not positional) so that
                            // macro function signatures do not need to declare __call-env__ /
                            // __call-span__ params.  Instead, builtin-eval-macro-ast reads them
                            // directly from the environment chain via the gensym-safe names below.
                            const MACRO_CALL_ENV_NAME: &str = "ᴍᴀᴄʀᴏ∷env";
                            const MACRO_CALL_SPAN_NAME: &str = "ᴍᴀᴄʀᴏ∷span";

                            let has_expr_params = params.iter().any(|p| {
                                p.annotation
                                    .as_ref()
                                    .map_or(false, |a| is_expr_annotation(&a.node))
                            });

                            if has_expr_params {
                                if let CoreExpr::Call {
                                    args: core_args, ..
                                } = &original_call.node
                                {
                                    let mut args_vec =
                                        args.take().expect("args set for @Expr quoting");

                                    // Replace each @Expr-annotated positional arg with the quoted AST
                                    // of the corresponding call-site CoreExpr.  Args at positions
                                    // beyond core_args (e.g. a variadic rest) are left as thunks.
                                    for (i, param) in params.iter().enumerate() {
                                        if param
                                            .annotation
                                            .as_ref()
                                            .map_or(false, |a| is_expr_annotation(&a.node))
                                        {
                                            if i < core_args.len() && i < args_vec.len() {
                                                let expr_value =
                                                    crate::surface_convert::core_expr_to_expr_value(
                                                        &core_args[i],
                                                        &thunk_ctx,
                                                    );
                                                args_vec[i] = Arc::new(Thunk::new_materialized(
                                                    expr_value,
                                                    core_args[i].span.clone(),
                                                ));
                                            }
                                        }
                                    }

                                    args = Some(args_vec);

                                    // Inject implicit named args: ᴍᴀᴄʀᴏ∷env and ᴍᴀᴄʀᴏ∷span.
                                    // These are passed as named args rather than positional so macro
                                    // fn signatures do not need to declare them.  builtin-eval-macro-ast
                                    // retrieves them from the named arg map of its BuiltinArgs.caller_env
                                    // environment chain.
                                    let inner_named = named.as_mut().expect("named set above");
                                    let named_map = inner_named
                                        .get_or_insert_with(|| Box::new(IndexMap::new()));
                                    named_map.insert(
                                        MACRO_CALL_ENV_NAME.to_string(),
                                        Arc::new(Thunk::new_materialized(
                                            Value::Environment(Arc::clone(&caller_env)),
                                            call_span.clone(),
                                        )),
                                    );
                                    let span_thunk_id =
                                        make_span_dict(&call_span, &thunk_ctx, &call_span);
                                    named_map.insert(
                                        MACRO_CALL_SPAN_NAME.to_string(),
                                        thunk_ctx.get_thunk(span_thunk_id),
                                    );
                                }
                                // If original_call is not CoreExpr::Call (e.g. it is a VarRef or
                                // literal that resolved to a function), there are no source args to
                                // quote.  Leave args unchanged — the macro will receive evaluated
                                // thunks for those positions, which is the best we can do.
                            }

                            if tail_hint {
                                let invoke_result = {
                                    let call_ctx = CallContext {
                                        params: &params,
                                        body: &body,
                                        closure_env: &env,
                                        positional: args.as_deref().expect("args set above"),
                                        named: named.as_ref().expect("named set above").as_deref(),
                                        default_env: &caller_env,
                                        call_span: call_span.clone(),
                                        origin: origin.clone(),
                                        ctx: &thunk_ctx,
                                    };
                                    invoke_function_tco(&call_ctx).await
                                };

                                match invoke_result.map_err(&decorate) {
                                    Ok((body_expr, new_env)) => {
                                        // TCO abandonment: this thunk stays InProgress and will be dropped
                                        // when this continuation is consumed (strong_count==1 guarantees no
                                        // other references exist). The result flows directly to the caller's
                                        // continuation via Action::EvalCore → run loop → new thunk
                                        // materialization.
                                        Action::EvalCore {
                                            expr: body_expr,
                                            env: new_env,
                                            ctx: thunk_ctx,
                                        }
                                    }
                                    Err(mut e) => {
                                        e.push_frame(
                                            origin.as_deref().unwrap_or("call").to_string(),
                                            call_span.clone(),
                                        );
                                        // eval_stack_guard pops on drop (armed)
                                        if e.kind.is_cacheable() {
                                            thunk.cache_failure_once(&e);
                                        } else {
                                            // Restore via RestoreState for consistency.
                                            let restore = RestoreState::Call {
                                                func: func_thunk,
                                                args: args.take().expect("args set above"),
                                                named: named.take().expect("named set above"),
                                                call_span,
                                                caller_env,
                                                ctx: thunk_ctx,
                                                original_call: original_call.clone(),
                                            };
                                            restore.restore(&thunk);
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
                                        call_span: call_span.clone(),
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
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
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
                                            call_span.clone(),
                                        );
                                        // eval_stack_guard pops on drop (armed)
                                        if e.kind.is_cacheable() {
                                            thunk.cache_failure_once(&e);
                                        } else {
                                            // Restore via RestoreState for consistency.
                                            let restore = RestoreState::Call {
                                                func: func_thunk,
                                                args: args.take().expect("args set above"),
                                                named: named.take().expect("named set above"),
                                                call_span,
                                                caller_env,
                                                ctx: thunk_ctx,
                                                original_call: original_call.clone(),
                                            };
                                            restore.restore(&thunk);
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
                                thunk.restore_unevaluated(
                                    crate::value::UnevaluatedState::Builtin {
                                        def,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above").map(|b| *b),
                                        call_span: call_span.clone(),
                                        caller_env: Arc::clone(&caller_env),
                                        ctx: thunk_ctx,
                                    },
                                );
                                return Action::Materialize { thunk, mat_span };
                            }

                            // All strict args are already materialized — call the builtin directly.
                            // The block scopes the borrows of args/named so the borrow
                            // checker allows args.take()/named.take() in the match arms below.
                            let builtin_result = {
                                let builtin_args = crate::value::BuiltinArgs {
                                    args: args.as_deref().expect("args set above").to_vec(),
                                    named: named
                                        .as_ref()
                                        .expect("named set above")
                                        .as_deref()
                                        .cloned(),
                                    call_span: call_span.clone(),
                                    caller_env: Arc::clone(&caller_env),
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
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
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
                        // Unit variant used as a constructor: e.g. `[Result.Ok payload]`.
                        // When a unit Variant (payload: None) is called with exactly one positional
                        // arg and no named args, treat it as constructing Variant(tag, payload).
                        // Unit constructors from `[type ...]` declarations are Value::Variant{payload:None}
                        // at runtime; calling them with one positional arg constructs a new Variant with that payload.
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
                            if !tail_hint {
                                thunk.set_materialized(result_val.clone());
                            }
                            Action::Continue(Ok(result_val))
                        }
                        other => {
                            let err = EvalError::type_mismatch(
                                "Function or Builtin",
                                other.type_name(),
                                call_span.clone(),
                            );
                            let decorated = decorate(Box::new(err));
                            // eval_stack_guard pops on drop (armed)
                            if decorated.kind.is_cacheable() {
                                thunk.cache_failure_once(&decorated);
                            } else {
                                // Restore via RestoreState for consistency.
                                let restore = RestoreState::Call {
                                    func: func_thunk,
                                    args: args.take().expect("args set above"),
                                    named: named.take().expect("named set above"),
                                    call_span: call_span.clone(),
                                    caller_env,
                                    ctx: thunk_ctx,
                                    original_call: original_call.clone(),
                                };
                                restore.restore(&thunk);
                            }
                            Action::Continue(Err(decorated))
                        }
                    } // end match func_value
                } // end Ok(func_value) block
                Err(e) => {
                    // Function materialization failed
                    // eval_stack_guard pops on drop (armed)
                    if e.kind.is_cacheable() {
                        thunk.cache_failure_once(&e);
                    } else {
                        // Restore via RestoreState for consistency.
                        let restore = RestoreState::Call {
                            func: func_thunk,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
                            call_span,
                            caller_env,
                            ctx: thunk_ctx,
                            original_call: original_call.clone(),
                        };
                        restore.restore(&thunk);
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
            let origin_for_decorate = origin.clone();
            let thunk_span_for_decorate = thunk_span.clone();
            let mat_span_for_decorate = mat_span.clone();
            let decorate = move |e| {
                attach_materialization_context(
                    e,
                    mat_span_for_decorate.as_ref(),
                    origin_for_decorate.as_deref(),
                    thunk_span_for_decorate.clone(),
                )
            };

            match result {
                Ok(value) => {
                    // Flatten Overlay to Dict before record validation.
                    // Value::Overlay is produced by $merge; guard wrapping it needs flattened entries.
                    // guard_ctx is Arc<EvalContext> (non-optional); destructured directly from the continuation.
                    let value = match value {
                        Value::Overlay(l, r) => {
                            match flatten_overlay(
                                &l,
                                &r,
                                "type guard",
                                &guard_ctx,
                                guard_span.clone(),
                            )
                            .await
                            {
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
                                guard_span.clone(),
                                inner_span.clone(),
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
                                        // hits a non-cacheable error, Memoize must be able to restore
                                        // the thunk to Guarded state — including the original default
                                        // so a retry can attempt it again.
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
                                                guard_span: guard_span.clone(),
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
                                                thunk_span.clone(),
                                            ),
                                        );
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin: Some(Arc::from("default fallback")),
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
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
                                        guard_span: guard_span.clone(),
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
                                        thunk_span.clone(),
                                    ),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
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
                                inner_span.clone(),
                            )
                            .with_materialization_span(guard_span.clone());
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err
                                    .with_secondary_span(inner_span.clone(), "value produced here");
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
                        if value_matches_type(&value, &expected, &guard_ctx) {
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
                                        guard_span: guard_span.clone(),
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
                                        thunk_span.clone(),
                                    ),
                                );
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
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
                                inner_span.clone(),
                            )
                            .with_materialization_span(guard_span.clone());
                            // Add secondary span if inner value was produced at a different
                            // location than the assertion site (guard_span).
                            if inner_span != guard_span {
                                err = err
                                    .with_secondary_span(inner_span.clone(), "value produced here");
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
                caller_env: builtin_caller_env,
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
            let origin_for_decorate = origin.clone();
            let thunk_span_for_decorate = thunk_span.clone();
            let mat_span_for_decorate = mat_span.clone();
            let decorate = move |e| {
                attach_materialization_context(
                    e,
                    mat_span_for_decorate.as_ref(),
                    origin_for_decorate.as_deref(),
                    thunk_span_for_decorate.clone(),
                )
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
                                call_span: call_span.clone(),
                                caller_env: builtin_caller_env,
                                ctx: thunk_ctx,
                                origin,
                                thunk_span: thunk_span.clone(),
                                mat_span: mat_span.clone(),
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
                            call_span: call_span.clone(),
                            caller_env: builtin_caller_env,
                            ctx: thunk_ctx,
                            origin,
                            thunk_span: thunk_span.clone(),
                            mat_span: mat_span.clone(),
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
                        call_span: call_span.clone(),
                        caller_env: Arc::clone(&builtin_caller_env),
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
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
                                    restore: Some(RestoreState::PendingBuiltin {
                                        def,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span: call_span.clone(),
                                        caller_env: Arc::clone(&builtin_caller_env),
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
                            // Restore to PendingBuiltin for non-cacheable errors.
                            if e.kind.is_cacheable() {
                                thunk.cache_failure_once(&e);
                            } else {
                                thunk.restore_unevaluated(
                                    crate::value::UnevaluatedState::Builtin {
                                        def,
                                        args: args.take().expect("args set above"),
                                        named: named.take().expect("named set above"),
                                        call_span: call_span.clone(),
                                        caller_env: builtin_caller_env,
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
                            caller_env: builtin_caller_env,
                            ctx: thunk_ctx,
                        });
                    }
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
                pipeline_blame,
            } = *data;
            let expected = *resolved;
            match result {
                Err(e) => {
                    // B-433: When inner expr materialization fails (CoreExpr::Error, undefined variable, etc.),
                    // check for `default:` annotation and evaluate it instead of propagating the error.
                    if let Some(default_node) = annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                    {
                        Action::EvalCore {
                            expr: Arc::new(crate::lower::lower(default_node)),
                            env,
                            ctx: Arc::clone(&ctx),
                        }
                    } else {
                        Action::Continue(Err(e))
                    }
                }
                Ok(value) => {
                    // For Record types and Intersection-of-Records, apply proxy contract wrapping.
                    // as_record_row_merged merges all required fields from all members into a Row.
                    if let Some(row) = as_record_row_merged(&expected) {
                        // Flatten Overlay to Dict before record type assertion.
                        let value = match value {
                            Value::Overlay(l, r) => {
                                match flatten_overlay(
                                    &l,
                                    &r,
                                    "type assert",
                                    &ctx,
                                    expr_span.clone(),
                                )
                                .await
                                {
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
                                    (Arc::new(crate::lower::lower(node)), Arc::clone(&env))
                                });
                            // Construct BlameLabel for TypeAssert boundary
                            let blame_label = Some(crate::error::BlameLabel {
                                origin_span: thunk_span.clone(),  // where the value was produced
                                boundary_span: expr_span.clone(), // where the TypeAssert annotation is
                                polarity: crate::error::BlameParity::Positive,
                            });
                            match validate_and_wrap_record(
                                entries,
                                row.as_ref(),
                                &mut vec![],
                                expr_span.clone(),
                                thunk_span.clone(),
                                &ctx,
                                default_opt.clone(),
                                blame_label,
                            ) {
                                Ok(new_entries) => Action::Continue(Ok(Value::Dict(new_entries))),
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
                                        // Attach pipeline blame if this assertion is from a --- expects: boundary.
                                        let err = if let Some(ref blame) = pipeline_blame {
                                            Box::new((*err).with_pipeline_blame(blame.clone()))
                                        } else {
                                            err
                                        };
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
                                    expr: Arc::new(crate::lower::lower(default_node)),
                                    env,
                                    ctx: Arc::clone(&ctx),
                                }
                            } else {
                                let mut err = EvalError::type_assert_failed(
                                    &format_type_for_assert(&expected),
                                    value.type_name(),
                                    thunk_span.clone(),
                                )
                                .with_materialization_span(expr_span.clone());
                                if thunk_span != expr_span {
                                    err = err.with_secondary_span(
                                        thunk_span.clone(),
                                        "value produced here",
                                    );
                                }
                                // Attach pipeline blame if this assertion is from a --- expects: boundary.
                                let err = if let Some(ref blame) = pipeline_blame {
                                    err.with_pipeline_blame(blame.clone())
                                } else {
                                    err
                                };
                                Action::Continue(Err(err.into()))
                            }
                        }
                    } else if value_matches_type(&value, &expected, &ctx) {
                        let is_predicate = annotation.node.get_property(IS_ANNOTATION_KEY).cloned();
                        if let Some(predicate_node) = is_predicate {
                            stack.push(Cont::PredicateCheck(Box::new(PredicateCheckData {
                                value: value.clone(),
                                annotation,
                                expr_span: expr_span.clone(),
                                thunk_span: thunk_span.clone(),
                                env: Arc::clone(&env),
                                ctx: Arc::clone(&ctx),
                                callable_invoked: false,
                            })));
                            Action::EvalCore {
                                expr: Arc::new(crate::lower::lower(&predicate_node)),
                                env,
                                ctx: Arc::clone(&ctx),
                            }
                        } else {
                            Action::Continue(Ok(value))
                        }
                    } else if let Some(default_node) =
                        annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                    {
                        Action::EvalCore {
                            expr: Arc::new(crate::lower::lower(default_node)),
                            env,
                            ctx: Arc::clone(&ctx),
                        }
                    } else {
                        let mut err = EvalError::type_assert_failed(
                            &format_type_for_assert(&expected),
                            value.type_name(),
                            thunk_span.clone(),
                        )
                        .with_materialization_span(expr_span.clone());
                        if thunk_span != expr_span {
                            err =
                                err.with_secondary_span(thunk_span.clone(), "value produced here");
                        }
                        // Attach pipeline blame if this assertion is from a --- expects: boundary.
                        let err: crate::error::EvalError = if let Some(ref blame) = pipeline_blame {
                            err.with_pipeline_blame(blame.clone())
                        } else {
                            err
                        };
                        Action::Continue(Err(err.into()))
                    }
                }
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
                                                // Annotated Var: name field is the bare identifier.
                                                CoreExpr::Var { name, .. } => Some(name.clone()),
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
                        // LetDecl in sequential fn-body position: [let name value] pairs.
                        // Evaluated as a Dict (see eval.rs CoreExpr::LetDecl arm); extract names
                        // so the Dict-based binding logic creates the correct child_env scope.
                        CoreExpr::LetDecl { bindings } => {
                            let mut keys = HashSet::new();
                            let mut i = 0;
                            while i + 1 < bindings.len() {
                                match &bindings[i].node {
                                    // lower_let_decl_binding converts declaration names to Str literals.
                                    CoreExpr::Str(name) | CoreExpr::Var { name, .. } => {
                                        keys.insert(name.clone());
                                    }
                                    _ => {}
                                }
                                i += 2;
                            }
                            if keys.is_empty() {
                                None
                            } else {
                                Some(keys)
                            }
                        }
                        _ => None,
                    };

                    if let Some(ref static_key_set) = static_keys {
                        // Flatten Overlay to Dict for scope chain binding
                        let map = match intermediate_value {
                            Value::Dict(map) => map,
                            Value::Overlay(l, r) => match crate::builtins::flatten_overlay(
                                &l,
                                &r,
                                "sequential expression",
                                &ctx,
                                current_expr.span.clone(),
                            )
                            .await
                            {
                                Ok(map) => map,
                                Err(e) => return Action::Continue(Err(e)),
                            },
                            Value::Variant {
                                payload: Some(payload_id),
                                ..
                            } => {
                                // Auto-unpack variant payload for sequential scope-chain binding.
                                // Unit Variants (no payload) fall through to the type error below.
                                // require_dict handles Dict, Overlay, and nested Variants recursively.
                                let payload_thunk = ctx.get_thunk(payload_id);

                                // Fast path: payload already materialized (common after earlier forcing).
                                // Use the cached value directly — no executor re-entry needed.
                                if let Some(payload_val) = payload_thunk.try_get_materialized() {
                                    match crate::builtins::require_dict(
                                        "sequential expression",
                                        payload_val,
                                        current_expr.span.clone(),
                                        &ctx,
                                        current_expr.span.clone(),
                                    )
                                    .await
                                    {
                                        Ok(map) => map,
                                        Err(e) => return Action::Continue(Err(e)),
                                    }
                                } else {
                                    // Slow path: payload not yet forced. Push a continuation to unpack
                                    // the payload after materialization, then force it through the CEK
                                    // machine. This replaces the sync materialization call that bypassed the
                                    // CEK continuation stack (B-396).
                                    stack.push(Cont::VariantUnpackForSeq(Box::new(
                                        VariantUnpackForSeqData {
                                            static_key_set: static_key_set.clone(),
                                            next_idx,
                                            exprs: Arc::clone(&exprs),
                                            env: Arc::clone(&env),
                                            ctx: Arc::clone(&ctx),
                                            seq_span: seq_span.clone(),
                                            current_expr_span: current_expr.span.clone(),
                                        },
                                    )));
                                    return Action::Materialize {
                                        thunk: payload_thunk,
                                        mat_span: Some(current_expr.span.clone()),
                                    };
                                }
                            }
                            _ => {
                                return Action::Continue(Err(Box::new(
                                    EvalError::type_mismatch_ctx(
                                        format!("sequential expression #{}", idx + 1),
                                        "Dict, Overlay, or Variant",
                                        intermediate_value.type_name(),
                                        current_expr.span.clone(),
                                    ),
                                )));
                            }
                        };

                        // Insert static-key entries as lazy thunks into child_env.
                        //
                        // Sequential dict bindings use lazy semantics: named entries are
                        // inserted as unevaluated thunks. They are forced only when accessed
                        // by subsequent expressions. Dead bindings (never accessed) never fire,
                        // which is the correct lazy evaluation behavior.
                        let child_env =
                            Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

                        {
                            let mut env_write = child_env.write().unwrap();
                            for (key, thunk_id) in map.into_iter() {
                                if let HashableValue::Str(name) = key {
                                    if static_key_set.contains(name.as_ref()) {
                                        let val_thunk = ctx.get_thunk(thunk_id);
                                        env_write.insert(name.to_string(), val_thunk);
                                    }
                                }
                            }
                        }

                        // Proceed directly to the next expression with the populated child_env.
                        let next_expr = &exprs[next_idx];
                        stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                            idx: next_idx,
                            exprs: Arc::clone(&exprs),
                            env: Arc::clone(&child_env),
                            ctx: Arc::clone(&ctx),
                            seq_span,
                        })));
                        Action::EvalCore {
                            expr: Arc::clone(next_expr),
                            env: child_env,
                            ctx,
                        }
                    } else {
                        // No static keys — no scope created, continue with same env.
                        let next_expr = &exprs[next_idx];
                        stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                            idx: next_idx,
                            exprs: Arc::clone(&exprs),
                            env: Arc::clone(&env),
                            ctx: Arc::clone(&ctx),
                            seq_span,
                        })));
                        Action::EvalCore {
                            expr: Arc::clone(next_expr),
                            env,
                            ctx,
                        }
                    }
                }
            }
        }
        Cont::VariantUnpackForSeq(data) => {
            let VariantUnpackForSeqData {
                static_key_set,
                next_idx,
                exprs,
                env,
                ctx,
                seq_span,
                current_expr_span,
            } = *data;

            // Result is the materialized payload value from the Variant
            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(payload_val) => {
                    // Unpack the payload dict using require_dict
                    let map = match crate::builtins::require_dict(
                        "sequential expression",
                        payload_val,
                        current_expr_span.clone(),
                        &ctx,
                        current_expr_span,
                    )
                    .await
                    {
                        Ok(map) => map,
                        Err(e) => return Action::Continue(Err(e)),
                    };

                    // Insert static-key entries as lazy thunks into child_env.
                    //
                    // Sequential dict bindings use lazy semantics: named entries are
                    // inserted as unevaluated thunks. They are forced only when accessed
                    // by subsequent expressions. Dead bindings (never accessed) never fire,
                    // which is the correct lazy evaluation behavior.
                    let child_env =
                        Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&env))));

                    {
                        let mut env_write = child_env.write().unwrap();
                        for (key, thunk_id) in map.into_iter() {
                            if let HashableValue::Str(name) = key {
                                if static_key_set.contains(name.as_ref()) {
                                    let val_thunk = ctx.get_thunk(thunk_id);
                                    env_write.insert(name.to_string(), val_thunk);
                                }
                            }
                        }
                    }

                    // Proceed directly to the next expression with the populated child_env.
                    let next_expr = &exprs[next_idx];
                    stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                        idx: next_idx,
                        exprs: Arc::clone(&exprs),
                        env: Arc::clone(&child_env),
                        ctx: Arc::clone(&ctx),
                        seq_span,
                    })));
                    Action::EvalCore {
                        expr: Arc::clone(next_expr),
                        env: child_env,
                        ctx,
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

                        // T-1140: Predicate patterns require CEK machine evaluation.
                        // Intercept Pattern::Predicate before match_pattern (which would panic).
                        //
                        // Design: the predicate SurfaceNode is a call-expression template.
                        // The scrutinee is appended as the last positional argument to form
                        // the complete call (e.g., `[starts-with? "foo"]` + scrutinee →
                        // `[starts-with? "foo" %pred_subj]`). For non-Call predicates
                        // (e.g., `[fn [let x] body]`), the predicate expression is evaluated
                        // as a function value, then called with the scrutinee via
                        // apply_predicate_to_subject (callable_invoked=false path).
                        //
                        // Scrutinee injection: a gensym name `%pred_subj` is inserted into a
                        // child env bound to the scrutinee value. A Var(`%pred_subj`)
                        // core expression is appended as the final arg. The % prefix is
                        // reserved and cannot appear in user-written identifiers.
                        if let crate::ast::Pattern::Predicate {
                            call: pred_node,
                            to_match_binding,
                        } = &arm.pattern.node
                        {
                            let resolved_binding = to_match_binding.get().cloned();
                            let lowered_pred = crate::lower::lower(pred_node);
                            let pred_span = lowered_pred.span.clone();
                            // Check if the lowered predicate is a Call expression.
                            // If so, extend its arg list with a Var referencing the scrutinee.
                            // If not (e.g., a Fn literal), evaluate it first and call the result.
                            let (eval_expr, eval_env) = match lowered_pred.node {
                                crate::ast::CoreExpr::Call {
                                    func,
                                    mut args,
                                    named_args,
                                    implied,
                                } => {
                                    // Insert scrutinee into a child env under a gensym name.
                                    let subj_name = "%pred_subj".to_string();
                                    let child_env = Arc::new(RwLock::new(
                                        Environment::with_parent(Arc::clone(&env)),
                                    ));
                                    let scrutinee_thunk = Arc::new(Thunk::new_materialized(
                                        scrutinee_value.clone(),
                                        pred_span.clone(),
                                    ));
                                    child_env
                                        .write()
                                        .unwrap()
                                        .insert(subj_name.clone(), scrutinee_thunk);
                                    // Append a Var referencing `%pred_subj` as the final positional arg.
                                    // The name is bound at slot 0 in child_env (the only binding).
                                    args.push(Arc::new(Spanned::new(
                                        crate::ast::CoreExpr::Var {
                                            name: subj_name,
                                            level: 0,
                                            slot: 0,
                                            annotation: None,
                                        },
                                        pred_span.clone(),
                                    )));
                                    let extended_call = Arc::new(Spanned::new(
                                        crate::ast::CoreExpr::Call {
                                            func,
                                            args,
                                            named_args,
                                            implied,
                                        },
                                        pred_span,
                                    ));
                                    (extended_call, child_env)
                                }
                                other => {
                                    // Non-Call predicate (e.g., Fn literal): evaluate as a function,
                                    // then apply_predicate_to_subject will call it with scrutinee.
                                    // MatchPredicateCheck (callable_invoked=false) handles this path.
                                    (Arc::new(Spanned::new(other, pred_span)), Arc::clone(&env))
                                }
                            };
                            let is_call_path =
                                matches!(&eval_expr.node, crate::ast::CoreExpr::Call { .. });
                            stack.push(Cont::MatchPredicateCheck(Box::new(
                                MatchPredicateCheckData {
                                    arm_idx: i,
                                    arms: Arc::clone(&arms),
                                    env: Arc::clone(&env),
                                    ctx: Arc::clone(&ctx),
                                    match_span: match_span.clone(),
                                    scrutinee_value: scrutinee_value.clone(),
                                    body: Arc::clone(&arm.body),
                                    // For the Call path, the result of EvalCore IS the predicate
                                    // result (Bool). For the non-Call path, the result is a function
                                    // value that must still be called with the scrutinee.
                                    callable_invoked: is_call_path,
                                    to_match_binding: resolved_binding.clone(),
                                },
                            )));
                            return Action::EvalCore {
                                expr: eval_expr,
                                env: eval_env,
                                ctx,
                            };
                        }

                        // Try the pattern. Since apply_cont is async, we can .await directly
                        // here without block_on_anywhere — this keeps async state on the heap
                        // rather than the Rust stack, preventing stack overflow on deeply
                        // nested patterns.
                        let matched_env = match match_pattern(
                            &arm.pattern.node,
                            &scrutinee_value,
                            &env,
                            &arm.pattern.span.clone(),
                            &ctx,
                        )
                        .await
                        {
                            Ok(opt) => opt,
                            Err(e) => return Action::Continue(Err(e)),
                        };

                        if let Some(arm_env) = matched_env {
                            // The sentinel pattern (Wildcard) matched. Now check if the body is a
                            // CaseArm — if so, the actual pattern evaluation is deferred here.
                            // CaseArm bodies are [case pattern body] arms stored with a Wildcard
                            // sentinel so the outer match loop can find them. The real pattern
                            // evaluation is our responsibility: process the [let ...] pattern or
                            // exact-value match ourselves, and either bind variables and evaluate
                            // the body, or soft-skip to the next arm.
                            let (final_env, eval_body) = if let CoreExpr::CaseArm {
                                let_bindings,
                                pattern,
                                body,
                            } = &arm.body.node
                            {
                                // 3-arg form: [case [let bindings] pattern body]
                                // Extract the binding name set from the let_bindings node.
                                // Walk the structural pattern, binding names in the set and
                                // pin-comparing names not in the set.
                                let binding_set = extract_let_binding_names(&let_bindings.node);
                                match eval_case_arm_structural_pattern(
                                    pattern,
                                    &binding_set,
                                    &scrutinee_value,
                                    &env,
                                    match_span.clone(),
                                    &ctx,
                                )
                                .await
                                {
                                    Ok(Some(bound_env)) => (bound_env, Arc::clone(body)),
                                    Ok(None) => {
                                        // Pattern did not match — move to next arm
                                        continue;
                                    }
                                    Err(e) => return Action::Continue(Err(e)),
                                }
                            } else {
                                // Not a CaseArm body — use the arm environment as-is.
                                (arm_env, Arc::clone(&arm.body))
                            };

                            // Pattern matched. If there is a guard, evaluate it.
                            if let Some(guard_expr) = &arm.guard {
                                // Push a continuation to check the guard result.
                                let guard_binding = arm.guard_matchable_binding.get().cloned();
                                stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                    arm_idx: i,
                                    arms: Arc::clone(&arms),
                                    env: Arc::clone(&env),
                                    ctx: Arc::clone(&ctx),
                                    match_span: match_span.clone(),
                                    arm_env: Arc::clone(&final_env),
                                    scrutinee_value: scrutinee_value.clone(),
                                    body: Arc::clone(&eval_body),
                                    callable_invoked: false,
                                    guard_matchable_binding: guard_binding,
                                })));

                                return Action::EvalCore {
                                    expr: Arc::clone(guard_expr),
                                    env: Arc::clone(&final_env),
                                    ctx,
                                };
                            }

                            // No guard — arm matched, evaluate body
                            return Action::EvalCore {
                                expr: eval_body,
                                env: final_env,
                                ctx,
                            };
                        }
                        // Pattern did not match — continue to next arm
                    }

                    // No arm matched: non-exhaustive match
                    Action::Continue(Err(Box::new(EvalError::match_exhaustion(
                        scrutinee_value.type_name(),
                        match_span.clone(),
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
                callable_invoked,
                guard_matchable_binding,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(guard_value) => {
                    // PM1: If the guard is callable and we haven't yet invoked it, do so
                    // iteratively via the CEK machine rather than block_on_anywhere.
                    if !callable_invoked {
                        if let Value::Function { .. } | Value::Builtin(_) = &guard_value {
                            // Create a thunk for the scrutinee
                            let scrutinee_thunk = Arc::new(Thunk::new_materialized(
                                scrutinee_value.clone(),
                                match_span.clone(),
                            ));
                            // Create a thunk for the guard callable
                            let pred_thunk =
                                Arc::new(Thunk::new_materialized(guard_value, match_span.clone()));
                            // Create a PendingCall thunk for guard(scrutinee)
                            let call_thunk = Arc::new(Thunk::new_pending_call(
                                pred_thunk,
                                vec![scrutinee_thunk],
                                IndexMap::new(),
                                match_span.clone(),
                                Arc::clone(&arm_env),
                                match_span.clone(),
                                None,
                                Arc::clone(&ctx),
                                Arc::new(Spanned {
                                    node: CoreExpr::Int(0),
                                    span: match_span.clone(),
                                }),
                            ));
                            // Push MatchGuardCheck again with callable_invoked=true to receive
                            // the call result, then return Materialize to drive the call
                            // iteratively through the CEK loop (no block_on_anywhere).
                            stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                arm_idx,
                                arms,
                                env,
                                ctx: Arc::clone(&ctx),
                                match_span: match_span.clone(),
                                arm_env,
                                scrutinee_value,
                                body,
                                callable_invoked: true,
                                guard_matchable_binding,
                            })));
                            return Action::Materialize {
                                thunk: call_thunk,
                                mat_span: Some(match_span),
                            };
                        }
                    }

                    let guard_passed = if let Some(ref binding_name) = guard_matchable_binding {
                        // Compile-time resolved: use the pre-resolved Matchable instance binding.
                        crate::eval::call_to_match_resolved(
                            &guard_value,
                            binding_name,
                            &arm_env,
                            &ctx,
                            &match_span,
                        )
                        .await
                    } else {
                        // Type checking was skipped — fall back to dynamic dispatch.
                        crate::eval::call_to_match(&guard_value, &arm_env, &ctx, &match_span).await
                    };

                    if guard_passed {
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
                            match_span: match_span.clone(),
                        })));
                        Action::Continue(Ok(scrutinee_value))
                    }
                }
            }
        }

        // T-1140: Handle predicate pattern arm evaluation.
        // Receives the result of evaluating the predicate expression (or the predicate call).
        // If callable_invoked=false: the result is the predicate function; invoke it with scrutinee.
        // If callable_invoked=true: the result is Bool(true/false); dispatch accordingly.
        Cont::MatchPredicateCheck(data) => {
            let MatchPredicateCheckData {
                arm_idx,
                arms,
                env,
                ctx,
                match_span,
                scrutinee_value,
                body,
                callable_invoked,
                to_match_binding,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(predicate_value) => {
                    if !callable_invoked {
                        if let Value::Function { .. } | Value::Builtin(_) = &predicate_value {
                            // Build a PendingCall thunk: predicate(scrutinee).
                            // apply_predicate_to_subject appends the scrutinee as the last arg.
                            let call_thunk = apply_predicate_to_subject(
                                predicate_value,
                                scrutinee_value.clone(),
                                match_span.clone(),
                                match_span.clone(),
                                &env,
                                &ctx,
                            );
                            // Push MatchPredicateCheck again with callable_invoked=true to receive
                            // the call result, then return Materialize to drive the call
                            // iteratively through the CEK loop (no block_on_anywhere).
                            stack.push(Cont::MatchPredicateCheck(Box::new(
                                MatchPredicateCheckData {
                                    arm_idx,
                                    arms,
                                    env,
                                    ctx: Arc::clone(&ctx),
                                    match_span: match_span.clone(),
                                    scrutinee_value,
                                    body,
                                    callable_invoked: true,
                                    to_match_binding,
                                },
                            )));
                            return Action::Materialize {
                                thunk: call_thunk,
                                mat_span: Some(match_span),
                            };
                        }
                    }

                    let matched = if let Some(ref binding_name) = to_match_binding {
                        // Compile-time resolved: look up the pre-resolved Matchable instance
                        // binding directly, avoiding dynamic dispatch.
                        crate::eval::call_to_match_resolved(
                            &predicate_value,
                            binding_name,
                            &env,
                            &ctx,
                            &match_span,
                        )
                        .await
                    } else {
                        // Type checking was skipped — fall back to dynamic dispatch.
                        crate::eval::call_to_match(&predicate_value, &env, &ctx, &match_span).await
                    };

                    if matched {
                        // Predicate returned true — arm matches.
                        // If the arm also has a guard, evaluate it before accepting the match.
                        // Predicate patterns bind no variables, so use `env` (not arm_env).
                        if let Some(guard_expr) = &arms[arm_idx].guard {
                            let guard_binding =
                                arms[arm_idx].guard_matchable_binding.get().cloned();
                            stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                arm_idx,
                                arms: Arc::clone(&arms),
                                env: Arc::clone(&env),
                                ctx: Arc::clone(&ctx),
                                match_span: match_span.clone(),
                                arm_env: Arc::clone(&env), // no bindings from predicate pattern
                                scrutinee_value,
                                body,
                                callable_invoked: false,
                                guard_matchable_binding: guard_binding,
                            })));
                            return Action::EvalCore {
                                expr: Arc::clone(guard_expr),
                                env,
                                ctx,
                            };
                        }
                        Action::EvalCore {
                            expr: body,
                            env,
                            ctx,
                        }
                    } else {
                        // Predicate returned false (or non-Bool) — skip to next arm.
                        stack.push(Cont::MatchDispatch(Box::new(MatchDispatchData {
                            arm_idx: arm_idx + 1,
                            arms,
                            env,
                            ctx: Arc::clone(&ctx),
                            match_span: match_span.clone(),
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
                callable_invoked,
            } = *data;

            match result {
                Err(e) => Action::Continue(Err(e)),
                Ok(predicate_value) => {
                    // If the predicate is callable and we haven't yet invoked it, do so
                    // iteratively via the CEK machine rather than block_on_anywhere.
                    if !callable_invoked {
                        if let Value::Function { .. } | Value::Builtin(_) = &predicate_value {
                            // Build a PendingCall thunk for predicate(value) via the helper.
                            let call_thunk = apply_predicate_to_subject(
                                predicate_value,
                                value.clone(),
                                expr_span.clone(),
                                thunk_span.clone(),
                                &env,
                                &ctx,
                            );
                            // Push PredicateCheck again with callable_invoked=true to receive
                            // the call result, then return Materialize to drive the call
                            // iteratively through the CEK loop (no block_on_anywhere).
                            stack.push(Cont::PredicateCheck(Box::new(PredicateCheckData {
                                value,
                                annotation,
                                expr_span: expr_span.clone(),
                                thunk_span,
                                env,
                                ctx: Arc::clone(&ctx),
                                callable_invoked: true,
                            })));
                            return Action::Materialize {
                                thunk: call_thunk,
                                mat_span: Some(expr_span),
                            };
                        }
                    }

                    let pred_passed =
                        crate::eval::call_to_match(&predicate_value, &env, &ctx, &expr_span).await;

                    if pred_passed {
                        // Predicate passed — return the original value
                        Action::Continue(Ok(value))
                    } else {
                        // Predicate failed — check for default: or fail
                        if let Some(default_node) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively
                            Action::EvalCore {
                                expr: Arc::new(crate::lower::lower(default_node)),
                                env,
                                ctx: Arc::clone(&ctx),
                            }
                        } else {
                            // No default — fail with predicate failed error
                            let mut err = EvalError::type_assert_failed(
                                "_ (is: predicate failed)",
                                value.type_name(),
                                thunk_span.clone(),
                            )
                            .with_materialization_span(expr_span.clone());
                            if thunk_span != expr_span {
                                err = err
                                    .with_secondary_span(thunk_span.clone(), "value produced here");
                            }
                            Action::Continue(Err(err.into()))
                        }
                    }
                }
            }
        }
    }
}

/// Extract the set of binding-target names from a `[let ...]` expression for a 3-arg
/// `[case [let bindings] pattern body]` arm.
///
/// The `[let ...]` node is the first argument of the 3-arg CaseArm. It declares which
/// names in the pattern are binding targets (as opposed to pin-comparisons). Any name
/// that appears in the `[let ...]` list is a binding target; any other name used in the
/// pattern expression will be evaluated from the current environment and compared for
/// equality (pin semantics).
///
/// Returns a `HashSet<String>` of the binding names. The set is threaded into
/// `eval_case_arm_structural_pattern` to distinguish bind vs. pin at each name position.
fn extract_let_binding_names(let_decl: &CoreExpr) -> std::collections::HashSet<String> {
    let mut names = std::collections::HashSet::new();
    if let CoreExpr::LetDecl { bindings } = let_decl {
        for binding in bindings {
            match &binding.node {
                // lower_let_decl_binding converts declaration-position names to CoreExpr::Str.
                // The "_" wildcard is excluded — it binds nothing.
                CoreExpr::Str(name) if name != "_" => {
                    names.insert(name.clone());
                }
                // Both plain and annotated Var (Var { annotation: Some(_) }) use the name field.
                CoreExpr::Var { name, .. } if name != "_" => {
                    names.insert(name.clone());
                }
                _ => {}
            }
        }
    }
    names
}

/// Evaluate the structural pattern of a 3-arg `[case [let bindings] pattern body]` arm.
///
/// Returns `Ok(Some(env))` if the pattern matches, `Ok(None)` if it does not match,
/// or `Err(e)` if evaluating the pattern itself produces an error (e.g., unresolvable
/// pin reference, failed field-get for constructor tag). Errors must not be silently
/// converted to no-match — that produces misleading diagnostics.
async fn eval_case_arm_structural_pattern(
    pattern: &Arc<Spanned<CoreExpr>>,
    binding_set: &std::collections::HashSet<String>,
    scrutinee_value: &Value,
    env: &Arc<RwLock<Environment>>,
    match_span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Option<Arc<RwLock<Environment>>>> {
    let arm_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(env))));
    if eval_structural_pattern_inner(
        &pattern.node,
        binding_set,
        scrutinee_value,
        env,
        &arm_env,
        match_span.clone(),
        ctx,
    )
    .await?
    {
        // Bind any remaining names from binding_set that weren't bound by the pattern.
        // This ensures [case [let n] _ body] binds n = scrutinee when pattern is wildcard
        // or when the pattern doesn't reference every declared name.
        {
            let mut env_write = arm_env.write().unwrap();
            for name in binding_set {
                if name != "_" && !env_write.slot_names.iter().any(|n| n == name) {
                    let thunk = Arc::new(Thunk::new_materialized(
                        scrutinee_value.clone(),
                        match_span.clone(),
                    ));
                    env_write.insert(name.clone(), thunk);
                }
            }
        }
        Ok(Some(arm_env))
    } else {
        Ok(None)
    }
}

/// Recursive inner of `eval_case_arm_structural_pattern`.
///
/// Returns `Ok(true)` if the pattern matches, `Ok(false)` if it does not,
/// or `Err(e)` if pattern evaluation itself fails (unresolvable reference,
/// constructor tag cannot be determined, etc.).
///
/// Uses `Box::pin` for recursive async calls (the codebase does not depend on
/// `async_recursion`).
fn eval_structural_pattern_inner<'a>(
    pattern: &'a CoreExpr,
    binding_set: &'a std::collections::HashSet<String>,
    scrutinee_value: &'a Value,
    env: &'a Arc<RwLock<Environment>>,
    arm_env: &'a Arc<RwLock<Environment>>,
    match_span: Span,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<bool>> + 'a>> {
    Box::pin(async move {
        match pattern {
            // Wildcard: always succeeds, no binding.
            // Var { name: "_" } is the wildcard — CoreExpr::Str("_") is a string literal
            // and must do exact-value comparison (handled by the Str arm below).
            CoreExpr::Var { name, .. } if name == "_" => Ok(true),

            // Plain name: bind or pin based on binding_set.
            // level and slot carry de Bruijn coordinates for the pin case.
            // Plain name (no annotation): bind or pin based on binding_set.
            CoreExpr::Var {
                name,
                level,
                slot,
                annotation: None,
            } => {
                bind_or_pin_name(
                    name,
                    binding_set,
                    scrutinee_value,
                    env,
                    arm_env,
                    &match_span,
                    ctx,
                    *level,
                    *slot,
                )
                .await
            }

            // Annotated name (name@Type): the annotation may contain an "is:" predicate.
            // Bind/pin first, then check the predicate if present.
            CoreExpr::Var {
                name,
                level,
                slot,
                annotation: Some(annotation),
            } => {
                // First, perform the bind-or-pin operation
                let bind_result = bind_or_pin_name(
                    name,
                    binding_set,
                    scrutinee_value,
                    env,
                    arm_env,
                    &match_span,
                    ctx,
                    *level,
                    *slot,
                )
                .await?;

                if !bind_result {
                    return Ok(false);
                }

                // Check if annotation contains an "is:" property (predicate)
                if let Some(pred_surface_node) = annotation.node.get_property(IS_ANNOTATION_KEY) {
                    let pred_expr_core = Arc::new(crate::lower::lower(pred_surface_node));

                    let pred_thunk = Arc::new(Thunk::new_unevaluated_core(
                        pred_expr_core,
                        Arc::clone(arm_env),
                        Arc::clone(ctx),
                        match_span.clone(),
                    ));

                    let pred_value = materialize(&pred_thunk, Some(&match_span), ctx).await?;

                    let bound_value_thunk = {
                        let env_read = arm_env.read().unwrap();
                        if let Some(idx) = env_read.slot_names.iter().rposition(|n| n == name) {
                            Arc::clone(&env_read.slots[idx])
                        } else {
                            return Err(EvalError::internal(
                                format!(
                                    "pattern name '{name}' not bound after bind_or_pin succeeded"
                                ),
                                match_span,
                            )
                            .into());
                        }
                    };

                    let bound_value =
                        materialize(&bound_value_thunk, Some(&match_span), ctx).await?;

                    let pred_call_thunk = apply_predicate_to_subject(
                        pred_value,
                        bound_value,
                        match_span.clone(),
                        match_span.clone(),
                        arm_env,
                        ctx,
                    );

                    let pred_result = materialize(&pred_call_thunk, Some(&match_span), ctx).await?;

                    if !crate::eval::call_to_match(&pred_result, arm_env, ctx, &match_span).await {
                        return Ok(false);
                    }
                }

                Ok(bind_result)
            }

            // Literal patterns: compare exact value
            CoreExpr::Int(n) => Ok(matches!(scrutinee_value, Value::Int(v) if v == n)),
            CoreExpr::U64(n) => Ok(matches!(scrutinee_value, Value::U64(v) if v == n)),
            CoreExpr::Float(f) => Ok(matches!(scrutinee_value, Value::Float(v) if v == f)),
            CoreExpr::Str(s) => {
                // Not a wildcard (handled above): literal string comparison
                Ok(scrutinee_value.as_str().is_some_and(|v| v == s.as_str()))
            }

            // Call pattern: [EXPR arg ...]
            // Evaluate EXPR to get a value, then branch on the result:
            //   - Value::Variant{tag, ..} → constructor pattern: match scrutinee tag, bind payload
            //   - Value::Function | Builtin → guard/predicate: call with scrutinee, check truthy
            //   - Anything else → error
            // No AST-level heuristics needed — the runtime type of EXPR's value determines semantics.
            CoreExpr::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                // Evaluate the func expression to determine pattern semantics.
                let func_thunk = Arc::new(Thunk::new_unevaluated_core(
                    Arc::clone(func),
                    Arc::clone(arm_env),
                    Arc::clone(ctx),
                    match_span.clone(),
                ));
                let func_val = {
                    let mut v = materialize(&func_thunk, Some(&match_span), ctx).await?;
                    // Peel Value::Annotated — annotated constructors wrap their Variant.
                    while let Value::Annotated { inner, .. } = v {
                        v = *inner;
                    }
                    v
                };

                // Extract the constructor tag if func_val is either:
                //   (a) a unit Variant (payload: None) — the old unit-constructor form
                //   (b) a Function with return_ann: Some(Annotation::Simple(tag)) — named-field ctor
                let ctor_tag_opt: Option<String> = match &func_val {
                    Value::Variant { tag, .. } => Some(tag.clone()),
                    Value::Function {
                        return_ann: Some(ann),
                        ..
                    } => {
                        if let crate::ast::Annotation::Simple(tag) = &ann.node {
                            Some(tag.clone())
                        } else {
                            None
                        }
                    }
                    _ => None,
                };

                if let Some(ctor_tag) = ctor_tag_opt {
                    // Constructor pattern: match scrutinee tag, bind payload.
                    // Peel annotations from scrutinee — annotated unit constructors wrap
                    // their inner Variant in Value::Annotated (e.g. @[doc:"..."] constructors).
                    let scrutinee_value = {
                        let mut v = scrutinee_value;
                        while let Value::Annotated { inner, .. } = v {
                            v = inner.as_ref();
                        }
                        v
                    };
                    let Value::Variant {
                        tag: scrutinee_tag,
                        payload,
                    } = scrutinee_value
                    else {
                        return Ok(false);
                    };
                    if scrutinee_tag != &ctor_tag {
                        return Ok(false);
                    }

                    // Named args in pattern: `[Ctor path: x handle: y]`
                    // These are field bindings, not function call arguments.
                    // Extract payload[field_name] and bind/match each.
                    if !named_args.is_empty() {
                        let Some(payload_id) = payload else {
                            return Ok(false);
                        };
                        let payload_thunk = ctx.get_thunk(*payload_id);
                        let payload_val =
                            materialize(&payload_thunk, Some(&match_span), ctx).await?;
                        let Value::Dict(payload_map) = &payload_val else {
                            return Ok(false);
                        };

                        for na in named_args.iter() {
                            let field_key = HashableValue::Str(na.node.name.clone().into());
                            let Some(field_thunk_id) = payload_map.get(&field_key) else {
                                return Ok(false);
                            };
                            let field_thunk = ctx.get_thunk(*field_thunk_id);
                            let field_val =
                                materialize(&field_thunk, Some(&match_span), ctx).await?;

                            if !eval_structural_pattern_inner(
                                &na.node.value.node,
                                binding_set,
                                &field_val,
                                env,
                                arm_env,
                                match_span.clone(),
                                ctx,
                            )
                            .await?
                            {
                                return Ok(false);
                            }
                        }
                        return Ok(true);
                    }

                    // Positional args handling (no named_args).
                    if args.is_empty() {
                        return Ok(true);
                    }

                    let Some(payload_id) = payload else {
                        return Ok(false);
                    };

                    let payload_thunk = ctx.get_thunk(*payload_id);
                    let payload_val = materialize(&payload_thunk, Some(&match_span), ctx).await?;

                    if args.len() == 1 {
                        return eval_structural_pattern_inner(
                            &args[0].node,
                            binding_set,
                            &payload_val,
                            env,
                            arm_env,
                            match_span,
                            ctx,
                        )
                        .await;
                    }

                    // Multi-arg: match positional args against payload dict integer keys.
                    let Value::Dict(payload_map) = &payload_val else {
                        return Ok(false);
                    };

                    for (idx, arg) in args.iter().enumerate() {
                        let field_key = HashableValue::Int(idx as i64);
                        let Some(field_thunk_id) = payload_map.get(&field_key) else {
                            return Ok(false);
                        };
                        let field_thunk = ctx.get_thunk(*field_thunk_id);
                        let field_val = materialize(&field_thunk, Some(&match_span), ctx).await?;

                        if !eval_structural_pattern_inner(
                            &arg.node,
                            binding_set,
                            &field_val,
                            env,
                            arm_env,
                            match_span.clone(),
                            ctx,
                        )
                        .await?
                        {
                            return Ok(false);
                        }
                    }
                    Ok(true)
                } else {
                    match func_val {
                        Value::Function { .. } | Value::Builtin(_) => {
                            // Guard/predicate: bind all declared names to scrutinee, evaluate
                            // the full call expression (func + args), check if result is truthy.
                            for name in binding_set {
                                let scrutinee_thunk = Arc::new(Thunk::new_materialized(
                                    scrutinee_value.clone(),
                                    match_span.clone(),
                                ));
                                arm_env
                                    .write()
                                    .unwrap()
                                    .insert(name.clone(), scrutinee_thunk);
                            }

                            let guard_expr_spanned = Arc::new(crate::ast::Spanned::new(
                                CoreExpr::Call {
                                    func: Arc::clone(func),
                                    args: args.to_vec(),
                                    named_args: named_args.to_vec(),
                                    implied: *implied,
                                },
                                match_span.clone(),
                            ));
                            let guard_thunk = Arc::new(Thunk::new_unevaluated_core(
                                guard_expr_spanned,
                                Arc::clone(arm_env),
                                Arc::clone(ctx),
                                match_span.clone(),
                            ));
                            let guard_result =
                                materialize(&guard_thunk, Some(&match_span), ctx).await?;
                            Ok(
                                crate::eval::call_to_match(
                                    &guard_result,
                                    arm_env,
                                    ctx,
                                    &match_span,
                                )
                                .await,
                            )
                        }

                        other => Err(EvalError::type_mismatch_ctx(
                            "pattern call".to_string(),
                            "Variant (constructor) or Function (predicate)",
                            other.type_name(),
                            match_span,
                        )
                        .into()),
                    }
                }
            }

            // Dict pattern: match fields of the scrutinee dict
            CoreExpr::Dict(entries) => {
                let Value::Dict(scrutinee_map) = scrutinee_value else {
                    return Ok(false);
                };
                for entry in entries {
                    let key = match entry.node.key.as_ref().map(|k| &k.node) {
                        Some(CoreExpr::Str(s)) => HashableValue::Str(s.clone().into()),
                        Some(CoreExpr::Int(n)) => HashableValue::Int(*n),
                        _ => {
                            return Err(EvalError::internal(
                                "pattern: dynamic key in dict pattern is not supported".to_string(),
                                match_span,
                            )
                            .into())
                        }
                    };
                    let Some(field_thunk_id) = scrutinee_map.get(&key) else {
                        return Ok(false); // Required field missing — genuine no-match
                    };
                    let field_thunk = ctx.get_thunk(*field_thunk_id);
                    let field_val = materialize(&field_thunk, Some(&match_span), ctx).await?;
                    if !eval_structural_pattern_inner(
                        &entry.node.value.node,
                        binding_set,
                        &field_val,
                        env,
                        arm_env,
                        match_span.clone(),
                        ctx,
                    )
                    .await?
                    {
                        return Ok(false);
                    }
                }
                Ok(true)
            }

            // Fallback: evaluate the pattern expression and compare with scrutinee.
            _ => {
                let spanned = Arc::new(crate::ast::Spanned::new(
                    pattern.clone(),
                    match_span.clone(),
                ));
                let pat_expr_thunk = Arc::new(Thunk::new_unevaluated_core(
                    spanned,
                    Arc::clone(env),
                    Arc::clone(ctx),
                    match_span.clone(),
                ));
                let pat_val = materialize(&pat_expr_thunk, Some(&match_span), ctx).await?;
                Ok(primitive_eq(pat_val, scrutinee_value.clone()))
            }
        }
    })
}

/// Implement bind-or-pin at a single name position in a 3-arg structural pattern.
///
/// - If `name` is in `binding_set`: insert a new_materialized thunk into `arm_env`
/// - If `name` is NOT in `binding_set`: look up `name` in `env` via de Bruijn coordinates
///   (`pin_level`, `pin_slot`), compare with `primitive_eq`; return false (soft skip) if not equal
///
/// `pin_level` and `pin_slot` are the de Bruijn coordinates of `name` in the enclosing scope.
/// `u32::MAX` for both means no resolver coordinates were available (resolver error) —
/// this is a resolver error and propagates as Err.
async fn bind_or_pin_name(
    name: &str,
    binding_set: &std::collections::HashSet<String>,
    scrutinee_value: &Value,
    env: &Arc<RwLock<Environment>>,
    arm_env: &Arc<RwLock<Environment>>,
    match_span: &Span,
    ctx: &Arc<EvalContext>,
    pin_level: u32,
    pin_slot: u32,
) -> EvalResult<bool> {
    if binding_set.contains(name) {
        // Bind: associate name with the scrutinee value in arm_env
        let thunk = Arc::new(Thunk::new_materialized(
            scrutinee_value.clone(),
            match_span.clone(),
        ));
        arm_env.write().unwrap().insert(name.to_string(), thunk);
        Ok(true)
    } else {
        // Pin: look up name via de Bruijn coordinates in env, compare with scrutinee.
        if pin_level == u32::MAX || pin_slot == u32::MAX {
            return Err(EvalError::internal(
                format!("pattern pin '{name}': no resolver coordinates (annotation without binding declaration?)"),
                match_span.clone(),
            ).into());
        }
        let pin_thunk = match env.read().unwrap().get_slot(pin_level, pin_slot) {
            Some(t) => t,
            None => {
                return Err(EvalError::internal(
                    format!("pattern pin '{name}': not found in runtime environment at level {pin_level}, slot {pin_slot}"),
                    match_span.clone(),
                ).into());
            }
        };
        let pin_val = materialize(&pin_thunk, Some(match_span), ctx).await?;
        Ok(primitive_eq(pin_val, scrutinee_value.clone()))
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
                action = match eval_core_expr(&expr, &env, &action_ctx).await {
                    Ok(thunk) => match thunk.try_get_materialized() {
                        Some(value) => Action::Continue(Ok(value)),
                        None => Action::Materialize {
                            thunk,
                            mat_span: Some(expr.span.clone()),
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
    use crate::value::{Environment, Thunk};
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

    /// Async shadow of `materialize()` for test contexts.
    async fn materialize(
        thunk: &crate::value::Thunk,
        mat_span: Option<&crate::ast::Span>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::eval::materialize(thunk, mat_span, ctx).await
    }

    /// Async shadow of `run()` for test contexts.
    async fn run(initial: Action, ctx: &Arc<EvalContext>) -> crate::error::EvalResult<Value> {
        super::run(initial, ctx).await
    }

    #[tokio::test]
    async fn test_restore_state_pending_builtin() {
        use crate::value::BuiltinFn;

        let span = test_span(1, 1, 1, 10);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));

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

        let caller_env = empty_env();
        let pending_thunk = Thunk::new_pending_builtin(
            dummy_def,
            args.clone(),
            None,
            span.clone(),
            Some(Arc::from("test_origin")),
            Arc::clone(&caller_env),
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
            caller_env,
            ctx: ctx.clone(),
        };
        restore.restore(&pending_thunk);

        // Verify state is restored
        assert!(
            pending_thunk.peek_builtin_def().is_some(),
            "Expected PendingBuiltin state (peek_builtin_def should return Some)"
        );
    }

    #[tokio::test]
    async fn test_restore_state_core_expr() {
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
        let func_thunk = Arc::new(Thunk::new_materialized(Value::Int(1), span.clone()));
        let thunk = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            span.clone(),
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

    #[tokio::test]
    async fn test_core_expr_restore_preserves_state() {
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

        let func_thunk = Arc::new(Thunk::new_materialized(Value::Int(1), span.clone()));
        let thunk = Arc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            span.clone(),
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

    #[tokio::test]
    async fn test_attach_materialization_context_adds_frame() {
        let thunk_span = test_span(1, 1, 1, 10);
        let err = EvalError::undefined_variable("x".to_string(), thunk_span.clone());
        let mat_span = test_span(10, 5, 10, 6);
        let origin = "test_origin";

        let decorated = attach_materialization_context(
            err.into(),
            Some(&mat_span),
            Some(origin),
            thunk_span.clone(),
        );

        // Verify materialization_span is set
        assert_eq!(decorated.materialization_span, Some(mat_span));

        // Verify origin frame is added
        assert!(
            decorated
                .stack
                .iter()
                .any(|f| f.label == origin && f.definition_span == thunk_span),
            "Expected origin frame with label '{}' and thunk_span, but stack frames were: {:?}",
            origin,
            decorated.stack
        );
    }

    #[tokio::test]
    async fn test_guarded_type_assertion_failure_has_secondary_span() {
        // Test that when a Guarded type assertion fails, the error includes
        // a secondary_span pointing to where the value was produced (if different
        // from the assertion site).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        // Create a simple expression that produces an Int
        let value_span = test_span(5, 1, 5, 3); // Line 5: the value production site
        let value_thunk = crate::value::Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), value_span.clone())),
            test_env(),
            test_ctx(),
            value_span.clone(),
        );

        // Create a Guarded thunk that expects String but wraps the Int
        let expected_type = Type::Str;
        let guard_span = test_span(10, 1, 10, 20); // Line 10: the assertion site
        let guarded = crate::value::Thunk::new_guarded(
            Arc::new(value_thunk),
            expected_type,
            Vec::new(),
            guard_span.clone(),
        );

        // Try to materialize - should fail
        let ctx = test_ctx();
        let result = materialize(&guarded, Some(&guard_span), &ctx).await;

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

    #[tokio::test]
    async fn test_guarded_secondary_span_suppressed_when_same_as_definition() {
        // Test that when the value production site is the same as the assertion site,
        // secondary_span is NOT set (would be redundant).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let same_span = test_span(1, 1, 1, 10);

        // Create a value at the same location as the guard
        let value_thunk = crate::value::Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), same_span.clone())),
            test_env(),
            test_ctx(),
            same_span.clone(),
        );

        // Create a Guarded thunk with the same span for both guard and inner
        let guarded = crate::value::Thunk::new_guarded(
            Arc::new(value_thunk),
            Type::Str,
            Vec::new(),
            same_span.clone(), // guard_span
        );

        let ctx = test_ctx();
        let result = materialize(&guarded, Some(&same_span), &ctx).await;

        assert!(result.is_err());
        let err = result.unwrap_err();

        // Secondary span should NOT be set because it would be the same as definition_span
        assert!(
            err.secondary_span.is_none(),
            "Secondary span should be suppressed when same as definition span"
        );
    }

    #[tokio::test]
    async fn test_cont_memoize_caches_result() {
        // Test that Cont::Memoize caches the materialization result into the parent thunk.
        // Create an Unevaluated thunk, force it via the CEK machine (run), and verify
        // it transitions to Materialized state with the correct cached value.
        let span = test_span(1, 1, 1, 10);
        let env = empty_env();
        let ctx = test_ctx();

        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
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
        )
        .await;

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
        )
        .await;
        assert_eq!(result2.unwrap(), Value::Int(42));
    }

    #[tokio::test]
    async fn test_cont_memoize_caches_error_in_failed_state() {
        // Test that when a thunk errors during materialization, the error is cached
        // in Failed state and subsequent materializations return the cached error.
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Create a thunk that will fail: reference a variable with no resolution entry.
        // slot u32::MAX is out of bounds — get_slot returns None → undefined variable error.
        let thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                span.clone(),
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
        )
        .await;

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
        )
        .await;
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

    #[tokio::test]
    async fn test_error_propagation_through_continuation() {
        // Test that errors propagate correctly through the continuation stack.
        // Force a dict thunk that contains an error-producing value and verify
        // the error propagates correctly through the materialization machinery.
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Create a dict with an entry that will error when materialized
        let error_thunk = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                span.clone(),
            )),
            Arc::clone(&env),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Directly materialize the error thunk — should produce an undefined variable error.
        let result = crate::eval::materialize(&error_thunk, None, &ctx).await;

        // Verify the error propagated
        assert!(
            result.is_err(),
            "Expected error to propagate through continuation"
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

    #[tokio::test]
    async fn test_guarded_validate_success_materializes_thunk() {
        // Branch 1: inner value matches expected type.
        // A Guarded thunk wrapping an Int value with an Int type expectation
        // should succeed and leave the thunk in Materialized state.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Inner thunk: an Int value that satisfies the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));

        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
        ));

        let result = materialize(&guarded, None, &ctx).await;
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

    #[tokio::test]
    async fn test_guarded_validate_failure_with_default_evaluates_default_in_caller_env() {
        // Branch 2: inner value fails type check but a default expression is present.
        // The default expression should be evaluated in the caller's environment,
        // and the thunk should memoize the default result.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();
        let env = empty_env();

        // Bind a variable in caller's env so the default expr can reference it.
        let fallback_thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span.clone()));
        env.write()
            .unwrap()
            .insert("fallback_val".into(), fallback_thunk);

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(
            crate::value::string_val("not an int"),
            span.clone(),
        ));

        // Default expression: a variable reference to `fallback_val` at slot 0 in this env.
        let default_expr = Arc::new(sp(CoreExpr::Var {
            name: "fallback_val".to_string(),
            level: 0,
            slot: 0,
            annotation: None,
        }));

        let guarded = Arc::new(Thunk::new_guarded_full(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
            None,
            Some((default_expr, Arc::clone(&env))),
        ));

        // Should succeed, returning the default value (99) evaluated in caller's env.
        let result = materialize(&guarded, None, &ctx).await;
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

    #[tokio::test]
    async fn test_guarded_validate_failure_without_default_propagates_error() {
        // Branch 3: inner value fails type check and no default is present.
        // The error should propagate to the caller and the thunk should cache the
        // failure (transition to Failed state) so subsequent access returns the
        // cached error without re-running the guard.
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        // Inner thunk: a Float value — fails the Int guard.
        let inner = Arc::new(Thunk::new_materialized(Value::Float(1.0), span.clone()));

        let guarded = Arc::new(Thunk::new_guarded(
            Arc::clone(&inner),
            Type::Int,
            vec![],
            span,
        ));

        // First materialization: guard fires, Float ≠ Int → type assertion failure.
        let result1 = materialize(&guarded, None, &ctx).await;
        assert!(
            result1.is_err(),
            "Float value should fail Int guard (no default), but got success"
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
        let result2 = materialize(&guarded, None, &ctx).await;
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
    #[tokio::test]
    async fn test_builtin_force_arg_cek_forces_arg_before_dispatch() {
        use crate::value::{BuiltinDef, BuiltinFn, Strictness};

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Create an unevaluated arg thunk: evaluates to an empty dict.
        // `CoreExpr::Dict(vec![])` produces `Value::Dict(IndexMap::new())`.
        let unevaluated_arg = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span.clone())),
            empty_env(),
            Arc::clone(&ctx),
            span.clone(),
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
            empty_env(),
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
        )
        .await;

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
    #[tokio::test]
    async fn test_builtin_force_arg_cek_force_count_two() {
        use crate::value::{BuiltinDef, BuiltinFn, Strictness};

        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Arg0: unevaluated dict (will be forced and used by builtin_keys).
        let unevaluated_arg0 = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span.clone())),
            empty_env(),
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Arg1: unevaluated int (will be force-materialized but not used by builtin_keys).
        let unevaluated_arg1 = Arc::new(Thunk::new_unevaluated_core(
            Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
            empty_env(),
            Arc::clone(&ctx),
            span.clone(),
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
            Box::pin(async move { Ok(Arc::new(Thunk::new_materialized(Value::Int(1), span))) })
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
            empty_env(),
            Arc::clone(&ctx),
        ));

        // Force via CEK — exercises BuiltinForceArg loop for both positions.
        let result = run(
            Action::Materialize {
                thunk: Arc::clone(&outer_thunk),
                mat_span: None,
            },
            &ctx,
        )
        .await;

        assert!(
            result.is_ok(),
            "BuiltinForceArg CEK must force both args when force_count=2; got: {:?}",
            result.unwrap_err()
        );
        assert_eq!(
            result.unwrap(),
            Value::Int(1),
            "dummy builtin must succeed with both args pre-materialized"
        );
    }
}

#[cfg(test)]
mod deep_tests {
    use super::*;
    use crate::test_util::test_span;

    #[tokio::test]
    async fn test_attach_materialization_context_preserves_spans() {
        // Test that attach_materialization_context correctly adds materialization
        // span and origin frame to errors.
        //
        // This is already tested by test_attach_materialization_context_adds_frame,
        // but we add a variant that tests the preservation of existing spans
        // (the "if err.materialization_span.is_none()" branch).

        let thunk_span = test_span(1, 1, 1, 10);
        let err = EvalError::undefined_variable("x".to_string(), thunk_span.clone());
        let mat_span = test_span(10, 5, 10, 6);
        let origin = "test_origin";

        // First attachment — should set materialization_span
        let decorated = attach_materialization_context(
            Box::new(err),
            Some(&mat_span),
            Some(origin),
            thunk_span.clone(),
        );

        assert_eq!(
            decorated.materialization_span,
            Some(mat_span.clone()),
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
}

// ============================================================================
// Utility functions for macro expansion
// ============================================================================

/// Recursively materialize all structured container values in a value tree.
/// Used by `to-tinct` serialization and macro expansion to ensure nested values
/// are pre-materialized before processing (e.g., `dict_to_surface_node` expects
/// all values reachable via `try_get_materialized`).
///
/// Handles four container types:
/// - `Value::Dict` — materializes all entry thunks and recurses into each value
/// - `Value::Variant` — materializes the payload thunk and recurses into it
/// - `Value::Variant` (nominal types including Seq) — handled by the Variant arm above
/// - `Value::Overlay` — flattens and recurses (same as Dict path after flatten)
///
/// All other value types (Int, String, Float, Bool, Function, Channel, ReactiveCell,
/// BroadcastChannel, OneshotSender, OneshotReceiver, etc.) are returned as-is
/// without further recursion.
///
/// Does NOT preserve sharing (may duplicate shared structures).
/// Uses cycle detection to avoid infinite loops.
///
/// Exported for use by:
/// - `expand_macro_call_surface` in expand.rs (Expr.* typed macro results)
/// - `builtin_to_tinct` in stream.rs (pre-force all nested values before SCN serialization)
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
                    let deep_thunk =
                        Arc::new(Thunk::new_materialized(deep_val, thunk.span.clone()));
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
                    let deep_thunk = Arc::new(Thunk::new_materialized(
                        deep_payload,
                        payload_thunk.span.clone(),
                    ));
                    let deep_id = ctx.alloc_thunk(deep_thunk);
                    Ok(Value::Variant {
                        tag: tag.clone(),
                        payload: Some(deep_id),
                    })
                } else {
                    Ok(val.clone())
                }
            }
            // Nominal variants (including Seq) handled by the Variant arm above.
            Value::Overlay(left, right) => {
                // Flatten the overlay to a dict, then recurse on the result
                let flattened_map =
                    flatten_overlay(left, right, "force_dict_tree", ctx, rust_span!()).await?;
                let dict_val = Value::Dict(flattened_map);
                force_dict_tree_impl(&dict_val, ctx, visited).await
            }
            // Primitives and other types are already fully materialized.
            // Includes: Int, Float, Bool, String, Function, Builtin, DirCap, NetCap,
            // File, RevocableDirCap, Decimal, BigInt, Bytes, Uri, Timestamp,
            // Duration, ClockCap, Timezone, QuicSession, Http2Session, Http3Session,
            // QuicDatagramHandle, DatagramHandle, Program, Document, Builder, Proxy.
            // Note: Handle and WriteHandle were removed in the File redesign sprint.
            // Expr.* variants (Value::Variant with tag starting with "Expr.") are handled above
            // by the Dict recursion path — their payload dicts are forced recursively.
            _ => Ok(val.clone()),
        }
    })
}
