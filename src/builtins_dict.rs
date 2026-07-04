//! Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv.
//!
//! These builtins operate on dictionary (Dict) values, providing primitive
//! operations for accessing and transforming key-value structures.
//!
//! **Dict primitives:**
//! - `keys`: Extract dict keys as an auto-indexed dict
//! - `length`: Count dict entries
//! - `merge`: Lazy overlay of two dicts (O(1), right overrides left)
//! - `append`: Insert value at next integer key
//! - `builtin-get`: Primitive dict key lookup
//!
//! **Dict single-step primitives (drive laziness from tinct side):**
//! - `builtin-dict-nth`: Get value at insertion-order position i, or Absent.Absent
//! - `builtin-dict-key-nth`: Get key at insertion-order position i, or Absent.Absent
//! - `builtin-dict-kv-nth`: Get {key,value} pair at position i, or Absent.Absent
//!
//! **Transient builders:**
//! - `make-builder`: Create an empty mutable builder
//! - `builder-set`: Set key-value pair, returns builder for chaining
//! - `builder-delete`: Remove key, returns builder for chaining
//! - `builder-finish`: Take inner dict, freeze builder
//! - `builder-snapshot`: Clone inner dict without freezing
//! - `builder-has?`: Check if key exists
//! - `builder-get`: Get value by key
//!
//! All three single-step primitives are O(1) (IndexMap::get_index).
//! Tinct wrappers in prelude.llt drive laziness via recursive thunks.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, HashableValue, Thunk, ThunkId, Value};

/// `keys`: Takes 1 arg (a Dict). Returns a Dict with integer keys `0..n`
/// mapping to the key values (Int keys become Int values, String keys become
/// String values). Insertion order is preserved.
/// Inherently materializing: must access IndexMap to enumerate keys.
pub(crate) fn builtin_keys(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("keys", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }
        // arg[0] is pre-forced by force_count.
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let map = crate::builtins::require_dict(
            "keys",
            val,
            args[0].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        let origin = call_span.clone();
        let mut result = IndexMap::with_capacity(map.len());
        for (i, (key, _)) in map.iter().enumerate() {
            let key_value = match key {
                HashableValue::Int(n) => Value::Int(*n),
                HashableValue::Str(s) => string_val(s),
                _ => unreachable!("dict keys are Int or Str"),
            };
            let thunk = Arc::new(Thunk::new_materialized(key_value, origin.clone()));
            let thunk_id = ctx.alloc_thunk(thunk);
            result.insert(
                HashableValue::Int(i64::try_from(i).map_err(|_| {
                    EvalError::internal("collection index overflow".to_string(), call_span.clone())
                })?),
                thunk_id,
            );
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `length`: Takes 1 arg (a Dict, String, or Bytes). Returns an Int with the number of entries/characters/bytes.
/// Dual-dispatch: Dict returns entry count, String returns character count, Bytes returns byte count.
/// Inherently materializing: must access IndexMap to count entries or count UTF-8 characters or bytes.
pub(crate) fn builtin_length(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("length", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        // arg[0] is pre-forced by force_count.
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::String { source, start, end } => {
                let s = &source[start..end];
                let len = s.chars().count();
                let len_i64 = i64::try_from(len).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "length: string length exceeds i64::MAX".to_string(),
                        call_span.clone(),
                    )
                })?;
                ok_val(Value::Int(len_i64), call_span)
            }
            Value::Bytes { start, end, .. } => {
                let len = end - start;
                let len_i64 = i64::try_from(len).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "length: byte length exceeds i64::MAX".to_string(),
                        call_span.clone(),
                    )
                })?;
                ok_val(Value::Int(len_i64), call_span)
            }
            _ => {
                let map = crate::builtins::require_dict(
                    "length",
                    val,
                    args[0].span.clone(),
                    &ctx,
                    call_span.clone(),
                )
                .await?;
                ok_val(Value::Int(map.len() as i64), call_span)
            }
        }
    })
}

/// `merge`: Takes 2 args (both Dicts). Returns a lazy `Value::Overlay(L, R)` — R
/// overrides L on key collision. Construction is O(1): neither L nor R is
/// materialized at merge time. Flattening to an IndexMap is deferred until the
/// overlay is actually accessed (via `require_dict`, `visit_value`, etc.).
///
/// Type validation (both args must be Dicts) is also deferred to flatten time,
/// which means type errors surface at access time rather than at call time.
/// This is the expected behavior for a lazy overlay.
pub(crate) fn builtin_merge(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("merge", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // O(1): store thunk pointers without forcing either side.
        let left_id = ctx.alloc_thunk(Arc::clone(&args[0]));
        let right_id = ctx.alloc_thunk(Arc::clone(&args[1]));
        Ok(Arc::new(Thunk::new_materialized(
            Value::Overlay(left_id, right_id),
            call_span,
        )))
    })
}

/// `append`: Takes 2 args: any value and a Dict. Returns a new dict with the
/// value inserted at the next integer key (one past the current maximum integer
/// key, or 0 for empty dicts / dicts with no integer keys).
///
/// This is O(n) for the clone but O(1) amortized for the insert itself,
/// compared to the old LLT `append` which did a full `merge` (copying the
/// entire accumulator into a new dict via two-dict iteration).
pub(crate) fn builtin_append(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("append", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // arg[0] (the value to append) is NOT materialized — it is inserted as a thunk
        // (Arc::clone at line below), preserving laziness of the appended value.
        // arg[1] (dict) is pre-forced by W1 pos_strictness[1]=Seq scan.
        let dict_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq via W1 scan");
        let mut map = crate::builtins::require_dict(
            "append",
            dict_val,
            args[1].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        // Compute the next integer key: max existing int key + 1, or 0 if none.
        let next_key = map
            .keys()
            .filter_map(|k| match k {
                HashableValue::Int(n) => Some(*n),
                _ => None,
            })
            .max();

        #[allow(clippy::result_large_err)] // EvalError size is acceptable for error path
        let next_idx = match next_key {
            Some(max) => max.checked_add(1).ok_or_else(|| {
                EvalError::integer_overflow("append".to_string(), call_span.clone())
            })?,
            None => 0,
        };

        let value_id = ctx.alloc_thunk(Arc::clone(&args[0]));
        map.insert(HashableValue::Int(next_idx), value_id);
        ok_val(Value::Dict(map), call_span)
    })
}

