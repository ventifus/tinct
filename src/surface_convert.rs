//! Dict-to-Surface conversion for macro expansion and quasiquoting.
//!
//! Converts dict representations (from `[quote expr]` or macro results) back to
//! `SurfaceNode` AST. This is the reverse direction of `surface_program_to_dict`.

use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{
    Annotation, DotKey, Position, Span, Spanned, SurfaceEntry, SurfaceExpression, SurfaceNamedArg,
    SurfaceNode, SurfaceParam,
};
use crate::value::{Key, Value};

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

/// Convert a dict representation back to a SurfaceNode.
///
/// Reads the Variant tag or `type:` field and dispatches to the native `SurfaceExpression`
/// constructor. All variants are handled natively. Unknown tags return a hard `AstError`;
/// there is no Expr-based fallback path.
pub fn dict_to_surface_node(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    // Short-circuit: Value::Expression is already a SurfaceNode, no dict deserialization needed.
    // Post-runtime-v2, [quote expr] returns Value::Expression(SurfaceNode) directly.
    if let Value::Expression(node) = val {
        return Ok(Arc::clone(node));
    }
    dict_to_surface_node_inner(val, ctx)
}

/// Inner implementation of `dict_to_surface_node`.
///
/// Extracts the Variant tag (or legacy `type:` string) and dispatches to the native
/// `SurfaceExpression` constructor. All known variants are handled here. Unknown tags
/// return a hard `AstError` — there is no Expr-based fallback bridge.
///
/// Variants handled here:
/// - `"VarRef"`, `"Literal"` (Int/Float/Bool/Str), `"Dict"`, `"Fn"`,
///   `"Call"`, `"DotAccess"`, `"Pipe"`
fn dict_to_surface_node_inner(
    val: &Value,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Arc<SurfaceNode>, AstError> {
    // Accept both Variant (canonical form from surface_node_to_thunk_id) and
    // Dict (legacy form with type: field).
    let (tag, dict) = match val {
        Value::Variant { tag, payload } => {
            let payload_thunk = payload.as_ref().ok_or_else(|| AstError {
                message: format!("Expr variant {} has no payload", tag),
                field_path: vec![],
            })?;
            let payload_val = ctx
                .get_thunk(*payload_thunk)
                .try_get_materialized()
                .ok_or_else(|| AstError {
                    message: "variant payload is not materialized".into(),
                    field_path: vec![],
                })?;
            match payload_val {
                Value::Dict(d) => (tag.clone(), d),
                _ => {
                    return Err(AstError {
                        message: format!(
                            "Expr variant payload must be Dict, got {}",
                            payload_val.type_name()
                        ),
                        field_path: vec![],
                    })
                }
            }
        }
        Value::Dict(d) => {
            let type_str = get_string_field(d, "type", &[], ctx)?;
            (type_str, d.clone())
        }
        _ => {
            return Err(AstError {
                message: "expected Variant or Dict".into(),
                field_path: vec![],
            })
        }
    };

    let span = extract_span(&dict, ctx).unwrap_or_else(Span::origin);

    let expr = match tag.as_str() {
        // ---- Literals (Int, Float, Bool, Str) ----
        "literal" | "Literal" => {
            let kind = get_string_field(&dict, "kind", &["type"], ctx)?;
            match kind.as_str() {
                "int" => {
                    let value = get_int_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Int(value)
                }
                "float" => {
                    let value = get_float_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Float(value)
                }
                "bool" => {
                    let value = get_bool_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Bool(value)
                }
                "str" => {
                    let value = get_string_field(&dict, "value", &["kind"], ctx)?;
                    SurfaceExpression::Str(value)
                }
                _ => {
                    return Err(AstError {
                        message: format!("unknown literal kind: {}", kind),
                        field_path: vec!["kind".into()],
                    })
                }
            }
        }

        // ---- VarRef ----
        "var" | "VarRef" => {
            let name = get_string_field(&dict, "name", &["type"], ctx)?;
            SurfaceExpression::VarRef {
                name,
                escaped: false,
            }
        }

        // ---- DotAccess ----
        "dot-access" | "DotAccess" => {
            let target_val = get_dict_field(&dict, "target", &["type"], ctx)?;
            let target = dict_to_surface_node_inner(&target_val, ctx)?;
            let field_val = get_field(&dict, "field", &["type"], ctx)?;
            let field = match field_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => DotKey::Ident(source[start..end].to_string()),
                Value::Int(n) => DotKey::Int(n),
                _ => {
                    return Err(AstError {
                        message: "field must be String or Int".into(),
                        field_path: vec!["field".into()],
                    })
                }
            };
            SurfaceExpression::DotAccess {
                expr: target,
                field,
            }
        }

        // ---- Pipe ----
        "pipe" | "Pipe" => {
            let lhs_val = get_dict_field(&dict, "lhs", &["type"], ctx)?;
            let rhs_val = get_dict_field(&dict, "rhs", &["type"], ctx)?;
            SurfaceExpression::Pipe {
                lhs: dict_to_surface_node_inner(&lhs_val, ctx)?,
                rhs: dict_to_surface_node_inner(&rhs_val, ctx)?,
            }
        }

        // ---- Dict ----
        "dict" | "Dict" => {
            let entries_val = get_dict_field(&dict, "entries", &["type"], ctx)?;
            let entries_list = extract_list(&entries_val, &["entries"], ctx)?;
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let entry = dict_to_surface_entry(&entry_val, &["entries", &i_str], ctx)?;
                entries.push(entry);
            }
            SurfaceExpression::Dict(entries)
        }

        // ---- Call ----
        "call" | "Call" => {
            let fn_val = get_dict_field(&dict, "fn", &["type"], ctx)?;
            let func = dict_to_surface_node_inner(&fn_val, ctx)?;

            let args_val = get_dict_field(&dict, "args", &["type"], ctx)?;
            let args_list = extract_list(&args_val, &["args"], ctx)?;
            let mut args = Vec::new();
            for arg_val in args_list {
                args.push(dict_to_surface_node_inner(&arg_val, ctx)?);
            }

            let named_args_val = get_dict_field(&dict, "named-args", &["type"], ctx)?;
            let named_args_list = extract_list(&named_args_val, &["named-args"], ctx)?;
            let mut named_args = Vec::new();
            for (i, na_val) in named_args_list.into_iter().enumerate() {
                let i_str = i.to_string();
                named_args.push(dict_to_surface_named_arg(
                    &na_val,
                    &["named-args", &i_str],
                    ctx,
                )?);
            }

            let implied = get_bool_field(&dict, "implied", &["type"], ctx)?;

            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            }
        }

        // ---- Fn ----
        "fn" | "Fn" => {
            let params_val = get_dict_field(&dict, "params", &["type"], ctx)?;
            let params_list = extract_list(&params_val, &["params"], ctx)?;
            let mut params = Vec::new();
            for (i, param_val) in params_list.into_iter().enumerate() {
                let i_str = i.to_string();
                params.push(dict_to_surface_param(&param_val, &["params", &i_str], ctx)?);
            }

            let return_ann = match get_optional_dict_field(&dict, "return-ann", ctx)? {
                Some(ann_val) if !is_empty_dict(&ann_val) => {
                    Some(dict_to_annotation(&ann_val, &["return-ann"], ctx)?)
                }
                _ => None,
            };

            let body_val = get_dict_field(&dict, "body", &["type"], ctx)?;
            let body = dict_to_surface_node_inner(&body_val, ctx)?;

            let desugared = get_bool_field(&dict, "desugared", &["type"], ctx)?;

            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared,
            }
        }

        // ---- TypeAssert ----
        "type-assert" | "TypeAssert" => {
            let annotation_val = get_dict_field(&dict, "annotation", &["type"], ctx)?;
            let annotation = dict_to_annotation(&annotation_val, &["annotation"], ctx)?;
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node_inner(&expr_val, ctx)?;
            SurfaceExpression::TypeAssert { annotation, expr }
        }

        // ---- Annotated ----
        "annotated" | "Annotated" => {
            let name = get_string_field(&dict, "name", &["type"], ctx)?;
            let annotation_val = get_dict_field(&dict, "annotation", &["type"], ctx)?;
            let annotation = dict_to_annotation(&annotation_val, &["annotation"], ctx)?;
            SurfaceExpression::Annotated { name, annotation }
        }

        // ---- Rest ----
        "rest" | "Rest" => {
            let name_val = get_dict_field(&dict, "name", &["type"], ctx)?;
            let name = match name_val {
                Value::Dict(ref d) if d.is_empty() => None,
                Value::String {
                    ref source,
                    start,
                    end,
                } => Some(source[start..end].to_string()),
                _ => {
                    return Err(AstError {
                        message: "name must be String or empty dict".into(),
                        field_path: vec!["name".into()],
                    })
                }
            };
            SurfaceExpression::Rest(name)
        }

        // ---- Quote ----
        "quote" | "Quote" => {
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node_inner(&expr_val, ctx)?;
            SurfaceExpression::Quote(expr)
        }

        // ---- Unquote ----
        "unquote" | "Unquote" => {
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node_inner(&expr_val, ctx)?;
            SurfaceExpression::Unquote(expr)
        }

        // ---- UnquoteSplice ----
        "unquote-splice" | "UnquoteSplice" => {
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node_inner(&expr_val, ctx)?;
            SurfaceExpression::UnquoteSplice(expr)
        }

        // ---- Sequential ----
        "sequential" | "Sequential" => {
            let exprs_val = get_dict_field(&dict, "exprs", &["type"], ctx)?;
            let exprs_list = extract_list(&exprs_val, &["exprs"], ctx)?;
            let mut exprs = Vec::new();
            for expr_val in exprs_list {
                exprs.push(dict_to_surface_node_inner(&expr_val, ctx)?);
            }
            SurfaceExpression::Sequential(exprs)
        }

        // ---- PatternDecl ----
        "pattern-decl" | "PatternDecl" => {
            let bindings_val = get_dict_field(&dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list {
                bindings.push(dict_to_surface_node_inner(&binding_val, ctx)?);
            }
            SurfaceExpression::PatternDecl { bindings }
        }

        // ---- LetDecl ----
        "let-decl" | "LetDecl" => {
            let bindings_val = get_dict_field(&dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list {
                bindings.push(dict_to_surface_node_inner(&binding_val, ctx)?);
            }
            SurfaceExpression::LetDecl { bindings }
        }

        // ---- Placeholder ----
        "placeholder" | "Placeholder" => SurfaceExpression::Placeholder,

        // ---- Error ----
        "ast-error" | "AstError" => {
            let error_span_val = get_dict_field(&dict, "span", &["type"], ctx)?;
            let error_span = match &error_span_val {
                Value::Dict(span_dict) => {
                    let start_id =
                        span_dict
                            .get(&Key::String("start".into()))
                            .ok_or_else(|| AstError {
                                message: "span dict missing 'start' field".into(),
                                field_path: vec!["span".into()],
                            })?;
                    let start_thunk = ctx.get_thunk(*start_id);
                    let start_val = start_thunk.try_get_materialized().ok_or_else(|| AstError {
                        message: "span.start is not materialized".into(),
                        field_path: vec!["span".into(), "start".into()],
                    })?;

                    let end_id =
                        span_dict
                            .get(&Key::String("end".into()))
                            .ok_or_else(|| AstError {
                                message: "span dict missing 'end' field".into(),
                                field_path: vec!["span".into()],
                            })?;
                    let end_thunk = ctx.get_thunk(*end_id);
                    let end_val = end_thunk.try_get_materialized().ok_or_else(|| AstError {
                        message: "span.end is not materialized".into(),
                        field_path: vec!["span".into(), "end".into()],
                    })?;

                    let start = extract_position(&start_val, ctx).ok_or_else(|| AstError {
                        message: "invalid start position".into(),
                        field_path: vec!["span".into(), "start".into()],
                    })?;
                    let end = extract_position(&end_val, ctx).ok_or_else(|| AstError {
                        message: "invalid end position".into(),
                        field_path: vec!["span".into(), "end".into()],
                    })?;

                    Span::new(start, end)
                }
                _ => {
                    return Err(AstError {
                        message: "span must be Dict".into(),
                        field_path: vec!["span".into()],
                    })
                }
            };
            SurfaceExpression::Error(error_span)
        }

        // ---- TypeApp ----
        "type-app" | "TypeApp" => {
            let func_val = get_dict_field(&dict, "func", &["type"], ctx)?;
            let func = dict_to_surface_node_inner(&func_val, ctx)?;
            let arg_val = get_dict_field(&dict, "arg", &["type"], ctx)?;
            let arg = dict_to_surface_node_inner(&arg_val, ctx)?;
            SurfaceExpression::TypeApp { func, arg }
        }

        // ---- Match ----
        "match" | "Match" => {
            use crate::ast::SurfaceMatchArm;
            let scrutinee_val = get_dict_field(&dict, "scrutinee", &["type"], ctx)?;
            let scrutinee = dict_to_surface_node_inner(&scrutinee_val, ctx)?;

            let arms_val = get_dict_field(&dict, "arms", &["type"], ctx)?;
            let arms_list = extract_list(&arms_val, &["arms"], ctx)?;
            let mut arms = Vec::new();
            for (i, arm_val) in arms_list.into_iter().enumerate() {
                let i_str = i.to_string();
                let arm_dict = match arm_val {
                    Value::Dict(d) => d,
                    _ => {
                        return Err(AstError {
                            message: format!("match arm {} must be Dict", i),
                            field_path: vec!["arms".into(), i_str.clone()],
                        })
                    }
                };
                let pattern_val = get_dict_field(&arm_dict, "pattern", &["arms", &i_str], ctx)?;
                let pattern = dict_to_pattern(&pattern_val, &["arms", &i_str, "pattern"], ctx)?;
                let guard = match get_optional_dict_field(&arm_dict, "guard", ctx)? {
                    Some(guard_val) if !is_empty_dict(&guard_val) => {
                        Some(dict_to_surface_node_inner(&guard_val, ctx)?)
                    }
                    _ => None,
                };
                let body_val = get_dict_field(&arm_dict, "body", &["arms", &i_str], ctx)?;
                let body = dict_to_surface_node_inner(&body_val, ctx)?;
                arms.push(SurfaceMatchArm {
                    pattern,
                    guard,
                    body,
                });
            }
            SurfaceExpression::Match { scrutinee, arms }
        }

        // ---- CaseArm ----
        "case-arm" | "CaseArm" => {
            let pattern_val = get_dict_field(&dict, "pattern", &["type"], ctx)?;
            let pattern = dict_to_surface_node_inner(&pattern_val, ctx)?;
            let body_val = get_dict_field(&dict, "body", &["type"], ctx)?;
            let body = dict_to_surface_node_inner(&body_val, ctx)?;
            SurfaceExpression::CaseArm { pattern, body }
        }

        // ---- Unknown variant: hard error (all variants must be handled natively) ----
        _ => {
            return Err(AstError {
                message: format!("dict_to_surface_node: unknown node type: {}", tag),
                field_path: vec![],
            });
        }
    };

    Ok(Arc::new(SurfaceNode { expr, span }))
}

