//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `(SurfaceNode, ResolutionTable, TypeAnnotationTable)`.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `FreeVar` (unresolvable)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (with resolved_type from TypeAnnotationTable or Type::Unknown)
//! - `TypeAssertPending` in patterns → `TypeAssert` (using TypeAnnotationTable.pattern_types)
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::Arc;

use crate::ast::{
    node_id, CoreEntry, CoreExpr, CoreMatchArm, CoreNamedArg, CoreParam, Pattern, ResolutionTable,
    Spanned, SurfaceEntry, SurfaceExpression, SurfaceNode, TypeAnnotationTable,
};
use crate::type_def::Type;

/// Lower a single surface node to a CoreExpr.
///
/// This is the entry point for per-thunk lowering. Called from `eval_materialize.rs`
/// when a `UnevaluatedState::Surface` thunk is first forced.
///
/// Lowering errors (malformed AST, impossible variant combinations) propagate as
/// `CoreExpr::Error` — consistent with tinct's lazy semantics where errors surface
/// at force time, not at construction time.
pub fn lower(
    arc: &Arc<SurfaceNode>,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> Spanned<CoreExpr> {
    let span = arc.span.clone();
    let core_expr = lower_expr(arc, &arc.expr, res, types);
    Spanned::new(core_expr, span)
}

/// Lower a `Pattern`, converting `TypeAssertPending → TypeAssert` using the type annotation
/// table populated by the type checker's elaboration pass (B-338).
///
/// Recursively walks all sub-patterns so nested `TypeAssertPending` nodes (e.g., inside
/// `Or`, `Dict`, `Seq`, or `Constructor` bindings) are also converted.
///
/// For `TypeAssertPending`:
/// - Looks up `annotation.span` in `types.pattern_types` (populated by `record_pattern_elaborations`)
/// - If found: produces `Pattern::TypeAssert { resolved_type, inner: lower(inner) }`
/// - If not found (type checking was skipped, or macro-synthesized pattern): leaves as
///   `TypeAssertPending` so the runtime fallback in `eval.rs` is still invoked for the
///   simple-name cases it handles.
///
/// For all other pattern variants: returns a structurally identical pattern with any
/// sub-patterns recursively lowered.
fn lower_pattern(pat: &Pattern, types: &TypeAnnotationTable) -> Pattern {
    match pat {
        Pattern::TypeAssertPending { annotation, inner } => {
            // Look up the resolved type by annotation span.
            match types.get_pattern(&annotation.span) {
                Some(resolved_type) => {
                    // Elaborate inner sub-pattern recursively.
                    let elaborated_inner = inner.as_ref().map(|boxed| {
                        Box::new(Spanned::new(
                            lower_pattern(&boxed.node, types),
                            boxed.span.clone(),
                        ))
                    });
                    Pattern::TypeAssert {
                        resolved_type: resolved_type.clone(),
                        inner: elaborated_inner,
                    }
                }
                None => {
                    // Type checking was skipped or this is a macro-synthesized pattern.
                    // Keep as TypeAssertPending; the runtime fallback handles Simple names.
                    let lowered_inner = inner.as_ref().map(|boxed| {
                        Box::new(Spanned::new(
                            lower_pattern(&boxed.node, types),
                            boxed.span.clone(),
                        ))
                    });
                    Pattern::TypeAssertPending {
                        annotation: annotation.clone(),
                        inner: lowered_inner,
                    }
                }
            }
        }

        Pattern::TypeAssert {
            resolved_type,
            inner,
        } => {
            // Already elaborated — recurse into inner.
            let elaborated_inner = inner.as_ref().map(|boxed| {
                Box::new(Spanned::new(
                    lower_pattern(&boxed.node, types),
                    boxed.span.clone(),
                ))
            });
            Pattern::TypeAssert {
                resolved_type: resolved_type.clone(),
                inner: elaborated_inner,
            }
        }

        Pattern::Or(branches) => Pattern::Or(
            branches
                .iter()
                .map(|b| Spanned::new(lower_pattern(&b.node, types), b.span.clone()))
                .collect(),
        ),

        Pattern::Constructor { tag, binding } => Pattern::Constructor {
            tag: tag.clone(),
            binding: binding
                .as_ref()
                .map(|b| Box::new(Spanned::new(lower_pattern(&b.node, types), b.span.clone()))),
        },

        Pattern::Dict { fields, rest } => Pattern::Dict {
            fields: fields
                .iter()
                .map(|(k, s)| {
                    (
                        k.clone(),
                        Spanned::new(lower_pattern(&s.node, types), s.span.clone()),
                    )
                })
                .collect(),
            rest: *rest,
        },

        Pattern::Seq { head, tail } => Pattern::Seq {
            head: Box::new(Spanned::new(
                lower_pattern(&head.node, types),
                head.span.clone(),
            )),
            tail: Box::new(Spanned::new(
                lower_pattern(&tail.node, types),
                tail.span.clone(),
            )),
        },

        // Leaf patterns: no sub-patterns to lower.
        Pattern::Wildcard | Pattern::Literal(_) | Pattern::Pin(_) | Pattern::TypeTag(_) => {
            pat.clone()
        }

        // T-1140: Predicate patterns carry a SurfaceNode — passed through unchanged.
        // The SurfaceNode is lowered on demand inside MatchDispatch at eval time,
        // using empty resolution/type tables (same as other surface-in-core eval sites).
        Pattern::Predicate(_) => pat.clone(),
    }
}

fn lower_expr(
    arc: &Arc<SurfaceNode>,
    expr: &SurfaceExpression,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int(*n),
        SurfaceExpression::U64(n) => CoreExpr::U64(*n),
        SurfaceExpression::Float(n) => CoreExpr::Float(*n),
        SurfaceExpression::Bool(b) => CoreExpr::Bool(*b),
        SurfaceExpression::Str(s) => CoreExpr::Str(s.clone()),

        SurfaceExpression::VarRef { name, .. } => {
            let id = node_id(arc);
            match res.get(&id) {
                Some(&(level, slot)) => CoreExpr::Var {
                    name: name.clone(),
                    level,
                    slot,
                },
                None => CoreExpr::FreeVar(name.clone()),
            }
        }

        SurfaceExpression::DotAccess { expr: inner, field } => CoreExpr::DotAccess {
            expr: Arc::new(lower(inner, res, types)),
            field: field.clone(),
        },

        // Pipe is syntactic sugar — rewrite to Call(rhs, [lhs]) so the evaluator
        // sees only Call nodes. Equivalent to: f |> g  ==  g(f).
        SurfaceExpression::Pipe { lhs, rhs } => CoreExpr::Call {
            func: Arc::new(lower(rhs, res, types)),
            args: vec![Arc::new(lower(lhs, res, types))],
            named_args: vec![],
            implied: true,
        },

        SurfaceExpression::Sequential(exprs) => CoreExpr::Sequential(
            exprs
                .iter()
                .map(|e| Arc::new(lower(e, res, types)))
                .collect(),
        ),

        SurfaceExpression::Dict(entries) => {
            let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::with_capacity(entries.len());
            for se in entries {
                // Most Decl forms (TypeAlias, ClassDecl, MacroDecl) are processed at type-check time.
                // At runtime:
                //
                // EXCEPTION 1 (B-353): InstanceDecl with non-empty arms produces a method dict.
                // desugar_instance_decls_surface_program is NOT called in lib.rs eval path so
                // the type checker's Pass 0c sees InstanceDecl entries for instance registration.
                // lower.rs handles the eval-side transformation here.
                //
                // EXCEPTION 2 (T-1193): TypeAlias produces a constructor dict at runtime.
                // `Color: [type Red Green Blue]` → `Color` is a dict `{Red: Variant("Color.Red"), ...}`
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    match decl.as_ref() {
                        crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. }
                            if !arms.is_empty() =>
                        {
                            // InstanceDecl: pass through to lower() which handles it
                            let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                            let value = Arc::new(lower(&se.node.value, res, types));
                            core_entries
                                .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                        }
                        crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                            // T-1193: TypeAlias produces a constructor dict at runtime.
                            // Extract type name from the dict entry key for qualified tags.
                            let type_name_opt = extract_type_name_from_key(&se.node.key);
                            let ctor_dict = lower_type_alias_to_constructor_dict(
                                type_name_opt,
                                body,
                                res,
                                types,
                            );
                            let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                            let value = Arc::new(Spanned::new(ctor_dict, se.span.clone()));
                            core_entries
                                .push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                        }
                        _ => {
                            // Other Decl forms (ClassDecl, MacroDecl, SyntaxClass) are skipped.
                            continue;
                        }
                    }
                } else {
                    // Non-Decl entries: lower normally
                    let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                    let value = Arc::new(lower(&se.node.value, res, types));
                    core_entries.push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
                }
            }
            CoreExpr::Dict(core_entries)
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => CoreExpr::Call {
            func: Arc::new(lower(func, res, types)),
            args: args
                .iter()
                .map(|a| Arc::new(lower(a, res, types)))
                .collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    Spanned::new(
                        CoreNamedArg {
                            name: na.node.name.clone(),
                            value: Arc::new(lower(&na.node.value, res, types)),
                        },
                        na.span.clone(),
                    )
                })
                .collect(),
            implied: *implied,
        },

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => CoreExpr::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    Spanned::new(
                        CoreParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: Arc::new(lower(body, res, types)),
            desugared: *desugared,
        },

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            let id = node_id(arc);
            match types.get(&id) {
                Some(Type::Error) | None => {
                    // Type::Error: failed_bindings entry — emit TypeAssert with Unknown.
                    // None: Macro-synthesized node — bypassed typechecking.
                    // Unknown passes via consistent subtyping (always compatible), so the
                    // annotation check is a no-op. The real error surfaces as the
                    // undefined_variable error at the failed-binding use-site.
                    CoreExpr::TypeAssert {
                        annotation: annotation.clone(),
                        expr: Arc::new(lower(inner, res, types)),
                        resolved_type: Type::Unknown,
                        pipeline_blame: None,
                    }
                }
                Some(ty) => CoreExpr::TypeAssert {
                    annotation: annotation.clone(),
                    expr: Arc::new(lower(inner, res, types)),
                    resolved_type: ty.clone(),
                    pipeline_blame: None,
                },
            }
        }

        SurfaceExpression::Annotated { name, annotation } => CoreExpr::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },

        SurfaceExpression::Rest(name) => CoreExpr::Rest(name.clone()),

        SurfaceExpression::Match { scrutinee, arms } => CoreExpr::Match {
            scrutinee: Arc::new(lower(scrutinee, res, types)),
            arms: arms
                .iter()
                .map(|arm| CoreMatchArm {
                    // B-338: lower_pattern converts TypeAssertPending → TypeAssert using
                    // the TypeAnnotationTable.pattern_types map populated by the type checker.
                    // This replaces the fragile runtime name-mapping fallback in eval.rs.
                    pattern: Spanned::new(
                        lower_pattern(&arm.pattern.node, types),
                        arm.pattern.span.clone(),
                    ),
                    guard: arm.guard.as_ref().map(|g| Arc::new(lower(g, res, types))),
                    body: Arc::new(lower(&arm.body, res, types)),
                })
                .collect(),
        },

        SurfaceExpression::Quote(inner) => CoreExpr::Quote(Arc::new(lower(inner, res, types))),

        SurfaceExpression::Unquote(inner) => CoreExpr::Unquote(Arc::new(lower(inner, res, types))),

        SurfaceExpression::UnquoteSplice(inner) => {
            CoreExpr::UnquoteSplice(Arc::new(lower(inner, res, types)))
        }

        SurfaceExpression::PatternDecl { bindings } => CoreExpr::PatternDecl {
            bindings: bindings.iter().map(|b| lower(b, res, types)).collect(),
        },

        SurfaceExpression::LetDecl { bindings } => CoreExpr::LetDecl {
            bindings: bindings.iter().map(|b| lower(b, res, types)).collect(),
        },

        SurfaceExpression::CaseArm {
            let_bindings,
            pattern,
            body,
        } => CoreExpr::CaseArm {
            let_bindings: let_bindings
                .as_ref()
                .map(|lb| Arc::new(lower(lb, res, types))),
            pattern: Arc::new(lower(pattern, res, types)),
            body: Arc::new(lower(body, res, types)),
        },

        SurfaceExpression::Placeholder => CoreExpr::Placeholder,

        // Declaration forms embedded in expression position (e.g., dict entry values).
        // Most Decl forms produce Placeholder (an error when forced); the type checker
        // registers them via Pass 0c before evaluation occurs.
        //
        // EXCEPTION 1 (B-353): InstanceDecl with non-empty arms produces a method dict at runtime.
        // desugar_instance_decls_surface_program is NOT called before lower.rs in lib.rs so
        // the type checker sees InstanceDecl. lower.rs handles the runtime transformation here.
        //
        // EXCEPTION 2 (T-1193): TypeAlias produces a constructor dict when accessed directly
        // (not via a dict entry). Dict entries are handled in the Dict arm above.
        SurfaceExpression::Decl(decl) => {
            match decl.as_ref() {
                crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } => {
                    if !arms.is_empty() {
                        let method_entries = &arms[0].1;
                        let core_entries: Vec<Spanned<CoreEntry>> = method_entries
                            .iter()
                            .map(|me| {
                                let key =
                                    me.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                                let value = Arc::new(lower(&me.node.value, res, types));
                                Spanned::new(CoreEntry { key, value }, me.span.clone())
                            })
                            .collect();
                        return CoreExpr::Dict(core_entries);
                    }
                    CoreExpr::Placeholder
                }
                crate::ast::SurfaceDeclaration::TypeAlias { body, .. } => {
                    // T-1193: TypeAlias accessed directly (not via dict entry).
                    // No type name available, use unqualified tags.
                    lower_type_alias_to_constructor_dict(None, body, res, types)
                }
                _ => CoreExpr::Placeholder,
            }
        }

        SurfaceExpression::Error(span) => CoreExpr::Error(span.clone()),
    }
}

