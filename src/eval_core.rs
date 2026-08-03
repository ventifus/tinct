//! CoreExpr evaluation layer.
//!
//! This module handles evaluation of lowered CoreExpr nodes to thunks. CoreExpr is the
//! internal AST representation after name resolution and before CEK machine execution.
//!
//! Key functions:
//! - `eval_core_expr`: Main entry point — evaluates a CoreExpr node to a lazy thunk
//! - `eval_quote_walk`: Handles quote/unquote evaluation for metaprogramming
//!
//! This module is called by:
//! - `eval_materialize.rs` (CEK machine force_step when taking thunk states)
//! - `eval_call.rs` (function evaluation)
//! - `builtins_async.rs` (eval builtin, macro transformers)

use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Span, Spanned, SurfaceMatchArm, VarAddr};
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval_call_core, eval_dict_core, materialize, EvalContext};
use crate::type_tags::*;
use crate::value::{string_val, EvalFrame, HashableValue, Thunk, Value};

// maybe_wrap_guard removed: type guards are now inline on SurfaceNode.type_guard
// (TypeAnnotation OnceLock). The lowerer wraps them in CoreExpr::TypeAssert during lowering.
// No runtime boundary guard wrapping from a side-table is needed.

/// Convert a runtime Value back to an Arc<SurfaceNode> for unquoting.
///
/// If the value is a Dict/Variant with a `type` field, treat it as an AST dict and use
/// `dict_to_surface_node`. Otherwise, convert the value to its literal SurfaceNode.
///
/// This is the SurfaceNode-native replacement for the old `value_to_expr`. No Expr round-trip.
fn value_to_surface_node(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<crate::ast::SurfaceNode>> {
    use crate::ast::{SurfaceExpression, SurfaceNode};
    let make_node = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, span.clone()));
    match value {
        Value::Int { n, .. } => Ok(make_node(SurfaceExpression::Int(*n))),
        Value::Float { n, .. } => Ok(make_node(SurfaceExpression::Float(*n))),
        Value::String {
            source, start, end, ..
        } => Ok(make_node(SurfaceExpression::StringLiteral {
            prefix: String::new(),
            delimiter: "\"".to_string(),
            content: source[*start..*end].to_string(),
        })),
        Value::Variant { .. } => {
            // Variant form of an AST node — convert via surface bridge
            crate::surface_convert::dict_to_surface_node(value, &span, ctx).map_err(|err| {
                EvalError::internal(
                    format!("unquote result Variant is not a valid AST: {}", err),
                    span,
                )
                .into()
            })
        }
        Value::Dict { entries: dict, .. } => {
            // Check if this is an AST dict (has a "type" field)
            if dict.contains_key(&HashableValue::Str("type".into())) {
                // It's an AST dict — convert via surface bridge
                crate::surface_convert::dict_to_surface_node(value, &span, ctx).map_err(|err| {
                    EvalError::internal(
                        format!("unquote result dict is not a valid AST: {}", err),
                        span,
                    )
                    .into()
                })
            } else {
                // It's a regular dict — dict values are thunk IDs, conversion not yet supported
                Err(EvalError::internal(
                    "unquote of non-AST dict is not yet supported".to_string(),
                    span,
                )
                .into())
            }
        }
        _ => Err(
            EvalError::internal(format!("unquote of {:?} is not supported", value), span).into(),
        ),
    }
}

