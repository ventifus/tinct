//! Arithmetic and comparison builtins: `+`, `-`, `*`, `/`, `=`, `<`.
//! `builtin-add/sub/mul/div` are pure Int/Float primitives — no typeclass dispatch. Dispatch belongs at the operator level.
//!
//! These builtins operate on numeric and boolean values. They are all inherently
//! materializing because they must inspect operand values to compute results.
//!
//! - Arithmetic (`+`, `-`, `*`, `/`): auto-promote Int/Float operands
//! - Comparison (`=`, `<`): cross-type Int/Float promotion; String/Bool same-type comparison
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
use crate::value::{BuiltinArgs, Thunk, ThunkId, Value};

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

/// `builtin-mul`: Pure Int/Float multiplication primitive. No typeclass dispatch.
pub(crate) fn builtin_mul(
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
        reject_named("*", named.as_ref(), call_span.clone())?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let left = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = thunk1
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => a
                .checked_mul(*b)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("*".to_string(), call_span).into()),
            (Value::Float(a), Value::Float(b)) => check_float_result(a * b, "*", call_span),
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, thunk0.span.clone())?;
                check_float_result((*a as f64) * b, "*", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, thunk1.span.clone())?;
                check_float_result(a * (*b as f64), "*", call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "Int or Float",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into()),
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
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("/", named.as_ref(), call_span.clone())?;
        if args.len() < 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let left = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = thunk1
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
                check_int_to_float_precision(*a, thunk0.span.clone())?;
                check_float_result((*a as f64) / b, "/", call_span)
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, thunk1.span.clone())?;
                check_float_result(a / (*b as f64), "/", call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "Int or Float",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-eq-int`: Type-specific integer equality.
///
/// Takes exactly two Int arguments, returns Int (1 if equal, 0 if not).
/// No cross-type comparison — both args must be Int.
pub(crate) fn builtin_eq_int(
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
        reject_named("builtin-eq-int", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let left = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = ctx.get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match (&left, &right) {
            (Value::Int(a), Value::Int(b)) => {
                ok_val(Value::Int(if a == b { 1 } else { 0 }), call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "Int",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-eq-float`: Type-specific float equality.
///
/// Takes exactly two Float arguments, returns Int (1 if equal, 0 if not).
/// No cross-type comparison — both args must be Float.
pub(crate) fn builtin_eq_float(
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
        reject_named("builtin-eq-float", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let left = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = ctx.get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match (&left, &right) {
            (Value::Float(a), Value::Float(b)) => {
                ok_val(Value::Int(if a == b { 1 } else { 0 }), call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "Float",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-eq-string`: Type-specific string equality.
///
/// Takes exactly two String arguments, returns Int (1 if equal, 0 if not).
/// No cross-type comparison — both args must be String.
pub(crate) fn builtin_eq_string(
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
        reject_named("builtin-eq-string", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let left = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = ctx.get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match (&left, &right) {
            (
                Value::String {
                    source: s1,
                    start: start1,
                    end: end1,
                },
                Value::String {
                    source: s2,
                    start: start2,
                    end: end2,
                },
            ) => {
                let eq = s1[*start1..*end1] == s2[*start2..*end2];
                ok_val(Value::Int(if eq { 1 } else { 0 }), call_span)
            }
            _ => Err(EvalError::type_mismatch(
                "String",
                &format!("{} and {}", left.type_name(), right.type_name()),
                call_span,
            )
            .into()),
        }
    })
}

/// `<`: Less-than comparison.
/// Works on Int, Float, String. Cross-type Int/Float comparison promotes
/// Int to Float. String comparison is lexicographic.
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
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("<", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Int/Float/String fast paths are handled directly. Other types fall through
        // to Comparable instance dispatch. ComparableInt.lt calls [builtin-lt a b] which hits
        // the (Int,Int) fast path — no infinite recursion.
        let thunk0 = ctx.get_thunk(args[0]);
        let thunk1 = ctx.get_thunk(args[1]);
        let left = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let right = thunk1
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
            // Cross-type: Int/Float promotion via `as f64` cast.
            // Precision guard: integers with |n| > 2^53 trigger an error, suggesting
            // explicit [float n] cast (doc/11-stdlib.md §Equality P3, P6).
            (Value::Int(a), Value::Float(b)) => {
                check_int_to_float_precision(*a, thunk0.span.clone())?;
                (*a as f64) < *b
            }
            (Value::Float(a), Value::Int(b)) => {
                check_int_to_float_precision(*b, thunk1.span.clone())?;
                *a < (*b as f64)
            }
            // For types not handled above, produce a type error.
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "<".to_string(),
                    "Int, Float, or String (same or compatible types)",
                    &format!("{} and {}", left.type_name(), right.type_name()),
                    thunk0.span.clone(),
                )
                .into());
            }
        };
        ok_val(Value::Int(if result { 1 } else { 0 }), call_span)
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
        caller_env,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("<=", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // a <= b ≡ !(b < a)
        let swapped_args = vec![args[1], args[0]];
        let gt_result = builtin_lt(BuiltinArgs {
            args: swapped_args,
            named,
            call_span: call_span.clone(),
            caller_env,
            caller_env_id: 0,
            ctx,
        })
        .await?;

        // Negate the result (builtin_lt always returns Value::Int(0/1))
        let val = gt_result
            .try_get_materialized()
            .expect("builtin_lt returns materialized");
        ok_val(
            Value::Int(if matches!(val, Value::Int(n) if n != 0) {
                0
            } else {
                1
            }),
            call_span,
        )
    })
}

/// Helper to extract one numeric operand and convert to f64.
fn extract_single_float(
    name: &str,
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<f64> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let thunk0 = ctx.get_thunk(args[0]);
    let val = thunk0
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    match val {
        Value::Int(n) => Ok(n as f64),
        Value::Float(f) => Ok(f),
        _ => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "Int or Float",
            val.type_name(),
            thunk0.span.clone(),
        )
        .into()),
    }
}

/// Helper to extract two numeric operands and convert to f64.
fn extract_two_floats(
    name: &str,
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(f64, f64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let thunk0 = ctx.get_thunk(args[0]);
    let thunk1 = ctx.get_thunk(args[1]);
    let left = thunk0
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let right = thunk1
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
                thunk0.span.clone(),
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
                thunk1.span.clone(),
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("nan?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::Int(if val.is_nan() { 1 } else { 0 }), call_span)
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
        ..
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("inf?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::Int(if val.is_infinite() { 1 } else { 0 }), call_span)
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
        ..
    } = ctx_arg;
    Box::pin(async move {
        let val = extract_single_float("finite?", &args, named.as_ref(), &ctx, call_span.clone())?;
        ok_val(Value::Int(if val.is_finite() { 1 } else { 0 }), call_span)
    })
}

/// Helper to extract two Int operands, enforcing arity == 2 and type Int.
fn extract_int_pair(
    name: &str,
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(i64, i64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let thunk0 = ctx.get_thunk(args[0]);
    let thunk1 = ctx.get_thunk(args[1]);
    let left = thunk0
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let right = thunk1
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    let a = match left {
        Value::Int(n) => n,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                name.to_string(),
                "Int",
                left.type_name(),
                thunk0.span.clone(),
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
                thunk1.span.clone(),
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
        ..
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
        ..
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
        ..
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
        ..
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
        ..
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
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("float", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let thunk0 = ctx.get_thunk(args[0]);
        let val = thunk0
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::Int(n) => ok_val(Value::Float(n as f64), call_span),
            Value::Float(f) => ok_val(Value::Float(f), call_span),
            _ => Err(EvalError::type_mismatch_ctx(
                "float".to_string(),
                "Int or Float",
                val.type_name(),
                thunk0.span.clone(),
            )
            .into()),
        }
    })
}

// ── Monomorphic arithmetic primitives ─────────────────────────────────────────
// Each handles exactly one type combination. Cross-type arithmetic (Integer+Float)
// is handled in tinct by explicit conversion via builtin-int-to-float.

pub(crate) fn builtin_int_add(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-add", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => x.checked_add(*y)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("builtin-int-add".to_string(), call_span).into()),
            _ => Err(EvalError::type_mismatch("Integer", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_float_add(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-float-add", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Float(x), Value::Float(y)) => check_float_result(x + y, "builtin-float-add", call_span),
            _ => Err(EvalError::type_mismatch("Float", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_int_to_float(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-to-float", named.as_ref(), call_span.clone())?;
        if args.len() != 1 { return Err(EvalError::arity_mismatch(1, args.len(), call_span).into()); }
        let thunk0 = ctx.get_thunk(args[0]);
        let v = thunk0.try_get_materialized().expect("pre-materialized");
        match v {
            Value::Int(n) => {
                // Precision guard: integers with |n| > 2^53 lose precision as f64.
                check_int_to_float_precision(n, thunk0.span.clone())?;
                ok_val(Value::Float(n as f64), call_span)
            }
            _ => Err(EvalError::type_mismatch("Integer", v.type_name(), call_span).into()),
        }
    })
}

pub(crate) fn builtin_int_sub(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-sub", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => x.checked_sub(*y)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("builtin-int-sub".to_string(), call_span).into()),
            _ => Err(EvalError::type_mismatch("Integer", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_float_sub(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-float-sub", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Float(x), Value::Float(y)) => check_float_result(x - y, "builtin-float-sub", call_span),
            _ => Err(EvalError::type_mismatch("Float", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_int_mul(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-mul", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => x.checked_mul(*y)
                .map(|r| Arc::new(Thunk::new_materialized(Value::Int(r), call_span.clone())))
                .ok_or_else(|| EvalError::integer_overflow("builtin-int-mul".to_string(), call_span).into()),
            _ => Err(EvalError::type_mismatch("Integer", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_float_mul(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-float-mul", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Float(x), Value::Float(y)) => check_float_result(x * y, "builtin-float-mul", call_span),
            _ => Err(EvalError::type_mismatch("Float", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

// ── Monomorphic comparison primitives ──────────────────────────────────────────
// Each handles exactly one type. Result is Integer 1 (true) or 0 (false).

pub(crate) fn builtin_int_gt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-gt", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(if x > y { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("Integer", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_float_gt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-float-gt", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Float(x), Value::Float(y)) => ok_val(Value::Int(if x > y { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("Float", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_str_gt(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-str-gt", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::String { source: sa, start: sta, end: ea }, Value::String { source: sb, start: stb, end: eb }) =>
                ok_val(Value::Int(if sa[*sta..*ea] > sb[*stb..*eb] { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("String", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_int_gte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-int-gte", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(if x >= y { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("Integer", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_float_gte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-float-gte", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::Float(x), Value::Float(y)) => ok_val(Value::Int(if x >= y { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("Float", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

pub(crate) fn builtin_str_gte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs { args, named, call_span, ctx, .. } = ctx_arg;
    Box::pin(async move {
        reject_named("builtin-str-gte", named.as_ref(), call_span.clone())?;
        if args.len() != 2 { return Err(EvalError::arity_mismatch(2, args.len(), call_span).into()); }
        let a = ctx.get_thunk(args[0]).try_get_materialized().expect("pre-materialized");
        let b = ctx.get_thunk(args[1]).try_get_materialized().expect("pre-materialized");
        match (&a, &b) {
            (Value::String { source: sa, start: sta, end: ea }, Value::String { source: sb, start: stb, end: eb }) =>
                ok_val(Value::Int(if sa[*sta..*ea] >= sb[*stb..*eb] { 1 } else { 0 }), call_span),
            _ => Err(EvalError::type_mismatch("String", &format!("{} and {}", a.type_name(), b.type_name()), call_span).into()),
        }
    })
}

/// Returns all "math" module Rust builtins.
///
/// These are the arithmetic, comparison, bitwise, and numeric conversion builtins that
/// are NOT in the Core-46 set. The Core-46 items (builtin-add, builtin-sub, builtin-gt,
/// builtin-gte, builtin-lt, builtin-eq-int, builtin-eq-string) stay in core_builtins()
/// for loader.llt which only has `--- uses: ["core"]`.
///
/// Consumed exclusively by `builtin_module("math")` in `src/builtins.rs`.
pub fn math_builtins() -> Vec<crate::value::BuiltinDef> {
    use crate::builtins::builtin;
    use crate::value::Strictness;
    vec![
        // ── Arithmetic (non-Core-46) ──────────────────────────────────────────────────
        builtin!(
            "builtin-mul",
            builtin_mul,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-div",
            builtin_div_float,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Comparison (non-Core-46) ──────────────────────────────────────────────────
        builtin!(
            "builtin-eq-float",
            builtin_eq_float,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-lte",
            builtin_lte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        // ── Math ─────────────────────────────────────────────────────────────────────
        builtin!("builtin-floor", crate::builtins::builtin_floor, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-round", crate::builtins::builtin_round, [Strictness::Seq], 1, ["n"]),
        builtin!(
            "builtin-pow",
            builtin_pow,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["base", "exp"]
        ),
        builtin!("builtin-sqrt", builtin_sqrt, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log", builtin_log, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log2", builtin_log2, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-log10", builtin_log10, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-exp", builtin_exp, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-sin", builtin_sin, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-cos", builtin_cos, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-tan", builtin_tan, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-asin", builtin_asin, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-acos", builtin_acos, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-atan", builtin_atan, [Strictness::Seq], 1, ["n"]),
        builtin!(
            "builtin-atan2",
            builtin_atan2,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["y", "x"]
        ),
        builtin!(
            "builtin-nan?",
            builtin_nan_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-inf?",
            builtin_inf_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            "builtin-finite?",
            builtin_finite_check,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        // ── Bitwise ──────────────────────────────────────────────────────────────────
        builtin!(
            "builtin-band",
            builtin_band,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-bor",
            builtin_bor,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-bxor",
            builtin_bxor,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-shl",
            builtin_shl,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["n", "bits"]
        ),
        builtin!(
            "builtin-shr",
            builtin_shr,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["n", "bits"]
        ),
        // ── Type conversion ──────────────────────────────────────────────────────────
        builtin!("builtin-float", builtin_float, [Strictness::Seq], 1, ["n"]),
        builtin!("builtin-to-int", crate::builtins::builtin_to_int, [Strictness::Seq]),
        builtin!("builtin-to-float", crate::builtins::builtin_to_float, [Strictness::Seq]),
    ]
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::env::Env;
    use crate::error::ErrorKind;
    use crate::test_util::test_span;
    use crate::value::{BuiltinArgs, Thunk, ThunkId, Value};
    use std::sync::RwLock;

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn no_named() -> Option<indexmap::IndexMap<String, ThunkId>> {
        None
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let env = Arc::new(RwLock::new(Env::new()));
        // T-1557: Env is type-metadata only; register slot names for the resolver.
        // Runtime thunks are pre-populated in the root FlatEnv by EvalContext::new_env_arena().
        if let Some(defs) = crate::builtins::builtin_module("core") {
            for def in defs {
                env.write().unwrap().insert_slot_name_only(def.name.to_string());
            }
        }
        crate::eval::EvalContext::new_empty(base_dir, env, false)
    }

    fn alloc(ctx: &Arc<crate::eval::EvalContext>, val: Value) -> ThunkId {
        ctx.alloc_thunk(Arc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5))))
    }

    /// Drive an async builtin to completion synchronously in tests.
    fn run(f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on(f)
    }

    // --- MAX_SAFE_INT boundary ---

    /// builtin-int-to-float at MAX_SAFE_INT: precision guard passes (boundary case).
    #[test]
    fn test_max_safe_int_boundary_ok() {
        let ctx = test_ctx();
        let result = run(builtin_int_to_float(BuiltinArgs {
            args: vec![alloc(&ctx, Value::Int(MAX_SAFE_INT))],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }));
        let t = result.expect("expected Float result at MAX_SAFE_INT boundary");
        assert!(
            matches!(t.try_get_materialized(), Some(Value::Float(_))),
            "expected Float at MAX_SAFE_INT boundary"
        );
    }

    /// builtin-int-to-float at MAX_SAFE_INT+1: precision guard fires → error.
    #[test]
    fn test_max_safe_int_plus_one_precision_error() {
        let ctx = test_ctx();
        let result = run(builtin_int_to_float(BuiltinArgs {
            args: vec![alloc(&ctx, Value::Int(MAX_SAFE_INT + 1))],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }));
        assert!(result.is_err(), "expected precision error for Int > MAX_SAFE_INT");
    }

    // --- Monomorphic int-add fast path ---

    /// builtin-int-add: Int + Int → Int(7).
    #[test]
    fn test_add_int_int_fast_path() {
        let ctx = test_ctx();
        let result = run(builtin_int_add(BuiltinArgs {
            args: vec![alloc(&ctx, Value::Int(3)), alloc(&ctx, Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }));
        let t = result.expect("expected Int(7)");
        assert_eq!(t.try_get_materialized(), Some(Value::Int(7)));
    }

    /// Int * Int uses the fast path — returns Int(42) without any instance registered.
    #[test]
    fn test_mul_int_int_fast_path() {
        let ctx = test_ctx();
        let result = run(builtin_mul(BuiltinArgs {
            args: vec![alloc(&ctx, Value::Int(6)), alloc(&ctx, Value::Int(7))],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }));
        let t = result.expect("expected Int(42)");
        assert_eq!(t.try_get_materialized(), Some(Value::Int(42)));
    }

    /// builtin-int-add: non-Integer types produce TypeMismatch error.
    #[test]
    fn test_add_non_numeric_type_mismatch_error() {
        use crate::value::string_val;
        let ctx = test_ctx();
        let result = run(builtin_int_add(BuiltinArgs {
            args: vec![alloc(&ctx, string_val("a")), alloc(&ctx, string_val("b"))],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }));
        // Non-Int/Float operands produce a TypeMismatch error.
        assert!(
            result.is_err(),
            "expected TypeMismatch error for String + String"
        );
        let err = result.unwrap_err();
        assert!(
            matches!(&err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got: {:?}",
            err.kind
        );
    }
}
