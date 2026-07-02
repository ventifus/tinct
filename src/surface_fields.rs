//! Surface AST field extraction for match dispatch and dot-access.
//!
//! These functions bridge the Surface AST types and the runtime value layer:
//! - `surface_node_get_field()` — field extraction for `AstNodeField` thunk evaluation
//!   and dot-access on `Expr.*` variants
//! - `surface_doc_match_view()`, `surface_program_match_view()` — match payload views
//!   for `Value::Document` and `Value::Program`
//!
//! The field extraction functions return `Value` variants. Child expression nodes are
//! returned as `Value::Variant { tag: "Expr.<Tag>", .. }` — the canonical runtime
//! representation. `Value::Document` and `Value::Program` carry their original Arc
//! wrappers for efficient sharing.

use std::sync::Arc;

use crate::ast::{
    SurfaceDeclaration, SurfaceDocument, SurfaceExpression, SurfaceNode, SurfaceProgram,
};
use crate::rust_span;

/// Extract the variant tag from a `SurfaceDeclaration` as a static string.
#[allow(dead_code)] // Used in Part E when Value::Declaration is added
pub fn surface_decl_tag(decl: &SurfaceDeclaration) -> &'static str {
    match decl {
        SurfaceDeclaration::TypeAlias { .. } => "TypeAlias",
        SurfaceDeclaration::ClassDecl { .. } => "ClassDecl",
        SurfaceDeclaration::InstanceDecl { .. } => "InstanceDecl",
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
#[allow(dead_code)] // Used in Part F when Value::Document is added
pub fn surface_doc_tag(_doc: &SurfaceDocument) -> &'static str {
    "Document"
}

/// Returns the match tag for a SurfaceProgram.
/// Always `"Program"` — used by match evaluator for Value::Program dispatch.
#[allow(dead_code)] // Used in Part F when Value::Program is added
pub fn surface_program_tag(_prog: &SurfaceProgram) -> &'static str {
    "Program"
}

// ============================================================================
// Surface node field names for each variant
// ============================================================================

// ============================================================================
// Field extraction — returns Value for field access on Expr.* variants
// ============================================================================

use crate::ast::{Annotation, DotKey, Spanned};
use crate::value::{string_val, HashableValue, Value};