/// Collect all elements from an integer-keyed Dict into a Vec.
/// Returns an error if the value is not an integer-keyed Dict.
/// Seq inputs are no longer supported (Rust must not know Seq's internal structure).
async fn collect_seq_elements(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Vec<Value>> {
    let mut elements = Vec::new();
    let current = value.clone();

    match current {
        Value::Dict {
            entries: ref dict, ..
        } => {
            // Integer-keyed Dict (from macro variadic args) — collect elements in key order
            // Validate that all keys are integers and sequential from 0
            let mut int_entries: Vec<(i64, Arc<crate::value::Thunk>)> = Vec::new();
            for (key, thunk) in dict {
                if let HashableValue::Int(i) = key {
                    int_entries.push((*i, Arc::clone(thunk)));
                } else {
                    return Err(EvalError::type_mismatch(
                        "Seq or integer-keyed Dict",
                        "Dict with non-integer keys",
                        span,
                    )
                    .into());
                }
            }

            // Sort by integer key
            int_entries.sort_by_key(|(i, _)| *i);

            // Validate sequential from 0
            for (idx, (i, _)) in int_entries.iter().enumerate() {
                if *i != idx as i64 {
                    return Err(EvalError::type_mismatch(
                        "Seq or sequential integer-keyed Dict (0, 1, 2, ...)",
                        &format!(
                            "Dict with non-sequential keys (expected {}, found {})",
                            idx, i
                        ),
                        span,
                    )
                    .into());
                }
            }

            // Materialize each element
            for (_, thunk) in int_entries {
                let value = materialize(&thunk, Some(&span), ctx).await?;
                elements.push(value);
            }
        }
        _ => {
            return Err(EvalError::type_mismatch(
                "Seq or integer-keyed Dict",
                current.type_name(),
                span,
            )
            .into());
        }
    }

    Ok(elements)
}

/// Recursively preprocess a quoted SurfaceNode tree to handle nested unquotes.
///
/// This walks the entire AST and:
/// - Evaluates `Unquote` nodes, converting the result back to a SurfaceNode
/// - Handles `UnquoteSplice` in call argument positions
/// - Recurses into all child SurfaceNodes
/// - Leaves non-unquote nodes unchanged (Arc::clone, no allocation)
///
/// Operates entirely on SurfaceNode — no Expr round-trip.
fn eval_quote_preprocess<'a>(
    node: Arc<crate::ast::SurfaceNode>,
    frame: Arc<EvalFrame>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = EvalResult<Arc<crate::ast::SurfaceNode>>> + Send + 'a>,
> {
    use crate::ast::{
        SurfaceDeclaration, SurfaceEntry, SurfaceExpression, SurfaceNamedArg, SurfaceNode,
    };
    Box::pin(async move {
        let span = node.span.clone();
        let make_node = |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, span.clone()));

        match &node.expr {
            SurfaceExpression::Unquote(inner) => {
                // Evaluate the unquoted expression and convert back to SurfaceNode
                let core = crate::lower::lower_inner(
                    inner,
                    &mut Vec::new(),
                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                );
                let thunk = eval_core_expr(&core, &frame, ctx).await?;
                let value = materialize(&thunk, Some(&inner.span), ctx).await?;
                value_to_surface_node(&value, inner.span.clone(), ctx)
            }

            SurfaceExpression::UnquoteSplice(_) => {
                // UnquoteSplice outside of call args or dict entries is an error.
                // Call args and dict entries handle UnquoteSplice in their own loops.
                Err(EvalError::unimplemented(
                    "unquote-splice must be in a list position (inside call args or dict entries)"
                        .to_string(),
                    span,
                )
                .into())
            }

            // Recursively process composite expressions
            SurfaceExpression::Dict(entries) => {
                let mut processed_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    // Handle unquote-splicing in dict entry value position
                    if let SurfaceExpression::UnquoteSplice(inner) = &entry.node.value.expr {
                        // Evaluate the unquote-splice expression
                        let core = crate::lower::lower_inner(
                            inner,
                            &mut Vec::new(),
                            ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                        );
                        let thunk = eval_core_expr(&core, &frame, ctx).await?;
                        let inner_span = inner.span.clone();
                        let value = materialize(&thunk, Some(&inner_span), ctx).await?;

                        // Value must be Dict or Seq for splicing
                        match value {
                            Value::Dict {
                                entries: ref dict, ..
                            } => {
                                // Splice dict entries into the current dict
                                for (key, value_thunk) in dict {
                                    // Convert the thunk to a Value, then to a SurfaceNode
                                    let value_val =
                                        materialize(value_thunk, Some(&inner_span), ctx).await?;
                                    let value_node =
                                        value_to_surface_node(&value_val, inner_span.clone(), ctx)?;

                                    // Convert the key to a SurfaceNode
                                    let key_node = match key {
                                        HashableValue::Int(n) => Arc::new(SurfaceNode::new(
                                            SurfaceExpression::Int(*n),
                                            inner_span.clone(),
                                        )),
                                        HashableValue::Str(s) => {
                                            // Check if it looks like an identifier (alphanumeric + hyphens)
                                            // If so, use VarRef; otherwise use Str
                                            let is_ident = s.chars().all(|c| {
                                                c.is_alphanumeric()
                                                    || c == '-'
                                                    || c == '_'
                                                    || c == '?'
                                            }) && !s.is_empty()
                                                && !s.chars().next().unwrap().is_numeric();
                                            if is_ident {
                                                Arc::new(SurfaceNode::new(
                                                    SurfaceExpression::VarRef {
                                                        name: s.to_string(),
                                                        escaped: false,
                                                        resolution: crate::ast::Resolution::new(),
                                                        annotation: None,
                                                        do_infer_placeholder: false,
                                                    },
                                                    inner_span.clone(),
                                                ))
                                            } else {
                                                Arc::new(SurfaceNode::new(
                                                    SurfaceExpression::StringLiteral {
                                                        prefix: String::new(),
                                                        delimiter: "\"".to_string(),
                                                        content: s.to_string(),
                                                    },
                                                    inner_span.clone(),
                                                ))
                                            }
                                        }
                                        // For non-Int/Str keys (Bool, Dict, Variant), render as string literal
                                        other => Arc::new(SurfaceNode::new(
                                            SurfaceExpression::StringLiteral {
                                                prefix: String::new(),
                                                delimiter: "\"".to_string(),
                                                content: other.to_string(),
                                            },
                                            inner_span.clone(),
                                        )),
                                    };

                                    processed_entries.push(Spanned::new(
                                        SurfaceEntry {
                                            key: Some(key_node),
                                            value: value_node,
                                        },
                                        inner_span.clone(),
                                    ));
                                }
                            }
                            _ => {
                                return Err(EvalError::type_mismatch(
                                    "Dict",
                                    value.type_name(),
                                    inner_span,
                                )
                                .into())
                            }
                        }
                    } else {
                        // Regular entry - recursively process
                        let processed_value = eval_quote_preprocess(
                            Arc::clone(&entry.node.value),
                            Arc::clone(&frame),
                            ctx,
                        )
                        .await?;
                        let processed_key = if let Some(ref key_node) = entry.node.key {
                            Some(
                                eval_quote_preprocess(
                                    Arc::clone(key_node),
                                    Arc::clone(&frame),
                                    ctx,
                                )
                                .await?,
                            )
                        } else {
                            None
                        };
                        processed_entries.push(Spanned::new(
                            SurfaceEntry {
                                key: processed_key,
                                value: processed_value,
                            },
                            entry.span.clone(),
                        ));
                    }
                }
                Ok(make_node(SurfaceExpression::Dict(processed_entries)))
            }

            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
                ..
            } => {
                let processed_func =
                    eval_quote_preprocess(Arc::clone(func), Arc::clone(&frame), ctx).await?;
                let mut processed_args: Vec<Arc<SurfaceNode>> = Vec::new();
                for arg in args {
                    // Handle unquote-splicing in call argument position
                    if let SurfaceExpression::UnquoteSplice(inner) = &arg.expr {
                        // Evaluate the unquote-splice expression
                        let core = crate::lower::lower_inner(
                            inner,
                            &mut Vec::new(),
                            ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                        );
                        let thunk = eval_core_expr(&core, &frame, ctx).await?;
                        let inner_span = inner.span.clone();
                        let value = materialize(&thunk, Some(&inner_span), ctx).await?;

                        // Extract elements from the sequence and convert each to SurfaceNode
                        let elements =
                            collect_seq_elements(&value, inner_span.clone(), ctx).await?;
                        for elem_value in elements {
                            let elem_node =
                                value_to_surface_node(&elem_value, inner_span.clone(), ctx)?;
                            processed_args.push(elem_node);
                        }
                    } else {
                        // Regular argument - recursively process
                        processed_args.push(
                            eval_quote_preprocess(Arc::clone(arg), Arc::clone(&frame), ctx).await?,
                        );
                    }
                }
                let mut processed_named_args: Vec<Spanned<SurfaceNamedArg>> =
                    Vec::with_capacity(named_args.len());
                for na in named_args {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&na.node.value), Arc::clone(&frame), ctx)
                            .await?;
                    processed_named_args.push(Spanned::new(
                        SurfaceNamedArg {
                            name: na.node.name.clone(),
                            value: processed_value,
                            annotation: na.node.annotation.clone(),
                        },
                        na.span.clone(),
                    ));
                }
                Ok(make_node(SurfaceExpression::Call {
                    func: processed_func,
                    args: processed_args,
                    named_args: processed_named_args,
                    implied: *implied,
                    pipe_span: None,
                }))
            }

            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared,
                resolved_captures: _,
                resolved_return_annotation: _,
            } => {
                let processed_body =
                    eval_quote_preprocess(Arc::clone(body), Arc::clone(&frame), ctx).await?;
                Ok(make_node(SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: processed_body,
                    desugared: *desugared,
                    resolved_captures: crate::ast::CapturesCell::new(),
                    resolved_return_annotation: crate::ast::TypeAnnotation::new(),
                }))
            }

            SurfaceExpression::Field {
                expr: Some(target),
                field,
                ..
            } => {
                let processed_target =
                    eval_quote_preprocess(Arc::clone(target), Arc::clone(&frame), ctx).await?;
                Ok(make_node(SurfaceExpression::Field {
                    expr: Some(processed_target),
                    field: field.clone(),
                    resolution: crate::ast::Resolution::new(),
                }))
            }

            // Leading-dot is a terminal in quote context — no sub-expression to preprocess.
            SurfaceExpression::Field {
                expr: None, field, ..
            } => Ok(make_node(SurfaceExpression::Field {
                expr: None,
                field: field.clone(),
                resolution: crate::ast::Resolution::new(),
            })),

            SurfaceExpression::Pipe {
                lhs,
                rhs,
                pipe_span,
            } => {
                let processed_lhs =
                    eval_quote_preprocess(Arc::clone(lhs), Arc::clone(&frame), ctx).await?;
                let processed_rhs =
                    eval_quote_preprocess(Arc::clone(rhs), Arc::clone(&frame), ctx).await?;
                Ok(make_node(SurfaceExpression::Pipe {
                    lhs: processed_lhs,
                    rhs: processed_rhs,
                    pipe_span: pipe_span.clone(),
                }))
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut processed_exprs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    processed_exprs
                        .push(eval_quote_preprocess(Arc::clone(e), Arc::clone(&frame), ctx).await?);
                }
                Ok(make_node(SurfaceExpression::Sequential(processed_exprs)))
            }

            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
                ..
            } => {
                let processed_expr =
                    eval_quote_preprocess(Arc::clone(inner), Arc::clone(&frame), ctx).await?;
                Ok(make_node(SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: processed_expr,
                    resolved_type: crate::ast::TypeAnnotation::new(),
                }))
            }

            SurfaceExpression::Quote(inner) => {
                // Nested quote: recurse so inner unquotes are still processed.
                let processed_inner =
                    eval_quote_preprocess(Arc::clone(inner), Arc::clone(&frame), ctx).await?;
                Ok(make_node(SurfaceExpression::Quote(processed_inner)))
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                let processed_scrutinee =
                    eval_quote_preprocess(Arc::clone(scrutinee), Arc::clone(&frame), ctx).await?;
                let mut processed_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let mut processed_body = Vec::with_capacity(arm.body.len());
                    for body_expr in &arm.body {
                        processed_body.push(
                            eval_quote_preprocess(Arc::clone(body_expr), Arc::clone(&frame), ctx)
                                .await?,
                        );
                    }
                    let processed_guard = if let Some(ref guard) = arm.guard {
                        Some(
                            eval_quote_preprocess(Arc::clone(guard), Arc::clone(&frame), ctx)
                                .await?,
                        )
                    } else {
                        None
                    };
                    let processed_pattern =
                        eval_quote_preprocess(Arc::clone(&arm.pattern), Arc::clone(&frame), ctx)
                            .await?;
                    processed_arms.push(SurfaceMatchArm {
                        pattern: processed_pattern,
                        let_bindings: arm.let_bindings.clone(),
                        guard: processed_guard,
                        body: processed_body,
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
                        case_captures: arm.case_captures.clone(),
                    });
                }
                Ok(make_node(SurfaceExpression::Match {
                    scrutinee: processed_scrutinee,
                    arms: processed_arms,
                }))
            }

            SurfaceExpression::Decl(decl) => {
                // Declaration forms inside a quote body — walk their child bodies
                // to find any nested unquotes. Declarations are rare in quoted code,
                // but users can write e.g. [quote [type Foo = Bar]] and expect
                // unquotes inside the alias body to be evaluated.
                let processed_decl = match decl.as_ref() {
                    SurfaceDeclaration::TypeAlias { params, body } => {
                        let processed_body =
                            eval_quote_preprocess(Arc::clone(body), Arc::clone(&frame), ctx)
                                .await?;
                        SurfaceDeclaration::TypeAlias {
                            params: params.clone(),
                            body: processed_body,
                        }
                    }
                    SurfaceDeclaration::SyntaxClass {
                        name,
                        pattern,
                        message,
                    } => {
                        let processed_pattern =
                            eval_quote_preprocess(Arc::clone(pattern), Arc::clone(&frame), ctx)
                                .await?;
                        SurfaceDeclaration::SyntaxClass {
                            name: name.clone(),
                            pattern: processed_pattern,
                            message: message.clone(),
                        }
                    }
                    SurfaceDeclaration::Splice(forms) => {
                        let mut processed_forms = Vec::with_capacity(forms.len());
                        for form in forms {
                            processed_forms.push(
                                eval_quote_preprocess(Arc::clone(form), Arc::clone(&frame), ctx)
                                    .await?,
                            );
                        }
                        SurfaceDeclaration::Splice(processed_forms)
                    }
                    // ClassDecl, InstanceDecl — complex; treat as leaves (no unquote recursion)
                    other => other.clone(),
                };
                Ok(make_node(SurfaceExpression::Decl(Box::new(processed_decl))))
            }

            // All other expressions have no child SurfaceNodes — return unchanged
            _ => Ok(node),
        }
    }) // end Box::pin(async move {
}

