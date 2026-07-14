//! AST-to-dict serialization and dict-to-Surface conversion for quasiquoting, macros, and formatter.
//!
//! Bidirectional conversion between AST nodes and tinct `Value::Variant` (Expr nodes) or `Value::Dict`
//! (structural nodes) matching the canonical schema in `doc/feature/ast-schema.md`.
//!
//! The canonical runtime representation of AST nodes is `Value::Variant { tag: "Expr.<Tag>", .. }`.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{
    Annotation, DotKey, Position, Span, Spanned, Stage, SurfaceDeclaration, SurfaceDocument,
    SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceNamedArg, SurfaceNode, SurfaceParam,
    SurfaceProgram,
};
use crate::error::EvalResult;
use crate::rust_span;
use crate::value::ThunkId;
use crate::value::{string_val, HashableValue, Thunk, Value};

/// Error type for AST dict validation failures during dict-to-AST conversion.
#[derive(Debug, Clone)]
pub struct AstError {
    pub message: String,
    /// Field path for error context, e.g. ["type"] or ["args", "0", "value"].
    /// Empty vector means error at the root level.
    pub field_path: Vec<String>,
}

impl fmt::Display for AstError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.field_path.is_empty() {
            write!(f, "{}", self.message)
        } else {
            write!(
                f,
                "{} at field path: {}",
                self.message,
                self.field_path.join(".")
            )
        }
    }
}

impl std::error::Error for AstError {}

/// Convert a `SurfaceNode` to a `Value::Variant { tag: "Expr.<Tag>", payload: Some(..) }`.
///
/// This is the canonical runtime representation of AST nodes. The tag is qualified
/// (e.g. `"Expr.VarRef"`) so that pattern matching against `Expr.*` constructors works.
/// The payload is a materialized dict with the node's fields.
///
/// Delegates to the `ExprConvert`-generated `to_expr_variant` method on `SurfaceExpression`.
pub fn surface_node_to_expr_variant(
    node: &Arc<SurfaceNode>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    let val = SurfaceExpression::to_expr_variant(node, ctx);
    // Inject the node's span into the Expr.* payload so round-trips through
    // Expr.* (macro expansion, metaprogramming) preserve source positions.
    // Synthetic nodes (origin span) serialize span fields as zero, which
    // dict_to_surface_node_inner reads back as a synthetic origin span.
    inject_span_into_expr_variant(val, &node.span, ctx)
}

/// Add `span: {start: {line, col, offset}, end: {...}}` to an Expr.* variant's payload.
fn inject_span_into_expr_variant(
    val: Value,
    span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::{HashableValue, Thunk, Value};

    let synth = rust_span!(); // span for the synthetic thunks wrapping position integers

    let make_pos = |pos: &Position| -> crate::value::ThunkId {
        let mut d: IndexMap<HashableValue, crate::value::ThunkId> = IndexMap::new();
        let mk = |n: i64| {
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(n),
                synth.clone(),
            )))
        };
        d.insert(HashableValue::Str("line".into()), mk(pos.line as i64));
        d.insert(HashableValue::Str("col".into()), mk(pos.column as i64));
        d.insert(HashableValue::Str("offset".into()), mk(pos.offset as i64));
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(d),
            synth.clone(),
        )))
    };

    let span_val = {
        let mut d: IndexMap<HashableValue, crate::value::ThunkId> = IndexMap::new();
        d.insert(HashableValue::Str("start".into()), make_pos(&span.start));
        d.insert(HashableValue::Str("end".into()), make_pos(&span.end));
        Value::Dict(d)
    };

    match val {
        Value::Variant {
            tag,
            payload: Some(payload_id),
        } => {
            let payload_thunk = ctx.get_thunk(payload_id);
            if let Some(Value::Dict(mut payload_dict)) = payload_thunk.try_get_materialized() {
                let span_id =
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(span_val, synth.clone())));
                payload_dict.insert(HashableValue::Str("span".into()), span_id);
                let new_payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(payload_dict),
                    span.clone(),
                )));
                Value::Variant {
                    tag,
                    payload: Some(new_payload_id),
                }
            } else {
                Value::Variant {
                    tag,
                    payload: Some(payload_id),
                }
            }
        }
        // Unit variants (no payload) or non-variant values: pass through unchanged.
        other => other,
    }
}

/// Convert a dict representation back to a SurfaceNode.
///
/// Reads the Variant tag or `type:` field and dispatches to the native `SurfaceExpression`
/// constructor. All variants are handled natively. Unknown tags return a hard `AstError`;
/// there is no Expr-based fallback path.
///
/// `call_site_span` is used as the fallback span when the dict representation has no
/// embedded span information (e.g., for AstError nodes).
pub fn dict_to_surface_node(
    val: &Value,
    call_site_span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    dict_to_surface_node_inner(val, call_site_span, ctx)
}

/// Inner implementation of `dict_to_surface_node`.
///
/// Delegates to the `ExprConvert`-generated `from_expr_variant` method on `SurfaceExpression`.
/// All known `Expr.*` Variant tags are handled there. Unknown tags return a hard `AstError`.
fn dict_to_surface_node_inner(
    val: &Value,
    call_site_span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    // AstError is a skip variant — to_expr_variant produces a unit Expr.AstError (no payload).
    // Handle it here before from_expr_variant tries to read a non-existent payload.
    if let Value::Variant { tag, payload: None } = val {
        let stripped = if tag.starts_with("Expr.") {
            &tag[5..]
        } else {
            tag.as_str()
        };
        if stripped == "AstError" {
            return Ok(Arc::new(SurfaceNode::new(
                SurfaceExpression::Error(call_site_span.clone()),
                call_site_span.clone(),
            )));
        }
    }

    let node = SurfaceExpression::from_expr_variant(val, ctx)?;

    // Extract the span from the Expr.* payload (injected by surface_node_to_expr_variant).
    // If present, use it — it encodes the original user source position for round-trips.
    if let Value::Variant {
        payload: Some(payload_id),
        ..
    } = val
    {
        let payload_thunk = ctx.get_thunk(*payload_id);
        if let Some(Value::Dict(dict)) = payload_thunk.try_get_materialized() {
            if let Some(span) = extract_span(&dict, ctx) {
                return Ok(Arc::new(SurfaceNode::new(node.expr.clone(), span)));
            }
        }
    }

    Ok(node)
}

