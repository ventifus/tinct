//! Dict/access builtins: keys, length, append, get, each, each-key, each-kv.
//!
//! These builtins operate on dictionary (Dict) values, providing primitive
//! operations for accessing and transforming key-value structures.
//!
//! **Dict primitives:**
//! - `keys`: Extract dict keys as an auto-indexed dict
//! - `length`: Count dict entries
//! - `append`: Insert value at next integer key
//! - `builtin-get`: Primitive dict key lookup (errors on missing key)
//! - `builtin-has-key?`: Returns Int 1 if key exists, Int 0 if not (O(1), no value force)
//!
//! **Dict single-step primitives (drive laziness from tinct side):**
//! - `builtin-dict-has-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-nth`: Get value at insertion-order position i (errors on out of bounds)
//! - `builtin-dict-has-key-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-key-nth`: Get key at insertion-order position i (errors on out of bounds)
//! - `builtin-dict-has-kv-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-kv-nth`: Get {key,value} pair at position i (errors on out of bounds)
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
use crate::rust_span;
use crate::value::{string_val, BuiltinArgs, HashableValue, Thunk, Value};

/// Convert a runtime `Value` to a `HashableValue` dict key.
/// Returns an error if the value is not hashable (Function, Handle, Seq, etc.).
fn value_to_hashable_key(
    val: &Value,
    builtin_name: &str,
    span: crate::ast::Span,
) -> EvalResult<HashableValue> {
    match val {
        Value::Int(n) => Ok(HashableValue::Int(*n)),
        Value::String { source, start, end } => {
            let s = &source[*start..*end];
            Ok(HashableValue::Str(s.into()))
        }
        Value::Variant {
            tycon,
            ctor,
            payload,
            ..
        } => {
            let tag = format!("{}.{}", tycon, ctor);
            if payload.is_none() {
                // Check for Boolean.True/False -> Bool
                if tag == "Boolean.True" {
                    return Ok(HashableValue::Bool(true));
                }
                if tag == "Boolean.False" {
                    return Ok(HashableValue::Bool(false));
                }
            }
            // General variant key
            let hv_payload = match payload {
                None => None,
                Some(p) => {
                    let p_val = p.try_get_value().ok_or_else(|| {
                        EvalError::internal(
                            format!("{builtin_name}: variant payload not materialized"),
                            span.clone(),
                        )
                    })?;
                    Some(Box::new(value_to_hashable_key(
                        p_val,
                        builtin_name,
                        span.clone(),
                    )?))
                }
            };
            Ok(HashableValue::Variant {
                tag: tag.into(),
                payload: hv_payload,
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            builtin_name.to_string(),
            "Int, String, Boolean, or Variant",
            other.type_name(),
            span,
        )
        .into()),
    }
}

/// Convert a `HashableValue` key back to a runtime `Value`.
fn hashable_value_to_value(hv: &HashableValue) -> Value {
    match hv {
        HashableValue::Int(n) => Value::Int(*n),
        HashableValue::Str(s) => string_val(s),
        HashableValue::Bool(b) => Value::Variant {
            tycon: "Boolean".into(),
            ctor: if *b { "True" } else { "False" }.into(),
            payload: None,
        },
        HashableValue::Dict(pairs) => {
            let mut map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(pairs.len());
            for (k, v) in pairs {
                let v_val = hashable_value_to_value(v);
                map.insert(k.clone(), Arc::new(Thunk::value(v_val, rust_span!())));
            }
            Value::Dict(map)
        }
        HashableValue::Variant { tag, payload } => {
            // Split tag on '.' to get tycon and ctor
            let (tycon, ctor) = if let Some(dot_pos) = tag.rfind('.') {
                (&tag[..dot_pos], &tag[dot_pos + 1..])
            } else {
                (tag.as_ref(), "")
            };
            let payload_thunk = payload.as_ref().map(|p| {
                let val = hashable_value_to_value(p);
                Arc::new(Thunk::value(val, rust_span!()))
            });
            Value::Variant {
                tycon: tycon.into(),
                ctor: ctor.into(),
                payload: payload_thunk,
            }
        }
    }
}

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
        let thunk0 = args[0].clone();
        let val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
        let map = crate::builtins::require_dict(
            "keys",
            val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        let origin = call_span.clone();
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(map.len());
        for (i, (key, _)) in map.iter().enumerate() {
            let key_value = hashable_value_to_value(key);
            let thunk = Arc::new(Thunk::value(key_value, origin.clone()));
            result.insert(
                HashableValue::Int(i64::try_from(i).map_err(|_| {
                    EvalError::internal("collection index overflow".to_string(), call_span.clone())
                })?),
                thunk,
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
        let thunk0 = args[0].clone();
        let val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
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
                    thunk0.span.clone(),
                    &ctx,
                    call_span.clone(),
                )
                .await?;
                ok_val(Value::Int(map.len() as i64), call_span)
            }
        }
    })
}