/// Convert a `Spanned<CoreExpr>` back to an `Arc<SurfaceNode>` for quote/unquote evaluation.
///
/// Bridges through `Expr` via `core_expr_to_expr` + `expr_to_surface_node`.
/// Used by the `CoreExpr::Quote` arm to get a `SurfaceNode` for `eval_quote_walk`.
///
/// The inner CoreExpr is converted back to SurfaceNode for eval_quote_walk.
/// This round-trip is necessary: Quote's inner expression is lowered (so unquote
/// expressions within it get proper variable slot resolution), but at eval time
/// the structural view is needed. CoreExpr::Var preserves the original name alongside
/// the slot, so the round-trip is lossless for variable names.
pub fn core_expr_to_surface_node(
    expr: &crate::ast::Spanned<crate::ast::CoreExpr>,
) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode {
        expr: core_expr_to_surface_expr(&expr.node),
        span: expr.span.clone(),
    })
}

fn core_expr_to_surface_expr(core: &crate::ast::CoreExpr) -> SurfaceExpression {
    use crate::ast::{CoreExpr, SurfaceMatchArm};
    match core {
        CoreExpr::Int(n) => SurfaceExpression::Int(*n),
        CoreExpr::U64(n) => SurfaceExpression::U64(*n),
        CoreExpr::Float(f) => SurfaceExpression::Float(*f),
        CoreExpr::Bool(b) => SurfaceExpression::Bool(*b),
        CoreExpr::Str(s) => SurfaceExpression::Str(s.clone()),
        CoreExpr::Var { name, .. } | CoreExpr::FreeVar(name) => SurfaceExpression::VarRef {
            name: name.clone(),
            escaped: false,
        },
        CoreExpr::DotAccess { expr, field } => SurfaceExpression::DotAccess {
            expr: core_expr_to_surface_node(expr),
            field: field.clone(),
        },
        CoreExpr::Sequential(exprs) => SurfaceExpression::Sequential(
            exprs.iter().map(|e| core_expr_to_surface_node(e)).collect(),
        ),
        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
        } => SurfaceExpression::Call {
            func: core_expr_to_surface_node(func),
            args: args.iter().map(|a| core_expr_to_surface_node(a)).collect(),
            named_args: named_args
                .iter()
                .map(|na| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: core_expr_to_surface_node(&na.node.value),
                            annotation: None,
                        },
                        na.span.clone(),
                    )
                })
                .collect(),
            implied: *implied,
        },
        CoreExpr::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => SurfaceExpression::Fn {
            return_ann: return_ann.clone(),
            params: params
                .iter()
                .map(|p| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceParam {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span.clone(),
                    )
                })
                .collect(),
            body: core_expr_to_surface_node(body),
            desugared: *desugared,
        },
        CoreExpr::TypeAssert {
            annotation, expr, ..
        } => SurfaceExpression::TypeAssert {
            annotation: annotation.clone(),
            expr: core_expr_to_surface_node(expr),
        },
        CoreExpr::Annotated { name, annotation } => SurfaceExpression::Annotated {
            name: name.clone(),
            annotation: annotation.clone(),
        },
        CoreExpr::Rest(name) => SurfaceExpression::Rest(name.clone()),
        CoreExpr::Match { scrutinee, arms } => SurfaceExpression::Match {
            scrutinee: core_expr_to_surface_node(scrutinee),
            arms: arms
                .iter()
                .map(|arm| SurfaceMatchArm {
                    pattern: arm.pattern.clone(),
                    guard: arm.guard.as_ref().map(|g| core_expr_to_surface_node(g)),
                    body: core_expr_to_surface_node(&arm.body),
                })
                .collect(),
        },
        CoreExpr::Quote(inner) => SurfaceExpression::Quote(core_expr_to_surface_node(inner)),
        CoreExpr::Unquote(inner) => SurfaceExpression::Unquote(core_expr_to_surface_node(inner)),
        CoreExpr::UnquoteSplice(inner) => {
            SurfaceExpression::UnquoteSplice(core_expr_to_surface_node(inner))
        }
        CoreExpr::PatternDecl { bindings } => SurfaceExpression::PatternDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::LetDecl { bindings } => SurfaceExpression::LetDecl {
            bindings: bindings.iter().map(core_expr_to_surface_node).collect(),
        },
        CoreExpr::Dict(entries) => SurfaceExpression::Dict(
            entries
                .iter()
                .map(|e| {
                    crate::ast::Spanned::new(
                        crate::ast::SurfaceEntry {
                            key: e.node.key.as_ref().map(|k| core_expr_to_surface_node(k)),
                            value: core_expr_to_surface_node(&e.node.value),
                        },
                        e.span.clone(),
                    )
                })
                .collect(),
        ),
        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => SurfaceExpression::CaseArm {
            let_bindings: let_bindings
                .as_ref()
                .map(|lb| core_expr_to_surface_node(lb.as_ref())),
            pattern: core_expr_to_surface_node(pattern),
            body: core_expr_to_surface_node(body),
        },
        CoreExpr::Error(span) => SurfaceExpression::Error(span.clone()),
        CoreExpr::Placeholder => SurfaceExpression::Placeholder,
    }
}

