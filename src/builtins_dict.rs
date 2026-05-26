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
//! **Dict iterators:**
//! - `each`: Convert dict to Seq of values
//! - `each-key`: Convert dict to Seq of keys
//! - `each-kv`: Convert dict to Seq of {key, value} pairs
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
//! All three iterators preserve insertion order and use an O(n) offset-based
//! recursion strategy to avoid O(n²) IndexMap rebuilds.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{builtin, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{string_val, BuiltinArgs, Key, StrKey, Strictness, Thunk, Value};

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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("keys", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        // arg[0] is pre-forced by force_count.
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let map = crate::builtins::require_dict("keys", val, args[0].span, &ctx, call_span)?;

        let origin = call_span;
        let mut result = IndexMap::with_capacity(map.len());
        for (i, (key, _)) in map.iter().enumerate() {
            let key_value = match key {
                Key::Int(n) => Value::Int(*n),
                Key::String(s) => string_val(s),
            };
            let thunk = Arc::new(Thunk::new_materialized(key_value, origin));
            let thunk_id = ctx.alloc_thunk(thunk);
            result.insert(
                Key::Int(i64::try_from(i).map_err(|_| {
                    EvalError::internal("collection index overflow".to_string(), call_span)
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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("length", named.as_ref(), call_span)?;
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
                        call_span,
                    )
                })?;
                ok_val(Value::Int(len_i64), call_span)
            }
            Value::Bytes { start, end, .. } => {
                let len = end - start;
                let len_i64 = i64::try_from(len).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "length: byte length exceeds i64::MAX".to_string(),
                        call_span,
                    )
                })?;
                ok_val(Value::Int(len_i64), call_span)
            }
            _ => {
                let map =
                    crate::builtins::require_dict("length", val, args[0].span, &ctx, call_span)?;
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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("merge", named.as_ref(), call_span)?;
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

/// `append`: Takes 2 args: a Dict and any value. Returns a new dict with the
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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("append", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // arg[0] is pre-forced by force_count.
        // arg[1] (the value to append) is NOT materialized — it is inserted as a thunk
        // (Arc::clone at line below), preserving laziness of the appended value.
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let mut map =
            crate::builtins::require_dict("append", dict_val, args[0].span, &ctx, call_span)?;

        // Compute the next integer key: max existing int key + 1, or 0 if none.
        let next_key = map
            .keys()
            .filter_map(|k| match k {
                Key::Int(n) => Some(*n),
                _ => None,
            })
            .max();

        #[allow(clippy::result_large_err)] // EvalError size is acceptable for error path
        let next_idx = match next_key {
            Some(max) => max
                .checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("append".to_string(), call_span))?,
            None => 0,
        };

        let value_id = ctx.alloc_thunk(Arc::clone(&args[1]));
        map.insert(Key::Int(next_idx), value_id);
        ok_val(Value::Dict(map), call_span)
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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-get", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Materialize the key
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-get".to_string(),
                    "Int or String",
                    other.type_name(),
                    args[0].span,
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
            Key::Int(n) => format!("key {n}"),
            Key::String(s) => format!("key \"{s}\""),
        };
        let context = format!("builtin-get ({key_display})");
        let map = crate::builtins::require_dict(&context, dict_val, args[1].span, &ctx, call_span)?;

        // Look up the key
        match map.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
                Ok(thunk)
            }
            None => {
                let key_str = match &key {
                    Key::Int(n) => n.to_string(),
                    Key::String(s) => s.to_string(),
                };
                let available_keys = map
                    .keys()
                    .map(|k| match k {
                        Key::Int(n) => n.to_string(),
                        Key::String(s) => s.to_string(),
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
/// Returns the value if the key exists, or Value::Dict(empty) (Null) if missing.
/// NO error on missing key (unlike `builtin-get` which errors).
pub(crate) fn builtin_get_optional(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("get?", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Materialize the key
        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "get?".to_string(),
                    "Int or String",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // Materialize the dict (spine only, not values)
        let dict_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let map = crate::builtins::require_dict("get?", dict_val, args[1].span, &ctx, call_span)?;

        // Look up the key
        match map.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(*thunk_id).clone();
                Ok(thunk)
            }
            None => {
                // Return empty dict (Null) on missing key
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
        }
    })
}

/// `each`: Convert a Dict to a Seq of its values in insertion order.
///
/// Takes 1 arg (a Dict). Returns a lazy Seq of values.
/// `Dict a → Seq a`
///
/// This is a Rust builtin because Seq construction is not expressible in tinct.
pub(crate) fn builtin_each(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("each", named.as_ref(), call_span)?;
        // Public API: 1 arg (dict).
        // Internal recursive call: 2 args (dict, offset: Int) — avoids O(n²) IndexMap rebuilds.
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Parse offset from optional 2nd arg (internal recursive call).
        // O(n) fix: recursive tail carries the original dict + an index instead of rebuilding.
        let offset = if args.len() == 2 {
            // args[1] is Spine-pre-materialized by pos_strictness[1] when present.
            match args[1]
                .try_get_materialized()
                .expect("pre-materialized by pos_strictness[1]=Spine")
            {
                Value::Int(n) => n as usize,
                _ => 0,
            }
        } else {
            0
        };

        // Materialize the dict (spine only, not values).
        // args[0] is Spine-pre-materialized by pos_strictness[0].
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = crate::builtins::require_dict("each", dict_val, args[0].span, &ctx, call_span)?;

        // Skip to current offset position in the dict.
        let remaining = map.len().saturating_sub(offset);

        // Build a Seq from the values in insertion order starting at offset
        if remaining == 0 {
            ok_val(Value::Dict(IndexMap::new()), call_span)
        } else {
            let (_, head_id) = map.get_index(offset).unwrap();
            let head_id = *head_id;
            let head = ctx.thunk_arena.lock().unwrap().get(head_id).clone();

            // Build tail: if more elements remain, recurse with (same_dict_thunk, offset+1).
            // O(n) design: the same original dict thunk is passed to each recursive call;
            // only the integer offset increments. This avoids the O(n²) cost of rebuilding
            // an IndexMap of remaining entries at every step. Keys are discarded — each
            // yields values only, regardless of whether the original keys are Int or String.
            if remaining == 1 {
                let tail = ok_val(Value::Dict(IndexMap::new()), call_span)?;
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            } else {
                let next_offset = ok_val(Value::Int((offset + 1) as i64), call_span)?;
                let tail_args = vec![Arc::clone(&args[0]), next_offset];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!(
                        "builtin-each",
                        builtin_each,
                        [Strictness::Spine, Strictness::Spine]
                    ),
                    tail_args,
                    None,
                    call_span,
                    Some(Arc::from("call $each")),
                    Arc::clone(&ctx),
                ));
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            }
        }
    })
}