/// `field-get`: Dot-access key lookup — the desugared form of `target.field`.
///
/// Takes 2 args: key (String or Int) and target (Dict, Proxy, Variant, Program, Document).
/// Returns the value at `key` in `target`, following the same rules as dot-access:
/// - Dict / Overlay: look up by HashableValue key
/// - Proxy: invoke the proxy handler with the key string
/// - Variant: auto-unpack the payload and retry
/// - Program / Document: field dispatch to well-known field names
/// - Environment: TYPE ERROR — should have been compiled to `slot-get` by the type checker
///
/// Registered at ROOT SCOPE SLOT 0 in `core_builtins()`. The lowerer hardcodes this slot.
pub(crate) fn builtin_field_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("field-get", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // arg[0]: key (String or Int) — pre-materialized by Strictness::Seq
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "field-get".to_string(),
                    "Int or String",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // arg[1]: target — pre-materialized by Strictness::Seq
        let target_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let target_span = args[1].span.clone();

        field_get_on_value(key, target_val, target_span, call_span, None, &ctx).await
    })
}

/// Inner recursive helper for `field-get` and Variant auto-unpack.
///
/// `variant_tag`: when accessing a Variant payload, carry the tag for TyConDef constant fallback.
async fn field_get_on_value(
    key: HashableValue,
    target_val: Value,
    target_span: crate::ast::Span,
    call_span: crate::ast::Span,
    variant_tag: Option<String>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let key_str = match &key {
        HashableValue::Int(n) => n.to_string(),
        HashableValue::Str(s) => s.to_string(),
        _ => "<other>".to_string(),
    };

    // Flatten Overlay to Dict before key lookup.
    let target_val = match target_val {
        Value::Overlay(l, r) => Value::Dict(
            crate::builtins::flatten_overlay(
                &l,
                &r,
                &format!(".{key_str}"),
                ctx,
                call_span.clone(),
            )
            .await?,
        ),
        other => other,
    };

    match target_val {
        Value::Dict(map) => {
            let thunk_id_opt = map.get(&key);
            match thunk_id_opt {
                Some(thunk_id) => {
                    let thunk = ctx.get_thunk(*thunk_id);
                    Ok(thunk)
                }
                None => {
                    // Key not found: check TyConDef constructor constants first
                    // when this access came through a variant payload (T-1358).
                    if let (HashableValue::Str(ref field_name), Some(ref tag)) =
                        (&key, &variant_tag)
                    {
                        if let Some(type_name) = tag.split('.').next() {
                            if let Some(tycon_env) = ctx.tycon_env.get() {
                                if let Some(def) = tycon_env.get(type_name) {
                                    if let Some(constants) =
                                        def.constructor_constants.get(tag.as_str())
                                    {
                                        if let Some(const_val) =
                                            constants.get(field_name.as_ref() as &str)
                                        {
                                            let thunk = Arc::new(Thunk::new_materialized(
                                                const_val.clone(),
                                                call_span.clone(),
                                            ));
                                            return Ok(thunk);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    // No constant found — report key-not-found error.
                    let available_keys: Vec<String> = map.keys().map(|k| k.to_string()).collect();
                    Err(EvalError::key_not_found(&key_str, available_keys, target_span).into())
                }
            }
        }
        Value::Proxy { handler } => {
            // Proxy handler invocation: call handler with the key string.
            let handler_thunk = ctx.get_thunk(handler);
            crate::eval_access::invoke_proxy_handler(
                &handler_thunk,
                string_val(&key_str),
                ctx,
                &call_span,
            )
            .await
        }
        Value::Variant { tag, payload } => {
            // Variant auto-unpacking: dot-access on a variant accesses the payload.
            match payload {
                Some(payload_id) => {
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_span = payload_thunk.span.clone();
                    let payload_val = materialize(&payload_thunk, Some(&call_span), ctx).await?;
                    // Recurse with variant_tag set so TyConDef constants can be found.
                    Box::pin(field_get_on_value(
                        key,
                        payload_val,
                        payload_span,
                        call_span,
                        Some(tag),
                        ctx,
                    ))
                    .await
                }
                None => {
                    // Unit variant: try TyConDef constructor constants first (T-1358).
                    if let HashableValue::Str(ref field_name) = key {
                        if let Some(type_name) = tag.split('.').next() {
                            if let Some(tycon_env) = ctx.tycon_env.get() {
                                if let Some(def) = tycon_env.get(type_name) {
                                    if let Some(constants) =
                                        def.constructor_constants.get(tag.as_str())
                                    {
                                        if let Some(const_val) =
                                            constants.get(field_name.as_ref() as &str)
                                        {
                                            let thunk = Arc::new(Thunk::new_materialized(
                                                const_val.clone(),
                                                call_span.clone(),
                                            ));
                                            return Ok(thunk);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(EvalError::internal(
                        format!("cannot access field .{key_str} on unit variant (no payload)"),
                        target_span,
                    )
                    .into())
                }
            }
        }
        Value::Program {
            program: prog,
            warnings,
            ..
        } => {
            let val = match key_str.as_str() {
                "documents" => {
                    let mut dict = indexmap::IndexMap::new();
                    for (i, doc_spanned) in prog.documents.iter().enumerate() {
                        let id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            Value::Document(Arc::new(doc_spanned.node.clone())),
                            call_span.clone(),
                        )));
                        dict.insert(HashableValue::Int(i as i64), id);
                    }
                    Value::Dict(dict)
                }
                "warnings" => {
                    let mut list = indexmap::IndexMap::new();
                    for (i, err) in warnings.iter().enumerate() {
                        let span = err.span();
                        let alloc = |v: Value| {
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())))
                        };
                        let mut w = indexmap::IndexMap::new();
                        w.insert(
                            HashableValue::Str("kind".into()),
                            alloc(string_val(err.kind_name())),
                        );
                        w.insert(
                            HashableValue::Str("message".into()),
                            alloc(string_val(&err.message())),
                        );
                        let span_id =
                            crate::eval_materialize::make_span_dict(span, ctx, &call_span);
                        w.insert(HashableValue::Str("span".into()), span_id);
                        let notes_val = {
                            let notes = err.notes();
                            if notes.is_empty() {
                                Value::Dict(indexmap::IndexMap::new())
                            } else {
                                let mut nd = indexmap::IndexMap::new();
                                for (ni, note) in notes.iter().enumerate() {
                                    nd.insert(
                                        HashableValue::Int(ni as i64),
                                        alloc(string_val(note)),
                                    );
                                }
                                Value::Dict(nd)
                            }
                        };
                        w.insert(HashableValue::Str("notes".into()), alloc(notes_val));
                        let call_stack_val = {
                            let frames = err.call_stack();
                            if frames.is_empty() {
                                Value::Dict(indexmap::IndexMap::new())
                            } else {
                                let mut cd = indexmap::IndexMap::new();
                                for (fi, frame) in frames.iter().enumerate() {
                                    let frame_span_id = crate::eval_materialize::make_span_dict(
                                        &frame.span,
                                        ctx,
                                        &call_span,
                                    );
                                    let mut fd = indexmap::IndexMap::new();
                                    fd.insert(
                                        HashableValue::Str("label".into()),
                                        alloc(string_val(&frame.label)),
                                    );
                                    fd.insert(HashableValue::Str("span".into()), frame_span_id);
                                    let frame_id = alloc(Value::Dict(fd));
                                    cd.insert(HashableValue::Int(fi as i64), frame_id);
                                }
                                Value::Dict(cd)
                            }
                        };
                        w.insert(
                            HashableValue::Str("call-stack".into()),
                            alloc(call_stack_val),
                        );
                        w.insert(
                            HashableValue::Str("macro-expand".into()),
                            alloc(Value::Dict(indexmap::IndexMap::new())),
                        );
                        w.insert(
                            HashableValue::Str("blame".into()),
                            alloc(Value::Dict(indexmap::IndexMap::new())),
                        );
                        let entry = alloc(Value::Dict(w));
                        list.insert(HashableValue::Int(i as i64), entry);
                    }
                    Value::Dict(list)
                }
                _ => Value::Dict(indexmap::IndexMap::new()),
            };
            Ok(Arc::new(Thunk::new_materialized(val, call_span)))
        }
        Value::Document(doc) => {
            let val = match key_str.as_str() {
                "expressions" => {
                    let mut dict = indexmap::IndexMap::new();
                    let mut i = 0usize;
                    for item in &doc.items {
                        if let crate::ast::SurfaceItem::Expr(node) = item {
                            let expr_val =
                                crate::surface_convert::surface_node_to_expr_variant(node, ctx);
                            let id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                expr_val,
                                call_span.clone(),
                            )));
                            dict.insert(HashableValue::Int(i as i64), id);
                            i += 1;
                        }
                    }
                    Value::Dict(dict)
                }
                "name" => match &doc.name {
                    Some(n) => Value::Variant {
                        tag: "Named".into(),
                        payload: Some(ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                            crate::value::string_val(n),
                            call_span.clone(),
                        )))),
                    },
                    None => Value::Variant {
                        tag: "Unnamed".into(),
                        payload: None,
                    },
                },
                "stage" => {
                    let stage_tag = match &doc.stage {
                        Some(crate::ast::Stage::Type) => "DocStage.Type",
                        Some(crate::ast::Stage::Runtime) | None => "DocStage.Runtime",
                    };
                    Value::Variant {
                        tag: stage_tag.to_string(),
                        payload: None,
                    }
                }
                "uses" => match &doc.uses {
                    None => Value::Dict(indexmap::IndexMap::new()),
                    Some(crate::ast::Spanned { node: _, .. })
                        if doc.uses.as_ref().map_or(true, |u| u.node.is_empty()) =>
                    {
                        Value::Dict(indexmap::IndexMap::new())
                    }
                    Some(crate::ast::Spanned { node: modules, .. }) => {
                        let mut dict = indexmap::IndexMap::new();
                        for (i, module_spanned) in modules.iter().enumerate() {
                            let id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                crate::value::string_val(&module_spanned.node),
                                call_span.clone(),
                            )));
                            dict.insert(HashableValue::Int(i as i64), id);
                        }
                        Value::Dict(dict)
                    }
                },
                "expects" => match &doc.expects {
                    None => Value::Dict(indexmap::IndexMap::new()),
                    Some(spanned_ann) => {
                        let inner = Arc::new(crate::ast::SurfaceNode::new(
                            crate::ast::SurfaceExpression::VarRef {
                                name: "%".to_string(),
                                escaped: false,
                                resolution: crate::ast::Resolution::new(),
                                call_dispatch: crate::ast::CallDispatch::new(),
                                annotation: None,
                            },
                            spanned_ann.span.clone(),
                        ));
                        let type_assert_node = Arc::new(crate::ast::SurfaceNode::new(
                            crate::ast::SurfaceExpression::TypeAssert {
                                annotation: spanned_ann.clone(),
                                expr: inner,
                                resolved_type: crate::ast::TypeAnnotation::new(),
                            },
                            spanned_ann.span.clone(),
                        ));
                        crate::surface_convert::surface_node_to_expr_variant(&type_assert_node, ctx)
                    }
                },
                _ => Value::Dict(indexmap::IndexMap::new()),
            };
            Ok(Arc::new(Thunk::new_materialized(val, call_span)))
        }
        Value::Environment(env_arc) => {
            // Own-frame slot_names scan — does NOT walk the parent chain.
            // This makes result-env.% and math.hypot work: the value is in the
            // environment's own bindings (slot 0 for %, or position N for exports).
            // The type-driven solution (T-1490) will replace this with slot-get once
            // the type checker annotates field_slot from return-type information.
            if let HashableValue::Str(ref field_name) = key {
                let env_read = env_arc.read().unwrap();
                match env_read
                    .slot_names
                    .iter()
                    .position(|n| n == field_name.as_ref())
                {
                    Some(pos) => Ok(Arc::clone(&env_read.slots[pos])),
                    None => {
                        Err(EvalError::key_not_found(field_name.as_ref(), vec![], target_span)
                            .into())
                    }
                }
            } else {
                Err(EvalError::type_mismatch_ctx(
                    "field-get".to_string(),
                    "String key for Environment dot access",
                    "Int",
                    target_span,
                )
                .into())
            }
        }
        other => Err(EvalError::type_mismatch_ctx(
            "field-get".to_string(),
            "Dict, Proxy, Variant, Program, or Document",
            other.type_name(),
            target_span,
        )
        .into()),
    }
}

