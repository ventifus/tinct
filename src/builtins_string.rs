//! String builtins: `str`, `split`, `replace`, `trim`, `str-length`,
//! `str-slice`, `str-chars`, `str-index-of`, `char-code`, `chr`, `str-bytes`, `bytes-str`,
//! `trim-start`, `trim-end`, `str-to-upper-char`, `str-to-lower-char`, `str-map-chars`,
//! `regex-match?`.
//!
//! Note: `upper` and `lower` are no longer Rust builtins. They live in `stdlib/strings.llt`
//! and are implemented using `str-map-chars` + `str-to-upper-char` / `str-to-lower-char`.
//!
//! These builtins operate on String values. They are all inherently materializing
//! because they must inspect string content to compute their results.
//!
//! Some builtins (`starts-with?`, `ends-with?`) support dual-dispatch: they work on
//! both String and Seq values.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{
    expect_one_arg, ok_val, reject_named, require_string, stringify, MAX_STRING_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::eval_call::CallContext;
use crate::value::{string_val, BuiltinArgs, Key, Thunk, ThunkId, Value};

/// Maximum number of parts produced by `$split` (1,000,000 elements).
/// Prevents heap exhaustion from splitting by empty separator or small patterns.
pub(crate) const MAX_SPLIT_PARTS: usize = 1_000_000;

/// `str`: Variadic string concatenation and toString.
///
/// Materializes each argument and concatenates their string representations.
/// With zero args, returns an empty string.
/// Inherently materializing: must inspect values to convert to string representation.
pub(crate) fn builtin_str(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("str", named.as_ref(), call_span.clone())?;

        // Pure primitive: no typeclass dispatch. Showable instances in stdlib/prelude.llt
        // are type-checker annotations only (same pattern as Addable/builtin-add for arithmetic).
        // Dispatch was removed because ShowableInt.str uses `str: str` (the builtin itself),
        // causing infinite recursion when dispatched back into builtin_str.
        // Estimate capacity: average string ~20 bytes, typical config keys ~10 bytes.
        // Conservative underestimate better than zero capacity.
        // NOTE: variadic builtin - can't use force_count, must materialize in loop
        let estimated_capacity = args.len() * 10;
        let mut result = String::with_capacity(estimated_capacity);
        for arg in &args {
            let val = materialize(arg, Some(&call_span), &ctx).await?;
            result.push_str(&stringify(&val));
        }
        ok_val(string_val(&result), call_span)
    })
}

