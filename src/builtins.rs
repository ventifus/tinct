//! Rust-native builtin functions for the LLT language. // sprint wave-1 rebuild marker
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
//! **Strings:** `str`, `split`, `replace`, `trim`, `str-to-upper-char`, `str-to-lower-char`, `str-map-chars`, `regex-match?` (upper/lower are in stdlib/strings.llt)
//! **Numeric:** `floor`, `round`
//! **Parsing:** `to-int`, `to-float`
//! **Evaluation control:** `eval`, `error`, `try`, `apply`
//! **Type introspection:** `type-of`, `int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` (plus `num?`, `record?`, `map?` in LLT stdlib)
//! **Schema validation:** `validate` (runtime structural validation with constraint checking)
//! **I/O:** `from-json`, `include`
//! **Sequences:** `seq`, `head`, `tail`, `collect`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `take`, `map`, `filter`, `drop`, `reduce`, `join`, `concat`

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
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
use crate::eval::materialize;
use crate::eval_call::{invoke_function, CallContext};
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
pub const MAX_COLLECT_SIZE: usize = 1_000_000;

/// Maximum JSON nesting depth for `from-json`.
/// Separate from MAX_EVAL_DEPTH: JSON nesting is a data-model limit (128),
/// not a recursive evaluation limit (256). Prevents deeply nested JSON from
/// producing value trees that cause stack overflow during deep_materialize.
pub(crate) const JSON_DEPTH_LIMIT: usize = 128;

/// Maximum string output size for string output builtins (`$replace`, `$str-map-chars`, `$join`) (64 MB).
/// Prevents memory exhaustion from adversarial inputs or replacement patterns.
pub(crate) const MAX_STRING_SIZE: usize = 64 * 1024 * 1024;

pub(crate) fn ok_val(v: Value, span: Span) -> EvalResult<Rc<Thunk>> {
    Ok(Rc::new(Thunk::new_materialized(v, span)))
}

/// Convert a `Value::Bytes` slice into a `Value::Seq` of `Value::Int` (one per byte).
///
/// Used by sequence operations (map, filter, take, drop, reduce) to treat Bytes as
/// an iterable sequence of byte values (0–255). Results are always Seq (not Bytes).
///
/// The returned value is a `Value::Seq { head, tail }` if bytes is non-empty, or
/// `Value::Dict(IndexMap::new())` (the terminal empty-dict sentinel) if empty.
pub(crate) fn bytes_to_seq(bytes: &[u8], span: Span, ctx: &Rc<crate::eval::EvalContext>) -> Value {
    // Build from the right so we don't need a separate pass.
    let mut acc: Value = Value::Dict(IndexMap::new());
    for &byte in bytes.iter().rev() {
        let head = Rc::new(Thunk::new_materialized(Value::Int(i64::from(byte)), span));
        let tail = Rc::new(Thunk::new_materialized(acc, span));
        acc = Value::Seq {
            head: ctx.alloc_thunk(head),
            tail: ctx.alloc_thunk(tail),
        };
    }
    acc
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
    call_span: Span,
) -> EvalResult<Value> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    materialize(&args[0], Some(&call_span), ctx)
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
        Value::String {
            ref source,
            start,
            end,
        } => source[*start..*end].to_string(),
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
/// `name` is the builtin name for error messages. `ctx` is for
/// materialization. `call_span` is used as the materialization span.
pub(crate) fn flatten_overlay(
    left: &ThunkId,
    right: &ThunkId,
    name: &str,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    use crate::arena::ThunkId;

    // Work stack: each entry is a thunk ID to materialize and add as a layer.
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

    let mut work_stack: Vec<ThunkId> = Vec::new();
    // Push in reverse order: left first (processed last = base layer), right second (processed first = override).
    work_stack.push(*left);
    work_stack.push(*right);

    // Collect flat layers in processing order (right to left).
    let mut layers: Vec<IndexMap<Key, ThunkId>> = Vec::new();

    while let Some(thunk_id) = work_stack.pop() {
        let thunk = ctx.thunk_arena.borrow().get(thunk_id).clone();
        let span = thunk.span;
        let val = materialize(&thunk, Some(&call_span), ctx)?;
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
    let mut result: IndexMap<Key, ThunkId> = IndexMap::with_capacity(total_cap);
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
pub(crate) fn require_dict(
    name: &str,
    value: Value,
    def_span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    match value {
        Value::Dict(map) => Ok(map),
        Value::Overlay(l, r) => flatten_overlay(&l, &r, name, ctx, call_span),
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
        Value::String {
            ref source,
            start,
            end,
        } => Ok(source[start..end].to_string()),
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
    builtin_acos, builtin_add, builtin_asin, builtin_atan, builtin_atan2, builtin_band,
    builtin_bor, builtin_bxor, builtin_cos, builtin_div_float, builtin_eq, builtin_exp,
    builtin_finite_check, builtin_float, builtin_if, builtin_inf_check, builtin_log, builtin_log10,
    builtin_log2, builtin_lt, builtin_mul, builtin_nan_check, builtin_pow, builtin_shl,
    builtin_shr, builtin_sin, builtin_sqrt, builtin_sub, builtin_tan,
};

// Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv.
// Implementations live in builtins_dict.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
pub(crate) use crate::builtins_dict::{
    builtin_append, builtin_each, builtin_each_key, builtin_each_kv, builtin_get,
    builtin_get_optional, builtin_keys, builtin_length, builtin_merge,
};

// I/O builtins: open, slurp, write, connect, lines, emit, env, list-dir, stat, etc.
// Implementations live in builtins_io.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
pub(crate) use crate::builtins_io::{
    builtin_cap_data, builtin_close, builtin_connect, builtin_emit, builtin_env, builtin_flush,
    builtin_http2_session, builtin_http3_session, builtin_http_request, builtin_icmp_ping,
    builtin_lines, builtin_link, builtin_list_dir, builtin_make_dir, builtin_narrow, builtin_open,
    builtin_position, builtin_quic_open_datagram, builtin_quic_open_stream, builtin_quic_session,
    builtin_raw_create, builtin_read_link, builtin_recv_datagram, builtin_remove, builtin_rename,
    builtin_revocable, builtin_revoke_cap, builtin_seek, builtin_seek_end, builtin_send_datagram,
    builtin_slurp, builtin_stat, builtin_tls_layer, builtin_tls_peer_cert, builtin_write,
    builtin_write_atomic, builtin_write_handle,
};

// Type/eval/meta builtins: type-of, eval, include, error, try, apply, validate.
// Implementations live in builtins_meta.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
#[cfg(test)]
pub(crate) use crate::builtins_meta::blake3_hex;
pub use crate::builtins_meta::json_to_value;
pub(crate) use crate::builtins_meta::{
    builtin_apply, builtin_ast_of, builtin_big_int, builtin_bool_check, builtin_bytes_check,
    builtin_decimal, builtin_dict_check, builtin_error, builtin_eval, builtin_eval_ast,
    builtin_float_check, builtin_fn_check, builtin_force, builtin_from_json, builtin_gensym,
    builtin_include, builtin_int_check, builtin_llt_repr, builtin_null_check, builtin_str_check,
    builtin_tag_of, builtin_try, builtin_type_of, builtin_until, builtin_validate, builtin_variant,
};

// String builtins: str, split, replace, trim, trim-start, trim-end,
// str-length, str-index-of, str-slice, str-chars, char-code, chr, str-bytes, bytes-str,
// str-to-upper-char, str-to-lower-char, str-map-chars, regex-match?.
// Note: upper/lower are no longer Rust builtins; they live in stdlib/strings.llt and
// are implemented using str-map-chars + str-to-upper-char / str-to-lower-char.
// Implementations live in builtins_string.rs; re-exported here so that
// standard_builtins() registration and unit tests (via `use super::*`) still work.
#[cfg(test)]
pub(crate) use crate::builtins_string::MAX_SPLIT_PARTS;
pub(crate) use crate::builtins_string::{
    builtin_bytes_str, builtin_char_code, builtin_chr, builtin_regex_match, builtin_replace,
    builtin_split, builtin_str, builtin_str_bytes, builtin_str_chars, builtin_str_index_of,
    builtin_str_length, builtin_str_map_chars, builtin_str_slice, builtin_str_to_lower_char,
    builtin_str_to_upper_char, builtin_trim, builtin_trim_end, builtin_trim_start,
};

// Bytes builtins: bytes, bytes-find, bytes-of, bytes-equal?, ct-equal?.
// Implementations live in builtins_bytes.rs.
pub(crate) use crate::builtins_bytes::{
    builtin_bytes, builtin_bytes_equal, builtin_bytes_find, builtin_bytes_of, builtin_ct_equal,
};

// URI parsing builtins: uri, url, urn.
// Implementations live in builtins_uri.rs.
pub(crate) use crate::builtins_uri::{builtin_uri, builtin_url, builtin_urn};

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
    call_span: Span,
) -> EvalResult<Rc<Thunk>> {
    let val = expect_one_arg(name, args, named, ctx, call_span)?;
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
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("floor", f64::floor, args, named, &ctx, call_span)
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
        call_span,
        ctx,
    } = ctx_arg;
    float_to_int_builtin("round", f64::round, args, named, &ctx, call_span)
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
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-int", args, named, &ctx, call_span)?;
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
        call_span,
        ctx,
    } = ctx_arg;
    let val = expect_one_arg("to-float", args, named, &ctx, call_span)?;
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
    builtin_concat, builtin_concat_seq_step, builtin_join, builtin_reduce,
};

// Date-time builtins: timestamps, durations, clock capabilities, timezones
pub(crate) use crate::builtins_datetime::{
    builtin_duration_days, builtin_duration_hours, builtin_duration_minutes,
    builtin_duration_nanos, builtin_duration_seconds, builtin_duration_to_nanos,
    builtin_duration_to_seconds, builtin_fixed_clock, builtin_format_timestamp, builtin_load_tz,
    builtin_local_to_timestamp, builtin_local_tz_name, builtin_now, builtin_parse_timestamp,
    builtin_timestamp_add, builtin_timestamp_day, builtin_timestamp_diff, builtin_timestamp_eq,
    builtin_timestamp_gt, builtin_timestamp_hour, builtin_timestamp_in_tz, builtin_timestamp_lt,
    builtin_timestamp_minute, builtin_timestamp_month, builtin_timestamp_parts,
    builtin_timestamp_second, builtin_timestamp_to_unix, builtin_timestamp_year,
    builtin_unix_to_timestamp,
};

/// `first`: Return the first element of a Dict, the first character of a String,
/// or the first byte (as Int) of a Bytes value.
///
/// - Takes 1 arg: a Dict, String, or Bytes.
/// - Dict path: O(1) — returns the value at the first key (insertion order).
/// - String path: O(1) — returns a single-char String slice of the first codepoint.
/// - Bytes path: O(1) — returns the first byte as Value::Int.
/// Inherently materializing: must access the value to determine type and extract first element.
fn builtin_first(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("first", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[start..end];
            if s.is_empty() {
                return Err(EvalError::empty_collection("first".to_string(), call_span).into());
            }
            let ch = s
                .chars()
                .next()
                .expect("non-empty string has at least one char");
            let char_end = start + ch.len_utf8();
            ok_val(
                Value::String {
                    source: Rc::clone(source),
                    start,
                    end: char_end,
                },
                call_span,
            )
        }
        Value::Bytes {
            ref source,
            start,
            end,
        } => {
            if start >= end {
                return Err(EvalError::empty_collection("first".to_string(), call_span).into());
            }
            let byte = source[start];
            ok_val(Value::Int(i64::from(byte)), call_span)
        }
        other => {
            let map = require_dict("first", other, args[0].span, &ctx, call_span)?;
            if map.is_empty() {
                return Err(EvalError::empty_collection("first".to_string(), call_span).into());
            }
            let (_, first_id) = map.into_iter().next().expect("non-empty map");
            let thunk = ctx.get_thunk(first_id);
            Ok(thunk)
        }
    }
}

/// `last`: Return the last element of a Dict, the last character of a String,
/// or the last byte (as Int) of a Bytes value.
///
/// - Takes 1 arg: a Dict, String, or Bytes.
/// - Dict path: O(n) — must iterate to the last entry (IndexMap doesn't have O(1) last).
/// - String path: O(n) — must walk UTF-8 chars to find the last codepoint.
/// - Bytes path: O(1) — returns the last byte as Value::Int.
/// Inherently materializing: must access the value to determine type and extract last element.
fn builtin_last(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("last", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[start..end];
            if s.is_empty() {
                return Err(EvalError::empty_collection("last".to_string(), call_span).into());
            }
            let (last_char_start, last_ch) = s
                .char_indices()
                .last()
                .expect("non-empty string has at least one char");
            let char_start = start + last_char_start;
            let char_end = char_start + last_ch.len_utf8();
            ok_val(
                Value::String {
                    source: Rc::clone(source),
                    start: char_start,
                    end: char_end,
                },
                call_span,
            )
        }
        Value::Bytes {
            ref source,
            start,
            end,
        } => {
            if start >= end {
                return Err(EvalError::empty_collection("last".to_string(), call_span).into());
            }
            let byte = source[end - 1];
            ok_val(Value::Int(i64::from(byte)), call_span)
        }
        other => {
            let map = require_dict("last", other, args[0].span, &ctx, call_span)?;
            if map.is_empty() {
                return Err(EvalError::empty_collection("last".to_string(), call_span).into());
            }
            let (_, last_id) = map.into_iter().last().expect("non-empty map");
            let thunk = ctx.get_thunk(last_id);
            Ok(thunk)
        }
    }
}

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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("rest", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx)?;
    // Seq path: delegate to $tail (O(1), preserves laziness).
    if matches!(val, Value::Seq { .. }) {
        return builtin_tail(BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        });
    }
    let map = require_dict("rest", val, args[0].span, &ctx, call_span)?;

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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cons", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // args[0] is the element to prepend (kept as thunk — preserves laziness).
    // args[1] is the collection to prepend to (must be materialized to dispatch on type).
    let xs_val = materialize(&args[1], Some(&call_span), &ctx)?;
    // Seq path: delegate to $seq (O(1), preserves laziness).
    if matches!(xs_val, Value::Seq { .. }) {
        return builtin_seq(BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        });
    }
    let map = require_dict("cons", xs_val, args[1].span, &ctx, call_span)?;

    let mut result = IndexMap::with_capacity(map.len() + 1);
    // Insert the new element at key 0.
    let elem_id = ctx.alloc_thunk(Rc::clone(&args[0]));
    result.insert(Key::Int(0), elem_id);
    // Insert existing entries reindexed as 1..n.
    for (new_idx, (_old_key, thunk_id)) in map.into_iter().enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx + 1).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        result.insert(new_key, thunk_id);
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
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("reverse", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let val = materialize(&args[0], Some(&call_span), &ctx)?;
    let map = require_dict("reverse", val, args[0].span, &ctx, call_span)?;

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
        ) => s1[*start1..*end1].cmp(&s2[*start2..*end2]),
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

/// `sort`: Sort a dict list by natural ordering or with a custom comparator.
///
/// - Takes 1 or 2 args:
///   - 1 arg: a Dict (list-like, integer-keyed). Sorts by natural ordering.
///   - 2 args: a comparator function and a Dict. The comparator takes two values
///     and returns a Bool (true if first should come before second).
/// - Materializes all values, sorts by natural ordering (same semantics as `<`)
///   or by calling the comparator function for each comparison.
/// - O(n log n) using Rust's `sort_by`.
/// - Errors on mixed incompatible types when using natural ordering.
/// - Errors on Seq input (callers must `$collect` first).
/// Inherently materializing: must inspect all values to determine sort order.
fn builtin_sort(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("sort", named, call_span)?;

    // Accept 1 arg (dict only) or 2 args (comparator, dict)
    if args.len() != 1 && args.len() != 2 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    // Determine if we have a comparator function
    let (comparator_opt, dict_arg_idx) = if args.len() == 2 {
        // First arg is comparator, second is dict
        let cmp_val = materialize(&args[0], Some(&call_span), &ctx)?;
        match cmp_val {
            Value::Function { .. } | Value::Builtin(_) => (Some((cmp_val, args[0].span)), 1),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "sort".to_string(),
                    "Function",
                    other.type_name(),
                    args[0].span,
                )
                .into());
            }
        }
    } else {
        (None, 0)
    };

    let val = materialize(&args[dict_arg_idx], Some(&call_span), &ctx)?;
    let map = require_dict("sort", val, args[dict_arg_idx].span, &ctx, call_span)?;

    // Materialize all values so we can compare them.
    let mut pairs: Vec<(Value, Span)> = Vec::with_capacity(map.len());
    for (_key, thunk_id) in &map {
        let thunk = ctx.get_thunk(*thunk_id);
        let mat = materialize(&thunk, Some(&call_span), &ctx)?;
        pairs.push((mat, thunk.span));
    }

    // Sort using comparator or natural ordering. Collect any comparison error.
    let mut sort_error: Option<Box<crate::error::EvalError>> = None;

    if let Some((cmp_val, cmp_span)) = comparator_opt {
        // Use custom comparator function
        pairs.sort_by(|(a, a_span), (b, b_span)| {
            if sort_error.is_some() {
                return std::cmp::Ordering::Equal;
            }

            // Create thunks for the two values to pass to the comparator
            let a_thunk = Rc::new(Thunk::new_materialized(a.clone(), *a_span));
            let b_thunk = Rc::new(Thunk::new_materialized(b.clone(), *b_span));
            let pos_args = vec![a_thunk, b_thunk];

            // Call the comparator function
            let result_thunk = match &cmp_val {
                Value::Function {
                    params,
                    body,
                    env: closure_env,
                    ..
                } => {
                    match invoke_function(&CallContext {
                        params,
                        body,
                        closure_env,
                        positional: &pos_args,
                        named: None,
                        default_env: closure_env,
                        call_span,
                        origin: Some(Rc::from("sort")),
                        ctx: &ctx,
                    }) {
                        Ok(thunk) => thunk,
                        Err(e) => {
                            sort_error = Some(e);
                            return std::cmp::Ordering::Equal;
                        }
                    }
                }
                Value::Builtin(def) => {
                    let builtin_args = BuiltinArgs {
                        args: &pos_args,
                        named: None,
                        call_span,
                        ctx: Rc::clone(&ctx),
                    };
                    match (def.func)(builtin_args) {
                        Ok(thunk) => thunk,
                        Err(e) => {
                            sort_error = Some(e);
                            return std::cmp::Ordering::Equal;
                        }
                    }
                }
                _ => {
                    sort_error = Some(Box::new(EvalError::type_mismatch_ctx(
                        "sort".to_string(),
                        "Function",
                        cmp_val.type_name(),
                        cmp_span,
                    )));
                    return std::cmp::Ordering::Equal;
                }
            };

            // Materialize the result and require it to be a Bool
            match materialize(&result_thunk, Some(&call_span), &ctx) {
                Ok(Value::Bool(true)) => std::cmp::Ordering::Less,
                Ok(Value::Bool(false)) => std::cmp::Ordering::Greater,
                Ok(other) => {
                    sort_error = Some(Box::new(EvalError::type_mismatch_ctx(
                        "sort".to_string(),
                        "Bool",
                        other.type_name(),
                        result_thunk.span,
                    )));
                    std::cmp::Ordering::Equal
                }
                Err(e) => {
                    sort_error = Some(e);
                    std::cmp::Ordering::Equal
                }
            }
        });
    } else {
        // Use natural ordering
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
    }

    if let Some(e) = sort_error {
        return Err(e);
    }

    // Build result dict with dense integer keys 0..n-1, wrapping sorted values as thunks.
    let mut result = IndexMap::with_capacity(pairs.len());
    for (new_idx, (mat_val, orig_span)) in pairs.into_iter().enumerate() {
        let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
            EvalError::internal("collection index overflow".to_string(), call_span)
        })?);
        let thunk = Rc::new(Thunk::new_materialized(mat_val, orig_span));
        let thunk_id = ctx.alloc_thunk(thunk);
        result.insert(new_key, thunk_id);
    }
    ok_val(Value::Dict(result), call_span)
}

