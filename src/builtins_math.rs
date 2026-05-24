//! Arithmetic, comparison, and control-flow builtins: `+`, `-`, `*`, `/`, `=`, `<`, `if`.
//! `builtin-add/sub/mul/div` are pure Int/Float primitives — no typeclass dispatch. Dispatch belongs at the operator level.
//!
//! These builtins operate on numeric and boolean values. They are all inherently
//! materializing because they must inspect operand values to compute results.
//!
//! - Arithmetic (`+`, `-`, `*`, `/`): auto-promote Int/Float operands
//! - Comparison (`=`, `<`): cross-type Int/Float promotion; String/Bool same-type comparison
//! - Control flow (`if`): materializes only the condition, returns the chosen branch thunk
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use std::rc::Rc;

use crate::ast::Span;
use crate::builtins::{check_float_result, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize_sync as materialize, EvalContext};
use crate::eval_call::{invoke_function_sync as invoke_function, CallContext};
use crate::value::Key;
use crate::value::{BuiltinArgs, Thunk, Value};

/// Maximum safe integer for Int→Float promotion (2^53).
/// Integers with |n| > MAX_SAFE_INT lose precision when cast to f64.
const MAX_SAFE_INT: i64 = 9007199254740992;

/// Check if an Int→Float promotion would lose precision.
/// Returns Err if |n| > 2^53, suggesting explicit [float n] cast.
/// Used by `=` and `<` for cross-type Int/Float comparison.
fn check_int_to_float_precision(n: i64, span: crate::ast::Span) -> EvalResult<()> {
    if n.abs() > MAX_SAFE_INT {
        return Err(EvalError::user_error(
            format!(
                "implicit Int→Float promotion loses precision for {}; use [float {}] for intentional cast",
                n, n
            ),
            span,
        )
        .into());
    }
    Ok(())
}

/// Try to dispatch a binary operation to a typeclass instance method.
///
/// Looks up `(class_name, type_tags)` in the runtime instance registry. If a
/// matching instance dict is found, extracts `method_name` from that dict and
/// calls it with `arg_thunks` as positional arguments.
///
/// Returns:
/// - `Ok(Some(thunk))` — dispatch succeeded; `thunk` is the method's result
/// - `Ok(None)` — no instance registered for these types (caller handles fallthrough)
/// - `Err(e)` — instance found but method call (or method materialization) failed
///
/// **Laziness note**: `arg_thunks` are passed as-is (already-allocated `Arc<Thunk>`)
/// without re-materializing. The called method decides what to force.
///
/// **Non-recursion guarantee**: arithmetic operators call `builtin-add/mul/…` (pure
/// primitives) which hit the Int/Float fast path and never dispatch. Equatable/Comparable
/// instances call `builtin-eq/lt` (pure) similarly. No infinite recursion is possible.
async fn try_dispatch_method(
    class_name: &'static str,
    method_name: &str,
    type_tags: Vec<String>,
    arg_thunks: Vec<Arc<Thunk>>,
    ctx: Arc<EvalContext>,
    call_span: Span,
) -> EvalResult<Option<Arc<Thunk>>> {
    // Fast-path: skip registry lookup if the class has no instances at all.
    // `registered_classes` is an O(1) HashSet updated in sync with `instance_registry`.
    {
        let state = ctx.state.lock().unwrap();
        if !state.registered_classes.contains(class_name) {
            return Ok(None);
        }
    }

    // Look up the instance dict for this (class, type_tags) pair.
    let instance_thunk = {
        let state = ctx.state.lock().unwrap();
        state
            .instance_registry
            .get(&(class_name, type_tags.clone()))
            .cloned()
    };

    let instance_thunk = match instance_thunk {
        Some(t) => t,
        None => return Ok(None),
    };

    // Materialize the instance dict.
    let instance_val = materialize(&instance_thunk, Some(&call_span), &ctx)?;

    // The instance dict has method names as string keys.
    let method_key = Key::String(Rc::from(method_name));
    let method_id = match &instance_val {
        Value::Dict(map) => match map.get(&method_key) {
            Some(id) => *id,
            None => {
                // Instance registered but method not present — should not happen with
                // well-formed prelude instances. Return NoInstance to surface the gap.
                return Err(EvalError::no_instance(class_name, type_tags, call_span).into());
            }
        },
        _ => {
            return Err(EvalError::internal(
                format!(
                    "instance registry entry for {} is not a Dict (got {})",
                    class_name,
                    instance_val.type_name()
                ),
                call_span,
            )
            .into());
        }
    };

    // Resolve the method ThunkId to Arc<Thunk> via the arena.
    let method_thunk = ctx.get_thunk(method_id);

    // Materialize the method itself to dispatch (Function or Builtin).
    let method_val = materialize(&method_thunk, Some(&call_span), &ctx)?;

    // Call the method with the original arg thunks.
    let result_thunk = match &method_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => invoke_function(&CallContext {
            params,
            body,
            closure_env,
            positional: &arg_thunks,
            named: None,
            default_env: closure_env,
            call_span,
            origin: Some(Arc::from(
                format!("[{class_name}.{method_name} ...]").as_str(),
            )),
            ctx: &ctx,
        })?,
        Value::Builtin(def) => {
            let dispatch_args: Vec<Arc<Thunk>> = arg_thunks.to_vec();
            (def.func)(BuiltinArgs {
                args: dispatch_args,
                named: None,
                call_span,
                ctx: Arc::clone(&ctx),
            })
            .await?
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                format!("{class_name}.{method_name}"),
                "Function or Builtin",
                method_val.type_name(),
                method_thunk.span,
            )
            .into());
        }
    };

    Ok(Some(result_thunk))
}

