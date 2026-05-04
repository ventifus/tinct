//! Rust-native builtin functions for the LLT language.
//!
//! All builtins follow the `BuiltinFn` signature:
//! `fn(BuiltinArgs) -> EvalResult<Rc<Thunk>>`
//!
//! ## Builtin groups
//!
//! **Arithmetic:** `+`, `-`, `*`, `/` (with auto-promotion table)
//! **Comparison:** `=`, `<` (cross-type Int/Float comparison allowed)
//! **Control:** `if` (selective materialization -- only the chosen branch is forced)
//! **Dict primitives:** `keys`, `length`, `merge`, `append`
//! **Strings:** `str`, `split`, `replace`, `upper`, `lower`, `trim`
//! **Numeric:** `floor`, `round`
//! **Parsing:** `to-int`, `to-float`
//! **Evaluation control:** `eval`, `error`, `try`, `apply`
//! **Type introspection:** `type-of`, `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?`
//! **I/O:** `from-json`, `include`
//! **Sequences:** `seq`, `head`, `tail`, `collect`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `take`, `map`, `filter`, `drop`, `reduce`, `join`, `concat`

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::value::Strictness;
// Circular module dependency: this module imports `invoke_function` and `materialize` from eval.rs.
// eval.rs calls builtins via function pointers stored in `Value::Builtin`.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
// SAFETY: builtins.rs and eval.rs have a circular dependency at the value level — builtins call
// materialize/invoke_function (eval.rs), and eval calls standard_builtins (builtins.rs). This is
// safe because the dependency is at function-call level, not at module initialization level.
// Rust modules can call each other's pub functions after initialization without deadlock.
use crate::eval::{invoke_function, materialize, CallContext, MAX_EVAL_DEPTH};
use crate::value::{BuiltinArgs, Environment, Key, Thunk, Value};

/// Construct a `BuiltinDef` with name, function, and optional strictness annotations.
///
/// `builtin!("name", fn)` — all-lazy (empty strictness array).
/// `builtin!("name", fn, [Seq, Id])` — with explicit per-argument strictness.
///
/// The macro co-locates the string name with the function reference so that
/// grep/rename tools and code review catch mismatches that a plain tuple would
/// hide (e.g., `("keys", builtin_length)`).
///
/// For operator names (`+`, `-`, `*`, `/`) and hyphenated names (`to-int`) the
/// string literal must be written explicitly because they are not valid Rust identifiers.
macro_rules! builtin {
    // 2-arg form: all-lazy (empty strictness array)
    ($name:literal, $func:expr) => {{
        const S: &[crate::value::Strictness] = &[];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
        }
    }};
    // 3-arg form: with strictness array
    ($name:literal, $func:expr, [$($strictness:expr),* $(,)?]) => {{
        const S: &[crate::value::Strictness] = &[$($strictness),*];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
        }
    }};
}
pub(crate) use builtin;

/// Maximum collection size for $collect (1,000,000 elements).
/// Prevents memory exhaustion from infinite sequences without $take.
pub(crate) const MAX_COLLECT_SIZE: usize = 1_000_000;

/// Maximum string output size for string output builtins (`$replace`, `$upper`, `$lower`, `$join`) (64 MB).
/// Prevents memory exhaustion from adversarial inputs or replacement patterns.
pub(crate) const MAX_STRING_SIZE: usize = 64 * 1024 * 1024;

pub(crate) fn ok_val(v: Value, span: Span) -> EvalResult<Rc<Thunk>> {
    Ok(Rc::new(Thunk::new_materialized(v, span)))
}

/// Maximum file size for reading LLT files: 10 MB.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Helper: materialize a single positional argument, enforcing exact arity of 1
/// and rejecting named arguments. Used by many single-arg builtins.
pub(crate) fn expect_one_arg(
    name: &str,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<Value> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    materialize(&args[0], Some(&call_span), ctx, depth)
}

/// Helper: check that an f64 value is within the representable range of i64
/// before casting. Returns an error if the value is non-finite or would saturate.
pub(crate) fn checked_f64_to_i64(name: &str, f: f64, call_span: Span) -> EvalResult<i64> {
    if !f.is_finite() {
        return Err(EvalError::float_not_finite(name.to_string(), f, call_span).into());
    }
    if f < (i64::MIN as f64) || f >= (i64::MAX as f64) {
        return Err(EvalError::float_out_of_range(name.to_string(), f, call_span).into());
    }
    Ok(f as i64)
}

/// Helper: check that an f64 arithmetic result is finite.
///
/// Returns an error if `val` is NaN or infinite (overflow, e.g. `1e308 + 1e308`).
/// Used by float arithmetic builtins to prevent silent NaN/Infinity propagation.
pub(crate) fn check_float_result(val: f64, op: &str, span: Span) -> EvalResult<Rc<Thunk>> {
    if !val.is_finite() {
        Err(EvalError::float_not_finite(op.to_string(), val, span).into())
    } else {
        ok_val(Value::Float(val), span)
    }
}

/// Stringify a single materialized value for `str` builtin.
///
/// - Int -> decimal representation (e.g. `42`)
/// - Float -> decimal representation (e.g. `3.14`)
/// - String -> the string itself (no quotes)
/// - Bool -> `"true"` / `"false"`
/// - Dict, Function, Builtin -> delegated to `Value::Display`
pub(crate) fn stringify(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => format!("{other}"),
    }
}

/// Flatten a `Value::Overlay(L, R)` into an `IndexMap` by materializing both sides.
///
/// L entries are inserted first, then R entries overwrite on key collision (R wins).
/// Both L and R must materialize as `Value::Dict` or `Value::Overlay` (recursively).
/// Errors if either side materializes to a non-dict value.
///
/// **Iterative implementation:** Uses an explicit work stack to unwind deeply nested
/// `Overlay(Overlay(A, B), C)` chains without consuming Rust call stack depth.
/// Chains arise from stdlib accumulator patterns (e.g., `$remove`, `$from-entries`,
/// `$take-while`) that build a dict via repeated `$merge [acc] [entry]` calls.
/// A chain of depth N no longer overflows the Rust stack.
///
/// `name` is the builtin name for error messages. `ctx` and `depth` are for
/// materialization. `call_span` is used as the materialization span.
pub(crate) fn flatten_overlay(
    left: &Rc<Thunk>,
    right: &Rc<Thunk>,
    name: &str,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<IndexMap<Key, Rc<Thunk>>> {
    // Work stack: each entry is a thunk to materialize and add as a layer.
    // We push L before R so that when we pop, R is processed first (override wins).
    // But to maintain correct left-to-right override order, we collect layers and reverse.
    //
    // Algorithm:
    //   stack = [(left, false), (right, true)]  -- (thunk, is_override)
    //   layers = []
    //   while stack not empty:
    //     (thunk, is_override) = stack.pop()
    //     val = materialize(thunk)
    //     if val is Dict: layers.push((map, is_override))
    //     if val is Overlay(L, R): push (L, is_override) then (R, is_override)  [R on top → processed before L]
    //   result = apply layers left-to-right (each layer overwrites previous on collision)
    //
    // Stack ordering: push L first (processed later = base), R second (processed sooner = override).
    // We want final application order: L base first, R override second.
    // So collect into layers stack: L pushed first, R pushed on top.
    // When we pop for processing: R is processed first → appended to layers first.
    // Then reverse layers to get [L_base, ..., R_override] order.

    let mut work_stack: Vec<Rc<Thunk>> = Vec::new();
    // Push in reverse order: left first (processed last = base layer), right second (processed first = override).
    work_stack.push(Rc::clone(left));
    work_stack.push(Rc::clone(right));

    // Collect flat layers in processing order (right to left).
    let mut layers: Vec<IndexMap<Key, Rc<Thunk>>> = Vec::new();

    while let Some(thunk) = work_stack.pop() {
        let span = thunk.span;
        let val = materialize(&thunk, Some(&call_span), ctx, depth)?;
        match val {
            Value::Dict(map) => {
                layers.push(map);
            }
            Value::Overlay(l, r) => {
                // Unwind: push L first (base, processed later), R second (override, processed sooner).
                work_stack.push(l);
                work_stack.push(r);
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    name.to_string(),
                    "Dict",
                    other.type_name(),
                    span,
                )
                .into())
            }
        }
    }

    // layers is in processing order: [rightmost_override, ..., leftmost_base].
    // Reverse to get [leftmost_base, ..., rightmost_override] for correct application.
    layers.reverse();

    let total_cap = layers.iter().map(|m| m.len()).sum();
    let mut result: IndexMap<Key, Rc<Thunk>> = IndexMap::with_capacity(total_cap);
    for map in layers {
        for (key, thunk) in map {
            result.insert(key, thunk);
        }
    }
    Ok(result)
}