/// Convert a dict to a `Spanned<SurfaceEntry>`.
///
/// Surface-native reverse for `surface_entry_to_thunk_id`.
fn dict_to_surface_entry(
    val: &Value,
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
        _ => Some(dict_to_surface_node_inner(&key_val, ctx)?),
    };

    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = dict_to_surface_node_inner(&value_val, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(SurfaceEntry { key, value }, span))
}

/// Convert a dict to a `Spanned<SurfaceNamedArg>`.
///
/// Surface-native reverse for `surface_named_arg_to_thunk_id`.
fn dict_to_surface_named_arg(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceNamedArg>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "named-arg must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let name = get_string_field(dict, "name", path, ctx)?;
    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = dict_to_surface_node_inner(&value_val, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(SurfaceNamedArg { name, value }, span))
}

/// Convert a dict to a `Spanned<SurfaceParam>`.
///
/// Surface-native reverse for `surface_param_to_thunk_id`.
fn dict_to_surface_param(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<SurfaceParam>, AstError> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => {
            return Err(AstError {
                message: "param must be Dict".into(),
                field_path: path.iter().map(|s| s.to_string()).collect(),
            })
        }
    };

    let name = get_string_field(dict, "name", path, ctx)?;

    let annotation = match get_optional_dict_field(dict, "annotation", ctx)? {
        Some(ann_val) if !is_empty_dict(&ann_val) => {
            let mut new_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            new_path.push("annotation".to_string());
            let path_refs: Vec<&str> = new_path.iter().map(|s| s.as_str()).collect();
            Some(dict_to_annotation(&ann_val, &path_refs, ctx)?)
        }
        _ => None,
    };

    let variadic = get_bool_field(dict, "variadic", path, ctx)?;

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

    Ok(Spanned::new(
        SurfaceParam {
            name,
            annotation,
            variadic,
        },
        span,
    ))
}