/// Convert a `CoreExpr` to an `Expr.*` `Value::Variant`.
///
/// This function converts a lowered `CoreExpr` back to the AST-as-data representation
/// that macros and metaprogramming tools consume. The tag is qualified (e.g. `"Expr.VarRef"`)
/// and the payload is a materialized dict with the node's fields.
///
/// **De Bruijn coordinates are dropped** — `CoreExpr::Var { level, slot, .. }` becomes
/// `Expr.VarRef { name: String(name) }`. This preserves variable names for metaprogramming
/// (e.g., quote/unquote) rather than exposing internal binding indices.
///
/// The function injects the source span from the `Spanned<CoreExpr>` into the Expr.*
/// payload, similar to `surface_node_to_expr_variant`.
pub fn core_expr_to_expr_value(
    core: &Spanned<crate::ast::CoreExpr>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::ast::CoreExpr;
    use crate::value::HashableValue;

    let synth = rust_span!(); // span for synthetic thunks wrapping primitive values

    // Helper to recursively convert child CoreExpr nodes
    let recurse = |child: &Spanned<CoreExpr>| -> ThunkId {
        let child_val = core_expr_to_expr_value(child, ctx);
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            child_val,
            child.span.clone(),
        )))
    };

    // Helper to convert a Vec of CoreExpr to a Dict (auto-indexed)
    let recurse_vec = |children: &[Arc<Spanned<CoreExpr>>]| -> ThunkId {
        let mut dict = IndexMap::new();
        for (i, child) in children.iter().enumerate() {
            let child_id = recurse(child);
            dict.insert(HashableValue::Int(i as i64), child_id);
        }
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(dict),
            synth.clone(),
        )))
    };

    // Helper to build an Expr.* variant with a payload dict
    let make_variant = |tag: &str, fields: IndexMap<HashableValue, ThunkId>| -> Value {
        make_variant_with_payload(&format!("Expr.{}", tag), fields, &core.span, ctx)
    };

    let val = match &core.node {
        // ── Literals ─────────────────────────────────────────────────────────────
        CoreExpr::Int(n) => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("int"),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(*n),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("bare".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(IndexMap::new()), // Null
                    synth.clone(),
                ))),
            );
            make_variant("Literal", payload)
        }

        CoreExpr::U64(n) => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("u64"),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(*n as i64),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("bare".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(IndexMap::new()), // Null
                    synth.clone(),
                ))),
            );
            make_variant("Literal", payload)
        }

        CoreExpr::Float(f) => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("float"),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Float(*f),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("bare".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(IndexMap::new()), // Null
                    synth.clone(),
                ))),
            );
            make_variant("Literal", payload)
        }

        CoreExpr::Str(s) => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("str"),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(s),
                    synth.clone(),
                ))),
            );
            payload.insert(
                HashableValue::Str("bare".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(IndexMap::new()), // Null
                    synth.clone(),
                ))),
            );
            make_variant("Literal", payload)
        }

        // ── Variables ────────────────────────────────────────────────────────────
        // DROP de Bruijn coordinates — only preserve the name
        CoreExpr::Var { name, .. } => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    synth.clone(),
                ))),
            );
            make_variant("VarRef", payload)
        }

        // ── Call ─────────────────────────────────────────────────────────────────
        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("fn".into()), recurse(func));
            payload.insert(HashableValue::Str("args".into()), recurse_vec(args));

            // Convert named args to Dict
            let mut named_dict = IndexMap::new();
            for (i, named_arg) in named_args.iter().enumerate() {
                let mut arg_dict = IndexMap::new();
                arg_dict.insert(
                    HashableValue::Str("name".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(&named_arg.node.name),
                        synth.clone(),
                    ))),
                );
                arg_dict.insert(
                    HashableValue::Str("value".into()),
                    recurse(&named_arg.node.value),
                );
                named_dict.insert(
                    HashableValue::Int(i as i64),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(arg_dict),
                        named_arg.span.clone(),
                    ))),
                );
            }
            payload.insert(
                HashableValue::Str("named-args".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(named_dict),
                    synth.clone(),
                ))),
            );

            payload.insert(
                HashableValue::Str("implied".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(if *implied { 1 } else { 0 }),
                    synth.clone(),
                ))),
            );

            make_variant("Call", payload)
        }

        // ── Dict ─────────────────────────────────────────────────────────────────
        CoreExpr::Dict(entries) => {
            let mut entries_dict = IndexMap::new();
            for (i, entry) in entries.iter().enumerate() {
                let mut entry_dict = IndexMap::new();
                entry_dict.insert(
                    HashableValue::Str("key".into()),
                    match &entry.node.key {
                        Some(k) => recurse(k),
                        None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(IndexMap::new()), // Null
                            synth.clone(),
                        ))),
                    },
                );
                entry_dict.insert(
                    HashableValue::Str("value".into()),
                    recurse(&entry.node.value),
                );
                entries_dict.insert(
                    HashableValue::Int(i as i64),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(entry_dict),
                        entry.span.clone(),
                    ))),
                );
            }
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("entries".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(entries_dict),
                    synth.clone(),
                ))),
            );
            make_variant("Dict", payload)
        }

        // ── Fn ───────────────────────────────────────────────────────────────────
        CoreExpr::Fn {
            params,
            body,
            return_ann,
            ..
        } => {
            let mut params_dict = IndexMap::new();
            for (i, param) in params.iter().enumerate() {
                let mut param_dict = IndexMap::new();
                param_dict.insert(
                    HashableValue::Str("name".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(&param.node.name),
                        synth.clone(),
                    ))),
                );
                param_dict.insert(
                    HashableValue::Str("annotation".into()),
                    alloc_annotation_opt(param.node.annotation.as_ref(), ctx),
                );
                param_dict.insert(
                    HashableValue::Str("variadic".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Int(if param.node.variadic { 1 } else { 0 }),
                        synth.clone(),
                    ))),
                );
                params_dict.insert(
                    HashableValue::Int(i as i64),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(param_dict),
                        param.span.clone(),
                    ))),
                );
            }

            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("params".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(params_dict),
                    synth.clone(),
                ))),
            );
            payload.insert(HashableValue::Str("body".into()), recurse(body));
            payload.insert(
                HashableValue::Str("return-ann".into()),
                alloc_annotation_opt(return_ann.as_ref(), ctx),
            );
            make_variant("Fn", payload)
        }

        // ── Sequential ───────────────────────────────────────────────────────────
        CoreExpr::Sequential(exprs) => {
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("exprs".into()), recurse_vec(exprs));
            make_variant("Sequential", payload)
        }

        // ── Match ────────────────────────────────────────────────────────────────
        CoreExpr::Match { scrutinee, arms } => {
            let mut arms_dict = IndexMap::new();
            for (i, arm) in arms.iter().enumerate() {
                // CoreMatchArm has pattern, guard, body — serialize as opaque Dict
                let mut arm_dict = IndexMap::new();
                // Pattern is Spanned<Pattern> — serialize as opaque for now
                arm_dict.insert(
                    HashableValue::Str("pattern".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()), // Opaque
                        arm.pattern.span.clone(),
                    ))),
                );
                arm_dict.insert(
                    HashableValue::Str("guard".into()),
                    match &arm.guard {
                        Some(g) => recurse(g),
                        None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(IndexMap::new()), // Null
                            synth.clone(),
                        ))),
                    },
                );
                arm_dict.insert(HashableValue::Str("body".into()), recurse(&arm.body));
                arms_dict.insert(
                    HashableValue::Int(i as i64),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(arm_dict),
                        synth.clone(),
                    ))),
                );
            }
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("scrutinee".into()), recurse(scrutinee));
            payload.insert(
                HashableValue::Str("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    synth.clone(),
                ))),
            );
            make_variant("Match", payload)
        }

        // ── Quote / Unquote / UnquoteSplice ──────────────────────────────────────
        CoreExpr::Quote(e) => {
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("Quote", payload)
        }

        CoreExpr::Unquote(e) => {
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("Unquote", payload)
        }

        CoreExpr::UnquoteSplice(e) => {
            let mut payload = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("UnquoteSplice", payload)
        }

        // ── TypeAssert ───────────────────────────────────────────────────────────
        CoreExpr::TypeAssert {
            annotation, expr, ..
        } => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("annotation".into()),
                alloc_annotation_opt(Some(annotation), ctx),
            );
            payload.insert(HashableValue::Str("expr".into()), recurse(expr));
            make_variant("TypeAssert", payload)
        }

        // ── Rest ─────────────────────────────────────────────────────────────────
        CoreExpr::Rest(name_opt) => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("name".into()),
                alloc_string_opt(name_opt.as_deref(), ctx),
            );
            make_variant("Rest", payload)
        }

        // ── LetDecl / PatternDecl ────────────────────────────────────────────────
        CoreExpr::LetDecl { bindings } => {
            let mut payload = IndexMap::new();
            let arc_bindings: Vec<Arc<Spanned<CoreExpr>>> =
                bindings.iter().map(|b| Arc::new(b.clone())).collect();
            payload.insert(
                HashableValue::Str("bindings".into()),
                recurse_vec(&arc_bindings),
            );
            make_variant("LetDecl", payload)
        }

        CoreExpr::PatternDecl { bindings } => {
            let mut payload = IndexMap::new();
            let arc_bindings: Vec<Arc<Spanned<CoreExpr>>> =
                bindings.iter().map(|b| Arc::new(b.clone())).collect();
            payload.insert(
                HashableValue::Str("bindings".into()),
                recurse_vec(&arc_bindings),
            );
            make_variant("PatternDecl", payload)
        }

        // ── CaseArm ──────────────────────────────────────────────────────────────
        CoreExpr::CaseArm {
            let_bindings,
            pattern,
            body,
        } => {
            let mut payload = IndexMap::new();
            payload.insert(
                HashableValue::Str("let-bindings".into()),
                recurse(let_bindings),
            );
            payload.insert(HashableValue::Str("pattern".into()), recurse(pattern));
            payload.insert(HashableValue::Str("body".into()), recurse(body));
            make_variant("CaseArm", payload)
        }

        // ── Variant ──────────────────────────────────────────────────────────────
        // This is AST variant construction (Expr.Variant), NOT Value::Variant itself
        CoreExpr::Variant { tag, payload } => {
            let mut fields = IndexMap::new();
            fields.insert(
                HashableValue::Str("tag".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(tag),
                    synth.clone(),
                ))),
            );
            fields.insert(
                HashableValue::Str("payload".into()),
                match payload {
                    Some(p) => recurse(p),
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()), // Null
                        synth.clone(),
                    ))),
                },
            );
            make_variant("Variant", fields)
        }

        // ── Placeholder ──────────────────────────────────────────────────────────
        CoreExpr::Placeholder => make_unit_variant("Expr.Placeholder"),
    };

    // Inject the span into the payload (like surface_node_to_expr_variant does)
    inject_span_into_expr_variant(val, &core.span, ctx)
}

/// Convert a dict to a `Spanned<SurfaceEntry>`.
///
/// Surface-native reverse for `surface_entry_to_thunk_id`.
fn dict_to_surface_entry(
    val: &Value,
    call_site_span: &Span,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceEntry>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "entry must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let key_val = get_dict_field(dict, "key", path, ctx)?;
    let key: Option<Arc<SurfaceNode>> = match &key_val {
        Value::Dict(d) if d.is_empty() => None,
        _ => Some(dict_to_surface_node(&key_val, call_site_span, ctx)?),
    };

    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = dict_to_surface_node(&value_val, call_site_span, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(|| call_site_span.clone());

    Ok(Spanned::new(SurfaceEntry { key, value }, span))
}

/// Deserialize an `Annotation` from a value produced by `annotation_to_value` or the old thunk format.
///
/// Handles two formats:
/// 1. Variant format from `annotation_to_value`: `Value::Variant { tag: "Annotation.Simple"|..., payload: Dict{text, name} }`
/// 2. Dict format with `kind` key: `Value::Dict { kind: "simple"|..., ... }`
///
/// Used by `dict_to_surface_node_inner` (Fn return annotation) and `dict_to_surface_param`.
fn dict_to_annotation(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Annotation>, AstError> {
    // Handle the Variant format produced by annotation_to_value / annotation_inner_to_value.
    // Tags: "Annotation.Simple", "Annotation.PropertyDict", "Annotation.Annotated", "Annotation.Unknown"
    if let Value::Variant {
        tag,
        payload: Some(payload_id),
    } = val
    {
        let stripped = if tag.starts_with("Annotation.") {
            &tag[11..]
        } else {
            tag.as_str()
        };
        let payload_thunk = ctx.get_thunk(*payload_id);
        let payload_val = payload_thunk
            .try_get_materialized()
            .unwrap_or_else(|| Value::Dict(indexmap::IndexMap::new()));
        let ann = match stripped {
            "Simple" => match &payload_val {
                Value::Dict(d) => {
                    let name = get_string_field(d, "name", path, ctx).unwrap_or_else(|_| {
                        get_string_field(d, "text", path, ctx).unwrap_or_default()
                    });
                    Annotation::Simple(name)
                }
                _ => Annotation::Simple(String::new()),
            },
            "PropertyDict" | "Unknown" => {
                // Reconstruct as a simple annotation using the text field
                match &payload_val {
                    Value::Dict(d) => {
                        let text = get_string_field(d, "text", path, ctx).unwrap_or_default();
                        Annotation::Simple(text)
                    }
                    _ => Annotation::Simple(String::new()),
                }
            }
            "Annotated" => match &payload_val {
                Value::Dict(d) => {
                    let name = get_string_field(d, "name", path, ctx).unwrap_or_default();
                    let inner = get_string_field(d, "inner", path, ctx).unwrap_or_default();
                    Annotation::Annotated(name, Box::new(Annotation::Simple(inner)))
                }
                _ => Annotation::Simple(String::new()),
            },
            _ => Annotation::Simple(String::new()),
        };
        return Ok(Spanned::new(ann, rust_span!()));
    }

    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "annotation must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let kind = get_string_field(dict, "kind", path, ctx)?;

    let ann = match kind.as_str() {
        "simple" => {
            let value = get_string_field(dict, "value", path, ctx)?;
            Annotation::Simple(value)
        }
        "annotated" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            let inner_val = get_dict_field(dict, "inner", path, ctx)?;
            let inner = dict_to_annotation(&inner_val, path, ctx)?;
            Annotation::Annotated(name, Box::new(inner.node))
        }
        "dict" => {
            let entries_val = get_dict_field(dict, "entries", path, ctx)?;
            let mut entries_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            entries_path.push("entries".to_string());
            let path_refs: Vec<&str> = entries_path.iter().map(|s| s.as_str()).collect();
            let entries_list = extract_list(&entries_val, &path_refs, ctx)?;
            let fallback_span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let mut entry_path = entries_path.clone();
                let i_str = i.to_string();
                entry_path.push(i_str.clone());
                let entry_path_refs: Vec<&str> = entry_path.iter().map(|s| s.as_str()).collect();
                let entry =
                    dict_to_surface_entry(&entry_val, &fallback_span, &entry_path_refs, ctx)?;
                entries.push(entry);
            }
            Annotation::PropertyDict(entries)
        }
        _ => {
            let mut kind_path = path.to_vec();
            kind_path.push("kind");
            return Err(AstError {
                message: format!("unknown annotation kind: {}", kind),
                field_path: kind_path.iter().map(|s| s.to_string()).collect(),
            });
        }
    };

    let span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());

    Ok(Spanned::new(ann, span))
}

