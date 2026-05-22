//! Byte sequence builtins: bytes, bytes-find, bytes-of, bytes-equal?, ct-equal?.

use std::sync::Arc;

use crate::builtins::{expect_one_arg, ok_val};
use crate::error::{EvalError, EvalResult};
use crate::eval;
use crate::value::{bytes_val, BuiltinArgs, Thunk, Value};
use subtle::ConstantTimeEq;

/// `bytes`: Concatenate multiple Bytes values (variadic).
///
/// Takes N arguments, all must be Bytes. Returns concatenated Bytes.
///
/// # Example
///
/// ```llt
/// (bytes b1 b2 b3)
/// ```
pub(crate) fn builtin_bytes(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    if let Some(named_map) = named {
        if !named_map.is_empty() {
            return Err(EvalError::internal(
                "bytes does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
    }

    if args.is_empty() {
        // Empty bytes
        return ok_val(bytes_val(&[]), call_span);
    }

    // Directly concatenate bytes without intermediate storage
    let mut result = Vec::new();

    for arg_thunk in args {
        let val = eval::materialize_sync(arg_thunk, Some(&call_span), &ctx)?;
        match val.as_bytes() {
            Some(bytes) => {
                result.extend_from_slice(bytes);
            }
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "bytes".to_string(),
                    "Bytes",
                    val.type_name(),
                    arg_thunk.span,
                )
                .into());
            }
        }
    }

    ok_val(bytes_val(&result), call_span)
}