/// `slot-get`: Positional slot access — the desugared form of typed `target.field`.
///
/// Takes 2 args: slot (Int) and target (Dict or Environment).
/// Returns the value at position `slot` in `target`:
/// - Dict: O(1) positional lookup via `get_index`
/// - Environment: direct slot lookup into the slots Vec
///
/// Registered at ROOT SCOPE SLOT 1 in `core_builtins()`. The lowerer hardcodes this slot.
pub(crate) fn builtin_slot_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("slot-get", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // arg[0]: slot (Int) — pre-materialized by Strictness::Seq
        let slot_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let slot = match slot_val {
            Value::Int(n) if n >= 0 => n as usize,
            Value::Int(n) => {
                return Err(EvalError::internal(
                    format!("slot-get: negative slot index {n}"),
                    call_span,
                )
                .into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "slot-get".to_string(),
                    "Int",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // arg[1]: target — pre-materialized by Strictness::Seq
        let target_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let target_span = args[1].span.clone();

        match target_val {
            Value::Dict(map) => match map.get_index(slot) {
                Some((_, &thunk_id)) => {
                    let thunk = ctx.get_thunk(thunk_id);
                    Ok(thunk)
                }
                None => Err(EvalError::internal(
                    format!(
                        "slot-get: slot {slot} out of bounds (dict has {} entries)",
                        map.len()
                    ),
                    target_span,
                )
                .into()),
            },
            Value::Environment(env_arc) => {
                let env = env_arc.read().unwrap();
                match env.slots.get(slot) {
                    Some(thunk) => Ok(Arc::clone(thunk)),
                    None => Err(EvalError::internal(
                        format!(
                            "slot-get: slot {slot} out of bounds (env has {} slots)",
                            env.slots.len()
                        ),
                        target_span,
                    )
                    .into()),
                }
            }
            other => Err(EvalError::type_mismatch_ctx(
                "slot-get".to_string(),
                "Dict or Environment",
                other.type_name(),
                target_span,
            )
            .into()),
        }
    })
}

/// `builtin-get`: Rust primitive for dict key lookup.
///
/// Takes 2 args: a key (Int or String) and a dict.
/// Returns the value at that key, or errors if the key is not found.
///
/// This is a thin primitive that `get` (in prelude.llt) wraps, following the
/// same pattern as `builtin-reduce` → `reduce` and `builtin-fold` → `fold`.
pub(crate) fn builtin_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-get", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // Materialize the key
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-get".to_string(),
                    "Int or String",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Materialize the dict (spine only, not values)
        let dict_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        // Include the key in the context string so the error message identifies WHICH
        // [get ...] call received the wrong type. This makes macro-expansion bugs diagnosable.
        let key_display = match &key {
            HashableValue::Int(n) => format!("key {n}"),
            HashableValue::Str(s) => format!("key \"{s}\""),
            _ => "key <other>".to_string(),
        };
        let context = format!("builtin-get ({key_display})");
        let map = crate::builtins::require_dict(
            &context,
            dict_val,
            args[1].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        // Look up the key
        match map.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
                Ok(thunk)
            }
            None => {
                let key_str = match &key {
                    HashableValue::Int(n) => n.to_string(),
                    HashableValue::Str(s) => s.to_string(),
                    _ => "<other>".to_string(),
                };
                let available_keys = map
                    .keys()
                    .map(|k| match k {
                        HashableValue::Int(n) => n.to_string(),
                        HashableValue::Str(s) => s.to_string(),
                        _ => "<other>".to_string(),
                    })
                    .collect();
                Err(EvalError::key_not_found(&key_str, available_keys, call_span).into())
            }
        }
    })
}