/// `each-key`: Convert a Dict to a Seq of its keys in insertion order.
///
/// Takes 1 arg (a Dict). Returns a lazy Seq of keys (Int or String values).
/// `Dict a → Seq Key`
///
/// This is a Rust builtin because Seq construction is not expressible in tinct.
pub(crate) fn builtin_each_key(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("each-key", named.as_ref(), call_span)?;
        // Public API: 1 arg (dict).
        // Internal recursive call: 2 args (dict, offset: Int) — avoids O(n²) IndexMap rebuilds.
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Parse offset from optional 2nd arg (internal recursive call).
        let offset = if args.len() == 2 {
            // args[1] is Spine-pre-materialized by pos_strictness[1] when present.
            match args[1]
                .try_get_materialized()
                .expect("pre-materialized by pos_strictness[1]=Spine")
            {
                Value::Int(n) => n as usize,
                _ => 0,
            }
        } else {
            0
        };

        // Materialize the dict (spine only, not values).
        // args[0] is Spine-pre-materialized by pos_strictness[0].
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map =
            crate::builtins::require_dict("each-key", dict_val, args[0].span, &ctx, call_span)?;

        // Skip to current offset position in the dict.
        let remaining = map.len().saturating_sub(offset);

        // Build a Seq from the keys in insertion order starting at offset.
        // Original keys are preserved (not synthetic Int indices) so callers receive correct key names.
        if remaining == 0 {
            ok_val(Value::Dict(IndexMap::new()), call_span)
        } else {
            let (head_key, _) = map.get_index(offset).unwrap();
            let head_val = match head_key {
                Key::Int(n) => Value::Int(*n),
                Key::String(s) => string_val(s),
            };
            let head = ok_val(head_val, call_span)?;

            // Build tail: if more elements remain, recurse with (same_dict_thunk, offset+1).
            // O(n) total: no IndexMap rebuild per step, just index increment.
            if remaining == 1 {
                let tail = ok_val(Value::Dict(IndexMap::new()), call_span)?;
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            } else {
                let next_offset = ok_val(Value::Int((offset + 1) as i64), call_span)?;
                let tail_args = vec![Arc::clone(&args[0]), next_offset];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!(
                        "each-key",
                        builtin_each_key,
                        [Strictness::Spine, Strictness::Spine]
                    ),
                    tail_args,
                    None,
                    call_span,
                    Some(Arc::from("call $each-key")),
                    Arc::clone(&ctx),
                ));
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            }
        }
    })
}