/// Extract a named field from a `SurfaceNode` as a `Value`.
///
/// Primitive fields return scalar values. Expression-typed fields (child nodes) return
/// `Value::Variant { tag: "Expr.<Tag>", .. }` — the canonical runtime representation.
/// Sequence-typed fields (args, params, entries, bindings, arms) return
/// `Value::Dict(IndexMap<HashableValue::Int, ThunkId>)` — integer-keyed lists of
/// `Expr.*` variants, allocated into the EvalContext arena.
/// The `span` field returns a Dict with start_line, start_col, end_line, end_col, start_offset, end_offset fields.
/// Unrecognized fields return null.
pub fn surface_node_get_field(
    node: &Arc<SurfaceNode>,
    field: &str,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    // null sentinel — returned for absent optional fields and unrecognized fields
    let null = || Value::Dict(indexmap::IndexMap::new());
    let expr_variant =
        |n: &Arc<SurfaceNode>| crate::surface_convert::surface_node_to_expr_variant(n, ctx);

    match (&node.expr, field) {
        // span field — convert to Dict with position fields
        (_, "span") => span_to_value(&node.span, ctx),

        // --- IntLiteral ---
        (SurfaceExpression::Int(n), "value") => Value::Int(*n),

        // --- U64Literal ---
        (SurfaceExpression::U64(n), "value") => Value::U64(*n),

        // --- FloatLiteral ---
        (SurfaceExpression::Float(n), "value") => Value::Float(*n),

        // --- StrLiteral ---
        (SurfaceExpression::Str(s), "value") => string_val(s),

        // --- Var ---
        (SurfaceExpression::VarRef { name, .. }, "name") => string_val(name),
        (SurfaceExpression::VarRef { escaped, .. }, "escaped") => Value::boolean(*escaped),

        // --- DotAccess ---
        (
            SurfaceExpression::Field {
                expr: Some(inner), ..
            },
            "target",
        ) => expr_variant(inner),
        (SurfaceExpression::Field { expr: None, .. }, "target") => {
            // Leading-dot has no target expression — return null (empty dict)
            null()
        }
        (SurfaceExpression::Field { field: dot_key, .. }, "field") => {
            dot_key_to_value(dot_key, ctx)
        }

        // --- Pipe ---
        (SurfaceExpression::Pipe { lhs, .. }, "lhs") => expr_variant(lhs),
        (SurfaceExpression::Pipe { rhs, .. }, "rhs") => expr_variant(rhs),

        // --- Sequential ---
        (SurfaceExpression::Sequential(exprs), "exprs") => nodes_to_list_dict(exprs, ctx),

        // --- Dict ---
        (SurfaceExpression::Dict(entries), "entries") => surface_entries_to_list_dict(entries, ctx),

        // --- Call ---
        (SurfaceExpression::Call { func, .. }, "fn") => expr_variant(func),
        (SurfaceExpression::Call { args, .. }, "args") => nodes_to_list_dict(args, ctx),
        (SurfaceExpression::Call { named_args, .. }, "named") => {
            named_args_to_list_dict(named_args, ctx)
        }
        (SurfaceExpression::Call { implied, .. }, "implied") => Value::boolean(*implied),

        // --- Fn ---
        (SurfaceExpression::Fn { params, .. }, "params") => params_to_list_dict(params, ctx),
        (SurfaceExpression::Fn { body, .. }, "body") => expr_variant(body),
        (SurfaceExpression::Fn { return_ann, .. }, "return-ann") => {
            annotation_opt_to_value(return_ann.as_ref(), ctx)
        }
        (SurfaceExpression::Fn { desugared, .. }, "desugared") => Value::boolean(*desugared),

        // --- TypeAssert ---
        (SurfaceExpression::TypeAssert { annotation, .. }, "annotation") => {
            annotation_to_value(annotation, ctx)
        }
        (SurfaceExpression::TypeAssert { expr: inner, .. }, "expr") => expr_variant(inner),

        // --- Annotated ---
        (SurfaceExpression::Annotated { name, .. }, "name") => string_val(name),
        (SurfaceExpression::Annotated { annotation, .. }, "annotation") => {
            annotation_to_value(annotation, ctx)
        }

        // --- Rest ---
        (SurfaceExpression::Rest(Some(n), _), "name") => string_val(n),
        (SurfaceExpression::Rest(None, _), "name") => null(),

        // --- Match ---
        (SurfaceExpression::Match { scrutinee, .. }, "scrutinee") => expr_variant(scrutinee),
        (SurfaceExpression::Match { arms, .. }, "arms") => match_arms_to_list_dict(arms, ctx),

        // --- Quote / Unquote / UnquoteSplice ---
        (SurfaceExpression::Quote(inner), "expr")
        | (SurfaceExpression::Unquote(inner), "expr")
        | (SurfaceExpression::UnquoteSplice(inner), "expr") => expr_variant(inner),

        // --- PatternDecl / LetDecl ---
        (SurfaceExpression::PatternDecl { bindings }, "bindings")
        | (SurfaceExpression::LetDecl { bindings }, "bindings") => {
            nodes_to_list_dict(bindings, ctx)
        }

        // --- CaseArm ---
        (SurfaceExpression::CaseArm { let_bindings, .. }, "let_bindings") => {
            expr_variant(let_bindings)
        }
        (SurfaceExpression::CaseArm { pattern, .. }, "pattern") => expr_variant(pattern),
        (SurfaceExpression::CaseArm { body, .. }, "body") => expr_variant(body),

        // --- Decl (TypeAlias) — expose arity for type declarations ---
        // Allows generate.llt to determine how many type parameters a type has
        // without executing the declaration (which is compile-time-only).
        (SurfaceExpression::Decl(decl), "arity") => {
            if let SurfaceDeclaration::TypeAlias { params, .. } = decl.as_ref() {
                Value::Int(params.len() as i64)
            } else {
                null()
            }
        }

        // Field not applicable to this variant — return null sentinel
        _ => null(),
    }
}

