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
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{check_float_result, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
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
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("+", named.as_ref(), call_span.clone())?;
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
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("+".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a + b, "+", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span.clone())?;
                check_float_result((*a as f64) + b, "+", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span.clone())?;
                check_float_result(a + (*b as f64), "+", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                Err(EvalError::no_instance("Addable", type_tags, call_span).into())
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
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("-", named.as_ref(), call_span.clone())?;
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
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("-".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a - b, "-", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span.clone())?;
                check_float_result((*a as f64) - b, "-", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span.clone())?;
                check_float_result(a - (*b as f64), "-", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                Err(EvalError::no_instance("Subtractable", type_tags, call_span).into())
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
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("*", named.as_ref(), call_span.clone())?;
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
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("*".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a * b, "*", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span.clone())?;
                check_float_result((*a as f64) * b, "*", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span.clone())?;
                check_float_result(a * (*b as f64), "*", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                Err(EvalError::no_instance("Multipliable", type_tags, call_span).into())
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
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("/", named.as_ref(), call_span.clone())?;
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
                        EvalError::division_by_zero("/".to_string(), call_span.clone()).into(),
                    );
                }
                check_float_result(*a as f64 / *b as f64, "/", call_span)
            }
            (Value::Float(a), Value::Float(b)) => check_float_result(a / b, "/", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span.clone())?;
                check_float_result((*a as f64) / b, "/", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span.clone())?;
                check_float_result(a / (*b as f64), "/", call_span)
            }
            _ => {
                let type_tags = vec![left.type_name().to_string(), right.type_name().to_string()];
                Err(EvalError::no_instance("Divisible", type_tags, call_span).into())
            }
        }
    })
}

/// `=`: Equality comparison.
///
/// Delegates all structural equality logic to the canonical `eval::values_equal`,
/// which handles Int, Float, String, Bool, Variant (with/without payload), Dict,
/// Seq. Cross-type combinations return false (no Int/Float promotion in equality).
///
/// Cycle detection for Dict/Seq is provided by `materialize`'s InProgress sentinel.
///
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
        reject_named("=", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Both args are pre-materialized by force_count/pos_strictness.
        // NOTE: The canonical values_equal handles all types. Fast paths for
        // Int/Float/String/Bool return immediately without async overhead.
        // Cross-type comparisons (e.g. Int vs Float) return false — no implicit promotion.
        // This prevents infinite recursion when EquatableInt.eq calls [builtin-eq a b]:
        // values_equal dispatches on the value type directly, not through typeclass dispatch.
        let left = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let result =
            crate::eval::values_equal(left, right, call_span.clone(), Arc::clone(&ctx)).await?;
        ok_val(Value::boolean(result), call_span)
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
        ctx: _,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("<", named.as_ref(), call_span.clone())?;
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
            (a, b) if a.as_bool().is_some() && b.as_bool().is_some() => {
                !a.as_bool().unwrap() && b.as_bool().unwrap() // false < true
            }
            // Cross-type: Int/Float promotion via `as f64` cast.
            // Precision guard: integers with |n| > 2^53 trigger an error, suggesting
            // explicit [float n] cast (doc/11-stdlib.md §Equality P3, P6).
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, args[0].span.clone())?;
                (*a as f64) < *b
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, args[1].span.clone())?;
                *a < (*b as f64)
            }
            // For types not handled above, produce a type error.
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "<".to_string(),
                    "Int, Float, String, or Bool (same or compatible types)",
                    &format!("{} and {}", left.type_name(), right.type_name()),
                    args[0].span.clone(),
                )
                .into());
            }
        };
        ok_val(Value::boolean(result), call_span)
    })
}

/// `>`: Greater-than comparison.
///
/// Implemented as `b < a` (argument order swap of `<`).
pub(crate) fn builtin_gt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named(">", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Swap arguments: a > b ≡ b < a
        let swapped_args = vec![Arc::clone(&args[1]), Arc::clone(&args[0])];
        builtin_lt(BuiltinArgs {
            args: swapped_args,
            named,
            call_span,
            ctx,
        })
        .await
    })
}

