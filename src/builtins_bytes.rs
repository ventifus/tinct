//! Byte sequence builtins: bytes, bytes-concat, bytes-find, bytes-of, bytes-equal?, ct-equal?, builtin-encode,
//! builtin-bytes-get, builtin-bytes-slice.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use crate::builtins::{expect_one_arg, ok_val};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
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
pub(crate) fn builtin_bytes(
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

        for &tid in &args {
            let arg_thunk = ctx.get_thunk(tid);
            let val = materialize(&arg_thunk, Some(&call_span), &ctx).await?; // H3: loop materialize (iterating bytes args)
            match val.as_bytes() {
                Some(bytes) => {
                    result.extend_from_slice(bytes);
                }
                None => {
                    return Err(EvalError::type_mismatch_ctx(
                        "bytes".to_string(),
                        "Bytes",
                        val.type_name(),
                        arg_thunk.span.clone(),
                    )
                    .into());
                }
            }
        }

        ok_val(bytes_val(&result), call_span)
    })
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
pub(crate) fn builtin_bytes_find(
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

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let haystack_val = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let needle_val = thunk1
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let haystack = match haystack_val.as_bytes() {
            Some(bytes) => bytes,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "bytes-find".to_string(),
                    "Bytes",
                    haystack_val.type_name(),
                    thunk0.span.clone(),
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
                    thunk1.span.clone(),
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
    })
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
pub(crate) fn builtin_bytes_of(
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

        let val = expect_one_arg("bytes-of", &args, named.as_ref(), &ctx, call_span.clone())?;

        let mut bytes = Vec::new();

        match val {
            Value::Dict(map) => {
                // Iterate dict values in insertion order — map is owned (Send)
                for (_key, thunk_id) in map {
                    let item_thunk = ctx.get_thunk(thunk_id);
                    let item_val = materialize(&item_thunk, Some(&call_span), &ctx).await?;

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
    })
}

/// `bytes-concat`: Concatenate exactly two Bytes values.
///
/// Takes 2 args: both must be Bytes. Returns concatenated Bytes.
/// For variadic concatenation of N byte sequences, use `bytes`.
///
/// # Example
///
/// ```llt
/// [builtin-bytes-concat b1 b2]  // → Bytes (b1 followed by b2)
/// ```
pub(crate) fn builtin_bytes_concat(
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

        if let Some(named_map) = named {
            if !named_map.is_empty() {
                return Err(EvalError::internal(
                    "bytes-concat does not accept named arguments".to_string(),
                    call_span,
                )
                .into());
            }
        }

        if args.len() != 2 {
            return Err(EvalError::internal(
                format!(
                    "bytes-concat requires exactly 2 arguments, got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let val1 = thunk0
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let val2 = thunk1
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");

        let bytes1 = match val1.as_bytes() {
            Some(bytes) => bytes,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "bytes-concat".to_string(),
                    "Bytes",
                    val1.type_name(),
                    thunk0.span.clone(),
                )
                .into());
            }
        };

        let bytes2 = match val2.as_bytes() {
            Some(bytes) => bytes,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "bytes-concat".to_string(),
                    "Bytes",
                    val2.type_name(),
                    thunk1.span.clone(),
                )
                .into());
            }
        };

        let mut result = Vec::with_capacity(bytes1.len() + bytes2.len());
        result.extend_from_slice(bytes1);
        result.extend_from_slice(bytes2);

        ok_val(bytes_val(&result), call_span)
    })
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
pub(crate) fn builtin_bytes_equal(
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

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let val1 = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let val2 = thunk1
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let bytes1 = match val1.as_bytes() {
            Some(bytes) => bytes,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "bytes-equal?".to_string(),
                    "Bytes",
                    val1.type_name(),
                    thunk0.span.clone(),
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
                    thunk1.span.clone(),
                )
                .into());
            }
        };

        ok_val(Value::Int(if bytes1 == bytes2 { 1 } else { 0 }), call_span)
    })
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
pub(crate) fn builtin_ct_equal(
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

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let val1 = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let val2 = thunk1
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let bytes1 = match val1.as_bytes() {
            Some(bytes) => bytes,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "ct-equal?".to_string(),
                    "Bytes",
                    val1.type_name(),
                    thunk0.span.clone(),
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
                    thunk1.span.clone(),
                )
                .into());
            }
        };

        // Different lengths: return false (still in constant time per subtle docs)
        if bytes1.len() != bytes2.len() {
            return ok_val(Value::Int(0), call_span);
        }

        // Constant-time comparison
        let result = bytes1.ct_eq(bytes2);
        ok_val(Value::Int(if result.into() { 1 } else { 0 }), call_span)
    })
}

