//! AST-to-dict serialization and dict-to-Surface conversion for quasiquoting, macros, and formatter.
//!
//! Bidirectional conversion between AST nodes and tinct `Value::Variant` (Expr nodes) or `Value::Dict`
//! (structural nodes) matching the canonical schema in `doc/feature/ast-schema.md`.

use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{
    Annotation, DotKey, Position, Span, Spanned, Stage, SurfaceDeclaration, SurfaceDocument,
    SurfaceEntry, SurfaceExpression, SurfaceItem, SurfaceNamedArg, SurfaceNode, SurfaceParam,
    SurfaceProgram,
};
use crate::error::EvalResult;
use crate::value::{string_val, Key, Thunk, Value};

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
            let target = dict_to_surface_node(&target_val, ctx)?;
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
                lhs: dict_to_surface_node(&lhs_val, ctx)?,
                rhs: dict_to_surface_node(&rhs_val, ctx)?,
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
            let func = dict_to_surface_node(&fn_val, ctx)?;

            let args_val = get_dict_field(&dict, "args", &["type"], ctx)?;
            let args_list = extract_list(&args_val, &["args"], ctx)?;
            let mut args = Vec::new();
            for arg_val in args_list {
                args.push(dict_to_surface_node(&arg_val, ctx)?);
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
            let body = dict_to_surface_node(&body_val, ctx)?;

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
            let expr = dict_to_surface_node(&expr_val, ctx)?;
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
            let expr = dict_to_surface_node(&expr_val, ctx)?;
            SurfaceExpression::Quote(expr)
        }

        // ---- Unquote ----
        "unquote" | "Unquote" => {
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node(&expr_val, ctx)?;
            SurfaceExpression::Unquote(expr)
        }

        // ---- UnquoteSplice ----
        "unquote-splice" | "UnquoteSplice" => {
            let expr_val = get_dict_field(&dict, "expr", &["type"], ctx)?;
            let expr = dict_to_surface_node(&expr_val, ctx)?;
            SurfaceExpression::UnquoteSplice(expr)
        }

        // ---- Sequential ----
        "sequential" | "Sequential" => {
            let exprs_val = get_dict_field(&dict, "exprs", &["type"], ctx)?;
            let exprs_list = extract_list(&exprs_val, &["exprs"], ctx)?;
            let mut exprs = Vec::new();
            for expr_val in exprs_list {
                exprs.push(dict_to_surface_node(&expr_val, ctx)?);
            }
            SurfaceExpression::Sequential(exprs)
        }

        // ---- PatternDecl ----
        "pattern-decl" | "PatternDecl" => {
            let bindings_val = get_dict_field(&dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list {
                bindings.push(dict_to_surface_node(&binding_val, ctx)?);
            }
            SurfaceExpression::PatternDecl { bindings }
        }

        // ---- LetDecl ----
        "let-decl" | "LetDecl" => {
            let bindings_val = get_dict_field(&dict, "bindings", &["type"], ctx)?;
            let bindings_list = extract_list(&bindings_val, &["bindings"], ctx)?;
            let mut bindings = Vec::new();
            for binding_val in bindings_list {
                bindings.push(dict_to_surface_node(&binding_val, ctx)?);
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
            let func = dict_to_surface_node(&func_val, ctx)?;
            let arg_val = get_dict_field(&dict, "arg", &["type"], ctx)?;
            let arg = dict_to_surface_node(&arg_val, ctx)?;
            SurfaceExpression::TypeApp { func, arg }
        }

        // ---- Match ----
        "match" | "Match" => {
            use crate::ast::SurfaceMatchArm;
            let scrutinee_val = get_dict_field(&dict, "scrutinee", &["type"], ctx)?;
            let scrutinee = dict_to_surface_node(&scrutinee_val, ctx)?;

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
                        Some(dict_to_surface_node(&guard_val, ctx)?)
                    }
                    _ => None,
                };
                let body_val = get_dict_field(&arm_dict, "body", &["arms", &i_str], ctx)?;
                let body = dict_to_surface_node(&body_val, ctx)?;
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
            let pattern = dict_to_surface_node(&pattern_val, ctx)?;
            let body_val = get_dict_field(&dict, "body", &["type"], ctx)?;
            let body = dict_to_surface_node(&body_val, ctx)?;
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
        _ => Some(dict_to_surface_node(&key_val, ctx)?),
    };

    let value_val = get_dict_field(dict, "value", path, ctx)?;
    let value = dict_to_surface_node(&value_val, ctx)?;

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
    let value = dict_to_surface_node(&value_val, ctx)?;

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
        Value::Expression(node) => {
            // Handle the case where AST field extraction returns a VarRef Expression node.
            // This happens when macro helpers pass VarRef nodes instead of extracting
            // the name string. Extract the name field from the VarRef.
            match &node.expr {
                crate::ast::SurfaceExpression::Str(s) => Ok(s.clone()),
                crate::ast::SurfaceExpression::VarRef { name, .. } => Ok(name.clone()),
                _ => Err(AstError {
                    message: format!(
                        "field '{}' must be String or VarRef, got Expression({})",
                        key,
                        crate::surface_fields::surface_expr_tag(&node.expr)
                    ),
                    field_path: path.iter().map(|s| s.to_string()).collect(),
                }),
            }
        }
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
        Value::Dict(_) | Value::Variant { .. } | Value::Expression(_) => Ok(val),
        _ => Err(AstError {
            message: format!("field '{}' must be Dict, Variant, or Expression", key),
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
        .unwrap_or_else(Span::origin);
    let mut root = IndexMap::new();

    root.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("file"),
            span.clone(),
        ))),
    );

    root.insert(
        Key::String("schema-version".into()),
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
        Key::String("documents".into()),
        list_to_thunk_id(docs.into_iter(), span.clone(), ctx)?,
    );

    root.insert(
        Key::String("span".into()),
        span_to_thunk_id(span.clone(), ctx)?,
    );

    Ok(Arc::new(Thunk::new_materialized(Value::Dict(root), span)))
}

