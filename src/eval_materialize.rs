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
    as_record_row_merged, format_expected_label, format_field_path, format_got_label,
    format_type_for_assert, match_pattern, materialize, primitive_eq, validate_and_wrap_record,
    value_matches_type, EvalContext, DEFAULT_ANNOTATION_KEY, IS_ANNOTATION_KEY,
};
use crate::eval_call::{invoke_function, invoke_function_tco, CallContext};
use crate::eval_core::eval_core_expr;
use crate::rust_span;
use crate::types::Type;
use crate::value::{
    string_val, HashableValue, Thunk, ThunkId, ThunkState, UnevaluatedState, Value,
};

tokio::task_local! {
    pub(crate) static TASK_EVAL_STACK: std::cell::RefCell<Vec<(std::sync::Arc<str>, crate::ast::Span)>>;
}

/// RAII guard for profiling spans. Automatically closes the span on drop.
struct ProfilingSpanGuard {
    profiling: Option<Arc<Mutex<crate::profiling::ProfilingCollector>>>,
    span_id: Option<u64>,
}

impl ProfilingSpanGuard {
    fn new(ctx: &Arc<EvalContext>, thunk: &Thunk) -> Self {
        let (profiling, span_id) = if let Some(ref prof) = ctx.profiling {
            // Extract span source information.
            // Span carries file: Arc<SourceFile>. Use the embedded path when it is a
            // real source file (not a synthetic span like <parse> or <origin>).
            let source_file: Option<String> = {
                let sf = &thunk.span.file;
                if !sf.path.starts_with('<') {
                    Some(sf.path.as_ref().to_string())
                } else {
                    None
                }
            };
            let (source_start, source_end) = if thunk.span != rust_span!() {
                (
                    Some((thunk.span.start.line, thunk.span.start.column)),
                    Some((thunk.span.end.line, thunk.span.end.column)),
                )
            } else {
                (None, None)
            };

            // Extract source text snippet from the embedded SourceFile content.
            let source_text: Option<String> = {
                let sf = &thunk.span.file;
                if !sf.path.starts_with('<') && !sf.content.is_empty() {
                    let content = sf.content.as_ref();
                    let start_byte = thunk.span.start.offset;
                    let end_byte = thunk.span.end.offset.min(content.len());
                    if start_byte < end_byte {
                        let snippet = &content[start_byte..end_byte];
                        // Keep first 60 chars; replace internal newlines with spaces
                        let truncated: String = snippet
                            .chars()
                            .take(60)
                            .map(|c| if c == '\n' { ' ' } else { c })
                            .collect();
                        Some(truncated)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            let (builtin_name, origin_builtin) = match &thunk.span.name {
                Some(name) if name.starts_with("builtin-") => (Some(name.to_string()), None),
                Some(name) => (None, Some(name.to_string())),
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

/// Type alias for the optional default expression + FlatEnv environment id pair carried by
/// guarded thunks. The u32 is an EnvId index into EvalContext.scope_arena. Matches value.rs
/// GuardDefault, enabling lossless round-trip through UnevaluatedState::Guarded.
type GuardDefault = (
    Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
    u32, // env_id into EvalContext.scope_arena
);

/// Collect all lower errors into a single EvalError, combining their messages.
/// Returns None if there are no errors.
pub fn lower_errors_to_eval_error(
    diags: Vec<crate::lower::LowerDiagnostic>,
) -> Option<Box<EvalError>> {
    let errors: Vec<_> = diags
        .into_iter()
        .filter(|d| matches!(d.kind, crate::lower::LowerDiagnosticKind::Error))
        .collect();
    if errors.is_empty() {
        return None;
    }
    let fmt_loc = |diag: &crate::lower::LowerDiagnostic| -> String {
        let sf = &diag.span.file;
        if !sf.path.starts_with('<') {
            format!(
                " (at {}:{}:{})",
                sf.path, diag.span.start.line, diag.span.start.column
            )
        } else {
            String::new()
        }
    };
    let first = &errors[0];
    let mut msg = format!("{}{}", first.message, fmt_loc(first));
    for extra in &errors[1..] {
        msg.push('\n');
        msg.push_str(&extra.message);
        msg.push_str(&fmt_loc(extra));
    }
    Some(EvalError::user_error(msg, first.span.clone()).into())
}

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
        // spans[0] is always the primary span. spans[1] is the first note span (typically
        // the materialization site). If no note span has been added yet, push this mat span.
        let first_note = err.spans.get(1).map(|(s, _)| s);
        if first_note.is_none() {
            err = Box::new(err.with_materialization_span(span.clone()));
        } else if first_note != Some(span) && !err.stack.iter().any(|f| f.definition_span == *span)
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

/// Payload for Cont::Memoize. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MemoizeData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
}

/// Payload for Cont::PendingCallDispatch. Boxed to keep the Cont enum ≤96 bytes.
///
/// After T-1557: args/named use ThunkId; caller_env_id is u32 into EvalContext.scope_arena.
pub(crate) struct PendingCallDispatchData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) args: Vec<ThunkId>,
    pub(crate) named: Option<Box<IndexMap<String, ThunkId>>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env_id: u32,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) origin: Option<Arc<str>>,
    pub(crate) thunk_span: Span,
    pub(crate) mat_span: Option<Span>,
    pub(crate) original_call: Arc<Spanned<CoreExpr>>,
    pub(crate) tail_hint: bool,
    /// Span of the function thunk — where the callee value was defined.
    /// Used as the primary span in not-a-function errors ("defined at").
    pub(crate) func_span: Span,
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
    /// FlatEnv env_id — for evaluating default: fallback expressions.
    pub(crate) env_id: u32,
    pub(crate) ctx: Arc<EvalContext>,
    /// Pipeline blame for `--- expects: @Type` contract assertions.
    /// Carried from `CoreExpr::TypeAssert::pipeline_blame` (set during expects annotation resolution).
    /// None for user-written `[@Type expr]` annotations.
    pub(crate) pipeline_blame: Option<crate::error::PipelineBlame>,
}

/// Payload for Cont::BuiltinForceArg. Boxed to keep the Cont enum ≤96 bytes.
///
/// After T-1557: args/named use ThunkId; caller_env_id is u32 into EvalContext.scope_arena.
pub(crate) struct BuiltinForceArgData {
    pub(crate) thunk: Arc<Thunk>,
    pub(crate) def: crate::value::BuiltinDef,
    pub(crate) args: Vec<ThunkId>,
    pub(crate) named: Option<IndexMap<String, ThunkId>>,
    pub(crate) call_span: Span,
    pub(crate) caller_env_id: u32,
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
    /// FlatEnv env_id — current scope for evaluating the next expression.
    pub(crate) env_id: u32,
    /// Arena length before evaluating exprs[idx]. If the expression allocated new FlatEnvs,
    /// the first new FlatEnv is at index `arena_len_before` — that is the intermediate dict's
    /// letrec root scope. Used by T-1558 to advance env_id for the next expression.
    pub(crate) arena_len_before: u32,
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
    /// FlatEnv env_id — current scope for evaluating subsequent expressions.
    pub(crate) env_id: u32,
    /// Arena length before evaluating this expression — same as SequentialStepData.arena_len_before.
    pub(crate) arena_len_before: u32,
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
    /// The original environment for matching (legacy Env chain; FlatEnv scope used via env_id). B-515: transitional.
    pub(crate) env: Arc<RwLock<crate::env::Env>>,
    pub(crate) env_id: u32,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) match_span: Span,
}

/// Payload for Cont::MatchGuardCheck. Boxed to keep the Cont enum ≤96 bytes.
pub(crate) struct MatchGuardCheckData {
    /// Current arm index (for continuing to next arm if guard fails)
    pub(crate) arm_idx: usize,
    pub(crate) arms: Arc<Vec<crate::ast::CoreMatchArm>>,
    /// FlatEnv env_id — the original environment for fallback matching.
    pub(crate) env_id: u32,
    pub(crate) ctx: Arc<EvalContext>,
    pub(crate) match_span: Span,
    /// Arm scope environment with pattern bindings (legacy Env chain; FlatEnv scope via env_id). B-515: transitional.
    pub(crate) arm_env: Arc<RwLock<crate::env::Env>>,
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
    /// FlatEnv env_id — environment for evaluating the default expression if predicate fails.
    pub(crate) env_id: u32,
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
    armed: bool,
}

impl EvalStackGuard {
    /// Push an entry onto the eval_stack and create a guard that will pop on drop.
    fn push(entry: (Arc<str>, Span)) -> Self {
        let _ = TASK_EVAL_STACK.try_with(|s| s.borrow_mut().push(entry));
        EvalStackGuard { armed: true }
    }

    /// Create a guard for an inherited eval_stack entry (no push, but will pop on drop).
    ///
    /// Used in `apply_cont` handlers where the eval_stack entry was pushed by a prior
    /// `force_step` call (e.g., `PendingCallDispatch` inherits from PendingCall's push,
    /// `BuiltinForceArg` inherits from PendingBuiltin's push, `Memoize` inherits from
    /// any pusher).
    fn inherited() -> Self {
        EvalStackGuard { armed: true }
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
            let _ = TASK_EVAL_STACK.try_with(|s| s.borrow_mut().pop());
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
        /// FlatEnv env_id — passed as parameter to eval_core_expr.
        env_id: u32,
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
/// builtin-typecheck-doc). The `call_span` argument is used as the creation span for the
/// materialized thunks that hold each field value.
///
/// `file` is the path string if present, or `[]` (empty dict) when no source file is known.
pub(crate) fn make_span_dict(
    span: &crate::ast::Span,
    ctx: &Arc<EvalContext>,
    call_span: &crate::ast::Span,
) -> ThunkId {
    let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
    let mut w = indexmap::IndexMap::new();
    w.insert(
        HashableValue::Str("file".into()),
        alloc({
            let sf = &span.file;
            if !sf.path.starts_with('<') {
                string_val(sf.path.as_ref())
            } else {
                Value::Dict(indexmap::IndexMap::new())
            }
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
        HashableValue::Str("start-offset".into()),
        alloc(Value::Int(span.start.offset as i64)),
    );
    w.insert(
        HashableValue::Str("end-line".into()),
        alloc(Value::Int(span.end.line as i64)),
    );
    w.insert(
        HashableValue::Str("end-col".into()),
        alloc(Value::Int(span.end.column as i64)),
    );
    w.insert(
        HashableValue::Str("end-offset".into()),
        alloc(Value::Int(span.end.offset as i64)),
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
    env: &Arc<RwLock<crate::env::Env>>,
    env_id: u32,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    let subject_thunk = Arc::new(Thunk::value(subject, subj_span));
    let pred_thunk = Arc::new(Thunk::value(predicate, pred_span.clone()));
    let subject_id = ctx.alloc_thunk(0, subject_thunk);
    let pred_id = ctx.alloc_thunk(0, pred_thunk);
    // Uses env_id as caller_env_id (B-515: case arm FlatEnv allocation pending).
    // The env parameter is retained for call_to_match compatibility (B-515 tracks full FlatEnv migration).
    let _ = env;
    Arc::new(Thunk::fn_call(
        pred_id,
        vec![subject_id],
        IndexMap::new(),
        pred_span.clone(),
        env_id,
        pred_span.clone(),
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
    let _profile_guard = ProfilingSpanGuard::new(ctx, thunk);

    loop {
        match thunk.state() {
            ThunkState::Materialized(v) => return Action::Continue(Ok(v)),

            ThunkState::Failed(e) => {
                let mut cloned = Box::new((*e).clone());
                if let Some(ref span) = mat_span {
                    let first_note = cloned.spans.get(1).map(|(s, _)| s);
                    if first_note.is_none() {
                        cloned = Box::new(cloned.with_materialization_span(span.clone()));
                    } else if first_note != Some(span)
                        && !cloned.stack.iter().any(|f| f.definition_span == *span)
                    {
                        cloned.push_frame("materialized".to_string(), span.clone());
                    }
                }
                return Action::Continue(Err(cloned));
            }

            ThunkState::InProgress { evaluating_task } => {
                let same = match (evaluating_task, tokio::task::try_id()) {
                    (Some(e), Some(c)) => e == c,
                    _ => true,
                };
                if same {
                    let cycle_path = TASK_EVAL_STACK
                        .try_with(|s| s.borrow().clone())
                        .unwrap_or_default();
                    let label = thunk.span.name.as_deref().unwrap_or("thunk");
                    let mut err =
                        EvalError::circular_dependency(label, thunk.span.clone(), cycle_path);
                    if let Some(ref span) = mat_span {
                        err = err.with_materialization_span(span.clone());
                    }
                    let err_boxed: Box<EvalError> = err.into();
                    thunk.settle(Err(Arc::new((*err_boxed).clone())));
                    return Action::Continue(Err(err_boxed));
                }
                thunk.settled().await;
                // Loop and re-read state
            }

            ThunkState::Unevaluated => {
                if let Some(state) = thunk.try_claim() {
                    // Won the claim — evaluate inline (same task, no spawn)
                    let env_id = state.initial_env_id();
                    return dispatch_state(state, thunk, stack, ctx, env_id).await;
                }
                // Lost race — loop and re-read state
            }
        }
    }
}

/// Convert a pre-claimed UnevaluatedState to the initial Action for the CEK machine.
///
/// This function contains the processing logic that was formerly in force_step's
/// take_* branches. It sets up continuations (Memoize, BuiltinForceArg,
/// PendingCallDispatch, GuardedValidate) and returns an Action.
async fn dispatch_state(
    state: UnevaluatedState,
    thunk: &Arc<Thunk>,
    stack: &mut Vec<Cont>,
    ctx: &Arc<EvalContext>,
    _env_id: u32,
) -> Action {
    let thunk_span = thunk.span.clone();
    if crate::memory_budget::is_oom_flagged() {
        return Action::Continue(Err(crate::error::EvalError::resource_limit_exceeded(
            "heap limit exceeded (arena bytes)".to_string(),
            thunk_span.clone(),
        )
        .into()));
    }
    let origin = thunk.span.name.clone();

    match state {
        UnevaluatedState::BuiltinCall {
            def,
            args,
            named,
            call_span,
            caller_env_id: builtin_caller_env,
            ctx: thunk_ctx,
        } => {
            // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
            // EvalStackGuard ensures pop on all exit paths; disarmed when delegating to a
            // continuation (BuiltinForceArg, Memoize) that inherits pop responsibility.
            let eval_stack_guard = EvalStackGuard::push((
                origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                thunk_span.clone(),
            ));

            // Wrap args/named in Option so each exclusive match arm can move them
            // without cloning. Each arm calls .take().expect("...") exactly once to extract
            // the owned value.
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
                        thunk_ctx
                            .get_thunk(args.as_ref().expect("args set above")[i])
                            .try_get_materialized()
                            .is_none()
                    })
                {
                    let arg_thunk =
                        thunk_ctx.get_thunk(args.as_ref().expect("args set above")[arg_idx]);
                    stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                        thunk: Arc::clone(thunk),
                        def,
                        args: args.take().expect("args set above"),
                        named: named.take().expect("named set above"),
                        call_span: call_span.clone(),
                        caller_env_id: builtin_caller_env,
                        ctx: thunk_ctx,
                        origin,
                        thunk_span: thunk_span.clone(),
                        mat_span: None,
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
                        && thunk_ctx
                            .get_thunk(args.as_ref().expect("args set above")[*i])
                            .try_get_materialized()
                            .is_none()
                })
            {
                let arg_thunk =
                    thunk_ctx.get_thunk(args.as_ref().expect("args set above")[arg_idx]);
                stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                    thunk: Arc::clone(thunk),
                    def,
                    args: args.take().expect("args set above"),
                    named: named.take().expect("named set above"),
                    call_span: call_span.clone(),
                    caller_env_id: builtin_caller_env,
                    ctx: thunk_ctx,
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
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

            let builtin_args = crate::value::BuiltinArgs {
                args: args.as_ref().expect("args set above").clone(),
                named: named.as_ref().expect("named set above").clone(),
                call_span: call_span.clone(),
                caller_env_id: builtin_caller_env,
                ctx: Arc::clone(&thunk_ctx),
            };

            match (def.func)(builtin_args).await.map_err(|mut e| {
                e.set_arity_callee(Some(def.name.into()));
                e
            }) {
                Ok(result_thunk) => {
                    // Fast path: if the builtin already materialized its result, skip recursion.
                    // Originals in args/named are dropped here — no restore clone needed.
                    if let Some(value) = result_thunk.try_get_materialized() {
                        // eval_stack_guard pops on drop (armed)
                        thunk.settle(Ok(value.clone()));
                        Action::Continue(Ok(value))
                    } else {
                        // Slow path: push Memoize continuation.
                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                            thunk: Arc::clone(thunk),
                            origin,
                            thunk_span: thunk_span.clone(),
                            mat_span: None,
                        })));
                        // Memoize continuation inherits eval_stack pop responsibility
                        eval_stack_guard.disarm();
                        Action::Materialize {
                            thunk: result_thunk,
                            mat_span: None,
                        }
                    }
                }
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        None,
                        origin.as_deref(),
                        thunk_span.clone(),
                    );
                    // eval_stack_guard pops on drop (armed)
                    thunk.settle(Err(Arc::new((*decorated).clone())));
                    Action::Continue(Err(decorated))
                }
            }
        }

        UnevaluatedState::FnCall {
            func,
            args,
            named,
            call_span,
            caller_env_id,
            ctx: thunk_ctx,
            original_call,
        } => {
            // Resolve func ThunkId to Arc<Thunk> for immediate materialization.
            let func_thunk = thunk_ctx.get_thunk(func);
            let func_span = func_thunk.span.clone();

            // TCO eligibility check: If Arc::strong_count == 1, nobody else holds this thunk.
            // Memoization is unnecessary, so we can skip the Memoize continuation push.
            // This achieves O(1) tail-call optimization by reusing the current frame.
            //
            // Race condition safety: Arc::strong_count() and try_claim() are both
            // synchronous (no .await between them). In tokio's LocalSet (cooperative,
            // single-threaded), the count is stable across this check.
            let tail_hint = Arc::strong_count(thunk) == 1;

            // Push to eval_stack after transitioning to InProgress (for cycle path reconstruction).
            // PendingCallDispatch continuation inherits eval_stack pop responsibility.
            // TCO: When tail_hint=true, eval_stack guard drops without disarm (no Memoize pushed).
            let eval_stack_guard = EvalStackGuard::push((
                origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                thunk_span.clone(),
            ));

            stack.push(Cont::PendingCallDispatch(Box::new(
                PendingCallDispatchData {
                    thunk: Arc::clone(thunk),
                    args,
                    named,
                    call_span: call_span.clone(),
                    caller_env_id,
                    ctx: thunk_ctx,
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
                    original_call,
                    tail_hint,
                    func_span,
                },
            )));
            eval_stack_guard.disarm();
            Action::Materialize {
                thunk: func_thunk,
                mat_span: Some(call_span.clone()),
            }
        }

        UnevaluatedState::Guarded {
            inner,
            expected,
            field_path,
            guard_span,
            blame_label,
            default,
        } => {
            // Resolve inner ThunkId to Arc<Thunk> for materialization.
            let inner_thunk = ctx.get_thunk(inner);
            let inner_span = inner_thunk.span.clone();
            // Always use the outer force_step ctx for GuardedValidate. All thunks in a single
            // evaluation share one EvalContext (same arena/state). The ctx is needed for:
            //   1. Flattening Value::Overlay results (flatten_overlay requires ctx)
            //   2. Allocating guard-wrapped field thunks (ctx.alloc_thunk in validate_and_wrap_record)
            let guard_ctx: Arc<EvalContext> = Arc::clone(ctx);
            stack.push(Cont::GuardedValidate(Box::new(GuardedValidateData {
                thunk: Arc::clone(thunk),
                expected: expected.clone(),
                field_path,
                guard_span: guard_span.clone(),
                inner_span,
                origin,
                thunk_span: thunk_span.clone(),
                mat_span: None,
                ctx: guard_ctx,
                blame_label,
                default,
            })));
            Action::Materialize {
                thunk: inner_thunk,
                mat_span: None,
            }
        }

        UnevaluatedState::Surface {
            node,
            res: _res,
            types: _types,
            env_id,
            ctx: thunk_ctx,
        } => {
            // Surface thunk handling in the CEK machine.
            //
            // The round-trip here is: SurfaceNode → lower() → Spanned<CoreExpr> → eval_core_expr()
            // → Arc<Thunk>. All cross-phase data (type annotations, field slots) is inline on nodes.
            // The lower() call reads inline fields directly — no external tables.
            //
            // After lower() we call eval_core_expr() to get a result thunk, then push a Memoize
            // continuation and return Action::Materialize to force the result thunk iteratively.
            // This keeps the Rust call stack flat (no recursive materialize() call).

            let (lowered, surface_lower_diags) =
                crate::lower::lower(&node, thunk_ctx.scope_frames.as_ref().map(|v| v.as_slice()));
            if let Some(err) = lower_errors_to_eval_error(surface_lower_diags) {
                let decorated = attach_materialization_context(
                    err,
                    None,
                    origin.as_deref(),
                    thunk_span.clone(),
                );
                thunk.settle(Err(Arc::new((*decorated).clone())));
                return Action::Continue(Err(decorated));
            }

            // Handle CoreExpr::TypeAssert inline after lowering — same loop risk as take_core_expr.
            if let crate::ast::CoreExpr::TypeAssert {
                annotation,
                expr: inner,
                resolved_type,
                pipeline_blame,
            } = &lowered.node
            {
                // B-433/B-429: If inner is a Placeholder (lowered from a parse-time error or
                // unresolvable VarRef) and annotation has default:, use the default instead.
                // Placeholder is the dead-code marker emitted by the lowerer when a diagnostic
                // is produced; it is never meant to evaluate successfully.
                let inner_thunk = if let (crate::ast::CoreExpr::Placeholder, Some(default_node)) =
                    (&inner.node, annotation.node.get_property("default"))
                {
                    let (lowered_default, lower_diags) = crate::lower::lower(
                        default_node,
                        thunk_ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                    );
                    if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                        let decorated = attach_materialization_context(
                            err,
                            None,
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        thunk.settle(Err(Arc::new((*decorated).clone())));
                        return Action::Continue(Err(decorated));
                    }
                    match eval_core_expr(&lowered_default, env_id, &thunk_ctx).await {
                        Ok(default_thunk) => default_thunk,
                        Err(e) => {
                            let decorated = attach_materialization_context(
                                e,
                                None,
                                origin.as_deref(),
                                thunk_span.clone(),
                            );
                            thunk.settle(Err(Arc::new((*decorated).clone())));
                            return Action::Continue(Err(decorated));
                        }
                    }
                } else {
                    match eval_core_expr(&inner, env_id, &thunk_ctx).await {
                        Ok(t) => t,
                        Err(e) => {
                            let decorated = attach_materialization_context(
                                e,
                                None,
                                origin.as_deref(),
                                thunk_span.clone(),
                            );
                            thunk.settle(Err(Arc::new((*decorated).clone())));
                            return Action::Continue(Err(decorated));
                        }
                    }
                };
                let inner_span = inner_thunk.span.clone();
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Arc::clone(thunk),
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
                })));
                stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                    annotation: Box::new(annotation.clone()),
                    resolved: Box::new(resolved_type.clone()),
                    expr_span: lowered.span.clone(),
                    thunk_span: inner_span,
                    env_id,
                    ctx: Arc::clone(&thunk_ctx),
                    pipeline_blame: pipeline_blame.clone(),
                })));
                return Action::Materialize {
                    thunk: inner_thunk,
                    mat_span: Some(lowered.span.clone()),
                };
            }

            // Remaining CoreExpr variants (Call, Dict, Quote, etc.) fall through to eval_core_expr.
            // Sequential and Match are handled inline above via CEK continuations.
            match eval_core_expr(&lowered, env_id, &thunk_ctx).await {
                Ok(result_thunk) => {
                    // Fast path: if eval_core_expr already produced a materialized thunk
                    // (e.g., literals), skip the Memoize push entirely.
                    if let Some(value) = result_thunk.try_get_materialized() {
                        thunk.settle(Ok(value.clone()));
                        Action::Continue(Ok(value))
                    } else {
                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                            thunk: Arc::clone(thunk),
                            origin,
                            thunk_span: thunk_span.clone(),
                            mat_span: None,
                        })));
                        Action::Materialize {
                            thunk: result_thunk,
                            mat_span: None,
                        }
                    }
                }
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        None,
                        origin.as_deref(),
                        thunk_span.clone(),
                    );
                    thunk.settle(Err(Arc::new((*decorated).clone())));
                    Action::Continue(Err(decorated))
                }
            }
        }

        UnevaluatedState::AstField {
            node,
            field,
            ctx: thunk_ctx,
        } => {
            // AstNodeField thunk: evaluate a single named field from a SurfaceNode.
            // This is a fast synchronous computation (no async, no eval recursion).
            // surface_node_get_field returns a Value directly — no thunk to force.
            let value = crate::surface_fields::surface_node_get_field(&node, field, &thunk_ctx);
            thunk.settle(Ok(value.clone()));
            Action::Continue(Ok(value))
        }

        UnevaluatedState::CoreExpr {
            expr: core_expr,
            env_id,
            ctx: thunk_ctx,
        } => {
            // CoreExpr thunk — created by invoke_function from Value::Function.body.

            // Handle CoreExpr::TypeAssert inline. eval_core_expr(CoreExpr::TypeAssert) wraps
            // in core_expr(CoreExpr::TypeAssert), which would loop back into this branch.
            if let crate::ast::CoreExpr::TypeAssert {
                annotation,
                expr: inner,
                resolved_type,
                pipeline_blame,
            } = &core_expr.node
            {
                // B-433/B-429: If inner is a Placeholder (lowered from a parse-time error or
                // unresolvable VarRef) and annotation has default:, use the default instead.
                let inner_thunk = if let (crate::ast::CoreExpr::Placeholder, Some(default_node)) =
                    (&inner.node, annotation.node.get_property("default"))
                {
                    let (lowered_default, lower_diags) = crate::lower::lower(
                        default_node,
                        thunk_ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                    );
                    if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                        let decorated = attach_materialization_context(
                            err,
                            None,
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        thunk.settle(Err(Arc::new((*decorated).clone())));
                        return Action::Continue(Err(decorated));
                    }
                    match eval_core_expr(&lowered_default, env_id, &thunk_ctx).await {
                        Ok(default_thunk) => default_thunk,
                        Err(e) => {
                            let decorated = attach_materialization_context(
                                e,
                                None,
                                origin.as_deref(),
                                thunk_span.clone(),
                            );
                            thunk.settle(Err(Arc::new((*decorated).clone())));
                            return Action::Continue(Err(decorated));
                        }
                    }
                } else {
                    match eval_core_expr(&inner, env_id, &thunk_ctx).await {
                        Ok(t) => t,
                        Err(e) => {
                            let decorated = attach_materialization_context(
                                e,
                                None,
                                origin.as_deref(),
                                thunk_span.clone(),
                            );
                            thunk.settle(Err(Arc::new((*decorated).clone())));
                            return Action::Continue(Err(decorated));
                        }
                    }
                };
                let inner_span = inner_thunk.span.clone();
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Arc::clone(thunk),
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
                })));
                stack.push(Cont::TypeAssertCheck(Box::new(TypeAssertCheckData {
                    annotation: Box::new(annotation.clone()),
                    resolved: Box::new(resolved_type.clone()),
                    expr_span: core_expr.span.clone(),
                    thunk_span: inner_span,
                    env_id,
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
                    thunk.settle(Ok(Value::Dict(IndexMap::new())));
                    return Action::Continue(Ok(Value::Dict(IndexMap::new())));
                }

                // Memoize the final result
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Arc::clone(thunk),
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
                })));

                // Evaluate the first expression and push a SequentialStep to handle the result
                let first_expr = &exprs[0];
                let arena_len_before_first = thunk_ctx.scope_arena.borrow().scopes.len() as u32;
                stack.push(Cont::SequentialStep(Box::new(
                    crate::eval_materialize::SequentialStepData {
                        idx: 0,
                        exprs: Arc::new(exprs.clone()),
                        env_id,
                        arena_len_before: arena_len_before_first,
                        ctx: Arc::clone(&thunk_ctx),
                        seq_span: core_expr.span.clone(),
                    },
                )));

                // Evaluate the first expression
                return Action::EvalCore {
                    expr: Arc::clone(first_expr),
                    env_id,
                    ctx: thunk_ctx,
                };
            }

            // Handle CoreExpr::Match inline — prevents loop through eval_core_expr.
            // The CEK machine evaluates arms iteratively via MatchDispatch continuations.
            if let crate::ast::CoreExpr::Match { scrutinee, arms } = &core_expr.node {
                // Evaluate the scrutinee first
                let scrutinee_thunk = match eval_core_expr(&scrutinee, env_id, &thunk_ctx).await {
                    Ok(t) => t,
                    Err(e) => {
                        let decorated = attach_materialization_context(
                            e,
                            None,
                            origin.as_deref(),
                            thunk_span.clone(),
                        );
                        thunk.settle(Err(Arc::new((*decorated).clone())));
                        return Action::Continue(Err(decorated));
                    }
                };

                // Push Memoize to cache the final match result
                stack.push(Cont::Memoize(Box::new(MemoizeData {
                    thunk: Arc::clone(thunk),
                    origin,
                    thunk_span: thunk_span.clone(),
                    mat_span: None,
                })));

                // Push MatchDispatch to try arms after scrutinee is materialized.
                // Pass this thunk's env_id so that arm bodies evaluate in the match expression's own scope.
                stack.push(Cont::MatchDispatch(Box::new(
                    crate::eval_materialize::MatchDispatchData {
                        arm_idx: 0,
                        arms: Arc::new(arms.clone()),
                        env: Arc::new(std::sync::RwLock::new(crate::env::Env::new())),
                        env_id,
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

            match eval_core_expr(&core_expr, env_id, &thunk_ctx).await {
                Ok(result_thunk) => {
                    // Fast path: literal or already-materialized result.
                    if let Some(value) = result_thunk.try_get_materialized() {
                        thunk.settle(Ok(value.clone()));
                        return Action::Continue(Ok(value));
                    }
                    // Defer to Memoize continuation.
                    stack.push(Cont::Memoize(Box::new(MemoizeData {
                        thunk: Arc::clone(thunk),
                        origin,
                        thunk_span: thunk_span.clone(),
                        mat_span: None,
                    })));
                    Action::Materialize {
                        thunk: result_thunk,
                        mat_span: None,
                    }
                }
                Err(e) => {
                    let decorated = attach_materialization_context(
                        e,
                        None,
                        origin.as_deref(),
                        thunk_span.clone(),
                    );
                    thunk.settle(Err(Arc::new((*decorated).clone())));
                    Action::Continue(Err(decorated))
                }
            }
        }
    }
}

/// Entry point for inline thunk evaluation. Called by materialize() in eval.rs in the same task.
///
/// Sets up the initial Memoize continuation and calls dispatch_state to begin CEK evaluation.
pub(crate) async fn run_owned(
    state: UnevaluatedState,
    thunk: &Arc<Thunk>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value> {
    let env_id = state.initial_env_id();
    let mut stack: Vec<Cont> = Vec::new();
    stack.push(Cont::Memoize(Box::new(MemoizeData {
        thunk: Arc::clone(thunk),
        origin: thunk.span.name.clone(),
        thunk_span: thunk.span.clone(),
        mat_span: None,
    })));
    let initial = dispatch_state(state, thunk, &mut stack, ctx, env_id).await;
    run_with_stack(initial, stack, ctx).await
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
            } = *data;
            // Inherited guard: Memoize always pops the eval_stack entry that was
            // pushed by the originating force_step (PendingBuiltin, PendingCall, or
            // GuardedValidate default fallback). The guard auto-pops on all exit paths.
            let _eval_stack_guard = EvalStackGuard::inherited();
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
                    thunk.settle(Ok(value.clone()));
                    Action::Continue(Ok(value))
                }
                Err(e) => {
                    // eval_stack_guard pops on drop (armed)
                    thunk.settle(Err(Arc::new((*e).clone())));
                    Action::Continue(Err(e))
                }
            }
        }
        Cont::PendingCallDispatch(data) => {
            let PendingCallDispatchData {
                thunk,
                args,
                named,
                call_span,
                caller_env_id,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                original_call,
                tail_hint,
                func_span,
            } = *data;
            // Inherited guard: PendingCallDispatch inherits the eval_stack entry
            // pushed by force_step(PendingCall). Auto-pops on all exit paths;
            // disarmed when delegating to Memoize or re-dispatching via PendingBuiltin.
            let eval_stack_guard = EvalStackGuard::inherited();
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
            // without cloning. Each arm calls .take().expect("...") exactly once to extract
            // the owned value.
            let mut args = Some(args);
            let mut named = Some(named);

            match result.map_err(&decorate) {
                Ok(func_value) => {
                    match func_value {
                        Value::Function {
                            params,
                            body,
                            closure_env_id,
                            ..
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
                            //   ᴍᴀᴄʀᴏ∷env  — the call-site FlatEnv id (Value::Int)
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
                                                args_vec[i] = thunk_ctx.alloc_thunk(
                                                    0,
                                                    Arc::new(Thunk::value(
                                                        expr_value,
                                                        core_args[i].span.clone(),
                                                    )),
                                                );
                                            }
                                        }
                                    }

                                    args = Some(args_vec);

                                    // Inject implicit named args: ᴍᴀᴄʀᴏ∷env and ᴍᴀᴄʀᴏ∷span.
                                    // ᴍᴀᴄʀᴏ∷env is the call-site FlatEnv id (Value::Int(caller_env_id)).
                                    // ᴍᴀᴄʀᴏ∷span is the call-site span dict.
                                    // Both are injected via BIND-SYSTEM (eval_call.rs:249) which
                                    // skips '∷'-named args from normal validation.
                                    let inner_named = named.as_mut().expect("named set above");
                                    let named_map = inner_named
                                        .get_or_insert_with(|| Box::new(IndexMap::new()));
                                    named_map.insert(
                                        MACRO_CALL_ENV_NAME.to_string(),
                                        thunk_ctx.alloc_thunk(
                                            0,
                                            Arc::new(Thunk::value(
                                                Value::Int(caller_env_id as i64),
                                                call_span.clone(),
                                            )),
                                        ),
                                    );
                                    let span_thunk_id =
                                        make_span_dict(&call_span, &thunk_ctx, &call_span);
                                    named_map
                                        .insert(MACRO_CALL_SPAN_NAME.to_string(), span_thunk_id);
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
                                        closure_env_id,
                                        positional: args.as_deref().expect("args set above"),
                                        named: named.as_ref().expect("named set above").as_deref(),
                                        default_env_id: caller_env_id,
                                        call_span: call_span.clone(),
                                        ctx: &thunk_ctx,
                                    };
                                    invoke_function_tco(&call_ctx).await
                                };

                                match invoke_result.map_err(&decorate) {
                                    Ok((body_expr, new_env_id)) => {
                                        // TCO abandonment: this thunk stays InProgress and will be dropped
                                        // when this continuation is consumed (strong_count==1 guarantees no
                                        // other references exist). The result flows directly to the caller's
                                        // continuation via Action::EvalCore → run loop → new thunk
                                        // materialization.
                                        Action::EvalCore {
                                            expr: body_expr,
                                            env_id: new_env_id,
                                            ctx: thunk_ctx,
                                        }
                                    }
                                    Err(mut e) => {
                                        e.push_frame(
                                            origin.as_deref().unwrap_or("call").to_string(),
                                            call_span.clone(),
                                        );
                                        // eval_stack_guard pops on drop (armed)
                                        thunk.settle(Err(Arc::new((*e).clone())));
                                        Action::Continue(Err(e))
                                    }
                                }
                            } else {
                                // Non-TCO path: create thunk and push Memoize continuation.
                                let invoke_result = {
                                    let call_ctx = CallContext {
                                        params: &params,
                                        body: &body,
                                        closure_env_id,
                                        positional: args.as_deref().expect("args set above"),
                                        named: named.as_ref().expect("named set above").as_deref(),
                                        default_env_id: caller_env_id,
                                        call_span: call_span.clone(),
                                        ctx: &thunk_ctx,
                                    };
                                    invoke_function(&call_ctx).await
                                };

                                match invoke_result.map_err(&decorate) {
                                    Ok(result_thunk) => {
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin,
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
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
                                        thunk.settle(Err(Arc::new((*e).clone())));
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
                                && (0..def.force_count.min(args_ref.len())).any(|i| {
                                    thunk_ctx
                                        .get_thunk(args_ref[i])
                                        .try_get_materialized()
                                        .is_none()
                                });
                            // Check if any W1 Seq/Spine positional args need pre-materialization.
                            let has_strict_unevaluated =
                                def.pos_strictness.iter().enumerate().any(|(i, &s)| {
                                    i < args_ref.len()
                                        && (s == Strictness::Seq || s == Strictness::Spine)
                                        && thunk_ctx
                                            .get_thunk(args_ref[i])
                                            .try_get_materialized()
                                            .is_none()
                                });

                            if has_force_count_unevaluated || has_strict_unevaluated {
                                // eval_stack_guard pops on drop (armed) before PendingBuiltin re-dispatch.
                                // force_step(PendingBuiltin) will push a fresh entry for this thunk.
                                // Transition thunk from InProgress → PendingBuiltin.
                                thunk.reset(crate::value::UnevaluatedState::BuiltinCall {
                                    def,
                                    args: args.take().expect("args set above"),
                                    named: named.take().expect("named set above").map(|b| *b),
                                    call_span: call_span.clone(),
                                    caller_env_id,
                                    ctx: thunk_ctx,
                                });
                                return Action::Materialize { thunk, mat_span };
                            }

                            // All strict args are already materialized — call the builtin directly.
                            let builtin_result = {
                                let builtin_args = crate::value::BuiltinArgs {
                                    args: args.as_deref().expect("args set above").to_vec(),
                                    named: named
                                        .as_ref()
                                        .expect("named set above")
                                        .as_deref()
                                        .cloned(),
                                    call_span: call_span.clone(),
                                    caller_env_id,
                                    ctx: Arc::clone(&thunk_ctx),
                                };
                                (def.func)(builtin_args).await.map_err(|mut e| {
                                    e.set_arity_callee(Some(def.name.into()));
                                    e
                                })
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
                                            thunk.settle(Ok(value.clone()));
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
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin,
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
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
                                    thunk.settle(Err(Arc::new((*e).clone())));
                                    Action::Continue(Err(e))
                                }
                            }
                        }
                        // Unit variant used as a constructor: e.g. `[Result.Ok payload]`.
                        // When a unit Variant (payload: None) is called with exactly one positional
                        // arg and no named args, treat it as constructing Variant(tycon, ctor, payload).
                        Value::Variant {
                            tycon,
                            ctor,
                            payload: None,
                        } if args.as_ref().is_some_and(|v| v.len() == 1)
                            && named
                                .as_ref()
                                .is_none_or(|m| m.as_ref().is_none_or(|b| b.is_empty())) =>
                        {
                            // The arg is a ThunkId — use it directly as the payload.
                            let payload_id = args.as_ref().expect("args set above")[0];
                            let result_val = Value::Variant {
                                tycon,
                                ctor,
                                payload: Some(payload_id),
                            };
                            // Fast path: the result is immediately materialized — no need to
                            // push a Memoize continuation. eval_stack_guard pops on drop (armed).
                            if !tail_hint {
                                thunk.settle(Ok(result_val.clone()));
                            }
                            Action::Continue(Ok(result_val))
                        }
                        other => {
                            // Extract the name of what was called (from original_call or call_span).
                            let callee_label: Option<String> = call_span
                                .name
                                .as_deref()
                                .map(|s| s.to_string())
                                .or_else(|| {
                                    if let crate::ast::CoreExpr::Call { ref func, .. } =
                                        original_call.node
                                    {
                                        if let crate::ast::CoreExpr::Var { ref name, .. } =
                                            func.node
                                        {
                                            return Some(name.clone());
                                        }
                                    }
                                    None
                                });
                            // For Dict values, list the first few keys to help identification.
                            let got_detail = match &other {
                                crate::value::Value::Dict(map) if !map.is_empty() => {
                                    let keys: Vec<String> =
                                        map.keys().take(5).map(|k| format!("{k}")).collect();
                                    let ellipsis = if map.len() > 5 { ", ..." } else { "" };
                                    format!("Dict {{{}{}}}", keys.join(", "), ellipsis)
                                }
                                _ => other.type_name().to_string(),
                            };
                            let message = if let Some(name) = callee_label {
                                format!(
                                    "expected Function or Builtin, but `{}` evaluated to {}",
                                    name, got_detail
                                )
                            } else {
                                format!("expected Function or Builtin, got {}", got_detail)
                            };
                            let err = EvalError::user_error(message, func_span.clone())
                                .with_secondary_span(call_span.clone(), "called here");
                            let decorated = decorate(Box::new(err));
                            // eval_stack_guard pops on drop (armed)
                            thunk.settle(Err(Arc::new((*decorated).clone())));
                            Action::Continue(Err(decorated))
                        }
                    } // end match func_value
                } // end Ok(func_value) block
                Err(e) => {
                    // Function materialization failed
                    // eval_stack_guard pops on drop (armed)
                    thunk.settle(Err(Arc::new((*e).clone())));
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
                                    thunk.settle(Err(Arc::new((*e).clone())));
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
                                0,
                                &guard_ctx,
                                default.clone(),
                                blame_label.clone(),
                            ) {
                                Ok(new_entries) => {
                                    let guarded_value = Value::Dict(new_entries);
                                    thunk.settle(Ok(guarded_value.clone()));
                                    Action::Continue(Ok(guarded_value))
                                }
                                Err(err) => {
                                    // Guard validation failed - use default if present
                                    if let Some((default_expr, default_env_id)) = default {
                                        let guard_eval_stack = EvalStackGuard::push((
                                            origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                                            thunk_span.clone(),
                                        ));
                                        stack.push(Cont::Memoize(Box::new(MemoizeData {
                                            thunk: Arc::clone(&thunk),
                                            origin: Some(Arc::from("default fallback")),
                                            thunk_span: thunk_span.clone(),
                                            mat_span: mat_span.clone(),
                                        })));
                                        // Memoize continuation inherits eval_stack pop responsibility
                                        guard_eval_stack.disarm();
                                        return Action::EvalCore {
                                            expr: Arc::clone(&default_expr),
                                            env_id: default_env_id,
                                            ctx: guard_ctx,
                                        };
                                    }
                                    let err = decorate(err);
                                    thunk.settle(Err(Arc::new((*err).clone())));
                                    Action::Continue(Err(err))
                                }
                            }
                        } else {
                            // Expected Record but got non-Dict - use default if present
                            if let Some((default_expr, default_env_id)) = default {
                                let guard_eval_stack = EvalStackGuard::push((
                                    origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                                    thunk_span.clone(),
                                ));
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
                                })));
                                guard_eval_stack.disarm();
                                return Action::EvalCore {
                                    expr: Arc::clone(&default_expr),
                                    env_id: default_env_id,
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
                            thunk.settle(Err(Arc::new((*err).clone())));
                            Action::Continue(Err(err))
                        }
                    } else {
                        // For non-Record types, simple value check
                        if value_matches_type(&value, &expected, &guard_ctx) {
                            thunk.settle(Ok(value.clone()));
                            Action::Continue(Ok(value))
                        } else {
                            // Type mismatch for non-Record types - use default if present
                            if let Some((default_expr, default_env_id)) = default {
                                let guard_eval_stack = EvalStackGuard::push((
                                    origin.clone().unwrap_or_else(|| Arc::from("thunk")),
                                    thunk_span.clone(),
                                ));
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin: Some(Arc::from("default fallback")),
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
                                })));
                                guard_eval_stack.disarm();
                                return Action::EvalCore {
                                    expr: Arc::clone(&default_expr),
                                    env_id: default_env_id,
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
                            thunk.settle(Err(Arc::new((*err).clone())));
                            Action::Continue(Err(err))
                        }
                    }
                }
                Err(e) => {
                    // Inner materialization error propagates
                    let e = decorate(e);
                    thunk.settle(Err(Arc::new((*e).clone())));
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
                caller_env_id: builtin_caller_env,
                ctx: thunk_ctx,
                origin,
                thunk_span,
                mat_span,
                arg_idx,
            } = *data;
            // Inherited guard: BuiltinForceArg inherits the eval_stack entry
            // pushed by force_step(PendingBuiltin). Auto-pops on all exit paths;
            // disarmed when delegating to another BuiltinForceArg or Memoize.
            let eval_stack_guard = EvalStackGuard::inherited();
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
                                thunk_ctx
                                    .get_thunk(args.as_ref().expect("args set above")[i])
                                    .try_get_materialized()
                                    .is_none()
                            })
                        {
                            let next_arg = thunk_ctx
                                .get_thunk(args.as_ref().expect("args set above")[next_idx]);
                            stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                                thunk,
                                def,
                                args: args.take().expect("args set above"),
                                named: named.take().expect("named set above"),
                                call_span: call_span.clone(),
                                caller_env_id: builtin_caller_env,
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

                    use crate::value::Strictness;
                    if let Some((next_idx, _)) = def
                        .pos_strictness
                        .iter()
                        .enumerate()
                        .skip(arg_idx + 1)
                        .find(|(i, &s)| {
                            *i < args.as_ref().expect("args set above").len()
                                && (s == Strictness::Seq || s == Strictness::Spine)
                                && thunk_ctx
                                    .get_thunk(args.as_ref().expect("args set above")[*i])
                                    .try_get_materialized()
                                    .is_none()
                        })
                    {
                        let next_arg =
                            thunk_ctx.get_thunk(args.as_ref().expect("args set above")[next_idx]);
                        stack.push(Cont::BuiltinForceArg(Box::new(BuiltinForceArgData {
                            thunk,
                            def,
                            args: args.take().expect("args set above"),
                            named: named.take().expect("named set above"),
                            call_span: call_span.clone(),
                            caller_env_id: builtin_caller_env,
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
                        caller_env_id: builtin_caller_env,
                        ctx: Arc::clone(&thunk_ctx),
                    };
                    match (def.func)(builtin_args)
                        .await
                        .map_err(|mut e| {
                            e.set_arity_callee(Some(def.name.into()));
                            e
                        })
                        .map_err(&decorate)
                    {
                        Ok(result_thunk) => {
                            if let Some(value) = result_thunk.try_get_materialized() {
                                // eval_stack_guard pops on drop (armed)
                                thunk.settle(Ok(value.clone()));
                                Action::Continue(Ok(value))
                            } else {
                                // Slow path: push Memoize continuation.
                                stack.push(Cont::Memoize(Box::new(MemoizeData {
                                    thunk: Arc::clone(&thunk),
                                    origin,
                                    thunk_span: thunk_span.clone(),
                                    mat_span: mat_span.clone(),
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
                            thunk.settle(Err(Arc::new((*e).clone())));
                            Action::Continue(Err(e))
                        }
                    }
                }
                Err(e) => {
                    let e = decorate(e);
                    // eval_stack_guard pops on drop (armed)
                    thunk.settle(Err(Arc::new((*e).clone())));
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
                env_id,
                ctx,
                pipeline_blame,
            } = *data;
            let expected = *resolved;
            match result {
                Err(e) => {
                    // B-433: When inner expr materialization fails (Placeholder, undefined variable, etc.),
                    // check for `default:` annotation and evaluate it instead of propagating the error.
                    if let Some(default_node) = annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                    {
                        let (lowered_default, lower_diags) = crate::lower::lower(
                            default_node,
                            ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                        );
                        if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                            Action::Continue(Err(err))
                        } else {
                            Action::EvalCore {
                                expr: Arc::new(lowered_default),
                                env_id,
                                ctx: Arc::clone(&ctx),
                            }
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
                            let default_opt = if let Some(node) =
                                annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                            {
                                let (lowered, lower_diags) = crate::lower::lower(
                                    node,
                                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                                );
                                if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                                    return Action::Continue(Err(err));
                                }
                                Some((Arc::new(lowered), env_id))
                            } else {
                                None
                            };
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
                                0,
                                &ctx,
                                default_opt.clone(),
                                blame_label,
                            ) {
                                Ok(new_entries) => Action::Continue(Ok(Value::Dict(new_entries))),
                                Err(err) => {
                                    if let Some((default, default_env_id)) = default_opt {
                                        // Evaluate default expression iteratively.
                                        Action::EvalCore {
                                            expr: default,
                                            env_id: default_env_id,
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
                                let (lowered_default, lower_diags) = crate::lower::lower(
                                    default_node,
                                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                                );
                                if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                                    return Action::Continue(Err(err));
                                }
                                Action::EvalCore {
                                    expr: Arc::new(lowered_default),
                                    env_id,
                                    ctx: Arc::clone(&ctx),
                                }
                            } else {
                                let mut err = EvalError::type_assert_failed(
                                    &format_expected_label(&expected, &ctx),
                                    &format_got_label(&value, &expected),
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
                            let (lowered_pred, lower_diags) = crate::lower::lower(
                                &predicate_node,
                                ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                            );
                            if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                                return Action::Continue(Err(err));
                            }
                            stack.push(Cont::PredicateCheck(Box::new(PredicateCheckData {
                                value: value.clone(),
                                annotation,
                                expr_span: expr_span.clone(),
                                thunk_span: thunk_span.clone(),
                                env_id,
                                ctx: Arc::clone(&ctx),
                                callable_invoked: false,
                            })));
                            Action::EvalCore {
                                expr: Arc::new(lowered_pred),
                                env_id,
                                ctx: Arc::clone(&ctx),
                            }
                        } else {
                            Action::Continue(Ok(value))
                        }
                    } else if let Some(default_node) =
                        annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                    {
                        let (lowered_default, lower_diags) = crate::lower::lower(
                            default_node,
                            ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                        );
                        if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                            return Action::Continue(Err(err));
                        }
                        Action::EvalCore {
                            expr: Arc::new(lowered_default),
                            env_id,
                            ctx: Arc::clone(&ctx),
                        }
                    } else {
                        let mut err = EvalError::type_assert_failed(
                            &format_expected_label(&expected, &ctx),
                            &format_got_label(&value, &expected),
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
                env_id,
                arena_len_before,
                ctx,
                seq_span,
            } = *data;

            // Result is the materialized value from the previous expression
            match result {
                Err(mut e) => {
                    e.push_frame("sequential expression".to_string(), seq_span);
                    Action::Continue(Err(e))
                }
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
                        let _map = match intermediate_value {
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
                                            env_id,
                                            arena_len_before,
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

                        // T-1558 complete: advance env_id to the intermediate dict's FlatEnv root.
                        // The resolver's Sequential handler calls enter_scope for each intermediate
                        // dict with static keys, so the next expression is resolved one scope level
                        // deeper than the call frame. The evaluator must match by advancing env_id
                        // to the first new FlatEnv allocated during this expression's evaluation
                        // (arena_len_before = that first new env = the dict's letrec root scope).
                        // This ensures VarRef level coordinates from the resolver align with the
                        // FlatEnv display chain at evaluation time.
                        let current_arena_len = ctx.scope_arena.borrow().scopes.len() as u32;
                        let seq_env_id = if current_arena_len > arena_len_before {
                            arena_len_before // first new FlatEnv = intermediate dict's letrec root
                        } else {
                            env_id // no new FlatEnv allocated (empty dict or all literals)
                        };

                        // Record arena length for the NEXT expression's SequentialStepData.
                        let next_arena_len = ctx.scope_arena.borrow().scopes.len() as u32;

                        // Proceed to the next expression.
                        let next_expr = &exprs[next_idx];
                        stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                            idx: next_idx,
                            exprs: Arc::clone(&exprs),
                            env_id: seq_env_id,
                            arena_len_before: next_arena_len,
                            ctx: Arc::clone(&ctx),
                            seq_span,
                        })));
                        Action::EvalCore {
                            expr: Arc::clone(next_expr),
                            env_id: seq_env_id,
                            ctx,
                        }
                    } else {
                        // No static keys — continue with same env_id.
                        let next_arena_len = ctx.scope_arena.borrow().scopes.len() as u32;
                        let next_expr = &exprs[next_idx];
                        stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                            idx: next_idx,
                            exprs: Arc::clone(&exprs),
                            env_id,
                            arena_len_before: next_arena_len,
                            ctx: Arc::clone(&ctx),
                            seq_span,
                        })));
                        Action::EvalCore {
                            expr: Arc::clone(next_expr),
                            env_id,
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
                env_id,
                arena_len_before,
                ctx,
                seq_span,
                current_expr_span,
            } = *data;

            // Result is the materialized payload value from the Variant
            match result {
                Err(mut e) => {
                    e.push_frame("sequential expression".to_string(), seq_span);
                    Action::Continue(Err(e))
                }
                Ok(payload_val) => {
                    // Unpack the payload dict using require_dict
                    let _map = match crate::builtins::require_dict(
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

                    // T-1558: FlatEnv scope advancement for variant payloads.
                    let _ = static_key_set;
                    let current_arena_len = ctx.scope_arena.borrow().scopes.len() as u32;
                    let seq_env_id = if current_arena_len > arena_len_before {
                        arena_len_before
                    } else {
                        env_id
                    };
                    let next_arena_len = ctx.scope_arena.borrow().scopes.len() as u32;

                    let next_expr = &exprs[next_idx];
                    stack.push(Cont::SequentialStep(Box::new(SequentialStepData {
                        idx: next_idx,
                        exprs: Arc::clone(&exprs),
                        env_id: seq_env_id,
                        arena_len_before: next_arena_len,
                        ctx: Arc::clone(&ctx),
                        seq_span,
                    })));
                    Action::EvalCore {
                        expr: Arc::clone(next_expr),
                        env_id: seq_env_id,
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
                env_id,
                ctx,
                match_span,
            } = *data;

            // Result is the materialized scrutinee value
            match result {
                Err(mut e) => {
                    e.push_frame("match expression".to_string(), match_span);
                    Action::Continue(Err(e))
                }
                Ok(scrutinee_value) => {
                    // Try each arm starting from arm_idx
                    for i in arm_idx..arms.len() {
                        let arm = &arms[i];

                        // Try the pattern. Since apply_cont is async, we can .await directly
                        // here without block_on_anywhere — this keeps async state on the heap
                        // rather than the Rust stack, preventing stack overflow on deeply
                        // nested patterns.
                        let matched_env = match match_pattern(
                            &arm.pattern,
                            &scrutinee_value,
                            &env,
                            &arm.pattern.span,
                            env_id,
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
                            // Determine the arm body expression and the FlatEnv id to use
                            // when evaluating the body (and guard). For 3-arg CaseArm forms,
                            // `eval_case_arm_structural_pattern` allocates a child FlatEnv,
                            // fills its slots with the bound values, and returns its id.
                            // For all other arm forms, the parent env id is used unchanged.
                            let (eval_body, arm_feid, legacy_arm_env) = if let CoreExpr::CaseArm {
                                let_bindings,
                                pattern,
                                body,
                            } = &arm.body.node
                            {
                                // 3-arg form: [case [let bindings] pattern body]
                                // Extract the binding name→slot map from the let_bindings node.
                                // Walk the structural pattern, binding names in the map and
                                // pin-comparing names not in the map.
                                let binding_map = extract_let_binding_names(&let_bindings.node);
                                match eval_case_arm_structural_pattern(
                                    pattern,
                                    &binding_map,
                                    &scrutinee_value,
                                    match_span.clone(),
                                    env_id,
                                    &ctx,
                                )
                                .await
                                {
                                    Ok(Some(feid)) => {
                                        // Pattern matched: use the allocated arm FlatEnv.
                                        (Arc::clone(body), feid, Arc::clone(&arm_env))
                                    }
                                    Ok(None) => {
                                        // Pattern did not match — move to next arm
                                        continue;
                                    }
                                    Err(e) => return Action::Continue(Err(e)),
                                }
                            } else {
                                // Not a CaseArm body — use the parent env id unchanged.
                                (Arc::clone(&arm.body), env_id, arm_env)
                            };

                            // Pattern matched. If there is a guard, evaluate it.
                            if let Some(guard_expr) = &arm.guard {
                                // Push a continuation to check the guard result.
                                let guard_binding = arm.guard_matchable_binding.get().cloned();
                                stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                    arm_idx: i,
                                    arms: Arc::clone(&arms),
                                    env_id: arm_feid,
                                    ctx: Arc::clone(&ctx),
                                    match_span: match_span.clone(),
                                    arm_env: legacy_arm_env,
                                    scrutinee_value: scrutinee_value.clone(),
                                    body: Arc::clone(&eval_body),
                                    callable_invoked: false,
                                    guard_matchable_binding: guard_binding,
                                })));

                                return Action::EvalCore {
                                    expr: Arc::clone(guard_expr),
                                    env_id: arm_feid,
                                    ctx,
                                };
                            }

                            // No guard — arm matched, evaluate body in the arm FlatEnv.
                            return Action::EvalCore {
                                expr: eval_body,
                                env_id: arm_feid,
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
                env_id,
                ctx,
                match_span,
                arm_env,
                scrutinee_value,
                body,
                callable_invoked,
                guard_matchable_binding,
            } = *data;

            match result {
                Err(mut e) => {
                    e.push_frame("match guard".to_string(), match_span);
                    Action::Continue(Err(e))
                }
                Ok(guard_value) => {
                    // PM1: If the guard is callable and we haven't yet invoked it, do so
                    // iteratively via the CEK machine rather than block_on_anywhere.
                    if !callable_invoked {
                        if let Value::Function { .. } | Value::Builtin(_) = &guard_value {
                            // Create ThunkIds for scrutinee and predicate.
                            let scrutinee_id = ctx.alloc_thunk(
                                0,
                                Arc::new(Thunk::value(scrutinee_value.clone(), match_span.clone())),
                            );
                            let pred_id = ctx.alloc_thunk(
                                0,
                                Arc::new(Thunk::value(guard_value, match_span.clone())),
                            );
                            // Create a PendingCall thunk for guard(scrutinee).
                            let call_thunk = Arc::new(Thunk::fn_call(
                                pred_id,
                                vec![scrutinee_id],
                                IndexMap::new(),
                                match_span.clone(),
                                env_id, // B-515: arm FlatEnv allocation pending
                                match_span.clone(),
                                Arc::clone(&ctx),
                                Arc::new(Spanned {
                                    node: CoreExpr::Int(0),
                                    span: match_span.clone(),
                                }),
                            ));
                            // Push MatchGuardCheck again with callable_invoked=true.
                            stack.push(Cont::MatchGuardCheck(Box::new(MatchGuardCheckData {
                                arm_idx,
                                arms,
                                env_id,
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

                    // call_to_match/call_to_match_resolved ignore legacy env (B-515 tracks FlatEnv arm binding).
                    let dummy_env =
                        Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
                    let guard_passed = if let Some(ref binding_name) = guard_matchable_binding {
                        // Compile-time resolved: use the pre-resolved Matchable instance binding.
                        crate::eval::call_to_match_resolved(
                            &guard_value,
                            binding_name,
                            &dummy_env,
                            &ctx,
                            &match_span,
                        )
                        .await
                    } else {
                        // Type checking was skipped — fall back to dynamic dispatch.
                        crate::eval::call_to_match(&guard_value, &dummy_env, &ctx, &match_span)
                            .await
                    };

                    if guard_passed {
                        // Guard passed — evaluate the body.
                        // Uses env_id; B-515 tracks FlatEnv scope allocation for arm bindings.
                        let _ = arm_env; // legacy env dropped
                        Action::EvalCore {
                            expr: body,
                            env_id,
                            ctx,
                        }
                    } else {
                        // Guard failed — try the next arm
                        stack.push(Cont::MatchDispatch(Box::new(MatchDispatchData {
                            arm_idx: arm_idx + 1,
                            arms,
                            env: arm_env,
                            env_id,
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
                env_id,
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
                            let dummy_env_pred =
                                Arc::new(std::sync::RwLock::new(crate::env::Env::new()));
                            let call_thunk = apply_predicate_to_subject(
                                predicate_value,
                                value.clone(),
                                expr_span.clone(),
                                thunk_span.clone(),
                                &dummy_env_pred,
                                env_id,
                                &ctx,
                            );
                            // Push PredicateCheck again with callable_invoked=true.
                            stack.push(Cont::PredicateCheck(Box::new(PredicateCheckData {
                                value,
                                annotation,
                                expr_span: expr_span.clone(),
                                thunk_span,
                                env_id,
                                ctx: Arc::clone(&ctx),
                                callable_invoked: true,
                            })));
                            return Action::Materialize {
                                thunk: call_thunk,
                                mat_span: Some(expr_span),
                            };
                        }
                    }

                    // call_to_match ignores legacy env (returns false conservatively; B-515 tracks FlatEnv arm binding).
                    let dummy_env =
                        Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
                    let pred_passed =
                        crate::eval::call_to_match(&predicate_value, &dummy_env, &ctx, &expr_span)
                            .await;

                    if pred_passed {
                        // Predicate passed — return the original value
                        Action::Continue(Ok(value))
                    } else {
                        // Predicate failed — check for default: or fail
                        if let Some(default_node) =
                            annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                        {
                            // Evaluate default expression iteratively
                            let (lowered_default, lower_diags) = crate::lower::lower(
                                default_node,
                                ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                            );
                            if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                                return Action::Continue(Err(err));
                            }
                            Action::EvalCore {
                                expr: Arc::new(lowered_default),
                                env_id,
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

/// Extract the binding-target names from a `[let ...]` expression for a 3-arg
/// `[case [let bindings] pattern body]` arm, returning a name→slot map.
///
/// The `[let ...]` node is the first argument of the 3-arg CaseArm. It declares which
/// names in the pattern are binding targets (as opposed to pin-comparisons). Any name
/// that appears in the `[let ...]` list is a binding target; any other name used in the
/// pattern expression will be evaluated from the current environment and compared for
/// equality (pin semantics).
///
/// Returns an `IndexMap<String, u32>` mapping each binding name to its slot index.
/// The slot is the declaration-order position (first name → 0, second → 1, …), which
/// matches the resolver's `enter_scope(&bound_names)` insertion-order slot assignment.
/// The map is threaded into `eval_case_arm_structural_pattern` to distinguish bind vs.
/// pin at each name position, and to fill the correct FlatEnv slot when binding.
fn extract_let_binding_names(let_decl: &CoreExpr) -> IndexMap<String, u32> {
    let mut map: IndexMap<String, u32> = IndexMap::new();
    if let CoreExpr::LetDecl { bindings } = let_decl {
        for binding in bindings {
            match &binding.node {
                // lower_let_decl_binding converts declaration-position names to CoreExpr::Str.
                // The "_" wildcard is excluded — it binds nothing.
                CoreExpr::Str(name) if name != "_" => {
                    let slot = map.len() as u32;
                    map.insert(name.clone(), slot);
                }
                // Both plain and annotated Var (Var { annotation: Some(_) }) use the name field.
                CoreExpr::Var { name, .. } if name != "_" => {
                    let slot = map.len() as u32;
                    map.insert(name.clone(), slot);
                }
                _ => {}
            }
        }
    }
    map
}

/// Evaluate the structural pattern of a 3-arg `[case [let bindings] pattern body]` arm.
///
/// Returns `Ok(Some(env))` if the pattern matches, `Ok(None)` if it does not match,
/// or `Err(e)` if evaluating the pattern itself produces an error (e.g., unresolvable
/// pin reference, failed field-get for constructor tag). Errors must not be silently
/// converted to no-match — that produces misleading diagnostics.
///
/// Returns `Ok(Some(arm_env_id))` on match, where `arm_env_id` is the FlatEnv id of
/// the freshly allocated child scope containing one slot per named binding. The caller
/// VarRef lookups at (level=0, slot=K) resolve into this scope's slots.
async fn eval_case_arm_structural_pattern(
    pattern: &Arc<Spanned<CoreExpr>>,
    binding_map: &IndexMap<String, u32>,
    scrutinee_value: &Value,
    match_span: Span,
    parent_env_id: u32,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Option<u32>> {
    // Pre-allocate a child FlatEnv for the arm's bindings.
    // Slot count = number of named bindings declared in the [let ...] block.
    // The resolver assigned (level=0, slot=K) coordinates to uses of these names
    // in the arm body, where K is the declaration-order index matching binding_map.
    let arm_env_id = ctx
        .scope_arena
        .borrow_mut()
        .alloc_child(crate::arena::ScopeId(parent_env_id), binding_map.len());

    // Reserve one slot per named binding in declaration order.
    // bind_or_pin_name fills these via fill_slot(arm_env_id, slot, thunk_id).
    {
        let mut arena = ctx.scope_arena.borrow_mut();
        for (_, expected_slot) in binding_map {
            let reserved = arena.reserve_slot(arm_env_id);
            debug_assert_eq!(
                reserved, *expected_slot,
                "arm slot reservation order must match binding_map"
            );
        }
    }

    // Legacy placeholder — eval_structural_pattern_inner no longer inserts into this env;
    // all bindings go into the arena slots allocated above.
    let arm_env_legacy = Arc::new(RwLock::new(crate::env::Env::new()));

    if eval_structural_pattern_inner(
        &pattern.node,
        binding_map,
        arm_env_id,
        scrutinee_value,
        &arm_env_legacy,
        parent_env_id,
        match_span.clone(),
        ctx,
    )
    .await?
    {
        Ok(Some(arm_env_id.0))
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
    binding_map: &'a IndexMap<String, u32>,
    arm_env_id: crate::arena::ScopeId,
    scrutinee_value: &'a Value,
    arm_env_legacy: &'a Arc<RwLock<crate::env::Env>>,
    parent_env_id: u32,
    match_span: Span,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<bool>> + 'a>> {
    Box::pin(async move {
        match pattern {
            // Wildcard: always succeeds, no binding.
            // In the new pattern design, `_` in pattern position is an undefined variable
            // (resolver sets Some(None)), which the lowerer converts to CoreExpr::Placeholder.
            // `...` in pattern position also becomes CoreExpr::Placeholder.
            CoreExpr::Placeholder => Ok(true),

            // Plain name: bind or pin based on binding_map.
            // level and slot carry de Bruijn coordinates for the pin case.
            CoreExpr::Var {
                name,
                level,
                slot,
                annotation: None,
            } => {
                bind_or_pin_name(
                    name,
                    binding_map,
                    arm_env_id,
                    scrutinee_value,
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
                    binding_map,
                    arm_env_id,
                    scrutinee_value,
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
                    let (lowered_pred, lower_diags) = crate::lower::lower(
                        pred_surface_node,
                        ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                    );
                    if let Some(err) = lower_errors_to_eval_error(lower_diags) {
                        return Err(err);
                    }
                    let pred_expr_core = Arc::new(lowered_pred);

                    let pred_thunk = Arc::new(Thunk::core_expr(
                        pred_expr_core,
                        arm_env_id.0, // Use the arm FlatEnv so the predicate closure sees arm-local bindings.
                        Arc::clone(ctx),
                        match_span.clone(),
                    ));

                    let pred_value = materialize(&pred_thunk, Some(&match_span), ctx).await?;

                    // The bound variable holds scrutinee_value. Pass it directly —
                    // it is already a Value, no need to wrap in a Thunk and materialize.
                    let pred_call_thunk = apply_predicate_to_subject(
                        pred_value,
                        scrutinee_value.clone(),
                        match_span.clone(),
                        match_span.clone(),
                        arm_env_legacy,
                        arm_env_id.0,
                        ctx,
                    );

                    let pred_result = materialize(&pred_call_thunk, Some(&match_span), ctx).await?;

                    // call_to_match ignores legacy env (B-515 tracks FlatEnv arm binding).
                    let dummy_env =
                        Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
                    if !crate::eval::call_to_match(&pred_result, &dummy_env, ctx, &match_span).await
                    {
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
                // Evaluate the func expression (constructor lookup) in the parent scope.
                // arm_env_id is a child of parent_env_id, so both work via the
                // parent chain, but using parent_env_id is semantically clearer:
                // constructors are resolved in the enclosing (non-arm) scope.
                let func_thunk = Arc::new(Thunk::core_expr(
                    Arc::clone(func),
                    parent_env_id,
                    Arc::clone(ctx),
                    match_span.clone(),
                ));
                let func_val = materialize(&func_thunk, Some(&match_span), ctx).await?;

                // Extract the constructor tag if func_val is either:
                //   (a) a unit Variant (payload: None) — the old unit-constructor form
                //   (b) a Function with return_ann annotation (named-field ctor)
                let ctor_tag_opt: Option<String> = match &func_val {
                    Value::Variant { tycon, ctor, .. } => Some(format!("{}.{}", tycon, ctor)),
                    Value::Function { annotation, .. } => annotation.as_deref().and_then(|ann| {
                        ann.return_ann.as_ref().and_then(|ret_ann| {
                            if let crate::ast::Annotation::Simple(tag) = ret_ann {
                                Some(tag.clone())
                            } else {
                                None
                            }
                        })
                    }),
                    _ => None,
                };

                if let Some(ctor_tag) = ctor_tag_opt {
                    // Constructor pattern: match scrutinee tag, bind payload.
                    let Value::Variant {
                        tycon: scrutinee_tycon,
                        ctor: scrutinee_ctor,
                        payload,
                    } = scrutinee_value
                    else {
                        return Ok(false);
                    };
                    let scrutinee_tag = format!("{}.{}", scrutinee_tycon, scrutinee_ctor);
                    if scrutinee_tag != ctor_tag {
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
                                binding_map,
                                arm_env_id,
                                &field_val,
                                arm_env_legacy,
                                parent_env_id,
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
                            binding_map,
                            arm_env_id,
                            &payload_val,
                            arm_env_legacy,
                            parent_env_id,
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
                            binding_map,
                            arm_env_id,
                            &field_val,
                            arm_env_legacy,
                            parent_env_id,
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
                            // T-1557: insert_value removed — guard bindings go into FlatEnv (T-1558).

                            let guard_expr_spanned = Arc::new(crate::ast::Spanned::new(
                                CoreExpr::Call {
                                    func: Arc::clone(func),
                                    args: args.to_vec(),
                                    named_args: named_args.to_vec(),
                                    implied: *implied,
                                },
                                match_span.clone(),
                            ));
                            let guard_thunk = Arc::new(Thunk::core_expr(
                                guard_expr_spanned,
                                arm_env_id.0, // The match arm's own FlatEnv scope so that closures capture arm-local names correctly.
                                Arc::clone(ctx),
                                match_span.clone(),
                            ));
                            let guard_result =
                                materialize(&guard_thunk, Some(&match_span), ctx).await?;
                            // call_to_match ignores legacy env (B-515 tracks FlatEnv arm binding).
                            let dummy_env =
                                Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
                            Ok(crate::eval::call_to_match(
                                &guard_result,
                                &dummy_env,
                                ctx,
                                &match_span,
                            )
                            .await)
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
                        binding_map,
                        arm_env_id,
                        &field_val,
                        arm_env_legacy,
                        parent_env_id,
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
                let pat_expr_thunk = Arc::new(Thunk::core_expr(
                    spanned,
                    arm_env_id.0, // B-515: arm FlatEnv allocation pending
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
/// - If `name` is in `binding_map`: allocate a Thunk::value for `scrutinee_value`
///   and fill the arm FlatEnv slot at `binding_map[name]`.
/// - If `name` is NOT in `binding_map`: look up `name` via de Bruijn coordinates
///   (`pin_level`, `pin_slot`) in the enclosing scope, compare with `primitive_eq`;
///   return false (soft skip) if not equal.
///
/// `pin_level` and `pin_slot` are the de Bruijn coordinates of `name` in the enclosing scope.
/// `u32::MAX` for both means no resolver coordinates were available (resolver error) —
/// this propagates as Err.
async fn bind_or_pin_name(
    name: &str,
    binding_map: &IndexMap<String, u32>,
    arm_env_id: crate::arena::ScopeId,
    scrutinee_value: &Value,
    match_span: &Span,
    ctx: &Arc<EvalContext>,
    pin_level: u32,
    pin_slot: u32,
) -> EvalResult<bool> {
    if let Some(&slot) = binding_map.get(name) {
        // Binding: create a materialized thunk for the scrutinee value and fill the arm
        // FlatEnv slot. The resolver assigned (level=0, slot=K) to uses of `name` in the
        // arm body; K matches the slot stored in binding_map.
        let thunk = Arc::new(Thunk::value(scrutinee_value.clone(), match_span.clone()));
        let thunk_id = ctx.alloc_thunk(arm_env_id.0, thunk);
        ctx.scope_arena
            .borrow_mut()
            .fill_slot(arm_env_id, slot, thunk_id);
        Ok(true)
    } else {
        // Pin: look up name via de Bruijn coordinates in env, compare with scrutinee.
        if pin_level == u32::MAX || pin_slot == u32::MAX {
            return Err(EvalError::internal(
                format!("pattern pin '{name}': no resolver coordinates (annotation without binding declaration?)"),
                match_span.clone(),
            ).into());
        }
        // FlatEnv dispatch for pin lookup: look up (pin_level, pin_slot) via parent chain.
        // walk_parent_chain walks `pin_level` hops from arm_env_id to reach the target scope.
        let pin_thunk = {
            let arena = ctx.scope_arena.borrow();
            let level_idx = pin_level as usize;
            match arena.walk_parent_chain(arm_env_id.0, level_idx) {
                Err(depth_reached) => {
                    drop(arena);
                    return Err(EvalError::internal(
                        format!(
                            "pattern pin '{name}': level={pin_level} out of range (chain depth={depth_reached})"
                        ),
                        match_span.clone(),
                    )
                    .into());
                }
                Ok(target_env_id) => {
                    let slot_idx = pin_slot as usize;
                    arena.scopes[target_env_id.0 as usize]
                        .get(slot_idx as u32)
                        .map(Arc::clone)
                }
            }
        };
        match pin_thunk {
            Some(t) => {
                let pin_val = materialize(&t, Some(match_span), ctx).await?;
                Ok(primitive_eq(pin_val, scrutinee_value.clone()))
            }
            None => Err(EvalError::internal(
                format!(
                    "pattern pin '{name}' at level={pin_level} slot={pin_slot}: slot empty in FlatEnv"
                ),
                match_span.clone(),
            )
            .into()),
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
/// Execute the CEK machine with an initial action and an empty stack.
///
/// Execute the CEK machine with a pre-populated continuation stack.
///
/// Called by run_owned() after setting up the initial Memoize continuation.
/// The stack parameter allows the caller to pre-push continuations before the CEK loop starts.
pub(crate) async fn run_with_stack(
    initial: Action,
    mut stack: Vec<Cont>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value> {
    let mut action = initial;

    loop {
        match action {
            Action::EvalCore {
                expr,
                env_id,
                ctx: action_ctx,
            } => {
                // Evaluate the CoreExpr to a thunk (without forcing).
                // Pass stored env_id to eval_core_expr.
                // If the result is already materialized (e.g., literals), take the
                // fast path and return Continue(Ok(value)) without pushing to the
                // continuation stack. Otherwise return Materialize to force iteratively.
                action = match eval_core_expr(&expr, env_id, &action_ctx).await {
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
    use crate::value::Thunk;
    fn empty_env() -> Arc<RwLock<crate::env::Env>> {
        Arc::new(RwLock::new(crate::env::Env::new()))
    }

    #[allow(dead_code)]
    fn test_env() -> Arc<RwLock<crate::env::Env>> {
        empty_env()
    }

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, false)
    }

    /// Async shadow of `materialize()` for test contexts.
    async fn materialize(
        thunk: &Arc<crate::value::Thunk>,
        mat_span: Option<&crate::ast::Span>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::eval::materialize(thunk, mat_span, ctx).await
    }

    /// Async shadow of `run()` for test contexts.
    async fn run(initial: Action, ctx: &Arc<EvalContext>) -> crate::error::EvalResult<Value> {
        super::run_with_stack(initial, Vec::new(), ctx).await
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

        // Verify materialization span is set as spans[1] with label "evaluated here"
        assert_eq!(decorated.spans.len(), 2);
        assert_eq!(decorated.spans[1].0, mat_span);
        assert_eq!(decorated.spans[1].1, "evaluated here");

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
        // a note span in spans[2..] pointing to where the value was produced
        // (if different from the assertion site).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        // Create a simple expression that produces an Int
        let value_span = test_span(5, 1, 5, 3); // Line 5: the value production site
        let ctx_for_test = test_ctx();
        let value_thunk = crate::value::Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(42), value_span.clone())),
            0, // root scope
            Arc::clone(&ctx_for_test),
            value_span.clone(),
        );

        // Create a Guarded thunk that expects String but wraps the Int
        let expected_type = Type::Str;
        let guard_span = test_span(10, 1, 10, 20); // Line 10: the assertion site
        let value_id_for_guard = ctx_for_test.alloc_thunk(0, Arc::new(value_thunk));
        let guarded = Arc::new(crate::value::Thunk::guarded(
            value_id_for_guard,
            expected_type,
            Vec::new(),
            guard_span.clone(),
            None,
            None,
        ));

        // Try to materialize - should fail
        let result = materialize(&guarded, Some(&guard_span), &ctx_for_test).await;

        assert!(result.is_err(), "Expected type assertion to fail");
        let err = result.unwrap_err();

        // spans[0] is the primary (definition site), spans[1..] are notes.
        // One of the note spans should point to the value production site.
        let note_spans: Vec<_> = err.spans.iter().skip(1).collect();
        assert!(
            !note_spans.is_empty(),
            "Expected at least one note span in spans[1..]"
        );
        let value_produced_note = note_spans
            .iter()
            .find(|(s, l)| s == &value_span && l == "value produced here");
        assert!(
            value_produced_note.is_some(),
            "Expected a note span pointing to value production site with label 'value produced here'"
        );
    }

    #[tokio::test]
    async fn test_guarded_secondary_span_suppressed_when_same_as_definition() {
        // Test that when the value production site is the same as the assertion site,
        // no "value produced here" note is added (would be redundant).
        // materialize is the local sync shadow defined at the top of this test module
        use crate::types::Type;

        let same_span = test_span(1, 1, 1, 10);

        // Create a value at the same location as the guard
        let ctx_same = test_ctx();
        let value_thunk = crate::value::Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(42), same_span.clone())),
            0, // root scope
            Arc::clone(&ctx_same),
            same_span.clone(),
        );
        let value_id = ctx_same.alloc_thunk(0, Arc::new(value_thunk));

        // Create a Guarded thunk with the same span for both guard and inner
        let guarded = Arc::new(crate::value::Thunk::guarded(
            value_id,
            Type::Str,
            Vec::new(),
            same_span.clone(),
            None,
            None,
        ));

        let result = materialize(&guarded, Some(&same_span), &ctx_same).await;

        assert!(result.is_err());
        let err = result.unwrap_err();

        // No "value produced here" note span should be present because it would be
        // the same as the definition span (redundant).
        let value_produced_note = err.spans.iter().find(|(_, l)| l == "value produced here");
        assert!(
            value_produced_note.is_none(),
            "No 'value produced here' note should be added when spans are equal"
        );
    }

    #[tokio::test]
    async fn test_cont_memoize_caches_result() {
        // Test that Cont::Memoize caches the materialization result into the parent thunk.
        // Create an Unevaluated thunk, force it via the CEK machine (run), and verify
        // it transitions to Materialized state with the correct cached value.
        let span = test_span(1, 1, 1, 10);
        let ctx = test_ctx();

        let thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
            0, // root scope
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

        // Create a thunk that will fail: reference a variable with no resolution entry.
        // slot u32::MAX is out of bounds — get_slot returns None → undefined variable error.
        let thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                span.clone(),
            )),
            0, // root scope
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

        // Create a dict with an entry that will error when materialized
        let error_thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(
                CoreExpr::Var {
                    name: "undefined_var".to_string(),
                    level: 0,
                    slot: u32::MAX,
                    annotation: None,
                },
                span.clone(),
            )),
            0, // root scope
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
        let inner = Arc::new(Thunk::value(Value::Int(42), span.clone()));
        let inner_id = ctx.alloc_thunk(0, inner);

        let guarded = Arc::new(Thunk::guarded(
            inner_id,
            Type::Int,
            vec![],
            span,
            None,
            None,
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
        // T-1557: insert_value removed — values no longer stored in Env. Rewired in T-1558.
        // fallback_val binding was used for testing default expression evaluation; deferred.

        // Inner thunk: a String value — fails the Int guard.
        let inner = Arc::new(Thunk::value(
            crate::value::string_val("not an int"),
            span.clone(),
        ));
        let inner_id = ctx.alloc_thunk(0, inner);

        // Default expression: a literal Int(99) since FlatEnv variable binding is pending (B-515).
        // The default expression evaluates to 99 directly without a variable lookup.
        let default_expr = Arc::new(sp(CoreExpr::Int(99)));

        let guarded = Arc::new(Thunk::guarded(
            inner_id,
            Type::Int,
            vec![],
            span,
            None,
            Some((default_expr, 0)), // root scope
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
        let inner = Arc::new(Thunk::value(Value::Float(1.0), span.clone()));
        let inner_id = ctx.alloc_thunk(0, inner);

        let guarded = Arc::new(Thunk::guarded(
            inner_id,
            Type::Int,
            vec![],
            span,
            None,
            None,
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
        let unevaluated_arg = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span.clone())),
            0, // root scope
            Arc::clone(&ctx),
            span.clone(),
        ));
        let unevaluated_arg_id = ctx.alloc_thunk(0, unevaluated_arg);
        let unevaluated_arg_ref = ctx.get_thunk(unevaluated_arg_id);

        // Verify the arg is NOT yet materialized.
        assert!(
            unevaluated_arg_ref.try_get_materialized().is_none(),
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
        let outer_thunk = Arc::new(Thunk::builtin_call(
            keys_def,
            vec![unevaluated_arg_id], // T-1558: use ThunkId
            None,
            span,
            0, // root scope
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
        let unevaluated_arg0 = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Dict(vec![]), span.clone())),
            0, // root scope
            Arc::clone(&ctx),
            span.clone(),
        ));
        let arg0_id = ctx.alloc_thunk(0, unevaluated_arg0);

        // Arg1: unevaluated int (will be force-materialized but not used by builtin_keys).
        let unevaluated_arg1 = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(42), span.clone())),
            0, // root scope
            Arc::clone(&ctx),
            span.clone(),
        ));
        let arg1_id = ctx.alloc_thunk(0, unevaluated_arg1);

        assert!(
            ctx.get_thunk(arg0_id).try_get_materialized().is_none(),
            "arg0 must be unevaluated"
        );
        assert!(
            ctx.get_thunk(arg1_id).try_get_materialized().is_none(),
            "arg1 must be unevaluated"
        );

        // Custom dummy builtin that accepts any arity and checks both args are pre-materialized.
        let dummy_func: BuiltinFn = |args| {
            // Both args must be materialized by force_count=2 before this is called.
            let _ = args
                .ctx
                .get_thunk(args.args[0])
                .try_get_materialized()
                .expect("pre-materialized by force_count");
            let _ = args
                .ctx
                .get_thunk(args.args[1])
                .try_get_materialized()
                .expect("pre-materialized by force_count");
            let span = args.call_span;
            Box::pin(async move { Ok(Arc::new(Thunk::value(Value::Int(1), span))) })
        };

        const DUMMY_STRICTNESS: &[Strictness] = &[];
        let dummy_def = BuiltinDef {
            func: dummy_func,
            name: "dummy-force2",
            pos_strictness: DUMMY_STRICTNESS,
            force_count: 2,
        };

        let outer_thunk = Arc::new(Thunk::builtin_call(
            dummy_def,
            vec![arg0_id, arg1_id],
            None,
            span,
            0, // root scope
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
        // (the "if first_note.is_none()" branch).

        let thunk_span = test_span(1, 1, 1, 10);
        let err = EvalError::undefined_variable("x".to_string(), thunk_span.clone());
        let mat_span = test_span(10, 5, 10, 6);
        let origin = "test_origin";

        // First attachment — should set spans[1] with label "evaluated here"
        let decorated = attach_materialization_context(
            Box::new(err),
            Some(&mat_span),
            Some(origin),
            thunk_span.clone(),
        );

        assert_eq!(
            decorated.spans.get(1).map(|(s, _)| s),
            Some(&mat_span),
            "spans[1] should be the first materialization span"
        );
        assert_eq!(
            decorated.spans.get(1).map(|(_, l)| l.as_str()),
            Some("evaluated here"),
            "spans[1] label should be 'evaluated here'"
        );

        // Second attachment with a different mat_span — should preserve the first
        let second_mat_span = test_span(20, 1, 20, 5);
        let decorated2 = attach_materialization_context(
            decorated,
            Some(&second_mat_span),
            Some("second_origin"),
            thunk_span,
        );

        // spans[1] should still be the original mat_span (preserved, not overwritten)
        assert_eq!(
            decorated2.spans.get(1).map(|(s, _)| s),
            Some(&mat_span),
            "spans[1] should preserve the first materialization span, not overwrite"
        );
    }
}

// ============================================================================
// T-1667: CEK lifecycle unit tests
// ============================================================================

#[cfg(test)]
mod cek_lifecycle_tests {
    use super::*;
    use crate::test_util::test_span;

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, false)
    }

    /// Verify Cont::Memoize construction: all fields round-trip correctly.
    ///
    /// Cont::Memoize is constructed in force_step when a builtin or function returns
    /// an unevaluated thunk. This test constructs the data struct directly and checks
    /// that the fields match what was provided — a structural invariant, not a
    /// behavior test (behavior is covered by test_cont_memoize_caches_result).
    #[tokio::test]
    async fn test_cont_memoize_construction() {
        let span = test_span(1, 1, 1, 10);
        let thunk = Arc::new(Thunk::value(Value::Int(7), span.clone()));
        let origin: Option<Arc<str>> = Some(Arc::from("test-origin"));

        let data = MemoizeData {
            thunk: Arc::clone(&thunk),
            origin: origin.clone(),
            thunk_span: span.clone(),
            mat_span: Some(span.clone()),
        };
        let cont = Cont::Memoize(Box::new(data));

        // Extract the boxed data and verify fields
        let Cont::Memoize(boxed) = cont else {
            panic!("Expected Cont::Memoize");
        };
        assert!(
            Arc::ptr_eq(&boxed.thunk, &thunk),
            "thunk field must be the same Arc"
        );
        assert_eq!(
            boxed.origin.as_deref(),
            Some("test-origin"),
            "origin field must round-trip"
        );
        assert_eq!(boxed.thunk_span, span, "thunk_span field must round-trip");
        assert_eq!(boxed.mat_span, Some(span), "mat_span field must round-trip");
    }

    /// Pre-materialized thunk passes straight through force_step as Action::Continue.
    ///
    /// When a thunk is already in Materialized state, force_step should return
    /// Action::Continue(Ok(value)) immediately without pushing any continuations.
    /// The continuation stack must remain empty after the call.
    #[tokio::test]
    async fn test_force_step_materialized() {
        let span = test_span(1, 1, 1, 5);
        let ctx = test_ctx();

        // A pre-materialized thunk: created with Thunk::value → already in Materialized state.
        let thunk = Arc::new(Thunk::value(Value::Int(99), span.clone()));

        assert_eq!(
            thunk.try_get_materialized(),
            Some(Value::Int(99)),
            "Thunk::value must start Materialized"
        );

        let mut stack: Vec<Cont> = Vec::new();
        let action = force_step(&thunk, None, &mut stack, &ctx).await;

        // A materialized thunk must return Continue immediately.
        match action {
            Action::Continue(Ok(v)) => {
                assert_eq!(v, Value::Int(99), "expected the materialized value Int(99)");
            }
            Action::Continue(Err(e)) => {
                panic!("expected Action::Continue(Ok(Int(99))), got error: {e}")
            }
            Action::Materialize { .. } | Action::EvalCore { .. } => {
                panic!("expected Action::Continue(Ok(Int(99))), got non-Continue action")
            }
        }

        // No continuations should have been pushed for a pre-materialized thunk.
        assert!(
            stack.is_empty(),
            "continuation stack must remain empty for a Materialized thunk"
        );
    }

    /// Failed thunk returns its cached error via force_step.
    ///
    /// When a thunk is already in Failed state (error cached via settle(Err(...))),
    /// force_step must return Action::Continue(Err(...)) without re-evaluating.
    #[tokio::test]
    async fn test_force_step_failed() {
        let span = test_span(2, 1, 2, 5);
        let ctx = test_ctx();

        // Create an Unevaluated thunk and force it to fail by settling with an error.
        let thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(0), span.clone())),
            0,
            Arc::clone(&ctx),
            span.clone(),
        ));
        // Inject a cached failure directly so we test the Failed branch without
        // triggering a full eval cycle.
        let err = EvalError::user_error("sentinel-error".to_string(), span.clone());
        thunk.settle(Err(Arc::new(err)));

        // Thunk must now be in Failed state.
        assert!(
            thunk.get_cached_error().is_some(),
            "thunk must be in Failed state after settle(Err(...))"
        );

        let mut stack: Vec<Cont> = Vec::new();
        let action = force_step(&thunk, None, &mut stack, &ctx).await;

        match action {
            Action::Continue(Err(e)) => {
                assert!(
                    e.kind.to_string().contains("sentinel-error"),
                    "error must propagate from Failed state: {}",
                    e.kind
                );
            }
            Action::Continue(Ok(v)) => {
                panic!("expected Action::Continue(Err(...)), got Ok({v:?})")
            }
            Action::Materialize { .. } | Action::EvalCore { .. } => {
                panic!("expected Action::Continue(Err(...)), got non-Continue action")
            }
        }

        // No continuations pushed for a failed thunk.
        assert!(
            stack.is_empty(),
            "continuation stack must remain empty for a Failed thunk"
        );
    }

    /// InProgress thunk from the same task produces a circular_dependency error.
    ///
    /// The cycle detection branch in force_step fires when:
    /// - ThunkState::InProgress { evaluating_task } is found, AND
    /// - the evaluating task matches the current task (same == true).
    ///
    /// We simulate this by claiming the thunk's UnevaluatedState (transition to InProgress)
    /// within a TASK_EVAL_STACK scope, then calling force_step without settling the thunk.
    #[tokio::test]
    async fn test_thunk_cycle_detection() {
        let span = test_span(3, 1, 3, 5);
        let ctx = test_ctx();

        // Create an unevaluated thunk.
        let thunk = Arc::new(Thunk::core_expr(
            Arc::new(Spanned::new(CoreExpr::Int(0), span.clone())),
            0,
            Arc::clone(&ctx),
            span.clone(),
        ));

        // Transition thunk to InProgress by claiming its UnevaluatedState.
        // After try_claim() the unevaluated field is None and evaluating_task is set
        // to the current task id — exactly the InProgress state.
        let _claimed = thunk.try_claim().expect("fresh thunk must be claimable");

        // Verify InProgress state before calling force_step.
        assert!(
            matches!(thunk.state(), ThunkState::InProgress { .. }),
            "thunk must be InProgress after try_claim"
        );

        // Run force_step inside a TASK_EVAL_STACK scope so cycle_path reconstruction works.
        let result = TASK_EVAL_STACK
            .scope(std::cell::RefCell::new(vec![]), async {
                let mut stack: Vec<Cont> = Vec::new();
                force_step(&thunk, None, &mut stack, &ctx).await
            })
            .await;

        // force_step must return Action::Continue(Err(circular_dependency)).
        match result {
            Action::Continue(Err(e)) => {
                let msg = e.kind.to_string();
                assert!(
                    msg.contains("circular") || msg.contains("cycle") || msg.contains("dependency"),
                    "expected circular_dependency error, got: {msg}"
                );
            }
            Action::Continue(Ok(v)) => {
                panic!("expected Action::Continue(Err(circular_dependency)), got Ok({v:?})")
            }
            Action::Materialize { .. } | Action::EvalCore { .. } => {
                panic!(
                    "expected Action::Continue(Err(circular_dependency)), got non-Continue action"
                )
            }
        }

        // Cycle detection calls thunk.settle(Err(...)) — thunk must be in Failed state.
        assert!(
            matches!(thunk.state(), ThunkState::Failed(_)),
            "cycle detection must settle thunk to Failed; got {:?}",
            thunk.state()
        );
    }

    /// TASK_EVAL_STACK push/pop via try_with works correctly.
    ///
    /// EvalStackGuard::push adds an entry and Drop removes it. This test verifies
    /// the task-local stack is empty before, non-empty after push, and empty again
    /// after the guard is dropped — using try_with to read the stack state.
    #[tokio::test]
    async fn test_eval_stack_guard_push_pop() {
        let span = test_span(1, 1, 1, 5);

        TASK_EVAL_STACK
            .scope(std::cell::RefCell::new(vec![]), async move {
                // Stack must start empty inside the scope.
                let len_before = TASK_EVAL_STACK
                    .try_with(|s| s.borrow().len())
                    .expect("TASK_EVAL_STACK must be set inside scope");
                assert_eq!(len_before, 0, "stack must be empty before push");

                {
                    let _guard = EvalStackGuard::push((Arc::from("test-frame"), span.clone()));

                    // Stack must have exactly one entry while guard is alive.
                    let len_during = TASK_EVAL_STACK
                        .try_with(|s| s.borrow().len())
                        .expect("TASK_EVAL_STACK must be set inside scope");
                    assert_eq!(len_during, 1, "stack must have 1 entry after push");

                    let label = TASK_EVAL_STACK
                        .try_with(|s| s.borrow()[0].0.to_string())
                        .expect("TASK_EVAL_STACK must be set inside scope");
                    assert_eq!(label, "test-frame", "pushed label must match");
                }
                // Guard dropped here — pop fires.

                // Stack must be empty again after guard is dropped.
                let len_after = TASK_EVAL_STACK
                    .try_with(|s| s.borrow().len())
                    .expect("TASK_EVAL_STACK must be set inside scope");
                assert_eq!(len_after, 0, "stack must be empty after guard drop");
            })
            .await;
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
                    let deep_thunk = Arc::new(Thunk::value(deep_val, thunk.span.clone()));
                    let deep_id = ctx.alloc_thunk(0, deep_thunk);
                    new_map.insert(key.clone(), deep_id);
                }
                Ok(Value::Dict(new_map))
            }
            Value::Variant {
                tycon,
                ctor,
                payload,
            } => {
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
                        Arc::new(Thunk::value(deep_payload, payload_thunk.span.clone()));
                    let deep_id = ctx.alloc_thunk(0, deep_thunk);
                    Ok(Value::Variant {
                        tycon: tycon.clone(),
                        ctor: ctor.clone(),
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