/// Build an integer-keyed list Dict from a sequence of SurfaceNodes.
/// Each entry is an `Expr.*` variant, allocated into the arena.
fn nodes_to_list_dict(
    nodes: &[Arc<SurfaceNode>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    let mut map = indexmap::IndexMap::new();
    for (i, node) in nodes.iter().enumerate() {
        use crate::value::Thunk;

        let thunk = Arc::new(Thunk::new_materialized(
            crate::surface_convert::surface_node_to_expr_variant(node, ctx),
            node.span.clone(),
        ));
        let tid = ctx.alloc_thunk(thunk);
        map.insert(HashableValue::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Public wrapper for use by `surface_node_to_expr_variant` in surface_convert.rs.
pub fn match_arms_to_list_dict_pub(
    arms: &[crate::ast::SurfaceMatchArm],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    match_arms_to_list_dict(arms, ctx)
}

/// Build a list Dict from SurfaceEntry nodes (for Dict.entries field).
fn surface_entries_to_list_dict(
    entries: &[crate::ast::Spanned<crate::ast::SurfaceEntry>],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let mut map = indexmap::IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // Build Entry Variant: {key: Expr.* variant, value: Expr.* variant, span: []}
        let key_val = entry.node.key.as_ref().map_or_else(
            || Value::Dict(indexmap::IndexMap::new()),
            |k| crate::surface_convert::surface_node_to_expr_variant(k, ctx),
        );
        let val_val = crate::surface_convert::surface_node_to_expr_variant(&entry.node.value, ctx);
        // Pack as a Variant("Entry", {key: ..., value: ...})
        let mut payload = indexmap::IndexMap::new();
        payload.insert(
            HashableValue::Str("key".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                key_val,
                entry.span.clone(),
            ))),
        );
        payload.insert(
            HashableValue::Str("value".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                val_val,
                entry.span.clone(),
            ))),
        );
        let entry_variant = Value::Variant {
            tag: "Entry".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                entry.span.clone(),
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            entry_variant,
            entry.span.clone(),
        )));
        map.insert(HashableValue::Int(i as i64), tid);
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
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&na.node.name),
                na.span.clone(),
            ))),
        );
        payload.insert(
            HashableValue::Str("value".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                crate::surface_convert::surface_node_to_expr_variant(&na.node.value, ctx),
                na.span.clone(),
            ))),
        );
        let na_variant = Value::Variant {
            tag: "NamedArg".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                na.span.clone(),
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            na_variant,
            na.span.clone(),
        )));
        map.insert(HashableValue::Int(i as i64), tid);
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
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&p.node.name),
                p.span.clone(),
            ))),
        );
        payload.insert(
            HashableValue::Str("variadic".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::boolean(p.node.variadic),
                p.span.clone(),
            ))),
        );
        // Expose the parameter's type annotation text so tinct code can reconstruct
        // full signatures (e.g. "n@Int") without source text parsing.
        let ann_val = annotation_opt_to_value(p.node.annotation.as_ref(), ctx);
        payload.insert(
            HashableValue::Str("annotation".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(ann_val, p.span.clone()))),
        );
        let p_variant = Value::Variant {
            tag: "Parameter".into(),
            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(payload),
                p.span.clone(),
            )))),
        };
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(p_variant, p.span.clone())));
        map.insert(HashableValue::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Serialize a Pattern to a dict Value in the format expected by dict_to_pattern.
fn pattern_to_value(
    pat: &crate::ast::Spanned<crate::ast::Pattern>,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::ast::{LiteralPattern, Pattern};
    use crate::value::{string_val, Thunk};
    use indexmap::IndexMap;

    let span = rust_span!();
    let mk_str = |s: &str| string_val(s);
    let alloc = |v: Value| ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, span.clone())));

    let mut d: IndexMap<crate::value::HashableValue, crate::value::ThunkId> = IndexMap::new();

    match &pat.node {
        Pattern::Wildcard => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("wildcard")),
            );
        }
        Pattern::Pin(name, _) => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("variable")),
            );
            d.insert(
                crate::value::HashableValue::Str("name".into()),
                alloc(mk_str(name)),
            );
        }
        Pattern::Literal(lit) => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("literal")),
            );
            let val = match lit {
                LiteralPattern::Int(n) => Value::Int(*n),
                LiteralPattern::U64(n) => Value::U64(*n),
                LiteralPattern::Float(f) => Value::Float(*f),
                LiteralPattern::Str(s) => string_val(s),
            };
            d.insert(crate::value::HashableValue::Str("value".into()), alloc(val));
        }
        Pattern::Dict { fields, rest } => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("dict")),
            );
            let mut fields_map: IndexMap<crate::value::HashableValue, crate::value::ThunkId> =
                IndexMap::new();
            for (i, (key, sub_pat)) in fields.iter().enumerate() {
                let mut field_dict: IndexMap<crate::value::HashableValue, crate::value::ThunkId> =
                    IndexMap::new();
                field_dict.insert(
                    crate::value::HashableValue::Str("key".into()),
                    alloc(mk_str(key)),
                );
                field_dict.insert(
                    crate::value::HashableValue::Str("pattern".into()),
                    alloc(pattern_to_value(sub_pat, ctx)),
                );
                fields_map.insert(
                    crate::value::HashableValue::Int(i as i64),
                    alloc(Value::Dict(field_dict)),
                );
            }
            d.insert(
                crate::value::HashableValue::Str("fields".into()),
                alloc(Value::Dict(fields_map)),
            );
            d.insert(
                crate::value::HashableValue::Str("rest".into()),
                alloc(Value::boolean(*rest)),
            );
        }
        Pattern::Constructor { tag, binding } => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("constructor")),
            );
            d.insert(
                crate::value::HashableValue::Str("tag".into()),
                alloc(mk_str(tag)),
            );
            if let Some(sub_pat) = binding {
                d.insert(
                    crate::value::HashableValue::Str("binding".into()),
                    alloc(pattern_to_value(sub_pat, ctx)),
                );
            } else {
                d.insert(
                    crate::value::HashableValue::Str("binding".into()),
                    alloc(Value::Dict(IndexMap::new())),
                );
            }
        }
        Pattern::Or(pats) => {
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("or")),
            );
            let mut pats_map: IndexMap<crate::value::HashableValue, crate::value::ThunkId> =
                IndexMap::new();
            for (i, sub_pat) in pats.iter().enumerate() {
                pats_map.insert(
                    crate::value::HashableValue::Int(i as i64),
                    alloc(pattern_to_value(sub_pat, ctx)),
                );
            }
            d.insert(
                crate::value::HashableValue::Str("patterns".into()),
                alloc(Value::Dict(pats_map)),
            );
        }
        _ => {
            // Unsupported pattern type — store as wildcard to avoid conversion errors
            d.insert(
                crate::value::HashableValue::Str("type".into()),
                alloc(mk_str("wildcard")),
            );
        }
    }
    Value::Dict(d)
}

