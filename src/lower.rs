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
    Spanned, SurfaceExpression, SurfaceNode, TypeAnnotationTable,
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
        Pattern::Variable(_)
        | Pattern::Wildcard
        | Pattern::Literal(_)
        | Pattern::Pin(_)
        | Pattern::TypeTag(_) => pat.clone(),
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
                // Most Decl forms (TypeAlias, ClassDecl, MacroDecl) are skipped at runtime.
                // Constructor injection is handled by the desugar pass; the type checker
                // handles Decl entries via Pass 0c (typecheck_dict.rs).
                //
                // EXCEPTION (B-353): InstanceDecl with a non-empty arm is NOT skipped —
                // it produces a method dict so that `MonadResult.bind` etc. work at runtime.
                // The SurfaceExpression::Decl arm in lower() handles the actual lowering.
                if let SurfaceExpression::Decl(decl) = &se.node.value.expr {
                    let is_instance = matches!(
                        decl.as_ref(),
                        crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. }
                            if !arms.is_empty()
                    );
                    if !is_instance {
                        continue;
                    }
                }
                let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                let value = Arc::new(lower(&se.node.value, res, types));
                core_entries.push(Spanned::new(CoreEntry { key, value }, se.span.clone()));
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

        SurfaceExpression::CaseArm { pattern, body } => CoreExpr::CaseArm {
            pattern: Arc::new(lower(pattern, res, types)),
            body: Arc::new(lower(body, res, types)),
        },

        SurfaceExpression::Placeholder => CoreExpr::Placeholder,

        // Declaration forms embedded in expression position (e.g., dict entry values).
        // At runtime these produce Placeholder (an error when forced); the type checker
        // registers them via Pass 0c before evaluation occurs.
        //
        // EXCEPTION: InstanceDecl produces a method dict as its runtime value (B-353).
        // This enables instance method access at runtime (e.g., MonadResult.bind).
        SurfaceExpression::Decl(decl) => match decl.as_ref() {
            crate::ast::SurfaceDeclaration::InstanceDecl { arms, .. } if !arms.is_empty() => {
                // Lower the methods from the first arm to a dict.
                //
                // Single-arm assumption: InstanceDecl lowering here only fires for instances
                // that appear as dict entry VALUES (expression position), e.g.:
                //   `MonadResult: [instance Monad [let m@Result]: [bind: ...]]`
                // All such instances in the prelude have exactly one [let ...] arm, so
                // arms[0] is always the correct and only arm.
                //
                // Multi-arm instances (e.g., `Addable` with four `[let a@T b@U c]` arms)
                // are declared at the top level as SurfaceItem::Decl, not as dict entry
                // values, and therefore never reach this code path.
                //
                // If a future multi-arm instance were declared in dict entry position,
                // arms[1..N] would be silently dropped. To support that case, the lowering
                // pass would need to know which arm applies (type-directed dispatch at
                // runtime), or all arms' methods would need to be merged into a single dict
                // (only valid when method names are disjoint across arms). The correct
                // long-term architecture is to transform instances in the desugar pass
                // (alongside inject_adt_constructors_surface_program) rather than here.
                let method_entries = &arms[0].1;
                let core_entries: Vec<Spanned<CoreEntry>> = method_entries
                    .iter()
                    .map(|se| {
                        let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                        let value = Arc::new(lower(&se.node.value, res, types));
                        Spanned::new(CoreEntry { key, value }, se.span.clone())
                    })
                    .collect();
                CoreExpr::Dict(core_entries)
            }
            _ => CoreExpr::Placeholder,
        },

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
        CoreExpr::CaseArm { pattern, body } => SurfaceExpression::CaseArm {
            pattern: core_expr_to_surface_node(pattern),
            body: core_expr_to_surface_node(body),
        },
        CoreExpr::Error(span) => SurfaceExpression::Error(span.clone()),
        CoreExpr::Placeholder => SurfaceExpression::Placeholder,
    }
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
