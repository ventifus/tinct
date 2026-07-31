//! AST-to-dict serialization and dict-to-Surface conversion for quasiquoting, macros, and formatter.
//!
//! Bidirectional conversion between AST nodes and tinct `Value::Variant` (Expr nodes) or `Value::Dict`
//! (structural nodes) matching the canonical schema in `doc/feature/ast-schema.md`.
//!
//! The canonical runtime representation of AST nodes is `Value::Variant { tag: "Expr.<Tag>", .. }`.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{
    class_decl_name, Annotation, DotKey, Span, Spanned, SurfaceDeclaration, SurfaceDocument,
    SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceNamedArg, SurfaceNode, SurfaceParam,
    SurfaceProgram,
};
use crate::error::EvalResult;
use crate::rust_span;
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

/// Add `span: {start: {line, col}, end: {...}}` to an Expr.* variant's payload.
fn inject_span_into_expr_variant(
    val: Value,
    span: &Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    use crate::value::{HashableValue, Thunk, Value};

    let synth = rust_span!(); // span for the synthetic thunks wrapping position integers

    let make_pos = |line: u32, col: u32| -> Arc<Thunk> {
        let mut d: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let mk = |n: i64| {
            Arc::new(Thunk::value(
                Value::Int {
                    n,
                    type_val: crate::value::unknown_type_val(),
                },
                synth.clone(),
            ))
        };
        d.insert(HashableValue::Str("line".into()), mk(line as i64));
        d.insert(HashableValue::Str("col".into()), mk(col as i64));
        Arc::new(Thunk::value(
            Value::Dict {
                entries: d,
                type_val: crate::value::unknown_type_val(),
            },
            synth.clone(),
        ))
    };

    let span_val = {
        let mut d: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        d.insert(
            HashableValue::Str("start".into()),
            make_pos(span.start_line, span.start_col),
        );
        d.insert(
            HashableValue::Str("end".into()),
            make_pos(span.end_line, span.end_col),
        );
        Value::Dict {
            entries: d,
            type_val: crate::value::unknown_type_val(),
        }
    };

    match val {
        Value::Variant {
            type_val,
            ctor,
            payload: Some(payload_thunk),
        } => {
            // payload_thunk is always Thunk::value(...) during AST conversion — no eval errors.
            if let Value::Dict {
                entries: mut payload_dict,
                ..
            } = payload_thunk
                .require_value()
                .expect("inject_span_into_expr_variant: payload thunk is always Thunk::value — impossible eval error")
                .clone()
            {
                let span_thunk = Arc::new(Thunk::value(span_val, synth.clone()));
                payload_dict.insert(HashableValue::Str("span".into()), span_thunk);
                let new_payload = Arc::new(Thunk::value(
                    Value::Dict {
                        entries: payload_dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                ));
                Value::Variant {
                    type_val,
                    ctor,
                    payload: Some(new_payload),
                }
            } else {
                Value::Variant {
                    type_val,
                    ctor,
                    payload: Some(payload_thunk),
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
    if let Value::Variant {
        ctor,
        payload: None,
        ..
    } = val
    {
        if ctor.as_ref() == "Expr.AstError" {
            return Ok(Arc::new(SurfaceNode::new(
                SurfaceExpression::Error(call_site_span.clone()),
                call_site_span.clone(),
            )));
        }
    }

    let node = SurfaceExpression::from_expr_variant(val, ctx)?;

    // Extract the span from the Expr.* payload (injected by surface_node_to_expr_variant).
    // If present, use it — it encodes the original user source position for round-trips.
    // Also detect the do-infer-placeholder field (protocol §7): when `do-infer-placeholder: 1`
    // is present in an Expr.VarRef payload, set the `do_infer_placeholder` flag on the
    // resulting VarRef so the type checker can dispatch on the flag instead of inspecting
    // the gensym name prefix.
    if let Value::Variant {
        ctor,
        payload: Some(ref payload_thunk),
        ..
    } = val
    {
        // payload_thunk and do-infer-placeholder thunk are Thunk::value(...) — no eval errors.
        let payload_val = thunk_value_or_ast_error(payload_thunk, "Expr variant payload", vec![])?;
        if let Value::Dict { entries: dict, .. } = payload_val {
            let span_opt = extract_span(&dict, ctx);
            let is_do_infer_placeholder = ctor.as_ref() == "Expr.VarRef"
                && dict
                    .get(&crate::value::HashableValue::Str(
                        "do-infer-placeholder".into(),
                    ))
                    .and_then(|t| {
                        Some(
                            t.require_value()
                                .expect("do-infer-placeholder thunk is always Thunk::value — impossible eval error"),
                        )
                    })
                    .is_some_and(|v| matches!(v, Value::Int { n, .. } if *n != 0));

            if is_do_infer_placeholder {
                // Rebuild the VarRef with do_infer_placeholder: true.
                let new_expr = if let SurfaceExpression::VarRef {
                    ref name, escaped, ..
                } = node.expr
                {
                    SurfaceExpression::VarRef {
                        name: name.clone(),
                        escaped,
                        resolution: crate::ast::Resolution::new(),
                        annotation: None,
                        do_infer_placeholder: true,
                    }
                } else {
                    node.expr.clone()
                };
                let span = span_opt.unwrap_or_else(|| node.span.clone());
                return Ok(Arc::new(SurfaceNode::new(new_expr, span)));
            }

            if let Some(span) = span_opt {
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

    // Helper to recursively convert child CoreExpr nodes — returns Arc<Thunk> for payload insertion
    let recurse = |child: &Spanned<CoreExpr>| -> Arc<Thunk> {
        let child_val = core_expr_to_expr_value(child, ctx);
        Arc::new(Thunk::value(child_val, child.span.clone()))
    };

    // Helper to convert a Vec of CoreExpr to a Dict (auto-indexed) — allocated for Variant payload use
    let recurse_vec = |children: &[Arc<Spanned<CoreExpr>>]| -> Arc<Thunk> {
        let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, child) in children.iter().enumerate() {
            let child_thunk = recurse(child);
            dict.insert(HashableValue::Int(i as i64), child_thunk);
        }
        Arc::new(Thunk::value(
            Value::Dict {
                entries: dict,
                type_val: crate::value::unknown_type_val(),
            },
            synth.clone(),
        ))
    };

    // Helper to build an Expr.* variant with a payload dict
    let make_variant = |tag: &str, fields: IndexMap<HashableValue, Arc<Thunk>>| -> Value {
        make_variant_with_payload(&format!("Expr.{}", tag), fields, &core.span, ctx)
    };

    // Helper: build a Dict-typed Arc<Thunk> from a map (for inner nested dicts)
    let mk_dict =
        |map: IndexMap<HashableValue, Arc<Thunk>>, span: crate::ast::Span| -> Arc<Thunk> {
            Arc::new(Thunk::value(
                Value::Dict {
                    entries: map,
                    type_val: crate::value::unknown_type_val(),
                },
                span,
            ))
        };
    // Helper: build a simple materialized Arc<Thunk> for scalar values
    let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, synth.clone())) };
    // Null sentinel (empty dict)
    let null_thunk = || {
        mk(Value::Dict {
            entries: IndexMap::new(),
            type_val: crate::value::unknown_type_val(),
        })
    };

    let val = match &core.node {
        // ── Literals ─────────────────────────────────────────────────────────────
        CoreExpr::Int(n) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("kind".into()), mk(string_val("int")));
            payload.insert(
                HashableValue::Str("value".into()),
                mk(Value::Int {
                    n: *n,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            payload.insert(HashableValue::Str("bare".into()), null_thunk());
            make_variant("Literal", payload)
        }

        CoreExpr::U64(n) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("kind".into()), mk(string_val("u64")));
            payload.insert(
                HashableValue::Str("value".into()),
                mk(Value::Int {
                    n: *n as i64,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            payload.insert(HashableValue::Str("bare".into()), null_thunk());
            make_variant("Literal", payload)
        }

        CoreExpr::Float(f) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("kind".into()), mk(string_val("float")));
            payload.insert(
                HashableValue::Str("value".into()),
                mk(Value::Float {
                    n: *f,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            payload.insert(HashableValue::Str("bare".into()), null_thunk());
            make_variant("Literal", payload)
        }

        CoreExpr::Str(s) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("kind".into()), mk(string_val("str")));
            payload.insert(HashableValue::Str("value".into()), mk(string_val(s)));
            payload.insert(HashableValue::Str("bare".into()), null_thunk());
            make_variant("Literal", payload)
        }

        // ── Variables ────────────────────────────────────────────────────────────
        // DROP de Bruijn coordinates — only preserve the name
        CoreExpr::Var { name, .. } => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("name".into()), mk(string_val(name)));
            make_variant("VarRef", payload)
        }

        // ── Call ─────────────────────────────────────────────────────────────────
        CoreExpr::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("fn".into()), recurse(func));
            payload.insert(HashableValue::Str("args".into()), recurse_vec(args));

            // Convert named args to Dict
            let mut named_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (i, named_arg) in named_args.iter().enumerate() {
                let mut arg_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                arg_dict.insert(
                    HashableValue::Str("name".into()),
                    mk(string_val(&named_arg.node.name)),
                );
                arg_dict.insert(
                    HashableValue::Str("value".into()),
                    recurse(&named_arg.node.value),
                );
                named_dict.insert(
                    HashableValue::Int(i as i64),
                    mk_dict(arg_dict, named_arg.span.clone()),
                );
            }
            payload.insert(
                HashableValue::Str("named-args".into()),
                mk_dict(named_dict, synth.clone()),
            );
            payload.insert(
                HashableValue::Str("implied".into()),
                mk(Value::Int {
                    n: if *implied { 1 } else { 0 },
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            make_variant("Call", payload)
        }

        // ── Dict ─────────────────────────────────────────────────────────────────
        CoreExpr::Dict(entries) => {
            let mut entries_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (i, entry) in entries.iter().enumerate() {
                let mut entry_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                entry_dict.insert(
                    HashableValue::Str("key".into()),
                    match &entry.node.key {
                        Some(k) => recurse(k),
                        None => null_thunk(),
                    },
                );
                entry_dict.insert(
                    HashableValue::Str("value".into()),
                    recurse(&entry.node.value),
                );
                entries_dict.insert(
                    HashableValue::Int(i as i64),
                    mk_dict(entry_dict, entry.span.clone()),
                );
            }
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(
                HashableValue::Str("entries".into()),
                mk_dict(entries_dict, synth.clone()),
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
            let mut params_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (i, param) in params.iter().enumerate() {
                let mut param_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                param_dict.insert(
                    HashableValue::Str("name".into()),
                    mk(string_val(&param.node.name)),
                );
                param_dict.insert(
                    HashableValue::Str("annotation".into()),
                    alloc_annotation_opt(param.node.annotation.as_ref(), ctx),
                );
                param_dict.insert(
                    HashableValue::Str("variadic".into()),
                    mk(Value::Int {
                        n: if param.node.variadic { 1 } else { 0 },
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                params_dict.insert(
                    HashableValue::Int(i as i64),
                    mk_dict(param_dict, param.span.clone()),
                );
            }

            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(
                HashableValue::Str("params".into()),
                mk_dict(params_dict, synth.clone()),
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
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("exprs".into()), recurse_vec(exprs));
            make_variant("Sequential", payload)
        }

        // ── Match ────────────────────────────────────────────────────────────────
        CoreExpr::Match { scrutinee, arms } => {
            let mut arms_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (i, arm) in arms.iter().enumerate() {
                // CoreMatchArm has pattern, guard, body — serialize as opaque Dict
                let mut arm_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                arm_dict.insert(
                    HashableValue::Str("pattern".into()),
                    Arc::new(Thunk::value(
                        surface_node_to_expr_variant(&arm.pattern, ctx),
                        arm.pattern.span.clone(),
                    )),
                );
                arm_dict.insert(
                    HashableValue::Str("guard".into()),
                    match &arm.guard {
                        Some(g) => recurse(g),
                        None => null_thunk(),
                    },
                );
                arm_dict.insert(HashableValue::Str("body".into()), recurse(&arm.body));
                arms_dict.insert(
                    HashableValue::Int(i as i64),
                    mk_dict(arm_dict, synth.clone()),
                );
            }
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("scrutinee".into()), recurse(scrutinee));
            payload.insert(
                HashableValue::Str("arms".into()),
                mk_dict(arms_dict, synth.clone()),
            );
            make_variant("Match", payload)
        }

        // ── Quote / Unquote / UnquoteSplice ──────────────────────────────────────
        CoreExpr::Quote(e) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("Quote", payload)
        }

        CoreExpr::Unquote(e) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("Unquote", payload)
        }

        CoreExpr::UnquoteSplice(e) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(HashableValue::Str("expr".into()), recurse(e));
            make_variant("UnquoteSplice", payload)
        }

        // ── TypeAssert ───────────────────────────────────────────────────────────
        CoreExpr::TypeAssert { check, expr, .. } => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            let annotation_opt = match check {
                crate::ast::TypeAssertCheck::Source { annotation } => Some(annotation),
                crate::ast::TypeAssertCheck::Resolved(_) => None,
            };
            payload.insert(
                HashableValue::Str("annotation".into()),
                alloc_annotation_opt(annotation_opt, ctx),
            );
            payload.insert(HashableValue::Str("expr".into()), recurse(expr));
            make_variant("TypeAssert", payload)
        }

        // ── Rest ─────────────────────────────────────────────────────────────────
        CoreExpr::Rest(name_opt) => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            payload.insert(
                HashableValue::Str("name".into()),
                alloc_string_opt(name_opt.as_deref(), ctx),
            );
            make_variant("Rest", payload)
        }

        // ── LetDecl / PatternDecl ────────────────────────────────────────────────
        CoreExpr::LetDecl { bindings } => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            let arc_bindings: Vec<Arc<Spanned<CoreExpr>>> =
                bindings.iter().map(|b| Arc::new(b.clone())).collect();
            payload.insert(
                HashableValue::Str("bindings".into()),
                recurse_vec(&arc_bindings),
            );
            make_variant("LetDecl", payload)
        }

        CoreExpr::PatternDecl { bindings } => {
            let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            let arc_bindings: Vec<Arc<Spanned<CoreExpr>>> =
                bindings.iter().map(|b| Arc::new(b.clone())).collect();
            payload.insert(
                HashableValue::Str("bindings".into()),
                recurse_vec(&arc_bindings),
            );
            make_variant("PatternDecl", payload)
        }

        // ── Variant ──────────────────────────────────────────────────────────────
        // This is AST variant construction (Expr.Variant), NOT Value::Variant itself
        CoreExpr::Variant { tag, payload } => {
            let mut fields: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            fields.insert(HashableValue::Str("tag".into()), mk(string_val(tag)));
            fields.insert(
                HashableValue::Str("payload".into()),
                match payload {
                    Some(p) => recurse(p),
                    None => null_thunk(),
                },
            );
            make_variant("Variant", fields)
        }

        CoreExpr::UnitVariant { tycon, ctor } => {
            let mut fields: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            fields.insert(HashableValue::Str("tycon".into()), mk(string_val(tycon)));
            fields.insert(HashableValue::Str("ctor".into()), mk(string_val(ctor)));
            make_variant("UnitVariant", fields)
        }

        // ── Placeholder ──────────────────────────────────────────────────────────
        CoreExpr::Placeholder => make_unit_variant("Expr.Placeholder"),

        // ── ReprDecl ─────────────────────────────────────────────────────────────
        // Transparent in AST representation — the quoted form is the inner dict.
        // The repr: metadata is evaluator-only and has no surface AST node type.
        CoreExpr::ReprDecl { inner, .. } => core_expr_to_expr_value(inner.as_ref(), ctx),
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
        Value::Dict { entries: d, .. } => d,
        _ => {
            return Err(AstError {
                message: "entry must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let key_val = get_dict_field(dict, "key", path, ctx)?;
    let key: Option<Arc<SurfaceNode>> = match &key_val {
        Value::Dict { entries: d, .. } if d.is_empty() => None,
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
        ctor,
        payload: Some(payload_thunk),
        ..
    } = val
    {
        if crate::value::tycon_name_from_ctor(ctor.as_ref()) != "Annotation" {
            return Err(AstError {
                message: format!("expected Annotation variant, got {}", ctor),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            });
        }
        let payload_val = thunk_value_or_ast_error(
            payload_thunk,
            "annotation payload",
            path.iter().map(|s| s.to_string()).collect(),
        )?;
        // Strip "Annotation." prefix to get bare constructor name for dispatch.
        let bare_ctor = ctor
            .as_ref()
            .split_once('.')
            .map(|(_, c)| c)
            .unwrap_or(ctor.as_ref());
        let ann = match bare_ctor {
            "Quote" => Annotation::Quote,
            "Simple" => match &payload_val {
                Value::Dict { entries: d, .. } => {
                    // Try "name" first (canonical field); fall back to "text" if absent.
                    // If both are absent, propagate the "text" field error.
                    let name = get_string_field(d, "name", path, ctx)
                        .or_else(|_| get_string_field(d, "text", path, ctx))?;
                    Annotation::Simple(name)
                }
                _ => Annotation::Simple(String::new()),
            },
            "PropertyDict" | "Unknown" => {
                // Reconstruct as a simple annotation using the text field
                match &payload_val {
                    Value::Dict { entries: d, .. } => {
                        let text = get_string_field(d, "text", path, ctx)?;
                        Annotation::Simple(text)
                    }
                    _ => Annotation::Simple(String::new()),
                }
            }
            "Annotated" => match &payload_val {
                Value::Dict { entries: d, .. } => {
                    // Deserialize outer: non-Simple outer is stored as a nested "outer" Annotation
                    // value; Simple outer uses the flat "name" string for backward compatibility.
                    let outer_ann = if let Ok(outer_val) = get_field(d, "outer", path, ctx) {
                        dict_to_annotation(&outer_val, path, ctx)?.node
                    } else {
                        let name = get_string_field(d, "name", path, ctx)?;
                        Annotation::Simple(name)
                    };
                    // Deserialize inner from "inner-ann" (structured recursive Annotation value).
                    let inner_ann_val = get_field(d, "inner-ann", path, ctx)?;
                    let inner_ann = dict_to_annotation(&inner_ann_val, path, ctx)?.node;
                    Annotation::Annotated(Box::new(outer_ann), Box::new(inner_ann))
                }
                _ => Annotation::Simple(String::new()),
            },
            _ => Annotation::Simple(String::new()),
        };
        return Ok(Spanned::new(ann, rust_span!()));
    }

    let dict = match val {
        Value::Dict { entries: d, .. } => d,
        _ => {
            return Err(AstError {
                message: "annotation must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let kind = get_string_field(dict, "kind", path, ctx)?;

    let ann = match kind.as_str() {
        "quote" => Annotation::Quote,
        "simple" => {
            let value = get_string_field(dict, "value", path, ctx)?;
            Annotation::Simple(value)
        }
        "annotated" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            let inner_val = get_dict_field(dict, "inner", path, ctx)?;
            let inner = dict_to_annotation(&inner_val, path, ctx)?;
            Annotation::Annotated(Box::new(Annotation::Simple(name)), Box::new(inner.node))
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

// dict_to_pattern deleted (T-1750) — patterns are now Arc<SurfaceNode>, deserialize via dict_to_surface_node.

// ============================================================================
// Helper functions for extracting values from dicts with error context
// ============================================================================

/// Get the materialized value from a thunk, returning an `AstError` on failure.
///
/// - `Some(Ok(v))` → `Ok(v.clone())`
/// - `Some(Err(e))` → `Err(AstError { message: "evaluation error: ..." })`
/// - `None` → `Err(AstError { message: "not materialized" })`
fn thunk_value_or_ast_error(
    thunk: &Arc<Thunk>,
    field_name: &str,
    field_path: Vec<String>,
) -> Result<Value, AstError> {
    match thunk.peek_result() {
        Some(Ok(v)) => Ok(v.clone()),
        Some(Err(e)) => Err(AstError {
            message: format!("evaluation error in '{}': {}", field_name, e),
            field_path,
        }),
        None => Err(AstError {
            message: format!("'{}' is not materialized", field_name),
            field_path,
        }),
    }
}

fn get_field(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    path: &[&str],
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let thunk = dict
        .get(&HashableValue::Str(key.into()))
        .ok_or_else(|| AstError {
            message: format!("missing required field: {}", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        })?;

    thunk_value_or_ast_error(thunk, key, path.iter().map(|s| s.to_string()).collect())
}

fn get_string_field(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
            ..
        } => Ok(source[start..end].to_string()),
        Value::Dict { entries: d, .. } if d.is_empty() => {
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
                    Value::Dict { entries: d, .. } => format!("Dict with {} entries", d.len()),
                    _ => format!("{:?}", val),
                },
                val.type_name()
            ),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_bool_field(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    Ok(matches!(val, Value::Int { n, .. } if n != 0))
}

fn get_dict_field(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Dict { .. } | Value::Variant { .. } => Ok(val),
        _ => Err(AstError {
            message: format!("field '{}' must be Dict or Variant", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_optional_dict_field(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Value>, AstError> {
    match dict.get(&HashableValue::Str(key.into())) {
        Some(thunk) => match thunk.peek_result() {
            Some(Ok(v)) => Ok(Some(v.clone())),
            Some(Err(e)) => Err(AstError {
                message: format!("evaluation error in optional field '{}': {}", key, e),
                field_path: vec![key.to_string()],
            }),
            None => Ok(None),
        },
        None => Ok(None),
    }
}

fn is_empty_dict(val: &Value) -> bool {
    matches!(val, Value::Dict { entries: d, .. } if d.is_empty())
}

pub(crate) fn extract_span(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Span> {
    let span_thunk = dict.get(&HashableValue::Str("span".into()))?;
    // These span thunks are always created by Thunk::value(...) during surface-to-dict
    // conversion and can never carry evaluation errors.
    let span_val = span_thunk
        .require_value()
        .expect("extract_span: span thunk is always Thunk::value — impossible eval error")
        .clone();

    match span_val {
        Value::Dict {
            entries: span_dict, ..
        } => {
            let start_thunk = span_dict.get(&HashableValue::Str("start".into()))?;
            let start_val = start_thunk
                .require_value()
                .expect("extract_span: start thunk is always Thunk::value — impossible eval error")
                .clone();

            let end_thunk = span_dict.get(&HashableValue::Str("end".into()))?;
            let end_val = end_thunk
                .require_value()
                .expect("extract_span: end thunk is always Thunk::value — impossible eval error")
                .clone();

            let (start_line, start_col) = extract_position(&start_val, ctx)?;
            let (end_line, end_col) = extract_position(&end_val, ctx)?;

            Some(Span::new(
                start_line,
                start_col,
                end_line,
                end_col,
                std::sync::Arc::from("<surface-convert>"),
            ))
        }
        _ => None,
    }
}

fn extract_position(val: &Value, _ctx: &Arc<crate::eval::EvalContext>) -> Option<(u32, u32)> {
    match val {
        Value::Dict { entries: dict, .. } => {
            let line_thunk = dict.get(&HashableValue::Str("line".into()))?;
            // These position thunks are Thunk::value(...) — no eval errors possible.
            let line = match line_thunk
                .require_value()
                .expect(
                    "extract_position: line thunk is always Thunk::value — impossible eval error",
                )
                .clone()
            {
                Value::Int { n, .. } => n as u32,
                _ => return None,
            };

            let col_thunk = dict.get(&HashableValue::Str("col".into()))?;
            let col = match col_thunk
                .require_value()
                .expect(
                    "extract_position: col thunk is always Thunk::value — impossible eval error",
                )
                .clone()
            {
                Value::Int { n, .. } => n as u32,
                _ => return None,
            };

            Some((line, col))
        }
        _ => None,
    }
}

fn extract_list(
    val: &Value,
    path: &[&str],
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Vec<Value>, AstError> {
    match val {
        Value::Dict { entries: d, .. } => {
            let mut result = Vec::new();
            for i in 0.. {
                match d.get(&HashableValue::Int(i)) {
                    Some(thunk) => {
                        let val = thunk_value_or_ast_error(
                            thunk,
                            &format!("element {}", i),
                            path.iter().map(|s| s.to_string()).collect(),
                        )?;
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

pub(crate) fn alloc_str(s: &str, span: &Span, _ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    Arc::new(Thunk::value(string_val(s), span.clone()))
}

pub(crate) fn alloc_bool(b: bool, span: &Span, _ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    Arc::new(Thunk::value(
        Value::Int {
            n: if b { 1 } else { 0 },
            type_val: crate::value::unknown_type_val(),
        },
        span.clone(),
    ))
}

pub(crate) fn alloc_int(n: i64, span: &Span, _ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    Arc::new(Thunk::value(
        Value::Int {
            n,
            type_val: crate::value::unknown_type_val(),
        },
        span.clone(),
    ))
}

pub(crate) fn alloc_u64(n: u64, span: &Span, _ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    Arc::new(Thunk::value(
        Value::U64 {
            n,
            type_val: crate::value::unknown_type_val(),
        },
        span.clone(),
    ))
}

pub(crate) fn alloc_float(f: f64, span: &Span, _ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    Arc::new(Thunk::value(
        Value::Float {
            n: f,
            type_val: crate::value::unknown_type_val(),
        },
        span.clone(),
    ))
}

pub(crate) fn alloc_expr_child(
    node: &Arc<SurfaceNode>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = surface_node_to_expr_variant(node, ctx);
    Arc::new(Thunk::value(val, node.span.clone()))
}

/// Allocate an optional child expression node.
/// `None` produces an empty dict (null) — consistent with how annotation_opt and string_opt
/// handle absent optional fields.
pub(crate) fn alloc_expr_child_opt(
    node: Option<&Arc<SurfaceNode>>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    match node {
        Some(n) => alloc_expr_child(n, ctx),
        None => Arc::new(Thunk::value(
            Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
            rust_span!(),
        )),
    }
}

pub(crate) fn alloc_child_list(
    nodes: &[Arc<SurfaceNode>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let mut map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (i, n) in nodes.iter().enumerate() {
        let thunk = Arc::new(Thunk::value(
            surface_node_to_expr_variant(n, ctx),
            n.span.clone(),
        ));
        map.insert(HashableValue::Int(i as i64), thunk);
    }
    Arc::new(Thunk::value(
        Value::Dict {
            entries: map,
            type_val: crate::value::unknown_type_val(),
        },
        rust_span!(),
    ))
}

pub(crate) fn alloc_entry_list(
    entries: &[Spanned<SurfaceEntry>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // key: Some(node) → Expr.* variant, None → null (empty dict)
        let key_val = match &entry.node.key {
            Some(key_node) => SurfaceExpression::to_expr_variant(key_node, ctx),
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        let key_thunk = Arc::new(Thunk::value(key_val, entry.span.clone()));
        // value: Expr.* variant
        let val_val = SurfaceExpression::to_expr_variant(&entry.node.value, ctx);
        let val_thunk = Arc::new(Thunk::value(val_val, entry.span.clone()));
        // Build payload dict for Expr.Entry
        let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        payload.insert(HashableValue::Str("key".into()), key_thunk);
        payload.insert(HashableValue::Str("value".into()), val_thunk);
        let payload_thunk = Arc::new(Thunk::value(
            Value::Dict {
                entries: payload,
                type_val: crate::value::unknown_type_val(),
            },
            entry.span.clone(),
        ));
        // Expr.Entry variant
        let entry_variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from("Expr.Entry"),
            payload: Some(payload_thunk),
        };
        let entry_thunk = Arc::new(Thunk::value(entry_variant, entry.span.clone()));
        dict.insert(HashableValue::Int(i as i64), entry_thunk);
    }
    Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        rust_span!(),
    ))
}

pub(crate) fn alloc_named_arg_list(
    args: &[Spanned<SurfaceNamedArg>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let mut na_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (i, na) in args.iter().enumerate() {
        let mut na_payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        na_payload.insert(
            HashableValue::Str("name".into()),
            Arc::new(Thunk::value(string_val(&na.node.name), na.span.clone())),
        );
        na_payload.insert(
            HashableValue::Str("value".into()),
            Arc::new(Thunk::value(
                surface_node_to_expr_variant(&na.node.value, ctx),
                na.span.clone(),
            )),
        );
        let payload_thunk = Arc::new(Thunk::value(
            Value::Dict {
                entries: na_payload,
                type_val: crate::value::unknown_type_val(),
            },
            na.span.clone(),
        ));
        let na_thunk = Arc::new(Thunk::value(
            Value::Variant {
                type_val: crate::value::unknown_type_val(),
                ctor: Arc::from("Expr.NamedArg"),
                payload: Some(payload_thunk),
            },
            na.span.clone(),
        ));
        na_map.insert(HashableValue::Int(i as i64), na_thunk);
    }
    Arc::new(Thunk::value(
        Value::Dict {
            entries: na_map,
            type_val: crate::value::unknown_type_val(),
        },
        rust_span!(),
    ))
}

pub(crate) fn alloc_param_list(
    params: &[Spanned<SurfaceParam>],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let mut params_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (i, p) in params.iter().enumerate() {
        let mut p_payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        p_payload.insert(
            HashableValue::Str("name".into()),
            Arc::new(Thunk::value(string_val(&p.node.name), p.span.clone())),
        );
        p_payload.insert(
            HashableValue::Str("variadic".into()),
            Arc::new(Thunk::value(
                Value::Int {
                    n: if p.node.variadic { 1 } else { 0 },
                    type_val: crate::value::unknown_type_val(),
                },
                p.span.clone(),
            )),
        );
        let ann_val =
            crate::surface_fields::annotation_opt_to_value(p.node.annotation.as_ref(), ctx);
        p_payload.insert(
            HashableValue::Str("annotation".into()),
            Arc::new(Thunk::value(ann_val, p.span.clone())),
        );
        let param_payload_thunk = Arc::new(Thunk::value(
            Value::Dict {
                entries: p_payload,
                type_val: crate::value::unknown_type_val(),
            },
            p.span.clone(),
        ));
        let p_thunk = Arc::new(Thunk::value(
            Value::Variant {
                type_val: crate::value::unknown_type_val(),
                ctor: Arc::from("Expr.Param"),
                payload: Some(param_payload_thunk),
            },
            p.span.clone(),
        ));
        params_map.insert(HashableValue::Int(i as i64), p_thunk);
    }
    Arc::new(Thunk::value(
        Value::Dict {
            entries: params_map,
            type_val: crate::value::unknown_type_val(),
        },
        rust_span!(),
    ))
}

pub(crate) fn alloc_match_arm_list(
    arms: &[crate::ast::SurfaceMatchArm],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = crate::surface_fields::match_arms_to_list_dict_pub(arms, ctx);
    Arc::new(Thunk::value(val, rust_span!()))
}

pub(crate) fn alloc_annotation(
    ann: &Spanned<Annotation>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = crate::surface_fields::annotation_to_value(ann, ctx);
    Arc::new(Thunk::value(val, ann.span.clone()))
}

pub(crate) fn alloc_annotation_opt(
    ann: Option<&Spanned<Annotation>>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = crate::surface_fields::annotation_opt_to_value(ann, ctx);
    Arc::new(Thunk::value(val, rust_span!()))
}

pub(crate) fn alloc_string_opt(
    s: Option<&str>,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = match s {
        Some(name) => string_val(name),
        None => Value::Dict {
            entries: IndexMap::new(),
            type_val: crate::value::unknown_type_val(),
        },
    };
    Arc::new(Thunk::value(val, rust_span!()))
}

pub(crate) fn alloc_dot_key(
    key: &DotKey,
    span: &Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Arc<Thunk> {
    let val = match key {
        DotKey::Ident(name) => string_val(name),
        DotKey::Int(n) => string_val(&n.to_string()),
    };
    Arc::new(Thunk::value(val, span.clone()))
}

pub(crate) fn alloc_span(span: &Span, ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
    let val = crate::surface_fields::span_to_value(span, ctx);
    Arc::new(Thunk::value(val, span.clone()))
}

pub(crate) fn make_variant_with_payload(
    tag: &str,
    payload: IndexMap<HashableValue, Arc<Thunk>>,
    span: &Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    let payload_val = Value::Dict {
        entries: payload,
        type_val: crate::value::unknown_type_val(),
    };
    let payload_thunk = Arc::new(Thunk::value(payload_val, span.clone()));
    Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(tag),
        payload: Some(payload_thunk),
    }
}

pub(crate) fn make_unit_variant(tag: &str) -> Value {
    Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(tag),
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
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Result<(String, IndexMap<HashableValue, Arc<Thunk>>), AstError> {
    match val {
        Value::Variant { ctor, payload, .. } => {
            let payload_thunk = payload.as_ref().ok_or_else(|| AstError {
                message: format!("Expr variant {} has no payload", ctor),
                field_path: vec![],
            })?;
            let payload_val = thunk_value_or_ast_error(payload_thunk, "variant payload", vec![])?;
            // Return the bare constructor name (strip tycon prefix) for dispatch in callers.
            let bare_ctor = ctor
                .as_ref()
                .split_once('.')
                .map(|(_, c)| c)
                .unwrap_or(ctor.as_ref())
                .to_string();
            match payload_val {
                Value::Dict { entries: d, .. } => Ok((bare_ctor, d)),
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    // Try primary key first
    if let Some(thunk) = dict.get(&HashableValue::Str(key.into())) {
        return thunk_value_or_ast_error(thunk, key, vec![key.to_string()]);
    }
    // Try aliases
    for alias in aliases {
        if let Some(thunk) = dict.get(&HashableValue::Str((*alias).into())) {
            return thunk_value_or_ast_error(
                thunk,
                &format!("{} (alias {})", key, alias),
                vec![key.to_string()],
            );
        }
    }
    Err(AstError {
        message: format!("missing required field: {}", key),
        field_path: vec![key.to_string()],
    })
}

pub(crate) fn get_string_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
            ..
        } => Ok(source[start..end].to_string()),
        Value::Dict { entries: d, .. } if d.is_empty() => Err(AstError {
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    Ok(matches!(val, Value::Int { n, .. } if n != 0))
}

pub(crate) fn get_int_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<i64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Int { n, .. } => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be Int", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_u64_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<u64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::U64 { n, .. } => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be U64", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_float_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<f64, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Float { n: f, .. } => Ok(f),
        _ => Err(AstError {
            message: format!("field '{}' must be Float", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_child_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
        Err(e) => Err(e),
    }
}

pub(crate) fn get_child_list_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
        let key_thunk = payload_dict
            .get(&HashableValue::Str("key".into()))
            .ok_or_else(|| AstError {
                message: "Expr.Entry missing key field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "key".to_string()],
            })?;
        let key_val = thunk_value_or_ast_error(
            key_thunk,
            "Expr.Entry key",
            vec![key.to_string(), i_str.clone(), "key".to_string()],
        )?;
        let key_node = match &key_val {
            Value::Dict { entries: d, .. } if d.is_empty() => None,
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
        let value_thunk = payload_dict
            .get(&HashableValue::Str("value".into()))
            .ok_or_else(|| AstError {
                message: "Expr.Entry missing value field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let value_val = thunk_value_or_ast_error(
            value_thunk,
            "Expr.Entry value",
            vec![key.to_string(), i_str.clone(), "value".to_string()],
        )?;
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
        let value_thunk = payload_dict
            .get(&HashableValue::Str("value".into()))
            .ok_or_else(|| AstError {
                message: "Expr.NamedArg missing value field".into(),
                field_path: vec![key.to_string(), i_str.clone(), "value".to_string()],
            })?;
        let value_val = thunk_value_or_ast_error(
            value_thunk,
            "Expr.NamedArg value",
            vec![key.to_string(), i_str.clone(), "value".to_string()],
        )?;
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
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
                resolved_annotation_type: crate::ast::TypeAnnotation::new(),
            },
            rust_span!(),
        ));
    }
    Ok(params)
}

pub(crate) fn get_match_arm_list_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
            Value::Dict { entries: d, .. } => d,
            _ => {
                return Err(AstError {
                    message: format!("match arm {} must be Dict", i),
                    field_path: vec![key.to_string(), i_str.clone()],
                })
            }
        };
        let arm_fallback_span = extract_span(&arm_dict, ctx).unwrap_or_else(|| rust_span!());
        let pattern_val = get_dict_field(&arm_dict, "pattern", &[key, &i_str], ctx)?;
        // T-1750: pattern is now Arc<SurfaceNode>, deserialize like guard/body
        let pattern = dict_to_surface_node(&pattern_val, &arm_fallback_span, ctx)?;
        let guard = match get_optional_dict_field(&arm_dict, "guard", ctx)? {
            Some(guard_val) if !is_empty_dict(&guard_val) => {
                Some(dict_to_surface_node(&guard_val, &arm_fallback_span, ctx)?)
            }
            _ => None,
        };
        let body_val = get_dict_field(&arm_dict, "body", &[key, &i_str], ctx)?;
        let body_node = dict_to_surface_node(&body_val, &arm_fallback_span, ctx)?;
        arms.push(SurfaceMatchArm {
            pattern,
            let_bindings: None,
            guard,
            body: vec![body_node],
            guard_matchable_binding: crate::ast::MatchableBinding::new(),
            case_captures: crate::ast::CapturesCell::new(),
        });
    }
    Ok(arms)
}

pub(crate) fn get_annotation_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Annotation>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    dict_to_annotation(&val, &[key], ctx)
}

pub(crate) fn get_annotation_opt_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Spanned<Annotation>>, AstError> {
    match get_field_with_aliases(dict, key, aliases, ctx) {
        Ok(val) if !is_empty_dict(&val) => dict_to_annotation(&val, &[key], ctx).map(Some),
        Ok(_) => Ok(None),
        Err(e) => Err(e),
    }
}

pub(crate) fn get_string_opt_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
    key: &str,
    aliases: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<String>, AstError> {
    let val = get_field_with_aliases(dict, key, aliases, ctx)?;
    match val {
        Value::Dict { entries: d, .. } if d.is_empty() => Ok(None),
        Value::String {
            ref source,
            start,
            end,
            ..
        } => Ok(Some(source[start..end].to_string())),
        _ => Err(AstError {
            message: format!("field '{}' must be String or empty dict", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_dot_key_field_with_aliases(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
            ..
        } => {
            let s = source[start..end].to_string();
            // Try to parse as integer first, fall back to Ident
            if let Ok(n) = s.parse::<i64>() {
                Ok(DotKey::Int(n))
            } else {
                Ok(DotKey::Ident(s))
            }
        }
        Value::Int { n, .. } => Ok(DotKey::Int(n)),
        _ => Err(AstError {
            message: format!("field '{}' must be String or Int for DotKey", key),
            field_path: vec![key.to_string()],
        }),
    }
}

pub(crate) fn get_span_from_dict(
    dict: &IndexMap<HashableValue, Arc<Thunk>>,
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
    pub leading_comments: &'a std::collections::BTreeMap<u64, Vec<String>>,
    pub trailing_comments: &'a std::collections::BTreeMap<u64, String>,
    pub blank_before: &'a std::collections::BTreeMap<u64, bool>,
}

// ============================================================================
// Surface AST to Dict conversion functions (AST → Arc<Thunk>)
// ============================================================================
//
// These functions convert Surface AST nodes to their dict representation as Arc<Thunk>.
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
    let mut root: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

    root.insert(
        HashableValue::Str("type".into()),
        Arc::new(Thunk::value(string_val("file"), span.clone())),
    );

    root.insert(
        HashableValue::Str("schema-version".into()),
        Arc::new(Thunk::value(
            Value::Int {
                n: 1,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );

    // documents: list of document dicts
    let doc_arcs: Vec<Arc<Thunk>> = program
        .documents
        .iter()
        .map(|doc| surface_document_to_thunk_id(&doc.node, doc.span.clone(), opts, ctx))
        .collect::<EvalResult<Vec<_>>>()?;

    root.insert(
        HashableValue::Str("documents".into()),
        list_to_thunk_id(doc_arcs.into_iter(), span.clone(), ctx)?,
    );

    root.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );

    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: root,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )))
}

/// Return the character at 1-based (line, col) in the given source text, if in bounds.
///
/// Used to determine whether a string literal key was written bare (without quotes) or
/// quoted, by checking if the first character at the span position is a double-quote.
fn get_char_at(source: &str, line: u32, col: u32) -> Option<char> {
    let target_line = line as usize; // 1-based
    let target_col = col as usize; // 1-based
    let line_text = source.lines().nth(target_line.checked_sub(1)?)?;
    line_text.chars().nth(target_col.checked_sub(1)?)
}

fn span_to_thunk_id(span: Span, _ctx: &Arc<crate::eval::EvalContext>) -> EvalResult<Arc<Thunk>> {
    let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

    // start position
    let mut start_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    start_dict.insert(
        HashableValue::Str("line".into()),
        Arc::new(Thunk::value(
            Value::Int {
                n: span.start_line as i64,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );
    start_dict.insert(
        HashableValue::Str("col".into()),
        Arc::new(Thunk::value(
            Value::Int {
                n: span.start_col as i64,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );

    // end position
    let mut end_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    end_dict.insert(
        HashableValue::Str("line".into()),
        Arc::new(Thunk::value(
            Value::Int {
                n: span.end_line as i64,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );
    end_dict.insert(
        HashableValue::Str("col".into()),
        Arc::new(Thunk::value(
            Value::Int {
                n: span.end_col as i64,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );

    dict.insert(
        HashableValue::Str("start".into()),
        Arc::new(Thunk::value(
            Value::Dict {
                entries: start_dict,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );
    dict.insert(
        HashableValue::Str("end".into()),
        Arc::new(Thunk::value(
            Value::Dict {
                entries: end_dict,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );

    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )))
}

/// Convert an iterator of Arc<Thunk> to a dict-based list (auto-indexed dict with integer keys).
pub(crate) fn list_to_thunk_id(
    items: impl ExactSizeIterator<Item = Arc<Thunk>>,
    span: Span,
    _ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(items.len());
    for (i, item) in items.enumerate() {
        dict.insert(HashableValue::Int(i as i64), item);
    }
    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )))
}

/// Convert a `SurfaceDocument` to an `Arc<Thunk>` containing its dict representation.
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
) -> EvalResult<Arc<Thunk>> {
    let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

    dict.insert(
        HashableValue::Str("type".into()),
        Arc::new(Thunk::value(string_val("document"), span.clone())),
    );

    // expressions: list of expression/declaration dicts (all SurfaceItems, both Expr and Decl)
    let item_arcs: Vec<Arc<Thunk>> = doc
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
        list_to_thunk_id(item_arcs.into_iter(), span.clone(), ctx)?,
    );

    // header: dict of header entries (as SurfaceNode values)
    let header_thunks: IndexMap<HashableValue, Arc<Thunk>> = doc
        .header
        .iter()
        .map(|(k, v)| {
            Ok((
                HashableValue::Str(k.clone().into()),
                surface_node_to_thunk_id(v, opts, ctx)?,
            ))
        })
        .collect::<EvalResult<_>>()?;
    dict.insert(
        HashableValue::Str("header".into()),
        Arc::new(Thunk::value(
            Value::Dict {
                entries: header_thunks,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        )),
    );

    // leading-comments: absent when None or empty
    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps
            .leading_comments
            .get(&crate::parser::span_key(span.start_line, span.start_col))
        {
            if !comments.is_empty() {
                let comment_arcs: Vec<Arc<Thunk>> = comments
                    .iter()
                    .map(|c| Arc::new(Thunk::value(string_val(c), span.clone())))
                    .collect();
                dict.insert(
                    HashableValue::Str("leading-comments".into()),
                    list_to_thunk_id(comment_arcs.into_iter(), span.clone(), ctx)?,
                );
            }
        }
    }

    dict.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );

    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        span,
    )))
}

/// Convert a `SurfaceDeclaration` to an `Arc<Thunk>` containing its dict representation.
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
) -> EvalResult<Arc<Thunk>> {
    let mut dict = IndexMap::new();
    let variant_tag: &str;

    match decl {
        SurfaceDeclaration::TypeAlias { params, body } => {
            variant_tag = "TypeAlias";
            if !params.is_empty() {
                let params_arcs: Vec<Arc<Thunk>> = params
                    .iter()
                    .map(|(name, _ann)| Arc::new(Thunk::value(string_val(name), span.clone())))
                    .collect();
                dict.insert(
                    HashableValue::Str("params".into()),
                    list_to_thunk_id(params_arcs.into_iter(), span.clone(), ctx)?,
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
                Arc::new(Thunk::value(string_val(name), span.clone())),
            );
            // params: integer-keyed list of param name strings
            let params_dict: IndexMap<HashableValue, Arc<Thunk>> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        HashableValue::Int(i as i64),
                        Arc::new(Thunk::value(string_val(p), span.clone())),
                    )
                })
                .collect();
            dict.insert(
                HashableValue::Str("params".into()),
                Arc::new(Thunk::value(
                    Value::Dict {
                        entries: params_dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                )),
            );
            // superclasses: Seq of [class-name, param1, param2, ...] seqs
            // Only emitted when non-empty (e.g. [class Ord a where Eq a] → [[Eq a]])
            if !superclasses.is_empty() {
                let pair_arcs: Vec<Arc<Thunk>> = superclasses
                    .iter()
                    .map(|(class_name, var_names)| {
                        let mut entries: Vec<(HashableValue, Arc<Thunk>)> = vec![(
                            HashableValue::Int(0),
                            Arc::new(Thunk::value(string_val(class_name), span.clone())),
                        )];
                        for (i, var_name) in var_names.iter().enumerate() {
                            entries.push((
                                HashableValue::Int((i + 1) as i64),
                                Arc::new(Thunk::value(string_val(var_name), span.clone())),
                            ));
                        }
                        let inner: IndexMap<HashableValue, Arc<Thunk>> =
                            entries.into_iter().collect();
                        Arc::new(Thunk::value(
                            Value::Dict {
                                entries: inner,
                                type_val: crate::value::unknown_type_val(),
                            },
                            span.clone(),
                        ))
                    })
                    .collect();
                dict.insert(
                    HashableValue::Str("superclasses".into()),
                    list_to_thunk_id(pair_arcs.into_iter(), span.clone(), ctx)?,
                );
            }
            // methods: string-keyed dict of method expression dicts
            // Keys are SurfaceExpression::StringLiteral bare words; values are the full entry value nodes.
            let methods_dict: IndexMap<HashableValue, Arc<Thunk>> = methods
                .iter()
                .map(
                    |method| -> EvalResult<Option<(HashableValue, Arc<Thunk>)>> {
                        if let Some(key) = method.node.key.as_ref() {
                            if let SurfaceExpression::StringLiteral {
                                content: key_str, ..
                            } = &key.expr
                            {
                                let thunk =
                                    surface_node_to_thunk_id(&method.node.value, opts, ctx)?;
                                return Ok(Some((
                                    HashableValue::Str(Arc::from(key_str.as_str())),
                                    thunk,
                                )));
                            }
                        }
                        Ok(None)
                    },
                )
                .collect::<EvalResult<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect();
            dict.insert(
                HashableValue::Str("methods".into()),
                Arc::new(Thunk::value(
                    Value::Dict {
                        entries: methods_dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                )),
            );
            // determines: optional integer-keyed list of expression dicts
            if !determines.is_empty() {
                let determines_dict: IndexMap<HashableValue, Arc<Thunk>> = determines
                    .iter()
                    .enumerate()
                    .map(|(i, fd_node)| -> EvalResult<(HashableValue, Arc<Thunk>)> {
                        Ok((
                            HashableValue::Int(i as i64),
                            surface_node_to_thunk_id(fd_node, opts, ctx)?,
                        ))
                    })
                    .collect::<EvalResult<Vec<_>>>()?
                    .into_iter()
                    .collect();
                dict.insert(
                    HashableValue::Str("determines".into()),
                    Arc::new(Thunk::value(
                        Value::Dict {
                            entries: determines_dict,
                            type_val: crate::value::unknown_type_val(),
                        },
                        span.clone(),
                    )),
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
                    Arc::new(Thunk::value(
                        Value::Int {
                            n: 1,
                            type_val: crate::value::unknown_type_val(),
                        },
                        span.clone(),
                    )),
                );
            }
        }

        SurfaceDeclaration::InstanceDecl { class_name, arms } => {
            variant_tag = "InstanceDecl";
            dict.insert(
                HashableValue::Str("class".into()),
                Arc::new(Thunk::value(
                    string_val(&class_decl_name(class_name)),
                    span.clone(),
                )),
            );
            // arms: integer-keyed list of {pattern, methods} dicts
            let arms_dict: IndexMap<HashableValue, Arc<Thunk>> = arms
                .iter()
                .enumerate()
                .map(
                    |(i, (pattern_node, methods))| -> EvalResult<(HashableValue, Arc<Thunk>)> {
                        let mut arm_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                        arm_dict.insert(
                            HashableValue::Str("pattern".into()),
                            surface_node_to_thunk_id(pattern_node, opts, ctx)?,
                        );
                        // methods: string-keyed dict matching ClassDecl.methods format
                        let methods_dict: IndexMap<HashableValue, Arc<Thunk>> = methods
                            .iter()
                            .map(
                                |method| -> EvalResult<Option<(HashableValue, Arc<Thunk>)>> {
                                    if let Some(key) = method.node.key.as_ref() {
                                        if let SurfaceExpression::StringLiteral {
                                            content: key_str,
                                            ..
                                        } = &key.expr
                                        {
                                            let thunk = surface_node_to_thunk_id(
                                                &method.node.value,
                                                opts,
                                                ctx,
                                            )?;
                                            return Ok(Some((
                                                HashableValue::Str(Arc::from(key_str.as_str())),
                                                thunk,
                                            )));
                                        }
                                    }
                                    Ok(None)
                                },
                            )
                            .collect::<EvalResult<Vec<_>>>()?
                            .into_iter()
                            .flatten()
                            .collect();
                        arm_dict.insert(
                            HashableValue::Str("methods".into()),
                            Arc::new(Thunk::value(
                                Value::Dict {
                                    entries: methods_dict,
                                    type_val: crate::value::unknown_type_val(),
                                },
                                span.clone(),
                            )),
                        );
                        Ok((
                            HashableValue::Int(i as i64),
                            Arc::new(Thunk::value(
                                Value::Dict {
                                    entries: arm_dict,
                                    type_val: crate::value::unknown_type_val(),
                                },
                                span.clone(),
                            )),
                        ))
                    },
                )
                .collect::<EvalResult<Vec<_>>>()?
                .into_iter()
                .collect();
            dict.insert(
                HashableValue::Str("arms".into()),
                Arc::new(Thunk::value(
                    Value::Dict {
                        entries: arms_dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                )),
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
                Arc::new(Thunk::value(string_val(name), span.clone())),
            );
            dict.insert(
                HashableValue::Str("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            if let Some(msg) = message {
                dict.insert(
                    HashableValue::Str("message".into()),
                    Arc::new(Thunk::value(string_val(msg), span.clone())),
                );
            }
        }

        SurfaceDeclaration::Splice(forms) => {
            variant_tag = "Splice";
            let form_arcs: Vec<Arc<Thunk>> = forms
                .iter()
                .map(|form| surface_node_to_thunk_id(form, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                HashableValue::Str("forms".into()),
                Arc::new(Thunk::value(
                    Value::Dict {
                        entries: form_arcs
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| (HashableValue::Int(i as i64), v))
                            .collect(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                )),
            );
        }
    }

    dict.insert(
        HashableValue::Str("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );
    let payload = Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        span.clone(),
    ));
    Ok(Arc::new(Thunk::value(
        Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(variant_tag),
            payload: Some(payload),
        },
        span,
    )))
}

/// Override the `bare` field in an `Expr.Literal` (kind: "str") variant's payload.
///
/// The `inject(bare = true)` attribute on `SurfaceExpression::StringLiteral` always generates
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
    _ctx: &Arc<crate::eval::EvalContext>,
) -> Value {
    if let Value::Variant {
        ref type_val,
        ref ctor,
        payload: Some(ref payload_thunk),
    } = val
    {
        if ctor.as_ref() == "Expr.Literal" {
            // payload_thunk is always Thunk::value(...) — no eval errors possible.
            if let Value::Dict {
                entries: mut dict, ..
            } = payload_thunk
                .require_value()
                .expect("override_bare_in_literal_variant: payload thunk is always Thunk::value — impossible eval error")
                .clone()
            {
                let new_bare_thunk = Arc::new(Thunk::value(
                    Value::Int {
                        n: if bare { 1 } else { 0 },
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                ));
                dict.insert(HashableValue::Str("bare".into()), new_bare_thunk);
                let new_payload = Arc::new(Thunk::value(
                    Value::Dict {
                        entries: dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                ));
                return Value::Variant {
                    type_val: Arc::clone(type_val),
                    ctor: ctor.clone(),
                    payload: Some(new_payload),
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
) -> EvalResult<Arc<Thunk>> {
    let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (i, entry) in entries.iter().enumerate() {
        // key: Some(node) → Expr.* variant with corrected bare flag, None → null
        let key_val = match &entry.node.key {
            Some(key_node) => {
                let mut val = SurfaceExpression::to_expr_variant(key_node, ctx);
                // Override bare for string literal keys: check source text at span offset.
                if let SurfaceExpression::StringLiteral { .. } = &key_node.expr {
                    let is_bare = match opts.source {
                        Some(source) => {
                            // Span starts at the opening quote for quoted strings,
                            // or at the first identifier char for bare words.
                            // Check the character AT the span start using line/col.
                            let first_char = get_char_at(
                                source,
                                key_node.span.start_line,
                                key_node.span.start_col,
                            );
                            first_char != Some('"')
                        }
                        None => false,
                    };
                    val = override_bare_in_literal_variant(val, is_bare, &key_node.span, ctx);
                }
                val
            }
            None => Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
        };
        let key_thunk = Arc::new(Thunk::value(key_val, entry.span.clone()));

        // value: Expr.* variant
        let val_val = SurfaceExpression::to_expr_variant(&entry.node.value, ctx);
        let val_thunk = Arc::new(Thunk::value(val_val, entry.span.clone()));

        // Build payload dict for Expr.Entry
        let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        payload.insert(HashableValue::Str("key".into()), key_thunk);
        payload.insert(HashableValue::Str("value".into()), val_thunk);

        // Add comment fields when comment maps are provided
        if let Some(ref comment_maps) = opts.comments {
            let key = crate::parser::span_key(entry.span.start_line, entry.span.start_col);

            // leading-comments: list of comment strings before this entry
            if let Some(comments) = comment_maps.leading_comments.get(&key) {
                if !comments.is_empty() {
                    let comment_arcs: Vec<Arc<Thunk>> = comments
                        .iter()
                        .map(|c| Arc::new(Thunk::value(string_val(c), entry.span.clone())))
                        .collect();
                    let comments_thunk =
                        list_to_thunk_id(comment_arcs.into_iter(), entry.span.clone(), ctx)?;
                    payload.insert(
                        HashableValue::Str("leading-comments".into()),
                        comments_thunk,
                    );
                }
            }

            // blank-before: true when there is a blank line before this entry
            let is_blank = comment_maps.blank_before.get(&key) == Some(&true);
            let blank_thunk = Arc::new(Thunk::value(
                Value::Int {
                    n: if is_blank { 1 } else { 0 },
                    type_val: crate::value::unknown_type_val(),
                },
                entry.span.clone(),
            ));
            payload.insert(HashableValue::Str("blank-before".into()), blank_thunk);
        }

        // When no comment maps, always include blank-before: false as default
        if opts.comments.is_none() {
            let blank_thunk = Arc::new(Thunk::value(
                Value::Int {
                    n: 0,
                    type_val: crate::value::unknown_type_val(),
                },
                entry.span.clone(),
            ));
            payload.insert(HashableValue::Str("blank-before".into()), blank_thunk);
        }

        let payload_thunk = Arc::new(Thunk::value(
            Value::Dict {
                entries: payload,
                type_val: crate::value::unknown_type_val(),
            },
            entry.span.clone(),
        ));
        // Expr.Entry variant
        let entry_variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from("Expr.Entry"),
            payload: Some(payload_thunk),
        };
        let entry_thunk = Arc::new(Thunk::value(entry_variant, entry.span.clone()));
        dict.insert(HashableValue::Int(i as i64), entry_thunk);
    }
    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: dict,
            type_val: crate::value::unknown_type_val(),
        },
        rust_span!(),
    )))
}

/// Convert a SurfaceNode to an `Arc<Thunk>` containing its `Expr.*` variant representation.
/// Uses `surface_node_to_expr_variant` — produces `Expr.*` variants consumable by `builtin-eval`.
///
/// For Dict expressions, uses `alloc_entry_list_with_opts` when opts is provided to
/// correctly handle the `bare` flag on string keys and add comment metadata to entries.
fn surface_node_to_thunk_id(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    // For Dict nodes, build the Expr.Dict variant with opts-aware entry conversion
    // so that bare flags and comment fields are correctly populated.
    let val = if let SurfaceExpression::Dict(entries) = &node.expr {
        let entries_thunk = alloc_entry_list_with_opts(entries, opts, ctx)?;
        let mut payload: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        payload.insert(HashableValue::Str("entries".into()), entries_thunk);
        let payload_thunk = Arc::new(Thunk::value(
            Value::Dict {
                entries: payload,
                type_val: crate::value::unknown_type_val(),
            },
            node.span.clone(),
        ));
        inject_span_into_expr_variant(
            Value::Variant {
                type_val: crate::value::unknown_type_val(),
                ctor: Arc::from("Expr.Dict"),
                payload: Some(payload_thunk),
            },
            &node.span,
            ctx,
        )
    } else {
        surface_node_to_expr_variant(node, ctx)
    };
    Ok(Arc::new(Thunk::value(val, node.span.clone())))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        crate::eval::EvalContext::new()
    }

    /// Peek a settled thunk's value. Panics with a clear message if the thunk holds an error.
    fn peek_value(thunk: &Thunk) -> Option<Value> {
        match thunk.peek_result() {
            Some(Ok(v)) => Some(v.clone()),
            Some(Err(e)) => panic!("unexpected error in test thunk: {e}"),
            None => None,
        }
    }

    /// Peel a `Value::Variant` to its payload dict.
    /// Panics with a helpful message if the value is not a Variant with a Dict payload.
    fn peel_variant(
        val: Value,
        _ctx: &Arc<crate::eval::EvalContext>,
    ) -> (String, IndexMap<HashableValue, Arc<Thunk>>) {
        match val {
            Value::Variant {
                ctor,
                payload: Some(payload_thunk),
                ..
            } => match peek_value(&payload_thunk) {
                Some(Value::Dict { entries: map, .. }) => (ctor.as_ref().to_string(), map),
                other => panic!("expected Dict payload for Variant, got {:?}", other),
            },
            other => panic!("expected Variant, got {:?}", other),
        }
    }

    fn test_file(_src: &str) -> Arc<str> {
        Arc::from(file!())
    }

    #[test]
    fn test_surface_program_to_dict_file_schema_version() {
        use crate::parser::parse;

        let src = "1";
        let parse_output = parse(src, test_file(src)).unwrap();
        let opts = AstToDictOpts::default();
        let ctx = test_ctx();
        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        match peek_value(&thunk) {
            Some(Value::Dict { entries: map, .. }) => {
                let type_thunk = map.get(&HashableValue::Str("type".into())).unwrap().clone();
                assert_eq!(peek_value(&type_thunk), Some(string_val("file")));

                let version_thunk = map
                    .get(&HashableValue::Str("schema-version".into()))
                    .unwrap()
                    .clone();
                assert_eq!(
                    peek_value(&version_thunk),
                    Some(Value::Int {
                        n: 1,
                        type_val: crate::value::unknown_type_val()
                    })
                );
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn test_bare_flag_on_bare_word_strings() {
        use crate::parser::parse;

        // Parse "[foo: 1]" — the key "foo" should have bare: true
        let input = "[foo: 1]";
        let parse_output = parse(input, test_file(input)).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the first document's first expression (the dict)
        match peek_value(&thunk) {
            Some(Value::Dict {
                entries: file_dict, ..
            }) => {
                let docs_thunk = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap()
                    .clone();
                match peek_value(&docs_thunk) {
                    Some(Value::Dict {
                        entries: docs_list, ..
                    }) => {
                        let doc_thunk = docs_list.get(&HashableValue::Int(0)).unwrap().clone();
                        match peek_value(&doc_thunk) {
                            Some(Value::Dict {
                                entries: doc_dict, ..
                            }) => {
                                let exprs_thunk = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap()
                                    .clone();
                                match peek_value(&exprs_thunk) {
                                    Some(Value::Dict {
                                        entries: exprs_list,
                                        ..
                                    }) => {
                                        let expr_thunk =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap().clone();
                                        let expr_val =
                                            peek_value(&expr_thunk).expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            // Get the entries list
                                            let entries_thunk = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap()
                                                .clone();
                                            match peek_value(&entries_thunk) {
                                                Some(Value::Dict {
                                                    entries: entries_list,
                                                    ..
                                                }) => {
                                                    let entry_thunk = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap()
                                                        .clone();
                                                    let entry_val = peek_value(&entry_thunk)
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Get the key expression
                                                        let key_thunk = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap()
                                                            .clone();
                                                        let key_val = peek_value(&key_thunk)
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        // Check bare: true
                                                        let bare_thunk = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing")
                                                            .clone();
                                                        assert_eq!(
                                                            peek_value(&bare_thunk),
                                                            Some(Value::Int { n: 1, type_val: crate::value::unknown_type_val() }),
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
        let parse_output = parse(input, test_file(input)).unwrap();
        let opts = AstToDictOpts {
            source: Some(input),
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false
        match peek_value(&thunk) {
            Some(Value::Dict {
                entries: file_dict, ..
            }) => {
                let docs_thunk = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap()
                    .clone();
                match peek_value(&docs_thunk) {
                    Some(Value::Dict {
                        entries: docs_list, ..
                    }) => {
                        let doc_thunk = docs_list.get(&HashableValue::Int(0)).unwrap().clone();
                        match peek_value(&doc_thunk) {
                            Some(Value::Dict {
                                entries: doc_dict, ..
                            }) => {
                                let exprs_thunk = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap()
                                    .clone();
                                match peek_value(&exprs_thunk) {
                                    Some(Value::Dict {
                                        entries: exprs_list,
                                        ..
                                    }) => {
                                        let expr_thunk =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap().clone();
                                        let expr_val =
                                            peek_value(&expr_thunk).expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_thunk = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap()
                                                .clone();
                                            match peek_value(&entries_thunk) {
                                                Some(Value::Dict {
                                                    entries: entries_list,
                                                    ..
                                                }) => {
                                                    let entry_thunk = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap()
                                                        .clone();
                                                    let entry_val = peek_value(&entry_thunk)
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        let key_thunk = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap()
                                                            .clone();
                                                        let key_val = peek_value(&key_thunk)
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        let bare_thunk = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing")
                                                            .clone();
                                                        assert_eq!(
                                                            peek_value(&bare_thunk),
                                                            Some(Value::Int { n: 0, type_val: crate::value::unknown_type_val() }),
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
        let parse_output = parse(input, test_file(input)).unwrap();
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
        match peek_value(&thunk) {
            Some(Value::Dict {
                entries: file_dict, ..
            }) => {
                let docs_thunk = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap()
                    .clone();
                match peek_value(&docs_thunk) {
                    Some(Value::Dict {
                        entries: docs_list, ..
                    }) => {
                        let doc_thunk = docs_list.get(&HashableValue::Int(0)).unwrap().clone();
                        match peek_value(&doc_thunk) {
                            Some(Value::Dict {
                                entries: doc_dict, ..
                            }) => {
                                let exprs_thunk = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap()
                                    .clone();
                                match peek_value(&exprs_thunk) {
                                    Some(Value::Dict {
                                        entries: exprs_list,
                                        ..
                                    }) => {
                                        let expr_thunk =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap().clone();
                                        let expr_val =
                                            peek_value(&expr_thunk).expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_thunk = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap()
                                                .clone();
                                            match peek_value(&entries_thunk) {
                                                Some(Value::Dict {
                                                    entries: entries_list,
                                                    ..
                                                }) => {
                                                    let entry_thunk = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap()
                                                        .clone();
                                                    let entry_val = peek_value(&entry_thunk)
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Check for leading-comments field
                                                        let comments_thunk = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "leading-comments".into(),
                                                            ))
                                                            .expect(
                                                                "leading-comments field missing",
                                                            )
                                                            .clone();
                                                        match peek_value(&comments_thunk) {
                                                            Some(Value::Dict {
                                                                entries: comments_list,
                                                                ..
                                                            }) => {
                                                                let comment_thunk = comments_list
                                                                    .get(&HashableValue::Int(0))
                                                                    .expect("comment 0 missing")
                                                                    .clone();
                                                                assert_eq!(
                                                                    peek_value(&comment_thunk),
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
        let parse_output = parse(input, test_file(input)).unwrap();

        // Mark 'b' (line 2, col 1) as having a blank line before it.
        // In "[a: 1\nb: 2]": 'b' is on line 2, column 1.
        let mut blank_before_map = BTreeMap::new();
        blank_before_map.insert(crate::parser::span_key(2, 1), true); // mark 'b' as having a blank line before it
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
        match peek_value(&thunk) {
            Some(Value::Dict {
                entries: file_dict, ..
            }) => {
                let docs_thunk = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap()
                    .clone();
                match peek_value(&docs_thunk) {
                    Some(Value::Dict {
                        entries: docs_list, ..
                    }) => {
                        let doc_thunk = docs_list.get(&HashableValue::Int(0)).unwrap().clone();
                        match peek_value(&doc_thunk) {
                            Some(Value::Dict {
                                entries: doc_dict, ..
                            }) => {
                                let exprs_thunk = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap()
                                    .clone();
                                match peek_value(&exprs_thunk) {
                                    Some(Value::Dict {
                                        entries: exprs_list,
                                        ..
                                    }) => {
                                        let expr_thunk =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap().clone();
                                        let expr_val =
                                            peek_value(&expr_thunk).expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_thunk = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap()
                                                .clone();
                                            match peek_value(&entries_thunk) {
                                                Some(Value::Dict {
                                                    entries: entries_list,
                                                    ..
                                                }) => {
                                                    let entry_thunk = entries_list
                                                        .get(&HashableValue::Int(1))
                                                        .unwrap()
                                                        .clone();
                                                    let entry_val = peek_value(&entry_thunk)
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        // Check blank-before: true
                                                        let blank_thunk = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "blank-before".into(),
                                                            ))
                                                            .expect("blank-before field missing")
                                                            .clone();
                                                        assert_eq!(
                                                            peek_value(&blank_thunk),
                                                            Some(Value::Int { n: 1, type_val: crate::value::unknown_type_val() }),
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
        let parse_output = parse(input, test_file(input)).unwrap();
        let opts = AstToDictOpts {
            source: None,
            comments: None,
        };
        let ctx = test_ctx();

        let thunk = surface_program_to_dict(&parse_output.program, &opts, &ctx).unwrap();

        // Navigate to the key and check bare: false (default when source is None)
        match peek_value(&thunk) {
            Some(Value::Dict {
                entries: file_dict, ..
            }) => {
                let docs_thunk = file_dict
                    .get(&HashableValue::Str("documents".into()))
                    .unwrap()
                    .clone();
                match peek_value(&docs_thunk) {
                    Some(Value::Dict {
                        entries: docs_list, ..
                    }) => {
                        let doc_thunk = docs_list.get(&HashableValue::Int(0)).unwrap().clone();
                        match peek_value(&doc_thunk) {
                            Some(Value::Dict {
                                entries: doc_dict, ..
                            }) => {
                                let exprs_thunk = doc_dict
                                    .get(&HashableValue::Str("expressions".into()))
                                    .unwrap()
                                    .clone();
                                match peek_value(&exprs_thunk) {
                                    Some(Value::Dict {
                                        entries: exprs_list,
                                        ..
                                    }) => {
                                        let expr_thunk =
                                            exprs_list.get(&HashableValue::Int(0)).unwrap().clone();
                                        let expr_val =
                                            peek_value(&expr_thunk).expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_thunk = dict_node
                                                .get(&HashableValue::Str("entries".into()))
                                                .unwrap()
                                                .clone();
                                            match peek_value(&entries_thunk) {
                                                Some(Value::Dict {
                                                    entries: entries_list,
                                                    ..
                                                }) => {
                                                    let entry_thunk = entries_list
                                                        .get(&HashableValue::Int(0))
                                                        .unwrap()
                                                        .clone();
                                                    let entry_val = peek_value(&entry_thunk)
                                                        .expect("entry not materialized");
                                                    let (_entry_tag, entry_dict) =
                                                        peel_variant(entry_val, &ctx);
                                                    {
                                                        let key_thunk = entry_dict
                                                            .get(&HashableValue::Str("key".into()))
                                                            .unwrap()
                                                            .clone();
                                                        let key_val = peek_value(&key_thunk)
                                                            .expect("key not materialized");
                                                        let (_key_tag, key_dict) =
                                                            peel_variant(key_val, &ctx);
                                                        let bare_thunk = key_dict
                                                            .get(&HashableValue::Str("bare".into()))
                                                            .expect("bare field missing")
                                                            .clone();
                                                        assert_eq!(
                                                            peek_value(&bare_thunk),
                                                            Some(Value::Int { n: 0, type_val: crate::value::unknown_type_val() }),
                                                            "bare should be false when source is None"
                                                        );

                                                        // Check that blank-before is still present (always included)
                                                        let blank_thunk = entry_dict
                                                            .get(&HashableValue::Str(
                                                                "blank-before".into(),
                                                            ))
                                                            .expect("blank-before field missing")
                                                            .clone();
                                                        assert_eq!(
                                                            peek_value(&blank_thunk),
                                                            Some(Value::Int { n: 0, type_val: crate::value::unknown_type_val() }),
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
