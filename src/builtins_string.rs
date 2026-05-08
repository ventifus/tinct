//! String builtins: `str`, `split`, `replace`, `upper`, `lower`, `trim`, `str-length`,
//! `str-contains?`, `str-slice`, `str-chars`, `starts-with?`, `ends-with?`.
//!
//! These builtins operate on String values. They are all inherently materializing
//! because they must inspect string content to compute their results.
//!
//! Some builtins (`starts-with?`, `ends-with?`) support dual-dispatch: they work on
//! both String and Seq values.
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
use crate::value::{string_val, BuiltinArgs, Key, Thunk, ThunkId, Value};

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
    // Estimate capacity: average string ~20 bytes, typical config keys ~10 bytes.
    // Conservative underestimate better than zero capacity.
    let estimated_capacity = args.len() * 10;
    let mut result = String::with_capacity(estimated_capacity);
    for arg in args {
        let val = materialize(arg, Some(&call_span), &ctx, depth)?;
        result.push_str(&stringify(&val));
    }
    ok_val(string_val(&result), call_span)
}

/// `split`: Split a string by a separator.
///
/// Takes 2 args: `separator` (String), `input` (String).
/// Returns a Dict with integer keys `0..n` mapping to the split substrings.
/// Zero-copy: split parts share the original string's Rc<str> allocation.
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

    // Extract the source Rc<str> from the input string for zero-copy slicing.
    let (input_source, input_start, input_end) = match input_val {
        Value::String { source, start, end } => (source, start, end),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "split".to_string(),
                "String",
                input_val.type_name(),
                args[1].span,
            )
            .into());
        }
    };
    let input_str = &input_source[input_start..input_end];

    // Bound allocation before the guard fires: take at most MAX_SPLIT_PARTS + 1 entries
    // so that adversarial input (e.g., splitting a large string by empty separator) cannot
    // heap-exhaust the process before the check.
    let mut map: IndexMap<Key, ThunkId> = IndexMap::new();

    // Use match_indices to get byte offsets of each separator occurrence.
    if sep.is_empty() {
        // Empty separator: Rust's split("") behavior is to return empty string, then each char, then empty string.
        // For "abc" split by "", we get: ["", "a", "b", "c", ""]
        // Check bounds before allocating char_boundaries Vec
        let char_count = input_str.chars().count();
        if char_count + 1 > MAX_SPLIT_PARTS {
            return Err(EvalError::resource_limit_exceeded(
                format!("$split: input produces more than {MAX_SPLIT_PARTS} parts"),
                call_span,
            )
            .into());
        }

        // Build char boundaries: [0, <after 'a'>, <after 'b'>, <after 'c'>]
        let char_boundaries: Vec<usize> = std::iter::once(0)
            .chain(input_str.char_indices().map(|(i, c)| i + c.len_utf8()))
            .collect();

        // First part: empty string at the start
        let thunk = Rc::new(Thunk::new_materialized(
            Value::String {
                source: Rc::clone(&input_source),
                start: input_start,
                end: input_start,
            },
            call_span,
        ));
        map.insert(Key::Int(0), ctx.alloc_thunk(thunk));

        // Each character
        for (i, window) in char_boundaries.windows(2).enumerate() {
            let part_start = input_start + window[0];
            let part_end = input_start + window[1];
            let thunk = Rc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: part_start,
                    end: part_end,
                },
                call_span,
            ));
            map.insert(
                Key::Int(i64::try_from(i + 1).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "$split: result index too large".to_string(),
                        call_span,
                    )
                })?),
                ctx.alloc_thunk(thunk),
            );
        }

        // Last part: empty string at the end
        let last_idx = char_boundaries.len();
        let thunk = Rc::new(Thunk::new_materialized(
            Value::String {
                source: Rc::clone(&input_source),
                start: input_end,
                end: input_end,
            },
            call_span,
        ));
        map.insert(
            Key::Int(i64::try_from(last_idx).map_err(|_| {
                EvalError::resource_limit_exceeded(
                    "$split: result index too large".to_string(),
                    call_span,
                )
            })?),
            ctx.alloc_thunk(thunk),
        );
    } else {
        // Non-empty separator: split by separator pattern.
        let mut last_end = 0;
        let mut part_count = 0;

        for (match_start, _) in input_str.match_indices(&sep) {
            if part_count >= MAX_SPLIT_PARTS {
                return Err(EvalError::resource_limit_exceeded(
                    format!("$split: input produces more than {MAX_SPLIT_PARTS} parts"),
                    call_span,
                )
                .into());
            }

            // Part before this separator
            let part_start = input_start + last_end;
            let part_end = input_start + match_start;
            let thunk = Rc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: part_start,
                    end: part_end,
                },
                call_span,
            ));
            map.insert(
                Key::Int(i64::try_from(part_count).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "$split: result index too large".to_string(),
                        call_span,
                    )
                })?),
                ctx.alloc_thunk(thunk),
            );

            part_count += 1;
            last_end = match_start + sep.len();
        }

        // Final part after the last separator (or entire string if no separator found)
        if part_count >= MAX_SPLIT_PARTS {
            return Err(EvalError::resource_limit_exceeded(
                format!("$split: input produces more than {MAX_SPLIT_PARTS} parts"),
                call_span,
            )
            .into());
        }
        let part_start = input_start + last_end;
        let part_end = input_end;
        let thunk = Rc::new(Thunk::new_materialized(
            Value::String {
                source: Rc::clone(&input_source),
                start: part_start,
                end: part_end,
            },
            call_span,
        ));
        map.insert(
            Key::Int(i64::try_from(part_count).map_err(|_| {
                EvalError::resource_limit_exceeded(
                    "$split: result index too large".to_string(),
                    call_span,
                )
            })?),
            ctx.alloc_thunk(thunk),
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
        input.matches(&pattern).count()
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
        return ok_val(string_val(&input), call_span);
    }

    ok_val(
        string_val(&input.replace(&pattern, &replacement)),
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

    ok_val(string_val(&result), call_span)
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

    ok_val(string_val(&result), call_span)
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
    ok_val(string_val(s.trim()), call_span)
}

