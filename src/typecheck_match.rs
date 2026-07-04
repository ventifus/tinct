//! Case arm and function literal type inference.
//!
//! Extracted from `typecheck.rs` — contains the subsystem that infers types for:
//! - `[case pattern body]` arms (`typecheck_case_arm`): pattern binding, constructor resolution,
//!   typed narrowing via BAS intersection, T018/T019/T020 diagnostics
//! - `[fn ...]` literals (`infer_fn`): parameter annotation, variadic handling, return type
//!   checking (both unification and checking modes)

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use super::{check_surface_expr, infer_surface_expr, TypeMap};
use crate::ast::{Annotation, Param, Pattern, Span, Spanned, SurfaceExpression, SurfaceNode};
use crate::type_errors::{GenericTypeError, TypeErrorTyped};
use crate::types::{instantiate_scheme, unify, Constraint, InferState, Type, TypeEnv, TypeError};

// resolve_annotation and resolve_fn_metadata come from typecheck_annot via the
// `use typecheck_annot::*` glob in typecheck.rs; they are re-exported into super's
// namespace, so we pull them through super here.
use super::{resolve_annotation, resolve_fn_metadata};

/// Elaboration pass: resolve `Pattern::TypeAssertPending → Pattern::TypeAssert` for all
/// patterns in a match arm BEFORE type-checking the arm body.
///
/// This pass must run before `collect_pattern_bindings` so that `TypeAssert { resolved_type }`
/// is available — `TypeAssertPending` carries only the annotation surface form and cannot
/// provide a concrete `Type` for variable binding.
///
/// The function recursively walks all pattern positions:
/// - `TypeAssertPending { annotation, inner }`: calls `resolve_annotation` to produce a
///   concrete `Type`, then rewrites to `TypeAssert { resolved_type, inner }`.
/// - `Or(branches)`: elaborates each branch independently.
/// - `Constructor { tag, binding }`: looks up `tag` in `state.tycon_env`.
///   - Builtin-type TyConDef (`def.builtin_type.is_some()`): rewrites to
///     `TypeAssert { resolved_type: TyCon(tag), inner: binding }` so match uses `value_matches_type`.
///   - Nominal TyConDef (`!def.constructors.is_empty()`): if `tag` is unqualified (no `.`),
///     emits T018 warning "unqualified constructor pattern tag — use qualified form" and still
///     auto-qualifies for backwards compatibility. If already qualified, uses tag as-is.
///   - Not found / empty constructors: leaves pattern UNCHANGED (graceful fallback for T-1003).
/// - `Dict { fields, .. }`: elaborates each field sub-pattern.
/// - `Cons { head, tail }`: elaborates both sub-patterns.
/// - `TypeAssert { inner, .. }`: already resolved — recurse into inner if present.
/// - All other patterns (`Variable`, `Wildcard`, `Literal`, `Pin`): pass through.
///
/// `span` is the source span of the outermost pattern being elaborated, used for diagnostics.
/// For recursive sub-patterns, each Spanned wrapper carries its own span.
/// For `TypeAssertPending`, the annotation's own span is used as the error location.
pub(crate) fn elaborate_pattern<'a>(
    pat: &'a Pattern,
    env: &'a Rc<TypeEnv>,
    state: &'a mut InferState,
    span: &'a Span,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Pattern, Vec<TypeError>>> + 'a>> {
    Box::pin(async move {
        match pat {
            Pattern::TypeAssertPending {
                annotation, inner, ..
            } => {
                // Resolve the annotation to a concrete Type.
                let mut pat_constraints: Vec<Constraint> = Vec::new();
                let resolved_type = resolve_annotation(
                    &annotation.node,
                    env,
                    annotation.span.clone(),
                    state,
                    &mut pat_constraints,
                    &mut None,
                    &mut None,
                    None,
                )
                .await
                .map_err(|e| vec![e])?;

                // Recursively elaborate the inner sub-pattern if present.
                let elaborated_inner = if let Some(ref boxed_spanned) = inner {
                    let elaborated =
                        elaborate_pattern(&boxed_spanned.node, env, state, &boxed_spanned.span)
                            .await?;
                    Some(Box::new(Spanned::new(
                        elaborated,
                        boxed_spanned.span.clone(),
                    )))
                } else {
                    None
                };

                Ok(Pattern::TypeAssert {
                    resolved_type,
                    inner: elaborated_inner,
                })
            }

            Pattern::Or(branches) => {
                let mut elaborated_branches = Vec::with_capacity(branches.len());
                for spanned_pat in branches.iter() {
                    let elaborated =
                        elaborate_pattern(&spanned_pat.node, env, state, &spanned_pat.span).await?;
                    elaborated_branches.push(Spanned::new(elaborated, spanned_pat.span.clone()));
                }
                Ok(Pattern::Or(elaborated_branches))
            }

            Pattern::Constructor { tag, binding } => {
                // Inline helper: elaborate optional payload binding asynchronously.
                let elaborated_binding = if let Some(ref boxed_spanned) = binding {
                    let elaborated =
                        elaborate_pattern(&boxed_spanned.node, env, state, &boxed_spanned.span)
                            .await?;
                    Some(Box::new(Spanned::new(
                        elaborated,
                        boxed_spanned.span.clone(),
                    )))
                } else {
                    None
                };

                let tycon_lookup = state.tycon_env.get(tag.as_str()).cloned();
                if let Some(ref def) = tycon_lookup {
                    if def.builtin_type.is_some() {
                        // Case b: builtin-type TyConDef (e.g., Int:, Str:, Bool: used as patterns).
                        // Rewrite to TypeAssert so the pattern-matching logic can use value_matches_type.
                        return Ok(Pattern::TypeAssert {
                            resolved_type: Type::TyCon(tag.clone()),
                            inner: elaborated_binding,
                        });
                    }
                    if !def.constructors.is_empty() {
                        // Case a: nominal user-defined type.
                        // If the tag is unqualified (no '.'), this is a hard error (T-1109).
                        // Bare constructor pattern tags silently misbehave at runtime because the
                        // evaluator matches against qualified runtime variant tags.
                        if !tag.contains('.') {
                            let qualified = env
                                .resolve_constructor_tag(tag)
                                .unwrap_or_else(|| tag.clone());
                            return Err(vec![crate::type_errors::TypeErrorTyped::Generic(
                            crate::type_errors::GenericTypeError {
                                message: format!(
                                    "unqualified constructor pattern tag '{tag}' — use qualified form '{qualified}'"
                                ),
                                span: span.clone(),
                                notes: vec![], call_stack: vec![],
                            },
                        )]);
                        }
                        return Ok(Pattern::Constructor {
                            tag: tag.clone(),
                            binding: elaborated_binding,
                        });
                    }
                }
                // Case c: not found in TyConEnv, or found with empty constructors and no
                // builtin_type — this is an open type or a non-nominal user type.
                Ok(Pattern::Constructor {
                    tag: tag.clone(),
                    binding: elaborated_binding,
                })
            }

            Pattern::Dict { fields, rest } => {
                let mut elaborated_fields = Vec::with_capacity(fields.len());
                for (key, spanned_pat) in fields.iter() {
                    let elaborated =
                        elaborate_pattern(&spanned_pat.node, env, state, &spanned_pat.span).await?;
                    elaborated_fields.push((
                        key.clone(),
                        Spanned::new(elaborated, spanned_pat.span.clone()),
                    ));
                }
                Ok(Pattern::Dict {
                    fields: elaborated_fields,
                    rest: *rest,
                })
            }

            Pattern::TypeAssert {
                resolved_type,
                inner,
            } => {
                // Already elaborated — recurse into inner sub-pattern if present.
                let elaborated_inner = if let Some(ref boxed_spanned) = inner {
                    let elaborated =
                        elaborate_pattern(&boxed_spanned.node, env, state, &boxed_spanned.span)
                            .await?;
                    Some(Box::new(Spanned::new(
                        elaborated,
                        boxed_spanned.span.clone(),
                    )))
                } else {
                    None
                };
                Ok(Pattern::TypeAssert {
                    resolved_type: resolved_type.clone(),
                    inner: elaborated_inner,
                })
            }

            // Pass-through patterns: Wildcard, Literal, Pin carry no
            // sub-patterns and require no annotation resolution.
            Pattern::Wildcard | Pattern::Literal(_) | Pattern::Pin(..) => Ok(pat.clone()),

            // T-1140: Predicate patterns — pass through unchanged.
            // The contained SurfaceNode is not type-checked here; it will be evaluated at runtime.
            // Return type is checked to be Bool/Unknown only by convention, not enforced here.
            // Predicate patterns do not affect exhaustiveness (treated as wildcard).
            Pattern::Predicate(_) => Ok(pat.clone()),
        }
    }) // end Box::pin(async move {
}

