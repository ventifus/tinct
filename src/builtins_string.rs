//! String builtins: `str`, `replace`, `trim`, `str-length`,
//! `str-slice`, `str-chars`, `str-index-of`, `char-code`, `chr`, `str-bytes`, `bytes-str`,
//! `trim-start`, `trim-end`, `str-to-upper-char`, `str-to-lower-char`, `str-map-chars`,
//! `regex-match?`, `int->string`, `float->string`, `builtin-string-concat`.
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
use std::sync::Arc;

use crate::builtins::{expect_one_arg, ok_val, reject_named, require_string};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{string_val, BuiltinArgs, Thunk, Value};

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
/// Inherently materializing: must inspect string content to find and replace patterns.
pub(crate) fn builtin_replace(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("replace", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        // All args pre-materialized by force_count=3
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let thunk2 = Arc::clone(&args[2]);
        let pattern_val = thunk0.require_value()?.clone();
        let replacement_val = thunk1.require_value()?.clone();
        let input_val = thunk2.require_value()?.clone();

        let pattern = require_string("replace", pattern_val, thunk0.span.clone())?;
        let replacement = require_string("replace", replacement_val, thunk1.span.clone())?;
        let input = require_string("replace", input_val, thunk2.span.clone())?;

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

        const MAX_REPLACE_OUTPUT: usize = 10 * 1024 * 1024; // 10MB
        if output_len > MAX_REPLACE_OUTPUT {
            return Err(Box::new(EvalError::resource_limit_exceeded(
                format!(
                    "replace: output would exceed {} bytes (estimated {})",
                    MAX_REPLACE_OUTPUT, output_len
                ),
                call_span,
            )));
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("trim", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim", val, Arc::clone(&args[0]).span.clone())?;
        ok_val(string_val(s.trim()), call_span)
    })
}

/// `str-length`: Return the length of a string in UTF-8 characters (not bytes).
///
/// Takes 1 arg (String). Returns an Int.
/// Inherently materializing: must inspect string content to count characters.
pub(crate) fn builtin_str_length(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("str-length", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("str-length", val, Arc::clone(&args[0]).span.clone())?;
        let len = s.chars().count();
        let len_i64 = i64::try_from(len).map_err(|_| {
            EvalError::resource_limit_exceeded(
                "str-length: string length exceeds i64::MAX".to_string(),
                call_span.clone(),
            )
        })?;
        ok_val(
            Value::Int {
                n: len_i64,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-str-byte-count`: Number of UTF-8 bytes in a String.
///
/// O(1) — `str.len()` is stored directly. Takes 1 arg (String). Returns an Int.
pub(crate) fn builtin_str_byte_count(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg(
            "str-byte-count",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("str-byte-count", val, Arc::clone(&args[0]).span.clone())?;
        ok_val(
            Value::Int {
                n: s.len() as i64,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-str-has-nth-byte?`: Check whether UTF-8 byte index `i` is valid.
///
/// O(1) — bounds check only. Takes 2 args (String, Int). Returns Int 1 or 0.
pub(crate) fn builtin_str_has_nth_byte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named(
            "builtin-str-has-nth-byte?",
            named.as_ref(),
            call_span.clone(),
        )?;
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = thunk0.require_value()?.clone();
        let (str_start, str_end) = match s_val {
            Value::String { start, end, .. } => (start, end),
            other => {
                return Err(EvalError::type_mismatch("String", other.type_name(), call_span).into())
            }
        };
        let idx = match thunk1.require_value()?.clone() {
            Value::Int { n, .. } => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        let len = (str_end - str_start) as i64;
        ok_val(
            Value::Int {
                n: if idx >= 0 && idx < len { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-str-nth-byte`: Get the UTF-8 byte at index `i` as an Int (0–255).
///
/// O(1) — direct byte array indexing. Takes 2 args (String, Int). Returns Int.
pub(crate) fn builtin_str_nth_byte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-str-nth-byte", named.as_ref(), call_span.clone())?;
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = thunk0.require_value()?.clone();
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return Err(EvalError::type_mismatch("String", other.type_name(), call_span).into())
            }
        };
        let idx = match thunk1.require_value()?.clone() {
            Value::Int { n, .. } => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };
        if idx < 0 || idx as usize >= str_end - str_start {
            return Err(EvalError::user_error(
                format!("builtin-str-nth-byte: index {idx} out of bounds"),
                call_span,
            )
            .into());
        }
        let byte = source.as_bytes()[str_start + idx as usize] as i64;
        ok_val(
            Value::Int {
                n: byte,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `str-slice`: Extract a substring by character indices [start, end).
///
/// Takes 3 args: `input` (String), `start` (Int), `end` (Int).
/// Returns a zero-copy slice of the input string.
/// Inherently materializing: must inspect string content to find character boundaries.
pub(crate) fn builtin_str_slice(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("str-slice", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        // Get pre-materialized arguments — calling convention: (string, start, end)
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let thunk2 = Arc::clone(&args[2]);
        let input_val = thunk0.require_value()?.clone();
        let start_val = thunk1.require_value()?.clone();
        let end_val = thunk2.require_value()?.clone();

        // Extract the source Arc<str> from the input string
        let (input_source, input_start, input_end) = match input_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "String",
                    input_val.type_name(),
                    thunk0.span.clone(),
                )
                .into());
            }
        };

        // Extract start and end indices
        let start_idx = match start_val {
            Value::Int { n, .. } if n >= 0 => n as usize,
            Value::Int { n, .. } => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "non-negative Int",
                    &format!("Int({n})"),
                    thunk1.span.clone(),
                )
                .into());
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "Int",
                    start_val.type_name(),
                    thunk1.span.clone(),
                )
                .into());
            }
        };

        let end_idx = match end_val {
            Value::Int { n, .. } if n >= 0 => n as usize,
            Value::Int { n, .. } => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "non-negative Int",
                    &format!("Int({n})"),
                    thunk2.span.clone(),
                )
                .into());
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "str-slice".to_string(),
                    "Int",
                    end_val.type_name(),
                    thunk2.span.clone(),
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
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-str-has-nth?`: Check whether Unicode codepoint index `i` is valid in a string.
///
/// Takes 2 args: (s: String, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// O(n) to find the position (Unicode requires sequential scan).
/// Prelude str-chars-step guards with this before calling `builtin-str-nth-char`.
/// `String → Int → Int`
pub(crate) fn builtin_str_has_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-str-has-nth?", named.as_ref(), call_span.clone())?;

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = thunk0.require_value()?.clone();
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return Err(EvalError::type_mismatch("String", other.type_name(), call_span).into())
            }
        };

        let idx = match thunk1.require_value()?.clone() {
            Value::Int { n, .. } => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };

        if idx < 0 {
            return ok_val(
                Value::Int {
                    n: 0,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            );
        }

        let s = &source[str_start..str_end];
        let exists = if s.char_indices().nth(idx as usize).is_some() {
            1i64
        } else {
            0i64
        };
        ok_val(
            Value::Int {
                n: exists,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-str-nth-char`: Get the character at Unicode codepoint index `i` in a string.
///
/// Takes 2 args: (s: String, i: Int). Returns the character at that position as a
/// zero-copy String slice. Errors if `i` is out of bounds.
/// O(n) to find the position (Unicode requires sequential scan), but drives laziness
/// from the tinct side — prelude `str-chars-step` guards with `builtin-str-has-nth?` first.
/// `String → Int → String`
pub(crate) fn builtin_str_nth_char(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        reject_named("builtin-str-nth-char", named.as_ref(), call_span.clone())?;

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = thunk0.require_value()?.clone();
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return Err(EvalError::type_mismatch("String", other.type_name(), call_span).into())
            }
        };

        let idx = match thunk1.require_value()?.clone() {
            Value::Int { n, .. } => n,
            other => {
                return Err(EvalError::type_mismatch("Int", other.type_name(), call_span).into())
            }
        };

        if idx < 0 {
            return Err(EvalError::user_error(
                format!("builtin-str-nth-char: index {idx} is negative"),
                call_span,
            )
            .into());
        }

        let s = &source[str_start..str_end];
        match s.char_indices().nth(idx as usize) {
            Some((byte_start, ch)) => {
                let byte_end = byte_start + ch.len_utf8();
                ok_val(
                    Value::String {
                        source,
                        start: str_start + byte_start,
                        end: str_start + byte_end,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
            }
            None => Err(EvalError::user_error(
                format!(
                    "builtin-str-nth-char: index {idx} out of bounds (string has {} codepoints)",
                    s.chars().count()
                ),
                call_span,
            )
            .into()),
        }
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("char-code", &args, named.as_ref(), &ctx, call_span.clone())?;

        let (input_source, input_start, input_end) = match val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "char-code".to_string(),
                    "String",
                    val.type_name(),
                    Arc::clone(&args[0]).span.clone(),
                )
                .into());
            }
        };

        let input_str = &input_source[input_start..input_end];

        if let Some(ch) = input_str.chars().next() {
            ok_val(
                Value::Int {
                    n: ch as u32 as i64,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )
        } else {
            Err(EvalError::internal("char-code: empty string".to_string(), call_span).into())
        }
    })
}

/// `chr`: Convert a Unicode codepoint to a single-character string.
///
/// Takes 1 arg: `codepoint` (Int).
/// Returns a String containing the character corresponding to the codepoint.
/// Returns an error if the codepoint is invalid.
/// Inherently materializing: must convert the integer to a character.
pub(crate) fn builtin_chr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("chr", &args, named.as_ref(), &ctx, call_span.clone())?;

        match val {
            Value::Int { n, .. } => {
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
                Arc::clone(&args[0]).span.clone(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        use crate::value::bytes_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
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
                Arc::clone(&args[0]).span.clone(),
            )
            .into()),
        }
    })
}

/// `str-index-of`: Find the character index of the first occurrence of needle in haystack.
///
/// Takes 2 args: `needle` (String), `haystack` (String) — subject-last for pipeline use.
/// Returns the character index of the first occurrence as an Int, or -1 if not found.
/// Returns a *character* index (not a byte index), consistent with `str-length` (char count)
/// and `str-slice` (char-based slicing). For ASCII strings, char index equals byte index.
/// The stdlib `str-find` delegates to this builtin.
/// This primitive uses Rust's O(n) `str::find` for the byte offset, then counts chars before
/// the match to convert to a character index.
/// Inherently materializing: must inspect string content to search for substring.
pub(crate) fn builtin_str_index_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("str-index-of", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let needle_val = thunk0.require_value()?.clone();
        let haystack_val = thunk1.require_value()?.clone();

        let needle = require_string("str-index-of", needle_val, thunk0.span.clone())?;
        let haystack = require_string("str-index-of", haystack_val, thunk1.span.clone())?;

        let index: i64 = match haystack.find(needle.as_str()) {
            Some(byte_idx) => {
                // Convert the byte offset to a character index so that all string builtins
                // use the same unit (chars). str-length returns chars, str-slice operates on
                // chars — str-index-of must return chars to be composable with both.
                let char_idx = haystack[..byte_idx].chars().count();
                i64::try_from(char_idx).map_err(|_| {
                    EvalError::resource_limit_exceeded(
                        "str-index-of: character index exceeds i64::MAX".to_string(),
                        call_span.clone(),
                    )
                })?
            }
            None => -1,
        };

        ok_val(
            Value::Int {
                n: index,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `trim-start`: Remove leading whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with leading whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_start(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("trim-start", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim-start", val, Arc::clone(&args[0]).span.clone())?;
        ok_val(string_val(s.trim_start()), call_span)
    })
}

/// `trim-end`: Remove trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with trailing whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_end(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg("trim-end", &args, named.as_ref(), &ctx, call_span.clone())?;
        let s = require_string("trim-end", val, Arc::clone(&args[0]).span.clone())?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        use crate::value::string_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
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
                Arc::clone(&args[0]).span.clone(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg(
            "str-to-upper-char",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("str-to-upper-char", val, Arc::clone(&args[0]).span.clone())?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = expect_one_arg(
            "str-to-lower-char",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("str-to-lower-char", val, Arc::clone(&args[0]).span.clone())?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("str-map-chars", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Get pre-materialized function (arg 0) and the input string (arg 1).
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let func_val = thunk0.require_value()?.clone();
        let input_val = thunk1.require_value()?.clone();

        let s = require_string("str-map-chars", input_val, thunk1.span.clone())?;

        // For an empty string, return empty string immediately.
        if s.is_empty() {
            return ok_val(string_val(""), call_span);
        }

        // Build the result by calling f on each character.
        let mut result = String::with_capacity(s.len());

        for ch in s.chars() {
            let ch_str = ch.to_string();
            // Wrap each char as a materialized thunk, then register it in the arena.
            let char_thunk = Arc::new(Thunk::value(string_val(&ch_str), call_span.clone()));

            // Call f(char_thunk) — dispatch on Value::Function vs Value::Builtin vs EffectPerformDispatcher.
            let call_result_thunk = match &func_val {
                Value::Function {
                    clauses,
                    closure_env,
                    ..
                } => {
                    let pos_args = vec![Arc::clone(&char_thunk)];
                    invoke_function(&CallContext {
                        clauses,
                        closure_env: Arc::clone(closure_env),
                        positional: &pos_args,
                        named: None,
                        call_span: call_span.clone().with_name(Arc::from("str-map-chars")),
                        ctx: &ctx,
                    })
                    .await?
                }
                Value::Builtin { def, .. } => {
                    let builtin_args = BuiltinArgs {
                        args: vec![Arc::clone(&char_thunk)],
                        named: None,
                        call_span: call_span.clone(),
                        caller_env_id: None,
                        ctx: Arc::clone(&ctx),
                    };
                    (def.func)(builtin_args).await?
                }
                Value::EffectPerformDispatcher { candidates, .. } => {
                    // Try each candidate in order until one matches.
                    let pos_args = vec![Arc::clone(&char_thunk)];
                    let mut last_error: Option<Box<crate::error::EvalError>> = None;
                    let mut matched = false;
                    let mut result_thunk = None;
                    for candidate_value in candidates.iter() {
                        if let Value::Function {
                            clauses,
                            closure_env,
                            ..
                        } = candidate_value.as_ref()
                        {
                            let call_ctx = CallContext {
                                clauses,
                                closure_env: Arc::clone(closure_env),
                                positional: &pos_args,
                                named: None,
                                call_span: call_span.clone().with_name(Arc::from("str-map-chars")),
                                ctx: &ctx,
                            };
                            match invoke_function(&call_ctx).await {
                                Ok(thunk) => {
                                    result_thunk = Some(thunk);
                                    matched = true;
                                    break;
                                }
                                Err(e) => {
                                    last_error = Some(e);
                                }
                            }
                        }
                    }
                    if matched {
                        result_thunk.expect("matched but no result_thunk")
                    } else if let Some(e) = last_error {
                        return Err(e);
                    } else {
                        return Err(EvalError::user_error(
                            "no matching instance for EffectPerform dispatch in str-map-chars"
                                .to_string(),
                            call_span.clone(),
                        )
                        .into());
                    }
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "str-map-chars".to_string(),
                        "Function",
                        other.type_name(),
                        thunk0.span.clone(),
                    )
                    .into());
                }
            };

            // Materialize the result and require it to be a String.
            let mapped_val = materialize(&call_result_thunk, Some(&call_span), &ctx).await?;
            let mapped_str = require_string("str-map-chars", mapped_val, call_span.clone())?;

            result.push_str(&mapped_str);
        }

        ok_val(string_val(&result), call_span)
    })
}

/// `int->string`: Convert an Int to its decimal string representation.
///
/// Takes exactly 1 arg: an `Int`.
/// Returns the decimal representation as a String (same as `[str n]` for integers).
/// This is the thin primitive backing `Printable` instances in the stdlib — it does
/// not call `stringify` and cannot infinitely recurse back through `str`.
pub(crate) fn builtin_int_to_string(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("int->string", named.as_ref(), call_span.clone())?;
        let thunk0 = args
            .into_iter()
            .next()
            .ok_or_else(|| EvalError::arity_mismatch(1, 0, call_span.clone()))?;
        let materialized = thunk0.require_value()?.clone();
        match materialized {
            Value::Int { n, .. } => ok_val(string_val(&n.to_string()), call_span),
            other => Err(EvalError::type_mismatch_ctx(
                "int->string".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `float->string`: Convert a Float to its string representation.
///
/// Takes exactly 1 arg: a `Float`.
/// Returns the Rust Display representation of the float as a String.
/// This is the thin primitive backing `Printable` instances in the stdlib — it does
/// not call `stringify` and cannot infinitely recurse back through `str`.
pub(crate) fn builtin_float_to_string(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("float->string", named.as_ref(), call_span.clone())?;
        let thunk0 = args
            .into_iter()
            .next()
            .ok_or_else(|| EvalError::arity_mismatch(1, 0, call_span.clone()))?;
        let materialized = thunk0.require_value()?.clone();
        match materialized {
            Value::Float { n, .. } => ok_val(string_val(&n.to_string()), call_span),
            other => Err(EvalError::type_mismatch_ctx(
                "float->string".to_string(),
                "Float",
                other.type_name(),
                call_span,
            )
            .into()),
        }
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("regex-match?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let pattern_val = thunk0.require_value()?.clone();
        let haystack_val = thunk1.require_value()?.clone();

        let pattern = require_string("regex-match?", pattern_val, thunk0.span.clone())?;
        let haystack = require_string("regex-match?", haystack_val, thunk1.span.clone())?;

        match regex::Regex::new(&pattern) {
            Ok(re) => ok_val(
                Value::Int {
                    n: if re.is_match(&haystack) { 1 } else { 0 },
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            ),
            Err(e) => Err(EvalError::internal(
                format!("regex-match?: invalid regex pattern {:?}: {}", pattern, e),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-string-concat`: Concatenate exactly two strings.
///
/// Takes 2 args: `s1` (String), `s2` (String).
/// Returns a new String that is the concatenation of `s1` and `s2`.
/// This is a primitive string operation that doesn't go through `str` (avoiding
/// circular recursion when str is eventually reimplemented via Printable).
/// Inherently materializing: must inspect string content to concatenate.
pub(crate) fn builtin_string_concat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        reject_named("builtin-string-concat", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s1_val = thunk0.require_value()?.clone();
        let s2_val = thunk1.require_value()?.clone();

        let s1 = require_string("builtin-string-concat", s1_val, thunk0.span.clone())?;
        let s2 = require_string("builtin-string-concat", s2_val, thunk1.span.clone())?;

        let result = format!("{}{}", s1, s2);
        ok_val(string_val(&result), call_span)
    })
}

/// Returns all "string" module Rust builtins.
///
/// These are the string transformation and bytes builtins that are NOT in the Core-46 set.
/// The Core-46 items (builtin-string-concat, builtin-str-bytes, builtin-str-length,
/// builtin-str-slice, builtin-str-index-of, builtin-int->string, builtin-bytes,
/// builtin-bytes-concat, builtin-bytes-str) stay in core_builtins() for loader.llt.
///
/// Consumed exclusively by `builtin_module("string")` in `src/builtins.rs`.
pub fn string_builtins() -> Vec<crate::value::BuiltinDef> {
    use crate::builtins::builtin;
    use crate::builtins_bytes::{
        builtin_bytes_equal, builtin_bytes_find, builtin_bytes_get, builtin_bytes_of,
        builtin_bytes_slice, builtin_bytes_to_int, builtin_ct_equal, builtin_encode,
    };
    use crate::value::Strictness;
    vec![
        // ── String conversion (non-Core-46) ───────────────────────────────────────────
        builtin!(
            "builtin-float->string",
            builtin_float_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        builtin!(
            // Hyphenated alias for builtin-float->string (used by type-foundations loader).
            "builtin-float-to-string",
            builtin_float_to_string,
            [Strictness::Seq],
            1,
            ["n"]
        ),
        // ── String inspection (non-Core-46) ───────────────────────────────────────────
        builtin!(
            "builtin-str-byte-count",
            builtin_str_byte_count,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-has-nth-byte?",
            builtin_str_has_nth_byte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "i"]
        ),
        builtin!(
            "builtin-str-nth-byte",
            builtin_str_nth_byte,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "i"]
        ),
        builtin!(
            "builtin-str-has-nth?",
            builtin_str_has_nth,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "n"]
        ),
        builtin!(
            "builtin-str-nth-char",
            builtin_str_nth_char,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["str", "n"]
        ),
        // ── String transformation (non-Core-46) ───────────────────────────────────────
        builtin!(
            "builtin-replace",
            builtin_replace,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["pattern", "replacement", "str"]
        ),
        builtin!("builtin-trim", builtin_trim, [Strictness::Seq], 1, ["str"]),
        builtin!(
            "builtin-trim-start",
            builtin_trim_start,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-trim-end",
            builtin_trim_end,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-to-upper-char",
            builtin_str_to_upper_char,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-to-lower-char",
            builtin_str_to_lower_char,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-map-chars",
            builtin_str_map_chars,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["f", "str"]
        ),
        builtin!(
            "builtin-regex-match?",
            builtin_regex_match,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["pattern", "str"]
        ),
        // ── Character operations ───────────────────────────────────────────────────────
        builtin!(
            "builtin-char-code",
            builtin_char_code,
            [Strictness::Seq],
            1,
            ["char"]
        ),
        builtin!("builtin-chr", builtin_chr, [Strictness::Seq], 1, ["n"]),
        // ── Bytes operations (non-Core-46) ────────────────────────────────────────────
        builtin!(
            "builtin-bytes-find",
            builtin_bytes_find,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["needle", "bytes"]
        ),
        builtin!("builtin-bytes-of", builtin_bytes_of, [Strictness::Seq]),
        builtin!(
            "builtin-bytes-equal?",
            builtin_bytes_equal,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-ct-equal?",
            builtin_ct_equal,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["a", "b"]
        ),
        builtin!(
            "builtin-encode",
            builtin_encode,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["encoding", "bytes"]
        ),
        builtin!(
            "builtin-bytes-get",
            builtin_bytes_get,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["bytes", "n"]
        ),
        builtin!(
            "builtin-bytes-slice",
            builtin_bytes_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq],
            3,
            ["bytes", "start", "end"]
        ),
        builtin!(
            "builtin-bytes-to-int",
            builtin_bytes_to_int,
            [Strictness::Seq],
            1,
            ["b"]
        ),
    ]
}