/// `builtin-get`: Rust primitive for keyed access on Dict, Variant, and Proxy values.
///
/// Takes 2 args: a key (Int or String) and a target value.
/// Returns the value at that key, or errors if the key is not found.
///
/// Handles:
/// - Dict: O(1) key lookup
/// - Variant (with payload): auto-unpacks payload dict and looks up key; falls back to
///   TyConDef constructor constants (T-1358) if not found in payload
/// - Variant (unit, no payload): looks up TyConDef constructor constants directly
/// - Proxy: invokes the proxy handler with the key string
///
/// This is the single primitive backing both dot-access (`target.field`) and explicit
/// `[get key target]`. The lowerer desugars dot-access to `[builtin-get key target]`.
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
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
        let key = value_to_hashable_key(&key_val, "builtin-get", thunk0.span.clone())?;

        // Materialize the target
        let thunk1 = args[1].clone();
        let target_val = thunk1
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
        let target_span = thunk1.span.clone();

        builtin_get_on_value(key, target_val, target_span, call_span, None, &ctx).await
    })
}

/// Inner recursive helper for `builtin-get`, handling Dict, Variant auto-unpack,
/// TyConDef constructor constants, and Proxy dispatch.
///
/// `variant_tag`: when accessing a Variant payload, carry the tag for TyConDef constant fallback.
async fn builtin_get_on_value(
    key: HashableValue,
    target_val: Value,
    target_span: crate::ast::Span,
    call_span: crate::ast::Span,
    variant_tag: Option<String>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
    let key_str = key.to_string();

    match target_val {
        Value::Dict(map) => {
            let thunk_opt = map.get(&key);
            match thunk_opt {
                Some(thunk) => Ok(Arc::clone(thunk)),
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
                                            let thunk = Arc::new(Thunk::value(
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
            crate::eval_access::invoke_proxy_handler(
                &handler,
                string_val(&key_str),
                0,
                ctx,
                &call_span,
            )
            .await
        }
        Value::Variant {
            tycon,
            ctor,
            payload,
        } => {
            // Variant auto-unpacking: dot-access on a variant accesses the payload.
            match payload {
                Some(payload_id) => {
                    let payload_span = payload_id.span.clone();
                    let payload_val = materialize(&payload_id, Some(&call_span), ctx).await?;
                    // Recurse with variant_tag set so TyConDef constants can be found.
                    let composite_tag = format!("{}.{}", tycon, ctor);
                    Box::pin(builtin_get_on_value(
                        key,
                        payload_val,
                        payload_span,
                        call_span,
                        Some(composite_tag),
                        ctx,
                    ))
                    .await
                }
                None => {
                    // Unit variant: try TyConDef constructor constants first (T-1358).
                    let composite_tag = format!("{}.{}", tycon, ctor);
                    if let HashableValue::Str(ref field_name) = key {
                        if let Some(tycon_env) = ctx.tycon_env.get() {
                            if let Some(def) = tycon_env.get(&*tycon) {
                                if let Some(constants) =
                                    def.constructor_constants.get(composite_tag.as_str())
                                {
                                    if let Some(const_val) =
                                        constants.get(field_name.as_ref() as &str)
                                    {
                                        let thunk = Arc::new(Thunk::value(
                                            const_val.clone(),
                                            call_span.clone(),
                                        ));
                                        return Ok(thunk);
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
        other => {
            let key_display = match &key {
                HashableValue::Int(n) => format!("key {n}"),
                HashableValue::Str(s) => format!("key \"{s}\""),
                other_key => format!("key {other_key}"),
            };
            let context = format!("builtin-get ({key_display})");
            Err(EvalError::type_mismatch_ctx(
                context,
                "Dict, Variant, or Proxy",
                other.type_name(),
                target_span,
            )
            .into())
        }
    }
}

/// `builtin-has-key?`: Check whether a key exists in a dict.
///
/// Takes 2 args: a key (Int or String) and a dict.
/// Returns `Int 1` if the key is present, `Int 0` if absent.
/// Does NOT force the value at the key — O(1) spine-only lookup.
/// Prelude composes this with `builtin-get` to implement `get?` without Rust knowing Absent.
pub(crate) fn builtin_has_key(
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
        reject_named("builtin-has-key?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // Materialize the key
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness")
            .clone();
        let key = value_to_hashable_key(&key_val, "builtin-has-key?", thunk0.span.clone())?;

        // Materialize the dict (spine only, not values)
        let thunk1 = args[1].clone();
        let dict_val = thunk1
            .try_get_value()
            .expect("pre-materialized by pos_strictness")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-has-key?",
            dict_val,
            thunk1.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        let exists = if map.contains_key(&key) { 1i64 } else { 0i64 };
        ok_val(Value::Int(exists), call_span)
    })
}

/// `builtin-dict-has-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// O(1) — used by prelude step functions to drive laziness without constructing Absent.
/// `Dict a → Int → Int`
pub(crate) fn builtin_dict_has_nth(
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
        reject_named("builtin-dict-has-nth?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-has-nth?",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        let exists = if usize::try_from(idx)
            .ok()
            .and_then(|i| map.get_index(i))
            .is_some()
        {
            1i64
        } else {
            0i64
        };
        ok_val(Value::Int(exists), call_span)
    })
}

/// `builtin-dict-nth`: Get the value at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Errors if `i` is out of bounds.
/// O(1) — drives laziness from the tinct side via prelude step functions that
/// guard with `builtin-dict-has-nth?` before calling this.
/// `Dict a → Int → a`
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
        reject_named("builtin-dict-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-nth",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((_, thunk)) => Ok(Arc::clone(thunk)),
            None => Err(EvalError::user_error(
                format!(
                    "builtin-dict-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-dict-has-key-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// Identical to `builtin-dict-has-nth?` but paired with `builtin-dict-key-nth`.
/// `Dict a → Int → Int`
pub(crate) fn builtin_dict_has_key_nth(
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
        reject_named(
            "builtin-dict-has-key-nth?",
            named.as_ref(),
            call_span.clone(),
        )?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-has-key-nth?",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        let exists = if usize::try_from(idx)
            .ok()
            .and_then(|i| map.get_index(i))
            .is_some()
        {
            1i64
        } else {
            0i64
        };
        ok_val(Value::Int(exists), call_span)
    })
}

/// `builtin-dict-key-nth`: Get the key at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Errors if `i` is out of bounds.
/// Prelude guards with `builtin-dict-has-key-nth?` before calling this.
/// `Dict a → Int → Key`
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
        reject_named("builtin-dict-key-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-key-nth",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((key, _)) => {
                let key_val = hashable_value_to_value(key);
                ok_val(key_val, call_span)
            }
            None => Err(EvalError::user_error(
                format!(
                    "builtin-dict-key-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-dict-has-kv-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// Identical to `builtin-dict-has-nth?` but paired with `builtin-dict-kv-nth`.
/// `Dict a → Int → Int`
pub(crate) fn builtin_dict_has_kv_nth(
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
        reject_named(
            "builtin-dict-has-kv-nth?",
            named.as_ref(),
            call_span.clone(),
        )?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-has-kv-nth?",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        let exists = if usize::try_from(idx)
            .ok()
            .and_then(|i| map.get_index(i))
            .is_some()
        {
            1i64
        } else {
            0i64
        };
        ok_val(Value::Int(exists), call_span)
    })
}

/// `builtin-dict-kv-nth`: Get the key-value pair at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns a `[key: K  value: V]` dict at that position.
/// Errors if `i` is out of bounds. Prelude guards with `builtin-dict-has-kv-nth?`.
/// `Dict a → Int → [key: Key  value: a]`
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
        reject_named("builtin-dict-kv-nth", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let thunk0 = args[0].clone();
        let dict_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness[0]=Spine")
            .clone();
        let map = crate::builtins::require_dict(
            "builtin-dict-kv-nth",
            dict_val,
            thunk0.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;
        let idx = match args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness[1]=Seq")
            .clone()
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        match usize::try_from(idx).ok().and_then(|i| map.get_index(i)) {
            Some((key, val_thunk)) => {
                let key_val = hashable_value_to_value(key);
                let mut kv: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                kv.insert(
                    HashableValue::Str("key".into()),
                    ok_val(key_val, call_span.clone())?,
                );
                kv.insert(HashableValue::Str("value".into()), Arc::clone(val_thunk));
                ok_val(Value::Dict(kv), call_span)
            }
            None => Err(EvalError::user_error(
                format!(
                    "builtin-dict-kv-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )
            .into()),
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get-or", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // args[1] (default value) is NOT materialized — inserted as Arc<Thunk> if key absent.
        let default_thunk = args[1].clone();

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let thunk2 = args[2].clone();
        let builder_val = thunk2
            .try_get_value()
            .expect("pre-materialized by pos_strictness[2]=Seq via W1 scan")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get-or".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk2.span.clone(),
                )
                .into())
            }
        };

        // Atomic get-or-insert: single mutex acquisition
        let result_thunk = builder
            .get_or(key, default_thunk)
            .map_err(|_| EvalError::builder_already_finished("builder-get-or", call_span))?;

        Ok(result_thunk)
    })
}

/// `build-dict`: Efficiently construct a dict from a Seq or Dict of key-value pairs.
///
/// Takes 1 arg (a Seq or Dict where each element is a dict with `key` and `value` fields).
/// Returns a new flat Dict with those entries.
///
/// - **Seq input:** Each element should be `[key: K, value: V]` (like what `each-kv` returns).
///   Forces the key (to extract it), keeps value lazy. Builds an IndexMap. O(n).
/// - **Dict input:** Copies all entries into a new flat IndexMap. O(n).
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
        let thunk0 = args[0].clone();
        let val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count=1")
            .clone();

        match val {
            // Dict input: build from key-value pair dicts in insertion order.
            // Each entry value must be a dict with "key" and "value" fields.
            Value::Dict(ref map) => {
                let mut result: IndexMap<HashableValue, Arc<Thunk>> =
                    IndexMap::with_capacity(map.len());
                for (_idx, entry_thunk) in map {
                    let entry_val = materialize(entry_thunk, None, &ctx).await?;
                    let entry_map = crate::builtins::require_dict(
                        "build-dict entry",
                        entry_val,
                        entry_thunk.span.clone(),
                        &ctx,
                        call_span.clone(),
                    )
                    .await?;
                    let key_thunk = entry_map
                        .get(&HashableValue::Str("key".into()))
                        .ok_or_else(|| {
                            EvalError::key_not_found("key", vec![], call_span.clone())
                        })?;
                    let value_thunk = entry_map
                        .get(&HashableValue::Str("value".into()))
                        .ok_or_else(|| {
                            EvalError::key_not_found("value", vec![], call_span.clone())
                        })?;
                    let key_val = materialize(key_thunk, None, &ctx).await?;
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
                    result.insert(key, Arc::clone(value_thunk));
                }
                ok_val(Value::Dict(result), call_span)
            }

            other => Err(EvalError::type_mismatch_ctx(
                "build-dict".to_string(),
                "Dict of [key: K  value: V] pairs",
                other.type_name(),
                thunk0.span.clone(),
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
            let cap_thunk = named_map.get("capacity").map(Arc::clone);
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-set", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // args[1] (value) is NOT materialized — pass the Arc<Thunk> directly to the builder
        let value_thunk = args[1].clone();

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let thunk2 = args[2].clone();
        let builder_val = thunk2
            .try_get_value()
            .expect("pre-materialized by pos_strictness[2]=Seq via W1 scan")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-set".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk2.span.clone(),
                )
                .into())
            }
        };

        // Set the key-value pair
        builder
            .set(key, value_thunk)
            .map_err(|_| EvalError::builder_already_finished("builder-set", call_span))?;

        // Return the builder for chaining
        Ok(args[2].clone())
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-delete", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = thunk1
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-delete".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk1.span.clone(),
                )
                .into())
            }
        };

        // Delete the key
        builder
            .delete(&key)
            .map_err(|_| EvalError::builder_already_finished("builder-delete", call_span))?;

        // Return the builder for chaining
        Ok(args[1].clone())
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-finish", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let thunk0 = args[0].clone();
        let builder_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-finish".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk0.span.clone(),
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-snapshot", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let thunk0 = args[0].clone();
        let builder_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-snapshot".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk0.span.clone(),
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-has?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = thunk1
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-has?".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk1.span.clone(),
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
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span.clone()).into());
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = thunk1
            .try_get_value()
            .expect("pre-materialized by force_count")
            .clone();
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get".to_string(),
                    "Builder",
                    other.type_name(),
                    thunk1.span.clone(),
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
            Some(thunk) => Ok(thunk),
            None => {
                let key_str = key.to_string();
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
/// errors if no match.
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
        let thunk0 = args[0].clone();
        let field_name_val = thunk0
            .try_get_value()
            .expect("pre-materialized by pos_strictness")
            .clone();
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
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        // arg[1]: field-value (Any) — pre-forced by pos_strictness
        let field_value = args[1]
            .try_get_value()
            .expect("pre-materialized by pos_strictness")
            .clone();

        // arg[2]: type-dict (Dict of Variants) — pre-forced to Spine by pos_strictness.
        // Eagerly collect all (unqualified-name → ThunkId) pairs from the dict so we
        // hold no borrow across the upcoming await points.
        let thunk2 = args[2].clone();
        let type_dict_val = thunk2
            .try_get_value()
            .expect("pre-materialized by pos_strictness (Spine)")
            .clone();
        // Collect (string-key → Arc<Thunk>) pairs; skip integer-keyed entries.
        let dict_entries: Vec<(String, Arc<Thunk>)> = match &type_dict_val {
            Value::Dict(map) => map
                .iter()
                .filter_map(|(k, thunk)| match k {
                    HashableValue::Str(s) => Some((s.to_string(), Arc::clone(thunk))),
                    _ => None,
                })
                .collect(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-get-by-field".to_string(),
                    "Dict",
                    other.type_name(),
                    thunk2.span.clone(),
                )
                .into())
            }
        };

        if dict_entries.is_empty() {
            return Err(EvalError::user_error(
                "builtin-get-by-field: type dict is empty".to_string(),
                call_span,
            )
            .into());
        }

        // Infer the type name from the first variant tag in the dict.
        // e.g. dict entry "ServFail" → Variant("DnsRcode.ServFail") → type_name = "DnsRcode".
        // Each entry thunk is a CoreExpr::Variant — force via materialize.
        let mut type_name: Option<String> = None;
        for (_name, entry_thunk) in &dict_entries {
            let entry_val = materialize(entry_thunk, Some(&call_span), &ctx).await?;
            if let Value::Variant { ref tycon, .. } = entry_val {
                // tycon is the type name
                {
                    let prefix = tycon;
                    type_name = Some(prefix.to_string());
                    break;
                }
            }
        }

        let type_name = match type_name {
            Some(n) => n,
            None => {
                return Err(EvalError::user_error(
                    "builtin-get-by-field: type dict contains no Variant values".to_string(),
                    call_span,
                )
                .into())
            }
        };

        // Look up TyConDef to scan constructor_constants.
        let tycon_env = match ctx.tycon_env.get() {
            Some(env) => env,
            None => {
                return Err(EvalError::user_error(
                    format!(
                        "builtin-get-by-field: type info not available for type {type_name} \
                         (--no-typecheck mode)"
                    ),
                    call_span,
                )
                .into())
            }
        };

        let def = match tycon_env.get(type_name.as_str()) {
            Some(d) => d,
            None => {
                return Err(EvalError::user_error(
                    format!("builtin-get-by-field: type {type_name} not found in type environment"),
                    call_span,
                )
                .into())
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
                    if let Some((_name, thunk)) =
                        dict_entries.iter().find(|(n, _)| n == unqualified)
                    {
                        return Ok(Arc::clone(thunk));
                    }
                }
            }
        }

        // No match found — error
        Err(EvalError::user_error(
            format!(
                "builtin-get-by-field: no constructor of {type_name} has field \"{field_name}\" \
                 matching the given value"
            ),
            call_span,
        )
        .into())
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
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("take", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
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

        let xs = Arc::clone(&args[1])
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();

        if n_int <= 0 {
            return ok_val(Value::Dict(IndexMap::new()), call_span);
        }

        match xs {
            Value::Dict(ref map) => {
                let taken: IndexMap<HashableValue, Arc<Thunk>> = map
                    .iter()
                    .take(n_int as usize)
                    .map(|(k, v)| (k.clone(), Arc::clone(v)))
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
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("drop", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let n = args[0]
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
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
            return Ok(args[1].clone());
        }

        let xs = args[1]
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();

        match xs {
            Value::Dict(ref map) => {
                let dropped: IndexMap<HashableValue, Arc<Thunk>> = map
                    .iter()
                    .skip(n_int as usize)
                    .map(|(k, v)| (k.clone(), Arc::clone(v)))
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

        let thunk0 = args[0].clone();
        let thunk1 = args[1].clone();
        let xs_span = thunk0.span.clone();
        let ys_span = thunk1.span.clone();
        let xs = thunk0
            .try_get_value()
            .expect("pre-materialized by force_count/pos_strictness")
            .clone();
        let ys_thunk = thunk1;

        match xs {
            Value::Dict(ref xs_map) => {
                // Dict path: eagerly merge with integer reindexing.
                if xs_map.is_empty() {
                    return Ok(ys_thunk);
                }

                let ys = materialize(&ys_thunk, None, &ctx).await?;
                match ys {
                    Value::Dict(ref ys_map) => {
                        let mut result: IndexMap<HashableValue, Arc<Thunk>> =
                            IndexMap::with_capacity(xs_map.len() + ys_map.len());
                        let mut idx = 0i64;
                        for (_key, thunk) in xs_map {
                            result.insert(HashableValue::Int(idx), Arc::clone(thunk));
                            idx = idx.checked_add(1).ok_or_else(|| {
                                EvalError::integer_overflow("concat".to_string(), call_span.clone())
                            })?;
                        }
                        for (_key, thunk) in ys_map {
                            result.insert(HashableValue::Int(idx), Arc::clone(thunk));
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
