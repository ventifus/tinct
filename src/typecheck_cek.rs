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

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    class_decl_name, node_id, Annotation, Span, Spanned, SurfaceDeclaration, SurfaceEntry,
    SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode, STANDARD_ANN_KEYS,
};
use crate::coverage;
use crate::env::Env;
use crate::error::{DiagnosticLevel, TypeDiagnostic};
use crate::type_def::{Label, Row, RowTail, TyConDef};
use crate::type_infer::Substitution;
use crate::types::{
    constrain, generalize, generalize_with_doc, instantiate_at_level, instantiate_scheme,
    resolve_has_field, Constraint, InferState, Kind, Type, TypeScheme,
};

use super::{typecheck_annot, typecheck_call, typecheck_narrow, TypeMap};

// ===== Action enum =====

/// Action returned by `infer_step` and `apply_cont`.
///
/// `Done(ty)` means the current sub-expression has been fully inferred.
/// `Eval(node, env)` means inference should continue by evaluating `node` in `env`.
pub(crate) enum TypeCheckAction {
    Done(Type),
    Eval(Arc<SurfaceNode>, Arc<RwLock<Env>>),
}

// ===== SCC =====

/// Strongly Connected Component — a group of mutually dependent bindings.
#[derive(Clone)]
pub(crate) struct Scc {
    /// Indices into the entries array.
    pub(crate) indices: Vec<usize>,
}

/// Type alias for the instantiated function signature tuple used in `CallFunc` handling:
/// `(params, return_type, typed_variadics, rest, required_count)`.
type InstFuncSig = (
    Vec<(Option<String>, crate::types::Type)>,
    crate::types::Type,
    Vec<(String, crate::types::Type)>,
    Option<Box<(String, crate::types::Type)>>,
    usize,
);

/// Instantiated function signature — groups the five signature fields that always travel
/// together through call-checking helpers. Owned data; no lifetime parameters.
struct FnSig {
    params: Vec<(Option<String>, Type)>,
    ret: Type,
    typed_variadics: Vec<(String, Type)>,
    rest: Option<Box<(String, Type)>>,
    required_count: usize,
}

/// Shared mutable context threaded through type-checking helpers.
/// Groups the three machinery parameters to keep function argument counts below threshold.
struct TypeCheckCtx<'a, 'b> {
    state: &'a mut InferState,
    errors: &'a mut Vec<TypeDiagnostic>,
    type_map: &'a mut Option<&'b mut TypeMap>,
}

// ===== TypeCheckCont enum =====

/// Explicit continuation stack for the type checker CEK machine.
///
/// Each variant stores the data needed to resume type checking after a child expression
/// has been inferred. The continuation stack replaces recursive calls to `infer_step`.
pub(crate) enum TypeCheckCont {
    /// Inferred a function body — restore saved level/expected_return and build fn type.
    FnBody {
        saved_level: u32,
        saved_expected_return: Option<Type>,
        /// Pre-resolved return annotation type (overrides body type when concrete).
        return_ann: Option<Type>,
        /// Resolved fixed param types (non-variadic).
        params: Vec<(Option<String>, Type)>,
        /// Typed variadic buckets: (name, Seq[T]) in declaration order.
        typed_variadics: Vec<(String, Type)>,
        /// Untyped variadic fallback: (name, TypeVar_whole_dict).
        rest: Option<Box<(String, Type)>>,
        required_count: usize,
        node_span: Span,
    },

    /// Inferred the function expression in a call — start processing arguments.
    CallFunc {
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
        /// Types of all positional args inferred so far.
        accumulated_arg_types: Vec<Type>,
        /// All positional arg nodes (full list, not just remaining) — used for type_guard.
        arg_nodes: Vec<Arc<SurfaceNode>>,
        /// Param types from the instantiated function type.
        param_types: Vec<(Option<String>, Type)>,
        fn_ret: Type,
        /// Typed variadic buckets in declaration order: (name, Seq[T]).
        typed_variadics: Vec<(String, Type)>,
        /// Untyped variadic fallback: (name, TypeVar_whole_dict).
        rest: Option<Box<(String, Type)>>,
        fn_required: usize,
        env: Arc<RwLock<Env>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        span: Span,
        call_node: Arc<SurfaceNode>,
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
        accumulated_types: Vec<Type>,
        scrutinee_ty: Type,
        remaining_scrutinee: Type,
        span: Span,
    },

    /// Pass 0 (key resolution) complete — run full multi-pass dict inference (T-1644).
    ///
    /// Pushed by the `Dict` arm of `infer_step`. The handler calls `run_typecheck_dict`
    /// with all fields needed for the full Passes 1–4 dict inference algorithm.
    DictPassZero {
        /// Dict entries — passed directly to run_typecheck_dict (avoids re-parse).
        entries: Vec<Spanned<SurfaceEntry>>,
        env: Arc<RwLock<Env>>,
    },

    /// Inferred a non-Dict intermediate body in a Sequential — extend env and continue.
    ///
    /// Pushed by `infer_step::Sequential` when encountering a non-Dict intermediate body.
    /// This replaces the `Box::pin(run_typecheck(...))` recursive call — the CEK machine
    /// must remain fully iterative (Ager et al. 2003).
    SequentialNonDictIntermediate {
        /// Span of the just-evaluated intermediate (for `not_a_record` error attribution).
        intermediate_span: Span,
        /// Remaining intermediate bodies after the current one.
        remaining_intermediates: Vec<Arc<SurfaceNode>>,
        /// The last expression — evaluated after all intermediates are processed.
        last: Arc<SurfaceNode>,
        /// Accumulated env before extending with this intermediate's result.
        env: Arc<RwLock<Env>>,
        /// `state.level` at push time — restored after evaluation.
        enclosing_level: u32,
    },

    /// Inferred the inner expression of a TypeAssert — validate against expected type.
    TypeAssertInner {
        expected: Type,
        has_default: bool,
        default_node: Option<Arc<SurfaceNode>>,
        env: Arc<RwLock<Env>>,
        span: Span,
    },

    /// Inferred the base expression of a Field access — look up the field type.
    ///
    /// Resolves the field type from the inferred base type and returns it via
    /// `TypeCheckAction::Done`. All dot-access desugars to `builtin-get` (key-based lookup).
    FieldBase {
        field: crate::ast::DotKey,
        span: Span,
    },

    /// Inferred the inner expression of an Unquote — return its type.
    Unquote,

    /// Inferred the inner expression of an UnquoteSplice — return Unknown.
    UnquoteSplice,
}

// ===== typecheck_for_errors =====