/// Deserialize a `Spanned<Pattern>` from a dict produced by `pattern_to_thunk_id`.
///
/// Used by `dict_to_surface_node_inner` (Match arm patterns).
fn dict_to_pattern(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<crate::ast::Pattern>, AstError> {
    use crate::ast::{LiteralPattern, Pattern};

    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "pattern must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
    let kind = get_string_field(dict, "type", path, ctx)?;

    let pattern = match kind.as_str() {
        "wildcard" => Pattern::Wildcard,

        "variable" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Pattern::Pin(name, crate::ast::Resolution::new())
        }

        "pin" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Pattern::Pin(name, crate::ast::Resolution::new())
        }

        "literal" => {
            let value_val = get_field(dict, "value", path, ctx)?;
            let lit = match value_val {
                Value::Int(n) => LiteralPattern::Int(n),
                Value::U64(n) => LiteralPattern::U64(n),
                Value::Float(f) => LiteralPattern::Float(f),
                Value::String {
                    ref source,
                    start,
                    end,
                } => LiteralPattern::Str(source[start..end].to_string()),
                _ => {
                    return Err(AstError {
                        message: "literal pattern value must be Int, U64, Float, Bool, or String"
                            .into(),
                        field_path: path.iter().map(|s| s.to_string()).collect(),
                    })
                }
            };
            Pattern::Literal(lit)
        }

        "dict" => {
            let fields_val = get_dict_field(dict, "fields", path, ctx)?;
            let fields_list = extract_list(&fields_val, path, ctx)?;
            let mut fields = Vec::new();
            for (i, field_val) in fields_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let field_dict = match field_val {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: format!("dict pattern field {} must be Dict", i),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })
                    }
                };
                let key = get_string_field(&field_dict, "key", &[&i_str], ctx)?;
                let pat_val = get_dict_field(&field_dict, "pattern", &[&i_str], ctx)?;
                let mut pat_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
                pat_path.push(i_str.clone());
                pat_path.push("pattern".to_string());
                let pat_path_refs: Vec<&str> = pat_path.iter().map(|s| s.as_str()).collect();
                let spanned_pat = dict_to_pattern(&pat_val, &pat_path_refs, ctx)?;
                fields.push((key, spanned_pat));
            }
            let rest = get_bool_field(dict, "rest", path, ctx)?;
            Pattern::Dict { fields, rest }
        }

        "constructor" => {
            let tag = get_string_field(dict, "tag", path, ctx)?;
            let binding = match get_optional_dict_field(dict, "binding", ctx)? {
                Some(binding_val) if !is_empty_dict(&binding_val) => {
                    let mut binding_path: Vec<String> =
                        path.iter().map(|s| s.to_string()).collect();
                    binding_path.push("binding".to_string());
                    let binding_path_refs: Vec<&str> =
                        binding_path.iter().map(|s| s.as_str()).collect();
                    let spanned = dict_to_pattern(&binding_val, &binding_path_refs, ctx)?;
                    Some(Box::new(spanned))
                }
                _ => None,
            };
            Pattern::Constructor { tag, binding }
        }

        "or" => {
            let patterns_val = get_dict_field(dict, "patterns", path, ctx)?;
            let patterns_list = extract_list(&patterns_val, path, ctx)?;
            let mut patterns = Vec::new();
            for (i, pat_val) in patterns_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let mut pat_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
                pat_path.push(i_str);
                let pat_path_refs: Vec<&str> = pat_path.iter().map(|s| s.as_str()).collect();
                patterns.push(dict_to_pattern(&pat_val, &pat_path_refs, ctx)?);
            }
            Pattern::Or(patterns)
        }

        _ => {
            return Err(AstError {
                message: format!("unknown pattern type: {}", kind),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    Ok(Spanned::new(pattern, span))
}

// ============================================================================
// Helper functions for extracting values from dicts with error context
// ============================================================================

fn get_field(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let thunk_id = dict
        .get(&HashableValue::Str(key.into()))
        .ok_or_else(|| AstError {
            message: format!("missing required field: {}", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        })?;

    let thunk = ctx.get_thunk(*thunk_id);
    thunk.try_get_materialized().ok_or_else(|| AstError {
        message: format!("field '{}' is not materialized", key),
        field_path: path.iter().map(|s| s.to_string()).collect(),
    })
}

fn get_string_field(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<String, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(source[start..end].to_string()),
        Value::Dict(d) if d.is_empty() => {
            // Empty dict represents "null" in tinct - field access failed silently
            Err(AstError {
                message: format!(
                    "field '{}' is empty dict (null) - field access may have failed",
                    key
                ),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
        _ => Err(AstError {
            message: format!(
                "field '{}' must be String, got {} (type: {})",
                key,
                match &val {
                    Value::Dict(d) => format!("Dict with {} entries", d.len()),
                    _ => format!("{:?}", val),
                },
                val.type_name()
            ),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_bool_field(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    Ok(matches!(val, Value::Bool(true)))
}

fn get_dict_field(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Dict(_) | Value::Variant { .. } => Ok(val),
        _ => Err(AstError {
            message: format!("field '{}' must be Dict or Variant", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_optional_dict_field(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Value>, AstError> {
    match dict.get(&HashableValue::Str(key.into())) {
        Some(thunk_id) => {
            let thunk = ctx.get_thunk(*thunk_id);
            Ok(thunk.try_get_materialized())
        }
        None => Ok(None),
    }
}

fn is_empty_dict(val: &Value) -> bool {
    matches!(val, Value::Dict(d) if d.is_empty())
}

pub(crate) fn extract_span(
    dict: &IndexMap<HashableValue, ThunkId>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Span> {
    let span_thunk_id = dict.get(&HashableValue::Str("span".into()))?;
    let span_thunk = ctx.get_thunk(*span_thunk_id);
    let span_val = span_thunk.try_get_materialized()?;

    match span_val {
        Value::Dict(span_dict) => {
            let start_id = span_dict.get(&HashableValue::Str("start".into()))?;
            let start_thunk = ctx.get_thunk(*start_id);
            let start_val = start_thunk.try_get_materialized()?;

            let end_id = span_dict.get(&HashableValue::Str("end".into()))?;
            let end_thunk = ctx.get_thunk(*end_id);
            let end_val = end_thunk.try_get_materialized()?;

            let start = extract_position(&start_val, ctx)?;
            let end = extract_position(&end_val, ctx)?;

            Some(Span::new(
                start,
                end,
                std::sync::Arc::new(crate::ast::SourceFile {
                    path: std::sync::Arc::from("<surface-convert>"),
                    content: std::sync::Arc::from(""),
                }),
            ))
        }
        _ => None,
    }
}

fn extract_position(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Option<Position> {
    match val {
        Value::Dict(dict) => {
            let line_id = dict.get(&HashableValue::Str("line".into()))?;
            let line_thunk = ctx.get_thunk(*line_id);
            let line = match line_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let col_id = dict.get(&HashableValue::Str("col".into()))?;
            let col_thunk = ctx.get_thunk(*col_id);
            let column = match col_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let offset_id = dict.get(&HashableValue::Str("offset".into()))?;
            let offset_thunk = ctx.get_thunk(*offset_id);
            let offset = match offset_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            Some(Position {
                line,
                column,
                offset,
            })
        }
        _ => None,
    }
}

fn extract_list(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Value>, AstError> {
    match val {
        Value::Dict(d) => {
            let mut result = Vec::new();
            for i in 0.. {
                match d.get(&HashableValue::Int(i)) {
                    Some(thunk_id) => {
                        let thunk = ctx.get_thunk(*thunk_id);
                        let val = thunk.try_get_materialized().ok_or_else(|| AstError {
                            message: format!("list element {} is not materialized", i),
                            field_path: path.iter().map(|s| s.to_string()).collect(),
                        })?;
                        result.push(val);
                    }
                    None => break,
                }
            }
            Ok(result)
        }
        _ => Err(AstError {
            message: "expected integer-keyed Dict".into(),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

// ============================================================================
// pub(crate) helpers called by proc-macro generated code (ExprConvert derive)
// ============================================================================
//
// These are the "build" (to-expr) and "extract" (from-expr) primitives that
// the generated `to_expr_variant` / `from_expr_variant` implementations call.
// They wrap the private helpers above and expose a stable interface for the
// generated code to use.

pub(crate) fn alloc_str(s: &str, span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        string_val(s),
        span.clone(),
    )))
}

pub(crate) fn alloc_bool(b: bool, span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Int(if b { 1 } else { 0 }),
        span.clone(),
    )))
}

pub(crate) fn alloc_int(n: i64, span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Int(n),
        span.clone(),
    )))
}

pub(crate) fn alloc_u64(n: u64, span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::U64(n),
        span.clone(),
    )))
}

pub(crate) fn alloc_float(f: f64, span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Float(f),
        span.clone(),
    )))
}

pub(crate) fn alloc_expr_child(
    node: &Arc<SurfaceNode>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let val = surface_node_to_expr_variant(node, ctx);
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, node.span.clone())))
}

/// Allocate an optional child expression node.
/// `None` produces an empty dict (null) — consistent with how annotation_opt and string_opt
/// handle absent optional fields.
pub(crate) fn alloc_expr_child_opt(
    node: Option<&Arc<SurfaceNode>>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    match node {
        Some(n) => alloc_expr_child(n, ctx),
        None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            rust_span!(),
        ))),
    }
}

