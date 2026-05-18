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

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{check_float_result, ok_val, reject_named};
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext};
use crate::eval_call::{invoke_function, CallContext};
use crate::value::Key;
use crate::value::{BuiltinArgs, Thunk, Value};

/// Helper to dispatch a typeclass method call if an instance is registered.
/// Returns Some(result) if instance was found and method called, None to fall through.
fn try_dispatch_method(
    class_name: &'static str,
    method_name: &str,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    call_span: Span,
    ctx: &Rc<EvalContext>,
) -> Option<EvalResult<Rc<Thunk>>> {
    if args.is_empty() {
        return None;
    }

    // Only materialize if at least one instance for this class is registered.
    // Registry keys use type names (post chr-prelude); until then all lookups miss.
    // Gating here avoids forcing a lazy thunk solely to check a registry key.
    let has_candidate = ctx.state.borrow().registered_classes.contains(class_name);
    if !has_candidate {
        return None;
    }

    let first_val = materialize(&args[0], Some(&call_span), ctx).ok()?;
    let type_name = first_val.type_name();

    let instance_thunk = ctx
        .state
        .borrow()
        .instance_registry
        .get(&(class_name, type_name.to_string()))
        .cloned()?;

    let instance_val = materialize(&instance_thunk, Some(&call_span), ctx).ok()?;

    if let Value::Dict(methods_map) = instance_val {
        let method_id = methods_map.get(&Key::String(method_name.to_string()))?;
        let method_thunk = ctx.get_thunk(*method_id);
        let method_val = materialize(&method_thunk, Some(&call_span), ctx).ok()?;

        match method_val {
            Value::Function {
                params,
                body,
                env: closure_env,
                ..
            } => Some(invoke_function(&CallContext {
                params: &params,
                body: &body,
                closure_env: &closure_env,
                positional: args,
                named: None,
                default_env: &closure_env,
                call_span,
                origin: Some(Rc::from(format!("{}.{}", class_name, method_name))),
                ctx,
            })),
            Value::Builtin(def) => Some((def.func)(BuiltinArgs {
                args,
                named,
                call_span,
                ctx: Rc::clone(ctx),
            })),
            _ => None,
        }
    } else {
        None
    }
}

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

/// Maximum safe integer for Int→Float promotion (2^53).
/// Integers with |n| > MAX_SAFE_INT lose precision when cast to f64.
const MAX_SAFE_INT: i64 = 9007199254740992;

/// Check if an Int→Float promotion would lose precision.
/// Returns Err if |n| > 2^53, suggesting explicit [float n] cast.
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