/// `builtin-add`: Pure Int/Float addition primitive. No typeclass dispatch.
/// Dispatch for user-defined numeric types happens at the `+` operator level.
/// Int + Int -> Int (checked), any Float operand -> Float (auto-promotion).
pub(crate) fn builtin_add(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("+", named.as_ref(), call_span)?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_add(*b)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span)))
                .ok_or_else(|| EvalError::integer_overflow("+".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a + b, "+", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                check_float_result((*a as f64) + b, "+", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                check_float_result(a + (*b as f64), "+", call_span)
            }
            _ => {
                // Non-Int/Float: try dispatching to an Addable instance.
                // type_tags uses num_determining=2 (a,b)→c functional dependency.
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                if let Some(result) =
                    try_dispatch_method("Addable", "+", type_tags.clone(), args, ctx, call_span)
                        .await?
                {
                    Ok(result)
                } else {
                    Err(EvalError::no_instance("Addable", type_tags, call_span).into())
                }
            }
        }
    })
}

/// `builtin-sub`: Pure Int/Float subtraction primitive. No typeclass dispatch.
pub(crate) fn builtin_sub(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("-", named.as_ref(), call_span)?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_sub(*b)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span)))
                .ok_or_else(|| EvalError::integer_overflow("-".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a - b, "-", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                check_float_result((*a as f64) - b, "-", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                check_float_result(a - (*b as f64), "-", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                if let Some(result) = try_dispatch_method(
                    "Subtractable",
                    "-",
                    type_tags.clone(),
                    args,
                    ctx,
                    call_span,
                )
                .await?
                {
                    Ok(result)
                } else {
                    Err(EvalError::no_instance("Subtractable", type_tags, call_span).into())
                }
            }
        }
    })
}

/// `builtin-mul`: Pure Int/Float multiplication primitive. No typeclass dispatch.
pub(crate) fn builtin_mul(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("*", named.as_ref(), call_span)?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_mul(*b)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span)))
                .ok_or_else(|| EvalError::integer_overflow("*".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a * b, "*", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                check_float_result((*a as f64) * b, "*", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                check_float_result(a * (*b as f64), "*", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                if let Some(result) = try_dispatch_method(
                    "Multipliable",
                    "*",
                    type_tags.clone(),
                    args,
                    ctx,
                    call_span,
                )
                .await?
                {
                    Ok(result)
                } else {
                    Err(EvalError::no_instance("Multipliable", type_tags, call_span).into())
                }
            }
        }
    })
}