/// `get?`: Rust primitive for optional dict key lookup.
///
/// Takes 2 args: a key (Int or String) and a dict.
/// Returns the value if the key exists, or `Absent.Absent` if missing.
/// NO error on missing key (unlike `builtin-get` which errors).
pub(crate) fn builtin_get_optional(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("get?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // Materialize the key
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "get?".to_string(),
                    "Int or String",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Materialize the dict (spine only, not values)
        let dict_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let map = crate::builtins::require_dict(
            "get?",
            dict_val,
            args[1].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        // Look up the key
        match map.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
                Ok(thunk)
            }
            None => {
                // Return Absent.Absent on missing key
                ok_val(
                    Value::Variant {
                        tag: "Absent.Absent".into(),
                        payload: None,
                    },
                    call_span,
                )
            }
        }
    })
}

/// `builtin-dict-nth`: Get the value at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns the value at that position, or `Absent.Absent`
/// if `i` is out of bounds. O(1) — drives laziness from the tinct side.
/// `Dict a → Int → a | Absent`
pub(crate) fn builtin_dict_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("dict-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = crate::builtins::require_dict(
            "dict-nth",
            dict_val,
            args[0].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq")
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((_, val_id)) => Ok(ctx.get_thunk(*val_id)),
            None => ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            ),
        }
    })
}