pub(crate) fn alloc_child_list(
    nodes: &[Arc<SurfaceNode>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let mut map = IndexMap::new();
    for (i, n) in nodes.iter().enumerate() {
        let tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            surface_node_to_expr_variant(n, ctx),
            n.span.clone(),
        )));
        map.insert(HashableValue::Int(i as i64), tid);
    }
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(map),
        rust_span!(),
    )))
}

pub(crate) fn alloc_entry_list(
    entries: &[Spanned<SurfaceEntry>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // key: Some(node) → Expr.* variant, None → null (empty dict)
        let key_val = match &entry.node.key {
            Some(key_node) => SurfaceExpression::to_expr_variant(key_node, ctx),
            None => Value::Dict(IndexMap::new()),
        };
        let key_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            key_val,
            entry.span.clone(),
        )));
        // value: Expr.* variant
        let val_val = SurfaceExpression::to_expr_variant(&entry.node.value, ctx);
        let val_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            val_val,
            entry.span.clone(),
        )));
        // Build payload dict for Expr.Entry
        let mut payload: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        payload.insert(HashableValue::Str("key".into()), key_thunk);
        payload.insert(HashableValue::Str("value".into()), val_thunk);
        let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(payload),
            entry.span.clone(),
        )));
        // Expr.Entry variant
        let entry_variant = Value::Variant {
            tag: "Expr.Entry".to_string(),
            payload: Some(payload_id),
        };
        let entry_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            entry_variant,
            entry.span.clone(),
        )));
        dict.insert(HashableValue::Int(i as i64), entry_thunk);
    }
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(dict),
        rust_span!(),
    )))
}

pub(crate) fn alloc_named_arg_list(
    args: &[Spanned<SurfaceNamedArg>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let mut na_map = IndexMap::new();
    for (i, na) in args.iter().enumerate() {
        let mut na_payload = IndexMap::new();
        na_payload.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&na.node.name),
                na.span.clone(),
            ))),
        );
        na_payload.insert(
            HashableValue::Str("value".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                surface_node_to_expr_variant(&na.node.value, ctx),
                na.span.clone(),
            ))),
        );
        let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(na_payload),
            na.span.clone(),
        )));
        let na_tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: "Expr.NamedArg".into(),
                payload: Some(payload_id),
            },
            na.span.clone(),
        )));
        na_map.insert(HashableValue::Int(i as i64), na_tid);
    }
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(na_map),
        rust_span!(),
    )))
}

pub(crate) fn alloc_param_list(
    params: &[Spanned<SurfaceParam>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let mut params_map = IndexMap::new();
    for (i, p) in params.iter().enumerate() {
        let mut p_payload = IndexMap::new();
        p_payload.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&p.node.name),
                p.span.clone(),
            ))),
        );
        p_payload.insert(
            HashableValue::Str("variadic".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(if p.node.variadic { 1 } else { 0 }),
                p.span.clone(),
            ))),
        );
        let ann_val =
            crate::surface_fields::annotation_opt_to_value(p.node.annotation.as_ref(), ctx);
        p_payload.insert(
            HashableValue::Str("annotation".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(ann_val, p.span.clone()))),
        );
        let param_payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(p_payload),
            p.span.clone(),
        )));
        let p_tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: "Expr.Param".into(),
                payload: Some(param_payload_id),
            },
            p.span.clone(),
        )));
        params_map.insert(HashableValue::Int(i as i64), p_tid);
    }
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(params_map),
        rust_span!(),
    )))
}

pub(crate) fn alloc_match_arm_list(
    arms: &[crate::ast::SurfaceMatchArm],
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let val = crate::surface_fields::match_arms_to_list_dict_pub(arms, ctx);
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, rust_span!())))
}

pub(crate) fn alloc_annotation(
    ann: &Spanned<Annotation>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let val = crate::surface_fields::annotation_to_value(ann, ctx);
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, ann.span.clone())))
}

pub(crate) fn alloc_annotation_opt(
    ann: Option<&Spanned<Annotation>>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let val = crate::surface_fields::annotation_opt_to_value(ann, ctx);
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, rust_span!())))
}

pub(crate) fn alloc_string_opt(s: Option<&str>, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    let val = match s {
        Some(name) => string_val(name),
        None => Value::Dict(IndexMap::new()),
    };
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, rust_span!())))
}

pub(crate) fn alloc_dot_key(
    key: &DotKey,
    span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> ThunkId {
    let val = match key {
        DotKey::Ident(name) => string_val(name),
        DotKey::Int(n) => string_val(&n.to_string()),
    };
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, span.clone())))
}

pub(crate) fn alloc_span(span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
    let val = crate::surface_fields::span_to_value(span, ctx);
    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, span.clone())))
}

pub(crate) fn make_variant_with_payload(
    tag: &str,
    payload: IndexMap<HashableValue, ThunkId>,
    span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    let payload_val = Value::Dict(payload);
    let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(payload_val, span.clone())));
    Value::Variant {
        tag: tag.to_string(),
        payload: Some(payload_id),
    }
}

pub(crate) fn make_unit_variant(tag: &str) -> Value {
    Value::Variant {
        tag: tag.to_string(),
        payload: None,
    }
}

pub(crate) fn make_surface_node(expr: SurfaceExpression, span: Span) -> Arc<SurfaceNode> {
    Arc::new(SurfaceNode::new(expr, span))
}

// ---- Extract helpers (from-expr direction, called by generated from_expr_variant) ----

/// Extract a (stripped_tag, dict) pair from a Value::Variant with an "Expr." prefix.
///
/// Strips the "Expr." prefix from the tag (e.g., "Expr.Sequential" → "Sequential"),
/// materializes the payload dict, and returns both. Used as the first step in
/// generated `from_expr_variant` implementations.
pub(crate) fn extract_tag_and_dict(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<(String, IndexMap<HashableValue, ThunkId>), AstError> {
    match val {
        Value::Variant { tag, payload } => {
            let stripped = if tag.starts_with("Expr.") {
                tag[5..].to_string()
            } else {
                tag.clone()
            };
            let payload_thunk_id = payload.as_ref().ok_or_else(|| AstError {
                message: format!("Expr variant {} has no payload", stripped),
                field_path: vec![],
            })?;
            let payload_val = ctx
                .get_thunk(*payload_thunk_id)
                .try_get_materialized()
                .ok_or_else(|| AstError {
                    message: "variant payload is not materialized".into(),
                    field_path: vec![],
                })?;
            match payload_val {
                Value::Dict(d) => Ok((stripped, d)),
                _ => Err(AstError {
                    message: format!(
                        "Expr variant payload must be Dict, got {}",
                        payload_val.type_name()
                    ),
                    field_path: vec![],
                }),
            }
        }
        _ => Err(AstError {
            message: "expected Expr.* Variant".into(),
            field_path: vec![],
        }),
    }
}

/// Try a primary key then each alias in order; return the first materialized value found.
fn get_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    // Try primary key first
    if let Some(thunk_id) = dict.get(&HashableValue::Str(key.into())) {
        let thunk = ctx.get_thunk(*thunk_id);
        return thunk.try_get_materialized().ok_or_else(|| AstError {
            message: format!("field '{}' is not materialized", key),
            field_path: vec![key.to_string()],
        });
    }
    // Try aliases
    for alias in aliases {
        if let Some(thunk_id) = dict.get(&HashableValue::Str((*alias).into())) {
            let thunk = ctx.get_thunk(*thunk_id);
            return thunk.try_get_materialized().ok_or_else(|| AstError {
                message: format!("field '{}' (alias '{}') is not materialized", key, alias),
                field_path: vec![key.to_string()],
            });
        }
    }
    Err(AstError {
        message: format!("missing required field: {}", key),
        field_path: vec![key.to_string()],
    })
}

