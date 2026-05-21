//! Surface AST field extraction for match dispatch and dot-access - MINIMAL VERSION
#![allow(dead_code)]

use crate::ast::{Annotation, DotKey, Spanned, SurfaceExpression, SurfaceNode};
use crate::value::Value;
use std::sync::Arc;

/// Extract a named field from a `SurfaceNode` as a `Value`.
///
/// STUB: returns empty dict for all field accesses — full implementation in Part D (runtime-v2 Sprint 1).
///
/// Any tinct code that accesses `(ast-of x).field` will receive `{}` instead of the actual
/// field value until Part D is implemented. In debug builds, accessing any field emits a warning
/// to stderr so the stub behavior is never silent during development.
///
/// TODO (Part D): implement field extraction per `SurfaceExpression` variant — each variant exposes
/// different fields (e.g., `Call` exposes `fn`, `args`, `named`; `Fn` exposes `params`, `body`).
pub fn surface_node_get_field(
    node: &Arc<SurfaceNode>,
    field: &str,
    _ctx: &std::rc::Rc<crate::eval::EvalContext>,
) -> Value {
    // STUB: field extraction not yet implemented (Part D).
    // Emits a diagnostic in debug builds so callers are not silently misled.
    #[cfg(debug_assertions)]
    {
        let tag = surface_expr_tag(&node.expr);
        eprintln!(
            "[tinct STUB] surface_node_get_field: field {:?} on {:?} node — returning {{}} (Part D not yet implemented)",
            field, tag
        );
    }
    #[cfg(not(debug_assertions))]
    let _ = node;
    #[cfg(not(debug_assertions))]
    let _ = field;
    Value::Dict(indexmap::IndexMap::new())
}

/// Convert a DotKey to a Value::Variant (Ident | Index).
pub fn dot_key_to_value(key: &DotKey) -> Value {
    match key {
        DotKey::Ident(_) => Value::Variant {
            tag: "Ident".into(),
            payload: None,
        },
        DotKey::Int(_) => Value::Variant {
            tag: "Index".into(),
            payload: None,
        },
    }
}

/// Convert an Annotation to a Value::Variant (Simple | PropertyDict | Annotated).
pub fn annotation_to_value(ann: &Spanned<Annotation>) -> Value {
    match &ann.node {
        Annotation::Simple(_) => Value::Variant {
            tag: "Simple".into(),
            payload: None,
        },
        Annotation::PropertyDict(_) => Value::Variant {
            tag: "PropertyDict".into(),
            payload: None,
        },
        Annotation::Annotated(_, _) => Value::Variant {
            tag: "Annotated".into(),
            payload: None,
        },
    }
}

/// Intern a field name string as a `&'static str` without leaking memory.
///
/// Returns a compile-time static string for the known SurfaceExpression field names.
/// For unknown field names, falls back to `Box::leak` — this only happens for
/// unrecognized fields, which return null anyway, so the leak is bounded.
pub fn intern_field_name(field: &str) -> &'static str {
    // Pre-computed static strings for all known SurfaceExpression field names.
    // This avoids Box::leak on every DotAccess of a Value::Expression.
    match field {
        "value" => "value",
        "span" => "span",
        "name" => "name",
        "escaped" => "escaped",
        "target" => "target",
        "field" => "field",
        "lhs" => "lhs",
        "rhs" => "rhs",
        "exprs" => "exprs",
        "entries" => "entries",
        "fn" => "fn",
        "args" => "args",
        "named" => "named",
        "implied" => "implied",
        "params" => "params",
        "body" => "body",
        "return-ann" => "return-ann",
        "desugared" => "desugared",
        "annotation" => "annotation",
        "expr" => "expr",
        "scrutinee" => "scrutinee",
        "arms" => "arms",
        "arg" => "arg",
        "bindings" => "bindings",
        "pattern" => "pattern",
        "guard" => "guard",
        "key" => "key",
        "tag" => "tag",
        "forms" => "forms",
        "inner" => "inner",
        "class-name" => "class-name",
        "message" => "message",
        "documents" => "documents",
        "expressions" => "expressions",
        "declarations" => "declarations",
        "output-type" => "output-type",
        "expects" => "expects",
        // Unknown field names: Box::leak is acceptable here because unrecognized
        // field access on Value::Expression returns null (minimal stub), so this
        // code path is only triggered for programming errors, not normal usage.
        other => Box::leak(other.to_string().into_boxed_str()),
    }
}

/// Extract the variant tag from a `SurfaceExpression` as a static string.
pub fn surface_expr_tag(expr: &SurfaceExpression) -> &'static str {
    match expr {
        SurfaceExpression::Int(_) => "IntLiteral",
        SurfaceExpression::Float(_) => "FloatLiteral",
        SurfaceExpression::Bool(_) => "BoolLiteral",
        SurfaceExpression::Str(_) => "StrLiteral",
        SurfaceExpression::VarRef { .. } => "Var",
        SurfaceExpression::DotAccess { .. } => "DotAccess",
        SurfaceExpression::Pipe { .. } => "Pipe",
        SurfaceExpression::Sequential(_) => "Sequential",
        SurfaceExpression::Dict(_) => "Dict",
        SurfaceExpression::Call { .. } => "Call",
        SurfaceExpression::Fn { .. } => "Fn",
        SurfaceExpression::TypeAssert { .. } => "TypeAssert",
        SurfaceExpression::Annotated { .. } => "Annotated",
        SurfaceExpression::Rest(_) => "Rest",
        SurfaceExpression::Match { .. } => "Match",
        SurfaceExpression::Quote(_) => "Quote",
        SurfaceExpression::Unquote(_) => "Unquote",
        SurfaceExpression::UnquoteSplice(_) => "UnquoteSplice",
        SurfaceExpression::PatternDecl { .. } => "PatternDecl",
        SurfaceExpression::LetDecl { .. } => "LetDecl",
        SurfaceExpression::CaseArm { .. } => "CaseArm",
        SurfaceExpression::TypeApp { .. } => "TypeApp",
        SurfaceExpression::Placeholder => "Placeholder",
        SurfaceExpression::Error(_) => "Error",
    }
}