/// `builtin-dict-key-nth`: Get the key at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns the key at that position, or `Absent.Absent`
/// if `i` is out of bounds. O(1) — drives laziness from the tinct side.
/// `Dict a → Int → Key | Absent`
pub(crate) fn builtin_dict_key_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("dict-key-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = crate::builtins::require_dict(
            "dict-key-nth",
            dict_val,
            args[0].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq")
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((key, _)) => {
                let key_val = match key {
                    HashableValue::Int(n) => Value::Int(*n),
                    HashableValue::Str(s) => string_val(s),
                    _ => unreachable!("dict keys are Int or Str"),
                };
                ok_val(key_val, call_span)
            }
            None => ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            ),
        }
    })
}

/// `builtin-dict-kv-nth`: Get the key-value pair at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns a `[key: K  value: V]` dict at that position,
/// or `Absent.Absent` if `i` is out of bounds. O(1) — drives laziness from the tinct side.
/// `Dict a → Int → [key: Key  value: a] | Absent`
pub(crate) fn builtin_dict_kv_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("dict-kv-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = crate::builtins::require_dict(
            "dict-kv-nth",
            dict_val,
            args[0].span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq")
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((key, val_id)) => {
                let key_val = match key {
                    HashableValue::Int(n) => Value::Int(*n),
                    HashableValue::Str(s) => string_val(s),
                    _ => unreachable!("dict keys are Int or Str"),
                };
                let mut kv = IndexMap::new();
                kv.insert(
                    HashableValue::Str("key".into()),
                    ctx.alloc_thunk(ok_val(key_val, call_span.clone())?),
                );
                kv.insert(HashableValue::Str("value".into()), *val_id);
                ok_val(Value::Dict(kv), call_span)
            }
            None => ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            ),
        }
    })
}

/// `builder-get-or`: Atomically get-or-insert in a builder.
///
/// Takes 3 args: key (Int or String), default_value (any), builder.
/// If `key` exists, returns the existing value. Otherwise inserts `default_value` at `key`
/// and returns it. Single mutex acquisition — no race between has? and set.
/// Returns the builder for chaining (NOT the looked-up value — returns builder so callers
/// can chain further operations).
///
/// NOTE: Returns the looked-up/inserted ThunkId value (not the builder).
/// Usage pattern: `[builder-set k [cons x [builder-get-or k [] b]] b]`
pub(crate) fn builtin_builder_get_or(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get-or", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get-or".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // args[1] (default value) is NOT materialized — inserted as a thunk if key absent.
        let default_id = ctx.alloc_thunk(Arc::clone(&args[1]));

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let builder_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[2]=Seq via W1 scan");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get-or".to_string(),
                    "Builder",
                    other.type_name(),
                    args[2].span.clone(),
                )
                .into())
            }
        };

        // Atomic get-or-insert: single mutex acquisition
        let result_id = builder
            .get_or(key, default_id)
            .map_err(|_| EvalError::builder_already_finished("builder-get-or", call_span))?;

        // Return the existing or newly-inserted value thunk
        let thunk = ctx.thunk_arena.lock().unwrap().get(result_id).clone();
        Ok(thunk)
    })
}

/// `build-dict`: Efficiently construct a dict from a Seq or Dict of key-value pairs.
///
/// Takes 1 arg (a Seq or Dict where each element is a dict with `key` and `value` fields).
/// Returns a new flat Dict with those entries.
///
/// - **Seq input:** Each element should be `[key: K, value: V]` (like what `each-kv` returns).
///   Forces the key (to extract it), keeps value lazy. Builds an IndexMap. O(n).
/// - **Dict input:** Copies all entries into a new flat IndexMap (eliminates Overlay depth). O(n).
///
/// Pre-allocates the IndexMap when size is known.
///
/// This replaces O(n²) merge-accumulation in dict-building stdlib functions like
/// `from-entries`, `map-entries`, `walk`, `transpose`, `collect-kv`, etc.
pub(crate) fn builtin_build_dict(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("build-dict", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        // args[0] is pre-materialized by force_count=1.
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");

        match val {
            // Dict input: build from key-value pair dicts in insertion order.
            // Each entry value must be a dict with "key" and "value" fields.
            Value::Dict(ref map) => {
                let mut result = IndexMap::with_capacity(map.len());
                for (_idx, entry_id) in map {
                    let entry_thunk = ctx.get_thunk(*entry_id);
                    let entry_val = materialize(&entry_thunk, None, &ctx).await?;
                    let entry_map = crate::builtins::require_dict(
                        "build-dict entry",
                        entry_val,
                        entry_thunk.span.clone(),
                        &ctx,
                        call_span.clone(),
                    )
                    .await?;
                    let key_id = entry_map
                        .get(&HashableValue::Str("key".into()))
                        .ok_or_else(|| {
                            EvalError::key_not_found("key", vec![], call_span.clone())
                        })?;
                    let value_id = entry_map
                        .get(&HashableValue::Str("value".into()))
                        .ok_or_else(|| {
                            EvalError::key_not_found("value", vec![], call_span.clone())
                        })?;
                    let key_thunk = ctx.get_thunk(*key_id);
                    let key_val = materialize(&key_thunk, None, &ctx).await?;
                    let key = match key_val {
                        Value::Int(n) => HashableValue::Int(n),
                        Value::String {
                            ref source,
                            start,
                            end,
                        } => HashableValue::Str((&source[start..end]).into()),
                        other => {
                            return Err(EvalError::type_mismatch_ctx(
                                "build-dict".to_string(),
                                "Int or String (for key)",
                                other.type_name(),
                                key_thunk.span.clone(),
                            )
                            .into())
                        }
                    };
                    result.insert(key, *value_id);
                }
                ok_val(Value::Dict(result), call_span)
            }

            // Overlay input: flatten to a new IndexMap (eliminates Overlay depth)
            Value::Overlay(_left_id, _right_id) => {
                let map = crate::builtins::require_dict(
                    "build-dict",
                    val,
                    args[0].span.clone(),
                    &ctx,
                    call_span.clone(),
                )
                .await?;
                let mut result = IndexMap::with_capacity(map.len());
                for (key, thunk_id) in &map {
                    result.insert(key.clone(), *thunk_id);
                }
                ok_val(Value::Dict(result), call_span)
            }

            other => Err(EvalError::type_mismatch_ctx(
                "build-dict".to_string(),
                "Dict of [key: K  value: V] pairs",
                other.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `make-builder`: Create an empty transient builder for efficient mutable dict construction.
/// Takes 0 args. Returns a new Builder.
pub(crate) fn builtin_make_builder(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span.clone()).into());
        }
        // Optional named arg: capacity: <Int> — pre-allocates the inner IndexMap.
        // Any other named arg is rejected.
        let capacity: usize = if let Some(ref named_map) = named {
            let cap_thunk = named_map.get("capacity").cloned();
            // Reject unexpected named args (all except "capacity").
            let unexpected: IndexMap<String, Arc<Thunk>> = named_map
                .iter()
                .filter(|(k, _)| k.as_str() != "capacity")
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect();
            if !unexpected.is_empty() {
                reject_named("make-builder", Some(&unexpected), call_span.clone())?;
            }
            if let Some(cap_thunk) = cap_thunk {
                let cap_val = materialize(&cap_thunk, None, &ctx).await?;
                match cap_val {
                    Value::Int(n) if n >= 0 => n as usize,
                    Value::Int(n) => {
                        return Err(EvalError::type_mismatch_ctx(
                            "make-builder".to_string(),
                            "non-negative Int",
                            &format!("Int({})", n),
                            cap_thunk.span.clone(),
                        )
                        .into())
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "make-builder".to_string(),
                            "Int",
                            other.type_name(),
                            cap_thunk.span.clone(),
                        )
                        .into())
                    }
                }
            } else {
                0
            }
        } else {
            0
        };
        let builder = if capacity > 0 {
            crate::value::Builder::with_capacity(capacity)
        } else {
            crate::value::Builder::new()
        };
        ok_val(Value::Builder(Arc::new(builder)), call_span)
    })
}