/// `split`: Split a string by a separator.
///
/// Takes 2 args: `separator` (String), `input` (String).
/// Returns a Dict with integer keys `0..n` mapping to the split substrings.
/// Zero-copy: split parts share the original string's Rc<str> allocation.
/// Returns a Dict (not Seq) for O(1) indexed access — the dominant use case
/// is `[get 0 [split sep s]]`. A lazy Seq variant is possible but would make
/// indexed access O(n).
pub(crate) fn builtin_split(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("split", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // Both args pre-materialized by force_count=2
        let sep_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let input_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let sep = require_string("split", sep_val, args[0].span.clone())?;

        // Extract the source Rc<str> from the input string for zero-copy slicing.
        let (input_source, input_start, input_end) = match input_val {
            Value::String { source, start, end } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "split".to_string(),
                    "String",
                    input_val.type_name(),
                    args[1].span.clone(),
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
            let thunk = Arc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: input_start,
                    end: input_start,
                },
                call_span.clone(),
            ));
            map.insert(Key::Int(0), ctx.alloc_thunk(thunk));

            // Each character
            for (i, window) in char_boundaries.windows(2).enumerate() {
                let part_start = input_start + window[0];
                let part_end = input_start + window[1];
                let thunk = Arc::new(Thunk::new_materialized(
                    Value::String {
                        source: Rc::clone(&input_source),
                        start: part_start,
                        end: part_end,
                    },
                    call_span.clone(),
                ));
                map.insert(
                    Key::Int(i64::try_from(i + 1).map_err(|_| {
                        EvalError::resource_limit_exceeded(
                            "$split: result index too large".to_string(),
                            call_span.clone(),
                        )
                    })?),
                    ctx.alloc_thunk(thunk),
                );
            }

            // Last part: empty string at the end
            let last_idx = char_boundaries.len();
            let thunk = Arc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: input_end,
                    end: input_end,
                },
                call_span.clone(),
            ));
            map.insert(
                Key::Int(i64::try_from(last_idx).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "$split: result index too large".to_string(),
                        call_span.clone(),
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
                        call_span.clone(),
                    )
                    .into());
                }

                // Part before this separator
                let part_start = input_start + last_end;
                let part_end = input_start + match_start;
                let thunk = Arc::new(Thunk::new_materialized(
                    Value::String {
                        source: Rc::clone(&input_source),
                        start: part_start,
                        end: part_end,
                    },
                    call_span.clone(),
                ));
                map.insert(
                    Key::Int(i64::try_from(part_count).map_err(|_| {
                        EvalError::resource_limit_exceeded(
                            "$split: result index too large".to_string(),
                            call_span.clone(),
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
                    call_span.clone(),
                )
                .into());
            }
            let part_start = input_start + last_end;
            let part_end = input_end;
            let thunk = Arc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: part_start,
                    end: part_end,
                },
                call_span.clone(),
            ));
            map.insert(
                Key::Int(i64::try_from(part_count).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "$split: result index too large".to_string(),
                        call_span.clone(),
                    )
                })?),
                ctx.alloc_thunk(thunk),
            );
        }

        ok_val(Value::Dict(map), call_span)
    }) // end Box::pin(async move { builtin_split
}

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
/// Inherently materializing: must inspect string content to find and replace patterns.
pub(crate) fn builtin_replace(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;
        reject_named("replace", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        // All args pre-materialized by force_count=3
        let pattern_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let replacement_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let input_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let pattern = require_string("replace", pattern_val, args[0].span.clone())?;
        let replacement = require_string("replace", replacement_val, args[1].span.clone())?;
        let input = require_string("replace", input_val, args[2].span.clone())?;

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
                call_span.clone(),
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
    })
}

/// `trim`: Remove leading and trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the trimmed string.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("trim", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim", val, args[0].span.clone())?;
        ok_val(string_val(s.trim()), call_span)
    })
}

/// `str-length`: Return the length of a string in UTF-8 characters (not bytes).
///
/// Takes 1 arg (String). Returns an Int.
/// Inherently materializing: must inspect string content to count characters.
pub(crate) fn builtin_str_length(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("str-length", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("str-length", val, args[0].span.clone())?;
        let len = s.chars().count();
        let len_i64 = i64::try_from(len).map_err(|_| {
            EvalError::resource_limit_exceeded(
                "str-length: string length exceeds i64::MAX".to_string(),
                call_span.clone(),
            )
        })?;
        ok_val(Value::Int(len_i64), call_span)
    })
}