/// `each-kv`: Convert a Dict to a Seq of key-value pair dicts.
///
/// Takes 1 arg (a Dict). Returns a lazy Seq where each element is a dict
/// like `[key: K, value: V]`.
/// `Dict a → Seq [key: Key, value: a]`
///
/// This is a Rust builtin because Seq construction is not expressible in tinct.
pub(crate) fn builtin_each_kv(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("each-kv", named.as_ref(), call_span)?;
        // Public API: 1 arg (dict).
        // Internal recursive call: 2 args (dict, offset: Int) — avoids O(n²) IndexMap rebuilds.
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Parse offset from optional 2nd arg (internal recursive call).
        let offset = if args.len() == 2 {
            // args[1] is Spine-pre-materialized by pos_strictness[1] when present.
            match args[1]
                .try_get_materialized()
                .expect("pre-materialized by pos_strictness[1]=Spine")
            {
                Value::Int(n) => n as usize,
                _ => 0,
            }
        } else {
            0
        };

        // Materialize the dict (spine only, not values).
        // args[0] is Spine-pre-materialized by pos_strictness[0].
        let dict_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map =
            crate::builtins::require_dict("each-kv", dict_val, args[0].span, &ctx, call_span)?;

        // Skip to current offset position in the dict.
        let remaining = map.len().saturating_sub(offset);

        // Build a Seq from key-value pairs in insertion order starting at offset
        if remaining == 0 {
            ok_val(Value::Dict(IndexMap::new()), call_span)
        } else {
            let (head_key, head_val_id) = map.get_index(offset).unwrap();

            // Build head: [key: K, value: V]
            let mut head_dict = IndexMap::new();
            let key_val = match head_key {
                Key::Int(n) => Value::Int(*n),
                Key::String(s) => string_val(s),
            };
            head_dict.insert(
                Key::String("key".into()),
                ctx.alloc_thunk(ok_val(key_val, call_span)?),
            );
            head_dict.insert(Key::String("value".into()), *head_val_id);
            let head = ok_val(Value::Dict(head_dict), call_span)?;

            // Build tail: if more elements remain, recurse with (same_dict_thunk, offset+1).
            // O(n) total: no IndexMap rebuild per step, just index increment.
            if remaining == 1 {
                let tail = ok_val(Value::Dict(IndexMap::new()), call_span)?;
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            } else {
                let next_offset = ok_val(Value::Int((offset + 1) as i64), call_span)?;
                let tail_args = vec![Arc::clone(&args[0]), next_offset];
                let tail = Arc::new(Thunk::new_pending_builtin(
                    builtin!(
                        "each-kv",
                        builtin_each_kv,
                        [Strictness::Spine, Strictness::Spine]
                    ),
                    tail_args,
                    None,
                    call_span,
                    Some(Arc::from("call $each-kv")),
                    Arc::clone(&ctx),
                ));
                let tail_id = ctx.alloc_thunk(tail);
                ok_val(
                    Value::Seq {
                        head: ctx.alloc_thunk(head),
                        tail: tail_id,
                    },
                    call_span,
                )
            }
        }
    })
}