/// `builder-set`: Set a key-value pair in a builder. Returns the builder for chaining.
/// Takes 3 args: key (Int or String), value (any), builder.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_set(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-set", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-set".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // args[1] (value) is NOT materialized — inserted as a thunk
        let value_id = ctx.alloc_thunk(Arc::clone(&args[1]));

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let builder_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[2]=Seq via W1 scan");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-set".to_string(),
                    "Builder",
                    other.type_name(),
                    args[2].span.clone(),
                )
                .into())
            }
        };

        // Set the key-value pair
        builder
            .set(key, value_id)
            .map_err(|_| EvalError::builder_already_finished("builder-set", call_span))?;

        // Return the builder for chaining
        Ok(Arc::clone(&args[2]))
    })
}

/// `builder-delete`: Remove a key from a builder. Returns the builder for chaining.
/// Takes 2 args: key (Int or String), builder.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_delete(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-delete", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-delete".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let builder_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-delete".to_string(),
                    "Builder",
                    other.type_name(),
                    args[1].span.clone(),
                )
                .into())
            }
        };

        // Delete the key
        builder
            .delete(&key)
            .map_err(|_| EvalError::builder_already_finished("builder-delete", call_span))?;

        // Return the builder for chaining
        Ok(Arc::clone(&args[1]))
    })
}

/// `builder-finish`: Take the inner dict from a builder, freezing it permanently.
/// Takes 1 arg: builder. Returns a Dict.
/// Errors if the builder is already frozen.
pub(crate) fn builtin_builder_finish(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-finish", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-finish".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Take the inner dict, freezing the builder
        let dict = builder.finish().map_err(|_| {
            EvalError::builder_already_finished("builder-finish", call_span.clone())
        })?;

        ok_val(Value::Dict(dict), call_span)
    })
}

/// `builder-snapshot`: Clone the inner dict without freezing the builder.
/// Takes 1 arg: builder. Returns a Dict.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_snapshot(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-snapshot", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-snapshot".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // Clone the inner dict
        let dict = builder.snapshot().map_err(|_| {
            EvalError::builder_already_finished("builder-snapshot", call_span.clone())
        })?;

        ok_val(Value::Dict(dict), call_span)
    })
}

/// `builder-has?`: Check if a key exists in a builder.
/// Takes 2 args: key (Int or String), builder. Returns Bool.
pub(crate) fn builtin_builder_has(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-has?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-has?".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let builder_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-has?".to_string(),
                    "Builder",
                    other.type_name(),
                    args[1].span.clone(),
                )
                .into())
            }
        };

        // Frozen builder: has? is an error, not a silent false
        if builder.is_frozen() {
            return Err(
                EvalError::builder_already_finished("builder-has?", call_span.clone()).into(),
            );
        }

        // Check if the key exists
        let has = builder.has(&key);
        ok_val(Value::Int(if has { 1 } else { 0 }), call_span)
    })
}

/// `builder-get`: Get a value from a builder by key.
/// Takes 2 args: key (Int or String), builder. Returns the value or errors if key not found.
pub(crate) fn builtin_builder_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => HashableValue::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let builder_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get".to_string(),
                    "Builder",
                    other.type_name(),
                    args[1].span.clone(),
                )
                .into())
            }
        };

        // Frozen builder: get is an error distinct from key-not-found
        if builder.is_frozen() {
            return Err(
                EvalError::builder_already_finished("builder-get", call_span.clone()).into(),
            );
        }

        // Get the value
        match builder.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(thunk_id).clone();
                Ok(thunk)
            }
            None => {
                let key_str = match &key {
                    HashableValue::Int(n) => n.to_string(),
                    HashableValue::Str(s) => s.to_string(),
                    _ => "<other>".to_string(),
                };
                Err(EvalError::key_not_found(&key_str, vec![], call_span).into())
            }
        }
    })
}