pub(crate) fn get_string_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<String, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(source[start..end].to_string()),
        Value::Dict(d) if d.is_empty() => Err(AstError {
            message: format!("field '{}' is empty dict (null)", key),
            field_path: vec![key.to_string()],
        }),
        _ => Err(AstError {
            message: format!("field '{}' must be String, got {}", key, val.type_name()),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_bool_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    Ok(matches!(val, Value::Bool(true)))
}

pub(crate) fn get_int_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<i64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Int(n) => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be Int", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_u64_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<u64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::U64(n) => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be U64", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_float_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<f64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Float(f) => Ok(f),
        _ => Err(AstError {
            message: format!("field '{}' must be Float", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_child_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    let fallback_span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    dict_to_surface_node(&val, &fallback_span, ctx).map_err(|mut e| {
        e.field_path.insert(0, key.to_string());
        e
    })
}

/// Get an optional child expression node.
/// Returns `Ok(None)` when the field is absent or null (empty dict).
pub(crate) fn get_child_opt_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Arc<SurfaceNode>>, AstError> {
    let fallback_span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
    match get_field_with_aliases(dict, key, aliases, ctx) {
        Ok(val) if !is_empty_dict(&val) => dict_to_surface_node(&val, &fallback_span, ctx)
            .map(Some)
            .map_err(|mut e| {
                e.field_path.insert(0, key.to_string());
                e
            }),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

pub(crate) fn get_child_list_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Arc<SurfaceNode>>, AstError> {
    let fallback_span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    let list = extract_list(&val, &[key], ctx)?;
    list.into_iter()
        .map(|v| dict_to_surface_node(&v, &fallback_span, ctx))
        .collect::<Result<Vec<_>, _>>()
}

pub(crate) fn get_entry_list_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Spanned<SurfaceEntry>>, AstError> {
    let fallback_span = extract_span(dict, ctx).unwrap_or_else(|| rust_span!());
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    let list = extract_list(&val, &[key], ctx)?;
    let mut entries = Vec::with_capacity(list.len());
    for (i, element_val) in list.into_iter().enumerate() {
        let i_str = i.to_string();
        let (tag, payload_dict) = extract_tag_and_dict(&element_val, ctx).map_err(|mut e| {
            e.field_path.insert(0, i_str.clone());
            e.field_path.insert(0, key.to_string());
            e
        })?;
        if tag != "Entry" {
            return Err(AstError {
                message: format!("expected Expr.Entry, got Expr.{}", tag),
                field_path: vec![key.to_string(), i_str],
            });
        }
        // key field: Expr.* or null (empty dict)
        let key_thunk_id = payload_dict
            .get(&HashableValue::Str("key".into()))
            .ok_or_else(|| AstError {
                message: "Expr.Entry missing key field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "key".to_string()],
            })?;
        let key_val = ctx
            .get_thunk(*key_thunk_id)
            .try_get_materialized()
            .ok_or_else(|| AstError {
                message: "Expr.Entry key not materialized".into(),
                field_path: vec![key.to_string(), i_str.clone(), "key".to_string()],
            })?;
        let key_node = match &key_val {
            Value::Dict(d) if d.is_empty() => None,
            _ => Some(
                dict_to_surface_node(&key_val, &fallback_span, ctx).map_err(|mut e| {
                    e.field_path.insert(0, "key".to_string());
                    e.field_path.insert(0, i_str.clone());
                    e.field_path.insert(0, key.to_string());
                    e
                })?,
            ),
        };
        // value field: Expr.*
        let value_thunk_id = payload_dict
            .get(&HashableValue::Str("value".into()))
            .ok_or_else(|| AstError {
                message: "Expr.Entry missing value field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let value_val = ctx
            .get_thunk(*value_thunk_id)
            .try_get_materialized()
            .ok_or_else(|| AstError {
                message: "Expr.Entry value not materialized".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let value_node =
            dict_to_surface_node(&value_val, &fallback_span, ctx).map_err(|mut e| {
                e.field_path.insert(0, "value".to_string());
                e.field_path.insert(0, i_str.clone());
                e.field_path.insert(0, key.to_string());
                e
            })?;
        entries.push(Spanned::new(
            SurfaceEntry {
                key: key_node,
                value: value_node,
            },
            fallback_span.clone(),
        ));
    }
    Ok(entries)
}

pub(crate) fn get_named_arg_list_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Spanned<SurfaceNamedArg>>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    let list = extract_list(&val, &[key], ctx)?;
    let mut named_args = Vec::with_capacity(list.len());
    for (i, element_val) in list.into_iter().enumerate() {
        let i_str = i.to_string();
        let (tag, payload_dict) = extract_tag_and_dict(&element_val, ctx).map_err(|mut e| {
            e.field_path.insert(0, i_str.clone());
            e.field_path.insert(0, key.to_string());
            e
        })?;
        if tag != "NamedArg" {
            return Err(AstError {
                message: format!("expected Expr.NamedArg, got Expr.{}", tag),
                field_path: vec![key.to_string(), i_str],
            });
        }
        // name field: String
        let name = get_string_field(&payload_dict, "name", &[key, &i_str], ctx)?;
        // value field: Expr.*
        let value_thunk_id = payload_dict
            .get(&HashableValue::Str("value".into()))
            .ok_or_else(|| AstError {
                message: "Expr.NamedArg missing value field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let value_val = ctx
            .get_thunk(*value_thunk_id)
            .try_get_materialized()
            .ok_or_else(|| AstError {
                message: "Expr.NamedArg value not materialized".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let fallback_span = extract_span(&payload_dict, ctx).unwrap_or_else(|| rust_span!());
        let value_node =
            dict_to_surface_node(&value_val, &fallback_span, ctx).map_err(|mut e| {
                e.field_path.insert(0, "value".to_string());
                e.field_path.insert(0, i_str.clone());
                e.field_path.insert(0, key.to_string());
                e
            })?;
        named_args.push(Spanned::new(
            SurfaceNamedArg {
                name,
                value: value_node,
                annotation: None,
            },
            fallback_span,
        ));
    }
    Ok(named_args)
}

pub(crate) fn get_param_list_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Spanned<SurfaceParam>>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    let list = extract_list(&val, &[key], ctx)?;
    let mut params = Vec::with_capacity(list.len());
    for (i, element_val) in list.into_iter().enumerate() {
        let i_str = i.to_string();
        let (tag, payload_dict) = extract_tag_and_dict(&element_val, ctx).map_err(|mut e| {
            e.field_path.insert(0, i_str.clone());
            e.field_path.insert(0, key.to_string());
            e
        })?;
        if tag != "Param" {
            return Err(AstError {
                message: format!("expected Expr.Param, got Expr.{}", tag),
                field_path: vec![key.to_string(), i_str],
            });
        }
        // name field: String
        let name = get_string_field(&payload_dict, "name", &[key, &i_str], ctx)?;
        // variadic field: Bool
        let variadic = get_bool_field(&payload_dict, "variadic", &[key, &i_str], ctx)?;
        // annotation field: Expr.* or null (empty dict) → Option<Spanned<Annotation>>
        let annotation =
            get_annotation_opt_field_with_aliases(&payload_dict, "annotation", &[], ctx)?;
        params.push(Spanned::new(
            SurfaceParam {
                name,
                annotation,
                variadic,
            },
            rust_span!(),
        ));
    }
    Ok(params)
}

pub(crate) fn get_match_arm_list_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<crate::ast::SurfaceMatchArm>, AstError> {
    use crate::ast::SurfaceMatchArm;
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    let list = extract_list(&val, &[key], ctx)?;
    let mut arms = Vec::new();
    for (i, arm_val) in list.into_iter().enumerate() {
        let i_str = i.to_string();
        let arm_dict = match arm_val {
            Value::Dict(d) => d,
            _ => {
                return Err(AstError {
                    message: format!("match arm {} must be Dict", i),
                    field_path: vec![key.to_string(), i_str.clone()],
                })
            }
        };
        let arm_fallback_span = extract_span(&arm_dict, ctx).unwrap_or_else(|| rust_span!());
        let pattern_val = get_dict_field(&arm_dict, "pattern", &[key, &i_str], ctx)?;
        let pattern = dict_to_pattern(&pattern_val, &[key, &i_str, "pattern"], ctx)?;
        let guard = match get_optional_dict_field(&arm_dict, "guard", ctx)? {
            Some(guard_val) if !is_empty_dict(&guard_val) => {
                Some(dict_to_surface_node(&guard_val, &arm_fallback_span, ctx)?)
            }
            _ => None,
        };
        let body_val = get_dict_field(&arm_dict, "body", &[key, &i_str], ctx)?;
        let body = dict_to_surface_node(&body_val, &arm_fallback_span, ctx)?;
        arms.push(SurfaceMatchArm {
            pattern,
            guard,
            body,
            guard_matchable_binding: crate::ast::MatchableBinding::new(),
        });
    }
    Ok(arms)
}

pub(crate) fn get_annotation_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Annotation>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    dict_to_annotation(&val, &[key], ctx)
}

pub(crate) fn get_annotation_opt_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Spanned<Annotation>>, AstError> {
    match get_field_with_aliases(dict, key, aliases, ctx) {
        Ok(val) if !is_empty_dict(&val) => dict_to_annotation(&val, &[key], ctx).map(Some),
        Ok(_) => Ok(None),
        Err(_) => Ok(None),
    }
}

pub(crate) fn get_string_opt_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<String>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Dict(d) if d.is_empty() => Ok(None),
        Value::String {
            ref source,
            start,
            end,
        } => Ok(Some(source[start..end].to_string())),
        _ => Err(AstError {
            message: format!("field '{}' must be String or empty dict", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_dot_key_field_with_aliases(
    dict: &IndexMap<HashableValue, ThunkId>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<DotKey, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => {
            let s = source[start..end].to_string();
            // Try to parse as integer first, fall back to Ident
            if let Ok(n) = s.parse::<i64>() {
                Ok(DotKey::Int(n))
            } else {
                Ok(DotKey::Ident(s))
            }
        }
        Value::Int(n) => Ok(DotKey::Int(n)),
        _ => Err(AstError {
            message: format!("field '{}' must be String or Int for DotKey", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_span_from_dict(
    dict: &IndexMap<HashableValue, ThunkId>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Span {
    extract_span(dict, ctx).unwrap_or_else(|| rust_span!())
}

// ============================================================================
// Surface AST to Dict Functions (AST → Value conversion)
// ============================================================================

/// Options controlling AST-to-dict output.
#[derive(Default, Clone)]
pub struct AstToDictOpts<'a> {
    /// Source text — enables `bare:` flag on string literals.
    /// None → bare is always false (safe default for generated code).
    pub source: Option<&'a str>,
    /// Comment maps from ParseOutput — enables leading-comments, trailing-comment,
    /// and blank-before fields on Entry and Document nodes.
    /// None → no comment fields emitted (compact formatter, quasiquoting).
    pub comments: Option<CommentMaps<'a>>,
}

/// Comment and blank-line metadata from ParseOutput.
#[derive(Clone)]
pub struct CommentMaps<'a> {
    pub leading_comments: &'a std::collections::BTreeMap<usize, Vec<String>>,
    pub trailing_comments: &'a std::collections::BTreeMap<usize, String>,
    pub blank_before: &'a std::collections::BTreeMap<usize, bool>,
}

// ============================================================================
// Surface AST to Dict conversion functions (AST → ThunkId)
// ============================================================================
//
// These functions convert Surface AST nodes to their dict representation as ThunkIds.
// The reverse direction (dict → Surface AST) is handled by the dict_to_* functions above.

/// Convert a SurfaceProgram to a dict representation.
///
/// Phase 4 native path: iterates `program.documents` directly, emitting each document
/// via `surface_document_to_thunk_id`. No longer bridges through `ast_convert`.
/// Schema matches the canonical AST schema in `doc/feature/ast-schema.md` — existing
/// tinct metaprogramming code sees no change.
pub fn surface_program_to_dict(
    program: &SurfaceProgram,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = program
        .documents
        .first()
        .map(|d| d.span.clone())
        .unwrap_or_else(|| rust_span!());
    let mut root = IndexMap::new();

    root.insert(
        HashableValue::Str("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("file"),
            span.clone(),
        ))),
    );

    root.insert(
        HashableValue::Str("schema-version".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(1),
            span.clone(),
        ))),
    );

    // documents: list of document dicts
    let docs: Vec<_> = program
        .documents
        .iter()
        .map(|doc| surface_document_to_thunk_id(&doc.node, doc.span.clone(), opts, ctx))
        .collect::<EvalResult<Vec<_>>>()?;

    root.insert(
        HashableValue::Str("documents".into()),
        list_to_thunk_id(docs.into_iter(), span.clone(), ctx)?,
    );

    root.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );

    Ok(Arc::new(Thunk::new_materialized(Value::Dict(root), span)))
}

fn span_to_thunk_id(span: Span, ctx: &Arc<crate::eval::EvalContext>) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    // start position
    let mut start_dict = IndexMap::new();
    start_dict.insert(
        HashableValue::Str("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            span.clone(),
        ))),
    );
    start_dict.insert(
        HashableValue::Str("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            span.clone(),
        ))),
    );
    start_dict.insert(
        HashableValue::Str("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            span.clone(),
        ))),
    );

    // end position
    let mut end_dict = IndexMap::new();
    end_dict.insert(
        HashableValue::Str("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            span.clone(),
        ))),
    );
    end_dict.insert(
        HashableValue::Str("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            span.clone(),
        ))),
    );
    end_dict.insert(
        HashableValue::Str("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            span.clone(),
        ))),
    );

    dict.insert(
        HashableValue::Str("start".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(start_dict),
            span.clone(),
        ))),
    );
    dict.insert(
        HashableValue::Str("end".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(end_dict),
            span.clone(),
        ))),
    );

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a Vec<ThunkId> to a dict-based list (auto-indexed dict with integer keys).
pub(crate) fn list_to_thunk_id(
    items: impl ExactSizeIterator<Item = ThunkId>,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::with_capacity(items.len());
    for (i, item) in items.enumerate() {
        dict.insert(HashableValue::Int(i as i64), item);
    }
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

pub(crate) fn annotation_to_thunk_id(
    ann: &Annotation,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        HashableValue::Str("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("annotation"),
            span.clone(),
        ))),
    );

    match ann {
        Annotation::Simple(name) => {
            dict.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("simple"),
                    span.clone(),
                ))),
            );
            dict.insert(
                HashableValue::Str("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
        }
        Annotation::Annotated(name, inner) => {
            dict.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("annotated"),
                    span.clone(),
                ))),
            );
            dict.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                HashableValue::Str("inner".into()),
                annotation_to_thunk_id(inner, span.clone(), ctx)?,
            );
        }
        Annotation::PropertyDict(entries) => {
            dict.insert(
                HashableValue::Str("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("dict"),
                    span.clone(),
                ))),
            );

            // Convert entries to thunk IDs - these are annotation entries (simpler than regular entries)
            let entry_ids: Vec<_> = entries
                .iter()
                .map(|e| {
                    let mut entry_dict = IndexMap::new();
                    entry_dict.insert(
                        HashableValue::Str("type".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val("entry"),
                            e.span.clone(),
                        ))),
                    );

                    // For annotation dicts, keys are always string literals (bare words).
                    // SurfaceEntry.key is Arc<SurfaceNode>; SurfaceEntry.value is Arc<SurfaceNode>.
                    let key_id = match &e.node.key {
                        Some(k) => match &k.expr {
                            crate::ast::SurfaceExpression::Str(s) => ctx.alloc_thunk(Arc::new(
                                Thunk::new_materialized(string_val(s), k.span.clone()),
                            )),
                            _ => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                Value::Dict(IndexMap::new()),
                                k.span.clone(),
                            ))),
                        },
                        None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(IndexMap::new()),
                            e.span.clone(),
                        ))),
                    };

                    entry_dict.insert(HashableValue::Str("key".into()), key_id);

                    // Annotation entry values are strings/ints for simple cases,
                    // or full AST dicts for compound values like [a: Numeric] or Seq@Int.
                    let value_id = match &e.node.value.expr {
                        crate::ast::SurfaceExpression::Str(s) => ctx.alloc_thunk(Arc::new(
                            Thunk::new_materialized(string_val(s), e.node.value.span.clone()),
                        )),
                        crate::ast::SurfaceExpression::Int(n) => ctx.alloc_thunk(Arc::new(
                            Thunk::new_materialized(Value::Int(*n), e.node.value.span.clone()),
                        )),
                        crate::ast::SurfaceExpression::U64(n) => ctx.alloc_thunk(Arc::new(
                            Thunk::new_materialized(Value::U64(*n), e.node.value.span.clone()),
                        )),
                        _ => {
                            surface_node_to_thunk_id(&e.node.value, &AstToDictOpts::default(), ctx)?
                        }
                    };

                    entry_dict.insert(HashableValue::Str("value".into()), value_id);
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(entry_dict),
                        e.span.clone(),
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                HashableValue::Str("entries".into()),
                list_to_thunk_id(entry_ids.into_iter(), span.clone(), ctx)?,
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a `SurfaceDocument` to a ThunkId containing its dict representation.
///
/// Phase 3 native path: iterates `doc.items` directly (instead of `doc.expressions`),
/// dispatching on `SurfaceItem::Expr` → `surface_node_to_thunk_id` and
/// `SurfaceItem::Decl` → `surface_decl_to_thunk_id`. The emitted schema is identical
/// to the old `document_to_dict` function — all fields and their types are unchanged.
fn surface_document_to_thunk_id(
    doc: &SurfaceDocument,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        HashableValue::Str("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("document"),
            span.clone(),
        ))),
    );

    // expressions: list of expression/declaration dicts (all SurfaceItems, both Expr and Decl)
    let item_ids: Vec<_> = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node) => surface_node_to_thunk_id(node, opts, ctx),
            SurfaceItem::Decl(decl) => {
                surface_decl_to_thunk_id(&decl.node, decl.span.clone(), opts, ctx)
            }
        })
        .collect::<EvalResult<Vec<_>>>()?;

    dict.insert(
        HashableValue::Str("expressions".into()),
        list_to_thunk_id(item_ids.into_iter(), span.clone(), ctx)?,
    );

    // name: string or []
    dict.insert(
        HashableValue::Str("name".into()),
        match &doc.name {
            Some(s) => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(s),
                span.clone(),
            ))),
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );

    // output-type: annotation or []
    dict.insert(
        HashableValue::Str("output-type".into()),
        match &doc.output_type {
            Some(a) => annotation_to_thunk_id(&a.node, span.clone(), ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );

    // expects: annotation or []
    dict.insert(
        HashableValue::Str("expects".into()),
        match &doc.expects {
            Some(a) => annotation_to_thunk_id(&a.node, span.clone(), ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );

    // stage: DocStage.Type | DocStage.Runtime — nominal variant based on document stage annotation
    let stage_tag = match &doc.stage {
        Some(Stage::Type) => "DocStage.Type",
        Some(Stage::Runtime) | None => "DocStage.Runtime",
    };
    dict.insert(
        HashableValue::Str("stage".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: stage_tag.to_string(),
                payload: None,
            },
            span.clone(),
        ))),
    );

    // leading-comments: absent when None or empty
    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&span.start.offset) {
            if !comments.is_empty() {
                let comment_ids: Vec<ThunkId> = comments
                    .iter()
                    .map(|c| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(c),
                            span.clone(),
                        )))
                    })
                    .collect();
                dict.insert(
                    HashableValue::Str("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span.clone(), ctx)?,
                );
            }
        }
    }

    dict.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a `SurfaceDeclaration` to a ThunkId containing its dict representation.