/// `builtin-encode`: Encode a numeric value as Bytes in the specified byte order/format.
///
/// Takes 2 args:
/// - `format`: a Variant tag from the `ByteOrder` type (e.g., `ByteOrder.Int64BE`,
///   `ByteOrder.Float32LE`, `ByteOrder.UInt8`)
/// - `value`: Int or Float to encode
///
/// The tag name (from `tag-of format`) determines the encoding:
/// - `ByteOrder.Int8`      — 1 byte signed
/// - `ByteOrder.UInt8`     — 1 byte unsigned
/// - `ByteOrder.Int16LE` / `ByteOrder.Int16BE`   — 2 bytes signed
/// - `ByteOrder.UInt16LE` / `ByteOrder.UInt16BE`  — 2 bytes unsigned
/// - `ByteOrder.Int32LE` / `ByteOrder.Int32BE`   — 4 bytes signed
/// - `ByteOrder.UInt32LE` / `ByteOrder.UInt32BE`  — 4 bytes unsigned
/// - `ByteOrder.Int64LE` / `ByteOrder.Int64BE`   — 8 bytes signed
/// - `ByteOrder.UInt64LE` / `ByteOrder.UInt64BE`  — 8 bytes unsigned
/// - `ByteOrder.Float32LE` / `ByteOrder.Float32BE` — 4 bytes IEEE 754
/// - `ByteOrder.Float64LE` / `ByteOrder.Float64BE` — 8 bytes IEEE 754
///
/// Returns Bytes of the appropriate length.
pub(crate) fn builtin_encode(
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

        if let Some(named_map) = named {
            if !named_map.is_empty() {
                return Err(EvalError::internal(
                    "builtin-encode does not accept named arguments".to_string(),
                    call_span,
                )
                .into());
            }
        }

        if args.len() != 2 {
            return Err(EvalError::internal(
                format!(
                    "builtin-encode requires exactly 2 arguments (format, value), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let fmt_val = thunk0
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let num_val = thunk1
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");

        // Extract the tag from the format variant
        let tag = match &fmt_val {
            Value::Variant { tycon, ctor, .. } => format!("{}.{}", tycon, ctor),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-encode".to_string(),
                    "ByteOrder variant",
                    fmt_val.type_name(),
                    thunk0.span.clone(),
                )
                .into());
            }
        };

        // Extract numeric value
        let as_i64 = match &num_val {
            Value::Int(n) => Some(*n),
            _ => None,
        };
        let as_f64 = match &num_val {
            Value::Float(f) => Some(*f),
            Value::Int(n) => Some(*n as f64),
            _ => None,
        };

        let bytes: Vec<u8> = match tag.as_str() {
            "ByteOrder.Int8" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                vec![n as i8 as u8]
            }
            "ByteOrder.UInt8" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                vec![n as u8]
            }
            "ByteOrder.Int16LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as i16).to_le_bytes().to_vec()
            }
            "ByteOrder.Int16BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as i16).to_be_bytes().to_vec()
            }
            "ByteOrder.UInt16LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u16).to_le_bytes().to_vec()
            }
            "ByteOrder.UInt16BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u16).to_be_bytes().to_vec()
            }
            "ByteOrder.Int32LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as i32).to_le_bytes().to_vec()
            }
            "ByteOrder.Int32BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as i32).to_be_bytes().to_vec()
            }
            "ByteOrder.UInt32LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u32).to_le_bytes().to_vec()
            }
            "ByteOrder.UInt32BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u32).to_be_bytes().to_vec()
            }
            "ByteOrder.Int64LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                n.to_le_bytes().to_vec()
            }
            "ByteOrder.Int64BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                n.to_be_bytes().to_vec()
            }
            "ByteOrder.UInt64LE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u64).to_le_bytes().to_vec()
            }
            "ByteOrder.UInt64BE" => {
                let n = as_i64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (n as u64).to_be_bytes().to_vec()
            }
            "ByteOrder.Float32LE" => {
                let f = as_f64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Float or Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (f as f32).to_le_bytes().to_vec()
            }
            "ByteOrder.Float32BE" => {
                let f = as_f64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Float or Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                (f as f32).to_be_bytes().to_vec()
            }
            "ByteOrder.Float64LE" => {
                let f = as_f64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Float or Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                f.to_le_bytes().to_vec()
            }
            "ByteOrder.Float64BE" => {
                let f = as_f64.ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "builtin-encode".to_string(),
                        "Float or Int",
                        num_val.type_name(),
                        thunk1.span.clone(),
                    )
                })?;
                f.to_be_bytes().to_vec()
            }
            other => {
                return Err(EvalError::user_error(
                    format!(
                        "builtin-encode: unknown ByteOrder tag '{}'. Expected one of: \
                         ByteOrder.Int8, ByteOrder.UInt8, ByteOrder.Int16LE, ByteOrder.Int16BE, \
                         ByteOrder.UInt16LE, ByteOrder.UInt16BE, ByteOrder.Int32LE, ByteOrder.Int32BE, \
                         ByteOrder.UInt32LE, ByteOrder.UInt32BE, ByteOrder.Int64LE, ByteOrder.Int64BE, \
                         ByteOrder.UInt64LE, ByteOrder.UInt64BE, ByteOrder.Float32LE, ByteOrder.Float32BE, \
                         ByteOrder.Float64LE, ByteOrder.Float64BE",
                        other
                    ),
                    call_span,
                )
                .into());
            }
        };

        ok_val(bytes_val(&bytes), call_span)
    })
}