/// `builtin-get-by-field`: Reverse lookup on a type-level lookup table (T-1378).
///
/// Takes 3 args:
/// - `field-name`: String — the constant field to match on (e.g. `"rcode"`)
/// - `field-value`: Any — the target constant value (e.g. `Int(2)`)
/// - `type-dict`: Dict — the runtime constructor dict produced by `[type ...]`
///   (e.g. `DnsRcode` = `{"NoError": Variant("DnsRcode.NoError"), ...}`)
///
/// Searches `TyConDef.constructor_constants` for the first variant whose
/// `field-name` constant equals `field-value`. Returns that variant or
/// `Absent.Absent` if no match.
///
/// `[get rcode: 2 DnsRcode]` desugars to `[builtin-get-by-field "rcode" 2 DnsRcode]`
/// via the `get` wrapper in prelude.
pub(crate) fn builtin_get_by_field(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-get-by-field", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }

        // arg[0]: field-name (String) — pre-forced by pos_strictness
        let field_name_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let field_name: String = match field_name_val {
            Value::String {
                ref source,
                start,
                end,
            } => source[start..end].to_string(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-get-by-field".to_string(),
                    "String",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into())
            }
        };

        // arg[1]: field-value (Any) — pre-forced by pos_strictness
        let field_value = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");

        // arg[2]: type-dict (Dict of Variants) — pre-forced to Spine by pos_strictness.
        // Eagerly collect all (unqualified-name → ThunkId) pairs from the dict so we
        // hold no borrow across the upcoming await points.
        let type_dict_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness (Spine)");
        // Collect (string-key → ThunkId) pairs; skip integer-keyed entries.
        let dict_entries: Vec<(String, ThunkId)> = match &type_dict_val {
            Value::Dict(map) => map
                .iter()
                .filter_map(|(k, thunk_id)| match k {
                    HashableValue::Str(s) => Some((s.to_string(), *thunk_id)),
                    _ => None,
                })
                .collect(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-get-by-field".to_string(),
                    "Dict",
                    other.type_name(),
                    args[2].span.clone(),
                )
                .into())
            }
        };

        if dict_entries.is_empty() {
            return ok_val(
                Value::Variant {
                    tag: "Absent.Absent".into(),
                    payload: None,
                },
                call_span,
            );
        }

        // Infer the type name from the first variant tag in the dict.
        // e.g. dict entry "ServFail" → Variant("DnsRcode.ServFail") → type_name = "DnsRcode".
        // Each entry thunk is a CoreExpr::Variant — force via materialize.
        let mut type_name: Option<String> = None;
        for (_name, thunk_id) in &dict_entries {
            let entry_thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
            let entry_val = materialize(&entry_thunk, Some(&call_span), &ctx).await?;
            if let Value::Variant { ref tag, .. } = entry_val {
                // tag is "TypeName.CtorName" — prefix before the first '.' is the type name
                if let Some(prefix) = tag.split('.').next() {
                    type_name = Some(prefix.to_string());
                    break;
                }
            }
        }

        let type_name = match type_name {
            Some(n) => n,
            None => {
                // Dict contained no Variant values — return Absent.Absent
                return ok_val(
                    Value::Variant {
                        tag: "Absent.Absent".into(),
                        payload: None,
                    },
                    call_span,
                );
            }
        };

        // Look up TyConDef to scan constructor_constants.
        // Falls back to Absent.Absent when no type info is available (--no-typecheck).
        let tycon_env = match ctx.tycon_env.get() {
            Some(env) => env,
            None => {
                return ok_val(
                    Value::Variant {
                        tag: "Absent.Absent".into(),
                        payload: None,
                    },
                    call_span,
                );
            }
        };

        let def = match tycon_env.get(type_name.as_str()) {
            Some(d) => d,
            None => {
                return ok_val(
                    Value::Variant {
                        tag: "Absent.Absent".into(),
                        payload: None,
                    },
                    call_span,
                );
            }
        };

        // Scan constructor_constants for the first variant where
        // constants[field_name] == field_value.
        // constructor_constants: IndexMap<String (qualified tag), IndexMap<String (field), Value>>
        for (qualified_tag, constants) in &def.constructor_constants {
            if let Some(constant_val) = constants.get(field_name.as_ref() as &str) {
                if *constant_val == field_value {
                    // Found a match. qualified_tag is e.g. "DnsRcode.ServFail".
                    // The dict_entries key is the unqualified name "ServFail".
                    let unqualified = qualified_tag
                        .strip_prefix(&format!("{}.", type_name))
                        .unwrap_or(qualified_tag.as_str());
                    // Find the matching ThunkId in our pre-collected dict_entries.
                    if let Some((_name, thunk_id)) =
                        dict_entries.iter().find(|(n, _)| n == unqualified)
                    {
                        let thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
                        return Ok(thunk);
                    }
                }
            }
        }

        // No match found
        ok_val(
            Value::Variant {
                tag: "Absent.Absent".into(),
                payload: None,
            },
            call_span,
        )
    })
}

// ── Collection operations (moved from builtins_seq_xform.rs and builtins_seq_reduce.rs
//    in T-1380 — Dict-only dispatch; Seq paths implemented as pure tinct in prelude.llt) ────────