/// Deserialize an `Annotation` from a dict produced by `annotation_to_thunk_id`.
///
/// Used by `dict_to_surface_node_inner` (Fn return annotation) and `dict_to_surface_param`.
/// The `"dict"` arm handles `Annotation::PropertyDict` using `dict_to_surface_entry`.
fn dict_to_annotation(
    val: &Value,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Spanned<Annotation>, AstError> {
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
            let mut entries = Vec::new();
            for (i, entry_val) in entries_list.into_iter().enumerate() {
                let mut entry_path = entries_path.clone();
                let i_str = i.to_string();
                entry_path.push(i_str.clone());
                let entry_path_refs: Vec<&str> = entry_path.iter().map(|s| s.as_str()).collect();
                let entry = dict_to_surface_entry(&entry_val, &entry_path_refs, ctx)?;
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

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);

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

    let span = extract_span(dict, ctx).unwrap_or_else(Span::origin);
    let kind = get_string_field(dict, "type", path, ctx)?;

    let pattern = match kind.as_str() {
        "wildcard" => Pattern::Wildcard,

        "variable" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Pattern::Variable(name)
        }

        "type_tag" => {
            let tag = get_string_field(dict, "tag", path, ctx)?;
            Pattern::TypeTag(tag)
        }

        "pin" => {
            let name = get_string_field(dict, "name", path, ctx)?;
            Pattern::Pin(name)
        }

        "literal" => {
            let value_val = get_field(dict, "value", path, ctx)?;
            let lit = match value_val {
                Value::Int(n) => LiteralPattern::Int(n),
                Value::Float(f) => LiteralPattern::Float(f),
                Value::Bool(b) => LiteralPattern::Bool(b),
                Value::String {
                    ref source,
                    start,
                    end,
                } => LiteralPattern::Str(source[start..end].to_string()),
                _ => {
                    return Err(AstError {
                        message: "literal pattern value must be Int, Float, Bool, or String".into(),
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

        "seq" => {
            let head_val = get_dict_field(dict, "head", path, ctx)?;
            let tail_val = get_dict_field(dict, "tail", path, ctx)?;
            let mut head_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            head_path.push("head".to_string());
            let mut tail_path: Vec<String> = path.iter().map(|s| s.to_string()).collect();
            tail_path.push("tail".to_string());
            let head_path_refs: Vec<&str> = head_path.iter().map(|s| s.as_str()).collect();
            let tail_path_refs: Vec<&str> = tail_path.iter().map(|s| s.as_str()).collect();
            let head = dict_to_pattern(&head_val, &head_path_refs, ctx)?;
            let tail = dict_to_pattern(&tail_val, &tail_path_refs, ctx)?;
            Pattern::Seq {
                head: Box::new(head),
                tail: Box::new(tail),
            }
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
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Value, AstError> {
    let thunk_id = dict.get(&Key::String(key.into())).ok_or_else(|| AstError {
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
    dict: &IndexMap<Key, ThunkId>,
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
        _ => Err(AstError {
            message: format!("field '{}' must be String", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_int_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<i64, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Int(n) => Ok(n),
        _ => Err(AstError {
            message: format!("field '{}' must be Int", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_float_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<f64, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Float(f) => Ok(f),
        _ => Err(AstError {
            message: format!("field '{}' must be Float", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_bool_field(
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    path: &[&str],
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<bool, AstError> {
    let val = get_field(dict, key, path, ctx)?;
    match val {
        Value::Bool(b) => Ok(b),
        _ => Err(AstError {
            message: format!("field '{}' must be Bool", key),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}

fn get_dict_field(
    dict: &IndexMap<Key, ThunkId>,
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
    dict: &IndexMap<Key, ThunkId>,
    key: &str,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Result<Option<Value>, AstError> {
    match dict.get(&Key::String(key.into())) {
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

fn extract_span(
    dict: &IndexMap<Key, ThunkId>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Span> {
    let span_thunk_id = dict.get(&Key::String("span".into()))?;
    let span_thunk = ctx.get_thunk(*span_thunk_id);
    let span_val = span_thunk.try_get_materialized()?;

    match span_val {
        Value::Dict(span_dict) => {
            let start_id = span_dict.get(&Key::String("start".into()))?;
            let start_thunk = ctx.get_thunk(*start_id);
            let start_val = start_thunk.try_get_materialized()?;

            let end_id = span_dict.get(&Key::String("end".into()))?;
            let end_thunk = ctx.get_thunk(*end_id);
            let end_val = end_thunk.try_get_materialized()?;

            let start = extract_position(&start_val, ctx)?;
            let end = extract_position(&end_val, ctx)?;

            Some(Span::new(start, end))
        }
        _ => None,
    }
}

fn extract_position(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> Option<Position> {
    match val {
        Value::Dict(dict) => {
            let line_id = dict.get(&Key::String("line".into()))?;
            let line_thunk = ctx.get_thunk(*line_id);
            let line = match line_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let col_id = dict.get(&Key::String("col".into()))?;
            let col_thunk = ctx.get_thunk(*col_id);
            let column = match col_thunk.try_get_materialized()? {
                Value::Int(n) => n as usize,
                _ => return None,
            };

            let offset_id = dict.get(&Key::String("offset".into()))?;
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
            // A list is represented as a dict with integer keys 0, 1, 2, ...
            let mut result = Vec::new();
            for i in 0.. {
                match d.get(&Key::Int(i)) {
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
            message: "expected list (dict with integer keys)".into(),
            field_path: path.iter().map(|s| s.to_string()).collect(),
        }),
    }
}
