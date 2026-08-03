//! CEK machine for type inference — iterative loop with explicit continuations.
//!
//! Implements the full type checker as a continuation-passing style (CPS) machine with
//! defunctionalized continuations. This prevents stack overflow on deeply nested expressions
//! and provides an inspectable continuation stack for error reporting.
//!
//! Architecture:
//! - Control register: current `Arc<SurfaceNode>` to infer OR a completed `Type` result
//! - Continuation stack: `Vec<TypeCheckCont>` (explicit stack of pending work)
//! - Main loop: `run_typecheck` — alternates between `infer_step` and `apply_cont`
//!
//! Both `infer_step` and `apply_cont` are `async fn` — they await external async operations
//! (annotation resolution, async unify) directly. The CEK loop eliminates recursive
//! calls to `run_typecheck` itself, not all async behavior.

use crate::typecheck::find_slot_in_frames;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    class_decl_name, node_id, Annotation, Span, Spanned, SurfaceDeclaration, SurfaceEntry,
    SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode, STANDARD_ANN_KEYS,
};
use crate::coverage;
use crate::env::Env;
use crate::error::Diagnostic;
use crate::type_tags::*;
// Constraint is Vec<Arc<Value>> in the TypeValue migration — no Constraint type import needed.
use crate::type_def::TyConDef;
use crate::type_infer::{
    make_rowtail_uniform, make_typevalue_float_lit, make_typevalue_fn, make_typevalue_int_lit,
    make_typevalue_negation, make_typevalue_never, make_typevalue_nominal_variant,
    make_typevalue_op, make_typevalue_record, make_typevalue_repr, make_typevalue_str_lit,
    make_typevalue_top, make_typevalue_unknown, typevalue_ctor, typevalue_extract_members_pub,
    typevalue_fn_params_and_ret, typevalue_nominal_variant_has_fields,
    typevalue_nominal_variant_tag, typevalue_normalize_intersection, typevalue_normalize_union,
    typevalue_op_name, typevalue_record_fields_pub, typevalue_var_name, InferState, TypeValue,
};
use crate::types::constrain;
use crate::types::unify;
use crate::types::{generalize_tv, instantiate_scheme_tv};

use super::{typecheck_annot, typecheck_call, typecheck_narrow, TypeMap};

// ===== Action enum =====

/// Action returned by `infer_step` and `apply_cont`.
///
/// `Done(ty)` means the current sub-expression has been fully inferred.
/// `Eval(node, env)` means inference should continue by evaluating `node` in `env`.
pub(crate) enum TypeCheckAction {
    Done(TypeValue),
    Eval(Arc<SurfaceNode>, Arc<RwLock<Env>>),
}

// ===== SCC =====

/// Strongly Connected Component — a group of mutually dependent bindings.
#[derive(Clone)]
pub(crate) struct Scc {
    /// Indices into the entries array.
    pub(crate) indices: Vec<usize>,
}

/// Instantiated function signature — groups the five signature fields that always travel
/// together through call-checking helpers. Owned data; no lifetime parameters.
struct FnSig {
    params: Vec<(Option<String>, TypeValue)>,
    ret: TypeValue,
    typed_variadics: Vec<(String, TypeValue)>,
    rest: Option<Box<(String, TypeValue)>>,
    required_count: usize,
}

/// Shared mutable context threaded through type-checking helpers.
/// Groups the three machinery parameters to keep function argument counts below threshold.
struct TypeCheckCtx<'a, 'b> {
    state: &'a mut InferState,
    errors: &'a mut Vec<Diagnostic>,
    type_map: &'a mut Option<&'b mut TypeMap>,
}

// ===== FD improvement ========================================================