/// Helper: require that a materialized value is a Dict (or Overlay), returning the inner IndexMap.
/// Overlays are flattened on demand by materializing L and R and merging.
///
/// `def_span` should be the thunk's span (where the value was defined), not call_span.
/// `call_span` is used as the materialization site span for errors during flattening.
fn require_dict(
    name: &str,
    value: Value,
    def_span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<IndexMap<Key, Rc<Thunk>>> {
    match value {
        Value::Dict(map) => Ok(map),
        Value::Overlay(l, r) => flatten_overlay(&l, &r, name, ctx, depth, call_span),
        other => {
            let err =
                EvalError::type_mismatch_ctx(name.to_string(), "Dict", other.type_name(), def_span);
            // Secondary span would be redundant: def_span already points to where argument was produced.
            Err(err.into())
        }
    }
}

/// Helper: require that a materialized value is a String, returning the inner String.
/// `def_span` should be the thunk's span (where the value was defined), not call_span.
pub(crate) fn require_string(name: &str, value: Value, def_span: Span) -> EvalResult<String> {
    match value {
        Value::String(s) => Ok(s),
        other => {
            let err = EvalError::type_mismatch_ctx(
                name.to_string(),
                "String",
                other.type_name(),
                def_span,
            );
            // Secondary span would be redundant here since def_span is already the argument's span.
            // The caller passes args[N].span as def_span, which is where the value was produced.
            Err(err.into())
        }
    }
}

/// Helper: reject named arguments for multi-arg builtins that don't accept them.
pub(crate) fn reject_named(
    name: &str,
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    call_span: Span,
) -> EvalResult<()> {
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    Ok(())
}

// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
// Implementations live in builtins_math.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
pub(crate) use crate::builtins_math::{
    builtin_add, builtin_div_float, builtin_eq, builtin_if, builtin_lt, builtin_mul, builtin_sub,
};

/// `keys`: Takes 1 arg (a Dict). Returns a Dict with integer keys `0..n`
/// mapping to the key values (Int keys become Int values, String keys become
/// String values). Insertion order is preserved.
/// Inherently materializing: must access IndexMap to enumerate keys.
fn builtin_keys(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("keys", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    // arg[0] is pre-forced by BuiltinForceArg; this call is an O(1) cache hit.
    let val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let map = require_dict("keys", val, args[0].span, &ctx, depth, call_span)?;

    let origin = call_span;
    let mut result = IndexMap::with_capacity(map.len());
    for (i, (key, _)) in map.iter().enumerate() {
        let key_value = match key {
            Key::Int(n) => Value::Int(*n),
            Key::String(s) => Value::String(s.clone()),
        };
        result.insert(
            Key::Int(i64::try_from(i).map_err(|_| {
                EvalError::internal("collection index overflow".to_string(), call_span)
            })?),
            Rc::new(Thunk::new_materialized(key_value, origin)),
        );
    }
    ok_val(Value::Dict(result), call_span)
}

/// `length`: Takes 1 arg (a Dict). Returns an Int with the number of entries.
/// Inherently materializing: must access IndexMap to count entries.
fn builtin_length(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("length", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    // arg[0] is pre-forced by BuiltinForceArg; this call is an O(1) cache hit.
    let val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let map = require_dict("length", val, args[0].span, &ctx, depth, call_span)?;
    ok_val(Value::Int(map.len() as i64), call_span)
}

/// `merge`: Takes 2 args (both Dicts). Returns a lazy `Value::Overlay(L, R)` — R
/// overrides L on key collision. Construction is O(1): neither L nor R is
/// materialized at merge time. Flattening to an IndexMap is deferred until the
/// overlay is actually accessed (via `require_dict`, `value_to_json`, etc.).
///
/// Type validation (both args must be Dicts) is also deferred to flatten time,
/// which means type errors surface at access time rather than at call time.
/// This is the expected behavior for a lazy overlay.
fn builtin_merge(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    reject_named("merge", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // O(1): store thunk pointers without forcing either side.
    Ok(Rc::new(Thunk::new_materialized(
        Value::Overlay(Rc::clone(&args[0]), Rc::clone(&args[1])),
        call_span,
    )))
}

/// `append`: Takes 2 args: a Dict and any value. Returns a new dict with the
/// value inserted at the next integer key (one past the current maximum integer
/// key, or 0 for empty dicts / dicts with no integer keys).
///
/// This is O(n) for the clone but O(1) amortized for the insert itself,
/// compared to the old LLT `append` which did a full `merge` (copying the
/// entire accumulator into a new dict via two-dict iteration).
fn builtin_append(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("append", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // arg[0] is pre-forced by BuiltinForceArg; this call is an O(1) cache hit.
    // arg[1] (the value to append) is NOT materialized — it is inserted as a thunk
    // (Rc::clone at line below), preserving laziness of the appended value.
    let dict_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let mut map = require_dict("append", dict_val, args[0].span, &ctx, depth, call_span)?;

    // Compute the next integer key: max existing int key + 1, or 0 if none.
    let next_key = map
        .keys()
        .filter_map(|k| match k {
            Key::Int(n) => Some(*n),
            _ => None,
        })
        .max()
        .map(|max| {
            max.checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("append".to_string(), call_span))
        })
        .transpose()?
        .unwrap_or(0);

    map.insert(Key::Int(next_key), Rc::clone(&args[1]));
    ok_val(Value::Dict(map), call_span)
}

// String builtins: str, split, replace, upper, lower, trim.
// Implementations live in builtins_string.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
#[cfg(test)]
pub(crate) use crate::builtins_string::MAX_SPLIT_PARTS;
pub(crate) use crate::builtins_string::{
    builtin_lower, builtin_replace, builtin_split, builtin_str, builtin_trim, builtin_upper,
};

/// Shared helper for `floor` and `round`: takes a builtin name and an f64->f64
/// operation, materializes one numeric arg, and applies the operation to floats.
///
/// - Int input: returned unchanged.
/// - Float input: checks for NaN/Infinity, applies `op`, converts to `i64`.
/// - Non-numeric input: type error.
fn float_to_int_builtin(
    name: &str,
    op: fn(f64) -> f64,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    depth: usize,
    call_span: Span,
) -> EvalResult<Rc<Thunk>> {
    let val = expect_one_arg(name, args, named, ctx, depth, call_span)?;
    match val {
        Value::Int(n) => ok_val(Value::Int(n), call_span),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(EvalError::float_not_finite(name.to_string(), f, args[0].span).into());
            }
            ok_val(
                Value::Int(checked_f64_to_i64(name, op(f), call_span)?),
                call_span,
            )
        }
        other => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "Int or Float",
            other.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `floor`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::floor()` then converts to `i64`.
/// - NaN or Infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
/// Inherently materializing: must inspect numeric value to convert/round.
fn builtin_floor(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("floor", f64::floor, args, named, &ctx, depth, call_span)
}

/// `round`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::round()` (half-away-from-zero) then converts to `i64`.
/// - NaN or Infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
/// Inherently materializing: must inspect numeric value to convert/round.
fn builtin_round(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("round", f64::round, args, named, &ctx, depth, call_span)
}

/// `to-int`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as an integer via `str::parse::<i64>()`. Returns Int.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse integer value.
fn builtin_to_int(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-int", args, named, &ctx, depth, call_span)?;
    let s = require_string("to-int", val, args[0].span)?;
    match s.parse::<i64>() {
        Ok(n) => ok_val(Value::Int(n), call_span),
        Err(_) => {
            Err(
                EvalError::parse_conversion("to-int".to_string(), s.clone(), "Int", call_span)
                    .into(),
            )
        }
    }
}

/// `to-float`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as a float via `str::parse::<f64>()`. Returns Float.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse float value.
fn builtin_to_float(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-float", args, named, &ctx, depth, call_span)?;
    let s = require_string("to-float", val, args[0].span)?;
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => ok_val(Value::Float(f), call_span),
        Ok(_f) => Err(EvalError::internal(
            format!("to-float: \"{s}\" parses to a non-finite value (NaN/Infinity not allowed)"),
            call_span,
        )
        .into()),
        Err(_) => {
            Err(
                EvalError::parse_conversion("to-float".to_string(), s.clone(), "Float", call_span)
                    .into(),
            )
        }
    }
}

/// Recursively materialize a value: if it is a Dict, materialize every entry
/// value and recurse into nested dicts.
/// `eval`: takes 1 arg, deep-forces all thunks recursively.
/// Delegates to [`crate::eval::deep_materialize`].
/// Inherently materializing: deep-forces all thunks by definition.
fn builtin_eval(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("eval", args, named, &ctx, depth, call_span)?;
    let deep = crate::eval::deep_materialize(&val, &ctx, depth, Some(&call_span))?;
    ok_val(deep, call_span)
}

/// `error`: takes 1 arg (String message), always raises.
/// Inherently materializing: constructs concrete error value.
fn builtin_error(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("error", args, named, &ctx, depth, call_span)?;
    let msg = require_string("error", val, args[0].span)?;
    Err(EvalError::user_error(msg.to_string(), call_span).into())
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[ok: value]`
/// on success or `[err: message]` on failure.
/// Inherently materializing: must materialize body to catch errors.
fn builtin_try(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("try", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let func_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;

    let call_result = match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            if !params.is_empty() {
                return Err(EvalError::type_mismatch_ctx(
                    "try".to_string(),
                    "zero-argument function",
                    &format!("{}-parameter function", params.len()),
                    call_span,
                )
                .into());
            }
            // Evaluate the body in the closure's environment
            let body_thunk = Rc::new(Thunk::new_unevaluated(
                Rc::clone(&body),
                Rc::clone(&closure_env),
                Rc::clone(&ctx),
                body.span,
            ));
            materialize(&body_thunk, Some(&call_span), &ctx, depth)
        }
        Value::Builtin(def) => {
            let builtin_args = BuiltinArgs {
                args: &[],
                named: None,
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            };
            match (def.func)(builtin_args) {
                Ok(result_thunk) => materialize(&result_thunk, Some(&call_span), &ctx, depth),
                Err(e) => Err(e),
            }
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "try".to_string(),
                "Function",
                func_val.type_name(),
                call_span,
            )
            .into())
        }
    };

    match call_result {
        Ok(value) => {
            let mut result = IndexMap::with_capacity(1);
            result.insert(
                Key::String("ok".to_string()),
                Rc::new(Thunk::new_materialized(value, call_span)),
            );
            ok_val(Value::Dict(result), call_span)
        }
        Err(e) => {
            // Resource limit errors (DepthExceeded, ResourceLimitExceeded) must not be catchable by user code.
            // Re-raise instead of converting to err dict.
            if !e.kind.is_catchable() {
                return Err(e);
            }
            let mut result = IndexMap::with_capacity(1);
            result.insert(
                Key::String("err".to_string()),
                Rc::new(Thunk::new_materialized(
                    Value::String(e.message()),
                    call_span,
                )),
            );
            ok_val(Value::Dict(result), call_span)
        }
    }
}

/// `until`: Iterate a function until a predicate holds.
/// Takes 3 args: (pred, f, init)
/// Applies f repeatedly to init until pred(val) returns true, then returns val.
///
/// This is a Rust builtin to avoid the recursion depth limit of the LLT version.
/// The LLT recursive version hits MAX_EVAL_DEPTH at ~230 iterations.
///
/// This implementation uses a Rust loop with eager materialization at each step,
/// avoiding both depth limits and stack overflow from long thunk chains.
fn builtin_until(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("until", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    let pred_thunk = Rc::clone(&args[0]);
    let f_thunk = Rc::clone(&args[1]);
    let mut val_thunk = Rc::clone(&args[2]);

    loop {
        // Create a pending call to pred(val) and materialize it
        let pred_result = Rc::new(Thunk::new_pending_call(
            Rc::clone(&pred_thunk),
            vec![Rc::clone(&val_thunk)],
            IndexMap::new(),
            call_span,
            Rc::clone(&ctx.config.stdlib_env),
            val_thunk.span,
            Some(Rc::from("until")),
            Rc::clone(&ctx),
        ));

        let pred_val = materialize(&pred_result, Some(&call_span), &ctx, depth)?;

        match pred_val {
            Value::Bool(true) => {
                // Predicate holds, return the current value (as thunk)
                return Ok(val_thunk);
            }
            Value::Bool(false) => {
                // Predicate doesn't hold yet, apply f and materialize to get next value
                let f_result = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![val_thunk],
                    IndexMap::new(),
                    call_span,
                    Rc::clone(&ctx.config.stdlib_env),
                    call_span,
                    Some(Rc::from("until")),
                    Rc::clone(&ctx),
                ));

                // Eagerly materialize f(val) and re-wrap as a thunk for the next iteration
                // This breaks the thunk chain and prevents stack overflow
                let f_val = materialize(&f_result, Some(&call_span), &ctx, depth)?;
                val_thunk = Rc::new(Thunk::new_materialized(f_val, call_span));
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "until".to_string(),
                    "Bool",
                    pred_val.type_name(),
                    call_span,
                )
                .into())
            }
        }
    }
}

/// `apply`: takes 2 args (function, dict/list). Spreads the dict's values as
/// positional arguments to the function call.
///
/// For user-defined functions, delegates to `eval::invoke_function` so that
/// default values, named args, and variadics are handled identically to `call`.
/// Helper that performs the actual $apply logic after args are pre-materialized.
/// This is separated from builtin_apply so that builtin_apply can return a
/// PendingBuiltin thunk, enabling iterative arg materialization via BuiltinForceArg.
fn builtin_apply_impl(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("apply", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // Both args[0] and args[1] have been pre-materialized by BuiltinForceArg,
    // so these materialize() calls are O(1) cache hits.
    let func_val = materialize(&args[0], None, &ctx, depth)?;
    let args_val = materialize(&args[1], None, &ctx, depth)?;

    let arg_dict = require_dict("apply", args_val, args[1].span, &ctx, depth, call_span)?;

    // Split dict entries: integer-keyed → positional, string-keyed → named
    let mut int_entries: Vec<(i64, Rc<Thunk>)> = Vec::with_capacity(arg_dict.len());
    let mut named_args: IndexMap<String, Rc<Thunk>> = IndexMap::with_capacity(arg_dict.len());
    for (key, thunk) in &arg_dict {
        match key {
            Key::Int(n) => int_entries.push((*n, Rc::clone(thunk))),
            Key::String(s) => {
                named_args.insert(s.clone(), Rc::clone(thunk));
            }
        }
    }
    int_entries.sort_by_key(|(k, _)| *k);
    let positional: Vec<Rc<Thunk>> = int_entries.into_iter().map(|(_, v)| v).collect();

    match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => invoke_function(&CallContext {
            params: &params,
            body: &body,
            closure_env: &closure_env,
            positional: &positional,
            named: if named_args.is_empty() {
                None
            } else {
                Some(&named_args)
            },
            default_env: &closure_env,
            ctx: &ctx,
            call_span,
            depth,
            origin: Some(Rc::from("apply")),
        }),
        Value::Builtin(def) => {
            let builtin_args = BuiltinArgs {
                args: &positional,
                named: if named_args.is_empty() {
                    None
                } else {
                    Some(&named_args)
                },
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            };
            (def.func)(builtin_args)
        }
        _ => Err(EvalError::type_mismatch_ctx(
            "apply".to_string(),
            "Function",
            func_val.type_name(),
            call_span,
        )
        .into()),
    }
}

fn builtin_apply(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    // Return a PendingBuiltin thunk that wraps builtin_apply_impl.
    // When materialized, the PendingBuiltin handler will use BuiltinForceArg
    // to pre-materialize both args[0] and args[1] iteratively, avoiding
    // Rust stack growth.
    // Pass named args through: $apply may forward named args to the target function.
    // Use None when named is empty to skip the IndexMap allocation.
    let named_opt = if named.map(|n| n.is_empty()).unwrap_or(true) {
        None
    } else {
        Some(named.expect("checked by if condition above").clone())
    };
    Ok(Rc::new(Thunk::new_pending_builtin(
        builtin!("apply", builtin_apply_impl),
        args.to_vec(),
        named_opt,
        depth,
        call_span,
        Some(Rc::from("apply")),
        ctx,
    )))
}

/// `type-of`: takes 1 arg, materializes it, returns the type name.
/// Both `Function` and `Builtin` return "Function" (from the user's perspective).
/// Returns "Dict" for all dicts, with no distinction between list-like (sequential int keys)
/// and map-like dicts — the type system does not track key structure at runtime.
/// Inherently materializing: must inspect value variant to determine type.
fn builtin_type_of(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("type-of", args, named, &ctx, depth, call_span)?;
    let name = match val.type_name() {
        "Builtin" => "Function",
        other => other,
    };
    ok_val(Value::String(name.to_string()), call_span)
}

/// `int?`: Return true if the argument is an Int.
fn builtin_int_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("int?", args, named, &ctx, depth, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Int(_))), call_span)
}

/// `float?`: Return true if the argument is a Float.
fn builtin_float_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("float?", args, named, &ctx, depth, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Float(_))), call_span)
}

/// `num?`: Return true if the argument is an Int or Float.
fn builtin_num_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("num?", args, named, &ctx, depth, call_span)?;
    ok_val(
        Value::Bool(matches!(val, Value::Int(_) | Value::Float(_))),
        call_span,
    )
}

/// `str?`: Return true if the argument is a String.
fn builtin_str_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("str?", args, named, &ctx, depth, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::String(_))), call_span)
}

/// `bool?`: Return true if the argument is a Bool.
fn builtin_bool_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("bool?", args, named, &ctx, depth, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Bool(_))), call_span)
}

/// `null?`: Return true if the argument is Null (represented as an empty Dict).
fn builtin_null_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("null?", args, named, &ctx, depth, call_span)?;
    let is_null = match val {
        Value::Dict(map) => map.is_empty(),
        Value::Overlay(l, r) => {
            let map = flatten_overlay(&l, &r, "null?", &ctx, depth, call_span)?;
            map.is_empty()
        }
        _ => false,
    };
    ok_val(Value::Bool(is_null), call_span)
}

/// `dict?`: Return true if the argument is a Dict (including lists and null).
fn builtin_dict_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("dict?", args, named, &ctx, depth, call_span)?;
    ok_val(
        Value::Bool(matches!(val, Value::Dict(_) | Value::Overlay(..))),
        call_span,
    )
}

/// `fn?`: Return true if the argument is callable (Function or Builtin).
fn builtin_fn_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("fn?", args, named, &ctx, depth, call_span)?;
    ok_val(
        Value::Bool(matches!(val, Value::Function { .. } | Value::Builtin(_))),
        call_span,
    )
}

/// Convert a `serde_json::Value` into an LLT `Value`.
///
/// JSON null maps to an empty dict, arrays map to integer-keyed dicts,
/// and objects map to string-keyed dicts. Numbers are converted to `Int`
/// when they fit in i64, otherwise `Float`.
pub fn json_to_value(json: &serde_json::Value, depth: usize, span: Span) -> EvalResult<Rc<Thunk>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::json_depth_exceeded(MAX_EVAL_DEPTH, span).into());
    }
    match json {
        serde_json::Value::Null => ok_val(Value::Dict(IndexMap::new()), span),
        serde_json::Value::Bool(b) => ok_val(Value::Bool(*b), span),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ok_val(Value::Int(i), span)
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    // JSON does not support NaN or Infinity, but some parsers
                    // (or manual serde_json::Number construction) can produce
                    // non-finite values. Reject them explicitly.
                    Err(EvalError::float_not_finite("from-json".to_string(), f, span).into())
                } else {
                    ok_val(Value::Float(f), span)
                }
            } else {
                // Unreachable with default serde_json: as_f64() covers all
                // non-i64 numbers. Return error instead of panicking.
                Err(EvalError::json_range(span).into())
            }
        }
        serde_json::Value::String(s) => ok_val(Value::String(s.clone()), span),
        serde_json::Value::Array(arr) => {
            if arr.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: array exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let thunk = json_to_value(item, depth + 1, span)?;
                map.insert(
                    Key::Int(i64::try_from(i).map_err(|_| {
                        EvalError::internal("collection index overflow".to_string(), span)
                    })?),
                    thunk,
                );
            }
            ok_val(Value::Dict(map), span)
        }
        serde_json::Value::Object(obj) => {
            if obj.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: object exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(obj.len());
            for (k, v) in obj {
                let thunk = json_to_value(v, depth + 1, span)?;
                map.insert(Key::String(k.clone()), thunk);
            }
            ok_val(Value::Dict(map), span)
        }
    }
}

/// `from-json`: takes 1 arg (String containing JSON), parses into LLT value.
/// Inherently materializing: must parse entire JSON string to construct value.
fn builtin_from_json(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("from-json", args, named, &ctx, depth, call_span)?;
    let json_str = require_string("from-json", val, args[0].span)?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| EvalError::json_parse(e.to_string(), call_span))?;
    json_to_value(&parsed, depth, call_span)
}

/// Parse an integrity hash string of the form `"algo:hexdigest"`.
///
/// Returns `(algo, hex)` on success. Only `"blake3"` is currently supported.
/// Validates that the algorithm is known and the digest is the correct length and format.
fn parse_integrity_hash(s: &str, call_span: Span) -> EvalResult<(&str, &str)> {
    let Some((algo, hex)) = s.split_once(':') else {
        return Err(EvalError::include_io_error(
            s.to_string(),
            "integrity hash must be \"algo:hexdigest\" (e.g. \"blake3:abc123...\")".to_string(),
            call_span,
        )
        .into());
    };
    match algo {
        "blake3" => {
            // BLAKE3 output is 32 bytes = 64 hex chars.
            if hex.len() != 64 {
                return Err(EvalError::include_io_error(
                    s.to_string(),
                    format!("blake3 digest must be 64 hex characters, got {}", hex.len()),
                    call_span,
                )
                .into());
            }
            if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err(EvalError::include_io_error(
                    s.to_string(),
                    "blake3 digest must contain only hex characters (0-9, a-f, A-F)".to_string(),
                    call_span,
                )
                .into());
            }
        }
        other => {
            return Err(EvalError::include_io_error(
                s.to_string(),
                format!("unsupported hash algorithm \"{other}\"; supported: blake3"),
                call_span,
            )
            .into());
        }
    }
    Ok((algo, hex))
}

/// Compute the blake3 hash of `bytes` and return a lowercase hex string.
fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// `include`: takes 1 or 2 args (path, optional "algo:hexdigest"), evaluates the file,
/// returns its result. The optional second argument is an integrity hash; if provided,
/// the file bytes are hashed and compared to the expected digest before evaluation.
///
/// Path resolution: relative paths are resolved against the including file's
/// directory. Absolute paths are used as-is. Cycle detection prevents A→B→A
/// circular includes. The included file gets an empty `%` and sees the stdlib
/// environment but NOT the caller's scope.
///
/// ## Argument strictness
///
/// - `args[0]` (path): **strict** — materialized immediately before any filesystem
///   access. The path string must be known before the file can be opened.
/// - `args[1]` (integrity hash, optional): **strict** — materialized immediately after
///   `args[0]` so the hash string is available for comparison against the file bytes.
///
/// Both arguments are forced eagerly; `$include` does not participate in lazy evaluation
/// of its path. This is intentional: lazily resolving the path would defer filesystem
/// errors and make cycle detection unreliable.
fn builtin_include(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;

    // Check if filesystem access is disabled before doing anything else.
    if ctx.config.no_fs {
        return Err(EvalError::include_forbidden(call_span).into());
    }

    // Accept 1 or 2 positional args; reject named args.
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    reject_named("include", named, call_span)?;

    let path_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let file_path_str = require_string("include", path_val, args[0].span)?;

    // Parse optional integrity hash from the second argument.
    // owned_hash = Some((algo, hexdigest)) when a hash was provided.
    let owned_hash: Option<(String, String)> = if args.len() == 2 {
        let hash_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
        let hash_str = require_string("include", hash_val, args[1].span)?;
        parse_integrity_hash(&hash_str, call_span)?; // validates format
        let colon_pos = hash_str.find(':').unwrap(); // safe: validated above
        Some((
            hash_str[..colon_pos].to_string(),
            hash_str[colon_pos + 1..].to_string(),
        ))
    } else {
        None
    };

    // Enforce --require-integrity: every $include must supply a hash.
    if ctx.config.require_integrity && owned_hash.is_none() {
        return Err(EvalError::include_hash_required(file_path_str.clone(), call_span).into());
    }

    // Open the file using cap-std. Absolute paths are rejected by cap-std (RESOLVE_BENEATH).
    let base_dir = &ctx.config.base_dir;
    let fd = base_dir.open(&file_path_str).map_err(|e| {
        EvalError::include_io_error(file_path_str.clone(), e.to_string(), call_span)
    })?;

    // Allowlist check: if allowed_paths is non-empty, the canonical path of the
    // included file must be a descendant of at least one allowed root.
    // This check runs after cap-std has already confirmed the file is within base_dir
    // (RESOLVE_BENEATH), so canonicalize() here is safe — the file exists and is accessible.
    if !ctx.config.allowed_paths.is_empty() {
        let canonical = base_dir.canonicalize(&file_path_str).map_err(|e| {
            EvalError::include_io_error(file_path_str.clone(), e.to_string(), call_span)
        })?;
        let permitted = ctx
            .config
            .allowed_paths
            .iter()
            .any(|allowed| canonical.starts_with(allowed));
        if !permitted {
            return Err(
                EvalError::include_path_not_allowed(file_path_str.clone(), call_span).into(),
            );
        }
    }

    // Get metadata from the fd (single operation, no TOCTOU).
    let metadata = fd.metadata().map_err(|e| {
        EvalError::include_io_error(file_path_str.clone(), e.to_string(), call_span)
    })?;

    // File-type guard: only regular files are allowed.
    if !metadata.is_file() {
        return Err(EvalError::include_io_error(
            file_path_str.clone(),
            "not a regular file".to_string(),
            call_span,
        )
        .into());
    }

    // Check file size.
    if metadata.len() > MAX_FILE_SIZE {
        return Err(EvalError::include_file_too_large(
            file_path_str.clone(),
            metadata.len(),
            MAX_FILE_SIZE,
            call_span,
        )
        .into());
    }

    // Get file identity (dev, ino) for cycle detection and caching.
    // On Unix, we can get these from metadata. On non-Unix, fall back to path-based approach.
    #[cfg(unix)]
    let file_id = {
        use cap_std::fs::MetadataExt;
        (metadata.dev(), metadata.ino())
    };

    #[cfg(not(unix))]
    let file_id = {
        // On non-Unix platforms, fall back to a hash of the file path as a best-effort identity.
        // This is not ideal (doesn't detect hardlinks) but better than nothing.
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        file_path_str.hash(&mut hasher);
        let hash = hasher.finish();
        (0u64, hash)
    };

    // Cache lookup: skip when a hash is provided (must read bytes to verify integrity).
    if owned_hash.is_none() {
        if let Some(cached) = ctx.state.borrow().include_cache.get(&file_id) {
            return Ok(Rc::clone(cached));
        }
    }

    // Cycle detection: check if this file is currently being evaluated.
    if ctx.state.borrow().include_guard.contains(&file_id) {
        return Err(EvalError::include_cycle(
            format!("{}  (dev={}, ino={})", file_path_str, file_id.0, file_id.1),
            call_span,
        )
        .into());
    }

    // Read the file bytes from the fd.
    use std::io::Read;
    let mut bytes = Vec::new();
    let mut file_handle = fd;
    file_handle.read_to_end(&mut bytes).map_err(|e| {
        EvalError::include_io_error(file_path_str.clone(), e.to_string(), call_span)
    })?;

    // Integrity check: verify hash before parsing or evaluating.
    if let Some((_algo, expected_hex)) = &owned_hash {
        let actual_hex = blake3_hex(&bytes);
        // Case-insensitive hex comparison (user may provide uppercase).
        if !actual_hex.eq_ignore_ascii_case(expected_hex) {
            return Err(EvalError::include_hash_mismatch(
                file_path_str.clone(),
                format!("blake3:{expected_hex}"),
                format!("blake3:{actual_hex}"),
                call_span,
            )
            .into());
        }
        // Hash verified — return cached evaluation if available.
        if let Some(cached) = ctx.state.borrow().include_cache.get(&file_id) {
            return Ok(Rc::clone(cached));
        }
    }

    // Convert bytes to UTF-8 source.
    let source = String::from_utf8(bytes).map_err(|e| {
        EvalError::include_io_error(
            file_path_str.clone(),
            format!("file is not valid UTF-8: {e}"),
            call_span,
        )
    })?;

    // Parse.
    let mut file = crate::parser::parse(&source).map_err(|e| {
        EvalError::include_parse_failed(file_path_str.clone(), e.to_string(), call_span)
    })?;

    // Desugar $_ implicit lambdas (pre-typecheck and pre-eval AST transformation).
    crate::desugar::desugar_file(&mut file.node);

    // Determine the parent directory for the included file.
    // We need to open a new Dir for relative includes within the included file.
    // This is done BEFORE inserting into the guard/chain so that if open_dir fails,
    // no cleanup is needed.
    let parent_path = std::path::Path::new(&file_path_str).parent();
    let included_dir = if let Some(pp) = parent_path.filter(|p| !p.as_os_str().is_empty()) {
        // Open a subdirectory relative to base_dir
        base_dir.open_dir(pp).map_err(|e| {
            EvalError::include_io_error(
                format!("{} (parent directory)", file_path_str),
                e.to_string(),
                call_span,
            )
        })?
    } else {
        // No parent directory means the file is in base_dir itself
        // We need to clone the Dir handle. cap-std Dir doesn't implement Clone,
        // so we reopen it using try_clone() or by opening "." relative to base_dir.
        base_dir.open_dir(".").map_err(|e| {
            EvalError::include_io_error(
                format!("{} (reopen base_dir)", file_path_str),
                e.to_string(),
                call_span,
            )
        })?
    };

    // Create a new EvalContext with the included file's directory.
    let included_ctx = ctx.with_base_dir(included_dir);

    let stdlib_env = Rc::clone(&ctx.config.stdlib_env);

    // Add to include guard and include chain before recursing.
    // The include chain records (file_path, call_span) for each active $include frame.
    // On error, the chain is prepended to the error's stack frames so the user sees
    // the full include path ("included from a.llt at 3:10 → included from b.llt at 1:5").
    {
        let mut state = ctx.state.borrow_mut();
        state.include_guard.insert(file_id);
        state.include_chain.push((file_path_str.clone(), call_span));
    }

    // Evaluate the included file with empty % and the stdlib env.
    let eval_result = crate::eval::eval_file(&file.node, stdlib_env, &included_ctx, depth + 1);

    // Remove from include guard and include chain regardless of success/failure.
    let cleanup = || {
        let mut state = ctx.state.borrow_mut();
        state.include_guard.remove(&file_id);
        state.include_chain.pop();
    };

    match eval_result {
        Ok(thunk) => {
            // Eagerly materialize: the include guard is only valid while
            // the file's identity is in the set. Returning a lazy thunk
            // would defer evaluation past the guard removal.
            let val = match crate::eval::materialize(&thunk, None, &included_ctx, depth + 1) {
                Ok(v) => {
                    cleanup();
                    v
                }
                Err(mut e) => {
                    // Prepend this include frame to the error's stack so nested errors
                    // show the full include path. Each $include level inserts its own
                    // frame at position 0 as the error propagates outward, producing
                    // outermost-first ordering in the final stack trace.
                    cleanup();
                    e.stack.insert(
                        0,
                        crate::error::StackFrame {
                            label: format!("included from {file_path_str}"),
                            span: call_span,
                        },
                    );
                    return Err(e);
                }
            };
            // Preserve the span from the included file's root expression
            let result_thunk = Rc::new(Thunk::new_materialized(val, thunk.span));

            // Cache the result thunk for future includes of this file.
            ctx.state
                .borrow_mut()
                .include_cache
                .insert(file_id, Rc::clone(&result_thunk));

            Ok(result_thunk)
        }
        Err(mut e) => {
            // Prepend this include frame to the error's stack so nested errors
            // show the full include path. Each $include level inserts its own
            // frame at position 0 as the error propagates outward, producing
            // outermost-first ordering in the final stack trace.
            cleanup();
            e.stack.insert(
                0,
                crate::error::StackFrame {
                    label: format!("included from {file_path_str}"),
                    span: call_span,
                },
            );
            Err(e)
        }
    }
}

// Seq primitive builtins: seq, head, tail, collect, seq?.
// Implementations live in builtins_seq_prim.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
pub(crate) use crate::builtins_seq_prim::{
    builtin_collect, builtin_head, builtin_seq, builtin_seq_check, builtin_tail,
};

// Sequence generator builtins: range, repeat, cycle, iterate, unfold.
// Implementations live in builtins_seq_gen.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
pub(crate) use crate::builtins_seq_gen::{
    builtin_cycle, builtin_iterate, builtin_range, builtin_repeat, builtin_unfold,
};

// Sequence transform builtins: map, filter, take, drop.
// Implementations live in builtins_seq_xform.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
// The step helpers (filter_dict_step, filter_seq_step, drop_seq_step) are only
// used in test code via `use super::*`, not in production code in this file, so
// suppress the unused-import lint for this block.
#[allow(unused_imports)]
pub(crate) use crate::builtins_seq_xform::{
    builtin_drop, builtin_drop_seq_step, builtin_filter, builtin_filter_dict_step,
    builtin_filter_seq_step, builtin_map, builtin_take,
};
// Sequence reduction builtins: reduce, join, concat.
// Implementations live in builtins_seq_reduce.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
// The step helpers (reduce_seq_step, concat_seq_step) are only used via test
// `use super::*`, not directly in this file, so suppress the unused-import lint.
#[allow(unused_imports)]
pub(crate) use crate::builtins_seq_reduce::{
    builtin_concat, builtin_concat_seq_step, builtin_join, builtin_reduce, builtin_reduce_seq_step,
};

/// `rest`: Returns all elements of a collection except the first, reindexed 0..n-1.
///
/// - Takes 1 arg: a Dict or Seq.
/// - Seq path: O(1) — delegates to `$tail` (returns the Seq's tail directly).
/// - Dict path: O(n) — drops the first entry by insertion order, rebuilds with dense
///   integer keys starting at 0. Same asymptotic cost as the LLT implementation, but
///   avoids interpreter loop overhead.
/// Inherently materializing for Dict: must copy all remaining entries.
/// Lazy for Seq: O(1) tail extraction.
fn builtin_rest(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("rest", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    // Seq path: delegate to $tail (O(1), preserves laziness).
    if matches!(val, Value::Seq { .. }) {
        return builtin_tail(BuiltinArgs {
            args,
            named,
            depth,
            call_span,
            ctx,
        });
    }
    let map = require_dict("rest", val, args[0].span, &ctx, depth, call_span)?;

    // Skip the first entry (index 0 by insertion order), reindex rest as 0..n-1.
    let mut result = IndexMap::with_capacity(map.len().saturating_sub(1));
    for (new_idx, (_old_key, thunk)) in map.into_iter().skip(1).enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        result.insert(new_key, thunk);
    }
    ok_val(Value::Dict(result), call_span)
}

/// `cons`: Prepend an element to a collection, reindexing all entries from 0.
///
/// - Takes 2 args: (element, collection).
/// - Seq path: O(1) — delegates to `$seq x xs` (returns a lazy Seq).
/// - Dict path: O(n) — builds a new dict with the element at key 0, followed by
///   the existing entries reindexed as 1..n. Same asymptotic cost as the LLT
///   implementation, but avoids interpreter loop overhead.
/// Inherently materializing for Dict: must copy all existing entries.
/// Lazy for Seq: O(1) prepend.
fn builtin_cons(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cons", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // args[0] is the element to prepend (kept as thunk — preserves laziness).
    // args[1] is the collection to prepend to (must be materialized to dispatch on type).
    let xs_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    // Seq path: delegate to $seq (O(1), preserves laziness).
    if matches!(xs_val, Value::Seq { .. }) {
        return builtin_seq(BuiltinArgs {
            args,
            named,
            depth,
            call_span,
            ctx,
        });
    }
    let map = require_dict("cons", xs_val, args[1].span, &ctx, depth, call_span)?;

    let mut result = IndexMap::with_capacity(map.len() + 1);
    // Insert the new element at key 0.
    result.insert(Key::Int(0), Rc::clone(&args[0]));
    // Insert existing entries reindexed as 1..n.
    for (new_idx, (_old_key, thunk)) in map.into_iter().enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx + 1).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        result.insert(new_key, thunk);
    }
    ok_val(Value::Dict(result), call_span)
}

/// `reverse`: Reverse the entries of a dict list, reindexing from 0.
///
/// - Takes 1 arg: a Dict.
/// - Materializes the dict, collects entries in reverse insertion order,
///   builds a new dict with dense integer keys 0..n-1.
/// - O(n) — avoids the recursive LLT accumulator pattern.
/// Inherently materializing: must know all entries to reverse order.
fn builtin_reverse(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reverse", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let map = require_dict("reverse", val, args[0].span, &ctx, depth, call_span)?;

    let mut result = IndexMap::with_capacity(map.len());
    // Collect values in reverse insertion order.
    let entries: Vec<_> = map.into_iter().collect();
    for (new_idx, (_old_key, thunk)) in entries.into_iter().rev().enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        result.insert(new_key, thunk);
    }
    ok_val(Value::Dict(result), call_span)
}

/// Compare two materialized `Value`s for sort ordering.
///
/// Mirrors the `<` builtin semantics:
/// - Int vs Int, Float vs Float, Int/Float cross-type (promote Int to f64)
/// - String vs String (lexicographic)
/// - Bool vs Bool (false < true)
/// - Mixed incompatible types: returns `Err` (type error).
///
/// Returns `Ok(std::cmp::Ordering)` on success, `Err` on incompatible types.
fn compare_values(a: &Value, b: &Value, call_span: Span) -> EvalResult<std::cmp::Ordering> {
    let result = match (a, b) {
        (Value::Int(x), Value::Int(y)) => x.cmp(y),
        (Value::Float(x), Value::Float(y)) => x.partial_cmp(y).unwrap_or(std::cmp::Ordering::Equal),
        (Value::Int(x), Value::Float(y)) => (*x as f64)
            .partial_cmp(y)
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::Float(x), Value::Int(y)) => x
            .partial_cmp(&(*y as f64))
            .unwrap_or(std::cmp::Ordering::Equal),
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "sort".to_string(),
                "Int, Float, String, or Bool (homogeneous collection)",
                &format!("{} and {}", a.type_name(), b.type_name()),
                call_span,
            )
            .into());
        }
    };
    Ok(result)
}

/// `sort`: Sort a dict list by natural ordering.
///
/// - Takes 1 arg: a Dict (list-like, integer-keyed).
/// - Materializes all values, sorts by natural ordering (same semantics as `<`).
/// - O(n log n) using Rust's `sort_by` with the `compare_values` helper.
/// - Errors on mixed incompatible types (e.g. Int and String in same collection).
/// - Errors on Seq input (callers must `$collect` first).
/// Inherently materializing: must inspect all values to determine sort order.
fn builtin_sort(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("sort", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let map = require_dict("sort", val, args[0].span, &ctx, depth, call_span)?;

    // Materialize all values so we can compare them.
    let mut pairs: Vec<(Value, Span)> = Vec::with_capacity(map.len());
    for (_key, thunk) in &map {
        let mat = materialize(thunk, Some(&call_span), &ctx, depth)?;
        pairs.push((mat, thunk.span));
    }

    // Sort by natural ordering. Collect any comparison error.
    let mut sort_error: Option<Box<crate::error::EvalError>> = None;
    pairs.sort_by(|(a, _), (b, _)| {
        if sort_error.is_some() {
            return std::cmp::Ordering::Equal;
        }
        match compare_values(a, b, call_span) {
            Ok(ord) => ord,
            Err(e) => {
                sort_error = Some(e);
                std::cmp::Ordering::Equal
            }
        }
    });
    if let Some(e) = sort_error {
        return Err(e);
    }

    // Build result dict with dense integer keys 0..n-1, wrapping sorted values as thunks.
    let mut result = IndexMap::with_capacity(pairs.len());
    for (new_idx, (mat_val, orig_span)) in pairs.into_iter().enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        result.insert(
            new_key,
            Rc::new(Thunk::new_materialized(mat_val, orig_span)),
        );
    }
    ok_val(Value::Dict(result), call_span)
}

fn builtin_proxy(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ..
    } = ctx_arg;
    reject_named("proxy", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    Ok(Rc::new(Thunk::new_materialized(
        Value::Proxy {
            handler: Rc::clone(&args[0]),
        },
        call_span,
    )))
}

/// Returns all builtin definitions with strictness metadata.
///
/// All builtins conform to the standard `BuiltinFn` signature, including `if`
/// which materializes only the chosen branch (the unchosen branch's thunk is
/// never forced, preserving lazy semantics).
pub fn standard_builtins() -> Vec<crate::value::BuiltinDef> {
    vec![
        // Arithmetic
        builtin!("+", builtin_add, [Strictness::Seq, Strictness::Seq]),
        builtin!("-", builtin_sub, [Strictness::Seq, Strictness::Seq]),
        builtin!("*", builtin_mul, [Strictness::Seq, Strictness::Seq]),
        builtin!("/", builtin_div_float, [Strictness::Seq, Strictness::Seq]),
        // Comparison
        builtin!("=", builtin_eq, [Strictness::Seq, Strictness::Seq]),
        builtin!("<", builtin_lt, [Strictness::Seq, Strictness::Seq]),
        // Control
        builtin!(
            "if",
            builtin_if,
            [Strictness::Seq, Strictness::Id, Strictness::Id]
        ),
        // Dict primitives
        builtin!("keys", builtin_keys, [Strictness::Spine]),
        builtin!("length", builtin_length, [Strictness::Spine]),
        builtin!("merge", builtin_merge),
        builtin!("append", builtin_append, [Strictness::Seq, Strictness::Id]),
        // Strings
        builtin!("str", builtin_str, [Strictness::Seq]),
        builtin!("split", builtin_split, [Strictness::Seq, Strictness::Seq]),
        builtin!(
            "replace",
            builtin_replace,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!("upper", builtin_upper, [Strictness::Seq]),
        builtin!("lower", builtin_lower, [Strictness::Seq]),
        builtin!("trim", builtin_trim, [Strictness::Seq]),
        // Numeric
        builtin!("floor", builtin_floor, [Strictness::Seq]),
        builtin!("round", builtin_round, [Strictness::Seq]),
        // Parsing
        builtin!("to-int", builtin_to_int, [Strictness::Seq]),
        builtin!("to-float", builtin_to_float, [Strictness::Seq]),
        // Evaluation control
        builtin!("eval", builtin_eval, [Strictness::Seq]),
        builtin!("error", builtin_error, [Strictness::Seq]),
        builtin!("try", builtin_try, [Strictness::Id]),
        builtin!("apply", builtin_apply, [Strictness::Seq, Strictness::Seq]),
        builtin!("until", builtin_until),
        // Type introspection
        builtin!("type-of", builtin_type_of, [Strictness::Seq]),
        builtin!("int?", builtin_int_check, [Strictness::Seq]),
        builtin!("float?", builtin_float_check, [Strictness::Seq]),
        builtin!("num?", builtin_num_check, [Strictness::Seq]),
        builtin!("str?", builtin_str_check, [Strictness::Seq]),
        builtin!("bool?", builtin_bool_check, [Strictness::Seq]),
        builtin!("null?", builtin_null_check, [Strictness::Seq]),
        builtin!("dict?", builtin_dict_check, [Strictness::Seq]),
        builtin!("fn?", builtin_fn_check, [Strictness::Seq]),
        // I/O
        builtin!("from-json", builtin_from_json, [Strictness::Seq]),
        builtin!("include", builtin_include, [Strictness::Seq]),
        // Sequences
        builtin!("seq", builtin_seq),
        builtin!("head", builtin_head, [Strictness::Seq]),
        builtin!("tail", builtin_tail, [Strictness::Seq]),
        builtin!("collect", builtin_collect, [Strictness::Spine]),
        builtin!("seq?", builtin_seq_check, [Strictness::Seq]),
        builtin!("range", builtin_range, [Strictness::Seq, Strictness::Seq]),
        builtin!("repeat", builtin_repeat),
        builtin!("cycle", builtin_cycle, [Strictness::Spine]),
        builtin!("iterate", builtin_iterate),
        builtin!("unfold", builtin_unfold),
        builtin!("map", builtin_map, [Strictness::Id, Strictness::Spine]),
        builtin!(
            "filter",
            builtin_filter,
            [Strictness::Id, Strictness::Spine]
        ),
        builtin!("take", builtin_take, [Strictness::Seq, Strictness::Spine]),
        builtin!("drop", builtin_drop, [Strictness::Seq, Strictness::Spine]),
        builtin!(
            "reduce",
            builtin_reduce,
            [Strictness::Id, Strictness::Id, Strictness::Spine]
        ),
        builtin!("join", builtin_join, [Strictness::Seq, Strictness::Spine]),
        builtin!(
            "concat",
            builtin_concat,
            [Strictness::Spine, Strictness::Seq]
        ),
        // List operations (moved from LLT stdlib to Rust for performance)
        builtin!("rest", builtin_rest, [Strictness::Spine]),
        builtin!("cons", builtin_cons, [Strictness::Id, Strictness::Spine]),
        builtin!("reverse", builtin_reverse, [Strictness::Spine]),
        builtin!("sort", builtin_sort, [Strictness::Spine]),
        // Proxy
        builtin!("proxy", builtin_proxy),
    ]
}

/// Create the root environment with all builtins registered as `Value::Builtin`.
pub fn create_root_env() -> Rc<RefCell<Environment>> {
    let env = Rc::new(RefCell::new(Environment::new()));
    for def in standard_builtins() {
        let thunk = Rc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
        env.borrow_mut().insert(def.name.to_string(), thunk);
    }

    // Add stable "builtin-*" aliases for operators that will be shadowed by prelude wrappers.
    // These provide an escape hatch to the raw Rust implementations.
    let aliases: Vec<crate::value::BuiltinDef> = vec![
        builtin!("builtin-lt", builtin_lt, [Strictness::Seq, Strictness::Seq]),
        builtin!("builtin-eq", builtin_eq, [Strictness::Seq, Strictness::Seq]),
        builtin!(
            "builtin-add",
            builtin_add,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-sub",
            builtin_sub,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-mul",
            builtin_mul,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-div",
            builtin_div_float,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-if",
            builtin_if,
            [Strictness::Seq, Strictness::Id, Strictness::Id]
        ),
        builtin!(
            "builtin-filter",
            builtin_filter,
            [Strictness::Id, Strictness::Spine]
        ),
        builtin!(
            "builtin-map",
            builtin_map,
            [Strictness::Id, Strictness::Spine]
        ),
        builtin!(
            "builtin-reduce",
            builtin_reduce,
            [Strictness::Id, Strictness::Id, Strictness::Spine]
        ),
        builtin!(
            "builtin-take",
            builtin_take,
            [Strictness::Seq, Strictness::Spine]
        ),
        builtin!(
            "builtin-drop",
            builtin_drop,
            [Strictness::Seq, Strictness::Spine]
        ),
    ];

    for def in aliases {
        let thunk = Rc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
        env.borrow_mut().insert(def.name.to_string(), thunk);
    }

    env
}

/// Create the stdlib environment: root builtins + prelude functions.
///
/// Parses and evaluates `stdlib/prelude.llt` using the root env, then
/// layers the prelude dict entries as a child scope. User code should
/// use this as the parent environment.
// Fatal: stdlib failure is not recoverable — callers should propagate or panic on Err.
pub fn create_stdlib_env() -> Result<Rc<RefCell<Environment>>, Box<crate::error::EvalError>> {
    let root_env = create_root_env();

    // Create a bootstrap EvalContext with just the root env (before stdlib is loaded)
    let bootstrap_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| {
            Box::new(crate::error::EvalError::internal(
                format!("cannot open bootstrap base_dir: {e}"),
                Span::origin(),
            ))
        })?;
    let bootstrap_ctx =
        crate::eval::EvalContext::new(bootstrap_base_dir, Rc::clone(&root_env), false);

    let prelude_source = include_str!("../stdlib/prelude.llt");
    let mut file = crate::parser::parse(prelude_source).map_err(|e| {
        crate::error::EvalError::internal(format!("prelude parse error: {e}"), Span::origin())
    })?;

    crate::desugar::desugar_file(&mut file.node);

    // Type errors are advisory; evaluation proceeds regardless.
    let _ = crate::typecheck::typecheck_file(&file.node);

    let thunk = crate::eval::eval_file(&file.node, Rc::clone(&root_env), &bootstrap_ctx, 0)?;

    let val = crate::eval::materialize(&thunk, None, &bootstrap_ctx, 0)?;

    let dict = match val {
        Value::Dict(map) => map,
        Value::Overlay(l, r) => {
            flatten_overlay(&l, &r, "prelude", &bootstrap_ctx, 0, Span::origin())?
        }
        other => {
            return Err(crate::error::EvalError::internal(
                format!("prelude must evaluate to a Dict, got {}", other.type_name()),
                Span::origin(),
            )
            .into())
        }
    };

    // Create a child env with the prelude entries
    let stdlib_env = Rc::new(RefCell::new(Environment::with_parent(root_env)));
    for (key, thunk) in dict {
        let name = match key {
            Key::String(s) => s,
            Key::Int(n) => n.to_string(),
        };
        stdlib_env.borrow_mut().insert(name, thunk);
    }

    Ok(stdlib_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Param, Spanned};
    use crate::error::ErrorKind;
    use crate::test_util::test_span;
    use crate::value::Strictness;

    /// Stack size for tests that exercise deep recursive evaluation chains.
    /// The default Rust test thread stack (8 MB) is too small for tests that push
    /// MAX_EVAL_DEPTH (256) levels of PendingBuiltin thunks; 16 MB provides headroom.
    const TEST_STACK_SIZE: usize = 128 * 1024 * 1024; // 128 MB — debug-mode materialize() needs ~100MB at 256 levels

    /// Helper: wrap a Value in a materialized Thunk inside an Rc.
    fn thunk(val: Value) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    fn thunk_with_span(val: Value, span: Span) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(val, span))
    }

    fn no_named() -> Option<&'static IndexMap<String, Rc<Thunk>>> {
        None
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Rc<crate::eval::EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        crate::eval::EvalContext::new(base_dir, create_root_env(), false)
    }

    fn mat(result: EvalResult<Rc<Thunk>>) -> Value {
        crate::eval::materialize(&result.unwrap(), None, &test_ctx(), 0).unwrap()
    }

    /// Helper: make a zero-arg function whose body is a single expression.
    fn zero_arg_fn(body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
        }
    }

    /// Helper: make an n-arg function whose body is a given expression.
    fn n_arg_fn(param_names: &[&str], body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(
                param_names
                    .iter()
                    .map(|name| Param {
                        name: name.to_string(),
                        annotation: None,
                        variadic: false,
                    })
                    .collect(),
            ),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
        }
    }

    fn thunk_dict(map: IndexMap<Key, Rc<Thunk>>) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Dict(map),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: flatten a Value (Dict or Overlay) to an IndexMap for test assertions.
    /// Since `builtin_merge` now returns `Value::Overlay` (lazy), tests that previously
    /// expected `Value::Dict` must use this helper to get the concrete entries.
    fn flatten_val(val: Value) -> IndexMap<Key, Rc<Thunk>> {
        match val {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => {
                flatten_overlay(&l, &r, "test", &test_ctx(), 0, test_span(1, 1, 1, 5)).unwrap()
            }
            other => panic!("expected Dict or Overlay, got {other:?}"),
        }
    }

    #[test]
    fn floor_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn floor_negative_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn floor_zero_int() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn floor_positive_float() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(3.7))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn floor_negative_float() {
        // floor(-3.2) = -4, not -3
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(-3.2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-4));
    }

    #[test]
    fn floor_float_exact_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(5.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn floor_float_just_below_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(2.9999999))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn floor_nan_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("NaN"), "got: {}", err.message());
    }

    #[test]
    fn floor_positive_infinity_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_negative_infinity_errors() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_string_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::String("3.5".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_bool_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_dict_type_error() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_wrong_arity_zero() {
        let err = builtin_floor(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_wrong_arity_two() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(3.5))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_large_positive_float_out_of_range() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(1e19))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("out of range for Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn floor_large_negative_float_out_of_range() {
        let err = builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Float(-1e19))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("out of range for Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn round_negative_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn round_positive_half_rounds_up() {
        // 0.5 rounds to 1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(0.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(1));
    }

    #[test]
    fn round_negative_half_rounds_down() {
        // -0.5 rounds to -1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-0.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-1));
    }

    #[test]
    fn round_positive_below_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(2.4))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_positive_above_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(2.6))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn round_negative_below_half() {
        // -2.4 rounds to -2
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-2.4))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_negative_above_half() {
        // -2.6 rounds to -3
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-2.6))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-3));
    }

    #[test]
    fn round_1_5_rounds_to_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(1.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_negative_1_5_rounds_to_negative_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-1.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_float_exact_integer() {
        let result = mat(builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(5.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn round_nan_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("NaN"), "got: {}", err.message());
    }

    #[test]
    fn round_positive_infinity_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_negative_infinity_errors() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite number"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_string_type_error() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::String("3.5".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_bool_type_error() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Int or Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_wrong_arity_zero() {
        let err = builtin_round(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_wrong_arity_two() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_large_positive_float_out_of_range() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(1e19))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("out of range for Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn round_large_negative_float_out_of_range() {
        let err = builtin_round(BuiltinArgs {
            args: &[thunk(Value::Float(-1e19))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("out of range for Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_valid_positive() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn to_int_valid_negative() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("-7".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn to_int_valid_zero() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("0".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn to_int_valid_large() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("9223372036854775807".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[test]
    fn to_int_invalid_float_string() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_text() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_empty() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_invalid_with_spaces() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String(" 42 ".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_int_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("Int"),
            "should mention Int, got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_float_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_bool_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_dict_input() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_wrong_arity_zero() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_wrong_arity_two() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[
                thunk(Value::String("1".into())),
                thunk(Value::String("2".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_valid_decimal() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(42.0));
    }

    #[test]
    fn to_float_valid_negative() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("-2.5".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(-2.5));
    }

    #[test]
    fn to_float_valid_scientific_notation() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("1.5e10".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[test]
    fn to_float_valid_negative_exponent() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("2.5e-3".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[test]
    fn to_float_valid_zero() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("0.0".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.0));
    }

    #[test]
    fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String(".5".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.5));
    }

    #[test]
    fn to_float_invalid_text() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_invalid_empty() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_inf() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("inf".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("parses to a non-finite value"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_negative_inf() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("-inf".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("parses to a non-finite value"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_infinity() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("infinity".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("parses to a non-finite value"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_nan() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("NaN".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("parses to a non-finite value"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_int_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_float_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_bool_input() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_wrong_arity_zero() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_wrong_arity_two() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[
                thunk(Value::String("1.0".into())),
                thunk(Value::String("2.0".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("1.0".into())));
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_overflow() {
        // One past i64::MAX
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("9223372036854775808".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot parse"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eval_primitive_int() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn eval_primitive_string() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn eval_primitive_float() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn eval_primitive_bool() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn eval_empty_dict() {
        let dict = Value::Dict(IndexMap::new());
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_flat_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let dict = Value::Dict(map);
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = materialize(&map[&Key::String("a".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = materialize(&map[&Key::String("b".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(b, Value::Int(2));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_nested_dict() {
        // Build [x: [y: 42]]
        let mut inner = IndexMap::new();
        inner.insert(Key::String("y".into()), thunk(Value::Int(42)));
        let inner_dict = Value::Dict(inner);

        let mut outer = IndexMap::new();
        outer.insert(Key::String("x".into()), thunk(inner_dict));
        let outer_dict = Value::Dict(outer);

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(outer_dict)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(outer_map) => {
                let x_val = materialize(&outer_map[&Key::String("x".into())], None, &test_ctx(), 0)
                    .unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val =
                            materialize(&inner_map[&Key::String("y".into())], None, &test_ctx(), 0)
                                .unwrap();
                        assert_eq!(y_val, Value::Int(42));
                    }
                    _ => panic!("expected inner Dict"),
                }
            }
            _ => panic!("expected outer Dict"),
        }
    }

    #[test]
    fn eval_with_unevaluated_thunk() {
        // Create an unevaluated thunk wrapping a literal -- eval should force it
        let expr = Rc::new(Spanned::new(Expr::Int(99), test_span(1, 1, 1, 5)));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(
            expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));

        let mut map = IndexMap::new();
        map.insert(Key::String("val".into()), unevaluated);
        let dict = Value::Dict(map);

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(dict)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let v =
                    materialize(&map[&Key::String("val".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_arity_error() {
        let err = builtin_eval(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn error_raises_with_message() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("boom".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert_eq!(err.message(), "boom");
    }

    #[test]
    fn error_custom_message() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("division by zero".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert_eq!(err.message(), "division by zero");
    }

    #[test]
    fn error_type_mismatch_on_non_string() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("String"), "got: {}", err.message());
    }

    #[test]
    fn error_arity_check() {
        let err = builtin_error(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_success_returns_ok_dict() {
        // [fn [] 42]
        let func = zero_arg_fn(Expr::Int(42));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("ok".into())));
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::Int(42));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_success_with_string_body() {
        let func = zero_arg_fn(Expr::Str("hello".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::String("hello".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_failure_returns_err_dict() {
        // [fn [] $nonexistent] -- references an undefined variable
        let func = zero_arg_fn(Expr::VarRef("nonexistent".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert!(map.contains_key(&Key::String("err".into())));
                let err_val =
                    materialize(&map[&Key::String("err".into())], None, &test_ctx(), 0).unwrap();
                match err_val {
                    Value::String(msg) => {
                        assert!(
                            msg.contains("undefined variable"),
                            "expected 'undefined variable' in error message, got: {msg}"
                        );
                    }
                    _ => panic!("expected String error message"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_non_function_type_error() {
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected Function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_non_zero_arg_function_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("zero-argument function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_arity_check() {
        let err = builtin_try(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_with_builtin_success() {
        fn ok_builtin(_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            ok_val(Value::Int(99), Span::origin())
        }
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: ok_builtin,
            name: "ok",
            pos_strictness: &[],
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let ok_val =
                    materialize(&map[&Key::String("ok".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(ok_val, Value::Int(99));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_with_builtin_failure() {
        fn err_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::internal("builtin error".to_string(), call_span).into())
        }
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: err_builtin,
            name: "fail",
            pos_strictness: &[],
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let err_val =
                    materialize(&map[&Key::String("err".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(err_val, Value::String("builtin error".into()));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn try_depth_exceeded_not_catchable() {
        // DepthExceeded errors should NOT be caught by $try - they should propagate
        // NOTE: No corpus test exists for this because triggering DepthExceeded
        // reliably requires either a custom builtin (not available in corpus tests)
        // or recursive thunk forcing with 16MB stack (impractical in corpus format).
        fn depth_exceeded_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::depth_exceeded(256, call_span).into())
        }
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: depth_exceeded_builtin,
            name: "depth_fail",
            pos_strictness: &[],
        });
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "expected depth error to propagate, got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E040");
    }

    #[test]
    fn try_resource_limit_exceeded_not_catchable() {
        // ResourceLimitExceeded errors should NOT be caught by $try - they should propagate
        fn resource_limit_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::resource_limit_exceeded(
                "test: exceeded resource limit (1000000)".to_string(),
                call_span,
            )
            .into())
        }
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: resource_limit_builtin,
            name: "resource_fail",
            pos_strictness: &[],
        });
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.message().contains("exceeded resource limit"),
            "expected resource limit error to propagate, got: {}",
            err.message()
        );
        assert_eq!(err.kind.code(), "E043");
    }

    #[test]
    fn apply_single_arg() {
        // [fn [x] $x] applied to [42]
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(42)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn apply_multiple_args_returns_first() {
        // [fn [a b] $a] applied to [10, 20]
        let func = n_arg_fn(&["a", "b"], Expr::VarRef("a".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(10)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(20)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(10));
    }

    #[test]
    fn apply_multiple_args_returns_second() {
        // [fn [a b] $b] applied to [10, 20]
        let func = n_arg_fn(&["a", "b"], Expr::VarRef("b".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(10)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(20)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn apply_with_builtin() {
        fn add_builtin(builtin_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs {
                args,
                call_span,
                ctx,
                ..
            } = builtin_ctx;
            let a = materialize(&args[0], None, &ctx, 0)?;
            let b = materialize(&args[1], None, &ctx, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(x + y), call_span),
                _ => Err(EvalError::type_mismatch("Int", "non-Int", call_span).into()),
            }
        }
        let func = Value::Builtin(crate::value::BuiltinDef {
            func: add_builtin,
            name: "add",
            pos_strictness: &[],
        });
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(3)));
        arg_dict.insert(Key::Int(1), thunk(Value::Int(4)));
        let args_val = Value::Dict(arg_dict);

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn apply_arity_mismatch() {
        let func = n_arg_fn(&["x", "y"], Expr::VarRef("x".into()));
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_function_type_error() {
        let mut arg_dict = IndexMap::new();
        arg_dict.insert(Key::Int(0), thunk(Value::Int(1)));
        let args_val = Value::Dict(arg_dict);

        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(args_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("expected Function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_dict_args_type_error() {
        let func = n_arg_fn(&["x"], Expr::VarRef("x".into()));
        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_wrong_arity() {
        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn type_of_int() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Int".into()));
    }

    #[test]
    fn type_of_float() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Float".into()));
    }

    #[test]
    fn type_of_string() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::String("hi".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("String".into()));
    }

    #[test]
    fn type_of_bool() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Bool".into()));
    }

    #[test]
    fn type_of_dict() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Dict".into()));
    }

    #[test]
    fn type_of_function() {
        let func = zero_arg_fn(Expr::Int(0));
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn type_of_builtin_returns_function() {
        fn dummy(_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            ok_val(Value::Int(0), Span::origin())
        }
        let builtin = Value::Builtin(crate::value::BuiltinDef {
            func: dummy,
            name: "dummy",
            pos_strictness: &[],
        });
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(builtin)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Function".into()));
    }

    #[test]
    fn test_type_of_seq() {
        // Seq values should report type name "Seq" from $type-of
        let seq = Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        };
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(seq)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Seq".into()));
    }

    #[test]
    fn type_of_arity_check() {
        let err = builtin_type_of(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_int() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn from_json_float() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("3.14".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn from_json_string() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(r#""hello""#.into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn from_json_bool_true() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("true".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn from_json_bool_false() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("false".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn from_json_null_becomes_empty_dict() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("null".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict for null"),
        }
    }

    #[test]
    fn from_json_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("[1, 2, 3]".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::Int(2));
                let v2 = materialize(&map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v2, Value::Int(3));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_object() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(
                r#"{"name": "Alice", "age": 30}"#.into(),
            ))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let name =
                    materialize(&map[&Key::String("name".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(name, Value::String("Alice".into()));
                let age =
                    materialize(&map[&Key::String("age".into())], None, &test_ctx(), 0).unwrap();
                assert_eq!(age, Value::Int(30));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_nested_structure() {
        let json = r#"{"users": [{"name": "Bob"}, {"name": "Eve"}]}"#;
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(json.into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                let users =
                    materialize(&map[&Key::String("users".into())], None, &test_ctx(), 0).unwrap();
                match users {
                    Value::Dict(arr) => {
                        assert_eq!(arr.len(), 2);
                        let user0 = materialize(&arr[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                        match user0 {
                            Value::Dict(u) => {
                                let name = materialize(
                                    &u[&Key::String("name".into())],
                                    None,
                                    &test_ctx(),
                                    0,
                                )
                                .unwrap();
                                assert_eq!(name, Value::String("Bob".into()));
                            }
                            _ => panic!("expected Dict for user"),
                        }
                    }
                    _ => panic!("expected Dict for users array"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_invalid_json() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("{bad json".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("invalid JSON"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_non_string_type_error() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_arity_check() {
        let err = builtin_from_json(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_empty_object() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("{}".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_empty_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("[]".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            _ => panic!("expected empty Dict"),
        }
    }

    #[test]
    fn from_json_mixed_array() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String(r#"[1, "two", true, null]"#.into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                let v0 = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::Int(1));
                let v1 = materialize(&map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::String("two".into()));
                let v2 = materialize(&map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(v2, Value::Bool(true));
                let v3 = materialize(&map[&Key::Int(3)], None, &test_ctx(), 0).unwrap();
                match v3 {
                    Value::Dict(m) => assert!(m.is_empty()),
                    _ => panic!("expected empty Dict for null"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. json_to_value recursion depth matches MAX_EVAL_DEPTH; verify depth guard policy only."]
    fn from_json_depth_guard() {
        // Build JSON nested beyond MAX_EVAL_DEPTH: {"a":{"a":{...}}}
        // serde_json's default recursion limit is 128, so we test json_to_value
        // directly with a pre-parsed serde_json::Value.
        fn build_deep(depth: usize) -> serde_json::Value {
            let mut val = serde_json::Value::Object(serde_json::Map::new());
            for _ in 0..depth {
                let mut obj = serde_json::Map::new();
                obj.insert("a".into(), val);
                val = serde_json::Value::Object(obj);
            }
            val
        }
        let deep = build_deep(MAX_EVAL_DEPTH + 1);
        let err = json_to_value(&deep, 0, call_span()).unwrap_err();
        assert!(
            err.message()
                .contains("maximum JSON nesting depth exceeded"),
            "expected depth error, got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_finite_float_accepted() {
        // serde_json::Number::from_f64 returns None for NaN/Inf, so we cannot
        // construct a non-finite serde_json Number through the public API.
        // The is_finite() guard in json_to_value is defensive against
        // non-standard parsers or direct serde_json::Number construction.
        // Verify that a normal finite float passes through correctly.
        let result = mat(json_to_value(
            &serde_json::Value::Number(serde_json::Number::from_f64(3.14).expect("finite")),
            0,
            call_span(),
        ));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn keys_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_int_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        map.insert(Key::Int(2), thunk(Value::String("c".into())));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                for i in 0..3 {
                    let val = materialize(&keys_map[&Key::Int(i)], None, &test_ctx(), 0).unwrap();
                    assert_eq!(val, Value::Int(i));
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_string_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(
            Key::String("name".into()),
            thunk(Value::String("Alice".into())),
        );
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 2);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::String("name".into()));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k1, Value::String("age".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_mixed_key_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("first".into())));
        map.insert(
            Key::String("label".into()),
            thunk(Value::String("second".into())),
        );
        map.insert(Key::Int(5), thunk(Value::String("third".into())));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::Int(0));
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k1, Value::String("label".into()));
                let k2 = materialize(&keys_map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k2, Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_preserves_insertion_order() {
        let mut map = IndexMap::new();
        map.insert(Key::String("z".into()), thunk(Value::Int(1)));
        map.insert(Key::String("a".into()), thunk(Value::Int(2)));
        map.insert(Key::String("m".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(keys_map) => {
                let k0 = materialize(&keys_map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
                let k1 = materialize(&keys_map[&Key::Int(1)], None, &test_ctx(), 0).unwrap();
                let k2 = materialize(&keys_map[&Key::Int(2)], None, &test_ctx(), 0).unwrap();
                assert_eq!(k0, Value::String("z".into()));
                assert_eq!(k1, Value::String("a".into()));
                assert_eq!(k2, Value::String("m".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn length_empty_dict() {
        let dict = thunk_dict(IndexMap::new());
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_non_empty_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn length_int_keyed_dict() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("x".into())));
        map.insert(Key::Int(1), thunk(Value::String("y".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn merge_disjoint_keys() {
        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), thunk(Value::Int(1)));
        left.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("c".into()), thunk(Value::Int(3)));
        right.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // builtin_merge now returns Value::Overlay; flatten to verify contents.
        let map = flatten_val(result);
        assert_eq!(map.len(), 4);
        assert!(map.contains_key(&Key::String("a".into())));
        assert!(map.contains_key(&Key::String("b".into())));
        assert!(map.contains_key(&Key::String("c".into())));
        assert!(map.contains_key(&Key::String("d".into())));
    }

    #[test]
    fn merge_overlapping_keys_right_wins() {
        let mut left = IndexMap::new();
        left.insert(Key::String("x".into()), thunk(Value::Int(1)));
        left.insert(Key::String("y".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("y".into()), thunk(Value::Int(99)));
        right.insert(Key::String("z".into()), thunk(Value::Int(3)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let map = flatten_val(result);
        assert_eq!(map.len(), 3);
        let x = materialize(&map[&Key::String("x".into())], None, &test_ctx(), 0).unwrap();
        assert_eq!(x, Value::Int(1));
        let y = materialize(&map[&Key::String("y".into())], None, &test_ctx(), 0).unwrap();
        assert_eq!(y, Value::Int(99)); // R overrides L
        let z = materialize(&map[&Key::String("z".into())], None, &test_ctx(), 0).unwrap();
        assert_eq!(z, Value::Int(3));
    }

    #[test]
    fn merge_empty_dicts() {
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(IndexMap::new()), thunk_dict(IndexMap::new())],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let map = flatten_val(result);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn builtin_def_strictness_array_validity() {
        // Verify all BuiltinDef entries have reasonable strictness arrays
        for def in standard_builtins() {
            // No builtin should have more than 10 positional arguments
            assert!(
                def.pos_strictness.len() <= 10,
                "builtin '{}' has {} strictness entries (max 10)",
                def.name,
                def.pos_strictness.len()
            );
            // All strictness values should be valid variants
            for (idx, &s) in def.pos_strictness.iter().enumerate() {
                match s {
                    Strictness::Id | Strictness::Seq | Strictness::Spine => {
                        // Valid
                    }
                }
                // The match above will fail to compile if a new variant is added
                // without updating this test (because the match has no wildcard arm).
                let _ = (idx, s); // Silence unused variable warning
            }
        }
    }

    #[test]
    fn merge_left_empty() {
        let mut right = IndexMap::new();
        right.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(IndexMap::new()), thunk_dict(right)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let map = flatten_val(result);
        assert_eq!(map.len(), 1);
        let v = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
        assert_eq!(v, Value::String("only".into()));
    }

    #[test]
    fn merge_right_empty() {
        let mut left = IndexMap::new();
        left.insert(Key::Int(0), thunk(Value::String("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(IndexMap::new())],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let map = flatten_val(result);
        assert_eq!(map.len(), 1);
        let v = materialize(&map[&Key::Int(0)], None, &test_ctx(), 0).unwrap();
        assert_eq!(v, Value::String("only".into()));
    }

    #[test]
    fn merge_preserves_thunks() {
        // With lazy overlay, the original thunks are preserved as-is (Rc::clone)
        // when the overlay is flattened.
        let span = test_span(1, 1, 1, 5);
        let left_thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let right_thunk = Rc::new(Thunk::new_materialized(Value::Int(99), span));

        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), Rc::clone(&left_thunk));
        let mut right = IndexMap::new();
        right.insert(Key::String("b".into()), Rc::clone(&right_thunk));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // Flatten and verify thunk identity is preserved through the overlay.
        let map = flatten_val(result);
        assert!(Rc::ptr_eq(&map[&Key::String("a".into())], &left_thunk));
        assert!(Rc::ptr_eq(&map[&Key::String("b".into())], &right_thunk));
    }

    #[test]
    fn merge_preserves_left_order() {
        let mut left = IndexMap::new();
        left.insert(Key::String("b".into()), thunk(Value::Int(1)));
        left.insert(Key::String("a".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("d".into()), thunk(Value::Int(3)));
        right.insert(Key::String("c".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left), thunk_dict(right)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let map = flatten_val(result);
        let keys: Vec<&Key> = map.keys().collect();
        assert_eq!(
            keys,
            vec![
                &Key::String("b".into()),
                &Key::String("a".into()),
                &Key::String("d".into()),
                &Key::String("c".into()),
            ]
        );
    }

    #[test]
    fn keys_wrong_arity_zero() {
        let err = builtin_keys(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_keys(BuiltinArgs {
            args: &[d.clone(), d],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_wrong_arity_zero() {
        let err = builtin_length(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_wrong_arity_two() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_length(BuiltinArgs {
            args: &[d.clone(), d],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_wrong_arity_one() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[d],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_wrong_arity_three() {
        let d = thunk_dict(IndexMap::new());
        let err = builtin_merge(BuiltinArgs {
            args: &[d.clone(), d.clone(), d],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_non_dict_int() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn keys_non_dict_string() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(
            err.message().contains("got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_non_dict_bool() {
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn length_non_dict() {
        let err = builtin_length(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("length"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_first_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let d = thunk_dict(IndexMap::new());
        let result = builtin_merge(BuiltinArgs {
            args: &[thunk(Value::Int(1)), d],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        // builtin_merge itself succeeds — returns Overlay(Int(1), {})
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize(&overlay_thunk, None, &test_ctx(), 0).unwrap();
        // Flatten fires the type error: left side is Int, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err =
                    flatten_overlay(&l, &r, "merge", &test_ctx(), 0, call_span()).unwrap_err();
                assert!(
                    err.message().contains("expected Dict"),
                    "got: {}",
                    err.message()
                );
                assert!(err.message().contains("got Int"), "got: {}", err.message());
            }
            other => panic!("expected Overlay, got {other:?}"),
        }
    }

    #[test]
    fn merge_second_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let d = thunk_dict(IndexMap::new());
        let result = builtin_merge(BuiltinArgs {
            args: &[d, thunk(Value::String("nope".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize(&overlay_thunk, None, &test_ctx(), 0).unwrap();
        // Flatten fires the type error: right side is String, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err =
                    flatten_overlay(&l, &r, "merge", &test_ctx(), 0, call_span()).unwrap_err();
                assert!(
                    err.message().contains("expected Dict"),
                    "got: {}",
                    err.message()
                );
                assert!(
                    err.message().contains("got String"),
                    "got: {}",
                    err.message()
                );
            }
            other => panic!("expected Overlay, got {other:?}"),
        }
    }

    #[test]
    fn append_to_empty_dict() {
        let empty = thunk_dict(IndexMap::new());
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let val =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_existing_list() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("a".into())));
        map.insert(Key::Int(1), thunk(Value::String("b".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::String("c".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val =
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_string_keys_only() {
        // Dict with only string keys -- next int key should be 0
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(99))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let val =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_gap_in_int_keys() {
        // Dict with keys 0, 5 -- next key should be 6 (max + 1)
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Int(10)));
        map.insert(Key::Int(5), thunk(Value::Int(50)));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(60))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val =
                    materialize(map.get(&Key::Int(6)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(val, Value::Int(60));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_existing_entries() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::String("first".into())));
        let dict = thunk_dict(map);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::String("second".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let first =
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(first, Value::String("first".into()));
                let second =
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(second, Value::String("second".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_value_stays_as_thunk() {
        // The value arg is not materialized -- it's inserted as a thunk
        let empty = thunk_dict(IndexMap::new());
        let val_thunk = thunk(Value::Int(7));
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, Rc::clone(&val_thunk)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                // The inserted thunk should be the same Rc (not a copy)
                assert!(Rc::ptr_eq(map.get(&Key::Int(0)).unwrap(), &val_thunk));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_wrong_arity_zero() {
        let err = builtin_append(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_wrong_arity_three() {
        let err = builtin_append(BuiltinArgs {
            args: &[
                thunk_dict(IndexMap::new()),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_first_arg_non_dict() {
        let err = builtin_append(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("append"), "got: {}", err.message());
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn append_key_overflow_at_i64_max() {
        let mut map = IndexMap::new();
        map.insert(Key::Int(i64::MAX), thunk(Value::Int(1)));
        let dict = thunk_dict(map);
        let err = builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn str_no_args() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_single_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("42".into()));
    }

    #[test]
    fn str_single_negative_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("-7".into()));
    }

    #[test]
    fn str_single_float() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("3.14".into()));
    }

    #[test]
    fn str_single_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn str_single_bool_true() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("true".into()));
    }

    #[test]
    fn str_single_bool_false() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("false".into()));
    }

    #[test]
    fn str_single_dict() {
        let mut map = IndexMap::new();
        map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(
                Value::Int(1),
                test_span(1, 1, 1, 5),
            )),
        );
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("[x: <thunk>]".into()));
    }

    #[test]
    fn str_single_empty_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn str_concat_multiple_strings() {
        let args = vec![
            thunk(Value::String("Hello".into())),
            thunk(Value::String(" ".into())),
            thunk(Value::String("World".into())),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("Hello World".into()));
    }

    #[test]
    fn str_concat_mixed_types() {
        let args = vec![
            thunk(Value::String("count: ".into())),
            thunk(Value::Int(42)),
            thunk(Value::String(", ratio: ".into())),
            thunk(Value::Float(3.14)),
            thunk(Value::String(", ok: ".into())),
            thunk(Value::Bool(true)),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(
            result,
            Value::String("count: 42, ratio: 3.14, ok: true".into())
        );
    }

    #[test]
    fn split_basic() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b,c".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_parts() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,,b".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v1, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_single_char_separator() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String("/".into())),
                thunk(Value::String("a/b/c/d".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 4),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_no_match() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("hello".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("hello".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_multi_char_separator() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String("::".into())),
                thunk(Value::String("a::b::c".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v1 = materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap();
                let v2 = materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("a".into()));
                assert_eq!(v1, Value::String("b".into()));
                assert_eq!(v2, Value::String("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_input() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
                assert_eq!(v0, Value::String("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_parts_limit_exceeded() {
        // Splitting "a" repeated MAX_SPLIT_PARTS+1 times by empty separator produces
        // MAX_SPLIT_PARTS+2 parts, which exceeds the limit.
        // Verifies that ResourceLimitExceeded is returned and that the error fires
        // after at most MAX_SPLIT_PARTS+1 allocations (not after the full split).
        // Note: corpus tests for this would be impractical (require >1M element inputs),
        // so we test the limit directly in unit tests.
        let input = "a".repeat(MAX_SPLIT_PARTS + 1);
        let result = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String("".into())), thunk(Value::String(input))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err(), "expected Err for > MAX_SPLIT_PARTS parts");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ResourceLimitExceeded { .. }),
            "expected ResourceLimitExceeded, got {:?}",
            err.kind
        );
    }

    #[test]
    fn split_parts_at_limit_succeeds() {
        // Splitting a string that produces exactly MAX_SPLIT_PARTS parts must succeed
        // (guard is `>`, not `>=`). Construct "a,a,a,...,a" with MAX_SPLIT_PARTS items
        // separated by commas, then split by "," — produces exactly MAX_SPLIT_PARTS parts.
        let input = vec!["a"; MAX_SPLIT_PARTS].join(",");
        let result = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String(input)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let val = match result {
            Ok(t) => mat(Ok(t)),
            Err(e) => panic!("expected Ok for exactly MAX_SPLIT_PARTS parts, got Err: {e:?}"),
        };
        match val {
            Value::Dict(map) => assert_eq!(map.len(), MAX_SPLIT_PARTS),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn replace_basic() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("world".into())),
                thunk(Value::String("Rust".into())),
                thunk(Value::String("hello world".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello Rust".into()));
    }

    #[test]
    fn replace_multiple_occurrences() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("o".into())),
                thunk(Value::String("banana".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("bonono".into()));
    }

    #[test]
    fn replace_no_match() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("xyz".into())),
                thunk(Value::String("abc".into())),
                thunk(Value::String("hello".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn replace_empty_pattern() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("".into())),
                thunk(Value::String("-".into())),
                thunk(Value::String("abc".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("-a-b-c-".into()));
    }

    #[test]
    fn replace_to_empty() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("l".into())),
                thunk(Value::String("".into())),
                thunk(Value::String("hello".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("heo".into()));
    }

    #[test]
    fn replace_output_size_limit_empty_pattern() {
        // Empty pattern with large replacement should error.
        // 1000 chars input, 100k chars replacement -> output would be ~100MB.
        let input = "a".repeat(1000);
        let replacement = "x".repeat(100_000);
        let result = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("".into())),
                thunk(Value::String(replacement)),
                thunk(Value::String(input)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("replace: output would exceed"));
    }

    #[test]
    fn replace_output_size_ok_normal_pattern() {
        // Normal pattern replacement should succeed even with moderate sizes.
        let input = "a".repeat(1000);
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("bb".into())),
                thunk(Value::String(input)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // 1000 'a' replaced with 'bb' -> 2000 'b'
        assert_eq!(result, Value::String("b".repeat(2000)));
    }

    #[test]
    fn upper_basic() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("HELLO".into()));
    }

    #[test]
    fn upper_mixed_case() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("Hello World".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("HELLO WORLD".into()));
    }

    #[test]
    fn upper_already_upper() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("ABC".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("ABC".into()));
    }

    #[test]
    fn upper_empty() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn upper_with_numbers() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("abc123".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("ABC123".into()));
    }

    #[test]
    fn lower_basic() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("HELLO".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn lower_mixed_case() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("Hello World".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello world".into()));
    }

    #[test]
    fn lower_already_lower() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("abc".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("abc".into()));
    }

    #[test]
    fn lower_empty() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn upper_size_limit_exceeded() {
        // Input string larger than MAX_STRING_SIZE (64MB) should fail.
        // Note: corpus tests for this would be impractical (require >64MB test files),
        // so we test the limit directly in unit tests.
        let large_string = "a".repeat(MAX_STRING_SIZE + 1);
        let result = builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String(large_string))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err(), "expected Err for input > MAX_STRING_SIZE");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ResourceLimitExceeded { .. }),
            "expected ResourceLimitExceeded, got {:?}",
            err.kind
        );
        assert!(
            err.message().contains("upper: input exceeds"),
            "expected 'upper: input exceeds' message, got: {}",
            err.message()
        );
    }

    #[test]
    fn lower_size_limit_exceeded() {
        // Input string larger than MAX_STRING_SIZE (64MB) should fail.
        // Note: corpus tests for this would be impractical (require >64MB test files),
        // so we test the limit directly in unit tests.
        let large_string = "A".repeat(MAX_STRING_SIZE + 1);
        let result = builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String(large_string))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err(), "expected Err for input > MAX_STRING_SIZE");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ResourceLimitExceeded { .. }),
            "expected ResourceLimitExceeded, got {:?}",
            err.kind
        );
        assert!(
            err.message().contains("lower: input exceeds"),
            "expected 'lower: input exceeds' message, got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_unicode() {
        let result = mat(builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("café résumé".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("CAFÉ RÉSUMÉ".into()));
    }

    #[test]
    fn lower_unicode() {
        let result = mat(builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("ZÜRICH МОСКВА".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("zürich москва".into()));
    }

    #[test]
    fn trim_basic() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("  hello  ".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_leading_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("   hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_trailing_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("hello   ".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_no_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_all_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("   ".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("\t\nhello\n\t".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn trim_empty() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String("".into()));
    }

    #[test]
    fn split_wrong_arity_too_few() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String(",".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected 2"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_wrong_arity_too_many() {
        let err = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
                thunk(Value::String("extra".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_wrong_arity() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected 3"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_arity_zero() {
        let err = builtin_upper(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_arity_two() {
        let err = builtin_upper(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lower_wrong_arity() {
        let err = builtin_lower(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn trim_wrong_arity() {
        let err = builtin_trim(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_wrong_type_separator() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn split_wrong_type_input() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(Value::String(",".into())), thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_wrong_type_pattern() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn replace_wrong_type_replacement() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::Bool(true)),
                thunk(Value::String("abc".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn replace_wrong_type_input() {
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::Float(3.14)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_wrong_type() {
        let err = builtin_upper(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Int"), "got: {}", err.message());
    }

    #[test]
    fn lower_wrong_type() {
        let err = builtin_lower(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn trim_wrong_type() {
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn upper_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_upper(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lower_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_lower(BuiltinArgs {
            args: &[thunk(Value::String("HELLO".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn trim_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::String("hi".into())));
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(Value::String("  hello  ".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eval_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn error_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(Value::String("boom".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn type_of_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn from_json_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_from_json(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn to_int_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(Value::String("42".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn split_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_split(BuiltinArgs {
            args: &[
                thunk(Value::String(",".into())),
                thunk(Value::String("a,b".into())),
            ],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn replace_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = builtin_replace(BuiltinArgs {
            args: &[
                thunk(Value::String("a".into())),
                thunk(Value::String("b".into())),
                thunk(Value::String("abc".into())),
            ],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn add_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(99)));
        let err = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(1))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn mul_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(2)), thunk(Value::Int(3))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn div_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn eq_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn lt_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn if_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_if(BuiltinArgs {
            args: &[
                thunk(Value::Bool(true)),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn keys_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let err = builtin_keys(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn length_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let map = IndexMap::new();
        let err = builtin_length(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn merge_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_merge(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn append_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_append(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new())), thunk(Value::Int(42))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn str_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = builtin_str(BuiltinArgs {
            args: &[thunk(Value::String("hello".into()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn try_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = zero_arg_fn(Expr::Int(42));
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = zero_arg_fn(Expr::Int(42));
        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(Value::Dict(IndexMap::new()))],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx(), 0).unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn standard_builtins_contains_all() {
        let builtins = standard_builtins();
        let names: Vec<&str> = builtins.iter().map(|def| def.name).collect();
        // Arithmetic
        assert!(names.contains(&"+"), "missing +");
        assert!(names.contains(&"-"), "missing -");
        assert!(names.contains(&"*"), "missing *");
        assert!(names.contains(&"/"), "missing /");
        // Comparison
        assert!(names.contains(&"="), "missing =");
        assert!(names.contains(&"<"), "missing <");
        // Control
        assert!(names.contains(&"if"), "missing if");
        // Dict primitives
        assert!(names.contains(&"keys"), "missing keys");
        assert!(names.contains(&"length"), "missing length");
        assert!(names.contains(&"merge"), "missing merge");
        assert!(names.contains(&"append"), "missing append");
        // Strings
        assert!(names.contains(&"str"), "missing str");
        assert!(names.contains(&"split"), "missing split");
        assert!(names.contains(&"replace"), "missing replace");
        assert!(names.contains(&"upper"), "missing upper");
        assert!(names.contains(&"lower"), "missing lower");
        assert!(names.contains(&"trim"), "missing trim");
        // Numeric
        assert!(names.contains(&"floor"), "missing floor");
        assert!(names.contains(&"round"), "missing round");
        // Parsing
        assert!(names.contains(&"to-int"), "missing to-int");
        assert!(names.contains(&"to-float"), "missing to-float");
        // Evaluation control
        assert!(names.contains(&"eval"), "missing eval");
        assert!(names.contains(&"error"), "missing error");
        assert!(names.contains(&"try"), "missing try");
        assert!(names.contains(&"apply"), "missing apply");
        // Type introspection
        assert!(names.contains(&"type-of"), "missing type-of");
        assert!(names.contains(&"int?"), "missing int?");
        assert!(names.contains(&"float?"), "missing float?");
        assert!(names.contains(&"num?"), "missing num?");
        assert!(names.contains(&"str?"), "missing str?");
        assert!(names.contains(&"bool?"), "missing bool?");
        assert!(names.contains(&"null?"), "missing null?");
        assert!(names.contains(&"dict?"), "missing dict?");
        assert!(names.contains(&"fn?"), "missing fn?");
        // I/O
        assert!(names.contains(&"from-json"), "missing from-json");
        assert!(names.contains(&"include"), "missing include");
        // Sequences
        assert!(names.contains(&"seq"), "missing seq");
        assert!(names.contains(&"head"), "missing head");
        assert!(names.contains(&"tail"), "missing tail");
        assert!(names.contains(&"collect"), "missing collect");
        assert!(names.contains(&"seq?"), "missing seq?");
        assert!(names.contains(&"range"), "missing range");
        assert!(names.contains(&"repeat"), "missing repeat");
        assert!(names.contains(&"cycle"), "missing cycle");
        assert!(names.contains(&"iterate"), "missing iterate");
        assert!(names.contains(&"unfold"), "missing unfold");
        assert!(names.contains(&"map"), "missing map");
        assert!(names.contains(&"filter"), "missing filter");
        assert!(names.contains(&"take"), "missing take");
        assert!(names.contains(&"drop"), "missing drop");
        assert!(names.contains(&"reduce"), "missing reduce");
        assert!(names.contains(&"join"), "missing join");
        assert!(names.contains(&"concat"), "missing concat");
        // List operations (moved from LLT to Rust)
        assert!(names.contains(&"rest"), "missing rest");
        assert!(names.contains(&"cons"), "missing cons");
        assert!(names.contains(&"reverse"), "missing reverse");
        assert!(names.contains(&"sort"), "missing sort");
        // Also assert proxy is present
        assert!(names.contains(&"proxy"), "missing proxy");
        // Total count: 47 original (incl. proxy) + 4 new list ops + 8 type predicates = 59
        assert_eq!(names.len(), 59, "expected 59 builtins, got {}", names.len());
    }

    #[test]
    fn add_int_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(8));
    }

    #[test]
    fn add_int_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Float(2.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Float(1.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(4.0));
    }

    #[test]
    fn add_negative_ints() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(-10)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn add_zeros() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn add_type_error_string() {
        let e = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("expected Int or Float"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_arity_one_arg() {
        let e = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_arity_three_args() {
        let e = builtin_add(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn add_overflow_error() {
        let err = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_overflow_error() {
        let err = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(i64::MIN)), thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_int_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn sub_int_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(3.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(6.5));
    }

    #[test]
    fn sub_float_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Float(10.5)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn sub_float_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Float(10.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.0));
    }

    #[test]
    fn sub_result_negative() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(10))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn sub_to_zero() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn sub_arity_zero_args() {
        let e = builtin_sub(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_arity_one_arg() {
        let e = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_arity_three_args() {
        let e = builtin_sub(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn sub_type_error_string() {
        let e = builtin_sub(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("expected Int or Float"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn mul_int_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(4)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(20));
    }

    #[test]
    fn mul_int_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(4)), thunk(Value::Float(2.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(4))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Float(3.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn mul_by_zero() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn mul_negative() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(-3)), thunk(Value::Int(4))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-12));
    }

    #[test]
    fn mul_by_negative_one() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(42)), thunk(Value::Int(-1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-42));
    }

    #[test]
    fn mul_overflow_error() {
        let err = builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Int(i64::MAX)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("integer overflow"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn add_float_overflow_to_infinity_is_error() {
        let err = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Float(1e308)), thunk(Value::Float(1e308))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn sub_float_nan_is_error() {
        // f64::INFINITY - f64::INFINITY = NaN
        let err = builtin_sub(BuiltinArgs {
            args: &[
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn mul_float_overflow_to_infinity_is_error() {
        let err = builtin_mul(BuiltinArgs {
            args: &[thunk(Value::Float(1e308)), thunk(Value::Float(10.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn div_float_nan_result_is_error() {
        // 0.0 / 0.0 produces NaN; the existing b==0.0 guard catches b==0.0,
        // but this test documents that NaN results from non-zero / 0-adjacent
        // ops are also caught. Use f64::NAN inputs via Float values directly:
        // f64::INFINITY / f64::INFINITY = NaN
        let err = builtin_div_float(BuiltinArgs {
            args: &[
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("is not a finite"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn div_float_int_int_returns_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_int_exact_returns_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => assert_eq!(f, 5.0),
            other => panic!("expected Float(5.0), got {other:?}"),
        }
    }

    #[test]
    fn div_float_int_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(3.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn div_float_float_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(7.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(3.0));
    }

    #[test]
    fn div_float_by_zero_int() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_by_zero_float() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_by_zero_mixed() {
        let e = builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Float(0.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("division by zero"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn div_float_negative_zero() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: &[thunk(Value::Float(-0.0)), thunk(Value::Float(1.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(0.0));
    }

    #[test]
    fn eq_int_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_int_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(6))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_float_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(3.14)), thunk(Value::Float(3.14))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_float_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(3.14)), thunk(Value::Float(2.71))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_string_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("hello".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_string_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::String("hello".into())),
                thunk(Value::String("world".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_bool_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_cross_type_int_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_float_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(5.0)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_int_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_dict_never_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_different_types_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("1".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_vs_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_nan_not_equal_to_self() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(f64::NAN))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_negative_zero_float() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Float(-0.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_arity_error() {
        let e = builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_int_int_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_int_int_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_int_int_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_float_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("apple".into())),
                thunk(Value::String("banana".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic_reverse() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("banana".into())),
                thunk(Value::String("apple".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("same".into())),
                thunk(Value::String("same".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_prefix() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::String("ab".into())),
                thunk(Value::String("abc".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_int_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Float(3.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_float_int() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_equal_values() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_incompatible_types_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::String("hello".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(e.message().contains("expected"), "got: {}", e.message());
    }

    #[test]
    fn lt_bool_false_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(false)), thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_bool_true_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_false_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(false)), thunk(Value::Bool(false))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_true_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_dict_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(e.message().contains("expected"), "got: {}", e.message());
    }

    #[test]
    fn lt_arity_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn lt_negative_numbers() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(-10)), thunk(Value::Int(-5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_nan_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Float(f64::NAN)), thunk(Value::Float(1.0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn if_true_returns_then_branch() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_false_returns_else_branch() {
        let args = vec![
            thunk(Value::Bool(false)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_does_not_materialize_unchosen_else_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        ));
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 10),
        ));

        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42)), error_thunk];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_does_not_materialize_unchosen_then_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::VarRef("nonexistent".to_string()),
            test_span(1, 1, 1, 10),
        ));
        let env = Rc::new(RefCell::new(Environment::new()));
        let error_thunk = Rc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            test_ctx(),
            test_span(1, 1, 1, 10),
        ));

        let args = vec![
            thunk(Value::Bool(false)),
            error_thunk,
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_non_bool_condition_error() {
        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("expected Bool"),
            "got: {}",
            e.message()
        );
        assert!(
            e.message().contains("Bool"),
            "expected Bool mentioned, got: {}",
            e.message()
        );
    }

    #[test]
    fn if_string_condition_error() {
        let args = vec![
            thunk(Value::String("true".into())),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("expected Bool"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn if_arity_too_few() {
        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42))];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn if_arity_too_many() {
        let args = vec![
            thunk(Value::Bool(true)),
            thunk(Value::Int(1)),
            thunk(Value::Int(2)),
            thunk(Value::Int(3)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            e.message().contains("arity mismatch"),
            "got: {}",
            e.message()
        );
    }

    #[test]
    fn if_non_bool_condition_has_secondary_span() {
        // Test that $if with a non-Bool condition includes secondary_span
        // pointing to where the condition was produced (if different from call site).
        let condition_span = test_span(5, 1, 5, 10); // Where the Int value is defined
        let call_span_val = test_span(10, 1, 10, 30); // Where the $if call is

        let args = vec![
            thunk_with_span(Value::Int(1), condition_span),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];

        let err = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span_val,
            ctx: test_ctx(),
        })
        .unwrap_err();

        // Check that the error has a secondary_span
        assert!(
            err.secondary_span.is_some(),
            "Expected secondary_span to be set for $if type mismatch"
        );

        let (sec_span, sec_label) = err.secondary_span.unwrap();
        assert_eq!(
            sec_span, condition_span,
            "Secondary span should point to where the condition value was produced"
        );
        assert!(
            sec_label.contains("condition evaluated to"),
            "Secondary label should mention 'condition evaluated to', got: {}",
            sec_label
        );
        assert!(
            sec_label.contains("Int"),
            "Secondary label should mention the actual type (Int), got: {}",
            sec_label
        );
    }

    #[test]
    fn if_non_bool_secondary_span_suppressed_when_same() {
        // Test that when the condition span equals the call span,
        // secondary_span is NOT set (would be redundant).
        let same_span = test_span(1, 1, 1, 10);

        let args = vec![
            thunk_with_span(Value::Int(1), same_span),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];

        let err = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: same_span,
            ctx: test_ctx(),
        })
        .unwrap_err();

        // Secondary span should NOT be set because it equals call_span
        assert!(
            err.secondary_span.is_none(),
            "Secondary span should be suppressed when same as call span"
        );
    }

    #[test]
    fn create_root_env_has_all_builtins() {
        let env = create_root_env();
        let env_ref = env.borrow();
        for def in standard_builtins() {
            let name = def.name;
            assert!(
                env_ref.get(name).is_some(),
                "root env missing builtin: {name}"
            );
        }
    }

    /// Parse-only smoke test for the prelude. Evaluating the full prelude requires a
    /// 128 MB thread stack (see corpus_tests.rs) due to deep Rc<Environment> drop chains
    /// that exceed the default and RUST_MIN_STACK=64MB test thread stacks.
    /// This test verifies the prelude parses without error — which was broken by the
    /// f1e38a2 VarRef colon-ahead detection regression (duplicate key "value" false positive).
    #[test]
    fn prelude_parses_without_error() {
        let prelude_source = include_str!("../stdlib/prelude.llt");
        match crate::parser::parse(prelude_source) {
            Ok(_) => {}
            Err(e) => panic!("prelude parse failed: {e}"),
        }
    }

    #[test]
    fn create_stdlib_env_has_builtins_and_prelude() {
        let env = create_stdlib_env().expect("stdlib env creation failed");
        let env_ref = env.borrow();
        // Should have builtins (via parent chain)
        assert!(env_ref.get("+").is_some(), "missing builtin +");
        assert!(env_ref.get("if").is_some(), "missing builtin if");
        // Should have prelude functions
        assert!(env_ref.get("not").is_some(), "missing prelude function not");
        assert!(env_ref.get("map").is_some(), "missing prelude function map");
        assert!(
            env_ref.get("filter").is_some(),
            "missing prelude function filter"
        );
        assert!(
            env_ref.get("identity").is_some(),
            "missing prelude function identity"
        );
    }

    /// Helper: create an EvalContext pointing at the given base directory.
    fn include_ctx(base_dir: &std::path::Path) -> Rc<crate::eval::EvalContext> {
        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let dir = cap_std::fs::Dir::open_ambient_dir(base_dir, cap_std::ambient_authority())
            .expect("failed to open base_dir");
        crate::eval::EvalContext::new(dir, stdlib_env, false)
    }

    /// Helper: write a temp file and return its path.
    fn write_temp_file(dir: &std::path::Path, name: &str, content: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, content).expect("write temp file");
        path
    }

    #[test]
    fn include_wrong_type_error() {
        let dir = std::env::temp_dir().join("llt_test_include_type");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::Int(42))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected String"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_file_not_found() {
        let dir = std::env::temp_dir().join("llt_test_include_notfound");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("nonexistent.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot access"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_simple_dict() {
        let dir = std::env::temp_dir().join("llt_test_include_simple");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "lib.llt", "[x: 42 y: \"hello\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("lib.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x = materialize(
                    map.get(&Key::String("x".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(x, Value::Int(42));
                let y = materialize(
                    map.get(&Key::String("y".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(y, Value::String("hello".into()));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_scalar_value() {
        let dir = std::env::temp_dir().join("llt_test_include_scalar");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "num.llt", "42");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("num.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        assert_eq!(result, Value::Int(42));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_parse_error() {
        let dir = std::env::temp_dir().join("llt_test_include_parse_err");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "bad.llt", "[x: ]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("bad.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("parse error"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_circular_detection() {
        let dir = std::env::temp_dir().join("llt_test_include_circular");
        std::fs::create_dir_all(&dir).ok();
        // File A includes file B at top level (not inside a dict entry, so
        // the include is evaluated eagerly during eval_file). File B includes
        // file A the same way, triggering the cycle.
        write_temp_file(&dir, "a.llt", "[call $include \"b.llt\"]");
        write_temp_file(&dir, "b.llt", "[call $include \"a.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("a.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("circular include"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_self_circular() {
        let dir = std::env::temp_dir().join("llt_test_include_self");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "self.llt", "[call $include \"self.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("self.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("circular include"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_nested() {
        let dir = std::env::temp_dir().join("llt_test_include_nested");
        std::fs::create_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).ok();
        write_temp_file(
            &dir,
            "outer.llt",
            "[inner: [call $include \"sub/inner.llt\"]]",
        );
        write_temp_file(&dir.join("sub"), "inner.llt", "[val: 99]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("outer.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let inner = materialize(
                    map.get(&Key::String("inner".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                match inner {
                    Value::Dict(inner_map) => {
                        let val = materialize(
                            inner_map.get(&Key::String("val".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        assert_eq!(val, Value::Int(99));
                    }
                    other => panic!("expected inner Dict, got {:?}", other),
                }
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_absolute_path() {
        // Absolute paths are rejected by cap-std's RESOLVE_BENEATH sandbox.
        let dir = std::env::temp_dir().join("llt_test_include_abs");
        std::fs::create_dir_all(&dir).ok();
        let file_path = write_temp_file(&dir, "abs.llt", "[val: 77]");
        // Use a different directory as base — the absolute path should be rejected.
        let other_dir = std::env::temp_dir().join("llt_test_include_abs_other");
        std::fs::create_dir_all(&other_dir).ok();
        let ctx = include_ctx(&other_dir);

        let args = vec![thunk(Value::String(
            file_path.to_string_lossy().into_owned(),
        ))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("cannot access"),
            "expected path rejection error, got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&other_dir).ok();
    }

    #[test]
    fn include_arity_error() {
        let dir = std::env::temp_dir().join("llt_test_include_arity");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        // No arguments
        let err = builtin_include(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Three arguments (only 1 or 2 accepted; 3 is an arity error)
        let args = vec![
            thunk(Value::String("a.llt".into())),
            thunk(Value::String(
                "blake3:0000000000000000000000000000000000000000000000000000000000000000".into(),
            )),
            thunk(Value::String("extra".into())),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_rejects_named_args() {
        let dir = std::env::temp_dir().join("llt_test_include_named");
        std::fs::create_dir_all(&dir).ok();
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("test.llt".into()))];
        let mut named = IndexMap::new();
        named.insert("path".to_string(), thunk(Value::String("x".into())));
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("does not accept named arguments"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_multi_document() {
        let dir = std::env::temp_dir().join("llt_test_include_multidoc");
        std::fs::create_dir_all(&dir).ok();
        // Two documents: first produces [x: 10], % pipeline passes to second
        write_temp_file(&dir, "multi.llt", "[x: 10]\n---\n[y: %.x]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("multi.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let y = materialize(
                    map.get(&Key::String("y".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(y, Value::Int(10));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_uses_stdlib() {
        // The included file should have access to stdlib builtins
        let dir = std::env::temp_dir().join("llt_test_include_stdlib");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "stdlib_test.llt", "[result: [call $+ 1 2]]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("stdlib_test.llt".into()))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let val = materialize(
                    map.get(&Key::String("result".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val, Value::Int(3));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verify that `$include` returns the **same Rc<Thunk> allocation** on the second call
    /// to the same file. This proves the cache stores a `Rc::clone()` of the original thunk
    /// rather than re-evaluating the file and creating a new Thunk allocation.
    ///
    /// This is the pointer-identity proof: `Rc::ptr_eq(&first, &second)` — both calls
    /// return an `Rc` pointing to the identical Thunk object in memory. This would only
    /// be true if the second call hit the cache.
    #[test]
    fn include_cache_returns_same_rc_ptr() {
        let dir = std::env::temp_dir().join("llt_test_include_rc_ptr");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "cached_ptr.llt", "[value: 99]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("cached_ptr.llt".into()))];

        // First include — builds and caches the Thunk
        let raw1 = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .expect("first include should succeed");

        // Second include — must return Rc::clone of the cached Thunk
        let raw2 = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .expect("second include should succeed");

        // Pointer identity: both Rcs must point to the same Thunk allocation.
        // If the cache is bypassed and the file is re-evaluated, a new Thunk is
        // allocated and ptr_eq returns false.
        assert!(
            Rc::ptr_eq(&raw1, &raw2),
            "Second $include of same file must return the same Rc<Thunk> as the first \
             (cache hit, not re-evaluation). Got distinct allocations — the cache is not working."
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_caches_result() {
        // Including the same file twice should return the cached result, not re-evaluate.
        let dir = std::env::temp_dir().join("llt_test_include_cache");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "cached.llt", "[value: 42]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("cached.llt".into()))];

        // First include
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include -- should hit cache
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = materialize(
                    map1.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let val2 = materialize(
                    map2.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val1, Value::Int(42));
                assert_eq!(val2, Value::Int(42));
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result1, result2),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_cache_respects_normalization() {
        // Including a file via different paths that resolve to the same canonical path
        // should hit the cache.
        let dir = std::env::temp_dir().join("llt_test_include_cache_norm");
        std::fs::create_dir_all(&dir).ok();
        let subdir = dir.join("subdir");
        std::fs::create_dir_all(&subdir).ok();
        write_temp_file(&dir, "target.llt", "[value: 99]");
        let ctx = include_ctx(&dir);

        // First include with relative path
        let args1 = vec![thunk(Value::String("./target.llt".into()))];
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args1,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include with normalized path
        let args2 = vec![thunk(Value::String("subdir/../target.llt".into()))];
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args2,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = materialize(
                    map1.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let val2 = materialize(
                    map2.get(&Key::String("value".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val1, Value::Int(99));
                assert_eq!(val2, Value::Int(99));
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result1, result2),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_cache_shared_across_nested() {
        // File A includes file B. File C also includes file B. Both should share
        // the cached result of B.
        let dir = std::env::temp_dir().join("llt_test_include_cache_nested");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "shared.llt", "[shared: 123]");
        write_temp_file(&dir, "file_a.llt", "[a: [call $include \"shared.llt\"]]");
        write_temp_file(&dir, "file_c.llt", "[c: [call $include \"shared.llt\"]]");
        let ctx = include_ctx(&dir);

        // Include file_a (which includes shared.llt)
        let args_a = vec![thunk(Value::String("file_a.llt".into()))];
        let result_a = mat(builtin_include(BuiltinArgs {
            args: &args_a,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Include file_c (which also includes shared.llt -- should hit cache)
        let args_c = vec![thunk(Value::String("file_c.llt".into()))];
        let result_c = mat(builtin_include(BuiltinArgs {
            args: &args_c,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));

        // Verify that both got the shared value
        match (&result_a, &result_c) {
            (Value::Dict(map_a), Value::Dict(map_c)) => {
                let a_val = materialize(
                    map_a.get(&Key::String("a".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                let c_val = materialize(
                    map_c.get(&Key::String("c".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();

                // Both should be dicts with "shared: 123"
                match (&a_val, &c_val) {
                    (Value::Dict(a_inner), Value::Dict(c_inner)) => {
                        let a_shared = materialize(
                            a_inner.get(&Key::String("shared".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        let c_shared = materialize(
                            c_inner.get(&Key::String("shared".into())).unwrap(),
                            None,
                            &test_ctx(),
                            0,
                        )
                        .unwrap();
                        assert_eq!(a_shared, Value::Int(123));
                        assert_eq!(c_shared, Value::Int(123));
                    }
                    _ => panic!("expected nested dicts, got {:?} and {:?}", a_val, c_val),
                }
            }
            _ => panic!("expected Dict, got {:?} and {:?}", result_a, result_c),
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_forbidden_when_no_fs() {
        // When no_fs is true, $include should return IncludeForbidden error
        let dir = std::env::temp_dir().join("llt_test_include_no_fs");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "test.llt", "[x: 42]");

        // Create context with no_fs: true
        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&dir, cap_std::ambient_authority())
            .expect("failed to open temp_dir");
        let ctx = crate::eval::EvalContext::new(base_dir, stdlib_env, true);

        let args = vec![thunk(Value::String("test.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();

        // Check error message and code
        let error_msg = format!("{}", err);
        assert!(
            error_msg.contains("filesystem access is disabled"),
            "got: {}",
            error_msg
        );
        assert!(
            error_msg.contains("[E042]"),
            "missing error code [E042], got: {}",
            error_msg
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_with_correct_blake3_hash() {
        // $include with a correct blake3 hash should succeed.
        let dir = std::env::temp_dir().join("llt_test_include_hash_ok");
        std::fs::create_dir_all(&dir).ok();
        let content = "[x: 99]";
        write_temp_file(&dir, "hashed.llt", content);
        let expected_hex = blake3_hex(content.as_bytes());
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(Value::String("hashed.llt".into())),
            thunk(Value::String(format!("blake3:{expected_hex}"))),
        ];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let val = materialize(
                    map.get(&Key::String("x".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_with_wrong_blake3_hash_errors() {
        // $include with a wrong blake3 hash should return IncludeHashMismatch.
        let dir = std::env::temp_dir().join("llt_test_include_hash_mismatch");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "data.llt", "[x: 1]");
        let wrong_hex = "0".repeat(64);
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(Value::String("data.llt".into())),
            thunk(Value::String(format!("blake3:{wrong_hex}"))),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("integrity check failed"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_hash_invalid_format_errors() {
        // A hash string without a colon should produce an error.
        let dir = std::env::temp_dir().join("llt_test_include_hash_format");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(Value::String("file.llt".into())),
            thunk(Value::String("notahash".into())),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("integrity hash must be"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_hash_unsupported_algo_errors() {
        // An unsupported algorithm should produce a clear error.
        let dir = std::env::temp_dir().join("llt_test_include_hash_algo");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(Value::String("file.llt".into())),
            thunk(Value::String("md5:abc".into())),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("unsupported hash algorithm"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_require_integrity_rejects_hashless() {
        // With require_integrity=true, a hashless $include should error with IncludeHashRequired.
        let dir = std::env::temp_dir().join("llt_test_include_require_integrity");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");

        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&dir, cap_std::ambient_authority())
            .expect("open dir");
        let ctx = crate::eval::EvalContext::new_with_options(base_dir, stdlib_env, false, true);

        let args = vec![thunk(Value::String("file.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("integrity hash required"),
            "got: {}",
            err.message()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_require_integrity_accepts_hashed() {
        // With require_integrity=true, a $include with a correct hash should succeed.
        let dir = std::env::temp_dir().join("llt_test_include_require_integrity_ok");
        std::fs::create_dir_all(&dir).ok();
        let content = "[y: 55]";
        write_temp_file(&dir, "ok.llt", content);
        let hex = blake3_hex(content.as_bytes());

        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&dir, cap_std::ambient_authority())
            .expect("open dir");
        let ctx = crate::eval::EvalContext::new_with_options(base_dir, stdlib_env, false, true);

        let args = vec![
            thunk(Value::String("ok.llt".into())),
            thunk(Value::String(format!("blake3:{hex}"))),
        ];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        }));
        match result {
            Value::Dict(map) => {
                let val = materialize(
                    map.get(&Key::String("y".into())).unwrap(),
                    None,
                    &test_ctx(),
                    0,
                )
                .unwrap();
                assert_eq!(val, Value::Int(55));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    // Include chain threading tests

    /// Verify that nested include errors carry the full include chain as stack frames.
    ///
    /// Setup: outer.llt includes middle.llt, which includes bad.llt (parse error).
    /// Expected stack frames on the error (outermost first in display):
    ///   [0] "included from outer.llt"   (added by the top-level include of outer.llt)
    ///   [1] "included from middle.llt"  (added by outer.llt's include of middle.llt)
    ///
    /// Note: bad.llt's parse fails before its guard/chain entry is pushed, so
    /// there is no "included from bad.llt" frame — the IncludeParseFailed error
    /// message already names bad.llt directly.
    #[test]
    fn include_chain_nested_error() {
        let dir = std::env::temp_dir().join("llt_test_include_chain");
        std::fs::create_dir_all(&dir).ok();

        // bad.llt: parse error
        write_temp_file(&dir, "bad.llt", "[x: ]");
        // middle.llt: includes bad.llt
        write_temp_file(&dir, "middle.llt", "[call $include \"bad.llt\"]");
        // outer.llt: includes middle.llt
        write_temp_file(&dir, "outer.llt", "[call $include \"middle.llt\"]");

        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("outer.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();

        // The error should be a parse failure (bad.llt is the innermost problem).
        assert!(
            err.message().contains("parse error"),
            "expected parse error, got: {}",
            err.message()
        );

        // The stack should contain include chain frames.
        // Frame 0: outer.llt frame (inserted at position 0 by the outermost include).
        // Frame 1: middle.llt frame (inserted at position 0 by middle.llt's include, then
        //          shifted to position 1 when outer.llt inserts its own frame at position 0).
        assert!(
            err.stack.len() >= 2,
            "expected at least 2 stack frames for the include chain, got {}: {:?}",
            err.stack.len(),
            err.stack
        );
        assert!(
            err.stack[0].label.contains("outer.llt"),
            "frame[0] should mention outer.llt: {:?}",
            err.stack[0]
        );
        assert!(
            err.stack[1].label.contains("middle.llt"),
            "frame[1] should mention middle.llt: {:?}",
            err.stack[1]
        );
        // The span on frame[0] should be the call_span we passed (the test's outer call site).
        assert_eq!(
            err.stack[0].span,
            call_span(),
            "frame[0] span should be the outer call_span"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verify that the include chain is cleaned up after a successful include.
    ///
    /// After `builtin_include` returns successfully, `state.include_chain` must be empty.
    /// A non-empty chain after success would corrupt future error annotations.
    #[test]
    fn include_chain_cleaned_up_after_success() {
        let dir = std::env::temp_dir().join("llt_test_include_chain_cleanup");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "ok.llt", "42");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("ok.llt".into()))];
        let _result = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        assert!(
            ctx.state.borrow().include_chain.is_empty(),
            "include_chain must be empty after successful include"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Verify that the include chain is cleaned up after a failed include.
    ///
    /// After `builtin_include` returns an error, `state.include_chain` must be empty.
    #[test]
    fn include_chain_cleaned_up_after_error() {
        let dir = std::env::temp_dir().join("llt_test_include_chain_err_cleanup");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "bad.llt", "[x: ]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(Value::String("bad.llt".into()))];
        let _err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap_err();

        assert!(
            ctx.state.borrow().include_chain.is_empty(),
            "include_chain must be empty after failed include"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    // Sequence builtins tests

    #[test]
    fn seq_basic() {
        let head_val = thunk(Value::Int(1));
        let tail_val = thunk(Value::Int(2));
        let result = mat(builtin_seq(BuiltinArgs {
            args: &[head_val.clone(), tail_val.clone()],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(&tail, None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn seq_arity_zero() {
        let result = builtin_seq(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_one() {
        let result = builtin_seq(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_three() {
        let result = builtin_seq(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn seq_lazy() {
        // Head can be a thunk wrapping a VarRef to a nonexistent variable.
        // If we tried to materialize this thunk, it would error (undefined variable).
        // But seq construction should succeed because it doesn't materialize args.
        let undef_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_var".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let tail_val = thunk(Value::Int(2));
        // seq construction should succeed even though head would error if materialized
        let result = builtin_seq(BuiltinArgs {
            args: &[undef_thunk, tail_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        // Verify the result is a Seq
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn head_basic() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::String("first".into())),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = builtin_head(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        let head = mat(result);
        assert_eq!(head, Value::String("first".into()));
    }

    #[test]
    fn head_non_seq() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_zero() {
        let result = builtin_head(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_two() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn head_empty_dict() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let err = result.unwrap_err();
        assert!(
            err.message().contains("on empty collection"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tail_empty_dict() {
        let result = builtin_tail(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        let err = result.unwrap_err();
        assert!(
            err.message().contains("on empty collection"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn tail_basic() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::String("first".into())),
            tail: thunk(Value::Int(99)),
        });
        let result = builtin_tail(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        let tail = mat(result);
        assert_eq!(tail, Value::Int(99));
    }

    #[test]
    fn tail_non_seq() {
        let result = builtin_tail(BuiltinArgs {
            args: &[thunk(Value::String("not a seq".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Seq {
                    head: thunk(Value::Int(3)),
                    tail: thunk(Value::Dict(IndexMap::new())),
                }),
            }),
        });
        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_empty_tail() {
        // Single element: Seq(42, {})
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(42)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_non_seq() {
        let result = builtin_collect(BuiltinArgs {
            args: &[thunk(Value::Int(123))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_invalid_tail() {
        // Seq with non-empty dict as tail (should error)
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(map)),
        });
        let result = builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_large_sequence() {
        // Test collect with a moderately-sized sequence (200 elements) to verify it works
        // correctly without hitting MAX_EVAL_DEPTH (256) or MAX_COLLECT_SIZE (1M).
        // Testing at the actual MAX_COLLECT_SIZE (1M) would be too slow/memory-intensive,
        // and with depth increment fixes, sequences hit MAX_EVAL_DEPTH around 256 elements.
        let range_result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let take_result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(200)), range_result],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let collect_result = builtin_collect(BuiltinArgs {
            args: &[take_result],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });

        assert!(
            collect_result.is_ok(),
            "collect should succeed for 200 elements"
        );
        match materialize(&collect_result.unwrap(), None, &test_ctx(), 0).unwrap() {
            Value::Dict(map) => {
                assert_eq!(map.len(), 200);
                // Spot-check first and last elements
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(199)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(199)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn collect_max_size_limit_enforced() {
        // Test that the MAX_COLLECT_SIZE check is present and triggers correctly.
        // We can't practically test with 1M+ elements in a unit test (too slow/memory-intensive),
        // but we can test that attempting to collect from an unbounded sequence without $take
        // will eventually hit either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE.
        //
        // This test verifies the error message is correct for the MAX_COLLECT_SIZE path.
        // Note: corpus tests for this would be impractical (require >1M element sequences),
        // so we test the limit directly in unit tests.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow when testing
        // depth-exceeded behavior (same pattern as corpus test runners and join_seq_size_limit).
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Attempt to collect infinite range without take
                // This will hit MAX_EVAL_DEPTH (256) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the PendingBuiltin chain.
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[range_result],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail (either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE)
                assert!(
                    collect_result.is_err(),
                    "collect should fail on infinite sequence"
                );
                let err = collect_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.message().contains("maximum evaluation depth");
                let is_size_error = err.message().contains("exceeded maximum collection size");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        // Propagate any panic from the spawned thread
        result.unwrap();
    }

    #[test]
    fn seq_check_true() {
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn seq_check_false() {
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[thunk(Value::String("not a seq".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn seq_check_dict() {
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    // === range builtin tests ===

    #[test]
    fn range_finite_basic() {
        // range(0, 5) → 0, 1, 2, 3, 4
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // Materialize tail to get next element
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(1)
                        );
                    }
                    other => panic!("expected Seq for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_empty() {
        // range(5, 5) → empty
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn range_negative_range() {
        // range(10, 5) → empty (start >= end)
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(10)), thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn range_single_element() {
        // range(0, 1) → 0
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // tail should be empty (terminal)
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Dict(map) if map.is_empty() => {} // Success
                    other => panic!("expected empty dict for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_infinite_basic() {
        // range(0) → 0, 1, 2, ... (take first 3)
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(1)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(2)
                                );
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn range_arity_zero() {
        let result = builtin_range(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_arity_three() {
        let result = builtin_range(BuiltinArgs {
            args: &[
                thunk(Value::Int(0)),
                thunk(Value::Int(5)),
                thunk(Value::Int(10)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_start() {
        let result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::String("not an int".into()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_end() {
        let result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Float(5.5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === repeat builtin tests ===

    #[test]
    fn repeat_basic() {
        // repeat(42) → 42, 42, 42, ... (take first 3)
        let result = mat(builtin_repeat(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(42)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(42)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(42)
                                );
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn repeat_laziness() {
        // Repeat an unevaluated thunk (would error if materialized)
        let undef_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_var".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        // repeat construction should succeed without materializing arg
        let result = builtin_repeat(BuiltinArgs {
            args: &[undef_thunk],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn repeat_arity_zero() {
        let result = builtin_repeat(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn repeat_arity_two() {
        let result = builtin_repeat(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === cycle builtin tests ===

    #[test]
    fn cycle_basic() {
        // cycle([a, b]) → a, b, a, b, ... (take first 4)
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::String("a".into())));
        map.insert(Key::String("y".into()), thunk(Value::String("b".into())));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_cycle(BuiltinArgs {
            args: &[dict_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                // First element: "a"
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::String("a".into())
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        // Second element: "b"
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::String("b".into())
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                // Third element: "a" (cycling back)
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::String("a".into())
                                );
                                let t3_val = materialize(&t3, None, &test_ctx(), 0).unwrap();
                                match t3_val {
                                    Value::Seq { head: h4, .. } => {
                                        // Fourth element: "b"
                                        assert_eq!(
                                            materialize(&h4, None, &test_ctx(), 0).unwrap(),
                                            Value::String("b".into())
                                        );
                                    }
                                    other => panic!("expected Seq, got {:?}", other),
                                }
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn cycle_empty_dict() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().message().contains("empty"));
    }

    #[test]
    fn cycle_non_dict() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn cycle_arity_zero() {
        let result = builtin_cycle(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === iterate builtin tests ===

    #[test]
    fn iterate_basic() {
        // iterate(+1, 0) → 0, 1, 2, ... (test structure)
        // For this test, we'll just verify the structure is correct
        // The tail is PendingBuiltin(iterate, [f, PendingCall(f, [x])])
        // Materializing it succeeds (returns another Seq), but materializing
        // the head of *that* Seq would error because f is Int(999), not a function
        let f_thunk = thunk(Value::Int(999)); // dummy, won't be called in structure test
        let x_thunk = thunk(Value::Int(0));

        let result = mat(builtin_iterate(BuiltinArgs {
            args: &[f_thunk, x_thunk.clone()],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                // Head should be x (0)
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(0)
                );
                // Tail is a PendingBuiltin wrapping iterate(f, f(x))
                // Materializing it returns another Seq (doesn't error yet)
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        // Trying to materialize h2 (which is PendingCall(Int(999), [Int(0)]))
                        // will error because Int(999) is not a function
                        let h2_result = materialize(&h2, None, &test_ctx(), 0);
                        assert!(h2_result.is_err());
                    }
                    other => panic!("expected Seq for tail, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn iterate_laziness() {
        // iterate doesn't materialize its args
        let undef_f = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_f".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let undef_x = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("undefined_x".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let result = builtin_iterate(BuiltinArgs {
            args: &[undef_f, undef_x],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        match mat(result) {
            Value::Seq { .. } => {} // Success
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn iterate_arity_one() {
        let result = builtin_iterate(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === unfold builtin tests ===

    #[test]
    fn unfold_basic_termination() {
        // unfold with a step that immediately returns empty dict (termination)
        // We can't easily test a full unfold without a real function, but we can
        // test that it returns a PendingBuiltin
        let step_thunk = thunk(Value::Int(999)); // dummy
        let seed_thunk = thunk(Value::Int(0));

        let result = builtin_unfold(BuiltinArgs {
            args: &[step_thunk, seed_thunk],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        // Result is a PendingBuiltin, not yet materialized
        // Materializing it would call unfold_step, which would error because
        // step is Int(999), not a function
        let result_val = materialize(&result.unwrap(), None, &test_ctx(), 0);
        assert!(result_val.is_err());
    }

    #[test]
    fn unfold_arity_one() {
        let result = builtin_unfold(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === take builtin tests ===

    #[test]
    fn take_dict_basic() {
        // take(2, [a: 1, b: 2, c: 3]) → [a: 1, b: 2]
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), dict_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(
                        map.get(&Key::String("a".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(
                        map.get(&Key::String("b".into())).unwrap(),
                        None,
                        &test_ctx(),
                        0
                    )
                    .unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_zero() {
        // take(0, dict) → []
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), dict_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_negative() {
        // take(-5, dict) → []
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(-5)), dict_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_more_than_length() {
        // take(10, [a: 1, b: 2]) → [a: 1, b: 2]
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let dict_val = thunk(Value::Dict(map));

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(10)), dict_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_seq_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Seq {
                    head: thunk(Value::Int(3)),
                    tail: thunk(Value::Dict(IndexMap::new())),
                }),
            }),
        });

        // take(2, seq) → Seq(1, Seq(2, []))
        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(2)
                        );
                        // tail of tail should be empty dict (terminal)
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Dict(map) if map.is_empty() => {} // Success
                            other => panic!("expected empty dict, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn take_seq_zero() {
        // take(0, seq) → []
        let seq_val = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), seq_val],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_n_non_int() {
        let result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::String("not int".into())), thunk(Value::Int(1))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn take_xs_non_dict_or_seq() {
        let result = builtin_take(BuiltinArgs {
            args: &[
                thunk(Value::Int(5)),
                thunk(Value::String("not dict or seq".into())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn take_arity_one() {
        let result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(5))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn concat_seq() {
        // Build two 2-element sequences and concat them
        // xs = Seq(1, Seq(2, {}))
        let xs = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Dict(IndexMap::new())),
            }),
        });

        // ys = Seq(3, Seq(4, {}))
        let ys = thunk(Value::Seq {
            head: thunk(Value::Int(3)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(4)),
                tail: thunk(Value::Dict(IndexMap::new())),
            }),
        });

        // concat(xs, ys) should produce Seq(1, Seq(2, Seq(3, Seq(4, {}))))
        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Materialize the result to verify structure
        let result_val = materialize(&result, None, &test_ctx(), 0).unwrap();
        match result_val {
            Value::Seq { head: h1, tail: t1 } => {
                assert_eq!(
                    materialize(&h1, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let t1_val = materialize(&t1, None, &test_ctx(), 0).unwrap();
                match t1_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(
                            materialize(&h2, None, &test_ctx(), 0).unwrap(),
                            Value::Int(2)
                        );
                        let t2_val = materialize(&t2, None, &test_ctx(), 0).unwrap();
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                assert_eq!(
                                    materialize(&h3, None, &test_ctx(), 0).unwrap(),
                                    Value::Int(3)
                                );
                                let t3_val = materialize(&t3, None, &test_ctx(), 0).unwrap();
                                match t3_val {
                                    Value::Seq { head: h4, tail: t4 } => {
                                        assert_eq!(
                                            materialize(&h4, None, &test_ctx(), 0).unwrap(),
                                            Value::Int(4)
                                        );
                                        let t4_val =
                                            materialize(&t4, None, &test_ctx(), 0).unwrap();
                                        match t4_val {
                                            Value::Dict(map) if map.is_empty() => {} // Success
                                            other => panic!("expected empty dict, got {:?}", other),
                                        }
                                    }
                                    other => panic!("expected Seq, got {:?}", other),
                                }
                            }
                            other => panic!("expected Seq, got {:?}", other),
                        }
                    }
                    other => panic!("expected Seq, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn concat_seq_empty_xs() {
        // concat({}, ys) should return ys
        let xs = thunk(Value::Dict(IndexMap::new()));
        let ys = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys.clone()],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Result should be ys (the same thunk)
        assert!(Rc::ptr_eq(&result, &ys));
    }

    #[test]
    fn concat_seq_empty_ys() {
        // concat(xs, {}) should return xs's elements followed by empty dict
        let xs = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Dict(IndexMap::new())),
        });
        let ys = thunk(Value::Dict(IndexMap::new()));

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        // Materialize to verify: Seq(1, {})
        let result_val = materialize(&result, None, &test_ctx(), 0).unwrap();
        match result_val {
            Value::Seq { head, tail } => {
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                let tail_val = materialize(&tail, None, &test_ctx(), 0).unwrap();
                match tail_val {
                    Value::Dict(map) if map.is_empty() => {} // Success
                    other => panic!("expected empty dict, got {:?}", other),
                }
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn concat_dict() {
        // concat([1, 2], [3, 4]) -> [1, 2, 3, 4] with integer reindexing
        let mut xs_map = IndexMap::new();
        xs_map.insert(Key::Int(0), thunk(Value::Int(1)));
        xs_map.insert(Key::Int(1), thunk(Value::Int(2)));
        let xs = thunk(Value::Dict(xs_map));

        let mut ys_map = IndexMap::new();
        ys_map.insert(Key::Int(0), thunk(Value::Int(3)));
        ys_map.insert(Key::Int(1), thunk(Value::Int(4)));
        let ys = thunk(Value::Dict(ys_map));

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn concat_seq_non_collection_ys_is_type_error() {
        // concat(seq(1, 2, 3), 42) should produce a type error, not silently succeed.
        // Before the fix, ys=42 was returned as-is when xs was exhausted, so the
        // consumer would see `42` as the tail of the last Seq node — a silent
        // correctness failure. After the fix, builtin_concat validates ys eagerly
        // when xs is a Seq, so the error fires at call time.
        //
        // Eager validation is a deliberate strictness point (parallels the Dict xs
        // path) and avoids the alternative of adding a materialize call deep in the
        // PendingBuiltin chain where Rust stack depth is highest.
        //
        // xs = Seq(1, Seq(2, Seq(3, {})))
        let xs = thunk(Value::Seq {
            head: thunk(Value::Int(1)),
            tail: thunk(Value::Seq {
                head: thunk(Value::Int(2)),
                tail: thunk(Value::Seq {
                    head: thunk(Value::Int(3)),
                    tail: thunk(Value::Dict(IndexMap::new())),
                }),
            }),
        });
        let ys = thunk(Value::Int(42));

        // builtin_concat itself fails immediately because ys=42 is not a collection.
        let err = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();

        match err.kind {
            crate::error::ErrorKind::TypeMismatch {
                context,
                expected,
                got,
            } => {
                assert_eq!(context.as_deref(), Some("concat"));
                assert_eq!(expected, "Dict or Seq");
                assert_eq!(got, "Int");
            }
            other => panic!("expected TypeMismatch, got {:?}", other),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn join_seq_size_limit() {
        // Test that join enforces MAX_COLLECT_SIZE on sequence iteration.
        // Similar to collect_max_size_limit_enforced, we verify that attempting to join
        // an unbounded sequence will hit either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow when testing
        // depth-exceeded behavior (same pattern as corpus test runners).
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Attempt to join infinite range without take
                // This will hit MAX_EVAL_DEPTH (256) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the sequence traversal.
                let join_result = builtin_join(BuiltinArgs {
                    args: &[thunk(Value::String(",".to_string())), range_result],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail (either MAX_EVAL_DEPTH or MAX_COLLECT_SIZE)
                assert!(
                    join_result.is_err(),
                    "join should fail on infinite sequence"
                );
                let err = join_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.message().contains("maximum evaluation depth");
                let is_size_error = err.message().contains("sequence exceeds");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        // Propagate any panic from the spawned thread
        result.unwrap();
    }

    #[test]
    fn join_empty_dict() {
        // Task 3: Test $join with empty Dict to verify the parts.is_empty() guard
        // prevents saturating_sub(1) wraparound
        let result = mat(builtin_join(BuiltinArgs {
            args: &[
                thunk(Value::String(",".to_string())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::String(String::new()));
    }

    #[test]
    fn concat_dict_basic() {
        // Task 4: Test $concat with two small dicts to verify correct behavior
        // This exercises the checked_add call site that prevents integer overflow
        let mut dict1 = IndexMap::new();
        dict1.insert(Key::String("a".into()), thunk(Value::Int(1)));
        dict1.insert(Key::String("b".into()), thunk(Value::Int(2)));

        let mut dict2 = IndexMap::new();
        dict2.insert(Key::String("c".into()), thunk(Value::Int(3)));
        dict2.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[thunk(Value::Dict(dict1)), thunk(Value::Dict(dict2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                // All values should be reindexed with integer keys 0, 1, 2, 3
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(3)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, &test_ctx(), 0).unwrap(),
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn filter_seq_step_no_depth_accumulation_on_consecutive_failures() {
        // Task 1: Verify that consecutive predicate failures in builtin_filter_seq_step
        // do NOT accumulate depth. Before the fix, each skipped element created a
        // PendingBuiltin at depth+1, so N failures consumed ~2N depth units and would
        // hit MAX_EVAL_DEPTH (256) after ~128 consecutive failing elements. After the
        // fix, the skip branch uses an internal loop, so N failures cost zero extra depth.
        //
        // Test: filter range(0, 300) with a predicate that only passes x == 299.
        // This triggers 299 consecutive failures. With the old PendingBuiltin-per-failure
        // approach, this would hit MAX_EVAL_DEPTH (~128 failures × 2 depth units each).
        // With the fix (internal loop for failures), all 299 failures are handled at
        // constant depth, and the result is Seq(Int(299), ...).
        //
        // The predicate is implemented as a Rust builtin (not an LLT function) to avoid
        // needing a closure env with stdlib builtins.
        fn pred_eq_299(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let val = crate::eval::materialize(&ctx.args[0], None, &ctx.ctx, ctx.depth)?;
            ok_val(Value::Bool(matches!(val, Value::Int(299))), Span::origin())
        }

        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Create range(0, 300): lazy Seq(0, 1, ..., 299) via PendingBuiltin chain
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0)), thunk(Value::Int(300))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                let pred = thunk(Value::Builtin(crate::value::BuiltinDef {
                    func: pred_eq_299,
                    name: "pred_eq_299",
                    pos_strictness: &[],
                }));

                let filter_result = builtin_filter(BuiltinArgs {
                    args: &[pred, range_result],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Force the filter result. Before the fix this would fail with depth
                // exceeded after ~128 consecutive failures. After the fix the internal
                // loop handles all 299 failures at constant depth.
                let val = crate::eval::materialize(&filter_result, None, &test_ctx(), 0).unwrap();
                match val {
                    Value::Seq { head, .. } => {
                        let head_val =
                            crate::eval::materialize(&head, None, &test_ctx(), 0).unwrap();
                        assert_eq!(
                            head_val,
                            Value::Int(299),
                            "expected Int(299) as first passing element"
                        );
                    }
                    other => panic!(
                        "expected Seq from filter with one passing element, got {:?}",
                        other
                    ),
                }
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn test_filter_dict_step_no_depth_accumulation() {
        // Verify that filter_dict_step with consecutive predicate failures in a Dict
        // does NOT accumulate depth. This test mirrors filter_seq_step_no_depth_accumulation_on_consecutive_failures
        // but for the dict path.
        //
        // Before the fix, each failing dict entry created a PendingBuiltin at depth+1,
        // so N failures consumed ~2N depth units. After the fix, the skip branch uses
        // an internal loop, so N failures cost zero extra depth.
        //
        // Test: Create a dict with ~300 entries where NONE pass the predicate (all fail).
        // Call builtin_filter with depth near MAX_EVAL_DEPTH (e.g., depth=200).
        // Collect the result via builtin_collect to force materialization.
        // Assert the result is an empty dict (no depth exceeded error).
        fn pred_always_false(_ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            ok_val(Value::Bool(false), Span::origin())
        }

        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Create a dict with 300 entries where all fail the predicate
                let mut dict_map = IndexMap::new();
                for i in 0..300 {
                    dict_map.insert(Key::Int(i), thunk(Value::Int(i)));
                }
                let dict_thunk = thunk(Value::Dict(dict_map));

                let pred = thunk(Value::Builtin(crate::value::BuiltinDef {
                    func: pred_always_false,
                    name: "pred_always_false",
                    pos_strictness: &[],
                }));

                // Call filter at depth=200 (near MAX_EVAL_DEPTH=256)
                // If filter_dict_step accumulates depth incorrectly, this would hit
                // DepthExceeded after ~27 entries (200 + 27*2 ≥ 256).
                // With the fix, all 300 failures are handled at constant depth.
                let filter_result = builtin_filter(BuiltinArgs {
                    args: &[pred, dict_thunk],
                    named: no_named(),
                    depth: 200,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Convert lazy Seq to Dict via builtin_collect, then materialize
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[filter_result],
                    named: no_named(),
                    depth: 200,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                let val =
                    crate::eval::materialize(&collect_result, None, &test_ctx(), 200).unwrap();
                match val {
                    Value::Dict(ref map) => {
                        assert_eq!(
                            map.len(),
                            0,
                            "expected empty dict (collect of filter with all failing entries)"
                        );
                    }
                    other => panic!(
                        "expected Dict from collect of filter with all failing entries, got {:?}",
                        other
                    ),
                }
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
    fn concat_empty_xs_dict_ys_non_collection_returns_type_error() {
        // Task 2: When xs is an empty Dict, concat must validate that ys is also a
        // collection (Dict or Seq). Before the fix, concat([], 42) would silently
        // return the integer thunk. After the fix, a type error is returned.
        let xs = thunk(Value::Dict(IndexMap::new())); // empty dict
        let ys = thunk(Value::Int(42)); // not a collection

        let err = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();

        // type_mismatch_ctx with a context produces "concat: expected ..., got ..."
        assert!(
            err.message().contains("concat"),
            "expected 'concat' in error, got: {}",
            err.message()
        );
        assert!(
            err.message().contains("Dict or Seq"),
            "expected 'Dict or Seq' in error, got: {}",
            err.message()
        );
        assert!(
            err.message().contains("Int"),
            "expected 'Int' in error (got type name), got: {}",
            err.message()
        );
    }

    #[test]
    fn concat_empty_xs_dict_ys_valid_dict_succeeds() {
        // Task 2: When xs is empty Dict and ys is a valid Dict, concat should succeed.
        let xs = thunk(Value::Dict(IndexMap::new())); // empty dict
        let mut ys_map = IndexMap::new();
        ys_map.insert(Key::Int(0), thunk(Value::Int(99)));
        let ys = thunk(Value::Dict(ys_map));

        // Should succeed and return ys (the same thunk or an equivalent materialized form)
        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        });

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let val = mat(result);
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(
                    crate::eval::materialize(m.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0)
                        .unwrap(),
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. Stack safety is guaranteed by the iterative materialize_rc loop; these tests verify the depth-limit POLICY only."]
    fn take_large_count_infinite_seq_depth_exceeded() {
        // Verify that $take with a count exceeding MAX_EVAL_DEPTH on an infinite sequence
        // hits the depth limit due to depth accumulation in the recursive PendingBuiltin chain.
        // This test verifies the fix where builtin_take passes depth+1 (not depth) when
        // creating the tail thunk.
        //
        // With the fix: depth accumulates as 1→2→...→257, hitting the depth > MAX_EVAL_DEPTH (256) guard.
        // (The initial call is at depth=0; each PendingBuiltin tail is created with depth+1,
        // so the chain of PendingBuiltin depths starts at 1.)
        // Without the fix: depth stays constant, allowing unbounded sequences.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow.
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Create infinite range starting at 0
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Try to take 260 elements (slightly more than MAX_EVAL_DEPTH=256)
                // This ensures we hit the depth limit.
                let take_result = builtin_take(BuiltinArgs {
                    args: &[thunk(Value::Int(260)), range_result],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Force the entire sequence by calling collect
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[take_result],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                });

                // Should fail with depth exceeded
                assert!(
                    collect_result.is_err(),
                    "collect(take(260, range(0))) should hit depth limit"
                );
                let err = collect_result.unwrap_err();
                assert!(
                    err.message().contains("maximum evaluation depth"),
                    "expected depth limit error, got: {}",
                    err.message()
                );
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
    fn test_proxy_returns_proxy_value() {
        let handler = thunk(Value::Int(42));
        let result = builtin_proxy(BuiltinArgs {
            args: &[handler.clone()],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap();

        let val = mat(Ok(result));
        match val {
            Value::Proxy { handler: h } => {
                // Verify the handler thunk is the same Rc
                assert!(Rc::ptr_eq(&h, &handler));
            }
            other => panic!("expected Proxy, got {:?}", other),
        }
    }

    #[test]
    fn test_proxy_arity_error() {
        // Zero args
        let err = builtin_proxy(BuiltinArgs {
            args: &[],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Two args
        let err = builtin_proxy(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Three args
        let err = builtin_proxy(BuiltinArgs {
            args: &[
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_proxy_named_arg_error() {
        let mut named = IndexMap::new();
        named.insert("handler".to_string(), thunk(Value::Int(42)));

        let err = builtin_proxy(BuiltinArgs {
            args: &[],
            named: Some(&named),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(
            err.message().contains("does not accept named arguments"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_drop_seq_step_non_int_remaining_error() {
        // Create a PendingBuiltin invocation of drop_seq_step where n_remaining
        // (first arg) is a String instead of an Int. This should trigger the
        // type mismatch error path.

        // Create args: [String("not an int"), Seq { head: Int(1), tail: empty dict }]
        let n_remaining = thunk(Value::String("not an int".to_string()));
        let seq_head = thunk(Value::Int(1));
        let seq_tail = thunk(Value::Dict(IndexMap::new()));
        let seq = thunk(Value::Seq {
            head: seq_head,
            tail: seq_tail,
        });

        // Create the PendingBuiltin thunk
        let pending_thunk = Rc::new(Thunk::new_pending_builtin(
            builtin!("drop", builtin_drop_seq_step),
            vec![n_remaining, seq],
            None,
            0,
            call_span(),
            Some(Rc::from("test drop_seq_step")),
            test_ctx(),
        ));

        // Materialize it and expect an error
        let result = crate::eval::materialize(&pending_thunk, None, &test_ctx(), 0);
        let err = result.unwrap_err();

        // Verify it's a TypeMismatch error with the expected message
        assert!(
            matches!(err.kind, crate::error::ErrorKind::TypeMismatch { .. }),
            "Expected ErrorKind::TypeMismatch, got: {:?}",
            err.kind
        );
        assert!(
            err.message().contains("drop") && err.message().contains("expected Int"),
            "Expected message to contain 'drop' and 'expected Int', got: {}",
            err.message()
        );
    }

    /// Verify that `create_stdlib_env()` loads without error and that a representative
    /// sample of expected public names is present in the resulting environment.
    ///
    /// This is a wholeness test: it catches regressions where a prelude function is
    /// accidentally removed, renamed, or fails to evaluate during stdlib loading.
    #[test]
    fn test_stdlib_wholeness() {
        let stdlib_env = create_stdlib_env().expect("create_stdlib_env() must not fail");
        let env = stdlib_env.borrow();

        // Names that must exist: Rust-native builtins registered in standard_builtins()
        // plus a representative selection of prelude-defined functions.
        let required_names: &[&str] = &[
            // Rust-native operators (registered in create_root_env())
            "$+",
            "$-",
            "$*",
            "$/",
            "$=",
            "$<",
            "$if",
            "$map",
            "$filter",
            "$reduce",
            "$fold",
            "$take",
            "$drop",
            // Prelude-defined wrappers and derived functions
            "$not",
            "$and",
            "$or",
            "$>",
            "$<=",
            "$>=",
            // Prelude utilities
            "$identity",
            "$first",
            "$rest",
            "$concat",
            "$reverse",
            "$empty?",
            "$get",
            "$has?",
            "$get-or",
            "$values",
            "$entries",
            "$sort",
            "$any?",
            "$all?",
            "$sum",
            "$min",
            "$max",
            "$count",
            "$contains?",
        ];

        for name in required_names {
            // Names in the stdlib env are stored without the leading '$'.
            let bare_name = name.trim_start_matches('$');
            assert!(
                env.get(bare_name).is_some(),
                "stdlib is missing expected binding: {name}"
            );
        }
    }

    // === drop/reduce/join PendingCall chain construction tests ===

    #[test]
    fn drop_constructs_pending_call() {
        // drop(2, seq) should return a PendingBuiltin wrapping a chain of drop_seq_step calls
        let seq = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(10))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        let result = mat(builtin_drop(BuiltinArgs {
            args: &[thunk(Value::Int(2)), thunk(seq)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        // Result should be a PendingBuiltin (can't inspect internal state, but can verify it materializes correctly)
        match result {
            Value::Seq { head, .. } => {
                // First element after dropping 2 should be 2
                assert_eq!(
                    materialize(&head, None, &test_ctx(), 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Seq from drop, got {:?}", other),
        }
    }

    #[test]
    fn reduce_constructs_pending_call() {
        // reduce(+, 0, [1, 2]) should create a PendingCall chain
        let seq_val = {
            let dict_entries = vec![
                (Key::Int(0), thunk(Value::Int(1))),
                (Key::Int(1), thunk(Value::Int(2))),
            ];
            let map = dict_entries.into_iter().collect();
            Value::Dict(map)
        };

        let add_builtin = standard_builtins()
            .into_iter()
            .find(|def| def.name == "+")
            .map(|def| Value::Builtin(def))
            .unwrap();

        let result = mat(builtin_reduce(BuiltinArgs {
            args: &[thunk(add_builtin), thunk(Value::Int(0)), thunk(seq_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        // Result should be 3 (0 + 1 + 2)
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn join_constructs_pending_call() {
        // join(",", ["a", "b"]) should create a PendingCall chain
        let seq_val = {
            let dict_entries = vec![
                (Key::Int(0), thunk(Value::String("a".into()))),
                (Key::Int(1), thunk(Value::String("b".into()))),
            ];
            let map = dict_entries.into_iter().collect();
            Value::Dict(map)
        };

        let result = mat(builtin_join(BuiltinArgs {
            args: &[thunk(Value::String(",".into())), thunk(seq_val)],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));

        // Result should be "a,b"
        assert_eq!(result, Value::String("a,b".into()));
    }

    /// Helper: create a function whose closure env contains builtins (needed for
    /// tests where the function body calls builtins like $builtin-add).
    fn n_arg_fn_with_builtins(param_names: &[&str], body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(
                param_names
                    .iter()
                    .map(|name| Param {
                        name: name.to_string(),
                        annotation: None,
                        variadic: false,
                    })
                    .collect(),
            ),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: create_root_env(),
        }
    }

    #[test]
    fn test_builtin_until_basic() {
        // Count from 0 to 10 using until
        // pred: [fn [x] [call $builtin-eq $x 10]]
        // f: [fn [x] [call $builtin-add $x 1]]
        // init: 0
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let pred = n_arg_fn_with_builtins(
                    &["x"],
                    Expr::Call {
                        func: Box::new(Spanned::new(
                            Expr::VarRef("builtin-eq".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::VarRef("x".to_string()),
                                test_span(1, 1, 1, 2),
                            )),
                            Rc::new(Spanned::new(Expr::Int(10), test_span(1, 1, 1, 2))),
                        ],
                        named_args: vec![],
                        implied: false,
                    },
                );
                let f = n_arg_fn_with_builtins(
                    &["x"],
                    Expr::Call {
                        func: Box::new(Spanned::new(
                            Expr::VarRef("builtin-add".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::VarRef("x".to_string()),
                                test_span(1, 1, 1, 2),
                            )),
                            Rc::new(Spanned::new(Expr::Int(1), test_span(1, 1, 1, 2))),
                        ],
                        named_args: vec![],
                        implied: false,
                    },
                );

                let result = mat(builtin_until(BuiltinArgs {
                    args: &[thunk(pred), thunk(f), thunk(Value::Int(0))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                }));

                assert_eq!(result, Value::Int(10));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_builtin_until_already_true() {
        // Predicate is true immediately, should return init unchanged
        // pred: [fn [x] true]
        // f: [fn [x] [call $error "should not be called"]]
        // init: 42
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let pred = n_arg_fn(&["x"], Expr::Bool(true));
                let f = n_arg_fn(
                    &["x"],
                    Expr::Call {
                        func: Box::new(Spanned::new(
                            Expr::VarRef("error".to_string()),
                            test_span(1, 1, 1, 5),
                        )),
                        args: vec![Rc::new(Spanned::new(
                            Expr::Str("should not be called".to_string()),
                            test_span(1, 1, 1, 20),
                        ))],
                        named_args: vec![],
                        implied: false,
                    },
                );

                let result = mat(builtin_until(BuiltinArgs {
                    args: &[thunk(pred), thunk(f), thunk(Value::Int(42))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                }));

                assert_eq!(result, Value::Int(42));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_builtin_until_many_iterations() {
        // Test that we can exceed MAX_EVAL_DEPTH (256) iterations
        // Count from 0 to 300
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let pred = n_arg_fn_with_builtins(
                    &["x"],
                    Expr::Call {
                        func: Box::new(Spanned::new(
                            Expr::VarRef("builtin-eq".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::VarRef("x".to_string()),
                                test_span(1, 1, 1, 2),
                            )),
                            Rc::new(Spanned::new(Expr::Int(300), test_span(1, 1, 1, 3))),
                        ],
                        named_args: vec![],
                        implied: false,
                    },
                );
                let f = n_arg_fn_with_builtins(
                    &["x"],
                    Expr::Call {
                        func: Box::new(Spanned::new(
                            Expr::VarRef("builtin-add".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::VarRef("x".to_string()),
                                test_span(1, 1, 1, 2),
                            )),
                            Rc::new(Spanned::new(Expr::Int(1), test_span(1, 1, 1, 2))),
                        ],
                        named_args: vec![],
                        implied: false,
                    },
                );

                let result = mat(builtin_until(BuiltinArgs {
                    args: &[thunk(pred), thunk(f), thunk(Value::Int(0))],
                    named: no_named(),
                    depth: 0,
                    call_span: call_span(),
                    ctx: test_ctx(),
                }));

                assert_eq!(result, Value::Int(300));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    // -------------------------------------------------------------------------
    // Unit tests: builtin_rest, builtin_cons, builtin_reverse, builtin_sort
    // -------------------------------------------------------------------------

    fn make_int_dict(vals: &[i64]) -> Value {
        let mut map = IndexMap::new();
        for (i, &v) in vals.iter().enumerate() {
            map.insert(Key::Int(i as i64), thunk(Value::Int(v)));
        }
        Value::Dict(map)
    }

    fn extract_int_at(map: &IndexMap<Key, Rc<Thunk>>, idx: i64) -> i64 {
        match crate::eval::materialize(map.get(&Key::Int(idx)).unwrap(), None, &test_ctx(), 0)
            .unwrap()
        {
            Value::Int(n) => n,
            other => panic!("expected Int at index {idx}, got {:?}", other),
        }
    }

    #[test]
    fn rest_three_elements_drops_first() {
        let result = mat(builtin_rest(BuiltinArgs {
            args: &[thunk(make_int_dict(&[10, 20, 30]))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 2);
        assert_eq!(extract_int_at(&m, 0), 20);
        assert_eq!(extract_int_at(&m, 1), 30);
    }

    #[test]
    fn rest_single_element_returns_empty() {
        let result = mat(builtin_rest(BuiltinArgs {
            args: &[thunk(make_int_dict(&[42]))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn rest_empty_dict_returns_empty() {
        let result = mat(builtin_rest(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn cons_prepends_element() {
        let result = mat(builtin_cons(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(make_int_dict(&[1, 2, 3]))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 4);
        assert_eq!(extract_int_at(&m, 0), 0);
        assert_eq!(extract_int_at(&m, 1), 1);
        assert_eq!(extract_int_at(&m, 3), 3);
    }

    #[test]
    fn cons_onto_empty_dict() {
        let result = mat(builtin_cons(BuiltinArgs {
            args: &[thunk(Value::Int(99)), thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 1);
        assert_eq!(extract_int_at(&m, 0), 99);
    }

    #[test]
    fn reverse_three_elements() {
        let result = mat(builtin_reverse(BuiltinArgs {
            args: &[thunk(make_int_dict(&[10, 20, 30]))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 3);
        assert_eq!(extract_int_at(&m, 0), 30);
        assert_eq!(extract_int_at(&m, 1), 20);
        assert_eq!(extract_int_at(&m, 2), 10);
    }

    #[test]
    fn reverse_empty_dict() {
        let result = mat(builtin_reverse(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn sort_integers_ascending() {
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk(make_int_dict(&[3, 1, 4, 1, 5]))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 5);
        let expected = [1i64, 1, 3, 4, 5];
        for (i, &exp) in expected.iter().enumerate() {
            assert_eq!(extract_int_at(&m, i as i64), exp, "at index {i}");
        }
    }

    #[test]
    fn sort_strings_lexicographic() {
        let mut map = IndexMap::new();
        for (i, s) in ["banana", "apple", "cherry"].iter().enumerate() {
            map.insert(Key::Int(i as i64), thunk(Value::String(s.to_string())));
        }
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk(Value::Dict(map))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        let v0 =
            crate::eval::materialize(m.get(&Key::Int(0)).unwrap(), None, &test_ctx(), 0).unwrap();
        assert_eq!(v0, Value::String("apple".into()));
    }

    #[test]
    fn sort_empty_dict() {
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            depth: 0,
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }
}
