//! Surface AST field extraction for match dispatch and dot-access.
// Functions are wired into eval.rs/eval_materialize.rs in Sprint 1 Part E.
#![allow(dead_code)]
//!
//! These functions bridge the Surface AST types and the runtime value layer:
//! - `surface_expr_tag()` — O(1) variant name extraction, used by the match evaluator
//! - `surface_node_get_field()` — field extraction for `AstNodeField` thunk evaluation
//!   and dot-access on `Value::Expression`
//! - `surface_doc_match_view()`, `surface_program_match_view()` — match payload views
//!   for `Value::Document` and `Value::Program`
//! - `surface_node_from_value()` — convert macro output back to `Arc<SurfaceNode>`
//!
//! The field extraction functions return `Value` variants. The new `Value::Expression`,
//! `Value::Document`, `Value::Program` variants are added in Sprint 1, Part F.
//! Until Part F lands, this module provides only the tag extraction functions that
//! do not require the new Value variants.

use std::sync::Arc;

use crate::ast::{
    SurfaceDeclaration, SurfaceDocument, SurfaceExpression, SurfaceNode, SurfaceProgram,
};

// ============================================================================
// Tag extraction — O(1), used by the match evaluator for Value::Expression dispatch
// ============================================================================

/// Extract the variant tag from a `SurfaceExpression` as a static string.
///
/// Returns the tinct-visible type name (e.g. `"Var"`, `"Call"`, `"IntLiteral"`).
/// This is called O(1) — no allocation, no recursion.
///
/// These names match the `Expression` tinct type declaration in prelude.llt.
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

/// Extract the variant tag from a `SurfaceDeclaration` as a static string.
pub fn surface_decl_tag(decl: &SurfaceDeclaration) -> &'static str {
    match decl {
        SurfaceDeclaration::TypeAlias { .. } => "TypeAlias",
        SurfaceDeclaration::ClassDecl { .. } => "ClassDecl",
        SurfaceDeclaration::InstanceDecl { .. } => "InstanceDecl",
        SurfaceDeclaration::DefMacro { .. } => "DefMacro",
        SurfaceDeclaration::MacroDecl { .. } => "MacroDecl",
        SurfaceDeclaration::SyntaxClass { .. } => "SyntaxClass",
        SurfaceDeclaration::Splice(_) => "Splice",
    }
}

// ============================================================================
// Document and program tag extraction
// ============================================================================

/// Returns the match tag for a SurfaceDocument.
/// Always `"Document"` — used by match evaluator for Value::Document dispatch.
pub fn surface_doc_tag(_doc: &SurfaceDocument) -> &'static str {
    "Document"
}

/// Returns the match tag for a SurfaceProgram.
/// Always `"Program"` — used by match evaluator for Value::Program dispatch.
pub fn surface_program_tag(_prog: &SurfaceProgram) -> &'static str {
    "Program"
}

// ============================================================================
// Surface node field names for each variant
// ============================================================================

/// Returns the list of field names that a given expression variant exposes.
///
/// Used by the match evaluator to know which bindings to create for a given arm.
/// The caller creates one `AstNodeField` thunk per field name in the match pattern.
pub fn surface_expr_field_names(expr: &SurfaceExpression) -> &'static [&'static str] {
    match expr {
        SurfaceExpression::Int(_) => &["value", "span"],
        SurfaceExpression::Float(_) => &["value", "span"],
        SurfaceExpression::Bool(_) => &["value", "span"],
        SurfaceExpression::Str(_) => &["value", "span"],
        SurfaceExpression::VarRef { .. } => &["name", "escaped", "span"],
        SurfaceExpression::DotAccess { .. } => &["target", "field", "span"],
        SurfaceExpression::Pipe { .. } => &["lhs", "rhs", "span"],
        SurfaceExpression::Sequential(_) => &["exprs", "span"],
        SurfaceExpression::Dict(_) => &["entries", "span"],
        SurfaceExpression::Call { .. } => &["fn", "args", "named", "implied", "span"],
        SurfaceExpression::Fn { .. } => &["params", "body", "return-ann", "desugared", "span"],
        SurfaceExpression::TypeAssert { .. } => &["annotation", "expr", "span"],
        SurfaceExpression::Annotated { .. } => &["name", "annotation", "span"],
        SurfaceExpression::Rest(_) => &["name", "span"],
        SurfaceExpression::Match { .. } => &["scrutinee", "arms", "span"],
        SurfaceExpression::Quote(_) => &["expr", "span"],
        SurfaceExpression::Unquote(_) => &["expr", "span"],
        SurfaceExpression::UnquoteSplice(_) => &["expr", "span"],
        SurfaceExpression::PatternDecl { .. } => &["bindings", "span"],
        SurfaceExpression::LetDecl { .. } => &["bindings", "span"],
        SurfaceExpression::CaseArm { .. } => &["pattern", "body", "span"],
        SurfaceExpression::TypeApp { .. } => &["fn", "arg", "span"],
        SurfaceExpression::Placeholder => &["span"],
        SurfaceExpression::Error(_) => &["span"],
    }
}