async fn eval_quote_walk(
    node: Arc<crate::ast::SurfaceNode>,
    frame: &Arc<EvalFrame>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = node.span.clone();
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_node = eval_quote_preprocess(node, Arc::clone(frame), ctx).await?;

    Ok(Arc::new(Thunk::value(
        crate::surface_convert::surface_node_to_expr_variant(&processed_node, ctx),
        span,
    )))
}

/// Extract non-standard annotation fields from a function `@[...]` annotation into an
/// `IndexMap<String, Value>` for storage in `FnAnnotation.extra`.
///
/// Standard keys (`return`, `constraint`, `doc`, `bind`, `kinds`) are filtered out since
/// they are consumed by the type system. All annotation values are evaluated at function-definition
/// time: literals are extracted directly, expression-valued fields (function/dict/call) are lowered
/// to CoreExpr and evaluated in the definition-site environment.
///
/// This supports the TypeNode protocol requirement that annotations like `as-type: [fn [let u] u]`
/// must be evaluable (see doc/whatif/equirecursive-types.md).
///
/// Called from both `eval_core.rs` (CoreExpr::Fn arm) and `eval.rs` (Fn arm) to avoid
/// duplicating this logic. The two call sites are identical except for crate-path prefixes.
pub(crate) async fn extract_fn_annotation_extra(
    return_ann: Option<&crate::ast::Spanned<crate::ast::Annotation>>,
    frame: &Arc<EvalFrame>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<IndexMap<String, Value>> {
    let Some(ann_spanned) = return_ann else {
        return Ok(IndexMap::new());
    };

    let crate::ast::Annotation::PropertyDict(entries) = &ann_spanned.node else {
        return Ok(IndexMap::new());
    };

    let mut extra = IndexMap::new();

    for e in entries {
        // Extract string key; skip non-string keys
        let Some(key_node) = e.node.key.as_ref() else {
            continue;
        };
        let crate::ast::SurfaceExpression::StringLiteral {
            content: ref key_str,
            ..
        } = key_node.expr
        else {
            continue;
        };

        // Evaluate the annotation value: literals directly, type-level VarRefs as string identity, expressions via eval
        let val = match &e.node.value.expr {
            // Literals extract directly without evaluation
            crate::ast::SurfaceExpression::StringLiteral { content: s, .. } => string_val(s),
            crate::ast::SurfaceExpression::Int(n) => Value::Int {
                n: *n,
                type_val: crate::value::unknown_type_val(),
            },
            crate::ast::SurfaceExpression::Float(f) => Value::Float {
                n: *f,
                type_val: crate::value::unknown_type_val(),
            },
            // Type-level VarRef with resolution Some(None) → produce string identity.
            // This handles @[return: String], @[return: a], @[is: Int], etc.
            crate::ast::SurfaceExpression::VarRef {
                name, resolution, ..
            } if resolution.get() == Some(None) => string_val(name),
            // Expression-valued fields: lower to CoreExpr, evaluate, materialize to Value.
            // Annotations like `as-type: [fn [let u] u]` are now evaluable.
            _ => {
                // Lower SurfaceNode → CoreExpr (inline fields provide all needed type info).
                let core_expr = crate::lower::lower_inner(
                    &e.node.value,
                    &mut Vec::new(),
                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                );

                // Evaluate the CoreExpr to a thunk, then materialize to a Value.
                match eval_core_expr(&core_expr, frame, ctx).await {
                    Ok(thunk) => materialize(&thunk, Some(&e.node.value.span), ctx).await?,
                    Err(e) if matches!(&e.kind, crate::error::ErrorKind::Unimplemented { .. }) => {
                        // Placeholder from nested type expressions → skip this key.
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        };

        extra.insert(key_str.clone(), val);
    }

    Ok(extra)
}

/// Evaluate a CoreExpr to a thunk.
///
/// Variable lookup uses `frame` (EvalFrame closure-conversion, replaces FlatEnv de Bruijn).
/// Dict/call/fn construction uses `ctx` for all scope management.
pub(crate) fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    frame: &'a Arc<EvalFrame>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + Send + 'a>> {
    Box::pin(async move {
        let span = expr.span.clone();
        match &expr.node {
            // Literals produce values directly — no thunk wrapping needed
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::value(
                Value::Int {
                    n: *n,
                    type_val: crate::value::unknown_type_val(),
                },
                span.clone(),
            ))),
            CoreExpr::U64(n) => Ok(Arc::new(Thunk::value(
                Value::U64 {
                    n: *n,
                    type_val: crate::value::unknown_type_val(),
                },
                span.clone(),
            ))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::value(
                Value::Float {
                    n: *f,
                    type_val: crate::value::unknown_type_val(),
                },
                span.clone(),
            ))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::value(string_val(s), span.clone()))),

            // Variable lookup via EvalFrame closure-conversion.
            // All names — builtins, capabilities, and user-defined — resolve through the
            // accumulated_group (frame.group) via their VarAddr. LGM(slot) uses absolute
            // cumulative slot indices: root-scope entries at 0..N-1, dict entries at
            // cumulative offsets above them. No outer-frame traversal needed.
            CoreExpr::Var { name, addr, .. } => {
                let thunk = match addr {
                    VarAddr::Dispatch(_, slot) => frame.group.get(*slot as usize),
                    VarAddr::ClosureCapture(i) => frame.closure_env.get(*i as usize),
                    VarAddr::Parameter(i) => frame.params.get(*i as usize).map(Arc::clone),
                    VarAddr::EffectPerform { class_id, method } => {
                        // T-2137: Scan the accumulated group from innermost (highest slot) to
                        // outermost (slot 0) for instance method implementations.
                        //
                        // An instance method is a `Value::Function` with
                        // `instance_of == Some((class_id, method))`. The scan uses
                        // `peek_result()` to avoid forcing unevaluated thunks — only
                        // already-materialized functions are considered. Instance methods are
                        // defined at document level and are always materialized before any
                        // reference to them via EffectPerform can be evaluated (letrec ordering
                        // within the same dict guarantees definitions precede references within
                        // the same scope; cross-scope references go through accumulated_group
                        // which is populated before evaluation of the referencing expression).
                        //
                        // Candidates are collected in innermost-first order (highest slot first).
                        // All candidates' clauses are concatenated to form a synthesized dispatch
                        // function whose `invoke_function` call tries the first matching clause.
                        let group = &frame.group;
                        let group_len = group.len();
                        let mut all_clauses: Vec<crate::ast::CoreClause> = Vec::new();

                        // Iterate from innermost (highest slot index) to outermost (slot 0).
                        for i in (0..group_len).rev() {
                            let Some(slot_thunk) = group.get(i) else {
                                continue;
                            };
                            if let Some(Ok(Value::Function {
                                clauses,
                                instance_of: Some((cid, m)),
                                ..
                            })) = slot_thunk.peek_result()
                            {
                                if cid == class_id && m == method {
                                    // Found a matching instance — add its clauses in order.
                                    all_clauses.extend(clauses.iter().cloned());
                                }
                            }
                        }

                        if all_clauses.is_empty() {
                            // No instance found — propagate as an evaluation error.
                            // Returning Err here causes the evaluating task to settle
                            // its thunk with this error; all dependents see the error
                            // when they force the thunk. This is the correct path for
                            // "no instance found" — it is a hard error, not a no-match
                            // that falls through to something else.
                            return Err(EvalError::user_error(
                                format!(
                                    "no instance found for typeclass method '{}' (class_id={}): \
                                     no Value::Function with instance_of=Some({}, {:?}) \
                                     in accumulated group ({} entries)",
                                    method, class_id, class_id, method, group_len
                                ),
                                span.clone(),
                            )
                            .into());
                        }

                        // Return a synthesized Value::Function with the concatenated clauses.
                        // `instance_of = None` — this is a resolved dispatch function, not itself
                        // a discoverable instance method. `closure_env = empty` — each candidate
                        // function's closure_env was already materialized at fn-creation time;
                        // the clauses carry their VarAddr-resolved bodies that reference their own
                        // closure frames. However, since we're concatenating clauses from different
                        // functions with different closure_envs, and CallContext only carries a
                        // single closure_env shared across all clauses, we must use the
                        // candidates' own closure_envs at dispatch time.
                        //
                        // INVARIANT: Each candidate's clauses reference VarAddr::ClosureCapture(i)
                        // to access captured variables. The synthesized function's shared closure_env
                        // is empty (vec![]), which means ClosureCapture(i) would fail for any i > 0.
                        // This is correct for the current sprint because instance methods lowered by
                        // lower.rs do not capture outer variables — they are top-level definitions
                        // whose free variables are all in the accumulated_group (Dispatch addrs).
                        //
                        // T-2141 will address multi-instance synthesis with non-empty closure_envs
                        // by carrying per-clause closure environments.
                        let synthesized = Value::Function {
                            clauses: std::sync::Arc::new(all_clauses),
                            instance_of: None,
                            closure_env: std::sync::Arc::new(vec![]),
                            annotation: None,
                            type_val: crate::value::unknown_type_val(),
                        };
                        return Ok(Arc::new(Thunk::value(synthesized, span.clone())));
                    }
                };
                match thunk {
                    Some(t) => Ok(t),
                    None => {
                        // VarAddr resolved to None — the resolver and evaluator are out of sync.
                        // Uses EvalError (not panic!) so the error propagates through the thunk
                        // graph to the caller, producing a useful error message rather than a crash.
                        // Compare with the Fn closure-build arm (below) which panics because a
                        // capture miss there is unrecoverable at fn-creation time.
                        Err(EvalError::undefined_variable(
                            format!(
                                "'{name}' at addr={addr:?} resolved to None — \
                                 resolver/evaluator out of sync \
                                 (frame.group.len()={}, frame.closure_env.len()={}, \
                                 frame.params.len()={})",
                                frame.group.len(),
                                frame.closure_env.len(),
                                frame.params.len(),
                            ),
                            span.clone(),
                        )
                        .into())
                    }
                }
            }

            // Variant: first-class variant constructor emitted by lower.rs for type declarations.
            // Unit variants materialize directly; payload variants evaluate their inner expression,
            // materialize it, and store as an Arc<Thunk> — preserving the laziness invariant that
            // the payload dict's fields remain as thunks until accessed.
            //
            // type_val: look up the type identity from ctx.type_identity_registry using the
            // type_decl_id embedded in this CoreExpr::Variant node. If type_decl_id is 0
            // (no parent TypeDecl — shouldn't happen in practice), fall back to unknown_type_val().
            // Registered identities are set by CoreExpr::TypeDecl before any constructor is forced.
            CoreExpr::Variant {
                tag,
                payload,
                type_decl_id,
            } => {
                let type_val = if *type_decl_id == 0 {
                    crate::value::unknown_type_val()
                } else {
                    ctx.type_identity_registry
                        .lock()
                        .map_err(|e| {
                            EvalError::internal(
                                format!("type_identity_registry mutex poisoned: {e}"),
                                span.clone(),
                            )
                        })?
                        .get(type_decl_id)
                        .map(Arc::clone)
                        .unwrap_or_else(crate::value::unknown_type_val)
                };
                match payload {
                    None => Ok(Arc::new(Thunk::value(
                        Value::Variant {
                            type_val,
                            type_decl_id: *type_decl_id,
                            ctor: Arc::from(tag.as_str()),
                            payload: None,
                        },
                        span.clone(),
                    ))),
                    Some(inner_expr) => {
                        let payload_thunk = eval_core_expr(inner_expr, frame, ctx).await?;
                        let payload_val = materialize(&payload_thunk, Some(&span), ctx).await?;
                        Ok(Arc::new(Thunk::value(
                            Value::Variant {
                                type_val,
                                type_decl_id: *type_decl_id,
                                ctor: Arc::from(tag.as_str()),
                                payload: Some(Arc::new(Thunk::value(payload_val, span.clone()))),
                            },
                            span.clone(),
                        )))
                    }
                }
            }

            // Unit variant: look up type identity by type_decl_id (B-714).
            // If CoreExpr::TypeDecl was evaluated first (which it always is, because forcing
            // Color.Red requires first forcing Color to access its "Red" entry), the registry
            // entry for `type_decl_id` exists and every variant of that type carries the same Arc.
            CoreExpr::UnitVariant {
                tycon,
                ctor,
                type_decl_id,
            } => {
                let type_val = if *type_decl_id == 0 {
                    crate::value::unknown_type_val()
                } else {
                    ctx.type_identity_registry
                        .lock()
                        .map_err(|e| {
                            EvalError::internal(
                                format!("type_identity_registry mutex poisoned: {e}"),
                                span.clone(),
                            )
                        })?
                        .get(type_decl_id)
                        .map(Arc::clone)
                        .unwrap_or_else(crate::value::unknown_type_val)
                };
                Ok(Arc::new(Thunk::value(
                    Value::Variant {
                        type_val,
                        type_decl_id: *type_decl_id,
                        ctor: Arc::from(format!("{}.{}", tycon, ctor).as_str()),
                        payload: None,
                    },
                    span.clone(),
                )))
            }

            // Sequential: evaluate each expression in order, extending the environment
            // with dict bindings from each intermediate dict expression.
            // Sequential: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via LetrecChainStep continuations.
            // This eliminates async recursion on the Rust stack for deeply nested sequential blocks.
            CoreExpr::Sequential(_) => Ok(Arc::new(Thunk::core_expr(
                Arc::new(expr.clone()),
                Arc::clone(frame),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Dict: call eval_dict_core with the current frame so dict value thunks inherit
            // the outer closure_env for ClosureCapture variable lookups, and so the letrec
            // group Vec is populated with the dict's own static-key thunks.
            CoreExpr::Dict(entries) => eval_dict_core(entries, frame, ctx, &span).await,

            // Call: use eval_call_core with the current EvalFrame so argument expressions
            // resolve Dispatch/ClosureCapture variables against the call site's scope.
            CoreExpr::Call {
                func,
                args,
                named_args,
                ..
            } => {
                eval_call_core(
                    func,
                    args,
                    named_args,
                    0,
                    ctx,
                    &span,
                    Arc::new(expr.clone()),
                    frame,
                )
                .await
            }

            // Fn: build Value::Function carrying the full clause list (T-2131).
            // `invoke_function` (eval_call.rs) iterates clauses via `try_clause` for dispatch.
            // T-2133 completes full multi-clause dispatch; single-clause functions work today.
            CoreExpr::Fn {
                clauses,
                captures,
                instance_of,
                resolved_fn_type,
                return_ann,
                ..
            } => {
                // All clauses are stored in Value::Function.clauses for multi-clause dispatch.
                // T-2133 implements full multi-clause dispatch in invoke_function.
                assert!(!clauses.is_empty(), "Fn must have at least one clause");

                // Populate extra from annotation fields (literals + expressions).
                // `doc` is now included in extra: triple-quoted strings desugar to
                // `[unindent "..."]` (a Call), which is evaluated here at definition time.
                // Expression-valued fields are evaluated at function-definition time.
                // return_ann is now carried from SurfaceExpression::Fn through CoreExpr::Fn
                // so that trace: and other annotation-driven runtime behaviors work correctly.
                let extra = extract_fn_annotation_extra(return_ann.as_ref(), frame, ctx).await?;

                // Derive FnAnnotation.doc from extra["doc"] so triple-quoted doc strings
                // (evaluated via `[unindent "..."]`) produce the correct runtime string.
                let doc: Option<String> = extra
                    .get("doc")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));

                // Always construct FnAnnotation — source_span is always available even for
                // unannotated functions, enabling ast-of and LSP go-to-definition.
                let annotation = Some(Box::new(crate::value::FnAnnotation {
                    doc,
                    return_ann: return_ann.as_ref().map(|a| a.node.clone()),
                    source_file: ctx.config.source_file.clone(),
                    source_span: span.clone(),
                    extra,
                }));

                // Store the body directly as Arc<Spanned<CoreExpr>>.
                // CoreExpr::Fn.clauses[0].body is Arc<Spanned<CoreExpr>> — no conversion needed.
                //
                // Build closure_env by looking up each captured variable in the current frame.
                // frame.group is the accumulated_group, which contains root entries at slots
                // 0..N-1 and all prior dict entries at cumulative slots above them.
                // Dispatch(depth, slot) absolute_slot looks up frame.group[slot] directly.
                // using its ORIGINAL VarAddr (from before the resolver converted it to
                // ClosureCapture for references inside this function).
                //
                // Each entry in `captures` is (name, original_addr) where original_addr is the
                // VarAddr the binding holds in the ENCLOSING EvalFrame:
                //   - Dispatch(_, i) → frame.group[i]   (absolute cumulative slot in
                //     accumulated_group: root entries at 0..N-1, dict entries at cumulative
                //     offsets. Builtins, capabilities, and prior-dict entries all use Dispatch.)
                //   - ClosureCapture(i)    → frame.closure_env[i] (outer fn closure capture)
                //   - Parameter(i)         → frame.params[i]  (outer function argument)
                //
                // Build closure_env in capture-index order. Use map (not filter_map) so that
                // each ClosureCapture(i) in the body resolves to closure_env[i] — skipping
                // entries would shift all subsequent indices.
                let mut closure_env_vec: Vec<Arc<Thunk>> = Vec::with_capacity(captures.len());
                for (name, original_addr) in captures.iter() {
                    let found = match original_addr {
                        VarAddr::Dispatch(_, slot) => frame.group.get(*slot as usize),
                        VarAddr::ClosureCapture(i) => frame.closure_env.get(*i as usize),
                        VarAddr::Parameter(i) => frame.params.get(*i as usize).map(Arc::clone),
                        VarAddr::EffectPerform { .. } => {
                            return Err(EvalError::internal(
                                format!(
                                    "EffectPerform address in closure capture for '{}' — \
                                     resolver invariant violated: EffectPerform addresses \
                                     cannot appear in fn capture lists",
                                    name
                                ),
                                span.clone(),
                            )
                            .into());
                        }
                    };
                    let thunk = found.unwrap_or_else(|| {
                        // Improve error message with capture list details
                        let cap_list_str = captures
                            .iter()
                            .enumerate()
                            .map(|(idx, (n, addr))| format!("  [{idx}] {n}: {addr:?}"))
                            .collect::<Vec<_>>()
                            .join("\n");
                        panic!(
                            "capture miss for '{}': {:?} resolved to None\n\
                             Frame state:\n\
                             - closure_env.len()={}\n\
                             - group.len()={}\n\
                             - params.len()={}\n\
                             Full capture list ({} entries):\n{}",
                            name,
                            original_addr,
                            frame.closure_env.len(),
                            frame.group.len(),
                            frame.params.len(),
                            captures.len(),
                            cap_list_str,
                        )
                    });
                    closure_env_vec.push(thunk);
                }
                Ok(Arc::new(Thunk::value(
                    Value::Function {
                        clauses: Arc::new(clauses.clone()),
                        closure_env: Arc::new(closure_env_vec),
                        instance_of: instance_of.clone(),
                        annotation,
                        type_val: resolved_fn_type
                            .as_ref()
                            .map(Arc::clone)
                            .unwrap_or_else(crate::value::unknown_type_val),
                    },
                    span.clone(),
                )))
            }

            // TypeAssert: wrap as CoreExpr thunk — force_step's take_core_expr branch
            // handles CoreExpr::TypeAssert inline, pushing a TypeAssertCheck continuation.
            // Wrapping here prevents direct recursion back through eval_core_expr.
            CoreExpr::TypeAssert { .. } => Ok(Arc::new(Thunk::core_expr(
                Arc::new(expr.clone()),
                Arc::clone(frame),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Rest: error (only valid in type expressions)
            CoreExpr::Rest(_) => Err(EvalError::internal(
                "rest marker (...) is only valid inside type expressions".to_string(),
                span.clone(),
            )
            .into()),

            // Match: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via MatchDispatch and MatchGuardCheck continuations.
            // This eliminates async recursion on the Rust stack for deeply nested match chains.
            CoreExpr::Match { .. } => Ok(Arc::new(Thunk::core_expr(
                Arc::new(expr.clone()),
                Arc::clone(frame),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Quote: convert CoreExpr→SurfaceNode and walk with eval_quote_walk.
            // The inner CoreExpr was lowered (giving unquotes proper variable slots),
            // then converted back here for structural traversal. CoreExpr::Var preserves
            // the original name alongside the slot so the round-trip is lossless.
            CoreExpr::Quote(inner) => {
                let surface_node = crate::lower::core_expr_to_surface_node(inner);
                eval_quote_walk(surface_node, frame, ctx).await
            }

            // Unquote: error (only valid inside quote)
            CoreExpr::Unquote(_) => Err(EvalError::internal(
                "unquote is only valid inside [quote ...]".to_string(),
                span.clone(),
            )
            .into()),

            // UnquoteSplice: error (only valid inside quote)
            CoreExpr::UnquoteSplice(_) => Err(EvalError::internal(
                "unquote-splice is only valid inside [quote ...]".to_string(),
                span.clone(),
            )
            .into()),

            // PatternDecl: error (not an expression)
            CoreExpr::PatternDecl { .. } => Err(EvalError::internal(
                "pattern declaration is only valid in instance match arms".to_string(),
                span.clone(),
            )
            .into()),

            // LetDecl in sequential fn-body context: evaluate as a Dict of (name → lazy-thunk) pairs.
            //
            // Syntax: [let name value] → bindings = [Str("name"), value_expr]
            // (lower_let_decl_binding converts declaration-position VarRef/Annotated/Rest to Str)
            // Pairs are (bindings[2i], bindings[2i+1]).
            // Returns a Dict so the LetrecChainStep can extract keys via its Dict-based binding logic.
            CoreExpr::LetDecl { bindings } => {
                let mut dict: IndexMap<HashableValue, Arc<crate::value::Thunk>> = IndexMap::new();
                let mut i = 0;
                while i + 1 < bindings.len() {
                    let name_expr = &bindings[i];
                    let val_expr = &bindings[i + 1];
                    let name = match &name_expr.node {
                        // lower_let_decl_binding converts declaration-position names to Str literals.
                        CoreExpr::Str(n) => n.clone(),
                        // Var node in declaration position: extract the name string directly.
                        // Annotated Var (Var { annotation: Some(_) }) is also handled here.
                        CoreExpr::Var { name: n, .. } => n.clone(),
                        _ => {
                            return Err(EvalError::internal(
                                format!(
                                    "let binding name must be an identifier, got: {:?}",
                                    name_expr.node
                                ),
                                name_expr.span.clone(),
                            )
                            .into());
                        }
                    };
                    let val_thunk = Arc::new(Thunk::core_expr(
                        Arc::new(val_expr.clone()),
                        Arc::clone(frame),
                        Arc::clone(ctx),
                        val_expr.span.clone(),
                    ));
                    dict.insert(
                        HashableValue::Str(Arc::from(name.as_str())),
                        Arc::clone(&val_thunk),
                    );
                    i += 2;
                }
                Ok(Arc::new(Thunk::value(
                    Value::Dict {
                        entries: dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    span.clone(),
                )))
            }

            CoreExpr::Placeholder => Err(EvalError::unimplemented(
                "placeholder `...` was evaluated — replace with an implementation".to_string(),
                span.clone(),
            )
            .into()),

            // TypeDecl: create a stable Arc-level identity for the type, register it in
            // ctx.type_identity_registry, evaluate the inner constructor dict, stamp the
            // identity on the dict's type_val, and return the dict.
            //
            // This is the T-2111 implementation of B-712. The identity is a fresh
            // Arc<Value::Variant { ctor: type_name, ... }> — unique per TypeDecl evaluation.
            // Storing and retrieving Arc::clone of the same Arc from the registry ensures
            // that all Value::Variants produced by this type's constructors share the
            // exact same Arc pointer as the constructor dict, enabling Arc::ptr_eq in
            // match_pattern to correctly identify type membership.
            //
            // B-714: registry is keyed by type_decl_id (u64) instead of type_name (String)
            // so that same-name types in nested scopes get independent identities.
            //
            // Necessary strictness: materializing the inner dict (CoreExpr::Dict or
            // CoreExpr::ReprDecl wrapping a Dict) produces a Value::Dict with Arc<Thunk>
            // entries — the entries themselves are NOT materialized. This is the same
            // strictness point as CoreExpr::ReprDecl (which also materializes for registration).
            CoreExpr::TypeDecl {
                type_name,
                type_decl_id,
                inner,
            } => {
                // Create the type identity: a fresh Arc<Value> unique to this type declaration.
                // The value is a unit Variant with ctor = type_name. This makes the identity
                // self-describing in debug output. Its own type_val is unknown_type_val() —
                // the identity is not itself typed (it is the identity, not a user value).
                let type_id: Arc<Value> = Arc::new(Value::Variant {
                    type_val: crate::value::unknown_type_val(),
                    type_decl_id: 0, // Identity sentinel — not a user-constructed variant.
                    ctor: Arc::from(type_name.as_str()),
                    payload: None,
                });

                // Register BEFORE evaluating inner so that any constructor thunks created
                // during inner evaluation (if inner is somehow forced immediately, which
                // eval_dict_core does not do — it creates lazy thunks) can find the identity.
                // In practice, unit variant thunks are CoreExpr::UnitVariant and are only
                // forced when accessed (e.g., Color.Red), which happens AFTER this TypeDecl
                // evaluation completes and the dict is returned.
                ctx.type_identity_registry
                    .lock()
                    .map_err(|e| {
                        EvalError::internal(
                            format!("type_identity_registry mutex poisoned: {e}"),
                            span.clone(),
                        )
                    })?
                    .insert(*type_decl_id, Arc::clone(&type_id));

                // Evaluate the inner constructor dict (or ReprDecl wrapping a dict).
                let inner_thunk = eval_core_expr(inner, frame, ctx).await?;

                // Materialize the dict to stamp type_val. This is a necessary strictness
                // point: we need the Value::Dict to replace its type_val field. The dict's
                // entries (constructor thunks) are NOT materialized — they remain as
                // Arc<Thunk> inside the IndexMap. Only the container structure is extracted.
                let inner_val = materialize(&inner_thunk, Some(&span), ctx).await?;

                // Stamp the type identity on the constructor dict's type_val.
                let stamped_val = match inner_val {
                    Value::Dict { entries, .. } => Value::Dict {
                        entries,
                        type_val: Arc::clone(&type_id),
                    },
                    other => {
                        return Err(EvalError::internal(
                            format!(
                                "CoreExpr::TypeDecl: inner expression evaluated to non-Dict value: {}",
                                other.type_name()
                            ),
                            span.clone(),
                        )
                        .into());
                    }
                };

                Ok(Arc::new(Thunk::value(stamped_val, span.clone())))
            }

            // ReprDecl: evaluate the inner constructor dict, register in ctx.repr_registry,
            // and return the inner dict thunk. The dict IS the TypeValue — ReprDecl is
            // transparent to callers; they see the same value as a plain [type ...] declaration.
            //
            // The `repr` string is validated against the Value variant allowlist before
            // insertion. If the `is:` predicate is present it is evaluated and registered
            // in ctx.is_predicates under the same key.
            CoreExpr::ReprDecl {
                repr,
                is_pred,
                inner,
            } => {
                // Validate repr string against the known Value variant names.
                if !is_valid_repr_string(&repr) {
                    return Err(EvalError::user_error(
                        format!("repr: {:?} is not a known Value variant", repr),
                        span.clone(),
                    )
                    .into());
                }

                // Evaluate the inner constructor dict.
                let inner_thunk = eval_core_expr(inner, frame, ctx).await?;

                // Materialize to get the constructor dict Value for registration.
                // This is a necessary strictness point: we need the concrete Value to store
                // in repr_registry. The thunk is returned as the expression result so callers
                // still see a lazy thunk (already-settled at this point).
                let inner_val = materialize(&inner_thunk, Some(&span), ctx).await?;

                // Register in repr_registry: repr_string → Arc<Value> (the constructor dict).
                ctx.repr_registry
                    .lock()
                    .map_err(|e| {
                        EvalError::internal(
                            format!("repr_registry mutex poisoned: {e}"),
                            span.clone(),
                        )
                    })?
                    .insert(repr.clone(), Arc::new(inner_val));

                // If an is: predicate is present, evaluate and register it.
                if let Some(is_expr) = is_pred {
                    let is_thunk = eval_core_expr(is_expr, frame, ctx).await?;
                    let is_val = materialize(&is_thunk, Some(&span), ctx).await?;
                    ctx.is_predicates
                        .lock()
                        .map_err(|e| {
                            EvalError::internal(
                                format!("is_predicates mutex poisoned: {e}"),
                                span.clone(),
                            )
                        })?
                        .insert(repr.clone(), Arc::new(is_val));
                }

                // Return the inner dict thunk — ReprDecl is transparent.
                Ok(inner_thunk)
            }
        }
        // Type guards are now inline on AST nodes (TypeAnnotation OnceLock);
        // the lowerer wraps them in CoreExpr::TypeAssert. No runtime guard wrapping needed here.
    }) // end Box::pin(async move {
}

/// Returns `true` if `s` is a valid `repr:` string for [`CoreExpr::ReprDecl`].
///
/// Each entry in the slice corresponds to exactly one variant of [`Value`] via the
/// canonical `REPR_*` constant from `type_tags`. When a new `Value` variant is added,
/// this function must be updated — the compiler cannot enforce this directly, but the
/// test [`tests::test_is_valid_repr_string`] verifies every entry explicitly and will
/// fail if an entry is removed or a new variant is added without updating here.
///
/// `Value::Builder` is intentionally excluded: Builder is a transient accumulator that
/// is consumed before type identity matters; it cannot meaningfully participate in the
/// `repr:` type-identity protocol.
fn is_valid_repr_string(s: &str) -> bool {
    [
        REPR_INT,
        REPR_U64,
        REPR_FLOAT,
        REPR_STRING,
        REPR_BYTES,
        REPR_DICT,
        REPR_FUNCTION,
        REPR_BUILTIN,
        REPR_PROXY,
        REPR_VARIANT,
        REPR_DECIMAL,
        REPR_BIGINT,
        REPR_DURATION,
        REPR_URI,
        REPR_TIMESTAMP,
        REPR_TIMEZONE,
        REPR_CLOCK_CAP,
        REPR_DIR_CAP,
        REPR_NET_CAP,
        REPR_FILE,
        REPR_REVOCABLE_DIR_CAP,
        REPR_QUIC_SESSION,
        REPR_HTTP2_SESSION,
        REPR_HTTP3_SESSION,
        REPR_QUIC_DATAGRAM_HANDLE,
        REPR_TASK,
        REPR_CHANNEL,
        REPR_BROADCAST_CHANNEL,
        REPR_ONESHOT_SENDER,
        REPR_ONESHOT_RECEIVER,
        REPR_CONTEXT,
        REPR_REACTIVE_CELL,
        REPR_ARENA,
        REPR_TYPE_CONTEXT,
        REPR_PROGRAM,
        REPR_DOCUMENT,
        REPR_EXPRESSION,
        REPR_CORE_DOCUMENT,
        REPR_ANNOTATED,
    ]
    .contains(&s)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::ast::{CoreExpr, Spanned, VarAddr};
    use crate::eval::EvalContext;
    use crate::test_util::test_span;
    use crate::value::{string_val, EvalFrame, Thunk, Value};

    fn test_ctx() -> Arc<EvalContext> {
        EvalContext::new()
    }

    async fn eval_and_materialize(
        expr: CoreExpr,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        let span = test_span(1, 1, 1, 5);
        let spanned = Spanned::new(expr, span.clone());
        let frame = EvalFrame::empty();
        let thunk = super::eval_core_expr(&spanned, &frame, ctx).await?;
        crate::eval::materialize(&thunk, Some(&span), ctx).await
    }

    /// `CoreExpr::Int(42)` evaluates to `Value::Int(42)`.
    ///
    /// Int literals return a pre-materialized Thunk::value without going
    /// through the CEK machine.
    #[tokio::test]
    async fn test_eval_int_literal() {
        let ctx = test_ctx();
        let val = eval_and_materialize(CoreExpr::Int(42), &ctx).await.unwrap();
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val()
            },
            "CoreExpr::Int(42) must evaluate to Int(42)"
        );
    }

    /// `CoreExpr::Str("hello")` evaluates to the corresponding string Value.
    ///
    /// String literals return Thunk::value directly — no CEK machine needed.
    /// We verify the value using string_val to account for the intern/slice representation.
    #[tokio::test]
    async fn test_eval_string_literal() {
        let ctx = test_ctx();
        let val = eval_and_materialize(CoreExpr::Str("hello".to_string()), &ctx)
            .await
            .unwrap();
        assert_eq!(
            val,
            string_val("hello"),
            "CoreExpr::Str(\"hello\") must evaluate to the string value 'hello'"
        );
    }

    /// `CoreExpr::Var` resolves a thunk from the EvalFrame via VarAddr.
    ///
    /// We construct an EvalFrame with a known thunk in the group slot and then
    /// evaluate a Var node with VarAddr::Dispatch(0, 0). The returned
    /// value must match the injected value.
    #[tokio::test]
    async fn test_eval_varref() {
        use crate::value::GroupSpine;
        let span = test_span(1, 1, 1, 5);
        let ctx = test_ctx();

        // Build an EvalFrame with the known thunk in group[0].
        let known_thunk = Arc::new(Thunk::value(
            Value::Int {
                n: 77,
                type_val: crate::value::unknown_type_val(),
            },
            span.clone(),
        ));
        let frame = Arc::new(EvalFrame {
            closure_env: GroupSpine::empty(),
            group: GroupSpine::from_flat(vec![Arc::clone(&known_thunk)]),
            params: Arc::new(vec![]),
        });

        // Build a Var node: addr = Dispatch(0, 0).
        let var_expr = Spanned::new(
            CoreExpr::Var {
                name: "test-binding".to_string(),
                addr: VarAddr::Dispatch(0, 0),
                annotation: None,
            },
            span.clone(),
        );

        // Evaluate the Var with the frame — should return the injected thunk.
        let result_thunk = super::eval_core_expr(&var_expr, &frame, &ctx)
            .await
            .unwrap();
        let val = crate::eval::materialize(&result_thunk, Some(&span), &ctx)
            .await
            .unwrap();

        assert_eq!(
            val,
            Value::Int {
                n: 77,
                type_val: crate::value::unknown_type_val()
            },
            "CoreExpr::Var with VarAddr::Dispatch(0, 0) must resolve to the injected Int(77)"
        );
    }

    /// Verifies that `is_valid_repr_string` returns `true` for every expected `Value`
    /// variant name and `false` for unknown strings.
    ///
    /// Each entry corresponds to one `REPR_*` constant from `type_tags`. If a constant
    /// is removed or a new variant is added without updating `is_valid_repr_string`,
    /// this test will fail.
    #[test]
    fn test_is_valid_repr_string() {
        use crate::type_tags::*;
        // Every entry that should be accepted — one per Value variant (excluding Builder).
        let valid = [
            REPR_INT,
            REPR_U64,
            REPR_FLOAT,
            REPR_STRING,
            REPR_BYTES,
            REPR_DICT,
            REPR_FUNCTION,
            REPR_BUILTIN,
            REPR_PROXY,
            REPR_VARIANT,
            REPR_DECIMAL,
            REPR_BIGINT,
            REPR_DURATION,
            REPR_URI,
            REPR_TIMESTAMP,
            REPR_TIMEZONE,
            REPR_CLOCK_CAP,
            REPR_DIR_CAP,
            REPR_NET_CAP,
            REPR_FILE,
            REPR_REVOCABLE_DIR_CAP,
            REPR_QUIC_SESSION,
            REPR_HTTP2_SESSION,
            REPR_HTTP3_SESSION,
            REPR_QUIC_DATAGRAM_HANDLE,
            REPR_TASK,
            REPR_CHANNEL,
            REPR_BROADCAST_CHANNEL,
            REPR_ONESHOT_SENDER,
            REPR_ONESHOT_RECEIVER,
            REPR_CONTEXT,
            REPR_REACTIVE_CELL,
            REPR_ARENA,
            REPR_TYPE_CONTEXT,
            REPR_PROGRAM,
            REPR_DOCUMENT,
            REPR_EXPRESSION,
            REPR_CORE_DOCUMENT,
            REPR_ANNOTATED,
        ];
        for s in &valid {
            assert!(
                super::is_valid_repr_string(s),
                "is_valid_repr_string must return true for {:?}",
                s
            );
        }

        // Builder is explicitly excluded — it is a transient accumulator.
        assert!(
            !super::is_valid_repr_string("Value::Builder"),
            "is_valid_repr_string must return false for Value::Builder (transient accumulator)"
        );

        // Unknown strings must be rejected.
        assert!(
            !super::is_valid_repr_string("Value::Unknown"),
            "is_valid_repr_string must return false for unknown variant names"
        );
        assert!(
            !super::is_valid_repr_string(""),
            "is_valid_repr_string must return false for empty string"
        );
        assert!(
            !super::is_valid_repr_string("Int"),
            "is_valid_repr_string must return false for variant name without 'Value::' prefix"
        );
    }
}