/// `builder-get-or`: Atomically get-or-insert in a builder.
///
/// Takes 3 args: builder, key (Int or String), default_value (any).
/// If `key` exists, returns the existing value. Otherwise inserts `default_value` at `key`
/// and returns it. Single mutex acquisition — no race between has? and set.
/// Returns the builder for chaining (NOT the looked-up value — returns builder so callers
/// can chain further operations).
///
/// NOTE: Returns the looked-up/inserted ThunkId value (not the builder).
/// Usage pattern: `[builder-set b k [cons x [builder-get-or b k []]]]`
pub(crate) fn builtin_builder_get_or(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get-or", named.as_ref(), call_span)?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get-or".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // args[1] (key) is pre-forced by force_count
        let key_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get-or".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        };

        // args[2] (default value) is NOT materialized — inserted as a thunk if key absent.
        let default_id = ctx.alloc_thunk(Arc::clone(&args[2]));

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
    } = ctx_arg;
    Box::pin(async move {
        reject_named("build-dict", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // args[0] is pre-materialized by force_count=1.
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count=1");

        match val {
            // Seq input: each element is [key: K, value: V]
            // Single-pass traversal: process each element as we walk the spine.
            // No pre-count needed — we use an IndexMap and let it grow dynamically.
            // Growth amortizes to O(1) per insert (same as Vec).
            Value::Seq { head, tail } => {
                let mut result = IndexMap::new();
                let mut current_head_id = head;
                let mut current_tail_id = tail;

                loop {
                    // Process the head: materialize it to get the [key: K, value: V] dict.
                    let head_thunk = ctx.thunk_arena.lock().unwrap().get(current_head_id).clone();
                    let head_val = materialize(&head_thunk, None, &ctx).await?;

                    let entry_map = crate::builtins::require_dict(
                        "build-dict (seq element)",
                        head_val,
                        head_thunk.span,
                        &ctx,
                        call_span,
                    )?;

                    // Extract key and value from the entry dict.
                    let key_id = entry_map.get(&StrKey("key")).ok_or_else(|| {
                        EvalError::key_not_found(
                            "key",
                            entry_map
                                .keys()
                                .map(|k| match k {
                                    Key::Int(n) => n.to_string(),
                                    Key::String(s) => s.to_string(),
                                })
                                .collect(),
                            call_span,
                        )
                    })?;

                    let value_id = entry_map.get(&StrKey("value")).ok_or_else(|| {
                        EvalError::key_not_found(
                            "value",
                            entry_map
                                .keys()
                                .map(|k| match k {
                                    Key::Int(n) => n.to_string(),
                                    Key::String(s) => s.to_string(),
                                })
                                .collect(),
                            call_span,
                        )
                    })?;

                    // Materialize the key to extract it (values stay lazy).
                    let key_thunk = ctx.thunk_arena.lock().unwrap().get(*key_id).clone();
                    let key_val = materialize(&key_thunk, None, &ctx).await?;

                    let key = match key_val {
                        Value::Int(n) => Key::Int(n),
                        Value::String {
                            ref source,
                            start,
                            end,
                        } => {
                            let s = &source[start..end];
                            Key::String(s.into())
                        }
                        other => {
                            return Err(EvalError::type_mismatch_ctx(
                                "build-dict".to_string(),
                                "Int or String (for key)",
                                other.type_name(),
                                key_thunk.span,
                            )
                            .into())
                        }
                    };

                    // Insert the entry (value stays as a thunk).
                    result.insert(key, *value_id);

                    // Advance to the next element in the Seq.
                    let tail_thunk = ctx.thunk_arena.lock().unwrap().get(current_tail_id).clone();
                    let tail_val = materialize(&tail_thunk, None, &ctx).await?;

                    match tail_val {
                        Value::Dict(map) if map.is_empty() => break,
                        Value::Seq { head, tail } => {
                            current_head_id = head;
                            current_tail_id = tail;
                        }
                        other => {
                            return Err(EvalError::type_mismatch_ctx(
                                "build-dict".to_string(),
                                "Seq (empty dict tail)",
                                other.type_name(),
                                args[0].span,
                            )
                            .into())
                        }
                    }
                }

                ok_val(Value::Dict(result), call_span)
            }

            // Dict input: flatten to a new IndexMap (eliminates Overlay depth)
            Value::Dict(map) => {
                // Pre-allocate with the known size
                let mut result = IndexMap::with_capacity(map.len());
                for (key, thunk_id) in &map {
                    result.insert(key.clone(), *thunk_id);
                }
                ok_val(Value::Dict(result), call_span)
            }

            // Overlay input: flatten to a new IndexMap
            Value::Overlay(_left_id, _right_id) => {
                // Use require_dict to flatten the overlay
                let map = crate::builtins::require_dict(
                    "build-dict",
                    val,
                    args[0].span,
                    &ctx,
                    call_span,
                )?;
                let mut result = IndexMap::with_capacity(map.len());
                for (key, thunk_id) in &map {
                    result.insert(key.clone(), *thunk_id);
                }
                ok_val(Value::Dict(result), call_span)
            }

            other => Err(EvalError::type_mismatch_ctx(
                "build-dict".to_string(),
                "Seq or Dict",
                other.type_name(),
                args[0].span,
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
    } = ctx_arg;
    Box::pin(async move {
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
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
                reject_named("make-builder", Some(&unexpected), call_span)?;
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
                            cap_thunk.span,
                        )
                        .into())
                    }
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "make-builder".to_string(),
                            "Int",
                            other.type_name(),
                            cap_thunk.span,
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
/// Takes 3 args: builder, key (Int or String), value (any).
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_set(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-set", named.as_ref(), call_span)?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-set".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // args[1] (key) is pre-forced by force_count
        let key_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-set".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        };

        // args[2] (value) is NOT materialized — inserted as a thunk
        let value_id = ctx.alloc_thunk(Arc::clone(&args[2]));

        // Set the key-value pair
        builder
            .set(key, value_id)
            .map_err(|_| EvalError::builder_already_finished("builder-set", call_span))?;

        // Return the builder for chaining
        Ok(Arc::clone(&args[0]))
    })
}

/// `builder-delete`: Remove a key from a builder. Returns the builder for chaining.
/// Takes 2 args: builder, key (Int or String).
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
        reject_named("builder-delete", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-delete".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // args[1] (key) is pre-forced by force_count
        let key_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-delete".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        };

        // Delete the key
        builder
            .delete(&key)
            .map_err(|_| EvalError::builder_already_finished("builder-delete", call_span))?;

        // Return the builder for chaining
        Ok(Arc::clone(&args[0]))
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
        reject_named("builder-finish", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
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
                    args[0].span,
                )
                .into())
            }
        };

        // Take the inner dict, freezing the builder
        let dict = builder
            .finish()
            .map_err(|_| EvalError::builder_already_finished("builder-finish", call_span))?;

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
        reject_named("builder-snapshot", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
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
                    args[0].span,
                )
                .into())
            }
        };

        // Clone the inner dict
        let dict = builder
            .snapshot()
            .map_err(|_| EvalError::builder_already_finished("builder-snapshot", call_span))?;

        ok_val(Value::Dict(dict), call_span)
    })
}

