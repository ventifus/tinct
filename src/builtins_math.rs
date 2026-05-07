//! Arithmetic, comparison, and control-flow builtins: `+`, `-`, `*`, `/`, `=`, `<`, `if`.
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

use std::rc::Rc;

use crate::builtins::{check_float_result, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Thunk, Value};

/// Two-operand numeric pair after auto-promotion.
///
/// Used by arithmetic builtins to implement the promotion table:
/// - Int op Int   -> Ints(a, b)
/// - Int op Float -> Floats(a as f64, b)
/// - Float op Int -> Floats(a, b as f64)
/// - Float op Float -> Floats(a, b)
enum NumPair {
    Ints(i64, i64),
    Floats(f64, f64),
}

/// Extract two numeric operands with auto-promotion, enforcing arity == 2.
fn extract_num_pair(
    args: &[Rc<Thunk>],
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: crate::ast::Span,
) -> EvalResult<NumPair> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], Some(&call_span), ctx, depth)?;
    let right = materialize(&args[1], Some(&call_span), ctx, depth)?;
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => Ok(NumPair::Ints(*a, *b)),
        (Value::Int(a), Value::Float(b)) => Ok(NumPair::Floats(*a as f64, *b)),
        (Value::Float(a), Value::Int(b)) => Ok(NumPair::Floats(*a, *b as f64)),
        (Value::Float(a), Value::Float(b)) => Ok(NumPair::Floats(*a, *b)),
        _ => Err(EvalError::type_mismatch_ctx(
            "+/-/*//".to_string(),
            "Int or Float",
            &format!("{} and {}", left.type_name(), right.type_name()),
            args[0].span,
        )
        .into()),
    }
}

/// `+`: Addition with auto-promotion. Int + Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute sum.
pub(crate) fn builtin_add(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("+", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_add(b)
            .map(|n| ok_val(Value::Int(n), call_span))
            .unwrap_or_else(|| {
                // Overflow error: def_span is call_span (the + operation itself)
                Err(EvalError::integer_overflow("+".to_string(), call_span).into())
            }),
        NumPair::Floats(a, b) => check_float_result(a + b, "+", call_span),
    }
}

/// `-`: Subtraction with auto-promotion. Int - Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute difference.
pub(crate) fn builtin_sub(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("-", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_sub(b)
            .map(|n| ok_val(Value::Int(n), call_span))
            .unwrap_or_else(|| {
                // Overflow error: def_span is call_span (the - operation itself)
                Err(EvalError::integer_overflow("-".to_string(), call_span).into())
            }),
        NumPair::Floats(a, b) => check_float_result(a - b, "-", call_span),
    }
}

/// `*`: Multiplication with auto-promotion. Int * Int -> Int, any Float operand -> Float.
/// Inherently materializing: must extract numeric values to compute product.
pub(crate) fn builtin_mul(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("*", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => a
            .checked_mul(b)
            .map(|n| ok_val(Value::Int(n), call_span))
            .unwrap_or_else(|| {
                // Overflow error: def_span is call_span (the * operation itself)
                Err(EvalError::integer_overflow("*".to_string(), call_span).into())
            }),
        NumPair::Floats(a, b) => check_float_result(a * b, "*", call_span),
    }
}

/// `/`: Float division. ALWAYS returns Float, even for Int / Int. Division by zero produces an error.
/// Inherently materializing: must extract numeric values to compute quotient.
pub(crate) fn builtin_div_float(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("/", named, call_span)?;
    match extract_num_pair(args, &ctx, depth, call_span)? {
        NumPair::Ints(a, b) => {
            if b == 0 {
                Err(EvalError::division_by_zero("/".to_string(), call_span).into())
            } else {
                ok_val(Value::Float(a as f64 / b as f64), call_span)
            }
        }
        NumPair::Floats(a, b) => {
            if b == 0.0 {
                Err(EvalError::division_by_zero("/".to_string(), call_span).into())
            } else {
                check_float_result(a / b, "/", call_span)
            }
        }
    }
}

/// `=`: Equality comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison
/// promotes Int to Float. Dict/Function/Builtin are never equal (returns false,
/// not an error).
/// Inherently materializing: must inspect values to determine equality.
pub(crate) fn builtin_eq(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("=", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let right = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    let result = match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        // Cross-type: Int/Float promotion via `as f64` cast.
        // Known limitation: integers with absolute value > 2^53 lose precision on
        // promotion (e.g. 9007199254740993i64 as f64 == 9007199254740992.0), which
        // can cause non-transitive equality (doc/11-stdlib.md §Equality P3). No
        // runtime guard is added — this matches Jsonnet's silent promotion approach.
        (Value::Int(a), Value::Float(b)) => (*a as f64) == *b,
        (Value::Float(a), Value::Int(b)) => *a == (*b as f64),
        // Variant: equal if tags match and payloads match
        // For Phase 1 (unit constructors), payload is always None
        // TODO(C3): implement recursive payload equality for payload constructors
        (
            Value::Variant {
                tag: tag_a,
                payload: payload_a,
            },
            Value::Variant {
                tag: tag_b,
                payload: payload_b,
            },
        ) => tag_a == tag_b && payload_a.is_none() && payload_b.is_none(),
        // Dict, Function, Builtin are never equal
        _ => false,
    };
    ok_val(Value::Bool(result), call_span)
}

/// `<`: Less-than comparison.
/// Works on Int, Float, String, Bool. Cross-type Int/Float comparison promotes
/// Int to Float. String comparison is lexicographic. Bool: false < true.
/// Incompatible types (e.g. Int vs String) produce a type error.
/// Inherently materializing: must inspect values to determine ordering.
pub(crate) fn builtin_lt(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("<", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let right = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    let result = match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => a < b,
        (Value::Float(a), Value::Float(b)) => a < b,
        (Value::String(a), Value::String(b)) => a < b,
        (Value::Bool(a), Value::Bool(b)) => !a && *b, // false < true
        // Cross-type: Int/Float promotion via `as f64` cast.
        // Known limitation: integers with absolute value > 2^53 lose precision on
        // promotion (doc/11-stdlib.md §Equality P3, P6). No runtime guard — matches
        // Jsonnet's silent promotion approach.
        (Value::Int(a), Value::Float(b)) => (*a as f64) < *b,
        (Value::Float(a), Value::Int(b)) => *a < (*b as f64),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "<".to_string(),
                "Int, Float, String, or Bool (same or compatible types)",
                &format!("{} and {}", left.type_name(), right.type_name()),
                args[0].span,
            )
            .into());
        }
    };
    ok_val(Value::Bool(result), call_span)
}

/// `if`: Conditional with selective materialization.
///
/// Takes 3 positional args: condition, then-branch, else-branch.
/// Materializes ONLY the condition, then materializes ONLY the chosen branch.
/// The unchosen branch's thunk is never materialized -- this preserves lazy
/// semantics because `eval_call` wraps each arg as a thunk before calling.
pub(crate) fn builtin_if(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("if", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    // Materialize only the condition
    let condition = materialize(&args[0], Some(&call_span), &ctx, depth)?;

    match condition {
        Value::Bool(true) => Ok(Rc::clone(&args[1])),
        Value::Bool(false) => Ok(Rc::clone(&args[2])),
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
}