fn span_to_thunk_id(span: Span, ctx: &Arc<crate::eval::EvalContext>) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    // start position
    let mut start_dict = IndexMap::new();
    start_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.line as i64),
            span.clone(),
        ))),
    );
    start_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.column as i64),
            span.clone(),
        ))),
    );
    start_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.start.offset as i64),
            span.clone(),
        ))),
    );

    // end position
    let mut end_dict = IndexMap::new();
    end_dict.insert(
        Key::String("line".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.line as i64),
            span.clone(),
        ))),
    );
    end_dict.insert(
        Key::String("col".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.column as i64),
            span.clone(),
        ))),
    );
    end_dict.insert(
        Key::String("offset".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Int(span.end.offset as i64),
            span.clone(),
        ))),
    );

    dict.insert(
        Key::String("start".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(start_dict),
            span.clone(),
        ))),
    );
    dict.insert(
        Key::String("end".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Dict(end_dict),
            span.clone(),
        ))),
    );

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Convert a Vec<ThunkId> to a dict-based list (auto-indexed dict with integer keys).
fn list_to_thunk_id(
    items: impl ExactSizeIterator<Item = ThunkId>,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::with_capacity(items.len());
    for (i, item) in items.enumerate() {
        dict.insert(Key::Int(i as i64), item);
    }
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `named_arg_to_thunk_id`. Uses `SurfaceNamedArg`.
fn surface_named_arg_to_thunk_id(
    named_arg: &SurfaceNamedArg,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&named_arg.name),
            span.clone(),
        ))),
    );
    dict.insert(
        Key::String("value".into()),
        surface_node_to_thunk_id(&named_arg.value, opts, ctx)?,
    );
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `param_to_thunk_id`. Uses `SurfaceParam`.
fn surface_param_to_thunk_id(
    param: &SurfaceParam,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();
    dict.insert(
        Key::String("name".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val(&param.name),
            span.clone(),
        ))),
    );
    dict.insert(
        Key::String("annotation".into()),
        match &param.annotation {
            Some(a) => annotation_to_thunk_id(&a.node, span.clone(), ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );
    dict.insert(
        Key::String("variadic".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Bool(param.variadic),
            span.clone(),
        ))),
    );
    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