///
/// This is the surface-native handler for Group B variants (compile-time-only declaration
/// forms that moved from `SurfaceExpression` to `SurfaceDeclaration`). Schema (Variant tags,
/// key names) is identical to the old Expr-based emitter — existing tinct metaprogramming
/// code sees no change.
fn surface_decl_to_thunk_id(
    decl: &SurfaceDeclaration,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    let variant_tag: &str;

    match decl {
        SurfaceDeclaration::TypeAlias { params, body } => {
            variant_tag = "TypeAlias";
            if !params.is_empty() {
                let params_thunk_ids: Vec<ThunkId> = params
                    .iter()
                    .map(|(name, _ann)| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(name),
                            span.clone(),
                        )))
                    })
                    .collect();
                dict.insert(
                    HashableValue::Str("params".into()),
                    list_to_thunk_id(params_thunk_ids.into_iter(), span.clone(), ctx)?,
                );
            }
            dict.insert(
                HashableValue::Str("expr".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceDeclaration::ClassDecl {
            name,
            params,
            superclasses,
            methods,
            determines,
            resolver,
            resolver_injective,
            structural: _,
        } => {
            variant_tag = "ClassDecl";
            dict.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            // params: integer-keyed list of param name strings
            let params_dict: IndexMap<HashableValue, ThunkId> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        HashableValue::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(p),
                            span.clone(),
                        ))),
                    )
                })
                .collect();
            dict.insert(
                HashableValue::Str("params".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(params_dict),
                    span.clone(),
                ))),
            );
            // superclasses: Seq of [class-name, param1, param2, ...] seqs
            // Only emitted when non-empty (e.g. [class Ord a where Eq a] → [[Eq a]])
            if !superclasses.is_empty() {
                let pair_thunk_ids: Vec<ThunkId> = superclasses
                    .iter()
                    .map(|(class_name, var_names)| {
                        let mut entries: Vec<(HashableValue, ThunkId)> = vec![(
                            HashableValue::Int(0),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(class_name),
                                span.clone(),
                            ))),
                        )];
                        for (i, var_name) in var_names.iter().enumerate() {
                            entries.push((
                                HashableValue::Int((i + 1) as i64),
                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                    string_val(var_name),
                                    span.clone(),
                                ))),
                            ));
                        }
                        let inner: IndexMap<HashableValue, ThunkId> = entries.into_iter().collect();
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(inner),
                            span.clone(),
                        )))
                    })
                    .collect();
                dict.insert(
                    HashableValue::Str("superclasses".into()),
                    list_to_thunk_id(pair_thunk_ids.into_iter(), span.clone(), ctx)?,
                );
            }
            // methods: string-keyed dict of method expression dicts
            // Keys are SurfaceExpression::Str bare words; values are the full entry value nodes.
            let methods_dict: IndexMap<HashableValue, ThunkId> = methods
                .iter()
                .filter_map(|method| {
                    method.node.key.as_ref().and_then(|key| {
                        if let SurfaceExpression::Str(key_str) = &key.expr {
                            Some((
                                HashableValue::Str(Rc::from(key_str.as_str())),
                                surface_node_to_thunk_id(&method.node.value, opts, ctx).ok()?,
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            dict.insert(
                HashableValue::Str("methods".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(methods_dict),
                    span.clone(),
                ))),
            );
            // determines: optional integer-keyed list of expression dicts
            if !determines.is_empty() {
                let determines_dict: IndexMap<HashableValue, ThunkId> = determines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, fd_node)| {
                        Some((
                            HashableValue::Int(i as i64),
                            surface_node_to_thunk_id(fd_node, opts, ctx).ok()?,
                        ))
                    })
                    .collect();
                dict.insert(
                    HashableValue::Str("determines".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(determines_dict),
                        span.clone(),
                    ))),
                );
            }
            // resolver: optional expression dict
            if let Some(resolver_node) = resolver {
                dict.insert(
                    HashableValue::Str("resolver".into()),
                    surface_node_to_thunk_id(resolver_node, opts, ctx)?,
                );
            }
            // injective: optional bool (only emitted when true)
            if *resolver_injective {
                dict.insert(
                    HashableValue::Str("injective".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Int(1),
                        span.clone(),
                    ))),
                );
            }
        }

        SurfaceDeclaration::InstanceDecl { class_name, arms } => {
            variant_tag = "InstanceDecl";
            dict.insert(
                HashableValue::Str("class".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(class_name),
                    span.clone(),
                ))),
            );
            // arms: integer-keyed list of {pattern, methods} dicts
            let arms_dict: IndexMap<HashableValue, ThunkId> = arms
                .iter()
                .enumerate()
                .filter_map(|(i, (pattern_node, methods))| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        HashableValue::Str("pattern".into()),
                        surface_node_to_thunk_id(pattern_node, opts, ctx).ok()?,
                    );
                    // methods: string-keyed dict matching ClassDecl.methods format
                    let methods_dict: IndexMap<HashableValue, ThunkId> = methods
                        .iter()
                        .filter_map(|method| {
                            method.node.key.as_ref().and_then(|key| {
                                if let SurfaceExpression::Str(key_str) = &key.expr {
                                    Some((
                                        HashableValue::Str(Rc::from(key_str.as_str())),
                                        surface_node_to_thunk_id(&method.node.value, opts, ctx)
                                            .ok()?,
                                    ))
                                } else {
                                    None
                                }
                            })
                        })
                        .collect();
                    arm_dict.insert(
                        HashableValue::Str("methods".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(methods_dict),
                            span.clone(),
                        ))),
                    );
                    Some((
                        HashableValue::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(arm_dict),
                            span.clone(),
                        ))),
                    ))
                })
                .collect();
            dict.insert(
                HashableValue::Str("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span.clone(),
                ))),
            );
        }

        SurfaceDeclaration::SyntaxClass {
            name,
            pattern,
            message,
        } => {
            variant_tag = "SyntaxClass";
            dict.insert(
                HashableValue::Str("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                HashableValue::Str("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            if let Some(msg) = message {
                dict.insert(
                    HashableValue::Str("message".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(msg),
                        span.clone(),
                    ))),
                );
            }
        }

        SurfaceDeclaration::Splice(forms) => {
            variant_tag = "Splice";
            let mut form_list = Vec::new();
            for form in forms {
                form_list.push(surface_node_to_thunk_id(form, opts, ctx)?);
            }
            dict.insert(
                HashableValue::Str("forms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(
                        form_list
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| (HashableValue::Int(i as i64), v))
                            .collect(),
                    ),
                    span.clone(),
                ))),
            );
        }
    }

    dict.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );
    let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(dict),
        span.clone(),
    )));
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Variant {
            tag: variant_tag.to_string(),
            payload: Some(payload_id),
        },
        span,
    ))))
}