/// `str-slice`: Extract a substring by character indices [start, end).
///
/// Takes 3 args: `start` (Int), `end` (Int), `input` (String).
/// Returns a zero-copy slice of the input string.
/// Inherently materializing: must inspect string content to find character boundaries.
pub(crate) fn builtin_str_slice(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;
        reject_named("str-slice", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // Get pre-materialized arguments
        let start_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let end_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let input_val = args[2]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract the source Rc<str> from the input string
        let (input_source, input_start, input_end) = match input_val {
            Value::String { source, start, end } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "String",
                    input_val.type_name(),
                    args[2].span.clone(),
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
                    args[0].span.clone(),
                )
                .into());
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "Int",
                    start_val.type_name(),
                    args[0].span.clone(),
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
                    args[1].span.clone(),
                )
                .into());
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "Int",
                    end_val.type_name(),
                    args[1].span.clone(),
                )
                .into());
            }
        };

        // Validate indices
        if start_idx > end_idx {
            return Err(EvalError::internal(
                format!("str-slice: start index {start_idx} > end index {end_idx}"),
                call_span.clone(),
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
            return Err(EvalError::internal(
                format!(
                    "str-slice: end index {end_idx} out of bounds (string has {char_count} characters)"
                ),
                call_span.clone(),
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
    })
}

/// `str-chars`: Convert a string to a lazy sequence of single-character strings.
///
/// Takes 1 arg: `input` (String).
/// Returns a lazy Seq where each element is a zero-copy String slice of one Unicode codepoint.
/// Each slice shares the original Rc<str> source.
/// Inherently materializing: must inspect string content to identify character boundaries.
pub(crate) fn builtin_str_chars(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("str-chars", &args, named.as_ref(), &ctx, call_span.clone())?;

        let (input_source, input_start, input_end) = match val {
            Value::String { source, start, end } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-chars".to_string(),
                    "String",
                    val.type_name(),
                    args[0].span.clone(),
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
        let mut result = Arc::new(Thunk::new_materialized(
            Value::Dict(indexmap::IndexMap::new()),
            call_span.clone(),
        ));

        for (char_start, char_end) in char_ranges.into_iter().rev() {
            let head = Arc::new(Thunk::new_materialized(
                Value::String {
                    source: Rc::clone(&input_source),
                    start: input_start + char_start,
                    end: input_start + char_end,
                },
                call_span.clone(),
            ));

            result = Arc::new(Thunk::new_materialized(
                Value::Seq {
                    head: ctx.alloc_thunk(head),
                    tail: ctx.alloc_thunk(result),
                },
                call_span.clone(),
            ));
        }

        Ok(result)
    })
}

/// `char-code`: Get the Unicode codepoint of the first character in a string.
///
/// Takes 1 arg: `input` (String).
/// Returns an Int representing the Unicode codepoint of the first character.
/// Returns an error if the string is empty.
/// Inherently materializing: must inspect string content to extract the first character.
pub(crate) fn builtin_char_code(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("char-code", &args, named.as_ref(), &ctx, call_span.clone())?;

        let (input_source, input_start, input_end) = match val {
            Value::String { source, start, end } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "char-code".to_string(),
                    "String",
                    val.type_name(),
                    args[0].span.clone(),
                )
                .into());
            }
        };

        let input_str = &input_source[input_start..input_end];

        if let Some(ch) = input_str.chars().next() {
            ok_val(Value::Int(ch as u32 as i64), call_span)
        } else {
            Err(EvalError::internal("char-code: empty string".to_string(), call_span).into())
        }
    })
}