/// `bytes-find`: Find the first occurrence of a pattern in a byte sequence.
///
/// Takes 2 args: haystack (Bytes), needle (Bytes).
/// Returns Int index of first occurrence, or -1 if not found.
///
/// # Example
///
/// ```llt
/// (bytes-find haystack needle)  // Returns Int or -1
/// ```
pub(crate) fn builtin_bytes_find(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
    } = ctx_arg;

    if let Some(named_map) = named {
        if !named_map.is_empty() {
            return Err(EvalError::internal(
                "bytes-find does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
    }

    if args.len() != 2 {
        return Err(EvalError::internal(
            format!(
                "bytes-find requires exactly 2 arguments, got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }

    let haystack_val = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let needle_val = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let haystack = match haystack_val.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "bytes-find".to_string(),
                "Bytes",
                haystack_val.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    let needle = match needle_val.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "bytes-find".to_string(),
                "Bytes",
                needle_val.type_name(),
                args[1].span,
            )
            .into());
        }
    };

    // Empty needle: found at position 0 (consistent with str-find behavior)
    if needle.is_empty() {
        return ok_val(Value::Int(0), call_span);
    }

    // Find the first occurrence
    let position = haystack
        .windows(needle.len())
        .position(|window| window == needle);

    let result = match position {
        Some(idx) => idx as i64,
        None => -1,
    };

    ok_val(Value::Int(result), call_span)
}

/// `bytes-of`: Collect integers (0-255) from a Seq or Dict into a Bytes value.
///
/// Takes 1 arg: a Seq or Dict of Int values (0-255).
/// Returns Bytes.
///
/// # Example
///
/// ```llt
/// (bytes-of [72 101 108 108 111])  // "Hello" as bytes
/// ```
pub(crate) fn builtin_bytes_of(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    let val = expect_one_arg("bytes-of", args, named, &ctx, call_span)?;

    let mut bytes = Vec::new();

    match &val {
        Value::Seq { head, tail } => {
            // Iterate the sequence
            let mut current_head = *head;
            let mut current_tail = *tail;

            loop {
                let head_thunk = ctx.get_thunk(current_head);
                let head_val = eval::materialize_sync(&head_thunk, Some(&call_span), &ctx)?;

                match head_val {
                    Value::Int(n) if (0..=255).contains(&n) => {
                        bytes.push(n as u8);
                    }
                    Value::Int(n) => {
                        return Err(EvalError::internal(
                            format!("bytes-of: integer {n} out of range 0-255"),
                            call_span,
                        )
                        .into());
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "bytes-of".to_string(),
                            "Int",
                            head_val.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }

                let tail_thunk = ctx.get_thunk(current_tail);
                let tail_val = eval::materialize_sync(&tail_thunk, Some(&call_span), &ctx)?;

                match tail_val {
                    Value::Dict(map) if map.is_empty() => {
                        // End of sequence
                        break;
                    }
                    Value::Seq { head, tail } => {
                        current_head = head;
                        current_tail = tail;
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "bytes-of".to_string(),
                            "Seq",
                            tail_val.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }
            }
        }
        Value::Dict(map) => {
            // Iterate dict values in insertion order
            for (_key, thunk_id) in map {
                let item_thunk = ctx.get_thunk(*thunk_id);
                let item_val = eval::materialize_sync(&item_thunk, Some(&call_span), &ctx)?;

                match item_val {
                    Value::Int(n) if (0..=255).contains(&n) => {
                        bytes.push(n as u8);
                    }
                    Value::Int(n) => {
                        return Err(EvalError::internal(
                            format!("bytes-of: integer {n} out of range 0-255"),
                            call_span,
                        )
                        .into());
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "bytes-of".to_string(),
                            "Int",
                            item_val.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }
            }
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "bytes-of".to_string(),
                "Seq or Dict",
                val.type_name(),
                call_span,
            )
            .into());
        }
    }

    ok_val(bytes_val(&bytes), call_span)
}

/// `bytes-equal?`: Fast structural equality check for Bytes values.
///
/// Takes 2 args: both Bytes.
/// Returns Bool.
///
/// # Example
///
/// ```llt
/// (bytes-equal? b1 b2)  // true or false
/// ```
pub(crate) fn builtin_bytes_equal(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
    } = ctx_arg;

    if let Some(named_map) = named {
        if !named_map.is_empty() {
            return Err(EvalError::internal(
                "bytes-equal? does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
    }

    if args.len() != 2 {
        return Err(EvalError::internal(
            format!(
                "bytes-equal? requires exactly 2 arguments, got {}",
                args.len()
            ),
            call_span,
        )
        .into());
    }

    let val1 = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let val2 = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let bytes1 = match val1.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "bytes-equal?".to_string(),
                "Bytes",
                val1.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    let bytes2 = match val2.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "bytes-equal?".to_string(),
                "Bytes",
                val2.type_name(),
                args[1].span,
            )
            .into());
        }
    };

    ok_val(Value::Bool(bytes1 == bytes2), call_span)
}

/// `ct-equal?`: Constant-time equality check for Bytes values.
///
/// Takes 2 args: both Bytes.
/// Returns Bool.
///
/// Uses `subtle::ConstantTimeEq` to prevent timing attacks.
/// Returns false for different lengths.
///
/// # Example
///
/// ```llt
/// (ct-equal? secret1 secret2)  // true or false, constant-time
/// ```
pub(crate) fn builtin_ct_equal(ctx_arg: BuiltinArgs) -> EvalResult<Arc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
    } = ctx_arg;

    if let Some(named_map) = named {
        if !named_map.is_empty() {
            return Err(EvalError::internal(
                "ct-equal? does not accept named arguments".to_string(),
                call_span,
            )
            .into());
        }
    }

    if args.len() != 2 {
        return Err(EvalError::internal(
            format!("ct-equal? requires exactly 2 arguments, got {}", args.len()),
            call_span,
        )
        .into());
    }

    let val1 = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let val2 = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let bytes1 = match val1.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "ct-equal?".to_string(),
                "Bytes",
                val1.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    let bytes2 = match val2.as_bytes() {
        Some(bytes) => bytes,
        None => {
            return Err(EvalError::type_mismatch_ctx(
                "ct-equal?".to_string(),
                "Bytes",
                val2.type_name(),
                args[1].span,
            )
            .into());
        }
    };

    // Different lengths: return false (still in constant time per subtle docs)
    if bytes1.len() != bytes2.len() {
        return ok_val(Value::Bool(false), call_span);
    }

    // Constant-time comparison
    let result = bytes1.ct_eq(bytes2);
    ok_val(Value::Bool(result.into()), call_span)
}