/// Run type inference for side effects only. Discards the inferred Type.
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
    errors: &mut Vec<TypeDiagnostic>,
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
/// Returns the final inferred type when the stack is empty.
pub(crate) async fn run_typecheck(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> Type {
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

/// Record an inferred type into the type_map for LSP hover.
fn record_type_map(type_map: &mut Option<&mut TypeMap>, span: &Span, ty: &Type) {
    if let Some(ref mut map) = type_map {
        let key = (span.start_line, span.start_col, span.end_line, span.end_col);
        let simplified = Type::simplify_type(ty.clone());
        map.insert(key, simplified);
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
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    match &node.expr {
        // ===== Leaf expressions =====
        SurfaceExpression::Int(n) => TypeCheckAction::Done(Type::IntLiteral(*n)),
        SurfaceExpression::Float(_) => TypeCheckAction::Done(Type::Float),
        SurfaceExpression::StringLiteral { content, .. } => {
            TypeCheckAction::Done(Type::StringLiteral(content.clone()))
        }
        SurfaceExpression::U64(_) => TypeCheckAction::Done(Type::Int),

        // Placeholder: typed hole — infer as a fresh TypeVar (unifies with context)
        SurfaceExpression::Placeholder(..) => {
            TypeCheckAction::Done(state.fresh_type_var(&node.span))
        }

        SurfaceExpression::Quote(_inner) => TypeCheckAction::Done(Type::Dict(Row {
            fields: indexmap::IndexMap::new(),
            tail: RowTail::Empty,
        })),

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

        // ===== Field access — compound: evaluate base first =====
        SurfaceExpression::Field { expr, field, .. } => match expr {
            None => TypeCheckAction::Done(Type::Unknown),
            Some(base) => {
                stack.push(TypeCheckCont::FieldBase {
                    field: field.clone(),
                    span: node.span.clone(),
                });
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
            let mut constraints: Vec<Constraint> = Vec::new();
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
                        env: Arc::clone(env),
                        span: node.span.clone(),
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
                return TypeCheckAction::Done(Type::Dict(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: RowTail::Empty,
                }));
            }
            if exprs.len() == 1 {
                return TypeCheckAction::Eval(Arc::clone(&exprs[0]), Arc::clone(env));
            }

            // Process all intermediate bodies (all but last) inline, extending env after each
            let mut current_env = Arc::clone(env);
            let intermediates = &exprs[0..exprs.len() - 1];
            let last = &exprs[exprs.len() - 1];

            for (i, intermediate) in intermediates.iter().enumerate() {
                // Check if this is a dict — if so, use run_typecheck_dict for proper letrec
                if let SurfaceExpression::Dict(entries) = &intermediate.expr {
                    let (_, schemes, mut dict_errs) =
                        run_typecheck_dict(entries, &current_env, state, type_map).await;
                    errors.append(&mut dict_errs);

                    // Extend env with schemes (preserving let-polymorphism)
                    let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
                    for (name, scheme) in &schemes {
                        new_env_inner.insert_scheme_named_only(name.clone(), scheme.clone());
                    }
                    current_env = Arc::new(RwLock::new(new_env_inner));
                } else {
                    // Non-Dict intermediate: push continuation, return Eval — no recursion.
                    // The AfterSequentialNonDictIntermediate handler resumes after evaluation,
                    // extends env, and continues processing remaining intermediates.
                    let enc_level = state.level;
                    state.level += 1;
                    stack.push(TypeCheckCont::SequentialNonDictIntermediate {
                        intermediate_span: intermediate.span.clone(),
                        remaining_intermediates: intermediates[i + 1..]
                            .iter()
                            .map(Arc::clone)
                            .collect(),
                        last: Arc::clone(last),
                        env: Arc::clone(&current_env),
                        enclosing_level: enc_level,
                    });
                    return TypeCheckAction::Eval(Arc::clone(intermediate), current_env);
                }
            }

            // All intermediates were Dict — return Eval for the last expression
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
            // Special-case: do-infer sentinel
            if let SurfaceExpression::Field {
                expr: Some(da_target),
                ..
            } = &func.expr
            {
                if let SurfaceExpression::VarRef { name, .. } = &da_target.expr {
                    if name.starts_with("ℊꜱʏᴍ⧼do-infer⧽") && named_args.is_empty() {
                        return TypeCheckAction::Done(Type::Unknown);
                    }
                }
            }

            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                // Special-case: builtin-get / get / get?
                if (name == "builtin-get" || name == "get" || name == "get?")
                    && args.len() == 2
                    && named_args.is_empty()
                {
                    let ty = infer_get_call(name, &args[0], &args[1], env, state, errors, type_map)
                        .await;
                    return TypeCheckAction::Done(ty);
                }

                // Special-case: get-in
                if name == "get-in" && args.len() == 2 && named_args.is_empty() {
                    let ty =
                        infer_get_in_call(&args[0], &args[1], env, state, errors, type_map).await;
                    return TypeCheckAction::Done(ty);
                }
            }

            // General call: push AfterCallFunc, evaluate func
            let args_cloned: Vec<Arc<SurfaceNode>> = args.iter().map(Arc::clone).collect();
            let named_args_cloned: Vec<Spanned<SurfaceNamedArg>> = named_args.to_vec();
            stack.push(TypeCheckCont::CallFunc {
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
        // to trigger apply_cont — there is no child node to evaluate at this point (T-1644).
        SurfaceExpression::Dict(entries) => {
            stack.push(TypeCheckCont::DictPassZero {
                entries: entries.to_vec(),
                env: Arc::clone(env),
            });
            TypeCheckAction::Done(Type::Unknown)
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

        // ===== CaseArm — set up let bindings, eval body =====
        SurfaceExpression::CaseArm {
            let_bindings,
            pattern: _,
            body,
        } => {
            let arm_env = build_case_arm_env(let_bindings, env, state, node);
            TypeCheckAction::Eval(Arc::clone(body), arm_env)
        }

        // ===== Decl — call declaration helpers directly (T-1641) =====
        SurfaceExpression::Decl(decl_box) => {
            // Call infer_class_decl_from_surface and infer_instance_decl_from_surface directly
            // now that they are pub(crate). TypeAlias declarations in expression position have
            // no runtime type (alias body validation occurs in Pass 2 of run_typecheck_dict).
            let result: Result<Type, Vec<TypeDiagnostic>> = match decl_box.as_ref() {
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
                    let sc_flat: Vec<(String, String)> = superclasses
                        .iter()
                        .flat_map(|(sc_name, sc_params)| {
                            sc_params
                                .iter()
                                .map(|p| (sc_name.clone(), p.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    super::infer_class_decl_from_surface(
                        &super::ClassDeclSurface {
                            name,
                            params,
                            superclasses: &sc_flat,
                            determines,
                            resolver,
                            resolver_injective: *resolver_injective,
                            structural,
                            span: node.span.clone(),
                        },
                        state,
                    )
                }
                SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                    Box::pin(super::infer_instance_decl_from_surface(
                        &class_decl_name(class_name),
                        arms,
                        node.span.clone(),
                        env,
                        state,
                        type_map,
                    ))
                    .await
                }
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Type alias declarations in expression position have no runtime type.
                    // Alias body validation occurs in Pass 2 of run_typecheck_dict.
                    Ok(Type::Any)
                }
                _ => Err(vec![TypeDiagnostic::error(
                    "type-error",
                    "unexpected declaration in expression position",
                    node.span.clone(),
                )]),
            };
            match result {
                Ok(t) => TypeCheckAction::Done(t),
                Err(mut errs) => {
                    errors.append(&mut errs);
                    TypeCheckAction::Done(Type::error_note("declaration inference error"))
                }
            }
        }

        // Parse-error node: the error was already recorded during parsing;
        // silently return Unknown here to avoid spurious secondary diagnostics.
        SurfaceExpression::Error(_) => TypeCheckAction::Done(Type::Unknown),

        _ => {
            let msg = format!(
                "unexpected {} in this context",
                crate::surface_fields::surface_expr_tag(&node.expr)
            );
            errors.push(TypeDiagnostic::error(
                "type-error",
                msg.clone(),
                node.span.clone(),
            ));
            TypeCheckAction::Done(Type::error_note(msg))
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
    child_ty: Type,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    match cont {
        // ===== AfterFnBody =====
        TypeCheckCont::FnBody {
            saved_level,
            saved_expected_return,
            return_ann,
            params,
            typed_variadics,
            rest,
            required_count,
            node_span,
        } => {
            state.level = saved_level;
            state.expected_return = saved_expected_return;

            let fn_ret_ty = match return_ann {
                Some(declared_ret) => {
                    let is_checkable_primitive = matches!(
                        declared_ret,
                        Type::Int | Type::Float | Type::Str | Type::Bytes
                    );
                    if is_checkable_primitive {
                        let body_resolved = state.subst.apply(&child_ty);
                        let body_is_concrete =
                            !matches!(body_resolved, Type::Unknown | Type::Any | Type::TypeVar(..));
                        if body_is_concrete
                            && !Type::is_consistent_subtype(
                                &body_resolved,
                                &declared_ret,
                                Some(&state.tycon_env),
                            )
                        {
                            errors.push(TypeDiagnostic::error(
                                "unification-failure",
                                format!("cannot unify {} with {}", &declared_ret, &body_resolved),
                                node_span.clone(),
                            ));
                        }
                    }
                    // For intersection annotations: validate that the inferred body type is
                    // consistent with each Function member of the intersection.
                    // This catches cases where a fn@[all Fn1 Fn2] annotation is inconsistent
                    // with the actual inferred body type.
                    if let Type::Intersection(members) = &declared_ret {
                        let body_resolved = state.subst.apply(&child_ty);
                        let body_is_concrete =
                            !matches!(body_resolved, Type::Unknown | Type::Any | Type::TypeVar(..));
                        if body_is_concrete {
                            for member in members {
                                if matches!(member, Type::Function { .. })
                                    && !Type::is_consistent_subtype(
                                        &body_resolved,
                                        member,
                                        Some(&state.tycon_env),
                                    )
                                {
                                    errors.push(TypeDiagnostic::error(
                                        "unification-failure",
                                        format!("cannot unify {} with {}", member, &body_resolved),
                                        node_span.clone(),
                                    ));
                                }
                            }
                        }
                    }

                    // T-1709: Emit diagnostic for explicit @Unknown return annotation
                    if matches!(&declared_ret, Type::Unknown) {
                        state.diagnostics.push(TypeDiagnostic {
                            level: DiagnosticLevel::Info,
                            kind: "explicit-unknown",
                            message:
                                "explicit @Unknown return annotation — type is not statically known"
                                    .to_string(),
                            spans: vec![(node_span.clone(), String::new())],
                            notes: vec![],
                            help: vec![],
                        });
                    }

                    // T-1710: Emit diagnostic for overbroad return annotation
                    {
                        let body_resolved = state.subst.apply(&child_ty);
                        if !matches!(body_resolved, Type::Unknown | Type::Any | Type::TypeVar(..))
                            && !matches!(declared_ret, Type::Unknown | Type::Any)
                        {
                            let is_sub = Type::is_subtype(
                                &body_resolved,
                                &declared_ret,
                                Some(&state.tycon_env),
                            );
                            let is_super = Type::is_subtype(
                                &declared_ret,
                                &body_resolved,
                                Some(&state.tycon_env),
                            );
                            if is_sub && !is_super {
                                state.diagnostics.push(TypeDiagnostic {
                                    level: DiagnosticLevel::Info,
                                    kind: "overbroad-annotation",
                                    message: format!(
                                        "return type declared as {}, inferred as {}",
                                        declared_ret, body_resolved
                                    ),
                                    spans: vec![(node_span.clone(), String::new())],
                                    notes: vec![],
                                    help: vec![],
                                });
                            }
                        }
                    }

                    match &declared_ret {
                        Type::Unknown => child_ty,
                        _ => declared_ret,
                    }
                }
                None => child_ty,
            };

            let fn_type = Type::Function {
                params,
                ret: Box::new(fn_ret_ty),
                typed_variadics,
                rest,
                required_count,
            };
            // Exhaustiveness check for typed variadic params.
            // A function with typed variadic buckets (...x@T) but no untyped fallback (...else)
            // cannot handle args of types not covered by any bucket — same as a non-exhaustive
            // match with no wildcard arm. This is a definition-time advisory warning; it does not
            // prevent the function from being called with matching-typed args.
            if let Type::Function {
                ref typed_variadics,
                ref rest,
                ..
            } = fn_type
            {
                if !typed_variadics.is_empty() && rest.is_none() {
                    let covered = Type::normalize_union(
                        typed_variadics.iter().map(|(_, t)| t.clone()).collect(),
                    );
                    errors.push(TypeDiagnostic::error("type-error",
                        format!(
                            "non-exhaustive variadic type dispatch: typed buckets cover {} but no fallback (...rest) handles other types — add ...rest for a wildcard bucket",
                            covered
                        ),
                        node_span.clone(),
                    ));
                }
            }

            // Record the function type with the Fn node's span for LSP hover.
            record_type_map(type_map, &node_span, &fn_type);
            TypeCheckAction::Done(fn_type)
        }

        // ===== CallFunc =====
        TypeCheckCont::CallFunc {
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
            apply_cont_call_func(func_ty, args, named_args, env, call_node, &mut ctx, stack).await
        }

        // ===== AfterCallArg =====
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
                &mut ctx,
            )
            .await
        }

        // ===== AfterMatchScrutinee =====
        TypeCheckCont::MatchScrutinee { arms, env, span } => {
            let scrutinee_ty = state.subst.apply(&child_ty);
            if arms.is_empty() {
                return TypeCheckAction::Done(Type::Unknown);
            }

            // Run exhaustiveness checking once upfront (using all arms).
            run_match_exhaustiveness_check(&scrutinee_ty, &arms, &span, state, errors);

            // Set up the first arm's environment iteratively; subsequent arms via AfterMatchArm.
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
                    // Arms is empty after setup — shouldn't happen since we checked above
                    TypeCheckAction::Done(Type::Unknown)
                }
                Some((arm_env, next_remaining_scrutinee)) => {
                    let remaining_arms: Vec<SurfaceMatchArm> = arms[1..].to_vec();
                    stack.push(TypeCheckCont::MatchArm {
                        remaining_arms,
                        env,
                        accumulated_types: Vec::new(),
                        scrutinee_ty,
                        remaining_scrutinee: next_remaining_scrutinee,
                        span,
                    });
                    TypeCheckAction::Eval(Arc::clone(arms[0].body_expr()), arm_env)
                }
            }
        }

        // ===== AfterMatchArm — process one arm body result and continue with next arm =====
        TypeCheckCont::MatchArm {
            remaining_arms,
            env,
            mut accumulated_types,
            scrutinee_ty,
            remaining_scrutinee,
            span,
        } => {
            accumulated_types.push(child_ty);

            if remaining_arms.is_empty() {
                // All arms done — compute union type.
                let match_ty = if accumulated_types.is_empty() {
                    Type::Unknown
                } else {
                    Type::simplify_type(Type::normalize_union(accumulated_types))
                };
                TypeCheckAction::Done(match_ty)
            } else {
                // Set up environment for the next arm and push another AfterMatchArm.
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
                        // Setup failed — treat as unknown and stop
                        let match_ty =
                            Type::simplify_type(Type::normalize_union(accumulated_types));
                        TypeCheckAction::Done(match_ty)
                    }
                    Some((arm_env, next_remaining_scrutinee)) => {
                        let next_remaining: Vec<SurfaceMatchArm> = remaining_arms[1..].to_vec();
                        stack.push(TypeCheckCont::MatchArm {
                            remaining_arms: next_remaining,
                            env,
                            accumulated_types,
                            scrutinee_ty,
                            remaining_scrutinee: next_remaining_scrutinee,
                            span,
                        });
                        TypeCheckAction::Eval(Arc::clone(remaining_arms[0].body_expr()), arm_env)
                    }
                }
            }
        }

        // ===== AfterSequentialNonDictIntermediate =====
        TypeCheckCont::SequentialNonDictIntermediate {
            intermediate_span,
            remaining_intermediates,
            last,
            env,
            enclosing_level,
        } => {
            // Restore level (was incremented before pushing this continuation).
            state.level = enclosing_level;

            // Extend env based on the type of the just-evaluated non-Dict intermediate.
            let mut current_env = env;
            match &child_ty {
                Type::Dict(Row { fields, .. }) => {
                    let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
                    for (name, field_ty) in fields {
                        let scheme = generalize(enclosing_level, field_ty, state);
                        new_env_inner.insert_scheme_named_only(name.clone(), scheme);
                    }
                    current_env = Arc::new(RwLock::new(new_env_inner));
                }
                Type::Unknown | Type::Any => {}
                _ => errors.push(TypeDiagnostic::error(
                    "type-error",
                    format!("expected record type, got {}", child_ty),
                    intermediate_span,
                )),
            }

            // Process remaining intermediates iteratively (Dict inline, non-Dict via continuation).
            for (i, intermediate) in remaining_intermediates.iter().enumerate() {
                if let SurfaceExpression::Dict(entries) = &intermediate.expr {
                    let (_, schemes, mut dict_errs) =
                        run_typecheck_dict(entries, &current_env, state, type_map).await;
                    errors.append(&mut dict_errs);
                    let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
                    for (name, scheme) in &schemes {
                        new_env_inner.insert_scheme_named_only(name.clone(), scheme.clone());
                    }
                    current_env = Arc::new(RwLock::new(new_env_inner));
                } else {
                    // Another non-Dict intermediate — push new continuation, return Eval.
                    let enc_level = state.level;
                    state.level += 1;
                    stack.push(TypeCheckCont::SequentialNonDictIntermediate {
                        intermediate_span: intermediate.span.clone(),
                        remaining_intermediates: remaining_intermediates[i + 1..]
                            .iter()
                            .map(Arc::clone)
                            .collect(),
                        last: Arc::clone(&last),
                        env: Arc::clone(&current_env),
                        enclosing_level: enc_level,
                    });
                    return TypeCheckAction::Eval(Arc::clone(intermediate), current_env);
                }
            }

            // All remaining intermediates processed — evaluate the last expression.
            TypeCheckAction::Eval(last, current_env)
        }

        // ===== AfterTypeAssertInner =====
        TypeCheckCont::TypeAssertInner {
            expected,
            has_default,
            default_node,
            env,
            span,
        } => {
            let actual = child_ty;
            let expected_resolved = state.subst.apply(&expected);
            let actual_resolved = state.subst.apply(&actual);

            // T-1708: Emit diagnostic for explicit @Unknown annotation
            if expected_resolved == Type::Unknown {
                state.diagnostics.push(TypeDiagnostic {
                    level: DiagnosticLevel::Info,
                    kind: "explicit-unknown",
                    message: "explicit @Unknown annotation — type is not statically known"
                        .to_string(),
                    spans: vec![(span.clone(), String::new())],
                    notes: vec![],
                    help: vec![],
                });
            }

            let mismatch_err = compute_type_assert_mismatch(
                &actual_resolved,
                &expected_resolved,
                has_default,
                &span,
            );

            if let Some(errs) = mismatch_err {
                if !has_default {
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
                let default_resolved = state.subst.apply(&default_ty);
                let passes = Type::is_subtype(
                    &default_resolved,
                    &expected_resolved,
                    Some(&state.tycon_env),
                ) || ((super::contains_unknown_or_top(&default_resolved)
                    || super::contains_unknown_or_top(&expected_resolved))
                    && Type::is_consistent(&default_resolved, &expected_resolved));
                if !passes {
                    errors.push(TypeDiagnostic::error("type-error",
                        format!(
                            "default value type mismatch: default has type {}, but assertion expects {}",
                            default_resolved, expected_resolved
                        ),
                        default_n.span.clone(),
                    ));
                }
            }

            TypeCheckAction::Done(expected)
        }

        // ===== AfterFieldBase =====
        TypeCheckCont::FieldBase { field, span } => {
            let resolved_base = state.subst.apply(&child_ty);
            let ty = field_type_from_base(&resolved_base, &field, &span, errors);

            // T-1711: Emit diagnostic for Unknown field access
            if ty == Type::Unknown {
                state.diagnostics.push(TypeDiagnostic {
                    level: DiagnosticLevel::Warn,
                    kind: "unknown-type",
                    message: format!(
                        "inferred type is Unknown for field access '.{}' — consider adding a type annotation",
                        field
                    ),
                    spans: vec![(span.clone(), String::new())],
                    notes: vec![],
                help: vec![],
                });
            }

            // Record the field access result with the Field node's span for LSP hover.
            record_type_map(type_map, &span, &ty);
            TypeCheckAction::Done(ty)
        }

        // ===== AfterUnquote =====
        TypeCheckCont::Unquote => TypeCheckAction::Done(child_ty),

        // ===== AfterUnquoteSplice =====
        TypeCheckCont::UnquoteSplice => TypeCheckAction::Done(Type::Unknown),

        // ===== AfterDictPassZero =====
        //
        // Pushed by the Dict arm of infer_step. Runs the full multi-pass dict inference via
        // run_typecheck_dict (T-1644). The entries from the continuation are passed directly;
        // run_typecheck_dict performs its own Pass 0 key resolution internally.
        //
        // Schemes are not propagated to the parent env here: in the DictPassZero path the dict
        // is the terminal expression being inferred, not an intermediate scope-chain body.
        // The dict's bindings are scoped to the dict itself (via dict_env inside
        // run_typecheck_dict) and do not escape to the parent. The returned record type
        // carries the full structural type information. Contrast with the Sequential path
        // (lines ~406 and ~1057) where intermediate dict schemes ARE extended into the env.
        TypeCheckCont::DictPassZero { entries, env } => {
            let (ty, _, mut dict_errors) =
                Box::pin(run_typecheck_dict(&entries, &env, state, type_map)).await;
            errors.append(&mut dict_errors);
            TypeCheckAction::Done(ty)
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
    errors: &mut Vec<TypeDiagnostic>,
) -> Type {
    // Resolver-address primary: resolution_table → get_scheme_at(depth, slot).
    // Extras fallback: get_extras_scheme(name) for narrowing overrides and class dispatch.
    let id = node_id(node);
    let scheme: Option<TypeScheme> = if let Some(addr) = state.resolution_table.get(&id) {
        let (level, slot) = match addr {
            crate::ast::VarAddr::LetrecGroupMember { depth, slot } => (*depth, *slot),
            crate::ast::VarAddr::ClosureCapture(i) => (1u32, *i),
            // B-593: Parameter maps to level=2 to avoid collision with LGM at level=0.
            crate::ast::VarAddr::Parameter(i) => (2u32, *i),
        };
        let slot_scheme = env.read().unwrap().get_scheme_at(level, slot);
        if slot_scheme.is_some() {
            slot_scheme
        } else {
            // Slot lookup failed — fall through to extras (narrowing, dispatch, name-only)
            env.read().unwrap().get_extras_scheme(name)
        }
    } else {
        // No resolver address — check extras only (narrowing overrides, class dispatch).
        // Every user-visible name must have a resolver address; if this fires for a
        // name that should be resolved, it is a bug in the resolver pass.
        env.read().unwrap().get_extras_scheme(name)
    };

    if let Some(scheme) = scheme {
        if !scheme.constraints.is_empty()
            || !scheme.type_vars.is_empty()
            || !scheme.kind_vars.is_empty()
        {
            if let Some(ref mut smap) = state.scheme_map {
                let key = (
                    node.span.start_line,
                    node.span.start_col,
                    node.span.end_line,
                    node.span.end_col,
                );
                smap.insert(key, scheme.clone());
            }
        }
        let constraints_len_before = state.constraints.len();
        let result_type = instantiate_scheme(
            &scheme,
            state.level,
            state,
            Some(name),
            Some(node.span.clone()),
            &node.span,
        );

        // Record dispatch obligations for new class constraints added by instantiate_scheme.
        for constraint in &state.constraints[constraints_len_before..] {
            if let crate::types::Constraint::Class { class, vars, .. } = constraint {
                let det_positions: Vec<usize> = if class.determines.is_empty() {
                    (0..vars.len()).collect()
                } else {
                    class.determines[0].0.clone()
                };

                for &det_pos in &det_positions {
                    if let Some(crate::type_class::ConstraintArg::Var(typevar_name)) =
                        vars.get(det_pos)
                    {
                        if let crate::ast::SurfaceExpression::VarRef { .. } = &node.expr {
                            state.dispatch_obligations.push(
                                crate::type_infer::DispatchObligation {
                                    typevar_name: typevar_name.clone(),
                                    varref_node: std::sync::Arc::clone(node),
                                    class_name: class.name.clone(),
                                    method_name: name.to_string(),
                                    constraint_vars: vars.clone(),
                                    det_positions: det_positions.clone(),
                                },
                            );
                        }
                    }
                }
            }
        }

        result_type
    } else {
        // T-1897: If the resolver successfully resolved this variable, the variable
        // genuinely exists in scope — the type checker simply doesn't have its type scheme.
        // Return Unknown (gradual typing) rather than a false "undefined variable" diagnostic.
        if let crate::ast::SurfaceExpression::VarRef { resolution, .. } = &node.expr {
            if let Some(Some(_)) = resolution.get() {
                return Type::Unknown;
            }
        }

        let mut err = TypeDiagnostic::error(
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
        // The annotation resolves to whatever type the annotation expression denotes.
        // No name-based special-casing — the annotation resolution machinery handles
        // all type expressions including function types via `resolve_annotation`.
        if let Some(ann) = annotation {
            let mut constraints: Vec<Constraint> = Vec::new();
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
                    Type::Unknown
                }
            };
            state
                .failed_bindings
                .insert(name.to_string(), node.span.clone());
            ty
        } else {
            errors.push(err.clone());
            Type::error_with(vec![err])
        }
    }
}

// ===== Inline helper: get / builtin-get / get? special case =====

async fn infer_get_call(
    name: &str,
    key_node: &Arc<SurfaceNode>,
    container_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Type {
    let container_ty = {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            container_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };
    let container_resolved = state.subst.apply(&container_ty);

    let key_ty = {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            key_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };
    let key_resolved = state.subst.apply(&key_ty);

    let field_ty = if let Type::StringLiteral(field_name) = &key_resolved {
        let label = Label::Concrete(field_name.clone());
        match resolve_has_field(&label, &container_resolved, state, key_node.span.clone(), 0) {
            Ok(ty) => ty,
            Err(e) => {
                errors.push(e);
                Type::Unknown
            }
        }
    } else {
        Type::Unknown
    };

    if name == "get?" {
        let null_ty = Type::Dict(Row {
            fields: indexmap::IndexMap::new(),
            tail: RowTail::Empty,
        });
        if matches!(field_ty, Type::Unknown) {
            Type::Unknown
        } else {
            Type::normalize_union(vec![field_ty, null_ty])
        }
    } else {
        field_ty
    }
}

// ===== Inline helper: get-in special case =====

async fn infer_get_in_call(
    path_node: &Arc<SurfaceNode>,
    dict_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Type {
    let dict_ty = {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            dict_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };
    let dict_ty = state.subst.apply(&dict_ty);

    let literal_path: Option<Vec<String>> = match &path_node.expr {
        SurfaceExpression::Dict(path_entries) => {
            let mut keys: Vec<String> = Vec::new();
            let mut all_literal = true;
            for (idx, entry) in path_entries.iter().enumerate() {
                let is_auto_indexed = match &entry.node.key {
                    None => true,
                    Some(k) => {
                        matches!(&k.expr, SurfaceExpression::Int(n) if *n == idx as i64)
                    }
                };
                if !is_auto_indexed {
                    all_literal = false;
                    break;
                }
                match &entry.node.value.expr {
                    SurfaceExpression::StringLiteral { content, .. } => keys.push(content.clone()),
                    _ => {
                        all_literal = false;
                        break;
                    }
                }
            }
            if all_literal {
                Some(keys)
            } else {
                None
            }
        }
        _ => None,
    };

    if let Some(keys) = literal_path {
        let mut local_stack = Vec::new();
        typecheck_for_errors(path_node, env, state, errors, type_map, &mut local_stack).await;
        if keys.is_empty() {
            return dict_ty;
        }
        let mut current = dict_ty;
        for key in &keys {
            current = state.subst.apply(&current);
            match &current {
                Type::Dict(row) => {
                    if let Some(field_ty) = row.fields.get(key.as_str()) {
                        current = field_ty.clone();
                    } else {
                        return Type::Unknown;
                    }
                }
                Type::Unknown => return Type::Unknown,
                _ => return Type::Unknown,
            }
        }
        return current;
    }

    let mut local_stack = Vec::new();
    typecheck_for_errors(path_node, env, state, errors, type_map, &mut local_stack).await;
    Type::Unknown
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
    errors: &mut Vec<TypeDiagnostic>,
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
    func_ty: Type,
    args: Vec<Arc<SurfaceNode>>,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    call_node: Arc<SurfaceNode>,
    ctx: &mut TypeCheckCtx<'_, '_>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    // Error cascade suppression: calling a Type::Error function still infers args
    // (to collect any nested errors) but does NOT emit a not_a_function diagnostic.
    // The error is already recorded at the definition site; re-emitting it here
    // creates a cascade that obscures the root cause.
    // Return Type::Error (not Unknown) — this is a definite failure; Unknown would
    // silently pass downstream consistency checks and mask the error.
    if let Type::Error(payload) = &func_ty {
        eval_args_for_errors(
            &args,
            &named_args,
            &env,
            ctx.state,
            ctx.errors,
            ctx.type_map,
        )
        .await;
        return TypeCheckAction::Done(Type::Error(payload.clone()));
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
        } => {
            // Instantiate if needed
            let (inst_params, inst_ret, inst_typed_variadics, inst_rest, inst_required): InstFuncSig =
                if func_ty.has_inference_vars() {
                // CALL-POLY: instantiate at current level
                let inst_ty = instantiate_at_level(&func_ty, ctx.state, &call_node.span);
                match inst_ty {
                    Type::Function {
                        params,
                        ret,
                        typed_variadics,
                        rest,
                        required_count,
                    } => (params, *ret, typed_variadics, rest, required_count),
                    _ => unreachable!("instantiate_at_level preserves Function variant"),
                }
            } else {
                // CALL-MONO: use as-is
                (
                    params.clone(),
                    (**ret).clone(),
                    typed_variadics.clone(),
                    rest.clone(),
                    *required_count,
                )
            };

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
                    let err = TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "arity mismatch: expected {} argument(s), got {}",
                            min_req, n_total
                        ),
                        call_node.span.clone(),
                    );
                    ctx.errors.push(err.clone());
                    return TypeCheckAction::Done(Type::error_with(vec![err]));
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
            });
            TypeCheckAction::Eval(first_arg, env)
        }

        Type::TypeVar(_, _) => {
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
            let ret_var = ctx
                .state
                .fresh_type_var_with(Some("ret"), None, Kind::Type, &call_node.span)
                .1;
            TypeCheckAction::Done(ret_var)
        }

        Type::Unknown => {
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
            ctx.state.diagnostics.push(crate::error::TypeDiagnostic {
                level: crate::error::DiagnosticLevel::Warn,
                kind: "unknown-call",
                message: "calling expression of Unknown type — may not be a function".to_string(),
                spans: vec![(call_node.span.clone(), String::new())],
                notes: vec![],
                help: vec![],
            });
            TypeCheckAction::Done(Type::Unknown)
        }

        Type::Any => {
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
            TypeCheckAction::Done(Type::Unknown)
        }

        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } if fields.fields.is_empty() => {
            // Unit variant constructor: wraps a single arg
            if args.len() != 1 {
                eval_args_for_errors(
                    &args,
                    &named_args,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                )
                .await;
                let err = TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "unit variant constructor takes exactly 1 argument, got {}",
                        args.len()
                    ),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }
            if !named_args.is_empty() {
                eval_args_for_errors(
                    &args,
                    &named_args,
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                )
                .await;
                let err = TypeDiagnostic::error(
                    "type-error",
                    "unit variant constructor does not accept named arguments",
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }
            let tycon = tycon.clone();
            let ctor = ctor.clone();
            let arg_ty = {
                let mut local_stack = Vec::new();
                Box::pin(run_typecheck(
                    &args[0],
                    &env,
                    ctx.state,
                    ctx.errors,
                    ctx.type_map,
                    &mut local_stack,
                ))
                .await
            };
            let mut payload_fields = indexmap::IndexMap::new();
            payload_fields.insert("0".to_string(), arg_ty);
            TypeCheckAction::Done(Type::NominalVariant {
                tycon,
                ctor,
                fields: Row {
                    fields: payload_fields,
                    tail: RowTail::Empty,
                },
            })
        }

        // Extract all Function-typed members from the intersection.
        // An intersection function type means the value satisfies all member signatures;
        // at a call site we select the unique member whose arity matches the supplied args.
        Type::Intersection(members) => {
            let fn_members: Vec<Type> = members
                .iter()
                .filter(|m| matches!(m, Type::Function { .. }))
                .cloned()
                .collect();

            if fn_members.is_empty() {
                let err = TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "expected function type, got intersection of non-function types: {}",
                        func_ty
                    ),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }

            let n_positional = args.len();
            let n_named = named_args.len();
            let n_total = n_positional + n_named;

            let matching: Vec<Type> = fn_members
                .into_iter()
                .filter(|m| {
                    if let Type::Function {
                        params,
                        typed_variadics,
                        rest,
                        required_count,
                        ..
                    } = m
                    {
                        let is_var = !typed_variadics.is_empty() || rest.is_some();
                        n_total >= *required_count && (is_var || n_positional <= params.len())
                    } else {
                        false
                    }
                })
                .collect();

            if matching.is_empty() {
                let err = TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "no overload of intersection type accepts {} argument(s)",
                        n_total
                    ),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }

            // Pick the most specific overload: smallest params.len() that fits,
            // preferring non-variadic over variadic.
            let selected = if matching.len() == 1 {
                matching.into_iter().next().unwrap()
            } else {
                matching
                    .into_iter()
                    .min_by_key(|m| {
                        if let Type::Function {
                            params,
                            typed_variadics,
                            rest,
                            ..
                        } = m
                        {
                            let is_var = !typed_variadics.is_empty() || rest.is_some();
                            if is_var {
                                (usize::MAX, params.len())
                            } else {
                                (0, params.len())
                            }
                        } else {
                            (usize::MAX, usize::MAX)
                        }
                    })
                    .unwrap()
            };

            Box::pin(apply_cont_call_func(
                selected, args, named_args, env, call_node, ctx, stack,
            ))
            .await
        }

        // For a union of function types at a call site, we select the member(s) matching
        // the supplied arity and union their return types.
        Type::Union(members) => {
            let fn_members: Vec<Type> = members
                .iter()
                .filter(|m| matches!(m, Type::Function { .. }))
                .cloned()
                .collect();

            if fn_members.is_empty() {
                let err = TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "expected function type, got union of non-function types: {}",
                        func_ty
                    ),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }

            let n_positional = args.len();
            let n_named = named_args.len();
            let n_total = n_positional + n_named;

            let matching: Vec<Type> = fn_members
                .into_iter()
                .filter(|m| {
                    if let Type::Function {
                        params,
                        typed_variadics,
                        rest,
                        required_count,
                        ..
                    } = m
                    {
                        let is_var = !typed_variadics.is_empty() || rest.is_some();
                        n_total >= *required_count && (is_var || n_positional <= params.len())
                    } else {
                        false
                    }
                })
                .collect();

            if matching.is_empty() {
                let err = TypeDiagnostic::error(
                    "type-error",
                    format!("no overload of union type accepts {} argument(s)", n_total),
                    call_node.span.clone(),
                );
                ctx.errors.push(err.clone());
                return TypeCheckAction::Done(Type::error_with(vec![err]));
            }

            // Pick the most specific overload: smallest params.len() that fits,
            // preferring non-variadic over variadic.
            let selected = if matching.len() == 1 {
                matching.into_iter().next().unwrap()
            } else {
                matching
                    .into_iter()
                    .min_by_key(|m| {
                        if let Type::Function {
                            params,
                            typed_variadics,
                            rest,
                            ..
                        } = m
                        {
                            let is_var = !typed_variadics.is_empty() || rest.is_some();
                            if is_var {
                                (usize::MAX, params.len())
                            } else {
                                (0, params.len())
                            }
                        } else {
                            (usize::MAX, usize::MAX)
                        }
                    })
                    .unwrap()
            };

            Box::pin(apply_cont_call_func(
                selected, args, named_args, env, call_node, ctx, stack,
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
            let err = TypeDiagnostic::error(
                "type-error",
                format!("expected function type, got {}", func_ty),
                call_node.span.clone(),
            );
            ctx.errors.push(err.clone());
            TypeCheckAction::Done(Type::error_with(vec![err]))
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
) -> Type {
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
        let err = TypeDiagnostic::error(
            "type-error",
            format!("arity mismatch: expected {} arguments, got 0", min_required),
            span.clone(),
        );
        ctx.errors.push(err.clone());
        return Type::error_with(vec![err]);
    }

    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            ctx.errors.push(TypeDiagnostic::error(
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
                let saved_bounds = ctx.state.bounds.clone();
                if let Err(e) = constrain(
                    &arg_ty,
                    &param_ty,
                    ctx.state,
                    &mut constraints,
                    na.span.clone(),
                )
                .await
                {
                    ctx.state.bounds = saved_bounds;
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
            }
            None => {
                if !variadic {
                    ctx.errors.push(TypeDiagnostic::error(
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
    arg_types: Vec<Type>,
    arg_nodes: Vec<Arc<SurfaceNode>>,
    sig: FnSig,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    span: Span,
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

    // Arity check — this is a post-collection check (all args already evaluated).
    // Return Type::Error rather than the return type to ensure definite failures
    // do not flow silently through downstream consistency checks.
    if total_supplied < min_required || (!fn_variadic && arg_types.len() > param_types.len()) {
        let err = TypeDiagnostic::error(
            "type-error",
            format!(
                "arity mismatch: expected {} arguments, got {}",
                min_required, total_supplied
            ),
            span.clone(),
        );
        ctx.errors.push(err.clone());
        return TypeCheckAction::Done(Type::error_with(vec![err]));
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
        let widened_arg = match arg_ty {
            Type::IntLiteral(_) => Type::Int,
            Type::StringLiteral(_) => Type::Str,
            other => other.clone(),
        };

        // Gradual typing boundary guard: Unknown-typed arg flowing into concrete param.
        // When an Unknown/Any arg flows into a concrete parameter, attach a runtime guard so
        // the evaluator can enforce the type contract at the Unknown→concrete boundary.
        if matches!(&widened_arg, Type::Unknown | Type::Any)
            && typecheck_call::is_concrete_type(&ctx.state.subst.apply(param_ty))
        {
            if let Some(arg_node) = arg_nodes.get(idx) {
                arg_node
                    .type_guard
                    .set(Some(ctx.state.subst.apply(param_ty)));
            }
        }

        let mut constraints = std::mem::take(&mut ctx.state.constraints);
        let saved_bounds = ctx.state.bounds.clone();
        if let Err(e) = constrain(
            &widened_arg,
            param_ty,
            ctx.state,
            &mut constraints,
            span.clone(),
        )
        .await
        {
            ctx.state.bounds = saved_bounds;
            ctx.errors.push(e);
        }
        ctx.state.constraints = constraints;
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
        let mut bucket_args: Vec<Vec<Type>> = vec![Vec::new(); typed_variadics.len()];
        // Rest accumulator: widened types for args that matched no typed bucket.
        let mut rest_positional_args: Vec<Type> = Vec::new();

        for arg_ty in variadic_args {
            let widened = match arg_ty {
                Type::IntLiteral(_) => Type::Int,
                Type::StringLiteral(_) => Type::Str,
                other => other.clone(),
            };

            // Match semantics: try each typed bucket in declaration order; first match wins.
            let mut routed = false;
            for (bucket_idx, (_, bucket_ty)) in typed_variadics.iter().enumerate() {
                let elem_ty = extract_seq_elem_type(bucket_ty);
                if Type::is_consistent_subtype(&widened, &elem_ty, Some(&ctx.state.tycon_env)) {
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
                    // Continue processing remaining args to collect further errors.
                    let err = TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "argument type {} does not match any variadic bucket",
                            widened
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
                let saved_bounds = ctx.state.bounds.clone();
                if let Err(e) = constrain(
                    matched_arg,
                    &elem_ty,
                    ctx.state,
                    &mut constraints,
                    span.clone(),
                )
                .await
                {
                    ctx.state.bounds = saved_bounds;
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
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
                let rest_dict = Type::Dict(Row {
                    fields,
                    tail: RowTail::Empty,
                });
                let mut constraints = std::mem::take(&mut ctx.state.constraints);
                let saved_bounds = ctx.state.bounds.clone();
                if let Err(e) = constrain(
                    &rest_dict,
                    rest_ty,
                    ctx.state,
                    &mut constraints,
                    span.clone(),
                )
                .await
                {
                    ctx.state.bounds = saved_bounds;
                    ctx.errors.push(e);
                }
                ctx.state.constraints = constraints;
            }
            // If no rest positional args, the rest TypeVar stays free (empty variadic dict).
        }
    }

    // Handle named args (CALL-POLY path).
    // Named args that don't match any fixed param are accumulated for the rest bucket.
    // If there is no rest bucket and no variadic at all, they produce an error.
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    let mut unmatched_named_arg_types: Vec<(String, Type)> = Vec::new();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            ctx.errors.push(TypeDiagnostic::error(
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
                    ctx.errors.push(TypeDiagnostic::error(
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
                let saved_bounds = ctx.state.bounds.clone();
                if let Err(e) = constrain(
                    &arg_ty,
                    &param_ty,
                    ctx.state,
                    &mut constraints,
                    na.span.clone(),
                )
                .await
                {
                    ctx.state.bounds = saved_bounds;
                    ctx.errors.push(TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "named argument '{}' type mismatch: {}",
                            na.node.name, e.message
                        ),
                        na.span.clone(),
                    ));
                }
                ctx.state.constraints = constraints;
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
                    ctx.errors.push(TypeDiagnostic::error(
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
                let widened = match ty {
                    Type::IntLiteral(_) => Type::Int,
                    Type::StringLiteral(_) => Type::Str,
                    other => other.clone(),
                };
                fields.insert(name.clone(), widened);
            }
            let named_dict = Type::Dict(Row {
                fields,
                tail: RowTail::Empty,
            });
            let mut constraints = std::mem::take(&mut ctx.state.constraints);
            let saved_bounds = ctx.state.bounds.clone();
            if let Err(e) = constrain(
                &named_dict,
                rest_ty,
                ctx.state,
                &mut constraints,
                span.clone(),
            )
            .await
            {
                ctx.state.bounds = saved_bounds;
                ctx.errors.push(e);
            }
            ctx.state.constraints = constraints;
        }
        // Named args arrived for a function with typed buckets but no untyped rest.
        // Typed buckets only accept positional args (matched by type); named args have
        // no bucket to go into. Emit an error for each unmatched named arg.
        if rest.is_none() {
            for (name, _) in &unmatched_named_arg_types {
                ctx.errors.push(TypeDiagnostic::error("type-error",
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
fn extract_seq_elem_type(bucket_ty: &Type) -> Type {
    if let Type::App(_, elem) = bucket_ty {
        return *elem.clone();
    }
    bucket_ty.clone()
}

// ===== Inline helper: Fn inference via AfterFnBody continuation =====

/// Resolve all function annotations, build the parameter environment, push `AfterFnBody`,
/// and return `Eval(body, fn_env)` so the CEK loop evaluates the body iteratively without
/// recursing on the Rust call stack.
async fn infer_fn_push_cont(
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
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
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut ann_mapping_opt = Some(&mut ann_mapping_str);
    let mut row_ann_mapping_str: HashMap<String, String> = HashMap::new();
    let mut row_ann_mapping_opt = Some(&mut row_ann_mapping_str);

    // Resolve return annotation first (populates bind: TypeVars into ann_mapping_str)
    let return_ann_type: Option<Type> = if let Some(ret_ann) = return_ann {
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

    // Consume expected_fn_params from state (single-use per fn invocation).
    // Set by infer_instance_decl_from_surface for bidirectional type checking of instance methods.
    // Taking it here prevents leaking into nested fn expressions in the body.
    let expected_params: Option<Vec<Type>> = state.expected_fn_params.take();

    // Resolve param annotations and build fn env
    let mut fn_env_inner = Env::with_parent(Arc::clone(env));
    let mut param_types: Vec<(Option<String>, Type)> = Vec::new();
    let mut typed_variadics: Vec<(String, Type)> = Vec::new();
    let mut rest: Option<Box<(String, Type)>> = None;
    let mut fixed_param_idx: usize = 0;

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
                        // Fall back to a bare TypeVar for the whole dict.
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
                    Type::Unknown
                }
            }
        } else {
            // Unannotated fixed param: use expected type from class method signature if available,
            // otherwise fall back to Type::Unknown for gradual typing.
            // expected_params is indexed by the fixed-param position (variadic params excluded).
            if let Some(ref expected) = expected_params {
                expected
                    .get(fixed_param_idx)
                    .cloned()
                    .unwrap_or(Type::Unknown)
            } else {
                Type::Unknown
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
        fn_env_inner
            .insert_scheme_named_only(p.node.name.clone(), TypeScheme::mono(param_ty.clone()));
        if p.node.variadic {
            // Variadic param: goes into typed_variadics or rest, not fixed params.
            if p.node.annotation.is_some() {
                // Typed variadic bucket declared after an untyped rest is a slot-ordering
                // error: the lowerer assigns slots in declaration order, but bind_args_thunks
                // assigns typed buckets before rest. Declaring them in the wrong order
                // causes silent slot inversion (data corruption at runtime).
                if rest.is_some() {
                    let err = TypeDiagnostic::error("type-error",
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
                let err = TypeDiagnostic::error("type-error",
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

    let fn_env_arc = Arc::new(RwLock::new(fn_env_inner));

    // required_count = number of fixed (non-variadic) params
    let required_count = param_types.len();

    let saved_level = state.level;
    let saved_expected_return = state.expected_return.clone();

    // Push the continuation so apply_cont can build the Function type from the body type.
    stack.push(TypeCheckCont::FnBody {
        saved_level,
        saved_expected_return,
        return_ann: return_ann_type,
        params: param_types,
        typed_variadics,
        rest,
        required_count,
        node_span: node.span.clone(),
    });

    // Evaluate body iteratively via the CEK loop.
    TypeCheckAction::Eval(Arc::clone(body), fn_env_arc)
}

// ===== Inline helper: Match arm environment setup =====
//
// Sets up the arm environment for guard inference, applies guard narrowing to the env,
// and computes `next_remaining_scrutinee` by accumulating I-Case3 negation for this arm.
// Returns the narrowed arm env and updated remaining scrutinee ready for body evaluation.
// Returns `None` only if called with no arms (should not happen in practice).

async fn setup_match_arm_env(
    arm: &SurfaceMatchArm,
    remaining_scrutinee: &Type,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<(Arc<RwLock<Env>>, Type)> {
    // Non-CaseArm match arm patterns introduce no bindings into the arm environment.
    // Only [case [let ...]] arms introduce bindings, handled by the CaseArm branch of
    // run_typecheck. The arm env starts as a clone of the outer env.
    let arm_env: Arc<RwLock<Env>> = Arc::clone(env);

    // Guard inference and narrowing (guard is inferred for its type-map side effects only)
    let arm_env = if let Some(guard) = &arm.guard {
        let mut local_stack = Vec::new();
        typecheck_for_errors(guard, &arm_env, state, errors, type_map, &mut local_stack).await;
        let guard_narrowings = typecheck_narrow::extract_narrowings(guard, &arm_env);
        if guard_narrowings.is_empty() {
            arm_env
        } else {
            typecheck_narrow::apply_narrowings(&arm_env, &guard_narrowings, state)
        }
    } else {
        arm_env
    };

    // Compute updated remaining_scrutinee (I-Case3 negation accumulation) for next arm.
    let next_remaining_scrutinee = if arm.guard.is_none() {
        match &arm.pattern.expr {
            crate::ast::SurfaceExpression::Field { .. } => {
                let tag =
                    crate::ast::flatten_dot_access_to_tag_node(&arm.pattern).unwrap_or_default();
                let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                let neg_tag = Type::Negation(Box::new(Type::NominalVariant {
                    tycon: tycon.to_string(),
                    ctor: ctor.to_string(),
                    fields: crate::type_def::Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    },
                }));
                Type::normalize_intersection(vec![remaining_scrutinee.clone(), neg_tag])
            }
            crate::ast::SurfaceExpression::Call { func, .. }
                if matches!(&func.expr, crate::ast::SurfaceExpression::Field { .. }) =>
            {
                let tag = crate::ast::flatten_dot_access_to_tag_node(func).unwrap_or_default();
                let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                let neg_tag = Type::Negation(Box::new(Type::NominalVariant {
                    tycon: tycon.to_string(),
                    ctor: ctor.to_string(),
                    fields: crate::type_def::Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    },
                }));
                Type::normalize_intersection(vec![remaining_scrutinee.clone(), neg_tag])
            }
            // Dict pattern — narrow the scrutinee by excluding the matched shape.
            //
            // A dict pattern `[ok: v]` matches any value that has at least the key `ok`.
            // After the arm fires, the remaining scrutinee is everything that does NOT
            // have that key set: `remaining ∩ ¬{ok: Any, _: Any}`.
            //
            // The negated type is an open dict (Uniform tail, value=Any) containing only
            // the static keys from the pattern.  Values cannot be narrowed here because the
            // pattern binds them via variables — only key presence is relevant.
            //
            // Soundness: the negation is conservative.  It only excludes values that are
            // *guaranteed* to have matched (dicts with ALL the pattern keys present).
            // If no static keys can be extracted the arm falls through to no narrowing.
            crate::ast::SurfaceExpression::Dict(entries) => {
                // Extract static key names: VarRef (bare word) or StringLiteral keys.
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
                    // No static keys (e.g. empty dict pattern or all computed keys).
                    // Cannot narrow — the `...` arm keeps the full scrutinee.
                    remaining_scrutinee.clone()
                } else {
                    // Build an open dict type that has exactly the pattern's static keys
                    // (each mapped to Any) plus a Uniform tail allowing any other fields.
                    // This represents "any dict that has at least these keys".
                    let mut key_fields: indexmap::IndexMap<String, Type> =
                        indexmap::IndexMap::new();
                    for name in key_names {
                        key_fields.insert(name, Type::Any);
                    }
                    let dict_with_keys = Type::Dict(crate::type_def::Row {
                        fields: key_fields,
                        tail: crate::type_def::RowTail::Uniform {
                            key: None,
                            value: Box::new(Type::Any),
                        },
                    });
                    Type::normalize_intersection(vec![
                        remaining_scrutinee.clone(),
                        Type::Negation(Box::new(dict_with_keys)),
                    ])
                }
            }
            // Wildcard forms: VarRef, Placeholder
            crate::ast::SurfaceExpression::VarRef { .. }
            | crate::ast::SurfaceExpression::Placeholder(..) => Type::Never,
            _ => remaining_scrutinee.clone(),
        }
    } else {
        remaining_scrutinee.clone()
    };

    Some((arm_env, next_remaining_scrutinee))
}

// ===== Inline helper: Match exhaustiveness checking =====

fn run_match_exhaustiveness_check(
    scrutinee_ty: &Type,
    arms: &[SurfaceMatchArm],
    span: &Span,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
) {
    let tycon_env_ref = state.tycon_env_ref();
    let sig = match scrutinee_ty {
        Type::Union(members) => coverage::ConstructorSignature::from_union(members, tycon_env_ref),
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => Some(coverage::ConstructorSignature::from_nominal_variant(
            tycon,
            ctor,
            fields,
            tycon_env_ref,
        )),
        Type::TyCon(name) => match tycon_env_ref.get(name.as_str()) {
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
        },
        _ => None,
    };

    if let Some(sig) = sig {
        let coverage_patterns: Vec<coverage::CoveragePattern> = arms
            .iter()
            .map(|arm| coverage::ast_pattern_to_coverage(&arm.pattern, Some(tycon_env_ref)))
            .collect();
        let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();
        let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);

        if !result.exhaustive {
            let witnesses = coverage::format_witnesses(&result.uncovered);
            errors.push(TypeDiagnostic::error(
                "type-error",
                format!("non-exhaustive match: missing coverage for {}", witnesses),
                span.clone(),
            ));
        }
        for &idx in &result.redundant {
            errors.push(TypeDiagnostic::error(
                "type-error",
                "unreachable match arm: this pattern is already covered by prior arms",
                arms[idx].pattern.span.clone(),
            ));
        }
        for &idx in &result.inaccessible {
            errors.push(TypeDiagnostic::error(
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
    node: &Arc<SurfaceNode>,
) -> Arc<RwLock<Env>> {
    let binding_names: Vec<String> = match &let_bindings.expr {
        SurfaceExpression::LetDecl { bindings } => bindings
            .iter()
            .filter_map(|b| {
                if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                    Some(name.clone())
                } else {
                    None
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    if binding_names.is_empty() {
        Arc::clone(env)
    } else {
        let mut child_inner = Env::with_parent(Arc::clone(env));
        for name in binding_names {
            child_inner
                .insert_scheme_named_only(name, TypeScheme::mono(state.fresh_type_var(&node.span)));
        }
        Arc::new(RwLock::new(child_inner))
    }
}

// ===== Inline helper: TypeAssert mismatch computation =====

fn compute_type_assert_mismatch(
    actual: &Type,
    expected: &Type,
    _has_default: bool,
    span: &Span,
) -> Option<Vec<TypeDiagnostic>> {
    match (actual, expected) {
        (
            Type::Function {
                params: p_actual,
                ret: r_actual,
                ..
            },
            Type::Function {
                params: p_expected,
                ret: r_expected,
                ..
            },
        ) => {
            if p_actual.len() != p_expected.len() {
                Some(vec![TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        p_expected.len(),
                        p_actual.len()
                    ),
                    span.clone(),
                )])
            } else {
                let mut param_err: Option<Vec<TypeDiagnostic>> = None;
                for ((_, p_act), (_, p_exp)) in p_actual.iter().zip(p_expected.iter()) {
                    if !Type::is_consistent_subtype(p_act, p_exp, None) {
                        param_err = Some(vec![TypeDiagnostic::error(
                            "type-error",
                            format!(
                                "parameter annotation {} is more restrictive than required type {}",
                                p_act, p_exp
                            ),
                            span.clone(),
                        )]);
                        break;
                    }
                }
                if param_err.is_some() {
                    param_err
                } else if !Type::is_consistent_subtype(r_actual, r_expected, None) {
                    Some(vec![TypeDiagnostic::error(
                        "unification-failure",
                        format!("cannot unify {} with {}", r_expected, r_actual),
                        span.clone(),
                    )])
                } else {
                    None
                }
            }
        }
        _ => {
            // For union types: TypeAssert might succeed if ANY member is consistent with
            // expected. Only error if NO member could possibly match (assertion is dead code).
            let definitely_fails = if let Type::Union(members) = actual {
                !members
                    .iter()
                    .any(|m| Type::is_consistent_subtype(m, expected, None))
            } else {
                !Type::is_consistent_subtype(actual, expected, None)
            };
            if definitely_fails {
                Some(vec![TypeDiagnostic::error(
                    "unification-failure",
                    format!("cannot unify {} with {}", expected, actual),
                    span.clone(),
                )])
            } else {
                None
            }
        }
    }
}

// ===== Inline helper: Field type lookup =====

/// Resolve the type of a field access `base_ty.field`.
///
/// Returns the type of the named field in `base_ty`, or `Type::Unknown` when the field
/// is not statically known. All dot-access desugars to `builtin-get` (key-based lookup);
/// there is no slot-indexed fast path.
///
/// ## Why Unknown is correct for missing fields
///
/// Under BAS (Bounded Abstraction Subtyping, Parreaux & Chau 2022) and gradual typing
/// (Siek & Taha 2006), the annotated record type `{x: Int}` is a LOWER BOUND on field
/// types, not an exhaustive declaration. A value typed `{x: Int}` may carry additional
/// fields at runtime — the annotation only constrains that field `x` is `Int`. Accessing
/// an unannotated field returns `Type::Unknown` (the dynamic type `?`), which is
/// consistent with all types. This is correct, not a misuse: it preserves the BAS
/// invariant that annotations are subsumption constraints, not closed-world specifications.
///
/// The `other` arm at the bottom of this function handles genuinely-wrong base types
/// (e.g., accessing `.field` on an `Int` or `Str` base) where a type error IS correct.
fn field_type_from_base(
    base_ty: &Type,
    field: &crate::ast::DotKey,
    span: &Span,
    errors: &mut Vec<TypeDiagnostic>,
) -> Type {
    let key = match field {
        crate::ast::DotKey::Ident(s) => s.clone(),
        crate::ast::DotKey::Int(n) => n.to_string(),
    };

    match base_ty {
        Type::Dict(row) => {
            match row.fields.get(&key) {
                Some(ty) => ty.clone(),
                // Field not found in the named fields — return Unknown (see doc comment above).
                // BAS width subtyping: the value may carry this field at runtime even if the
                // annotated type does not list it. Unknown is the correct gradual type here.
                None => Type::Unknown,
            }
        }
        // Intersection: try each member. If no member has the field, return Unknown.
        // The intersection may be partially resolved; Unknown defers the constraint.
        Type::Intersection(members) => {
            for m in members {
                if let Type::Dict(row) = m {
                    if let Some(ty) = row.fields.get(&key) {
                        return ty.clone();
                    }
                }
            }
            Type::Unknown
        }
        // Gradual: Unknown/Any base → Unknown field (consistency, not subtyping).
        // TypeVar: the base type is not yet resolved — defer field type to Unknown.
        Type::Unknown | Type::Any | Type::TypeVar(_, _) => Type::Unknown,
        // Negation, Union, NominalVariant, TyCon: structural field access is not defined
        // for these types in the static type system. Return Unknown (gradual) rather than
        // an error — these types may be refined by later unification or narrowing.
        Type::Negation(_) => Type::Unknown,
        Type::Union(_) => Type::Unknown,
        Type::NominalVariant { .. } => Type::Unknown,
        Type::TyCon(_) | Type::TyConResolved(_, _) | Type::App(_, _) => Type::Unknown,
        // Error propagation: absorb silently (cascade prevention).
        Type::Error(payload) => Type::Error(payload.clone()),
        // Concrete non-record types: field access is definitely wrong — emit type error.
        other => {
            let err = TypeDiagnostic::error(
                "type-error",
                format!("expected record type for field access, but got {}", other),
                span.clone(),
            );
            errors.push(err.clone());
            Type::error_with(vec![err])
        }
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
            SurfaceExpression::Match { scrutinee, arms } => {
                worklist.push((scrutinee, Arc::clone(&locals)));
                for arm in arms {
                    for body_expr in &arm.body {
                        worklist.push((body_expr, Arc::clone(&locals)));
                    }
                    if let Some(ref guard) = arm.guard {
                        worklist.push((guard, Arc::clone(&locals)));
                    }
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
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                // let_bindings and pattern are scanned with the current locals.
                worklist.push((let_bindings, Arc::clone(&locals)));
                worklist.push((pattern, Arc::clone(&locals)));
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
                worklist.push((body, body_locals));
            }
            SurfaceExpression::Error(_) => {}
        }
    }

    deps
}

/// Occurs check: returns true if TypeVar `name` appears free anywhere in `ty`.
pub(crate) fn type_contains_typevar(ty: &Type, name: &str) -> bool {
    match ty {
        Type::TypeVar(n, _) => n.as_str() == name,
        Type::Union(members) | Type::Intersection(members) => {
            members.iter().any(|m| type_contains_typevar(m, name))
        }
        Type::Negation(inner) => type_contains_typevar(inner, name),
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            ..
        } => {
            params.iter().any(|(_, t)| type_contains_typevar(t, name))
                || typed_variadics
                    .iter()
                    .any(|(_, t)| type_contains_typevar(t, name))
                || rest
                    .as_ref()
                    .is_some_and(|r| type_contains_typevar(&r.1, name))
                || type_contains_typevar(ret, name)
        }
        Type::App(f, arg) => type_contains_typevar(f, name) || type_contains_typevar(arg, name),
        Type::Dict(row) => {
            row.fields.values().any(|t| type_contains_typevar(t, name))
                || match &row.tail {
                    RowTail::Uniform { key: k, value: v } => {
                        k.as_ref().is_some_and(|t| type_contains_typevar(t, name))
                            || type_contains_typevar(v, name)
                    }
                    RowTail::Empty => false,
                }
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
/// Called from `run_typecheck_dict` Pass 2 (type alias registration) and from
/// `AfterDictPassZero` handler when building constructor scheme types.
pub(crate) fn adt_value_type(alias_body: &Type) -> Type {
    let members: Vec<&Type> = match alias_body {
        Type::Union(ms) => ms.iter().collect(),
        nv @ Type::NominalVariant { .. } => vec![nv],
        _ => return alias_body.clone(),
    };
    let mut ctor_dict_fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    for m in members {
        if let Type::NominalVariant { ctor, fields, .. } = m {
            let ctor_type = if fields.fields.is_empty() {
                m.clone()
            } else {
                let fn_params: Vec<(Option<String>, Type)> = fields
                    .fields
                    .iter()
                    .map(|(k, v)| (Some(k.clone()), v.clone()))
                    .collect();
                let required_count = fn_params.len();
                Type::Function {
                    params: fn_params,
                    ret: Box::new(m.clone()),
                    typed_variadics: vec![],
                    rest: None,
                    required_count,
                }
            };
            ctor_dict_fields.insert(ctor.clone(), ctor_type);
        }
    }
    if ctor_dict_fields.is_empty() {
        alias_body.clone()
    } else {
        Type::Dict(Row {
            fields: ctor_dict_fields,
            tail: RowTail::Empty,
        })
    }
}

/// Extract the key name from a dict entry.
///
/// Handles `StringLiteral`, `Int`, and `VarRef` directly.  For any other key
/// expression (computed keys) falls back to `run_typecheck` and accepts a
/// `Type::StringLiteral` or `Type::IntLiteral` result, mirroring the behaviour
/// of the `typecheck_dict.rs` implementation.
pub(crate) async fn entry_key_name(
    entry: &SurfaceEntry,
    auto_index: &mut i64,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeDiagnostic>,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            SurfaceExpression::Int(n) => Some(n.to_string()),
            _ => {
                match Box::pin(run_typecheck(
                    key_node,
                    env,
                    state,
                    errors,
                    type_map,
                    &mut Vec::new(),
                ))
                .await
                {
                    Type::StringLiteral(s) => Some(s),
                    Type::IntLiteral(n) => Some(n.to_string()),
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
/// Returns `(record_type, schemes, errors)` where:
/// - `record_type` is the inferred `Type::Dict(...)` for the dict literal
/// - `schemes` is an `IndexMap<String, TypeScheme>` of per-entry generalized schemes (needed
///   by `process_document` for cross-document scoping and by `Sequential` for
///   let-polymorphism across multi-body function steps)
/// - `errors` is the accumulated vector of type errors (inference is best-effort)
///
/// Called by:
/// - `AfterDictPassZero` handler (dict expressions encountered during CEK run_typecheck)
/// - `process_document` (top-level dict expressions in a document)
/// - `run_typecheck`'s Sequential arm (intermediate dict bodies in multi-body functions)
///
/// Tracked by T-1644.
pub(crate) async fn run_typecheck_dict(
    entries: &[Spanned<SurfaceEntry>],
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> (
    Type,
    indexmap::IndexMap<String, TypeScheme>,
    Vec<TypeDiagnostic>,
) {
    // Level management: save enclosing level, increment for dict body
    let enclosing_level = state.level;
    state.level += 1;

    let dict_env: Arc<RwLock<Env>> = Arc::new(RwLock::new(Env::with_parent(Arc::clone(env))));

    // Extra schemes from ADT constructors — injected in Pass 2, merged into final schemes.
    // IndexMap preserves insertion order so constructor scheme ordering is deterministic.
    let mut ctor_schemes: indexmap::IndexMap<String, TypeScheme> = indexmap::IndexMap::new();
    let mut key_entries: Vec<(Option<String>, bool, bool)> = Vec::new();
    let mut auto_index: i64 = 0;
    let mut errors: Vec<TypeDiagnostic> = Vec::new();

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
    let mut fresh_vars_by_name: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
    for ((key_name, is_alias, is_static_key), entry) in key_entries.iter().zip(entries.iter()) {
        // (a) Static-key entry.
        if *is_static_key {
            if let Some(ref name) = key_name {
                if let SurfaceExpression::Fn { params, .. } = &entry.node.value.expr {
                    // B-520: fn entries get Type::Function skeleton so recursive calls see a
                    // function-shaped callee type without requiring a return annotation.
                    // Variadic params go into typed_variadics/rest, NOT into params — matching the
                    // structure that infer_fn_push_cont produces. Putting variadics in params with
                    // Dict(Uniform(TypeVar)) caused apply_call_args_poly to unify Int args against
                    // the Dict param type → "cannot unify Dict Any ? with Integer".
                    let mut fn_params: Vec<(Option<String>, Type)> = Vec::new();
                    let mut pre_typed_variadics: Vec<(String, Type)> = Vec::new();
                    let mut pre_rest: Option<Box<(String, Type)>> = None;
                    for p in params {
                        if p.node.variadic {
                            let param_ty = state.fresh_type_var(&p.span);
                            if p.node.annotation.is_some() {
                                pre_typed_variadics.push((p.node.name.clone(), param_ty));
                            } else {
                                pre_rest = Some(Box::new((p.node.name.clone(), param_ty)));
                            }
                        } else {
                            let ty = state.fresh_type_var(&p.span);
                            fn_params.push((Some(p.node.name.clone()), ty));
                        }
                    }
                    let ret_var = state.fresh_type_var(&entry.span);
                    let required_count = fn_params.len();
                    let fn_type = Type::Function {
                        params: fn_params,
                        ret: Box::new(ret_var),
                        typed_variadics: pre_typed_variadics,
                        rest: pre_rest,
                        required_count,
                    };
                    if !is_alias {
                        fresh_vars_by_name.insert(name.clone(), fn_type.clone());
                    }
                    dict_env
                        .write()
                        .unwrap()
                        .insert_scheme_named_only(name.clone(), TypeScheme::mono(fn_type));
                } else {
                    let fresh_var = state.fresh_type_var(&entry.span);
                    if !is_alias {
                        fresh_vars_by_name.insert(name.clone(), fresh_var.clone());
                    }
                    dict_env
                        .write()
                        .unwrap()
                        .insert_scheme_named_only(name.clone(), TypeScheme::mono(fresh_var));
                }
            }
        }
        // (b) Anonymous InstanceDecl entry: insert ɪ-prefixed placeholders at this source position.
        if entry.node.key.is_none() {
            if let SurfaceExpression::Decl(decl) = &entry.node.value.expr {
                if let SurfaceDeclaration::InstanceDecl { class_name, arms } = decl.as_ref() {
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
                            dict_env.write().unwrap().insert_scheme_named_only(
                                binding_name,
                                TypeScheme::mono(fresh_var),
                            );
                        }
                    }
                }
            }
        }
    }

    // B-477: Inject a synthetic innermost scope frame into state.scope_frames for this dict.
    //
    // When the user declares an [instance ...] in this dict, Pass 1 above pre-inserts the
    // ɪ-prefixed mangled binding name into dict_env.slots with a fresh TypeVar placeholder.
    // During Pass 3, check_constraints_on_var fires when the TypeVar resolves, and calls
    // resolve_name_in_frames to convert the mangled name to a (level, slot) pair, then
    // calls call_dispatch.set(debruijn_to_var_addr(level, slot)).  Without this synthetic
    // frame, resolve_name_in_frames only
    // searches the parent runtime scope chain (outer evaluated scopes) and misses the current
    // dict's instance bindings — because the user's document has not been evaluated yet.
    //
    // Fix: snapshot dict_env.slots after Pass 1 into a new innermost frame (appended to
    // frames, since frames[n-1] is innermost and level=0).  This frame mirrors what the
    // lowerer will generate at runtime via surface_dict_static_keys.  We pop this synthetic
    // frame at the end of run_typecheck_dict to avoid frame leakage to parent scopes.
    let pushed_synthetic_frame = if state.scope_frames.is_some() {
        let synthetic: indexmap::IndexMap<String, u32> = {
            let env_guard = dict_env.read().unwrap();
            env_guard
                .slots
                .iter()
                .enumerate()
                .filter_map(|(i, entry)| entry.as_ref().map(|(name, _)| (name.clone(), i as u32)))
                .collect()
        };
        if let Some(ref mut frames) = state.scope_frames {
            frames.push(synthetic);
            true
        } else {
            false
        }
    } else {
        false
    };

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
                                Kind::Type,
                                &param_span,
                            )
                            .0;
                        alias_ann_map.insert(param_name.clone(), fresh.clone());
                    }

                    let alias_name = key_name.as_deref().unwrap_or("");
                    let mut alias_constraints: Vec<Constraint> = Vec::new();
                    let mut ann_map_for_body = alias_ann_map.clone();
                    let resolved_body: Type = match &body.expr {
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
                                None,
                                alias_name,
                            )
                            .await;
                            match dict_result {
                                Ok(t) => t,
                                Err(e) => {
                                    errors.push(e);
                                    Type::Unknown
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
                                    Type::Unknown
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
                    let qualify_nominal = |ty: Type| -> Type {
                        match ty {
                            Type::NominalVariant {
                                tycon: _,
                                ctor,
                                fields,
                            } => {
                                let qualified_tag = qualify_tag(&ctor);
                                let (new_tycon, new_ctor) = qualified_tag
                                    .split_once('.')
                                    .unwrap_or(("", qualified_tag.as_str()));
                                Type::NominalVariant {
                                    tycon: new_tycon.to_string(),
                                    ctor: new_ctor.to_string(),
                                    fields,
                                }
                            }
                            other => other,
                        }
                    };
                    let qualified_body = match resolved_body {
                        Type::NominalVariant {
                            tycon: _,
                            ctor,
                            fields,
                        } => {
                            let qualified_tag = qualify_tag(&ctor);
                            let (new_tycon, new_ctor) = qualified_tag
                                .split_once('.')
                                .unwrap_or(("", qualified_tag.as_str()));
                            Type::NominalVariant {
                                tycon: new_tycon.to_string(),
                                ctor: new_ctor.to_string(),
                                fields,
                            }
                        }
                        Type::Union(members) => Type::normalize_union(
                            members.into_iter().map(qualify_nominal).collect(),
                        ),
                        other => other,
                    };
                    let constructors: Vec<(String, usize)> = match &qualified_body {
                        Type::NominalVariant {
                            tycon,
                            ctor,
                            fields,
                        } => {
                            let arity = if fields.fields.is_empty() { 0 } else { 1 };
                            let qualified_tag = if tycon.is_empty() {
                                ctor.clone()
                            } else {
                                format!("{}.{}", tycon, ctor)
                            };
                            vec![(qualified_tag, arity)]
                        }
                        Type::Union(members) => members
                            .iter()
                            .filter_map(|m| match m {
                                Type::NominalVariant {
                                    tycon,
                                    ctor,
                                    fields,
                                } => {
                                    let arity = if fields.fields.is_empty() { 0 } else { 1 };
                                    let qualified_tag = if tycon.is_empty() {
                                        ctor.clone()
                                    } else {
                                        format!("{}.{}", tycon, ctor)
                                    };
                                    Some((qualified_tag, arity))
                                }
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
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
                        constructor_constants: indexmap::IndexMap::new(),
                        definition_span: Some(entry.span.clone()),
                    });
                    let alias_ty = qualified_body;

                    if let Some(name) = key_name {
                        dict_env
                            .write()
                            .unwrap()
                            .insert_tycon_def(name.clone(), std::sync::Arc::clone(&tycon_def));
                        state.tycon_env.entry(name.clone()).or_insert(tycon_def);
                        // T-1805: Wire type declarations into type_stage_scope so
                        // resolve_type_head can find user-declared types via the
                        // scope chain. or_insert preserves type-stage entries
                        // (type-stage has priority over runtime-declared types).
                        if state.type_stage_scope.is_empty() {
                            state
                                .type_stage_scope
                                .push(std::collections::HashMap::new());
                        }
                        state.type_stage_scope[0].entry(name.clone()).or_insert(
                            crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::TyCon(
                                name.clone(),
                            )),
                        );
                        if params.is_empty() {
                            let value_scheme_ty = adt_value_type(&alias_ty);
                            if let Type::Dict(ref row) = value_scheme_ty {
                                for (ctor_name, ctor_ty) in &row.fields {
                                    ctor_schemes.insert(
                                        ctor_name.clone(),
                                        TypeScheme::mono(ctor_ty.clone()),
                                    );
                                }
                            }
                            dict_env.write().unwrap().insert_scheme_named_only(
                                name.clone(),
                                TypeScheme::mono(value_scheme_ty),
                            );
                        }
                    }
                }
            }
        }
    }

    // Initialize local substitution and field types accumulator.
    // IndexMap preserves insertion (source) order for field_types so that resolved_field_types
    // — and thus Type::Dict row fields — are in deterministic source order across runs,
    // eliminating non-deterministic warning ordering caused by HashMap's random iteration.
    let subst = Substitution {
        type_map: std::cell::RefCell::new(HashMap::new()),
    };
    let mut field_types: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();

    // entry_inner_schemes and entry_constraints are only accessed by key lookup (not iterated
    // for output), so HashMap ordering does not affect diagnostic determinism here.
    let mut entry_inner_schemes: HashMap<String, HashMap<String, TypeScheme>> = HashMap::new();
    let mut entry_constraints: HashMap<String, Vec<Constraint>> = HashMap::new();

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
                let result: Result<Type, Vec<TypeDiagnostic>> = match decl_box.as_ref() {
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
                        let sc_flat: Vec<(String, String)> = superclasses
                            .iter()
                            .flat_map(|(sc_name, sc_params)| {
                                sc_params
                                    .iter()
                                    .map(|p| (sc_name.clone(), p.clone()))
                                    .collect::<Vec<_>>()
                            })
                            .collect();
                        super::infer_class_decl_from_surface(
                            &super::ClassDeclSurface {
                                name,
                                params,
                                superclasses: &sc_flat,
                                determines,
                                resolver,
                                resolver_injective: *resolver_injective,
                                structural,
                                span: entry.node.value.span.clone(),
                            },
                            state,
                        )
                    }
                    SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                        Box::pin(super::infer_instance_decl_from_surface(
                            &class_decl_name(class_name),
                            arms,
                            entry.node.value.span.clone(),
                            &dict_env,
                            state,
                            type_map,
                        ))
                        .await
                    }
                    _ => Ok(Type::Any),
                };

                let (ref key_name, _, _) = key_entries[idx];
                match result {
                    Ok(ty) => {
                        if let Some(name) = key_name {
                            field_types.insert(name.clone(), ty);
                        }
                        // T-1733: Register class method TypeSchemes after successful ClassDecl processing
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

                            if let Some(class_arc) = class_arc_opt {
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
                                            let lvl = state.level;
                                            state.levels.insert(param_name.clone(), lvl);
                                            state
                                                .type_vars
                                                .entry(param_name.clone())
                                                .or_insert_with(|| {
                                                    crate::type_infer::TypeVarEntry::blank(
                                                        lvl,
                                                        crate::types::Kind::Type,
                                                    )
                                                });
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
                                                // T-1853: Store the polymorphic method type in
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
                                                            env_guard.insert_class(class_decl);
                                                        }
                                                    }
                                                }

                                                // Build Class constraint with class Arc and param vars
                                                let constraint_vars: Vec<
                                                    crate::type_class::ConstraintArg,
                                                > = params
                                                    .iter()
                                                    .map(|p| {
                                                        crate::type_class::ConstraintArg::Var(
                                                            p.clone(),
                                                        )
                                                    })
                                                    .collect();
                                                let class_constraint =
                                                    crate::types::Constraint::Class {
                                                        class: class_arc.clone(),
                                                        vars: constraint_vars,
                                                        origin_name: None,
                                                        origin_span: None,
                                                    };

                                                // Build TypeScheme with params, constraint, and method type
                                                let scheme = crate::type_infer::TypeScheme {
                                                    type_vars: params.clone(),
                                                    constraints: vec![class_constraint],
                                                    body: method_type,
                                                    label_vars: Vec::new(),
                                                    kind_vars: Vec::new(),
                                                    doc: None,
                                                    inner_schemes: None,
                                                    param_narrowings: Vec::new(),
                                                };

                                                // Insert into dict_env — class method names are
                                                // not resolver-assigned slots (ClassDecl does not
                                                // inject method names into surface_dict_static_keys),
                                                // so use insert_scheme_named_only (extras path).
                                                dict_env
                                                    .write()
                                                    .unwrap()
                                                    .insert_scheme_named_only(method_name, scheme);
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
                            field_types.insert(name.clone(), Type::error_with(errs.clone()));
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
            Singleton(String, Type),
            Multiple(indexmap::IndexMap<String, Type>),
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

                // Infer the entry value using run_typecheck (CEK path, no Rust stack recursion).
                // TypeAssert annotation resolution is not yet wired into this CEK path; type_assert_ty = None.
                // For nested Dict values, call run_typecheck_dict directly to capture schemes.
                let (value_ty, nested_schemes_opt) =
                    if let SurfaceExpression::Dict(nested_entries) = &entry.node.value.expr {
                        let (ty, schemes, mut nested_errs) = Box::pin(run_typecheck_dict(
                            nested_entries,
                            &scc_env,
                            state,
                            type_map,
                        ))
                        .await;
                        errors.append(&mut nested_errs);
                        (Ok(ty), Some(schemes))
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
                        let result = if local_errors.is_empty() {
                            Ok(ty)
                        } else {
                            Err(local_errors)
                        };
                        (result, None)
                    };

                let this_entry_constraints =
                    std::mem::replace(&mut state.constraints, saved_constraints);
                if !this_entry_constraints.is_empty() {
                    entry_constraints.insert(name.clone(), this_entry_constraints);
                }

                if let Some(nested_schemes) = nested_schemes_opt {
                    entry_inner_schemes.insert(name.clone(), nested_schemes.into_iter().collect());
                }

                match value_ty {
                    Ok(value_ty) => {
                        let bound_var_opt = match &fresh_vars_storage {
                            Some(FreshVars::Singleton(n, ty)) if n == name.as_str() => Some(ty),
                            Some(FreshVars::Multiple(map)) => map.get(name.as_str()),
                            _ => None,
                        };

                        if let Some(bound_var) = bound_var_opt {
                            match bound_var {
                                Type::TypeVar(var_name, _) => {
                                    subst
                                        .type_map
                                        .borrow_mut()
                                        .insert(var_name.clone(), value_ty.clone());
                                }
                                Type::Function {
                                    params: pre_params,
                                    ret: pre_ret,
                                    ..
                                } => {
                                    if let Type::Function {
                                        params: actual_params,
                                        ret: actual_ret,
                                        ..
                                    } = &value_ty
                                    {
                                        if let Type::TypeVar(ret_name, _) = pre_ret.as_ref() {
                                            let actual_ret_applied =
                                                subst.apply(actual_ret.as_ref());
                                            if !type_contains_typevar(&actual_ret_applied, ret_name)
                                            {
                                                subst
                                                    .type_map
                                                    .borrow_mut()
                                                    .insert(ret_name.clone(), actual_ret_applied);
                                            }
                                        }
                                        for ((_, pre_ty), (_, actual_ty)) in
                                            pre_params.iter().zip(actual_params.iter())
                                        {
                                            match pre_ty {
                                                Type::TypeVar(param_name, _) => {
                                                    let actual_applied = subst.apply(actual_ty);
                                                    if !type_contains_typevar(
                                                        &actual_applied,
                                                        param_name,
                                                    ) {
                                                        subst.type_map.borrow_mut().insert(
                                                            param_name.clone(),
                                                            actual_applied,
                                                        );
                                                    }
                                                }
                                                Type::Dict(Row {
                                                    tail:
                                                        RowTail::Uniform {
                                                            value: elem_var, ..
                                                        },
                                                    ..
                                                }) => {
                                                    if let Type::TypeVar(elem_name, _) =
                                                        elem_var.as_ref()
                                                    {
                                                        if let Type::Dict(Row {
                                                            tail:
                                                                RowTail::Uniform {
                                                                    value: actual_elem,
                                                                    ..
                                                                },
                                                            ..
                                                        }) = actual_ty
                                                        {
                                                            let actual_elem_applied =
                                                                subst.apply(actual_elem.as_ref());
                                                            if !type_contains_typevar(
                                                                &actual_elem_applied,
                                                                elem_name,
                                                            ) {
                                                                subst.type_map.borrow_mut().insert(
                                                                    elem_name.clone(),
                                                                    actual_elem_applied,
                                                                );
                                                            }
                                                        }
                                                    }
                                                }
                                                _ => {}
                                            }
                                        }
                                    }
                                }
                                _ => {}
                            }
                            field_types.insert(name.clone(), value_ty);
                        } else {
                            field_types.insert(name.clone(), value_ty);
                        }
                    }
                    Err(mut errs) => {
                        let error_ty = Type::error_with(errs.clone());
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

        // Merge state.subst into local subst after each SCC.
        {
            let state_type_entries: Vec<(String, Type)> = {
                let state_map = state.subst.type_map.borrow();
                state_map
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            };
            for (k, v) in state_type_entries {
                let applied_v = subst.apply(&v);
                let existing_opt = subst.type_map.borrow().get(&k).cloned();
                match existing_opt {
                    Some(_existing) => {
                        let resolved = subst.apply(&applied_v);
                        subst.type_map.borrow_mut().insert(k, resolved);
                    }
                    None => {
                        subst.type_map.borrow_mut().insert(k, applied_v);
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
            match crate::types::process_deferred_equalities(state, &mut scc_constraints, scc_span)
                .await
            {
                Ok(()) => {}
                Err(e) => errors.push(e),
            }
            state.constraints = scc_constraints;
        }

        // Apply substitution to this SCC's field types
        for &idx in &scc.indices {
            let (ref key_name, _, _) = key_entries[idx];
            if let Some(name) = key_name {
                if let Some(ty) = field_types.get(name) {
                    let resolved_ty = subst.apply(ty);
                    field_types.insert(name.clone(), resolved_ty);
                }
            }
        }

        // Merge local subst into state.subst BEFORE generalization.
        for (k, v) in subst.type_map.borrow().iter() {
            state
                .subst
                .type_map
                .borrow_mut()
                .insert(k.clone(), v.clone());
        }

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

                    // Extract annotation-based narrowing hints for this binding (T-1761).
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
                    // Both produce `scheme.param_narrowings = vec![Some(Type)]` so that
                    // `extract_narrowings(cond, env)` can read it without knowing which
                    // annotation style was used. `None` means "no narrowing declared".
                    let param_narrowings: Vec<Option<crate::type_def::Type>> = 'narrowing: {
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
                                        let mut constraints: Vec<Constraint> = Vec::new();
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
                                                Type::error_note("type resolution failed for narrowing annotation")
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
                                            let mut constraints: Vec<Constraint> = Vec::new();
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
                                                        Type::error_note("type resolution failed for narrowing annotation")
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
                        let mut scheme = generalize_with_doc(
                            enclosing_level,
                            ty,
                            state,
                            doc,
                            entry.span.clone(),
                        );
                        if let Some(inner) = entry_inner_schemes.get(name) {
                            scheme.inner_schemes = Some(inner.clone());
                        }
                        scheme.param_narrowings = param_narrowings;
                        dict_env
                            .write()
                            .unwrap()
                            .insert_scheme_named_only(name.clone(), scheme);
                        continue;
                    }

                    let saved_constraints = std::mem::replace(
                        &mut state.constraints,
                        entry_constraints.get(name).cloned().unwrap_or_default(),
                    );

                    let mut scheme =
                        generalize_with_doc(enclosing_level, ty, state, doc, entry.span.clone());

                    state.constraints = saved_constraints;

                    if let Some(inner) = entry_inner_schemes.get(name) {
                        scheme.inner_schemes = Some(inner.clone());
                    }
                    scheme.param_narrowings = param_narrowings;

                    dict_env
                        .write()
                        .unwrap()
                        .insert_scheme_named_only(name.clone(), scheme);
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
                        dict_env.write().unwrap().insert_scheme_named_only(
                            name.clone(),
                            TypeScheme::mono(adt_value_type(&def.body)),
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
    state.level = enclosing_level;

    // Compact the levels map.
    state.compact_levels();

    // Apply substitutions and detect 2-cycles in field types.
    let resolved_field_types: indexmap::IndexMap<String, Type> = field_types
        .into_iter()
        .map(|(k, v)| {
            let after_local = subst.apply(&v);
            let after_state = state.subst.apply(&after_local);
            let resolved = match (&v, &after_local) {
                (Type::TypeVar(orig_name, _), Type::TypeVar(next_name, _))
                    if orig_name != next_name =>
                {
                    let local_map = subst.type_map.borrow();
                    let is_cycle = local_map
                        .get(next_name.as_str())
                        .is_some_and(|t| matches!(t, Type::TypeVar(n, _) if n == orig_name));
                    drop(local_map);
                    if is_cycle {
                        Type::Unknown
                    } else {
                        after_state
                    }
                }
                _ => after_state,
            };
            (k, resolved)
        })
        .collect();

    let has_spread = entries.iter().any(|e| {
        e.node.key.is_none() && matches!(&e.node.value.expr, SurfaceExpression::Placeholder(..))
    });
    let tail = if has_spread {
        RowTail::Uniform {
            key: None,
            value: Box::new(Type::Any),
        }
    } else {
        RowTail::Empty
    };
    let record_type = Type::Dict(Row {
        fields: resolved_field_types,
        tail,
    });

    // Drain remaining dispatch obligations as type errors.
    for obligation in state.dispatch_obligations.drain(..) {
        if let crate::ast::SurfaceExpression::VarRef { call_dispatch, .. } =
            &obligation.varref_node.expr
        {
            if call_dispatch.get().is_none() {
                errors.push(crate::error::TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "no [{}] instance found for method [{}]",
                        obligation.class_name, obligation.method_name
                    ),
                    obligation.varref_node.span.clone(),
                ));
            }
        }
    }

    // B-477: Pop the synthetic scope frame we pushed before Pass 2 (if we pushed one).
    if pushed_synthetic_frame {
        if let Some(ref mut frames) = state.scope_frames {
            frames.pop();
        }
    }

    (record_type, schemes, errors)
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
            call_dispatch: crate::ast::CallDispatch::new(),
            annotation: None,
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
        })
    }

    /// B-524: `collect_dependencies` must not produce a dep edge from a function value to a
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

    // ===== AfterFieldBase type inference tests =====

    /// `field_type_from_base` returns the correct type for a closed Dict row.
    ///
    /// For `{x: Int, y: Str}` (closed, Empty tail), accessing `.x` must return `Int`
    /// and accessing `.y` must return `Str`. All dot-access desugars to `builtin-get`
    /// (key-based lookup); no slot index is computed or returned.
    #[test]
    fn test_field_type_from_base_closed_dict_slot() {
        use crate::ast::DotKey;
        use crate::type_def::{Row, RowTail};
        use indexmap::IndexMap;

        let mut fields: IndexMap<String, Type> = IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        fields.insert("y".to_string(), Type::Str);
        let row = Row {
            fields,
            tail: RowTail::Empty,
        };
        let base_ty = Type::Dict(row);

        let span = crate::rust_span!();
        let mut errors = Vec::new();

        // Accessing .x
        let ty_x = field_type_from_base(
            &base_ty,
            &DotKey::Ident("x".to_string()),
            &span,
            &mut errors,
        );
        assert_eq!(ty_x, Type::Int, ".x should resolve to Int");
        assert!(errors.is_empty(), "No errors expected for .x");

        // Accessing .y
        let ty_y = field_type_from_base(
            &base_ty,
            &DotKey::Ident("y".to_string()),
            &span,
            &mut errors,
        );
        assert_eq!(ty_y, Type::Str, ".y should resolve to Str");
        assert!(errors.is_empty(), "No errors expected for .y");

        // Accessing an absent field — Unknown type.
        // Under BAS width subtyping, an annotated record type {x: Int} (Empty tail) does NOT
        // mean "exactly these fields" — it means "at least these fields." A value at this type
        // may have additional fields at runtime. Returning Unknown (not an error) is correct:
        // the field may exist, we just don't statically know its type.
        let ty_z = field_type_from_base(
            &base_ty,
            &DotKey::Ident("z".to_string()),
            &span,
            &mut errors,
        );
        assert_eq!(
            ty_z,
            Type::Unknown,
            ".z absent from row → Unknown (BAS width subtyping)"
        );
        assert!(
            errors.is_empty(),
            "No errors expected for absent field — BAS allows extra fields"
        );
    }

    /// Open Dict rows (Uniform tail) resolve field types correctly.
    ///
    /// A Uniform tail means the row is open — additional fields may be present at runtime.
    /// All dot-access desugars to `builtin-get` (key-based lookup) regardless of tail kind.
    #[test]
    fn test_field_type_from_base_open_dict_no_slot() {
        use crate::ast::DotKey;
        use crate::type_def::{Row, RowTail};
        use indexmap::IndexMap;

        let mut fields: IndexMap<String, Type> = IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        let row = Row {
            fields,
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Any),
            },
        };
        let base_ty = Type::Dict(row);

        let span = crate::rust_span!();
        let mut errors = Vec::new();

        let ty = field_type_from_base(
            &base_ty,
            &DotKey::Ident("x".to_string()),
            &span,
            &mut errors,
        );
        assert_eq!(ty, Type::Int, ".x should resolve to Int in open row");
        assert!(errors.is_empty());
    }

    /// `AfterFieldBase` resolves the field type and returns it via `TypeCheckAction::Done`.
    ///
    /// Directly calls `apply_cont` with an `AfterFieldBase` continuation and a closed
    /// Dict base type `{x: Int, y: Str}` to verify that `.x` resolves to `Type::Int`.
    /// This tests the handler in isolation without requiring the full `run_typecheck` pipeline.
    #[tokio::test]
    async fn test_after_field_base_resolves_type() {
        use crate::ast::DotKey;
        use crate::type_def::{Row, RowTail};
        use indexmap::IndexMap;

        // Closed Dict row: {x: Int, y: Str}
        let mut fields: IndexMap<String, Type> = IndexMap::new();
        fields.insert("x".to_string(), Type::Int);
        fields.insert("y".to_string(), Type::Str);
        let base_ty = Type::Dict(Row {
            fields,
            tail: RowTail::Empty,
        });

        // Set up the continuation
        let cont = TypeCheckCont::FieldBase {
            field: DotKey::Ident("x".to_string()),
            span: crate::rust_span!(),
        };

        let mut state = InferState::new();
        let mut errors: Vec<TypeDiagnostic> = Vec::new();
        let mut type_map: Option<&mut TypeMap> = None;
        let mut stack: Vec<TypeCheckCont> = Vec::new();

        let action = apply_cont(
            cont,
            base_ty,
            &mut state,
            &mut errors,
            &mut type_map,
            &mut stack,
        )
        .await;

        // apply_cont must return Done(Int) for the .x field
        match action {
            TypeCheckAction::Done(ty) => {
                assert_eq!(ty, Type::Int, "AfterFieldBase must resolve .x to Int");
            }
            TypeCheckAction::Eval(_, _) => {
                panic!("AfterFieldBase must not produce Eval action");
            }
        }

        // No unexpected errors
        assert!(errors.is_empty(), "No type errors expected: {:?}", errors);
    }
}