/// Build a list Dict from SurfaceMatchArm nodes.
fn match_arms_to_list_dict(
    arms: &[crate::ast::SurfaceMatchArm],
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let span = rust_span!();
    let mut map = indexmap::IndexMap::new();
    for (i, arm) in arms.iter().enumerate() {
        let pattern_val = pattern_to_value(&arm.pattern, ctx);
        let body_val = crate::surface_convert::surface_node_to_expr_variant(&arm.body, ctx);
        let guard_val = arm.guard.as_ref().map_or_else(
            || Value::Dict(indexmap::IndexMap::new()),
            |g| crate::surface_convert::surface_node_to_expr_variant(g, ctx),
        );
        // Arms are stored as plain Dicts (not Variants) so get_match_arm_list_field_with_aliases
        // can read them directly.
        let mut arm_dict = indexmap::IndexMap::new();
        arm_dict.insert(
            HashableValue::Str("pattern".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(pattern_val, span.clone()))),
        );
        arm_dict.insert(
            HashableValue::Str("body".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(body_val, span.clone()))),
        );
        arm_dict.insert(
            HashableValue::Str("guard".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(guard_val, span.clone()))),
        );
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(arm_dict),
            span.clone(),
        )));
        map.insert(HashableValue::Int(i as i64), tid);
    }
    Value::Dict(map)
}