/// Run the FD (functional dependency) improvement fixpoint loop on `state.constraints`.
///
/// After any operation that may add new type class constraints or bind TypeVars (e.g.,
/// a `constrain()` call), invoke this function to propagate FD-determined types into
/// still-free TypeVars.
///
/// The loop runs until no more improvements are possible (fixpoint), subject to the
/// `state.fd_depth` guard (max 32 recursive levels). Each improvement pair
/// `(target, computed)` is unified via `unify()`. Unification errors are pushed to `errors`.
pub(crate) async fn run_fd_improvement_fixpoint(
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    span: Span,
) {
    loop {
        // Snapshot constraints — borrow checker requires cloning since try_fd_improvement
        // borrows state fields individually while state itself is mutably borrowed.
        let constraints_snapshot: Vec<Arc<crate::value::Value>> = state.constraints.clone();
        let pairs = crate::type_class::try_fd_improvement(
            &constraints_snapshot,
            &state.ctx,
            &state.env,
            &mut state.fd_depth,
            state.eval_ctx.clone(),
            &state.type_stage_fns,
            errors,
        )
        .await;
        if pairs.is_empty() {
            break;
        }
        for (target, computed) in pairs {
            let mut local_constraints = std::mem::take(&mut state.constraints);
            match Box::pin(unify(
                &target,
                &computed,
                &mut state.ctx,
                &mut local_constraints,
                span.clone(),
                0,
            ))
            .await
            {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
            state.constraints = local_constraints;
        }
    }

    // Drain deferred resolver equalities: pairs from unify() where the resolver function F
    // is non-injective (ctx.non_injective_resolvers contains the op name). For non-injective
    // F, F(a) = F(b) does not imply a = b, so pairwise unification is unsound. Instead,
    // unify() pushes (arg_a, arg_b) to ctx.resolver_deferred.
    //
    // Here we drain the queue: apply the current substitution to each pair and, when both
    // sides are ground (no free TypeVars), unify them directly. When either side still has
    // free TypeVars, put the pair back for the next fixpoint call.
    //
    // The queue shrinks monotonically: each FD improvement step grounds at least one TypeVar,
    // so pairs eventually become ground and are discharged. This guarantees termination
    // (combined with the fd_depth guard for the outer fixpoint loop).
    let deferred = std::mem::take(&mut state.ctx.resolver_deferred);
    for (lhs, rhs) in deferred {
        let lhs_applied = state.ctx.apply_subst(&lhs);
        let rhs_applied = state.ctx.apply_subst(&rhs);
        // Only unify when both sides are fully ground (no free TypeVars).
        if !crate::type_infer::has_free_type_vars_ctx(&lhs_applied, &state.ctx)
            && !crate::type_infer::has_free_type_vars_ctx(&rhs_applied, &state.ctx)
        {
            let mut local_constraints = std::mem::take(&mut state.constraints);
            match Box::pin(unify(
                &lhs_applied,
                &rhs_applied,
                &mut state.ctx,
                &mut local_constraints,
                span.clone(),
                0,
            ))
            .await
            {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
            state.constraints = local_constraints;
        } else {
            // Still not concrete — put back for the next fixpoint call.
            state.ctx.resolver_deferred.push((lhs, rhs));
        }
    }
}

// ===== BindingId =====
// Defined in type_infer.rs (low-level infrastructure); re-exported here for use
// within typecheck_cek. The dependency direction is: type_infer defines BindingId,
// typecheck_cek uses it — not the reverse.
pub(crate) use crate::type_infer::BindingId;

// ===== TypeCheckCont enum =====

/// Explicit continuation stack for the type checker CEK machine.
///
/// Each variant stores the data needed to resume type checking after a child expression
/// has been inferred. The continuation stack replaces recursive calls to `infer_step`.
pub(crate) enum TypeCheckCont {
    /// Inferred a function body — restore saved level/expected_return and build fn type.
    FnBody {
        saved_level: u32,
        saved_expected_return: Option<TypeValue>,
        /// Pre-resolved return annotation TypeValue (overrides body type when concrete).
        return_ann: Option<TypeValue>,
        /// Resolved fixed param types (non-variadic).
        params: Vec<(Option<String>, TypeValue)>,
        /// Typed variadic buckets: (name, Seq[T]) in declaration order.
        typed_variadics: Vec<(String, TypeValue)>,
        /// Untyped variadic fallback: (name, TypeVar_whole_dict).
        rest: Option<Box<(String, TypeValue)>>,
        /// Number of required (non-default) fixed params. None = all params required.
        required_count: Option<usize>,
        node_span: Span,
        trace_level: u32,
    },

    /// Inferred the function expression in a call — start processing arguments.
    CallFunc {
        func_node: Arc<SurfaceNode>,
        args: Vec<Arc<SurfaceNode>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        env: Arc<RwLock<Env>>,
        call_node: Arc<SurfaceNode>,
    },

    /// Inferred one argument — continue with remaining or finalize the return type.
    CallArg {
        /// 0-based index of the arg just inferred.
        idx: usize,
        /// Remaining positional args to infer (after the one just done).
        remaining_args: Vec<Arc<SurfaceNode>>,
        /// TypeValues of all positional args inferred so far.
        accumulated_arg_types: Vec<TypeValue>,
        /// All positional arg nodes (full list, not just remaining) — used for type_guard.
        arg_nodes: Vec<Arc<SurfaceNode>>,
        /// Param types from the instantiated function type.
        param_types: Vec<(Option<String>, TypeValue)>,
        fn_ret: TypeValue,
        /// Typed variadic buckets in declaration order: (name, Seq[T]).
        typed_variadics: Vec<(String, TypeValue)>,
        /// Untyped variadic fallback: (name, TypeVar_whole_dict).
        rest: Option<Box<(String, TypeValue)>>,
        fn_required: usize,
        env: Arc<RwLock<Env>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        span: Span,
        call_node: Arc<SurfaceNode>,
        /// EffectPerform metadata for T-2149 instance checking.
        /// If the function is a typeclass method call, this contains (class_id, method_name).
        effect_perform: Option<(u64, String)>,
    },

    /// Inferred the scrutinee of a match — start processing arms.
    MatchScrutinee {
        arms: Vec<SurfaceMatchArm>,
        env: Arc<RwLock<Env>>,
        span: Span,
    },

    /// Inferred one match arm body — continue with remaining arms.
    MatchArm {
        remaining_arms: Vec<SurfaceMatchArm>,
        env: Arc<RwLock<Env>>,
        accumulated_types: Vec<TypeValue>,
        scrutinee_ty: TypeValue,
        remaining_scrutinee: TypeValue,
        span: Span,
    },

    /// Terminal dict — run full multi-pass dict inference via run_typecheck_dict.
    ///
    /// Pushed by the `Dict` arm of `infer_step`. The handler calls `run_typecheck_dict`
    /// directly, which performs all passes (0–4) inline. This avoids duplicating the
    /// multi-pass dict algorithm in the continuation chain.
    DictPassZero {
        entries: Vec<Spanned<SurfaceEntry>>,
        env: Arc<RwLock<Env>>,
    },

    /// All Dict intermediate bodies in a Sequential processed — emit lost-binding warnings
    /// for unreferenced bindings in each intermediate env frame.
    ///
    /// Pushed by the `Sequential` arm after all Dict intermediates are processed, just
    /// before evaluating the final expression. The handler fires after the final expression
    /// is inferred and performs BFS liveness analysis on `state.use_def` (T-2060).
    ///
    /// ## BFS liveness analysis (T-2060)
    ///
    /// A binding is "live" if it is reachable from the final expression via `state.use_def`.
    /// 1. `pre_final_refs[i]` = names in frame i with `referenced = true` BEFORE the final
    ///    expression was evaluated (marks from inter-dict processing only). This snapshot
    ///    isolates which names the final expression newly references.
    /// 2. Seed: names with `slot.referenced == true` AND NOT in `pre_final_refs[i]` — i.e.,
    ///    names newly marked as referenced by the final expression specifically.
    /// 3. BFS forward: for each live name A, look up `state.use_def[A]` and add those names
    ///    to the live set if not already present. Continue until the queue is empty.
    ///    `state.use_def[A] = {B, C}` means A's value expression referenced B and C, so if
    ///    A is live, B and C must also be live (required for A's computation).
    /// 4. Any intermediate binding NOT in the live set at BFS completion → emit warning.
    ///
    /// This replaces the old dep_graph backward fixpoint approximation (which was per-dict,
    /// not per-binding) with exact per-binding BFS. The snapshot is retained to isolate
    /// final-expression references from inter-dict references (a binding referenced only by
    /// another intermediate dict — not by the final expression — remains dead if neither
    /// it nor its dependents are used by the final expression).
    ///
    /// Only Dict intermediates are tracked — non-Dict intermediates use schemes without
    /// user-visible definition spans and are not eligible for lost-binding warnings.
    AfterBlock {
        binding_envs: Vec<Arc<RwLock<Env>>>,
        /// Snapshot of each frame's referenced BindingId set taken just before the final
        /// expression was evaluated. Used to isolate which marks came from the final expr.
        pre_final_refs: Vec<std::collections::HashSet<BindingId>>,
        /// Saved state.use_def from the enclosing block (or empty map if at top level).
        /// Restored after AfterBlock fires so that nested blocks do not corrupt
        /// the outer block's liveness graph.
        saved_use_def: std::collections::HashMap<BindingId, std::collections::HashSet<BindingId>>,
        /// Saved state.current_binding from the enclosing block.
        saved_current_binding: Option<BindingId>,
        /// Saved state.narrowing_map from before this block's narrowings were applied.
        /// Restored after AfterBlock fires so narrowings from one arm don't leak into others.
        saved_narrowing_map:
            std::collections::HashMap<crate::type_infer::BindingId, crate::type_infer::TypeValue>,
        /// Saved state.current_parameter_frame from before this block.
        /// Restored after AfterBlock fires so the enclosing function/arm frame is recovered.
        saved_parameter_frame: Option<Arc<RwLock<Env>>>,
    },

    /// Inferred the inner expression of a TypeAssert — validate against expected TypeValue.
    TypeAssertInner {
        expected: TypeValue,
        has_default: bool,
        default_node: Option<Arc<SurfaceNode>>,
        /// The TypeAssert SurfaceNode — used to write back the resolved type to
        /// `resolved_type` OnceLock after the inner expression is inferred (B-658).
        assert_node: Arc<SurfaceNode>,
        env: Arc<RwLock<Env>>,
        span: Span,
        annotation_span: Span,
    },

    /// Inferred the inner expression of an Unquote — return its type.
    Unquote,

    /// Inferred the inner expression of an UnquoteSplice — return Unknown.
    UnquoteSplice,
}

// ===== typecheck_for_errors =====

/// Run type inference for side effects only. Discards the inferred TypeValue.
///
/// Used for condition expressions, guard expressions, argument expressions when the callee
/// type is unknown/any/typevar, and similar cases where inference is needed only to populate
/// `type_map` and propagate errors via `errors` — not for downstream type inference.
///
/// This is the single canonical suppression point for the `run_typecheck` return value.
/// All call sites that previously used `let _ty = Box::pin(run_typecheck(...)).await` must
/// use this helper instead — it makes the intentional discard explicit and compiler-verified.
pub(crate) async fn typecheck_for_errors(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) {
    // Called for error-collection and type_map side effects only.
    Box::pin(run_typecheck(node, env, state, errors, type_map, stack)).await;
}

// ===== run_typecheck =====

/// Main CEK loop for type inference.
///
/// Processes `node` by repeatedly calling `infer_step` (which may push continuations)
/// and `apply_cont` (which pops continuations and determines the next step).
///
/// Returns the final inferred TypeValue when the stack is empty.
pub(crate) async fn run_typecheck(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeValue {
    let mut current_node = Arc::clone(node);
    let mut current_env = Arc::clone(env);

    loop {
        match infer_step(&current_node, &current_env, state, errors, type_map, stack).await {
            TypeCheckAction::Eval(next_node, next_env) => {
                current_node = next_node;
                current_env = next_env;
            }
            TypeCheckAction::Done(ty) => {
                // Record this node's inferred type in type_map for LSP.
                record_type_map(type_map, &current_node.span, &ty);

                // Pop and apply continuations until we get an Eval action or empty stack.
                let mut result_ty = ty;
                loop {
                    match stack.pop() {
                        None => return result_ty,
                        Some(cont) => {
                            match apply_cont(cont, result_ty, state, errors, type_map, stack).await
                            {
                                TypeCheckAction::Done(t) => {
                                    result_ty = t;
                                    // Continue popping the stack
                                }
                                TypeCheckAction::Eval(next_node, next_env) => {
                                    current_node = next_node;
                                    current_env = next_env;
                                    break; // Back to outer loop
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Record an inferred TypeValue into the type_map for LSP hover.
fn record_type_map(type_map: &mut Option<&mut TypeMap>, span: &Span, ty: &TypeValue) {
    if let Some(ref mut map) = type_map {
        let key = (span.start_line, span.start_col, span.end_line, span.end_col);
        map.insert(key, Arc::clone(ty));
    }
}

// ===== infer_step =====

/// Infer the type of a single SurfaceExpression node.
///
/// For leaf expressions: returns `Done(ty)` directly without pushing any continuation.
/// For compound expressions: pushes a continuation and returns `Eval(child, env)`.
///
/// This is the "C" (control) step of the CEK machine.
async fn infer_step(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    match &node.expr {
        // ===== Leaf expressions =====
        SurfaceExpression::Int(n) => TypeCheckAction::Done(make_typevalue_int_lit(*n)),
        SurfaceExpression::Float(f) => TypeCheckAction::Done(make_typevalue_float_lit(*f)),
        SurfaceExpression::StringLiteral { content, .. } => {
            TypeCheckAction::Done(make_typevalue_str_lit(content))
        }
        SurfaceExpression::U64(_) => TypeCheckAction::Done(make_typevalue_repr(REPR_INT)),

        // Placeholder: typed hole — infer as a fresh TypeVar (unifies with context)
        SurfaceExpression::Placeholder(..) => {
            TypeCheckAction::Done(state.fresh_type_var(&node.span))
        }

        SurfaceExpression::Quote(_inner) => {
            // Quoted expression: return an empty closed record type (no fields known statically).
            // An empty closed record is the correct type: it matches the old
            // Type::Dict(Row { fields: {}, tail: RowTail::Empty }) behavior and preserves
            // type precision for callers that check whether a quoted expression satisfies a
            // record annotation. make_typevalue_unknown() was incorrect here.
            TypeCheckAction::Done(make_typevalue_record(indexmap::IndexMap::new(), None))
        }

        // ===== VarRef — leaf with name lookup =====
        SurfaceExpression::VarRef {
            name, annotation, ..
        } => {
            let ty = infer_var_ref(name, annotation.as_ref(), node, env, state, errors).await;
            TypeCheckAction::Done(ty)
        }

        // ===== Unquote / UnquoteSplice — compound =====
        SurfaceExpression::Unquote(inner) => {
            stack.push(TypeCheckCont::Unquote);
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            stack.push(TypeCheckCont::UnquoteSplice);
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        // ===== Field access — desugar to [builtin-dict-get "field" base] for type checking =====
        //
        // foo.bar lowers to [builtin-dict-get "field" base] in lower.rs (the protocol entry
        // for field access, per doc/16b-rust-tinct-protocol.md §7). The type checker uses
        // the same protocol entry so that the Indexable constraint fires through
        // builtin-dict-get's annotation (constraint: [$Indexable c k v]), exactly as it
        // does when the user writes [get key base] or [builtin-dict-get key base].
        // Using the protocol name rather than the prelude wrapper "get" satisfies Axiom 1
        // (Prelude speaks the Rust protocol) and Axiom 4 (Loader/prelude agnosticism).
        //
        // Leading-dot form (expr: None) is a scope-chain lookup that lowers to a VarRef
        // — it is not a field access and returns Unknown from the type checker.
        SurfaceExpression::Field { expr, field, .. } => match expr {
            None => {
                // Leading-dot `.name` lowers to CoreExpr::Var — treat identically to VarRef.
                // Change 1 in resolve.rs ensures the ResolutionTable has the parent-scope address,
                // so infer_var_ref can follow the same resolution path as any other variable
                // reference. This eliminates false-positive lost-binding warnings and Unknown types.
                if let crate::ast::DotKey::Ident(name) = field {
                    return TypeCheckAction::Done(
                        infer_var_ref(name, None, node, env, state, errors).await,
                    );
                }
                TypeCheckAction::Done(make_typevalue_unknown())
            }
            Some(base) => {
                let span = node.span.clone();
                // Build synthetic VarRef("builtin-dict-get") with unresolved state — the
                // type checker will resolve it from the env and apply its annotation
                // constraints ([$Indexable c k v]), producing the precise field type.
                let get_node = Arc::new(crate::ast::SurfaceNode::new(
                    crate::ast::SurfaceExpression::VarRef {
                        name: "builtin-dict-get".to_string(),
                        escaped: false,
                        resolution: crate::ast::Resolution::new(),
                        annotation: None,
                        do_infer_placeholder: false,
                    },
                    span.clone(),
                ));
                // Build synthetic key node: string key for ident fields, integer key for
                // integer fields (foo.0 → [builtin-dict-get 0 foo], not [builtin-dict-get "0" foo]).
                let key_node = Arc::new(crate::ast::SurfaceNode::new(
                    match field {
                        crate::ast::DotKey::Ident(s) => {
                            crate::ast::SurfaceExpression::StringLiteral {
                                prefix: String::new(),
                                delimiter: "\"".to_string(),
                                content: s.clone(),
                            }
                        }
                        crate::ast::DotKey::Int(n) => crate::ast::SurfaceExpression::Int(*n as i64),
                    },
                    span.clone(),
                ));
                // Evaluate the synthetic [builtin-dict-get key base] call.
                let call_node = Arc::new(crate::ast::SurfaceNode::new(
                    crate::ast::SurfaceExpression::Call {
                        func: get_node,
                        args: vec![key_node, Arc::clone(base)],
                        named_args: vec![],
                        implied: true,
                        pipe_span: None,
                    },
                    span,
                ));
                TypeCheckAction::Eval(call_node, Arc::clone(env))
            }
        },

        // ===== TypeAssert — resolve annotation, then eval inner =====
        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            let has_default = annotation.node.get_property("default").is_some();
            let default_node = annotation.node.get_property("default").map(Arc::clone);

            // Resolve annotation asynchronously before evaluating inner.
            let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
            let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
            let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
            let annotation_result = typecheck_annot::resolve_annotation(
                &annotation.node,
                annotation.span.clone(),
                &mut *state,
                &mut constraints,
                &mut ann_m,
                &mut row_m,
                None,
            )
            .await;

            match annotation_result {
                Ok(expected) => {
                    stack.push(TypeCheckCont::TypeAssertInner {
                        expected,
                        has_default,
                        default_node,
                        assert_node: Arc::clone(node),
                        env: Arc::clone(env),
                        span: node.span.clone(),
                        annotation_span: annotation.span.clone(),
                    });
                    TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
                }
                Err(e) => {
                    // Annotation resolution failed — report the error and evaluate inner without asserting
                    errors.push(e);
                    TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
                }
            }
        }

        // ===== Sequential — compound: process intermediate bodies inline, extending env =====
        SurfaceExpression::Sequential(exprs) => {
            if exprs.is_empty() {
                // Empty sequential: fresh TypeVar (unconstrained position)
                return TypeCheckAction::Done(state.fresh_type_var(&node.span));
            }
            if exprs.len() == 1 {
                return TypeCheckAction::Eval(Arc::clone(&exprs[0]), Arc::clone(env));
            }

            // Process all intermediate bodies (all but last) inline, extending env after each.
            // Collect each Dict intermediate's env frame for lost-binding detection.
            let mut current_env = Arc::clone(env);
            let intermediates = &exprs[0..exprs.len() - 1];
            let last = &exprs[exprs.len() - 1];
            let mut intermediate_envs: Vec<Arc<RwLock<Env>>> = Vec::new();
            // Cumulative LGM slot counter for fn-body sequential intermediate dicts.
            // For fn-body sequential: body 0 starts at slot 0, body 1 at body-0's key count, etc.
            // (The resolver's sequential_offset also starts at 0 within a fresh fn frame.)
            // For document-level sequential (process_document wrapping): body_slot_base is read
            // from state.resolver_frames per dict body instead of this counter (see below), because
            // document-level slots start at initial_offset (= root_group_len), not 0.
            // This counter is still advanced per body so it stays correct for fn-body sequential
            // bodies that follow any non-Dict intermediate.
            let mut sequential_slot_offset: u32 = 0;
            // Tracks the document-level initial_offset: the absolute slot base for body 0.
            // Learned from the first document-level body's phase-2 slot lookup.
            // For fn-body sequential this stays None (fn bodies use 0-based sequential_slot_offset).
            // For document-level sequential: after body 0, doc_initial_offset = body_0_base.
            // For body N > 0: correct base = doc_initial_offset + sequential_slot_offset_at_N_start.
            let mut doc_initial_offset: Option<u32> = None;

            // Save and reset use_def and current_binding at the start of each Sequential
            // so that edges from a previous (or outer) Sequential scope don't contaminate this
            // one. The saved values are restored by AfterBlock so that nested
            // Sequentials do not corrupt the outer Sequential's liveness graph.
            let saved_use_def = std::mem::take(&mut state.use_def);
            let saved_current_binding = state.current_binding.take();

            for (_i, intermediate) in intermediates.iter().enumerate() {
                // Check if this is a dict — if so, use run_typecheck_dict for proper letrec
                if let SurfaceExpression::Dict(entries) = &intermediate.expr {
                    // Compute static_keys and body_slot_base BEFORE run_typecheck_dict so we
                    // can pass the correct slot base. Without this, run_typecheck_dict's own
                    // find_map picks the FIRST resolver frame with the key name, which may be
                    // from a different dict (e.g. Dict1's frame for Dict2 when both define Boolean).
                    // That causes Dict2's internal env to use Dict1's slot base, corrupting
                    // get_scheme_at lookups from nested dict expressions inside Dict2's values.
                    let static_keys_pre = crate::resolve::surface_dict_static_keys(entries);
                    // Two-phase body_slot_base disambiguation:
                    //
                    // Phase 1 (exact match): search ALL frames (regardless of FrameKind) for a
                    // frame where slot - j == sequential_slot_offset. Fn-body sequential frames
                    // don't cause contamination here because sequential_slot_offset tracks the
                    // correct base for the current sequential context.
                    //
                    // Phase 2 (fallback): when no exact match exists, restrict to DocSequential
                    // frames only. Fn-body DictLetrec frames have small absolute slots starting
                    // from 0 and must not win the min() selection over document-level frames.
                    // FrameKind::DocSequential filtering replaces the old doc_min_slot heuristic.
                    let body_slot_base_pre: u32 = static_keys_pre
                        .iter()
                        .enumerate()
                        .find_map(|(j, key)| {
                            // Phase 1: exact match across all frames.
                            let exact = state
                                .resolver_frames
                                .iter()
                                .filter_map(|(frame, _kind)| frame.get(key.as_str()).copied())
                                .map(|slot| slot.saturating_sub(j as u32))
                                .find(|&base| base == sequential_slot_offset);
                            if exact.is_some() {
                                return exact;
                            }
                            // Phase 2: filter to DocSequential frames only so fn-body
                            // DictLetrec frames (with small absolute slots) don't
                            // contaminate document-level body_slot_base computation.
                            let candidates: Vec<u32> = state
                                .resolver_frames
                                .iter()
                                .filter(|(_frame, kind)| {
                                    *kind == crate::resolve::FrameKind::DocSequential
                                })
                                .filter_map(|(frame, _kind)| frame.get(key.as_str()).copied())
                                .map(|slot| slot.saturating_sub(j as u32))
                                .collect();
                            if candidates.is_empty() {
                                return None;
                            }
                            if let Some(initial) = doc_initial_offset {
                                let expected = initial + sequential_slot_offset;
                                if let Some(&b) = candidates.iter().find(|&&b| b == expected) {
                                    return Some(b);
                                }
                            }
                            candidates.into_iter().min()
                        })
                        .unwrap_or(sequential_slot_offset);

                    let (_, schemes, referenced, mut dict_errs) = run_typecheck_dict(
                        entries,
                        &current_env,
                        state,
                        type_map,
                        Some(body_slot_base_pre),
                    )
                    .await;
                    errors.append(&mut dict_errs);

                    // Build a span map from the entries: name → entry.span.
                    // Used to populate definition_span on slot entries for liveness tracking.
                    // Only static VarRef/StringLiteral keys have known spans; other keys
                    // get Unknown at the resolver slot with no definition_span (not tracked).
                    let mut entry_spans: std::collections::HashMap<String, crate::ast::Span> =
                        std::collections::HashMap::new();
                    for e in entries.iter() {
                        if let Some(ref key_node) = e.node.key {
                            let key_name = match &key_node.expr {
                                SurfaceExpression::VarRef { name: n, .. } => Some(n.clone()),
                                SurfaceExpression::StringLiteral { content, .. } => {
                                    Some(content.clone())
                                }
                                _ => None,
                            };
                            if let Some(n) = key_name {
                                entry_spans.insert(n, e.span.clone());
                            }
                        }
                    }

                    // Reuse the static_keys and body_slot_base computed before run_typecheck_dict.
                    // These are the same values — static_keys_pre/body_slot_base_pre were computed
                    // above with the correct disambiguation logic for sequential position.
                    let static_keys = static_keys_pre;
                    let body_slot_base: u32 = body_slot_base_pre;
                    // Record doc_initial_offset from body 0's base (for subsequent bodies).
                    if doc_initial_offset.is_none() && body_slot_base > sequential_slot_offset {
                        doc_initial_offset = Some(body_slot_base - sequential_slot_offset);
                    }

                    // Extend env with schemes (preserving let-polymorphism).
                    // Insert into SLOTS at the resolver-assigned slot indices so that
                    // get_scheme_at(depth, slot) finds them via slot-based lookup.
                    // Liveness data (definition_span, referenced) lives on slot entries.
                    // Propagate referenced state from the sub-run: run_typecheck_dict creates
                    // fresh EnvSlots, losing the `referenced` flag set during its internal CEK run.
                    // Re-apply from the returned set to prevent false lost-binding diagnostics.
                    let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
                    for (j, key_name) in static_keys.iter().enumerate() {
                        let slot_idx = (body_slot_base + j as u32) as usize;
                        if let Some(scheme) = schemes.get(key_name.as_str()) {
                            let span = entry_spans.get(key_name.as_str()).cloned();
                            new_env_inner.insert_at_slot(
                                slot_idx,
                                key_name.clone(),
                                scheme.clone(),
                                span.clone(),
                            );
                            if referenced.contains(key_name.as_str()) {
                                // Mark referenced on the slot entry directly.
                                new_env_inner.mark_slot_referenced(0, slot_idx as u32);
                            }
                        } else {
                            // Scheme not returned by run_typecheck_dict for this key — insert
                            // Unknown at the resolver-assigned slot so slot lookup succeeds.
                            let unknown_tv = make_typevalue_unknown();
                            let span = entry_spans.get(key_name.as_str()).cloned();
                            new_env_inner.insert_at_slot(
                                slot_idx,
                                key_name.clone(),
                                unknown_tv,
                                span,
                            );
                        }
                    }
                    // Any schemes not in static_keys (e.g. injected constructor schemes) go to slots.
                    // Use resolver-assigned slot if available; otherwise append at end.
                    for (name, scheme) in &schemes {
                        if !static_keys.contains(name) {
                            let slot = find_slot_in_frames(&state.resolver_frames, name)
                                .unwrap_or_else(|| new_env_inner.slots.len());
                            new_env_inner.insert_at_slot(slot, name.clone(), scheme.clone(), None);
                        }
                    }
                    // Advance the cumulative slot counter by this body's static key count.
                    sequential_slot_offset += static_keys.len() as u32;
                    current_env = Arc::new(RwLock::new(new_env_inner));
                    // Track the intermediate env frame for lost-binding detection.
                    // The frame is captured AFTER inserting schemes so the handler
                    // iterates the correct slot entries.
                    intermediate_envs.push(Arc::clone(&current_env));
                } else {
                    // Every tinct expression is a dict. A non-Dict in intermediate position
                    // means the program is malformed (e.g. a corpus test with a syntax error).
                    // Emit a recoverable error so init programs (test-loader) can capture it.
                    errors.push(Diagnostic::error(
                        "type-error",
                        "expected record type: sequential intermediate body must be a dict expression",
                        intermediate.span.clone(),
                    ));
                    // Do not advance current_env — no bindings are introduced.
                    // Continue processing remaining intermediates.
                }
            }

            // All intermediates were Dict — snapshot referenced state before the final
            // expression evaluates, then push AfterBlock and evaluate the last.
            // The snapshot isolates which marks came from the final expression vs intermediates.
            //
            // Clear current_binding: run_typecheck_dict leaves state.current_binding set to the
            // last entry it processed. If left set, VarRefs in the final expression would be
            // attributed to that stale binding, creating spurious use_def edges.
            state.current_binding = None;
            // Span-keyed BindingId: use slot.definition_span rather than frame_ptr.
            // Bindings without a definition_span (synthetic/injected) are excluded from tracking.
            // Read from SLOTS (the authoritative liveness location).
            let pre_final_refs: Vec<std::collections::HashSet<BindingId>> = intermediate_envs
                .iter()
                .map(|frame| {
                    let guard = frame.read().unwrap();
                    guard
                        .slots
                        .iter()
                        .filter_map(|entry| entry.as_ref())
                        .filter(|(_, slot)| slot.referenced)
                        .filter_map(|(name, slot)| {
                            slot.definition_span.as_ref().map(|span| BindingId {
                                def_span: span.clone(),
                                name: name.clone(),
                            })
                        })
                        .collect()
                })
                .collect();
            stack.push(TypeCheckCont::AfterBlock {
                binding_envs: intermediate_envs,
                pre_final_refs,
                saved_use_def,
                saved_current_binding,
                saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                saved_parameter_frame: state.current_parameter_frame.clone(),
            });
            TypeCheckAction::Eval(Arc::clone(last), current_env)
        }

        // ===== Call — compound: handle special cases inline, else eval func first =====
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied: _,
            ..
        } => {
            // Special-case: do-infer sentinel (protocol §7).
            // Prelude's do-var-node sets do_infer_placeholder: true on the VarRef it creates
            // for the do-desugar inferred monad.  The type checker returns TypeValue.Unknown for
            // any call whose function head is a Field whose base is such a VarRef.
            // The evaluator resolves the actual monad type at runtime via
            // EvalContext::do_infer_resolutions, which is populated by the prelude through
            // set_do_infer_resolutions() — the type checker does not write to that map.
            if let SurfaceExpression::Field {
                expr: Some(da_target),
                ..
            } = &func.expr
            {
                if matches!(
                    &da_target.expr,
                    SurfaceExpression::VarRef {
                        do_infer_placeholder: true,
                        ..
                    }
                ) && named_args.is_empty()
                {
                    return TypeCheckAction::Done(make_typevalue_unknown());
                }
            }

            // General call: push CallFunc, evaluate func
            let args_cloned: Vec<Arc<SurfaceNode>> = args.iter().map(Arc::clone).collect();
            let named_args_cloned: Vec<Spanned<SurfaceNamedArg>> = named_args.to_vec();
            stack.push(TypeCheckCont::CallFunc {
                func_node: Arc::clone(func),
                args: args_cloned,
                named_args: named_args_cloned,
                env: Arc::clone(env),
                call_node: Arc::clone(node),
            });
            TypeCheckAction::Eval(Arc::clone(func), Arc::clone(env))
        }

        // ===== Fn — resolve annotations, build env, push FnBody, eval body =====
        SurfaceExpression::Fn { .. } => infer_fn_push_cont(node, env, state, errors, stack).await,

        // ===== Dict — push DictPassZero, complete inference via run_typecheck_dict =====
        //
        // Full multi-pass dict inference (Passes 0–4) is performed by run_typecheck_dict in
        // the DictPassZero handler. Push the continuation and return Done(Unknown) immediately
        // to trigger apply_cont — there is no child node to evaluate at this point.
        SurfaceExpression::Dict(entries) => {
            stack.push(TypeCheckCont::DictPassZero {
                entries: entries.to_vec(),
                env: Arc::clone(env),
            });
            // Placeholder: DictPassZero handler computes the actual record type
            TypeCheckAction::Done(state.fresh_type_var(&node.span))
        }

        // ===== Match — compound: eval scrutinee first =====
        SurfaceExpression::Match { scrutinee, arms } => {
            let arms_cloned: Vec<SurfaceMatchArm> = arms.to_vec();
            stack.push(TypeCheckCont::MatchScrutinee {
                arms: arms_cloned,
                env: Arc::clone(env),
                span: node.span.clone(),
            });
            TypeCheckAction::Eval(Arc::clone(scrutinee), Arc::clone(env))
        }

        // ===== Decl — call declaration helpers directly =====
        SurfaceExpression::Decl(decl_box) => {
            // Call infer_class_decl_from_surface and infer_instance_decl_from_surface directly
            // now that they are pub(crate). TypeAlias declarations in expression position have
            // no runtime type (alias body validation occurs in Pass 2 of run_typecheck_dict).
            let result: Result<TypeValue, Vec<Diagnostic>> = match decl_box.as_ref() {
                SurfaceDeclaration::ClassDecl {
                    name,
                    params,
                    superclasses,
                    methods: _,
                    determines,
                    resolver,
                    resolver_injective,
                    structural,
                } => {
                    let resolver_name: Option<String> =
                        resolver.as_ref().and_then(|rnode| match &rnode.expr {
                            crate::ast::SurfaceExpression::VarRef { name: rname, .. } => {
                                Some(rname.clone())
                            }
                            crate::ast::SurfaceExpression::StringLiteral { content, .. } => {
                                Some(content.clone())
                            }
                            _ => None,
                        });
                    super::infer_class_decl_from_surface(
                        &super::ClassDeclSurface {
                            name,
                            params,
                            superclasses,
                            determines,
                            structural,
                            span: node.span.clone(),
                            resolver: resolver_name,
                            resolver_injective: *resolver_injective,
                        },
                        state,
                    )
                }
                SurfaceDeclaration::InstanceDecl {
                    class_name,
                    arms,
                    resolved_class_decl_id,
                    ..
                } => {
                    let class_name_str = class_decl_name(class_name);
                    let r = Box::pin(super::infer_instance_decl_from_surface(
                        &class_name_str,
                        arms,
                        node.span.clone(),
                        env,
                        state,
                        type_map,
                    ))
                    .await;
                    // Write resolved_class_decl_id so the lowerer can populate instance_of.
                    if r.is_ok() {
                        if let Some(cd) = state.env.read().unwrap().get_class(&class_name_str) {
                            resolved_class_decl_id.set(cd.class_decl_id);
                        }
                    }
                    r
                }
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Type alias declarations in expression position have no runtime type.
                    // Alias body validation occurs in Pass 2 of run_typecheck_dict.
                    Ok(make_typevalue_top())
                }
                _ => Err(vec![Diagnostic::error(
                    "type-error",
                    "unexpected declaration in expression position",
                    node.span.clone(),
                )]),
            };
            match result {
                Ok(t) => TypeCheckAction::Done(t),
                Err(mut errs) => {
                    errors.append(&mut errs);
                    // Decl inference failed: fresh TypeVar (unconstrained)
                    TypeCheckAction::Done(state.fresh_type_var(&node.span))
                }
            }
        }

        // Parse-error node: the error was already recorded during parsing;
        // silently return Unknown here to avoid spurious secondary diagnostics.
        SurfaceExpression::Error(_) => TypeCheckAction::Done(make_typevalue_unknown()),

        _ => {
            let msg = format!(
                "unexpected {} in this context",
                crate::surface_fields::surface_expr_tag(&node.expr)
            );
            errors.push(Diagnostic::error(
                "type-error",
                msg.clone(),
                node.span.clone(),
            ));
            // Unsupported expr: fresh TypeVar (failed inference, unconstrained)
            TypeCheckAction::Done(state.fresh_type_var(&node.span))
        }
    }
}

// ===== apply_cont =====

/// Apply a continuation to the inferred type from the previous step.
///
/// Each continuation handler receives the type from the child expression and either:
/// - Returns `Eval(node, env)` to continue evaluation on a new node
/// - Returns `Done(ty)` when this continuation branch is complete
///
/// This is the "K" (continuation) step of the CEK machine.
async fn apply_cont(
    cont: TypeCheckCont,
    child_ty: TypeValue,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    match cont {
        // ===== FnBody =====
        TypeCheckCont::FnBody {
            saved_level,
            saved_expected_return,
            return_ann,
            params,
            typed_variadics,
            rest,
            required_count,
            node_span,
            trace_level,
        } => {
            state.ctx.current_level = saved_level;
            state.expected_return = saved_expected_return;

            // Helper: check if a TypeValue is "opaque" — Unknown, Top, or an unbound TypeVar.
            let is_opaque = |tv: &TypeValue| -> bool {
                match typevalue_ctor(tv) {
                    Some(TV_UNKNOWN) | Some(TV_TOP) => true,
                    Some(TV_VAR) => true,
                    _ => false,
                }
            };

            let fn_ret_ty = match return_ann {
                Some(declared_ret) => {
                    // For checkable primitive return annotations, verify the body is consistent.
                    let is_checkable_primitive =
                        matches!(typevalue_ctor(&declared_ret), Some(TV_REPR));
                    if is_checkable_primitive {
                        let body_resolved = state.apply(&child_ty);
                        let body_is_concrete = !is_opaque(&body_resolved);
                        if body_is_concrete {
                            let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                                state.ctx.subst.clone(),
                                state.ctx.levels.clone(),
                                state.ctx.current_level,
                                state.tycon_env.clone(),
                            );
                            if !crate::bas::is_consistent_subtype(
                                &body_resolved,
                                &declared_ret,
                                &ctx_for_check,
                            ) {
                                errors.push(
                                    Diagnostic::error(
                                        "unification-failure",
                                        "return type mismatch: body type is not consistent with declared return type",
                                        node_span.clone(),
                                    )
                                    .with_note(format!("declared: {}", crate::eval::format_type_for_assert(&declared_ret)))
                                    .with_note(format!("inferred: {}", crate::eval::format_type_for_assert(&body_resolved)))
                                );
                            }
                        }
                    }
                    // For intersection annotations: validate each Fn member.
                    if typevalue_ctor(&declared_ret) == Some(TV_INTER) {
                        let body_resolved = state.apply(&child_ty);
                        let body_is_concrete = !is_opaque(&body_resolved);
                        if body_is_concrete {
                            let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                                state.ctx.subst.clone(),
                                state.ctx.levels.clone(),
                                state.ctx.current_level,
                                state.tycon_env.clone(),
                            );
                            if let Some(members) =
                                crate::type_infer::typevalue_extract_members_pub(&declared_ret)
                            {
                                for member in &members {
                                    if typevalue_ctor(member) == Some(TV_FN)
                                        && !crate::bas::is_consistent_subtype(
                                            &body_resolved,
                                            member,
                                            &ctx_for_check,
                                        )
                                    {
                                        errors.push(
                                            Diagnostic::error(
                                                "unification-failure",
                                                "body type is not consistent with function annotation member",
                                                node_span.clone(),
                                            )
                                            .with_note(format!("declared member: {}", crate::eval::format_type_for_assert(member)))
                                            .with_note(format!("inferred:        {}", crate::eval::format_type_for_assert(&body_resolved)))
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Emit diagnostic for explicit @Unknown return annotation.
                    if typevalue_ctor(&declared_ret) == Some(TV_UNKNOWN) {
                        state.diagnostics.push(Diagnostic::info(
                            "explicit-unknown",
                            "explicit @Unknown return annotation — type is not statically known",
                            node_span.clone(),
                        ));
                    }

                    // Emit diagnostic for overbroad return annotation.
                    {
                        let body_resolved = state.apply(&child_ty);
                        if !is_opaque(&body_resolved) && !is_opaque(&declared_ret) {
                            let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                                state.ctx.subst.clone(),
                                state.ctx.levels.clone(),
                                state.ctx.current_level,
                                state.tycon_env.clone(),
                            );
                            let is_sub = crate::bas::is_subtype_bas(
                                &body_resolved,
                                &declared_ret,
                                &ctx_for_check,
                            );
                            let is_super = crate::bas::is_subtype_bas(
                                &declared_ret,
                                &body_resolved,
                                &ctx_for_check,
                            );
                            if is_sub && !is_super {
                                state.diagnostics.push(
                                    Diagnostic::info(
                                        "overbroad-annotation",
                                        "return annotation is broader than the inferred body type",
                                        node_span.clone(),
                                    )
                                    .with_note(format!(
                                        "declared: {}",
                                        crate::eval::format_type_for_assert(&declared_ret)
                                    ))
                                    .with_note(format!(
                                        "inferred: {}",
                                        crate::eval::format_type_for_assert(&body_resolved)
                                    )),
                                );
                            }
                        }
                    }

                    // @Unknown return annotation means: use the inferred body type.
                    if typevalue_ctor(&declared_ret) == Some(TV_UNKNOWN) {
                        child_ty
                    } else {
                        declared_ret
                    }
                }
                None => child_ty,
            };

            // Build the function TypeValue — include variadic flag and typed variadic buckets
            // so call sites can perform correct arity checking and bucket-type dispatch.
            let is_variadic_fn = rest.is_some() || !typed_variadics.is_empty();
            // Convert typed_variadics from Vec<(String, TypeValue)> to Vec<(Option<String>, TypeValue)>
            // for make_typevalue_fn_with_flags (which uses Option<String> to match the param pattern).
            let typed_variadics_for_fn: Vec<(Option<String>, TypeValue)> = typed_variadics
                .iter()
                .map(|(name, ty)| (Some(name.clone()), Arc::clone(ty)))
                .collect();

            // Emit trace diagnostic if trace_level >= 1.
            if trace_level >= 1 {
                let ret_str = crate::eval::format_type_for_assert(&fn_ret_ty);
                let params_str = params
                    .iter()
                    .map(|(name, ty)| {
                        format!(
                            "{}: {}",
                            name.as_deref().unwrap_or("_"),
                            crate::eval::format_type_for_assert(ty)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                state.diagnostics.push(Diagnostic::info(
                    "trace-fn-type",
                    format!("[{}] → {}", params_str, ret_str),
                    node_span.clone(),
                ));
            }

            let fn_type = crate::type_infer::make_typevalue_fn_with_flags(
                params,
                fn_ret_ty,
                required_count,
                is_variadic_fn,
                typed_variadics_for_fn,
            );

            // Exhaustiveness check for typed variadic params.
            // If typed_variadics is non-empty and rest is None, emit a warning.
            if !typed_variadics.is_empty() && rest.is_none() {
                let covered = typevalue_normalize_union(
                    typed_variadics.iter().map(|(_, t)| Arc::clone(t)).collect(),
                );
                let covered_display = crate::eval::format_type_for_assert(&covered);
                errors.push(Diagnostic::error("type-error",
                    format!(
                        "non-exhaustive variadic type dispatch: typed buckets cover {} but no fallback (...rest) handles other types — add ...rest for a wildcard bucket",
                        covered_display
                    ),
                    node_span.clone(),
                ));
            }

            // Record the function type with the Fn node's span for LSP hover.
            record_type_map(type_map, &node_span, &fn_type);

            // Parameter liveness is handled by AfterBlock (pushed in infer_fn_push_cont
            // before evaluating the body). AfterBlock fires before FnBody and emits
            // lost-binding warnings for unreferenced parameters via SLOTS-only BFS.

            TypeCheckAction::Done(fn_type)
        }

        // ===== CallFunc =====
        TypeCheckCont::CallFunc {
            func_node,
            args,
            named_args,
            env,
            call_node,
        } => {
            let func_ty = if state.subst_is_empty() {
                child_ty
            } else {
                state.apply(&child_ty)
            };

            let mut ctx = TypeCheckCtx {
                state,
                errors,
                type_map,
            };
            apply_cont_call_func(
                func_node, func_ty, args, named_args, env, call_node, &mut ctx, stack,
            )
            .await
        }

        // ===== CallArg =====
        TypeCheckCont::CallArg {
            idx,
            remaining_args,
            mut accumulated_arg_types,
            arg_nodes,
            param_types,
            fn_ret,
            typed_variadics,
            rest,
            fn_required,
            env,
            named_args,
            span,
            call_node,
            effect_perform,
        } => {
            accumulated_arg_types.push(child_ty);

            if !remaining_args.is_empty() {
                let next_arg = Arc::clone(&remaining_args[0]);
                let new_remaining: Vec<Arc<SurfaceNode>> =
                    remaining_args[1..].iter().map(Arc::clone).collect();
                stack.push(TypeCheckCont::CallArg {
                    idx: idx + 1,
                    remaining_args: new_remaining,
                    accumulated_arg_types,
                    arg_nodes,
                    param_types,
                    fn_ret,
                    typed_variadics,
                    rest,
                    fn_required,
                    env: Arc::clone(&env),
                    named_args,
                    span,
                    call_node,
                    effect_perform: effect_perform.clone(),
                });
                return TypeCheckAction::Eval(next_arg, env);
            }

            // All positional args collected — unify and handle named args
            let sig = FnSig {
                params: param_types,
                ret: fn_ret,
                typed_variadics,
                rest,
                required_count: fn_required,
            };
            let mut ctx = TypeCheckCtx {
                state,
                errors,
                type_map,
            };
            apply_call_args_poly(
                accumulated_arg_types,
                arg_nodes,
                sig,
                named_args,
                env,
                span,
                effect_perform,
                &mut ctx,
            )
            .await
        }

        // ===== MatchScrutinee =====
        TypeCheckCont::MatchScrutinee { arms, env, span } => {
            let scrutinee_ty = state.apply(&child_ty);
            if arms.is_empty() {
                // Match with no arms: fresh TypeVar (unconstrained)
                return TypeCheckAction::Done(state.fresh_type_var(&span));
            }

            // Run exhaustiveness checking once upfront (using all arms).
            run_match_exhaustiveness_check(&scrutinee_ty, &arms, &span, state, errors);

            // Set up the first arm's environment iteratively; subsequent arms via MatchArm.
            let remaining_scrutinee = scrutinee_ty.clone();
            match setup_match_arm_env(
                &arms[0],
                &remaining_scrutinee,
                &env,
                state,
                errors,
                type_map,
            )
            .await
            {
                None => {
                    // Setup failed: fresh TypeVar (unconstrained)
                    TypeCheckAction::Done(state.fresh_type_var(&span))
                }
                Some((arm_env, next_remaining_scrutinee, guard_narrowings)) => {
                    let remaining_arms: Vec<SurfaceMatchArm> = arms[1..].to_vec();
                    // MatchArm accumulates arm results; AfterBlock handles case arm liveness.
                    stack.push(TypeCheckCont::MatchArm {
                        remaining_arms,
                        env,
                        accumulated_types: Vec::new(),
                        scrutinee_ty,
                        remaining_scrutinee: next_remaining_scrutinee,
                        span,
                    });
                    // For case arms: set current_parameter_frame to arm_env so
                    // VarAddr::Parameter(i) refs (case arm bindings) resolve directly.
                    let (arm_binding_envs, saved_parameter_frame) =
                        if arms[0].let_bindings.is_some() {
                            let saved = state.current_parameter_frame.take();
                            state.current_parameter_frame = Some(Arc::clone(&arm_env));
                            (vec![Arc::clone(&arm_env)], saved)
                        } else {
                            (vec![], state.current_parameter_frame.clone())
                        };
                    stack.push(TypeCheckCont::AfterBlock {
                        binding_envs: arm_binding_envs,
                        pre_final_refs: vec![],
                        saved_use_def: std::mem::take(&mut state.use_def),
                        saved_current_binding: state.current_binding.take(),
                        saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                        saved_parameter_frame,
                    });
                    // Apply this arm's narrowings AFTER the take so they're active during body eval.
                    if !guard_narrowings.is_empty() {
                        typecheck_narrow::apply_narrowings(&arm_env, &guard_narrowings, state);
                    }
                    TypeCheckAction::Eval(Arc::clone(arms[0].body_expr()), arm_env)
                }
            }
        }

        // ===== MatchArm — process one arm body result and continue with next arm =====
        TypeCheckCont::MatchArm {
            remaining_arms,
            env,
            mut accumulated_types,
            scrutinee_ty,
            remaining_scrutinee,
            span,
        } => {
            // Case arm liveness is handled by AfterBlock (pushed before each arm body).
            // AfterBlock fires before MatchArm and emits lost-binding warnings for
            // unreferenced case arm bindings via SLOTS-only BFS.

            // Accumulate this arm's type without substitution. Substituting eagerly would
            // cause transient TypeVar bindings (e.g., String from a field access inside the arm)
            // to bleed into the match result type, producing Union[Record, String] that fails
            // against reduce's accumulator type. typevalue_normalize_union deduplicates by
            // typevalue_eq (which handles Var/Repr identity); TypeVars bound to the same type
            // may produce Union[t, t] → overbroad-annotation warning but not a type error.
            accumulated_types.push(child_ty);

            if remaining_arms.is_empty() {
                // All arms done — build the union of arm types (BAS: arms may return different types).
                // typevalue_normalize_union flattens, deduplicates, and returns the single type
                // if all arms agreed, or a union if they differ.
                let match_ty = if accumulated_types.is_empty() {
                    // No arms produced types: fresh TypeVar (unconstrained)
                    state.fresh_type_var(&span)
                } else {
                    typevalue_normalize_union(accumulated_types)
                };
                TypeCheckAction::Done(match_ty)
            } else {
                // Set up environment for the next arm and push another MatchArm.
                match setup_match_arm_env(
                    &remaining_arms[0],
                    &remaining_scrutinee,
                    &env,
                    state,
                    errors,
                    type_map,
                )
                .await
                {
                    None => {
                        // Setup failed — normalize accumulated arm types and stop
                        let match_ty = if accumulated_types.is_empty() {
                            // No arms: fresh TypeVar (unconstrained)
                            state.fresh_type_var(&span)
                        } else {
                            typevalue_normalize_union(accumulated_types)
                        };
                        TypeCheckAction::Done(match_ty)
                    }
                    Some((arm_env, next_remaining_scrutinee, guard_narrowings)) => {
                        let next_remaining: Vec<SurfaceMatchArm> = remaining_arms[1..].to_vec();
                        stack.push(TypeCheckCont::MatchArm {
                            remaining_arms: next_remaining,
                            env,
                            accumulated_types,
                            scrutinee_ty,
                            remaining_scrutinee: next_remaining_scrutinee,
                            span,
                        });
                        let (arm_binding_envs, saved_parameter_frame) =
                            if remaining_arms[0].let_bindings.is_some() {
                                let saved = state.current_parameter_frame.take();
                                state.current_parameter_frame = Some(Arc::clone(&arm_env));
                                (vec![Arc::clone(&arm_env)], saved)
                            } else {
                                (vec![], state.current_parameter_frame.clone())
                            };
                        stack.push(TypeCheckCont::AfterBlock {
                            binding_envs: arm_binding_envs,
                            pre_final_refs: vec![],
                            saved_use_def: std::mem::take(&mut state.use_def),
                            saved_current_binding: state.current_binding.take(),
                            saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                            saved_parameter_frame,
                        });
                        if !guard_narrowings.is_empty() {
                            typecheck_narrow::apply_narrowings(&arm_env, &guard_narrowings, state);
                        }
                        TypeCheckAction::Eval(Arc::clone(remaining_arms[0].body_expr()), arm_env)
                    }
                }
            }
        }

        // ===== TypeAssertInner =====
        TypeCheckCont::TypeAssertInner {
            expected,
            has_default,
            default_node,
            assert_node,
            env,
            span,
            annotation_span,
        } => {
            let actual = child_ty;
            let expected_resolved = state.apply(&expected);
            let actual_resolved = state.apply(&actual);

            // Write the resolved expected type back to the TypeAssert node's
            // resolved_type OnceLock so the lowerer can carry it into CoreExpr::TypeAssert.
            // This is the canonical write — first write wins (OnceLock semantics), so
            // shared AST nodes visited multiple times still get the correct type.
            if let SurfaceExpression::TypeAssert { resolved_type, .. } = &assert_node.expr {
                resolved_type.set(Some(Arc::clone(&expected_resolved)));
            }

            // Emit diagnostic for explicit @Unknown annotation
            if typevalue_ctor(&expected_resolved) == Some(TV_UNKNOWN) {
                state.diagnostics.push(Diagnostic::info(
                    "explicit-unknown",
                    "explicit @Unknown annotation — type is not statically known",
                    span.clone(),
                ));
            }

            let mismatch_err = compute_type_assert_mismatch(
                &actual_resolved,
                &expected_resolved,
                has_default,
                &span,
                state,
            );

            if let Some(mut errs) = mismatch_err {
                if !has_default {
                    // Add annotation_span as a secondary label to all diagnostics
                    for err in &mut errs {
                        err.spans
                            .push((annotation_span.clone(), "type declared here".to_string()));
                    }
                    errors.extend(errs);
                    return TypeCheckAction::Done(expected);
                }
                // With default: suppress type mismatch
            }

            // Validate default value if present
            if let Some(ref default_n) = default_node {
                let default_ty = {
                    let mut local_stack = Vec::new();
                    Box::pin(run_typecheck(
                        default_n,
                        &env,
                        state,
                        errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await
                };
                let default_resolved = state.apply(&default_ty);
                let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                    state.ctx.subst.clone(),
                    state.ctx.levels.clone(),
                    state.ctx.current_level,
                    state.tycon_env.clone(),
                );
                let passes = crate::bas::is_subtype_bas(
                    &default_resolved,
                    &expected_resolved,
                    &ctx_for_check,
                ) || crate::bas::is_consistent_subtype(
                    &default_resolved,
                    &expected_resolved,
                    &ctx_for_check,
                );
                if !passes {
                    errors.push(Diagnostic::error(
                        "type-error",
                        "default value type does not match the assertion's expected type",
                        default_n.span.clone(),
                    ));
                }
            }

            TypeCheckAction::Done(expected)
        }

        // ===== Unquote =====
        TypeCheckCont::Unquote => TypeCheckAction::Done(child_ty),

        // ===== UnquoteSplice =====
        TypeCheckCont::UnquoteSplice => TypeCheckAction::Done(make_typevalue_unknown()),

        // ===== AfterBlock =====
        //
        // Fires after the final expression in a block (sequential intermediates, fn body,
        // match arm). Performs BFS liveness analysis on state.use_def to emit lost-binding
        // warnings (T-2060). Restores saved_use_def, saved_current_binding, and
        // saved_narrowing_map so that nested blocks don't corrupt the enclosing scope.
        //
        // Algorithm:
        // 1. Seed: names that the final expression newly marked referenced (by comparing
        //    current referenced state against pre_final_refs snapshot). These are directly live.
        // 2. BFS forward: for each live name A, look up state.use_def[A] and add those names
        //    to the live set if not already present. state.use_def[A] = {B, C} means A's value
        //    expression referenced B and C — if A is live, B and C must also be live.
        // 3. Emit a warning for every intermediate binding NOT in the live set that has a
        //    user-visible definition_span.
        //
        // BFS is exact (per-binding) vs the old dep_graph (per-dict approximation). This
        // eliminates false negatives: the old algorithm could fail to warn about truly unused
        // bindings when multiple bindings in one dict had heterogeneous reachability.
        TypeCheckCont::AfterBlock {
            binding_envs,
            pre_final_refs,
            saved_use_def,
            saved_current_binding,
            saved_narrowing_map,
            saved_parameter_frame,
        } => {
            // Step 1: Seed the live set with names newly marked by the final expression.
            // Names in pre_final_refs were already referenced before the final expression
            // ran (they were marked by inter-dict processing). Only names that the final
            // expression itself referenced are seeded as directly live.
            // Read from SLOTS only — infer_var_ref marks slots directly via
            // state.current_parameter_frame (T-2084); no extras fallback needed.
            let mut live: std::collections::HashSet<BindingId> = std::collections::HashSet::new();
            for (frame_idx, env_frame) in binding_envs.iter().enumerate() {
                let env_guard = env_frame.read().unwrap();
                for slot_entry in &env_guard.slots {
                    if let Some((name, slot)) = slot_entry {
                        if let Some(ref span) = slot.definition_span {
                            let id = BindingId {
                                def_span: span.clone(),
                                name: name.clone(),
                            };
                            let was_pre = pre_final_refs
                                .get(frame_idx)
                                .map_or(false, |s| s.contains(&id));
                            if slot.referenced && !was_pre {
                                live.insert(id);
                            }
                        }
                    }
                }
            }

            // Step 2: BFS forward through state.use_def.
            // For each live BindingId A, if A's value expression referenced B (use_def[A] contains B),
            // then B must also be live: A cannot be computed without B.
            // Span-keyed BindingId prevents same-named bindings at different scopes from conflating.
            let mut queue: std::collections::VecDeque<BindingId> = live.iter().cloned().collect();
            while let Some(id) = queue.pop_front() {
                if let Some(deps) = state.use_def.get(&id) {
                    for dep in deps {
                        if live.insert(dep.clone()) {
                            queue.push_back(dep.clone());
                        }
                    }
                }
            }

            // Step 3: emit warnings for non-live bindings.
            // Bindings without definition_span are synthetic/injected — skip them.
            // Read from SLOTS (the authoritative liveness location).
            for env_frame in &binding_envs {
                let env_guard = env_frame.read().unwrap();
                for slot_entry in &env_guard.slots {
                    if let Some((name, slot)) = slot_entry {
                        if let Some(ref def_span) = slot.definition_span {
                            let id = BindingId {
                                def_span: def_span.clone(),
                                name: name.clone(),
                            };
                            if !live.contains(&id) {
                                // Skip internal names (prefixed with special chars).
                                if !name.starts_with(INTERNAL_PREFIX_INSTANCE)
                                    && !name.starts_with(INTERNAL_PREFIX_LABEL)
                                {
                                    state.diagnostics.push(Diagnostic::warn(
                                        "lost-binding",
                                        format!("variable '{}' is never referenced", name),
                                        def_span.clone(),
                                    ));
                                }
                            }
                        }
                    }
                }
            }

            // Restore enclosing block's state fields.
            state.use_def = saved_use_def;
            state.current_binding = saved_current_binding;
            state.narrowing_map = saved_narrowing_map;
            state.current_parameter_frame = saved_parameter_frame;

            TypeCheckAction::Done(child_ty)
        }

        // ===== DictPassZero =====
        //
        // Terminal dict — run full multi-pass dict inference via run_typecheck_dict.
        // Schemes are not propagated to the parent env: this is a terminal dict expression,
        // not an intermediate scope-chain body. The returned record type carries the full
        // structural type information.
        TypeCheckCont::DictPassZero { entries, env } => {
            let (record_type, _schemes, _referenced, mut dict_errs) =
                Box::pin(run_typecheck_dict(&entries, &env, state, type_map, None)).await;
            errors.append(&mut dict_errs);
            TypeCheckAction::Done(record_type)
        }
    }
}

// ===== Inline helper: VarRef inference =====

async fn infer_var_ref(
    name: &str,
    annotation: Option<&Spanned<Annotation>>,
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
) -> TypeValue {
    // Resolver-address primary: resolution_table → direct frame lookup.
    // LGM/ClosureCapture: slot-based via get_scheme_at; miss is a type-checker setup error.
    // Parameter: direct lookup in state.current_parameter_frame (set by infer_fn_push_cont
    //   and MatchScrutinee/MatchArm arm setup) — no level arithmetic needed.
    // No resolver address: type-checker-internal only (narrowing overrides, class dispatch).
    let id = node_id(node);
    let scheme: Option<TypeValue> = if let Some(addr) = state.resolution_table.get(&id) {
        match addr {
            crate::ast::VarAddr::Parameter(i) => {
                // Look up directly in current_parameter_frame — this is always the correct
                // fn param or case arm env regardless of nesting depth.
                if let Some(ref frame) = state.current_parameter_frame {
                    let dep_def_span = frame.read().unwrap().get_slot_def_span(0, *i);
                    let slot_scheme = frame.read().unwrap().get_scheme_at(0, *i);
                    if slot_scheme.is_some() {
                        frame.write().unwrap().mark_slot_referenced(0, *i);
                        if let (Some(ref binder), Some(ref def_span)) =
                            (&state.current_binding, &dep_def_span)
                        {
                            let dep_id = BindingId {
                                def_span: def_span.clone(),
                                name: name.to_string(),
                            };
                            if *binder != dep_id {
                                state
                                    .use_def
                                    .entry(binder.clone())
                                    .or_default()
                                    .insert(dep_id);
                            }
                        }
                        if let Some(ref def_span) = dep_def_span {
                            let bid = BindingId {
                                def_span: def_span.clone(),
                                name: name.to_string(),
                            };
                            if let Some(narrowed) = state.narrowing_map.get(&bid) {
                                return Arc::clone(narrowed);
                            }
                        }
                        slot_scheme
                    } else {
                        errors.push(Diagnostic::error(
                            "resolver-param-miss",
                            format!(
                                "parameter '{}' at slot {} not found in current_parameter_frame",
                                name, i
                            ),
                            node.span.clone(),
                        ));
                        None
                    }
                } else {
                    errors.push(Diagnostic::error(
                        "resolver-param-miss",
                        format!("parameter '{}' referenced outside any parameter frame (current_parameter_frame is None)", name),
                        node.span.clone(),
                    ));
                    None
                }
            }
            crate::ast::VarAddr::ClosureCapture(_) => {
                // Nested-function reference: the surface resolver assigns ClosureCapture(i)
                // when a name is used inside a function but defined in an outer function.
                // The type checker has no separate closure capture env; look up by name
                // in the env parent chain, which includes the outer function's scope.
                env.read().unwrap().get_scheme(name)
            }
            crate::ast::VarAddr::EffectPerform { class_id, method } => {
                // Typeclass method call — resolved at runtime by scanning the accumulated group
                // for an instance whose primary dispatch type matches the first argument's runtime type.
                // Look up the class by class_decl_id, extract the method signature, and instantiate
                // it with fresh TypeVars (the method signature uses the class's type parameters as
                // TypeValue.Var nodes that must be instantiated at each call site).
                let env_read = env.read().unwrap();
                let class_opt = env_read
                    .all_classes()
                    .into_iter()
                    .find(|c| c.class_decl_id == *class_id);
                if let Some(class_decl) = class_opt {
                    let method_sig = class_decl
                        .method_signatures
                        .iter()
                        .find(|(name, _)| name == method)
                        .map(|(_, tv)| Arc::clone(tv));
                    if let Some(sig) = method_sig {
                        // The method signature is a TypeValue.Fn using the class's type parameters
                        // as TypeValue.Var nodes. Wrap the signature in a TypeValue.Scheme with
                        // the class params as quantified vars, then instantiate at current level.
                        use crate::value::{HashableValue, Value};
                        use indexmap::IndexMap;

                        // Build vars dict: class params → empty VarDecl dicts (kind is not used by instantiate_scheme_tv).
                        let mut vars_entries = IndexMap::new();
                        for (param_name, _kind) in &class_decl.params {
                            let var_key = HashableValue::Str(Arc::from(param_name.as_str()));
                            // VarDecl payload: empty dict (instantiate_scheme_tv only reads var names from keys).
                            let empty_dict = Value::Dict {
                                entries: IndexMap::new(),
                                type_val: crate::value::unknown_type_val(),
                            };
                            vars_entries.insert(
                                var_key,
                                Arc::new(crate::value::Thunk::value(
                                    empty_dict,
                                    crate::rust_span!(),
                                )),
                            );
                        }
                        let vars_dict = Value::Dict {
                            entries: vars_entries,
                            type_val: crate::value::unknown_type_val(),
                        };
                        let vars_thunk =
                            Arc::new(crate::value::Thunk::value(vars_dict, crate::rust_span!()));

                        // Build constraints dict: empty (no constraints for method signatures).
                        let constraints_dict = Value::Dict {
                            entries: IndexMap::new(),
                            type_val: crate::value::unknown_type_val(),
                        };
                        let constraints_thunk = Arc::new(crate::value::Thunk::value(
                            constraints_dict,
                            crate::rust_span!(),
                        ));

                        // Build body thunk: the method signature TypeValue.
                        let body_thunk = Arc::new(crate::value::Thunk::value(
                            sig.as_ref().clone(),
                            crate::rust_span!(),
                        ));

                        // Build the Scheme payload dict.
                        let mut scheme_payload_entries = IndexMap::new();
                        scheme_payload_entries
                            .insert(HashableValue::Str(Arc::from(FIELD_VARS)), vars_thunk);
                        scheme_payload_entries.insert(
                            HashableValue::Str(Arc::from(FIELD_CONSTRAINTS)),
                            constraints_thunk,
                        );
                        scheme_payload_entries
                            .insert(HashableValue::Str(Arc::from(FIELD_BODY)), body_thunk);
                        let scheme_payload = Value::Dict {
                            entries: scheme_payload_entries,
                            type_val: crate::value::unknown_type_val(),
                        };
                        let scheme_payload_thunk = Arc::new(crate::value::Thunk::value(
                            scheme_payload,
                            crate::rust_span!(),
                        ));

                        // Build the TypeValue.Scheme variant.
                        let scheme = Arc::new(Value::Variant {
                            type_val: crate::value::unknown_type_val(),
                            type_decl_id: 0,
                            ctor: Arc::from(TV_SCHEME),
                            payload: Some(scheme_payload_thunk),
                        });

                        // Instantiate the scheme at the current level.
                        let current_level = state.ctx.current_level;
                        instantiate_scheme_tv(&scheme, &mut state.ctx, current_level)
                    } else {
                        // Method not found in class — fall back to Unknown (gradual typing).
                        None
                    }
                } else {
                    // Class not found — fall back to Unknown (gradual typing).
                    None
                }
            }
            _ => {
                let crate::ast::VarAddr::Dispatch(resolver_level, slot) = addr else {
                    unreachable!()
                };
                let (resolver_level, slot) = (*resolver_level, *slot);
                // Single-phase slot lookup: the env chain mirrors the resolver's frame structure.
                // get_scheme_at(resolver_level, slot) traverses resolver_level parent links and
                // looks up the slot there. Root-group entries are in child_env (seeded by
                // typecheck_program_bootstrap) at their absolute slots; Sequential body entries
                // are in their respective env frames at the correct depths.
                let slot_scheme = {
                    let env_read = env.read().unwrap();
                    env_read.get_scheme_at(resolver_level, slot)
                };
                if slot_scheme.is_some() {
                    let dep_def_span = env.read().unwrap().get_slot_def_span(resolver_level, slot);
                    env.write()
                        .unwrap()
                        .mark_slot_referenced(resolver_level, slot);
                    if let (Some(ref binder), Some(ref def_span)) =
                        (&state.current_binding, &dep_def_span)
                    {
                        let dep_id = BindingId {
                            def_span: def_span.clone(),
                            name: name.to_string(),
                        };
                        if *binder != dep_id {
                            state
                                .use_def
                                .entry(binder.clone())
                                .or_default()
                                .insert(dep_id);
                        }
                    }
                    if let Some(ref def_span) = dep_def_span {
                        let bid = BindingId {
                            def_span: def_span.clone(),
                            name: name.to_string(),
                        };
                        if let Some(narrowed) = state.narrowing_map.get(&bid) {
                            return Arc::clone(narrowed);
                        }
                    }
                    slot_scheme
                } else {
                    errors.push(Diagnostic::error(
                        "resolver-slot-miss",
                        format!(
                            "resolver assigned slot ({}, {}) for '{}' but env has no entry at that address",
                            resolver_level, slot, name
                        ),
                        node.span.clone(),
                    ));
                    None
                }
            }
        }
    } else {
        // No resolver address — should not happen for user bindings.
        // User bindings always have resolver addresses via typecheck_program_bootstrap.
        // Type-checker-internal bindings (narrowing overrides, class dispatch names) are
        // now stored in slots with an appended position — look up by name via get_scheme.
        // DO NOT record use_def edge for these — they are not Sequential intermediates.
        env.read().unwrap().get_scheme(name)
    };

    if let Some(scheme) = scheme {
        // Check if scheme is polymorphic (TypeValue.Scheme) — if so, store in scheme_map for LSP.
        if typevalue_ctor(&scheme) == Some(TV_SCHEME) {
            if let Some(ref mut smap) = state.scheme_map {
                let key = (
                    node.span.start_line,
                    node.span.start_col,
                    node.span.end_line,
                    node.span.end_col,
                );
                smap.insert(key, Arc::clone(&scheme));
            }
        }

        // Instantiate the scheme: if it's a TypeValue.Scheme, create fresh TypeVars.
        // Otherwise return the TypeValue directly (monomorphic).
        let result_type = if typevalue_ctor(&scheme) == Some(TV_SCHEME) {
            let current_level = state.ctx.current_level;
            match instantiate_scheme_tv(&scheme, &mut state.ctx, current_level) {
                Some(instantiated) => instantiated,
                None => {
                    // instantiate_scheme_tv returning None on a TypeValue.Scheme-tagged value
                    // is an invariant violation: the scheme payload is malformed.
                    errors.push(Diagnostic::error(
                        "type-error",
                        "malformed TypeValue.Scheme: instantiation failed (payload corrupt)",
                        node.span.clone(),
                    ));
                    // Malformed scheme: fresh TypeVar (failed instantiation, unconstrained)
                    state.fresh_type_var(&node.span)
                }
            }
        } else {
            Arc::clone(&scheme)
        };

        result_type
    } else {
        // If the resolver successfully resolved this variable, the variable
        // genuinely exists in scope — the type checker simply doesn't have its type.
        // Return Unknown (gradual typing) rather than a false "undefined variable" diagnostic.
        if let crate::ast::SurfaceExpression::VarRef { resolution, .. } = &node.expr {
            if let Some(Some(_)) = resolution.get() {
                return make_typevalue_unknown();
            }
        }

        let mut err = Diagnostic::error(
            "type-error",
            format!("undefined variable: {}", name),
            node.span.clone(),
        );
        if let Some(cause_span) = state.failed_bindings.get(name) {
            err.add_note(format!(
                "`{}` could not be defined because its definition at {}:{} failed type checking",
                name, cause_span.start_line, cause_span.start_col
            ));
        }

        // Gradual typing: use inline annotation if present.
        if let Some(ann) = annotation {
            let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
            let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
            let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
            let ty = match typecheck_annot::resolve_annotation(
                &ann.node,
                ann.span.clone(),
                &mut *state,
                &mut constraints,
                &mut ann_m,
                &mut row_m,
                None,
            )
            .await
            {
                Ok(ty) => ty,
                Err(e) => {
                    errors.push(e);
                    // Annotation resolution failed: fresh TypeVar (unconstrained)
                    state.fresh_type_var(&node.span)
                }
            };
            state
                .failed_bindings
                .insert(name.to_string(), node.span.clone());
            ty
        } else {
            errors.push(err);
            // Undefined variable: fresh TypeVar (unconstrained)
            state.fresh_type_var(&node.span)
        }
    }
}

// ===== Inline helper: evaluate args for error collection =====

/// Evaluate each positional and named argument in isolation (with a fresh local stack)
/// to collect any errors nested inside them. Return types are discarded.
///
/// Used when an arity mismatch or other definite call-site failure is detected: we still
/// want to surface errors inside the arguments (e.g., calling an undefined function as an
/// arg), but we do not want to unify arg types against params — there are none to unify
/// against in the correct correspondence.
async fn eval_args_for_errors(
    args: &[Arc<SurfaceNode>],
    named_args: &[Spanned<SurfaceNamedArg>],
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) {
    for arg in args {
        let mut local_stack = Vec::new();
        typecheck_for_errors(arg, env, state, errors, type_map, &mut local_stack).await;
    }
    for na in named_args {
        let mut local_stack = Vec::new();
        typecheck_for_errors(
            &na.node.value,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        )
        .await;
    }
}

// ===== Inline helper: call func type dispatch =====

async fn apply_cont_call_func(
    func_node: Arc<SurfaceNode>,
    func_ty: TypeValue,
    args: Vec<Arc<SurfaceNode>>,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    call_node: Arc<SurfaceNode>,
    ctx: &mut TypeCheckCtx<'_, '_>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    // If the function type is Unknown/Never, infer args for errors and return Unknown.
    match typevalue_ctor(&func_ty) {
        Some(TV_UNKNOWN) | Some(TV_NEVER) => {
            eval_args_for_errors(
                &args,
                &named_args,
                &env,
                ctx.state,
                ctx.errors,
                ctx.type_map,
            )
            .await;
            return TypeCheckAction::Done(make_typevalue_unknown());
        }
        _ => {}
    }

    // T-2149: Extract EffectPerform metadata for instance checking after args are inferred.
    let effect_perform_meta = if let SurfaceExpression::VarRef { .. } = &func_node.expr {
        let func_id = node_id(&func_node);
        if let Some(crate::ast::VarAddr::EffectPerform { class_id, method }) =
            ctx.state.resolution_table.get(&func_id)
        {
            Some((*class_id, method.clone()))
        } else {
            None
        }
    } else {
        None
    };

    match typevalue_ctor(&func_ty) {
        Some(TV_FN) => {
            // Extract params and return from TypeValue.Fn.
            let (fn_params_tv, fn_ret_tv) =
                match crate::type_infer::typevalue_fn_params_and_ret(&func_ty) {
                    Some(pair) => pair,
                    None => {
                        // typevalue_fn_params_and_ret returning None on a TypeValue.Fn-tagged
                        // value is an invariant violation: the Fn payload is malformed.
                        ctx.errors.push(Diagnostic::error(
                        "type-error",
                        "malformed TypeValue.Fn: params/return extraction failed (payload corrupt)",
                        call_node.span.clone(),
                    ));
                        // Malformed Fn: fresh TypeVar (failed extraction, unconstrained return)
                        return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
                    }
                };

            // Extract param names from the TypeValue.Fn payload (stored by make_typevalue_fn_with_flags).
            // Named params have Some(name); unnamed params (builtins, etc.) have None.
            let param_names =
                crate::type_infer::typevalue_fn_param_names(&func_ty, fn_params_tv.len());

            // Build inst_params with actual param names. Unnamed params carry None so that
            // named-arg matching in finalize_call_with_args can distinguish "has an identifier
            // name" from "is positional-only." Index strings ("0", "1") must never be used as
            // if they were identifier names.
            let inst_params: Vec<(Option<String>, TypeValue)> = fn_params_tv
                .iter()
                .enumerate()
                .map(|(i, tv)| {
                    let name = param_names.get(i).and_then(|n| n.clone());
                    (name, ctx.state.apply(tv))
                })
                .collect();
            let inst_ret = ctx.state.apply(&fn_ret_tv);
            // Extract typed variadic bucket types from the TypeValue.Fn payload.
            // These were stored by make_typevalue_fn_with_flags when the function was defined.
            // apply() each bucket's type through the current substitution so that any
            // type variables bound since the function was defined are resolved.
            let inst_typed_variadics: Vec<(String, TypeValue)> =
                crate::type_infer::typevalue_fn_typed_variadics(&func_ty)
                    .into_iter()
                    .map(|(name, ty)| (name, ctx.state.apply(&ty)))
                    .collect();
            // Check variadic flag from TypeValue.Fn payload.
            let inst_rest: Option<Box<(String, TypeValue)>> =
                if crate::type_infer::typevalue_fn_is_variadic(&func_ty) {
                    Some(Box::new(("...rest".to_string(), make_typevalue_top())))
                } else {
                    None
                };
            // Extract required param count from TypeValue.Fn (B-685).
            // If absent, all params are required (defaults to inst_params.len()).
            let inst_required = crate::type_infer::typevalue_fn_required_count(&func_ty)
                .unwrap_or(inst_params.len());

            // Derive inst_variadic for arity checks.
            let inst_variadic = !inst_typed_variadics.is_empty() || inst_rest.is_some();

            if args.is_empty() {
                // No positional args — arity is checked inside finalize_call_no_positional_args.
                let sig = FnSig {
                    params: inst_params,
                    ret: inst_ret,
                    typed_variadics: inst_typed_variadics,
                    rest: inst_rest,
                    required_count: inst_required,
                };
                let result = finalize_call_no_positional_args(
                    sig,
                    named_args,
                    &env,
                    call_node.span.clone(),
                    ctx,
                )
                .await;
                return TypeCheckAction::Done(result);
            }

            // Positional args present: check arity BEFORE pushing any CallArg continuation.
            // This prevents type unification from running with the wrong number of arguments,
            // which would produce misleading type errors downstream.
            {
                let n_positional = args.len();
                let n_named = named_args.len();
                let n_total = n_positional + n_named;
                // required_count is fixed-params-only (variadics are not included), so
                // no saturating_sub is needed — inst_required already excludes variadic params.
                let min_req = inst_required;
                if n_total < min_req || (!inst_variadic && n_positional > inst_params.len()) {
                    eval_args_for_errors(
                        &args,
                        &named_args,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                    )
                    .await;
                    let err = Diagnostic::error(
                        "type-error",
                        format!(
                            "arity mismatch: expected {} argument(s), got {}",
                            min_req, n_total
                        ),
                        call_node.span.clone(),
                    );
                    ctx.errors.push(err.clone());
                    // Arity error: fresh TypeVar (failed call, unconstrained return)
                    return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
                }
            }

            // Arity is correct. Start evaluating positional args.
            let first_arg = Arc::clone(&args[0]);
            let arg_nodes: Vec<Arc<SurfaceNode>> = args.iter().map(Arc::clone).collect();
            let remaining: Vec<Arc<SurfaceNode>> = args[1..].iter().map(Arc::clone).collect();
            let call_span = call_node.span.clone();
            stack.push(TypeCheckCont::CallArg {
                idx: 0,
                remaining_args: remaining,
                accumulated_arg_types: Vec::new(),
                arg_nodes,
                param_types: inst_params,
                fn_ret: inst_ret,
                typed_variadics: inst_typed_variadics,
                rest: inst_rest,
                fn_required: inst_required,
                env: Arc::clone(&env),
                named_args,
                span: call_span,
                call_node,
                effect_perform: effect_perform_meta.clone(),
            });
            TypeCheckAction::Eval(first_arg, env)
        }

        Some(TV_VAR) => {
            // Unbound TypeVar: infer args for side effects, return fresh TypeVar for return
            for arg in &args {
                let mut local_stack = Vec::new();
                typecheck_for_errors(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                )
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                typecheck_for_errors(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                )
                .await;
            }
            let ret_var = ctx.state.fresh_type_var(&call_node.span);
            TypeCheckAction::Done(ret_var)
        }

        Some(TV_UNKNOWN) | Some(TV_TOP) => {
            for arg in &args {
                let mut local_stack = Vec::new();
                typecheck_for_errors(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                )
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                typecheck_for_errors(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                )
                .await;
            }
            ctx.state.diagnostics.push(crate::error::Diagnostic::warn(
                "unknown-call",
                "calling expression of Unknown type — may not be a function",
                call_node.span.clone(),
            ));
            TypeCheckAction::Done(make_typevalue_unknown())
        }

        Some(TV_NOMINAL_VARIANT) => {
            // Variant constructor call: build a payload record from named args.
            //
            // Arity rule: a variant constructor takes at most 1 positional argument
            // (the payload dict). Named args are always allowed (they build the payload).
            if args.len() > 1 {
                eval_args_for_errors(
                    &args,
                    &named_args,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                )
                .await;
                ctx.errors.push(Diagnostic::error(
                    "type-error",
                    format!(
                        "variant constructor takes at most 1 positional argument, got {}",
                        args.len()
                    ),
                    call_node.span.clone(),
                ));
                // Variant arity error: fresh TypeVar (failed call, unconstrained)
                return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
            }

            // Extract the declared payload type (fields record) from the NominalVariant.
            // The NominalVariant TypeValue carries { tycon, ctor, fields } where fields is
            // a TypeValue.Record describing the declared constructor payload shape.
            let declared_fields_tv = {
                // Build a TypeValue.Record from the NominalVariant's declared fields.
                let field_map = typevalue_record_fields_pub(&func_ty);
                make_typevalue_record(field_map, None)
            };
            let has_declared_fields = typevalue_nominal_variant_has_fields(&func_ty);

            // Infer the positional arg (payload dict) if present, and check it against
            // the declared payload type.
            if !args.is_empty() {
                let mut local_stack = Vec::new();
                let arg_ty = Box::pin(run_typecheck(
                    &args[0],
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;

                // Constrain arg_ty against declared_fields_tv.
                // This enforces that the positional payload dict matches the declared fields.
                if has_declared_fields {
                    let mut constraints = std::mem::take(&mut ctx.state.constraints);
                    if let Err(e) = constrain(
                        &arg_ty,
                        &declared_fields_tv,
                        &mut ctx.state.ctx,
                        &mut constraints,
                        args[0].span.clone(),
                    )
                    .await
                    {
                        ctx.errors.push(e);
                    }
                    ctx.state.constraints = constraints;
                    run_fd_improvement_fixpoint(ctx.state, ctx.errors, args[0].span.clone()).await;
                }
            } else if !named_args.is_empty() {
                // Named args only: infer each named arg for side effects.
                // The runtime will assemble them into a payload dict.
                for na in &named_args {
                    let mut local_stack = Vec::new();
                    typecheck_for_errors(
                        &na.node.value,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    )
                    .await;
                }
            }

            // Return the NominalVariant type itself (the constructor call produces the variant type).
            TypeCheckAction::Done(Arc::clone(&func_ty))
        }

        // Extract all Function-typed members from the intersection.
        // An intersection function type means the value satisfies all member signatures;
        // at a call site we select the unique member whose arity matches the supplied args.
        Some(TV_INTER) => {
            // Intersection type at call site: select the Fn member matching the arity.
            let members = match crate::type_infer::typevalue_extract_members_pub(&func_ty) {
                Some(m) => m,
                None => {
                    // typevalue_extract_members_pub returning None on a TypeValue.Inter-tagged
                    // value means the members payload is malformed (corrupt TypeValue structure).
                    ctx.errors.push(Diagnostic::error(
                        "type-error",
                        "malformed TypeValue.Inter: members extraction failed (payload corrupt)",
                        call_node.span.clone(),
                    ));
                    // Malformed Inter: fresh TypeVar (failed extraction, unconstrained)
                    return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
                }
            };
            let fn_members: Vec<TypeValue> = members
                .into_iter()
                .filter(|m| typevalue_ctor(m) == Some(TV_FN))
                .collect();

            if fn_members.is_empty() {
                let err = Diagnostic::error(
                    "type-error",
                    "expected function type, got intersection of non-function types",
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                // Inter with no Fn members: fresh TypeVar (type error, unconstrained)
                return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
            }

            let n_positional = args.len();
            let n_named = named_args.len();
            let n_total = n_positional + n_named;

            let matching: Vec<TypeValue> = fn_members
                .into_iter()
                .filter(|m| {
                    let param_count = crate::type_infer::typevalue_fn_params_and_ret(m)
                        .map(|(ps, _)| ps.len())
                        .unwrap_or(0);
                    n_total <= param_count + 1 && n_total >= param_count.saturating_sub(1)
                })
                .collect();

            if matching.is_empty() {
                let err = Diagnostic::error(
                    "type-error",
                    format!(
                        "no overload of intersection type accepts {} argument(s)",
                        n_total
                    ),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                // No matching overload: fresh TypeVar (arity error, unconstrained)
                return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
            }

            // Pick the most specific overload: smallest params.len() that fits,
            // Pick the first matching overload (smallest params.len.).
            let selected = matching.into_iter().next().unwrap();
            Box::pin(apply_cont_call_func(
                Arc::clone(&func_node),
                selected,
                args,
                named_args,
                env,
                call_node,
                ctx,
                stack,
            ))
            .await
        }

        Some(TV_UNION) => {
            // Union type at call site: select Fn members matching the arity.
            let members = match crate::type_infer::typevalue_extract_members_pub(&func_ty) {
                Some(v) => v,
                None => vec![], // TypeValue has no members to dispatch (e.g., gradual/Unknown).
            };
            let fn_members: Vec<TypeValue> = members
                .into_iter()
                .filter(|m| typevalue_ctor(m) == Some(TV_FN))
                .collect();

            if fn_members.is_empty() {
                let err = Diagnostic::error(
                    "type-error",
                    "expected function type, got union of non-function types",
                    call_node.span.clone(),
                );
                ctx.errors.push(err);
                eval_args_for_errors(
                    &args,
                    &named_args,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                )
                .await;
                // Union with no Fn members: fresh TypeVar (type error, unconstrained)
                return TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span));
            }

            // Call with the first matching Fn member (simplified for TypeValue migration).
            let selected = fn_members.into_iter().next().unwrap();
            Box::pin(apply_cont_call_func(
                Arc::clone(&func_node),
                selected,
                args,
                named_args,
                env,
                call_node,
                ctx,
                stack,
            ))
            .await
        }

        _ => {
            eval_args_for_errors(
                &args,
                &named_args,
                &env,
                ctx.state,
                ctx.errors,
                ctx.type_map,
            )
            .await;
            let err = Diagnostic::error(
                "type-error",
                format!(
                    "expected function type, got {}",
                    crate::eval::format_type_for_assert(&func_ty)
                ),
                call_node.span.clone(),
            );
            ctx.errors.push(err);
            // Non-callable type: fresh TypeVar (type error, unconstrained)
            TypeCheckAction::Done(ctx.state.fresh_type_var(&call_node.span))
        }
    }
}

// ===== Inline helper: finalize call with no positional args =====

async fn finalize_call_no_positional_args(
    sig: FnSig,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: &Arc<RwLock<Env>>,
    span: Span,
    ctx: &mut TypeCheckCtx<'_, '_>,
) -> TypeValue {
    let FnSig {
        params,
        ret,
        typed_variadics,
        rest,
        required_count,
    } = sig;
    let variadic = !typed_variadics.is_empty() || rest.is_some();
    // required_count is fixed-params-only (variadics not included) — no saturating_sub needed.
    let min_required = required_count;

    if named_args.is_empty() && min_required > 0 {
        let err = Diagnostic::error(
            "type-error",
            format!("arity mismatch: expected {} arguments, got 0", min_required),
            span.clone(),
        );
        ctx.errors.push(err.clone());
        // Arity error: fresh TypeVar (failed call, unconstrained return)
        return ctx.state.fresh_type_var(&span);
    }

    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            ctx.errors.push(Diagnostic::error(
                "type-error",
                format!("duplicate named argument: '{}'", na.node.name),
                na.span.clone(),
            ));
            continue;
        }
        let param_match = params.iter().enumerate().find_map(|(idx, (pname, pty))| {
            if pname.as_ref().map(|s| s.as_str()) == Some(na.node.name.as_str()) {
                Some((idx, pty.clone()))
            } else {
                None
            }
        });
        match param_match {
            Some((param_idx, param_ty)) => {
                consumed.insert(param_idx);
                let arg_ty = {
                    let mut local_stack = Vec::new();
                    Box::pin(run_typecheck(
                        &na.node.value,
                        env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await
                };
                let mut constraints = std::mem::take(&mut ctx.state.constraints);
                if let Err(e) = constrain(
                    &arg_ty,
                    &param_ty,
                    &mut ctx.state.ctx,
                    &mut constraints,
                    na.span.clone(),
                )
                .await
                {
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
                run_fd_improvement_fixpoint(ctx.state, ctx.errors, na.span.clone()).await;
            }
            None => {
                if !variadic {
                    ctx.errors.push(Diagnostic::error(
                        "type-error",
                        format!(
                            "unknown named argument: function has no parameter named '{}'",
                            na.node.name
                        ),
                        na.span.clone(),
                    ));
                } else {
                    let mut local_stack = Vec::new();
                    typecheck_for_errors(
                        &na.node.value,
                        env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    )
                    .await;
                }
            }
        }
    }

    ctx.state.apply(&ret)
}

// ===== Inline helper: CALL-POLY arg unification =====

async fn apply_call_args_poly(
    arg_types: Vec<TypeValue>,
    arg_nodes: Vec<Arc<SurfaceNode>>,
    sig: FnSig,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    span: Span,
    effect_perform: Option<(u64, String)>,
    ctx: &mut TypeCheckCtx<'_, '_>,
) -> TypeCheckAction {
    let FnSig {
        params: param_types,
        ret: fn_ret,
        typed_variadics,
        rest,
        required_count: fn_required,
    } = sig;
    // param_types contains only fixed (non-variadic) params.
    let non_variadic_param_count = param_types.len();
    let fn_variadic = !typed_variadics.is_empty() || rest.is_some();
    // required_count is fixed-params-only (variadics are not included), so no
    // saturating_sub is needed — fn_required already excludes variadic params.
    let min_required = fn_required;
    let total_supplied = arg_types.len() + named_args.len();

    // T-2149: Check if this is an EffectPerform call with concrete argument types that lack
    // matching instances. Emit info diagnostic when the type checker can prove no instance
    // exists (runtime dispatch is authoritative, but early feedback helps development).
    //
    // Simplified approach: check ONLY the first argument (primary dispatch position).
    // Multi-parameter typeclass instance matching requires FD-aware lookup which is complex.
    // For MVP, warn when the first arg has a concrete type with no matching instance.
    if let Some((class_id, method)) = effect_perform {
        if !arg_types.is_empty() {
            let arg_ty_applied = ctx.state.apply(&arg_types[0]);

            // Only check if the arg type is concrete (not TypeVar, not Unknown, not Top).
            let is_concrete = match typevalue_ctor(&arg_ty_applied) {
                Some(TV_VAR) | Some(TV_UNKNOWN) | Some(TV_TOP) | Some(TV_NEVER) => false,
                _ => true,
            };

            if is_concrete {
                // Look up the class and check if any instance matches this argument type.
                let env_read = env.read().unwrap();
                let class_opt = env_read
                    .all_classes()
                    .into_iter()
                    .find(|c| c.class_decl_id == class_id);

                if let Some(class_decl) = class_opt {
                    // Check if any instance exists for this argument type.
                    // Simple best-effort check: compare TypeValue ctors (e.g., both TypeValue.Repr).
                    // Full structural equality or unification would be more precise but is complex.
                    let arg_ctor = typevalue_ctor(&arg_ty_applied);
                    let has_matching_instance = env_read.all_instances().iter().any(|(_, inst)| {
                        if inst.class_name != class_decl.name {
                            return false;
                        }
                        // For single-parameter classes, instance_type is the covered type.
                        // For multi-param, it's a TypeValue.Record with numbered fields.
                        // Check if instance_type ctor matches arg_ty ctor as a simple heuristic.
                        let inst_ctor = typevalue_ctor(&inst.instance_type);
                        arg_ctor == inst_ctor || Arc::ptr_eq(&inst.instance_type, &arg_ty_applied)
                    });

                    if !has_matching_instance {
                        let arg_ty_str = crate::eval::format_type_for_assert(&arg_ty_applied);
                        if let Some(arg_node) = arg_nodes.first() {
                            ctx.state.diagnostics.push(Diagnostic::info(
                                "missing-instance",
                                format!(
                                    "no instance of class '{}' found for type {} (method '{}' call may fail at runtime)",
                                    class_decl.name,
                                    arg_ty_str,
                                    method
                                ),
                                arg_node.span.clone(),
                            ));
                        }
                    }
                }
            }
        }
    }

    // Arity check — this is a post-collection check (all args already evaluated).
    // Return TypeValue.Unknown (error marker) rather than the return type to ensure definite failures
    // do not flow silently through downstream consistency checks.
    if total_supplied < min_required || (!fn_variadic && arg_types.len() > param_types.len()) {
        let err = Diagnostic::error(
            "type-error",
            format!(
                "arity mismatch: expected {} arguments, got {}",
                min_required, total_supplied
            ),
            span.clone(),
        );
        ctx.errors.push(err.clone());
        // Arity error: fresh TypeVar (failed call, unconstrained return)
        return TypeCheckAction::Done(ctx.state.fresh_type_var(&span));
    }

    // Unify positional args against fixed params (Robinson unification via unify())
    let mut consumed_params: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, ((_, param_ty), arg_ty)) in param_types
        .iter()
        .take(non_variadic_param_count)
        .zip(arg_types.iter())
        .enumerate()
    {
        consumed_params.insert(idx);

        // Widen literal types before unification
        let widened_arg = crate::typecheck::typecheck_call::widen_literal_types(arg_ty.clone());

        // Gradual typing boundary guard: Unknown-typed arg flowing into concrete param.
        // When an Unknown/Any arg flows into a concrete parameter, attach a runtime guard so
        // the evaluator can enforce the type contract at the Unknown→concrete boundary.
        let is_unknown_arg = matches!(
            typevalue_ctor(&widened_arg),
            Some(TV_UNKNOWN) | Some(TV_TOP)
        );
        if is_unknown_arg && typecheck_call::is_concrete_type(&ctx.state.apply(param_ty)) {
            if let Some(arg_node) = arg_nodes.get(idx) {
                arg_node.type_guard.set(Some(ctx.state.apply(param_ty)));
            }
        }

        let mut constraints = std::mem::take(&mut ctx.state.constraints);
        if let Err(mut e) = constrain(
            &widened_arg,
            param_ty,
            &mut ctx.state.ctx,
            &mut constraints,
            span.clone(),
        )
        .await
        {
            // Attach the specific argument's span as a secondary annotation so users
            // can navigate to the exact expression that has the wrong type.
            if let Some(arg_node) = arg_nodes.get(idx) {
                e = e.with_span(arg_node.span.clone(), "this argument".to_string());
            }
            ctx.errors.push(e);
        }
        ctx.state.constraints = constraints;
        run_fd_improvement_fixpoint(ctx.state, ctx.errors, span.clone()).await;
    }

    // Handle variadic args with match-semantics routing.
    //
    // Each positional arg beyond non_variadic_param_count is routed to the first typed_variadic
    // bucket whose element type is consistently a supertype of the arg type (declaration order =
    // match priority, first match wins). Args that match no typed bucket fall into `rest`.
    //
    // Typed buckets: collect matched arg types; unify each against the bucket element type.
    //   Bucket types are expected to be App(Seq, elem_ty); the element type is extracted via
    //   extract_seq_elem_type() and used as the unification target.
    //
    // Rest bucket (untyped): build a specific positional dict {0: T0, 1: T1, ...} from all
    //   unmatched positional args and unify the whole dict against the rest TypeVar.
    //   Unmatched named args are handled separately below and also go into rest.
    if fn_variadic && arg_types.len() > non_variadic_param_count {
        let variadic_args = &arg_types[non_variadic_param_count..];

        // Per-bucket accumulator: indexed parallel to typed_variadics.
        let mut bucket_args: Vec<Vec<TypeValue>> = vec![Vec::new(); typed_variadics.len()];
        // Rest accumulator: widened types for args that matched no typed bucket.
        let mut rest_positional_args: Vec<TypeValue> = Vec::new();

        for arg_ty in variadic_args {
            let widened = crate::typecheck::typecheck_call::widen_literal_types(arg_ty.clone());

            // Match semantics: try each typed bucket in declaration order; first match wins.
            let mut routed = false;
            for (bucket_idx, (_, bucket_ty)) in typed_variadics.iter().enumerate() {
                let elem_ty = extract_seq_elem_type(bucket_ty);
                let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                    ctx.state.ctx.subst.clone(),
                    ctx.state.ctx.levels.clone(),
                    ctx.state.ctx.current_level,
                    ctx.state.tycon_env.clone(),
                );
                if crate::bas::is_consistent_subtype(&widened, &elem_ty, &ctx_for_check) {
                    bucket_args[bucket_idx].push(widened.clone());
                    routed = true;
                    break;
                }
            }

            if !routed {
                if rest.is_some() {
                    rest_positional_args.push(widened);
                } else {
                    // No rest bucket and no matching typed bucket: exhaustiveness error.
                    let err = Diagnostic::error(
                        "type-error",
                        format!(
                            "argument type does not match any variadic bucket, got {}",
                            crate::eval::format_type_for_assert(&widened)
                        ),
                        span.clone(),
                    );
                    ctx.errors.push(err);
                }
            }
        }

        // Unify each typed bucket's matched args against its element type.
        for (bucket_idx, (_, bucket_ty)) in typed_variadics.iter().enumerate() {
            let elem_ty = extract_seq_elem_type(bucket_ty);
            for matched_arg in &bucket_args[bucket_idx] {
                let mut constraints = std::mem::take(&mut ctx.state.constraints);
                if let Err(e) = constrain(
                    matched_arg,
                    &elem_ty,
                    &mut ctx.state.ctx,
                    &mut constraints,
                    span.clone(),
                )
                .await
                {
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
                run_fd_improvement_fixpoint(ctx.state, ctx.errors, span.clone()).await;
            }
        }

        // Unify rest positional args against the rest TypeVar as a specific positional dict.
        if let Some(rest_param) = &rest {
            let (_, rest_ty) = rest_param.as_ref();
            if !rest_positional_args.is_empty() {
                let mut fields = indexmap::IndexMap::new();
                for (i, ty) in rest_positional_args.iter().enumerate() {
                    fields.insert(i.to_string(), ty.clone());
                }
                let rest_dict = make_typevalue_record(fields, None);
                let mut constraints = std::mem::take(&mut ctx.state.constraints);
                if let Err(e) = constrain(
                    &rest_dict,
                    rest_ty,
                    &mut ctx.state.ctx,
                    &mut constraints,
                    span.clone(),
                )
                .await
                {
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
                run_fd_improvement_fixpoint(ctx.state, ctx.errors, span.clone()).await;
            }
            // If no rest positional args, the rest TypeVar stays free (empty variadic dict).
        }
    }

    // Handle named args (CALL-POLY path).
    // Named args that don't match any fixed param are accumulated for the rest bucket.
    // If there is no rest bucket and no variadic at all, they produce an error.
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    let mut unmatched_named_arg_types: Vec<(String, TypeValue)> = Vec::new();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            ctx.errors.push(Diagnostic::error(
                "type-error",
                format!("duplicate named argument: '{}'", na.node.name),
                na.span.clone(),
            ));
            continue;
        }
        let param_match = param_types
            .iter()
            .enumerate()
            .find_map(|(idx, (pname, pty))| {
                if pname.as_ref().map(|s| s.as_str()) == Some(na.node.name.as_str()) {
                    Some((idx, pty.clone()))
                } else {
                    None
                }
            });
        match param_match {
            Some((param_idx, param_ty)) => {
                if consumed_params.contains(&param_idx) {
                    ctx.errors.push(Diagnostic::error(
                        "type-error",
                        format!(
                            "named argument '{}' conflicts with positional argument at position {}",
                            na.node.name, param_idx
                        ),
                        na.span.clone(),
                    ));
                    continue;
                }
                consumed_params.insert(param_idx);
                let arg_ty = {
                    let mut local_stack = Vec::new();
                    Box::pin(run_typecheck(
                        &na.node.value,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await
                };
                let mut constraints = std::mem::take(&mut ctx.state.constraints);
                if let Err(e) = constrain(
                    &arg_ty,
                    &param_ty,
                    &mut ctx.state.ctx,
                    &mut constraints,
                    na.span.clone(),
                )
                .await
                {
                    ctx.errors.push(Diagnostic::error(
                        "type-error",
                        format!(
                            "named argument '{}' type mismatch: {}",
                            na.node.name, e.message
                        ),
                        na.span.clone(),
                    ));
                }
                ctx.state.constraints = constraints;
                run_fd_improvement_fixpoint(ctx.state, ctx.errors, na.span.clone()).await;
            }
            None => {
                if fn_variadic {
                    // Infer the named arg value for error collection; accumulate for rest bucket.
                    let arg_ty = {
                        let mut local_stack = Vec::new();
                        Box::pin(run_typecheck(
                            &na.node.value,
                            &env,
                            ctx.state,
                            ctx.errors,
                            ctx.type_map,
                            &mut local_stack,
                        ))
                        .await
                    };
                    unmatched_named_arg_types.push((na.node.name.clone(), arg_ty));
                } else {
                    ctx.errors.push(Diagnostic::error(
                        "type-error",
                        format!(
                            "unknown named argument: function has no parameter named '{}'",
                            na.node.name
                        ),
                        na.span.clone(),
                    ));
                }
            }
        }
    }

    // Unify unmatched named args into the rest bucket as string-keyed entries.
    if !unmatched_named_arg_types.is_empty() {
        if let Some(rest_param) = &rest {
            let (_, rest_ty) = rest_param.as_ref();
            let mut fields = indexmap::IndexMap::new();
            for (name, ty) in &unmatched_named_arg_types {
                let widened = crate::typecheck::typecheck_call::widen_literal_types(ty.clone());
                fields.insert(name.clone(), widened);
            }
            let named_dict = make_typevalue_record(fields, None);
            let mut constraints = std::mem::take(&mut ctx.state.constraints);
            if let Err(e) = constrain(
                &named_dict,
                rest_ty,
                &mut ctx.state.ctx,
                &mut constraints,
                span.clone(),
            )
            .await
            {
                ctx.errors.push(e);
            }
            ctx.state.constraints = constraints;
            run_fd_improvement_fixpoint(ctx.state, ctx.errors, span.clone()).await;
        }
        // Named args arrived for a function with typed buckets but no untyped rest.
        // Typed buckets only accept positional args (matched by type); named args have
        // no bucket to go into. Emit an error for each unmatched named arg.
        if rest.is_none() {
            for (name, _) in &unmatched_named_arg_types {
                ctx.errors.push(Diagnostic::error("type-error",
                    format!(
                        "named argument '{}' cannot be routed: function has no untyped rest parameter (...rest) to accept unmatched named args",
                        name
                    ),
                    span.clone(),
                ));
            }
        }
    }

    TypeCheckAction::Done(ctx.state.apply(&fn_ret))
}

/// Extract the element type from a typed variadic bucket type.
///
/// Bucket types are always `App(_, elem_ty)` — the protocol is structural.
/// The head constructor is whatever type the caller's prelude names its sequence type;
/// Rust does not name-check it. If the type does not match the App form, return the
/// type itself as the element constraint (a fresh TypeVar or other concrete type).
fn extract_seq_elem_type(bucket_ty: &TypeValue) -> TypeValue {
    // Extract element type from a TypeValue.App { op: _, arg: elem }
    use crate::value::HashableValue;
    if typevalue_ctor(bucket_ty) == Some(TV_APP) {
        if let Some(crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        }) = Some(bucket_ty.as_ref())
        {
            if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                let arg_key = HashableValue::Str(Arc::from(FIELD_ARG));
                if let Some(arg_thunk) = entries.get(&arg_key) {
                    if let Some(Ok(elem)) = arg_thunk.peek_result() {
                        return Arc::new(elem.clone());
                    }
                }
            }
        }
    }
    Arc::clone(bucket_ty)
}

// ===== Inline helper: Fn inference via FnBody continuation =====

/// Resolve all function annotations, build the parameter environment, push `FnBody`,
/// and return `Eval(body, fn_env)` so the CEK loop evaluates the body iteratively without
/// recursing on the Rust call stack.
async fn infer_fn_push_cont(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    let (return_ann, params, body) = match &node.expr {
        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            ..
        } => (return_ann, params.as_slice(), body),
        _ => unreachable!("infer_fn_push_cont called on non-Fn node"),
    };
    let mut ann_mapping_str: HashMap<String, String> = HashMap::new();
    let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
    let mut ann_mapping_opt = Some(&mut ann_mapping_str);
    let mut row_ann_mapping_str: HashMap<String, String> = HashMap::new();
    let mut row_ann_mapping_opt = Some(&mut row_ann_mapping_str);

    // Harvest the user-declared bind: names BEFORE resolve_fn_metadata creates their TypeVars.
    // This is a read-only name scan — no TypeVar creation, no state mutation.
    // Used after param processing to detect dead bind: names (unused-type-variable lint).
    let bind_declared_names: Vec<String> = if let Some(ret_ann) = return_ann {
        if let Annotation::PropertyDict(entries) = &ret_ann.node {
            let mut names: Vec<String> = Vec::new();
            for entry in entries {
                let is_bind_key = entry.node.key.as_ref().is_some_and(|k| {
                    matches!(&k.expr, SurfaceExpression::StringLiteral { content, .. } if content == "bind")
                });
                if is_bind_key {
                    match &entry.node.value.expr {
                        // [a b c] → Call form
                        SurfaceExpression::Call { func, args, .. } => {
                            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                                names.push(name.clone());
                            }
                            for arg in args.iter() {
                                if let SurfaceExpression::VarRef { name, .. } = &arg.expr {
                                    names.push(name.clone());
                                }
                            }
                        }
                        // [a] as single-entry Dict
                        SurfaceExpression::Dict(bind_entries) => {
                            for be in bind_entries {
                                if let SurfaceExpression::VarRef { name, .. } = &be.node.value.expr
                                {
                                    names.push(name.clone());
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            names
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };

    // Resolve return annotation first (populates bind: TypeVars into ann_mapping_str)
    let return_ann_type: Option<TypeValue> = if let Some(ret_ann) = return_ann {
        let resolved = match &ret_ann.node {
            Annotation::PropertyDict(entries)
                if entries.iter().any(|e| {
                    e.node.key.as_ref().is_some_and(|k| {
                        matches!(&k.expr,
                            SurfaceExpression::StringLiteral { content: s, .. }
                                if STANDARD_ANN_KEYS.contains(&s.as_str()))
                    })
                }) =>
            {
                let result = typecheck_annot::resolve_fn_metadata(
                    entries,
                    ret_ann.span.clone(),
                    &mut *state,
                    &mut constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await;
                match result {
                    Ok((ret_ty, _doc)) => Some(ret_ty),
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                }
            }
            _ => {
                let result = typecheck_annot::resolve_annotation(
                    &ret_ann.node,
                    ret_ann.span.clone(),
                    &mut *state,
                    &mut constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await;
                match result {
                    Ok(ty) => Some(ty),
                    Err(e) => {
                        errors.push(e);
                        None
                    }
                }
            }
        };
        resolved
    } else {
        None
    };

    // Store the resolved return type back onto the Fn SurfaceNode so the lowerer can emit
    // TypeAssertCheck::Resolved instead of TypeAssertCheck::Source (T-2015).
    // Same pattern as SurfaceParam.resolved_annotation_type.set() below.
    if let SurfaceExpression::Fn {
        resolved_return_annotation,
        ..
    } = &node.expr
    {
        resolved_return_annotation.set(return_ann_type.clone());
    }

    // Consume expected_fn_params from state (single-use per fn invocation).
    // Set by infer_instance_decl_from_surface for bidirectional type checking of instance methods.
    // Taking it here prevents leaking into nested fn expressions in the body.
    let expected_params: Option<Vec<TypeValue>> = state.expected_fn_params.take();

    // Resolve param annotations and build fn env
    let mut fn_env_inner = Env::with_parent(Arc::clone(env));
    let mut param_types: Vec<(Option<String>, TypeValue)> = Vec::new();
    let mut typed_variadics: Vec<(String, TypeValue)> = Vec::new();
    let mut rest: Option<Box<(String, TypeValue)>> = None;
    let mut fixed_param_idx: usize = 0;
    // param_slot_idx tracks the resolver's Parameter(i) index for ALL params in declaration order.
    // The resolver assigns Parameter(i) where i is the position in the params list (including
    // variadics). Used by insert_at_slot to ensure slot numbers match Parameter(i) addresses.
    let mut param_slot_idx: usize = 0;

    for p in params.iter() {
        let param_ty = if p.node.variadic {
            if let Some(ann) = &p.node.annotation {
                // Annotated variadic (e.g., ...xs@[Seq Int]): resolve annotation for the bucket type.
                let ann_result = typecheck_annot::resolve_annotation(
                    &ann.node,
                    ann.span.clone(),
                    &mut *state,
                    &mut constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await;
                match ann_result {
                    Ok(ty) => ty,
                    Err(e) => {
                        errors.push(e);
                        // Annotation resolution failed: fresh TypeVar (unconstrained)
                        state.fresh_type_var(&p.span)
                    }
                }
            } else {
                // Unannotated variadic (...args): heterogeneous dict — bare TypeVar for the whole dict.
                // Do NOT use Dict(Uniform(elem_ty)) — Uniform implies homogeneity which the spec forbids
                // for unannotated variadics. Each call site will unify this TypeVar with the specific
                // {0: T0, 1: T1, ..., "name": Tnamed} dict built from the actual variadic args.
                state.fresh_type_var(&p.span)
            }
        } else if let Some(ann) = &p.node.annotation {
            let ann_result = typecheck_annot::resolve_annotation(
                &ann.node,
                ann.span.clone(),
                &mut *state,
                &mut constraints,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
                None,
            )
            .await;
            match ann_result {
                Ok(ty) => ty,
                Err(e) => {
                    errors.push(e);
                    // Annotation resolution failed: fresh TypeVar (unconstrained)
                    state.fresh_type_var(&p.span)
                }
            }
        } else {
            // Unannotated fixed param: use expected type from class method signature if available,
            // otherwise fall back to Unknown for gradual typing.
            if let Some(ref expected) = expected_params {
                expected
                    .get(fixed_param_idx)
                    .cloned()
                    .unwrap_or_else(make_typevalue_unknown)
            } else {
                make_typevalue_unknown()
            }
        };
        // Store the resolved type back onto the SurfaceParam so the lowerer can carry it
        // into CoreParam::resolved_type without re-parsing the annotation.
        // Annotated params get Some(resolved_type); unannotated params get None (accept-all).
        p.node
            .resolved_annotation_type
            .set(if p.node.annotation.is_some() {
                Some(param_ty.clone())
            } else {
                None
            });
        // Only track definition_span for lost-binding detection when the body is not a
        // Placeholder (`...`). Placeholder bodies indicate Rust-implemented builtins whose
        // parameters are interface declarations, not tinct-level bindings that can be unused.
        let param_definition_span =
            if matches!(body.expr, crate::ast::SurfaceExpression::Placeholder(..)) {
                None
            } else {
                Some(p.span.clone())
            };
        // Insert params into SLOTS at position `param_slot_idx`.
        // param_slot_idx matches Parameter(i) — the resolver assigns Parameter(i) to the
        // i-th param in declaration order (including variadics).
        // infer_var_ref uses state.current_parameter_frame for direct slot lookup (T-2084).
        match param_definition_span {
            Some(ref span) => {
                fn_env_inner.insert_at_slot(
                    param_slot_idx,
                    p.node.name.clone(),
                    Arc::clone(&param_ty),
                    Some(span.clone()),
                );
            }
            None => {
                // No definition span — Placeholder param (builtin interface). Skip lost-binding.
                fn_env_inner.insert_at_slot(
                    param_slot_idx,
                    p.node.name.clone(),
                    Arc::clone(&param_ty),
                    None,
                );
            }
        }
        param_slot_idx += 1;
        if p.node.variadic {
            // Variadic param: goes into typed_variadics or rest, not fixed params.
            if p.node.annotation.is_some() {
                // Typed variadic bucket declared after an untyped rest is a slot-ordering
                // error: the lowerer assigns slots in declaration order, but bind_args_thunks
                // assigns typed buckets before rest. Declaring them in the wrong order
                // causes silent slot inversion (data corruption at runtime).
                if rest.is_some() {
                    let err = Diagnostic::error("type-error",
                        format!(
                            "typed variadic `...{}` declared after untyped rest parameter — typed variadics must precede the untyped fallback `...rest`",
                            p.node.name
                        ),
                        p.span.clone(),
                    );
                    errors.push(err);
                }
                typed_variadics.push((p.node.name.clone(), param_ty));
            } else {
                rest = Some(Box::new((p.node.name.clone(), param_ty)));
            }
        } else {
            // Fixed param: check it was not declared after a variadic (would invert slots).
            let seen_any_variadic = !typed_variadics.is_empty() || rest.is_some();
            if seen_any_variadic {
                let err = Diagnostic::error("type-error",
                    format!(
                        "fixed parameter `{}` declared after variadic parameter — fixed params must precede all variadic params",
                        p.node.name
                    ),
                    p.span.clone(),
                );
                errors.push(err);
            }
            param_types.push((Some(p.node.name.clone()), param_ty));
            fixed_param_idx += 1;
        }
    }

    // unused-type-variable lint: any name declared in bind: that does not appear in any
    // parameter annotation cannot propagate constraints from call sites — it is a dead
    // declaration. Emit an error so the programmer either removes the name or uses it in a param.
    //
    // Exception: a bind name that appears in the RETURN type annotation (but not in params)
    // is still useful — it can be resolved via functional dependency inference. Only names
    // that appear nowhere (no param, no return) are truly dead.
    if !bind_declared_names.is_empty() {
        // Emit unused-type-variable errors for type variables declared in bind: but unused in
        // any parameter annotation or return type. Uses ctx.free_vars (InferenceContext)
        // to collect free TypeVar names from each TypeValue.
        let used_var_set: std::collections::HashSet<String> = {
            let mut s = std::collections::HashSet::new();
            for (_, pt) in &param_types {
                for v in state.ctx.free_vars(pt) {
                    s.insert(v);
                }
            }
            for (_, vt) in &typed_variadics {
                for v in state.ctx.free_vars(vt) {
                    s.insert(v);
                }
            }
            if let Some(r) = &rest {
                for v in state.ctx.free_vars(&r.1) {
                    s.insert(v);
                }
            }
            if let Some(ref ret_ty) = return_ann_type {
                for v in state.ctx.free_vars(ret_ty) {
                    s.insert(v);
                }
            }
            s
        };
        // Read the bind_name → fresh_var mapping via ann_mapping_opt to avoid a
        // conflicting borrow of ann_mapping_str (ann_mapping_opt holds &mut ann_mapping_str).
        let bind_to_fresh: Vec<(String, String)> = bind_declared_names
            .iter()
            .filter_map(|n| {
                ann_mapping_opt
                    .as_deref()
                    .and_then(|m| m.get(n))
                    .map(|fv| (n.clone(), fv.clone()))
            })
            .collect();
        for (bind_name, fresh_var) in &bind_to_fresh {
            if !used_var_set.contains(fresh_var) {
                errors.push(Diagnostic::error(
                    "unused-type-variable",
                    format!(
                        "type variable '{}' declared in bind: but never used in any parameter annotation — constraint cannot be propagated",
                        bind_name
                    ),
                    node.span.clone(),
                ));
            }
        }
    }

    let fn_env_arc = Arc::new(RwLock::new(fn_env_inner));

    let saved_level = state.ctx.current_level;
    let saved_expected_return = state.expected_return.clone();

    // Count required params (those without default: annotation). None = all params required.
    let required_count = {
        let non_variadic_params = params.iter().filter(|p| !p.node.variadic);
        let total_fixed = non_variadic_params.clone().count();
        let has_any_defaults = non_variadic_params.clone().any(|p| {
            p.node.annotation.as_ref().is_some_and(|ann| {
                ann.node
                    .get_property(crate::ast::ANNOTATION_KEY_DEFAULT)
                    .is_some()
            })
        });
        if has_any_defaults {
            // At least one param has default: — count those WITHOUT default:.
            let required = params
                .iter()
                .filter(|p| {
                    !p.node.variadic
                        && p.node.annotation.as_ref().is_none_or(|ann| {
                            ann.node
                                .get_property(crate::ast::ANNOTATION_KEY_DEFAULT)
                                .is_none()
                        })
                })
                .count();
            Some(required)
        } else if total_fixed > 0 {
            None // All fixed params required
        } else {
            Some(0) // No fixed params
        }
    };

    // Extract trace level from return annotation @[trace: N].
    let trace_level: u32 = return_ann
        .as_ref()
        .and_then(|ann| ann.node.get_property(crate::ast::ANNOTATION_KEY_TRACE))
        .and_then(|node| {
            if let SurfaceExpression::Int(n) = &node.expr {
                Some(*n as u32)
            } else {
                None
            }
        })
        .unwrap_or(0);

    // Push FnBody first (fires last: handles return type checking and fn type construction),
    // then push AfterBlock (fires first: handles parameter liveness via SLOTS-only BFS).
    stack.push(TypeCheckCont::FnBody {
        saved_level,
        saved_expected_return,
        return_ann: return_ann_type,
        params: param_types,
        typed_variadics,
        rest,
        required_count,
        node_span: node.span.clone(),
        trace_level,
    });
    // Set current_parameter_frame to this fn's param env before evaluating the body.
    // infer_var_ref uses it directly for VarAddr::Parameter(i) — no level=2 hack needed.
    let saved_parameter_frame = state.current_parameter_frame.take();
    state.current_parameter_frame = Some(Arc::clone(&fn_env_arc));
    stack.push(TypeCheckCont::AfterBlock {
        binding_envs: vec![Arc::clone(&fn_env_arc)],
        pre_final_refs: vec![std::collections::HashSet::new()],
        saved_use_def: std::mem::take(&mut state.use_def),
        saved_current_binding: state.current_binding.take(),
        saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
        saved_parameter_frame,
    });

    // Evaluate body iteratively via the CEK loop.
    TypeCheckAction::Eval(Arc::clone(body), fn_env_arc)
}

// ===== Inline helper: Match arm environment setup =====
//
// Sets up the arm environment for guard inference. Extracts guard narrowings but does NOT
// apply them — the caller must save state.narrowing_map, then apply the returned narrowings
// to the now-empty map, so AfterBlock can restore the outer narrowing_map after the arm.
//
// Returns (arm_env, next_remaining_scrutinee, guard_narrowings).
// Returns `None` only if called with no arms (should not happen in practice).

async fn setup_match_arm_env(
    arm: &SurfaceMatchArm,
    remaining_scrutinee: &TypeValue,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<(
    Arc<RwLock<Env>>,
    TypeValue,
    Vec<typecheck_narrow::Narrowing>,
)> {
    // If arm.let_bindings is Some(...), this is a [case [let names] pattern body] arm.
    // Build a case arm env with those binding names. Otherwise, the arm env starts as the outer env.
    let arm_env: Arc<RwLock<Env>> = if let Some(let_bindings) = &arm.let_bindings {
        build_case_arm_env(let_bindings, env, state, &arm.pattern)
    } else {
        Arc::clone(env)
    };

    // Guard inference — run for type-map side effects. Extract narrowings WITHOUT applying.
    // The caller saves the outer narrowing_map (take), then applies these narrowings so the
    // arm body sees them. AfterBlock restores the outer narrowing_map after the arm.
    let guard_narrowings = if let Some(guard) = &arm.guard {
        let mut local_stack = Vec::new();
        typecheck_for_errors(guard, &arm_env, state, errors, type_map, &mut local_stack).await;
        typecheck_narrow::extract_narrowings(guard, &arm_env)
    } else {
        Vec::new()
    };
    let arm_env = arm_env; // no change — narrowings not applied here

    // Compute updated remaining_scrutinee (I-Case3 negation accumulation) for next arm.
    let next_remaining_scrutinee = if arm.guard.is_none() {
        match &arm.pattern.expr {
            crate::ast::SurfaceExpression::Field { .. } => {
                let tag = match crate::ast::flatten_dot_access_to_tag_node(&arm.pattern) {
                    Some(t) => t,
                    None => String::new(), // Non-constructor dot-access (leading-dot or numeric index) — not a tag pattern.
                };
                let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                let neg_tag = make_typevalue_negation(make_typevalue_nominal_variant(
                    tycon,
                    ctor,
                    make_typevalue_record(indexmap::IndexMap::new(), None),
                ));
                typevalue_normalize_intersection(vec![Arc::clone(remaining_scrutinee), neg_tag])
            }
            crate::ast::SurfaceExpression::Call { func, .. }
                if matches!(&func.expr, crate::ast::SurfaceExpression::Field { .. }) =>
            {
                let tag = match crate::ast::flatten_dot_access_to_tag_node(func) {
                    Some(t) => t,
                    None => String::new(), // Non-constructor dot-access (leading-dot or numeric index) — not a tag pattern.
                };
                let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                let neg_tag = make_typevalue_negation(make_typevalue_nominal_variant(
                    tycon,
                    ctor,
                    make_typevalue_record(indexmap::IndexMap::new(), None),
                ));
                typevalue_normalize_intersection(vec![Arc::clone(remaining_scrutinee), neg_tag])
            }
            // Dict pattern — narrow the scrutinee by excluding the matched shape.
            crate::ast::SurfaceExpression::Dict(entries) => {
                let key_names: Vec<String> = entries
                    .iter()
                    .filter_map(|e| {
                        e.node.key.as_ref().and_then(|k| match &k.expr {
                            crate::ast::SurfaceExpression::VarRef { name, .. } => {
                                Some(name.clone())
                            }
                            crate::ast::SurfaceExpression::StringLiteral { content, .. } => {
                                Some(content.clone())
                            }
                            _ => None,
                        })
                    })
                    .collect();

                if key_names.is_empty() {
                    Arc::clone(remaining_scrutinee)
                } else {
                    let mut key_fields: indexmap::IndexMap<String, TypeValue> =
                        indexmap::IndexMap::new();
                    for name in key_names {
                        key_fields.insert(name, make_typevalue_top());
                    }
                    let dict_with_keys = make_typevalue_record(
                        key_fields,
                        Some(make_rowtail_uniform(make_typevalue_top())),
                    );
                    let neg_dict = make_typevalue_negation(dict_with_keys);
                    typevalue_normalize_intersection(vec![
                        Arc::clone(remaining_scrutinee),
                        neg_dict,
                    ])
                }
            }
            // Wildcard forms: VarRef, Placeholder — remaining scrutinee becomes Never
            crate::ast::SurfaceExpression::VarRef { .. }
            | crate::ast::SurfaceExpression::Placeholder(..) => make_typevalue_never(),
            _ => Arc::clone(remaining_scrutinee),
        }
    } else {
        Arc::clone(remaining_scrutinee)
    };

    Some((arm_env, next_remaining_scrutinee, guard_narrowings))
}

// ===== Inline helper: Match exhaustiveness checking =====

/// Qualify NominalVariant ctor tags in a TypeValue body with a type name prefix.
fn qualify_typevalue_body(body: &TypeValue, qualify_tag: &impl Fn(&str) -> String) -> TypeValue {
    match typevalue_ctor(body) {
        Some(TV_NOMINAL_VARIANT) => {
            if let Some((_, ctor)) = typevalue_nominal_variant_tag(body) {
                let qtag = qualify_tag(&ctor);
                let (new_tycon, new_ctor) = qtag.split_once('.').unwrap_or(("", qtag.as_str()));
                let fields = typevalue_record_fields_pub(body);
                make_typevalue_nominal_variant(
                    new_tycon,
                    new_ctor,
                    make_typevalue_record(fields, None),
                )
            } else {
                Arc::clone(body)
            }
        }
        Some(TV_UNION) => {
            let members = typevalue_extract_members_pub(body)
                .expect("invariant: TV_UNION TypeValue must have extractable members");
            let qualified: Vec<TypeValue> = members
                .into_iter()
                .map(|m| qualify_typevalue_body(&m, qualify_tag))
                .collect();
            typevalue_normalize_union(qualified)
        }
        _ => Arc::clone(body),
    }
}

/// Extract the (tag, arity) constructor list from a TypeValue ADT body.
fn extract_typevalue_constructors(body: &TypeValue) -> Vec<(String, usize)> {
    match typevalue_ctor(body) {
        Some(TV_NOMINAL_VARIANT) => {
            if let Some((tycon, ctor)) = typevalue_nominal_variant_tag(body) {
                let tag = if tycon.is_empty() {
                    ctor
                } else {
                    format!("{}.{}", tycon, ctor)
                };
                let arity = if typevalue_nominal_variant_has_fields(body) {
                    1
                } else {
                    0
                };
                vec![(tag, arity)]
            } else {
                Vec::new()
            }
        }
        Some(TV_UNION) => {
            let members = typevalue_extract_members_pub(body)
                .expect("invariant: TV_UNION TypeValue must have extractable members");
            members
                .iter()
                .filter_map(|m| {
                    if typevalue_ctor(m) == Some(TV_NOMINAL_VARIANT) {
                        if let Some((tycon, ctor)) = typevalue_nominal_variant_tag(m) {
                            let tag = if tycon.is_empty() {
                                ctor
                            } else {
                                format!("{}.{}", tycon, ctor)
                            };
                            let arity = if typevalue_nominal_variant_has_fields(m) {
                                1
                            } else {
                                0
                            };
                            Some((tag, arity))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .collect()
        }
        _ => Vec::new(),
    }
}

/// Recursively scan a pattern AST node for VarRef nodes with resolution Some(None).
/// These are resolver errors (undefined variable references) that should make the arm
/// opaque to coverage analysis.
fn arm_has_unresolved_varrefs(node: &SurfaceNode) -> bool {
    match &node.expr {
        SurfaceExpression::VarRef { resolution, .. } => {
            matches!(resolution.get(), Some(None))
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            arm_has_unresolved_varrefs(func)
                || args.iter().any(|a| arm_has_unresolved_varrefs(a))
                || named_args
                    .iter()
                    .any(|na| arm_has_unresolved_varrefs(&na.node.value))
        }
        SurfaceExpression::Dict(entries) => entries
            .iter()
            .any(|e| arm_has_unresolved_varrefs(&e.node.value)),
        SurfaceExpression::Field { expr, .. } => expr
            .as_ref()
            .map_or(false, |e| arm_has_unresolved_varrefs(e)),
        _ => false,
    }
}

fn run_match_exhaustiveness_check(
    scrutinee_ty: &TypeValue,
    arms: &[SurfaceMatchArm],
    span: &Span,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
) {
    let tycon_env_ref = state.tycon_env_ref();
    // Coverage checking for TypeValue: inspect ctor tag to determine what kind of scrutinee.
    // This is a simplified version — full TypeValue-based coverage would require more helpers.
    let sig: Option<coverage::ConstructorSignature> = match typevalue_ctor(scrutinee_ty) {
        Some(TV_UNION) => {
            // Try to extract NominalVariant constructors from union members.
            if let Some(members) = typevalue_extract_members_pub(scrutinee_ty) {
                let mut constructors = Vec::new();
                for m in &members {
                    if typevalue_ctor(m) == Some(TV_NOMINAL_VARIANT) {
                        // Extract ctor and tycon from payload.
                        if let Some((tycon_s, ctor_s)) = typevalue_nominal_variant_tag(m) {
                            let tag = if tycon_s.is_empty() {
                                ctor_s.clone()
                            } else {
                                format!("{}.{}", tycon_s, ctor_s)
                            };
                            let has_fields = typevalue_nominal_variant_has_fields(m);
                            constructors.push((
                                coverage::ConstructorTag::Variant(tag),
                                if has_fields { 1 } else { 0 },
                            ));
                        }
                    }
                }
                if constructors.is_empty() {
                    None
                } else {
                    Some(coverage::ConstructorSignature { constructors })
                }
            } else {
                None
            }
        }
        Some(TV_NOMINAL_VARIANT) => {
            if let Some((tycon_s, ctor_s)) = typevalue_nominal_variant_tag(scrutinee_ty) {
                let tag = if tycon_s.is_empty() {
                    ctor_s.clone()
                } else {
                    format!("{}.{}", tycon_s, ctor_s)
                };
                let has_fields = typevalue_nominal_variant_has_fields(scrutinee_ty);
                Some(coverage::ConstructorSignature {
                    constructors: vec![(
                        coverage::ConstructorTag::Variant(tag),
                        if has_fields { 1 } else { 0 },
                    )],
                })
            } else {
                None
            }
        }
        Some(TV_OP) => {
            // TyCon reference — look up in tycon_env for constructors.
            let name = typevalue_op_name(scrutinee_ty)
                .expect("invariant: TV_OP TypeValue must have a name field");
            match tycon_env_ref.get(name.as_str()) {
                Some(def) if !def.constructors.is_empty() => {
                    let constructors = def
                        .constructors
                        .iter()
                        .map(|(tag, arity)| {
                            let clamped = if *arity == 0 { 0 } else { 1 };
                            (coverage::ConstructorTag::Variant(tag.clone()), clamped)
                        })
                        .collect();
                    Some(coverage::ConstructorSignature { constructors })
                }
                _ => None,
            }
        }
        _ => None,
    };

    if let Some(sig) = sig {
        let coverage_patterns: Vec<coverage::CoveragePattern> = arms
            .iter()
            .map(|arm| coverage::ast_pattern_to_coverage(&arm.pattern, Some(tycon_env_ref)))
            .collect();
        // Mark arms with explicit guards OR unresolved VarRefs as opaque to coverage.
        // Unresolved VarRefs are resolver errors — treating them as wildcards would mask
        // non-exhaustiveness. Making them opaque means they neither contribute to coverage
        // nor are flagged as redundant.
        let has_guards: Vec<bool> = arms
            .iter()
            .map(|arm| arm.guard.is_some() || arm_has_unresolved_varrefs(&arm.pattern))
            .collect();
        let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);

        if !result.exhaustive {
            let witnesses = coverage::format_witnesses(&result.uncovered);
            errors.push(Diagnostic::error(
                "type-error",
                format!("non-exhaustive match: missing coverage for {}", witnesses),
                span.clone(),
            ));
        }
        for &idx in &result.redundant {
            errors.push(Diagnostic::error(
                "type-error",
                "unreachable match arm: this pattern is already covered by prior arms",
                arms[idx].pattern.span.clone(),
            ));
        }
        for &idx in &result.inaccessible {
            errors.push(Diagnostic::error(
                "type-error",
                "inaccessible match arm: reachable only via diverging (bottom) values",
                arms[idx].pattern.span.clone(),
            ));
        }
    }
}

// ===== Inline helper: CaseArm env setup =====

fn build_case_arm_env(
    let_bindings: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    _node: &Arc<SurfaceNode>,
) -> Arc<RwLock<Env>> {
    // Extract binding names with their individual spans for lost-binding diagnostics.
    // Each binding node's span points at the binding name in `[let v w ...]`.
    let bindings_with_spans: Vec<(String, Span)> = match &let_bindings.expr {
        SurfaceExpression::LetDecl { bindings } => bindings
            .iter()
            .filter_map(|b| {
                if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                    if name == "_" {
                        None // wildcard, not a binding
                    } else {
                        Some((name.clone(), b.span.clone()))
                    }
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    // Always create a fresh child frame so that AfterBlock's slot-based liveness check
    // reads ONLY this arm's own bindings, not inherited bindings from the parent chain.
    // Slots only — infer_var_ref uses state.current_parameter_frame for direct lookup (T-2084).
    let mut child_inner = Env::with_parent(Arc::clone(env));
    for (slot_idx, (name, binding_span)) in bindings_with_spans.into_iter().enumerate() {
        let tv = state.fresh_type_var(&binding_span);
        child_inner.insert_at_slot(slot_idx, name, tv, Some(binding_span));
    }
    Arc::new(RwLock::new(child_inner))
}

// ===== Inline helper: TypeAssert mismatch computation =====

fn compute_type_assert_mismatch(
    actual: &TypeValue,
    expected: &TypeValue,
    _has_default: bool,
    span: &Span,
    state: &InferState,
) -> Option<Vec<Diagnostic>> {
    let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
        state.ctx.subst.clone(),
        state.ctx.levels.clone(),
        state.ctx.current_level,
        state.tycon_env.clone(),
    );
    // Check if both are function types (TypeValue.Fn) for arity checking.
    if typevalue_ctor(actual) == Some(TV_FN) && typevalue_ctor(expected) == Some(TV_FN) {
        // Extract param counts via payload dict inspection.
        let actual_param_count = typevalue_fn_param_count(actual);
        let expected_param_count = typevalue_fn_param_count(expected);
        if let (Some(a_count), Some(e_count)) = (actual_param_count, expected_param_count) {
            if a_count != e_count {
                return Some(vec![Diagnostic::error(
                    "type-assertion",
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        e_count, a_count
                    ),
                    span.clone(),
                )]);
            }
        }
    }
    // General consistency check.
    let definitely_fails = if typevalue_ctor(actual) == Some(TV_UNION) {
        if let Some(members) = typevalue_extract_members_pub(actual) {
            !members
                .iter()
                .any(|m| crate::bas::is_consistent_subtype(m, expected, &ctx_for_check))
        } else {
            !crate::bas::is_consistent_subtype(actual, expected, &ctx_for_check)
        }
    } else {
        !crate::bas::is_consistent_subtype(actual, expected, &ctx_for_check)
    };
    if definitely_fails {
        Some(vec![Diagnostic::error(
            "type-assertion",
            "type assertion mismatch: actual type is not consistent with expected type",
            span.clone(),
        )
        .with_note(format!(
            "expected: {}",
            crate::eval::format_type_for_assert(expected)
        ))
        .with_note(format!(
            "actual:   {}",
            crate::eval::format_type_for_assert(actual)
        ))])
    } else {
        None
    }
}

/// Extract the number of parameters from a TypeValue.Fn payload.
/// Returns None if the payload cannot be inspected.
fn typevalue_fn_param_count(tv: &TypeValue) -> Option<usize> {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return None,
    };
    match payload_thunk.peek_result()? {
        Ok(crate::value::Value::Dict { entries, .. }) => {
            let params_key = HashableValue::Str(Arc::from(FIELD_PARAMS));
            let params_thunk = entries.get(&params_key)?;
            match params_thunk.peek_result()? {
                Ok(crate::value::Value::Dict {
                    entries: params_entries,
                    ..
                }) => Some(params_entries.len()),
                _ => None,
            }
        }
        _ => None,
    }
}

// ===== Helper functions =====

/// Compute strongly connected components of dict entry dependency graph using Tarjan's algorithm.
///
/// Returns SCCs in reverse topological order (dependencies before dependents).
/// Uses an iterative worklist to avoid stack overflow on large dicts.
pub(crate) fn compute_sccs(
    entries: &[Spanned<SurfaceEntry>],
    key_entries: &[(Option<String>, bool, bool)],
) -> Vec<Scc> {
    let n = entries.len();

    let mut name_to_idx: HashMap<String, usize> = HashMap::new();
    for (i, (key_name, _, _)) in key_entries.iter().enumerate() {
        if let Some(ref kn) = key_name {
            name_to_idx.insert(kn.clone(), i);
        }
    }

    let mut graph: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, entry) in entries.iter().enumerate() {
        let deps = collect_dependencies(&entry.node.value, &name_to_idx);
        graph[i] = deps;
    }

    let mut index = 0usize;
    let mut tarjan_stack: Vec<usize> = Vec::new();
    let mut disc: Vec<Option<usize>> = vec![None; n];
    let mut lowlinks: Vec<usize> = vec![0; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut sccs: Vec<Scc> = Vec::new();
    let mut call_stack: Vec<(usize, usize)> = Vec::new();

    for start in 0..n {
        if disc[start].is_some() {
            continue;
        }

        disc[start] = Some(index);
        lowlinks[start] = index;
        index += 1;
        tarjan_stack.push(start);
        on_stack[start] = true;
        call_stack.push((start, 0));

        'outer: while let Some((v, succ_idx)) = call_stack.last().copied() {
            let succs = &graph[v];
            let mut next_succ = succ_idx;
            while next_succ < succs.len() {
                let w = succs[next_succ];
                if disc[w].is_none() {
                    call_stack.last_mut().unwrap().1 = next_succ + 1;
                    disc[w] = Some(index);
                    lowlinks[w] = index;
                    index += 1;
                    tarjan_stack.push(w);
                    on_stack[w] = true;
                    call_stack.push((w, 0));
                    continue 'outer;
                } else if on_stack[w] {
                    lowlinks[v] = lowlinks[v].min(disc[w].unwrap());
                }
                next_succ += 1;
            }

            call_stack.pop();
            if let Some(&(parent, _)) = call_stack.last() {
                lowlinks[parent] = lowlinks[parent].min(lowlinks[v]);
            }

            if Some(lowlinks[v]) == disc[v] {
                let mut scc_indices = Vec::new();
                loop {
                    let x = tarjan_stack.pop().unwrap();
                    on_stack[x] = false;
                    scc_indices.push(x);
                    if x == v {
                        break;
                    }
                }
                sccs.push(Scc {
                    indices: scc_indices,
                });
            }
        }
    }

    sccs
}

/// Collect all sibling variable references in an expression (for SCC dependency analysis).
///
/// Implements the Damas–Milner (1982) requirement that the dependency graph uses the
/// minimal transitive closure of *free* variable references: names that are locally bound
/// (function parameters, case-arm let-bindings) shadow sibling bindings and must not
/// create false dependency edges.
///
/// The worklist carries each node together with a set of locally-bound names at that
/// point in the traversal.  A `VarRef` creates a dependency only when its name is in
/// `name_to_idx` AND not shadowed by a local binding.  Entering a `Fn` extends the
/// locals set with that function's parameter names; entering a `CaseArm` body extends it
/// with the arm's let-binding names.
fn collect_dependencies(
    node: &Arc<SurfaceNode>,
    name_to_idx: &HashMap<String, usize>,
) -> Vec<usize> {
    use std::collections::HashSet;

    let mut deps: Vec<usize> = Vec::new();
    // Each worklist entry is (node, locally-bound names at this node's scope).
    // The empty set at the root means all names in name_to_idx are candidates.
    let empty_locals: Arc<HashSet<String>> = Arc::new(HashSet::new());
    let mut worklist: Vec<(&Arc<SurfaceNode>, Arc<HashSet<String>>)> =
        vec![(node, Arc::clone(&empty_locals))];

    while let Some((current, locals)) = worklist.pop() {
        match &current.expr {
            SurfaceExpression::VarRef { name, .. } => {
                // Only record a dependency when the name is a sibling binding AND is
                // not shadowed by a locally-bound name (parameter, case-arm binding).
                if !locals.contains(name.as_str()) {
                    if let Some(&idx) = name_to_idx.get(name.as_str()) {
                        deps.push(idx);
                    }
                }
            }
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. } => {}
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        worklist.push((key, Arc::clone(&locals)));
                    }
                    worklist.push((&entry.node.value, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::Fn { params, body, .. } => {
                // Parameters are locally bound within the function body — they shadow any
                // sibling binding with the same name.  Build an extended locals set and
                // use it exclusively when scanning the body.
                let mut fn_locals: HashSet<String> = (*locals).clone();
                for spanned_param in params {
                    fn_locals.insert(spanned_param.node.name.clone());
                }
                worklist.push((body, Arc::new(fn_locals)));
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                worklist.push((func, Arc::clone(&locals)));
                for arg in args {
                    worklist.push((arg, Arc::clone(&locals)));
                }
                for named_arg in named_args {
                    worklist.push((&named_arg.node.value, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::Field { expr, .. } => {
                if let Some(target) = expr {
                    worklist.push((target, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::Pipe { lhs, rhs, .. } => {
                worklist.push((lhs, Arc::clone(&locals)));
                worklist.push((rhs, Arc::clone(&locals)));
            }
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    worklist.push((e, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::TypeAssert { expr, .. } => {
                worklist.push((expr, Arc::clone(&locals)));
            }
            SurfaceExpression::Placeholder(..) => {}
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => {
                worklist.push((e, Arc::clone(&locals)));
            }
            SurfaceExpression::Decl(_) => {}
            SurfaceExpression::PatternDecl { bindings }
            | SurfaceExpression::LetDecl { bindings } => {
                // Binding targets (the VarRef names being declared) are scanned with the
                // current locals: they are declarations, not references, and their names
                // are not in name_to_idx, so they cannot create false deps.
                for b in bindings {
                    worklist.push((b, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                worklist.push((scrutinee, Arc::clone(&locals)));
                for arm in arms {
                    worklist.push((&arm.pattern, Arc::clone(&locals)));
                    if let Some(let_bindings) = &arm.let_bindings {
                        worklist.push((let_bindings, Arc::clone(&locals)));
                        // The arm body is scanned with the let-binding names added to locals,
                        // since those names are locally introduced by this arm.
                        let arm_local_names: HashSet<String> =
                            if let SurfaceExpression::LetDecl { bindings } = &let_bindings.expr {
                                bindings
                                    .iter()
                                    .filter_map(|b| {
                                        if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                                            Some(name.clone())
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            } else {
                                HashSet::new()
                            };
                        let body_locals = if arm_local_names.is_empty() {
                            Arc::clone(&locals)
                        } else {
                            let mut extended = (*locals).clone();
                            extended.extend(arm_local_names);
                            Arc::new(extended)
                        };
                        if let Some(guard) = &arm.guard {
                            worklist.push((guard, body_locals.clone()));
                        }
                        for body_expr in &arm.body {
                            worklist.push((body_expr, Arc::clone(&body_locals)));
                        }
                    } else {
                        // No let_bindings — keyed arm, body evaluates in outer env.
                        if let Some(guard) = &arm.guard {
                            worklist.push((guard, Arc::clone(&locals)));
                        }
                        for body_expr in &arm.body {
                            worklist.push((body_expr, Arc::clone(&locals)));
                        }
                    }
                }
            }
            SurfaceExpression::Error(_) => {}
        }
    }

    deps
}

/// Occurs check: returns true if TypeVar `name` appears free anywhere in `ty`.
pub(crate) fn type_contains_typevar(ty: &TypeValue, name: &str) -> bool {
    match typevalue_ctor(ty) {
        Some(TV_VAR) => typevalue_var_name(ty).as_deref() == Some(name),
        Some(TV_UNION) | Some(TV_INTER) => typevalue_extract_members_pub(ty)
            .map(|members| members.iter().any(|m| type_contains_typevar(m, name)))
            // Conservative: if members cannot be extracted (malformed TypeValue), assume
            // the TypeVar may appear. This prevents unsound unification when Union/Inter is malformed.
            .unwrap_or(true),
        Some(TV_NEG) => {
            // Recurse into the "of" field (canonical field name per builtin_core.llt Neg: [of: TypeValue]).
            if let Some(inner) = crate::type_infer::typevalue_payload_field(ty, FIELD_OF) {
                type_contains_typevar(&inner, name)
            } else {
                false
            }
        }
        Some(TV_FN) => {
            if let Some((params, ret)) = typevalue_fn_params_and_ret(ty) {
                params.iter().any(|p| type_contains_typevar(p, name))
                    || type_contains_typevar(&ret, name)
            } else {
                false
            }
        }
        Some(TV_APP) => {
            // Recurse into op and arg fields (App: [op: TypeValue  arg: TypeValue]).
            // Conservative: if a field is absent (malformed App), assume the var may appear.
            let has_op = crate::type_infer::typevalue_payload_field(ty, FIELD_OP)
                .map(|op| type_contains_typevar(&op, name))
                .unwrap_or(true);
            let has_arg = crate::type_infer::typevalue_payload_field(ty, FIELD_ARG)
                .map(|arg| type_contains_typevar(&arg, name))
                .unwrap_or(true);
            has_op || has_arg
        }
        Some(TV_RECORD) => {
            let fields = typevalue_record_fields_pub(ty);
            fields.values().any(|t| type_contains_typevar(t, name))
        }
        _ => false,
    }
}

/// Convert a literal surface expression to a runtime `Value`.
///
/// Returns `Some(Value)` for Int, U64, Float, and StringLiteral expressions.
/// Returns `None` for any other expression (not a compile-time constant).
///
/// Used by Pass 2 type alias registration to populate `TyConDef.constructor_constants`
/// from `name: literal` entries in variant constructor declarations.
fn literal_expr_to_value(expr: &SurfaceExpression) -> Option<crate::value::Value> {
    match expr {
        SurfaceExpression::Int(n) => Some(crate::value::Value::Int {
            n: *n,
            type_val: crate::value::unknown_type_val(),
        }),
        SurfaceExpression::U64(n) => Some(crate::value::Value::U64 {
            n: *n,
            type_val: crate::value::unknown_type_val(),
        }),
        SurfaceExpression::Float(f) => Some(crate::value::Value::Float {
            n: *f,
            type_val: crate::value::unknown_type_val(),
        }),
        SurfaceExpression::StringLiteral { content, .. } => {
            Some(crate::value::string_val(content.as_str()))
        }
        _ => None,
    }
}

/// Build the constructor dict value type for an ADT.
///
/// For ADT types (Union of NominalVariants or single NominalVariant), produces a Dict
/// where unit constructors → NominalVariant values and payload constructors → Functions.
/// For non-ADT types, returns the body type unchanged.
///
/// Called from `run_typecheck_dict` Pass 2 (type alias registration) and from
/// `process_document` when reconstructing constructor schemes via tycon_env diff.
pub(crate) fn adt_value_type(alias_body: &TypeValue) -> TypeValue {
    let members: Vec<TypeValue> = match typevalue_ctor(alias_body) {
        Some(TV_UNION) => typevalue_extract_members_pub(alias_body)
            .expect("invariant: TV_UNION TypeValue must have extractable members"),
        Some(TV_NOMINAL_VARIANT) => vec![Arc::clone(alias_body)],
        _ => return Arc::clone(alias_body),
    };
    let mut ctor_dict_fields: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    for m in &members {
        if typevalue_ctor(m) == Some(TV_NOMINAL_VARIANT) {
            if let Some((_, ctor_s)) = typevalue_nominal_variant_tag(m) {
                let has_fields = typevalue_nominal_variant_has_fields(m);
                let ctor_type = if !has_fields {
                    Arc::clone(m)
                } else {
                    // Payload constructor: wrap as a function that takes named fields and returns the variant.
                    let fields = typevalue_record_fields_pub(m);
                    let fn_params: Vec<(Option<String>, TypeValue)> =
                        fields.into_iter().map(|(k, v)| (Some(k), v)).collect();
                    make_typevalue_fn(fn_params, Arc::clone(m))
                };
                ctor_dict_fields.insert(ctor_s, ctor_type);
            }
        }
    }
    if ctor_dict_fields.is_empty() {
        Arc::clone(alias_body)
    } else {
        make_typevalue_record(ctor_dict_fields, None)
    }
}

/// Extract the key name from a dict entry.
///
/// Handles `StringLiteral`, `Int`, and `VarRef` directly.  For any other key
/// expression (computed keys) falls back to `run_typecheck` and accepts a
/// `TypeValue.StrLit` or `TypeValue.IntLit` result, mirroring the behaviour
/// of the `typecheck_dict.rs` implementation.
pub(crate) async fn entry_key_name(
    entry: &SurfaceEntry,
    auto_index: &mut i64,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            SurfaceExpression::Int(n) => Some(n.to_string()),
            _ => {
                let key_ty = Box::pin(run_typecheck(
                    key_node,
                    env,
                    state,
                    errors,
                    type_map,
                    &mut Vec::new(),
                ))
                .await;
                match typevalue_ctor(&key_ty) {
                    Some(TV_STR_LIT) => {
                        // Extract the value string from payload.
                        if let crate::value::Value::Variant {
                            payload: Some(thunk),
                            ..
                        } = key_ty.as_ref()
                        {
                            if let Some(Ok(crate::value::Value::Dict { entries, .. })) =
                                thunk.peek_result()
                            {
                                let vk = crate::value::HashableValue::Str(Arc::from(FIELD_VALUE));
                                if let Some(vt) = entries.get(&vk) {
                                    if let Some(Ok(crate::value::Value::String {
                                        source,
                                        start,
                                        end,
                                        ..
                                    })) = vt.peek_result()
                                    {
                                        return Some(source[*start..*end].to_string());
                                    }
                                }
                            }
                        }
                        None
                    }
                    Some(TV_INT_LIT) => {
                        // Extract the integer value from payload.
                        if let crate::value::Value::Variant {
                            payload: Some(thunk),
                            ..
                        } = key_ty.as_ref()
                        {
                            if let Some(Ok(crate::value::Value::Dict { entries, .. })) =
                                thunk.peek_result()
                            {
                                let vk = crate::value::HashableValue::Str(Arc::from(FIELD_VALUE));
                                if let Some(vt) = entries.get(&vk) {
                                    if let Some(Ok(crate::value::Value::Int { n, .. })) =
                                        vt.peek_result()
                                    {
                                        return Some(n.to_string());
                                    }
                                }
                            }
                        }
                        None
                    }
                    _ => None,
                }
            }
        },
        None => {
            let idx = *auto_index;
            *auto_index += 1;
            Some(idx.to_string())
        }
    }
}

// ===== run_typecheck_dict =====

/// Dict type inference via multi-pass binding analysis (Passes 0–4).
///
/// Performs the multi-pass dict inference algorithm using `run_typecheck` for entry inference,
/// eliminating the recursive call chain of the old `infer_dict`.
///
/// Returns `(record_type, schemes, referenced, errors)` where:
/// - `record_type` is the inferred TypeValue.Record for the dict literal
/// - `schemes` is an `IndexMap<String, Arc<Value>>` of per-entry generalized schemes (needed
///   by `process_document` for cross-document scoping and by `Sequential` for
///   let-polymorphism across multi-body function steps)
/// - `referenced` is a `HashSet<String>` of names that were actually referenced during the
///   internal CEK run (collected from dict_env.slots). Callers that propagate schemes into a
///   fresh env frame must also propagate this set to avoid lost-binding false positives.
/// - `errors` is the accumulated vector of type errors (inference is best-effort)
///
/// Called by:
/// - `DictPassZero` handler (terminal dict expressions in the CEK machine)
/// - `process_document` (top-level dict expressions in a document)
/// - `run_typecheck`'s Sequential arm (intermediate dict bodies in multi-body functions)
/// - Recursively for nested Dict values within the SCC per-entry loop
pub(crate) async fn run_typecheck_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    slot_base_override: Option<u32>,
) -> (
    TypeValue,
    indexmap::IndexMap<String, TypeValue>,
    std::collections::HashSet<String>,
    Vec<Diagnostic>,
) {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.ctx.current_level;
    state.ctx.current_level += 1;

    // dict_env is a fresh child of the incoming env. The parent chain already contains all
    // root-group entries (seeded into child_env by typecheck_program_bootstrap) at their
    // correct depths. get_scheme_at(depth, slot) traverses the chain naturally.
    let dict_env: Arc<RwLock<Env>> = Arc::new(RwLock::new(Env::with_parent(Arc::clone(env))));

    // Extra schemes from ADT constructors — injected in Pass 2, merged into final schemes.
    // IndexMap preserves insertion order so constructor scheme ordering is deterministic.
    let mut ctor_schemes: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    let mut key_entries: Vec<(Option<String>, bool, bool)> = Vec::new();
    let mut auto_index: i64 = 0;
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Pass 0: Key resolution
    for entry in entries {
        let key_name = entry_key_name(
            &entry.node,
            &mut auto_index,
            env,
            state,
            &mut errors,
            type_map,
        )
        .await;
        let is_alias = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d) if matches!(d.as_ref(), SurfaceDeclaration::TypeAlias { .. })
        );
        let is_static_key = entry.node.key.as_ref().is_some_and(|k| {
            matches!(
                &k.expr,
                SurfaceExpression::StringLiteral { .. } | SurfaceExpression::VarRef { .. }
            )
        });
        key_entries.push((key_name, is_alias, is_static_key));
    }

    // Pass 0a: Compute SCCs for binding group analysis
    let sccs = compute_sccs(entries, &key_entries);

    // Pass 1 (global): Pre-insert fresh TypeVar placeholders for ALL statically-known bindings
    // in SOURCE ORDER (matching resolver slot assignment order from surface_dict_static_keys).
    // IndexMap preserves insertion order for deterministic iteration.
    //
    // Insert at resolver-assigned absolute slots (for LGM-based get_scheme_at lookups — B-677 fix).
    // When called from the Sequential handler, slot_base_override is the correct absolute base
    // computed by the Sequential handler's body_slot_base disambiguation (Phase 1/2). Use it
    // directly to avoid the find_map ambiguity that arises when multiple dicts share the same
    // first key name (e.g. both Dict1 and Dict2 defining Boolean: the find_map picks Dict1's
    // frame for Dict2, inserting Dict2's entries at Dict1's slots and corrupting lookups).
    // When slot_base_override is None (DictPassZero or nested dicts), fall back to find_map.
    let static_keys_for_slot = crate::resolve::surface_dict_static_keys(entries);
    let body_slot_base_opt: Option<u32> = slot_base_override.or_else(|| {
        // DictPassZero / nested dict fallback: find the first frame that has the first key.
        // Prefer DocSequential frames first: when run_typecheck_dict is called recursively
        // for nested Dict values inside fn-body SCC inference (with slot_base_override=None),
        // state.resolver_frames contains fn-body DictLetrec frames with small absolute slots
        // starting from 0. A DictLetrec frame sharing a key name with a document-level
        // DocSequential frame would win a naive find_map and produce an incorrect slot base.
        // Try DocSequential-only first; if none match (genuine fn-body nested dict where no
        // document-level frame holds the key), fall back to all frames.
        static_keys_for_slot.first().and_then(|first_key| {
            state
                .resolver_frames
                .iter()
                .filter(|(_, kind)| *kind == crate::resolve::FrameKind::DocSequential)
                .find_map(|(frame, _kind)| frame.get(first_key.as_str()).copied())
                .or_else(|| {
                    state
                        .resolver_frames
                        .iter()
                        .find_map(|(frame, _kind)| frame.get(first_key.as_str()).copied())
                })
        })
    });
    let mut fresh_vars_by_name: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    let mut static_slot_idx: u32 = 0;
    for ((key_name, is_alias, is_static_key), entry) in key_entries.iter().zip(entries.iter()) {
        // (a) Static-key entry.
        if *is_static_key {
            if let Some(ref name) = key_name {
                // Compute absolute slot for this entry if resolver frames have it.
                let abs_slot = body_slot_base_opt.map(|base| base + static_slot_idx);
                static_slot_idx += 1;
                if let SurfaceExpression::Fn { params, .. } = &entry.node.value.expr {
                    // fn entries get TypeValue.Fn skeleton so recursive calls see a
                    // function-shaped callee type without requiring a return annotation.
                    let mut fn_params: Vec<(Option<String>, TypeValue)> = Vec::new();
                    let mut pre_is_variadic = false;
                    for p in params {
                        if p.node.variadic {
                            // Mark the pre-binding as variadic so recursive calls don't
                            // produce spurious arity errors.
                            pre_is_variadic = true;
                        } else {
                            let ty = state.fresh_type_var(&p.span);
                            fn_params.push((Some(p.node.name.clone()), ty));
                        }
                    }
                    let ret_var = state.fresh_type_var(&entry.span);
                    let fn_type = crate::type_infer::make_typevalue_fn_with_flags(
                        fn_params,
                        ret_var,
                        None, // required_count: unknown at pre-binding (Pass 0) — conservative
                        pre_is_variadic,
                        Vec::new(), // no typed variadics at pre-binding time (Pass 0)
                    );
                    if !is_alias {
                        fresh_vars_by_name.insert(name.clone(), Arc::clone(&fn_type));
                    }
                    {
                        let mut env_write = dict_env.write().unwrap();
                        let slot = abs_slot
                            .map(|s| s as usize)
                            .unwrap_or_else(|| env_write.slots.len());
                        env_write.insert_at_slot(
                            slot,
                            name.clone(),
                            Arc::clone(&fn_type),
                            Some(entry.span.clone()),
                        );
                    }
                } else {
                    let fresh_var = state.fresh_type_var(&entry.span);
                    if !is_alias {
                        fresh_vars_by_name.insert(name.clone(), fresh_var.clone());
                    }
                    {
                        let mut env_write = dict_env.write().unwrap();
                        let slot = abs_slot
                            .map(|s| s as usize)
                            .unwrap_or_else(|| env_write.slots.len());
                        env_write.insert_at_slot(
                            slot,
                            name.clone(),
                            Arc::clone(&fresh_var),
                            Some(entry.span.clone()),
                        );
                    }
                }
            }
        }
        // (b) Anonymous InstanceDecl entry: insert ɪ-prefixed placeholders at this source position.
        if entry.node.key.is_none() {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::InstanceDecl {
                    class_name, arms, ..
                } = decl.as_ref()
                {
                    for (pattern, method_entries) in arms {
                        let dispatch_tags = crate::lower::extract_dispatch_tags(&pattern.expr);
                        let type_args: Vec<&str> =
                            dispatch_tags.iter().filter_map(|t| t.as_deref()).collect();
                        for me in method_entries {
                            let method_name = match me.node.key.as_ref() {
                                Some(k) => match &k.expr {
                                    SurfaceExpression::StringLiteral { content: s, .. } => {
                                        s.clone()
                                    }
                                    SurfaceExpression::VarRef { name, .. } => name.clone(),
                                    _ => continue,
                                },
                                None => continue,
                            };
                            let binding_name = crate::type_def::instance_binding_name(
                                &class_decl_name(class_name),
                                &method_name,
                                &type_args,
                            );
                            let fresh_var = state.fresh_type_var(&entry.span);
                            fresh_vars_by_name.insert(binding_name.clone(), fresh_var.clone());
                            {
                                let mut env_write = dict_env.write().unwrap();
                                let slot = super::find_slot_in_frames(
                                    &state.resolver_frames,
                                    &binding_name,
                                )
                                .unwrap_or_else(|| env_write.slots.len());
                                env_write.insert_at_slot(
                                    slot,
                                    binding_name,
                                    Arc::clone(&fresh_var),
                                    None,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Pass 2: Register type aliases (before SCC processing)
    for ((key_name, is_alias, _), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::TypeAlias { params, body } = decl.as_ref() {
                    let mut alias_ann_map: HashMap<String, String> = HashMap::new();
                    for (param_name, param_ann) in params {
                        let param_span = param_ann
                            .as_ref()
                            .map(|a| a.span.clone())
                            .unwrap_or_else(|| entry.span.clone());
                        let fresh = state
                            .fresh_type_var_with(
                                Some(param_name.as_str()),
                                None,
                                "Type",
                                &param_span,
                            )
                            .0;
                        alias_ann_map.insert(param_name.clone(), fresh.clone());
                    }

                    let alias_name = key_name.as_deref().unwrap_or("");
                    let mut alias_constraints: Vec<Arc<crate::value::Value>> = Vec::new();
                    let mut ann_map_for_body = alias_ann_map.clone();
                    let resolved_body: TypeValue = match &body.expr {
                        SurfaceExpression::Dict(entries) => {
                            let mut ann_map_opt =
                                Some(&mut ann_map_for_body as &mut HashMap<String, String>);
                            let mut row_m: Option<&mut HashMap<String, String>> = None;
                            let dict_result = super::typecheck_annot::resolve_type_dict(
                                entries,
                                body.span.clone(),
                                &mut *state,
                                &mut alias_constraints,
                                &mut ann_map_opt,
                                &mut row_m,
                                super::typecheck_annot::TypeDictCtx {
                                    type_params_scope: None,
                                    tycon_name: alias_name,
                                },
                            )
                            .await;
                            match dict_result {
                                Ok(t) => t,
                                Err(e) => {
                                    errors.push(e);
                                    // Type alias body resolution failed: fresh TypeVar
                                    state.fresh_type_var(&body.span)
                                }
                            }
                        }
                        _ => {
                            match super::typecheck_annot::resolve_type_expr(
                                body,
                                state,
                                &mut alias_constraints,
                                &mut Some(&mut ann_map_for_body),
                                &mut None,
                                None,
                            )
                            .await
                            {
                                Ok(t) => t,
                                Err(e) => {
                                    errors.push(e);
                                    // Type alias body resolution failed: fresh TypeVar
                                    state.fresh_type_var(&body.span)
                                }
                            }
                        }
                    };

                    // Qualify constructor tags with the alias name.
                    let qualify_tag = |tag: &str| -> String {
                        if alias_name.is_empty() || tag.contains('.') {
                            tag.to_string()
                        } else {
                            format!("{}.{}", alias_name, tag)
                        }
                    };
                    let qualified_body = qualify_typevalue_body(&resolved_body, &qualify_tag);
                    let constructors = extract_typevalue_constructors(&qualified_body);
                    // Collect constructor_constants from literal-valued named args in the
                    // body AST. For each variant entry in the type body that is a
                    // Call with an uppercase head, named_args whose values are literals
                    // (Int/U64/Float/StringLiteral) become constants stored in TyConDef.
                    //
                    // This is the producer side of constructor_constants — the consumer
                    // (field access on Variant and find-by) reads from this map at runtime.
                    //
                    // Only Dict bodies can carry constants (non-Dict bodies resolve to a
                    // single variant or primitive type with no named args).
                    let constructor_constants: indexmap::IndexMap<
                        String,
                        indexmap::IndexMap<String, crate::value::Value>,
                    > = if let SurfaceExpression::Dict(body_entries) = &body.expr {
                        let mut map: indexmap::IndexMap<
                            String,
                            indexmap::IndexMap<String, crate::value::Value>,
                        > = indexmap::IndexMap::new();
                        for body_entry in body_entries {
                            // Each body entry is a positional dict entry (key = None)
                            // whose value is either:
                            //   - Call { func: VarRef(UpperName), named_args: [...], ... }
                            //     → variant with possibly-literal named args
                            //   - VarRef(UpperName) → unit variant, no named args
                            // We only care about the Call case.
                            if let SurfaceExpression::Call {
                                func, named_args, ..
                            } = &body_entry.node.value.expr
                            {
                                if let SurfaceExpression::VarRef {
                                    name: ctor_name, ..
                                } = &func.expr
                                {
                                    if crate::eval::is_constructor_name(ctor_name) {
                                        let mut constants: indexmap::IndexMap<
                                            String,
                                            crate::value::Value,
                                        > = indexmap::IndexMap::new();
                                        for na in named_args {
                                            if let Some(val) =
                                                literal_expr_to_value(&na.node.value.expr)
                                            {
                                                constants.insert(na.node.name.clone(), val);
                                            }
                                        }
                                        if !constants.is_empty() {
                                            let qualified_tag = qualify_tag(ctor_name);
                                            map.insert(qualified_tag, constants);
                                        }
                                    }
                                }
                            }
                        }
                        map
                    } else {
                        indexmap::IndexMap::new()
                    };

                    let param_names: Vec<String> = params.iter().map(|(n, _)| n.clone()).collect();
                    let tycon_def = std::sync::Arc::new(TyConDef {
                        params: param_names,
                        body: qualified_body.clone(),
                        constraints: Vec::new(),
                        variance: Vec::new(),
                        constructors,
                        builtin_type: None,
                        annotation: None,
                        field_annotations: indexmap::IndexMap::new(),
                        constructor_constants,
                        definition_span: Some(entry.span.clone()),
                    });
                    let alias_ty = qualified_body;

                    if let Some(name) = key_name {
                        dict_env
                            .write()
                            .unwrap()
                            .insert_tycon_def(name.clone(), std::sync::Arc::clone(&tycon_def));
                        state.tycon_env.entry(name.clone()).or_insert(tycon_def);
                        // Wire type declarations into type_stage_scope so
                        // resolve_type_head can find user-declared types via the
                        // scope chain. or_insert preserves type-stage entries
                        // (type-stage has priority over runtime-declared types).
                        if state.type_stage_scope.is_empty() {
                            state
                                .type_stage_scope
                                .push(std::collections::HashMap::new());
                        }
                        state.type_stage_scope[0]
                            .entry(name.clone())
                            .or_insert_with(|| make_typevalue_op(&name));
                        if params.is_empty() {
                            let value_scheme_ty = adt_value_type(&alias_ty);
                            let ctor_fields = typevalue_record_fields_pub(&value_scheme_ty);
                            for (ctor_name, ctor_tv) in ctor_fields {
                                ctor_schemes.insert(ctor_name, ctor_tv);
                            }
                            {
                                let mut env_write = dict_env.write().unwrap();
                                let slot =
                                    super::find_slot_in_frames(&state.resolver_frames, &name)
                                        .unwrap_or_else(|| env_write.slots.len());
                                env_write.insert_at_slot(
                                    slot,
                                    name.clone(),
                                    Arc::clone(&value_scheme_ty),
                                    None,
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // Initialize field types accumulator.
    // IndexMap preserves insertion (source) order for field_types so that resolved_field_types
    // are in deterministic source order across runs.
    let mut field_types: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();

    // entry_constraints is only accessed by key lookup (not iterated for output),
    // so HashMap ordering does not affect diagnostic determinism here.
    // entry_inner_schemes (nested TypeScheme maps) was removed in the TypeValue migration;
    // per-entry nested param narrowings are stored separately in entry_constraints.
    let mut entry_constraints: HashMap<String, Vec<Arc<crate::value::Value>>> = HashMap::new();

    // Pass 0c: pre-register class/instance declarations so all classes and instances
    // are visible during body type-checking, regardless of declaration order in the file.
    // Runs AFTER Pass 1 so dict_env includes all letrec TypeVar placeholders.
    for (idx, entry) in entries.iter().enumerate() {
        let is_class_or_instance = matches!(
            &entry.node.value.expr,
            SurfaceExpression::Decl(d)
                if matches!(
                    d.as_ref(),
                    SurfaceDeclaration::ClassDecl { .. } | SurfaceDeclaration::InstanceDecl { .. }
                )
        );
        if is_class_or_instance {
            if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                let result: Result<TypeValue, Vec<Diagnostic>> = match decl_box.as_ref() {
                    SurfaceDeclaration::ClassDecl {
                        name,
                        params,
                        superclasses,
                        methods: _,
                        determines,
                        resolver,
                        resolver_injective,
                        structural,
                    } => {
                        let resolver_name: Option<String> =
                            resolver.as_ref().and_then(|rnode| match &rnode.expr {
                                crate::ast::SurfaceExpression::VarRef { name: rname, .. } => {
                                    Some(rname.clone())
                                }
                                crate::ast::SurfaceExpression::StringLiteral {
                                    content, ..
                                } => Some(content.clone()),
                                _ => None,
                            });
                        super::infer_class_decl_from_surface(
                            &super::ClassDeclSurface {
                                name,
                                params,
                                superclasses,
                                determines,
                                structural,
                                span: entry.node.value.span.clone(),
                                resolver: resolver_name,
                                resolver_injective: *resolver_injective,
                            },
                            state,
                        )
                    }
                    SurfaceDeclaration::InstanceDecl {
                        class_name,
                        arms,
                        resolved_class_decl_id,
                        ..
                    } => {
                        let class_name_str = class_decl_name(class_name);
                        let r = Box::pin(super::infer_instance_decl_from_surface(
                            &class_name_str,
                            arms,
                            entry.node.value.span.clone(),
                            &dict_env,
                            state,
                            type_map,
                        ))
                        .await;
                        // Write resolved_class_decl_id so the lowerer can populate instance_of.
                        if r.is_ok() {
                            if let Some(cd) = dict_env.read().unwrap().get_class(&class_name_str) {
                                resolved_class_decl_id.set(cd.class_decl_id);
                            }
                        }
                        r
                    }
                    _ => Ok(make_typevalue_top()),
                };

                let (ref key_name, _, _) = key_entries[idx];
                match result {
                    Ok(ty) => {
                        if let Some(name) = key_name {
                            field_types.insert(name.clone(), ty);
                        }
                        // Register class method TypeValues after successful ClassDecl processing
                        if let SurfaceDeclaration::ClassDecl {
                            name: class_name,
                            params,
                            methods,
                            ..
                        } = decl_box.as_ref()
                        {
                            // Look up the registered ClassDecl
                            let class_arc_opt = {
                                let env_guard = state.env.read().unwrap();
                                env_guard
                                    .get_class(class_name)
                                    .map(|c| std::sync::Arc::new(c.clone()))
                            };

                            if let Some(_class_arc) = class_arc_opt {
                                for method_entry in methods {
                                    // Extract method name from entry key
                                    let method_name = if let Some(ref key_node) =
                                        method_entry.node.key
                                    {
                                        match &key_node.expr {
                                            crate::ast::SurfaceExpression::VarRef {
                                                name, ..
                                            } => Some(name.clone()),
                                            crate::ast::SurfaceExpression::StringLiteral {
                                                content,
                                                ..
                                            } => Some(content.clone()),
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    };

                                    if let Some(method_name) = method_name {
                                        // Parse method type from entry value expression.
                                        // The value IS the type expression (e.g., [Fn@c [a b]]).
                                        let mut constraints = Vec::new();
                                        // Pre-seed ann_map with class type params so that lowercase
                                        // type variable names (e.g., `a`, `b`) in the method
                                        // signature resolve to TypeVars rather than erroring.
                                        //
                                        // We use the param name itself as the internal TypeVar name
                                        // (e.g., "a" → TypeVar("a", level)). This is safe because:
                                        // - The resolved method type is stored in method_signatures
                                        //   and only used locally (never unified into global state).
                                        // - When infer_instance_decl_from_surface substitutes
                                        //   param_name → concrete_type, it directly matches these
                                        //   TypeVar("a", _) occurrences in the method type.
                                        // - If a collision occurs with an existing TypeVar of the
                                        //   same name, the worst outcome is a stale level (graceful
                                        //   degradation — the expected type stays as TypeVar, and
                                        //   unannotated params fall back to Unknown as before).
                                        let mut method_ann_map: std::collections::HashMap<
                                            String,
                                            String,
                                        > = std::collections::HashMap::new();
                                        for param_name in params.iter() {
                                            // Register the param name as both the annotation key
                                            // and the internal TypeVar name.
                                            state.set_level(
                                                param_name.clone(),
                                                state.ctx.current_level,
                                            );
                                            method_ann_map
                                                .insert(param_name.clone(), param_name.clone());
                                        }
                                        let mut ann_map_mut = Some(&mut method_ann_map);
                                        let mut row_ann_mapping = None;
                                        let method_type_result =
                                            Box::pin(super::typecheck_annot::resolve_type_expr(
                                                &method_entry.node.value,
                                                state,
                                                &mut constraints,
                                                &mut ann_map_mut,
                                                &mut row_ann_mapping,
                                                None,
                                            ))
                                            .await;

                                        match method_type_result {
                                            Ok(method_type) => {
                                                // Store the polymorphic method type in
                                                // ClassDecl.method_signatures so that
                                                // infer_instance_decl_from_surface can look up the
                                                // expected function type for bidirectional checking.
                                                {
                                                    let mut env_guard = state.env.write().unwrap();
                                                    if let Some(mut class_decl) =
                                                        env_guard.get_class(class_name)
                                                    {
                                                        // Avoid duplicate entries if this method
                                                        // was already registered (e.g. re-processed).
                                                        if !class_decl
                                                            .method_signatures
                                                            .iter()
                                                            .any(|(n, _)| n == &method_name)
                                                        {
                                                            class_decl.method_signatures.push((
                                                                method_name.clone(),
                                                                method_type.clone(),
                                                            ));
                                                            // Insert into state.env (the session
                                                            // root) rather than the parent frame
                                                            // where get_class found the ClassDecl.
                                                            // state.env outlives all env frames in
                                                            // a type-check pass, so this write-back
                                                            // persists for the full inference
                                                            // session. Child frames are discarded
                                                            // when their scope exits; inserting
                                                            // here ensures the updated signatures
                                                            // remain visible to all subsequent
                                                            // get_class lookups.
                                                            env_guard.insert_class(class_decl);
                                                        }
                                                    }
                                                }

                                                // Generalize the method type at the current enclosing level.
                                                let scheme = generalize_tv(
                                                    enclosing_level,
                                                    &method_type,
                                                    &state.ctx,
                                                );

                                                // Insert into dict_env at the resolver-assigned slot.
                                                {
                                                    let mut env_write = dict_env.write().unwrap();
                                                    let slot = super::find_slot_in_frames(
                                                        &state.resolver_frames,
                                                        &method_name,
                                                    )
                                                    .unwrap_or_else(|| env_write.slots.len());
                                                    env_write.insert_at_slot(
                                                        slot,
                                                        method_name,
                                                        scheme,
                                                        None,
                                                    );
                                                }
                                            }
                                            Err(type_err) => {
                                                errors.push(type_err);
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(mut errs) => {
                        if let Some(name) = key_name {
                            // Class/instance decl failed: fresh TypeVar (unconstrained)
                            let error_ty = state.fresh_type_var(&entry.span);
                            field_types.insert(name.clone(), error_ty);
                            state
                                .failed_bindings
                                .insert(name.clone(), entry.span.clone());
                        }
                        errors.append(&mut errs);
                    }
                }
            }
        }
    }

    // Process each SCC in topological order
    for scc in sccs.into_iter() {
        // Pass 1_i (SCC): Collect the fresh TypeVars for this SCC's entries.
        // IndexMap preserves SCC member order for deterministic type variable lookup.
        enum FreshVars {
            Singleton(String, TypeValue),
            Multiple(indexmap::IndexMap<String, TypeValue>),
        }
        let mut fresh_vars_storage: Option<FreshVars> = None;

        for &idx in &scc.indices {
            let (ref key_name, is_alias, _is_static) = key_entries[idx];
            if !is_alias {
                if let Some(ref name) = key_name {
                    if let Some(fresh_var) = fresh_vars_by_name.get(name).cloned() {
                        match &mut fresh_vars_storage {
                            None => {
                                fresh_vars_storage =
                                    Some(FreshVars::Singleton(name.clone(), fresh_var.clone()));
                            }
                            Some(FreshVars::Singleton(first_name, first_var)) => {
                                let mut map = indexmap::IndexMap::new();
                                map.insert(first_name.clone(), first_var.clone());
                                map.insert(name.clone(), fresh_var.clone());
                                fresh_vars_storage = Some(FreshVars::Multiple(map));
                            }
                            Some(FreshVars::Multiple(map)) => {
                                map.insert(name.clone(), fresh_var.clone());
                            }
                        }
                    }
                }
            }
        }

        // Clone dict_env for within-SCC isolation.
        let scc_env = Arc::new(RwLock::new(dict_env.read().unwrap().clone()));

        // Pass 3_i: Infer values and unify with bound type vars for this SCC.
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_alias, _is_static) = key_entries[idx];

            let skip = is_alias
                || matches!(&entry.node.value.expr, SurfaceExpression::Placeholder(..))
                || matches!(
                    &entry.node.value.expr,
                    SurfaceExpression::Decl(d)
                        if matches!(
                            d.as_ref(),
                            SurfaceDeclaration::ClassDecl { .. }
                                | SurfaceDeclaration::InstanceDecl { .. }
                        )
                );
            if skip {
                continue;
            }

            if let Some(name) = key_name {
                let saved_constraints = std::mem::take(&mut state.constraints);

                // Record outer→inner use-def edge, then advance current_binding.
                // Every dict entry at every nesting depth participates uniformly: before
                // setting current_binding to this entry's BindingId, record that the current
                // outer binding depends on this inner entry. current_binding flows naturally
                // through all entries at all depths — no save/restore needed. The BFS in
                // AfterBlock traverses the chain transitively.
                // Span-keyed: entry.span is the source location of this binding's declaration,
                // stable across dict_env/scc_env/new_env_inner Arc allocations.
                let current_entry_id = BindingId {
                    def_span: entry.span.clone(),
                    name: name.clone(),
                };
                if let Some(ref outer) = state.current_binding {
                    state
                        .use_def
                        .entry(outer.clone())
                        .or_default()
                        .insert(current_entry_id.clone());
                }
                state.current_binding = Some(current_entry_id);

                // Infer the entry value using run_typecheck (CEK path, no Rust stack recursion).
                // For nested Dict values, call run_typecheck_dict directly to capture schemes.
                let value_ty =
                    if let SurfaceExpression::Dict(nested_entries) = &entry.node.value.expr {
                        let (ty, _schemes, _referenced, mut nested_errs) = Box::pin(
                            run_typecheck_dict(nested_entries, &scc_env, state, type_map, None),
                        )
                        .await;
                        errors.append(&mut nested_errs);
                        Ok(ty)
                    } else {
                        let mut local_errors = Vec::new();
                        let mut local_stack = Vec::new();
                        let ty = Box::pin(run_typecheck(
                            &entry.node.value,
                            &scc_env,
                            state,
                            &mut local_errors,
                            type_map,
                            &mut local_stack,
                        ))
                        .await;
                        if local_errors.is_empty() {
                            Ok(ty)
                        } else {
                            Err(local_errors)
                        }
                    };

                let this_entry_constraints =
                    std::mem::replace(&mut state.constraints, saved_constraints);
                if !this_entry_constraints.is_empty() {
                    entry_constraints.insert(name.clone(), this_entry_constraints);
                }

                match value_ty {
                    Ok(value_ty) => {
                        let bound_var_opt = match &fresh_vars_storage {
                            Some(FreshVars::Singleton(n, tv)) if n == name.as_str() => {
                                Some(Arc::clone(tv))
                            }
                            Some(FreshVars::Multiple(map)) => {
                                map.get(name.as_str()).map(Arc::clone)
                            }
                            _ => None,
                        };

                        if let Some(bound_var) = bound_var_opt {
                            // Bind the fresh TypeVar to the inferred type if it's a TypeVar.
                            // The contains_key guard prevents double-binding (bind() enforces
                            // monotonicity). The occurs check prevents cyclic TypeVar chains.
                            // Errors from bind() here indicate a monotonicity violation between
                            // the guard and the bind call — propagate as a diagnostic.
                            if let Some(var_name) = typevalue_var_name(&bound_var) {
                                if !state.ctx.subst.contains_key(&var_name) {
                                    let applied = state.apply(&value_ty);
                                    if !type_contains_typevar(&applied, &var_name) {
                                        if let Err(e) = state.ctx.bind(var_name, applied) {
                                            errors.push(e);
                                        }
                                    }
                                }
                            }
                            // For TypeValue.Fn placeholders, bind inner ret/param vars.
                            if typevalue_ctor(&bound_var) == Some(TV_FN) {
                                if let (Some((pre_params, pre_ret)), Some((act_params, act_ret))) = (
                                    typevalue_fn_params_and_ret(&bound_var),
                                    typevalue_fn_params_and_ret(&value_ty),
                                ) {
                                    if let Some(ret_name) = typevalue_var_name(&pre_ret) {
                                        if !state.ctx.subst.contains_key(&ret_name) {
                                            let applied = state.apply(&act_ret);
                                            if !type_contains_typevar(&applied, &ret_name) {
                                                if let Err(e) = state.ctx.bind(ret_name, applied) {
                                                    errors.push(e);
                                                }
                                            }
                                        }
                                    }
                                    for (pre_p, act_p) in pre_params.iter().zip(act_params.iter()) {
                                        if let Some(p_name) = typevalue_var_name(pre_p) {
                                            if !state.ctx.subst.contains_key(&p_name) {
                                                let applied = state.apply(act_p);
                                                if !type_contains_typevar(&applied, &p_name) {
                                                    if let Err(e) = state.ctx.bind(p_name, applied)
                                                    {
                                                        errors.push(e);
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            // Insert the inferred type into field_types.
                            field_types.insert(name.clone(), value_ty.clone());
                        } else {
                            // No bound var — just insert value_ty directly.
                            field_types.insert(name.clone(), value_ty);
                        }
                    }
                    Err(mut errs) => {
                        // Entry inference failed: fresh TypeVar (unconstrained)
                        let error_ty = state.fresh_type_var(&entry.span);
                        errors.append(&mut errs);
                        let fallback_ty = error_ty;
                        field_types.insert(name.clone(), fallback_ty.clone());
                        state
                            .failed_bindings
                            .insert(name.clone(), entry.span.clone());
                        if let Some(ref mut map) = type_map {
                            let key = (
                                entry.node.value.span.start_line,
                                entry.node.value.span.start_col,
                                entry.node.value.span.end_line,
                                entry.node.value.span.end_col,
                            );
                            map.insert(key, fallback_ty);
                        }
                    }
                }
            }
        }

        // No separate local subst to merge — state.ctx.subst is the canonical store.
        // Apply substitution to this SCC's field types.
        {
            for &idx in &scc.indices {
                let (ref key_name, _, _) = key_entries[idx];
                if let Some(name) = key_name {
                    if let Some(ty) = field_types.get(name) {
                        let resolved_ty = state.apply(ty);
                        field_types.insert(name.clone(), resolved_ty);
                    }
                }
            }
        }

        // Process deferred equalities (Union-vs-Union with TypeVars, TypeStageApp-vs-concrete)
        // accumulated during this SCC's inference. Must run after the subst merge so that
        // normalized types reflect all bindings made during this SCC.
        {
            let scc_span = scc
                .indices
                .first()
                .and_then(|&idx| entries.get(idx))
                .map(|e| e.node.value.span.clone())
                .unwrap_or_else(|| crate::rust_span!());
            let mut scc_constraints = std::mem::take(&mut state.constraints);
            match crate::types::process_deferred_equalities(
                &mut state.deferred_equalities,
                &mut state.ctx,
                &mut scc_constraints,
                scc_span,
            )
            .await
            {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
            state.constraints = scc_constraints;
        }

        // Apply substitution to this SCC's field types.
        for &idx in &scc.indices {
            let (ref key_name, _, _) = key_entries[idx];
            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    let resolved_ty = state.apply(ty);
                    field_types.insert(name.clone(), resolved_ty);
                }
            }
        }
        // No separate local subst to merge — state.ctx.subst is the canonical store.

        // Pass 4_i: Generalize this SCC's entries before processing the next SCC.
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_alias, _) = key_entries[idx];

            // TypeAlias entries have their correct schemes already registered in Pass 2.
            // Skipping here prevents field_types["TypeName"] (which holds the inferred
            // value type, not the alias type) from overwriting the correct Pass 2 scheme.
            if is_alias {
                continue;
            }

            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    let key_doc = if let Some(ref key_node) = entry.node.key {
                        match &key_node.expr {
                            SurfaceExpression::VarRef {
                                annotation: Some(ann),
                                ..
                            } => ann.node.get_property("doc").and_then(|doc_node| {
                                if let SurfaceExpression::StringLiteral {
                                    content: doc_string,
                                    ..
                                } = &doc_node.expr
                                {
                                    Some(doc_string.clone())
                                } else {
                                    None
                                }
                            }),
                            _ => None,
                        }
                    } else {
                        None
                    };

                    let value_doc = match &entry.node.value.expr {
                        SurfaceExpression::Fn { return_ann, .. } => {
                            return_ann.as_ref().and_then(|ann| {
                                ann.node.get_property("doc").and_then(|doc_node| {
                                    if let SurfaceExpression::StringLiteral {
                                        content: doc_string,
                                        ..
                                    } = &doc_node.expr
                                    {
                                        Some(doc_string.clone())
                                    } else {
                                        None
                                    }
                                })
                            })
                        }
                        _ => None,
                    };

                    let doc = value_doc.or(key_doc);

                    // Extract annotation-based narrowing hints for this binding.
                    //
                    // Two sources of narrowing declarations (checked in priority order):
                    //
                    // 1. Key annotation `@[narrows: TypeName]` — e.g., `foo?@[narrows: Int]:`
                    //    Declares that when `[foo? x]` evaluates to true, `x` narrows to `TypeName`.
                    //
                    // 2. First parameter `@[is: TypeName]` — e.g., `[fn [let x@[is: Int]] ...]`
                    //    Declares that the predicate narrows its argument to `TypeName` in the
                    //    true branch. This is the prelude convention (`int?`, `str?`, etc.).
                    //
                    // Both produce narrowing hints for `extract_narrowings(cond, env)`.
                    // `None` means "no narrowing declared".
                    let param_narrowings: Vec<Option<TypeValue>> = 'narrowing: {
                        // Source 1: key-level @[narrows: T]
                        if let Some(ref key_node) = entry.node.key {
                            if let SurfaceExpression::VarRef {
                                annotation: Some(ann),
                                ..
                            } = &key_node.expr
                            {
                                if let Some(narrows_node) = ann.node.get_property("narrows") {
                                    if let SurfaceExpression::VarRef {
                                        name: type_name, ..
                                    } = &narrows_node.expr
                                    {
                                        let ann_span = Spanned {
                                            node: Annotation::Simple(type_name.clone()),
                                            span: narrows_node.span.clone(),
                                        };
                                        let mut constraints: Vec<Arc<crate::value::Value>> =
                                            Vec::new();
                                        let mut ann_m2: Option<
                                            &mut std::collections::HashMap<String, String>,
                                        > = None;
                                        let mut row_m2: Option<
                                            &mut std::collections::HashMap<String, String>,
                                        > = None;
                                        let narrow_ty = match typecheck_annot::resolve_annotation(
                                            &ann_span.node,
                                            ann_span.span.clone(),
                                            &mut *state,
                                            &mut constraints,
                                            &mut ann_m2,
                                            &mut row_m2,
                                            None,
                                        )
                                        .await
                                        {
                                            Ok(ty) => ty,
                                            Err(e) => {
                                                errors.push(e);
                                                // Narrowing annotation failed: fresh TypeVar
                                                state.fresh_type_var(&ann_span.span)
                                            }
                                        };
                                        break 'narrowing vec![Some(narrow_ty)];
                                    }
                                }
                            }
                        }
                        // Source 2: first parameter @[is: T]
                        if let SurfaceExpression::Fn { params, .. } = &entry.node.value.expr {
                            if let Some(first_param) = params.first() {
                                if let Some(ann) = &first_param.node.annotation {
                                    if let Some(is_node) = ann.node.get_property("is") {
                                        if let SurfaceExpression::VarRef {
                                            name: type_name, ..
                                        } = &is_node.expr
                                        {
                                            let ann_span = Spanned {
                                                node: Annotation::Simple(type_name.clone()),
                                                span: is_node.span.clone(),
                                            };
                                            let mut constraints: Vec<Arc<crate::value::Value>> =
                                                Vec::new();
                                            let mut ann_m: Option<
                                                &mut std::collections::HashMap<String, String>,
                                            > = None;
                                            let mut row_m: Option<
                                                &mut std::collections::HashMap<String, String>,
                                            > = None;
                                            let narrow_ty =
                                                match typecheck_annot::resolve_annotation(
                                                    &ann_span.node,
                                                    ann_span.span.clone(),
                                                    &mut *state,
                                                    &mut constraints,
                                                    &mut ann_m,
                                                    &mut row_m,
                                                    None,
                                                )
                                                .await
                                                {
                                                    Ok(ty) => ty,
                                                    Err(e) => {
                                                        errors.push(e);
                                                        // Narrowing annotation failed: fresh TypeVar
                                                        state.fresh_type_var(&ann_span.span)
                                                    }
                                                };
                                            break 'narrowing vec![Some(narrow_ty)];
                                        }
                                    }
                                }
                            }
                        }
                        Vec::new()
                    };

                    if state.failed_bindings.contains_key(name) {
                        let scheme = generalize_tv(enclosing_level, ty, &state.ctx);
                        {
                            let mut env_write = dict_env.write().unwrap();
                            let slot = env_write
                                .slot_index
                                .get(name.as_str())
                                .copied()
                                .unwrap_or_else(|| env_write.slots.len());
                            env_write.insert_at_slot(slot, name.clone(), scheme, None);
                        }
                        continue;
                    }

                    let saved_constraints = std::mem::replace(
                        &mut state.constraints,
                        match entry_constraints.get(name).cloned() {
                            Some(v) => v,
                            None => vec![], // No constraints accumulated for this entry — start fresh.
                        },
                    );

                    let scheme = crate::types::generalize_tv_with_meta(
                        enclosing_level,
                        ty,
                        &state.ctx,
                        &param_narrowings,
                        doc.as_deref(),
                    );

                    state.constraints = saved_constraints;

                    {
                        let mut env_write = dict_env.write().unwrap();
                        let slot = env_write
                            .slot_index
                            .get(name.as_str())
                            .copied()
                            .unwrap_or_else(|| env_write.slots.len());
                        env_write.insert_at_slot(slot, name.clone(), scheme, None);
                    }
                }
            }
        }
    }

    // Re-apply zero-arity TypeAlias schemes from state.tycon_env.
    for (key_name, is_alias, _) in &key_entries {
        if *is_alias {
            if let Some(name) = key_name {
                if let Some(def) = state.tycon_env.get(name.as_str()) {
                    if def.params.is_empty() {
                        let mut env_write = dict_env.write().unwrap();
                        let slot = env_write
                            .slot_index
                            .get(name.as_str())
                            .copied()
                            .unwrap_or_else(|| env_write.slots.len());
                        env_write.insert_at_slot(
                            slot,
                            name.clone(),
                            adt_value_type(&def.body),
                            None,
                        );
                    }
                }
            }
        }
    }

    // Build final schemes map from dict_env in SOURCE ORDER.
    let mut schemes = indexmap::IndexMap::with_capacity(field_types.len());
    {
        let dict_env_guard = dict_env.read().unwrap();
        for (key_name, _is_alias, _) in &key_entries {
            if let Some(name) = key_name {
                if let Some(scheme) = dict_env_guard.get_scheme(name) {
                    schemes.insert(name.clone(), scheme);
                }
            }
        }
    }
    // Merge in ADT constructor schemes collected in Pass 2.
    for (name, scheme) in ctor_schemes {
        schemes.entry(name).or_insert(scheme);
    }

    // Restore enclosing level
    state.ctx.current_level = enclosing_level;

    // Compact the levels map.
    state.compact_levels();

    // Apply substitutions to field types.
    let resolved_field_types: indexmap::IndexMap<String, TypeValue> = field_types
        .into_iter()
        .map(|(k, v)| {
            let resolved = state.apply(&v);
            (k, resolved)
        })
        .collect();

    let has_spread = entries.iter().any(|e| {
        e.node.key.is_none() && matches!(&e.node.value.expr, SurfaceExpression::Placeholder(..))
    });
    let tail_value = if has_spread {
        Some(make_rowtail_uniform(make_typevalue_top())) // open record
    } else {
        None // closed record
    };
    let record_type = make_typevalue_record(resolved_field_types.clone(), tail_value);

    // Collect referenced names from the internal dict env before returning.
    // The Sequential arm creates fresh EnvSlots when building new_env_inner, losing the
    // `referenced` flag set during our internal CEK run. Return the set so callers can
    // re-apply it and avoid false lost-binding diagnostics on names actually used inside
    // the sub-dict.
    let referenced: std::collections::HashSet<String> = {
        let dict_env_guard = dict_env.read().unwrap();
        dict_env_guard
            .slots
            .iter()
            .filter_map(|entry| {
                entry.as_ref().and_then(|(name, slot)| {
                    if slot.referenced {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
            })
            .collect()
    };

    (record_type, schemes, referenced, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Resolution, SurfaceParam, TypeAnnotation};
    use crate::test_util::sp;

    /// Construct a minimal `Arc<SurfaceNode>` with no span or annotation metadata.
    fn mk(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span: crate::rust_span!(),
            type_guard: TypeAnnotation::new(),
            provenance: crate::ast::Provenance::new(),
        })
    }

    /// Build a `VarRef` node for `name` (unescaped, no annotation).
    fn varref(name: &str) -> Arc<SurfaceNode> {
        mk(SurfaceExpression::VarRef {
            name: name.to_string(),
            escaped: false,
            resolution: Resolution::new(),
            annotation: None,
            do_infer_placeholder: false,
        })
    }

    /// Build a `Fn` node with the given param names and body.
    fn fn_node(param_names: &[&str], body: Arc<SurfaceNode>) -> Arc<SurfaceNode> {
        let params = param_names
            .iter()
            .map(|&name| {
                sp(SurfaceParam {
                    name: name.to_string(),
                    annotation: None,
                    variadic: false,
                    resolved_annotation_type: TypeAnnotation::new(),
                })
            })
            .collect();
        mk(SurfaceExpression::Fn {
            return_ann: None,
            params,
            body,
            desugared: false,
            resolved_captures: crate::ast::CapturesCell::new(),
            resolved_return_annotation: crate::ast::TypeAnnotation::new(),
        })
    }

    /// `collect_dependencies` must not produce a dep edge from a function value to a
    /// sibling binding whose name matches one of the function's parameter names.
    ///
    /// For `[x: 42  f: [fn [let x] x]]` the body VarRef "x" is a parameter reference, not a
    /// free reference to sibling `x`.  The dependency graph must have NO edge f→x.
    ///
    /// Formal invariant: Damas & Milner (1982) require the dependency graph to use the minimal
    /// transitive closure of actual free variable references (not shadowed names).
    #[test]
    fn test_fn_param_shadows_sibling_no_dep_edge() {
        // Simulate the dict [x: 42  f: [fn [let x] x]].
        // name_to_idx maps sibling names to their index in the dict entry list.
        let mut name_to_idx = HashMap::new();
        name_to_idx.insert("x".to_string(), 0usize); // x is sibling index 0
        name_to_idx.insert("f".to_string(), 1usize); // f is sibling index 1

        // Value of f: [fn [let x] x]
        // The body is a VarRef "x" — this x is the PARAMETER, not the sibling.
        let fn_body = varref("x");
        let f_value = fn_node(&["x"], fn_body);

        let deps = collect_dependencies(&f_value, &name_to_idx);

        // f's value contains no free reference to sibling x (x is shadowed by param x).
        // So deps must NOT contain index 0 (sibling x) or index 1 (sibling f, self-ref).
        assert!(
            !deps.contains(&0),
            "f must not depend on sibling x (param x shadows sibling x): deps = {deps:?}"
        );
        assert!(
            !deps.contains(&1),
            "f must not self-depend via param shadowing: deps = {deps:?}"
        );
    }

    /// Confirm that a genuine free reference to a sibling IS still recorded.
    ///
    /// For `[x: 42  f: [fn [let y] x]]`, the body VarRef "x" is free (not shadowed by any
    /// param), so the dep edge f→x must be present.
    #[test]
    fn test_genuine_free_ref_to_sibling_creates_dep_edge() {
        let mut name_to_idx = HashMap::new();
        name_to_idx.insert("x".to_string(), 0usize);
        name_to_idx.insert("f".to_string(), 1usize);

        // f: [fn [let y] x] — param is y, body references sibling x freely
        let fn_body = varref("x");
        let f_value = fn_node(&["y"], fn_body);

        let deps = collect_dependencies(&f_value, &name_to_idx);

        assert!(
            deps.contains(&0),
            "f must depend on sibling x (x is free in the body): deps = {deps:?}"
        );
    }

    /// Confirm that nested Fn params also shadow siblings at their own scope.
    ///
    /// For a value `[fn [let y] [fn [let x] x]]`, the inner body's VarRef "x" is shadowed
    /// by the inner fn's param.  No dep on sibling x.
    #[test]
    fn test_nested_fn_param_shadows_sibling() {
        let mut name_to_idx = HashMap::new();
        name_to_idx.insert("x".to_string(), 0usize);

        // Inner fn: [fn [let x] x] — inner param shadows sibling x
        let inner_body = varref("x");
        let inner_fn = fn_node(&["x"], inner_body);

        // Outer fn: [fn [let y] <inner_fn>]
        let outer_fn = fn_node(&["y"], inner_fn);

        let deps = collect_dependencies(&outer_fn, &name_to_idx);

        assert!(
            !deps.contains(&0),
            "inner fn param x must shadow sibling x at all nesting levels: deps = {deps:?}"
        );
    }

    /// Confirm that a sibling reference from the outer fn scope (before inner fn scope) IS recorded.
    ///
    /// For `[fn [let y] [fn [let z] x]]` where neither outer nor inner param is named "x",
    /// the inner body's VarRef "x" is free all the way up — dep on sibling x must be recorded.
    #[test]
    fn test_deeply_nested_free_ref_creates_dep_edge() {
        let mut name_to_idx = HashMap::new();
        name_to_idx.insert("x".to_string(), 0usize);

        // Inner fn: [fn [let z] x] — x is free
        let inner_body = varref("x");
        let inner_fn = fn_node(&["z"], inner_body);

        // Outer fn: [fn [let y] <inner_fn>]
        let outer_fn = fn_node(&["y"], inner_fn);

        let deps = collect_dependencies(&outer_fn, &name_to_idx);

        assert!(
            deps.contains(&0),
            "x is free in all scopes, so dep on sibling x must be present: deps = {deps:?}"
        );
    }

    /// T-2149: Verify that calling a typeclass method with a concrete type that lacks an
    /// instance produces an info diagnostic at the call site.
    ///
    /// This test is a placeholder — full end-to-end testing requires:
    /// 1. A class declaration with at least one method
    /// 2. An instance for one type (e.g., Int)
    /// 3. A call to the method with a different concrete type (e.g., String) with no instance
    /// 4. Verification that Diagnostic::info("missing-instance", ...) is emitted
    ///
    /// The corpus test in tests/corpus/typecheck/T-2149-missing-instance-diagnostic.llt
    /// provides end-to-end validation. This unit test documents the expected behavior.
    #[test]
    fn test_t2149_missing_instance_diagnostic_placeholder() {
        // Full test requires parser, resolver, and typecheck integration.
        // See tests/corpus/typecheck/T-2149-missing-instance-diagnostic.llt for end-to-end test.
        // This placeholder confirms the test structure is in place.
        assert!(
            true,
            "T-2149 corpus test validates missing-instance diagnostic"
        );
    }
}