/// `builtin-div`: Pure Int/Float division primitive. No typeclass dispatch.
/// Always returns Float (even Int / Int). Division by zero is an error.
pub(crate) fn builtin_div_float(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("/", named.as_ref(), call_span)?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => {
                if *b == 0 {
                    return Err(
                        EvalError::user_error("division by zero".to_string(), call_span).into(),
                    );
                }
                check_float_result(*a as f64 / *b as f64, "/", call_span)
            }
            (Value::Float(a), Value::Float(b)) => check_float_result(a / b, "/", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                check_float_result((*a as f64) / b, "/", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                check_float_result(a / (*b as f64), "/", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                if let Some(result) =
                    try_dispatch_method("Divisible", "/", type_tags.clone(), args, ctx, call_span)
                        .await?
                {
                    Ok(result)
                } else {
                    Err(EvalError::no_instance("Divisible", type_tags, call_span).into())
                }
            }
        }
    })
}

/// `=`: Equality comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison
/// promotes Int to Float. Dict/Function/Builtin are never equal (returns false,
/// not an error).
/// Inherently materializing: must inspect values to determine equality.
pub(crate) fn builtin_eq(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("=", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Fast paths for built-in types are handled directly. Non-Int/Float/String/Bool/Variant/Dict
        // types fall through to Equatable instance dispatch.
        // NOTE: Int/Float/String/Bool fast paths MUST come before dispatch. This prevents
        // infinite recursion: EquatableInt.eq calls [builtin-eq a b] → hits (Int,Int) fast path
        // → returns immediately without dispatch. Safe.
        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Helper to compute structural equality (used for Dict/Variant arms below).
        // Sync helper: uses materialize (= materialize_sync) throughout. No async.
        fn values_eq_impl(
            left: &Value,
            right: &Value,
            ctx: &Arc<EvalContext>,
            call_span: Span,
            visited: &mut std::collections::HashSet<(usize, usize)>,
        ) -> EvalResult<bool> {
            use crate::builtins::require_dict;

            match (left, right) {
                (Value::Int(a), Value::Int(b)) => Ok(a == b),
                (Value::Float(a), Value::Float(b)) => Ok(a == b),
                (
                    Value::String {
                        source: source_a,
                        start: start_a,
                        end: end_a,
                    },
                    Value::String {
                        source: source_b,
                        start: start_b,
                        end: end_b,
                    },
                ) => Ok(source_a[*start_a..*end_a] == source_b[*start_b..*end_b]),
                (Value::Bool(a), Value::Bool(b)) => Ok(a == b),
                // Cross-type: Int/Float promotion
                (Value::Int(a), Value::Float(b)) => {
                    check_int_to_float_precision(*a, call_span)?;
                    Ok((*a as f64) == *b)
                }
                (Value::Float(a), Value::Int(b)) => {
                    check_int_to_float_precision(*b, call_span)?;
                    Ok(*a == (*b as f64))
                }
                // Variant: equal if tags match and payloads match (recursive comparison)
                (
                    Value::Variant {
                        tag: tag_a,
                        payload: payload_a,
                    },
                    Value::Variant {
                        tag: tag_b,
                        payload: payload_b,
                    },
                ) => {
                    if tag_a != tag_b {
                        return Ok(false);
                    }
                    match (payload_a, payload_b) {
                        (None, None) => Ok(true),
                        (Some(p1_id), Some(p2_id)) => {
                            let p1_thunk = ctx.get_thunk(*p1_id);
                            let p2_thunk = ctx.get_thunk(*p2_id);
                            let p1_val = materialize(&p1_thunk, Some(&call_span), ctx)?;
                            let p2_val = materialize(&p2_thunk, Some(&call_span), ctx)?;
                            // Recurse with visited set threaded through
                            values_eq_impl(&p1_val, &p2_val, ctx, call_span, visited)
                        }
                        _ => Ok(false),
                    }
                }
                // Dict: structural equality with cycle detection
                (Value::Dict(_), Value::Dict(_)) | (Value::Overlay(..), Value::Overlay(..)) => {
                    // Get the dicts (flattening Overlay if necessary)
                    let left_map = require_dict("=", left.clone(), call_span, ctx, call_span)?;
                    let right_map = require_dict("=", right.clone(), call_span, ctx, call_span)?;

                    // Check pointer identity cycle detection
                    let left_ptr = left as *const Value as usize;
                    let right_ptr = right as *const Value as usize;
                    let pair = (left_ptr, right_ptr);
                    if visited.contains(&pair) {
                        // Already visiting this pair - treat as equal (structural coinduction)
                        return Ok(true);
                    }
                    visited.insert(pair);

                    // Compare keys (order-insensitive)
                    if left_map.len() != right_map.len() {
                        visited.remove(&pair);
                        return Ok(false);
                    }

                    // Extract and sort keys for canonical comparison
                    let mut left_keys: Vec<_> = left_map.keys().collect();
                    let mut right_keys: Vec<_> = right_map.keys().collect();
                    let key_cmp = |a: &&Key, b: &&Key| match (a, b) {
                        (Key::Int(x), Key::Int(y)) => x.cmp(y),
                        (Key::String(x), Key::String(y)) => x.cmp(y),
                        (Key::Int(_), Key::String(_)) => std::cmp::Ordering::Less,
                        (Key::String(_), Key::Int(_)) => std::cmp::Ordering::Greater,
                    };
                    left_keys.sort_by(key_cmp);
                    right_keys.sort_by(key_cmp);

                    if left_keys != right_keys {
                        visited.remove(&pair);
                        return Ok(false);
                    }

                    // Compare values for each key - RECURSIVELY with SAME visited set
                    for key in left_keys {
                        let left_val_id = left_map.get(key).unwrap();
                        let right_val_id = right_map.get(key).unwrap();

                        let left_thunk = ctx.get_thunk(*left_val_id);
                        let right_thunk = ctx.get_thunk(*right_val_id);

                        let left_val = materialize(&left_thunk, Some(&call_span), ctx)?;
                        let right_val = materialize(&right_thunk, Some(&call_span), ctx)?;

                        // Recurse with visited set threaded through
                        if !values_eq_impl(&left_val, &right_val, ctx, call_span, visited)? {
                            visited.remove(&pair);
                            return Ok(false);
                        }
                    }

                    visited.remove(&pair);
                    Ok(true)
                }
                // Function, Builtin, or cross-type incompatibility
                _ => Ok(false),
            }
        }

        let result = match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a == b,
            (Value::Float(a), Value::Float(b)) => a == b,
            (
                Value::String {
                    source: source_a,
                    start: start_a,
                    end: end_a,
                },
                Value::String {
                    source: source_b,
                    start: start_b,
                    end: end_b,
                },
            ) => source_a[*start_a..*end_a] == source_b[*start_b..*end_b],
            (Value::Bool(a), Value::Bool(b)) => a == b,
            // Cross-type: Int/Float promotion via `as f64` cast.
            // Precision guard: integers with |n| > 2^53 trigger an error, suggesting
            // explicit [float n] cast. This prevents non-transitive equality bugs
            // (doc/11-stdlib.md §Equality P3).
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                (*a as f64) == *b
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                *a == (*b as f64)
            }
            // Variant: equal if tags match and payloads match (recursive comparison)
            (
                Value::Variant {
                    tag: tag_a,
                    payload: payload_a,
                },
                Value::Variant {
                    tag: tag_b,
                    payload: payload_b,
                },
            ) => {
                if tag_a != tag_b {
                    false
                } else {
                    match (payload_a, payload_b) {
                        (None, None) => true,
                        (Some(p1_id), Some(p2_id)) => {
                            // Resolve ThunkIds to Arc<Thunk> via arena
                            let p1_thunk = ctx.get_thunk(*p1_id);
                            let p2_thunk = ctx.get_thunk(*p2_id);
                            // Recurse by calling builtin_eq — inside async block so .await is valid
                            let recursive_args = vec![Arc::clone(&p1_thunk), Arc::clone(&p2_thunk)];
                            let result_thunk = builtin_eq(BuiltinArgs {
                                args: recursive_args,
                                named: None,
                                call_span,
                                ctx: Arc::clone(&ctx),
                            })
                            .await?;
                            let result_val = materialize(&result_thunk, Some(&call_span), &ctx)?;
                            match result_val {
                                Value::Bool(b) => b,
                                _ => unreachable!("builtin_eq always returns Bool"),
                            }
                        }
                        _ => false, // One has payload, other doesn't
                    }
                }
            }
            // Dict: structural equality (order-insensitive key comparison, recursive value comparison)
            (Value::Dict(_), Value::Dict(_)) | (Value::Overlay(..), Value::Overlay(..)) => {
                let mut visited = std::collections::HashSet::new();
                values_eq_impl(&left, &right, &ctx, call_span, &mut visited)?
            }
            // For types not handled above (e.g. user-defined opaque wrappers), try dispatching
            // to an Equatable instance. Equatable uses num_determining=1 (single type param).
            // If no instance is registered, fall back to `false` (preserving prior behavior).
            _ => {
                let type_tags = vec![left.type_name().to_string()];
                match try_dispatch_method(
                    "Equatable",
                    "eq",
                    type_tags,
                    args,
                    Arc::clone(&ctx),
                    call_span,
                )
                .await?
                {
                    Some(result_thunk) => {
                        // The instance method must return a Bool.
                        let val = materialize(&result_thunk, Some(&call_span), &ctx)?;
                        match val {
                            Value::Bool(b) => b,
                            _ => {
                                return Err(EvalError::type_mismatch_ctx(
                                    "Equatable.eq".to_string(),
                                    "Bool",
                                    val.type_name(),
                                    call_span,
                                )
                                .into())
                            }
                        }
                    }
                    // No Equatable instance: heterogeneous/unknown types are not equal.
                    None => false,
                }
            }
        };
        ok_val(Value::Bool(result), call_span)
    })
}