#[allow(clippy::items_after_test_module)]
// Additional builtin functions (chr, str-bytes, etc.) come after test module; moving them before would disrupt module organization
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Span;
    use crate::error::ErrorKind;
    use crate::eval::materialize_sync as materialize;
    use crate::test_util::test_span;
    use crate::value::Environment;
    use crate::value::{BuiltinArgs, Thunk, Value};
    use std::sync::{Arc, RwLock};

    fn str_thunk(s: &str, span: Span) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(crate::value::string_val(s), span))
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn no_named() -> Option<IndexMap<String, Arc<Thunk>>> {
        None
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let env = Arc::new(RwLock::new(Environment::new()));
        if let Some(defs) = crate::builtins::builtin_module("core") {
            for def in defs {
                let name = def.name.to_string();
                let thunk = Arc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
                env.write().unwrap().insert(name, thunk);
            }
        }
        crate::eval::EvalContext::new_empty(base_dir, env, false)
    }

    // --- MAX_SPLIT_PARTS guard ---

    /// The guard constant itself has the documented value.
    #[test]
    fn test_max_split_parts_constant() {
        assert_eq!(MAX_SPLIT_PARTS, 1_000_000);
    }

    fn call_builtin<F: std::future::Future>(f: F) -> F::Output {
        crate::async_rt::block_on_anywhere(f)
    }

    /// Splitting a string with a non-empty separator that would produce exactly 2 parts
    /// succeeds (well within the limit).
    #[test]
    fn test_split_two_parts_succeeds() {
        let span = call_span();
        let ctx = test_ctx();
        let result = call_builtin(builtin_split(BuiltinArgs {
            args: vec![str_thunk(",", span.clone()), str_thunk("a,b", span.clone())],
            named: no_named(),
            call_span: span,
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_ok(), "splitting 'a,b' by ',' should succeed");
        let val = materialize(&result.unwrap(), None, &ctx).unwrap();
        // Should produce a 2-entry dict {0: "a", 1: "b"}
        assert!(
            matches!(val, Value::Dict(_)),
            "split result should be a Dict"
        );
    }

    /// Empty separator on a very short string succeeds (char_count + 1 is small).
    #[test]
    fn test_split_empty_separator_short_string() {
        let span = call_span();
        let ctx = test_ctx();
        let result = call_builtin(builtin_split(BuiltinArgs {
            args: vec![str_thunk("", span.clone()), str_thunk("abc", span.clone())],
            named: no_named(),
            call_span: span,
            ctx: Arc::clone(&ctx),
        }));
        assert!(
            result.is_ok(),
            "splitting 'abc' by empty string should succeed: {:?}",
            result.err()
        );
    }

    // --- builtin_str: pure primitive (no Showable dispatch) ---

    /// `builtin_str` is a pure primitive: no typeclass dispatch.
    /// The result is the decimal representation of the integer.
    #[test]
    fn test_str_no_showable_instance_falls_through() {
        let span = call_span();
        let ctx = test_ctx();
        let int_thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));
        let result = call_builtin(builtin_str(BuiltinArgs {
            args: vec![int_thunk],
            named: no_named(),
            call_span: span,
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_ok(), "builtin_str(42) should succeed");
        let val = materialize(&result.unwrap(), None, &ctx).unwrap();
        assert!(
            matches!(&val, Value::String { source, start, end } if &source[*start..*end] == "42"),
            "expected string '42', got: {:?}",
            val
        );
    }

    /// `builtin_str` with zero args returns an empty string.
    #[test]
    fn test_str_zero_args_returns_empty_string() {
        let span = call_span();
        let ctx = test_ctx();
        let result = call_builtin(builtin_str(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: span,
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_ok(), "builtin_str() with 0 args should succeed");
        let val = materialize(&result.unwrap(), None, &ctx).unwrap();
        assert!(
            matches!(&val, Value::String { source, start, end } if source[*start..*end].is_empty()),
            "expected empty string, got: {:?}",
            val
        );
    }

    /// Arity error: `builtin_split` with 1 arg returns an arity mismatch error.
    #[test]
    fn test_split_arity_error() {
        let span = call_span();
        let ctx = test_ctx();
        let result = call_builtin(builtin_split(BuiltinArgs {
            args: vec![str_thunk(",", span.clone())],
            named: no_named(),
            call_span: span,
            ctx,
        }));
        assert!(result.is_err(), "split with 1 arg should return an error");
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ArityMismatch { .. }),
            "expected ArityMismatch, got: {:?}",
            err.kind
        );
    }
}