/// `str-length`: Return the length of a string in UTF-8 characters (not bytes).
///
/// Takes 1 arg (String). Returns an Int.
/// Inherently materializing: must inspect string content to count characters.
pub(crate) fn builtin_str_length(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("str-length", args, named, &ctx, depth, call_span)?;
    let s = require_string("str-length", val, args[0].span)?;
    let len = s.chars().count();
    let len_i64 = i64::try_from(len).map_err(|_| {
        EvalError::resource_limit_exceeded(
            "str-length: string length exceeds i64::MAX".to_string(),
            call_span,
        )
    })?;
    ok_val(Value::Int(len_i64), call_span)
}

/// `str-contains?`: Check if a string contains a substring.
///
/// Takes 2 args: `haystack` (String), `needle` (String).
/// Returns a Bool indicating whether needle is found in haystack.
/// Inherently materializing: must inspect string content to search for substring.
pub(crate) fn builtin_str_contains(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("str-contains?", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let haystack_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let needle_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    let haystack = require_string("str-contains?", haystack_val, args[0].span)?;
    let needle = require_string("str-contains?", needle_val, args[1].span)?;

    ok_val(Value::Bool(haystack.contains(&needle)), call_span)
}

/// `str-slice`: Extract a substring by character indices [start, end).
///
/// Takes 3 args: `input` (String), `start` (Int), `end` (Int).
/// Returns a zero-copy slice of the input string.
/// Inherently materializing: must inspect string content to find character boundaries.
pub(crate) fn builtin_str_slice(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("str-slice", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    // Materialize all arguments
    let input_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let start_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;
    let end_val = materialize(&args[2], Some(&call_span), &ctx, depth)?;

    // Extract the source Rc<str> from the input string
    let (input_source, input_start, input_end) = match input_val {
        Value::String { source, start, end } => (source, start, end),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "str-slice".to_string(),
                "String",
                input_val.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    // Extract start and end indices
    let start_idx = match start_val {
        Value::Int(n) if n >= 0 => n as usize,
        Value::Int(n) => {
            return Err(EvalError::type_mismatch_ctx(
                "str-slice".to_string(),
                "non-negative Int",
                &format!("Int({n})"),
                args[1].span,
            )
            .into());
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "str-slice".to_string(),
                "Int",
                start_val.type_name(),
                args[1].span,
            )
            .into());
        }
    };

    let end_idx = match end_val {
        Value::Int(n) if n >= 0 => n as usize,
        Value::Int(n) => {
            return Err(EvalError::type_mismatch_ctx(
                "str-slice".to_string(),
                "non-negative Int",
                &format!("Int({n})"),
                args[2].span,
            )
            .into());
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "str-slice".to_string(),
                "Int",
                end_val.type_name(),
                args[2].span,
            )
            .into());
        }
    };

    // Validate indices
    if start_idx > end_idx {
        return Err(EvalError::new(
            format!("str-slice: start index {start_idx} > end index {end_idx}"),
            call_span,
        )
        .into());
    }

    // Convert character indices to byte offsets
    let input_str = &input_source[input_start..input_end];
    let char_indices: Vec<usize> = input_str
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(input_str.len()))
        .collect();

    let char_count = char_indices.len().saturating_sub(1);
    if end_idx > char_count {
        return Err(EvalError::new(
            format!(
                "str-slice: end index {end_idx} out of bounds (string has {char_count} characters)"
            ),
            call_span,
        )
        .into());
    }

    let byte_start = input_start + char_indices[start_idx];
    let byte_end = input_start + char_indices[end_idx];

    ok_val(
        Value::String {
            source: input_source,
            start: byte_start,
            end: byte_end,
        },
        call_span,
    )
}