/// Convert a DotKey to a Value::Variant (Ident | Index) with payload containing the actual value.
pub fn dot_key_to_value(key: &DotKey, ctx: &std::sync::Arc<crate::eval::EvalContext>) -> Value {
    use crate::value::HashableValue;
    use crate::value::{string_val, Thunk};
    use indexmap::IndexMap;
    use std::sync::Arc;

    let span = rust_span!();
    match key {
        DotKey::Ident(name) => {
            let mut payload_dict = IndexMap::new();
            payload_dict.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            Value::Variant {
                tag: "Ident".into(),
                payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(payload_dict),
                    span,
                )))),
            }
        }
        DotKey::Int(index) => {
            let mut payload_dict = IndexMap::new();
            payload_dict.insert(
                HashableValue::Str("index".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(*index),
                    span.clone(),
                ))),
            );
            Value::Variant {
                tag: "Index".into(),
                payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(payload_dict),
                    span,
                )))),
            }
        }
    }
}

/// Convert an Annotation to a Value::Variant with text content exposed.
///
/// Returns a Variant with a payload Dict that exposes the annotation's content:
///
/// - `Simple(name)`        → `[Simple  text: name]`
/// - `PropertyDict(entries)` → `[PropertyDict  text: "display"  doc: "..."  return: "..."]`
///   The `doc:` and `return:` fields are present only when those keys exist.
/// - `Annotated(name, inner)` → `[Annotated  text: "Name@Inner"  name: "Name"  inner: "Inner"]`
///
/// The `text` field always contains the Display representation.
/// This enables tinct AST-traversal code to extract annotation content (return types,
/// doc strings) without falling back to text parsing.
pub fn annotation_to_value(
    ann: &Spanned<Annotation>,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    annotation_inner_to_value(&ann.node, ann.span.clone(), ctx)
}