/// `<=`: Less-than-or-equal comparison.
///
/// Implemented as `!(b < a)` (negation of `>`).
pub(crate) fn builtin_lte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("<=", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // a <= b ≡ !(b < a)
        let swapped_args = vec![Arc::clone(&args[1]), Arc::clone(&args[0])];
        let gt_result = builtin_lt(BuiltinArgs {
            args: swapped_args,
            named,
            call_span: call_span.clone(),
            ctx,
        })
        .await?;

        // Negate the result
        let val = gt_result
            .try_get_materialized()
            .expect("builtin_lt returns materialized Bool");
        match val.as_bool() {
            Some(b) => ok_val(Value::boolean(!b), call_span),
            None => unreachable!("builtin_lt always returns Bool"),
        }
    })
}

/// `>=`: Greater-than-or-equal comparison.
///
/// Implemented as `!(a < b)` (negation of `<`).
pub(crate) fn builtin_gte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named(">=", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // a >= b ≡ !(a < b)
        let lt_result = builtin_lt(BuiltinArgs {
            args,
            named,
            call_span: call_span.clone(),
            ctx,
        })
        .await?;

        // Negate the result
        let val = lt_result
            .try_get_materialized()
            .expect("builtin_lt returns materialized Bool");
        match val.as_bool() {
            Some(b) => ok_val(Value::boolean(!b), call_span),
            None => unreachable!("builtin_lt always returns Bool"),
        }
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
        reject_named("if", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // Get the pre-materialized condition
        let condition = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match condition.as_bool() {
            Some(true) => Ok(Arc::clone(&args[1])),
            Some(false) => Ok(Arc::clone(&args[2])),
            None => {
                let cond_span = args[0].span.clone();
                let mut err = EvalError::type_mismatch_ctx(
                    "if".to_string(),
                    "Bool",
                    condition.type_name(),
                    cond_span.clone(),
                );
                // Add secondary span if different from definition span
                if call_span != cond_span {
                    err = err.with_secondary_span(
                        cond_span,
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
            args[0].span.clone(),
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
                args[0].span.clone(),
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
                args[1].span.clone(),
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
        let (base, exp) =
            extract_two_floats("pow", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("sqrt", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("log", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("log2", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("log10", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("exp", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("sin", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("cos", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("tan", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("asin", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("acos", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("atan", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let (y, x) = extract_two_floats("atan2", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let val = extract_single_float("nan?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::boolean(val.is_nan()), call_span)
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
        let val = extract_single_float("inf?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::boolean(val.is_infinite()), call_span)
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
        let val = extract_single_float("finite?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::boolean(val.is_finite()), call_span)
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
                args[0].span.clone(),
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
                args[1].span.clone(),
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
        let (a, b) = extract_int_pair("band", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let (a, b) = extract_int_pair("bor", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let (a, b) = extract_int_pair("bxor", &args, named.as_ref(), &ctx, call_span.clone())?;
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
        let (value, bits) =
            extract_int_pair("shl", &args, named.as_ref(), &ctx, call_span.clone())?;

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
        let (value, bits) =
            extract_int_pair("shr", &args, named.as_ref(), &ctx, call_span.clone())?;

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
        reject_named("float", named.as_ref(), call_span.clone())?;
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
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// Register `builtin-*` type aliases for math and comparison builtins (T-1102).
///
/// Each alias copies the TypeScheme from the canonical name already registered in
/// `core_type_env`. Call this AFTER `core_type_env` has run.
pub fn math_builtin_types(env: &mut crate::types::TypeEnv) {
    env.alias_types(&[
        ("builtin-lt", "<"),
        ("builtin-gt", ">"),
        ("builtin-gte", ">="),
        ("builtin-lte", "<="),
        ("builtin-eq", "="),
        ("builtin-add", "+"),
        ("builtin-sub", "-"),
        ("builtin-mul", "*"),
        ("builtin-div", "/"),
        ("builtin-if", "if"),
        ("builtin-band", "band"),
        ("builtin-bor", "bor"),
        ("builtin-bxor", "bxor"),
        ("builtin-shl", "shl"),
        ("builtin-shr", "shr"),
        ("builtin-float", "float"),
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::error::ErrorKind;
    use crate::rust_span;
    use crate::test_util::test_span;
    use crate::value::Environment;
    use crate::value::{BuiltinArgs, Thunk, Value};
    use std::sync::RwLock;

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
        let env = Arc::new(RwLock::new(Environment::new()));
        if let Some(defs) = crate::builtins::builtin_module("core") {
            for def in defs {
                let name = def.name.to_string();
                let thunk = Arc::new(Thunk::new_materialized(Value::Builtin(def), rust_span!()));
                env.write().unwrap().insert(name, thunk);
            }
        }
        crate::eval::EvalContext::new_empty(base_dir, env, false)
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
}