/// `<`: Less-than comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison promotes
/// Int to Float. String comparison is lexicographic. Bool: false < true.
/// Incompatible types (e.g. Int vs String) produce a type error.
/// Inherently materializing: must inspect values to determine ordering.
pub(crate) fn builtin_lt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("<", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Int/Float/String/Bool fast paths are handled directly. Other types fall through
        // to Comparable instance dispatch. ComparableInt.lt calls [builtin-lt a b] which hits
        // the (Int,Int) fast path — no infinite recursion.
        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let result = match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a < b,
            (Value::Float(a), Value::Float(b)) => a < b,
            (
                Value::String {
                    source: source_a,
                    start: start_a,
                    end: end_a,
                },
                Value::String {
                    source: source_b,
                    start: start_b,
                    end: end_b,
                },
            ) => source_a[*start_a..*end_a] < source_b[*start_b..*end_b],
            (Value::Bool(a), Value::Bool(b)) => !a && *b, // false < true
            // Cross-type: Int/Float promotion via `as f64` cast.
            // Precision guard: integers with |n| > 2^53 trigger an error, suggesting
            // explicit [float n] cast (doc/11-stdlib.md §Equality P3, P6).
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span)?;
                (*a as f64) < *b
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span)?;
                *a < (*b as f64)
            }
            // For types not handled above, try dispatching to a Comparable instance.
            // Comparable uses num_determining=1 (single type param: the left operand's type).
            // If no instance is registered, fall back to a type error (same as before).
            _ => {
                let type_tags = vec![left.type_name().to_string()];
                // Save arg spans before moving args into try_dispatch_method.
                let arg0_span = args[0].span;
                match try_dispatch_method(
                    "Comparable",
                    "lt",
                    type_tags,
                    args,
                    Arc::clone(&ctx),
                    call_span,
                )
                .await?
                {
                    Some(result_thunk) => {
                        let val = materialize(&result_thunk, Some(&call_span), &ctx)?;
                        match val {
                            Value::Bool(b) => b,
                            _ => {
                                return Err(EvalError::type_mismatch_ctx(
                                    "Comparable.lt".to_string(),
                                    "Bool",
                                    val.type_name(),
                                    call_span,
                                )
                                .into())
                            }
                        }
                    }
                    None => {
                        return Err(EvalError::type_mismatch_ctx(
                            "<".to_string(),
                            "Int, Float, String, or Bool (same or compatible types)",
                            &format!("{} and {}", left.type_name(), right.type_name()),
                            arg0_span,
                        )
                        .into());
                    }
                }
            }
        };
        ok_val(Value::Bool(result), call_span)
    })
}

