//! String builtins: `str`, `split`, `replace`, `upper`, `lower`, `trim`.
//!
//! These builtins operate on String values. They are all inherently materializing
//! because they must inspect string content to compute their results.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::builtins::{
    expect_one_arg, ok_val, reject_named, require_string, stringify, MAX_STRING_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Key, Thunk, Value};

/// Maximum number of parts produced by `$split` (1,000,000 elements).
/// Prevents heap exhaustion from splitting by empty separator or small patterns.
pub(crate) const MAX_SPLIT_PARTS: usize = 1_000_000;

/// `str`: Variadic string concatenation and toString.
///
/// Materializes each argument and concatenates their string representations.
/// With zero args, returns an empty string.
/// Inherently materializing: must inspect values to convert to string representation.
pub(crate) fn builtin_str(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("str", named, call_span)?;
    let mut result = String::new();
    for arg in args {
        let val = materialize(arg, Some(&call_span), &ctx, depth)?;
        result.push_str(&stringify(&val));
    }
    ok_val(Value::String(result), call_span)
}

/// `split`: Split a string by a separator.
///
/// Takes 2 args: `separator` (String), `input` (String).
/// Returns a Dict with integer keys `0..n` mapping to the split substrings.
/// Inherently materializing: must inspect string content to split into substrings.
pub(crate) fn builtin_split(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("split", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // arg[0] is pre-forced by BuiltinForceArg; this call is an O(1) cache hit.
    let sep_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    // arg[1] is forced synchronously; BuiltinForceArg only covers arg[0].
    // Acceptable: the input string is typically a small literal or bound variable.
    let input_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    let sep = require_string("split", sep_val, args[0].span)?;
    let input = require_string("split", input_val, args[1].span)?;

    // Bound allocation before the guard fires: take at most MAX_SPLIT_PARTS + 1 entries
    // so that adversarial input (e.g., splitting a large string by empty separator) cannot
    // heap-exhaust the process before the check.
    let parts: Vec<&str> = input
        .split(sep.as_str())
        .take(MAX_SPLIT_PARTS + 1)
        .collect();
    if parts.len() > MAX_SPLIT_PARTS {
        return Err(EvalError::resource_limit_exceeded(
            format!("$split: input produces more than {MAX_SPLIT_PARTS} parts"),
            call_span,
        )
        .into());
    }
    let mut map = IndexMap::with_capacity(parts.len());
    for (i, part) in parts.into_iter().enumerate() {
        map.insert(
            Key::Int(i64::try_from(i).map_err(|_| {
                EvalError::resource_limit_exceeded(
                    "$split: result index too large".to_string(),
                    call_span,
                )
            })?),
            Rc::new(Thunk::new_materialized(
                Value::String(part.to_string()),
                call_span,
            )),
        );
    }
    ok_val(Value::Dict(map), call_span)
}

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
/// Inherently materializing: must inspect string content to find and replace patterns.
pub(crate) fn builtin_replace(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("replace", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }
    // arg[0] is pre-forced by BuiltinForceArg; this call is an O(1) cache hit.
    let pattern_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    // args[1] and args[2] are forced synchronously; BuiltinForceArg only covers arg[0].
    // Acceptable: replacement and input strings are typically small literals or bound variables.
    let replacement_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let input_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    let pattern = require_string("replace", pattern_val, args[0].span)?;
    let replacement = require_string("replace", replacement_val, args[1].span)?;
    let input = require_string("replace", input_val, args[2].span)?;

    // Pre-check output size to prevent memory exhaustion.
    // Empty pattern inserts replacement between every character.
    let match_count = if pattern.is_empty() {
        input.chars().count() + 1
    } else {
        input.matches(pattern.as_str()).count()
    };

    // output_len = input.len() - (match_count * pattern.len()) + (match_count * replacement.len())
    let removed_bytes = match_count.saturating_mul(pattern.len());
    let added_bytes = match_count.saturating_mul(replacement.len());
    let output_len = input
        .len()
        .saturating_sub(removed_bytes)
        .saturating_add(added_bytes);

    if output_len > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "replace: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                output_len
            ),
            call_span,
        )
        .into());
    }

    // Fast-path: if there are no matches, return the input unchanged
    if match_count == 0 {
        return ok_val(Value::String(input.into()), call_span);
    }

    ok_val(
        Value::String(input.replace(pattern.as_str(), &replacement)),
        call_span,
    )
}

/// `upper`: Convert a string to uppercase. Takes 1 arg (String).
/// Inherently materializing: must inspect string content to convert case.
pub(crate) fn builtin_upper(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("upper", args, named, &ctx, depth, call_span)?;
    let s = require_string("upper", val, args[0].span)?;
    // Fast-path: if input already exceeds the limit, output cannot be smaller (Unicode expansion only grows).
    if s.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "upper: input exceeds {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                s.len()
            ),
            call_span,
        )
        .into());
    }
    let result = s.to_uppercase();
    // Post-conversion guard for Unicode expansion (e.g., ß → SS).
    if result.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "upper: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                result.len()
            ),
            call_span,
        )
        .into());
    }

    ok_val(Value::String(result), call_span)
}

/// `lower`: Convert a string to lowercase. Takes 1 arg (String).
/// Inherently materializing: must inspect string content to convert case.
pub(crate) fn builtin_lower(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("lower", args, named, &ctx, depth, call_span)?;
    let s = require_string("lower", val, args[0].span)?;
    // Fast-path: if input already exceeds the limit, output cannot be smaller (Unicode expansion only grows).
    if s.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "lower: input exceeds {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                s.len()
            ),
            call_span,
        )
        .into());
    }
    let result = s.to_lowercase();
    // Post-conversion guard for Unicode expansion (e.g., İ → i\u{307}).
    if result.len() > MAX_STRING_SIZE {
        return Err(EvalError::resource_limit_exceeded(
            format!(
                "lower: output would exceed {} MB limit ({} bytes)",
                MAX_STRING_SIZE / (1024 * 1024),
                result.len()
            ),
            call_span,
        )
        .into());
    }

    ok_val(Value::String(result), call_span)
}

/// `trim`: Remove leading and trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the trimmed string.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("trim", args, named, &ctx, depth, call_span)?;
    let s = require_string("trim", val, args[0].span)?;
    ok_val(Value::String(s.trim().to_string()), call_span)
}