// ============================================================================
// Macro output reconstruction
// ============================================================================

/// Error returned when converting a macro output value back to a SurfaceNode fails.
#[derive(Debug)]
pub struct MacroConversionError {
    pub message: String,
}

impl std::fmt::Display for MacroConversionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "macro output conversion error: {}", self.message)
    }
}

impl std::error::Error for MacroConversionError {}

/// Convert a macro output `Value` back to a `SurfaceNode`.
///
/// This is the inverse of quoting: used when a macro returns a `Value` that needs
/// to be inserted into the AST. The macro expander calls this to convert the
/// transformer's output back to `Arc<SurfaceNode>`.
///
/// Supported conversions:
/// - `Value::Expression(node)` → return the node directly
/// - `Value::Dict` or `Value::Variant` → use `dict_to_ast()` then `expr_to_surface_node()`
///
/// Returns `Err` if the value is not a valid AST representation (e.g., primitive values,
/// non-AST dicts).
pub fn surface_node_from_value(
    v: &Value,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, crate::error::EvalError> {
    use crate::ast_convert::expr_to_surface_node;
    use crate::ast_dict::dict_to_ast;

    match v {
        // Fast path: already a SurfaceNode
        Value::Expression(node) => Ok(Arc::clone(node)),

        // Slow path: Dict or Variant → dict_to_ast → expr_to_surface_node
        Value::Dict(_) | Value::Variant { .. } => {
            let expr = dict_to_ast(v, ctx).map_err(|ast_err| {
                crate::error::EvalError::macro_error(
                    format!("macro output is not a valid AST: {}", ast_err),
                    crate::ast::Span::origin(),
                )
            })?;
            Ok(expr_to_surface_node(&expr))
        }

        // Non-AST values
        _ => Err(crate::error::EvalError::macro_error(
            format!(
                "macro output must be an Expression, Dict, or Variant; got {}",
                v.type_name()
            ),
            crate::ast::Span::origin(),
        )),
    }
}

// ============================================================================
// Field extraction — returns Value for field access on Value::Expression
// ============================================================================

use crate::ast::{Annotation, DotKey, Spanned};
use crate::value::{string_val, Key, Value};

/// Extract a named field from a `SurfaceNode` as a `Value`.
///
/// Primitive and expression-typed fields return values directly.
/// Sequence-typed fields (args, params, entries, bindings, arms) return
/// `Value::Dict(IndexMap<Key::Int, ThunkId>)` — integer-keyed lists of
/// `Value::Expression` entries, allocated into the EvalContext arena.
/// The `span` field returns null (Span→Dict encoding deferred).
/// Unrecognized fields return null.
pub fn surface_node_get_field(
    node: &Arc<SurfaceNode>,
    field: &str,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    // null sentinel — returned for absent optional fields and unrecognized fields
    let null = || Value::Dict(indexmap::IndexMap::new());

    match (&node.expr, field) {
        // span field — convert to Dict with position fields
        (_, "span") => span_to_value(&node.span, ctx),

        // --- IntLiteral ---
        (SurfaceExpression::Int(n), "value") => Value::Int(*n),

        // --- FloatLiteral ---
        (SurfaceExpression::Float(n), "value") => Value::Float(*n),

        // --- BoolLiteral ---
        (SurfaceExpression::Bool(b), "value") => Value::Bool(*b),

        // --- StrLiteral ---
        (SurfaceExpression::Str(s), "value") => string_val(s),

        // --- Var ---
        (SurfaceExpression::VarRef { name, .. }, "name") => string_val(name),
        (SurfaceExpression::VarRef { escaped, .. }, "escaped") => Value::Bool(*escaped),

        // --- DotAccess ---
        (SurfaceExpression::DotAccess { expr: inner, .. }, "target") => {
            Value::Expression(Arc::clone(inner))
        }
        (SurfaceExpression::DotAccess { field: dot_key, .. }, "field") => dot_key_to_value(dot_key),

        // --- Pipe ---
        (SurfaceExpression::Pipe { lhs, .. }, "lhs") => Value::Expression(Arc::clone(lhs)),
        (SurfaceExpression::Pipe { rhs, .. }, "rhs") => Value::Expression(Arc::clone(rhs)),

        // --- Sequential ---
        (SurfaceExpression::Sequential(exprs), "exprs") => nodes_to_list_dict(exprs, ctx),

        // --- Dict ---
        (SurfaceExpression::Dict(entries), "entries") => surface_entries_to_list_dict(entries, ctx),

        // --- Call ---
        (SurfaceExpression::Call { func, .. }, "fn") => Value::Expression(Arc::clone(func)),
        (SurfaceExpression::Call { args, .. }, "args") => nodes_to_list_dict(args, ctx),
        (SurfaceExpression::Call { named_args, .. }, "named") => {
            named_args_to_list_dict(named_args, ctx)
        }
        (SurfaceExpression::Call { implied, .. }, "implied") => Value::Bool(*implied),

        // --- Fn ---
        (SurfaceExpression::Fn { params, .. }, "params") => params_to_list_dict(params, ctx),
        (SurfaceExpression::Fn { body, .. }, "body") => Value::Expression(Arc::clone(body)),
        (SurfaceExpression::Fn { return_ann, .. }, "return-ann") => {
            annotation_opt_to_value(return_ann.as_ref())
        }
        (SurfaceExpression::Fn { desugared, .. }, "desugared") => Value::Bool(*desugared),

        // --- TypeAssert ---
        (SurfaceExpression::TypeAssert { annotation, .. }, "annotation") => {
            annotation_to_value(annotation)
        }
        (SurfaceExpression::TypeAssert { expr: inner, .. }, "expr") => {
            Value::Expression(Arc::clone(inner))
        }

        // --- Annotated ---
        (SurfaceExpression::Annotated { name, .. }, "name") => string_val(name),
        (SurfaceExpression::Annotated { annotation, .. }, "annotation") => {
            annotation_to_value(annotation)
        }

        // --- Rest ---
        (SurfaceExpression::Rest(name_opt), "name") => match name_opt {
            Some(n) => string_val(n),
            None => null(),
        },

        // --- Match ---
        (SurfaceExpression::Match { scrutinee, .. }, "scrutinee") => {
            Value::Expression(Arc::clone(scrutinee))
        }
        (SurfaceExpression::Match { arms, .. }, "arms") => match_arms_to_list_dict(arms, ctx),

        // --- Quote / Unquote / UnquoteSplice ---
        (SurfaceExpression::Quote(inner), "expr")
        | (SurfaceExpression::Unquote(inner), "expr")
        | (SurfaceExpression::UnquoteSplice(inner), "expr") => Value::Expression(Arc::clone(inner)),

        // --- PatternDecl / LetDecl ---
        (SurfaceExpression::PatternDecl { bindings }, "bindings")
        | (SurfaceExpression::LetDecl { bindings }, "bindings") => {
            nodes_to_list_dict(bindings, ctx)
        }

        // --- CaseArm ---
        (SurfaceExpression::CaseArm { pattern, .. }, "pattern") => {
            Value::Expression(Arc::clone(pattern))
        }
        (SurfaceExpression::CaseArm { body, .. }, "body") => Value::Expression(Arc::clone(body)),

        // --- TypeApp ---
        (SurfaceExpression::TypeApp { func, .. }, "fn") => Value::Expression(Arc::clone(func)),
        (SurfaceExpression::TypeApp { arg, .. }, "arg") => Value::Expression(Arc::clone(arg)),

        // Field not applicable to this variant — return null sentinel
        _ => null(),
    }
}

/// Build an integer-keyed list Dict from a sequence of SurfaceNodes.
/// Each entry is `Value::Expression(Arc<SurfaceNode>)`, allocated into the arena.
fn nodes_to_list_dict(
    nodes: &[Arc<SurfaceNode>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    let mut map = indexmap::IndexMap::new();
    for (i, node) in nodes.iter().enumerate() {
        use crate::value::Thunk;
    
        let thunk = Arc::new(Thunk::new_materialized(
            Value::Expression(Arc::clone(node)),
            node.span,
        ));
        let tid = ctx.alloc_thunk(thunk);
        map.insert(Key::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Build a list Dict from SurfaceEntry nodes (for Dict.entries field).
fn surface_entries_to_list_dict(
    entries: &[crate::ast::Spanned<crate::ast::SurfaceEntry>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let mut map = indexmap::IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // Build Entry Variant: {key: Expression, value: Expression, span: []}
        let key_val = entry.node.key.as_ref().map_or_else(
            || Value::Dict(indexmap::IndexMap::new()),
            |k| Value::Expression(Arc::clone(k)),
        );
        let val_val = Value::Expression(Arc::clone(&entry.node.value));
        // Pack as a Variant("Entry", {key: ..., value: ...})
        let mut payload = indexmap::IndexMap::new();
        payload.insert(
            Key::String("key".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(key_val, entry.span))),
        );
        payload.insert(
            Key::String("value".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val_val, entry.span))),
        );
        let entry_variant = Value::Variant {
            tag: "Entry".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                entry.span,
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(entry_variant, entry.span)));
        map.insert(Key::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Build a list Dict from SurfaceNamedArg nodes.
fn named_args_to_list_dict(
    named_args: &[crate::ast::Spanned<crate::ast::SurfaceNamedArg>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let mut map = indexmap::IndexMap::new();
    for (i, na) in named_args.iter().enumerate() {
        let mut payload = indexmap::IndexMap::new();
        payload.insert(
            Key::String("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&na.node.name),
                na.span,
            ))),
        );
        payload.insert(
            Key::String("value".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Expression(Arc::clone(&na.node.value)),
                na.span,
            ))),
        );
        let na_variant = Value::Variant {
            tag: "NamedArg".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                na.span,
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(na_variant, na.span)));
        map.insert(Key::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Build a list Dict from SurfaceParam nodes.
fn params_to_list_dict(
    params: &[crate::ast::Spanned<crate::ast::SurfaceParam>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let mut map = indexmap::IndexMap::new();
    for (i, p) in params.iter().enumerate() {
        let mut payload = indexmap::IndexMap::new();
        payload.insert(
            Key::String("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&p.node.name),
                p.span,
            ))),
        );
        payload.insert(
            Key::String("variadic".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Bool(p.node.variadic),
                p.span,
            ))),
        );
        let p_variant = Value::Variant {
            tag: "Parameter".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                p.span,
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(p_variant, p.span)));
        map.insert(Key::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Build a list Dict from SurfaceMatchArm nodes.
fn match_arms_to_list_dict(
    arms: &[crate::ast::SurfaceMatchArm],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::ast::Span;
    use crate::value::Thunk;

    let span = Span::origin();
    let mut map = indexmap::IndexMap::new();
    for (i, arm) in arms.iter().enumerate() {
        let body_val = Value::Expression(Arc::clone(&arm.body));
        let guard_val = arm.guard.as_ref().map_or_else(
            || Value::Dict(indexmap::IndexMap::new()),
            |g| Value::Expression(Arc::clone(g)),
        );
        let mut payload = indexmap::IndexMap::new();
        payload.insert(
            Key::String("body".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(body_val, span))),
        );
        payload.insert(
            Key::String("guard".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(guard_val, span))),
        );
        let arm_variant = Value::Variant {
            tag: "MatchArm".into(),
            payload: Some(
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(payload), span))),
            ),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(arm_variant, span)));
        map.insert(Key::Int(i as i64), tid);
    }
    Value::Dict(map)
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

