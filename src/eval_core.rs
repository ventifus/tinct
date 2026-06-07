//! CoreExpr evaluation layer.
//!
//! This module handles evaluation of lowered CoreExpr nodes to thunks. CoreExpr is the
//! internal AST representation after name resolution and before CEK machine execution.
//!
//! Key functions:
//! - `eval_core_expr`: Main entry point — evaluates a CoreExpr node to a lazy thunk
//! - `maybe_wrap_guard`: Applies boundary type guards from the type checker
//! - `eval_quote_walk`: Handles quote/unquote evaluation for metaprogramming
//!
//! This module is called by:
//! - `eval_materialize.rs` (CEK machine force_step when taking thunk states)
//! - `eval_call.rs` (function evaluation)
//! - `builtins_async.rs` (eval builtin, macro transformers)

use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{CoreExpr, Param, Span, Spanned};
use crate::builtins::MAX_COLLECT_SIZE;
use crate::error::{EvalError, EvalResult};
use crate::eval::{eval_call_core, eval_dict_core, materialize, EvalContext};
use crate::value::{string_val, Environment, Key, Thunk, Value};

/// Wrap a thunk with a boundary guard if the span matches a guard in the context.
///
/// Boundary guards are populated by the type checker to enforce type constraints at
/// specific expression boundaries (e.g., function parameters, type assertions).
///
/// If `span` matches a guard in `ctx.boundary_guards`, wraps `thunk` in a `Guarded`
/// thunk that will check the type when forced. Otherwise returns `thunk` unchanged.
pub(crate) fn maybe_wrap_guard(
    thunk: Arc<Thunk>,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> Arc<Thunk> {
    // Skip guard lookup for synthetic origin spans. All synthetic CoreExpr nodes produced
    // by macro expansion or internal code synthesis share Span::origin() (offset 0, line 1,
    // col 1). If a boundary guard is keyed by Span::origin(), it would match every synthetic
    // node — applying the wrong type guard to unrelated expressions. Synthetic nodes are not
    // user-written expressions and should never carry boundary guards.
    if span.is_origin() {
        return thunk;
    }
    let guards = ctx.boundary_guards.read().unwrap();
    if let Some(expected_type) = guards.get(&span) {
        Arc::new(Thunk::new_guarded(
            thunk,
            expected_type.clone(),
            vec![], // empty field path for top-level guards
            span,
        ))
    } else {
        thunk
    }
}

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
    let make_node = |expr: SurfaceExpression| {
        Arc::new(SurfaceNode {
            expr,
            span: span.clone(),
        })
    };
    match value {
        Value::Int(n) => Ok(make_node(SurfaceExpression::Int(*n))),
        Value::Float(f) => Ok(make_node(SurfaceExpression::Float(*f))),
        Value::Bool(b) => Ok(make_node(SurfaceExpression::Bool(*b))),
        Value::String { source, start, end } => Ok(make_node(SurfaceExpression::Str(
            source[*start..*end].to_string(),
        ))),
        Value::Variant { .. } => {
            // Variant form of an AST node — convert via surface bridge
            crate::surface_convert::dict_to_surface_node(value, ctx).map_err(|err| {
                EvalError::internal(
                    format!("unquote result Variant is not a valid AST: {}", err),
                    span,
                )
                .into()
            })
        }
        Value::Dict(dict) => {
            // Check if this is an AST dict (has a "type" field)
            if dict.contains_key(&Key::String("type".into())) {
                // It's an AST dict — convert via surface bridge
                crate::surface_convert::dict_to_surface_node(value, ctx).map_err(|err| {
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
        Value::Expression(node) => {
            // Value::Expression — already a SurfaceNode, use it directly (no round-trip needed)
            Ok(Arc::clone(node))
        }
        _ => Err(
            EvalError::internal(format!("unquote of {:?} is not supported", value), span).into(),
        ),
    }
}

/// Collect all elements from a sequence value into a Vec.
/// Returns an error if the value is not a sequence.
async fn collect_seq_elements(
    value: &Value,
    span: Span,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Vec<Value>> {
    let mut elements = Vec::new();
    let mut current = value.clone();

    loop {
        match current {
            Value::Variant {
                ref tag,
                payload: None,
            } if tag == "Seq.Nil" => {
                // Empty sequence — we're done
                break;
            }
            Value::Variant {
                ref tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                // Extract head and tail from Seq.Cons payload
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = materialize(&payload_thunk, Some(&span), ctx).await?;
                let (head, tail) = if let Value::Dict(ref d) = payload_val {
                    let head = *d
                        .get(&Key::String("head".into()))
                        .expect("Seq.Cons must have head");
                    let tail = *d
                        .get(&Key::String("tail".into()))
                        .expect("Seq.Cons must have tail");
                    (head, tail)
                } else {
                    return Err(EvalError::internal(
                        "Seq.Cons payload must be a Dict".to_string(),
                        span,
                    )
                    .into());
                };

                // Materialize the head element
                let head_thunk = ctx.get_thunk(head);
                let head_value = materialize(&head_thunk, Some(&span), ctx).await?;
                elements.push(head_value);

                // Enforce size limit to prevent infinite sequences from looping forever
                if elements.len() >= MAX_COLLECT_SIZE {
                    return Err(EvalError::resource_limit_exceeded(
                        format!(
                            "unquote-splice: too many elements (limit {})",
                            MAX_COLLECT_SIZE
                        ),
                        span,
                    )
                    .into());
                }

                // Materialize and move to the tail
                let tail_thunk = ctx.get_thunk(tail);
                current = materialize(&tail_thunk, Some(&span), ctx).await?;
            }
            _ => {
                return Err(EvalError::type_mismatch("Seq", current.type_name(), span).into());
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
    env: &'a Arc<RwLock<Environment>>,
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
        let make_node = |expr: SurfaceExpression| {
            Arc::new(SurfaceNode {
                expr,
                span: span.clone(),
            })
        };

        match &node.expr {
            SurfaceExpression::Unquote(inner) => {
                // Evaluate the unquoted expression and convert back to SurfaceNode
                let core = crate::lower::lower(
                    inner,
                    crate::ast::empty_resolution_table(),
                    crate::ast::empty_type_annotation_table(),
                );
                let thunk = eval_core_expr(&core, env, ctx).await?;
                let value = materialize(&thunk, Some(&inner.span), ctx).await?;
                value_to_surface_node(&value, inner.span.clone(), ctx)
            }

            SurfaceExpression::UnquoteSplice(_) => {
                // UnquoteSplice at non-list position is an error.
                // Call args handle UnquoteSplice in their own loop below.
                Err(EvalError::unimplemented(
                    "unquote-splice must be in a list position (inside call args); dict entry splicing is not yet implemented"
                        .to_string(),
                    span,
                )
                .into())
            }

            // Recursively process composite expressions
            SurfaceExpression::Dict(entries) => {
                let mut processed_entries = Vec::with_capacity(entries.len());
                for entry in entries {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&entry.node.value), env, ctx).await?;
                    let processed_key = if let Some(ref key_node) = entry.node.key {
                        Some(eval_quote_preprocess(Arc::clone(key_node), env, ctx).await?)
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
                Ok(make_node(SurfaceExpression::Dict(processed_entries)))
            }

            SurfaceExpression::Call {
                func,
                args,
                named_args,
                implied,
            } => {
                let processed_func = eval_quote_preprocess(Arc::clone(func), env, ctx).await?;
                let mut processed_args: Vec<Arc<SurfaceNode>> = Vec::new();
                for arg in args {
                    // Handle unquote-splicing in call argument position
                    if let SurfaceExpression::UnquoteSplice(inner) = &arg.expr {
                        // Evaluate the unquote-splice expression
                        let core = crate::lower::lower(
                            inner,
                            crate::ast::empty_resolution_table(),
                            crate::ast::empty_type_annotation_table(),
                        );
                        let thunk = eval_core_expr(&core, env, ctx).await?;
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
                            .push(eval_quote_preprocess(Arc::clone(arg), env, ctx).await?);
                    }
                }
                let mut processed_named_args: Vec<Spanned<SurfaceNamedArg>> =
                    Vec::with_capacity(named_args.len());
                for na in named_args {
                    let processed_value =
                        eval_quote_preprocess(Arc::clone(&na.node.value), env, ctx).await?;
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
                let processed_body = eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Fn {
                    return_ann: return_ann.clone(),
                    params: params.clone(),
                    body: processed_body,
                    desugared: *desugared,
                }))
            }

            SurfaceExpression::DotAccess {
                expr: target,
                field,
            } => {
                let processed_target = eval_quote_preprocess(Arc::clone(target), env, ctx).await?;
                Ok(make_node(SurfaceExpression::DotAccess {
                    expr: processed_target,
                    field: field.clone(),
                }))
            }

            SurfaceExpression::Pipe { lhs, rhs } => {
                let processed_lhs = eval_quote_preprocess(Arc::clone(lhs), env, ctx).await?;
                let processed_rhs = eval_quote_preprocess(Arc::clone(rhs), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Pipe {
                    lhs: processed_lhs,
                    rhs: processed_rhs,
                }))
            }

            SurfaceExpression::Sequential(exprs) => {
                let mut processed_exprs = Vec::with_capacity(exprs.len());
                for e in exprs {
                    processed_exprs.push(eval_quote_preprocess(Arc::clone(e), env, ctx).await?);
                }
                Ok(make_node(SurfaceExpression::Sequential(processed_exprs)))
            }

            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
            } => {
                let processed_expr = eval_quote_preprocess(Arc::clone(inner), env, ctx).await?;
                Ok(make_node(SurfaceExpression::TypeAssert {
                    annotation: annotation.clone(),
                    expr: processed_expr,
                }))
            }

            SurfaceExpression::Quote(inner) => {
                // Nested quote: recurse so inner unquotes are still processed.
                let processed_inner = eval_quote_preprocess(Arc::clone(inner), env, ctx).await?;
                Ok(make_node(SurfaceExpression::Quote(processed_inner)))
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                let processed_scrutinee =
                    eval_quote_preprocess(Arc::clone(scrutinee), env, ctx).await?;
                let mut processed_arms = Vec::with_capacity(arms.len());
                for arm in arms {
                    let processed_body =
                        eval_quote_preprocess(Arc::clone(&arm.body), env, ctx).await?;
                    let processed_guard = if let Some(ref guard) = arm.guard {
                        Some(eval_quote_preprocess(Arc::clone(guard), env, ctx).await?)
                    } else {
                        None
                    };
                    processed_arms.push(SurfaceMatchArm {
                        pattern: arm.pattern.clone(),
                        guard: processed_guard,
                        body: processed_body,
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
                            eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                        SurfaceDeclaration::TypeAlias {
                            params: params.clone(),
                            body: processed_body,
                        }
                    }
                    SurfaceDeclaration::MacroDecl { name, params, body } => {
                        let processed_params =
                            eval_quote_preprocess(Arc::clone(params), env, ctx).await?;
                        let processed_body =
                            eval_quote_preprocess(Arc::clone(body), env, ctx).await?;
                        SurfaceDeclaration::MacroDecl {
                            name: name.clone(),
                            params: processed_params,
                            body: processed_body,
                        }
                    }
                    SurfaceDeclaration::SyntaxClass {
                        name,
                        pattern,
                        message,
                    } => {
                        let processed_pattern =
                            eval_quote_preprocess(Arc::clone(pattern), env, ctx).await?;
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
                                .push(eval_quote_preprocess(Arc::clone(form), env, ctx).await?);
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
    env: Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let span = node.span.clone();
    // Preprocess to handle nested unquotes (rewrites unquote subexpressions)
    let processed_node = eval_quote_preprocess(node, &env, ctx).await?;

    // runtime-v2 Part G: return Value::Expression (was: ast_to_dict_expr returning Variant Dict)
    // Macro transformer code in prelude.llt is dual-dispatch ready (tag-of handles both Expression and Variant).
    Ok(Arc::new(Thunk::new_materialized(
        Value::Expression(processed_node),
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
    env: &Arc<RwLock<Environment>>,
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
        let crate::ast::SurfaceExpression::Str(ref key_str) = key_node.expr else {
            continue;
        };

        // Skip standard annotation keys processed by the type system
        if crate::ast::STANDARD_ANN_KEYS.contains(&key_str.as_str()) {
            continue;
        }

        // Evaluate the annotation value: literals fast-path, expressions via eval
        let val = match &e.node.value.expr {
            // Fast path: literals extract directly without evaluation
            crate::ast::SurfaceExpression::Str(s) => string_val(s),
            crate::ast::SurfaceExpression::Int(n) => Value::Int(*n),
            crate::ast::SurfaceExpression::Float(f) => Value::Float(*f),
            crate::ast::SurfaceExpression::Bool(b) => Value::Bool(*b),

            // Expression-valued fields: lower to CoreExpr, evaluate, materialize to Value.
            // This is the T-1124 fix: annotations like `as-type: [fn [let u] u]` are now evaluable.
            //
            // If evaluation fails (e.g., VarRef to a type-level name like `Int`, `Str`, `String`
            // that only exists in the type-stage env but not the runtime env), skip this entry
            // rather than propagating the error. This preserves backward compatibility with
            // annotations like `fn@[ok: Int  err: Str]` where `Int`/`Str` are type-level names.
            _ => {
                // Lower SurfaceNode → CoreExpr (using empty resolution/type tables since
                // annotation expressions are evaluated in the definition-site environment).
                let core_expr = crate::lower::lower(
                    &e.node.value,
                    crate::ast::empty_resolution_table(),
                    crate::ast::empty_type_annotation_table(),
                );

                // Evaluate the CoreExpr to a thunk; skip this annotation entry on failure.
                let thunk = match eval_core_expr(&core_expr, env, ctx).await {
                    Ok(t) => t,
                    Err(_) => continue,
                };

                // Materialize the thunk to a Value (annotation values are eager); skip on failure.
                match materialize(&thunk, Some(&e.node.value.span), ctx).await {
                    Ok(v) => v,
                    Err(_) => continue,
                }
            }
        };

        extra.insert(key_str.clone(), val);
    }

    Ok(extra)
}

/// Evaluate a CoreExpr to a thunk (transitional path for runtime-v2).
///
/// This is the new CoreExpr evaluation entry point. It handles:
/// - Primitive variants natively: Int, Float, Bool, Str (direct materialization)
/// - Variables natively: Var, FreeVar (environment lookup with de Bruijn coordinates)
/// - Complex variants via bridge: Dict, Call, Fn, Match, etc. convert back to Expr
///   and call existing helpers (eval_dict, eval_call, etc.)
///
/// This is intentionally TRANSITIONAL. The round-trips to Expr are ACCEPTED for this
/// sprint (E1). Future sprints (E2/E3) will implement native CoreExpr handlers for
/// Dict/Call/Fn to eliminate the bridge conversions.
pub(crate) fn eval_core_expr<'a>(
    expr: &'a Spanned<CoreExpr>,
    env: &'a Arc<RwLock<Environment>>,
    ctx: &'a Arc<EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>> + 'a>> {
    Box::pin(async move {
        let span = expr.span.clone();
        match &expr.node {
            // Fast path: literals materialize directly without wrapping in Unevaluated
            CoreExpr::Int(n) => Ok(Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                span.clone(),
            ))),
            CoreExpr::U64(n) => Ok(Arc::new(Thunk::new_materialized(
                Value::U64(*n),
                span.clone(),
            ))),
            CoreExpr::Float(f) => Ok(Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                span.clone(),
            ))),
            CoreExpr::Bool(b) => Ok(Arc::new(Thunk::new_materialized(
                Value::Bool(*b),
                span.clone(),
            ))),
            CoreExpr::Str(s) => Ok(Arc::new(Thunk::new_materialized(
                string_val(s),
                span.clone(),
            ))),

            // Variable lookup with de Bruijn coordinates (fast path)
            CoreExpr::Var { name, level, slot } => {
                let env_lock = env.read().unwrap();
                // Try slot-based lookup first (O(1) when level and slot are correct)
                // get_by_slot verifies the key at slot matches name; falls back to
                // name-based lookup if there's a mismatch (slot-shift bug).
                if let Some(thunk) = env_lock.get_by_slot(*level, *slot, name) {
                    Ok(thunk)
                } else {
                    // Fallback to name-based lookup (for stale slot references)
                    let name_owned = name.clone();
                    env_lock.get(name).ok_or_else(|| {
                        EvalError::undefined_variable(name_owned, span.clone()).into()
                    })
                }
            }

            // Free variable: name-based lookup only (no slot available)
            CoreExpr::FreeVar(name) => {
                // Special case: inferred [do] sentinel variable (e.g., `ℊꜱʏᴍ⧼do-infer⧽0`).
                // Generated by gensym in prelude.llt `do-desugar-inferred`. The type checker
                // resolves the sentinel to a concrete monad name (e.g., "result") and records
                // the mapping in ctx.do_infer_resolutions. At eval time, substitute the sentinel
                // with the resolved monad dict from the environment.
                if name.starts_with("ℊꜱʏᴍ⧼do-infer⧽") {
                    let monad_name = ctx
                        .do_infer_resolutions
                        .read()
                        .unwrap()
                        .get(name.as_str())
                        .cloned();
                    if let Some(monad_name) = monad_name {
                        let env_lock = env.read().unwrap();
                        return env_lock.get(&monad_name).ok_or_else(|| {
                            EvalError::undefined_variable(monad_name, span.clone()).into()
                        });
                    }
                }
                let name_owned = name.clone();
                let env_lock = env.read().unwrap();
                env_lock
                    .get(name)
                    .ok_or_else(|| EvalError::undefined_variable(name_owned, span.clone()).into())
            }

            // DotAccess: wrap as a CoreExpr thunk directly.
            //
            // force_step handles CoreExpr::DotAccess INLINE in both take_core_expr and
            // take_surface branches (eval_materialize.rs), so when run() forces this thunk
            // the take_core_expr inline handler fires and pushes Memoize + DotAccessForce
            // without re-entering eval_core_expr. No core_expr_to_expr round-trip needed.
            CoreExpr::DotAccess { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Sequential: evaluate each expression in order, extending the environment
            // with dict bindings from each intermediate dict expression.
            // Sequential: wrap as CoreExpr thunk — the CEK machine will handle iterative
            // evaluation via SequentialStep continuations.
            // This eliminates async recursion on the Rust stack for deeply nested sequential blocks.
            CoreExpr::Sequential(_) => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Dict: call eval_dict_core directly with the CoreEntry slice.
            // Eliminates the Vec<Spanned<Entry>> allocation and per-entry core_expr_to_expr
            // calls previously required by the round-trip through eval_dict.
            // eval_dict_core now uses Thunk::new_unevaluated_core for non-literal dict entries
            // (UnevaluatedState::CoreExpr), eliminating the per-entry core_expr_to_expr round-trip.
            CoreExpr::Dict(entries) => eval_dict_core(entries, env, ctx, &span).await,

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
                    env,
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
                    })
                    .collect();

                // Extract doc string from annotation if present.
                // Uses get_property("doc") which works directly on SurfaceEntry via SurfaceExpression::Str keys.
                let doc: Option<String> = return_ann.as_ref().and_then(|ann_spanned| {
                    ann_spanned.node.get_property("doc").and_then(|doc_node| {
                        if let crate::ast::SurfaceExpression::Str(s) = &doc_node.expr {
                            Some(s.clone())
                        } else {
                            None
                        }
                    })
                });
                let return_ann_clone: Option<crate::ast::Annotation> =
                    return_ann.as_ref().map(|a| a.node.clone());

                // Populate extra from non-standard annotation fields (literals + expressions).
                // T-1124: expression-valued fields are now evaluated at function-definition time.
                let extra = extract_fn_annotation_extra(return_ann.as_ref(), env, ctx).await?;

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
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Function {
                        params: Rc::new(fn_params),
                        body: Arc::clone(body),
                        env: Arc::clone(env),
                        annotation,
                    },
                    span.clone(),
                )))
            }

            // TypeAssert: wrap as CoreExpr thunk — force_step's take_core_expr branch
            // handles CoreExpr::TypeAssert inline, pushing a TypeAssertCheck continuation.
            // Wrapping here prevents direct recursion back through eval_core_expr.
            CoreExpr::TypeAssert { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Annotated: evaluate as bare string
            CoreExpr::Annotated { name, .. } => Ok(Arc::new(Thunk::new_materialized(
                string_val(name),
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
            CoreExpr::Match { .. } => Ok(Arc::new(Thunk::new_unevaluated_core(
                Arc::new(expr.clone()),
                Arc::clone(env),
                Arc::clone(ctx),
                span.clone(),
            ))),

            // Quote: convert CoreExpr→SurfaceNode and walk with eval_quote_walk.
            // The inner CoreExpr was lowered (giving unquotes proper variable slots),
            // then converted back here for structural traversal. CoreExpr::Var preserves
            // the original name alongside the slot so the round-trip is lossless.
            CoreExpr::Quote(inner) => {
                let surface_node = crate::lower::core_expr_to_surface_node(inner);
                eval_quote_walk(surface_node, env.clone(), ctx).await
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
            // Syntax: [let name value] → bindings = [FreeVar("name"), value_expr]
            // Pairs are (bindings[2i], bindings[2i+1]).
            // Returns a Dict so the SequentialStep can extract keys via its Dict-based binding logic.
            CoreExpr::LetDecl { bindings } => {
                let mut dict: IndexMap<Key, ThunkId> = IndexMap::new();
                let mut i = 0;
                while i + 1 < bindings.len() {
                    let name_expr = &bindings[i];
                    let val_expr = &bindings[i + 1];
                    let name = match &name_expr.node {
                        CoreExpr::FreeVar(n) => n.clone(),
                        CoreExpr::Var { name: n, .. } => n.clone(),
                        CoreExpr::Annotated { name: n, .. } => n.clone(),
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
                    let val_thunk = Arc::new(Thunk::new_unevaluated_core(
                        Arc::new(val_expr.clone()),
                        Arc::clone(env),
                        Arc::clone(ctx),
                        val_expr.span.clone(),
                    ));
                    let thunk_id = ctx.alloc_thunk(val_thunk);
                    dict.insert(Key::String(Rc::from(name.as_str())), thunk_id);
                    i += 2;
                }
                Ok(Arc::new(Thunk::new_materialized(
                    Value::Dict(dict),
                    span.clone(),
                )))
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

            // Error: propagate as internal error
            CoreExpr::Error(err_span) => Err(EvalError::internal(
                format!(
                    "syntax error at {}:{} (cannot evaluate error node)",
                    err_span.start.line, err_span.start.column
                ),
                span.clone(),
            )
            .into()),
        }
        .map(|thunk| maybe_wrap_guard(thunk, span, ctx))
    }) // end Box::pin(async move {
}