/// Surface-native equivalent of `entry_to_thunk_id`. Uses `SurfaceEntry` instead of `Entry`.
fn surface_entry_to_thunk_id(
    entry: &SurfaceEntry,
    entry_span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let span = entry.value.span.clone();
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("entry"),
            span.clone(),
        ))),
    );

    dict.insert(
        Key::String("key".into()),
        match &entry.key {
            Some(k) => surface_node_to_thunk_id(k, opts, ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );

    dict.insert(
        Key::String("value".into()),
        surface_node_to_thunk_id(&entry.value, opts, ctx)?,
    );

    let blank_before = opts
        .comments
        .as_ref()
        .and_then(|maps| maps.blank_before.get(&entry_span.start.offset))
        .copied()
        .unwrap_or(false);
    dict.insert(
        Key::String("blank-before".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            Value::Bool(blank_before),
            span.clone(),
        ))),
    );

    if let Some(comment_maps) = &opts.comments {
        if let Some(comments) = comment_maps.leading_comments.get(&entry_span.start.offset) {
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
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span.clone(), ctx)?,
                );
            }
        }
        if let Some(comment) = comment_maps.trailing_comments.get(&entry_span.start.offset) {
            dict.insert(
                Key::String("trailing-comment".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(comment),
                    span.clone(),
                ))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn pattern_to_thunk_id(
    pattern: &crate::ast::Pattern,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    use crate::ast::{LiteralPattern, Pattern};
    let mut dict = IndexMap::new();

    match pattern {
        Pattern::Wildcard => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("wildcard"),
                    span.clone(),
                ))),
            );
        }
        Pattern::Variable(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("variable"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
        }
        Pattern::TypeTag(tag) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("type_tag"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("tag".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(tag),
                    span.clone(),
                ))),
            );
        }
        Pattern::Pin(name) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("pin"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
        }
        Pattern::Literal(lit) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("literal"),
                    span.clone(),
                ))),
            );
            let value = match lit {
                LiteralPattern::Int(n) => Value::Int(*n),
                LiteralPattern::Float(f) => Value::Float(*f),
                LiteralPattern::Bool(b) => Value::Bool(*b),
                LiteralPattern::Str(s) => string_val(s),
            };
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(value, span.clone()))),
            );
        }
        Pattern::Dict { fields, rest } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("dict"),
                    span.clone(),
                ))),
            );
            // Convert fields to a dict
            let mut fields_dict = IndexMap::new();
            for (i, (key, pat)) in fields.iter().enumerate() {
                let mut field_dict = IndexMap::new();
                field_dict.insert(
                    Key::String("key".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(key),
                        pat.span.clone(),
                    ))),
                );
                field_dict.insert(
                    Key::String("pattern".into()),
                    pattern_to_thunk_id(&pat.node, pat.span.clone(), ctx)?,
                );
                fields_dict.insert(
                    Key::String(Rc::from(i.to_string().as_str())),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(field_dict),
                        pat.span.clone(),
                    ))),
                );
            }
            dict.insert(
                Key::String("fields".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(fields_dict),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("rest".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*rest),
                    span.clone(),
                ))),
            );
        }
        Pattern::Seq { head, tail } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("seq"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("head".into()),
                pattern_to_thunk_id(&head.node, head.span.clone(), ctx)?,
            );
            dict.insert(
                Key::String("tail".into()),
                pattern_to_thunk_id(&tail.node, tail.span.clone(), ctx)?,
            );
        }
        Pattern::Constructor { tag, binding } => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("constructor"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("tag".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(tag),
                    span.clone(),
                ))),
            );
            if let Some(pat) = binding {
                dict.insert(
                    Key::String("binding".into()),
                    pattern_to_thunk_id(&pat.node, pat.span.clone(), ctx)?,
                );
            }
        }
        Pattern::Or(patterns) => {
            dict.insert(
                Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("or"),
                    span.clone(),
                ))),
            );
            let pattern_thunks: Vec<_> = patterns
                .iter()
                .map(|pat| pattern_to_thunk_id(&pat.node, pat.span.clone(), ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            let patterns_dict: IndexMap<Key, ThunkId> = pattern_thunks
                .into_iter()
                .enumerate()
                .map(|(i, thunk_id)| (Key::Int(i as i64), thunk_id))
                .collect();
            dict.insert(
                Key::String("patterns".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(patterns_dict),
                    span.clone(),
                ))),
            );
        }
    }

    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(Value::Dict(dict), span))))
}

