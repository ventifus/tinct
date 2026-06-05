//! Seq primitive builtins: `seq`, `head`, `tail`, `collect`, `seq?`.
//!
//! These are the low-level cons-cell operations for LLT's lazy linked-list
//! sequence type. Extracted from `builtins.rs` to keep that file manageable.
//!
//! All five functions follow the standard `BuiltinFn` signature:
//! `fn(BuiltinArgs) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>>`
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{expect_one_arg, ok_val, reject_named, MAX_COLLECT_SIZE};
use crate::error::{EvalError, EvalResult};
// builtin_collect uses synchronous materialization (spine-strict by design —
// all Seq elements must be forced before constructing the Dict index).
use crate::eval::materialize_sync as materialize;
use crate::value::{make_seq_cons, BuiltinArgs, Key, Thunk, Value};

/// `seq`: Low-level cons constructor for lazy linked-list sequences.
///
/// Creates a `Seq` with the given head and tail. Both args remain as thunks
/// (fully lazy, no materialization). The tail is NOT validated eagerly -- if it
/// eventually materializes to a non-Seq/non-empty-dict, that's an error at
/// materialization time, not construction time.
pub(crate) fn builtin_seq(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("seq", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let head_id = ctx.alloc_thunk(Arc::clone(&args[0]));
        let tail_id = ctx.alloc_thunk(Arc::clone(&args[1]));
        ok_val(make_seq_cons(head_id, tail_id, &ctx), call_span)
    })
}