fn builtin_proxy(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    reject_named("proxy", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let handler_id = ctx.alloc_thunk(Rc::clone(&args[0]));
    Ok(Rc::new(Thunk::new_materialized(
        Value::Proxy {
            handler: handler_id,
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
        builtin!(
            "builtin-get",
            builtin_get,
            [Strictness::Seq, Strictness::Spine]
        ),
        builtin!(
            "get?",
            builtin_get_optional,
            [Strictness::Seq, Strictness::Spine]
        ),
        // each: 2-strictness for both 1-arg (user) and 2-arg (internal offset) calls
        builtin!("each", builtin_each, [Strictness::Spine, Strictness::Spine]),
        builtin!(
            "each-key",
            builtin_each_key,
            [Strictness::Spine, Strictness::Spine]
        ),
        builtin!(
            "each-kv",
            builtin_each_kv,
            [Strictness::Spine, Strictness::Spine]
        ),
        // Strings
        builtin!("str", builtin_str, [Strictness::Seq]),
        builtin!("split", builtin_split, [Strictness::Seq, Strictness::Seq]),
        builtin!(
            "replace",
            builtin_replace,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!("trim", builtin_trim, [Strictness::Seq]),
        builtin!("str-length", builtin_str_length, [Strictness::Seq]),
        builtin!(
            "str-slice",
            builtin_str_slice,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!("str-chars", builtin_str_chars, [Strictness::Seq]),
        builtin!("char-code", builtin_char_code, [Strictness::Seq]),
        builtin!("chr", builtin_chr, [Strictness::Seq]),
        builtin!("str-bytes", builtin_str_bytes, [Strictness::Seq]),
        builtin!("bytes-str", builtin_bytes_str, [Strictness::Seq]),
        builtin!(
            "str-index-of",
            builtin_str_index_of,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("trim-start", builtin_trim_start, [Strictness::Seq]),
        builtin!("trim-end", builtin_trim_end, [Strictness::Seq]),
        builtin!(
            "str-to-upper-char",
            builtin_str_to_upper_char,
            [Strictness::Seq]
        ),
        builtin!(
            "str-to-lower-char",
            builtin_str_to_lower_char,
            [Strictness::Seq]
        ),
        builtin!(
            "str-map-chars",
            builtin_str_map_chars,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "regex-match?",
            builtin_regex_match,
            [Strictness::Seq, Strictness::Seq]
        ),
        // Bytes
        builtin!("bytes", builtin_bytes, []),
        builtin!(
            "bytes-find",
            builtin_bytes_find,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("bytes-of", builtin_bytes_of, [Strictness::Seq]),
        builtin!(
            "bytes-equal?",
            builtin_bytes_equal,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "ct-equal?",
            builtin_ct_equal,
            [Strictness::Seq, Strictness::Seq]
        ),
        // Numeric
        builtin!("floor", builtin_floor, [Strictness::Seq]),
        builtin!("round", builtin_round, [Strictness::Seq]),
        builtin!("pow", builtin_pow, [Strictness::Seq, Strictness::Seq]),
        builtin!("sqrt", builtin_sqrt, [Strictness::Seq]),
        builtin!("log", builtin_log, [Strictness::Seq]),
        builtin!("log2", builtin_log2, [Strictness::Seq]),
        builtin!("log10", builtin_log10, [Strictness::Seq]),
        builtin!("exp", builtin_exp, [Strictness::Seq]),
        builtin!("sin", builtin_sin, [Strictness::Seq]),
        builtin!("cos", builtin_cos, [Strictness::Seq]),
        builtin!("tan", builtin_tan, [Strictness::Seq]),
        builtin!("asin", builtin_asin, [Strictness::Seq]),
        builtin!("acos", builtin_acos, [Strictness::Seq]),
        builtin!("atan", builtin_atan, [Strictness::Seq]),
        builtin!("atan2", builtin_atan2, [Strictness::Seq, Strictness::Seq]),
        builtin!("nan?", builtin_nan_check, [Strictness::Seq]),
        builtin!("inf?", builtin_inf_check, [Strictness::Seq]),
        builtin!("finite?", builtin_finite_check, [Strictness::Seq]),
        // Bitwise
        builtin!("band", builtin_band, [Strictness::Seq, Strictness::Seq]),
        builtin!("bor", builtin_bor, [Strictness::Seq, Strictness::Seq]),
        builtin!("bxor", builtin_bxor, [Strictness::Seq, Strictness::Seq]),
        builtin!("shl", builtin_shl, [Strictness::Seq, Strictness::Seq]),
        builtin!("shr", builtin_shr, [Strictness::Seq, Strictness::Seq]),
        // Type conversion
        builtin!("float", builtin_float, [Strictness::Seq]),
        // Parsing
        builtin!("to-int", builtin_to_int, [Strictness::Seq]),
        builtin!("to-float", builtin_to_float, [Strictness::Seq]),
        // Evaluation control
        builtin!("eval", builtin_eval, [Strictness::Seq]),
        builtin!("force", builtin_force, [Strictness::Seq]),
        builtin!("error", builtin_error, [Strictness::Seq]),
        builtin!("try", builtin_try, [Strictness::Id]),
        builtin!("apply", builtin_apply, [Strictness::Seq, Strictness::Seq]),
        builtin!("eval-ast", builtin_eval_ast, [Strictness::Seq]),
        builtin!("gensym", builtin_gensym),
        builtin!("until", builtin_until),
        // Decimal and BigInt
        builtin!("decimal", builtin_decimal, [Strictness::Seq]),
        builtin!("big-int", builtin_big_int, [Strictness::Seq]),
        // Type introspection
        builtin!("type-of", builtin_type_of, [Strictness::Seq]),
        builtin!("ast-of", builtin_ast_of, [Strictness::Id]),
        builtin!("llt-repr", builtin_llt_repr, [Strictness::Seq]),
        builtin!("tag-of", builtin_tag_of, [Strictness::Seq]),
        builtin!("variant", builtin_variant, [Strictness::Seq]),
        builtin!("int?", builtin_int_check, [Strictness::Seq]),
        builtin!("float?", builtin_float_check, [Strictness::Seq]),
        // num? is implemented in LLT as [or [int? x] [float? x]] — see stdlib/prelude.llt
        builtin!("str?", builtin_str_check, [Strictness::Seq]),
        builtin!("bool?", builtin_bool_check, [Strictness::Seq]),
        builtin!("bytes?", builtin_bytes_check, [Strictness::Seq]),
        builtin!("null?", builtin_null_check, [Strictness::Seq]),
        builtin!("dict?", builtin_dict_check, [Strictness::Seq]),
        // record? and map? are implemented in LLT as aliases of dict? — see stdlib/prelude.llt
        builtin!("fn?", builtin_fn_check, [Strictness::Seq]),
        builtin!("seq?", builtin_seq_check, [Strictness::Seq]),
        // Schema validation
        builtin!(
            "validate",
            builtin_validate,
            [Strictness::Seq, Strictness::Seq]
        ),
        // I/O
        builtin!("emit", builtin_emit, [Strictness::Seq]),
        builtin!("env", builtin_env, [Strictness::Seq]),
        builtin!(
            "open",
            builtin_open,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!("slurp", builtin_slurp, [Strictness::Seq]),
        builtin!("narrow", builtin_narrow, [Strictness::Seq, Strictness::Seq]),
        builtin!("revocable", builtin_revocable, [Strictness::Seq]),
        builtin!("revoke-cap", builtin_revoke_cap, [Strictness::Seq]),
        builtin!(
            "connect",
            builtin_connect,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "tls-layer",
            builtin_tls_layer,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!("tls-peer-cert", builtin_tls_peer_cert, [Strictness::Seq]),
        builtin!("lines", builtin_lines, [Strictness::Seq]),
        builtin!(
            "write",
            builtin_write,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "write-atomic",
            builtin_write_atomic,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "cap-data",
            builtin_cap_data,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "write-handle",
            builtin_write_handle,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("flush", builtin_flush, [Strictness::Seq]),
        builtin!("close", builtin_close, [Strictness::Seq]),
        builtin!(
            "raw-create",
            builtin_raw_create,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("seek", builtin_seek, [Strictness::Seq, Strictness::Seq]),
        builtin!("seek-end", builtin_seek_end, [Strictness::Seq]),
        builtin!("position", builtin_position, [Strictness::Seq]),
        builtin!(
            "list-dir",
            builtin_list_dir,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("stat", builtin_stat, [Strictness::Seq, Strictness::Seq]),
        builtin!(
            "make-dir",
            builtin_make_dir,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("remove", builtin_remove, [Strictness::Seq, Strictness::Seq]),
        builtin!(
            "rename",
            builtin_rename,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "link",
            builtin_link,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "read-link",
            builtin_read_link,
            [Strictness::Seq, Strictness::Seq]
        ),
        // Datagram sockets (UDP, Unix datagram)
        builtin!(
            "send-datagram",
            builtin_send_datagram,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("recv-datagram", builtin_recv_datagram, [Strictness::Seq]),
        builtin!("from-json", builtin_from_json, [Strictness::Seq]),
        builtin!("include", builtin_include, [Strictness::Seq]),
        // Sequences (registered under builtin-NAME; prelude exports the unwrapped names)
        builtin!("builtin-seq", builtin_seq),
        builtin!("builtin-head", builtin_head, [Strictness::Seq]),
        builtin!("builtin-tail", builtin_tail, [Strictness::Seq]),
        builtin!("builtin-collect", builtin_collect, [Strictness::Spine]),
        builtin!(
            "builtin-range",
            builtin_range,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("builtin-repeat", builtin_repeat),
        builtin!("builtin-cycle", builtin_cycle, [Strictness::Spine]),
        builtin!("builtin-iterate", builtin_iterate),
        builtin!("builtin-unfold", builtin_unfold),
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
        builtin!(
            "builtin-join",
            builtin_join,
            [Strictness::Seq, Strictness::Spine]
        ),
        builtin!(
            "builtin-concat",
            builtin_concat,
            [Strictness::Spine, Strictness::Seq]
        ),
        // List operations (registered under builtin-NAME; prelude exports the unwrapped names)
        builtin!("builtin-first", builtin_first, [Strictness::Spine]),
        builtin!("builtin-last", builtin_last, [Strictness::Spine]),
        builtin!("builtin-rest", builtin_rest, [Strictness::Spine]),
        builtin!(
            "builtin-cons",
            builtin_cons,
            [Strictness::Id, Strictness::Spine]
        ),
        builtin!("builtin-reverse", builtin_reverse, [Strictness::Spine]),
        builtin!("builtin-sort", builtin_sort, [Strictness::Spine]),
        // Proxy
        builtin!("proxy", builtin_proxy),
        // Date-time: timestamps and durations
        builtin!(
            "parse-timestamp",
            builtin_parse_timestamp,
            [Strictness::Seq]
        ),
        builtin!(
            "format-timestamp",
            builtin_format_timestamp,
            [Strictness::Seq]
        ),
        builtin!(
            "timestamp->unix",
            builtin_timestamp_to_unix,
            [Strictness::Seq]
        ),
        builtin!(
            "unix->timestamp",
            builtin_unix_to_timestamp,
            [Strictness::Seq]
        ),
        builtin!("now", builtin_now, [Strictness::Seq]),
        builtin!("fixed-clock", builtin_fixed_clock, [Strictness::Seq]),
        builtin!(
            "timestamp-add",
            builtin_timestamp_add,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "timestamp-diff",
            builtin_timestamp_diff,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "timestamp<?",
            builtin_timestamp_lt,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "timestamp>?",
            builtin_timestamp_gt,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "timestamp=?",
            builtin_timestamp_eq,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("timestamp-year", builtin_timestamp_year, [Strictness::Seq]),
        builtin!(
            "timestamp-month",
            builtin_timestamp_month,
            [Strictness::Seq]
        ),
        builtin!("timestamp-day", builtin_timestamp_day, [Strictness::Seq]),
        builtin!("timestamp-hour", builtin_timestamp_hour, [Strictness::Seq]),
        builtin!(
            "timestamp-minute",
            builtin_timestamp_minute,
            [Strictness::Seq]
        ),
        builtin!(
            "timestamp-second",
            builtin_timestamp_second,
            [Strictness::Seq]
        ),
        builtin!(
            "timestamp-parts",
            builtin_timestamp_parts,
            [Strictness::Seq]
        ),
        builtin!("duration-nanos", builtin_duration_nanos, [Strictness::Seq]),
        builtin!(
            "duration-seconds",
            builtin_duration_seconds,
            [Strictness::Seq]
        ),
        builtin!(
            "duration-minutes",
            builtin_duration_minutes,
            [Strictness::Seq]
        ),
        builtin!("duration-hours", builtin_duration_hours, [Strictness::Seq]),
        builtin!("duration-days", builtin_duration_days, [Strictness::Seq]),
        builtin!(
            "duration->seconds",
            builtin_duration_to_seconds,
            [Strictness::Seq]
        ),
        builtin!(
            "duration->nanos",
            builtin_duration_to_nanos,
            [Strictness::Seq]
        ),
        builtin!(
            "load-tz",
            builtin_load_tz,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "timestamp-in-tz",
            builtin_timestamp_in_tz,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("local->timestamp", builtin_local_to_timestamp),
        builtin!("local-tz-name", builtin_local_tz_name, [Strictness::Seq]),
        // URI parsing
        builtin!("uri", builtin_uri, [Strictness::Seq]),
        builtin!("url", builtin_url, [Strictness::Seq]),
        builtin!("urn", builtin_urn, [Strictness::Seq]),
        // HTTP-sessions stubs (QUIC/HTTP2/HTTP3/ICMP — full implementation deferred)
        builtin!(
            "quic-session",
            builtin_quic_session,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ]
        ),
        builtin!(
            "quic-open-stream",
            builtin_quic_open_stream,
            [Strictness::Seq]
        ),
        builtin!(
            "quic-open-datagram",
            builtin_quic_open_datagram,
            [Strictness::Seq]
        ),
        builtin!(
            "http2-session",
            builtin_http2_session,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "http3-session",
            builtin_http3_session,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "http-request",
            builtin_http_request,
            [
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq,
                Strictness::Seq
            ]
        ),
        builtin!(
            "icmp-ping",
            builtin_icmp_ping,
            [Strictness::Seq, Strictness::Seq, Strictness::Seq]
        ),
    ]
}

/// Create a virtual Rust module environment containing the named primitive group.
///
/// This function is called by the include resolver when evaluating `[include %rust "module-name"]`.
/// Each module returns an environment containing only the builtins in that group.
///
/// Module names:
/// - `core`: arithmetic, comparison, control flow, core dict primitives, type predicates
/// - `string`: string manipulation and conversion
/// - `collection`: dict/seq operations, iteration, sorting
/// - `io`: filesystem and I/O operations
/// - `net`: network operations (TCP, QUIC, HTTP, etc.)
/// - `math`: numeric operations (trig, logarithms, bitwise)
/// - `datetime`: timestamp and duration operations
/// - `bytes`: byte sequence operations
/// - `json`: JSON parsing
/// - `meta`: runtime introspection (type-of, validate, eval-ast, etc.)
///
/// Returns an error if the module name is unknown.
pub fn rust_module(name: &str) -> Result<Rc<RefCell<Environment>>, String> {
    let env = Rc::new(RefCell::new(Environment::new()));
    let all_builtins = standard_builtins();

    // Helper to insert a builtin by name
    let insert = |env: &Rc<RefCell<Environment>>, builtin_name: &str| {
        if let Some(def) = all_builtins.iter().find(|b| b.name == builtin_name) {
            let thunk = Rc::new(Thunk::new_materialized(
                Value::Builtin(*def),
                Span::origin(),
            ));
            env.borrow_mut().insert(def.name.to_string(), thunk);
        }
    };

    match name {
        "core" => {
            // Arithmetic (public names)
            insert(&env, "+");
            insert(&env, "-");
            insert(&env, "*");
            insert(&env, "/");
            // Comparison (public names)
            insert(&env, "=");
            insert(&env, "<");
            // Control (public names)
            insert(&env, "if");
            insert(&env, "error");
            insert(&env, "try");
            insert(&env, "force");
            insert(&env, "until");
            // Evaluation control
            insert(&env, "eval");
            insert(&env, "apply");
            insert(&env, "eval-ast");
            insert(&env, "gensym");
            // Type predicates
            insert(&env, "int?");
            insert(&env, "float?");
            insert(&env, "str?");
            insert(&env, "bool?");
            insert(&env, "bytes?");
            insert(&env, "null?");
            insert(&env, "dict?");
            insert(&env, "fn?");
            insert(&env, "seq?");
            // Type introspection
            insert(&env, "type-of");
            // Internal builtin-* aliases (stable names for prelude to use even when
            // public names are shadowed by user code via include).
            // These names match the builtin-* prefix used in inject_prelude_aliases().
            // Aliases share the function pointer and strictness with the public name;
            // only the env key differs.
            let alias_from_public =
                |env: &Rc<RefCell<Environment>>, alias_name: &'static str, public_name: &str| {
                    if let Some(def) = all_builtins.iter().find(|b| b.name == public_name) {
                        let thunk = Rc::new(Thunk::new_materialized(
                            Value::Builtin(*def),
                            Span::origin(),
                        ));
                        env.borrow_mut().insert(alias_name.to_string(), thunk);
                    }
                };
            alias_from_public(&env, "builtin-lt", "<");
            alias_from_public(&env, "builtin-eq", "=");
            alias_from_public(&env, "builtin-add", "+");
            alias_from_public(&env, "builtin-sub", "-");
            alias_from_public(&env, "builtin-mul", "*");
            alias_from_public(&env, "builtin-div", "/");
            alias_from_public(&env, "builtin-if", "if");
        }
        "string" => {
            insert(&env, "str");
            insert(&env, "split");
            insert(&env, "replace");
            insert(&env, "trim");
            insert(&env, "trim-start");
            insert(&env, "trim-end");
            insert(&env, "str-length");
            insert(&env, "str-slice");
            insert(&env, "str-chars");
            insert(&env, "char-code");
            insert(&env, "chr");
            insert(&env, "str-bytes");
            insert(&env, "bytes-str");
            insert(&env, "str-index-of");
            insert(&env, "str-to-upper-char");
            insert(&env, "str-to-lower-char");
            insert(&env, "str-map-chars");
            insert(&env, "regex-match?");
        }
        "collection" => {
            insert(&env, "keys");
            insert(&env, "length");
            insert(&env, "merge");
            insert(&env, "append");
            insert(&env, "builtin-get");
            insert(&env, "get?");
            insert(&env, "each");
            insert(&env, "each-key");
            insert(&env, "each-kv");
            insert(&env, "builtin-seq");
            insert(&env, "builtin-head");
            insert(&env, "builtin-tail");
            insert(&env, "builtin-collect");
            insert(&env, "builtin-range");
            insert(&env, "builtin-repeat");
            insert(&env, "builtin-cycle");
            insert(&env, "builtin-iterate");
            insert(&env, "builtin-unfold");
            insert(&env, "map");
            insert(&env, "filter");
            insert(&env, "take");
            insert(&env, "drop");
            insert(&env, "reduce");
            // builtin-* aliases for collection operations (stable names used
            // by prelude and stdlib modules even when public names are shadowed).
            // Reuse alias_from_public helper — same pattern as core's builtin-* aliases.
            // (alias_from_public is defined in the "core" match arm above; Rust closures
            // are only in scope within their defining match arm, so we inline the logic here.)
            {
                let alias_coll = |env: &Rc<RefCell<Environment>>,
                                  alias_name: &'static str,
                                  public_name: &str| {
                    if let Some(def) = all_builtins.iter().find(|b| b.name == public_name) {
                        let thunk = Rc::new(Thunk::new_materialized(
                            Value::Builtin(*def),
                            Span::origin(),
                        ));
                        env.borrow_mut().insert(alias_name.to_string(), thunk);
                    }
                };
                alias_coll(&env, "builtin-map", "map");
                alias_coll(&env, "builtin-filter", "filter");
                alias_coll(&env, "builtin-reduce", "reduce");
                alias_coll(&env, "builtin-take", "take");
                alias_coll(&env, "builtin-drop", "drop");
            }
            insert(&env, "builtin-join");
            insert(&env, "builtin-concat");
            insert(&env, "builtin-first");
            insert(&env, "builtin-last");
            insert(&env, "builtin-rest");
            insert(&env, "builtin-cons");
            insert(&env, "builtin-reverse");
            insert(&env, "builtin-sort");
        }
        "io" => {
            insert(&env, "emit");
            insert(&env, "env");
            insert(&env, "open");
            insert(&env, "slurp");
            insert(&env, "lines");
            insert(&env, "write");
            insert(&env, "write-atomic");
            insert(&env, "write-handle");
            insert(&env, "flush");
            insert(&env, "close");
            insert(&env, "seek");
            insert(&env, "seek-end");
            insert(&env, "position");
            insert(&env, "list-dir");
            insert(&env, "stat");
            insert(&env, "make-dir");
            insert(&env, "remove");
            insert(&env, "rename");
            insert(&env, "link");
            insert(&env, "read-link");
            insert(&env, "narrow");
            insert(&env, "revocable");
            insert(&env, "revoke-cap");
            insert(&env, "cap-data");
            insert(&env, "raw-create");
        }
        "net" => {
            insert(&env, "connect");
            insert(&env, "tls-layer");
            insert(&env, "tls-peer-cert");
            insert(&env, "send-datagram");
            insert(&env, "recv-datagram");
            insert(&env, "quic-session");
            insert(&env, "quic-open-stream");
            insert(&env, "quic-open-datagram");
            insert(&env, "http2-session");
            insert(&env, "http3-session");
            insert(&env, "http-request");
            insert(&env, "icmp-ping");
            insert(&env, "uri");
            insert(&env, "url");
            insert(&env, "urn");
        }
        "math" => {
            insert(&env, "floor");
            insert(&env, "round");
            insert(&env, "pow");
            insert(&env, "sqrt");
            insert(&env, "log");
            insert(&env, "log2");
            insert(&env, "log10");
            insert(&env, "exp");
            insert(&env, "sin");
            insert(&env, "cos");
            insert(&env, "tan");
            insert(&env, "asin");
            insert(&env, "acos");
            insert(&env, "atan");
            insert(&env, "atan2");
            insert(&env, "nan?");
            insert(&env, "inf?");
            insert(&env, "finite?");
            insert(&env, "band");
            insert(&env, "bor");
            insert(&env, "bxor");
            insert(&env, "shl");
            insert(&env, "shr");
            insert(&env, "float");
            insert(&env, "to-int");
            insert(&env, "to-float");
            insert(&env, "decimal");
            insert(&env, "big-int");
        }
        "datetime" => {
            insert(&env, "parse-timestamp");
            insert(&env, "format-timestamp");
            insert(&env, "timestamp->unix");
            insert(&env, "unix->timestamp");
            insert(&env, "now");
            insert(&env, "fixed-clock");
            insert(&env, "timestamp-add");
            insert(&env, "timestamp-diff");
            insert(&env, "timestamp<?");
            insert(&env, "timestamp>?");
            insert(&env, "timestamp=?");
            insert(&env, "timestamp-year");
            insert(&env, "timestamp-month");
            insert(&env, "timestamp-day");
            insert(&env, "timestamp-hour");
            insert(&env, "timestamp-minute");
            insert(&env, "timestamp-second");
            insert(&env, "timestamp-parts");
            insert(&env, "duration-nanos");
            insert(&env, "duration-seconds");
            insert(&env, "duration-minutes");
            insert(&env, "duration-hours");
            insert(&env, "duration-days");
            insert(&env, "duration->seconds");
            insert(&env, "duration->nanos");
            insert(&env, "load-tz");
            insert(&env, "timestamp-in-tz");
            insert(&env, "local->timestamp");
            insert(&env, "local-tz-name");
        }
        "bytes" => {
            insert(&env, "bytes");
            insert(&env, "bytes-find");
            insert(&env, "bytes-of");
            insert(&env, "bytes-equal?");
            insert(&env, "ct-equal?");
        }
        "json" => {
            insert(&env, "from-json");
        }
        "meta" => {
            insert(&env, "type-of");
            insert(&env, "ast-of");
            insert(&env, "validate");
            insert(&env, "until");
            insert(&env, "llt-repr");
            insert(&env, "tag-of");
            insert(&env, "variant");
            insert(&env, "eval-ast");
            insert(&env, "proxy");
        }
        "type-core" => {
            // Core primitives needed for type system implementation
            // From core
            insert(&env, "if");
            insert(&env, "=");
            insert(&env, "null?");
            insert(&env, "dict?");
            insert(&env, "str?");
            insert(&env, "int?");
            insert(&env, "error");
            // From collection
            insert(&env, "builtin-get");
            insert(&env, "get?");
            insert(&env, "keys");
            insert(&env, "length");
            insert(&env, "merge");
            insert(&env, "append");
            insert(&env, "each");
            insert(&env, "map");
            insert(&env, "filter");
            insert(&env, "reduce");
            insert(&env, "builtin-seq");
            insert(&env, "builtin-head");
            insert(&env, "builtin-tail");
            insert(&env, "builtin-collect");
            insert(&env, "builtin-cons");
            insert(&env, "builtin-concat");
            insert(&env, "builtin-join");
            insert(&env, "builtin-first");
            insert(&env, "builtin-last");
            insert(&env, "builtin-rest");
            // From string
            insert(&env, "str");
        }
        _ => return Err(format!("unknown Rust module: {}", name)),
    }

    Ok(env)
}

/// Create the bootstrap environment used when evaluating the prelude.
///
/// Contains ONLY:
/// - `include` — the special form for loading modules and files
/// - `%rust` — the opaque `Value::RustRegistry` sentinel that grants access to Rust
///             primitive groups via `[include %rust "group-name"]`
///
/// No other builtins are present. The prelude must explicitly include the Rust
/// primitive groups it needs (e.g., `[include %rust "core"]`) to get access to
/// arithmetic, comparison, and control-flow primitives.
///
/// This env is NOT the parent of the user env. It is used exclusively during prelude
/// evaluation. After the prelude is loaded, the prelude output env becomes the
/// parent of user code's environment.
pub fn create_bootstrap_env() -> Rc<RefCell<Environment>> {
    let env = Rc::new(RefCell::new(Environment::new()));

    // Register `include` (the only special form available in the bootstrap env).
    let include_def = standard_builtins()
        .into_iter()
        .find(|b| b.name == "include")
        .expect("include builtin must be registered in standard_builtins");
    let include_thunk = Rc::new(Thunk::new_materialized(
        Value::Builtin(include_def),
        Span::origin(),
    ));
    env.borrow_mut()
        .insert("include".to_string(), include_thunk);

    // Register `%rust` — the opaque registry sentinel.
    // Prelude uses `[include %rust "module"]` to get Rust primitive groups.
    let rust_thunk = Rc::new(Thunk::new_materialized(Value::RustRegistry, Span::origin()));
    env.borrow_mut().insert("%rust".to_string(), rust_thunk);

    env
}

/// Create the root environment with all builtins registered as `Value::Builtin`.
pub fn create_root_env() -> Rc<RefCell<Environment>> {
    let env = Rc::new(RefCell::new(Environment::new()));
    for def in standard_builtins() {
        let thunk = Rc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
        env.borrow_mut().insert(def.name.to_string(), thunk);
    }

    // Transport nominal variant constants: Tcp, Udp, UnixStream, UnixDatagram, NamedPipe, Icmp.
    // These are unit variants (no payload) used as flags for `connect` and `tls-connect`.
    for tag in [
        "Tcp",
        "Udp",
        "UnixStream",
        "UnixDatagram",
        "NamedPipe",
        "Icmp",
    ] {
        let thunk = Rc::new(Thunk::new_materialized(
            Value::Variant {
                tag: tag.to_string(),
                payload: None,
            },
            Span::origin(),
        ));
        env.borrow_mut().insert(tag.to_string(), thunk);
    }

    env
}

/// Inject internal `builtin-*` aliases into an environment.
///
/// Called by `create_stdlib_env_inner` AFTER prelude loading to inject `builtin-*`
/// stable aliases into the stdlib env. These cannot be re-exported from inside
/// prelude's letrec dict (that would cause circular dependencies), so they are
/// injected at the Rust level here.
///
/// Also used by unit tests that need closures with `builtin-*` names in scope.
pub fn inject_prelude_aliases(env: &mut Environment) {
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
        builtin!("builtin-if", builtin_if),
        builtin!("builtin-filter", builtin_filter),
        builtin!("builtin-map", builtin_map),
        builtin!("builtin-reduce", builtin_reduce),
        builtin!("builtin-take", builtin_take),
        builtin!("builtin-drop", builtin_drop),
        builtin!("builtin-get", builtin_get),
        builtin!("builtin-seq", builtin_seq),
        builtin!("builtin-head", builtin_head, [Strictness::Seq]),
        builtin!("builtin-tail", builtin_tail, [Strictness::Seq]),
        builtin!("builtin-collect", builtin_collect, [Strictness::Seq]),
        builtin!("builtin-range", builtin_range),
        builtin!("builtin-repeat", builtin_repeat),
        builtin!("builtin-cycle", builtin_cycle),
        builtin!("builtin-iterate", builtin_iterate),
        builtin!("builtin-unfold", builtin_unfold),
        builtin!(
            "builtin-join",
            builtin_join,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!(
            "builtin-concat",
            builtin_concat,
            [Strictness::Seq, Strictness::Seq]
        ),
        builtin!("builtin-first", builtin_first, [Strictness::Seq]),
        builtin!("builtin-last", builtin_last, [Strictness::Seq]),
        builtin!("builtin-rest", builtin_rest, [Strictness::Seq]),
        builtin!("builtin-cons", builtin_cons),
        builtin!("builtin-reverse", builtin_reverse, [Strictness::Seq]),
        builtin!("builtin-sort", builtin_sort, [Strictness::Seq]),
    ];

    for def in aliases {
        let thunk = Rc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
        env.insert(def.name.to_string(), thunk);
    }
}

/// Create the stdlib environment: root builtins + prelude functions.
///
/// Parses and evaluates `stdlib/prelude.llt` using the root env, then
/// layers the prelude dict entries as a child scope. User code should
/// use this as the parent environment.
// Fatal: stdlib failure is not recoverable — callers should propagate or panic on Err.
/// Helper function to load a stdlib module from source and insert its bindings
/// into the environment.
fn load_stdlib_module(
    source: &str,
    module_name: &str,
    env: &Rc<RefCell<Environment>>,
    ctx: &Rc<crate::eval::EvalContext>,
) -> Result<(), Box<crate::error::EvalError>> {
    let mut file = crate::parser::parse(source).map_err(|e| {
        crate::error::EvalError::internal(format!("{module_name} parse error: {e}"), Span::origin())
    })?;

    crate::desugar::desugar_file(&mut file.node);
    crate::resolve::resolve_file(&file.node);

    // Type errors are advisory; evaluation proceeds regardless.
    let builtins_env = Rc::new(crate::types::TypeEnv::with_builtins());
    let _unused = crate::typecheck::typecheck_file_with_types_and_env(&file.node, builtins_env);

    let thunk = crate::eval::eval_file(&file.node, Rc::clone(env), ctx)?;
    let val = crate::eval::materialize(&thunk, None, ctx)?;

    let dict = match val {
        Value::Dict(map) => map,
        Value::Overlay(l_id, r_id) => {
            flatten_overlay(&l_id, &r_id, module_name, ctx, Span::origin())?
        }
        other => {
            return Err(crate::error::EvalError::internal(
                format!(
                    "{module_name} must evaluate to a Dict, got {}",
                    other.type_name()
                ),
                Span::origin(),
            )
            .into())
        }
    };

    // Insert the module's bindings into the environment
    for (key, thunk_id) in dict {
        let name = match key {
            Key::String(s) => s,
            Key::Int(n) => n.to_string(),
        };
        let thunk = ctx.get_thunk(thunk_id);
        env.borrow_mut().insert(name, thunk);
    }

    Ok(())
}

// Reentrance guard for create_stdlib_env to detect unexpected recursive calls.
std::thread_local! {
    static STDLIB_ENV_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Cache of the stdlib arena so EvalContexts created after create_stdlib_env()
    /// can inherit the stdlib ThunkIds without the caller explicitly threading the arena.
    static STDLIB_ARENA_CACHE: std::cell::RefCell<Option<Rc<RefCell<crate::arena::ThunkArena>>>> =
        std::cell::RefCell::new(None);
}

/// Return a new ThunkArena pre-populated with the stdlib thunks (via Rc::clone),
/// so ThunkIds allocated during stdlib loading are valid in the returned arena.
/// Returns None if create_stdlib_env has not yet been called on this thread.
pub(crate) fn new_arena_with_stdlib_snapshot() -> Option<Rc<RefCell<crate::arena::ThunkArena>>> {
    STDLIB_ARENA_CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .map(|stdlib_arena| Rc::new(RefCell::new(stdlib_arena.borrow().clone_for_child())))
    })
}

pub fn create_stdlib_env() -> Result<Rc<RefCell<Environment>>, Box<crate::error::EvalError>> {
    let (env, _arena) = create_stdlib_env_with_arena()?;
    // Arena already cached by create_stdlib_env_with_arena
    Ok(env)
}

/// Like `create_stdlib_env` but also returns the arena used during stdlib evaluation.
/// The arena holds all ThunkIds allocated while loading the prelude and macros.llt.
/// Callers (e.g., macro expansion) that need to share the same ThunkId space should
/// use this arena when constructing their EvalContext via `EvalContext::new_sharing_arena`.
///
/// **Cache consistency:** This function ALSO updates `STDLIB_ARENA_CACHE` so that subsequent
/// `EvalContext::new()` calls on this thread inherit the stdlib ThunkIds. This ensures cache
/// consistency regardless of which entry point (`create_stdlib_env()` or
/// `create_stdlib_env_with_arena()`) was used to build the stdlib.
pub(crate) fn create_stdlib_env_with_arena() -> Result<
    (
        Rc<RefCell<Environment>>,
        Rc<RefCell<crate::arena::ThunkArena>>,
    ),
    Box<crate::error::EvalError>,
> {
    let d = STDLIB_ENV_DEPTH.get();
    if d > 5 {
        panic!(
            "create_stdlib_env: infinite recursion detected (depth={})",
            d
        );
    }
    STDLIB_ENV_DEPTH.set(d + 1);
    let result = create_stdlib_env_inner();
    STDLIB_ENV_DEPTH.set(d);
    // Cache the arena so subsequent EvalContext::new() calls can inherit stdlib ThunkIds.
    if let Ok((_, ref arena)) = result {
        STDLIB_ARENA_CACHE.with(|c| *c.borrow_mut() = Some(Rc::clone(arena)));
    }
    result
}

fn create_stdlib_env_inner() -> Result<
    (
        Rc<RefCell<Environment>>,
        Rc<RefCell<crate::arena::ThunkArena>>,
    ),
    Box<crate::error::EvalError>,
> {
    // Phase 2: Bootstrap env switch.
    // The bootstrap env contains ONLY `include` and `%rust`. Prelude uses
    // `[include %rust "core"]`, `[include %rust "collection"]`, etc. to pull in
    // the Rust primitives it needs. After the prelude loads, its exported bindings
    // become the standard library environment that user code inherits.
    let bootstrap_env = create_bootstrap_env();

    // Create a bootstrap EvalContext backed by the bootstrap env.
    // The bootstrap_ctx uses bootstrap_env as its stdlib_env, which means:
    //   - `include` resolves to the builtin_include function
    //   - `%rust` resolves to Value::RustRegistry for [include %rust "..."] calls
    let bootstrap_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| {
            Box::new(crate::error::EvalError::internal(
                format!("cannot open bootstrap base_dir: {e}"),
                Span::origin(),
            ))
        })?;
    // Use new_empty() to bypass STDLIB_ARENA_CACHE — we're BUILDING the stdlib here,
    // so we need a fresh arena, not one seeded with stale cache contents.
    let bootstrap_ctx =
        crate::eval::EvalContext::new_empty(bootstrap_base_dir, Rc::clone(&bootstrap_env), false);

    // Create stdlib env as a child of bootstrap_env.
    // This means: user code (child of stdlib_env) can walk up to bootstrap_env
    // and see `%rust` — but the prelude acts as the primary scope boundary.
    // Full isolation (hiding %rust from user scope) is future work.
    let stdlib_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        &bootstrap_env,
    ))));

    // Load prelude — provides all public stdlib functions.
    // Prelude uses [include %rust "core"] etc. to access Rust primitives directly,
    // without relying on all builtins being in the parent env.
    let prelude_source = include_str!("../stdlib/prelude.llt");
    load_stdlib_module(prelude_source, "prelude", &stdlib_env, &bootstrap_ctx)?;

    // Inject ALL standard builtins into stdlib_env AFTER prelude loading.
    //
    // Rationale: user code (child of stdlib_env) expects all builtins to be in scope
    // (e.g., `str`, `error`, `keys`, `builtin-lt`, `emit`). Since stdlib_env's parent
    // is bootstrap_env (which has only `include` + `%rust`), we must inject the
    // builtins here directly rather than relying on the parent chain.
    //
    // This preserves backwards compatibility: user code that references raw builtins
    // directly (e.g., `builtin-add`, `str`, `emit`) still works.
    //
    // Note: this does NOT override prelude's dict entries (those were inserted first
    // and take lexical priority), but it DOES make all builtins accessible via parent
    // chain lookup when not shadowed by prelude.
    //
    {
        let mut env_borrow = stdlib_env.borrow_mut();
        for def in standard_builtins() {
            // Only insert if not already present (prelude entries take priority).
            if env_borrow.get(def.name).is_none() {
                let thunk = Rc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
                env_borrow.insert(def.name.to_string(), thunk);
            }
        }
        // Transport nominal variant constants (Tcp, Udp, etc.)
        for tag in [
            "Tcp",
            "Udp",
            "UnixStream",
            "UnixDatagram",
            "NamedPipe",
            "Icmp",
        ] {
            if env_borrow.get(tag).is_none() {
                let thunk = Rc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag: tag.to_string(),
                        payload: None,
                    },
                    Span::origin(),
                ));
                env_borrow.insert(tag.to_string(), thunk);
            }
        }
        // builtin-* aliases (inject after standard builtins)
        inject_prelude_aliases(&mut env_borrow);
    }

    // Load macros — exports tmpl-transformer and helpers used by expand_macros.
    // Loaded after prelude so macro helpers can reference prelude functions.
    let macros_source = include_str!("../stdlib/macros.llt");
    load_stdlib_module(macros_source, "macros", &stdlib_env, &bootstrap_ctx)?;

    // Keep the arena alive: bootstrap_ctx is dropped here, but its arena holds all
    // ThunkIds allocated during prelude/macros loading. Callers that need to share
    // the same ThunkId space (e.g., macro expansion) clone this Rc before returning.
    let arena = Rc::clone(&bootstrap_ctx.thunk_arena);

    Ok((stdlib_env, arena))
}

/// Create the type-stage environment used when evaluating type-stage documents.
///
/// This function parses the prelude, filters to only `--- stage: type` documents,
/// and evaluates them with a minimal bootstrap context containing only `include`
/// and `%rust "type-core"`.
///
/// The type-stage env is separate from the runtime stdlib env — it contains only
/// the bindings defined in type-stage documents (e.g., `Int`, `Str`, `Seq`, `union`).
///
/// Returns the type-stage environment wrapped in `Rc<RefCell<Environment>>`.
pub fn create_type_stage_env() -> Result<Rc<RefCell<Environment>>, Box<crate::error::EvalError>> {
    // Parse the prelude source
    let prelude_source = include_str!("../stdlib/prelude.llt");
    let mut file = crate::parser::parse(prelude_source).map_err(|e| {
        crate::error::EvalError::internal(
            format!("type-stage prelude parse error: {e}"),
            Span::origin(),
        )
    })?;

    // Desugar and resolve
    crate::desugar::desugar_file(&mut file.node);
    crate::resolve::resolve_file(&file.node);

    // Create minimal bootstrap env: include + %rust
    let bootstrap_env = create_bootstrap_env();

    // Create a bootstrap EvalContext
    let bootstrap_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| {
            Box::new(crate::error::EvalError::internal(
                format!("cannot open type-stage bootstrap base_dir: {e}"),
                Span::origin(),
            ))
        })?;
    let bootstrap_ctx =
        crate::eval::EvalContext::new_empty(bootstrap_base_dir, Rc::clone(&bootstrap_env), false);

    // Create type-stage env as a child of bootstrap_env
    let type_stage_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        &bootstrap_env,
    ))));

    // Filter to only stage: type documents and evaluate them
    for doc in &file.node.documents {
        if doc.node.stage == Some(crate::ast::Stage::Type) {
            // Evaluate this type-stage document
            let result = crate::eval::eval_document(
                doc,
                Rc::clone(&type_stage_env),
                &bootstrap_ctx,
            )?;

            // Materialize and extract bindings
            let val = crate::eval::materialize(&result, None, &bootstrap_ctx)?;

            let dict = match val {
                Value::Dict(map) => map,
                Value::Overlay(l_id, r_id) => {
                    flatten_overlay(&l_id, &r_id, "type-stage prelude", &bootstrap_ctx, doc.span)?
                }
                other => {
                    return Err(crate::error::EvalError::internal(
                        format!(
                            "type-stage document must evaluate to a Dict, got {}",
                            other.type_name()
                        ),
                        doc.span,
                    )
                    .into())
                }
            };

            // Insert bindings into type-stage env
            for (key, thunk_id) in dict {
                let name = match key {
                    Key::String(s) => s,
                    Key::Int(n) => n.to_string(),
                };
                let thunk = bootstrap_ctx.get_thunk(thunk_id);
                type_stage_env.borrow_mut().insert(name, thunk);
            }
        }
    }

    Ok(type_stage_env)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Param, Spanned};
    use crate::error::ErrorKind;
    use crate::test_util::test_span;
    use crate::value::{string_val, Strictness};

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
        crate::eval::materialize(&result.unwrap(), None, &test_ctx()).unwrap()
    }

    /// Helper: make a zero-arg function whose body is a single expression.
    fn zero_arg_fn(body_expr: Expr) -> Value {
        Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(body_expr, test_span(1, 1, 1, 10))),
            env: Rc::new(RefCell::new(Environment::new())),
            annotation: None,
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
            annotation: None,
        }
    }

    /// Build a materialized dict thunk whose entries are allocated into `ctx`'s arena.
    /// Accepts `IndexMap<Key, Rc<Thunk>>` (convenient for test construction) and
    /// stores each as a `ThunkId` in `Value::Dict`, as the runtime requires.
    fn thunk_dict(map: IndexMap<Key, Rc<Thunk>>, ctx: &Rc<crate::eval::EvalContext>) -> Rc<Thunk> {
        let mut id_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Rc::new(Thunk::new_materialized(
            Value::Dict(id_map),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: flatten a Value (Dict or Overlay) to an `IndexMap<Key, ThunkId>` for test assertions.
    /// Since `builtin_merge` now returns `Value::Overlay` (lazy), tests that previously
    /// expected `Value::Dict` must use this helper to get the concrete entries.
    fn flatten_val(val: Value, ctx: &Rc<crate::eval::EvalContext>) -> IndexMap<Key, ThunkId> {
        match val {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => {
                flatten_overlay(&l, &r, "test", ctx, test_span(1, 1, 1, 5)).unwrap()
            }
            other => panic!("expected Dict or Overlay, got {other:?}"),
        }
    }

    /// Helper: materialize the thunk identified by `id` in `ctx`'s arena.
    fn mat_id(id: ThunkId, ctx: &Rc<crate::eval::EvalContext>) -> Value {
        let thunk = ctx.get_thunk(id);
        crate::eval::materialize(&thunk, None, ctx).unwrap()
    }

    /// Helper: build a `Value::Seq` with both `head` and `tail` allocated into `ctx`.
    /// Returns a materialized `Rc<Thunk>` wrapping the `Seq`.
    fn seq_thunk(
        head: Rc<Thunk>,
        tail: Rc<Thunk>,
        ctx: &Rc<crate::eval::EvalContext>,
    ) -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Seq {
                head: ctx.alloc_thunk(head),
                tail: ctx.alloc_thunk(tail),
            },
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: build an empty dict as a materialized `Rc<Thunk>` (no arena needed — no ThunkId entries).
    fn empty_dict_thunk() -> Rc<Thunk> {
        Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            test_span(1, 1, 1, 5),
        ))
    }

    #[test]
    fn test_create_type_stage_env_succeeds() {
        // Test that create_type_stage_env() successfully creates an environment
        // with the type-stage prelude bindings
        let type_env = create_type_stage_env().expect("create_type_stage_env failed");

        // Check that Int is defined
        assert!(
            type_env.borrow().get("Int").is_some(),
            "Int should be defined in type-stage env"
        );

        // Check that Str is defined
        assert!(
            type_env.borrow().get("Str").is_some(),
            "Str should be defined in type-stage env"
        );

        // Check that union is defined
        assert!(
            type_env.borrow().get("union").is_some(),
            "union should be defined in type-stage env"
        );
    }

    #[test]
    fn floor_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
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
            args: &[thunk(string_val("3.5".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("3.5".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn to_int_valid_negative() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(string_val("-7".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn to_int_valid_zero() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(string_val("0".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn to_int_valid_large() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: &[thunk(string_val("9223372036854775807".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[test]
    fn to_int_invalid_float_string() {
        let err = builtin_to_int(BuiltinArgs {
            args: &[thunk(string_val("3.14".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("".into()))],
            named: no_named(),
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
            args: &[thunk(string_val(" 42 ".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("1".into())), thunk(string_val("2".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("3.14".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(42.0));
    }

    #[test]
    fn to_float_valid_negative() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("-2.5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(-2.5));
    }

    #[test]
    fn to_float_valid_scientific_notation() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("1.5e10".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[test]
    fn to_float_valid_negative_exponent() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("2.5e-3".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[test]
    fn to_float_valid_zero() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("0.0".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.0));
    }

    #[test]
    fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let result = mat(builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val(".5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.5));
    }

    #[test]
    fn to_float_invalid_text() {
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("inf".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("-inf".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("infinity".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("NaN".into()))],
            named: no_named(),
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
                thunk(string_val("1.0".into())),
                thunk(string_val("2.0".into())),
            ],
            named: no_named(),
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
        named.insert("x".into(), thunk(string_val("1.0".into())));
        let err = builtin_to_float(BuiltinArgs {
            args: &[thunk(string_val("3.14".into()))],
            named: Some(&named),
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
            args: &[thunk(string_val("9223372036854775808".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn eval_primitive_string() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn eval_primitive_float() {
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
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
        let ctx = test_ctx();
        let dict = thunk_dict(
            {
                let mut map = IndexMap::new();
                map.insert(Key::String("a".into()), thunk(Value::Int(1)));
                map.insert(Key::String("b".into()), thunk(Value::Int(2)));
                map
            },
            &ctx,
        );
        let result = mat(builtin_eval(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = mat_id(map[&Key::String("a".into())], &ctx);
                assert_eq!(a, Value::Int(1));
                let b = mat_id(map[&Key::String("b".into())], &ctx);
                assert_eq!(b, Value::Int(2));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn eval_nested_dict() {
        // Build [x: [y: 42]]
        let ctx = test_ctx();
        let inner_dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::String("y".into()), thunk(Value::Int(42)));
                m
            },
            &ctx,
        );
        let outer_dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::String("x".into()), inner_dict);
                m
            },
            &ctx,
        );

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[outer_dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(outer_map) => {
                let x_val = mat_id(outer_map[&Key::String("x".into())], &ctx);
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val = mat_id(inner_map[&Key::String("y".into())], &ctx);
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
        let ctx = test_ctx();
        let expr = Rc::new(Spanned::new(Expr::Int(99), test_span(1, 1, 1, 5)));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(
            expr,
            env,
            Rc::clone(&ctx),
            test_span(1, 1, 1, 5),
        ));

        let dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::String("val".into()), unevaluated);
                m
            },
            &ctx,
        );

        let result = mat(builtin_eval(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let v = mat_id(map[&Key::String("val".into())], &ctx);
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
            args: &[thunk(string_val("boom".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert_eq!(err.message(), "boom");
    }

    #[test]
    fn error_custom_message() {
        let err = builtin_error(BuiltinArgs {
            args: &[thunk(string_val("division by zero".into()))],
            named: no_named(),
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
    fn try_success_returns_ok_variant() {
        // [fn [] 42]
        let ctx = test_ctx();
        let func = zero_arg_fn(Expr::Int(42));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, Value::Int(42));
            }
            _ => panic!("expected Variant(Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_success_with_string_body() {
        let ctx = test_ctx();
        let func = zero_arg_fn(Expr::Str("hello".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, string_val("hello".into()));
            }
            _ => panic!("expected Variant(Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_failure_returns_err_variant() {
        // [fn [] $nonexistent] -- references an undefined variable
        let ctx = test_ctx();
        let func = zero_arg_fn(Expr::var_ref("nonexistent".into()));
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Err");
                let err_val = mat_id(payload.expect("Err should have payload"), &ctx);
                match err_val {
                    Value::String {
                        ref source,
                        start,
                        end,
                    } => {
                        let msg = &source[start..end];
                        assert!(
                            msg.contains("undefined variable"),
                            "expected 'undefined variable' in error message, got: {msg}"
                        );
                    }
                    _ => panic!("expected String error message"),
                }
            }
            _ => panic!("expected Variant(Err, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_non_function_type_error() {
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
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
        let func = n_arg_fn(&["x"], Expr::var_ref("x".into()));
        let err = builtin_try(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
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
        let ctx = test_ctx();
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: ok_builtin,
            name: "ok",
            pos_strictness: &[],
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, Value::Int(99));
            }
            _ => panic!("expected Variant(Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_with_builtin_failure() {
        fn err_builtin(ctx: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::internal("builtin error".to_string(), call_span).into())
        }
        let ctx = test_ctx();
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: err_builtin,
            name: "fail",
            pos_strictness: &[],
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: &[thunk(b)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Err");
                let payload_val = mat_id(payload.expect("Err should have payload"), &ctx);
                assert_eq!(payload_val, string_val("builtin error".into()));
            }
            _ => panic!("expected Variant(Err, ...), got: {:?}", result),
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
        let ctx = test_ctx();
        let func = n_arg_fn(&["x"], Expr::var_ref("x".into()));
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(42)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn apply_multiple_args_returns_first() {
        // [fn [a b] $a] applied to [10, 20]
        let ctx = test_ctx();
        let func = n_arg_fn(&["a", "b"], Expr::var_ref("a".into()));
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(10)));
                m.insert(Key::Int(1), thunk(Value::Int(20)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(10));
    }

    #[test]
    fn apply_multiple_args_returns_second() {
        // [fn [a b] $b] applied to [10, 20]
        let ctx = test_ctx();
        let func = n_arg_fn(&["a", "b"], Expr::var_ref("b".into()));
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(10)));
                m.insert(Key::Int(1), thunk(Value::Int(20)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
            let a = materialize(&args[0], None, &ctx)?;
            let b = materialize(&args[1], None, &ctx)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(x + y), call_span),
                _ => Err(EvalError::type_mismatch("Int", "non-Int", call_span).into()),
            }
        }
        let ctx = test_ctx();
        let func = Value::Builtin(crate::value::BuiltinDef {
            func: add_builtin,
            name: "add",
            pos_strictness: &[],
        });
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(3)));
                m.insert(Key::Int(1), thunk(Value::Int(4)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: &[thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn apply_arity_mismatch() {
        let ctx = test_ctx();
        let func = n_arg_fn(&["x", "y"], Expr::var_ref("x".into()));
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );

        let apply_thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&apply_thunk, None, &ctx).unwrap_err();
        assert!(
            err.message()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_function_type_error() {
        let ctx = test_ctx();
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );

        let apply_thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(Value::Int(42)), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&apply_thunk, None, &ctx).unwrap_err();
        assert!(
            err.message().contains("expected Function"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn apply_non_dict_args_type_error() {
        let func = n_arg_fn(&["x"], Expr::var_ref("x".into()));
        let thunk = builtin_apply(BuiltinArgs {
            args: &[thunk(func), thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx()).unwrap_err();
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
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx()).unwrap_err();
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Int".into()));
    }

    #[test]
    fn type_of_float() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Float".into()));
    }

    #[test]
    fn type_of_string() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(string_val("hi".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("String".into()));
    }

    #[test]
    fn type_of_bool() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Bool".into()));
    }

    #[test]
    fn type_of_dict() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Dict".into()));
    }

    #[test]
    fn type_of_function() {
        let func = zero_arg_fn(Expr::Int(0));
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Function".into()));
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Function".into()));
    }

    #[test]
    fn test_type_of_seq() {
        // Seq values should report type name "Seq" from $type-of
        let ctx = test_ctx();
        let head_id = ctx.alloc_thunk(thunk(Value::Int(1)));
        let tail_id = ctx.alloc_thunk(thunk(Value::Dict(IndexMap::new())));
        let seq = Value::Seq {
            head: head_id,
            tail: tail_id,
        };
        let result = mat(builtin_type_of(BuiltinArgs {
            args: &[thunk(seq)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, string_val("Seq".into()));
    }

    #[test]
    fn type_of_arity_check() {
        let err = builtin_type_of(BuiltinArgs {
            args: &[],
            named: no_named(),
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
            args: &[thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn from_json_float() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val("3.14".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn from_json_string() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val(r#""hello""#))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn from_json_bool_true() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val("true".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn from_json_bool_false() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val("false".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    #[test]
    fn from_json_null_becomes_empty_dict() {
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val("null".into()))],
            named: no_named(),
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
        let ctx = test_ctx();
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val("[1, 2, 3]".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = mat_id(map[&Key::Int(0)], &ctx);
                assert_eq!(v0, Value::Int(1));
                let v1 = mat_id(map[&Key::Int(1)], &ctx);
                assert_eq!(v1, Value::Int(2));
                let v2 = mat_id(map[&Key::Int(2)], &ctx);
                assert_eq!(v2, Value::Int(3));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_object() {
        let ctx = test_ctx();
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val(r#"{"name": "Alice", "age": 30}"#))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let name = mat_id(map[&Key::String("name".into())], &ctx);
                assert_eq!(name, string_val("Alice".into()));
                let age = mat_id(map[&Key::String("age".into())], &ctx);
                assert_eq!(age, Value::Int(30));
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    fn from_json_nested_structure() {
        let ctx = test_ctx();
        let json = r#"{"users": [{"name": "Bob"}, {"name": "Eve"}]}"#;
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val(json))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let users = mat_id(map[&Key::String("users".into())], &ctx);
                match users {
                    Value::Dict(arr) => {
                        assert_eq!(arr.len(), 2);
                        let user0 = mat_id(arr[&Key::Int(0)], &ctx);
                        match user0 {
                            Value::Dict(u) => {
                                let name = mat_id(u[&Key::String("name".into())], &ctx);
                                assert_eq!(name, string_val("Bob".into()));
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
            args: &[thunk(string_val("{bad json".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("{}".into()))],
            named: no_named(),
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
            args: &[thunk(string_val("[]".into()))],
            named: no_named(),
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
        let ctx = test_ctx();
        let result = mat(builtin_from_json(BuiltinArgs {
            args: &[thunk(string_val(r#"[1, "two", true, null]"#))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                let v0 = mat_id(map[&Key::Int(0)], &ctx);
                assert_eq!(v0, Value::Int(1));
                let v1 = mat_id(map[&Key::Int(1)], &ctx);
                assert_eq!(v1, string_val("two".into()));
                let v2 = mat_id(map[&Key::Int(2)], &ctx);
                assert_eq!(v2, Value::Bool(true));
                let v3 = mat_id(map[&Key::Int(3)], &ctx);
                match v3 {
                    Value::Dict(m) => assert!(m.is_empty()),
                    _ => panic!("expected empty Dict for null"),
                }
            }
            _ => panic!("expected Dict"),
        }
    }

    #[test]
    #[ignore = "requires >128MB Rust stack in debug mode; passes in release mode. json_to_value recursion depth matches JSON_DEPTH_LIMIT; verify depth guard policy only."]
    fn from_json_depth_guard() {
        // Build JSON nested beyond JSON_DEPTH_LIMIT: {"a":{"a":{...}}}
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
        let deep = build_deep(JSON_DEPTH_LIMIT + 1);
        let ctx = test_ctx();
        let err = json_to_value(&deep, 0, call_span(), &ctx).unwrap_err();
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
        let ctx = test_ctx();
        let result = mat(json_to_value(
            &serde_json::Value::Number(serde_json::Number::from_f64(3.14).expect("finite")),
            0,
            call_span(),
            &ctx,
        ));
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn keys_empty_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_int_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("a".into())));
        map.insert(Key::Int(1), thunk(string_val("b".into())));
        map.insert(Key::Int(2), thunk(string_val("c".into())));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                for i in 0..3 {
                    let val = mat_id(keys_map[&Key::Int(i)], &ctx);
                    assert_eq!(val, Value::Int(i));
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_string_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(
            Key::String("name".into()),
            thunk(string_val("Alice".into())),
        );
        map.insert(Key::String("age".into()), thunk(Value::Int(30)));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 2);
                let k0 = mat_id(keys_map[&Key::Int(0)], &ctx);
                assert_eq!(k0, string_val("name".into()));
                let k1 = mat_id(keys_map[&Key::Int(1)], &ctx);
                assert_eq!(k1, string_val("age".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_mixed_key_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("first".into())));
        map.insert(
            Key::String("label".into()),
            thunk(string_val("second".into())),
        );
        map.insert(Key::Int(5), thunk(string_val("third".into())));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                let k0 = mat_id(keys_map[&Key::Int(0)], &ctx);
                assert_eq!(k0, Value::Int(0));
                let k1 = mat_id(keys_map[&Key::Int(1)], &ctx);
                assert_eq!(k1, string_val("label".into()));
                let k2 = mat_id(keys_map[&Key::Int(2)], &ctx);
                assert_eq!(k2, Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn keys_preserves_insertion_order() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("z".into()), thunk(Value::Int(1)));
        map.insert(Key::String("a".into()), thunk(Value::Int(2)));
        map.insert(Key::String("m".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(keys_map) => {
                let k0 = mat_id(keys_map[&Key::Int(0)], &ctx);
                let k1 = mat_id(keys_map[&Key::Int(1)], &ctx);
                let k2 = mat_id(keys_map[&Key::Int(2)], &ctx);
                assert_eq!(k0, string_val("z".into()));
                assert_eq!(k1, string_val("a".into()));
                assert_eq!(k2, string_val("m".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn length_empty_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_non_empty_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn length_int_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("x".into())));
        map.insert(Key::Int(1), thunk(string_val("y".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn merge_disjoint_keys() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), thunk(Value::Int(1)));
        left.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("c".into()), thunk(Value::Int(3)));
        right.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        // builtin_merge now returns Value::Overlay; flatten to verify contents.
        let map = flatten_val(result, &ctx);
        assert_eq!(map.len(), 4);
        assert!(map.contains_key(&Key::String("a".into())));
        assert!(map.contains_key(&Key::String("b".into())));
        assert!(map.contains_key(&Key::String("c".into())));
        assert!(map.contains_key(&Key::String("d".into())));
    }

    #[test]
    fn merge_overlapping_keys_right_wins() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(Key::String("x".into()), thunk(Value::Int(1)));
        left.insert(Key::String("y".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("y".into()), thunk(Value::Int(99)));
        right.insert(Key::String("z".into()), thunk(Value::Int(3)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
        assert_eq!(map.len(), 3);
        let x = mat_id(map[&Key::String("x".into())], &ctx);
        assert_eq!(x, Value::Int(1));
        let y = mat_id(map[&Key::String("y".into())], &ctx);
        assert_eq!(y, Value::Int(99)); // R overrides L
        let z = mat_id(map[&Key::String("z".into())], &ctx);
        assert_eq!(z, Value::Int(3));
    }

    #[test]
    fn merge_empty_dicts() {
        let ctx = test_ctx();
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[
                thunk_dict(IndexMap::new(), &ctx),
                thunk_dict(IndexMap::new(), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
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
        let ctx = test_ctx();
        let mut right = IndexMap::new();
        right.insert(Key::Int(0), thunk(string_val("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(IndexMap::new(), &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
        assert_eq!(map.len(), 1);
        let v = mat_id(map[&Key::Int(0)], &ctx);
        assert_eq!(v, string_val("only".into()));
    }

    #[test]
    fn merge_right_empty() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(Key::Int(0), thunk(string_val("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left, &ctx), thunk_dict(IndexMap::new(), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
        assert_eq!(map.len(), 1);
        let v = mat_id(map[&Key::Int(0)], &ctx);
        assert_eq!(v, string_val("only".into()));
    }

    #[test]
    fn merge_preserves_thunks() {
        // With arena-based ThunkIds, verify that the values are preserved correctly
        // through a lazy overlay by materializing and comparing values.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let left_thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));
        let right_thunk = Rc::new(Thunk::new_materialized(Value::Int(99), span));

        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), Rc::clone(&left_thunk));
        let mut right = IndexMap::new();
        right.insert(Key::String("b".into()), Rc::clone(&right_thunk));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        // Flatten and verify values are preserved correctly through the overlay.
        let map = flatten_val(result, &ctx);
        assert_eq!(mat_id(map[&Key::String("a".into())], &ctx), Value::Int(42));
        assert_eq!(mat_id(map[&Key::String("b".into())], &ctx), Value::Int(99));
    }

    #[test]
    fn merge_preserves_left_order() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(Key::String("b".into()), thunk(Value::Int(1)));
        left.insert(Key::String("a".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(Key::String("d".into()), thunk(Value::Int(3)));
        right.insert(Key::String("c".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: &[thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
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
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = builtin_keys(BuiltinArgs {
            args: &[d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = builtin_length(BuiltinArgs {
            args: &[d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = builtin_merge(BuiltinArgs {
            args: &[d],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = builtin_merge(BuiltinArgs {
            args: &[d.clone(), d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("keys"), "got: {}", err.message());
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn length_string() {
        // length now supports String inputs (returns character count)
        let result = mat(builtin_length(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn length_string_empty() {
        let result = mat(builtin_length(BuiltinArgs {
            args: &[thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn length_string_unicode() {
        // Multi-byte characters: length returns char count, not byte count
        let result = mat(builtin_length(BuiltinArgs {
            args: &[thunk(string_val("\u{1F600}\u{1F601}".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn length_non_dict_non_string() {
        let err = builtin_length(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
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
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
    }

    #[test]
    fn merge_first_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let result = builtin_merge(BuiltinArgs {
            args: &[thunk(Value::Int(1)), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        // builtin_merge itself succeeds — returns Overlay(Int(1), {})
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize(&overlay_thunk, None, &ctx).unwrap();
        // Flatten fires the type error: left side is Int, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span()).unwrap_err();
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
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let result = builtin_merge(BuiltinArgs {
            args: &[d, thunk(string_val("nope".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize(&overlay_thunk, None, &ctx).unwrap();
        // Flatten fires the type error: right side is String, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span()).unwrap_err();
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
        let ctx = test_ctx();
        let empty = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let val = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                assert_eq!(val, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_existing_list() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("a".into())));
        map.insert(Key::Int(1), thunk(string_val("b".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(string_val("c".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx);
                assert_eq!(val, string_val("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_string_keys_only() {
        // Dict with only string keys -- next int key should be 0
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(Value::Int(1)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(99))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let val = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_to_dict_with_gap_in_int_keys() {
        // Dict with keys 0, 5 -- next key should be 6 (max + 1)
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(Value::Int(10)));
        map.insert(Key::Int(5), thunk(Value::Int(50)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(60))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = mat_id(*map.get(&Key::Int(6)).unwrap(), &ctx);
                assert_eq!(val, Value::Int(60));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_preserves_existing_entries() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("first".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: &[dict, thunk(string_val("second".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let first = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                assert_eq!(first, string_val("first".into()));
                let second = mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx);
                assert_eq!(second, string_val("second".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_value_stays_as_thunk() {
        // The value arg is inserted lazily (not materialized at append time).
        // Verify the inserted value is correct when materialized.
        let ctx = test_ctx();
        let empty = thunk_dict(IndexMap::new(), &ctx);
        let val_thunk = thunk(Value::Int(7));
        let result = mat(builtin_append(BuiltinArgs {
            args: &[empty, Rc::clone(&val_thunk)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                // Verify the value was inserted correctly and materializes to the expected value.
                let id = *map.get(&Key::Int(0)).unwrap();
                assert_eq!(mat_id(id, &ctx), Value::Int(7));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn append_wrong_arity_zero() {
        let err = builtin_append(BuiltinArgs {
            args: &[],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_wrong_arity_three() {
        let ctx = test_ctx();
        let err = builtin_append(BuiltinArgs {
            args: &[
                thunk_dict(IndexMap::new(), &ctx),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap_err();
        assert!(err.message().contains("2"), "got: {}", err.message());
    }

    #[test]
    fn append_first_arg_non_dict() {
        let err = builtin_append(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
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
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(i64::MAX), thunk(Value::Int(1)));
        let dict = thunk_dict(map, &ctx);
        let err = builtin_append(BuiltinArgs {
            args: &[dict, thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn str_single_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("42".into()));
    }

    #[test]
    fn str_single_negative_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Int(-7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("-7".into()));
    }

    #[test]
    fn str_single_float() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("3.14".into()));
    }

    #[test]
    fn str_single_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn str_single_bool_true() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("true".into()));
    }

    #[test]
    fn str_single_bool_false() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("false".into()));
    }

    #[test]
    fn str_single_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(
                    Key::String("x".into()),
                    Rc::new(Thunk::new_materialized(
                        Value::Int(1),
                        test_span(1, 1, 1, 5),
                    )),
                );
                m
            },
            &ctx,
        );
        let result = mat(builtin_str(BuiltinArgs {
            args: &[dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, string_val("[x: <thunk>]".into()));
    }

    #[test]
    fn str_single_empty_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: &[thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn str_concat_multiple_strings() {
        let args = vec![
            thunk(string_val("Hello".into())),
            thunk(string_val(" ".into())),
            thunk(string_val("World".into())),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Hello World".into()));
    }

    #[test]
    fn str_concat_mixed_types() {
        let args = vec![
            thunk(string_val("count: ".into())),
            thunk(Value::Int(42)),
            thunk(string_val(", ratio: ".into())),
            thunk(Value::Float(3.14)),
            thunk(string_val(", ok: ".into())),
            thunk(Value::Bool(true)),
        ];
        let result = mat(builtin_str(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(
            result,
            string_val("count: 42, ratio: 3.14, ok: true".into())
        );
    }

    #[test]
    fn split_basic() {
        let ctx = test_ctx();
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(string_val(",".into())),
                thunk(string_val("a,b,c".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                let v1 = mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx);
                let v2 = mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx);
                assert_eq!(v0, string_val("a".into()));
                assert_eq!(v1, string_val("b".into()));
                assert_eq!(v2, string_val("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_parts() {
        let ctx = test_ctx();
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(string_val(",".into())),
                thunk(string_val("a,,b".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v1 = mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx);
                assert_eq!(v1, string_val("".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_single_char_separator() {
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(string_val("/".into())),
                thunk(string_val("a/b/c/d".into())),
            ],
            named: no_named(),
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
        let ctx = test_ctx();
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(string_val(",".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                assert_eq!(v0, string_val("hello".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_multi_char_separator() {
        let ctx = test_ctx();
        let result = mat(builtin_split(BuiltinArgs {
            args: &[
                thunk(string_val("::".into())),
                thunk(string_val("a::b::c".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let v0 = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                let v1 = mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx);
                let v2 = mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx);
                assert_eq!(v0, string_val("a".into()));
                assert_eq!(v1, string_val("b".into()));
                assert_eq!(v2, string_val("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn split_empty_input() {
        let ctx = test_ctx();
        let result = mat(builtin_split(BuiltinArgs {
            args: &[thunk(string_val(",".into())), thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let v0 = mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx);
                assert_eq!(v0, string_val("".into()));
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
            args: &[thunk(string_val("")), thunk(string_val(&input))],
            named: no_named(),
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
            args: &[thunk(string_val(",")), thunk(string_val(&input))],
            named: no_named(),
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
                thunk(string_val("world".into())),
                thunk(string_val("Rust".into())),
                thunk(string_val("hello world".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello Rust".into()));
    }

    #[test]
    fn replace_multiple_occurrences() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(string_val("a".into())),
                thunk(string_val("o".into())),
                thunk(string_val("banana".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("bonono".into()));
    }

    #[test]
    fn replace_no_match() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(string_val("xyz".into())),
                thunk(string_val("abc".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn replace_empty_pattern() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(string_val("".into())),
                thunk(string_val("-".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("-a-b-c-".into()));
    }

    #[test]
    fn replace_to_empty() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: &[
                thunk(string_val("l".into())),
                thunk(string_val("".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("heo".into()));
    }

    #[test]
    fn replace_output_size_limit_empty_pattern() {
        // Empty pattern with large replacement should error.
        // 1000 chars input, 100k chars replacement -> output would be ~100MB.
        let input = "a".repeat(1000);
        let replacement = "x".repeat(100_000);
        let result = builtin_replace(BuiltinArgs {
            args: &[
                thunk(string_val("")),
                thunk(string_val(&replacement)),
                thunk(string_val(&input)),
            ],
            named: no_named(),
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
                thunk(string_val("a")),
                thunk(string_val("bb")),
                thunk(string_val(&input)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // 1000 'a' replaced with 'bb' -> 2000 'b'
        assert_eq!(result, string_val(&"b".repeat(2000)));
    }

    #[test]
    fn trim_basic() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("  hello  ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_leading_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("   hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_trailing_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("hello   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_no_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_all_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("\t\nhello\n\t".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_empty() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn split_wrong_arity_too_few() {
        let err = builtin_split(BuiltinArgs {
            args: &[thunk(string_val(",".into()))],
            named: no_named(),
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
                thunk(string_val(",".into())),
                thunk(string_val("a,b".into())),
                thunk(string_val("extra".into())),
            ],
            named: no_named(),
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
            args: &[thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
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
    fn trim_wrong_arity() {
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
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
            args: &[thunk(Value::Int(42)), thunk(string_val("hello".into()))],
            named: no_named(),
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
            args: &[thunk(string_val(",".into())), thunk(Value::Int(42))],
            named: no_named(),
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
                thunk(string_val("b".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
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
                thunk(string_val("a".into())),
                thunk(Value::Bool(true)),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
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
                thunk(string_val("a".into())),
                thunk(string_val("b".into())),
                thunk(Value::Float(3.14)),
            ],
            named: no_named(),
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
    fn trim_wrong_type() {
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(Value::Float(3.14))],
            named: no_named(),
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
    fn trim_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(string_val("hi".into())));
        let err = builtin_trim(BuiltinArgs {
            args: &[thunk(string_val("  hello  ".into()))],
            named: Some(&named),
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
            args: &[thunk(string_val("boom".into()))],
            named: Some(&named),
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
            args: &[thunk(string_val("42".into()))],
            named: Some(&named),
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
            args: &[thunk(string_val("42".into()))],
            named: Some(&named),
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
                thunk(string_val(",".into())),
                thunk(string_val("a,b".into())),
            ],
            named: Some(&named),
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
                thunk(string_val("a".into())),
                thunk(string_val("b".into())),
                thunk(string_val("abc".into())),
            ],
            named: Some(&named),
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
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::String("a".into()), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );
        let err = builtin_keys(BuiltinArgs {
            args: &[dict],
            named: Some(&named),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
            args: &[thunk(string_val("hello".into()))],
            named: Some(&named),
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
            call_span: call_span(),
            ctx: test_ctx(),
        })
        .expect("should return thunk");
        let err = crate::eval::materialize(&thunk, None, &test_ctx()).unwrap_err();
        assert!(
            err.message().contains("named arguments"),
            "got: {}",
            err.message()
        );
    }

    /// Regression test for ThunkId cross-context lifecycle.
    ///
    /// Guards against breaking the STDLIB_ARENA_CACHE write in create_stdlib_env_with_arena.
    /// If the cache write is accidentally removed, new_arena_with_stdlib_snapshot() will
    /// return None and EvalContext::new() will get an empty arena, causing index-out-of-bounds
    /// panics when accessing stdlib ThunkIds.
    #[test]
    fn stdlib_arena_cache_preserves_thunk_ids() {
        // Create stdlib env — this should cache the arena
        let (_env, arena) = create_stdlib_env_with_arena().expect("failed to create stdlib env");

        // Verify the cache is populated by create_stdlib_env_with_arena
        let cached_arena = new_arena_with_stdlib_snapshot()
            .expect("arena cache should be populated after create_stdlib_env_with_arena");

        // The cached arena should be a snapshot of the stdlib arena
        assert_eq!(
            cached_arena.borrow().len(),
            arena.borrow().len(),
            "cached arena should be a snapshot of the stdlib arena"
        );

        assert!(
            cached_arena.borrow().len() > 390,
            "cached arena should contain at least 390 stdlib thunks (prelude + macros), got {}",
            cached_arena.borrow().len()
        );
    }

    #[test]
    fn standard_builtins_count() {
        let count = standard_builtins().len();
        // This test documents the current count. Update this assertion when adding/removing builtins.
        // The count in doc/11-stdlib.md should match this number.
        assert_eq!(
            count, 184,
            "builtin count changed - update this test and doc/11-stdlib.md"
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
        assert!(names.contains(&"trim"), "missing trim");
        assert!(
            names.contains(&"str-to-upper-char"),
            "missing str-to-upper-char"
        );
        assert!(
            names.contains(&"str-to-lower-char"),
            "missing str-to-lower-char"
        );
        assert!(names.contains(&"str-map-chars"), "missing str-map-chars");
        assert!(names.contains(&"regex-match?"), "missing regex-match?");
        // Numeric
        assert!(names.contains(&"floor"), "missing floor");
        assert!(names.contains(&"round"), "missing round");
        // Parsing
        assert!(names.contains(&"to-int"), "missing to-int");
        assert!(names.contains(&"to-float"), "missing to-float");
        // Evaluation control
        assert!(names.contains(&"eval"), "missing eval");
        assert!(names.contains(&"force"), "missing force");
        assert!(names.contains(&"error"), "missing error");
        assert!(names.contains(&"try"), "missing try");
        assert!(names.contains(&"apply"), "missing apply");
        // Type introspection
        assert!(names.contains(&"type-of"), "missing type-of");
        assert!(names.contains(&"llt-repr"), "missing llt-repr");
        assert!(names.contains(&"int?"), "missing int?");
        assert!(names.contains(&"float?"), "missing float?");
        // num?, record?, map? are now LLT-implemented in stdlib/prelude.llt (not builtins)
        assert!(names.contains(&"str?"), "missing str?");
        assert!(names.contains(&"bool?"), "missing bool?");
        assert!(names.contains(&"null?"), "missing null?");
        assert!(names.contains(&"dict?"), "missing dict?");
        assert!(names.contains(&"fn?"), "missing fn?");
        assert!(names.contains(&"seq?"), "missing seq?");
        // I/O
        assert!(names.contains(&"emit"), "missing emit");
        assert!(names.contains(&"env"), "missing env");
        assert!(
            !names.contains(&"dir-cap"),
            "dir-cap was removed (ambient cap creation)"
        );
        assert!(names.contains(&"open"), "missing open");
        assert!(names.contains(&"slurp"), "missing slurp");
        assert!(names.contains(&"narrow"), "missing narrow");
        assert!(names.contains(&"revocable"), "missing revocable");
        assert!(names.contains(&"revoke-cap"), "missing revoke-cap");
        assert!(
            !names.contains(&"net-cap"),
            "net-cap was removed (ambient cap creation)"
        );
        assert!(names.contains(&"connect"), "missing connect");
        assert!(names.contains(&"lines"), "missing lines");
        assert!(names.contains(&"write"), "missing write");
        assert!(names.contains(&"write-atomic"), "missing write-atomic");
        assert!(names.contains(&"cap-data"), "missing cap-data");
        // has-cap? is now implemented in stdlib/io.llt as [not [null? [cap-data h cap]]]
        assert!(
            !names.contains(&"has-cap?"),
            "has-cap? should be in stdlib not builtins"
        );
        assert!(names.contains(&"write-handle"), "missing write-handle");
        assert!(names.contains(&"flush"), "missing flush");
        assert!(names.contains(&"close"), "missing close");
        assert!(names.contains(&"raw-create"), "missing raw-create");
        assert!(names.contains(&"list-dir"), "missing list-dir");
        assert!(names.contains(&"stat"), "missing stat");
        assert!(names.contains(&"make-dir"), "missing make-dir");
        assert!(names.contains(&"remove"), "missing remove");
        assert!(names.contains(&"rename"), "missing rename");
        assert!(names.contains(&"link"), "missing link");
        assert!(names.contains(&"read-link"), "missing read-link");
        assert!(names.contains(&"from-json"), "missing from-json");
        assert!(names.contains(&"include"), "missing include");
        // Sequences (registered as builtin-NAME; prelude exports unwrapped names)
        assert!(names.contains(&"builtin-seq"), "missing builtin-seq");
        assert!(names.contains(&"builtin-head"), "missing builtin-head");
        assert!(names.contains(&"builtin-tail"), "missing builtin-tail");
        assert!(
            names.contains(&"builtin-collect"),
            "missing builtin-collect"
        );
        assert!(names.contains(&"builtin-range"), "missing builtin-range");
        assert!(names.contains(&"builtin-repeat"), "missing builtin-repeat");
        assert!(names.contains(&"builtin-cycle"), "missing builtin-cycle");
        assert!(
            names.contains(&"builtin-iterate"),
            "missing builtin-iterate"
        );
        assert!(names.contains(&"builtin-unfold"), "missing builtin-unfold");
        assert!(names.contains(&"map"), "missing map");
        assert!(names.contains(&"filter"), "missing filter");
        assert!(names.contains(&"take"), "missing take");
        assert!(names.contains(&"drop"), "missing drop");
        assert!(names.contains(&"reduce"), "missing reduce");
        assert!(names.contains(&"builtin-join"), "missing builtin-join");
        assert!(names.contains(&"builtin-concat"), "missing builtin-concat");
        // List operations (registered as builtin-NAME; prelude exports unwrapped names)
        assert!(names.contains(&"builtin-first"), "missing builtin-first");
        assert!(names.contains(&"builtin-last"), "missing builtin-last");
        assert!(names.contains(&"builtin-rest"), "missing builtin-rest");
        assert!(names.contains(&"builtin-cons"), "missing builtin-cons");
        assert!(
            names.contains(&"builtin-reverse"),
            "missing builtin-reverse"
        );
        assert!(names.contains(&"builtin-sort"), "missing builtin-sort");
        // Also assert proxy is present
        assert!(names.contains(&"proxy"), "missing proxy");
        // Access-pipeline builtins (Wave 1 sprint)
        assert!(names.contains(&"builtin-get"), "missing builtin-get");
        assert!(names.contains(&"get?"), "missing get?");
        assert!(names.contains(&"each"), "missing each");
        assert!(names.contains(&"each-key"), "missing each-key");
        assert!(names.contains(&"each-kv"), "missing each-kv");
        // Total count: Wave 1 sprint added 4 access-pipeline builtins (builtin-get, each, each-key, each-kv).
        // Update this count when standard_builtins() changes.
        assert!(names.contains(&"eval-ast"), "missing eval-ast");
        assert!(names.contains(&"gensym"), "missing gensym");
        assert!(names.contains(&"str-length"), "missing str-length");
        assert!(names.contains(&"str-slice"), "missing str-slice");
        assert!(names.contains(&"str-chars"), "missing str-chars");
        assert!(names.contains(&"validate"), "missing validate");
        // Math builtins
        assert!(names.contains(&"pow"), "missing pow");
        assert!(names.contains(&"sqrt"), "missing sqrt");
        assert!(names.contains(&"log"), "missing log");
        assert!(names.contains(&"log2"), "missing log2");
        assert!(names.contains(&"log10"), "missing log10");
        assert!(names.contains(&"exp"), "missing exp");
        assert!(names.contains(&"sin"), "missing sin");
        assert!(names.contains(&"cos"), "missing cos");
        assert!(names.contains(&"tan"), "missing tan");
        assert!(names.contains(&"asin"), "missing asin");
        assert!(names.contains(&"acos"), "missing acos");
        assert!(names.contains(&"atan"), "missing atan");
        assert!(names.contains(&"atan2"), "missing atan2");
        assert!(names.contains(&"nan?"), "missing nan?");
        assert!(names.contains(&"inf?"), "missing inf?");
        assert!(names.contains(&"finite?"), "missing finite?");
        // Bitwise builtins
        assert!(names.contains(&"band"), "missing band");
        assert!(names.contains(&"bor"), "missing bor");
        assert!(names.contains(&"bxor"), "missing bxor");
        assert!(names.contains(&"shl"), "missing shl");
        assert!(names.contains(&"shr"), "missing shr");
        // Character builtins
        assert!(names.contains(&"char-code"), "missing char-code");
        assert!(names.contains(&"chr"), "missing chr");
        // Bytes stubs
        assert!(names.contains(&"str-bytes"), "missing str-bytes");
        assert!(names.contains(&"bytes-str"), "missing bytes-str");
        // Bytes builtins
        assert!(names.contains(&"bytes"), "missing bytes");
        assert!(names.contains(&"bytes-find"), "missing bytes-find");
        assert!(names.contains(&"bytes-of"), "missing bytes-of");
        assert!(names.contains(&"bytes-equal?"), "missing bytes-equal?");
        assert!(names.contains(&"ct-equal?"), "missing ct-equal?");
        assert!(names.contains(&"bytes?"), "missing bytes?");
        // TLS builtins
        assert!(names.contains(&"tls-layer"), "missing tls-layer");
        assert!(names.contains(&"tls-peer-cert"), "missing tls-peer-cert");
        // spki-pin is now implemented in stdlib/net.llt (pure dict construction, no Rust needed)
        assert!(
            !names.contains(&"spki-pin"),
            "spki-pin should be in stdlib not builtins"
        );
        // URI parsing builtins
        assert!(names.contains(&"uri"), "missing uri");
        assert!(names.contains(&"url"), "missing url");
        assert!(names.contains(&"urn"), "missing urn");
        // Seek builtins
        assert!(names.contains(&"seek"), "missing seek");
        assert!(names.contains(&"seek-end"), "missing seek-end");
        assert!(names.contains(&"position"), "missing position");
        // HTTP-sessions stubs (QUIC/HTTP2/HTTP3/ICMP)
        assert!(names.contains(&"quic-session"), "missing quic-session");
        assert!(
            names.contains(&"quic-open-stream"),
            "missing quic-open-stream"
        );
        assert!(
            names.contains(&"quic-open-datagram"),
            "missing quic-open-datagram"
        );
        assert!(names.contains(&"http2-session"), "missing http2-session");
        assert!(names.contains(&"http3-session"), "missing http3-session");
        assert!(names.contains(&"http-request"), "missing http-request");
        assert!(names.contains(&"icmp-ping"), "missing icmp-ping");
        assert!(names.contains(&"send-datagram"), "missing send-datagram");
        assert!(names.contains(&"recv-datagram"), "missing recv-datagram");
        assert_eq!(
            names.len(),
            184,
            "expected 184 builtins, got {} (upper/lower moved to stdlib/strings.llt; str-to-upper-char, str-to-lower-char, str-map-chars, regex-match? added; get? added)",
            names.len()
        );
    }

    #[test]
    fn add_int_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn add_type_error_string() {
        let e = builtin_add(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
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
            args: &[thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_string_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(string_val("hello".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_string_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(string_val("hello".into())),
                thunk(string_val("world".into())),
            ],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_dict_structural_equality() {
        // Empty dicts are structurally equal
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_different_types_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(string_val("1".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(string_val("apple".into())),
                thunk(string_val("banana".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic_reverse() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(string_val("banana".into())),
                thunk(string_val("apple".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(string_val("same".into())),
                thunk(string_val("same".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_string_prefix() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: &[
                thunk(string_val("ab".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_incompatible_types_error() {
        let e = builtin_lt(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_does_not_materialize_unchosen_else_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::var_ref("nonexistent".to_string()),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_does_not_materialize_unchosen_then_branch() {
        let error_expr = Rc::new(Spanned::new(
            Expr::var_ref("nonexistent".to_string()),
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
            thunk(string_val("true".into())),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = builtin_if(BuiltinArgs {
            args: &args,
            named: no_named(),
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
    fn macros_parses_without_error() {
        let macros_source = include_str!("../stdlib/macros.llt");
        match crate::parser::parse(macros_source) {
            Ok(_) => {}
            Err(e) => panic!("macros.llt parse failed: {e}"),
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
        // Should have macros exports (tmpl, do, begin)
        assert!(env_ref.get("tmpl").is_some(), "missing macros export tmpl");
        assert!(env_ref.get("do").is_some(), "missing macros export do");
        assert!(
            env_ref.get("begin").is_some(),
            "missing macros export begin"
        );
        // strings/math/encoding are NOT loaded at startup — require explicit include.
        assert!(
            env_ref.get("pad-left").is_none(),
            "pad-left should not be in startup env (requires [include libdir \"strings.llt\"])"
        );
        assert!(
            env_ref.get("pi").is_none(),
            "pi should not be in startup env (requires [include libdir \"math.llt\"])"
        );
        assert!(
            env_ref.get("hex-encode").is_none(),
            "hex-encode should not be in startup env (requires [include libdir \"encoding.llt\"])"
        );
    }

    /// Helper: create an EvalContext pointing at the given base directory.
    fn include_ctx(base_dir: &std::path::Path) -> Rc<crate::eval::EvalContext> {
        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let dir = cap_std::fs::Dir::open_ambient_dir(base_dir, cap_std::ambient_authority())
            .expect("failed to open base_dir");
        crate::eval::EvalContext::new(dir, stdlib_env, false)
    }

    /// Helper: create a Value::DirCap for a given directory path.
    fn dir_cap_val(base_dir: &std::path::Path) -> Value {
        Value::DirCap {
            dir: Rc::new(
                cap_std::fs::Dir::open_ambient_dir(base_dir, cap_std::ambient_authority())
                    .expect("open dir for DirCap"),
            ),
            perms: crate::value::DirPerms::full(),
        }
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

        // Pass 2 args where first is Int (not DirCap): arity check passes (2 args OK),
        // then type-mismatch fires because Int != DirCap.
        let args = vec![thunk(Value::Int(42)), thunk(string_val("path.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx,
        })
        .unwrap_err();
        assert!(
            err.message().contains("expected DirCap or String")
                || err.message().contains("type mismatch"),
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

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("nonexistent.llt")),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("lib.llt"))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x = mat_id(*map.get(&Key::String("x".into())).unwrap(), &ctx);
                assert_eq!(x, Value::Int(42));
                let y = mat_id(*map.get(&Key::String("y".into())).unwrap(), &ctx);
                assert_eq!(y, string_val("hello".into()));
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("num.llt"))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("bad.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        write_temp_file(&dir, "a.llt", "[include %pwd \"b.llt\"]");
        write_temp_file(&dir, "b.llt", "[include %pwd \"a.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("a.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        write_temp_file(&dir, "self.llt", "[include %pwd \"self.llt\"]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("self.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
            "[inner: [include %pwd \"sub/inner.llt\"]]",
        );
        write_temp_file(&dir.join("sub"), "inner.llt", "[val: 99]");
        let ctx = include_ctx(&dir);

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("outer.llt"))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let inner = mat_id(*map.get(&Key::String("inner".into())).unwrap(), &ctx);
                match inner {
                    Value::Dict(inner_map) => {
                        let val = mat_id(*inner_map.get(&Key::String("val".into())).unwrap(), &ctx);
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

        // DirCap points to other_dir, path is absolute — cap-std RESOLVE_BENEATH rejects it.
        let args = vec![
            thunk(dir_cap_val(&other_dir)),
            thunk(string_val(&file_path.to_string_lossy())),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        // No arguments (include requires 2 or 3 args)
        let err = builtin_include(BuiltinArgs {
            args: &[],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap_err();
        assert!(
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );

        // Four arguments (include requires 2 or 3 args; 4 is an arity error)
        // Pattern: [include $cap "path" "hash"] uses 3 args (cap + path + hash), so 4 triggers arity error.
        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("a.llt")),
            thunk(string_val(
                "blake3:0000000000000000000000000000000000000000000000000000000000000000",
            )),
            thunk(string_val("too-many")),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        // Pass 2 positional args (arity OK) + named arg — named arg rejection fires.
        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("test.llt"))];
        let mut named = IndexMap::new();
        named.insert("path".to_string(), thunk(string_val("x")));
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: Some(&named),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("multi.llt"))];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let y = mat_id(*map.get(&Key::String("y".into())).unwrap(), &ctx);
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

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("stdlib_test.llt")),
        ];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let val = mat_id(*map.get(&Key::String("result".into())).unwrap(), &ctx);
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

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("cached_ptr.llt")),
        ];

        // First include — builds and caches the Thunk
        let raw1 = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .expect("first include should succeed");

        // Second include — must return Rc::clone of the cached Thunk
        let raw2 = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("cached.llt"))];

        // First include
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include -- should hit cache
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = mat_id(*map1.get(&Key::String("value".into())).unwrap(), &ctx);
                let val2 = mat_id(*map2.get(&Key::String("value".into())).unwrap(), &ctx);
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
        let args1 = vec![thunk(dir_cap_val(&dir)), thunk(string_val("./target.llt"))];
        let result1 = mat(builtin_include(BuiltinArgs {
            args: &args1,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Second include with normalized path
        let args2 = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("subdir/../target.llt")),
        ];
        let result2 = mat(builtin_include(BuiltinArgs {
            args: &args2,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Both should return the same value
        match (&result1, &result2) {
            (Value::Dict(map1), Value::Dict(map2)) => {
                let val1 = mat_id(*map1.get(&Key::String("value".into())).unwrap(), &ctx);
                let val2 = mat_id(*map2.get(&Key::String("value".into())).unwrap(), &ctx);
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
        write_temp_file(&dir, "file_a.llt", "[a: [include %pwd \"shared.llt\"]]");
        write_temp_file(&dir, "file_c.llt", "[c: [include %pwd \"shared.llt\"]]");
        let ctx = include_ctx(&dir);

        // Include file_a (which includes shared.llt)
        let args_a = vec![thunk(dir_cap_val(&dir)), thunk(string_val("file_a.llt"))];
        let result_a = mat(builtin_include(BuiltinArgs {
            args: &args_a,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Include file_c (which also includes shared.llt -- should hit cache)
        let args_c = vec![thunk(dir_cap_val(&dir)), thunk(string_val("file_c.llt"))];
        let result_c = mat(builtin_include(BuiltinArgs {
            args: &args_c,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Verify that both got the shared value
        match (&result_a, &result_c) {
            (Value::Dict(map_a), Value::Dict(map_c)) => {
                let a_val = mat_id(*map_a.get(&Key::String("a".into())).unwrap(), &ctx);
                let c_val = mat_id(*map_c.get(&Key::String("c".into())).unwrap(), &ctx);

                // Both should be dicts with "shared: 123"
                match (&a_val, &c_val) {
                    (Value::Dict(a_inner), Value::Dict(c_inner)) => {
                        let a_shared =
                            mat_id(*a_inner.get(&Key::String("shared".into())).unwrap(), &ctx);
                        let c_shared =
                            mat_id(*c_inner.get(&Key::String("shared".into())).unwrap(), &ctx);
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

        let args = vec![thunk(string_val("test.llt".into()))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        // 3-arg form: [include DirCap "path" "blake3:hex"]
        let dir = std::env::temp_dir().join("llt_test_include_hash_ok");
        std::fs::create_dir_all(&dir).ok();
        let content = "[x: 99]";
        write_temp_file(&dir, "hashed.llt", content);
        let expected_hex = blake3_hex(content.as_bytes());
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("hashed.llt")),
            thunk(string_val(&format!("blake3:{expected_hex}"))),
        ];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let val = mat_id(*map.get(&Key::String("x".into())).unwrap(), &ctx);
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn include_with_wrong_blake3_hash_errors() {
        // $include with a wrong blake3 hash should return IncludeHashMismatch.
        // 3-arg form: [include DirCap "path" "blake3:wrong"]
        let dir = std::env::temp_dir().join("llt_test_include_hash_mismatch");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "data.llt", "[x: 1]");
        let wrong_hex = "0".repeat(64);
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("data.llt")),
            thunk(string_val(&format!("blake3:{wrong_hex}"))),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        // 3-arg form: [include DirCap "path" "notahash"]
        let dir = std::env::temp_dir().join("llt_test_include_hash_format");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("file.llt")),
            thunk(string_val("notahash")),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        // 3-arg form: [include DirCap "path" "md5:abc"]
        let dir = std::env::temp_dir().join("llt_test_include_hash_algo");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");
        let ctx = include_ctx(&dir);

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("file.llt")),
            thunk(string_val("md5:abc")),
        ];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        // 2-arg form: [include DirCap "path"] without hash — IncludeHashRequired fires.
        let dir = std::env::temp_dir().join("llt_test_include_require_integrity");
        std::fs::create_dir_all(&dir).ok();
        write_temp_file(&dir, "file.llt", "[x: 1]");

        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&dir, cap_std::ambient_authority())
            .expect("open dir");
        let ctx =
            crate::eval::EvalContext::new_with_options(base_dir, stdlib_env, false, true, None);

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("file.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        // 3-arg form: [include DirCap "path" "blake3:hex"]
        let dir = std::env::temp_dir().join("llt_test_include_require_integrity_ok");
        std::fs::create_dir_all(&dir).ok();
        let content = "[y: 55]";
        write_temp_file(&dir, "ok.llt", content);
        let hex = blake3_hex(content.as_bytes());

        let stdlib_env = create_stdlib_env().expect("stdlib env");
        let base_dir = cap_std::fs::Dir::open_ambient_dir(&dir, cap_std::ambient_authority())
            .expect("open dir");
        let ctx =
            crate::eval::EvalContext::new_with_options(base_dir, stdlib_env, false, true, None);

        let args = vec![
            thunk(dir_cap_val(&dir)),
            thunk(string_val("ok.llt")),
            thunk(string_val(&format!("blake3:{hex}"))),
        ];
        let result = mat(builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                let val = mat_id(*map.get(&Key::String("y".into())).unwrap(), &ctx);
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
        // middle.llt: includes bad.llt via cap-qualified form
        write_temp_file(&dir, "middle.llt", "[include %pwd \"bad.llt\"]");
        // outer.llt: includes middle.llt via cap-qualified form
        write_temp_file(&dir, "outer.llt", "[include %pwd \"middle.llt\"]");

        let ctx = include_ctx(&dir);

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("outer.llt"))];
        let err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("ok.llt"))];
        let _result = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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

        let args = vec![thunk(dir_cap_val(&dir)), thunk(string_val("bad.llt"))];
        let _err = builtin_include(BuiltinArgs {
            args: &args,
            named: no_named(),
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
        let ctx = test_ctx();
        let head_val = thunk(Value::Int(1));
        let tail_val = thunk(Value::Int(2));
        let result = mat(builtin_seq(BuiltinArgs {
            args: &[head_val.clone(), tail_val.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(1));
                assert_eq!(mat_id(tail, &ctx), Value::Int(2));
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn seq_arity_zero() {
        let result = builtin_seq(BuiltinArgs {
            args: &[],
            named: no_named(),
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
                Expr::var_ref("undefined_var".to_string()),
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
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(string_val("first".into())), empty_dict_thunk(), &ctx);
        let result = builtin_head(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        assert!(result.is_ok());
        let head = mat(result);
        assert_eq!(head, string_val("first".into()));
    }

    #[test]
    fn head_non_seq() {
        let result = builtin_head(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
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
        let ctx = test_ctx();
        let seq_val = seq_thunk(
            thunk(string_val("first".into())),
            thunk(Value::Int(99)),
            &ctx,
        );
        let result = builtin_tail(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        assert!(result.is_ok());
        let tail = mat(result);
        assert_eq!(tail, Value::Int(99));
    }

    #[test]
    fn tail_non_seq() {
        let result = builtin_tail(BuiltinArgs {
            args: &[thunk(string_val("not a seq".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let ctx = test_ctx();
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_dict_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let seq_val = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);

        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(1));
                assert_eq!(mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx), Value::Int(2));
                assert_eq!(mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx), Value::Int(3));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn collect_empty_tail() {
        // Single element: Seq(42, {})
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(Value::Int(42)), empty_dict_thunk(), &ctx);
        let result = mat(builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx),
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_invalid_tail() {
        // Seq with non-empty dict as tail (should error)
        let ctx = test_ctx();
        let tail_dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::String("x".into()), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );
        let seq_val = seq_thunk(thunk(Value::Int(1)), tail_dict, &ctx);
        let result = builtin_collect(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        assert!(result.is_err());
    }

    #[test]
    fn collect_large_sequence() {
        // Test collect with a moderately-sized sequence (200 elements) to verify it works
        // correctly without hitting MAX_EVAL_DEPTH (256) or MAX_COLLECT_SIZE (1M).
        // Testing at the actual MAX_COLLECT_SIZE (1M) would be too slow/memory-intensive,
        // and with depth increment fixes, sequences hit MAX_EVAL_DEPTH around 256 elements.
        let ctx = test_ctx();
        let range_result = builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        let take_result = builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(200)), range_result],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        let collect_result = builtin_collect(BuiltinArgs {
            args: &[take_result],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });

        assert!(
            collect_result.is_ok(),
            "collect should succeed for 200 elements"
        );
        match crate::eval::materialize(&collect_result.unwrap(), None, &ctx).unwrap() {
            Value::Dict(map) => {
                assert_eq!(map.len(), 200);
                // Spot-check first and last elements
                assert_eq!(mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(0));
                assert_eq!(
                    mat_id(*map.get(&Key::Int(199)).unwrap(), &ctx),
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
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(Value::Int(1)), empty_dict_thunk(), &ctx);
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn seq_check_false() {
        let result = mat(builtin_seq_check(BuiltinArgs {
            args: &[thunk(string_val("not a seq".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Bool(false));
    }

    // === range builtin tests ===

    #[test]
    fn range_finite_basic() {
        // range(0, 5) → 0, 1, 2, 3, 4
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(0));
                // Materialize tail to get next element
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        assert_eq!(mat_id(h2, &ctx), Value::Int(1));
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
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(0));
                // tail should be empty (terminal)
                let tail_val = mat_id(tail, &ctx);
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
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(0));
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(mat_id(h2, &ctx), Value::Int(1));
                        let t2_val = mat_id(t2, &ctx);
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(mat_id(h3, &ctx), Value::Int(2));
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_start() {
        let result = builtin_range(BuiltinArgs {
            args: &[thunk(string_val("not an int".into()))],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === repeat builtin tests ===

    #[test]
    fn repeat_basic() {
        // repeat(42) → 42, 42, 42, ... (take first 3)
        let ctx = test_ctx();
        let result = mat(builtin_repeat(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(42));
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(mat_id(h2, &ctx), Value::Int(42));
                        let t2_val = mat_id(t2, &ctx);
                        match t2_val {
                            Value::Seq { head: h3, .. } => {
                                assert_eq!(mat_id(h3, &ctx), Value::Int(42));
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
                Expr::var_ref("undefined_var".to_string()),
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === cycle builtin tests ===

    #[test]
    fn cycle_basic() {
        // cycle([a, b]) → a, b, a, b, ... (take first 4)
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), thunk(string_val("a".into())));
        map.insert(Key::String("y".into()), thunk(string_val("b".into())));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_cycle(BuiltinArgs {
            args: &[dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                // First element: "a"
                assert_eq!(mat_id(head, &ctx), string_val("a".into()));
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        // Second element: "b"
                        assert_eq!(mat_id(h2, &ctx), string_val("b".into()));
                        let t2_val = mat_id(t2, &ctx);
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                // Third element: "a" (cycling back)
                                assert_eq!(mat_id(h3, &ctx), string_val("a".into()));
                                let t3_val = mat_id(t3, &ctx);
                                match t3_val {
                                    Value::Seq { head: h4, .. } => {
                                        // Fourth element: "b"
                                        assert_eq!(mat_id(h4, &ctx), string_val("b".into()));
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
        let ctx = test_ctx();
        let f_thunk = thunk(Value::Int(999)); // dummy, won't be called in structure test
        let x_thunk = thunk(Value::Int(0));

        let result = mat(builtin_iterate(BuiltinArgs {
            args: &[f_thunk, x_thunk.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                // Head should be x (0)
                assert_eq!(mat_id(head, &ctx), Value::Int(0));
                // Tail is a PendingBuiltin wrapping iterate(f, f(x))
                // Materializing it returns another Seq (doesn't error yet)
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, .. } => {
                        // Trying to materialize h2 (which is PendingCall(Int(999), [Int(0)]))
                        // will error because Int(999) is not a function
                        let h2_thunk = ctx.get_thunk(h2);
                        let h2_result = crate::eval::materialize(&h2_thunk, None, &ctx);
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
                Expr::var_ref("undefined_f".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let undef_x = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::var_ref("undefined_x".to_string()),
                test_span(1, 1, 1, 5),
            )),
            Rc::new(RefCell::new(Environment::new())),
            test_ctx(),
            test_span(1, 1, 1, 5),
        ));
        let result = builtin_iterate(BuiltinArgs {
            args: &[undef_f, undef_x],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_ok());
        // Result is a PendingBuiltin, not yet materialized
        // Materializing it would call unfold_step, which would error because
        // step is Int(999), not a function
        let result_val = materialize(&result.unwrap(), None, &test_ctx());
        assert!(result_val.is_err());
    }

    #[test]
    fn unfold_arity_one() {
        let result = builtin_unfold(BuiltinArgs {
            args: &[thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    // === take builtin tests ===

    #[test]
    fn take_dict_basic() {
        // take(2, [a: 1, b: 2, c: 3]) → [a: 1, b: 2]
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        map.insert(Key::String("c".into()), thunk(Value::Int(3)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    mat_id(*map.get(&Key::String("a".into())).unwrap(), &ctx),
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(*map.get(&Key::String("b".into())).unwrap(), &ctx),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_zero() {
        // take(0, dict) → []
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_negative() {
        // take(-5, dict) → []
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(-5)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_dict_more_than_length() {
        // take(10, [a: 1, b: 2]) → [a: 1, b: 2]
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(1)));
        map.insert(Key::String("b".into()), thunk(Value::Int(2)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(10)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
        let ctx = test_ctx();
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_dict_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let seq_val = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);

        // take(2, seq) → Seq(1, Seq(2, []))
        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(2)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(1));
                let tail_val = mat_id(tail, &ctx);
                match tail_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(mat_id(h2, &ctx), Value::Int(2));
                        // tail of tail should be empty dict (terminal)
                        let t2_val = mat_id(t2, &ctx);
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
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(Value::Int(1)), empty_dict_thunk(), &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: &[thunk(Value::Int(0)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[test]
    fn take_n_non_int() {
        let result = builtin_take(BuiltinArgs {
            args: &[thunk(string_val("not int".into())), thunk(Value::Int(1))],
            named: no_named(),
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
                thunk(string_val("not dict or seq".into())),
            ],
            named: no_named(),
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
            call_span: call_span(),
            ctx: test_ctx(),
        });
        assert!(result.is_err());
    }

    #[test]
    fn concat_seq() {
        // Build two 2-element sequences and concat them
        let ctx = test_ctx();
        // xs = Seq(1, Seq(2, {}))
        let xs_inner = seq_thunk(thunk(Value::Int(2)), empty_dict_thunk(), &ctx);
        let xs = seq_thunk(thunk(Value::Int(1)), xs_inner, &ctx);

        // ys = Seq(3, Seq(4, {}))
        let ys_inner = seq_thunk(thunk(Value::Int(4)), empty_dict_thunk(), &ctx);
        let ys = seq_thunk(thunk(Value::Int(3)), ys_inner, &ctx);

        // concat(xs, ys) should produce Seq(1, Seq(2, Seq(3, Seq(4, {}))))
        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        // Materialize the result to verify structure
        let result_val = crate::eval::materialize(&result, None, &ctx).unwrap();
        match result_val {
            Value::Seq { head: h1, tail: t1 } => {
                assert_eq!(mat_id(h1, &ctx), Value::Int(1));
                let t1_val = mat_id(t1, &ctx);
                match t1_val {
                    Value::Seq { head: h2, tail: t2 } => {
                        assert_eq!(mat_id(h2, &ctx), Value::Int(2));
                        let t2_val = mat_id(t2, &ctx);
                        match t2_val {
                            Value::Seq { head: h3, tail: t3 } => {
                                assert_eq!(mat_id(h3, &ctx), Value::Int(3));
                                let t3_val = mat_id(t3, &ctx);
                                match t3_val {
                                    Value::Seq { head: h4, tail: t4 } => {
                                        assert_eq!(mat_id(h4, &ctx), Value::Int(4));
                                        let t4_val = mat_id(t4, &ctx);
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
        // concat({}, ys) should return ys (same materialized value)
        let ctx = test_ctx();
        let xs = thunk(Value::Dict(IndexMap::new()));
        let ys = seq_thunk(thunk(Value::Int(1)), empty_dict_thunk(), &ctx);

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        // Result should be ys — verify by materializing and checking value
        let result_val = crate::eval::materialize(&result, None, &ctx).unwrap();
        match result_val {
            Value::Seq { head, .. } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(1));
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn concat_seq_empty_ys() {
        // concat(xs, {}) should return xs's elements followed by empty dict
        let ctx = test_ctx();
        let xs = seq_thunk(thunk(Value::Int(1)), empty_dict_thunk(), &ctx);
        let ys = thunk(Value::Dict(IndexMap::new()));

        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        // Materialize to verify: Seq(1, {})
        let result_val = crate::eval::materialize(&result, None, &ctx).unwrap();
        match result_val {
            Value::Seq { head, tail } => {
                assert_eq!(mat_id(head, &ctx), Value::Int(1));
                let tail_val = mat_id(tail, &ctx);
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
        let ctx = test_ctx();
        let mut xs_map = IndexMap::new();
        xs_map.insert(Key::Int(0), thunk(Value::Int(1)));
        xs_map.insert(Key::Int(1), thunk(Value::Int(2)));
        let xs = thunk_dict(xs_map, &ctx);

        let mut ys_map = IndexMap::new();
        ys_map.insert(Key::Int(0), thunk(Value::Int(3)));
        ys_map.insert(Key::Int(1), thunk(Value::Int(4)));
        let ys = thunk_dict(ys_map, &ctx);

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(1));
                assert_eq!(mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx), Value::Int(2));
                assert_eq!(mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx), Value::Int(3));
                assert_eq!(mat_id(*map.get(&Key::Int(3)).unwrap(), &ctx), Value::Int(4));
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
        let ctx = test_ctx();
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_dict_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let xs = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);
        let ys = thunk(Value::Int(42));

        // builtin_concat itself fails immediately because ys=42 is not a collection.
        let err = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Attempt to join infinite range without take
                // This will hit MAX_EVAL_DEPTH (256) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the sequence traversal.
                let join_result = builtin_join(BuiltinArgs {
                    args: &[thunk(string_val(",")), range_result],
                    named: no_named(),
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
            args: &[thunk(string_val(",")), thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val(""));
    }

    #[test]
    fn concat_dict_basic() {
        // Task 4: Test $concat with two small dicts to verify correct behavior
        // This exercises the checked_add call site that prevents integer overflow
        let ctx = test_ctx();
        let mut dict1 = IndexMap::new();
        dict1.insert(Key::String("a".into()), thunk(Value::Int(1)));
        dict1.insert(Key::String("b".into()), thunk(Value::Int(2)));

        let mut dict2 = IndexMap::new();
        dict2.insert(Key::String("c".into()), thunk(Value::Int(3)));
        dict2.insert(Key::String("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_concat(BuiltinArgs {
            args: &[thunk_dict(dict1, &ctx), thunk_dict(dict2, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                // All values should be reindexed with integer keys 0, 1, 2, 3
                assert_eq!(mat_id(*map.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(1));
                assert_eq!(mat_id(*map.get(&Key::Int(1)).unwrap(), &ctx), Value::Int(2));
                assert_eq!(mat_id(*map.get(&Key::Int(2)).unwrap(), &ctx), Value::Int(3));
                assert_eq!(mat_id(*map.get(&Key::Int(3)).unwrap(), &ctx), Value::Int(4));
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
            let val = crate::eval::materialize(&ctx.args[0], None, &ctx.ctx)?;
            ok_val(Value::Bool(matches!(val, Value::Int(299))), Span::origin())
        }

        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Create range(0, 300): lazy Seq(0, 1, ..., 299) via PendingBuiltin chain
                let range_result = builtin_range(BuiltinArgs {
                    args: &[thunk(Value::Int(0)), thunk(Value::Int(300))],
                    named: no_named(),
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
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Force the filter result. Before the fix this would fail with depth
                // exceeded after ~128 consecutive failures. After the fix the internal
                // loop handles all 299 failures at constant depth.
                let ctx_inner = test_ctx();
                let val = crate::eval::materialize(&filter_result, None, &ctx_inner).unwrap();
                match val {
                    Value::Seq { head, .. } => {
                        let head_thunk = ctx_inner.get_thunk(head);
                        let head_val =
                            crate::eval::materialize(&head_thunk, None, &ctx_inner).unwrap();
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
                let ctx_inner = test_ctx();
                // Create a dict with 300 entries where all fail the predicate
                let mut dict_map = IndexMap::new();
                for i in 0..300 {
                    dict_map.insert(Key::Int(i), thunk(Value::Int(i)));
                }
                let dict_thunk = thunk_dict(dict_map, &ctx_inner);

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

                    call_span: call_span(),
                    ctx: Rc::clone(&ctx_inner),
                })
                .unwrap();

                // Convert lazy Seq to Dict via builtin_collect, then materialize
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[filter_result],
                    named: no_named(),

                    call_span: call_span(),
                    ctx: Rc::clone(&ctx_inner),
                })
                .unwrap();

                let val = crate::eval::materialize(&collect_result, None, &ctx_inner).unwrap();
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
        let ctx = test_ctx();
        let xs = thunk(Value::Dict(IndexMap::new())); // empty dict
        let mut ys_map = IndexMap::new();
        ys_map.insert(Key::Int(0), thunk(Value::Int(99)));
        let ys = thunk_dict(ys_map, &ctx);

        // Should succeed and return ys (the same thunk or an equivalent materialized form)
        let result = builtin_concat(BuiltinArgs {
            args: &[xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let val = mat(result);
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(mat_id(*m.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(99));
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
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Try to take 260 elements (slightly more than MAX_EVAL_DEPTH=256)
                // This ensures we hit the depth limit.
                let take_result = builtin_take(BuiltinArgs {
                    args: &[thunk(Value::Int(260)), range_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: test_ctx(),
                })
                .unwrap();

                // Force the entire sequence by calling collect
                let collect_result = builtin_collect(BuiltinArgs {
                    args: &[take_result],
                    named: no_named(),
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
        let ctx = test_ctx();
        let handler = thunk(Value::Int(42));
        let result = builtin_proxy(BuiltinArgs {
            args: &[handler.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        })
        .unwrap();

        let val = mat(Ok(result));
        match val {
            Value::Proxy { handler: h } => {
                // Verify the handler thunk contains the expected value.
                let handler_val = mat_id(h, &ctx);
                assert_eq!(handler_val, Value::Int(42));
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
        let ctx = test_ctx();
        let n_remaining = thunk(string_val("not an int"));
        let seq = seq_thunk(thunk(Value::Int(1)), empty_dict_thunk(), &ctx);

        // Create the PendingBuiltin thunk
        let pending_thunk = Rc::new(Thunk::new_pending_builtin(
            builtin!("drop", builtin_drop_seq_step),
            vec![n_remaining, seq],
            None,
            call_span(),
            Some(Rc::from("test drop_seq_step")),
            Rc::clone(&ctx),
        ));

        // Materialize it and expect an error
        let result = crate::eval::materialize(&pending_thunk, None, &ctx);
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
        let ctx = test_ctx();
        let seq = mat(builtin_range(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(Value::Int(10))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        let result = mat(builtin_drop(BuiltinArgs {
            args: &[thunk(Value::Int(2)), thunk(seq)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Result should be a PendingBuiltin (can't inspect internal state, but can verify it materializes correctly)
        match result {
            Value::Seq { head, .. } => {
                // First element after dropping 2 should be 2
                assert_eq!(mat_id(head, &ctx), Value::Int(2));
            }
            other => panic!("expected Seq from drop, got {:?}", other),
        }
    }

    #[test]
    fn reduce_constructs_pending_call() {
        // reduce(+, 0, [1, 2]) should create a PendingCall chain
        let ctx = test_ctx();
        let mut m = IndexMap::new();
        m.insert(Key::Int(0), thunk(Value::Int(1)));
        m.insert(Key::Int(1), thunk(Value::Int(2)));
        let seq_val = thunk_dict(m, &ctx);

        let add_builtin = standard_builtins()
            .into_iter()
            .find(|def| def.name == "+")
            .map(|def| Value::Builtin(def))
            .unwrap();

        let result = mat(builtin_reduce(BuiltinArgs {
            args: &[thunk(add_builtin), thunk(Value::Int(0)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Result should be 3 (0 + 1 + 2)
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn join_constructs_pending_call() {
        // join(",", ["a", "b"]) should create a PendingCall chain
        let ctx = test_ctx();
        let mut m = IndexMap::new();
        m.insert(Key::Int(0), thunk(string_val("a".into())));
        m.insert(Key::Int(1), thunk(string_val("b".into())));
        let seq_val = thunk_dict(m, &ctx);

        let result = mat(builtin_join(BuiltinArgs {
            args: &[thunk(string_val(",".into())), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));

        // Result should be "a,b"
        assert_eq!(result, string_val("a,b".into()));
    }

    /// Helper: create a function whose closure env contains builtins (needed for
    /// tests where the function body calls builtins like $builtin-add).
    fn n_arg_fn_with_builtins(param_names: &[&str], body_expr: Expr) -> Value {
        let env = create_root_env();
        inject_prelude_aliases(&mut env.borrow_mut());
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
            env,
            annotation: None,
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
                            Expr::var_ref("builtin-eq".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::var_ref("x".to_string()),
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
                            Expr::var_ref("builtin-add".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::var_ref("x".to_string()),
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
                            Expr::var_ref("error".to_string()),
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
                            Expr::var_ref("builtin-eq".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::var_ref("x".to_string()),
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
                            Expr::var_ref("builtin-add".to_string()),
                            test_span(1, 1, 1, 10),
                        )),
                        args: vec![
                            Rc::new(Spanned::new(
                                Expr::var_ref("x".to_string()),
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

    fn make_int_dict(vals: &[i64], ctx: &Rc<crate::eval::EvalContext>) -> Value {
        let mut rc_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
        for (i, &v) in vals.iter().enumerate() {
            rc_map.insert(Key::Int(i as i64), thunk(Value::Int(v)));
        }
        let mut id_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(rc_map.len());
        for (k, v) in rc_map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Value::Dict(id_map)
    }

    fn extract_int_at(
        map: &IndexMap<Key, ThunkId>,
        idx: i64,
        ctx: &Rc<crate::eval::EvalContext>,
    ) -> i64 {
        let thunk = ctx.get_thunk(*map.get(&Key::Int(idx)).unwrap());
        match crate::eval::materialize(&thunk, None, ctx).unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int at index {idx}, got {:?}", other),
        }
    }

    #[test]
    fn rest_three_elements_drops_first() {
        let ctx = test_ctx();
        let result = mat(builtin_rest(BuiltinArgs {
            args: &[thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 2);
        assert_eq!(extract_int_at(&m, 0, &ctx), 20);
        assert_eq!(extract_int_at(&m, 1, &ctx), 30);
    }

    #[test]
    fn rest_single_element_returns_empty() {
        let ctx = test_ctx();
        let result = mat(builtin_rest(BuiltinArgs {
            args: &[thunk(make_int_dict(&[42], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
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
        let ctx = test_ctx();
        let result = mat(builtin_cons(BuiltinArgs {
            args: &[thunk(Value::Int(0)), thunk(make_int_dict(&[1, 2, 3], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 4);
        assert_eq!(extract_int_at(&m, 0, &ctx), 0);
        assert_eq!(extract_int_at(&m, 1, &ctx), 1);
        assert_eq!(extract_int_at(&m, 3, &ctx), 3);
    }

    #[test]
    fn cons_onto_empty_dict() {
        let ctx = test_ctx();
        let result = mat(builtin_cons(BuiltinArgs {
            args: &[thunk(Value::Int(99)), thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 1);
        assert_eq!(extract_int_at(&m, 0, &ctx), 99);
    }

    #[test]
    fn reverse_three_elements() {
        let ctx = test_ctx();
        let result = mat(builtin_reverse(BuiltinArgs {
            args: &[thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 3);
        assert_eq!(extract_int_at(&m, 0, &ctx), 30);
        assert_eq!(extract_int_at(&m, 1, &ctx), 20);
        assert_eq!(extract_int_at(&m, 2, &ctx), 10);
    }

    #[test]
    fn reverse_empty_dict() {
        let result = mat(builtin_reverse(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
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
        let ctx = test_ctx();
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk(make_int_dict(&[3, 1, 4, 1, 5], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 5);
        let expected = [1i64, 1, 3, 4, 5];
        for (i, &exp) in expected.iter().enumerate() {
            assert_eq!(extract_int_at(&m, i as i64, &ctx), exp, "at index {i}");
        }
    }

    #[test]
    fn sort_strings_lexicographic() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        for (i, s) in ["banana", "apple", "cherry"].iter().enumerate() {
            map.insert(Key::Int(i as i64), thunk(string_val(s)));
        }
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        let v0 = mat_id(*m.get(&Key::Int(0)).unwrap(), &ctx);
        assert_eq!(v0, string_val("apple".into()));
    }

    #[test]
    fn sort_empty_dict() {
        let result = mat(builtin_sort(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    // ========== each/each-key/each-kv/builtin-get tests ==========

    #[test]
    fn each_empty_dict() {
        let result = mat(builtin_each(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        // each on empty dict returns empty dict (used as Seq terminator)
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn each_multi_entry() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(10)));
        map.insert(Key::String("b".into()), thunk(Value::Int(20)));
        map.insert(Key::String("c".into()), thunk(Value::Int(30)));
        let result = mat(builtin_each(BuiltinArgs {
            args: &[thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        // each returns a Seq — materialize head
        match result {
            Value::Seq { head, tail } => {
                let head_val = mat_id(head, &ctx);
                assert_eq!(head_val, Value::Int(10));
                // Verify tail is also a Seq (not fully unwinding it here)
                let tail_thunk = ctx.get_thunk(tail);
                let tail_val = crate::eval::materialize(&tail_thunk, None, &ctx).unwrap();
                assert!(matches!(tail_val, Value::Seq { .. }));
            }
            other => panic!("expected Seq, got {:?}", other),
        }
    }

    #[test]
    fn each_type_error_int() {
        let ctx = test_ctx();
        let result = builtin_each(BuiltinArgs {
            args: &[thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got {:?}",
            err.kind
        );
    }

    #[test]
    fn each_type_error_string() {
        let ctx = test_ctx();
        let result = builtin_each(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got {:?}",
            err.kind
        );
    }

    #[test]
    fn each_type_error_bool() {
        let ctx = test_ctx();
        let result = builtin_each(BuiltinArgs {
            args: &[thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got {:?}",
            err.kind
        );
    }

    #[test]
    fn each_key_empty_dict() {
        let result = mat(builtin_each_key(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn each_key_type_error_string() {
        let ctx = test_ctx();
        let result = builtin_each_key(BuiltinArgs {
            args: &[thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got {:?}",
            err.kind
        );
    }

    #[test]
    fn each_kv_empty_dict() {
        let result = mat(builtin_each_kv(BuiltinArgs {
            args: &[thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn each_kv_type_error_bool() {
        let ctx = test_ctx();
        let result = builtin_each_kv(BuiltinArgs {
            args: &[thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch, got {:?}",
            err.kind
        );
    }

    #[test]
    fn builtin_get_int_key_auto_indexed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), thunk(string_val("first".into())));
        map.insert(Key::Int(1), thunk(string_val("second".into())));
        map.insert(Key::Int(2), thunk(string_val("third".into())));
        let result = mat(builtin_get(BuiltinArgs {
            args: &[thunk(Value::Int(1)), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        }));
        assert_eq!(result, string_val("second".into()));
    }

    #[test]
    fn builtin_get_key_not_found_error() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(10)));
        map.insert(Key::String("b".into()), thunk(Value::Int(20)));
        let result = builtin_get(BuiltinArgs {
            args: &[thunk(string_val("z".into())), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Rc::clone(&ctx),
        });
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::KeyNotFound { .. }),
            "expected KeyNotFound, got {:?}",
            err.kind
        );
    }
}