/// `starts-with?`: Check if a string or sequence starts with a prefix.
///
/// Dual-dispatch:
/// - String mode: Takes 2 args: `haystack` (String), `prefix` (String).
///   Returns a Bool indicating whether haystack starts with prefix.
/// - Seq mode: Takes 2 args: `haystack` (Seq), `prefix` (Seq).
///   Returns a Bool indicating whether haystack's elements match prefix's elements.
/// Inherently materializing: must inspect string/seq content to check prefix.
pub(crate) fn builtin_starts_with(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("starts-with?", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let haystack_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let prefix_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    match (&haystack_val, &prefix_val) {
        (Value::String { .. }, Value::String { .. }) => {
            let haystack = require_string("starts-with?", haystack_val, args[0].span)?;
            let prefix = require_string("starts-with?", prefix_val, args[1].span)?;
            ok_val(Value::Bool(haystack.starts_with(&prefix)), call_span)
        }
        (Value::Seq { .. }, Value::Seq { .. }) => {
            // Element-by-element prefix matching
            let mut haystack_thunk = ctx.get_thunk(match haystack_val {
                Value::Seq { head: _, tail } => tail,
                _ => unreachable!(),
            });
            let mut prefix_thunk = ctx.get_thunk(match prefix_val {
                Value::Seq { head: _, tail } => tail,
                _ => unreachable!(),
            });

            // Check first elements
            let haystack_head = ctx.get_thunk(match haystack_val {
                Value::Seq { head, tail: _ } => head,
                _ => unreachable!(),
            });
            let prefix_head = ctx.get_thunk(match prefix_val {
                Value::Seq { head, tail: _ } => head,
                _ => unreachable!(),
            });

            let haystack_head_val = materialize(&haystack_head, Some(&call_span), &ctx, depth)?;
            let prefix_head_val = materialize(&prefix_head, Some(&call_span), &ctx, depth)?;

            if haystack_head_val != prefix_head_val {
                return ok_val(Value::Bool(false), call_span);
            }

            // Check remaining elements
            loop {
                let prefix_tail_val = materialize(&prefix_thunk, Some(&call_span), &ctx, depth)?;

                match prefix_tail_val {
                    Value::Dict(ref map) if map.is_empty() => {
                        // Prefix exhausted, match succeeds
                        return ok_val(Value::Bool(true), call_span);
                    }
                    Value::Seq {
                        head: prefix_h,
                        tail: prefix_t,
                    } => {
                        let haystack_tail_val =
                            materialize(&haystack_thunk, Some(&call_span), &ctx, depth)?;

                        match haystack_tail_val {
                            Value::Dict(ref map) if map.is_empty() => {
                                // Haystack exhausted before prefix, no match
                                return ok_val(Value::Bool(false), call_span);
                            }
                            Value::Seq {
                                head: haystack_h,
                                tail: haystack_t,
                            } => {
                                let h_val = materialize(
                                    &ctx.get_thunk(haystack_h),
                                    Some(&call_span),
                                    &ctx,
                                    depth,
                                )?;
                                let p_val = materialize(
                                    &ctx.get_thunk(prefix_h),
                                    Some(&call_span),
                                    &ctx,
                                    depth,
                                )?;

                                if h_val != p_val {
                                    return ok_val(Value::Bool(false), call_span);
                                }

                                haystack_thunk = ctx.get_thunk(haystack_t);
                                prefix_thunk = ctx.get_thunk(prefix_t);
                            }
                            _ => {
                                return Err(EvalError::type_mismatch_ctx(
                                    "starts-with?".to_string(),
                                    "Seq",
                                    haystack_tail_val.type_name(),
                                    args[0].span,
                                )
                                .into());
                            }
                        }
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "starts-with?".to_string(),
                            "Seq",
                            prefix_tail_val.type_name(),
                            args[1].span,
                        )
                        .into());
                    }
                }
            }
        }
        _ => Err(EvalError::type_mismatch_ctx(
            "starts-with?".to_string(),
            "String or Seq",
            haystack_val.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `ends-with?`: Check if a string or sequence ends with a suffix.
///
/// Dual-dispatch:
/// - String mode: Takes 2 args: `haystack` (String), `suffix` (String).
///   Returns a Bool indicating whether haystack ends with suffix.
/// - Seq mode: Takes 2 args: `haystack` (Seq), `suffix` (Seq).
///   Returns a Bool indicating whether haystack's trailing elements match suffix's elements.
/// Inherently materializing: must inspect string/seq content to check suffix.
pub(crate) fn builtin_ends_with(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("ends-with?", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let haystack_val = materialize(&args[0], Some(&call_span), &ctx, depth)?;
    let suffix_val = materialize(&args[1], Some(&call_span), &ctx, depth)?;

    match (&haystack_val, &suffix_val) {
        (Value::String { .. }, Value::String { .. }) => {
            let haystack = require_string("ends-with?", haystack_val, args[0].span)?;
            let suffix = require_string("ends-with?", suffix_val, args[1].span)?;
            ok_val(Value::Bool(haystack.ends_with(&suffix)), call_span)
        }
        (Value::Seq { .. }, Value::Seq { .. }) => {
            // Convert both sequences to vectors for easier comparison
            let mut haystack_vec = Vec::new();
            let mut current_thunk = Rc::clone(&args[0]);
            loop {
                let val = materialize(&current_thunk, Some(&call_span), &ctx, depth)?;
                match val {
                    Value::Dict(ref map) if map.is_empty() => break,
                    Value::Seq { head, tail } => {
                        haystack_vec.push(ctx.get_thunk(head));
                        current_thunk = ctx.get_thunk(tail);
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "ends-with?".to_string(),
                            "Seq",
                            val.type_name(),
                            args[0].span,
                        )
                        .into());
                    }
                }
            }

            let mut suffix_vec = Vec::new();
            let mut current_thunk = Rc::clone(&args[1]);
            loop {
                let val = materialize(&current_thunk, Some(&call_span), &ctx, depth)?;
                match val {
                    Value::Dict(ref map) if map.is_empty() => break,
                    Value::Seq { head, tail } => {
                        suffix_vec.push(ctx.get_thunk(head));
                        current_thunk = ctx.get_thunk(tail);
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "ends-with?".to_string(),
                            "Seq",
                            val.type_name(),
                            args[1].span,
                        )
                        .into());
                    }
                }
            }

            // Check if suffix is longer than haystack
            if suffix_vec.len() > haystack_vec.len() {
                return ok_val(Value::Bool(false), call_span);
            }

            // Compare elements from the end
            let offset = haystack_vec.len() - suffix_vec.len();
            for (i, suffix_thunk) in suffix_vec.iter().enumerate() {
                let haystack_elem =
                    materialize(&haystack_vec[offset + i], Some(&call_span), &ctx, depth)?;
                let suffix_elem = materialize(suffix_thunk, Some(&call_span), &ctx, depth)?;
                if haystack_elem != suffix_elem {
                    return ok_val(Value::Bool(false), call_span);
                }
            }

            ok_val(Value::Bool(true), call_span)
        }
        _ => Err(EvalError::type_mismatch_ctx(
            "ends-with?".to_string(),
            "String or Seq",
            haystack_val.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `str-chars`: Convert a string to a lazy sequence of single-character strings.
///
/// Takes 1 arg: `input` (String).
/// Returns a lazy Seq where each element is a zero-copy String slice of one Unicode codepoint.
/// Each slice shares the original Rc<str> source.
/// Inherently materializing: must inspect string content to identify character boundaries.
pub(crate) fn builtin_str_chars(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("str-chars", args, named, &ctx, depth, call_span)?;

    let (input_source, input_start, input_end) = match val {
        Value::String { source, start, end } => (source, start, end),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "str-chars".to_string(),
                "String",
                val.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    let input_str = &input_source[input_start..input_end];

    // Build character ranges: each character is [start_byte, end_byte)
    let mut char_ranges: Vec<(usize, usize)> = Vec::new();
    for (byte_idx, ch) in input_str.char_indices() {
        char_ranges.push((byte_idx, byte_idx + ch.len_utf8()));
    }

    // Build the sequence from the end to the beginning (right-to-left)
    let mut result = Rc::new(Thunk::new_materialized(
        Value::Dict(indexmap::IndexMap::new()),
        call_span,
    ));

    for (char_start, char_end) in char_ranges.into_iter().rev() {
        let head = Rc::new(Thunk::new_materialized(
            Value::String {
                source: Rc::clone(&input_source),
                start: input_start + char_start,
                end: input_start + char_end,
            },
            call_span,
        ));

        result = Rc::new(Thunk::new_materialized(
            Value::Seq {
                head: ctx.alloc_thunk(head),
                tail: ctx.alloc_thunk(result),
            },
            call_span,
        ));
    }

    Ok(result)
}

/// `char-code`: Get the Unicode codepoint of the first character in a string.
///
/// Takes 1 arg: `input` (String).
/// Returns an Int representing the Unicode codepoint of the first character.
/// Returns an error if the string is empty.
/// Inherently materializing: must inspect string content to extract the first character.
pub(crate) fn builtin_char_code(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("char-code", args, named, &ctx, depth, call_span)?;

    let (input_source, input_start, input_end) = match val {
        Value::String { source, start, end } => (source, start, end),
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "char-code".to_string(),
                "String",
                val.type_name(),
                args[0].span,
            )
            .into());
        }
    };

    let input_str = &input_source[input_start..input_end];

    if let Some(ch) = input_str.chars().next() {
        ok_val(Value::Int(ch as u32 as i64), call_span)
    } else {
        Err(EvalError::new("char-code: empty string".to_string(), call_span).into())
    }
}

/// `chr`: Convert a Unicode codepoint to a single-character string.
///
/// Takes 1 arg: `codepoint` (Int).
/// Returns a String containing the character corresponding to the codepoint.
/// Returns an error if the codepoint is invalid.
/// Inherently materializing: must convert the integer to a character.
pub(crate) fn builtin_chr(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("chr", args, named, &ctx, depth, call_span)?;

    match val {
        Value::Int(n) => {
            if let Some(ch) = char::from_u32(n as u32) {
                ok_val(string_val(&ch.to_string()), call_span)
            } else {
                Err(
                    EvalError::new(format!("chr: invalid Unicode codepoint {}", n), call_span)
                        .into(),
                )
            }
        }
        _ => Err(EvalError::type_mismatch_ctx(
            "chr".to_string(),
            "Int",
            val.type_name(),
            args[0].span,
        )
        .into()),
    }
}

/// `str-bytes`: Convert a string to a sequence of byte values (stub).
///
/// This is a placeholder for the bytes-type sprint. Currently returns an error.
pub(crate) fn builtin_str_bytes(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let call_span = ctx_arg.call_span;
    Err(EvalError::new(
        "str-bytes requires Value::Bytes from bytes-type sprint".to_string(),
        call_span,
    )
    .into())
}

/// `bytes-str`: Convert a sequence of byte values to a string (stub).
///
/// This is a placeholder for the bytes-type sprint. Currently returns an error.
pub(crate) fn builtin_bytes_str(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let call_span = ctx_arg.call_span;
    Err(EvalError::new(
        "bytes-str requires Value::Bytes from bytes-type sprint".to_string(),
        call_span,
    )
    .into())
}