/// Override the `bare` field in an `Expr.Literal` (kind: "str") variant's payload.
///
/// The `inject(bare = true)` attribute on `SurfaceExpression::Str` always generates
/// `bare: true`. This function corrects the value based on the actual source context:
/// - `bare: true` when the string was a bare word (no quotes) in source
/// - `bare: false` when quoted or when source is unavailable
///
/// Returns the modified value, or the original value unchanged if it is not an
/// `Expr.Literal` with a dict payload.
fn override_bare_in_literal_variant(
    val: Value,
    bare: bool,
    span: &Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    if let Value::Variant {
        ref tag,
        payload: Some(payload_id),
    } = val
    {
        if tag == "Expr.Literal" {
            let payload_thunk = ctx.get_thunk(payload_id);
            if let Some(Value::Dict(mut dict)) = payload_thunk.try_get_materialized() {
                let new_bare_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(if bare { 1 } else { 0 }),
                    span.clone(),
                )));
                dict.insert(HashableValue::Str("bare".into()), new_bare_id);
                let new_payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(dict),
                    span.clone(),
                )));
                return Value::Variant {
                    tag: tag.clone(),
                    payload: Some(new_payload_id),
                };
            }
        }
    }
    val
}

/// Like `alloc_entry_list`, but uses opts to:
/// - Correct the `bare` flag on string-key literals (bare word vs quoted)
/// - Add `leading-comments` and `blank-before` fields to entry payloads
///
/// Called from `surface_node_to_thunk_id` for Dict nodes when opts is available,
/// replacing the derived `alloc_entry_list` which has no access to opts.
fn alloc_entry_list_with_opts(
    entries: &[Spanned<SurfaceEntry>],
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // key: Some(node) → Expr.* variant with corrected bare flag, None → null
        let key_val = match &entry.node.key {
            Some(key_node) => {
                let mut val = SurfaceExpression::to_expr_variant(key_node, ctx);
                // Override bare for string literal keys: check source text at span offset.
                if let SurfaceExpression::Str(_) = &key_node.expr {
                    let is_bare = match opts.source {
                        Some(source) => {
                            // Span starts at the opening quote for quoted strings,
                            // or at the first identifier char for bare words.
                            // Check the character AT the span start.
                            let offset = key_node.span.start.offset;
                            source.as_bytes().get(offset) != Some(&b'"')
                        }
                        None => false,
                    };
                    val = override_bare_in_literal_variant(val, is_bare, &key_node.span, ctx);
                }
                val
            }
            None => Value::Dict(IndexMap::new()),
        };
        let key_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            key_val,
            entry.span.clone(),
        )));

        // value: Expr.* variant
        let val_val = SurfaceExpression::to_expr_variant(&entry.node.value, ctx);
        let val_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            val_val,
            entry.span.clone(),
        )));

        // Build payload dict for Expr.Entry
        let mut payload: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        payload.insert(HashableValue::Str("key".into()), key_thunk);
        payload.insert(HashableValue::Str("value".into()), val_thunk);

        // Add comment fields when comment maps are provided
        if let Some(ref comment_maps) = opts.comments {
            let offset = entry.span.start.offset;

            // leading-comments: list of comment strings before this entry
            if let Some(comments) = comment_maps.leading_comments.get(&offset) {
                if !comments.is_empty() {
                    let comment_ids: Vec<ThunkId> = comments
                        .iter()
                        .map(|c| {
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(c),
                                entry.span.clone(),
                            )))
                        })
                        .collect();
                    let comments_tid =
                        list_to_thunk_id(comment_ids.into_iter(), entry.span.clone(), ctx)?;
                    payload.insert(HashableValue::Str("leading-comments".into()), comments_tid);
                }
            }

            // blank-before: true when there is a blank line before this entry
            let is_blank = comment_maps.blank_before.get(&offset) == Some(&true);
            let blank_tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(if is_blank { 1 } else { 0 }),
                entry.span.clone(),
            )));
            payload.insert(HashableValue::Str("blank-before".into()), blank_tid);
        }

        // When no comment maps, always include blank-before: false as default
        if opts.comments.is_none() {
            let blank_tid = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(0),
                entry.span.clone(),
            )));
            payload.insert(HashableValue::Str("blank-before".into()), blank_tid);
        }

        let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(payload),
            entry.span.clone(),
        )));
        // Expr.Entry variant
        let entry_variant = Value::Variant {
            tag: "Expr.Entry".to_string(),
            payload: Some(payload_id),
        };
        let entry_thunk = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            entry_variant,
            entry.span.clone(),
        )));
        dict.insert(HashableValue::Int(i as i64), entry_thunk);
    }
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
        Value::Dict(dict),
        rust_span!(),
    ))))
}