/// `if`: Conditional with selective materialization.
///
/// Takes 3 positional args: condition, then-branch, else-branch.
/// Materializes ONLY the condition, then materializes ONLY the chosen branch.
/// The unchosen branch's thunk is never materialized -- this preserves lazy
/// semantics because `eval_call` wraps each arg as a thunk before calling.
pub(crate) fn builtin_if(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("if", named.as_ref(), call_span)?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // Get the pre-materialized condition
        let condition = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match condition {
            Value::Bool(true) => Ok(Arc::clone(&args[1])),
            Value::Bool(false) => Ok(Arc::clone(&args[2])),
            _ => {
                let mut err = EvalError::type_mismatch_ctx(
                    "if".to_string(),
                    "Bool",
                    condition.type_name(),
                    args[0].span,
                );
                // Add secondary span if different from definition span
                if call_span != args[0].span {
                    err = err.with_secondary_span(
                        args[0].span,
                        format!("condition evaluated to {} here", condition.type_name()),
                    );
                }
                Err(err.into())
            }
        }
    })
}

/// Helper to extract one numeric operand and convert to f64.
fn extract_single_float(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<f64> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let val = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    match val {
        Value::Int(n) => Ok(n as f64),
        Value::Float(f) => Ok(f),
        _ => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "Int or Float",
            val.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// Helper to extract two numeric operands and convert to f64.
fn extract_two_floats(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(f64, f64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let left = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let right = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let a = match left {
        Value::Int(n) => n as f64,
        Value::Float(f) => f,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                name.to_string(),
                "Int or Float",
                left.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let b = match right {
        Value::Int(n) => n as f64,
        Value::Float(f) => f,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                name.to_string(),
                "Int or Float",
                right.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    Ok((a, b))
}

/// `pow`: Power function. Takes 2 numeric args (base, exponent). Returns Float.
/// Inherently materializing: must extract numeric values to compute power.
pub(crate) fn builtin_pow(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (base, exp) = extract_two_floats("pow", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(base.powf(exp), "pow", call_span)
    })
}

/// `sqrt`: Square root. Takes 1 numeric arg. Returns Float.
/// Allows NaN results (e.g., sqrt(-1)) for downstream predicates to check.
/// Inherently materializing: must extract numeric value to compute square root.
pub(crate) fn builtin_sqrt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("sqrt", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Float(val.sqrt()), call_span)
    })
}

/// `log`: Natural logarithm (ln). Takes 1 numeric arg. Returns Float.
/// Allows -Inf results (e.g., log(0)) for downstream predicates to check.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("log", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Float(val.ln()), call_span)
    })
}

