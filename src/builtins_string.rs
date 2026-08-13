//! String builtins: `str`, `replace`, `trim`, `str-length`,
//! `str-slice`, `str-chars`, `str-index-of`, `char-code`, `chr`, `str-bytes`, `bytes-str`,
//! `trim-start`, `trim-end`, `upper`, `lower`,
//! `regex-match?`, `int->string`, `float->string`, `builtin-string-concat`.
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
use crate::error::Diagnostic;
use crate::value::{string_val, BuiltinArgs, Thunk, Value};

const KIND: &str = "eval";

/// `replace`: Replace all occurrences of a pattern in a string.
///
/// Takes 3 args: `pattern` (String), `replacement` (String), `input` (String).
/// Returns a new String with all occurrences of `pattern` replaced by `replacement`.
/// Inherently materializing: must inspect string content to find and replace patterns.
pub(crate) fn builtin_replace(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("replace", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 3 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(3),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        // All args pre-materialized by force_count=3
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let thunk2 = Arc::clone(&args[2]);
        let pattern_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let replacement_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let input_val = {
            let (result, diags) = thunk2.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let pattern = {
            let (result, diags) = require_string("replace", pattern_val, thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let replacement = {
            let (result, diags) = require_string("replace", replacement_val, thunk1.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let input = {
            let (result, diags) = require_string("replace", input_val, thunk2.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

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
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "replace: output would exceed {} bytes (estimated {})",
                    MAX_REPLACE_OUTPUT, output_len
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Fast-path: if there are no matches, return the input unchanged
        if match_count == 0 {
            return ok_val(string_val(&input), call_span).with_diagnostics(_dr_diags);
        }

        ok_val(
            string_val(&input.replace(&pattern, &replacement)),
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `trim`: Remove leading and trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the trimmed string.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("trim", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("trim", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(s.trim()), call_span).with_diagnostics(_dr_diags)
    })
}

/// `str-length`: Return the length of a string in UTF-8 characters (not bytes).
///
/// Takes 1 arg (String). Returns an Int.
/// Inherently materializing: must inspect string content to count characters.
pub(crate) fn builtin_str_length(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("str-length", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("str-length", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let len = s.chars().count();
        let len_i64 = match i64::try_from(len) {
            Ok(v) => v,
            Err(_) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "str-length: string length exceeds i64::MAX".to_string(),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
        };
        ok_val(
            Value::Int {
                n: len_i64,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-str-byte-count`: Number of UTF-8 bytes in a String.
///
/// O(1) — `str.len()` is stored directly. Takes 1 arg (String). Returns an Int.
pub(crate) fn builtin_str_byte_count(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg(
                "str-byte-count",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string(
                "str-byte-count",
                val,
                Arc::clone(&args[0]).span.clone(),
                &ctx,
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(
            Value::Int {
                n: s.len() as i64,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-str-has-nth-byte?`: Check whether UTF-8 byte index `i` is valid.
///
/// O(1) — bounds check only. Takes 2 args (String, Int). Returns Int 1 or 0.
pub(crate) fn builtin_str_has_nth_byte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        {
            let (result, diags) = reject_named(
                "builtin-str-has-nth-byte?",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (str_start, str_end) = match s_val {
            Value::String { start, end, .. } => (start, end),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "String",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };
        let idx = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => match v.clone() {
                    Value::Int { n, .. } => n,
                    other => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "type mismatch: expected {}, got {}",
                                "Int",
                                ctx.type_display_name(other.type_decl_id_of())
                            ),
                            call_span,
                        ))
                        .with_diagnostics(_dr_diags)
                    }
                },
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let len = (str_end - str_start) as i64;
        ok_val(
            Value::Int {
                n: if idx >= 0 && idx < len { 1 } else { 0 },
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-str-nth-byte`: Get the UTF-8 byte at index `i` as an Int (0–255).
///
/// O(1) — direct byte array indexing. Takes 2 args (String, Int). Returns Int.
pub(crate) fn builtin_str_nth_byte(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        {
            let (result, diags) = reject_named("builtin-str-nth-byte", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "String",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };
        let idx = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => match v.clone() {
                    Value::Int { n, .. } => n,
                    other => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "type mismatch: expected {}, got {}",
                                "Int",
                                ctx.type_display_name(other.type_decl_id_of())
                            ),
                            call_span,
                        ))
                        .with_diagnostics(_dr_diags)
                    }
                },
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        if idx < 0 || idx as usize >= str_end - str_start {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("builtin-str-nth-byte: index {idx} out of bounds"),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        let byte = source.as_bytes()[str_start + idx as usize] as i64;
        ok_val(
            Value::Int {
                n: byte,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `str-slice`: Extract a substring by character indices [start, end).
///
/// Takes 3 args: `input` (String), `start` (Int), `end` (Int).
/// Returns a zero-copy slice of the input string.
/// Inherently materializing: must inspect string content to find character boundaries.
pub(crate) fn builtin_str_slice(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("str-slice", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 3 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(3),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Get pre-materialized arguments — calling convention: (string, start, end)
        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let thunk2 = Arc::clone(&args[2]);
        let input_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let start_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let end_val = {
            let (result, diags) = thunk2.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Extract the source Arc<str> from the input string
        let (input_source, input_start, input_end) = match input_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "str-slice",
                        "String",
                        ctx.type_display_name(input_val.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Extract start and end indices
        let start_idx = match start_val {
            Value::Int { n, .. } if n >= 0 => n as usize,
            Value::Int { n, .. } => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "str-slice",
                        "non-negative Int",
                        format!("Int({n})")
                    ),
                    thunk1.span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "str-slice",
                        "Int",
                        ctx.type_display_name(start_val.type_decl_id_of())
                    ),
                    thunk1.span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let end_idx = match end_val {
            Value::Int { n, .. } if n >= 0 => n as usize,
            Value::Int { n, .. } => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "str-slice",
                        "non-negative Int",
                        format!("Int({n})")
                    ),
                    thunk2.span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "str-slice",
                        "Int",
                        ctx.type_display_name(end_val.type_decl_id_of())
                    ),
                    thunk2.span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Validate indices
        if start_idx > end_idx {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("str-slice: start index {start_idx} > end index {end_idx}"),
                call_span.clone(),
            ))
            .with_diagnostics(_dr_diags);
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
            return crate::error::DiagnosticResult::err(Diagnostic::error(KIND,
                format!(
                    "str-slice: end index {end_idx} out of bounds (string has {char_count} characters)"
                ),
                call_span.clone(),
            ))
            .with_diagnostics(_dr_diags);
        }

        let byte_start = input_start + char_indices[start_idx];
        let byte_end = input_start + char_indices[end_idx];

        ok_val(
            Value::String {
                source: input_source,
                start: byte_start,
                end: byte_end,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_STRING,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        {
            let (result, diags) = reject_named("builtin-str-has-nth?", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "String",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        let idx = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => match v.clone() {
                    Value::Int { n, .. } => n,
                    other => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "type mismatch: expected {}, got {}",
                                "Int",
                                ctx.type_display_name(other.type_decl_id_of())
                            ),
                            call_span,
                        ))
                        .with_diagnostics(_dr_diags)
                    }
                },
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        if idx < 0 {
            return ok_val(
                Value::Int {
                    n: 0,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_INT,
                    )),
                },
                call_span,
            )
            .with_diagnostics(_dr_diags);
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
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        {
            let (result, diags) = reject_named("builtin-str-nth-char", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (source, str_start, str_end) = match s_val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "String",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        let idx = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => match v.clone() {
                    Value::Int { n, .. } => n,
                    other => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "type mismatch: expected {}, got {}",
                                "Int",
                                ctx.type_display_name(other.type_decl_id_of())
                            ),
                            call_span,
                        ))
                        .with_diagnostics(_dr_diags)
                    }
                },
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        if idx < 0 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("builtin-str-nth-char: index {idx} is negative"),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
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
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_STRING,
                        )),
                    },
                    call_span,
                )
                .with_diagnostics(_dr_diags)
            }
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-str-nth-char: index {idx} out of bounds (string has {} codepoints)",
                    s.chars().count()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("char-code", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let (input_source, input_start, input_end) = match val {
            Value::String {
                source, start, end, ..
            } => (source, start, end),
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "char-code",
                        "String",
                        ctx.type_display_name(val.type_decl_id_of())
                    ),
                    Arc::clone(&args[0]).span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let input_str = &input_source[input_start..input_end];

        if let Some(ch) = input_str.chars().next() {
            ok_val(
                Value::Int {
                    n: ch as u32 as i64,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_INT,
                    )),
                },
                call_span,
            )
            .with_diagnostics(_dr_diags)
        } else {
            crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "char-code: empty string".to_string(), call_span),
            )
            .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("chr", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match val {
            Value::Int { n, .. } => {
                if let Some(ch) = char::from_u32(n as u32) {
                    ok_val(string_val(&ch.to_string()), call_span)
                        .with_diagnostics(_dr_diags)
                } else {
                    crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("chr: invalid Unicode codepoint {}", n),
                        call_span,
                    ))
                    .with_diagnostics(_dr_diags)
                }
            }
            _ => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "chr",
                    "Int",
                    ctx.type_display_name(val.type_decl_id_of())
                ),
                Arc::clone(&args[0]).span.clone(),
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        use crate::value::bytes_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "str-bytes",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match val.as_str() {
            Some(s) => ok_val(bytes_val(s.as_bytes()), call_span)
                .with_diagnostics(_dr_diags),
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "str-bytes",
                    "String",
                    ctx.type_display_name(val.type_decl_id_of())
                ),
                Arc::clone(&args[0]).span.clone(),
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("str-index-of", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let needle_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let haystack_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let needle = {
            let (result, diags) = require_string("str-index-of", needle_val, thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let haystack = {
            let (result, diags) = require_string("str-index-of", haystack_val, thunk1.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let index: i64 = match haystack.find(needle.as_str()) {
            Some(byte_idx) => {
                // Convert the byte offset to a character index so that all string builtins
                // use the same unit (chars). str-length returns chars, str-slice operates on
                // chars — str-index-of must return chars to be composable with both.
                let char_idx = haystack[..byte_idx].chars().count();
                match i64::try_from(char_idx) {
                    Ok(v) => v,
                    Err(_) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        "str-index-of: character index exceeds i64::MAX".to_string(),
                        call_span,
                    ))
                    .with_diagnostics(_dr_diags),
                }
            }
            None => -1,
        };

        ok_val(
            Value::Int {
                n: index,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `trim-start`: Remove leading whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with leading whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_start(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("trim-start", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("trim-start", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(s.trim_start()), call_span).with_diagnostics(_dr_diags)
    })
}

/// `trim-end`: Remove trailing whitespace from a string.
///
/// Takes 1 arg (String). Returns the string with trailing whitespace stripped.
/// Inherently materializing: must inspect string content to identify and remove whitespace.
pub(crate) fn builtin_trim_end(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("trim-end", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("trim-end", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(s.trim_end()), call_span).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        use crate::value::string_val;

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "bytes-str",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match val.as_bytes() {
            Some(bytes) => match std::str::from_utf8(bytes) {
                Ok(s) => ok_val(string_val(s), call_span)
                    .with_diagnostics(_dr_diags),
                Err(e) => crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!("bytes-str: invalid UTF-8 sequence: {}", e),
                    call_span,
                ))
                .with_diagnostics(_dr_diags),
            },
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "bytes-str",
                    "Bytes",
                    ctx.type_display_name(val.type_decl_id_of())
                ),
                Arc::clone(&args[0]).span.clone(),
            ))
            .with_diagnostics(_dr_diags),
        }
    })
}

/// `upper`: Convert a string to uppercase using Unicode case mapping.
///
/// Takes 1 arg: a String.
/// Returns a String with all characters converted to uppercase.
pub(crate) fn builtin_upper(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("upper", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("upper", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(&s.to_uppercase()), call_span).with_diagnostics(_dr_diags)
    })
}

/// `lower`: Convert a string to lowercase using Unicode case mapping.
///
/// Takes 1 arg: a String.
/// Returns a String with all characters converted to lowercase.
pub(crate) fn builtin_lower(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = expect_one_arg("lower", &args, named.as_ref(), &ctx, call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("lower", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(&s.to_lowercase()), call_span).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("int->string", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        let thunk0 = match args.into_iter().next() {
            Some(v) => v,
            None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    0
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
        };
        let materialized = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match materialized {
            Value::Int { n, .. } => ok_val(string_val(&n.to_string()), call_span)
                .with_diagnostics(_dr_diags),
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "int->string",
                    "Int",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("float->string", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        let thunk0 = match args.into_iter().next() {
            Some(v) => v,
            None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    0
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
        };
        let materialized = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match materialized {
            Value::Float { n, .. } => ok_val(string_val(&n.to_string()), call_span)
                .with_diagnostics(_dr_diags),
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "float->string",
                    "Float",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("regex-match?", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let pattern_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let haystack_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let pattern = {
            let (result, diags) = require_string("regex-match?", pattern_val, thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let haystack = {
            let (result, diags) = require_string("regex-match?", haystack_val, thunk1.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match regex::Regex::new(&pattern) {
            Ok(re) => ok_val(
                Value::Int {
                    n: if re.is_match(&haystack) { 1 } else { 0 },
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_INT,
                    )),
                },
                call_span,
            )
            .with_diagnostics(_dr_diags),
            Err(e) => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("regex-match?: invalid regex pattern {:?}: {}", pattern, e),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-string-concat", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let thunk0 = Arc::clone(&args[0]);
        let thunk1 = Arc::clone(&args[1]);
        let s1_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s2_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let s1 = {
            let (result, diags) = require_string("builtin-string-concat", s1_val, thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s2 = {
            let (result, diags) = require_string("builtin-string-concat", s2_val, thunk1.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let result = format!("{}{}", s1, s2);
        ok_val(string_val(&result), call_span).with_diagnostics(_dr_diags)
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
            "builtin-str-upper",
            builtin_upper,
            [Strictness::Seq],
            1,
            ["str"]
        ),
        builtin!(
            "builtin-str-lower",
            builtin_lower,
            [Strictness::Seq],
            1,
            ["str"]
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