/// `builder-has?`: Check if a key exists in a builder.
/// Takes 2 args: builder, key (Int or String). Returns Bool.
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
        reject_named("builder-has?", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-has?".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // args[1] (key) is pre-forced by force_count
        let key_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-has?".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        };

        // Frozen builder: has? is an error, not a silent false
        if builder.is_frozen() {
            return Err(EvalError::builder_already_finished("builder-has?", call_span).into());
        }

        // Check if the key exists
        let has = builder.has(&key);
        ok_val(Value::Bool(has), call_span)
    })
}

/// `builder-get`: Get a value from a builder by key.
/// Takes 2 args: builder, key (Int or String). Returns the value or errors if key not found.
pub(crate) fn builtin_builder_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("builder-get", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // args[0] (builder) is pre-forced by force_count
        let builder_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get".to_string(),
                    "Builder",
                    other.type_name(),
                    args[0].span,
                )
                .into())
            }
        };

        // args[1] (key) is pre-forced by force_count
        let key_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count");
        let key = match key_val {
            Value::Int(n) => Key::Int(n),
            Value::String {
                ref source,
                start,
                end,
            } => {
                let s = &source[start..end];
                Key::String(s.into())
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builder-get".to_string(),
                    "Int or String (for key)",
                    other.type_name(),
                    args[1].span,
                )
                .into())
            }
        };

        // Frozen builder: get is an error distinct from key-not-found
        if builder.is_frozen() {
            return Err(EvalError::builder_already_finished("builder-get", call_span).into());
        }

        // Get the value
        match builder.get(&key) {
            Some(thunk_id) => {
                let thunk = ctx.thunk_arena.lock().unwrap().get(thunk_id).clone();
                Ok(thunk)
            }
            None => {
                let key_str = match &key {
                    Key::Int(n) => n.to_string(),
                    Key::String(s) => s.to_string(),
                };
                Err(EvalError::key_not_found(&key_str, vec![], call_span).into())
            }
        }
    })
}
