//! Lowering pass: converts `SurfaceExpression` to `CoreExpr` for the evaluator.
// Functions are wired into eval_materialize.rs in Sprint 1 Part E.
#![allow(dead_code)]
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

        SurfaceExpression::Dict(entries) => CoreExpr::Dict(
            entries
                .iter()
                .map(|se| {
                    let key = se.node.key.as_ref().map(|k| Arc::new(lower(k, res, types)));
                    let value = Arc::new(lower(&se.node.value, res, types));
                    Spanned::new(CoreEntry { key, value }, se.span)
                })
                .collect(),
        ),

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

        SurfaceExpression::Error(span) => CoreExpr::Error(*span),
    }
}

/// Lower an entire SurfaceProgram's expressions in a document.
///
/// Utility for batch-lowering all expression items in a document.
/// Declaration items (SurfaceItem::Decl) are skipped — they were processed by the expander.
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
