//! CEK machine for type inference — iterative loop with explicit continuations.
//!
//! Eliminates recursive calls to `infer_surface_expr` by converting the type checker
//! to a continuation-passing style (CPS) machine with defunctionalized continuations.
//! This prevents stack overflow on deeply nested expressions and provides an inspectable
//! continuation stack for error reporting.
//!
//! Architecture:
//! - Control register: current `Arc<SurfaceNode>` to infer OR a completed `Type` result
//! - Continuation stack: `Vec<TypeCheckCont>` (explicit stack of pending work)
//! - Main loop: `run_typecheck` — alternates between `infer_step` and `apply_cont`
//!
//! Both `infer_step` and `apply_cont` are `async fn` — they await external async operations
//! (annotation resolution, async unify) directly. The CEK loop eliminates recursive
//! calls to `run_typecheck` itself, not all async behavior.

#![allow(clippy::too_many_arguments)]

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    node_id, Annotation, Pattern, Span, Spanned, SurfaceDeclaration, SurfaceEntry,
    SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg, SurfaceNode, SurfaceParam,
    STANDARD_ANN_KEYS,
};
use crate::coverage;
use crate::env::Env;
use crate::type_def::{Row, RowTail};
use crate::type_infer::Substitution;
use crate::types::{
    instantiate_at_level, instantiate_scheme, unify, Constraint, InferState, Kind, Type, TypeEnv,
    TypeError, TypeScheme,
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

// ===== TypeCheckCont enum =====

/// Explicit continuation stack for the type checker CEK machine.
///
/// Each variant stores the data needed to resume type checking after a child expression
/// has been inferred. The continuation stack replaces recursive calls to `infer_step`.
///
/// Note: `AfterDictSccMember`, `AfterTypeAliasReg`, and `AfterClassInstancePreReg` are
/// intentional placeholder variants for T-1644 (full dict CEK path). They have `apply_cont`
/// handlers but are not yet pushed (the Dict arm currently delegates to `AfterDictPassZero`
/// which in turn delegates to `infer_surface_expr`). The `#[allow(dead_code)]` below
/// suppresses the "variant never constructed" warning for these three.
#[allow(dead_code)]
pub(crate) enum TypeCheckCont {
    /// After inferring a function body, restore saved level/expected_return and build fn type.
    AfterFnBody {
        saved_level: u32,
        saved_expected_return: Option<Type>,
        /// Pre-resolved return annotation type (overrides body type when concrete).
        return_ann: Option<Type>,
        /// Resolved param types.
        params: Vec<(Option<String>, Type)>,
        is_variadic: bool,
        required_count: usize,
        node_span: Span,
    },

    /// After inferring the function expression in a call, start processing arguments.
    AfterCallFunc {
        args: Vec<Arc<SurfaceNode>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        env: Arc<RwLock<Env>>,
        span: Span,
        call_node: Arc<SurfaceNode>,
    },

    /// After inferring one argument, continue with remaining or finalize the return type.
    AfterCallArg {
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
        fn_variadic: bool,
        fn_required: usize,
        env: Arc<RwLock<Env>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        span: Span,
        call_node: Arc<SurfaceNode>,
    },

    /// After inferring the scrutinee of a match, start processing arms.
    AfterMatchScrutinee {
        arms: Vec<SurfaceMatchArm>,
        env: Arc<RwLock<Env>>,
        span: Span,
    },

    /// After inferring one match arm body, continue with remaining arms.
    AfterMatchArm {
        remaining_arms: Vec<SurfaceMatchArm>,
        env: Arc<RwLock<Env>>,
        accumulated_types: Vec<Type>,
        scrutinee_ty: Type,
        remaining_scrutinee: Type,
        span: Span,
    },

    /// After inferring one member of an SCC, advance to the next.
    AfterDictSccMember {
        scc_indices: Vec<usize>,
        current_member_pos: usize,
        entries: Vec<Spanned<SurfaceEntry>>,
        key_entries: Vec<(Option<String>, bool, bool)>,
        field_types: HashMap<String, Type>,
        schemes: indexmap::IndexMap<String, TypeScheme>,
        scc_env: Arc<RwLock<Env>>,
        dict_env: Arc<RwLock<Env>>,
        local_subst: Substitution,
        entry_constraints: HashMap<String, Vec<Constraint>>,
        entry_inner_schemes: HashMap<String, HashMap<String, TypeScheme>>,
        ctor_schemes: HashMap<String, TypeScheme>,
        fresh_vars_by_name: HashMap<String, Type>,
        saved_level: u32,
        remaining_sccs: Vec<Scc>,
        enclosing_level: u32,
        errors: Vec<TypeError>,
        span: Span,
    },

    /// After Pass 0 (key resolution), transition to SCC analysis and body inference.
    ///
    /// Pushed by the `Dict` arm of `infer_step`. The handler delegates the full
    /// multi-pass dict inference to `infer_surface_expr` (which calls `infer_dict`)
    /// until the dict CEK path is fully implemented (T-1644).
    ///
    /// Fields `entries`, `key_entries`, `enclosing_level`, and `span` are intentionally
    /// included for use when T-1644 implements the full dict CEK path — the handler will
    /// need them for SCC computation and pass transitions. Until then only `dict_node` and
    /// `env` are read by the handler.
    AfterDictPassZero {
        /// The original Dict node — used to delegate to infer_surface_expr.
        dict_node: Arc<SurfaceNode>,
        #[allow(dead_code)]
        entries: Vec<Spanned<SurfaceEntry>>,
        #[allow(dead_code)]
        key_entries: Vec<(Option<String>, bool, bool)>,
        env: Arc<RwLock<Env>>,
        #[allow(dead_code)]
        enclosing_level: u32,
        #[allow(dead_code)]
        span: Span,
    },

    /// After registering type aliases (Pass 2), proceed to class/instance pre-registration.
    AfterTypeAliasReg {
        entries: Vec<Spanned<SurfaceEntry>>,
        key_entries: Vec<(Option<String>, bool, bool)>,
        dict_env: Arc<RwLock<Env>>,
        ctor_schemes: HashMap<String, TypeScheme>,
        fresh_vars_by_name: HashMap<String, Type>,
        enclosing_level: u32,
        span: Span,
    },

    /// After class/instance pre-registration (Pass 0c), start SCC processing.
    AfterClassInstancePreReg {
        sccs: Vec<Scc>,
        entries: Vec<Spanned<SurfaceEntry>>,
        key_entries: Vec<(Option<String>, bool, bool)>,
        dict_env: Arc<RwLock<Env>>,
        ctor_schemes: HashMap<String, TypeScheme>,
        fresh_vars_by_name: HashMap<String, Type>,
        field_types: HashMap<String, Type>,
        errors: Vec<TypeError>,
        enclosing_level: u32,
        span: Span,
    },

    /// After inferring one sequential expression, continue with the next.
    AfterSequentialExpr {
        remaining: Vec<Arc<SurfaceNode>>,
        env: Arc<RwLock<Env>>,
    },

    /// After inferring the inner expression of a TypeAssert, validate against expected type.
    AfterTypeAssertInner {
        expected: Type,
        has_default: bool,
        default_node: Option<Arc<SurfaceNode>>,
        env: Arc<RwLock<Env>>,
        span: Span,
        /// Retained for future error reporting (annotating mismatch with annotation source location).
        #[allow(dead_code)]
        annotation_span: Span,
    },

    /// After inferring the base expression of a Field access, look up the field type.
    AfterFieldBase {
        field: crate::ast::DotKey,
        span: Span,
    },

    /// After inferring the inner expression of an Unquote, return its type.
    AfterUnquote,

    /// After inferring the inner expression of an UnquoteSplice, return Unknown.
    AfterUnquoteSplice,
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
    errors: &mut Vec<TypeError>,
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
        let key = (span.start.offset, span.end.offset);
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
    errors: &mut Vec<TypeError>,
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
        SurfaceExpression::Placeholder => TypeCheckAction::Done(Type::Unknown),

        SurfaceExpression::Rest(..) => TypeCheckAction::Done(Type::Dict(Row {
            fields: indexmap::IndexMap::new(),
            tail: RowTail::Uniform {
                key: None,
                value: Box::new(Type::Any),
            },
        })),

        SurfaceExpression::Pipe { .. } => {
            errors.push(TypeError::new(
                "Pipe should be desugared before type checking",
                node.span.clone(),
            ));
            TypeCheckAction::Done(Type::error_note(
                "Pipe should be desugared before type checking",
            ))
        }

        SurfaceExpression::LetDecl { .. } => {
            let msg = "binding declaration [let ...] is not valid in expression position";
            errors.push(TypeError::new(msg, node.span.clone()));
            TypeCheckAction::Done(Type::error_note(msg.to_string()))
        }

        SurfaceExpression::PatternDecl { .. } => {
            errors.push(TypeError::new(
                "pattern declaration is only valid in instance match arms",
                node.span.clone(),
            ));
            TypeCheckAction::Done(Type::error_note(
                "pattern declaration is only valid in instance match arms",
            ))
        }

        SurfaceExpression::Error(_) => {
            let msg = "parse error node in expression position";
            errors.push(TypeError::new(msg, node.span.clone()));
            TypeCheckAction::Done(Type::error_note(msg.to_string()))
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
            stack.push(TypeCheckCont::AfterUnquote);
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            stack.push(TypeCheckCont::AfterUnquoteSplice);
            TypeCheckAction::Eval(Arc::clone(inner), Arc::clone(env))
        }

        // ===== Field access — compound: evaluate base first =====
        SurfaceExpression::Field { expr, field, .. } => match expr {
            None => TypeCheckAction::Done(Type::Unknown),
            Some(base) => {
                stack.push(TypeCheckCont::AfterFieldBase {
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
            let stub_env = TypeEnv::new();
            let mut constraints: Vec<Constraint> = Vec::new();
            let annotation_result = typecheck_annot::resolve_annotation(
                &annotation.node,
                &stub_env,
                annotation.span.clone(),
                state,
                &mut constraints,
                &mut None,
                &mut None,
                None,
            )
            .await;

            match annotation_result {
                Ok(expected) => {
                    stack.push(TypeCheckCont::AfterTypeAssertInner {
                        expected,
                        has_default,
                        default_node,
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

        // ===== Sequential — compound: evaluate first, then chain =====
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
            let first = Arc::clone(&exprs[0]);
            let remaining: Vec<Arc<SurfaceNode>> = exprs[1..].iter().map(Arc::clone).collect();
            stack.push(TypeCheckCont::AfterSequentialExpr {
                remaining,
                env: Arc::clone(env),
            });
            TypeCheckAction::Eval(first, Arc::clone(env))
        }

        // ===== Call — compound: handle special cases inline, else eval func first =====
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied: _,
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

            // Special-case: [if cond t f] with path-sensitive narrowing (handled inline)
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "if" && args.len() == 3 && named_args.is_empty() {
                    let ty = infer_if_expr(
                        &args[0], &args[1], &args[2], node, env, state, errors, type_map,
                    )
                    .await;
                    return TypeCheckAction::Done(ty);
                }

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
            let named_args_cloned: Vec<Spanned<SurfaceNamedArg>> =
                named_args.iter().cloned().collect();
            stack.push(TypeCheckCont::AfterCallFunc {
                args: args_cloned,
                named_args: named_args_cloned,
                env: Arc::clone(env),
                span: node.span.clone(),
                call_node: Arc::clone(node),
            });
            TypeCheckAction::Eval(Arc::clone(func), Arc::clone(env))
        }

        // ===== Fn — resolve annotations, build env, push AfterFnBody, eval body =====
        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            ..
        } => {
            infer_fn_push_cont(
                return_ann, params, body, node, env, state, errors, type_map, stack,
            )
            .await
        }

        // ===== Dict — push AfterDictPassZero, complete inference in apply_cont =====
        //
        // The full multi-pass dict inference (Passes 0–3) is performed in the AfterDictPassZero
        // handler. We push the continuation first, then return Done(Unknown) to immediately
        // trigger apply_cont — no child node to evaluate for Pass 0 (it is synchronous).
        // This makes AfterDictPassZero actively pushed (non-dead) while the implementation
        // in the handler still delegates to infer_surface_expr until T-1644 is complete.
        SurfaceExpression::Dict(entries) => {
            let key_entries = {
                let mut auto_index: i64 = 0;
                let mut result = Vec::with_capacity(entries.len());
                for entry in entries {
                    let key_name =
                        entry_key_name(&entry.node, &mut auto_index, env, state, type_map).await;
                    let is_alias = matches!(
                        &entry.node.value.expr,
                        SurfaceExpression::Decl(d) if matches!(d.as_ref(), SurfaceDeclaration::TypeAlias { .. })
                    );
                    let is_static_key = entry.node.key.as_ref().is_some_and(|k| {
                        matches!(
                            &k.expr,
                            SurfaceExpression::StringLiteral { .. }
                                | SurfaceExpression::VarRef { .. }
                        )
                    });
                    result.push((key_name, is_alias, is_static_key));
                }
                result
            };
            let enclosing_level = state.level;
            stack.push(TypeCheckCont::AfterDictPassZero {
                dict_node: Arc::clone(node),
                entries: entries.to_vec(),
                key_entries,
                env: Arc::clone(env),
                enclosing_level,
                span: node.span.clone(),
            });
            // Return Done(Unknown) immediately to trigger apply_cont — Pass 0 is synchronous,
            // there is no child node to evaluate. The handler does the actual inference.
            TypeCheckAction::Done(Type::Unknown)
        }

        // ===== Match — compound: eval scrutinee first =====
        SurfaceExpression::Match { scrutinee, arms } => {
            let arms_cloned: Vec<SurfaceMatchArm> = arms.iter().cloned().collect();
            stack.push(TypeCheckCont::AfterMatchScrutinee {
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

        // ===== Decl — delegate to infer_surface_expr (private helpers not accessible) =====
        SurfaceExpression::Decl(_decl_box) => {
            // Delegate to the existing infer_surface_expr which handles all Decl variants.
            // ClassDecl/InstanceDecl/TypeAlias/Splice/SyntaxClass require private helpers in
            // typecheck.rs (infer_class_decl_from_surface, infer_instance_decl_from_surface, etc.)
            // that are not accessible from this module. infer_surface_expr is the bridge.
            match Box::pin(super::infer_surface_expr(node, env, state, type_map)).await {
                Ok(t) => TypeCheckAction::Done(t),
                Err(mut errs) => {
                    errors.append(&mut errs);
                    TypeCheckAction::Done(Type::error_note("declaration inference error"))
                }
            }
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
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    match cont {
        // ===== AfterFnBody =====
        TypeCheckCont::AfterFnBody {
            saved_level,
            saved_expected_return,
            return_ann,
            params,
            is_variadic,
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
                            && !Type::is_consistent_subtype(&body_resolved, &declared_ret)
                        {
                            errors.push(TypeError::type_mismatch(
                                &declared_ret,
                                &body_resolved,
                                node_span,
                            ));
                        }
                    }
                    match &declared_ret {
                        Type::Unknown => child_ty,
                        _ => declared_ret,
                    }
                }
                None => child_ty,
            };

            TypeCheckAction::Done(Type::Function {
                params,
                ret: Box::new(fn_ret_ty),
                variadic: is_variadic,
                required_count,
            })
        }

        // ===== AfterCallFunc =====
        TypeCheckCont::AfterCallFunc {
            args,
            named_args,
            env,
            span,
            call_node,
        } => {
            let func_ty = if state.subst_is_empty() {
                child_ty
            } else {
                state.apply(&child_ty)
            };

            apply_cont_call_func(
                func_ty, args, named_args, env, span, call_node, state, errors, type_map, stack,
            )
            .await
        }

        // ===== AfterCallArg =====
        TypeCheckCont::AfterCallArg {
            idx,
            remaining_args,
            mut accumulated_arg_types,
            arg_nodes,
            param_types,
            fn_ret,
            fn_variadic,
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
                stack.push(TypeCheckCont::AfterCallArg {
                    idx: idx + 1,
                    remaining_args: new_remaining,
                    accumulated_arg_types,
                    arg_nodes,
                    param_types,
                    fn_ret,
                    fn_variadic,
                    fn_required,
                    env: Arc::clone(&env),
                    named_args,
                    span,
                    call_node,
                });
                return TypeCheckAction::Eval(next_arg, env);
            }

            // All positional args collected — unify and handle named args
            apply_call_args_poly(
                accumulated_arg_types,
                arg_nodes,
                param_types,
                fn_ret,
                fn_variadic,
                fn_required,
                named_args,
                env,
                span,
                state,
                errors,
                type_map,
            )
            .await
        }

        // ===== AfterMatchScrutinee =====
        TypeCheckCont::AfterMatchScrutinee { arms, env, span } => {
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
                &scrutinee_ty,
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
                    let remaining_arms: Vec<SurfaceMatchArm> = arms[1..].iter().cloned().collect();
                    stack.push(TypeCheckCont::AfterMatchArm {
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
        TypeCheckCont::AfterMatchArm {
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
                    &scrutinee_ty,
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
                        let next_remaining: Vec<SurfaceMatchArm> =
                            remaining_arms[1..].iter().cloned().collect();
                        stack.push(TypeCheckCont::AfterMatchArm {
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

        // ===== AfterDictSccMember =====
        TypeCheckCont::AfterDictSccMember {
            scc_indices,
            current_member_pos,
            entries,
            key_entries,
            mut field_types,
            schemes,
            scc_env,
            dict_env,
            local_subst,
            entry_constraints,
            entry_inner_schemes,
            ctor_schemes,
            fresh_vars_by_name,
            saved_level,
            remaining_sccs,
            enclosing_level,
            errors: dict_errors,
            span,
        } => {
            // Record the type for the completed member
            let member_idx = scc_indices[current_member_pos];
            let (ref key_name, is_alias, _) = key_entries[member_idx];
            if let Some(name) = key_name {
                if !is_alias {
                    field_types.insert(name.clone(), child_ty);
                }
            }

            let next_pos = current_member_pos + 1;
            if next_pos < scc_indices.len() {
                // Find next non-alias, non-Rest member in this SCC
                let next_idx = scc_indices[next_pos];
                let (ref _next_key, next_is_alias, _) = key_entries[next_idx];
                let skip = next_is_alias
                    || matches!(
                        &entries[next_idx].node.value.expr,
                        SurfaceExpression::Rest(..)
                    )
                    || matches!(
                        &entries[next_idx].node.value.expr,
                        SurfaceExpression::Decl(d)
                            if matches!(
                                d.as_ref(),
                                SurfaceDeclaration::ClassDecl { .. }
                                    | SurfaceDeclaration::InstanceDecl { .. }
                            )
                    );

                if skip {
                    // Re-push this continuation with advanced position, return Done to trigger re-pop
                    stack.push(TypeCheckCont::AfterDictSccMember {
                        scc_indices,
                        current_member_pos: next_pos,
                        entries,
                        key_entries,
                        field_types,
                        schemes,
                        scc_env,
                        dict_env,
                        local_subst,
                        entry_constraints,
                        entry_inner_schemes,
                        ctor_schemes,
                        fresh_vars_by_name,
                        saved_level,
                        remaining_sccs,
                        enclosing_level,
                        errors: dict_errors,
                        span,
                    });
                    return TypeCheckAction::Done(Type::Unknown);
                }

                stack.push(TypeCheckCont::AfterDictSccMember {
                    scc_indices,
                    current_member_pos: next_pos,
                    entries: entries.clone(),
                    key_entries: key_entries.clone(),
                    field_types,
                    schemes,
                    scc_env: Arc::clone(&scc_env),
                    dict_env,
                    local_subst,
                    entry_constraints,
                    entry_inner_schemes,
                    ctor_schemes,
                    fresh_vars_by_name,
                    saved_level,
                    remaining_sccs,
                    enclosing_level,
                    errors: dict_errors,
                    span,
                });
                TypeCheckAction::Eval(Arc::clone(&entries[next_idx].node.value), scc_env)
            } else {
                // SCC complete — AfterDictPassZero delegated to infer_surface_expr for now (T-1644).
                TypeCheckAction::Done(Type::Unknown)
            }
        }

        // ===== AfterSequentialExpr =====
        TypeCheckCont::AfterSequentialExpr { remaining, env } => {
            if remaining.is_empty() {
                TypeCheckAction::Done(child_ty)
            } else if remaining.len() == 1 {
                TypeCheckAction::Eval(Arc::clone(&remaining[0]), env)
            } else {
                let next = Arc::clone(&remaining[0]);
                let new_remaining: Vec<Arc<SurfaceNode>> =
                    remaining[1..].iter().map(Arc::clone).collect();
                stack.push(TypeCheckCont::AfterSequentialExpr {
                    remaining: new_remaining,
                    env: Arc::clone(&env),
                });
                TypeCheckAction::Eval(next, env)
            }
        }

        // ===== AfterTypeAssertInner =====
        TypeCheckCont::AfterTypeAssertInner {
            expected,
            has_default,
            default_node,
            env,
            span,
            annotation_span: _,
        } => {
            let actual = child_ty;
            let expected_resolved = state.subst.apply(&expected);
            let actual_resolved = state.subst.apply(&actual);

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
                ) || ((contains_unknown_or_top_local(&default_resolved)
                    || contains_unknown_or_top_local(&expected_resolved))
                    && Type::is_consistent(&default_resolved, &expected_resolved));
                if !passes {
                    errors.push(TypeError::new(
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
        TypeCheckCont::AfterFieldBase { field, span } => {
            let resolved_base = state.subst.apply(&child_ty);
            let ty = field_type_from_base(&resolved_base, &field, &span, errors);
            TypeCheckAction::Done(ty)
        }

        // ===== AfterUnquote =====
        TypeCheckCont::AfterUnquote => TypeCheckAction::Done(child_ty),

        // ===== AfterUnquoteSplice =====
        TypeCheckCont::AfterUnquoteSplice => TypeCheckAction::Done(Type::Unknown),

        // ===== AfterDictPassZero =====
        //
        // Pushed by the Dict arm of infer_step after synchronous Pass 0 (key name resolution).
        // Delegates the full multi-pass dict inference to infer_surface_expr, which calls
        // infer_dict in typecheck_dict.rs.
        //
        // Once T-1644 implements the full dict CEK path (AfterTypeAliasReg →
        // AfterClassInstancePreReg → AfterDictSccMember), this handler will implement
        // the pass transitions directly rather than delegating to infer_surface_expr.
        TypeCheckCont::AfterDictPassZero { dict_node, env, .. } => {
            let ty = match Box::pin(super::infer_surface_expr(&dict_node, &env, state, type_map))
                .await
            {
                Ok(t) => t,
                Err(mut errs) => {
                    errors.append(&mut errs);
                    Type::error_note("dict inference error")
                }
            };
            TypeCheckAction::Done(ty)
        }

        // ===== AfterTypeAliasReg =====
        //
        // Pushed by AfterDictPassZero once the full dict CEK implementation is in place (T-1644).
        // Currently unreachable — AfterDictPassZero delegates to infer_surface_expr instead.
        TypeCheckCont::AfterTypeAliasReg { .. } => TypeCheckAction::Done(child_ty),

        // ===== AfterClassInstancePreReg =====
        //
        // Pushed by AfterTypeAliasReg once the full dict CEK implementation is in place (T-1644).
        // Currently unreachable — AfterDictPassZero delegates to infer_surface_expr instead.
        TypeCheckCont::AfterClassInstancePreReg { .. } => TypeCheckAction::Done(child_ty),
    }
}

// ===== Inline helper: VarRef inference =====

async fn infer_var_ref(
    name: &str,
    annotation: Option<&Spanned<Annotation>>,
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
) -> Type {
    // Name-based lookup first, then slot-based fallback for ɪ-prefixed class methods.
    let name_scheme = env.read().unwrap().get_scheme(name);
    let scheme: Option<TypeScheme> = if name_scheme.is_some() {
        name_scheme
    } else {
        let mut slot_scheme: Option<TypeScheme> = None;
        if let Some(ref table) = state.resolution_table {
            let id = node_id(node);
            if let Some(&(level, slot)) = table.get(&id) {
                slot_scheme = env.read().unwrap().get_scheme_at(level, slot);
            }
        }
        slot_scheme
    };

    if let Some(scheme) = scheme {
        if !scheme.constraints.is_empty()
            || !scheme.type_vars.is_empty()
            || !scheme.kind_vars.is_empty()
        {
            if let Some(ref mut smap) = state.scheme_map {
                let key = (node.span.start.offset, node.span.end.offset);
                smap.insert(key, scheme.clone());
            }
        }
        instantiate_scheme(
            &scheme,
            state.level,
            state,
            Some(name),
            Some(node.span.clone()),
            &node.span,
        )
    } else {
        let mut err = TypeError::undefined_variable(name, node.span.clone());
        if let Some(cause_span) = state.failed_bindings.get(name) {
            err.notes.push(format!(
                "  = note: `{}` could not be defined because its definition at {}:{} failed type checking",
                name, cause_span.start.line, cause_span.start.column
            ));
        }

        // Gradual typing: use inline annotation if present
        if let Some(ann) = annotation {
            let stub_env = TypeEnv::new();
            let mut constraints: Vec<Constraint> = Vec::new();
            if name == "Fn" || name == "Function" {
                let ret_ty = match typecheck_annot::resolve_annotation(
                    &ann.node,
                    &stub_env,
                    ann.span.clone(),
                    state,
                    &mut constraints,
                    &mut None,
                    &mut None,
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
                Type::Function {
                    params: vec![],
                    ret: Box::new(ret_ty),
                    variadic: false,
                    required_count: 0,
                }
            } else {
                let ty = match typecheck_annot::resolve_annotation(
                    &ann.node,
                    &stub_env,
                    ann.span.clone(),
                    state,
                    &mut constraints,
                    &mut None,
                    &mut None,
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
            }
        } else {
            errors.push(err);
            Type::Error(Arc::new(vec![]))
        }
    }
}

// ===== Inline helper: [if cond t f] with narrowing =====

async fn infer_if_expr(
    cond_node: &Arc<SurfaceNode>,
    true_node: &Arc<SurfaceNode>,
    false_node: &Arc<SurfaceNode>,
    _call_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
) -> Type {
    let _cond_ty = {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            cond_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };

    let narrowings = typecheck_narrow::extract_narrowings(cond_node);

    let true_ty = if narrowings.is_empty() {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            true_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    } else {
        let narrowed_env = typecheck_narrow::apply_narrowings(env, &narrowings, state);
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            true_node,
            &narrowed_env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };

    let false_ty = {
        let mut local_stack = Vec::new();
        Box::pin(run_typecheck(
            false_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await
    };

    let true_ty = typecheck_call::widen_literal_types(true_ty);
    let false_ty = typecheck_call::widen_literal_types(false_ty);

    if true_ty == false_ty {
        true_ty
    } else {
        Type::normalize_union(vec![true_ty, false_ty])
    }
}

// ===== Inline helper: get / builtin-get / get? special case =====

async fn infer_get_call(
    name: &str,
    key_node: &Arc<SurfaceNode>,
    container_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
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

    let field_ty = if let (Type::StringLiteral(field_name), Type::Dict(row)) =
        (&key_resolved, &container_resolved)
    {
        row.fields
            .get(field_name.as_str())
            .cloned()
            .unwrap_or(Type::Unknown)
    } else if let (Type::StringLiteral(field_name), Type::Union(members)) =
        (&key_resolved, &container_resolved)
    {
        let field_types: Vec<Type> = members
            .iter()
            .filter_map(|m| {
                if let Type::Dict(row) = m {
                    row.fields.get(field_name.as_str()).cloned()
                } else {
                    None
                }
            })
            .collect();
        if field_types.is_empty() {
            Type::Unknown
        } else {
            Type::normalize_union(field_types)
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
    errors: &mut Vec<TypeError>,
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
        let _ = Box::pin(run_typecheck(
            path_node,
            env,
            state,
            errors,
            type_map,
            &mut local_stack,
        ))
        .await;
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
    let _ = Box::pin(run_typecheck(
        path_node,
        env,
        state,
        errors,
        type_map,
        &mut local_stack,
    ))
    .await;
    Type::Unknown
}

// ===== Inline helper: call func type dispatch =====

async fn apply_cont_call_func(
    func_ty: Type,
    args: Vec<Arc<SurfaceNode>>,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    span: Span,
    call_node: Arc<SurfaceNode>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    // Error cascade suppression
    if matches!(func_ty, Type::Error(_)) {
        for arg in &args {
            let mut local_stack = Vec::new();
            let _ = Box::pin(run_typecheck(
                arg,
                &env,
                state,
                errors,
                type_map,
                &mut local_stack,
            ))
            .await;
        }
        for na in &named_args {
            let mut local_stack = Vec::new();
            let _ = Box::pin(run_typecheck(
                &na.node.value,
                &env,
                state,
                errors,
                type_map,
                &mut local_stack,
            ))
            .await;
        }
        errors.push(TypeError::not_a_function(&func_ty, call_node.span.clone()));
        return TypeCheckAction::Done(Type::Unknown);
    }

    match &func_ty {
        Type::Function {
            params,
            ret,
            variadic,
            required_count,
        } => {
            // Instantiate if needed
            let (inst_params, inst_ret, inst_variadic, inst_required): (
                Vec<(Option<String>, Type)>,
                Type,
                bool,
                usize,
            ) = if func_ty.has_inference_vars() {
                // CALL-POLY: instantiate at current level
                let inst_ty = instantiate_at_level(&func_ty, state, &span);
                match inst_ty {
                    Type::Function {
                        params,
                        ret,
                        variadic,
                        required_count,
                    } => (params, *ret, variadic, required_count),
                    _ => unreachable!("instantiate_at_level preserves Function variant"),
                }
            } else {
                // CALL-MONO: use as-is
                (params.clone(), (**ret).clone(), *variadic, *required_count)
            };

            if args.is_empty() {
                // No positional args — handle named args and return
                let result = finalize_call_no_positional_args(
                    inst_params,
                    inst_ret,
                    inst_variadic,
                    inst_required,
                    named_args,
                    &env,
                    span,
                    state,
                    errors,
                    type_map,
                )
                .await;
                return TypeCheckAction::Done(result);
            }

            // Start evaluating positional args
            let first_arg = Arc::clone(&args[0]);
            let arg_nodes: Vec<Arc<SurfaceNode>> = args.iter().map(Arc::clone).collect();
            let remaining: Vec<Arc<SurfaceNode>> = args[1..].iter().map(Arc::clone).collect();
            stack.push(TypeCheckCont::AfterCallArg {
                idx: 0,
                remaining_args: remaining,
                accumulated_arg_types: Vec::new(),
                arg_nodes,
                param_types: inst_params,
                fn_ret: inst_ret,
                fn_variadic: inst_variadic,
                fn_required: inst_required,
                env: Arc::clone(&env),
                named_args,
                span,
                call_node,
            });
            TypeCheckAction::Eval(first_arg, env)
        }

        Type::TypeVar(_, _) => {
            // Unbound TypeVar: infer args for side effects, return fresh TypeVar for return
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    state,
                    errors,
                    type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    state,
                    errors,
                    type_map,
                    &mut local_stack,
                ))
                .await;
            }
            let ret_var = state
                .fresh_type_var_with(Some("ret"), None, Kind::Type, &span)
                .1;
            TypeCheckAction::Done(ret_var)
        }

        Type::Unknown | Type::Any => {
            for arg in &args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    arg,
                    &env,
                    state,
                    errors,
                    type_map,
                    &mut local_stack,
                ))
                .await;
            }
            for na in &named_args {
                let mut local_stack = Vec::new();
                let _ = Box::pin(run_typecheck(
                    &na.node.value,
                    &env,
                    state,
                    errors,
                    type_map,
                    &mut local_stack,
                ))
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
                errors.push(TypeError::new(
                    format!(
                        "unit variant constructor takes exactly 1 argument, got {}",
                        args.len()
                    ),
                    span,
                ));
                return TypeCheckAction::Done(Type::Unknown);
            }
            if !named_args.is_empty() {
                errors.push(TypeError::new(
                    "unit variant constructor does not accept named arguments",
                    span,
                ));
                return TypeCheckAction::Done(Type::Unknown);
            }
            let tycon = tycon.clone();
            let ctor = ctor.clone();
            let arg_ty = {
                let mut local_stack = Vec::new();
                Box::pin(run_typecheck(
                    &args[0],
                    &env,
                    state,
                    errors,
                    type_map,
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

        _ => {
            errors.push(TypeError::new(
                format!("expected function type, got {}", func_ty),
                call_node.span.clone(),
            ));
            TypeCheckAction::Done(Type::Unknown)
        }
    }
}

// ===== Inline helper: finalize call with no positional args =====

async fn finalize_call_no_positional_args(
    params: Vec<(Option<String>, Type)>,
    ret: Type,
    variadic: bool,
    required_count: usize,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: &Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
) -> Type {
    let min_required = if variadic && !params.is_empty() {
        required_count.saturating_sub(1)
    } else {
        required_count
    };

    if named_args.is_empty() && min_required > 0 {
        errors.push(TypeError::new(
            format!("arity mismatch: expected {} arguments, got 0", min_required),
            span.clone(),
        ));
        return state.apply(&ret);
    }

    let mut consumed: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            errors.push(TypeError::new(
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
                        state,
                        errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await
                };
                let mut constraints = std::mem::take(&mut state.constraints);
                let _ = unify(&arg_ty, &param_ty, state, &mut constraints, na.span.clone()).await;
                state.constraints = constraints;
            }
            None => {
                if !variadic {
                    errors.push(TypeError::new(
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
                        state,
                        errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
            }
        }
    }

    state.apply(&ret)
}

// ===== Inline helper: CALL-POLY arg unification =====

async fn apply_call_args_poly(
    arg_types: Vec<Type>,
    arg_nodes: Vec<Arc<SurfaceNode>>,
    param_types: Vec<(Option<String>, Type)>,
    fn_ret: Type,
    fn_variadic: bool,
    fn_required: usize,
    named_args: Vec<Spanned<SurfaceNamedArg>>,
    env: Arc<RwLock<Env>>,
    span: Span,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
) -> TypeCheckAction {
    let non_variadic_param_count = if fn_variadic && !param_types.is_empty() {
        param_types.len() - 1
    } else {
        param_types.len()
    };
    let min_required = if fn_variadic && !param_types.is_empty() {
        fn_required.saturating_sub(1)
    } else {
        fn_required
    };
    let total_supplied = arg_types.len() + named_args.len();

    // Arity check
    if total_supplied < min_required || (!fn_variadic && arg_types.len() > param_types.len()) {
        errors.push(TypeError::new(
            format!(
                "arity mismatch: expected {} arguments, got {}",
                min_required, total_supplied
            ),
            span.clone(),
        ));
        return TypeCheckAction::Done(state.apply(&fn_ret));
    }

    // Unify positional args against params (Robinson unification via unify())
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

        // Gradual typing boundary guard (per check_call_args pattern in typecheck_call.rs).
        // When an Unknown/Any arg flows into a concrete parameter, attach a runtime guard so
        // the evaluator can enforce the type contract at the Unknown→concrete boundary.
        if matches!(&widened_arg, Type::Unknown | Type::Any)
            && typecheck_call::is_concrete_type(&state.subst.apply(param_ty))
        {
            if let Some(arg_node) = arg_nodes.get(idx) {
                arg_node.type_guard.set(Some(state.subst.apply(param_ty)));
            }
        }

        let mut constraints = std::mem::take(&mut state.constraints);
        if let Err(e) = Box::pin(unify(
            param_ty,
            &widened_arg,
            state,
            &mut constraints,
            span.clone(),
        ))
        .await
        {
            errors.push(e);
        }
        state.constraints = constraints;
    }

    // Handle variadic args
    if fn_variadic && arg_types.len() > non_variadic_param_count {
        if let Some((_, variadic_param_ty)) = param_types.last() {
            let elem_ty: Option<Type> = if let Type::Dict(row) = variadic_param_ty {
                match &row.tail {
                    RowTail::Uniform { value, .. } => Some(*value.clone()),
                    _ => None,
                }
            } else if let Type::App(f, arg) = variadic_param_ty {
                if matches!(f.as_ref(), Type::TyCon(n) if n == "Seq") {
                    Some(*arg.clone())
                } else {
                    None
                }
            } else if matches!(variadic_param_ty, Type::TypeVar(_, _)) {
                Some(variadic_param_ty.clone())
            } else {
                None
            };
            if let Some(elem_ty) = elem_ty {
                for arg_ty in arg_types.iter().skip(non_variadic_param_count) {
                    let widened = match arg_ty {
                        Type::IntLiteral(_) => Type::Int,
                        Type::StringLiteral(_) => Type::Str,
                        other => other.clone(),
                    };
                    let mut constraints = std::mem::take(&mut state.constraints);
                    if let Err(e) = Box::pin(unify(
                        &elem_ty,
                        &widened,
                        state,
                        &mut constraints,
                        span.clone(),
                    ))
                    .await
                    {
                        errors.push(e);
                    }
                    state.constraints = constraints;
                }
            }
        }
    }

    // Handle named args (CALL-POLY path)
    let mut seen_named_arg_names: std::collections::HashSet<String> = Default::default();
    for na in &named_args {
        if !seen_named_arg_names.insert(na.node.name.clone()) {
            errors.push(TypeError::new(
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
                    errors.push(TypeError::new(
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
                        state,
                        errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await
                };
                let mut constraints = std::mem::take(&mut state.constraints);
                if let Err(e) = Box::pin(unify(
                    &arg_ty,
                    &param_ty,
                    state,
                    &mut constraints,
                    na.span.clone(),
                ))
                .await
                {
                    errors.push(TypeError::new(
                        format!(
                            "named argument '{}' type mismatch: {}",
                            na.node.name, e.message
                        ),
                        na.span.clone(),
                    ));
                }
                state.constraints = constraints;
            }
            None => {
                if !fn_variadic {
                    errors.push(TypeError::new(
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
                        &env,
                        state,
                        errors,
                        type_map,
                        &mut local_stack,
                    ))
                    .await;
                }
            }
        }
    }

    TypeCheckAction::Done(state.apply(&fn_ret))
}

// ===== Inline helper: Fn inference via AfterFnBody continuation =====

/// Resolve all function annotations, build the parameter environment, push `AfterFnBody`,
/// and return `Eval(body, fn_env)` so the CEK loop evaluates the body iteratively without
/// recursing on the Rust call stack.
async fn infer_fn_push_cont(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<SurfaceParam>],
    body: &Arc<SurfaceNode>,
    node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    _type_map: &mut Option<&mut TypeMap>,
    stack: &mut Vec<TypeCheckCont>,
) -> TypeCheckAction {
    let mut ann_mapping_str: HashMap<String, String> = HashMap::new();
    let stub_type_env = TypeEnv::new();
    let mut constraints: Vec<Constraint> = Vec::new();
    let mut ann_mapping_opt = Some(&mut ann_mapping_str);
    let mut row_ann_mapping_str: HashMap<String, String> = HashMap::new();
    let mut row_ann_mapping_opt = Some(&mut row_ann_mapping_str);

    // Resolve return annotation first (populates bind: TypeVars into ann_mapping_str)
    let return_ann_type: Option<Type> = if let Some(ret_ann) = return_ann {
        let resolved = match &ret_ann.node {
            Annotation::PropertyDict(entries)
                if entries.iter().any(|e| {
                    e.node.key.as_ref().map_or(false, |k| {
                        matches!(&k.expr,
                            SurfaceExpression::StringLiteral { content: s, .. }
                                if STANDARD_ANN_KEYS.contains(&s.as_str()))
                    })
                }) =>
            {
                let result = typecheck_annot::resolve_fn_metadata(
                    entries,
                    &stub_type_env,
                    ret_ann.span.clone(),
                    state,
                    &mut constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await;
                match result {
                    Ok((ret_ty, _doc)) => Some(ret_ty),
                    Err(e) => {
                        errors.push(TypeError::from(e));
                        None
                    }
                }
            }
            _ => match typecheck_annot::resolve_annotation(
                &ret_ann.node,
                &stub_type_env,
                ret_ann.span.clone(),
                state,
                &mut constraints,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
                None,
            )
            .await
            {
                Ok(ty) => Some(ty),
                Err(e) => {
                    errors.push(e);
                    None
                }
            },
        };
        resolved
    } else {
        None
    };

    // Resolve param annotations and build fn env
    let mut fn_env_inner = Env::with_parent(Arc::clone(env));
    let mut param_types: Vec<(Option<String>, Type)> = Vec::new();

    for p in params {
        let param_ty = if p.node.variadic {
            let elem_ty = state.fresh_type_var(&p.span);
            Type::Dict(Row {
                fields: indexmap::IndexMap::new(),
                tail: RowTail::Uniform {
                    key: None,
                    value: Box::new(elem_ty),
                },
            })
        } else if let Some(ann) = &p.node.annotation {
            match typecheck_annot::resolve_annotation(
                &ann.node,
                &stub_type_env,
                ann.span.clone(),
                state,
                &mut constraints,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
                None,
            )
            .await
            {
                Ok(ty) => ty,
                Err(e) => {
                    errors.push(e);
                    Type::Unknown
                }
            }
        } else {
            Type::Unknown
        };
        fn_env_inner.insert(p.node.name.clone(), param_ty.clone());
        param_types.push((Some(p.node.name.clone()), param_ty));
    }

    let fn_env_arc = Arc::new(RwLock::new(fn_env_inner));

    let is_variadic = params.iter().any(|p| p.node.variadic);
    let required_count = if is_variadic {
        params.len().saturating_sub(1)
    } else {
        params.len()
    };

    let saved_level = state.level;
    let saved_expected_return = state.expected_return.clone();

    // Push the continuation so apply_cont can build the Function type from the body type.
    stack.push(TypeCheckCont::AfterFnBody {
        saved_level,
        saved_expected_return,
        return_ann: return_ann_type,
        params: param_types,
        is_variadic,
        required_count,
        node_span: node.span.clone(),
    });

    // Evaluate body iteratively via the CEK loop.
    TypeCheckAction::Eval(Arc::clone(body), fn_env_arc)
}

// ===== Inline helper: Match arm environment setup =====
//
// Sets up pattern bindings, infers guard (for side effects), applies guard narrowing,
// and returns the arm environment ready for body evaluation.
// Also returns the updated `remaining_scrutinee` after applying I-Case3 negation for this arm.
// Returns `None` only if called with no arms (should not happen in practice).

async fn setup_match_arm_env(
    arm: &SurfaceMatchArm,
    scrutinee_ty: &Type,
    remaining_scrutinee: &Type,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    errors: &mut Vec<TypeError>,
    type_map: &mut Option<&mut TypeMap>,
) -> Option<(Arc<RwLock<Env>>, Type)> {
    // Compute arm-local scrutinee type (I-Case3 narrowing)
    let arm_scrutinee_ty = match &arm.pattern.node {
        Pattern::Constructor { tag, .. } => {
            let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
            if matches!(remaining_scrutinee, Type::NominalVariant { tycon: t, ctor: c, .. } if t == tycon && c == ctor)
            {
                remaining_scrutinee.clone()
            } else {
                let tag_ty = Type::NominalVariant {
                    tycon: tycon.to_string(),
                    ctor: ctor.to_string(),
                    fields: crate::type_def::Row {
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    },
                };
                let members = vec![remaining_scrutinee.clone(), tag_ty];
                Type::normalize_intersection(members)
            }
        }
        Pattern::Wildcard | Pattern::Pin(..) => remaining_scrutinee.clone(),
        _ => scrutinee_ty.clone(),
    };

    let mut pat_bindings: Vec<(String, Type)> = Vec::new();
    typecheck_narrow::collect_pattern_bindings(
        &arm.pattern.node,
        &arm_scrutinee_ty,
        &mut pat_bindings,
    );
    let arm_env: Arc<RwLock<Env>> = if pat_bindings.is_empty() {
        Arc::clone(env)
    } else {
        let mut child_inner = Env::with_parent(Arc::clone(env));
        for (name, ty) in pat_bindings {
            child_inner.insert(name, ty);
        }
        Arc::new(RwLock::new(child_inner))
    };

    // Guard inference and narrowing (guard is inferred for its type-map side effects only)
    let arm_env = if let Some(guard) = &arm.guard {
        let _guard_ty = {
            let mut local_stack = Vec::new();
            Box::pin(run_typecheck(
                guard,
                &arm_env,
                state,
                errors,
                type_map,
                &mut local_stack,
            ))
            .await
        };
        let guard_narrowings = typecheck_narrow::extract_narrowings(guard);
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
        match &arm.pattern.node {
            Pattern::Constructor { tag, .. } => {
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
            Pattern::Wildcard | Pattern::Pin(..) => Type::Never,
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
    errors: &mut Vec<TypeError>,
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
            .map(|arm| coverage::ast_pattern_to_coverage(&arm.pattern.node, Some(tycon_env_ref)))
            .collect();
        let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();
        let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);

        if !result.exhaustive {
            let witnesses = coverage::format_witnesses(&result.uncovered);
            errors.push(TypeError::new(
                format!("non-exhaustive match: missing coverage for {}", witnesses),
                span.clone(),
            ));
        }
        for &idx in &result.redundant {
            errors.push(TypeError::new(
                "unreachable match arm: this pattern is already covered by prior arms",
                arms[idx].pattern.span.clone(),
            ));
        }
        for &idx in &result.inaccessible {
            errors.push(TypeError::new(
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
            child_inner.insert(name, state.fresh_type_var(&node.span));
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
) -> Option<Vec<TypeError>> {
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
                Some(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        p_expected.len(),
                        p_actual.len()
                    ),
                    span.clone(),
                )])
            } else {
                let mut param_err: Option<Vec<TypeError>> = None;
                for ((_, p_act), (_, p_exp)) in p_actual.iter().zip(p_expected.iter()) {
                    if !Type::is_consistent_subtype(p_act, p_exp) {
                        param_err = Some(vec![TypeError::new(
                            format!(
                                "[TypeError] parameter annotation {} is more restrictive than required type {}",
                                p_act, p_exp
                            ),
                            span.clone(),
                        )]);
                        break;
                    }
                }
                if param_err.is_some() {
                    param_err
                } else if !Type::is_consistent_subtype(r_actual, r_expected) {
                    Some(vec![TypeError::type_mismatch(
                        r_expected,
                        r_actual,
                        span.clone(),
                    )])
                } else {
                    None
                }
            }
        }
        _ => {
            if !Type::is_consistent_subtype(actual, expected) {
                Some(vec![TypeError::type_mismatch(
                    expected,
                    actual,
                    span.clone(),
                )])
            } else {
                None
            }
        }
    }
}

// ===== Inline helper: Field type lookup =====

fn field_type_from_base(
    base_ty: &Type,
    field: &crate::ast::DotKey,
    span: &Span,
    errors: &mut Vec<TypeError>,
) -> Type {
    let key = match field {
        crate::ast::DotKey::Ident(s) => s.clone(),
        crate::ast::DotKey::Int(n) => n.to_string(),
    };

    match base_ty {
        Type::Dict(row) => row.fields.get(&key).cloned().unwrap_or(Type::Unknown),
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
        Type::Unknown | Type::Any | Type::TypeVar(_, _) => Type::Unknown,
        Type::Negation(_) => Type::Unknown,
        Type::Union(_) => Type::Unknown,
        Type::NominalVariant { .. } => Type::Unknown,
        Type::TyCon(_) | Type::App(_, _) => Type::Unknown,
        other => {
            errors.push(TypeError::new(
                format!("expected record type for field access, but got {}", other),
                span.clone(),
            ));
            Type::Unknown
        }
    }
}

// ===== Inline helper: contains_unknown_or_top =====
//
// Delegates to the canonical implementation in typecheck.rs (now pub(crate)).
// Previously a local copy missing the TypeVar arm; using the canonical version
// ensures correctness including TypeVar-as-gradual semantics (Siek & Taha 2006).

fn contains_unknown_or_top_local(ty: &Type) -> bool {
    super::contains_unknown_or_top(ty)
}

// ===== Helper functions (duplicated from typecheck_dict.rs for CEK-internal use) =====

/// Tarjan's algorithm for computing SCCs in topological order.
/// Returns SCCs in reverse topological order (dependencies before dependents).
///
/// Uses an iterative worklist implementation to avoid stack overflow on large dicts.
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
fn collect_dependencies(
    node: &Arc<SurfaceNode>,
    name_to_idx: &HashMap<String, usize>,
) -> Vec<usize> {
    let mut deps: Vec<usize> = Vec::new();
    let mut worklist: Vec<&Arc<SurfaceNode>> = vec![node];

    while let Some(current) = worklist.pop() {
        match &current.expr {
            SurfaceExpression::VarRef { name, .. } => {
                if let Some(&idx) = name_to_idx.get(name.as_str()) {
                    deps.push(idx);
                }
            }
            SurfaceExpression::Int(_)
            | SurfaceExpression::U64(_)
            | SurfaceExpression::Float(_)
            | SurfaceExpression::StringLiteral { .. } => {}
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(ref key) = entry.node.key {
                        worklist.push(key);
                    }
                    worklist.push(&entry.node.value);
                }
            }
            SurfaceExpression::Fn { body, .. } => {
                worklist.push(body);
            }
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } => {
                worklist.push(func);
                for arg in args {
                    worklist.push(arg);
                }
                for named_arg in named_args {
                    worklist.push(&named_arg.node.value);
                }
            }
            SurfaceExpression::Match { scrutinee, arms } => {
                worklist.push(scrutinee);
                for arm in arms {
                    for body_expr in &arm.body {
                        worklist.push(body_expr);
                    }
                    if let Some(ref guard) = arm.guard {
                        worklist.push(guard);
                    }
                }
            }
            SurfaceExpression::Field { expr, .. } => {
                if let Some(target) = expr {
                    worklist.push(target);
                }
            }
            SurfaceExpression::Pipe { lhs, rhs } => {
                worklist.push(lhs);
                worklist.push(rhs);
            }
            SurfaceExpression::Sequential(exprs) => {
                for e in exprs {
                    worklist.push(e);
                }
            }
            SurfaceExpression::TypeAssert { expr, .. } => {
                worklist.push(expr);
            }
            SurfaceExpression::Rest(..) => {}
            SurfaceExpression::Quote(e)
            | SurfaceExpression::Unquote(e)
            | SurfaceExpression::UnquoteSplice(e) => {
                worklist.push(e);
            }
            SurfaceExpression::Decl(_) => {}
            SurfaceExpression::PatternDecl { bindings }
            | SurfaceExpression::LetDecl { bindings } => {
                for b in bindings {
                    worklist.push(b);
                }
            }
            SurfaceExpression::CaseArm {
                let_bindings,
                pattern,
                body,
            } => {
                worklist.push(let_bindings);
                worklist.push(pattern);
                worklist.push(body);
            }
            SurfaceExpression::Placeholder | SurfaceExpression::Error(_) => {}
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
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, t)| type_contains_typevar(t, name))
                || type_contains_typevar(ret, name)
        }
        Type::App(f, arg) => type_contains_typevar(f, name) || type_contains_typevar(arg, name),
        Type::Dict(row) => {
            row.fields.values().any(|t| type_contains_typevar(t, name))
                || match &row.tail {
                    RowTail::Uniform { key: k, value: v } => {
                        k.as_ref().map_or(false, |t| type_contains_typevar(t, name))
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
/// Called by `infer_dict` in `typecheck_dict.rs` (via a delegation shim) and will be used
/// directly by the full dict CEK path once T-1644 implements `AfterDictPassZero` → pass transitions.
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
                    variadic: false,
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
    type_map: &mut Option<&mut TypeMap>,
) -> Option<String> {
    match &entry.key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            SurfaceExpression::Int(n) => Some(n.to_string()),
            _ => {
                let mut errors = Vec::new();
                match Box::pin(run_typecheck(
                    key_node,
                    env,
                    state,
                    &mut errors,
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