/// `builtin-take`: Take the first n elements from a Dict by position.
///
/// Dict path only (T-1380). For Seq inputs, use the pure-tinct `take` wrapper in prelude.llt.
/// Returns a Dict with the first n entries preserving keys.
///
/// Args: (n, xs)
pub(crate) fn builtin_take(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("take", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let n_int = match n {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "take".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        if n_int <= 0 {
            return ok_val(Value::Dict(IndexMap::new()), call_span);
        }

        // Flatten Overlay to Dict before dispatch.
        let xs = match xs {
            Value::Overlay(l, r) => Value::Dict(
                crate::builtins::flatten_overlay(&l, &r, "take", &ctx, call_span.clone()).await?,
            ),
            other => other,
        };

        match xs {
            Value::Dict(ref map) => {
                let taken: IndexMap<HashableValue, ThunkId> = map
                    .iter()
                    .take(n_int as usize)
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                ok_val(Value::Dict(taken), call_span)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "take".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-drop`: Drop the first n elements from a Dict by position.
///
/// Dict path only (T-1380). For Seq inputs, use the pure-tinct `drop` wrapper in prelude.llt.
/// Returns a Dict with the remaining entries after skipping first n, preserving keys.
///
/// Args: (n, xs)
pub(crate) fn builtin_drop(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("drop", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let n_int = match n {
            Value::Int(i) => i,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "drop".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if n_int <= 0 {
            return Ok(Arc::clone(&args[1]));
        }

        let xs = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Flatten Overlay to Dict before dispatch.
        let xs = match xs {
            Value::Overlay(l, r) => Value::Dict(
                crate::builtins::flatten_overlay(&l, &r, "drop", &ctx, call_span.clone()).await?,
            ),
            other => other,
        };

        match xs {
            Value::Dict(ref map) => {
                let dropped: IndexMap<HashableValue, ThunkId> = map
                    .iter()
                    .skip(n_int as usize)
                    .map(|(k, v)| (k.clone(), *v))
                    .collect();
                ok_val(Value::Dict(dropped), call_span)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "drop".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-concat`: Concatenate two collections (Dict or Seq).
///
/// Moved from builtins_seq_reduce.rs to builtins_dict.rs in T-1380.
/// Seq path is preserved for the type-stage evaluator (TypeNode.children uses builtin-concat
/// on Seq values). For user code, the `concat` prelude wrapper dispatches to concat-seq
/// (pure tinct) for Seq inputs.
///
/// - For Seq: lazily chain xs and ys (O(1) initial, O(n) on materialization).
/// - For Dict: eagerly materialize both dicts and merge them with integer reindexing.
///
/// Args: (xs, ys)
pub(crate) fn builtin_concat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("concat", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let xs_span = args[0].span.clone();
        let ys_span = args[1].span.clone();
        let xs = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let ys_thunk = Arc::clone(&args[1]);
        // Flatten Overlay to Dict before dispatch.
        let xs = match xs {
            Value::Overlay(l, r) => Value::Dict(
                crate::builtins::flatten_overlay(&l, &r, "concat", &ctx, call_span.clone()).await?,
            ),
            other => other,
        };

        match xs {
            Value::Dict(ref xs_map) => {
                // Dict path: eagerly merge with integer reindexing.
                if xs_map.is_empty() {
                    return Ok(ys_thunk);
                }

                let ys = materialize(&ys_thunk, None, &ctx).await?;
                let ys = match ys {
                    Value::Overlay(l, r) => Value::Dict(
                        crate::builtins::flatten_overlay(&l, &r, "concat", &ctx, call_span.clone())
                            .await?,
                    ),
                    other => other,
                };
                match ys {
                    Value::Dict(ref ys_map) => {
                        let mut result = IndexMap::with_capacity(xs_map.len() + ys_map.len());
                        let mut idx = 0i64;
                        for (_key, value_thunk_id) in xs_map {
                            result.insert(HashableValue::Int(idx), *value_thunk_id);
                            idx = idx.checked_add(1).ok_or_else(|| {
                                EvalError::integer_overflow("concat".to_string(), call_span.clone())
                            })?;
                        }
                        for (_key, value_thunk_id) in ys_map {
                            result.insert(HashableValue::Int(idx), *value_thunk_id);
                            idx = idx.checked_add(1).ok_or_else(|| {
                                EvalError::integer_overflow("concat".to_string(), call_span.clone())
                            })?;
                        }
                        ok_val(Value::Dict(result), call_span)
                    }
                    other => Err(EvalError::type_mismatch_ctx(
                        "concat".to_string(),
                        "Dict",
                        other.type_name(),
                        ys_span,
                    )
                    .with_materialization_span(call_span)
                    .into()),
                }
            }
            other => Err(EvalError::type_mismatch_ctx(
                "concat".to_string(),
                "Dict",
                other.type_name(),
                xs_span,
            )
            .with_materialization_span(call_span)
            .into()),
        }
    })
}

/// Register `builtin-*` type aliases for dict/builder builtins (T-1102).
///
/// Each alias copies the TypeScheme from the canonical name already registered in
/// `core_type_env`. Call this AFTER `core_type_env` has run.
pub fn dict_builtin_types(env: &mut crate::types::TypeEnv) {
    env.alias_types(&[
        ("builtin-keys", "keys"),
        ("builtin-merge", "merge"),
        ("builtin-dict-nth", "dict-nth"),
        ("builtin-dict-key-nth", "dict-key-nth"),
        ("builtin-dict-kv-nth", "dict-kv-nth"),
        ("builtin-append", "append"),
        ("builtin-length", "length"),
        ("builtin-make-builder", "make-builder"),
        ("builtin-builder-set", "builder-set"),
        ("builtin-builder-delete", "builder-delete"),
        ("builtin-builder-finish", "builder-finish"),
        ("builtin-builder-snapshot", "builder-snapshot"),
        ("builtin-builder-has?", "builder-has?"),
        ("builtin-builder-get", "builder-get"),
        ("builtin-builder-get-or", "builder-get-or"),
        ("builtin-reduce", "reduce"),
        ("builtin-map", "map"),
        ("builtin-filter", "filter"),
        ("builtin-take", "take"),
        ("builtin-drop", "drop"),
        ("builtin-join", "join"),
        ("builtin-concat", "concat"),
    ]);
}