/// `builtin-bytes-get`: Return the byte at index `i` as an Int (0–255).
///
/// Takes 2 args:
/// - `i`: Int — zero-based byte index
/// - `b`: Bytes — the byte sequence
///
/// Returns `Int` in range 0–255. Errors if `i` is out of bounds.
/// O(1) — direct slice index into the underlying `Rc<[u8]>`.
pub(crate) fn builtin_bytes_get(
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

        if let Some(ref named_map) = named {
            if !named_map.is_empty() {
                return Err(EvalError::internal(
                    "builtin-bytes-get does not accept named arguments".to_string(),
                    call_span,
                )
                .into());
            }
        }

        if args.len() != 2 {
            return Err(EvalError::internal(
                format!(
                    "builtin-bytes-get requires exactly 2 arguments (i, b), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let i_val = thunk0
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let b_val = thunk1
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");

        let i = match i_val {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-bytes-get".to_string(),
                    "Int",
                    other.type_name(),
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        let bytes = match b_val.as_bytes() {
            Some(b) => b,
            None => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-bytes-get".to_string(),
                    "Bytes",
                    b_val.type_name(),
                    thunk1.span.clone(),
                )
                .into())
            }
        };

        let len = bytes.len() as i64;
        if i < 0 || i >= len {
            return Err(EvalError::user_error(
                format!(
                    "builtin-bytes-get: index {} out of bounds for Bytes of length {}",
                    i, len
                ),
                call_span,
            )
            .into());
        }

        let byte_val = bytes[i as usize] as i64;
        ok_val(Value::Int(byte_val), call_span)
    })
}

/// `builtin-bytes-slice`: Return a sub-slice of a Bytes value as a new Bytes.
///
/// Takes 3 args:
/// - `b`: Bytes — the source byte sequence
/// - `start`: Int — zero-based start index (inclusive)
/// - `len`: Int — number of bytes to include
///
/// Returns `Bytes`. Errors if `start` or `start + len` is out of bounds,
/// or if `len` is negative.
/// O(1) — shares the underlying `Rc<[u8]>` without copying, just adjusts offsets.
pub(crate) fn builtin_bytes_slice(
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

        if let Some(ref named_map) = named {
            if !named_map.is_empty() {
                return Err(EvalError::internal(
                    "builtin-bytes-slice does not accept named arguments".to_string(),
                    call_span,
                )
                .into());
            }
        }

        if args.len() != 3 {
            return Err(EvalError::internal(
                format!(
                    "builtin-bytes-slice requires exactly 3 arguments (b, start, len), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let thunk2 = ctx.get_thunk(args[2]);
        let b_val = thunk0
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let start_val = thunk1
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");
        let len_val = thunk2
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness");

        let (source, base_start, base_end) = match &b_val {
            Value::Bytes { source, start, end } => (std::rc::Rc::clone(source), *start, *end),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-bytes-slice".to_string(),
                    "Bytes",
                    other.type_name(),
                    thunk0.span.clone(),
                )
                .into())
            }
        };

        let start_i = match start_val {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-bytes-slice".to_string(),
                    "Int",
                    other.type_name(),
                    thunk1.span.clone(),
                )
                .into())
            }
        };

        let len_i = match len_val {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-bytes-slice".to_string(),
                    "Int",
                    other.type_name(),
                    thunk2.span.clone(),
                )
                .into())
            }
        };

        let total_len = (base_end - base_start) as i64;

        if len_i < 0 {
            return Err(EvalError::user_error(
                format!(
                    "builtin-bytes-slice: len must be non-negative, got {}",
                    len_i
                ),
                call_span,
            )
            .into());
        }
        if start_i < 0 || start_i > total_len {
            return Err(EvalError::user_error(
                format!(
                    "builtin-bytes-slice: start {} out of bounds for Bytes of length {}",
                    start_i, total_len
                ),
                call_span,
            )
            .into());
        }
        let end_i = start_i + len_i;
        if end_i > total_len {
            return Err(EvalError::user_error(
                format!(
                    "builtin-bytes-slice: start {} + len {} = {} exceeds Bytes length {}",
                    start_i, len_i, end_i, total_len
                ),
                call_span,
            )
            .into());
        }

        // Zero-copy subslice: share the same Rc<[u8]>, adjust offsets only.
        let new_start = base_start + start_i as usize;
        let new_end = base_start + end_i as usize;
        ok_val(
            Value::Bytes {
                source,
                start: new_start,
                end: new_end,
            },
            call_span,
        )
    })
}