/// Extract two numeric operands with auto-promotion, enforcing arity == 2.
/// Int→Float promotion is guarded by precision check (|n| ≤ 2^53).
fn extract_num_pair(
    args: &[Rc<Thunk>],
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<NumPair> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    let left = materialize(&args[0], Some(&call_span), ctx)?;
    let right = materialize(&args[1], Some(&call_span), ctx)?;
    match (&left, &right) {
        (Value::Int(a), Value::Int(b)) => Ok(NumPair::Ints(*a, *b)),
        (Value::Int(a), Value::Float(b)) => {
            check_int_to_float_precision(*a, args[0].span)?;
            Ok(NumPair::Floats(*a as f64, *b))
        }
        (Value::Float(a), Value::Int(b)) => {
            check_int_to_float_precision(*b, args[1].span)?;
            Ok(NumPair::Floats(*a, *b as f64))
        }
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("+", named, call_span)?;

    // Runtime typeclass dispatch: check for Add instance
    if let Some(result) = try_dispatch_method("Add", "add", args, named, call_span, &ctx) {
        return result;
    }

    // Fall through to existing Rust dispatch
    match extract_num_pair(args, &ctx, call_span)? {
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("-", named, call_span)?;

    // Runtime typeclass dispatch: check for Sub instance
    if let Some(result) = try_dispatch_method("Sub", "sub", args, named, call_span, &ctx) {
        return result;
    }

    // Fall through to existing Rust dispatch
    match extract_num_pair(args, &ctx, call_span)? {
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("*", named, call_span)?;

    // Runtime typeclass dispatch: check for Mul instance
    if let Some(result) = try_dispatch_method("Mul", "mul", args, named, call_span, &ctx) {
        return result;
    }

    // Fall through to existing Rust dispatch
    match extract_num_pair(args, &ctx, call_span)? {
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("/", named, call_span)?;

    // Runtime typeclass dispatch: check for Div instance
    if let Some(result) = try_dispatch_method("Div", "div", args, named, call_span, &ctx) {
        return result;
    }

    // Fall through to existing Rust dispatch
    match extract_num_pair(args, &ctx, call_span)? {
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("=", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    // Runtime typeclass dispatch: check for Equatable instance BEFORE materializing.
    // try_dispatch_method materializes args[0] internally, so checking first avoids
    // double-materialization when a typeclass instance exists.
    if let Some(result) = try_dispatch_method("Equatable", "eq", args, named, call_span, &ctx) {
        return result;
    }

    // No typeclass instance — materialize for default equality comparison
    let left = materialize(&args[0], Some(&call_span), &ctx)?;
    let right = materialize(&args[1], Some(&call_span), &ctx)?;

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
        ) => &source_a[*start_a..*end_a] == &source_b[*start_b..*end_b],
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
                        // Resolve ThunkIds to Rc<Thunk> via arena
                        let p1_thunk = ctx.get_thunk(*p1_id);
                        let p2_thunk = ctx.get_thunk(*p2_id);
                        // Recurse by calling builtin_eq
                        let result_thunk = builtin_eq(BuiltinArgs {
                            args: &[p1_thunk, p2_thunk],
                            named: None,
                            call_span,
                            ctx: Rc::clone(&ctx),
                        })?;
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
            // Helper to compare two values with cycle detection threaded through recursion
            fn values_eq_impl(
                left: &Value,
                right: &Value,
                ctx: &Rc<EvalContext>,
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
                    ) => Ok(&source_a[*start_a..*end_a] == &source_b[*start_b..*end_b]),
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
                        let right_map =
                            require_dict("=", right.clone(), call_span, ctx, call_span)?;

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

                            // Recurse with visited set threaded through - this is the key fix
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

            let mut visited = std::collections::HashSet::new();
            values_eq_impl(&left, &right, &ctx, call_span, &mut visited)?
        }
        // Function, Builtin are never equal
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("<", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // Runtime typeclass dispatch: check for Comparable instance before materializing,
    // so registered types don't pay double-materialization cost.
    if let Some(result) = try_dispatch_method("Comparable", "lt", args, named, call_span, &ctx) {
        return result;
    }

    let left = materialize(&args[0], Some(&call_span), &ctx)?;
    let right = materialize(&args[1], Some(&call_span), &ctx)?;

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
        ) => &source_a[*start_a..*end_a] < &source_b[*start_b..*end_b],
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("if", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    // Materialize only the condition
    let condition = materialize(&args[0], Some(&call_span), &ctx)?;

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

/// Helper to extract one numeric operand and convert to f64.
fn extract_single_float(
    name: &str,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<f64> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let val = materialize(&args[0], Some(&call_span), ctx)?;
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
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(f64, f64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let left = materialize(&args[0], Some(&call_span), ctx)?;
    let right = materialize(&args[1], Some(&call_span), ctx)?;

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
pub(crate) fn builtin_pow(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (base, exp) = extract_two_floats("pow", args, named, &ctx, call_span)?;
    check_float_result(base.powf(exp), "pow", call_span)
}

/// `sqrt`: Square root. Takes 1 numeric arg. Returns Float.
/// Allows NaN results (e.g., sqrt(-1)) for downstream predicates to check.
/// Inherently materializing: must extract numeric value to compute square root.
pub(crate) fn builtin_sqrt(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("sqrt", args, named, &ctx, call_span)?;
    ok_val(Value::Float(val.sqrt()), call_span)
}

/// `log`: Natural logarithm (ln). Takes 1 numeric arg. Returns Float.
/// Allows -Inf results (e.g., log(0)) for downstream predicates to check.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("log", args, named, &ctx, call_span)?;
    ok_val(Value::Float(val.ln()), call_span)
}

/// `log2`: Base-2 logarithm. Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log2(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("log2", args, named, &ctx, call_span)?;
    check_float_result(val.log2(), "log2", call_span)
}

/// `log10`: Base-10 logarithm. Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute logarithm.
pub(crate) fn builtin_log10(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("log10", args, named, &ctx, call_span)?;
    check_float_result(val.log10(), "log10", call_span)
}

/// `exp`: Exponential function (e^x). Takes 1 numeric arg. Returns Float.
/// Inherently materializing: must extract numeric value to compute exponential.
pub(crate) fn builtin_exp(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("exp", args, named, &ctx, call_span)?;
    check_float_result(val.exp(), "exp", call_span)
}

/// `sin`: Sine function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute sine.
pub(crate) fn builtin_sin(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("sin", args, named, &ctx, call_span)?;
    check_float_result(val.sin(), "sin", call_span)
}

/// `cos`: Cosine function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute cosine.
pub(crate) fn builtin_cos(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("cos", args, named, &ctx, call_span)?;
    check_float_result(val.cos(), "cos", call_span)
}

/// `tan`: Tangent function. Takes 1 numeric arg (radians). Returns Float.
/// Inherently materializing: must extract numeric value to compute tangent.
pub(crate) fn builtin_tan(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("tan", args, named, &ctx, call_span)?;
    check_float_result(val.tan(), "tan", call_span)
}

/// `asin`: Arcsine function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arcsine.
pub(crate) fn builtin_asin(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("asin", args, named, &ctx, call_span)?;
    check_float_result(val.asin(), "asin", call_span)
}

/// `acos`: Arccosine function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arccosine.
pub(crate) fn builtin_acos(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("acos", args, named, &ctx, call_span)?;
    check_float_result(val.acos(), "acos", call_span)
}

/// `atan`: Arctangent function. Takes 1 numeric arg. Returns Float (radians).
/// Inherently materializing: must extract numeric value to compute arctangent.
pub(crate) fn builtin_atan(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("atan", args, named, &ctx, call_span)?;
    check_float_result(val.atan(), "atan", call_span)
}

/// `atan2`: Two-argument arctangent (atan2(y, x)). Takes 2 numeric args. Returns Float (radians).
/// Inherently materializing: must extract numeric values to compute arctangent.
pub(crate) fn builtin_atan2(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (y, x) = extract_two_floats("atan2", args, named, &ctx, call_span)?;
    check_float_result(y.atan2(x), "atan2", call_span)
}

/// `nan?`: Checks if a float is NaN. Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for NaN.
pub(crate) fn builtin_nan_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("nan?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(val.is_nan()), call_span)
}

/// `inf?`: Checks if a float is infinite. Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for infinity.
pub(crate) fn builtin_inf_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("inf?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(val.is_infinite()), call_span)
}

/// `finite?`: Checks if a float is finite (not NaN or infinite). Takes 1 numeric arg. Returns Bool.
/// Inherently materializing: must extract numeric value to check for finiteness.
pub(crate) fn builtin_finite_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = extract_single_float("finite?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(val.is_finite()), call_span)
}

/// Helper to extract two Int operands, enforcing arity == 2 and type Int.
fn extract_int_pair(
    name: &str,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: crate::ast::Span,
) -> EvalResult<(i64, i64)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    reject_named(name, named, call_span)?;
    let left = materialize(&args[0], Some(&call_span), ctx)?;
    let right = materialize(&args[1], Some(&call_span), ctx)?;

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
pub(crate) fn builtin_band(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (a, b) = extract_int_pair("band", args, named, &ctx, call_span)?;
    ok_val(Value::Int(a & b), call_span)
}

/// `bor`: Bitwise OR. Takes 2 Int args. Returns Int.
/// Inherently materializing: must extract numeric values to compute bitwise OR.
pub(crate) fn builtin_bor(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (a, b) = extract_int_pair("bor", args, named, &ctx, call_span)?;
    ok_val(Value::Int(a | b), call_span)
}

/// `bxor`: Bitwise XOR. Takes 2 Int args. Returns Int.
/// Inherently materializing: must extract numeric values to compute bitwise XOR.
pub(crate) fn builtin_bxor(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (a, b) = extract_int_pair("bxor", args, named, &ctx, call_span)?;
    ok_val(Value::Int(a ^ b), call_span)
}

/// `shl`: Left shift. Takes 2 Int args (value, bits). Returns Int.
/// Inherently materializing: must extract numeric values to compute left shift.
pub(crate) fn builtin_shl(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (value, bits) = extract_int_pair("shl", args, named, &ctx, call_span)?;

    // Negative shift is undefined; shifts >= 64 produce 0
    if bits < 0 {
        return Err(
            EvalError::new(format!("shl: negative shift count {}", bits), call_span).into(),
        );
    }

    if bits >= 64 {
        return ok_val(Value::Int(0), call_span);
    }

    ok_val(Value::Int(value << bits), call_span)
}

/// `shr`: Logical right shift (zero-fill). Takes 2 Int args (value, bits). Returns Int.
/// Inherently materializing: must extract numeric values to compute right shift.
pub(crate) fn builtin_shr(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let (value, bits) = extract_int_pair("shr", args, named, &ctx, call_span)?;

    // Negative shift is undefined; shifts >= 64 produce 0
    if bits < 0 {
        return Err(
            EvalError::new(format!("shr: negative shift count {}", bits), call_span).into(),
        );
    }

    if bits >= 64 {
        return ok_val(Value::Int(0), call_span);
    }

    // Logical shift: cast to u64, shift, cast back to i64
    let result = ((value as u64) >> bits) as i64;
    ok_val(Value::Int(result), call_span)
}

/// `float`: Explicit Int→Float conversion without precision checking.
/// - Int → Float: cast via `as f64` (user explicitly opted into potential precision loss)
/// - Float → Float: no-op
/// - Other types → error
/// Inherently materializing: must inspect value to determine type and perform conversion.
pub(crate) fn builtin_float(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("float", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx)?;
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
}