/// `chr`: Convert a Unicode codepoint to a single-character string.
///
/// Takes 1 arg: `codepoint` (Int).
/// Returns a String containing the character corresponding to the codepoint.
/// Returns an error if the codepoint is invalid.
/// Inherently materializing: must convert the integer to a character.
pub(crate) fn builtin_chr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("chr", &args, named.as_ref(), &ctx, call_span.clone())?;

        match val {
            Value::Int(n) => {
                if let Some(ch) = char::from_u32(n as u32) {
                    ok_val(string_val(&ch.to_string()), call_span)
                } else {
                    Err(EvalError::internal(
                        format!("chr: invalid Unicode codepoint {}", n),
                        call_span,
                    )
                    .into())
                }
            }
            _ => Err(EvalError::type_mismatch_ctx(
                "chr".to_string(),
                "Int",
                val.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `str-bytes`: Convert a string to Bytes (UTF-8 encoding).
///
/// Takes 1 arg: a String.
/// Returns Bytes containing the UTF-8 encoding of the string.
///
/// # Example
///
/// ```llt
/// (str-bytes "Hello")  // Bytes of UTF-8 encoding
/// ```
pub(crate) fn builtin_str_bytes(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        use crate::value::bytes_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "str-bytes",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        match val.as_str() {
            Some(s) => ok_val(bytes_val(s.as_bytes()), call_span),
            None => Err(EvalError::type_mismatch_ctx(
                "str-bytes".to_string(),
                "String",
                val.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `str-index-of`: Find the byte index of the first occurrence of needle in haystack.
///
/// Takes 2 args: `haystack` (String), `needle` (String).
/// Returns the byte index of the first occurrence as an Int, or -1 if not found.
/// Note: returns a *byte* index (not a character index). For ASCII strings, byte
/// index equals character index. The stdlib `str-find` delegates to this builtin.
/// This primitive uses Rust's O(n) `str::find` (two-way algorithm), replacing the
/// O(n²) recursive `str-find-impl` that was previously in prelude.
/// Inherently materializing: must inspect string content to search for substring.
pub(crate) fn builtin_str_index_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;
        reject_named("str-index-of", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let haystack_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let needle_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let haystack = require_string("str-index-of", haystack_val, args[0].span.clone())?;
        let needle = require_string("str-index-of", needle_val, args[1].span.clone())?;

        let index: i64 = match haystack.find(needle.as_str()) {
            Some(byte_idx) => i64::try_from(byte_idx).map_err(|_| {
                EvalError::resource_limit_exceeded(
                    "str-index-of: byte index exceeds i64::MAX".to_string(),
                    call_span.clone(),
                )
            })?,
            None => -1,
        };

        ok_val(Value::Int(index), call_span)
    })
}

/// `trim-start`: Remove leading whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with leading whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_start(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("trim-start", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim-start", val, args[0].span.clone())?;
        ok_val(string_val(s.trim_start()), call_span)
    })
}

/// `trim-end`: Remove trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with trailing whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_end(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg("trim-end", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim-end", val, args[0].span.clone())?;
        ok_val(string_val(s.trim_end()), call_span)
    })
}

/// `bytes-str`: Convert Bytes to a string (UTF-8 decoding).
///
/// Takes 1 arg: Bytes.
/// Returns String if valid UTF-8, otherwise returns an error.
///
/// # Example
///
/// ```llt
/// (bytes-str some-bytes)  // String if valid UTF-8
/// ```
pub(crate) fn builtin_bytes_str(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        use crate::value::string_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let val = crate::builtins::expect_one_arg(
            "bytes-str",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        match val.as_bytes() {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => ok_val(string_val(s), call_span),
                Err(e) => Err(EvalError::internal(
                    format!("bytes-str: invalid UTF-8 sequence: {}", e),
                    call_span,
                )
                .into()),
            },
            None => Err(EvalError::type_mismatch_ctx(
                "bytes-str".to_string(),
                "Bytes",
                val.type_name(),
                args[0].span.clone(),
            )
            .into()),
        }
    })
}

/// `str-to-upper-char`: Convert a single character (as a Str) to uppercase.
///
/// Takes 1 arg: `c` (String, expected to be a single Unicode character).
/// Returns a String containing the uppercase version of the character.
/// For multi-char or empty strings, applies to_uppercase() to the whole input.
/// This is the primitive used by the stdlib `upper` function via `str-map-chars`.
/// Inherently materializing: must inspect string content to convert case.
pub(crate) fn builtin_str_to_upper_char(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg(
            "str-to-upper-char",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("str-to-upper-char", val, args[0].span.clone())?;
        ok_val(string_val(&s.to_uppercase()), call_span)
    })
}

/// `str-to-lower-char`: Convert a single character (as a Str) to lowercase.
///
/// Takes 1 arg: `c` (String, expected to be a single Unicode character).
/// Returns a String containing the lowercase version of the character.
/// For multi-char or empty strings, applies to_lowercase() to the whole input.
/// This is the primitive used by the stdlib `lower` function via `str-map-chars`.
/// Inherently materializing: must inspect string content to convert case.
pub(crate) fn builtin_str_to_lower_char(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = expect_one_arg(
            "str-to-lower-char",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("str-to-lower-char", val, args[0].span.clone())?;
        ok_val(string_val(&s.to_lowercase()), call_span)
    })
}