/// `log2`: Base-2 logarithm. Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log2(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("log2", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.log2(), "log2", call_span)
    })
}

/// `log10`: Base-10 logarithm. Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log10(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("log10", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.log10(), "log10", call_span)
    })
}

/// `exp`: Exponential function (e^x). Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute exponential.
pub(crate) fn builtin_exp(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("exp", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.exp(), "exp", call_span)
    })
}

/// `sin`: Sine function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute sine.
pub(crate) fn builtin_sin(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("sin", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.sin(), "sin", call_span)
    })
}

/// `cos`: Cosine function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute cosine.
pub(crate) fn builtin_cos(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("cos", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.cos(), "cos", call_span)
    })
}

/// `tan`: Tangent function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute tangent.
pub(crate) fn builtin_tan(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("tan", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.tan(), "tan", call_span)
    })
}

/// `asin`: Arcsine function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arcsine.
pub(crate) fn builtin_asin(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("asin", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.asin(), "asin", call_span)
    })
}

/// `acos`: Arccosine function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arccosine.
pub(crate) fn builtin_acos(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("acos", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.acos(), "acos", call_span)
    })
}

/// `atan`: Arctangent function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arctangent.
pub(crate) fn builtin_atan(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("atan", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(val.atan(), "atan", call_span)
    })
}

/// `atan2`: Two-argument arctangent (atan2(y, x)). Takes 2 numeric args. Returns Float (radians).
/// Inherently materializing: must extract numeric values to compute arctangent.
pub(crate) fn builtin_atan2(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (y, x) = extract_two_floats("atan2", &args, named.as_ref(), &ctx, call_span)?;
        check_float_result(y.atan2(x), "atan2", call_span)
    })
}

/// `nan?`: Checks if a float is NaN. Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for NaN.
pub(crate) fn builtin_nan_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("nan?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(val.is_nan()), call_span)
    })
}

/// `inf?`: Checks if a float is infinite. Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for infinity.
pub(crate) fn builtin_inf_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("inf?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(val.is_infinite()), call_span)
    })
}

/// `finite?`: Checks if a float is finite (not NaN or infinite). Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for finiteness.
pub(crate) fn builtin_finite_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("finite?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(val.is_finite()), call_span)
    })
}