/// Extract the type name from a dict entry key for TypeAlias qualified tags.
///
/// Recognized key forms (same as desugar.rs):
/// - `Str(s)` — plain string key
/// - `VarRef { name }` — bare identifier key
/// - `Annotated { name, .. }` — annotated name key (T-1052)
///
/// Returns None for computed keys or absent keys.
fn extract_type_name_from_key(key: &Option<Arc<SurfaceNode>>) -> Option<String> {
    match key {
        Some(key_node) => match &key_node.expr {
            SurfaceExpression::Str(s) => Some(s.clone()),
            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
            _ => None, // Computed key
        },
        None => None, // Positional entry
    }
}

/// Lower a TypeAlias body to a constructor dict at runtime (T-1193).
///
/// Produces a `CoreExpr::Dict` containing constructor entries:
/// - Unit constructors (no annotation) → `CtorName: [builtin-variant "TypeName.CtorName"]`
/// - Unit constructors (with annotation) → `CtorName: [builtin-make-annotated [builtin-variant "TypeName.CtorName"] [key: val ...]]`
/// - Payload constructors → `CtorName: [fn [...fields] [builtin-variant "TypeName.CtorName" payload]]`
///
/// The type name (if present) qualifies the variant tags. When absent, uses unqualified tags.
///
/// Produces `CoreExpr` nodes for each constructor entry in the runtime dict.
fn lower_type_alias_to_constructor_dict(
    type_name_opt: Option<String>,
    body: &Arc<SurfaceNode>,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> CoreExpr {
    use crate::ast::{CoreEntry, CoreParam, Span};

    // Extract constructors from the body using the desugar.rs helpers.
    // We need to import the extraction logic. For now, we'll inline a simplified version.
    let ctors = extract_constructors_from_body(&body.expr);

    let syn_span = Span::origin();
    let mut core_entries: Vec<Spanned<CoreEntry>> = Vec::new();

    for ctor in ctors {
        let qualified_tag = match &type_name_opt {
            Some(tn) => format!("{}.{}", tn, ctor.name),
            None => ctor.name.clone(),
        };

        // Create the key for this constructor entry
        let key = Some(Arc::new(Spanned::new(
            CoreExpr::Str(ctor.name.clone()),
            syn_span.clone(),
        )));

        // Create the value: either a unit variant or a constructor function
        let value = if ctor.is_unit {
            // Unit constructor: [builtin-variant "TypeName.CtorName"]
            // If the constructor carries a @[...] annotation (T-1121), wrap with make-annotated.
            let variant_call = Arc::new(Spanned::new(
                CoreExpr::Call {
                    func: Arc::new(Spanned::new(
                        CoreExpr::FreeVar("builtin-variant".to_string()),
                        syn_span.clone(),
                    )),
                    args: vec![Arc::new(Spanned::new(
                        CoreExpr::Str(qualified_tag),
                        syn_span.clone(),
                    ))],
                    named_args: vec![],
                    implied: false,
                },
                syn_span.clone(),
            ));

            if let Some(ann_entries) = &ctor.annotation {
                // Build annotation dict CoreExpr from PropertyDict entries.
                // Each entry is a SurfaceEntry with a string key and literal value.
                // Lower the values through the normal lower() pipeline for correct resolution.
                let ann_core_entries: Vec<Spanned<CoreEntry>> = ann_entries
                    .iter()
                    .map(|se| {
                        let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                        let value = Arc::new(lower(&se.node.value, res, types));
                        Spanned::new(CoreEntry { key, value }, se.span.clone())
                    })
                    .collect();
                let ann_dict = Arc::new(Spanned::new(
                    CoreExpr::Dict(ann_core_entries),
                    syn_span.clone(),
                ));
                // [builtin-make-annotated [builtin-variant "TypeName.CtorName"] [ann_entries...]]
                Arc::new(Spanned::new(
                    CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::FreeVar("builtin-make-annotated".to_string()),
                            syn_span.clone(),
                        )),
                        args: vec![variant_call, ann_dict],
                        named_args: vec![],
                        implied: false,
                    },
                    syn_span.clone(),
                ))
            } else {
                variant_call
            }
        } else {
            // Payload constructor: function that takes named args and returns a variant
            // [fn [let ...fields] [builtin-variant "TypeName.CtorName" [dict field: value ...]]]
            let params: Vec<Spanned<CoreParam>> = ctor
                .fields
                .iter()
                .map(|field_name| {
                    Spanned::new(
                        CoreParam {
                            name: field_name.clone(),
                            annotation: None,
                            variadic: false,
                        },
                        syn_span.clone(),
                    )
                })
                .collect();

            // Build the payload dict: [dict field: field-value ...]
            //
            // CRITICAL: Use CoreExpr::Var { level: 1, slot: idx } instead of
            // CoreExpr::FreeVar(field_name) here. The payload dict is evaluated by
            // eval_dict_core which creates a letrec environment where each field name
            // is bound as a key. Using FreeVar(field_name) would shadow the function
            // param of the same name: FreeVar lookup starts from the dict's own letrec
            // env, finds the dict's own "field" entry (which is the thunk being forced),
            // and triggers E070 circular dependency.
            //
            // Var { level: 1 } skips one level up past the dict's letrec env to the
            // function's call env (created by bind_args_thunks), where the param is
            // bound at slot `idx`. This correctly references the caller's argument
            // without shadowing through the payload dict's letrec scope.
            let payload_entries: Vec<Spanned<CoreEntry>> = ctor
                .fields
                .iter()
                .enumerate()
                .map(|(idx, field_name)| {
                    Spanned::new(
                        CoreEntry {
                            key: Some(Arc::new(Spanned::new(
                                CoreExpr::Str(field_name.clone()),
                                syn_span.clone(),
                            ))),
                            value: Arc::new(Spanned::new(
                                // level=1: one env level up from the payload dict's letrec
                                // env → reaches the function's call env (bind_args_thunks).
                                // slot=idx: params are inserted in declaration order, so
                                // the i-th field is at slot i in the call env.
                                CoreExpr::Var {
                                    name: field_name.clone(),
                                    level: 1,
                                    slot: idx as u32,
                                },
                                syn_span.clone(),
                            )),
                        },
                        syn_span.clone(),
                    )
                })
                .collect();

            let payload_dict = CoreExpr::Dict(payload_entries);

            // Build [builtin-variant "TypeName.CtorName" payload]
            let variant_call = CoreExpr::Call {
                func: Arc::new(Spanned::new(
                    CoreExpr::FreeVar("builtin-variant".to_string()),
                    syn_span.clone(),
                )),
                args: vec![
                    Arc::new(Spanned::new(CoreExpr::Str(qualified_tag), syn_span.clone())),
                    Arc::new(Spanned::new(payload_dict, syn_span.clone())),
                ],
                named_args: vec![],
                implied: false,
            };

            let fn_expr = Arc::new(Spanned::new(
                CoreExpr::Fn {
                    return_ann: None,
                    params,
                    body: Arc::new(Spanned::new(variant_call, syn_span.clone())),
                    desugared: false,
                },
                syn_span.clone(),
            ));

            // Wrap payload constructor with annotation if present (mirrors unit constructor handling)
            if let Some(ann_entries) = &ctor.annotation {
                let ann_core_entries: Vec<Spanned<CoreEntry>> = ann_entries
                    .iter()
                    .map(|se| {
                        let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                        let value = Arc::new(lower(&se.node.value, res, types));
                        Spanned::new(CoreEntry { key, value }, se.span.clone())
                    })
                    .collect();
                let ann_dict = Arc::new(Spanned::new(
                    CoreExpr::Dict(ann_core_entries),
                    syn_span.clone(),
                ));
                Arc::new(Spanned::new(
                    CoreExpr::Call {
                        func: Arc::new(Spanned::new(
                            CoreExpr::FreeVar("builtin-make-annotated".to_string()),
                            syn_span.clone(),
                        )),
                        args: vec![fn_expr, ann_dict],
                        named_args: vec![],
                        implied: false,
                    },
                    syn_span.clone(),
                ))
            } else {
                fn_expr
            }
        };

        core_entries.push(Spanned::new(CoreEntry { key, value }, syn_span.clone()));
    }

    CoreExpr::Dict(core_entries)
}