fn annotation_opt_to_value(ann: Option<&Spanned<Annotation>>) -> Value {
    match ann {
        Some(a) => annotation_to_value(a),
        None => Value::Dict(indexmap::IndexMap::new()),
    }
}

/// Convert a `Span` to a `Value::Dict` with position fields.
///
/// Returns a Dict with integer fields:
/// - `start_line`, `start_col`, `end_line`, `end_col` (1-based)
/// - `start_offset`, `end_offset` (0-based byte offsets)
///
/// This is used when extracting the `span` field from AST nodes. The Dict is
/// materialized directly (not thunked) since all fields are primitive integers.
pub fn span_to_value(
    span: &crate::ast::Span,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;


    let mut map = indexmap::IndexMap::new();

    map.insert(
        Key::String("start_line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            *span,
        ))),
    );
    map.insert(
        Key::String("start_col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            *span,
        ))),
    );
    map.insert(
        Key::String("end_line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            *span,
        ))),
    );
    map.insert(
        Key::String("end_col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            *span,
        ))),
    );
    map.insert(
        Key::String("start_offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            *span,
        ))),
    );
    map.insert(
        Key::String("end_offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            *span,
        ))),
    );

    Value::Dict(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Span, SurfaceNode};
    use std::sync::Arc;

    fn make_node(expr: SurfaceExpression) -> Arc<SurfaceNode> {
        Arc::new(SurfaceNode {
            expr,
            span: Span::origin(),
        })
    }

    #[test]
    fn test_surface_expr_tag_literals() {
        assert_eq!(surface_expr_tag(&SurfaceExpression::Int(42)), "IntLiteral");
        assert_eq!(
            surface_expr_tag(&SurfaceExpression::Float(3.14)),
            "FloatLiteral"
        );
        assert_eq!(
            surface_expr_tag(&SurfaceExpression::Bool(true)),
            "BoolLiteral"
        );
        assert_eq!(
            surface_expr_tag(&SurfaceExpression::Str("hello".into())),
            "StrLiteral"
        );
    }

    #[test]
    fn test_surface_expr_tag_varref() {
        let expr = SurfaceExpression::VarRef {
            name: "x".into(),
            escaped: false,
        };
        assert_eq!(surface_expr_tag(&expr), "Var");
    }

    #[test]
    fn test_surface_expr_tag_call() {
        let func = make_node(SurfaceExpression::VarRef {
            name: "f".into(),
            escaped: false,
        });
        let expr = SurfaceExpression::Call {
            func,
            args: vec![],
            named_args: vec![],
            implied: true,
        };
        assert_eq!(surface_expr_tag(&expr), "Call");
    }

    #[test]
    fn test_surface_expr_tag_all_variants() {
        let node = make_node(SurfaceExpression::Int(0));

        let variants: Vec<(&str, SurfaceExpression)> = vec![
            (
                "Var",
                SurfaceExpression::VarRef {
                    name: "x".into(),
                    escaped: false,
                },
            ),
            (
                "DotAccess",
                SurfaceExpression::DotAccess {
                    expr: node.clone(),
                    field: crate::ast::DotKey::Ident("foo".into()),
                },
            ),
            (
                "Pipe",
                SurfaceExpression::Pipe {
                    lhs: node.clone(),
                    rhs: node.clone(),
                },
            ),
            ("Sequential", SurfaceExpression::Sequential(vec![])),
            ("Dict", SurfaceExpression::Dict(vec![])),
            (
                "Call",
                SurfaceExpression::Call {
                    func: node.clone(),
                    args: vec![],
                    named_args: vec![],
                    implied: true,
                },
            ),
            (
                "Fn",
                SurfaceExpression::Fn {
                    return_ann: None,
                    params: vec![],
                    body: node.clone(),
                    desugared: false,
                },
            ),
            (
                "TypeAssert",
                SurfaceExpression::TypeAssert {
                    annotation: crate::ast::Spanned::new(
                        crate::ast::Annotation::Simple("Int".into()),
                        Span::origin(),
                    ),
                    expr: node.clone(),
                },
            ),
            (
                "Annotated",
                SurfaceExpression::Annotated {
                    name: "Foo".into(),
                    annotation: crate::ast::Spanned::new(
                        crate::ast::Annotation::Simple("Bar".into()),
                        Span::origin(),
                    ),
                },
            ),
            ("Rest", SurfaceExpression::Rest(None)),
            (
                "Match",
                SurfaceExpression::Match {
                    scrutinee: node.clone(),
                    arms: vec![],
                },
            ),
            ("Quote", SurfaceExpression::Quote(node.clone())),
            ("Unquote", SurfaceExpression::Unquote(node.clone())),
            (
                "UnquoteSplice",
                SurfaceExpression::UnquoteSplice(node.clone()),
            ),
            (
                "PatternDecl",
                SurfaceExpression::PatternDecl { bindings: vec![] },
            ),
            ("LetDecl", SurfaceExpression::LetDecl { bindings: vec![] }),
            (
                "CaseArm",
                SurfaceExpression::CaseArm {
                    pattern: node.clone(),
                    body: node.clone(),
                },
            ),
            (
                "TypeApp",
                SurfaceExpression::TypeApp {
                    func: node.clone(),
                    arg: node.clone(),
                },
            ),
            ("Placeholder", SurfaceExpression::Placeholder),
            ("Error", SurfaceExpression::Error(Span::origin())),
        ];

        for (expected_tag, expr) in variants {
            assert_eq!(
                surface_expr_tag(&expr),
                expected_tag,
                "tag mismatch for variant"
            );
        }
    }
}