fn annotation_inner_to_value(
    ann: &Annotation,
    span: crate::ast::Span,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::Thunk;

    let text = ann.to_string();

    let mut payload_map = indexmap::IndexMap::new();
    payload_map.insert(
        HashableValue::Str("text".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&text),
            span.clone(),
        ))),
    );

    let tag = match ann {
        Annotation::Simple(name) => {
            payload_map.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            "Annotation.Simple"
        }
        Annotation::PropertyDict(entries) => {
            // Expose positional entries as a "parts" list (integer-keyed dict of annotation values).
            // This allows tinct code to access the structure of type annotations like @[Seq k]
            // without string parsing. Named entries (doc:, return:) are also exposed by name.
            let positional: Vec<_> = entries.iter().filter(|e| e.node.key.is_none()).collect();
            let mut parts_map = indexmap::IndexMap::new();
            for (i, pos_entry) in positional.iter().enumerate() {
                // Convert the positional entry value to an annotation-like value using its text.
                // For simple names (VarRef), expose as Simple. Otherwise expose text.
                let part_val = match &pos_entry.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => {
                        // Simple name like "Seq", "k", "union"
                        let mut p = indexmap::IndexMap::new();
                        p.insert(
                            HashableValue::Str("text".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(name),
                                pos_entry.span.clone(),
                            ))),
                        );
                        p.insert(
                            HashableValue::Str("name".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(name),
                                pos_entry.span.clone(),
                            ))),
                        );
                        Value::Variant {
                            tag: "Annotation.Simple".into(),
                            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                Value::Dict(p),
                                pos_entry.span.clone(),
                            )))),
                        }
                    }
                    SurfaceExpression::Annotated { name, annotation } => {
                        // e.g. Fn@a → Annotated
                        let inner_text = annotation.node.to_string();
                        let full_text = format!("{}@{}", name, inner_text);
                        let mut p = indexmap::IndexMap::new();
                        p.insert(
                            HashableValue::Str("text".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(&full_text),
                                pos_entry.span.clone(),
                            ))),
                        );
                        p.insert(
                            HashableValue::Str("name".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(name),
                                pos_entry.span.clone(),
                            ))),
                        );
                        p.insert(
                            HashableValue::Str("inner".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(&inner_text),
                                pos_entry.span.clone(),
                            ))),
                        );
                        Value::Variant {
                            tag: "Annotation.Annotated".into(),
                            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                Value::Dict(p),
                                pos_entry.span.clone(),
                            )))),
                        }
                    }
                    _ => {
                        // Complex expression (e.g. [Map k v] parsed as Call) — expose as text only.
                        // Uses "Annotation.Unknown" to distinguish from real PropertyDict annotations.
                        let text = pos_entry.node.value.to_string();
                        let mut p = indexmap::IndexMap::new();
                        p.insert(
                            HashableValue::Str("text".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(&text),
                                pos_entry.span.clone(),
                            ))),
                        );
                        Value::Variant {
                            tag: "Annotation.Unknown".into(),
                            payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                Value::Dict(p),
                                pos_entry.span.clone(),
                            )))),
                        }
                    }
                };
                parts_map.insert(
                    HashableValue::Int(i as i64),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        part_val,
                        pos_entry.span.clone(),
                    ))),
                );
            }
            payload_map.insert(
                HashableValue::Str("parts".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(parts_map),
                    span.clone(),
                ))),
            );
            // Also expose well-known named keys: doc: and return:
            for entry in entries {
                if let Some(key_node) = &entry.node.key {
                    if let SurfaceExpression::Str(key_name) = &key_node.expr {
                        if key_name == "doc" || key_name == "return" {
                            let clean = if let SurfaceExpression::Str(s) = &entry.node.value.expr {
                                s.clone()
                            } else {
                                entry.node.value.to_string()
                            };
                            payload_map.insert(
                                HashableValue::Str(key_name.clone().into()),
                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                    string_val(&clean),
                                    entry.span.clone(),
                                ))),
                            );
                        }
                    }
                }
            }
            "Annotation.PropertyDict"
        }
        Annotation::Annotated(name, inner) => {
            let inner_text = inner.to_string();
            payload_map.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            payload_map.insert(
                HashableValue::Str("inner".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(&inner_text),
                    span.clone(),
                ))),
            );
            "Annotation.Annotated"
        }
    };

    let payload_tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(payload_map),
        span,
    )));
    Value::Variant {
        tag: tag.into(),
        payload: Some(payload_tid),
    }
}

pub fn annotation_opt_to_value(
    ann: Option<&Spanned<Annotation>>,
    ctx: &std::sync::Arc<crate::eval::EvalContext>,
) -> Value {
    match ann {
        Some(a) => annotation_to_value(a, ctx),
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
        HashableValue::Str("start_line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            span.clone(),
        ))),
    );
    map.insert(
        HashableValue::Str("start_col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            span.clone(),
        ))),
    );
    map.insert(
        HashableValue::Str("end_line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            span.clone(),
        ))),
    );
    map.insert(
        HashableValue::Str("end_col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            span.clone(),
        ))),
    );
    map.insert(
        HashableValue::Str("start_offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            span.clone(),
        ))),
    );
    map.insert(
        HashableValue::Str("end_offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            span.clone(),
        ))),
    );

    Value::Dict(map)
}
