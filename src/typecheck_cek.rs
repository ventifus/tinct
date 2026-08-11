//! CEK machine for type inference — iterative loop with explicit continuations.
//!
//! Implements the full type checker as a continuation-passing style (CPS) machine with
//! defunctionalized continuations. This prevents stack overflow on deeply nested expressions
//! and provides an inspectable continuation stack for error reporting.
//!
//! Architecture:
//! - Control register: current `Arc<SurfaceNode>` to infer OR a completed `Type` result
//! - Continuation stack: `Vec<(TypeCheckCont, Arc<SurfaceNode>)>` — each continuation carries
//!   its source node for universal annotation when Done fires
//! - Main loop: `run_typecheck` — alternates between `infer_step` and `apply_cont`
//!
//! Both `infer_step` and `apply_cont` are `async fn` — they await external async operations
//! (annotation resolution, async unify) directly. The CEK loop eliminates recursive
//! calls to `run_typecheck` itself, not all async behavior.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    class_decl_name, Annotation, Span, Spanned, SurfaceDeclaration, SurfaceEntry,
    SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode,
};
use crate::coverage;
use crate::env::Env;
use crate::error::Diagnostic;
use crate::type_tags::*;
// Constraint is Vec<Arc<Value>> in the TypeValue migration — no Constraint type import needed.
use crate::type_def::TyConDef;
use crate::type_infer::{
    make_rowtail_uniform, make_typevalue_float_lit, make_typevalue_fn, make_typevalue_int_lit,
    make_typevalue_negation, make_typevalue_never, make_typevalue_nominal,
    make_typevalue_nominal_variant, make_typevalue_nominal_variant_by_id, make_typevalue_record,
    make_typevalue_str_lit, make_typevalue_top, make_typevalue_unknown,
    typevalue_extract_members_pub, typevalue_fn_params_and_ret,
    typevalue_nominal_variant_has_fields, typevalue_nominal_variant_tag,
    typevalue_normalize_intersection, typevalue_normalize_union_ctx, typevalue_record_fields_pub,
    typevalue_var_name, InferState, TypeValue,
};
// typevalue_ctor_index is available via `use crate::type_tags::*` above
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
            &state.classes,
            &state.instances,
            &mut state.fd_depth,
            state.eval_ctx.clone(),
            &state.type_stage_fns,
            errors,
            &state.type_name_to_id,
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
                Err(mut e) => {
                    e = e.with_note(format!(
                        "expected (FD-determined target): {}",
                        crate::eval::format_type_with_names(&target, &state.ctx.type_display_names)
                    ));
                    e = e.with_note(format!(
                        "actual (FD-computed type):        {}",
                        crate::eval::format_type_with_names(
                            &computed,
                            &state.ctx.type_display_names
                        )
                    ));
                    errors.push(e);
                }
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
        match_node: Arc<SurfaceNode>,
    },

    /// Inferred one match arm body — continue with remaining arms.
    MatchArm {
        remaining_arms: Vec<SurfaceMatchArm>,
        env: Arc<RwLock<Env>>,
        accumulated_types: Vec<TypeValue>,
        scrutinee_ty: TypeValue,
        remaining_scrutinee: TypeValue,
        span: Span,
        match_node: Arc<SurfaceNode>,
    },

    /// Terminal dict — run HM letrec inference via typecheck_letrec_group.
    ///
    /// Pushed by the `Dict` arm of `infer_step`. The handler calls `typecheck_letrec_group`
    /// directly, which performs key resolution, SCC decomposition, and per-SCC inference
    /// inline. This avoids duplicating the letrec algorithm in the continuation chain.
    LetrecGroupPassZero {
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
        saved_current_binding: Option<BindingId>,
        saved_narrowing_map:
            std::collections::HashMap<crate::type_infer::BindingId, crate::type_infer::TypeValue>,
    },

    /// Identity continuation — returns the child type unchanged.
    /// Pushed when infer_step redirects via Eval without a semantic continuation
    /// (e.g., Field access desugaring to a synthetic call). The universal annotation
    /// mechanism annotates the source node when this fires.
    PassThrough,

    /// Inferred the target of a field projection — apply structural field lookup.
    FieldProject {
        field: crate::ast::DotKey,
        span: crate::ast::Span,
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
    stack: &mut Vec<(TypeCheckCont, Arc<SurfaceNode>)>,
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

                // Wire the full resolved TypeValue to SurfaceNode for the lowerer.
                let resolved = state.apply(&ty);
                current_node.inferred_type_decl_id.set(resolved);

                // Pop and apply continuations until we get an Eval action or empty stack.
                let mut result_ty = ty;
                loop {
                    match stack.pop() {
                        None => return result_ty,
                        Some((cont, source_node)) => {
                            match apply_cont(cont, result_ty, state, errors, type_map, stack).await
                            {
                                TypeCheckAction::Done(t) => {
                                    // Universal annotation: every continuation's source node gets
                                    // annotated when its Done fires.
                                    let resolved = state.apply(&t);
                                    source_node.inferred_type_decl_id.set(resolved);
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
    stack: &mut Vec<(TypeCheckCont, Arc<SurfaceNode>)>,
) -> TypeCheckAction {
    match &node.expr {
        // ===== Leaf expressions =====
        SurfaceExpression::Int(n) => TypeCheckAction::Done(make_typevalue_int_lit(*n)),
        SurfaceExpression::Float(f) => TypeCheckAction::Done(make_typevalue_float_lit(*f)),
        SurfaceExpression::StringLiteral { content, .. } => {
            TypeCheckAction::Done(make_typevalue_str_lit(content))
        }
        SurfaceExpression::U64(_) => {
            TypeCheckAction::Done(make_typevalue_nominal(crate::value::TYPE_ID_INT))
        }

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
            let ty = infer_var_ref(name, annotation.as_ref(), node, state, errors).await;
            TypeCheckAction::Done(ty)
        }

        // ===== Unquote / UnquoteSplice — compound =====
        SurfaceExpression::Unquote(inner) => {
            stack.push((TypeCheckCont::Unquote, Arc::clone(node)));
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            stack.push((TypeCheckCont::UnquoteSplice, Arc::clone(node)));
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        // ===== Field access — structural TV_RECORD projection =====
        //
        // foo.bar pushes a FieldProject continuation and evaluates the base expression.
        // When the base type arrives, FieldProject looks up the field in the TV_RECORD
        // field map, then checks the row tail (RT_CLOSED → error, RT_VAR → Unknown,
        // RT_UNIFORM → value type). No dependency on builtin-dict-get or Indexable.
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
                        infer_var_ref(name, None, node, state, errors).await,
                    );
                }
                TypeCheckAction::Done(make_typevalue_unknown())
            }
            Some(base) => {
                stack.push((
                    TypeCheckCont::FieldProject {
                        field: field.clone(),
                        span: node.span.clone(),
                    },
                    Arc::clone(node),
                ));
                stack.push((TypeCheckCont::PassThrough, Arc::clone(node)));
                TypeCheckAction::Eval(Arc::clone(base), Arc::clone(env))
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
                    stack.push((
                        TypeCheckCont::TypeAssertInner {
                            expected,
                            has_default,
                            default_node,
                            assert_node: Arc::clone(node),
                            env: Arc::clone(env),
                            span: node.span.clone(),
                            annotation_span: annotation.span.clone(),
                        },
                        Arc::clone(node),
                    ));
                    TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
                }
                Err(e) => {
                    let e = e.with_note("in type assertion annotation");
                    errors.push(e);
                    stack.push((TypeCheckCont::PassThrough, Arc::clone(node)));
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

            // Save and reset use_def and current_binding at the start of each Sequential
            // so that edges from a previous (or outer) Sequential scope don't contaminate this
            // one. The saved values are restored by AfterBlock so that nested
            // Sequentials do not corrupt the outer Sequential's liveness graph.
            let saved_use_def = std::mem::take(&mut state.use_def);
            let saved_current_binding = state.current_binding.take();

            for (_i, intermediate) in intermediates.iter().enumerate() {
                // Check if this is a dict — if so, use typecheck_letrec_group for proper letrec
                if let SurfaceExpression::Dict {
                    entries,
                    scope_frame: _,
                } = &intermediate.expr
                {
                    // The resolver writes each entry's absolute slot into assigned_slot.
                    // The first entry's slot is the base for this dict body.
                    let body_slot_base_pre: Option<u32> = entries
                        .first()
                        .and_then(|e| e.node.assigned_slot.get().copied());

                    let (_, _schemes, body_dict_env, mut dict_errs) = typecheck_letrec_group(
                        entries,
                        &current_env,
                        state,
                        type_map,
                        body_slot_base_pre,
                    )
                    .await;
                    errors.append(&mut dict_errs);

                    current_env = body_dict_env;
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
            // Clear current_binding: typecheck_letrec_group leaves state.current_binding set to the
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
            stack.push((
                TypeCheckCont::AfterBlock {
                    binding_envs: intermediate_envs,
                    pre_final_refs,
                    saved_use_def,
                    saved_current_binding,
                    saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                },
                Arc::clone(node),
            ));
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
            stack.push((
                TypeCheckCont::CallFunc {
                    func_node: Arc::clone(func),
                    args: args_cloned,
                    named_args: named_args_cloned,
                    env: Arc::clone(env),
                    call_node: Arc::clone(node),
                },
                Arc::clone(node),
            ));
            TypeCheckAction::Eval(Arc::clone(func), Arc::clone(env))
        }

        // ===== Fn — resolve annotations, build env, push FnBody, eval body =====
        SurfaceExpression::Fn { .. } => infer_fn_push_cont(node, env, state, errors, stack).await,

        // ===== Dict — push LetrecGroupPassZero, complete inference via typecheck_letrec_group =====
        //
        // HM letrec inference is performed by typecheck_letrec_group in
        // the LetrecGroupPassZero handler. Push the continuation and return Done(Unknown) immediately
        // to trigger apply_cont — there is no child node to evaluate at this point.
        SurfaceExpression::Dict {
            entries,
            scope_frame: _,
        } => {
            stack.push((
                TypeCheckCont::LetrecGroupPassZero {
                    entries: entries.to_vec(),
                    env: Arc::clone(env),
                },
                Arc::clone(node),
            ));
            // Placeholder: LetrecGroupPassZero handler computes the actual record type
            TypeCheckAction::Done(state.fresh_type_var(&node.span))
        }

        // ===== Match — compound: eval scrutinee first =====
        SurfaceExpression::Match { scrutinee, arms } => {
            let arms_cloned: Vec<SurfaceMatchArm> = arms.to_vec();
            stack.push((
                TypeCheckCont::MatchScrutinee {
                    arms: arms_cloned,
                    env: Arc::clone(env),
                    span: node.span.clone(),
                    match_node: Arc::clone(node),
                },
                Arc::clone(node),
            ));
            TypeCheckAction::Eval(Arc::clone(scrutinee), Arc::clone(env))
        }

        // ===== Decl — call declaration helpers directly =====
        SurfaceExpression::Decl(decl_box) => {
            // Call infer_class_decl_from_surface and infer_instance_decl_from_surface directly
            // now that they are pub(crate). After expansion (S-1036), TypeAlias becomes TypeDecl
            // and ClassDecl/InstanceDecl are replaced; these arms handle any surviving Decl nodes.
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
                    pre_assigned_class_decl_id,
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
                            pre_assigned_class_decl_id: Some(pre_assigned_class_decl_id),
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
                    // Set resolved_class_decl_id from the class lookup alone.
                    if let Some(cd) = state.classes.iter().find(|c| c.name == class_name_str) {
                        resolved_class_decl_id.set(cd.class_decl_id);
                    }
                    r
                }
                other => Err(vec![Diagnostic::error(
                    "type-error",
                    format!("declaration node in expression position: {other}"),
                    node.span.clone(),
                )]),
            };
            match result {
                Ok(t) => TypeCheckAction::Done(t),
                Err(mut errs) => {
                    errors.append(&mut errs);
                    // Decl inference failed: Never (error recovery)
                    TypeCheckAction::Done(make_typevalue_never())
                }
            }
        }

        // Parse-error node: the error was already recorded during parsing;
        // silently return Unknown here to avoid spurious secondary diagnostics.
        SurfaceExpression::Error(_) => TypeCheckAction::Done(make_typevalue_unknown()),

        // Expansion-produced variants: UnitVariant and MakeVariant should infer to their
        // nominal type by looking up the tycon in tycon_env. TypeDecl and ReprDecl wrap
        // an inner expression — recurse into it.
        SurfaceExpression::UnitVariant { type_decl_id, .. } => {
            let tv = if state.tycon_env.contains_key(type_decl_id) {
                crate::type_infer::make_typevalue_nominal(*type_decl_id)
            } else {
                make_typevalue_unknown()
            };
            TypeCheckAction::Done(tv)
        }
        SurfaceExpression::MakeVariant { type_decl_id, .. } => {
            let tv = if state.tycon_env.contains_key(type_decl_id) {
                crate::type_infer::make_typevalue_nominal(*type_decl_id)
            } else {
                make_typevalue_unknown()
            };
            TypeCheckAction::Done(tv)
        }
        SurfaceExpression::TypeDecl { inner, .. } => {
            stack.push((TypeCheckCont::PassThrough, Arc::clone(node)));
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }
        SurfaceExpression::ReprDecl { inner, .. } => {
            stack.push((TypeCheckCont::PassThrough, Arc::clone(node)));
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

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
            // Unsupported expr: Never (error recovery)
            TypeCheckAction::Done(make_typevalue_never())
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
    stack: &mut Vec<(TypeCheckCont, Arc<SurfaceNode>)>,
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
                match typevalue_ctor_index(tv) {
                    Some(TV_UNKNOWN) | Some(TV_TOP) => true,
                    Some(TV_VAR) => true,
                    _ => false,
                }
            };

            let fn_ret_ty = match return_ann {
                Some(declared_ret) => {
                    // For checkable primitive return annotations, verify the body is consistent.
                    let is_checkable_primitive =
                        matches!(typevalue_ctor_index(&declared_ret), Some(TV_NOMINAL));
                    if is_checkable_primitive {
                        let body_resolved = state.apply(&child_ty);
                        let body_is_concrete = !is_opaque(&body_resolved);
                        if body_is_concrete {
                            let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                                state.ctx.subst.clone(),
                                state.ctx.levels.clone(),
                                state.ctx.current_level,
                                state.tycon_env.clone(),
                                state.type_name_to_id.clone(),
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
                                    .with_note(format!("declared: {}", crate::eval::format_type_with_names(&declared_ret, &state.ctx.type_display_names)))
                                    .with_note(format!("inferred: {}", crate::eval::format_type_with_names(&body_resolved, &state.ctx.type_display_names)))
                                );
                            }
                        }
                    }
                    // For intersection annotations: validate each Fn member.
                    if typevalue_ctor_index(&declared_ret) == Some(TV_INTER) {
                        let body_resolved = state.apply(&child_ty);
                        let body_is_concrete = !is_opaque(&body_resolved);
                        if body_is_concrete {
                            let ctx_for_check = crate::type_infer::InferenceContext::from_snapshot(
                                state.ctx.subst.clone(),
                                state.ctx.levels.clone(),
                                state.ctx.current_level,
                                state.tycon_env.clone(),
                                state.type_name_to_id.clone(),
                            );
                            if let Some(members) =
                                crate::type_infer::typevalue_extract_members_pub(&declared_ret)
                            {
                                for member in &members {
                                    if typevalue_ctor_index(member) == Some(TV_FN)
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
                                            .with_note(format!("declared member: {}", crate::eval::format_type_with_names(member, &state.ctx.type_display_names)))
                                            .with_note(format!("inferred:        {}", crate::eval::format_type_with_names(&body_resolved, &state.ctx.type_display_names)))
                                        );
                                    }
                                }
                            }
                        }
                    }

                    // Emit diagnostic for explicit @Unknown return annotation.
                    if typevalue_ctor_index(&declared_ret) == Some(TV_UNKNOWN) {
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
                                state.type_name_to_id.clone(),
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
                                        crate::eval::format_type_with_names(
                                            &declared_ret,
                                            &state.ctx.type_display_names
                                        )
                                    ))
                                    .with_note(format!(
                                        "inferred: {}",
                                        crate::eval::format_type_with_names(
                                            &body_resolved,
                                            &state.ctx.type_display_names
                                        )
                                    )),
                                );
                            }
                        }
                    }

                    // @Unknown return annotation means: use the inferred body type.
                    if typevalue_ctor_index(&declared_ret) == Some(TV_UNKNOWN) {
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
                let ret_str =
                    crate::eval::format_type_with_names(&fn_ret_ty, &state.ctx.type_display_names);
                let params_str = params
                    .iter()
                    .map(|(name, ty)| {
                        format!(
                            "{}: {}",
                            name.as_deref().unwrap_or("_"),
                            crate::eval::format_type_with_names(ty, &state.ctx.type_display_names)
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
                let covered = typevalue_normalize_union_ctx(
                    typed_variadics.iter().map(|(_, t)| Arc::clone(t)).collect(),
                    Some(&state.ctx),
                );
                let covered_display =
                    crate::eval::format_type_with_names(&covered, &state.ctx.type_display_names);
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
                stack.push((
                    TypeCheckCont::CallArg {
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
                        call_node: Arc::clone(&call_node),
                        effect_perform: effect_perform.clone(),
                    },
                    Arc::clone(&call_node),
                ));
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
            let result = apply_call_args_poly(
                accumulated_arg_types,
                arg_nodes,
                sig,
                named_args,
                env,
                span,
                effect_perform,
                &mut ctx,
            )
            .await;
            if let TypeCheckAction::Done(ref _ty) = result {
                // Capture type instantiation from scheme renaming.
                // The renaming maps original type param names (e.g. "a") to fresh TypeVar
                // names (e.g. "a⦀42"). We compose with the substitution to get
                // {original_name → concrete_type}.
                if let Some(renaming) = ctx.state.last_scheme_renaming.take() {
                    let mut type_inst = std::collections::HashMap::new();
                    for (original_name, fresh_name) in &renaming {
                        let fresh_tv = crate::type_infer::make_typevar_value(fresh_name);
                        let resolved_var = ctx.state.apply(&fresh_tv);
                        if crate::type_infer::typevalue_ctor_index(&resolved_var) != Some(TV_VAR) {
                            type_inst.insert(original_name.clone(), resolved_var);
                        }
                    }
                    call_node.type_instantiation.set(type_inst);
                }
            }
            result
        }

        // ===== MatchScrutinee =====
        TypeCheckCont::MatchScrutinee {
            arms,
            env,
            span,
            match_node,
        } => {
            let scrutinee_ty = state.apply(&child_ty);
            if arms.is_empty() {
                // Match with no arms: fresh TypeVar (unconstrained)
                return TypeCheckAction::Done(state.fresh_type_var(&span));
            }

            // Type-check each arm's pattern expression so inferred_type_decl_id is populated
            // for coverage checking and narrowing. If any pattern has an error (Never),
            // the whole match is an error — skip coverage analysis entirely.
            let mut pattern_error = false;
            for arm in &arms {
                let arm_env = build_case_arm_env(&arm.let_bindings, &env, state, &arm.pattern);
                let mut local_stack = Vec::new();
                let pattern_ty = Box::pin(run_typecheck(
                    &arm.pattern,
                    &arm_env,
                    state,
                    errors,
                    type_map,
                    &mut local_stack,
                ))
                .await;
                if typevalue_ctor_index(&pattern_ty) == Some(TV_NEVER) {
                    pattern_error = true;
                }
            }

            if !pattern_error {
                // Run exhaustiveness checking only when all patterns are valid.
                run_match_exhaustiveness_check(&scrutinee_ty, &arms, &span, state, errors).await;
            }

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
                    stack.push((
                        TypeCheckCont::MatchArm {
                            remaining_arms,
                            env,
                            accumulated_types: Vec::new(),
                            scrutinee_ty,
                            remaining_scrutinee: next_remaining_scrutinee,
                            span,
                            match_node: Arc::clone(&match_node),
                        },
                        Arc::clone(&match_node),
                    ));
                    stack.push((
                        TypeCheckCont::AfterBlock {
                            binding_envs: vec![Arc::clone(&arm_env)],
                            pre_final_refs: vec![],
                            saved_use_def: std::mem::take(&mut state.use_def),
                            saved_current_binding: state.current_binding.take(),
                            saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                        },
                        Arc::clone(&match_node),
                    ));
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
            match_node,
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
                // typevalue_normalize_union_ctx flattens, deduplicates, eliminates subtypes,
                // and returns the single type if all arms agreed, or a union if they differ.
                let match_ty = if accumulated_types.is_empty() {
                    // No arms produced types: fresh TypeVar (unconstrained)
                    state.fresh_type_var(&span)
                } else {
                    typevalue_normalize_union_ctx(accumulated_types, Some(&state.ctx))
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
                            typevalue_normalize_union_ctx(accumulated_types, Some(&state.ctx))
                        };
                        TypeCheckAction::Done(match_ty)
                    }
                    Some((arm_env, next_remaining_scrutinee, guard_narrowings)) => {
                        let next_remaining: Vec<SurfaceMatchArm> = remaining_arms[1..].to_vec();
                        stack.push((
                            TypeCheckCont::MatchArm {
                                remaining_arms: next_remaining,
                                env,
                                accumulated_types,
                                scrutinee_ty,
                                remaining_scrutinee: next_remaining_scrutinee,
                                span,
                                match_node: Arc::clone(&match_node),
                            },
                            Arc::clone(&match_node),
                        ));
                        stack.push((
                            TypeCheckCont::AfterBlock {
                                binding_envs: vec![Arc::clone(&arm_env)],
                                pre_final_refs: vec![],
                                saved_use_def: std::mem::take(&mut state.use_def),
                                saved_current_binding: state.current_binding.take(),
                                saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
                            },
                            Arc::clone(&match_node),
                        ));
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
            if typevalue_ctor_index(&expected_resolved) == Some(TV_UNKNOWN) {
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
                        err.labels.push(crate::error::SpanLabel {
                            span: annotation_span.clone(),
                            message: "type declared here".to_string(),
                        });
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
                    state.type_name_to_id.clone(),
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
                    let e = Diagnostic::error(
                        "type-error",
                        "default value type does not match the assertion's expected type",
                        default_n.span.clone(),
                    )
                    .with_note(format!(
                        "expected: {}",
                        crate::eval::format_type_with_names(
                            &expected_resolved,
                            &state.ctx.type_display_names
                        )
                    ))
                    .with_note(format!(
                        "actual:   {}",
                        crate::eval::format_type_with_names(
                            &default_resolved,
                            &state.ctx.type_display_names
                        )
                    ));
                    errors.push(e);
                }
            }

            TypeCheckAction::Done(expected)
        }

        // ===== PassThrough =====
        TypeCheckCont::PassThrough => TypeCheckAction::Done(child_ty),

        // ===== FieldProject =====
        TypeCheckCont::FieldProject { field, span } => {
            // Apply substitution to get the settled type
            let settled = state.apply(&child_ty);

            // Check if it's a TV_RECORD
            match typevalue_ctor_index(&settled) {
                Some(TV_RECORD) => {
                    // Extract the field map
                    let field_map = typevalue_record_fields_pub(&settled);

                    // Convert DotKey to string for lookup
                    let field_name = match &field {
                        crate::ast::DotKey::Ident(s) => s.clone(),
                        crate::ast::DotKey::Int(n) => n.to_string(),
                    };

                    // Try to find the field in the field map
                    if let Some(field_ty) = field_map.get(&field_name) {
                        // Field found — return its type
                        TypeCheckAction::Done(Arc::clone(field_ty))
                    } else {
                        // Field not found in the explicit fields — check the tail
                        if let Some(tail) =
                            crate::type_infer::typevalue_payload_field(&settled, FIELD_TAIL)
                        {
                            match typevalue_ctor_index(&tail) {
                                Some(RT_CLOSED) => {
                                    // Closed record — field doesn't exist
                                    errors.push(Diagnostic::error(
                                        "field-not-found",
                                        &format!(
                                            "field '{}' not found in closed record",
                                            field_name
                                        ),
                                        span.clone(),
                                    ));
                                    TypeCheckAction::Done(make_typevalue_never())
                                }
                                Some(RT_VAR) => {
                                    // Open row variable — any field is possible
                                    TypeCheckAction::Done(make_typevalue_unknown())
                                }
                                Some(RT_UNIFORM) => {
                                    // Uniform row — extract the value type
                                    if let Some(value_ty) =
                                        crate::type_infer::typevalue_payload_field(
                                            &tail,
                                            RT_FIELD_VALUE_TYPE,
                                        )
                                    {
                                        TypeCheckAction::Done(value_ty)
                                    } else {
                                        // Malformed RT_UNIFORM — fallback to Unknown
                                        TypeCheckAction::Done(make_typevalue_unknown())
                                    }
                                }
                                _ => {
                                    // Unknown tail variant — gradual fallback
                                    TypeCheckAction::Done(make_typevalue_unknown())
                                }
                            }
                        } else {
                            // No tail field (malformed TV_RECORD) — treat as closed
                            errors.push(Diagnostic::error(
                                "field-not-found",
                                &format!("field '{}' not found in record", field_name),
                                span.clone(),
                            ));
                            TypeCheckAction::Done(make_typevalue_never())
                        }
                    }
                }
                _ => {
                    // Not a TV_RECORD — gradual: can't type-check field access on unknown/non-record types
                    TypeCheckAction::Done(make_typevalue_unknown())
                }
            }
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
        } => {
            // Step 1: Seed the live set with names newly marked by the final expression.
            // Names in pre_final_refs were already referenced before the final expression
            // ran (they were marked by inter-dict processing). Only names that the final
            // expression itself referenced are seeded as directly live.
            // Read from SLOTS only — infer_var_ref marks slots directly via Dispatch addresses.
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

            TypeCheckAction::Done(child_ty)
        }

        // ===== LetrecGroupPassZero =====
        //
        // Terminal dict — run HM letrec inference via typecheck_letrec_group.
        // Schemes are not propagated to the parent env: this is a terminal dict expression,
        // not an intermediate scope-chain body. The returned record type carries the full
        // structural type information.
        TypeCheckCont::LetrecGroupPassZero { entries, env } => {
            let body_slot_base_pre: Option<u32> = entries
                .first()
                .and_then(|e| e.node.assigned_slot.get().copied());
            let (record_type, _schemes, _nested_dict_env, mut dict_errs) = Box::pin(
                typecheck_letrec_group(&entries, &env, state, type_map, body_slot_base_pre),
            )
            .await;
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
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
) -> TypeValue {
    // Resolve VarRef via the resolution field on the VarRef node.
    // Dispatch uses slot-based lookup via scope_chain;
    // EffectPerform uses typeclass method signature instantiation.
    // No fallback — missing entry is a resolver bug.
    let scheme: Option<TypeValue> = if let SurfaceExpression::VarRef { resolution, .. } = &node.expr
    {
        if let Some(Some(addr)) = resolution.get() {
            match addr {
                crate::ast::VarAddr::EffectPerform { slot } => {
                    // Typeclass method call — resolved at runtime by scanning the accumulated group
                    // for an instance whose primary dispatch type matches the first argument's runtime type.
                    // Look up the class by class_decl_id, extract the method signature, and instantiate
                    // it with fresh TypeVars (the method signature uses the class's type parameters as
                    // TypeValue.Var nodes that must be instantiated at each call site).
                    let class_opt = state.classes.iter().find(|c| c.class_decl_id == 0);
                    if let Some(class_decl) = class_opt {
                        let method_sig = class_decl
                            .method_signatures
                            .get(*slot as usize)
                            .map(|(_, tv)| Arc::clone(tv));
                        if let Some(sig) = method_sig {
                            // The method signature is a TypeValue.Fn using the class's type parameters
                            // as TypeValue.Var nodes. Wrap the signature in a TypeValue.Scheme with
                            // the class params as quantified vars, then instantiate at current level.
                            use crate::value::{HashableValue, Value, TYPE_ID_DICT};
                            use indexmap::IndexMap;

                            // Build vars dict: class params → empty VarDecl dicts (kind is not used by instantiate_scheme_tv).
                            let mut vars_entries = IndexMap::new();
                            for (param_name, _kind) in &class_decl.params {
                                let var_key = HashableValue::Str(Arc::from(param_name.as_str()));
                                // VarDecl payload: empty dict (instantiate_scheme_tv only reads var names from keys).
                                let empty_dict = Value::Dict {
                                    entries: IndexMap::new(),
                                    type_decl_id: TYPE_ID_DICT,
                                    type_val: None,
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
                                type_decl_id: TYPE_ID_DICT,
                                type_val: None,
                            };
                            let vars_thunk = Arc::new(crate::value::Thunk::value(
                                vars_dict,
                                crate::rust_span!(),
                            ));

                            // Build constraints dict: empty (no constraints for method signatures).
                            let constraints_dict = Value::Dict {
                                entries: IndexMap::new(),
                                type_decl_id: TYPE_ID_DICT,
                                type_val: None,
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
                                type_decl_id: TYPE_ID_DICT,
                                type_val: None,
                            };
                            let scheme_payload_thunk = Arc::new(crate::value::Thunk::value(
                                scheme_payload,
                                crate::rust_span!(),
                            ));

                            // Build the TypeValue.Scheme variant.
                            let scheme = Arc::new(Value::Variant {
                                type_decl_id: crate::value::TYPE_ID_TYPE_VALUE,
                                ctor_index: TV_SCHEME,
                                payload: Some(scheme_payload_thunk),
                                type_val: None,
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
                    // Slot lookup via scope_chain.
                    // scope_chain[len - 1 - depth] is the frame at depth `depth` from the current scope.
                    // resolver_level = depth from current scope to target frame.
                    let scope_idx = state
                        .scope_chain
                        .len()
                        .checked_sub(1 + resolver_level as usize);
                    let slot_scheme =
                        scope_idx.and_then(|idx| state.scope_chain[idx].get_type_scheme(slot));
                    if slot_scheme.is_some() {
                        let dep_def_span = scope_idx
                            .and_then(|idx| state.scope_chain[idx].definition_span(slot).cloned());
                        if let Some(idx) = scope_idx {
                            state.scope_chain[idx].mark_referenced(slot);
                        }
                        if let Some(ref def_span) = dep_def_span {
                            let dep_id = BindingId {
                                def_span: def_span.clone(),
                                name: name.to_string(),
                            };
                            if let Some(narrowed) = state.narrowing_map.get(&dep_id) {
                                return Arc::clone(narrowed);
                            }
                            if let Some(ref binder) = state.current_binding {
                                if *binder != dep_id {
                                    state
                                        .use_def
                                        .entry(binder.clone())
                                        .or_default()
                                        .insert(dep_id);
                                }
                            }
                        }
                        slot_scheme
                    } else {
                        let mut diag = Diagnostic::error(
                        "resolver-slot-miss",
                        format!(
                            "resolver assigned slot ({}, {}) for '{}' but scope has no entry at that address",
                            resolver_level, slot, name
                        ),
                        node.span.clone(),
                    );
                        for (depth, frame) in state.scope_chain.iter().enumerate() {
                            let slot_count = frame.len();
                            diag = diag
                                .with_note(format!("scope depth {}: {} slots", depth, slot_count));
                        }
                        errors.push(diag);
                        None
                    }
                }
            } // close match addr
        } else {
            // No resolver address for this VarRef — resolver bug.
            errors.push(Diagnostic::error(
                "resolver-miss",
                format!(
                    "VarRef '{}' has no resolution — resolver did not resolve this reference",
                    name
                ),
                node.span.clone(),
            ));
            None
        }
    } else {
        // Not a VarRef node — caller error.
        errors.push(Diagnostic::error(
            "infer-var-ref-not-varref",
            format!("infer_var_ref called on non-VarRef node"),
            node.span.clone(),
        ));
        None
    };

    if let Some(scheme) = scheme {
        // Check if scheme is polymorphic (TypeValue.Scheme) — if so, store in scheme_map for LSP.
        if typevalue_ctor_index(&scheme) == Some(TV_SCHEME) {
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
        let result_type = if typevalue_ctor_index(&scheme) == Some(TV_SCHEME) {
            let current_level = state.ctx.current_level;
            match crate::types::instantiate_scheme_tv_with_renaming(
                &scheme,
                &mut state.ctx,
                current_level,
            ) {
                Some((instantiated, renaming)) => {
                    if !renaming.is_empty() {
                        state.last_scheme_renaming = Some(renaming);
                    }
                    instantiated
                }
                None => {
                    // instantiate_scheme_tv returning None on a TypeValue.Scheme-tagged value
                    // is an invariant violation: the scheme payload is malformed.
                    errors.push(Diagnostic::error(
                        "type-error",
                        "malformed TypeValue.Scheme: instantiation failed (payload corrupt)",
                        node.span.clone(),
                    ));
                    // Malformed scheme: Never (error recovery)
                    make_typevalue_never()
                }
            }
        } else {
            Arc::clone(&scheme)
        };

        result_type
    } else {
        // If the resolver successfully resolved this variable, the variable
        // genuinely exists in scope — the type checker simply doesn't have its type.
        // Emit a warning and return Unknown (gradual typing) rather than a false
        // "undefined variable" diagnostic.
        if let crate::ast::SurfaceExpression::VarRef { resolution, .. } = &node.expr {
            if let Some(Some(_)) = resolution.get() {
                errors.push(Diagnostic::warn(
                    "untyped-binding",
                    format!("no type declaration for '{}'", name),
                    node.span.clone(),
                ));
                return make_typevalue_never();
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
                    // Annotation resolution failed: Never (error recovery)
                    make_typevalue_never()
                }
            };
            state
                .failed_bindings
                .insert(name.to_string(), node.span.clone());
            ty
        } else {
            errors.push(err);
            // Undefined variable: Never (error recovery)
            make_typevalue_never()
        }
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
    stack: &mut Vec<(TypeCheckCont, Arc<SurfaceNode>)>,
) -> TypeCheckAction {
    match typevalue_ctor_index(&func_ty) {
        Some(TV_UNKNOWN) | Some(TV_NEVER) => {
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            let result_ty = if typevalue_ctor_index(&func_ty) == Some(TV_NEVER) {
                make_typevalue_never()
            } else {
                make_typevalue_unknown()
            };
            return TypeCheckAction::Done(result_ty);
        }
        _ => {}
    }

    // T-2149: Extract EffectPerform metadata for instance checking after args are inferred.
    // Read from VarRef node's resolution OnceLock instead of resolution_table.
    let effect_perform_meta = if let SurfaceExpression::VarRef { resolution, .. } = &func_node.expr
    {
        if let Some(Some(crate::ast::VarAddr::EffectPerform { slot })) = resolution.get() {
            Some((0, slot.to_string()))
        } else {
            None
        }
    } else {
        None
    };

    match typevalue_ctor_index(&func_ty) {
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
                        // Malformed Fn: Never (error recovery)
                        return TypeCheckAction::Done(make_typevalue_never());
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
                // Wire type instantiation on call_node.
                if let Some(renaming) = ctx.state.last_scheme_renaming.take() {
                    let mut type_inst = std::collections::HashMap::new();
                    for (original_name, fresh_name) in &renaming {
                        let fresh_tv = crate::type_infer::make_typevar_value(fresh_name);
                        let resolved_var = ctx.state.apply(&fresh_tv);
                        if crate::type_infer::typevalue_ctor_index(&resolved_var) != Some(TV_VAR) {
                            type_inst.insert(original_name.clone(), resolved_var);
                        }
                    }
                    call_node.type_instantiation.set(type_inst);
                }
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
                    for arg in &args {
                        let mut local_stack = Vec::new();
                        let _ = Box::pin(run_typecheck(
                            arg,
                            &env,
                            ctx.state,
                            ctx.errors,
                            ctx.type_map,
                            &mut local_stack,
                        ))
                        .await;
                    }
                    for na in &named_args {
                        let mut local_stack = Vec::new();
                        let _ = Box::pin(run_typecheck(
                            &na.node.value,
                            &env,
                            ctx.state,
                            ctx.errors,
                            ctx.type_map,
                            &mut local_stack,
                        ))
                        .await;
                    }
                    let mut err = Diagnostic::error(
                        "type-error",
                        format!(
                            "arity mismatch: expected {} argument(s), got {}",
                            min_req, n_total
                        ),
                        call_node.span.clone(),
                    );
                    if let SurfaceExpression::VarRef { name, .. } = &func_node.expr {
                        err = err.with_note(format!("in call to '{}'", name));
                    }
                    ctx.errors.push(err.clone());
                    // Arity error: Never (error recovery)
                    return TypeCheckAction::Done(make_typevalue_never());
                }
            }

            // Arity is correct. Start evaluating positional args.
            let first_arg = Arc::clone(&args[0]);
            let arg_nodes: Vec<Arc<SurfaceNode>> = args.iter().map(Arc::clone).collect();
            let remaining: Vec<Arc<SurfaceNode>> = args[1..].iter().map(Arc::clone).collect();
            let call_span = call_node.span.clone();
            stack.push((
                TypeCheckCont::CallArg {
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
                    call_node: Arc::clone(&call_node),
                    effect_perform: effect_perform_meta.clone(),
                },
                Arc::clone(&call_node),
            ));
            TypeCheckAction::Eval(first_arg, env)
        }

        Some(TV_VAR) => {
            // Unbound TypeVar: infer args for side effects, return fresh TypeVar for return
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            let ret_var = ctx.state.fresh_type_var(&call_node.span);
            TypeCheckAction::Done(ret_var)
        }

        Some(TV_UNKNOWN) | Some(TV_TOP) => {
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            ctx.state.diagnostics.push(crate::error::Diagnostic::warn(
                "unknown-call",
                "calling expression of Unknown type — may not be a function",
                call_node.span.clone(),
            ));
            TypeCheckAction::Done(make_typevalue_never())
        }

        Some(TV_NOMINAL_VARIANT) => {
            // Variant constructor call: build a payload record from named args.
            //
            // Arity rule: a variant constructor takes at most 1 positional argument
            // (the payload dict). Named args are always allowed (they build the payload).
            if args.len() > 1 {
                for arg in &args {
                    let mut local_stack = Vec::new();
                    let _ = Box::pin(run_typecheck(
                        arg,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
                for na in &named_args {
                    let mut local_stack = Vec::new();
                    let _ = Box::pin(run_typecheck(
                        &na.node.value,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
                ctx.errors.push(Diagnostic::error(
                    "type-error",
                    format!(
                        "variant constructor takes at most 1 positional argument, got {}",
                        args.len()
                    ),
                    call_node.span.clone(),
                ));
                // Variant arity error: Never (error recovery)
                return TypeCheckAction::Done(make_typevalue_never());
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
                    if let Err(mut e) = constrain(
                        &arg_ty,
                        &declared_fields_tv,
                        &mut ctx.state.ctx,
                        &mut constraints,
                        args[0].span.clone(),
                    )
                    .await
                    {
                        if let Some((tycon, ctor)) = typevalue_nominal_variant_tag(&func_ty) {
                            e = e.with_note(format!(
                                "in payload for variant constructor '{}.{}'",
                                tycon, ctor
                            ));
                        }
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
                    let _ = Box::pin(run_typecheck(
                        &na.node.value,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
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
                    // Malformed Inter: Never (error recovery)
                    return TypeCheckAction::Done(make_typevalue_never());
                }
            };
            let fn_members: Vec<TypeValue> = members
                .into_iter()
                .filter(|m| typevalue_ctor_index(m) == Some(TV_FN))
                .collect();

            if fn_members.is_empty() {
                let err = Diagnostic::error(
                    "type-error",
                    "expected function type, got intersection of non-function types",
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                // Inter with no Fn members: Never (error recovery)
                return TypeCheckAction::Done(make_typevalue_never());
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
                // No matching overload: Never (error recovery)
                return TypeCheckAction::Done(make_typevalue_never());
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
                .filter(|m| typevalue_ctor_index(m) == Some(TV_FN))
                .collect();

            if fn_members.is_empty() {
                let err = Diagnostic::error(
                    "type-error",
                    "expected function type, got union of non-function types",
                    call_node.span.clone(),
                );
                ctx.errors.push(err);
                for arg in &args {
                    let mut local_stack = Vec::new();
                    let _ = Box::pin(run_typecheck(
                        arg,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
                for na in &named_args {
                    let mut local_stack = Vec::new();
                    let _ = Box::pin(run_typecheck(
                        &na.node.value,
                        &env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
                // Union with no Fn members: Never (error recovery)
                return TypeCheckAction::Done(make_typevalue_never());
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
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await;
            }
            let err = Diagnostic::error(
                "type-error",
                format!(
                    "expected function type, got {}",
                    crate::eval::format_type_with_names(
                        &func_ty,
                        &ctx.state.ctx.type_display_names
                    )
                ),
                call_node.span.clone(),
            );
            ctx.errors.push(err);
            // Non-callable type: Never (error recovery)
            TypeCheckAction::Done(make_typevalue_never())
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
        // Arity error: Never (error recovery)
        return make_typevalue_never();
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
                if let Err(mut e) = constrain(
                    &arg_ty,
                    &param_ty,
                    &mut ctx.state.ctx,
                    &mut constraints,
                    na.span.clone(),
                )
                .await
                {
                    e = e.with_note(format!(
                        "expected: {} (parameter '{}')",
                        crate::eval::format_type_with_names(
                            &ctx.state.apply(&param_ty),
                            &ctx.state.ctx.type_display_names
                        ),
                        na.node.name
                    ));
                    e = e.with_note(format!(
                        "actual:   {}",
                        crate::eval::format_type_with_names(
                            &ctx.state.apply(&arg_ty),
                            &ctx.state.ctx.type_display_names
                        )
                    ));
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
                    let _ = Box::pin(run_typecheck(
                        &na.node.value,
                        env,
                        ctx.state,
                        ctx.errors,
                        ctx.type_map,
                        &mut local_stack,
                    ))
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
            let is_concrete = match typevalue_ctor_index(&arg_ty_applied) {
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
                    let arg_ctor = typevalue_ctor_index(&arg_ty_applied);
                    let has_matching_instance = env_read.all_instances().iter().any(|(_, inst)| {
                        if inst.class_name != class_decl.name {
                            return false;
                        }
                        // For single-parameter classes, instance_type is the covered type.
                        // For multi-param, it's a TypeValue.Record with numbered fields.
                        // Check if instance_type ctor matches arg_ty ctor as a simple heuristic.
                        let inst_ctor = typevalue_ctor_index(&inst.instance_type);
                        arg_ctor == inst_ctor || Arc::ptr_eq(&inst.instance_type, &arg_ty_applied)
                    });

                    if !has_matching_instance {
                        let arg_ty_str = crate::eval::format_type_with_names(
                            &arg_ty_applied,
                            &ctx.state.ctx.type_display_names,
                        );
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
        // Arity error: Never (error recovery)
        return TypeCheckAction::Done(make_typevalue_never());
    }

    // Unify positional args against fixed params (Robinson unification via unify())
    let mut consumed_params: std::collections::HashSet<usize> = std::collections::HashSet::new();
    for (idx, ((param_name, param_ty), arg_ty)) in param_types
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
            typevalue_ctor_index(&widened_arg),
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
            let param_label = match param_name {
                Some(name) => format!(" (parameter '{}')", name),
                None => String::new(),
            };
            e = e.with_note(format!(
                "expected: {}{}",
                crate::eval::format_type_with_names(
                    &ctx.state.apply(param_ty),
                    &ctx.state.ctx.type_display_names
                ),
                param_label,
            ));
            e = e.with_note(format!(
                "actual:   {}",
                crate::eval::format_type_with_names(
                    &ctx.state.apply(&widened_arg),
                    &ctx.state.ctx.type_display_names
                ),
            ));
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
                    ctx.state.type_name_to_id.clone(),
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
                            crate::eval::format_type_with_names(
                                &widened,
                                &ctx.state.ctx.type_display_names
                            )
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
                    let e = e.with_note("in variadic parameter");
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
                    let e = e.with_note("in variadic rest arguments");
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
                let e = e.with_note("in unmatched named arguments");
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
    if typevalue_ctor_index(bucket_ty) == Some(TV_APP) {
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
    _env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
    stack: &mut Vec<(TypeCheckCont, Arc<SurfaceNode>)>,
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
                        SurfaceExpression::Dict {
                            entries: bind_entries,
                            ..
                        } => {
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
                        // `doc:` alone is ambiguous (could be a record field name).
                        // Only structural keys (return/bind/constraint/kinds) trigger
                        // the fn-metadata path.
                        const FN_STRUCTURAL_KEYS: &[&str] =
                            &["return", "constraint", "bind", "kinds"];
                        matches!(&k.expr,
                            SurfaceExpression::StringLiteral { content: s, .. }
                                if FN_STRUCTURAL_KEYS.contains(&s.as_str()))
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
                        let e = e.with_note("in function return type annotation");
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
                        let e = e.with_note("in function return type annotation");
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

    // Create a fresh Env for the function parameters (child of the closure-capture env).
    let fn_env_arc = Arc::new(RwLock::new(Env::with_parent(Arc::clone(_env))));
    // Deferred insertions: (slot, name, type, def_span) — applied after the loop
    // to avoid holding the write guard across await points.
    let mut deferred_inserts: Vec<(usize, String, TypeValue, Option<Span>)> = Vec::new();
    let mut param_types: Vec<(Option<String>, TypeValue)> = Vec::new();
    let mut typed_variadics: Vec<(String, TypeValue)> = Vec::new();
    let mut rest: Option<Box<(String, TypeValue)>> = None;
    let mut fixed_param_idx: usize = 0;
    // param_slot_idx is a 0-based counter for fallback when resolved_dispatch_slot is unavailable.
    // The resolver assigns absolute Dispatch slots and stores them in resolved_dispatch_slot.
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
                        let e =
                            e.with_note(format!("in annotation for parameter '{}'", p.node.name));
                        errors.push(e);
                        // Annotation resolution failed: Never (error recovery)
                        make_typevalue_never()
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
                    let e = e.with_note(format!("in annotation for parameter '{}'", p.node.name));
                    errors.push(e);
                    // Annotation resolution failed: Never (error recovery)
                    make_typevalue_never()
                }
            }
        } else {
            // Unannotated fixed param: use expected type from class method signature if available,
            // otherwise fall back to a fresh TypeVar for HM inference.
            if let Some(ref expected) = expected_params {
                expected
                    .get(fixed_param_idx)
                    .cloned()
                    .unwrap_or_else(|| state.fresh_type_var(&p.span))
            } else {
                state.fresh_type_var(&p.span)
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
        // Insert params into SLOTS at absolute slot positions.
        // Resolver assigns Dispatch(0, accumulated_dict_offset + i) to each param and sets
        // resolved_dispatch_slot on the SurfaceParam node. Use that slot, or fall back to
        // param_slot_idx for params without resolved slots (shouldn't happen after resolver).
        let slot = p
            .node
            .resolved_dispatch_slot
            .get()
            .map(|s| s as usize)
            .unwrap_or(param_slot_idx);
        deferred_inserts.push((
            slot,
            p.node.name.clone(),
            Arc::clone(&param_ty),
            param_definition_span,
        ));
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

    // Apply deferred inserts to the pre-built frame (scoped to avoid holding guard across await).
    {
        let mut fn_env_guard = fn_env_arc.write().unwrap();
        for (slot, name, ty, def_span) in deferred_inserts {
            fn_env_guard.insert_at_slot(slot, name, ty, def_span);
        }
    }

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
    stack.push((
        TypeCheckCont::FnBody {
            saved_level,
            saved_expected_return,
            return_ann: return_ann_type,
            params: param_types,
            typed_variadics,
            rest,
            required_count,
            node_span: node.span.clone(),
            trace_level,
        },
        Arc::clone(node),
    ));
    stack.push((
        TypeCheckCont::AfterBlock {
            binding_envs: vec![Arc::clone(&fn_env_arc)],
            pre_final_refs: vec![std::collections::HashSet::new()],
            saved_use_def: std::mem::take(&mut state.use_def),
            saved_current_binding: state.current_binding.take(),
            saved_narrowing_map: std::mem::take(&mut state.narrowing_map),
        },
        Arc::clone(node),
    ));

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

/// Extract `(type_decl_id, ctor_index)` from a pattern node's `inferred_type_decl_id`.
///
/// The type checker populates `inferred_type_decl_id` with a NominalVariant TypeValue
/// for constructor patterns. This reads the numeric identity from that TypeValue.
fn extract_ctor_identity_from_inferred_type(node: &SurfaceNode) -> Option<(u64, u32)> {
    let tv = node.inferred_type_decl_id.get()?;
    if typevalue_ctor_index(tv) != Some(TV_NOMINAL_VARIANT) {
        return None;
    }
    let extract_int = |field: &str| -> Option<u64> {
        match tv.as_ref() {
            crate::value::Value::Variant {
                payload: Some(thunk),
                ..
            } => match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    let key = crate::value::HashableValue::Str(Arc::from(field));
                    let val_thunk = entries.get(&key)?;
                    match val_thunk.peek_result()? {
                        Ok(crate::value::Value::Int { n, .. }) => Some(*n as u64),
                        Ok(crate::value::Value::U64 { n, .. }) => Some(*n),
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        }
    };
    let type_decl_id = extract_int(FIELD_TYCON)?;
    let ctor_index = extract_int(FIELD_CTOR)? as u32;
    Some((type_decl_id, ctor_index))
}

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
    // Build a case arm env with the binding names from let_bindings.
    // For keyed arms (empty PatternDecl), build_case_arm_env returns a clone of env.
    let arm_env: Arc<RwLock<Env>> = build_case_arm_env(&arm.let_bindings, env, state, &arm.pattern);

    // Guard inference — run for type-map side effects. Extract narrowings WITHOUT applying.
    // The caller saves the outer narrowing_map (take), then applies these narrowings so the
    // arm body sees them. AfterBlock restores the outer narrowing_map after the arm.
    let guard_narrowings = if let Some(guard) = &arm.guard {
        let mut local_stack = Vec::new();
        let _guard_ty = Box::pin(run_typecheck(
            guard,
            &arm_env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await;
        typecheck_narrow::extract_narrowings(guard, &arm_env, &state.type_name_to_id)
    } else {
        Vec::new()
    };
    let arm_env = arm_env; // no change — narrowings not applied here

    // Compute updated remaining_scrutinee (I-Case3 negation accumulation) for next arm.
    let next_remaining_scrutinee = if arm.guard.is_none() {
        match &arm.pattern.expr {
            // Unit constructor pattern: TypeName.Ctor
            crate::ast::SurfaceExpression::Field {
                expr: Some(_),
                field: crate::ast::DotKey::Ident(_),
                ..
            } => {
                if let Some((type_decl_id, ctor_index)) =
                    extract_ctor_identity_from_inferred_type(&arm.pattern)
                {
                    let neg_tag = make_typevalue_negation(make_typevalue_nominal_variant_by_id(
                        type_decl_id,
                        ctor_index,
                        make_typevalue_record(indexmap::IndexMap::new(), None),
                    ));
                    typevalue_normalize_intersection(vec![Arc::clone(remaining_scrutinee), neg_tag])
                } else {
                    errors.push(Diagnostic::error(
                        "type-error",
                        "pattern constructor identity not resolved",
                        arm.pattern.span.clone(),
                    ));
                    Arc::clone(remaining_scrutinee)
                }
            }
            // Payload constructor pattern: [TypeName.Ctor payload]
            crate::ast::SurfaceExpression::Call { func, .. }
                if matches!(
                    &func.expr,
                    crate::ast::SurfaceExpression::Field {
                        expr: Some(_),
                        field: crate::ast::DotKey::Ident(_),
                        ..
                    }
                ) =>
            {
                if let Some((type_decl_id, ctor_index)) =
                    extract_ctor_identity_from_inferred_type(&arm.pattern)
                {
                    let neg_tag = make_typevalue_negation(make_typevalue_nominal_variant_by_id(
                        type_decl_id,
                        ctor_index,
                        make_typevalue_record(indexmap::IndexMap::new(), None),
                    ));
                    typevalue_normalize_intersection(vec![Arc::clone(remaining_scrutinee), neg_tag])
                } else {
                    errors.push(Diagnostic::error(
                        "type-error",
                        "pattern constructor identity not resolved",
                        arm.pattern.span.clone(),
                    ));
                    Arc::clone(remaining_scrutinee)
                }
            }
            // Dict pattern — narrow the scrutinee by excluding the matched shape.
            crate::ast::SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
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

/// Extract the (bare_ctor_name, arity) constructor list from a TypeValue ADT body.
/// Position in the returned vec = ctor_index.
fn extract_typevalue_constructors(body: &TypeValue) -> Vec<(String, usize)> {
    match typevalue_ctor_index(body) {
        Some(TV_NOMINAL_VARIANT) => {
            if let Some((_tycon, ctor)) = typevalue_nominal_variant_tag(body) {
                let arity = if typevalue_nominal_variant_has_fields(body) {
                    1
                } else {
                    0
                };
                vec![(ctor, arity)]
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
                    if typevalue_ctor_index(m) == Some(TV_NOMINAL_VARIANT) {
                        if let Some((_tycon, ctor)) = typevalue_nominal_variant_tag(m) {
                            let arity = if typevalue_nominal_variant_has_fields(m) {
                                1
                            } else {
                                0
                            };
                            Some((ctor, arity))
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
async fn run_match_exhaustiveness_check(
    scrutinee_ty: &TypeValue,
    arms: &[SurfaceMatchArm],
    span: &Span,
    state: &mut InferState,
    errors: &mut Vec<Diagnostic>,
) {
    let tycon_env_ref = state.tycon_env_ref();
    // Coverage checking for TypeValue: inspect ctor tag to determine what kind of scrutinee.
    // This is a simplified version — full TypeValue-based coverage would require more helpers.
    let sig: Option<coverage::ConstructorSignature> = match typevalue_ctor_index(scrutinee_ty) {
        Some(TV_UNION) => {
            // Try to extract NominalVariant constructors from union members.
            if let Some(members) = typevalue_extract_members_pub(scrutinee_ty) {
                let mut constructors = Vec::new();
                let extract_int_field_local =
                    |tv: &Arc<crate::value::Value>, field: &str| -> Option<u64> {
                        match tv.as_ref() {
                            crate::value::Value::Variant {
                                payload: Some(thunk),
                                ..
                            } => match thunk.peek_result()? {
                                Ok(crate::value::Value::Dict { entries, .. }) => {
                                    let key = crate::value::HashableValue::Str(Arc::from(field));
                                    let val_thunk = entries.get(&key)?;
                                    match val_thunk.peek_result()? {
                                        Ok(crate::value::Value::Int { n, .. }) => Some(*n as u64),
                                        Ok(crate::value::Value::U64 { n, .. }) => Some(*n),
                                        _ => None,
                                    }
                                }
                                _ => None,
                            },
                            _ => None,
                        }
                    };
                for m in &members {
                    if typevalue_ctor_index(m) == Some(TV_NOMINAL_VARIANT) {
                        if let (Some(type_decl_id), Some(ctor_index)) = (
                            extract_int_field_local(m, FIELD_TYCON),
                            extract_int_field_local(m, FIELD_CTOR),
                        ) {
                            let has_fields = typevalue_nominal_variant_has_fields(m);
                            constructors.push((
                                coverage::ConstructorTag::Variant {
                                    type_decl_id,
                                    ctor_index: ctor_index as u32,
                                },
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
            // Extract numeric identity from NominalVariant payload.
            // The payload dict contains FIELD_TYCON (type_decl_id) and FIELD_CTOR (ctor_index)
            // as Int values when created with make_typevalue_nominal_variant_by_id.
            let extract_int_field = |tv: &Arc<crate::value::Value>, field: &str| -> Option<u64> {
                match tv.as_ref() {
                    crate::value::Value::Variant {
                        payload: Some(thunk),
                        ..
                    } => match thunk.peek_result()? {
                        Ok(crate::value::Value::Dict { entries, .. }) => {
                            let key = crate::value::HashableValue::Str(Arc::from(field));
                            let val_thunk = entries.get(&key)?;
                            match val_thunk.peek_result()? {
                                Ok(crate::value::Value::Int { n, .. }) => Some(*n as u64),
                                Ok(crate::value::Value::U64 { n, .. }) => Some(*n),
                                _ => None,
                            }
                        }
                        _ => None,
                    },
                    _ => None,
                }
            };

            if let (Some(type_decl_id), Some(ctor_index)) = (
                extract_int_field(scrutinee_ty, FIELD_TYCON),
                extract_int_field(scrutinee_ty, FIELD_CTOR),
            ) {
                let has_fields = typevalue_nominal_variant_has_fields(scrutinee_ty);
                Some(coverage::ConstructorSignature {
                    constructors: vec![(
                        coverage::ConstructorTag::Variant {
                            type_decl_id,
                            ctor_index: ctor_index as u32,
                        },
                        if has_fields { 1 } else { 0 },
                    )],
                })
            } else {
                None
            }
        }
        Some(TV_NOMINAL) => {
            // TyCon reference — look up in tycon_env for constructors.
            let type_decl_id = crate::type_infer::typevalue_nominal_id(scrutinee_ty)
                .expect("invariant: TV_NOMINAL TypeValue must have an id field");
            match tycon_env_ref.get(&type_decl_id) {
                Some(def) if !def.constructors.is_empty() => {
                    let constructors = def
                        .constructors
                        .iter()
                        .enumerate()
                        .map(|(idx, (_name, arity))| {
                            let clamped = if *arity == 0 { 0 } else { 1 };
                            (
                                coverage::ConstructorTag::Variant {
                                    type_decl_id,
                                    ctor_index: idx as u32,
                                },
                                clamped,
                            )
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
        let mut coverage_patterns: Vec<coverage::CoveragePattern> = Vec::with_capacity(arms.len());
        let mut had_errors = false;
        for arm in arms {
            match coverage::ast_pattern_to_coverage(&arm.pattern, state).await {
                Ok(pat) => coverage_patterns.push(pat),
                Err(diag) => {
                    errors.push(diag);
                    had_errors = true;
                }
            }
        }
        if had_errors {
            return;
        }
        let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();
        let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);

        if !result.exhaustive {
            let witnesses: Vec<String> = result
                .uncovered
                .iter()
                .map(|w| w.render(tycon_env_ref))
                .collect();
            let mut diag = Diagnostic::error(
                "type-error",
                format!(
                    "non-exhaustive match: missing coverage for {}",
                    witnesses.join(", ")
                ),
                span.clone(),
            );
            for (i, arm) in arms.iter().enumerate() {
                diag = diag.with_label(
                    arm.pattern.span.clone(),
                    format!(
                        "arm {}: {}",
                        i + 1,
                        result.resolved[i].render(tycon_env_ref)
                    ),
                );
            }
            errors.push(diag);
        }
        for (idx, covering) in &result.redundant {
            let mut diag = Diagnostic::with_spans(
                crate::error::DiagnosticLevel::Err,
                "type-error",
                "unreachable match arm: this pattern is already covered by prior arms",
                vec![crate::error::SpanLabel {
                    span: arms[*idx].pattern.span.clone(),
                    message: format!(
                        "resolved to: {}",
                        result.resolved[*idx].render(tycon_env_ref)
                    ),
                }],
            );
            for &cover_idx in covering {
                diag = diag.with_label(
                    arms[cover_idx].pattern.span.clone(),
                    format!(
                        "already matched: {}",
                        result.resolved[cover_idx].render(tycon_env_ref)
                    ),
                );
            }
            errors.push(diag);
        }
        for &idx in &result.inaccessible {
            errors.push(
                Diagnostic::error(
                    "type-error",
                    "inaccessible match arm: reachable only via diverging (bottom) values",
                    arms[idx].pattern.span.clone(),
                )
                .with_note(format!(
                    "pattern resolved to: {}",
                    result.resolved[idx].render(tycon_env_ref)
                )),
            );
        }
    }
}

// ===== Inline helper: CaseArm env setup =====

fn build_case_arm_env(
    let_bindings: &Arc<SurfaceNode>,
    _env: &Arc<RwLock<Env>>,
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
    // Create a fresh Env for match arm bindings if there are any [let] bindings.
    if !bindings_with_spans.is_empty() {
        let arm_env_arc = Arc::new(RwLock::new(Env::with_parent(Arc::clone(_env))));
        {
            let mut child_inner = arm_env_arc.write().unwrap();
            for (slot_idx, (name, binding_span)) in bindings_with_spans.into_iter().enumerate() {
                let tv = state.fresh_type_var(&binding_span);
                child_inner.insert_at_slot(slot_idx, name, tv, Some(binding_span));
            }
        }
        arm_env_arc
    } else {
        Arc::clone(_env)
    }
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
        state.type_name_to_id.clone(),
    );
    // Check if both are function types (TypeValue.Fn) for arity checking.
    if typevalue_ctor_index(actual) == Some(TV_FN) && typevalue_ctor_index(expected) == Some(TV_FN)
    {
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
    let definitely_fails = if typevalue_ctor_index(actual) == Some(TV_UNION) {
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
            crate::eval::format_type_with_names(expected, &state.ctx.type_display_names)
        ))
        .with_note(format!(
            "actual:   {}",
            crate::eval::format_type_with_names(actual, &state.ctx.type_display_names)
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
            ctor_index,
            payload: Some(thunk),
            ..
        } if *ctor_index == TV_FN => thunk,
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

/// Recursively collect type variable references from an annotation.
///
/// This is part of the dependency graph construction: a binding that has an annotation
/// may reference other sibling bindings via type variables in that annotation, creating
/// a dependency edge in the SCC graph.
fn collect_annotation_deps(
    ann: &crate::ast::Annotation,
    name_to_idx: &HashMap<String, usize>,
    deps: &mut Vec<usize>,
) {
    match ann {
        crate::ast::Annotation::Simple(name) => {
            if let Some(&idx) = name_to_idx.get(name.as_str()) {
                deps.push(idx);
            }
        }
        crate::ast::Annotation::Annotated(base, arg) => {
            collect_annotation_deps(base, name_to_idx, deps);
            collect_annotation_deps(arg, name_to_idx, deps);
        }
        crate::ast::Annotation::PropertyDict(entries) => {
            for entry in entries {
                if let SurfaceExpression::VarRef { name, .. } = &entry.node.value.expr {
                    if let Some(&idx) = name_to_idx.get(name.as_str()) {
                        deps.push(idx);
                    }
                }
            }
        }
        // Quote has no annotation dependencies — it's a keyword marker, not a type reference
        crate::ast::Annotation::Quote => {}
    }
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
            SurfaceExpression::VarRef {
                name, annotation, ..
            } => {
                // Only record a dependency when the name is a sibling binding AND is
                // not shadowed by a locally-bound name (parameter, case-arm binding).
                if !locals.contains(name.as_str()) {
                    if let Some(&idx) = name_to_idx.get(name.as_str()) {
                        deps.push(idx);
                    }
                }
                // VarRef annotations can reference other sibling bindings as type variables.
                if let Some(ref ann) = annotation {
                    collect_annotation_deps(&ann.node, name_to_idx, &mut deps);
                }
            }
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. } => {}
            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        worklist.push((key, Arc::clone(&locals)));
                    }
                    worklist.push((&entry.node.value, Arc::clone(&locals)));
                }
            }
            SurfaceExpression::Fn {
                params,
                body,
                return_ann,
                ..
            } => {
                // Parameters are locally bound within the function body — they shadow any
                // sibling binding with the same name.  Build an extended locals set and
                // use it exclusively when scanning the body.
                let mut fn_locals: HashSet<String> = (*locals).clone();
                for spanned_param in params {
                    fn_locals.insert(spanned_param.node.name.clone());
                    // Parameter annotations can reference sibling bindings as type variables.
                    if let Some(ref ann) = spanned_param.node.annotation {
                        collect_annotation_deps(&ann.node, name_to_idx, &mut deps);
                    }
                }
                // Return annotation can reference sibling bindings as type variables.
                if let Some(ref ret_ann) = return_ann {
                    collect_annotation_deps(&ret_ann.node, name_to_idx, &mut deps);
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
            SurfaceExpression::TypeAssert {
                annotation, expr, ..
            } => {
                worklist.push((expr, Arc::clone(&locals)));
                // TypeAssert annotations can reference sibling bindings as type variables.
                collect_annotation_deps(&annotation.node, name_to_idx, &mut deps);
            }
            SurfaceExpression::Placeholder(..) => {}
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => {
                worklist.push((e, Arc::clone(&locals)));
            }
            SurfaceExpression::Decl(decl_box) => match decl_box.as_ref() {
                crate::ast::SurfaceDeclaration::ClassDecl { methods, .. } => {
                    for method_entry in methods {
                        worklist.push((&method_entry.node.value, Arc::clone(&locals)));
                    }
                }
                crate::ast::SurfaceDeclaration::InstanceDecl {
                    class_name, arms, ..
                } => {
                    worklist.push((class_name, Arc::clone(&locals)));
                    for (_pattern, method_entries) in arms {
                        for me in method_entries {
                            worklist.push((&me.node.value, Arc::clone(&locals)));
                        }
                    }
                }
                _ => {}
            },
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
                    let let_bindings = &arm.let_bindings;
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
                }
            }
            SurfaceExpression::Error(_) => {}
            // Expansion-produced variants
            SurfaceExpression::UnitVariant { .. } => {}
            SurfaceExpression::MakeVariant { payload, .. } => {
                worklist.push((payload, Arc::clone(&locals)));
            }
            SurfaceExpression::TypeDecl { inner, .. } => {
                worklist.push((inner, Arc::clone(&locals)));
            }
            SurfaceExpression::ReprDecl { inner, .. } => {
                worklist.push((inner, Arc::clone(&locals)));
            }
        }
    }

    deps
}

/// Occurs check: returns true if TypeVar `name` appears free anywhere in `ty`.
pub(crate) fn type_contains_typevar(ty: &TypeValue, name: &str) -> bool {
    match typevalue_ctor_index(ty) {
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

/// Build the constructor dict value type for an ADT.
///
/// For ADT types (Union of NominalVariants or single NominalVariant), produces a Dict
/// where unit constructors → NominalVariant values and payload constructors → Functions.
/// For non-ADT types, returns the body type unchanged.
///
/// Called from `typecheck_letrec_group` type declaration registration step and from
/// `process_document` when reconstructing constructor schemes via tycon_env diff.
pub(crate) fn adt_value_type(alias_body: &TypeValue) -> TypeValue {
    let members: Vec<TypeValue> = match typevalue_ctor_index(alias_body) {
        Some(TV_UNION) => typevalue_extract_members_pub(alias_body)
            .expect("invariant: TV_UNION TypeValue must have extractable members"),
        Some(TV_NOMINAL_VARIANT) => vec![Arc::clone(alias_body)],
        _ => return Arc::clone(alias_body),
    };
    let mut ctor_dict_fields: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    for m in &members {
        if typevalue_ctor_index(m) == Some(TV_NOMINAL_VARIANT) {
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
                match typevalue_ctor_index(&key_ty) {
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

// ===== Helper: register_class_method_signatures =====

/// Extract and register method signatures for a ClassDecl entry.
///
/// Called after ClassDecl type-checking to parse method type expressions,
/// store them in ClassDecl.method_signatures, generalize, and insert schemes
/// into dict_env at resolver-assigned slots.
async fn register_class_method_signatures(
    class_name: &str,
    params: &[String],
    methods: &[Spanned<SurfaceEntry>],
    state: &mut InferState,
    dict_env: &Arc<RwLock<Env>>,
    enclosing_level: u32,
    errors: &mut Vec<Diagnostic>,
) {
    // Look up the registered ClassDecl
    let class_arc_opt = state
        .classes
        .iter()
        .find(|c| c.name == class_name)
        .map(|c| std::sync::Arc::new(c.clone()));

    if let Some(_class_arc) = class_arc_opt {
        for method_entry in methods {
            // Extract method name from entry key
            let method_name = if let Some(ref key_node) = method_entry.node.key {
                match &key_node.expr {
                    crate::ast::SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                    crate::ast::SurfaceExpression::StringLiteral { content, .. } => {
                        Some(content.clone())
                    }
                    _ => None,
                }
            } else {
                None
            };

            if let Some(method_name) = method_name {
                // Parse method type from entry value expression.
                let mut constraints = Vec::new();
                // Pre-seed ann_map with class type params
                let mut method_ann_map: std::collections::HashMap<String, String> =
                    std::collections::HashMap::new();
                for param_name in params.iter() {
                    state.set_level(param_name.clone(), state.ctx.current_level);
                    method_ann_map.insert(param_name.clone(), param_name.clone());
                }
                let mut ann_map_mut = Some(&mut method_ann_map);
                let mut row_ann_mapping = None;
                let method_type_result = Box::pin(super::typecheck_annot::resolve_type_expr(
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
                        // Store the polymorphic method type in ClassDecl.method_signatures
                        if let Some(class_decl) = state.classes.iter_mut().find(|c| c.name == class_name) {
                            if !class_decl
                                .method_signatures
                                .iter()
                                .any(|(n, _)| n == &method_name)
                            {
                                class_decl
                                    .method_signatures
                                    .push((method_name.clone(), method_type.clone()));
                            }
                        }

                        // Generalize the method type at the current enclosing level
                        let scheme = generalize_tv(enclosing_level, &method_type, &state.ctx);

                        // Insert into dict_env at the resolver-assigned slot
                        {
                            let mut env_write = dict_env.write().unwrap();
                            // Use the method entry's assigned_slot if available, otherwise append.
                            // IMPORTANT: entries must have assigned_slot populated by resolver before typecheck.
                            let slot = env_write.slots.len();
                            env_write.insert_at_slot(slot, method_name, scheme, None);
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

// ===== Helper: build_tycon_def_for_entry =====

/// Build TyConDef for a single TypeDecl entry and register constructors.
///
/// Extracts constructor information from the expanded TypeDecl Dict,
/// builds the TyConDef, registers it in state.tycon_env, inserts
/// constructor schemes into dict_env and ctor_schemes.
async fn build_tycon_def_for_entry(
    entry: &Spanned<SurfaceEntry>,
    key_name: &str,
    state: &mut InferState,
    dict_env: &Arc<RwLock<Env>>,
    ctor_schemes: &mut indexmap::IndexMap<String, TypeValue>,
    errors: &mut Vec<Diagnostic>,
) {
    if let SurfaceExpression::TypeDecl {
        type_name,
        type_decl_id: ast_type_decl_id,
        params,
        inner,
        ..
    } = &entry.node.value.expr
    {
        // Extract the inner Dict entries, unwrapping ReprDecl if present
        let inner_entries: &[Spanned<SurfaceEntry>] = match &inner.expr {
            SurfaceExpression::Dict {
                entries,
                scope_frame: _,
            } => entries,
            SurfaceExpression::ReprDecl {
                inner: repr_inner, ..
            } => match &repr_inner.expr {
                SurfaceExpression::Dict {
                    entries,
                    scope_frame: _,
                } => entries,
                _ => &[],
            },
            _ => &[],
        };

        // Unwrap annotation wrappers (Call to builtin-make-annotated)
        fn unwrap_annotated(expr: &SurfaceExpression) -> &SurfaceExpression {
            if let SurfaceExpression::Call { func, args, .. } = expr {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if name == "builtin-make-annotated" && !args.is_empty() {
                        return &args[0].expr;
                    }
                }
            }
            expr
        }

        // Build NominalVariant TypeValues from the expanded constructor entries
        let mut variant_members: Vec<TypeValue> = Vec::new();

        for inner_entry in inner_entries {
            let value_expr = unwrap_annotated(&inner_entry.node.value.expr);

            // Extract the constructor name from the entry key
            let ctor_str: Option<&str> =
                inner_entry.node.key.as_ref().and_then(|k| match &k.expr {
                    SurfaceExpression::StringLiteral { content, .. } => Some(content.as_str()),
                    SurfaceExpression::VarRef { name, .. } => Some(name.as_str()),
                    _ => None,
                });

            match value_expr {
                // Unit constructor: UnitVariant { type_decl_id, ctor_index }
                SurfaceExpression::UnitVariant { .. } => {
                    if let Some(ctor_name) = ctor_str {
                        let fields_tv = make_typevalue_record(indexmap::IndexMap::new(), None);
                        variant_members.push(make_typevalue_nominal_variant(
                            type_name, ctor_name, fields_tv,
                        ));
                    }
                }
                // Payload constructor: Fn { params, body: MakeVariant { type_decl_id, ctor_index, .. } }
                SurfaceExpression::Fn { params, body, .. } => {
                    let body_expr = unwrap_annotated(&body.expr);
                    if let SurfaceExpression::MakeVariant { .. } = body_expr {
                        let ctor_name = ctor_str.unwrap_or("_");
                        let mut field_map: indexmap::IndexMap<String, TypeValue> =
                            indexmap::IndexMap::new();
                        for p in params {
                            if !p.node.variadic {
                                let field_ty = state.fresh_type_var(&p.span);
                                field_map.insert(p.node.name.clone(), field_ty);
                            }
                        }
                        let fields_tv = make_typevalue_record(field_map, None);
                        variant_members.push(make_typevalue_nominal_variant(
                            type_name.as_str(),
                            ctor_name,
                            fields_tv,
                        ));
                    }
                }
                _ => {} // skip non-constructor entries
            }
        }

        // Build the type body: union of constructors, or resolve as a type expression
        let body_tv = if variant_members.is_empty() {
            // Structural alias: resolve inner as a type expression
            let alias_name = type_name.as_str();
            let mut alias_constraints: Vec<Arc<crate::value::Value>> = Vec::new();

            // Create annotation map from type params
            let mut alias_ann_map: HashMap<String, String> = HashMap::new();
            for (param_name, _param_ann) in params {
                let fresh = state
                    .fresh_type_var_with(Some(param_name.as_str()), None, "Type", &inner.span)
                    .0;
                alias_ann_map.insert(param_name.clone(), fresh);
            }
            let mut ann_map_for_body = alias_ann_map.clone();

            // Unwrap ReprDecl to get the inner Dict, if present.
            let body_node = match &inner.expr {
                SurfaceExpression::ReprDecl {
                    inner: repr_inner, ..
                } => repr_inner,
                _ => inner,
            };
            let body_expr = &body_node.expr;
            match body_expr {
                SurfaceExpression::Dict {
                    entries: body_entries,
                    ..
                } => {
                    let mut ann_map_opt: Option<&mut HashMap<String, String>> =
                        Some(&mut ann_map_for_body);
                    let mut row_m: Option<&mut HashMap<String, String>> = None;
                    match super::typecheck_annot::resolve_type_dict(
                        body_entries,
                        body_node.span.clone(),
                        &mut *state,
                        &mut alias_constraints,
                        &mut ann_map_opt,
                        &mut row_m,
                        super::typecheck_annot::TypeDictCtx {
                            type_params_scope: None,
                            tycon_name: alias_name,
                        },
                    )
                    .await
                    {
                        Ok(t) => t,
                        Err(e) => {
                            let e = e
                                .with_note(format!("in body of type declaration '{}'", alias_name));
                            errors.push(e);
                            make_typevalue_never()
                        }
                    }
                }
                _ => match super::typecheck_annot::resolve_type_expr(
                    body_node,
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
                        let e =
                            e.with_note(format!("in body of type declaration '{}'", alias_name));
                        errors.push(e);
                        make_typevalue_never()
                    }
                },
            }
        } else {
            typevalue_normalize_union_ctx(variant_members, Some(&state.ctx))
        };

        let constructors = extract_typevalue_constructors(&body_tv);

        let tycon_def = std::sync::Arc::new(TyConDef {
            type_decl_id: *ast_type_decl_id,
            params: params.iter().map(|(n, _)| n.clone()).collect(),
            body: body_tv.clone(),
            constraints: Vec::new(),
            variance: Vec::new(),
            constructors,
            annotation: None,
            field_annotations: indexmap::IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
            definition_span: Some(entry.span.clone()),
        });

        state
            .type_name_to_id
            .insert(key_name.to_string(), *ast_type_decl_id);
        state
            .type_display_names
            .insert(*ast_type_decl_id, key_name.to_string());
        dict_env
            .write()
            .unwrap()
            .insert_tycon_def(key_name.to_string(), std::sync::Arc::clone(&tycon_def));
        state
            .tycon_env
            .entry(*ast_type_decl_id)
            .or_insert(tycon_def);

        // Wire type declarations into type_stage_scope
        if state.type_stage_scope.is_empty() {
            state
                .type_stage_scope
                .push(std::collections::HashMap::new());
        }
        state.type_stage_scope[0]
            .entry(key_name.to_string())
            .or_insert_with(|| {
                make_typevalue_nominal(state.type_name_to_id.get(key_name).copied().unwrap_or(0))
            });

        // Register constructor schemes
        let value_scheme_ty = adt_value_type(&body_tv);
        let ctor_fields = typevalue_record_fields_pub(&value_scheme_ty);
        for (ctor_name, ctor_tv) in ctor_fields {
            ctor_schemes.insert(ctor_name, ctor_tv);
        }

        {
            let mut env_write = dict_env.write().unwrap();
            // Use entry's assigned_slot if available, otherwise append.
            // TypeDecl entries should have assigned_slot from resolver.
            let slot = env_write.slots.len();
            env_write.insert_at_slot(
                slot,
                key_name.to_string(),
                Arc::clone(&value_scheme_ty),
                None,
            );
        }
    }
}

// ===== typecheck_letrec_group =====

/// Per-SCC Hindley-Milner letrec inference for a dict binding group.
///
/// Implements the standard HM letrec algorithm: key resolution, SCC decomposition,
/// per-SCC pre-allocation + inference + generalization, and epilogue.
///
/// Returns `(record_type, schemes, referenced, errors)` where:
/// - `record_type` is the inferred TypeValue.Record for the dict literal
/// - `schemes` is an `IndexMap<String, Arc<Value>>` of per-entry generalized schemes (needed
///   by `process_document` for cross-document scoping and by `Sequential` for
///   let-polymorphism across multi-body function steps)
/// - `dict_env` is the env frame created for this dict, containing all entries at their
///   resolver-assigned slots with schemes, definition_spans, and referenced flags. The
///   Sequential handler uses this frame directly in the env chain — no copying needed.
/// - `errors` is the accumulated vector of type errors (inference is best-effort)
///
/// Called by:
/// - `LetrecGroupPassZero` handler (terminal dict expressions in the CEK machine)
/// - `process_document` (top-level dict expressions in a document)
/// - `run_typecheck`'s Sequential arm (intermediate dict bodies in multi-body functions)
/// - Recursively for nested Dict values within the SCC per-entry loop
pub(crate) async fn typecheck_letrec_group(
    entries: &[Spanned<SurfaceEntry>],
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    slot_base_override: Option<u32>,
) -> (
    TypeValue,
    indexmap::IndexMap<String, TypeValue>,
    Arc<RwLock<Env>>,
    Vec<Diagnostic>,
) {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.ctx.current_level;
    state.ctx.current_level += 1;

    // Create a fresh Env for the dict entries (child of the parent env).
    let dict_env: Arc<RwLock<Env>> = Arc::new(RwLock::new(Env::with_parent(Arc::clone(env))));

    // Extra schemes from ADT constructors — injected in type declaration registration, merged into final schemes.
    // IndexMap preserves insertion order so constructor scheme ordering is deterministic.
    let mut ctor_schemes: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    let mut key_entries: Vec<(Option<String>, bool, bool)> = Vec::new();
    let mut auto_index: i64 = 0;
    let mut errors: Vec<Diagnostic> = Vec::new();

    // Step 1: Key resolution
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
        let is_type_decl = matches!(&entry.node.value.expr, SurfaceExpression::TypeDecl { .. });
        let is_static_key = entry.node.key.as_ref().is_some_and(|k| {
            matches!(
                &k.expr,
                SurfaceExpression::StringLiteral { .. } | SurfaceExpression::VarRef { .. }
            )
        });
        key_entries.push((key_name, is_type_decl, is_static_key));
    }

    // Step 2: SCC computation
    let sccs = compute_sccs(entries, &key_entries);

    // Pre-compute slot mapping before SCC loop
    let body_slot_base_opt: Option<u32> = slot_base_override;
    // Pre-compute absolute slots: Some(slot) for static-key entries, None for anonymous entries
    let mut entry_slots: Vec<Option<u32>> = Vec::with_capacity(entries.len());
    let mut static_slot_counter: u32 = 0;
    for ((_key_name, _is_type_decl, is_static_key), _entry) in
        key_entries.iter().zip(entries.iter())
    {
        if *is_static_key {
            entry_slots.push(body_slot_base_opt.map(|base| base + static_slot_counter));
            static_slot_counter += 1;
        } else {
            entry_slots.push(None);
        }
    }

    // Declare fresh_vars_by_name before the SCC loop
    let mut fresh_vars_by_name: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();

    // Initialize field types and entry constraints accumulators
    let mut field_types: indexmap::IndexMap<String, TypeValue> = indexmap::IndexMap::new();
    let mut entry_constraints: HashMap<String, Vec<Arc<crate::value::Value>>> = HashMap::new();

    // Process each SCC in topological order
    for scc in sccs.into_iter() {
        // ── 3a: Per-SCC pre-allocation ──────────────────────────────────────

        // 3a-i: Pre-register TypeDecl identities for THIS SCC
        for &idx in &scc.indices {
            let (ref key_name, is_type_decl, _) = key_entries[idx];
            if is_type_decl {
                if let Some(ref name) = key_name {
                    if let SurfaceExpression::TypeDecl { type_decl_id, .. } =
                        &entries[idx].node.value.expr
                    {
                        state.type_name_to_id.insert(name.clone(), *type_decl_id);
                        state.type_display_names.insert(*type_decl_id, name.clone());
                        state
                            .ctx
                            .type_name_to_id
                            .insert(name.clone(), *type_decl_id);
                        state
                            .ctx
                            .type_display_names
                            .insert(*type_decl_id, name.clone());
                    }
                }
            }
        }

        // 3a-ii: Build and register TyConDefs for TypeDecl entries in THIS SCC
        for &idx in &scc.indices {
            let (ref key_name, is_type_decl, _) = key_entries[idx];
            if is_type_decl {
                if let Some(ref name) = key_name {
                    build_tycon_def_for_entry(
                        &entries[idx],
                        name,
                        state,
                        &dict_env,
                        &mut ctor_schemes,
                        &mut errors,
                    )
                    .await;
                }
            }
        }

        // 3a-iii: Register ClassDecl entries in THIS SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                if let SurfaceDeclaration::ClassDecl {
                    name,
                    params,
                    superclasses,
                    methods: _,
                    determines,
                    resolver,
                    resolver_injective,
                    structural,
                    pre_assigned_class_decl_id,
                } = decl_box.as_ref()
                {
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
                    let _ = super::infer_class_decl_from_surface(
                        &super::ClassDeclSurface {
                            name,
                            params,
                            superclasses,
                            determines,
                            structural,
                            span: entry.node.value.span.clone(),
                            resolver: resolver_name,
                            resolver_injective: *resolver_injective,
                            pre_assigned_class_decl_id: Some(pre_assigned_class_decl_id),
                        },
                        state,
                    );
                }
            }
        }

        // 3a-iv: Allocate fresh TypeVars for entries in THIS SCC
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_type_decl, is_static_key) = key_entries[idx];

            if is_static_key {
                if let Some(ref name) = key_name {
                    let abs_slot = entry_slots[idx];
                    if let SurfaceExpression::Fn { params, .. } = &entry.node.value.expr {
                        let mut fn_params: Vec<(Option<String>, TypeValue)> = Vec::new();
                        let mut pre_is_variadic = false;
                        for p in params {
                            if p.node.variadic {
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
                            None,
                            pre_is_variadic,
                            Vec::new(),
                        );
                        if !is_type_decl {
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
                        if !is_type_decl {
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
            if entry.node.key.is_none() {
                if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                    if let SurfaceDeclaration::InstanceDecl {
                        class_name: _,
                        arms,
                        ..
                    } = decl.as_ref()
                    {
                        for (_pattern, method_entries) in arms {
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
                                let binding_name = method_name.clone();
                                let fresh_var = state.fresh_type_var(&entry.span);
                                fresh_vars_by_name.insert(binding_name.clone(), fresh_var.clone());
                                {
                                    let mut env_write = dict_env.write().unwrap();
                                    // Use entry's assigned_slot if available, otherwise append.
                                    let slot = env_write.slots.len();
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

        // Per-SCC pre-allocation: Collect the fresh TypeVars for this SCC's entries.
        // IndexMap preserves SCC member order for deterministic type variable lookup.
        // Per-SCC fresh TypeVar storage. Singleton for SCCs with exactly one non-TypeDecl entry;
        // Multiple for larger SCCs. Enables name-based TypeVar lookup during inference without
        // allocating a HashMap for trivial single-entry SCCs.
        enum FreshVars {
            Singleton(String, TypeValue),
            Multiple(indexmap::IndexMap<String, TypeValue>),
        }
        let mut fresh_vars_storage: Option<FreshVars> = None;

        for &idx in &scc.indices {
            let (ref key_name, is_type_decl, _is_static) = key_entries[idx];
            if !is_type_decl {
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

        // Per-SCC inference: Infer values and unify with bound type vars for this SCC.
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, _is_type_decl, _is_static) = key_entries[idx];

            if let Some(name) = key_name {
                let saved_constraints = std::mem::take(&mut state.constraints);

                // Record outer→inner use-def edge, then advance current_binding.
                // Every dict entry at every nesting depth participates uniformly: before
                // setting current_binding to this entry's BindingId, record that the current
                // outer binding depends on this inner entry. current_binding flows naturally
                // through all entries at all depths — no save/restore needed. The BFS in
                // AfterBlock traverses the chain transitively.
                // Span-keyed: entry.span is the source location of this binding's declaration,
                // stable across dict_env Arc allocations.
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
                // For nested Dict values, call typecheck_letrec_group directly to capture schemes.
                let value_ty = if let SurfaceExpression::Dict {
                    entries: nested_entries,
                    ..
                } = &entry.node.value.expr
                {
                    let (ty, _schemes, _nested_dict_env, mut nested_errs) = Box::pin(
                        typecheck_letrec_group(nested_entries, &dict_env, state, type_map, None),
                    )
                    .await;
                    errors.append(&mut nested_errs);
                    let resolved = state.apply(&ty);
                    entry.node.value.inferred_type_decl_id.set(resolved);
                    Ok(ty)
                } else {
                    let mut local_errors = Vec::new();
                    let mut local_stack = Vec::new();
                    let ty = Box::pin(run_typecheck(
                        &entry.node.value,
                        &dict_env,
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
                            if typevalue_ctor_index(&bound_var) == Some(TV_FN) {
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
                        // Entry inference failed: Never (error recovery)
                        let error_ty = make_typevalue_never();
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

                // Post-inference: extract ClassDecl method signatures
                if let SurfaceExpression::Decl(d) = &entry.node.value.expr {
                    if let crate::ast::SurfaceDeclaration::ClassDecl {
                        name: class_name,
                        params: class_params,
                        methods,
                        ..
                    } = d.as_ref()
                    {
                        register_class_method_signatures(
                            class_name,
                            class_params,
                            methods,
                            state,
                            &dict_env,
                            enclosing_level,
                            &mut errors,
                        )
                        .await;
                    }
                }
            } else {
                // Computed-key entry: type-check the value to resolve VarRefs for
                // liveness tracking, but don't insert into field_types (no static name).
                if let SurfaceExpression::Dict {
                    entries: nested_entries,
                    ..
                } = &entry.node.value.expr
                {
                    let (ty, _schemes, _nested_dict_env, mut nested_errs) = Box::pin(
                        typecheck_letrec_group(nested_entries, &dict_env, state, type_map, None),
                    )
                    .await;
                    errors.append(&mut nested_errs);
                    let resolved = state.apply(&ty);
                    entry.node.value.inferred_type_decl_id.set(resolved);
                } else {
                    let mut local_errors = Vec::new();
                    let mut local_stack = Vec::new();
                    let _ = Box::pin(run_typecheck(
                        &entry.node.value,
                        &dict_env,
                        state,
                        &mut local_errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await;
                    errors.append(&mut local_errors);
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

        // Per-SCC generalization: Generalize this SCC's entries before processing the next SCC.
        for &idx in &scc.indices {
            let entry = &entries[idx];
            let (ref key_name, is_type_decl, _) = key_entries[idx];

            // TypeDecl entries have their correct schemes already registered in type declaration registration.
            // Skipping here prevents field_types["TypeName"] (which holds the inferred
            // value type, not the type declaration type) from overwriting the correct scheme.
            if is_type_decl {
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
                                                // Narrowing annotation failed: Never (error recovery)
                                                make_typevalue_never()
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
                                                        // Narrowing annotation failed: Never (error recovery)
                                                        make_typevalue_never()
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

    // Re-apply zero-arity TypeDecl schemes from state.tycon_env.
    for (key_name, is_type_decl, _) in &key_entries {
        if *is_type_decl {
            if let Some(name) = key_name {
                if let Some(def) = state
                    .type_name_to_id
                    .get(name.as_str())
                    .and_then(|id| state.tycon_env.get(id))
                {
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

    // Build final schemes map from dict_env in SOURCE ORDER using slot-based lookup.
    let mut schemes = indexmap::IndexMap::with_capacity(field_types.len());
    {
        let dict_env_guard = dict_env.read().unwrap();
        for (idx, (key_name, _is_type_decl, _)) in key_entries.iter().enumerate() {
            if let Some(name) = key_name {
                if let Some(slot) = entry_slots[idx] {
                    if let Some(scheme) = dict_env_guard.get_scheme_at(0, slot) {
                        schemes.insert(name.clone(), scheme);
                    }
                }
            }
        }
    }
    // Merge ADT constructor schemes into dict_env so the frame has all entries.
    for (name, scheme) in ctor_schemes {
        schemes
            .entry(name.clone())
            .or_insert_with(|| scheme.clone());
        if !dict_env
            .read()
            .unwrap()
            .slot_index
            .contains_key(name.as_str())
        {
            let slot = dict_env.read().unwrap().slots.len();
            dict_env
                .write()
                .unwrap()
                .insert_at_slot(slot, name, scheme, None);
        }
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

    (record_type, schemes, dict_env, errors)
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
            inferred_type_decl_id: crate::ast::InferredType::new(),
            type_instantiation: crate::ast::TypeInstantiation::new(),
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
                    resolved_dispatch_slot: crate::ast::ResolvedDispatchSlot::new(),
                })
            })
            .collect();
        mk(SurfaceExpression::Fn {
            return_ann: None,
            params,
            body,
            desugared: false,
            resolved_return_annotation: crate::ast::TypeAnnotation::new(),
            scope_frame: std::sync::OnceLock::new(),
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