/// Type-check a case arm: pattern + body.
///
/// - If pattern is `Expr::LetDecl { bindings }`: extract bindings, narrow scrutinee type,
///   introduce bindings into scope for body
/// - If pattern is an expression: type-check the pattern expression (exact-value match)
/// - Type-check body with the extended environment
/// - Return the body type
///
/// For simplified implementation:
/// - Intersection with scrutinee type: if annotation present, use annotation; else use scrutinee
/// - Structural test patterns (name: Constructor) are recognized but not fully implemented yet
pub(crate) async fn typecheck_case_arm(
    pattern: &Arc<SurfaceNode>,
    body: &Arc<SurfaceNode>,
    scrutinee_ty: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    match &pattern.expr {
        SurfaceExpression::LetDecl { bindings } => {
            // Process each binding element against the scrutinee type.
            // For now, simplified: extract binding names and types, extend env, infer body.
            let mut arm_env = TypeEnv::with_parent(env);

            for binding in bindings {
                match &binding.expr {
                    // Wildcard: _ (check first to avoid binding "_" as a variable)
                    SurfaceExpression::VarRef { name, annotation: None, .. } if name == "_" => {
                        // Wildcard - no binding introduced
                    }

                    // Plain binding: name (no annotation)
                    SurfaceExpression::VarRef { name, annotation: None, .. } => {
                        // Bind name to scrutinee type
                        arm_env.insert(name.clone(), scrutinee_ty.clone());
                    }

                    // Annotated binding — either typed or structural test:
                    //
                    // Case A: `name@Type` (typed binding) — produced by `@` syntax.
                    //   annotation = Simple(TypeName) or PropertyDict or Annotated
                    //   Introduces `name : scrutinee_ty ∩ ann_ty` (BAS intersection narrowing).
                    //
                    // Case B: `name: Constructor` (structural test) — produced by `:` syntax in [let ...].
                    //   annotation = PropertyDict([_constructor: "ConstructorName"])
                    //   Looks up Constructor in TypeEnv to determine payload type.
                    //   Binds `name : payload_type(Constructor)` (Unknown if lookup fails).
                    //   The constructor tag check is a runtime concern (eval soft-skip).
                    //
                    // Disambiguation: PropertyDict with "_constructor" sentinel key = structural test.
                    // All other annotation forms = typed binding.
                    // Annotated VarRef: annotation is now on VarRef directly.
                    SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
                        let name = name.as_str();
                        // Check if this is a structural test: PropertyDict with "_constructor" sentinel
                        let constructor_name_opt = annotation
                            .node
                            .get_property("_constructor")
                            .and_then(|v| match &v.expr {
                                SurfaceExpression::Str(s) => Some(s.clone()),
                                _ => None,
                            });
                        let is_structural_test = constructor_name_opt.is_some();

                        if is_structural_test {
                            // Structural test: `name: Constructor`
                            // Look up Constructor in TypeEnv. Constructor functions are registered
                            // as Type::Function { params: [(None, payload_ty)], ret: ... } when
                            // the type system has full constructor type information.
                            //
                            // ADT constructors from `[type ...]` declarations are lowered by lower.rs
                            // as `CoreExpr::Variant { tag: "TypeName.CtorName", .. }` expressions.
                            // Their TypeEnv entries may be Type::Any when constructor type information
                            // is unavailable. In that case, fall back to Type::Unknown as the payload
                            // type (sound under gradual typing).
                            // SAFETY: is_structural_test is only true when constructor_name_opt is Some.
                            let constructor_name = constructor_name_opt.unwrap();

                            // Look up the constructor in the type environment
                            let payload_ty = if let Some(scheme) = env.get(&constructor_name) {
                                // Instantiate the scheme at the current level to get fresh type vars
                                let ctor_ty = instantiate_scheme(
                                    scheme,
                                    state.level,
                                    state,
                                    constraints,
                                    Some(&constructor_name),
                                    Some(binding.span.clone()),
                                );
                                // If the constructor is a single-param function, extract the param type
                                match ctor_ty {
                                    Type::Function { mut params, .. } if params.len() == 1 => {
                                        params.remove(0).1 // payload type is the single param's type
                                    }
                                    Type::Function { params, .. } if params.is_empty() => {
                                        // Nullary constructor — no payload; binding a name is a
                                        // type error per unified-bindings.md (§Constructor Structural Tests):
                                        // a nullary constructor carries no value to bind. The runtime
                                        // soft-skips this arm (the tag check passes but payload
                                        // extraction finds nothing). Emit T019 to guide the user.
                                        if name != "_" {
                                            state.diagnostics.push(crate::error::TypeDiagnostic {
                                                message: format!(
                                                    "nullary constructor `{constructor_name}` has no payload; \
                                                     `{name}` cannot be bound — use `[let _: {constructor_name}]` \
                                                     to match without binding"
                                                ),
                                                span: binding.span.clone(),
                                                code: super::typecheck_diag::T019_MATCH_GUARD_FAILURE,
                                                level: crate::error::DiagnosticLevel::Warn,
                                            });
                                        }
                                        Type::Unknown
                                    }
                                    _ => {
                                        // Constructor type is Top, Unknown, or some other form —
                                        // fall back to Unknown payload (gradual typing escape hatch).
                                        Type::Unknown
                                    }
                                }
                            } else {
                                // Constructor not in scope — emit a T018 warning so the user
                                // learns about the typo/missing definition. The runtime will also
                                // soft-skip this arm (the tag will never match), so the program
                                // is safe to evaluate. Payload type falls back to Unknown.
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: format!(
                                        "undefined constructor `{constructor_name}` in structural test; \
                                         no variable with this name is in scope — the arm will never match"
                                    ),
                                    span: binding.span.clone(),
                                    code: super::typecheck_diag::T018_MATCH_PATTERN_MISMATCH,
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                                Type::Unknown
                            };

                            // Future work — intersection dead-arm warning (new T-code TBD):
                            // When `name@AnnotationType: Constructor` is supported by the parser
                            // (requires extending the Colon handler to also handle Annotated nodes
                            // as LHS, not just VarRef), add a dead-arm check here:
                            //   if payload_ty ∩ annotation_type == Never {
                            //       emit warning: "this arm can never match: Constructor payload
                            //                      type is incompatible with annotation"
                            //   }
                            // This requires:
                            //   1. Parser support for `name@Type: Constructor` (Annotated as LHS)
                            //   2. normalize_intersection returning Type::Never for disjoint types
                            //   3. A Type::Never variant (intersections of disjoint types currently
                            //      return Top or Unknown rather than Never)
                            // The runtime is unaffected (it only checks the constructor tag);
                            // this warning is purely a static dead-code diagnostic.

                            if name != "_" {
                                arm_env.insert(name.to_string(), payload_ty);
                            }
                        } else {
                            // Typed binding: `name@Type`
                            // This implements the BAS intersection narrowing rule from unified-bindings.md:
                            // [let n@T] binds n with type scrutinee_ty ∩ T.
                            // Unknown is the identity in intersection (AGT lifting), so when scrutinee_ty
                            // is Unknown, the intersection reduces to ann_ty (via normalize_intersection).
                            let ann_ty = resolve_annotation(
                                &annotation.node,
                                env,
                                annotation.span.clone(),
                                state,
                                constraints,
                                &mut None,
                                &mut None,
                                None,
                            )
                            .await
                            .map_err(|e| vec![e])?;

                            // T020: Dead-arm warning — check if pattern type is disjoint from scrutinee type.
                            // If types_are_disjoint(scrutinee_ty, ann_ty) is true, the arm can never match
                            // at runtime because the scrutinee will never have a value of the pattern type.
                            if Type::types_are_disjoint(scrutinee_ty, &ann_ty) {
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: format!(
                                        "dead match arm — pattern type `{}` is disjoint from scrutinee type `{}`",
                                        ann_ty, scrutinee_ty
                                    ),
                                    span: binding.span.clone(),
                                    code: super::typecheck_diag::T020_MATCH_EXHAUSTIVENESS,
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                            }

                            // Narrow: scrutinee_ty ∩ ann_ty (BAS type narrowing).
                            // normalize_intersection handles Unknown-as-identity and Top-as-identity.
                            let narrowed_ty =
                                Type::normalize_intersection(vec![scrutinee_ty.clone(), ann_ty]);
                            arm_env.insert(name.to_string(), narrowed_ty);
                        }
                    }

                    // Nested LetDecl for multi-payload destructuring: [a b] in [let [a b]: Constructor]
                    // Parser now correctly produces a nested LetDecl for `[a b]` inside [let ...].
                    // Look up constructor field types from TypeEnv if available; if the constructor
                    // is in scope and has a Function type with multiple params, extract field types
                    // from the param list. Unannotated bindings fall back to Unknown if no constructor
                    // type info is available (gradual typing).
                    SurfaceExpression::LetDecl {
                        bindings: nested_bindings,
                    } => {
                        // Multi-payload destructuring: [a b] in [let [a b]: Constructor]
                        // Field type lookup deferred (B-275 tracking this improvement).
                        let field_types: Vec<Type> = Vec::new();

                        for (idx, nested) in nested_bindings.iter().enumerate() {
                            match &nested.expr {
                                // Annotated VarRef (annotation is now on VarRef directly).
                                SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
                                    let ann_ty = resolve_annotation(
                                        &annotation.node,
                                        env,
                                        annotation.span.clone(),
                                        state,
                                        constraints,
                                        &mut None,
                                        &mut None,
                                        None,
                                    )
                                    .await
                                    .map_err(|e| vec![e])?;
                                    arm_env.insert(name.clone(), ann_ty);
                                }
                                SurfaceExpression::VarRef { name, annotation: None, .. } if name != "_" => {
                                    // Use constructor field type if available, otherwise Unknown
                                    let field_ty =
                                        field_types.get(idx).cloned().unwrap_or(Type::Unknown);
                                    arm_env.insert(name.clone(), field_ty);
                                }
                                _ => {
                                    // Wildcard or other — no binding
                                }
                            }
                        }
                    }

                    _ => {
                        // Other binding forms not yet supported
                        return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                            message: "unsupported binding pattern in case arm".to_string(),
                            span: binding.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })]);
                    }
                }
            }

            // Type-check body with extended environment (body is already Arc<SurfaceNode>)
            let arm_env = Rc::new(arm_env);
            infer_surface_expr(body, &arm_env, state, constraints, type_map).await
        }

        _ => {
            // Exact-value match: infer pattern expression type, then infer body.
            // Both pattern and body are already Arc<SurfaceNode> — no conversion needed.
            let pattern_ty = infer_surface_expr(pattern, env, state, constraints, type_map).await?;

            // T020: Dead-arm warning — check if pattern type is disjoint from scrutinee type.
            // If types_are_disjoint(scrutinee_ty, pattern_ty) is true, the arm can never match
            // at runtime because the scrutinee will never have a value of the pattern type.
            if Type::types_are_disjoint(scrutinee_ty, &pattern_ty) {
                state.diagnostics.push(crate::error::TypeDiagnostic {
                    message: format!(
                        "dead match arm — pattern type `{}` is disjoint from scrutinee type `{}`",
                        pattern_ty, scrutinee_ty
                    ),
                    span: pattern.span.clone(),
                    code: super::typecheck_diag::T020_MATCH_EXHAUSTIVENESS,
                    level: crate::error::DiagnosticLevel::Warn,
                });
            }

            // Check that pattern is scalar or nullary (design doc requirement)
            // For now, just issue a warning if it's not - don't block
            match &pattern_ty {
                Type::Int
                | Type::IntLiteral(_)
                | Type::Float
                | Type::Str
                | Type::StringLiteral(_) => {
                    // Valid scalar type - OK
                }
                _ => {
                    // Non-scalar - could be nullary constructor, or could be error
                    // For now, allow it (conservative)
                }
            }

            // Body is checked in the enclosing environment (no new bindings from exact-value match)
            infer_surface_expr(body, env, state, constraints, type_map).await
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn infer_fn(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<Param>],
    body: &Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    _span: Span,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Create a fresh annotation mapping for this function to prevent
    // cross-contamination of type variables.
    // Only allocate if any param has an annotation or there's a return annotation.
    // This guard is a performance optimization only: if there are no annotations,
    // resolve_annotation is never called (it receives Type::Unknown directly), so an empty
    // HashMap would never be consulted. Skipping allocation has no behavior impact.
    let has_annotations =
        params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some();
    let mut ann_mapping = if has_annotations {
        Some(HashMap::new())
    } else {
        None
    };
    let mut ann_mapping_opt = ann_mapping.as_mut();
    // row_ann_mapping tracks named row variables (e.g., ...r in [a: Int ...r]) per function scope.
    // It is separate from ann_mapping (which tracks type-kind variables) to enforce kinded
    // substitution: a name used as a row variable cannot also be used as a type variable.
    let mut row_ann_mapping = if has_annotations {
        Some(HashMap::new())
    } else {
        None
    };
    let mut row_ann_mapping_opt = row_ann_mapping.as_mut();

    let mut param_types: Vec<(Option<String>, Type)> = Vec::with_capacity(params.len());
    for p in params.iter() {
        let ty = match &p.node.annotation {
            Some(ann) if crate::ast::is_expr_annotation(&ann.node) => {
                // @Expr params receive quoted AST at runtime. Resolve to the declared Expr
                // sum type (Expr: [type [Literal ...] [VarRef ...] [Call ...] ...] in prelude)
                // by looking up "Expr" as a simple named type. This avoids the bug where
                // PropertyDict{type: VarRef("Expr")} is misinterpreted as a structural record
                // type [type: <Expr-as-function>] instead of the Expr sum type.
                let expr_ann = crate::ast::Annotation::Simple("Expr".into());
                resolve_annotation(
                    &expr_ann,
                    env,
                    ann.span.clone(),
                    state,
                    constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await
                .map_err(|e| vec![e])?
            }
            Some(ann) => resolve_annotation(
                &ann.node,
                env,
                ann.span.clone(),
                state,
                constraints,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
                None,
            )
            .await
            .map_err(|e| vec![e])?,
            // Unannotated params use Unknown (gradual typing escape hatch).
            //
            // WHY NOT fresh_type_var(): Using fresh TypeVars for unannotated params
            // causes O(N²) blowup in the prelude type-checking. Each unannotated param
            // becomes a TypeVar that unifies with constrained TypeVars from + / builtin-add
            // etc. (∀a[Numeric]. Fn(a a → a)), creating TypeVar→TypeVar chains in
            // state.subst. With ~170 prelude functions each having 2-3 unannotated params,
            // state.subst grows to hundreds of entries. The substitution merge loop in
            // infer_dict (typecheck_dict.rs:380-406) is O(|state.subst|²) in practice,
            // making prelude type-checking take 120+ seconds.
            //
            // FUTURE WORK: To enable TypeVars here, fix the merge loop to be O(N) instead
            // of O(N²) — e.g., by not calling subst.apply() for each entry, or by using
            // Gradual: unannotated parameter gets Unknown type.
            // TODO: once union-find substitution lands (doc/whatif/union-find-substitution.md),
            // restore: None => Ok(state.fresh_type_var()) and update test_fn_unannotated.
            None => Type::Unknown,
        };
        param_types.push((Some(p.node.name.clone()), ty));
    }

    let mut fn_env = TypeEnv::with_parent(env);
    for (i, param) in params.iter().enumerate() {
        if param.node.variadic {
            let elem_ty = state.fresh_type_var();
            let variadic_ty = Type::Record(crate::type_def::Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Uniform { key: None, value: Box::new(elem_ty) },
            });
            param_types[i].1 = variadic_ty.clone();
            fn_env.insert(param.node.name.clone(), variadic_ty);
        } else {
            fn_env.insert(param.node.name.clone(), param_types[i].1.clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            // Check if this is a metadata dict annotation: @[return: Type doc: "..." constraint: ...]
            let actual_ann = match &ann.node {
                Annotation::PropertyDict(surface_entries) => {
                    // Dispatch based on whether the PropertyDict contains function metadata keys.
                    // Function metadata dict: @[return: Type doc: "..." constraint: ...]
                    //   → call resolve_fn_metadata which extracts return:, constraint:, doc:, bind:, kinds:
                    // Pure positional/structural annotation: @[Int Null] (union type), @[x: Int] (record type)
                    //   → call resolve_annotation which delegates to resolve_type_dict
                    // Check for function metadata keys directly on SurfaceEntries (no bridge needed for this check)
                    let has_fn_key = surface_entries.iter().any(|e| {
                        e.node.key.as_ref().is_some_and(|k| {
                            matches!(&k.expr, SurfaceExpression::Str(s) if crate::ast::STANDARD_ANN_KEYS.contains(&s.as_str()))
                        })
                    });
                    // Check if all entries are keyed (no positional entries)
                    let all_keyed = surface_entries.iter().all(|e| e.node.key.is_some());

                    if has_fn_key || all_keyed {
                        // B-355: all-keyed dicts (including custom-only keys) are fn metadata.
                        if !all_keyed {
                            return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                                message: "fn annotation must use either named keys (return:, constraint:, doc:, bind:, kinds:) or positional entries (union return type), not both".to_string(),
                                span: ann.span.clone(),
                                notes: vec![], call_stack: vec![],
                            })]);
                        }
                        // Function metadata dict: extract return type from return: key.
                        let (ret, _doc) = resolve_fn_metadata(
                            surface_entries,
                            env,
                            ann.span.clone(),
                            state,
                            constraints,
                            &mut ann_mapping_opt,
                            &mut row_ann_mapping_opt,
                            None,
                        )
                        .await
                        .map_err(|e| vec![e])?;
                        ret
                    } else {
                        // Structural/union type dict: @[Int Null], @[x: Type], etc.
                        // Delegate to resolve_annotation which calls resolve_type_dict.
                        resolve_annotation(
                            &ann.node,
                            env,
                            ann.span.clone(),
                            state,
                            constraints,
                            &mut ann_mapping_opt,
                            &mut row_ann_mapping_opt,
                            None,
                        )
                        .await
                        .map_err(|e| vec![e])?
                    }
                }
                _ => {
                    // Simple annotation - resolve normally
                    resolve_annotation(
                        &ann.node,
                        env,
                        ann.span.clone(),
                        state,
                        constraints,
                        &mut ann_mapping_opt,
                        &mut row_ann_mapping_opt,
                        None,
                    )
                    .await
                    .map_err(|e| vec![e])?
                }
            };

            // Set expected_return for inferred [do] macro support.
            // Save the old value to restore after body inference (for nested fn defs).
            let prev_expected_return = state.expected_return.take();
            state.expected_return = Some(actual_ann.clone());

            // When declared return type contains type variables, switch to unification mode
            // (doc/06 §[CHECK-FN], Damas & Milner 1982, Pierce & Turner 2000 §3.2).
            // is_subtype returns true for ANY TypeVar unconditionally (conservative approximation),
            // so it would silently accept without binding the TypeVar. We must use unification
            // mode to actually BIND the TypeVars via constraint solving. See is_subtype_bas
            // docstring and B-446 for the full explanation.
            let result = if actual_ann.has_inference_vars() {
                let body_ty =
                    infer_surface_expr(body, &fn_env, state, constraints, type_map).await?;
                let result = Box::pin(unify(
                    &body_ty,
                    &actual_ann,
                    state,
                    constraints,
                    body.span.clone(),
                ))
                .await;
                result.map_err(|e| vec![e])?;
                // Apply substitution to resolve any TypeVars bound during unification.
                // Without this, the returned Type::Function would have has_inference_vars() == true,
                // causing check_call to enter the CALL-POLY path unnecessarily (see check_call's
                // has_inference_vars guard). This prevents call sites from entering CALL-POLY.
                state.apply(&actual_ann)
            } else {
                // Use checking mode for concrete return types (no type variables)
                check_surface_expr(body, &actual_ann, &fn_env, state, constraints, type_map)
                    .await?;
                actual_ann
            };

            // Restore previous expected_return
            state.expected_return = prev_expected_return;
            result
        }
        None => infer_surface_expr(body, &fn_env, state, constraints, type_map).await?,
    };

    // Check if any parameter is variadic
    let has_variadic = params.iter().any(|p| p.node.variadic);

    // Compute required_count: params that have no `default:` annotation are required.
    // A param with `@[type: T  default: expr]` annotation is optional — callers may omit it.
    // B-349: this is the fix for spurious arity-mismatch errors on calls that omit optional params.
    let required_count = params
        .iter()
        .filter(|p| {
            p.node
                .annotation
                .as_ref()
                .and_then(|ann| ann.node.get_property("default"))
                .is_none()
        })
        .count();

    Ok(Type::Function {
        params: param_types,
        ret: Box::new(ret_type),
        variadic: has_variadic,
        required_count,
    })
}
