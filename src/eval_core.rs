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

use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval_call_core, eval_dict_core, materialize, EvalContext};
use crate::value::ThunkId;
use crate::value::{string_val, HashableValue, Thunk, Value};

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
        Value::Int(n) => Ok(make_node(SurfaceExpression::Int(*n))),
        Value::Float(f) => Ok(make_node(SurfaceExpression::Float(*f))),
        Value::String { source, start, end } => Ok(make_node(SurfaceExpression::StringLiteral {
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
        Value::Dict(dict) => {
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
/// Seq inputs are no longer supported (T-1324: Rust must not know Seq's internal structure).
async fn collect_seq_elements(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Vec<Value>> {
    let mut elements = Vec::new();
    let current = value.clone();

    loop {
        match current {
            Value::Dict(ref dict) => {
                // Integer-keyed Dict (from macro variadic args) — collect elements in key order
                // Validate that all keys are integers and sequential from 0
                let mut int_entries: Vec<(i64, ThunkId)> = Vec::new();
                for (key, thunk_id) in dict {
                    if let HashableValue::Int(i) = key {
                        int_entries.push((*i, *thunk_id));
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
                for (_, thunk_id) in int_entries {
                    let thunk = ctx.get_thunk(thunk_id);
                    let value = materialize(&thunk, Some(&span), ctx).await?;
                    elements.push(value);
                }

                // Done — Dict entries have been processed
                break;
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
    env_id: u32,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = EvalResult<Arc<crate::ast::SurfaceNode>>> + 'a>,
> {
    use crate::ast::{
        SurfaceDeclaration, SurfaceEntry, SurfaceExpression, SurfaceMatchArm, SurfaceNamedArg,
        SurfaceNode,
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
                let thunk = eval_core_expr(&core, env_id, ctx).await?;
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
                        let thunk = eval_core_expr(&core, env_id, ctx).await?;
                        let inner_span = inner.span.clone();
                        let value = materialize(&thunk, Some(&inner_span), ctx).await?;

                        // Value must be Dict or Seq for splicing
                        match value {
                            Value::Dict(ref dict) => {
                                // Splice dict entries into the current dict
                                for (key, value_thunk_id) in dict {
                                    // Convert the thunk to a Value, then to a SurfaceNode
                                    let value_thunk = ctx.get_thunk(*value_thunk_id);
                                    let value_val =
                                        materialize(&value_thunk, Some(&inner_span), ctx).await?;
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
                                                        call_dispatch:
                                                            crate::ast::CallDispatch::new(),
                                                        annotation: None,
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
                        let processed_value =
                            eval_quote_preprocess(Arc::clone(&entry.node.value), env_id, ctx)
                                .await?;
                        let processed_key = if let Some(ref key_node) = entry.node.key {
                            Some(eval_quote_preprocess(Arc::clone(key_node), env_id, ctx).await?)
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
            } => {
                let processed_func = eval_quote_preprocess(Arc::clone(func), env_id, ctx).await?;
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
                        let thunk = eval_core_expr(&core, env_id, ctx).await?;
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
                        processed_args
                            .push(eval_quote_preprocess(Arc::clone(arg), env_id, ctx).await?);
                    }
                }
                let mut processed_named_args: Vec<Spanned<SurfaceNamedArg>> =
                    Vec::with_capacity(named_args.len());
                for na in named_args {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&na.node.value), env_id, ctx).await?;
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
                }))
            }

            SurfaceExpression::Fn {
                return_ann,
                params,
                body,
                desugared,
            } => {
                let processed_body = eval_quote_preprocess(Arc::clone(body), env_id, ctx).await?;
                Ok(make_node(SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: processed_body,
                    desugared: *desugared,
                }))
            }

            SurfaceExpression::Field {
                expr: Some(target),
                field,
                ..
            } => {
                let processed_target =
                    eval_quote_preprocess(Arc::clone(target), env_id, ctx).await?;
                Ok(make_node(SurfaceExpression::Field {
                    expr: Some(processed_target),
                    field: field.clone(),
                    resolution: crate::ast::Resolution::new(),
                    field_slot: crate::ast::SlotAnnotation::new(),
                }))
            }

            // Leading-dot is a terminal in quote context — no sub-expression to preprocess.
            SurfaceExpression::Field {
                expr: None, field, ..
            } => Ok(make_node(SurfaceExpression::Field {
                expr: None,
                field: field.clone(),
                resolution: crate::ast::Resolution::new(),
                field_slot: crate::ast::SlotAnnotation::new(),
            })),

            SurfaceExpression::Pipe { lhs, rhs } => {
                let processed_lhs = eval_quote_preprocess(Arc::clone(lhs), env_id, ctx).await?;
                let processed_rhs = eval_quote_preprocess(Arc::clone(rhs), env_id, ctx).await?;
                Ok(make_node(SurfaceExpression::Pipe {
                    lhs: processed_lhs,
                    rhs: processed_rhs,
                }))
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut processed_exprs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    processed_exprs.push(eval_quote_preprocess(Arc::clone(e), env_id, ctx).await?);
                }
                Ok(make_node(SurfaceExpression::Sequential(processed_exprs)))
            }

            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
                ..
            } => {
                let processed_expr = eval_quote_preprocess(Arc::clone(inner), env_id, ctx).await?;
                Ok(make_node(SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: processed_expr,
                    resolved_type: crate::ast::TypeAnnotation::new(),
                }))
            }

            SurfaceExpression::Quote(inner) => {
                // Nested quote: recurse so inner unquotes are still processed.
                let processed_inner = eval_quote_preprocess(Arc::clone(inner), env_id, ctx).await?;
                Ok(make_node(SurfaceExpression::Quote(processed_inner)))
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                let processed_scrutinee =
                    eval_quote_preprocess(Arc::clone(scrutinee), env_id, ctx).await?;
                let mut processed_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let mut processed_body = Vec::with_capacity(arm.body.len());
                    for body_expr in &arm.body {
                        processed_body
                            .push(eval_quote_preprocess(Arc::clone(body_expr), env_id, ctx).await?);
                    }
                    let processed_guard = if let Some(ref guard) = arm.guard {
                        Some(eval_quote_preprocess(Arc::clone(guard), env_id, ctx).await?)
                    } else {
                        None
                    };
                    processed_arms.push(SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: processed_guard,
                        body: processed_body,
                        guard_matchable_binding: arm.guard_matchable_binding.clone(),
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
                            eval_quote_preprocess(Arc::clone(body), env_id, ctx).await?;
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
                            eval_quote_preprocess(Arc::clone(pattern), env_id, ctx).await?;
                        SurfaceDeclaration::SyntaxClass {
                            name: name.clone(),
                            pattern: processed_pattern,
                            message: message.clone(),
                        }
                    }
                    SurfaceDeclaration::Splice(forms) => {
                        let mut processed_forms = Vec::with_capacity(forms.len());
                        for form in forms {
                            processed_forms
                                .push(eval_quote_preprocess(Arc::clone(form), env_id, ctx).await?);
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
    env_id: u32,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = node.span.clone();
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_node = eval_quote_preprocess(node, env_id, ctx).await?;

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
    env_id: u32,
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

        // Skip annotation keys that cannot be safely evaluated as runtime values.
        // See ast::ANNOTATION_EVAL_EXCLUDED_KEYS for the rationale.
        if crate::ast::ANNOTATION_EVAL_EXCLUDED_KEYS.contains(&key_str.as_str()) {
            continue;
        }

        // Evaluate the annotation value: literals fast-path, expressions via eval
        let val = match &e.node.value.expr {
            // Fast path: literals extract directly without evaluation
            crate::ast::SurfaceExpression::StringLiteral { content: s, .. } => string_val(s),
            crate::ast::SurfaceExpression::Int(n) => Value::Int(*n),
            crate::ast::SurfaceExpression::Float(f) => Value::Float(*f),
            // Expression-valued fields: lower to CoreExpr, evaluate, materialize to Value.
            // This is the T-1124 fix: annotations like `as-type: [fn [let u] u]` are now evaluable.
            _ => {
                // Lower SurfaceNode → CoreExpr (inline fields provide all needed type info).
                let core_expr = crate::lower::lower_inner(
                    &e.node.value,
                    &mut Vec::new(),
                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                );

                // Evaluate the CoreExpr to a thunk, then materialize to a Value.
                let thunk = eval_core_expr(&core_expr, env_id, ctx).await?;
                materialize(&thunk, Some(&e.node.value.span), ctx).await?
            }
        };

        extra.insert(key_str.clone(), val);
    }

    Ok(extra)
}

/// Evaluate a CoreExpr to a thunk.
///
/// Variable lookup uses `env_id` (FlatEnv de Bruijn dispatch, T-1558).
/// Dict/call/fn construction uses `ctx` for all scope management.
pub(crate) fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env_id: u32,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + 'a>> {
    Box::pin(async move {
        if crate::memory_budget::is_oom_flagged() {
            return Err(crate::error::EvalError::resource_limit_exceeded(
                "heap limit exceeded (arena bytes)".to_string(),
                expr.span.clone(),
            )
            .into());
        }
        let span = expr.span.clone();
        match &expr.node {
            // Fast path: literals materialize directly without wrapping in Unevaluated
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::value(Value::Int(*n), span.clone()))),
            CoreExpr::U64(n) => Ok(Arc::new(Thunk::value(Value::U64(*n), span.clone()))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::value(Value::Float(*f), span.clone()))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::value(string_val(s), span.clone()))),

            // Variable lookup with de Bruijn coordinates.
            // Coordinates are assigned at resolve time; slot lookup is fatal on miss —
            // there is no name-based fallback. A miss means the resolver failed to assign
            // correct coordinates, which is a compiler bug.
            CoreExpr::Var {
                name, level, slot, ..
            } => {
                // FlatEnv dispatch via parent-chain traversal.
                // Walk the parent chain `level` times from the current scope.
                // level=0 is innermost (current scope — no hops needed)
                // level=N is N scopes outward → walk N parent pointers
                let thunk = {
                    let arena = ctx.scope_arena.borrow();
                    let level_idx = *level as usize;
                    match arena.walk_parent_chain(env_id, level_idx) {
                        Ok(target_env_id) => {
                            let slot_idx = *slot as usize;
                            arena.scopes[target_env_id.0 as usize]
                                .get(slot_idx as u32)
                                .map(Arc::clone)
                        }
                        Err(depth_reached) => {
                            // Build a summary of scope levels for diagnostics.
                            let chain = arena.collect_parent_chain(env_id);
                            let scope_depth = chain.len();
                            let scope_summary: String = chain
                                .iter()
                                .enumerate()
                                .map(|(chain_idx, env_id)| {
                                    // chain[0] is root (outermost), chain[N-1] is innermost.
                                    // level 0 = innermost = chain[N-1], level k = chain[N-1-k].
                                    let scope_level = scope_depth - 1 - chain_idx;
                                    let preview: Vec<String> = arena.scopes[env_id.0 as usize]
                                        .slots
                                        .iter()
                                        .filter_map(|t| {
                                            t.as_ref()?.span.name.as_deref().map(str::to_string)
                                        })
                                        .take(5)
                                        .collect();
                                    let total_names = arena.scopes[env_id.0 as usize]
                                        .slots
                                        .iter()
                                        .filter(|t| {
                                            t.as_ref()
                                                .and_then(|t| t.span.name.as_deref())
                                                .is_some()
                                        })
                                        .count();
                                    let ellipsis = if total_names > 5 { ", ..." } else { "" };
                                    format!(
                                        "\n    level {scope_level} (scope {env_id:?}): [{}{}]",
                                        preview.join(", "),
                                        ellipsis
                                    )
                                })
                                .collect();
                            drop(arena);
                            return Err(EvalError::internal(
                                format!("'{name}' — resolver level {level} out of range for scope chain depth {scope_depth} (ran out of parents at hop {depth_reached}){scope_summary}"),
                                span.clone(),
                            )
                            .into());
                        }
                    }
                };
                match thunk {
                    Some(t) => Ok(t),
                    None => Err(EvalError::undefined_variable(
                        format!("'{name}' at level={level} slot={slot}"),
                        span.clone(),
                    )
                    .into()),
                }
            }

            // Variant: first-class variant constructor emitted by lower.rs for type declarations.
            // Unit variants materialize directly; payload variants evaluate their inner expression,
            // materialize it, and store as a ThunkId — preserving the laziness invariant that
            // the payload dict's fields remain as thunks until accessed.
            CoreExpr::Variant { tag, payload } => {
                let (tycon, ctor) = tag.split_once('.').unwrap_or((tag.as_str(), ""));
                match payload {
                    None => Ok(Arc::new(Thunk::value(
                        Value::Variant {
                            tycon: tycon.to_string(),
                            ctor: ctor.to_string(),
                            payload: None,
                        },
                        span.clone(),
                    ))),
                    Some(inner_expr) => {
                        let payload_thunk = eval_core_expr(inner_expr, env_id, ctx).await?;
                        let payload_val = materialize(&payload_thunk, Some(&span), ctx).await?;
                        let payload_id =
                            ctx.alloc_thunk(0, Arc::new(Thunk::value(payload_val, span.clone())));
                        Ok(Arc::new(Thunk::value(
                            Value::Variant {
                                tycon: tycon.to_string(),
                                ctor: ctor.to_string(),
                                payload: Some(payload_id),
                            },
                            span.clone(),
                        )))
                    }
                }
            }

            CoreExpr::UnitVariant { tycon, ctor } => Ok(Arc::new(Thunk::value(
                Value::Variant {
                    tycon: tycon.clone(),
                    ctor: ctor.clone(),
                    payload: None,
                },
                span.clone(),
            ))),

            // Sequential: evaluate each expression in order, extending the environment
            // with dict bindings from each intermediate dict expression.
            // Sequential: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via SequentialStep continuations.
            // This eliminates async recursion on the Rust stack for deeply nested sequential blocks.
            CoreExpr::Sequential(_) => Ok(Arc::new(Thunk::core_expr(
                Arc::new(expr.clone()),
                env_id,
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Dict: call eval_dict_core directly with the CoreEntry slice.
            // eval_dict_core uses Thunk::core_expr for non-literal dict entries
            // (UnevaluatedState::CoreExpr), avoiding the per-entry core_expr_to_expr round-trip.
            CoreExpr::Dict(entries) => eval_dict_core(entries, env_id, ctx, &span).await,

            // Call: use eval_call_core — no CoreExpr→Expr round-trip for func or named args.
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
                    env_id,
                    ctx,
                    &span,
                    Arc::new(expr.clone()),
                )
                .await
            }

            // Fn: store body as Arc<Spanned<CoreExpr>> directly — no round-trip to Expr.
            CoreExpr::Fn {
                return_ann,
                params,
                body,
                ..
            } => {
                let fn_params: Vec<Param> = params
                    .iter()
                    .map(|p| Param {
                        name: p.node.name.clone(),
                        annotation: p.node.annotation.clone(),
                        variadic: p.node.variadic,
                        slot: p.node.slot,
                        resolved_type: p.node.resolved_type.clone(),
                    })
                    .collect();

                // Populate extra from annotation fields (literals + expressions).
                // `doc` is now included in extra: triple-quoted strings desugar to
                // `[unindent "..."]` (a Call), which is evaluated here at definition time.
                // T-1124: expression-valued fields are evaluated at function-definition time.
                let extra = extract_fn_annotation_extra(return_ann.as_ref(), env_id, ctx).await?;

                // Derive FnAnnotation.doc from extra["doc"] so triple-quoted doc strings
                // (evaluated via `[unindent "..."]`) produce the correct runtime string.
                let doc: Option<String> = extra
                    .get("doc")
                    .and_then(|v| v.as_str().map(|s| s.to_string()));
                let return_ann_clone: Option<crate::ast::Annotation> =
                    return_ann.as_ref().map(|a| a.node.clone());

                // Always construct FnAnnotation — source_span is always available even for
                // unannotated functions, enabling ast-of and LSP go-to-definition.
                let annotation = Some(Box::new(crate::value::FnAnnotation {
                    doc,
                    return_ann: return_ann_clone,
                    source_file: ctx.config.source_file.clone(),
                    source_span: span.clone(),
                    extra,
                }));

                // Store the body directly as Arc<Spanned<CoreExpr>>.
                // CoreExpr::Fn.body is already Arc<Spanned<CoreExpr>> — no conversion needed.
                // Closure captures env_id as the FlatEnv scope at definition time.
                Ok(Arc::new(Thunk::value(
                    Value::Function {
                        params: Rc::new(fn_params),
                        body: Arc::clone(body),
                        closure_env_id: env_id,
                        annotation,
                    },
                    span.clone(),
                )))
            }

            // TypeAssert: wrap as CoreExpr thunk — force_step's take_core_expr branch
            // handles CoreExpr::TypeAssert inline, pushing a TypeAssertCheck continuation.
            // Wrapping here prevents direct recursion back through eval_core_expr.
            CoreExpr::TypeAssert { .. } => Ok(Arc::new(Thunk::core_expr(
                Arc::new(expr.clone()),
                env_id,
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
                env_id,
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Quote: convert CoreExpr→SurfaceNode and walk with eval_quote_walk.
            // The inner CoreExpr was lowered (giving unquotes proper variable slots),
            // then converted back here for structural traversal. CoreExpr::Var preserves
            // the original name alongside the slot so the round-trip is lossless.
            CoreExpr::Quote(inner) => {
                let surface_node = crate::lower::core_expr_to_surface_node(inner);
                eval_quote_walk(surface_node, env_id, ctx).await
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
            // Returns a Dict so the SequentialStep can extract keys via its Dict-based binding logic.
            CoreExpr::LetDecl { bindings } => {
                let mut dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
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
                        env_id,
                        Arc::clone(ctx),
                        val_expr.span.clone(),
                    ));
                    let thunk_id = ctx.alloc_thunk(env_id, val_thunk);
                    dict.insert(HashableValue::Str(Rc::from(name.as_str())), thunk_id);
                    i += 2;
                }
                Ok(Arc::new(Thunk::value(Value::Dict(dict), span.clone())))
            }

            // CaseArm: error (not an expression)
            CoreExpr::CaseArm { .. } => Err(EvalError::internal(
                "case arms are not expressions".to_string(),
                span.clone(),
            )
            .into()),

            CoreExpr::Placeholder => Err(EvalError::unimplemented(
                "placeholder `...` was evaluated — replace with an implementation".to_string(),
                span.clone(),
            )
            .into()),
        }
        // Type guards are now inline on AST nodes (TypeAnnotation OnceLock);
        // the lowerer wraps them in CoreExpr::TypeAssert. No runtime guard wrapping needed here.
    }) // end Box::pin(async move {
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::ast::{CoreExpr, Spanned};
    use crate::eval::EvalContext;
    use crate::test_util::test_span;
    use crate::value::{string_val, Thunk, Value};

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, false)
    }

    async fn eval_and_materialize(
        expr: CoreExpr,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        let span = test_span(1, 1, 1, 5);
        let spanned = Spanned::new(expr, span.clone());
        let thunk = super::eval_core_expr(&spanned, 0, ctx).await?;
        crate::eval::materialize(&thunk, Some(&span), ctx).await
    }

    /// `CoreExpr::Int(42)` evaluates to `Value::Int(42)`.
    ///
    /// Int literals are on the fast path in eval_core_expr: they return a
    /// pre-materialized Thunk::value without going through the CEK machine.
    #[tokio::test]
    async fn test_eval_int_literal() {
        let ctx = test_ctx();
        let val = eval_and_materialize(CoreExpr::Int(42), &ctx).await.unwrap();
        assert_eq!(
            val,
            Value::Int(42),
            "CoreExpr::Int(42) must evaluate to Int(42)"
        );
    }

    /// `CoreExpr::Str("hello")` evaluates to the corresponding string Value.
    ///
    /// String literals are on the fast path: Thunk::value is returned directly.
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

    /// `CoreExpr::Var` resolves a slot from the FlatEnv arena.
    ///
    /// We build a child scope, reserve a named slot, fill it with a known thunk
    /// (sourced from the root scope), and then evaluate a Var node pointing at
    /// level=0, slot=0. The returned value must match the injected value.
    #[tokio::test]
    async fn test_eval_varref() {
        let span = test_span(1, 1, 1, 5);
        let ctx = test_ctx();

        // Step 1: store the known thunk in root scope (scope_id 0) as an anonymous slot.
        // This gives us a stable ThunkId to use as the source for fill_slot.
        let known_thunk = Arc::new(Thunk::value(Value::Int(77), span.clone()));
        let source_thunk_id = ctx.alloc_thunk(0, known_thunk);

        // Step 2: allocate a child scope with 0 pre-reserved slots, then
        // reserve and fill slot 0 manually (two-phase letrec protocol).
        let env_id = {
            let mut arena = ctx.scope_arena.borrow_mut();
            let root_id = crate::arena::ScopeId(0);
            let env_id = arena.alloc_child(root_id, 0);
            let slot_idx = arena.reserve_slot(env_id);
            assert_eq!(slot_idx, 0, "first reserved slot must be 0");
            arena.fill_slot(env_id, 0, source_thunk_id);
            env_id
        };

        // Build a Var node: level=0 (current scope), slot=0.
        let var_expr = Spanned::new(
            CoreExpr::Var {
                name: "test-binding".to_string(),
                level: 0,
                slot: 0,
                annotation: None,
            },
            span.clone(),
        );

        // Evaluate the Var in env_id — should return the injected thunk.
        let result_thunk = super::eval_core_expr(&var_expr, env_id.0, &ctx)
            .await
            .unwrap();
        let val = crate::eval::materialize(&result_thunk, Some(&span), &ctx)
            .await
            .unwrap();

        assert_eq!(
            val,
            Value::Int(77),
            "CoreExpr::Var at level=0 slot=0 must resolve to the injected Int(77)"
        );
    }

    /// B-526: OOM guard fires before CoreExpr dispatch and returns ResourceLimitExceeded.
    ///
    /// Sets the memory budget limit to 1 byte and trips the OOM flag, then calls
    /// eval_core_expr and verifies ResourceLimitExceeded is returned — not a panic
    /// or a successful evaluation.
    #[tokio::test]
    async fn test_eval_core_expr_oom_guard_fires() {
        use crate::error::ErrorKind;

        let span = test_span(1, 1, 1, 5);
        let ctx = test_ctx();

        // Trip the OOM flag: set limit to 1 byte, record any allocation.
        crate::memory_budget::set_limit(1);
        crate::memory_budget::record_and_check(2); // exceeds limit → sets OOM flag

        let expr = Spanned::new(CoreExpr::Int(42), span.clone());
        let result = super::eval_core_expr(&expr, 0, &ctx).await;

        // Reset global state so other tests are not affected.
        crate::memory_budget::reset_for_test();

        match result {
            Err(e) => assert!(
                matches!(e.kind, ErrorKind::ResourceLimitExceeded { .. }),
                "OOM guard must return ResourceLimitExceeded, got: {:?}",
                e.kind
            ),
            Ok(_) => panic!("expected ResourceLimitExceeded from OOM guard, got Ok"),
        }
    }
}