/// Simplified constructor info for lowering.
struct ConstructorInfo {
    name: String,
    is_unit: bool,
    fields: Vec<String>,
    /// Annotation entries from `@[...]` on the constructor declaration (T-1121).
    /// Present when the constructor was written as `CtorName@[key: val ...]` in the type body.
    /// Used by `lower_type_alias_to_constructor_dict` to emit `[builtin-make-annotated ...]`.
    annotation: Option<Vec<Spanned<SurfaceEntry>>>,
}

/// Extract constructor information from a TypeAlias body.
///
/// Handles the common constructor forms:
/// 1. Bare VarRef uppercase → unit constructor (e.g., `Red`, `None`)
/// 2. Annotated uppercase → unit constructor with annotation
/// 3. Call with uppercase func + no named args → unit constructor (e.g., `[Ok a]`, `[Error String]`)
/// 4. Call with uppercase func + named args → named-field constructor (e.g., `[Circle r: Int]`)
/// 5. Dict with first positional VarRef/Annotated + keyed entries → named-field constructor
fn extract_constructors_from_body(body: &SurfaceExpression) -> Vec<ConstructorInfo> {
    let mut ctors = Vec::new();

    fn is_ctor(s: &str) -> bool {
        crate::eval::is_constructor_name(s)
    }

    fn try_extract_one(expr: &SurfaceExpression, ctors: &mut Vec<ConstructorInfo>) {
        match expr {
            // Bare uppercase VarRef → unit constructor (no annotation)
            SurfaceExpression::VarRef { name, .. } if is_ctor(name) => {
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    fields: Vec::new(),
                    annotation: None,
                });
            }
            // Annotated uppercase → unit constructor with annotation (T-1121).
            // `Red@[category: "primary"]` → unit constructor carrying the PropertyDict entries.
            SurfaceExpression::Annotated { name, annotation } if is_ctor(name) => {
                // Extract PropertyDict entries for annotation wrapping at runtime.
                let ann_entries = match &annotation.node {
                    crate::ast::Annotation::PropertyDict(entries) if !entries.is_empty() => {
                        Some(entries.clone())
                    }
                    _ => None,
                };
                ctors.push(ConstructorInfo {
                    name: name.clone(),
                    is_unit: true,
                    fields: Vec::new(),
                    annotation: ann_entries,
                });
            }
            // Call with uppercase func → unit or named-field constructor
            // [Ok a] → Call { func: VarRef("Ok"), args: [VarRef("a")], named_args: [] } → unit
            // [Circle r: Int] → Call { func: VarRef("Circle"), named_args: [(r, Int)] } → named-field
            SurfaceExpression::Call {
                func, named_args, ..
            } => {
                if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                    if is_ctor(name) {
                        if named_args.is_empty() {
                            // Positional-only args are type params → unit constructor at runtime
                            ctors.push(ConstructorInfo {
                                name: name.clone(),
                                is_unit: true,
                                fields: Vec::new(),
                                annotation: None,
                            });
                        } else {
                            // Named args → named-field constructor
                            let fields = named_args.iter().map(|na| na.node.name.clone()).collect();
                            ctors.push(ConstructorInfo {
                                name: name.clone(),
                                is_unit: false,
                                fields,
                                annotation: None,
                            });
                        }
                    }
                }
            }
            // Dict `[Constructor field: Type ...]` — single named-field constructor
            SurfaceExpression::Dict(entries) if !entries.is_empty() => {
                let first = &entries[0];
                if first.node.key.is_some() {
                    return;
                }
                // Extract constructor name and annotation from the first (positional) entry
                let (ctor_name, ctor_annotation) = match &first.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } if is_ctor(name) => (name.clone(), None),
                    SurfaceExpression::Annotated { name, annotation } if is_ctor(name) => {
                        // Extract PropertyDict annotation entries for the constructor
                        let ann = match &annotation.node {
                            crate::ast::Annotation::PropertyDict(entries)
                                if !entries.is_empty() =>
                            {
                                Some(entries.clone())
                            }
                            _ => None,
                        };
                        (name.clone(), ann)
                    }
                    _ => return,
                };
                let fields: Vec<String> = entries[1..]
                    .iter()
                    .filter_map(|e| {
                        let key = e.node.key.as_ref()?;
                        match &key.expr {
                            SurfaceExpression::Str(s) => Some(s.clone()),
                            SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                            SurfaceExpression::Annotated { name, .. } => Some(name.clone()),
                            _ => None,
                        }
                    })
                    .collect();
                let is_unit = fields.is_empty();
                ctors.push(ConstructorInfo {
                    name: ctor_name,
                    is_unit,
                    fields,
                    annotation: ctor_annotation,
                });
            }
            _ => {}
        }
    }

    // Top-level dispatch: distinguish single-constructor dict from union of constructors.
    match body {
        SurfaceExpression::Dict(entries) => {
            // Distinguish "single named-field constructor dict" from "union of constructors":
            // - Constructor dict: first positional is VarRef/Annotated uppercase AND has keyed entries
            // - Union: each positional entry is a separate constructor
            let is_single_ctor_dict = entries.first().is_some_and(|first| {
                if first.node.key.is_some() {
                    return false;
                }
                let first_is_ctor = matches!(&first.node.value.expr,
                    SurfaceExpression::VarRef { name, .. } if is_ctor(name))
                    || matches!(&first.node.value.expr,
                    SurfaceExpression::Annotated { name, .. } if is_ctor(name));
                let has_keyed = entries[1..].iter().any(|e| e.node.key.is_some());
                first_is_ctor && has_keyed
            });
            if is_single_ctor_dict {
                try_extract_one(body, &mut ctors);
            } else {
                for entry in entries {
                    if entry.node.key.is_none() {
                        try_extract_one(&entry.node.value.expr, &mut ctors);
                    }
                }
            }
        }
        other => try_extract_one(other, &mut ctors),
    }
    ctors
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{ResolutionTable, SurfaceExpression, SurfaceNode, TypeAnnotationTable};
    use std::sync::Arc;

    fn make_node(expr: SurfaceExpression, span: crate::ast::Span) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode { expr, span })
    }

    #[test]
    fn test_lower_int_literal() {
        let span = crate::ast::Span::origin();
        let node = make_node(SurfaceExpression::Int(42), span.clone());
        let res = ResolutionTable::new();
        let types = TypeAnnotationTable::new();

        let lowered = lower(&node, &res, &types);

        assert_eq!(lowered.span, span);
        assert!(matches!(lowered.node, CoreExpr::Int(42)));
    }

    #[test]
    fn test_lower_varref_with_resolution() {
        let span = crate::ast::Span::origin();
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "x".into(),
                escaped: false,
            },
            span,
        );
        let mut res = ResolutionTable::new();
        let types = TypeAnnotationTable::new();

        // Simulate resolver inserting a binding: level 0, slot 3
        let id = crate::ast::node_id(&node);
        res.insert(id, (0, 3));

        let lowered = lower(&node, &res, &types);

        match lowered.node {
            CoreExpr::Var { name, level, slot } => {
                assert_eq!(name, "x");
                assert_eq!(level, 0);
                assert_eq!(slot, 3);
            }
            _ => panic!("expected CoreExpr::Var, got {:?}", lowered.node),
        }
    }

    #[test]
    fn test_lower_varref_without_resolution() {
        let span = crate::ast::Span::origin();
        let node = make_node(
            SurfaceExpression::VarRef {
                name: "unbound".into(),
                escaped: false,
            },
            span,
        );
        let res = ResolutionTable::new(); // Empty — no resolution entry
        let types = TypeAnnotationTable::new();

        let lowered = lower(&node, &res, &types);

        match lowered.node {
            CoreExpr::FreeVar(name) => {
                assert_eq!(name, "unbound");
            }
            _ => panic!("expected CoreExpr::FreeVar, got {:?}", lowered.node),
        }
    }
}
