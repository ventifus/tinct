//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
//!
//! `lower()` is called per-thunk when a `Surface` thunk is first forced.
//! It is a pure function of `(SurfaceNode, ResolutionTable, TypeAnnotationTable)`.
//!
//! Key transformations:
//! - `VarRef` → `Var` (resolved de Bruijn coordinates) or `FreeVar` (unresolvable)
//! - `Pipe { lhs, rhs }` → `Call { func: rhs, args: [lhs], implied: true }` (syntactic sugar)
//! - `TypeAssert` → `TypeAssert` (with resolved_type from TypeAnnotationTable) or
//!   `RuntimeTypeCheck` (for macro-synthesized nodes absent from the table)
//! - All other variants: structural lowering, recursing into child nodes

use std::sync::Arc;

use crate::ast::{
    node_id, Annotation, CoreEntry, CoreExpr, CoreMatchArm, CoreNamedArg, CoreParam,
    ResolutionTable, Spanned, SurfaceExpression, SurfaceNode, TypeAnnotationTable,
};

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
    let span = arc.span;
    let core_expr = lower_expr(arc, &arc.expr, res, types);
    Spanned::new(core_expr, span)
}

fn lower_expr(
    arc: &Arc<SurfaceNode>,
    expr: &SurfaceExpression,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> CoreExpr {
    match expr {
        SurfaceExpression::Int(n) => CoreExpr::Int(*n),
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
                // ADT constructor scoping: skip TypeAlias (and other Decl) entries at runtime.
                // TypeAlias entries are compile-time declarations; forcing them as values
                // produces a Placeholder error (E081). The constructors were already injected
                // as real `CtorName: [variant "CtorName"]` surface entries by
                // `desugar::inject_adt_constructors_surface_program` (runs before resolve),
                // so skipping the Decl here is safe.
                if let SurfaceExpression::Decl(_) = &se.node.value.expr {
                    // Declaration form in dict value position: skip entirely at runtime.
                    // Type checker handles it via Pass 0c (typecheck_dict.rs).
                    continue;
                }
                let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                let value = Arc::new(lower(&se.node.value, res, types));
                core_entries.push(Spanned::new(CoreEntry { key, value }, se.span));
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
                        na.span,
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
                        p.span,
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
                Some(ty) => CoreExpr::TypeAssert {
                    annotation: annotation.clone(),
                    expr: Arc::new(lower(inner, res, types)),
                    resolved_type: ty.clone(),
                },
                None => {
                    // Macro-synthesized node — bypassed typechecking.
                    // Use RuntimeTypeCheck for best-effort dynamic validation.
                    CoreExpr::RuntimeTypeCheck {
                        annotation: annotation.clone(),
                        expr: Arc::new(lower(inner, res, types)),
                        default: None,
                    }
                }
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
                    pattern: arm.pattern.clone(),
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

        SurfaceExpression::TypeApp { func, arg } => CoreExpr::TypeApp {
            func: Arc::new(lower(func, res, types)),
            arg: Arc::new(lower(arg, res, types)),
        },

        SurfaceExpression::Placeholder => CoreExpr::Placeholder,

        // Declaration forms embedded in expression position (e.g., dict entry values).
        // At runtime these produce Placeholder (an error when forced); the type checker
        // registers them via Pass 0c before evaluation occurs.
        SurfaceExpression::Decl(_) => CoreExpr::Placeholder,

        SurfaceExpression::Error(span) => CoreExpr::Error(*span),
    }
}

/// Convert a `Spanned<CoreExpr>` back to an `Arc<SurfaceNode>` for quote/unquote evaluation.
///
/// Bridges through `Expr` via `core_expr_to_expr` + `expr_to_surface_node`.
/// Used by the `CoreExpr::Quote` arm to get a `SurfaceNode` for `eval_quote_walk`.
///
/// DESIGN DECISION: This round-trip (CoreExpr→Expr→SurfaceNode) is intentional.
/// Quote captures *surface syntax* for metaprogramming, not the desugared CoreExpr form
/// with de Bruijn indices. Users expect `[quote x]` to show the variable name "x",
/// not `FreeVar(0)`.
///
/// Direct CoreExpr→SurfaceNode conversion (no Expr bridge).
pub fn core_expr_to_surface_node(
    expr: &crate::ast::Spanned<crate::ast::CoreExpr>,
) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode {
        expr: core_expr_to_surface_expr(&expr.node),
        span: expr.span,
    })
}

fn core_expr_to_surface_expr(core: &crate::ast::CoreExpr) -> SurfaceExpression {
    use crate::ast::{CoreExpr, SurfaceMatchArm};
    match core {
        CoreExpr::Int(n) => SurfaceExpression::Int(*n),
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
                        },
                        na.span,
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
                        p.span,
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
        // RuntimeTypeCheck has no SurfaceExpression equivalent; map to TypeAssert (annotation-only)
        CoreExpr::RuntimeTypeCheck {
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
            bindings: bindings
                .iter()
                .map(|b| core_expr_to_surface_node(b))
                .collect(),
        },
        CoreExpr::LetDecl { bindings } => SurfaceExpression::LetDecl {
            bindings: bindings
                .iter()
                .map(|b| core_expr_to_surface_node(b))
                .collect(),
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
                        e.span,
                    )
                })
                .collect(),
        ),
        CoreExpr::CaseArm { pattern, body } => SurfaceExpression::CaseArm {
            pattern: core_expr_to_surface_node(pattern),
            body: core_expr_to_surface_node(body),
        },
        CoreExpr::TypeApp { func, arg } => SurfaceExpression::TypeApp {
            func: core_expr_to_surface_node(func),
            arg: core_expr_to_surface_node(arg),
        },
        CoreExpr::Error(span) => SurfaceExpression::Error(*span),
        CoreExpr::Placeholder => SurfaceExpression::Placeholder,
    }
}

/// Lower an entire SurfaceProgram's expressions in a document.
///
/// Utility for batch-lowering all expression items in a document.
/// Declaration items (SurfaceItem::Decl) are skipped — they were processed by the expander.
#[allow(dead_code)] // Used in Part E when batch lowering is activated
pub fn lower_document_exprs<'a>(
    nodes: impl Iterator<Item = &'a Arc<SurfaceNode>>,
    res: &ResolutionTable,
    types: &TypeAnnotationTable,
) -> Vec<Spanned<CoreExpr>> {
    nodes.map(|node| lower(node, res, types)).collect()
}

/// Lower a single annotation (recursing into PropertyDict entry values).
///
/// Annotation property dict values are SurfaceExpression nodes stored in the old Entry type.
/// This function lowers them in place if needed for annotation-driven evaluation.
/// In practice, annotations are resolved statically during typechecking and do not
/// need runtime lowering — this function exists for completeness.
#[allow(dead_code)] // Used if annotation runtime evaluation is needed in future sprints
pub fn lower_annotation(
    ann: &Annotation,
    _res: &ResolutionTable,
    _types: &TypeAnnotationTable,
) -> Annotation {
    match ann {
        Annotation::Simple(_) | Annotation::Annotated(_, _) => ann.clone(),
        Annotation::PropertyDict(_) => {
            // PropertyDict entries contain old Expr nodes (pre-migration).
            // During the full migration (when Annotation uses SurfaceNode), this
            // function will recurse into entry values. For now, clone as-is.
            ann.clone()
        }
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
        let node = make_node(SurfaceExpression::Int(42), span);
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