fn annotation_to_thunk_id(
    ann: &Annotation,
    span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let mut dict = IndexMap::new();

    dict.insert(
        Key::String("type".into()),
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            string_val("annotation"),
            span.clone(),
        ))),
    );

    match ann {
        Annotation::Simple(name) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("simple"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
        }
        Annotation::Annotated(name, inner) => {
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("annotated"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("inner".into()),
                annotation_to_thunk_id(inner, span.clone(), ctx)?,
            );
        }
        Annotation::PropertyDict(entries) => {
            dict.insert(
                Key::String("kind".into()),
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
                        Key::String("type".into()),
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

                    entry_dict.insert(Key::String("key".into()), key_id);

                    // Annotation entry values are strings/ints for simple cases,
                    // or full AST dicts for compound values like [a: Numeric] or Seq@Int.
                    let value_id = match &e.node.value.expr {
                        crate::ast::SurfaceExpression::Str(s) => ctx.alloc_thunk(Arc::new(
                            Thunk::new_materialized(string_val(s), e.node.value.span.clone()),
                        )),
                        crate::ast::SurfaceExpression::Int(n) => ctx.alloc_thunk(Arc::new(
                            Thunk::new_materialized(Value::Int(*n), e.node.value.span.clone()),
                        )),
                        _ => {
                            surface_node_to_thunk_id(&e.node.value, &AstToDictOpts::default(), ctx)?
                        }
                    };

                    entry_dict.insert(Key::String("value".into()), value_id);
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(entry_dict),
                        e.span.clone(),
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;

            dict.insert(
                Key::String("entries".into()),
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
        Key::String("type".into()),
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
        Key::String("expressions".into()),
        list_to_thunk_id(item_ids.into_iter(), span.clone(), ctx)?,
    );

    // name: string or []
    dict.insert(
        Key::String("name".into()),
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
        Key::String("output-type".into()),
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
        Key::String("expects".into()),
        match &doc.expects {
            Some(a) => annotation_to_thunk_id(&a.node, span.clone(), ctx)?,
            None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                span.clone(),
            ))),
        },
    );

    // stage: [Runtime] | [Type] — nominal variant based on document stage annotation
    let stage_tag = match &doc.stage {
        Some(Stage::Type) => "Type",
        Some(Stage::Runtime) | None => "Runtime",
    };
    dict.insert(
        Key::String("stage".into()),
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
                    Key::String("leading-comments".into()),
                    list_to_thunk_id(comment_ids.into_iter(), span.clone(), ctx)?,
                );
            }
        }
    }

    dict.insert(
        Key::String("span".into()),
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
                    .map(|p| {
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(p),
                            span.clone(),
                        )))
                    })
                    .collect();
                dict.insert(
                    Key::String("params".into()),
                    list_to_thunk_id(params_thunk_ids.into_iter(), span.clone(), ctx)?,
                );
            }
            dict.insert(
                Key::String("expr".into()),
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
        } => {
            variant_tag = "ClassDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            // params: integer-keyed list of param name strings
            let params_dict: IndexMap<Key, ThunkId> = params
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(p),
                            span.clone(),
                        ))),
                    )
                })
                .collect();
            dict.insert(
                Key::String("params".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(params_dict),
                    span.clone(),
                ))),
            );
            // superclasses: Seq of 2-element Seqs [[class-name, var-name] ...]
            // Only emitted when non-empty (e.g. [class Ord a where Eq a] → [[Eq a]])
            if !superclasses.is_empty() {
                let pair_thunk_ids: Vec<ThunkId> = superclasses
                    .iter()
                    .map(|(class_name, var_name)| {
                        let inner: IndexMap<Key, ThunkId> = [
                            (
                                Key::Int(0),
                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                    string_val(class_name),
                                    span.clone(),
                                ))),
                            ),
                            (
                                Key::Int(1),
                                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                    string_val(var_name),
                                    span.clone(),
                                ))),
                            ),
                        ]
                        .into_iter()
                        .collect();
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(inner),
                            span.clone(),
                        )))
                    })
                    .collect();
                dict.insert(
                    Key::String("superclasses".into()),
                    list_to_thunk_id(pair_thunk_ids.into_iter(), span.clone(), ctx)?,
                );
            }
            // methods: string-keyed dict of method expression dicts
            // Keys are SurfaceExpression::Str bare words; values are the full entry value nodes.
            let methods_dict: IndexMap<Key, ThunkId> = methods
                .iter()
                .filter_map(|method| {
                    method.node.key.as_ref().and_then(|key| {
                        if let SurfaceExpression::Str(key_str) = &key.expr {
                            Some((
                                Key::String(Rc::from(key_str.as_str())),
                                surface_node_to_thunk_id(&method.node.value, opts, ctx).ok()?,
                            ))
                        } else {
                            None
                        }
                    })
                })
                .collect();
            dict.insert(
                Key::String("methods".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(methods_dict),
                    span.clone(),
                ))),
            );
            // determines: optional integer-keyed list of expression dicts
            if !determines.is_empty() {
                let determines_dict: IndexMap<Key, ThunkId> = determines
                    .iter()
                    .enumerate()
                    .filter_map(|(i, fd_node)| {
                        Some((
                            Key::Int(i as i64),
                            surface_node_to_thunk_id(fd_node, opts, ctx).ok()?,
                        ))
                    })
                    .collect();
                dict.insert(
                    Key::String("determines".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(determines_dict),
                        span.clone(),
                    ))),
                );
            }
            // resolver: optional expression dict
            if let Some(resolver_node) = resolver {
                dict.insert(
                    Key::String("resolver".into()),
                    surface_node_to_thunk_id(resolver_node, opts, ctx)?,
                );
            }
            // injective: optional bool (only emitted when true)
            if *resolver_injective {
                dict.insert(
                    Key::String("injective".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Bool(true),
                        span.clone(),
                    ))),
                );
            }
        }

        SurfaceDeclaration::InstanceDecl { class_name, arms } => {
            variant_tag = "InstanceDecl";
            dict.insert(
                Key::String("class".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(class_name),
                    span.clone(),
                ))),
            );
            // arms: integer-keyed list of {pattern, methods} dicts
            let arms_dict: IndexMap<Key, ThunkId> = arms
                .iter()
                .enumerate()
                .filter_map(|(i, (pattern_node, methods))| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        surface_node_to_thunk_id(pattern_node, opts, ctx).ok()?,
                    );
                    // methods: string-keyed dict matching ClassDecl.methods format
                    let methods_dict: IndexMap<Key, ThunkId> = methods
                        .iter()
                        .filter_map(|method| {
                            method.node.key.as_ref().and_then(|key| {
                                if let SurfaceExpression::Str(key_str) = &key.expr {
                                    Some((
                                        Key::String(Rc::from(key_str.as_str())),
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
                        Key::String("methods".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(methods_dict),
                            span.clone(),
                        ))),
                    );
                    Some((
                        Key::Int(i as i64),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Dict(arm_dict),
                            span.clone(),
                        ))),
                    ))
                })
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span.clone(),
                ))),
            );
        }

        SurfaceDeclaration::MacroDecl { name, params, body } => {
            variant_tag = "MacroDecl";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("params".into()),
                surface_node_to_thunk_id(params, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceDeclaration::SyntaxClass {
            name,
            pattern,
            message,
        } => {
            variant_tag = "SyntaxClass";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            if let Some(msg) = message {
                dict.insert(
                    Key::String("message".into()),
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
                Key::String("forms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(
                        form_list
                            .into_iter()
                            .enumerate()
                            .map(|(i, v)| (Key::Int(i as i64), v))
                            .collect(),
                    ),
                    span.clone(),
                ))),
            );
        }
    }

    dict.insert(
        Key::String("span".into()),
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

/// Convert a SurfaceNode to a ThunkId containing its dict representation.
///
/// Handles all `SurfaceExpression` variants. Schema (Variant tags, key names) is the
/// canonical AST schema — existing tinct metaprogramming code sees no change.
fn surface_node_to_thunk_id(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<ThunkId> {
    let expr = &node.expr;
    let span = node.span.clone();

    let capacity = match expr {
        SurfaceExpression::Int(_) | SurfaceExpression::Float(_) | SurfaceExpression::Bool(_) => 2,
        SurfaceExpression::Str(_) => 3,
        SurfaceExpression::VarRef { .. } => 1,
        SurfaceExpression::DotAccess { .. } => 2,
        SurfaceExpression::Pipe { .. } => 2,
        SurfaceExpression::Sequential(_) => 1,
        SurfaceExpression::Dict(_) => 1,
        SurfaceExpression::Call { .. } => 4,
        SurfaceExpression::Fn { .. } => 4,
        SurfaceExpression::TypeAssert { .. } => 2,
        SurfaceExpression::Annotated { .. } => 2,
        SurfaceExpression::Rest(_) => 1,
        SurfaceExpression::Quote(_)
        | SurfaceExpression::Unquote(_)
        | SurfaceExpression::UnquoteSplice(_) => 1,
        SurfaceExpression::Match { .. } => 2,
        SurfaceExpression::PatternDecl { .. } | SurfaceExpression::LetDecl { .. } => 1,
        SurfaceExpression::CaseArm { .. } => 2,
        SurfaceExpression::Placeholder | SurfaceExpression::Decl(_) => 0,
        SurfaceExpression::TypeApp { .. } => 2,
        SurfaceExpression::Error(_) => 1,
    };

    let mut dict = IndexMap::with_capacity(capacity);
    let variant_tag: &str;

    match expr {
        SurfaceExpression::Int(n) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("int"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Int(*n),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::Float(f) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("float"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Float(*f),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::Bool(b) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("bool"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*b),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::Str(s) => {
            variant_tag = "Literal";
            dict.insert(
                Key::String("kind".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val("str"),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("value".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(s),
                    span.clone(),
                ))),
            );
            let bare = opts
                .source
                .map(|src| {
                    let offset = span.start.offset;
                    src.as_bytes()
                        .get(offset)
                        .map(|&b| b != b'"')
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            dict.insert(
                Key::String("bare".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(bare),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::VarRef { name, .. } => {
            variant_tag = "VarRef";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::DotAccess {
            expr: target,
            field,
        } => {
            variant_tag = "DotAccess";
            dict.insert(
                Key::String("target".into()),
                surface_node_to_thunk_id(target, opts, ctx)?,
            );
            match field {
                DotKey::Ident(s) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            string_val(s),
                            span.clone(),
                        ))),
                    );
                }
                DotKey::Int(n) => {
                    dict.insert(
                        Key::String("field".into()),
                        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Int(*n),
                            span.clone(),
                        ))),
                    );
                }
            }
        }

        SurfaceExpression::Pipe { lhs, rhs } => {
            variant_tag = "Pipe";
            dict.insert(
                Key::String("lhs".into()),
                surface_node_to_thunk_id(lhs, opts, ctx)?,
            );
            dict.insert(
                Key::String("rhs".into()),
                surface_node_to_thunk_id(rhs, opts, ctx)?,
            );
        }

        SurfaceExpression::Sequential(exprs) => {
            variant_tag = "Sequential";
            let expr_ids: Vec<_> = exprs
                .iter()
                .map(|e| surface_node_to_thunk_id(e, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("exprs".into()),
                list_to_thunk_id(expr_ids.into_iter(), span.clone(), ctx)?,
            );
        }

        SurfaceExpression::Dict(entries) => {
            variant_tag = "Dict";
            let entry_ids: Vec<_> = entries
                .iter()
                .map(|e| surface_entry_to_thunk_id(&e.node, e.span.clone(), opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("entries".into()),
                list_to_thunk_id(entry_ids.into_iter(), span.clone(), ctx)?,
            );
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied,
        } => {
            variant_tag = "Call";
            dict.insert(
                Key::String("fn".into()),
                surface_node_to_thunk_id(func, opts, ctx)?,
            );
            let arg_ids: Vec<_> = args
                .iter()
                .map(|a| surface_node_to_thunk_id(a, opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("args".into()),
                list_to_thunk_id(arg_ids.into_iter(), span.clone(), ctx)?,
            );
            let named_arg_ids: Vec<_> = named_args
                .iter()
                .map(|na| surface_named_arg_to_thunk_id(&na.node, na.span.clone(), opts, ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("named-args".into()),
                list_to_thunk_id(named_arg_ids.into_iter(), span.clone(), ctx)?,
            );
            dict.insert(
                Key::String("implied".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*implied),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            desugared,
        } => {
            variant_tag = "Fn";
            let param_ids: Vec<_> = params
                .iter()
                .map(|p| surface_param_to_thunk_id(&p.node, span.clone(), ctx))
                .collect::<EvalResult<Vec<_>>>()?;
            dict.insert(
                Key::String("params".into()),
                list_to_thunk_id(param_ids.into_iter(), span.clone(), ctx)?,
            );
            dict.insert(
                Key::String("return-ann".into()),
                match return_ann {
                    Some(a) => annotation_to_thunk_id(&a.node, span.clone(), ctx)?,
                    None => ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        span.clone(),
                    ))),
                },
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
            dict.insert(
                Key::String("desugared".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Bool(*desugared),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            variant_tag = "TypeAssert";
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span.clone(), ctx)?,
            );
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Annotated { name, annotation } => {
            variant_tag = "Annotated";
            dict.insert(
                Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    string_val(name),
                    span.clone(),
                ))),
            );
            dict.insert(
                Key::String("annotation".into()),
                annotation_to_thunk_id(&annotation.node, span.clone(), ctx)?,
            );
        }

        SurfaceExpression::Rest(name_opt) => {
            variant_tag = "Rest";
            dict.insert(
                Key::String("name".into()),
                match name_opt {
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
        }

        SurfaceExpression::Quote(inner) => {
            variant_tag = "Quote";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Unquote(inner) => {
            variant_tag = "Unquote";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            variant_tag = "UnquoteSplice";
            dict.insert(
                Key::String("expr".into()),
                surface_node_to_thunk_id(inner, opts, ctx)?,
            );
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            variant_tag = "Match";
            dict.insert(
                Key::String("scrutinee".into()),
                surface_node_to_thunk_id(scrutinee, opts, ctx)?,
            );
            let arms_thunks: Vec<ThunkId> = arms
                .iter()
                .map(|arm| {
                    let mut arm_dict = IndexMap::new();
                    arm_dict.insert(
                        Key::String("pattern".into()),
                        pattern_to_thunk_id(&arm.pattern.node, arm.pattern.span.clone(), ctx)?,
                    );
                    if let Some(guard) = &arm.guard {
                        arm_dict.insert(
                            Key::String("guard".into()),
                            surface_node_to_thunk_id(guard, opts, ctx)?,
                        );
                    }
                    arm_dict.insert(
                        Key::String("body".into()),
                        surface_node_to_thunk_id(&arm.body, opts, ctx)?,
                    );
                    Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(arm_dict),
                        arm.pattern.span.clone(),
                    ))))
                })
                .collect::<EvalResult<Vec<_>>>()?;
            let arms_dict: IndexMap<Key, ThunkId> = arms_thunks
                .into_iter()
                .enumerate()
                .map(|(i, id)| (Key::Int(i as i64), id))
                .collect();
            dict.insert(
                Key::String("arms".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(arms_dict),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::PatternDecl { bindings } => {
            variant_tag = "PatternDecl";
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .map(|(i, b)| Ok((Key::Int(i as i64), surface_node_to_thunk_id(b, opts, ctx)?)))
                .collect::<EvalResult<IndexMap<_, _>>>()?;
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::LetDecl { bindings } => {
            variant_tag = "LetDecl";
            let bindings_dict: IndexMap<Key, ThunkId> = bindings
                .iter()
                .enumerate()
                .map(|(i, b)| Ok((Key::Int(i as i64), surface_node_to_thunk_id(b, opts, ctx)?)))
                .collect::<EvalResult<IndexMap<_, _>>>()?;
            dict.insert(
                Key::String("bindings".into()),
                ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(bindings_dict),
                    span.clone(),
                ))),
            );
        }

        SurfaceExpression::CaseArm { pattern, body } => {
            variant_tag = "CaseArm";
            dict.insert(
                Key::String("pattern".into()),
                surface_node_to_thunk_id(pattern, opts, ctx)?,
            );
            dict.insert(
                Key::String("body".into()),
                surface_node_to_thunk_id(body, opts, ctx)?,
            );
        }

        SurfaceExpression::Placeholder | SurfaceExpression::Decl(_) => {
            variant_tag = "Placeholder";
        }

        SurfaceExpression::TypeApp { func, arg } => {
            variant_tag = "TypeApp";
            dict.insert(
                Key::String("func".into()),
                surface_node_to_thunk_id(func, opts, ctx)?,
            );
            dict.insert(
                Key::String("arg".into()),
                surface_node_to_thunk_id(arg, opts, ctx)?,
            );
        }

        SurfaceExpression::Error(error_span) => {
            variant_tag = "AstError";
            dict.insert(
                Key::String("span".into()),
                span_to_thunk_id(error_span.clone(), ctx)?,
            );
            let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(dict),
                error_span.clone(),
            )));
            return Ok(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Variant {
                    tag: variant_tag.to_string(),
                    payload: Some(payload_id),
                },
                error_span.clone(),
            ))));
        }
    }

    dict.insert(
        Key::String("span".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        use crate::value::Environment;
        use std::sync::RwLock;

        let env = Arc::new(RwLock::new(Environment::new()));
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new(base_dir, Arc::clone(&env), Arc::clone(&env), false)
    }

    /// Peel a `Value::Variant` to its payload dict.
    /// Panics with a helpful message if the value is not a Variant with a Dict payload.
    fn peel_variant(
        val: Value,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> (String, IndexMap<Key, ThunkId>) {
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
                let type_id = map.get(&Key::String("type".into())).unwrap();
                let type_thunk = ctx.get_thunk(*type_id);
                assert_eq!(type_thunk.try_get_materialized(), Some(string_val("file")));

                let version_id = map.get(&Key::String("schema-version".into())).unwrap();
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
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            // Get the entries list
                                            let entries_id = dict_node
                                                .get(&Key::String("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id =
                                                        entries_list.get(&Key::Int(0)).unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    match entry_thunk.try_get_materialized() {
                                                        Some(Value::Dict(entry_dict)) => {
                                                            // Get the key expression
                                                            let key_id = entry_dict
                                                                .get(&Key::String("key".into()))
                                                                .unwrap();
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            // Check bare: true
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(true)),
                                                                "bare should be true for bare word 'foo'"
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
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
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&Key::String("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id =
                                                        entries_list.get(&Key::Int(0)).unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    match entry_thunk.try_get_materialized() {
                                                        Some(Value::Dict(entry_dict)) => {
                                                            let key_id = entry_dict
                                                                .get(&Key::String("key".into()))
                                                                .unwrap();
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(false)),
                                                                "bare should be false for quoted string \"foo\""
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
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
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&Key::String("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id =
                                                        entries_list.get(&Key::Int(0)).unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    match entry_thunk.try_get_materialized() {
                                                        Some(Value::Dict(entry_dict)) => {
                                                            // Check for leading-comments field
                                                            let comments_id = entry_dict
                                                                .get(&Key::String(
                                                                    "leading-comments".into(),
                                                                ))
                                                                .expect("leading-comments field missing");
                                                            let comments_thunk =
                                                                ctx.get_thunk(*comments_id);
                                                            match comments_thunk
                                                                .try_get_materialized()
                                                            {
                                                                Some(Value::Dict(
                                                                    comments_list,
                                                                )) => {
                                                                    let comment_id =
                                                                        comments_list
                                                                            .get(&Key::Int(0))
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
                                                        _ => panic!("expected Dict for entry"),
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
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&Key::String("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id =
                                                        entries_list.get(&Key::Int(1)).unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    match entry_thunk.try_get_materialized() {
                                                        Some(Value::Dict(entry_dict)) => {
                                                            // Check blank-before: true
                                                            let blank_id = entry_dict
                                                                .get(&Key::String(
                                                                    "blank-before".into(),
                                                                ))
                                                                .expect(
                                                                    "blank-before field missing",
                                                                );
                                                            let blank_thunk =
                                                                ctx.get_thunk(*blank_id);
                                                            assert_eq!(
                                                                blank_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(true)),
                                                                "blank-before should be true for second entry"
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
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
                let docs_id = file_dict.get(&Key::String("documents".into())).unwrap();
                let docs_thunk = ctx.get_thunk(*docs_id);
                match docs_thunk.try_get_materialized() {
                    Some(Value::Dict(docs_list)) => {
                        let doc_id = docs_list.get(&Key::Int(0)).unwrap();
                        let doc_thunk = ctx.get_thunk(*doc_id);
                        match doc_thunk.try_get_materialized() {
                            Some(Value::Dict(doc_dict)) => {
                                let exprs_id =
                                    doc_dict.get(&Key::String("expressions".into())).unwrap();
                                let exprs_thunk = ctx.get_thunk(*exprs_id);
                                match exprs_thunk.try_get_materialized() {
                                    Some(Value::Dict(exprs_list)) => {
                                        let expr_id = exprs_list.get(&Key::Int(0)).unwrap();
                                        let expr_thunk = ctx.get_thunk(*expr_id);
                                        let expr_val = expr_thunk
                                            .try_get_materialized()
                                            .expect("expr not materialized");
                                        let (_tag, dict_node) = peel_variant(expr_val, &ctx);
                                        {
                                            let entries_id = dict_node
                                                .get(&Key::String("entries".into()))
                                                .unwrap();
                                            let entries_thunk = ctx.get_thunk(*entries_id);
                                            match entries_thunk.try_get_materialized() {
                                                Some(Value::Dict(entries_list)) => {
                                                    let entry_id =
                                                        entries_list.get(&Key::Int(0)).unwrap();
                                                    let entry_thunk = ctx.get_thunk(*entry_id);
                                                    match entry_thunk.try_get_materialized() {
                                                        Some(Value::Dict(entry_dict)) => {
                                                            let key_id = entry_dict
                                                                .get(&Key::String("key".into()))
                                                                .unwrap();
                                                            let key_thunk = ctx.get_thunk(*key_id);
                                                            let key_val = key_thunk
                                                                .try_get_materialized()
                                                                .expect("key not materialized");
                                                            let (_key_tag, key_dict) =
                                                                peel_variant(key_val, &ctx);
                                                            let bare_id = key_dict
                                                                .get(&Key::String("bare".into()))
                                                                .expect("bare field missing");
                                                            let bare_thunk =
                                                                ctx.get_thunk(*bare_id);
                                                            assert_eq!(
                                                                bare_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(false)),
                                                                "bare should be false when source is None"
                                                            );

                                                            // Check that blank-before is still present (always included)
                                                            let blank_id = entry_dict
                                                                .get(&Key::String(
                                                                    "blank-before".into(),
                                                                ))
                                                                .expect(
                                                                    "blank-before field missing",
                                                                );
                                                            let blank_thunk =
                                                                ctx.get_thunk(*blank_id);
                                                            assert_eq!(
                                                                blank_thunk
                                                                    .try_get_materialized(),
                                                                Some(Value::Bool(false)),
                                                                "blank-before should be false when comments is None"
                                                            );

                                                            // Check that leading-comments is absent
                                                            assert!(
                                                                entry_dict
                                                                    .get(&Key::String(
                                                                        "leading-comments".into()
                                                                    ))
                                                                    .is_none(),
                                                                "leading-comments should be absent when comments is None"
                                                            );
                                                        }
                                                        _ => panic!("expected Dict for entry"),
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