/// `head`: Extract the first element of a sequence.
///
/// Materializes the argument to verify it's a Seq, then returns the head thunk
/// directly (lazy -- the head is not materialized). Empty dict (terminal value)
/// produces a specific error message.
pub(crate) fn builtin_head(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = expect_one_arg("head", &args, named.as_ref(), &ctx, call_span.clone())?;
        match val {
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                // Materialize the payload dict to extract the "head" key
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = materialize(&payload_thunk, None, &ctx)?;
                match payload_val {
                    Value::Dict(ref dict) => {
                        if let Some(head_id) = dict.get(&Key::String("head".into())) {
                            Ok(ctx.get_thunk(*head_id))
                        } else {
                            Err(EvalError::internal(
                                "Seq.Cons payload missing 'head' key".to_string(),
                                call_span,
                            )
                            .into())
                        }
                    }
                    _ => Err(EvalError::internal(
                        "Seq.Cons payload is not a Dict".to_string(),
                        call_span,
                    )
                    .into()),
                }
            }
            Value::Variant { tag, payload: None } if tag == "Seq.Nil" => {
                Err(EvalError::empty_collection("head".to_string(), call_span).into())
            }
            other => Err(EvalError::type_mismatch_ctx(
                "head".to_string(),
                "Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `tail`: Extract the rest of a sequence.
///
/// Materializes the argument to verify it's a Seq, then returns the tail thunk
/// directly (lazy -- the tail is not materialized). Empty dict (terminal value)
/// produces a specific error message.
pub(crate) fn builtin_tail(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = expect_one_arg("tail", &args, named.as_ref(), &ctx, call_span.clone())?;
        match val {
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                // Materialize the payload dict to extract the "tail" key
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = materialize(&payload_thunk, None, &ctx)?;
                match payload_val {
                    Value::Dict(ref dict) => {
                        if let Some(tail_id) = dict.get(&Key::String("tail".into())) {
                            Ok(ctx.get_thunk(*tail_id))
                        } else {
                            Err(EvalError::internal(
                                "Seq.Cons payload missing 'tail' key".to_string(),
                                call_span,
                            )
                            .into())
                        }
                    }
                    _ => Err(EvalError::internal(
                        "Seq.Cons payload is not a Dict".to_string(),
                        call_span,
                    )
                    .into()),
                }
            }
            Value::Variant { tag, payload: None } if tag == "Seq.Nil" => {
                Err(EvalError::empty_collection("tail".to_string(), call_span).into())
            }
            other => Err(EvalError::type_mismatch_ctx(
                "tail".to_string(),
                "Seq",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `collect`: Materialize a Seq into a dict with integer keys.
///
/// Iterates through the sequence spine, collecting head thunks into an IndexMap
/// with keys 0, 1, 2, ... Head elements remain as thunks (lazy). Each tail is materialized to check
/// if it's another Seq or the terminal value (empty dict). Terminal condition:
/// tail materializes to an empty dict (Dict with 0 entries). If tail is anything
/// other than Seq or empty dict, error. Empty dict as input returns empty dict.
pub(crate) fn builtin_collect(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        // Capture arg span before expect_one_arg consumes args.
        let arg_span = args
            .first()
            .map(|a| a.span.clone())
            .unwrap_or(call_span.clone());
        let val = expect_one_arg("collect", &args, named.as_ref(), &ctx, call_span.clone())?;

        // Handle Seq.Nil (empty sequence) as input
        if let Value::Variant { tag, payload: None } = &val {
            if tag == "Seq.Nil" {
                return ok_val(Value::Dict(IndexMap::new()), call_span);
            }
        }

        // Check that we have a Seq.Cons
        if !matches!(
            val,
            Value::Variant {
                ref tag,
                payload: Some(_)
            } if tag == "Seq.Cons"
        ) {
            return Err(EvalError::type_mismatch_ctx(
                "collect".to_string(),
                "Seq",
                val.type_name(),
                arg_span,
            )
            .with_materialization_span(call_span)
            .into());
        }

        let mut map = IndexMap::new();
        let mut index = 0i64;
        let mut current = val;

        loop {
            match current {
                Value::Variant {
                    tag,
                    payload: Some(payload_id),
                } if tag == "Seq.Cons" => {
                    // Materialize the payload dict to extract head and tail
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_val = materialize(&payload_thunk, None, &ctx)?;
                    match payload_val {
                        Value::Dict(ref dict) => {
                            let head_id =
                                dict.get(&Key::String("head".into())).ok_or_else(|| {
                                    EvalError::internal(
                                        "Seq.Cons payload missing 'head' key".to_string(),
                                        call_span.clone(),
                                    )
                                })?;
                            let tail_id =
                                dict.get(&Key::String("tail".into())).ok_or_else(|| {
                                    EvalError::internal(
                                        "Seq.Cons payload missing 'tail' key".to_string(),
                                        call_span.clone(),
                                    )
                                })?;

                            // Insert head thunk (not materialized -- stay lazy)
                            map.insert(Key::Int(index), *head_id);
                            index = index.checked_add(1).ok_or_else(|| {
                                EvalError::integer_overflow(
                                    "collect".to_string(),
                                    call_span.clone(),
                                )
                            })?;

                            // Check collection size limit
                            if index as usize >= MAX_COLLECT_SIZE {
                                return Err(EvalError::resource_limit_exceeded(
                                    format!(
                                        "collect: exceeded maximum collection size ({}). Use $take to limit infinite sequences before collecting.",
                                        MAX_COLLECT_SIZE
                                    ),
                                    call_span,
                                )
                                .into());
                            }

                            // Materialize tail to check if we should continue
                            let tail_thunk = ctx.get_thunk(*tail_id);
                            current = materialize(&tail_thunk, None, &ctx)?;
                        }
                        _ => {
                            return Err(EvalError::internal(
                                "Seq.Cons payload is not a Dict".to_string(),
                                call_span,
                            )
                            .into())
                        }
                    }
                }
                Value::Variant { tag, payload: None } if tag == "Seq.Nil" => {
                    // Terminal: Seq.Nil
                    break;
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "collect".to_string(),
                        "Seq",
                        other.type_name(),
                        arg_span,
                    )
                    .with_materialization_span(call_span)
                    .into());
                }
            }
        }

        ok_val(Value::Dict(map), call_span)
    })
}

// seq? was removed from builtins_seq_prim.rs in the type-predicates-to-tinct sprint.
// It is now implemented in stdlib/prelude.llt as:
//   seq?: [fn [let x] [match x Seq: true _: false]]