/// Convert a SurfaceNode to a ThunkId containing its `Expr.*` variant representation.
/// Uses `surface_node_to_expr_variant` — produces `Expr.*` variants consumable by `builtin-eval`.
///
/// For Dict expressions, uses `alloc_entry_list_with_opts` when opts is provided to
/// correctly handle the `bare` flag on string keys and add comment metadata to entries.
fn surface_node_to_thunk_id(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    // For Dict nodes, build the Expr.Dict variant with opts-aware entry conversion
    // so that bare flags and comment fields are correctly populated.
    let val = if let SurfaceExpression::Dict(entries) = &node.expr {
        let entries_tid = alloc_entry_list_with_opts(entries, opts, ctx)?;
        let mut payload: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        payload.insert(HashableValue::Str("entries".into()), entries_tid);
        let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(payload),
            node.span.clone(),
        )));
        inject_span_into_expr_variant(
            Value::Variant {
                tag: "Expr.Dict".to_string(),
                payload: Some(payload_id),
            },
            &node.span,
            ctx,
        )
    } else {
        surface_node_to_expr_variant(node, ctx)
    };
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, node.span.clone()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new(base_dir, false)
    }

    /// Peel a `Value::Variant` to its payload dict.
    /// Panics with a helpful message if the value is not a Variant with a Dict payload.
    fn peel_variant(
        val: Value,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> (String, IndexMap<HashableValue, ThunkId>) {
        match val {
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } => {
                let payload_thunk = ctx.get_thunk(payload_id);
                match payload_thunk.try_get_materialized() {
                    Some(Value::Dict(map)) => (tag, map),
                    other => panic!("expected Dict payload for Variant, got {:?}", other),
                }
            }
            other => panic!("expected Variant, got {:?}", other),
        }
    }

    #[test]
    fn test_surface_program_to_dict_file_schema_version() {
        use crate::parser::parse;

        let parse_output = parse("1").unwrap();
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        match thunk.try_get_materialized() {
            Some(Value::Dict(map)) => {
                let type_id = map.get(&HashableValue::Str("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(type_thunk.try_get_materialized(), Some(string_val("file")));

                let version_id = map
                    .get(&HashableValue::Str("schema-version".into()))
                    .unwrap();
                let version_thunk = ctx.get_thunk(*version_id);
                assert_eq!(version_thunk.try_get_materialized(), Some(Value::Int(1)));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_bare_flag_on_bare_word_strings() {
        use crate::parser::parse;

        // Parse "[foo: 1]" — the key "foo" should have bare: true
        let input = "[foo: 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the first document's first expression (the dict)
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&HashableValue::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            // Get the entries list
                                            let entries_id = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    let entry_val = entry_thunk
                                                        .try_get_materialized()
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Get the key expression
                                                        let key_id = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap();
                                                        let key_thunk = ctx.get_thunk(*key_id);
                                                        let key_val = key_thunk
                                                            .try_get_materialized()
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        // Check bare: true
                                                        let bare_id = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing");
                                                        let bare_thunk = ctx.get_thunk(*bare_id);
                                                        assert_eq!(
                                                            bare_thunk
                                                                .try_get_materialized(),
                                                            Some(Value::Int(1)),
                                                            "bare should be true for bare word 'foo'"
                                                        );
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_bare_flag_on_quoted_strings() {
        use crate::parser::parse;

        // Parse "[\"foo\": 1]" — the key "foo" should have bare: false
        let input = "[\"foo\": 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&HashableValue::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    let entry_val = entry_thunk
                                                        .try_get_materialized()
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        let key_id = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap();
                                                        let key_thunk = ctx.get_thunk(*key_id);
                                                        let key_val = key_thunk
                                                            .try_get_materialized()
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        let bare_id = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing");
                                                        let bare_thunk = ctx.get_thunk(*bare_id);
                                                        assert_eq!(
                                                            bare_thunk
                                                                .try_get_materialized(),
                                                            Some(Value::Int(0)),
                                                            "bare should be false for quoted string \"foo\""
                                                        );
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_comment_embedding() {
        use crate::parser::parse;

        // Parse "[# comment\nx: 1]" — the entry should have leading-comments: [" comment"]
        let input = "[# comment\nx: 1]";
        let parse_output = parse(input).unwrap();
        let comment_maps = CommentMaps {
            leading_comments: &parse_output.leading_comments,
            trailing_comments: &parse_output.trailing_comments,
            blank_before: &parse_output.blank_before,
        };
        let opts = AstToDictOpts {
            source: Some(input),
            comments: Some(comment_maps),
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the entry and check for leading-comments
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&HashableValue::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    let entry_val = entry_thunk
                                                        .try_get_materialized()
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Check for leading-comments field
                                                        let comments_id = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "leading-comments".into(),
                                                            ))
                                                            .expect(
                                                                "leading-comments field missing",
                                                            );
                                                        let comments_thunk =
                                                            ctx.get_thunk(*comments_id);
                                                        match comments_thunk.try_get_materialized()
                                                        {
                                                            Some(Value::Dict(comments_list)) => {
                                                                let comment_id = comments_list
                                                                    .get(&HashableValue::Int(0))
                                                                    .expect("comment 0 missing");
                                                                let comment_thunk =
                                                                    ctx.get_thunk(*comment_id);
                                                                assert_eq!(
                                                                    comment_thunk
                                                                        .try_get_materialized(),
                                                                    Some(string_val(" comment")),
                                                                    "leading comment should be ' comment'"
                                                                );
                                                            }
                                                            _ => panic!(
                                                                "expected Dict for comments list"
                                                            ),
                                                        }
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_blank_before_flag() {
        use crate::parser::parse;
        use std::collections::BTreeMap;

        // Manually inject blank-before data to test the ast_dict lookup.
        // The parser's main loop does not track blank lines between dict entries
        // (skip_whitespace_tokens handles that in specific call sites), so we
        // construct the blank_before map by hand.
        let input = "[a: 1\nb: 2]";
        let parse_output = parse(input).unwrap();

        // Find the offset of 'b' (the second entry's key).
        // In "[a: 1\nb: 2]": [ at 0, a at 1, : at 2, ' ' at 3, 1 at 4, \n at 5, b at 6
        let mut blank_before_map = BTreeMap::new();
        blank_before_map.insert(6usize, true); // mark 'b' as having a blank line before it
        let comment_maps = CommentMaps {
            leading_comments: &parse_output.leading_comments,
            trailing_comments: &parse_output.trailing_comments,
            blank_before: &blank_before_map,
        };
        let opts = AstToDictOpts {
            source: Some(input),
            comments: Some(comment_maps),
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the second entry and check blank-before: true
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&HashableValue::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id = entries_list
                                                        .get(&HashableValue::Int(1))
                                                        .unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    let entry_val = entry_thunk
                                                        .try_get_materialized()
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Check blank-before: true
                                                        let blank_id = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "blank-before".into(),
                                                            ))
                                                            .expect("blank-before field missing");
                                                        let blank_thunk = ctx.get_thunk(*blank_id);
                                                        assert_eq!(
                                                            blank_thunk
                                                                .try_get_materialized(),
                                                            Some(Value::Int(1)),
                                                            "blank-before should be true for second entry"
                                                        );
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }

    #[test]
    fn test_both_none_mode_unchanged() {
        use crate::parser::parse;

        // Parse "[foo: 1]" with both source and comments None
        let input = "[foo: 1]";
        let parse_output = parse(input).unwrap();
        let opts = AstToDictOpts {
            source: None,
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false (default when source is None)
        match thunk.try_get_materialized() {
            Some(Value::Dict(file_dict)) => {
                let docs_id = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&HashableValue::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    let entry_val = entry_thunk
                                                        .try_get_materialized()
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        let key_id = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap();
                                                        let key_thunk = ctx.get_thunk(*key_id);
                                                        let key_val = key_thunk
                                                            .try_get_materialized()
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        let bare_id = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing");
                                                        let bare_thunk = ctx.get_thunk(*bare_id);
                                                        assert_eq!(
                                                            bare_thunk
                                                                .try_get_materialized(),
                                                            Some(Value::Int(0)),
                                                            "bare should be false when source is None"
                                                        );

                                                        // Check that blank-before is still present (always included)
                                                        let blank_id = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "blank-before".into(),
                                                            ))
                                                            .expect("blank-before field missing");
                                                        let blank_thunk = ctx.get_thunk(*blank_id);
                                                        assert_eq!(
                                                            blank_thunk
                                                                .try_get_materialized(),
                                                            Some(Value::Int(0)),
                                                            "blank-before should be false when comments is None"
                                                        );

                                                        // Check that leading-comments is absent
                                                        assert!(
                                                            entry_dict
                                                                .get(&HashableValue::Str(
                                                                    "leading-comments".into()
                                                                ))
                                                                .is_none(),
                                                            "leading-comments should be absent when comments is None"
                                                        );
                                                    }
                                                }
                                                _ => panic!("expected Dict for entries list"),
                                            }
                                        }
                                    }
                                    _ => panic!("expected Dict for exprs list"),
                                }
                            }
                            _ => panic!("expected Dict for document"),
                        }
                    }
                    _ => panic!("expected Dict for docs list"),
                }
            }
            _ => panic!("expected Dict for file"),
        }
    }
}