/// `str-map-chars`: Map a tinct function over every Unicode character in a string.
///
/// Takes 2 args: `f` (Function: Str → Str), `s` (String).
/// Applies `f` to each Unicode character (as a single-char string) in `s`,
/// then concatenates all results into a new string.
/// The output of `f` need not be a single character — it can be any string.
/// Inherently materializing: must iterate characters and force each function call.
pub(crate) fn builtin_str_map_chars(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("str-map-chars", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Get pre-materialized function (arg 0) and the input string (arg 1).
        let func_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let input_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let s = require_string("str-map-chars", input_val, args[1].span.clone())?;

        // For an empty string, return empty string immediately.
        if s.is_empty() {
            return ok_val(string_val(""), call_span);
        }

        // Build the result by calling f on each character.
        // Note: func_val is Value (contains Rc) so we use sync materialize/invoke here
        // to avoid holding a !Send Value across .await points. The outer Box::pin(async
        // move { ... }) wrapper satisfies the BuiltinFn signature; the actual work is
        // synchronous via block_on_anywhere.
        let materialize_s = crate::eval::materialize_sync;
        let invoke_s = crate::eval_call::invoke_function_sync;
        let mut result = String::with_capacity(s.len());

        for ch in s.chars() {
            let ch_str = ch.to_string();
            // Wrap each char as a materialized thunk.
            let char_thunk = Arc::new(Thunk::new_materialized(
                string_val(&ch_str),
                call_span.clone(),
            ));

            // Call f(char_thunk) — dispatch on Value::Function vs Value::Builtin.
            let call_result_thunk = match &func_val {
                Value::Function {
                    params,
                    body,
                    env: closure_env,
                    ..
                } => {
                    let pos_args = vec![Arc::clone(&char_thunk)];
                    invoke_s(&CallContext {
                        params,
                        body,
                        closure_env,
                        positional: &pos_args,
                        named: None,
                        default_env: closure_env,
                        call_span: call_span.clone(),
                        origin: Some(Arc::from("str-map-chars")),
                        ctx: &ctx,
                    })?
                }
                Value::Builtin(def) => {
                    let builtin_args = BuiltinArgs {
                        args: vec![Arc::clone(&char_thunk)],
                        named: None,
                        call_span: call_span.clone(),
                        ctx: Arc::clone(&ctx),
                    };
                    crate::async_rt::block_on_anywhere((def.func)(builtin_args))?
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "str-map-chars".to_string(),
                        "Function",
                        other.type_name(),
                        args[0].span.clone(),
                    )
                    .into());
                }
            };

            // Materialize the result and require it to be a String.
            let mapped_val = materialize_s(&call_result_thunk, Some(&call_span), &ctx)?;
            let mapped_str = require_string("str-map-chars", mapped_val, call_span.clone())?;

            // Guard against excessive output size.
            if result.len() + mapped_str.len() > MAX_STRING_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "str-map-chars: output would exceed {} MB limit",
                        MAX_STRING_SIZE / (1024 * 1024)
                    ),
                    call_span.clone(),
                )
                .into());
            }

            result.push_str(&mapped_str);
        }

        ok_val(string_val(&result), call_span)
    })
}

/// `regex-match?`: Test if a regex pattern matches anywhere in a haystack string.
///
/// Takes 2 args: `pattern` (String), `haystack` (String).
/// Returns `true` if the regex matches anywhere in `haystack`, `false` otherwise.
/// Returns an error if `pattern` is not a valid regex.
/// Uses the `regex` crate (RE2-compatible, no backtracking).
/// Inherently materializing: must inspect string content to apply the regex.
pub(crate) fn builtin_regex_match(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;
        reject_named("regex-match?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let pattern_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let haystack_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let pattern = require_string("regex-match?", pattern_val, args[0].span.clone())?;
        let haystack = require_string("regex-match?", haystack_val, args[1].span.clone())?;

        match regex::Regex::new(&pattern) {
            Ok(re) => ok_val(Value::Bool(re.is_match(&haystack)), call_span),
            Err(e) => Err(EvalError::internal(
                format!("regex-match?: invalid regex pattern {:?}: {}", pattern, e),
                call_span,
            )
            .into()),
        }
    })
}