/// Helper to extract two Int operands, enforcing arity == 2 and type Int.
fn extract_int_pair(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(i64, i64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let left = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let right = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let a = match left {
        Value::Int(n) => n,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                name.to_string(),
                "Int",
                left.type_name(),
                args[0].span,
            )
            .into())
        }
    };

    let b = match right {
        Value::Int(n) => n,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                name.to_string(),
                "Int",
                right.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    Ok((a, b))
}

/// `band`: Bitwise AND. Takes 2 Int args. Returns Int.
/// Inherently materializing: must extract numeric values to compute bitwise AND.
pub(crate) fn builtin_band(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (a, b) = extract_int_pair("band", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Int(a & b), call_span)
    })
}

/// `bor`: Bitwise OR. Takes 2 Int args. Returns Int.
/// Inherently materializing: must extract numeric values to compute bitwise OR.
pub(crate) fn builtin_bor(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (a, b) = extract_int_pair("bor", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Int(a | b), call_span)
    })
}

/// `bxor`: Bitwise XOR. Takes 2 Int args. Returns Int.
/// Inherently materializing: must extract numeric values to compute bitwise XOR.
pub(crate) fn builtin_bxor(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (a, b) = extract_int_pair("bxor", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Int(a ^ b), call_span)
    })
}

/// `shl`: Left shift. Takes 2 Int args (value, bits). Returns Int.
/// Inherently materializing: must extract numeric values to compute left shift.
pub(crate) fn builtin_shl(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (value, bits) = extract_int_pair("shl", &args, named.as_ref(), &ctx, call_span)?;

        // Negative shift is undefined; shifts >= 64 produce 0
        if bits < 0 {
            return Err(EvalError::internal(
                format!("shl: negative shift count {}", bits),
                call_span,
            )
            .into());
        }

        if bits >= 64 {
            return ok_val(Value::Int(0), call_span);
        }

        ok_val(Value::Int(value << bits), call_span)
    })
}

/// `shr`: Logical right shift (zero-fill). Takes 2 Int args (value, bits). Returns Int.
/// Inherently materializing: must extract numeric values to compute right shift.
pub(crate) fn builtin_shr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        let (value, bits) = extract_int_pair("shr", &args, named.as_ref(), &ctx, call_span)?;

        // Negative shift is undefined; shifts >= 64 produce 0
        if bits < 0 {
            return Err(EvalError::internal(
                format!("shr: negative shift count {}", bits),
                call_span,
            )
            .into());
        }

        if bits >= 64 {
            return ok_val(Value::Int(0), call_span);
        }

        // Logical shift: cast to u64, shift, cast back to i64
        let result = ((value as u64) >> bits) as i64;
        ok_val(Value::Int(result), call_span)
    })
}

/// `float`: Explicit Int→Float conversion without precision checking.
/// - Int → Float: cast via `as f64` (user explicitly opted into potential precision loss)
/// - Float → Float: no-op
/// - Other types → error
///
/// Inherently materializing: must inspect value to determine type and perform conversion.
pub(crate) fn builtin_float(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("float", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::Int(n) => ok_val(Value::Float(n as f64), call_span),
            Value::Float(f) => ok_val(Value::Float(f), call_span),
            _ => Err(EvalError::type_mismatch_ctx(
                "float".to_string(),
                "Int or Float",
                val.type_name(),
                args[0].span,
            )
            .into()),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builtins::create_root_env;
    use crate::error::ErrorKind;
    use crate::test_util::test_span;
    use crate::value::{BuiltinArgs, Thunk, Value};

    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn no_named() -> Option<indexmap::IndexMap<String, Arc<Thunk>>> {
        None
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let root_env = create_root_env();
        crate::eval::EvalContext::new(
            base_dir,
            Arc::clone(&root_env),
            Arc::clone(&root_env),
            false,
        )
    }

    /// Drive an async builtin to completion synchronously in tests.
    fn run(f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on(f)
    }

    // --- MAX_SAFE_INT boundary ---

    /// Int(MAX_SAFE_INT) + Float(0.0): precision guard boundary — at exactly MAX_SAFE_INT
    /// the guard passes (n.abs() > MAX_SAFE_INT is false), so conversion succeeds.
    #[test]
    fn test_max_safe_int_boundary_ok() {
        let result = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(MAX_SAFE_INT)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // MAX_SAFE_INT.abs() > MAX_SAFE_INT is false → precision check passes → Float result
        let t = result.expect("expected Float result at MAX_SAFE_INT boundary");
        assert!(
            matches!(t.try_get_materialized(), Some(Value::Float(_))),
            "expected Float at MAX_SAFE_INT boundary"
        );
    }

    /// Int(MAX_SAFE_INT + 1) + Float(0.0): exceeds precision boundary → error.
    #[test]
    fn test_max_safe_int_plus_one_precision_error() {
        let result = run(builtin_add(BuiltinArgs {
            args: vec![
                thunk(Value::Int(MAX_SAFE_INT + 1)),
                thunk(Value::Float(0.0)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(
            result.is_err(),
            "expected precision error for Int > MAX_SAFE_INT"
        );
        // Should NOT be a NoInstance error — the fast path handles Int/Float before dispatch
        let err = result.unwrap_err();
        assert!(
            !matches!(&err.kind, ErrorKind::NoInstance { .. }),
            "expected precision error, not NoInstance, got: {:?}",
            err.kind
        );
    }

    // --- Int/Float fast path (no prelude needed) ---

    /// Int + Int uses the fast path — returns Int(7) without any instance registered.
    #[test]
    fn test_add_int_int_fast_path() {
        let result = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let t = result.expect("expected Int(7)");
        assert_eq!(t.try_get_materialized(), Some(Value::Int(7)));
    }

    /// Int * Int uses the fast path — returns Int(42) without any instance registered.
    #[test]
    fn test_mul_int_int_fast_path() {
        let result = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(6)), thunk(Value::Int(7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let t = result.expect("expected Int(42)");
        assert_eq!(t.try_get_materialized(), Some(Value::Int(42)));
    }

    /// Non-numeric types with no Addable instance → NoInstance error.
    /// With operator-level dispatch, String+String falls through to try_dispatch_method
    /// which finds no Addable instance (no prelude loaded in test_ctx) → NoInstance.
    #[test]
    fn test_add_non_numeric_no_instance_error() {
        use crate::value::string_val;
        let result = run(builtin_add(BuiltinArgs {
            args: vec![thunk(string_val("a")), thunk(string_val("b"))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // With dispatch restored: String+String → no Addable instance → NoInstance error.
        assert!(
            result.is_err(),
            "expected NoInstance error for String + String"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.kind, ErrorKind::NoInstance { .. }),
            "expected NoInstance, got: {:?}",
            err.kind
        );
    }

    // --- Equatable/Comparable: no infinite recursion with prelude loaded ---
    //
    // These tests verify that = and < do NOT infinitely recurse when Equatable/Comparable
    // instances are registered in stdlib/prelude.llt. The fast paths for Int/Float/String/Bool
    // handle those types BEFORE dispatch is attempted. Prelude instances for Int call
    // [builtin-eq a b] / [builtin-lt a b] which are aliases for = / < — but since the
    // (Int,Int) fast path runs first, they never re-dispatch. No infinite recursion.

    /// [= 1 1] returns true with prelude loaded (Equatable instances registered).
    /// Fast path handles (Int,Int) before any dispatch attempt.
    #[test]
    fn test_eq_int_no_infinite_recursion_with_prelude() {
        let result = crate::eval_source_with_config("[= 1 1]", true);
        assert!(
            result.is_ok(),
            "expected [= 1 1] to succeed, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), "Bool(true)");
    }

    /// [< 1 2] returns true with prelude loaded (Comparable instances registered).
    /// Fast path handles (Int,Int) before any dispatch attempt — no infinite recursion.
    #[test]
    fn test_lt_int_no_infinite_recursion_with_prelude() {
        let result = crate::eval_source_with_config("[< 1 2]", true);
        assert!(
            result.is_ok(),
            "expected [< 1 2] to succeed, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), "Bool(true)");
    }

    /// [sort [3 1 2]] works with prelude loaded (sort uses < internally).
    /// Int fast path prevents dispatch loops.
    #[test]
    fn test_sort_no_infinite_recursion_with_prelude() {
        let result = crate::eval_source_with_config("[sort [3 1 2]]", true);
        assert!(
            result.is_ok(),
            "expected [sort [3 1 2]] to succeed, got: {:?}",
            result.err()
        );
        assert_eq!(result.unwrap(), "Dict({0: Int(1), 1: Int(2), 2: Int(3)})");
    }
}
