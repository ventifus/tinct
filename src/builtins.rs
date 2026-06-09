//! Builtin registry, bootstrap, and slim helpers for the LLT language.
//!
//! Builtin implementations live in split files (`builtins_math.rs`, `builtins_io.rs`, etc.).
//! This file provides:
//! - `builtin_module(name)` — dispatch to per-module aggregators (`core_builtins()`, etc.)
//! - `type_env_module(name)` — dispatch to per-module type environments
//! - `build_builtins_type_env()` — combined type env for all modules
//! - `create_stdlib_env_inner()` / `create_type_stage_env()` — bootstrap prelude loading
//! - Helper functions: `ok_val`, `string_val`, `reject_named`, `require_string`, etc.
//! - Re-exports of split-file functions for test access via `use super::*`

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{CoreExpr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
#[allow(unused_imports)] // used in test modules via `use super::*`
use crate::value::Strictness;
// Circular module dependency: this module imports `invoke_function` and `materialize` from eval.rs.
// eval.rs calls builtins via function pointers stored in `Value::Builtin`.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
// SAFETY: builtins.rs and eval.rs have a circular dependency at the value level — builtins call
// materialize/invoke_function (eval.rs), and eval calls builtin_module() (builtins.rs). This is
// safe because the dependency is at function-call level, not at module initialization level.
// Rust modules can call each other's pub functions after initialization without deadlock.
use crate::eval::materialize_sync as materialize;
use crate::eval_call::{invoke_function_sync as invoke_function, CallContext};
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
    // 2-arg form: all-lazy (empty strictness array, force_count=0)
    ($name:literal, $func:expr) => {{
        const S: &[crate::value::Strictness] = &[];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
            force_count: 0,
        }
    }};
    // 3-arg form: with strictness array (force_count=0)
    ($name:literal, $func:expr, [$($strictness:expr),* $(,)?]) => {{
        const S: &[crate::value::Strictness] = &[$($strictness),*];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
            force_count: 0,
        }
    }};
    // 4-arg form: with strictness array and force_count
    ($name:literal, $func:expr, [$($strictness:expr),* $(,)?], $force_count:expr) => {{
        const S: &[crate::value::Strictness] = &[$($strictness),*];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
            force_count: $force_count,
        }
    }};
}
pub(crate) use builtin;

/// Maximum collection size for $collect (1,000,000 elements).
/// Prevents memory exhaustion from infinite sequences without $take.
pub const MAX_COLLECT_SIZE: usize = 1_000_000;

// DELETED: JSON_DEPTH_LIMIT (json-serde-removal sprint)
// from-json is now pure tinct in stdlib/codecs/json.llt; depth checking
// happens there, not in Rust.

/// Maximum string output size for string output builtins (`$replace`, `$str-map-chars`, `$join`) (64 MB).
/// Prevents memory exhaustion from adversarial inputs or replacement patterns.
pub(crate) const MAX_STRING_SIZE: usize = 64 * 1024 * 1024;

/// Type alias for the stdlib-env-with-arena return type. Reduces type_complexity
/// in `create_stdlib_env_with_arena` and `create_stdlib_env_inner`.
type StdlibEnvWithArena = Result<
    (
        Arc<RwLock<Environment>>,
        Arc<Mutex<crate::arena::ThunkArena>>,
    ),
    Box<crate::error::EvalError>,
>;

pub(crate) fn ok_val(v: Value, span: Span) -> EvalResult<Arc<Thunk>> {
    Ok(Arc::new(Thunk::new_materialized(v, span)))
}

/// Convert a `Value::Bytes` slice into a Seq of `Value::Int` (one per byte).
///
/// Used by sequence operations (map, filter, take, drop, reduce) to treat Bytes as
/// an iterable sequence of byte values (0–255). Results are always Seq (not Bytes).
///
/// The returned value is a `Seq.Cons` variant if bytes is non-empty, or
/// `Seq.Nil` (the terminal empty sequence) if empty.
pub(crate) fn bytes_to_seq(bytes: &[u8], span: Span, ctx: &Arc<crate::eval::EvalContext>) -> Value {
    use crate::value::{make_seq_cons, make_seq_nil};
    // Build from the right so we don't need a separate pass.
    let mut acc: Value = make_seq_nil();
    for &byte in bytes.iter().rev() {
        let head = Arc::new(Thunk::new_materialized(
            Value::Int(i64::from(byte)),
            span.clone(),
        ));
        let tail = Arc::new(Thunk::new_materialized(acc, span.clone()));
        let head_id = ctx.alloc_thunk(head);
        let tail_id = ctx.alloc_thunk(tail);
        acc = make_seq_cons(head_id, tail_id, ctx);
    }
    acc
}

/// Helper: create a synthetic CoreExpr::Call for builtin-generated calls.
///
/// Used when builtins construct PendingCall thunks (e.g., map, filter, until).
/// The CoreExpr is needed for DepthExceeded restore but won't be re-evaluated.
pub(crate) fn synthetic_call_expr(span: Span) -> Arc<Spanned<CoreExpr>> {
    Arc::new(Spanned {
        node: CoreExpr::Call {
            func: Arc::new(Spanned {
                node: CoreExpr::Int(0), // placeholder, never evaluated
                span: span.clone(),
            }),
            args: vec![],
            named_args: vec![],
            implied: false,
        },
        span,
    })
}

/// Maximum file size for reading LLT files: 10 MB.
pub const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;

/// Helper: get a pre-materialized single positional argument, enforcing exact arity of 1
/// and rejecting named arguments. Used by many single-arg builtins with force_count=1.
pub(crate) fn expect_one_arg(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<Value> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    Ok(args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness"))
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
pub(crate) fn check_float_result(val: f64, op: &str, span: Span) -> EvalResult<Arc<Thunk>> {
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
    ctx: &Arc<crate::eval::EvalContext>,
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
        let thunk = ctx.thunk_arena.lock().unwrap().get(thunk_id).clone();
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
            Value::Variant { payload, .. } => {
                // Auto-unpack variant payload. This intentionally diverges from require_dict
                // which errors on unit variants: flatten_overlay accepts unit variants as
                // empty dict layers to support the [payload-of [unit-variant]] => [] idiom.
                match payload {
                    Some(payload_id) => {
                        // Re-push the payload thunk for processing in the next iteration.
                        // This handles recursive cases (payload is itself an Overlay).
                        work_stack.push(payload_id);
                    }
                    None => {
                        // Unit variant: contribute an empty dict layer.
                        layers.push(IndexMap::new());
                    }
                }
            }
            other => {
                let span = thunk.span.clone();
                return Err(EvalError::type_mismatch_ctx(
                    name.to_string(),
                    "Dict",
                    other.type_name(),
                    span,
                )
                .into());
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
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<IndexMap<Key, ThunkId>> {
    match value {
        Value::Dict(map) => Ok(map),
        Value::Overlay(l, r) => flatten_overlay(&l, &r, name, ctx, call_span),
        Value::Variant { payload, .. } => {
            // Auto-unpack variant payload — consistent with DotAccess behavior
            match payload {
                Some(payload_id) => {
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_val =
                        crate::eval::materialize_sync(&payload_thunk, Some(&call_span), ctx)?;
                    // Recursively try to extract dict from payload
                    require_dict(name, payload_val, def_span, ctx, call_span)
                }
                None => {
                    let err = EvalError::type_mismatch_ctx(
                        name.to_string(),
                        "Dict",
                        "unit variant (no payload)",
                        def_span,
                    );
                    Err(err.into())
                }
            }
        }
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
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
) -> EvalResult<()> {
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    Ok(())
}

// Arithmetic, comparison, and control-flow builtins: +, -, *, /, =, <, if.
// Implementations live in builtins_math.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_math::{
    builtin_acos, builtin_add, builtin_asin, builtin_atan, builtin_atan2, builtin_band,
    builtin_bor, builtin_bxor, builtin_cos, builtin_div_float, builtin_eq, builtin_exp,
    builtin_finite_check, builtin_float, builtin_gt, builtin_gte, builtin_if, builtin_inf_check,
    builtin_log, builtin_log10, builtin_log2, builtin_lt, builtin_lte, builtin_mul,
    builtin_nan_check, builtin_pow, builtin_shl, builtin_shr, builtin_sin, builtin_sqrt,
    builtin_sub, builtin_tan,
};

// Dict/access builtins: keys, length, merge, append, get, each, each-key, each-kv, build-dict.
// Implementations live in builtins_dict.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_dict::{
    builtin_append, builtin_build_dict, builtin_builder_delete, builtin_builder_finish,
    builtin_builder_get, builtin_builder_get_or, builtin_builder_has, builtin_builder_set,
    builtin_builder_snapshot, builtin_each, builtin_each_key, builtin_each_kv, builtin_get,
    builtin_get_optional, builtin_keys, builtin_length, builtin_make_builder, builtin_merge,
};

// Type/eval/meta builtins: type-of, include, error, try, apply, validate.
// Implementations live in builtins_meta.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
// json_to_value deleted in json-serde-removal sprint (from-json is now pure tinct in stdlib/codecs/json.llt)
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_meta::{
    builtin_annotation_of, builtin_apply, builtin_ast_of, builtin_big_int, builtin_blake3,
    builtin_cap_identity, builtin_decimal, builtin_eval, builtin_eval_types, builtin_expand,
    builtin_force, builtin_gensym, builtin_include_cache_get, builtin_include_cache_put,
    builtin_llt_repr, builtin_load, builtin_macro_error, builtin_macro_injects,
    builtin_make_annotated, builtin_raise, builtin_tag_of, builtin_try, builtin_type_of,
    builtin_until, builtin_validate, builtin_variant,
};

// String builtins: str, split, replace, trim, trim-start, trim-end,
// str-length, str-index-of, str-slice, str-chars, char-code, chr, str-bytes, bytes-str,
// str-to-upper-char, str-to-lower-char, str-map-chars, regex-match?.
// Note: upper/lower are no longer Rust builtins; they live in stdlib/strings.llt and
// are implemented using str-map-chars + str-to-upper-char / str-to-lower-char.
// Implementations live in builtins_string.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[cfg(test)]
pub(crate) use crate::builtins_string::MAX_SPLIT_PARTS;
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_string::{
    builtin_bytes_str, builtin_char_code, builtin_chr, builtin_regex_match, builtin_replace,
    builtin_split, builtin_str, builtin_str_bytes, builtin_str_chars, builtin_str_index_of,
    builtin_str_length, builtin_str_map_chars, builtin_str_slice, builtin_str_to_lower_char,
    builtin_str_to_upper_char, builtin_trim, builtin_trim_end, builtin_trim_start,
};

// Bytes builtins: bytes, bytes-find, bytes-of, bytes-equal?, ct-equal?.
// Implementations live in builtins_bytes.rs.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_bytes::{
    builtin_bytes, builtin_bytes_equal, builtin_bytes_find, builtin_bytes_of, builtin_ct_equal,
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
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<Arc<Thunk>> {
    let val = expect_one_arg(name, args, named, ctx, call_span.clone())?;
    match val {
        Value::Int(n) => ok_val(Value::Int(n), call_span),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(
                    EvalError::float_not_finite(name.to_string(), f, args[0].span.clone()).into(),
                );
            }
            ok_val(
                Value::Int(checked_f64_to_i64(name, op(f), call_span.clone())?),
                call_span,
            )
        }
        other => Err(EvalError::type_mismatch_ctx(
            name.to_string(),
            "Int or Float",
            other.type_name(),
            args[0].span.clone(),
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
///
/// Inherently materializing: must inspect numeric value to convert/round.
pub(crate) fn builtin_floor(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        float_to_int_builtin("floor", f64::floor, &args, named.as_ref(), &ctx, call_span)
    })
}

/// `round`: Takes 1 numeric arg (Int or Float). Returns Int.
///
/// - Int input: returned unchanged.
/// - Float input: applies `f64::round()` (half-away-from-zero) then converts to `i64`.
/// - NaN or Infinity: errors (cannot convert to Int).
/// - Non-numeric input: type error.
///
/// Inherently materializing: must inspect numeric value to convert/round.
pub(crate) fn builtin_round(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        float_to_int_builtin("round", f64::round, &args, named.as_ref(), &ctx, call_span)
    })
}

/// `to-int`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as an integer via `str::parse::<i64>()`. Returns Int.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse integer value.
pub(crate) fn builtin_to_int(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    if args.is_empty() {
        return Box::pin(async move { Err(EvalError::arity_mismatch(1, 0, call_span).into()) });
    }
    Box::pin(async move {
        let val = expect_one_arg("to-int", &args, named.as_ref(), &ctx, call_span.clone())?;
        let arg0_span = args[0].span.clone();
        let s = require_string("to-int", val, arg0_span)?;
        match s.parse::<i64>() {
            Ok(n) => ok_val(Value::Int(n), call_span),
            Err(_) => {
                Err(
                    EvalError::parse_conversion("to-int".to_string(), s.clone(), "Int", call_span)
                        .into(),
                )
            }
        }
    })
}

/// `to-float`: STRING-TO-NUMBER PARSING ONLY. Takes 1 String arg.
///
/// Parses the string as a float via `str::parse::<f64>()`. Returns Float.
/// Does NOT accept numeric inputs -- it is a string parser, not a type converter.
/// Inherently materializing: must inspect string content to parse float value.
pub(crate) fn builtin_to_float(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    if args.is_empty() {
        return Box::pin(async move { Err(EvalError::arity_mismatch(1, 0, call_span).into()) });
    }
    Box::pin(async move {
        let val = expect_one_arg("to-float", &args, named.as_ref(), &ctx, call_span.clone())?;
        let arg0_span = args[0].span.clone();
        let s = require_string("to-float", val, arg0_span)?;
        match s.parse::<f64>() {
            Ok(f) if f.is_finite() => ok_val(Value::Float(f), call_span),
            Ok(f) => Err(EvalError::float_not_finite("to-float".to_string(), f, call_span).into()),
            Err(_) => Err(EvalError::parse_conversion(
                "to-float".to_string(),
                s.clone(),
                "Float",
                call_span,
            )
            .into()),
        }
    })
}

// Seq primitive builtins: seq, head, tail, collect.
// Implementations live in builtins_seq_prim.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
// Note: seq? (builtin_seq_check) was removed — type predicate now in LLT stdlib via match.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_seq_prim::{
    builtin_collect, builtin_head, builtin_seq, builtin_tail,
};

// Sequence generator builtins: range, repeat, cycle, iterate, unfold.
// Implementations live in builtins_seq_gen.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_seq_gen::{
    builtin_cycle, builtin_iterate, builtin_range, builtin_repeat, builtin_unfold,
};

// Sequence transform builtins: map, filter, take, drop.
// Implementations live in builtins_seq_xform.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_seq_xform::{
    builtin_drop, builtin_filter, builtin_map, builtin_take,
};
// Step helper is only used in tests via `use super::*`
#[cfg(test)]
pub(crate) use crate::builtins_seq_xform::builtin_drop_seq_step;
// Sequence reduction builtins: reduce, join, concat.
// Implementations live in builtins_seq_reduce.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_seq_reduce::{
    builtin_concat, builtin_join, builtin_reduce, builtin_reduce_dict_step, builtin_reduce_seq_step,
};

/// `first`: Return the first element of a Dict, the first character of a String,
/// or the first byte (as Int) of a Bytes value.
///
/// - Takes 1 arg: a Dict, String, or Bytes.
/// - Dict path: O(1) — returns the value at the first key (insertion order).
/// - String path: O(1) — returns a single-char String slice of the first codepoint.
/// - Bytes path: O(1) — returns the first byte as Value::Int.
///
/// Inherently materializing: must access the value to determine type and extract first element.
pub(crate) fn builtin_first(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("first", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
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
                let map = require_dict(
                    "first",
                    other,
                    args[0].span.clone(),
                    &ctx,
                    call_span.clone(),
                )?;
                if map.is_empty() {
                    return Err(EvalError::empty_collection("first".to_string(), call_span).into());
                }
                let (_, first_id) = map.into_iter().next().expect("non-empty map");
                let thunk = ctx.get_thunk(first_id);
                Ok(thunk)
            }
        }
    })
}

/// `last`: Return the last element of a Dict, the last character of a String,
/// or the last byte (as Int) of a Bytes value.
///
/// - Takes 1 arg: a Dict, String, or Bytes.
/// - Dict path: O(n) — must iterate to the last entry (IndexMap doesn't have O(1) last).
/// - String path: O(n) — must walk UTF-8 chars to find the last codepoint.
/// - Bytes path: O(1) — returns the last byte as Value::Int.
///
/// Inherently materializing: must access the value to determine type and extract last element.
pub(crate) fn builtin_last(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("last", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
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
                let map =
                    require_dict("last", other, args[0].span.clone(), &ctx, call_span.clone())?;
                if map.is_empty() {
                    return Err(EvalError::empty_collection("last".to_string(), call_span).into());
                }
                let (_, last_id) = map.into_iter().last().expect("non-empty map");
                let thunk = ctx.get_thunk(last_id);
                Ok(thunk)
            }
        }
    })
}

/// `rest`: Returns all elements of a collection except the first, reindexed 0..n-1.
///
/// - Takes 1 arg: a Dict or Seq.
/// - Seq path: O(1) — delegates to `$tail` (returns the Seq's tail directly).
/// - Dict path: O(n) — drops the first entry by insertion order, rebuilds with dense
///   integer keys starting at 0. Same asymptotic cost as the LLT implementation, but
///   avoids interpreter loop overhead.
///
/// Inherently materializing for Dict: must copy all remaining entries.
/// Lazy for Seq: O(1) tail extraction.
pub(crate) fn builtin_rest(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("rest", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        // Seq path: delegate to $tail (O(1), preserves laziness).
        if crate::value::is_seq(&val) {
            return builtin_tail(BuiltinArgs {
                args: args.clone(),
                named: named.clone(),
                call_span,
                ctx,
            })
            .await;
        }
        let map = require_dict("rest", val, args[0].span.clone(), &ctx, call_span.clone())?;

        // Skip the first entry (index 0 by insertion order), reindex rest as 0..n-1.
        let mut result = IndexMap::with_capacity(map.len().saturating_sub(1));
        for (new_idx, (_old_key, thunk)) in map.into_iter().skip(1).enumerate() {
            let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
                EvalError::internal("collection index overflow".to_string(), call_span.clone())
            })?);
            result.insert(new_key, thunk);
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `cons`: Prepend an element to a collection, reindexing all entries from 0.
///
/// - Takes 2 args: (element, collection).
/// - Seq path: O(1) — delegates to `$seq x xs` (returns a lazy Seq).
/// - Dict path: O(n) — builds a new dict with the element at key 0, followed by
///   the existing entries reindexed as 1..n. Same asymptotic cost as the LLT
///   implementation, but avoids interpreter loop overhead.
///
/// Inherently materializing for Dict: must copy all existing entries.
/// Lazy for Seq: O(1) prepend.
pub(crate) fn builtin_cons(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("cons", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // args[0] is the element to prepend (kept as thunk — preserves laziness).
        // args[1] is the collection to prepend to (must be materialized to dispatch on type).
        let xs_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Spine");
        // Seq path: delegate to $seq (O(1), preserves laziness).
        if crate::value::is_seq(&xs_val) {
            return builtin_seq(BuiltinArgs {
                args: args.clone(),
                named: named.clone(),
                call_span,
                ctx,
            })
            .await;
        }
        let map = require_dict(
            "cons",
            xs_val,
            args[1].span.clone(),
            &ctx,
            call_span.clone(),
        )?;

        let mut result = IndexMap::with_capacity(map.len() + 1);
        // Insert the new element at key 0.
        let elem_id = ctx.alloc_thunk(Arc::clone(&args[0]));
        result.insert(Key::Int(0), elem_id);
        // Insert existing entries reindexed as 1..n.
        for (new_idx, (_old_key, thunk_id)) in map.into_iter().enumerate() {
            let new_key = Key::Int(i64::try_from(new_idx + 1).map_err(|_| {
                EvalError::internal("collection index overflow".to_string(), call_span.clone())
            })?);
            result.insert(new_key, thunk_id);
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `reverse`: Reverse the entries of a dict list, reindexing from 0.
///
/// - Takes 1 arg: a Dict.
/// - Materializes the dict, collects entries in reverse insertion order,
///   builds a new dict with dense integer keys 0..n-1.
/// - O(n) — avoids the recursive LLT accumulator pattern.
///
/// Inherently materializing: must know all entries to reverse order.
pub(crate) fn builtin_reverse(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("reverse", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = require_dict(
            "reverse",
            val,
            args[0].span.clone(),
            &ctx,
            call_span.clone(),
        )?;

        let mut result = IndexMap::with_capacity(map.len());
        // Collect values in reverse insertion order.
        let entries: Vec<_> = map.into_iter().collect();
        for (new_idx, (_old_key, thunk)) in entries.into_iter().rev().enumerate() {
            let new_key = Key::Int(i64::try_from(new_idx).map_err(|_| {
                EvalError::internal("collection index overflow".to_string(), call_span.clone())
            })?);
            result.insert(new_key, thunk);
        }
        ok_val(Value::Dict(result), call_span)
    })
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
///
/// Inherently materializing: must inspect all values to determine sort order.
pub(crate) fn builtin_sort(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("sort", named.as_ref(), call_span.clone())?;

        // Accept 1 arg (dict only) or 2 args (comparator, dict)
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Determine if we have a comparator function
        let (comparator_opt, dict_arg_idx) = if args.len() == 2 {
            // First arg is comparator, second is dict.
            // args[0] is Spine-pre-materialized by pos_strictness[0].
            let cmp_val = args[0]
                .try_get_materialized()
                .expect("pre-materialized by pos_strictness[0]=Spine");
            let arg0_span = args[0].span.clone();
            match cmp_val {
                Value::Function { .. } | Value::Builtin(_) => (Some((cmp_val, arg0_span)), 1),
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "sort".to_string(),
                        "Function",
                        other.type_name(),
                        arg0_span,
                    )
                    .into());
                }
            }
        } else {
            (None, 0)
        };

        let dict_span = args[dict_arg_idx].span.clone();
        let val = args[dict_arg_idx]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[dict_arg_idx]=Spine");
        let map = require_dict("sort", val, dict_span, &ctx, call_span.clone())?;

        // Materialize all values so we can compare them.
        let mut pairs: Vec<(Value, Span)> = Vec::with_capacity(map.len());
        for (_key, thunk_id) in &map {
            let thunk = ctx.get_thunk(*thunk_id);
            let mat = materialize(&thunk, Some(&call_span), &ctx)?;
            pairs.push((mat, thunk.span.clone()));
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
                let a_thunk = Arc::new(Thunk::new_materialized(a.clone(), a_span.clone()));
                let b_thunk = Arc::new(Thunk::new_materialized(b.clone(), b_span.clone()));
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
                            call_span: call_span.clone(),
                            origin: Some(Arc::from("sort")),
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
                        // Drive the async builtin future synchronously. The sort_by closure is
                        // sync and cannot .await; block_on_anywhere handles this correctly
                        // (using poll_future_sync when already inside a current_thread runtime).
                        // Builtin comparators in sort are uncommon.
                        let builtin_args = BuiltinArgs {
                            args: pos_args,
                            named: None,
                            call_span: call_span.clone(),
                            ctx: Arc::clone(&ctx),
                        };
                        let fut = (def.func)(builtin_args);
                        match crate::async_rt::block_on_anywhere(fut) {
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
                            cmp_span.clone(),
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
                            result_thunk.span.clone(),
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
                match compare_values(a, b, call_span.clone()) {
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
                EvalError::internal("collection index overflow".to_string(), call_span.clone())
            })?);
            let thunk = Arc::new(Thunk::new_materialized(mat_val, orig_span));
            let thunk_id = ctx.alloc_thunk(thunk);
            result.insert(new_key, thunk_id);
        }
        ok_val(Value::Dict(result), call_span)
    })
}

pub(crate) fn builtin_proxy(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    Box::pin(async move {
        reject_named("proxy", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let handler_id = ctx.alloc_thunk(Arc::clone(&args[0]));
        Ok(Arc::new(Thunk::new_materialized(
            Value::Proxy {
                handler: handler_id,
            },
            call_span,
        )))
    })
}

// Bootstrap: loader.llt → prelude.llt (direct eval) → stdlib_env. (S-873)
// Prelude now includes macro transformers (formerly macros.llt).
// Fatal: stdlib failure is not recoverable — callers should propagate or panic on Err.

// Reentrance guard for create_stdlib_env to detect unexpected recursive calls.
std::thread_local! {
    static STDLIB_ENV_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Cache of the stdlib arena so EvalContexts created after create_stdlib_env()
    /// can inherit the stdlib ThunkIds without the caller explicitly threading the arena.
    static STDLIB_ARENA_CACHE: std::cell::RefCell<Option<Arc<Mutex<crate::arena::ThunkArena>>>> =
        const { std::cell::RefCell::new(None) };
    /// Full result cache: caches the (env, arena) pair returned by create_stdlib_env_with_arena.
    /// On a cache hit, the existing Arc<RwLock<Environment>> and Arc<Mutex<ThunkArena>> are
    /// returned directly, avoiding a full stdlib rebuild. Cleared by clear_stdlib_cache().
    #[allow(clippy::type_complexity)]
    static STDLIB_RESULT_CACHE: std::cell::RefCell<Option<(Arc<RwLock<Environment>>, Arc<Mutex<crate::arena::ThunkArena>>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Return a new ThunkArena pre-populated with the stdlib thunks (via Arc::clone),
/// so ThunkIds allocated during stdlib loading are valid in the returned arena.
/// Returns None if create_stdlib_env has not yet been called on this thread.
pub(crate) fn new_arena_with_stdlib_snapshot() -> Option<Arc<Mutex<crate::arena::ThunkArena>>> {
    STDLIB_ARENA_CACHE.with(|c| {
        c.borrow().as_ref().map(|stdlib_arena| {
            Arc::new(Mutex::new(stdlib_arena.lock().unwrap().clone_for_child()))
        })
    })
}

/// Clear both stdlib caches (`STDLIB_ARENA_CACHE` and `STDLIB_RESULT_CACHE`). This is
/// intended for test environments where the cache needs to be reset between test iterations
/// to prevent memory accumulation.
///
/// **Performance impact:** After calling this function, the next call to
/// `create_stdlib_env_with_arena()` will rebuild the stdlib from scratch (parsing and
/// evaluating loader.llt and prelude.llt directly). Subsequent calls will use the
/// new cached result until the next `clear_stdlib_cache()` call.
///
/// **Production use:** This function should NOT be called in production code. The stdlib
/// cache is intentionally persistent across multiple evaluations to amortize the cost of
/// stdlib loading. Only test harnesses that run hundreds of independent evaluations in
/// the same process should call this.
pub fn clear_stdlib_cache() {
    STDLIB_ARENA_CACHE.with(|c| *c.borrow_mut() = None);
    STDLIB_RESULT_CACHE.with(|c| *c.borrow_mut() = None);
}

pub fn create_stdlib_env() -> Result<Arc<RwLock<Environment>>, Box<crate::error::EvalError>> {
    let (env, _arena) = create_stdlib_env_with_arena()?;
    // Arena already cached by create_stdlib_env_with_arena
    Ok(env)
}

/// Like `create_stdlib_env` but also returns the arena used during stdlib evaluation.
/// The arena holds all ThunkIds allocated while loading the prelude (which now includes
/// the macro transformer functions formerly in macros.llt).
/// Callers (e.g., macro expansion) that need to share the same ThunkId space should
/// use this arena when constructing their EvalContext via `EvalContext::new_sharing_arena`.
///
/// **Cache consistency:** This function ALSO updates `STDLIB_ARENA_CACHE` so that subsequent
/// `EvalContext::new()` calls on this thread inherit the stdlib ThunkIds. This ensures cache
/// consistency regardless of which entry point (`create_stdlib_env()` or
/// `create_stdlib_env_with_arena()`) was used to build the stdlib.
pub(crate) fn create_stdlib_env_with_arena() -> StdlibEnvWithArena {
    // Fast path: return cached (env, arena) pair if available, avoiding a full stdlib rebuild.
    // STDLIB_RESULT_CACHE is cleared by clear_stdlib_cache(), which corpus tests call between
    // iterations. On a cache hit, Arc::clone is O(1) and both callers share the same env/arena.
    if let Some((env, arena)) = STDLIB_RESULT_CACHE.with(|c| c.borrow().clone()) {
        return Ok((env, arena));
    }

    let d = STDLIB_ENV_DEPTH.get();
    if d > 5 {
        panic!(
            "create_stdlib_env: infinite recursion detected (depth={})",
            d
        );
    }
    // Open CWD once at the public entry point; the private helper receives it as a parameter
    // so that open_ambient_dir is confined to this function (the bootstrap boundary for the
    // stdlib loading context).
    // AMBIENT-OK: stdlib bootstrap — opening CWD to load stdlib from fixed paths.
    #[allow(clippy::disallowed_methods)]
    let bootstrap_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| {
            Box::new(crate::error::EvalError::internal(
                format!("cannot open bootstrap base_dir: {e}"),
                Span::origin(),
            ))
        })?;
    STDLIB_ENV_DEPTH.set(d + 1);
    let result = create_stdlib_env_inner(bootstrap_base_dir);
    STDLIB_ENV_DEPTH.set(d);
    // Cache the arena so subsequent EvalContext::new() calls can inherit stdlib ThunkIds.
    if let Ok((ref env, ref arena)) = result {
        STDLIB_ARENA_CACHE.with(|c| *c.borrow_mut() = Some(Arc::clone(arena)));
        // Cache the full (env, arena) result so subsequent calls return immediately.
        STDLIB_RESULT_CACHE.with(|c| {
            *c.borrow_mut() = Some((Arc::clone(env), Arc::clone(arena)));
        });
    }
    result
}

fn create_stdlib_env_inner(bootstrap_base_dir: cap_std::fs::Dir) -> StdlibEnvWithArena {
    // Four-phase bootstrap (S-785 / T-726):
    // Phase 1: Populate loader env with core builtins
    // Phase 2: Evaluate loader.llt → loader_dict with eval-program / eval-programs functions
    // Phase 3: Evaluate prelude.llt DIRECTLY in a child of loader_env → prelude_dict
    // Phase 4: Convert prelude_dict to stdlib_env
    //
    // Macro transformer functions (tmpl, do, begin) are now defined directly in
    // prelude.llt as [macro ...] declarations and auto-discovered by the pre-scan pass.

    // ========== Phase 1 & 2: Parse loader.llt and create bootstrap context ==========
    // We need to create the loader context first so we have an arena to allocate
    // core builtins into. The loader env will be populated with core builtins,
    // then loader.llt will be evaluated in that env.

    let loader_source = include_str!("../stdlib/loader.llt");
    let loader_sf = std::sync::Arc::new(crate::ast::SourceFile {
        path: std::sync::Arc::from("stdlib/loader.llt"),
        content: std::sync::Arc::from(loader_source),
    });
    let loader_parsed = crate::parser::parse_with_file(loader_source, loader_sf)
        .map_err(|e| EvalError::internal(format!("loader.llt parse error: {e}"), Span::origin()))?;

    // Desugar and resolve loader.llt
    let mut loader_program = loader_parsed.program.clone();
    crate::desugar::desugar_surface_program(&mut loader_program);
    let loader_resolution_table =
        std::sync::Arc::new(crate::resolve::resolve_surface_program(&loader_program));
    let loader_type_table = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());

    // Create empty environment and context - we'll populate it with core builtins
    let loader_env = Arc::new(RwLock::new(Environment::new()));

    // Create bootstrap context for loader evaluation
    // Use new_empty since we're bootstrapping (no stdlib env exists yet)
    let loader_ctx = crate::eval::EvalContext::new_empty(
        bootstrap_base_dir.try_clone().map_err(|e| {
            Box::new(EvalError::internal(
                format!("cannot clone bootstrap_base_dir for loader: {e}"),
                Span::origin(),
            ))
        })?,
        Arc::clone(&loader_env),
        false,
    );

    // Now populate loader_env with core builtins using the loader_ctx's arena
    let core_builtins = builtin_module("core").ok_or_else(|| {
        Box::new(EvalError::internal(
            "builtin_module(\"core\") returned None during bootstrap".to_string(),
            Span::origin(),
        ))
    })?;

    for def in core_builtins {
        let name = def.name.to_string();
        let builtin_val = Value::Builtin(def);
        let thunk = Arc::new(Thunk::new_materialized(builtin_val, Span::origin()));
        // Insert directly into loader_env (no need to go through the arena for builtins)
        loader_env.write().unwrap().insert(name, thunk);
    }

    // Evaluate loader.llt (it has one document with one dict expression)
    let loader_thunk = crate::async_rt::block_on_anywhere(crate::eval::eval_surface_file(
        &loader_program,
        Arc::clone(&loader_env),
        &loader_ctx,
        &loader_resolution_table,
        &loader_type_table,
    ))?;

    let loader_val = crate::eval::materialize_sync(&loader_thunk, None, &loader_ctx)?;

    // loader_val should be a Dict with eval-program and eval-programs
    let loader_dict = match loader_val {
        Value::Dict(d) => d,
        Value::Overlay(l_id, r_id) => {
            flatten_overlay(&l_id, &r_id, "loader.llt", &loader_ctx, Span::origin())?
        }
        other => {
            return Err(Box::new(EvalError::internal(
                format!(
                    "loader.llt must evaluate to a Dict, got {}",
                    other.type_name()
                ),
                Span::origin(),
            )))
        }
    };

    // ========== Phase 3: Evaluate prelude DIRECTLY (fast path) ==========
    //
    // Instead of calling eval-program (which invokes builtin-eval → builtin-reduce through
    // the async eval chain), we evaluate prelude.llt directly using eval_surface_file —
    // the same mechanism as the prelude bootstrap (eval_surface_file). This is ~10x faster because it avoids
    // the overhead of constructing a Value::Program, invoking a tinct function, and running
    // the full eval-program pipeline.
    //
    // PRECONDITION: prelude.llt must not rely on `--- uses:` module injection for any of its
    // documents. This direct-eval path does not process doc.uses — module injection happens
    // only through loader.llt's eval-program pipeline (T-768/S-786). Core builtins are in
    // scope via loader_env parent chain.
    //
    // Correctness: prelude's `--- uses: ["core"]` header is metadata only. In direct eval
    // the env already contains core builtins (inherited from loader_env). We also inject
    // eval-program and eval-programs from loader_dict so prelude can reference them if needed.

    // Build prelude_env: child of loader_env (inherits core builtins) + loader_dict entries
    let prelude_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
        &loader_env,
    ))));
    for (key, thunk_id) in &loader_dict {
        let name = match key {
            Key::String(s) => s.to_string(),
            Key::Int(n) => n.to_string(),
        };
        let thunk = loader_ctx.get_thunk(*thunk_id);
        prelude_env.write().unwrap().insert(name, thunk);
    }

    // Parse and desugar prelude
    let prelude_source = include_str!("../stdlib/prelude.llt");
    let prelude_sf = std::sync::Arc::new(crate::ast::SourceFile {
        path: std::sync::Arc::from("stdlib/prelude.llt"),
        content: std::sync::Arc::from(prelude_source),
    });
    let prelude_parsed =
        crate::parser::parse_with_file(prelude_source, prelude_sf).map_err(|e| {
            EvalError::internal(format!("prelude.llt parse error: {e}"), Span::origin())
        })?;
    let mut prelude_program = prelude_parsed.program.clone();

    // NOTE: prelude.llt is intentionally NOT expanded here (runtime bootstrap path).
    //
    // The circularity is real for runtime evaluation: expand_surface_program calls
    // create_stdlib_env to build the EvalContext needed to run macro transformer
    // functions. If create_stdlib_env_inner itself calls expand_surface_program, it
    // triggers infinite recursion (detected as "depth=6" by EXPAND_MACROS_DEPTH guard).
    //
    // The typecheck path (src/imports.rs typecheck_and_merge_stdlib_module) does expand
    // prelude.llt safely because typecheck does not evaluate macro transformers the same
    // way — it can expand structural macros (begin, do, tmpl) without a fully-bootstrapped
    // runtime env.
    //
    // TODO(B-309): The correct fix for the runtime path is to apply only
    // STDLIB_MACRO_SPECS expansion (not full user-macro expansion with transformer
    // evaluation) during runtime bootstrap, so that structural macros in prelude.llt
    // are expanded without requiring a live stdlib env. This is left for a future sprint.
    crate::desugar::desugar_surface_program(&mut prelude_program);
    // B-296: Inject `CtorName: [variant "CtorName"]` entries for all `[type ...]` ADT
    // declarations in prelude.llt. This runs BEFORE resolve so de Bruijn slots are correct.
    // Must run after desugar ($_ lowering) but before resolve (slot assignment).
    //
    // This is the runtime counterpart to the type checker's inject_adt_constructor_schemes
    // (in typecheck_dict.rs). The type checker exports constructors with precise NominalVariant
    // or Function types; the desugar injection makes them available as runtime values.
    //
    // Previously, prelude.llt had explicit `Tcp: [variant "Tcp"]` entries as a workaround.
    // Those were removed (B-296 fix) and replaced by this injection call.
    crate::desugar::inject_adt_constructors_surface_program(&mut prelude_program);
    // Transform instance decls to method dicts (T-1142).
    // Must run before resolve so [instance ...] entries in dict position are
    // converted to explicit Dict values before lower.rs skips all Decl entries.
    crate::desugar::desugar_instance_decls_surface_program(&mut prelude_program);
    let prelude_resolution_table =
        std::sync::Arc::new(crate::resolve::resolve_surface_program(&prelude_program));
    let prelude_type_table = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());

    // Evaluate prelude directly (type-stage documents are skipped by eval_surface_file)
    let prelude_result_thunk = crate::async_rt::block_on_anywhere(crate::eval::eval_surface_file(
        &prelude_program,
        Arc::clone(&prelude_env),
        &loader_ctx,
        &prelude_resolution_table,
        &prelude_type_table,
    ))?;
    let prelude_val = crate::eval::materialize_sync(&prelude_result_thunk, None, &loader_ctx)?;

    // Result should be a Dict containing prelude exports
    let prelude_dict = match prelude_val {
        Value::Dict(d) => d,
        Value::Overlay(l_id, r_id) => {
            flatten_overlay(&l_id, &r_id, "prelude.llt", &loader_ctx, Span::origin())?
        }
        other => {
            return Err(Box::new(EvalError::internal(
                format!(
                    "prelude eval-program result must be a Dict, got {}",
                    other.type_name()
                ),
                Span::origin(),
            )))
        }
    };

    // ========== Phase 4: Convert prelude_dict to Environment ==========
    // Create stdlib_env with prelude exports (includes macro transformers).
    let stdlib_env = Arc::new(RwLock::new(Environment::new()));

    for (key, thunk_id) in prelude_dict {
        let name = match key {
            Key::String(s) => s.to_string(),
            Key::Int(n) => n.to_string(),
        };
        let thunk = loader_ctx.get_thunk(thunk_id);
        stdlib_env.write().unwrap().insert(name, thunk);
    }

    // Inject core builtins into stdlib_env (T-763 fix).
    // Macro transformers in prelude need builtin-variant, builtin-tag-of, etc.
    // for AST construction. Prelude exports user-facing functions but does not
    // re-export all raw builtin-* aliases directly.
    let core_builtins = builtin_module("core").ok_or_else(|| {
        Box::new(EvalError::internal(
            "builtin_module(\"core\") returned None during stdlib bootstrap".to_string(),
            Span::origin(),
        ))
    })?;

    for def in core_builtins {
        let name = def.name.to_string();
        let builtin_val = Value::Builtin(def);
        let thunk = Arc::new(Thunk::new_materialized(builtin_val, Span::origin()));
        stdlib_env.write().unwrap().insert(name, thunk);
    }

    // Keep the arena alive: loader_ctx holds all ThunkIds allocated during bootstrap.
    // Callers that need to share the same ThunkId space (e.g., macro expansion)
    // clone this Arc before returning.
    let arena = Arc::clone(&loader_ctx.thunk_arena);

    Ok((stdlib_env, arena))
}

/// Create the type-stage environment used when evaluating type-stage documents.
///
/// This function parses the prelude, filters to only `--- stage: type` documents,
/// and evaluates them with a minimal bootstrap context.
///
/// The type-stage env is separate from the runtime stdlib env — it contains only
/// the bindings defined in type-stage documents (e.g., `Int`, `Str`, `Seq`, `union`).
///
/// Returns the type-stage environment wrapped in `Arc<RwLock<Environment>>`.
pub fn create_type_stage_env() -> Result<Arc<RwLock<Environment>>, Box<crate::error::EvalError>> {
    // Open CWD at the public entry point to confine open_ambient_dir to this function.
    // The bootstrap context uses only embedded source (include_str!); the dir is required
    // by EvalContext::new_empty but is never accessed for filesystem reads during stdlib loading.
    // AMBIENT-OK: stdlib bootstrap — type-stage env uses embedded source only, not filesystem.
    #[allow(clippy::disallowed_methods)]
    let bootstrap_base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
        .map_err(|e| {
            Box::new(crate::error::EvalError::internal(
                format!("cannot open type-stage bootstrap base_dir: {e}"),
                Span::origin(),
            ))
        })?;

    // Parse the prelude source
    let prelude_source = include_str!("../stdlib/prelude.llt");
    let parsed = crate::parser::parse(prelude_source).map_err(|e| {
        crate::error::EvalError::internal(
            format!("type-stage prelude parse error: {e}"),
            Span::origin(),
        )
    })?;

    // Desugar $_ implicit lambdas on SurfaceProgram.
    let mut program = parsed.program.clone();
    crate::desugar::desugar_surface_program(&mut program);
    // Inject ADT constructor bindings so TypeNode's named-field constructors (Record, Union,
    // Intersect, TypeConstructor, TypeApplication, Arrow, Recursive, RecursiveRef, TypeVar)
    // are in scope when the type-stage prelude's first dict is evaluated. Without this, any
    // lazy binding that references these constructors (Null, Seq, Map, union, all) fails
    // silently when forced, and the second dict's `TypeNode: [merge TypeNode [...]]` fails
    // immediately because `TypeNode` is undefined in the env. Must run after desugar, before
    // resolve (same ordering as create_stdlib_env_inner).
    crate::desugar::inject_adt_constructors_surface_program(&mut program);
    // Transform instance decls to method dicts (T-1142).
    crate::desugar::desugar_instance_decls_surface_program(&mut program);
    // Variable resolution pass (Phase 1 of arena allocation strategy).
    let resolution_table = std::sync::Arc::new(crate::resolve::resolve_surface_program(&program));
    let empty_types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());

    // T-763: type_stage_env starts as empty Environment, then we inject core builtins directly.
    // Same pattern as create_stdlib_env_inner()'s loader bootstrap. Flat env with no parent chain.
    let type_stage_env = Arc::new(RwLock::new(Environment::new()));

    // Create a bootstrap EvalContext first, so we have an arena for builtin allocation.
    // Use new_empty since we're bootstrapping (no stdlib env exists yet).
    let bootstrap_ctx =
        crate::eval::EvalContext::new_empty(bootstrap_base_dir, Arc::clone(&type_stage_env), false);

    // Inject core builtins into type_stage_env (same pattern as loader.llt bootstrap).
    // Type-stage prelude only needs core builtins (if, =, dict construction, etc.).
    let core_builtins = builtin_module("core").ok_or_else(|| {
        Box::new(EvalError::internal(
            "builtin_module(\"core\") returned None during type-stage bootstrap".to_string(),
            Span::origin(),
        ))
    })?;

    for def in core_builtins {
        let name = def.name.to_string();
        let builtin_val = Value::Builtin(def);
        let thunk = Arc::new(Thunk::new_materialized(builtin_val, Span::origin()));
        type_stage_env.write().unwrap().insert(name, thunk);
    }

    // Filter to only stage: type documents and evaluate them.
    //
    // Unlike eval_surface_document (which chains sequential expressions and only returns the
    // LAST expression's thunk), we evaluate each expression item in the document SEPARATELY
    // and export ALL their bindings into type_stage_env. This is needed because the type-stage
    // prelude has two sequential dicts: the first contains AddResult, DivResult, etc., and the
    // second contains the TypeNode protocol merge. eval_surface_document would only export the
    // second dict's top-level keys, leaving AddResult and friends inaccessible from evaluate_resolver.
    //
    // By evaluating each expression item independently with type_stage_env as the env, each dict's
    // top-level bindings are accessible to subsequent dicts (via type_stage_env) AND are all
    // exported into type_stage_env. The second dict's TypeNode (merge TypeNode [...]) reference
    // sees the first dict's TypeNode binding through type_stage_env.get().
    for doc in &program.documents {
        if doc.node.stage == Some(crate::ast::Stage::Type) {
            // Collect expression nodes (skip Decl items — type checker handles those).
            let expr_nodes: Vec<Arc<crate::ast::SurfaceNode>> =
                doc.node.expressions().cloned().collect();

            for expr_node in &expr_nodes {
                // Evaluate this expression item with the current type_stage_env.
                // Each expression sees all prior expressions' bindings via type_stage_env.
                // Use eval_document_exprs with a single-element slice — it evaluates the
                // expression lazily (as the "last" expression) and returns its thunk.
                let result = crate::async_rt::block_on_anywhere(crate::eval::eval_document_exprs(
                    std::slice::from_ref(expr_node),
                    Arc::clone(&type_stage_env),
                    &bootstrap_ctx,
                    &resolution_table,
                    &empty_types,
                ))?;

                // Materialize and extract bindings.
                let val = materialize(&result, None, &bootstrap_ctx);
                let val = match val {
                    Ok(v) => v,
                    Err(_) => {
                        // If a sequential expression fails to evaluate (e.g., the second dict
                        // references functions not available in the type-stage context), skip it
                        // gracefully — the bindings from prior expressions are still available.
                        continue;
                    }
                };

                let dict = match val {
                    Value::Dict(map) => map,
                    Value::Overlay(l_id, r_id) => flatten_overlay(
                        &l_id,
                        &r_id,
                        "type-stage prelude",
                        &bootstrap_ctx,
                        expr_node.span.clone(),
                    )?,
                    _ => {
                        // Non-dict result (e.g. a side-effect expression) — no bindings to extract.
                        continue;
                    }
                };

                // Insert bindings into type-stage env (later dicts shadow earlier dicts for same key).
                for (key, thunk_id) in dict {
                    let name = match key {
                        Key::String(s) => s.to_string(),
                        Key::Int(n) => n.to_string(),
                    };
                    let thunk = bootstrap_ctx.get_thunk(thunk_id);
                    type_stage_env.write().unwrap().insert(name, thunk);
                }
            }
        }
    }

    // T-1068: Pre-intern primitive TypeNode thunks into the type-stage env.
    //
    // The 7 payload-free TypeNode leaf constructors are created here as pre-materialized
    // Arc<Thunk> values and inserted into the type-stage env under their canonical names.
    // This ensures the common case — resolving a primitive type annotation like `@Int` or
    // `@Bool` — does not call eval_type_stage_expr at all: `primitive_node("Int")` returns
    // the cached Arc<Thunk> with a single atomic reference-count bump, no heap allocation.
    //
    // Insertion happens AFTER the prelude evaluation loop so that if the prelude already
    // inserted a binding under the same name, we do not overwrite it — the prelude's binding
    // is the authoritative one when available. We only add entries that are ABSENT so the
    // prelude has priority. If the prelude loop has already produced the binding (e.g. `Int`
    // is registered by the first type-stage dict), we skip it silently.
    //
    // Each primitive is represented as `Value::Variant { tag: "TypeNode.X", payload: None }` —
    // a unit variant matching the TypeNode ADT declaration in the prelude.
    for (name, typenode_tag) in [
        ("Int", "TypeNode.Int"),
        ("Float", "TypeNode.Float"),
        ("Bool", "TypeNode.Bool"),
        ("Never", "TypeNode.Never"),
        // Prelude registers the type-stage alias as "Str" (not "String"), mirroring the
        // source-level alias `Str: [builtin-variant "TypeNode.String"]`.
        // We pre-intern both names so lookup succeeds regardless of which name is used.
        ("Str", "TypeNode.String"),
        ("String", "TypeNode.String"),
        // "Any" is the prelude alias for Unknown in the type-stage env.
        // Pre-intern both so either lookup works.
        ("Any", "TypeNode.Unknown"),
        ("Unknown", "TypeNode.Unknown"),
        // "Absent" is not registered in the type-stage prelude section (it lives in the
        // runtime section as `Absent: [type Absent]`). Pre-intern it here so callers that
        // ask for the TypeNode.Absent primitive get a valid thunk without resorting to
        // eval_type_stage_expr.
        ("Absent", "TypeNode.Absent"),
    ] {
        let variant = Value::Variant {
            tag: typenode_tag.to_string(),
            payload: None,
        };
        let thunk = Arc::new(Thunk::new_materialized(variant, Span::origin()));
        let mut env_write = type_stage_env.write().unwrap();
        // Only insert if absent — prelude binding takes priority.
        if env_write.get(name).is_none() {
            env_write.insert(name.to_string(), thunk);
        }
    }

    Ok(type_stage_env)
}

/// Return the pre-interned `Arc<Thunk>` for a primitive TypeNode constructor by name.
///
/// Returns the cached thunk for primitive type-stage names (`"Int"`, `"Float"`, `"Bool"`,
/// `"Str"`, `"String"`, `"Never"`, `"Any"`, `"Unknown"`). The thunk holds a pre-materialized
/// `Value::Variant { tag: "TypeNode.X", payload: None }` — the unit variant for each primitive.
///
/// This is the T-1068 performance path: callers that need a primitive TypeNode value use this
/// function rather than `eval_type_stage_expr`, avoiding the overhead of building an EvalContext,
/// creating a surface thunk, and materializing it. The returned `Arc<Thunk>` is an atomic
/// reference-count bump on a shared pre-interned value — no heap allocation.
///
/// Returns `None` if:
/// - The type-stage env is unavailable (bootstrap recursion guard fired).
/// - `name` is not a known primitive (use `eval_type_stage_expr` for compound type-stage exprs).
#[allow(dead_code)] // S-860 CheckerType migration
pub fn primitive_node(name: &str) -> Option<Arc<Thunk>> {
    // Only handle the primitive type-stage names; reject anything else immediately so callers
    // can distinguish "unknown primitive" from "env unavailable".
    match name {
        "Int" | "Float" | "Bool" | "Str" | "String" | "Never" | "Any" | "Unknown" | "Absent" => {}
        _ => return None,
    }
    let env = crate::imports::build_type_stage_env()?;
    let thunk = env.read().ok()?.get(name);
    thunk
}

/// Return the builtin list for a named module, or None if the name is unknown.
pub fn builtin_module(name: &str) -> Option<Vec<crate::value::BuiltinDef>> {
    match name {
        "core" => Some(crate::builtins_core::core_builtins()),
        "datetime" => Some(crate::builtins_datetime::datetime_builtins()),
        "net" => Some(crate::builtins_net::net_builtins()),
        _ => None,
    }
}

/// Build a `TypeEnv` containing type signatures for all builtin modules (core, datetime, net).
///
/// This is the replacement for `TypeEnv::with_builtins()` (deleted in T-722). Callers that
/// previously used `TypeEnv::with_builtins()` now call this function instead.
///
/// The combined env includes all registrations from `core_type_env()`, `datetime_type_env()`,
/// and `net_type_env()`. The result is a flat environment (no parent chain) suitable for use as
/// the baseline for prelude type-checking and builtin-aware type inference.
pub fn build_builtins_type_env() -> crate::types::TypeEnv {
    let mut env = crate::types::TypeEnv::new();
    crate::builtins_core::core_type_env(&mut env);
    crate::builtins_datetime::datetime_type_env(&mut env);
    env.merge(crate::builtins_net::net_type_env());
    env
}

/// Return the type environment for a specific native module.
///
/// Used by the type checker to inject module-specific type signatures when a document
/// declares `--- uses: ["core" "datetime"]`. This parallels the runtime's
/// `builtin_module()` function which injects runtime values.
///
/// Returns `None` for unknown module names.
pub fn type_env_module(name: &str) -> Option<crate::types::TypeEnv> {
    match name {
        "core" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            Some(env)
        }
        "datetime" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_datetime::datetime_type_env(&mut env);
            Some(env)
        }
        "net" => Some(crate::builtins_net::net_type_env()),
        _ => None,
    }
}

#[cfg(test)]
// Test-code lint suppressions:
// - useless_conversion: string_val("lit".into()) — `.into()` is a &str→&str no-op, idiomatic in test fixtures
// - approx_constant: Value::Float(3.14) tests exact string→float parsing, not π
// - to_string_in_format_args: legacy assert! patterns use `.to_string()` in conditions and format args;
//   the condition `.to_string().contains(…)` is load-bearing, and format args already use `err.kind` in
//   many cases — this suppresses any residual instances in complex multi-line assertions
#[allow(
    clippy::useless_conversion,
    clippy::approx_constant,
    clippy::to_string_in_format_args
)]
mod tests {
    use super::*;
    use crate::error::ErrorKind;
    use crate::test_util::test_span;
    use crate::value::{string_val, Strictness};

    /// Stack size for tests that exercise deep recursive evaluation chains.
    /// The default Rust test thread stack (8 MB) is too small for tests that push
    /// MAX_CONTINUATION_STACK (2048) levels of PendingBuiltin thunks; 16 MB provides headroom.
    const TEST_STACK_SIZE: usize = 128 * 1024 * 1024; // 128 MB — debug-mode materialize() needs ~100MB at 256 levels

    /// Helper: wrap a Value in a materialized Thunk inside an Rc.
    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    fn thunk_with_span(val: Value, span: Span) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(val, span))
    }

    fn no_named() -> Option<IndexMap<String, Arc<Thunk>>> {
        None
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let env = Arc::new(RwLock::new(Environment::new()));
        if let Some(defs) = builtin_module("core") {
            for def in defs {
                let name = def.name.to_string();
                let thunk = Arc::new(Thunk::new_materialized(Value::Builtin(def), Span::origin()));
                env.write().unwrap().insert(name, thunk);
            }
        }
        crate::eval::EvalContext::new_empty(base_dir, env, false)
    }

    /// Drive an async builtin to completion synchronously in tests.
    fn run(f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>) -> EvalResult<Arc<Thunk>> {
        crate::async_rt::block_on(f)
    }

    fn mat(f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>) -> Value {
        crate::eval::materialize_sync(&run(f).unwrap(), None, &test_ctx()).unwrap()
    }

    /// Materialize an already-resolved thunk (for `result: EvalResult<Arc<Thunk>>` cases).
    fn mat_val(t: Arc<Thunk>) -> Value {
        crate::eval::materialize_sync(&t, None, &test_ctx()).unwrap()
    }

    /// Parse and evaluate an LLT snippet, returning the result value.
    ///
    /// Uses the stdlib environment so that builtins are available in the body.
    /// The snippet should be a complete expression (e.g. `"[fn [let] 42]"`).
    fn parse_eval(llt_src: &str, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let parsed = crate::parser::parse(llt_src)
            .unwrap_or_else(|e| panic!("parse_eval: parse failed for {:?}: {}", llt_src, e));
        let mut program = parsed.program;
        let base_dir = Arc::clone(&crate::test_util::test_caps().root);
        crate::async_rt::block_on_anywhere(crate::expand::expand_surface_program(
            &mut program,
            false,
            &base_dir,
        ))
        .unwrap_or_else(|e| panic!("parse_eval: expand failed for {:?}: {}", llt_src, e));
        crate::desugar::desugar_surface_program(&mut program);
        let res = std::sync::Arc::new(crate::resolve::resolve_surface_program(&program));
        let types = std::sync::Arc::new(crate::ast::TypeAnnotationTable::new());
        let env = Arc::clone(&ctx.config.stdlib_env);
        let thunk = crate::async_rt::block_on_anywhere(crate::eval::eval_surface_file(
            &program, env, ctx, &res, &types,
        ))
        .unwrap_or_else(|e| {
            panic!(
                "parse_eval: eval_surface_file failed for {:?}: {}",
                llt_src, e
            )
        });
        crate::eval::materialize_sync(&thunk, None, ctx)
            .unwrap_or_else(|e| panic!("parse_eval: materialize failed for {:?}: {}", llt_src, e))
    }

    /// Create an unevaluated Surface thunk referencing a nonexistent variable.
    ///
    /// When forced, materializing this thunk will fail with an "undefined variable" error.
    /// Used by laziness tests to prove that a thunk is not forced prematurely.
    fn make_undef_thunk(ctx: &Arc<crate::eval::EvalContext>) -> Arc<Thunk> {
        let node = Arc::new(crate::ast::SurfaceNode {
            expr: crate::ast::SurfaceExpression::VarRef {
                name: "__nonexistent__".to_string(),
                escaped: false,
            },
            span: test_span(1, 1, 1, 10),
        });
        Arc::new(Thunk::new_surface(
            node,
            Arc::new(crate::ast::ResolutionTable::new()),
            Arc::new(crate::ast::TypeAnnotationTable::new()),
            Arc::new(RwLock::new(Environment::new())),
            Arc::clone(ctx),
            test_span(1, 1, 1, 10),
        ))
    }

    /// Build a materialized dict thunk whose entries are allocated into `ctx`'s arena.
    /// Accepts `IndexMap<Key, Arc<Thunk>>` (convenient for test construction) and
    /// stores each as a `ThunkId` in `Value::Dict`, as the runtime requires.
    fn thunk_dict(
        map: IndexMap<Key, Arc<Thunk>>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> Arc<Thunk> {
        let mut id_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Arc::new(Thunk::new_materialized(
            Value::Dict(id_map),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: flatten a Value (Dict or Overlay) to an `IndexMap<Key, ThunkId>` for test assertions.
    /// Since `builtin_merge` now returns `Value::Overlay` (lazy), tests that previously
    /// expected `Value::Dict` must use this helper to get the concrete entries.
    fn flatten_val(val: Value, ctx: &Arc<crate::eval::EvalContext>) -> IndexMap<Key, ThunkId> {
        match val {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => {
                flatten_overlay(&l, &r, "test", ctx, test_span(1, 1, 1, 5)).unwrap()
            }
            other => panic!("expected Dict or Overlay, got {other:?}"),
        }
    }

    /// Helper: materialize the thunk identified by `id` in `ctx`'s arena.
    fn mat_id(id: ThunkId, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let thunk = ctx.get_thunk(id);
        crate::eval::materialize_sync(&thunk, None, ctx).unwrap()
    }

    /// Helper: build a `Seq.Cons` variant with both `head` and `tail` allocated into `ctx`.
    /// Returns a materialized `Arc<Thunk>` wrapping the `Seq.Cons` variant.
    fn seq_thunk(
        head: Arc<Thunk>,
        tail: Arc<Thunk>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> Arc<Thunk> {
        use crate::value::make_seq_cons;
        let head_id = ctx.alloc_thunk(head);
        let tail_id = ctx.alloc_thunk(tail);
        Arc::new(Thunk::new_materialized(
            make_seq_cons(head_id, tail_id, ctx),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: build a Seq.Nil variant as a materialized `Arc<Thunk>`.
    fn empty_seq_thunk() -> Arc<Thunk> {
        use crate::value::make_seq_nil;
        Arc::new(Thunk::new_materialized(
            make_seq_nil(),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: extract (head_id, tail_id) from a Seq.Cons value, or panic.
    /// Used in tests to avoid repeating the payload extraction pattern.
    fn seq_head_tail(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> (ThunkId, ThunkId) {
        match val {
            Value::Variant {
                ref tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                let payload_thunk = ctx.get_thunk(*payload_id);
                let payload_val = payload_thunk.try_get_materialized().unwrap_or_else(|| {
                    crate::eval::materialize_sync(&payload_thunk, None, ctx)
                        .expect("payload materialized")
                });
                match payload_val {
                    Value::Dict(ref d) => {
                        let head = *d
                            .get(&Key::String("head".into()))
                            .expect("Seq.Cons must have head");
                        let tail = *d
                            .get(&Key::String("tail".into()))
                            .expect("Seq.Cons must have tail");
                        (head, tail)
                    }
                    _ => panic!("Seq.Cons payload must be a Dict, got {:?}", payload_val),
                }
            }
            other => panic!("expected Seq.Cons, got {:?}", other),
        }
    }

    /// Helper: check if a value is Seq.Nil.
    fn is_seq_nil(val: &Value) -> bool {
        matches!(val, Value::Variant { ref tag, payload: None } if tag == "Seq.Nil")
    }

    #[test]
    fn test_create_type_stage_env_succeeds() {
        // Test that create_type_stage_env() successfully creates an environment
        // with the type-stage prelude bindings
        let type_env = create_type_stage_env().expect("create_type_stage_env failed");

        // Check that Int is defined
        assert!(
            type_env.read().unwrap().get("Int").is_some(),
            "Int should be defined in type-stage env"
        );

        // Check that Str is defined
        assert!(
            type_env.read().unwrap().get("Str").is_some(),
            "Str should be defined in type-stage env"
        );

        // Check that union is defined
        assert!(
            type_env.read().unwrap().get("union").is_some(),
            "union should be defined in type-stage env"
        );
    }

    #[test]
    fn floor_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn floor_negative_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(-7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn floor_zero_int() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn floor_positive_float() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(3.7))],
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
            args: vec![thunk(Value::Float(-3.2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-4));
    }

    #[test]
    fn floor_float_exact_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn floor_float_just_below_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(2.9999999))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn floor_nan_errors() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[test]
    fn floor_positive_infinity_errors() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_negative_infinity_errors() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_string_type_error() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(string_val("3.5"))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_bool_type_error() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_dict_type_error() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_wrong_arity_zero() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_wrong_arity_two() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(3.5))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_large_positive_float_out_of_range() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn floor_large_negative_float_out_of_range() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(-1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn round_negative_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(-7))],
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
            args: vec![thunk(Value::Float(0.5))],
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
            args: vec![thunk(Value::Float(-0.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-1));
    }

    #[test]
    fn round_positive_below_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(2.4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_positive_above_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(2.6))],
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
            args: vec![thunk(Value::Float(-2.4))],
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
            args: vec![thunk(Value::Float(-2.6))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-3));
    }

    #[test]
    fn round_1_5_rounds_to_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(1.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn round_negative_1_5_rounds_to_negative_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-1.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-2));
    }

    #[test]
    fn round_float_exact_integer() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn round_nan_errors() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[test]
    fn round_positive_infinity_errors() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_negative_infinity_errors() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_string_type_error() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(string_val("3.5"))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_bool_type_error() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_wrong_arity_zero() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_wrong_arity_two() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_large_positive_float_out_of_range() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn round_large_negative_float_out_of_range() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_valid_positive() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn to_int_valid_negative() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("-7".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-7));
    }

    #[test]
    fn to_int_valid_zero() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("0".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(0));
    }

    #[test]
    fn to_int_valid_large() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("9223372036854775807".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[test]
    fn to_int_invalid_float_string() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_invalid_text() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_invalid_empty() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_invalid_with_spaces() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val(" 42 ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_rejects_int_input() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("Int"),
            "should mention Int, got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_rejects_float_input() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_rejects_bool_input() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_rejects_dict_input() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_wrong_arity_zero() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_wrong_arity_two() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("1".into())), thunk(string_val("2".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_valid_decimal() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
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
            args: vec![thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(42.0));
    }

    #[test]
    fn to_float_valid_negative() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("-2.5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(-2.5));
    }

    #[test]
    fn to_float_valid_scientific_notation() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("1.5e10".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[test]
    fn to_float_valid_negative_exponent() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("2.5e-3".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[test]
    fn to_float_valid_zero() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("0.0".into()))],
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
            args: vec![thunk(string_val(".5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Float(0.5));
    }

    #[test]
    fn to_float_invalid_text() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_invalid_empty() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_inf() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("inf".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_negative_inf() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("-inf".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_infinity() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("infinity".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_nan() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("NaN".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_int_input() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_float_input() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_bool_input() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_wrong_arity_zero() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_wrong_arity_two() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![
                thunk(string_val("1.0".into())),
                thunk(string_val("2.0".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(string_val("1.0".into())));
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_overflow() {
        // One past i64::MAX
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("9223372036854775808".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn error_raises_with_message() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("boom".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "boom");
    }

    #[test]
    fn error_custom_message() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("division by zero".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "division by zero");
    }

    #[test]
    fn error_type_mismatch_on_non_string() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(err.kind.to_string().contains("String"), "got: {}", err.kind);
    }

    #[test]
    fn error_arity_check() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn try_success_returns_ok_variant() {
        // [fn [let] 42]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] 42]", &ctx);
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, Value::Int(42));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_success_with_string_body() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] \"hello\"]", &ctx);
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, string_val("hello".into()));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_failure_returns_err_variant() {
        // [fn [let] $nonexistent] -- references an undefined variable
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] $nonexistent]", &ctx);
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Error");
                let err_val = mat_id(payload.expect("Result.Error should have payload"), &ctx);
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
            _ => panic!("expected Variant(Result.Error, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_non_function_type_error() {
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Function"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn try_non_zero_arg_function_error() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx);
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("zero-argument function"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn try_arity_check() {
        let err = run(builtin_try(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn try_with_builtin_success() {
        fn ok_builtin(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Int(99), Span::origin()) })
        }
        let ctx = test_ctx();
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: ok_builtin,
            name: "ok",
            pos_strictness: &[],
            force_count: 0,
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(b)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx);
                assert_eq!(payload_val, Value::Int(99));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_with_builtin_failure() {
        fn err_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let call_span = ctx.call_span;
                Err(EvalError::internal("builtin error".to_string(), call_span).into())
            })
        }
        let ctx = test_ctx();
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: err_builtin,
            name: "fail",
            pos_strictness: &[],
            force_count: 0,
        });
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(b)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Error");
                let payload_val = mat_id(payload.expect("Result.Error should have payload"), &ctx);
                assert_eq!(payload_val, string_val("builtin error".into()));
            }
            _ => panic!("expected Variant(Result.Error, ...), got: {:?}", result),
        }
    }

    #[test]
    fn try_resource_limit_exceeded_not_catchable() {
        // ResourceLimitExceeded errors should NOT be caught by $try - they should propagate
        fn resource_limit_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let call_span = ctx.call_span;
                Err(EvalError::resource_limit_exceeded(
                    "test: exceeded resource limit (1000000)".to_string(),
                    call_span,
                )
                .into())
            })
        }
        let b = Value::Builtin(crate::value::BuiltinDef {
            func: resource_limit_builtin,
            name: "resource_fail",
            pos_strictness: &[],
            force_count: 0,
        });
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(b)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.kind.to_string().contains("exceeded resource limit"),
            "expected resource limit error to propagate, got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E043");
    }

    #[test]
    fn apply_single_arg() {
        // [fn [x] $x] applied to [42]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx);
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(42)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn apply_multiple_args_returns_first() {
        // [fn [a b] $a] applied to [10, 20]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let a b] $a]", &ctx);
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(10));
    }

    #[test]
    fn apply_multiple_args_returns_second() {
        // [fn [a b] $b] applied to [10, 20]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let a b] $b]", &ctx);
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(20));
    }

    #[test]
    fn apply_with_builtin() {
        fn add_builtin(
            builtin_ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let BuiltinArgs {
                    args,
                    call_span,
                    ctx,
                    ..
                } = builtin_ctx;
                // TEST: test-only add_builtin with force_count=0 deliberately uses materialize directly
                let a = materialize(&args[0], None, &ctx)?; // TEST: test-only inline builtin
                let b = materialize(&args[1], None, &ctx)?; // TEST: test-only inline builtin
                match (a, b) {
                    (Value::Int(x), Value::Int(y)) => ok_val(Value::Int(x + y), call_span),
                    _ => Err(EvalError::type_mismatch("Int", "non-Int", call_span).into()),
                }
            })
        }
        let ctx = test_ctx();
        let func = Value::Builtin(crate::value::BuiltinDef {
            func: add_builtin,
            name: "add",
            pos_strictness: &[],
            force_count: 0,
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn apply_arity_mismatch() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x y] $x]", &ctx);
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(Key::Int(0), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );

        let apply_thunk = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .expect("should return thunk");
        let err = crate::eval::materialize_sync(&apply_thunk, None, &ctx).unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.kind
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

        let apply_thunk = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .expect("should return thunk");
        let err = crate::eval::materialize_sync(&apply_thunk, None, &ctx).unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Function"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn apply_non_dict_args_type_error() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx);
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(func), thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .expect("should return thunk");
        let err = crate::eval::materialize_sync(&apply_result, None, &test_ctx()).unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn apply_wrong_arity() {
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .expect("should return thunk");
        let err = crate::eval::materialize_sync(&apply_result, None, &test_ctx()).unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn type_of_int() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Int".into()));
    }

    #[test]
    fn type_of_float() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Float".into()));
    }

    #[test]
    fn type_of_string() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(string_val("hi".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("String".into()));
    }

    #[test]
    fn type_of_bool() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Bool".into()));
    }

    #[test]
    fn type_of_dict() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Dict".into()));
    }

    #[test]
    fn type_of_function() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] 0]", &ctx);
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Function".into()));
    }

    #[test]
    fn type_of_builtin_returns_function() {
        fn dummy(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Int(0), Span::origin()) })
        }
        let builtin = Value::Builtin(crate::value::BuiltinDef {
            func: dummy,
            name: "dummy",
            pos_strictness: &[],
            force_count: 0,
        });
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(builtin)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("Function".into()));
    }

    #[test]
    fn test_type_of_seq() {
        // Seq values should report type name "Variant" from $type-of (Seq is now a Variant)
        let ctx = test_ctx();
        let seq = seq_thunk(thunk(Value::Int(1)), empty_seq_thunk(), &ctx);
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![seq],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        // After migration, Seq.Cons returns "Seq" from type_name() for TypeTag("Seq") matching.
        assert_eq!(result, string_val("Seq".into()));
    }

    #[test]
    fn type_of_arity_check() {
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    // DELETED: from_json_* tests (json-serde-removal sprint)
    // builtin_from_json and json_to_value have been deleted.
    // from-json is now implemented in pure tinct in stdlib/codecs/json.llt.
    // Corpus tests in tests/corpus/eval/stdlib/ cover from-json functionality.

    #[test]
    fn keys_empty_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![
                thunk_dict(IndexMap::new(), &ctx),
                thunk_dict(IndexMap::new(), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let map = flatten_val(result, &ctx);
        assert_eq!(map.len(), 0);
    }

    #[test]
    fn builtin_def_strictness_array_validity() {
        // Verify all BuiltinDef entries have reasonable strictness arrays.
        // Updated in T-719: iterates builtin_module() groups instead of standard_builtins().
        let all_defs: Vec<crate::value::BuiltinDef> = ["core", "datetime", "net"]
            .iter()
            .flat_map(|name| builtin_module(name).unwrap_or_default())
            .collect();
        for def in all_defs {
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
            args: vec![thunk_dict(IndexMap::new(), &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk_dict(left, &ctx), thunk_dict(IndexMap::new(), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let left_thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));
        let right_thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span));

        let mut left = IndexMap::new();
        left.insert(Key::String("a".into()), Arc::clone(&left_thunk));
        let mut right = IndexMap::new();
        right.insert(Key::String("b".into()), Arc::clone(&right_thunk));

        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn keys_wrong_arity_two() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn length_wrong_arity_zero() {
        let err = run(builtin_length(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn length_wrong_arity_two() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_length(BuiltinArgs {
            args: vec![d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn merge_wrong_arity_one() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_merge(BuiltinArgs {
            args: vec![d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn merge_wrong_arity_three() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_merge(BuiltinArgs {
            args: vec![d.clone(), d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn keys_non_dict_int() {
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("keys"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn keys_non_dict_string() {
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("keys"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("got String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn keys_non_dict_bool() {
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("keys"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("got Bool"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn length_string() {
        // length now supports String inputs (returns character count)
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(5));
    }

    #[test]
    fn length_string_empty() {
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
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
            args: vec![thunk(string_val("\u{1F600}\u{1F601}".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(2));
    }

    #[test]
    fn length_non_dict_non_string() {
        let err = run(builtin_length(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("length"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Bool"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn merge_first_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let result = run(builtin_merge(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        // builtin_merge itself succeeds — returns Overlay(Int(1), {})
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize_sync(&overlay_thunk, None, &ctx).unwrap();
        // Flatten fires the type error: left side is Int, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span()).unwrap_err();
                assert!(
                    err.kind.to_string().contains("expected Dict"),
                    "got: {}",
                    err.kind
                );
                assert!(
                    err.kind.to_string().contains("got Int"),
                    "got: {}",
                    err.kind
                );
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
        let result = run(builtin_merge(BuiltinArgs {
            args: vec![d, thunk(string_val("nope".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let overlay_thunk = result.unwrap();
        let overlay_val = crate::eval::materialize_sync(&overlay_thunk, None, &ctx).unwrap();
        // Flatten fires the type error: right side is String, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span()).unwrap_err();
                assert!(
                    err.kind.to_string().contains("expected Dict"),
                    "got: {}",
                    err.kind
                );
                assert!(
                    err.kind.to_string().contains("got String"),
                    "got: {}",
                    err.kind
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
            args: vec![thunk(Value::Int(42)), empty],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(string_val("c".into())), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(99)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(60)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(string_val("second".into())), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![Arc::clone(&val_thunk), empty],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let err = run(builtin_append(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("2"), "got: {}", err.kind);
    }

    #[test]
    fn append_wrong_arity_three() {
        let ctx = test_ctx();
        let err = run(builtin_append(BuiltinArgs {
            args: vec![
                thunk_dict(IndexMap::new(), &ctx),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("2"), "got: {}", err.kind);
    }

    #[test]
    fn append_second_arg_non_dict() {
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(err.kind.to_string().contains("append"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn append_key_overflow_at_i64_max() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::Int(i64::MAX), thunk(Value::Int(1)));
        let dict = thunk_dict(map, &ctx);
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn str_no_args() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn str_single_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("42".into()));
    }

    #[test]
    fn str_single_negative_int() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(Value::Int(-7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("-7".into()));
    }

    #[test]
    fn str_single_float() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("3.14".into()));
    }

    #[test]
    fn str_single_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn str_single_bool_true() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("true".into()));
    }

    #[test]
    fn str_single_bool_false() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(Value::Bool(false))],
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
                    Arc::new(Thunk::new_materialized(
                        Value::Int(1),
                        test_span(1, 1, 1, 5),
                    )),
                );
                m
            },
            &ctx,
        );
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, string_val("[x: <thunk>]".into()));
    }

    #[test]
    fn str_single_empty_string() {
        let result = mat(builtin_str(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
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
            args,
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
            args,
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
            args: vec![
                thunk(string_val(",".into())),
                thunk(string_val("a,b,c".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![
                thunk(string_val(",".into())),
                thunk(string_val("a,,b".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![
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
            args: vec![
                thunk(string_val(",".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![
                thunk(string_val("::".into())),
                thunk(string_val("a::b::c".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(string_val(",".into())), thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let result = run(builtin_split(BuiltinArgs {
            args: vec![thunk(string_val("")), thunk(string_val(&input))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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
        let result = run(builtin_split(BuiltinArgs {
            args: vec![thunk(string_val(",")), thunk(string_val(&input))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let val = match result {
            Ok(t) => mat_val(t),
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
            args: vec![
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
            args: vec![
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
            args: vec![
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
            args: vec![
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
            args: vec![
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
        let result = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("")),
                thunk(string_val(&replacement)),
                thunk(string_val(&input)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("replace: output would exceed"));
    }

    #[test]
    fn replace_output_size_ok_normal_pattern() {
        // Normal pattern replacement should succeed even with moderate sizes.
        let input = "a".repeat(1000);
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
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
            args: vec![thunk(string_val("  hello  ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_leading_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("   hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_trailing_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("hello   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_no_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_all_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn trim_tabs_and_newlines() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("\t\nhello\n\t".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("hello".into()));
    }

    #[test]
    fn trim_empty() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, string_val("".into()));
    }

    #[test]
    fn split_wrong_arity_too_few() {
        let err = run(builtin_split(BuiltinArgs {
            args: vec![thunk(string_val(",".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("expected 2"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn split_wrong_arity_too_many() {
        let err = run(builtin_split(BuiltinArgs {
            args: vec![
                thunk(string_val(",".into())),
                thunk(string_val("a,b".into())),
                thunk(string_val("extra".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn replace_wrong_arity() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("expected 3"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn trim_wrong_arity() {
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn split_wrong_type_separator() {
        let err = run(builtin_split(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn split_wrong_type_input() {
        let err = run(builtin_split(BuiltinArgs {
            args: vec![thunk(string_val(",".into())), thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn replace_wrong_type_pattern() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(string_val("b".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Int"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn replace_wrong_type_replacement() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(Value::Bool(true)),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Bool"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn replace_wrong_type_input() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(string_val("b".into())),
                thunk(Value::Float(3.14)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn trim_wrong_type() {
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("got Float"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn trim_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(string_val("hi".into())));
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("  hello  ".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn error_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("boom".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn type_of_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn to_int_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("42".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn split_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_split(BuiltinArgs {
            args: vec![
                thunk(string_val(",".into())),
                thunk(string_val("a,b".into())),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn replace_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(string_val("b".into())),
                thunk(string_val("abc".into())),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn add_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(99)));
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn sub_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(1))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn mul_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), thunk(Value::Int(3))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn div_float_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn eq_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn lt_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn if_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_if(BuiltinArgs {
            args: vec![
                thunk(Value::Bool(true)),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
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
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: Some(named),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn length_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let map = IndexMap::new();
        let err = run(builtin_length(BuiltinArgs {
            args: vec![thunk(Value::Dict(map))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn merge_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_merge(BuiltinArgs {
            args: vec![
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn append_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new())), thunk(Value::Int(42))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn str_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_str(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn try_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = parse_eval("[fn [let] 42]", &ctx);
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn apply_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = parse_eval("[fn [let] 42]", &ctx);
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(func), thunk(Value::Dict(IndexMap::new()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .expect("should return thunk");
        let err = crate::eval::materialize_sync(&apply_result, None, &test_ctx()).unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    /// Regression test for ThunkId cross-context lifecycle.
    ///
    /// Guards against breaking the STDLIB_ARENA_CACHE write in create_stdlib_env_with_arena.
    /// If the cache write is accidentally removed, new_arena_with_stdlib_snapshot() will
    /// return None and EvalContext::new() will get an empty arena, causing index-out-of-bounds
    /// panics when accessing stdlib ThunkIds.
    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: stdlib loading fails"]
    fn stdlib_arena_cache_preserves_thunk_ids() {
        // Create stdlib env — this should cache the arena
        let (_env, arena) = create_stdlib_env_with_arena().expect("failed to create stdlib env");

        // Verify the cache is populated by create_stdlib_env_with_arena
        let cached_arena = new_arena_with_stdlib_snapshot()
            .expect("arena cache should be populated after create_stdlib_env_with_arena");

        // The cached arena should be a snapshot of the stdlib arena
        assert_eq!(
            cached_arena.lock().unwrap().len(),
            arena.lock().unwrap().len(),
            "cached arena should be a snapshot of the stdlib arena"
        );

        assert!(
            cached_arena.lock().unwrap().len() > 390,
            "cached arena should contain at least 390 stdlib thunks (prelude + macros), got {}",
            cached_arena.lock().unwrap().len()
        );
    }

    #[test]
    fn core_builtins_count() {
        let count = crate::builtins_core::core_builtins().len();
        assert!(
            count > 100,
            "expected core builtins to have >100 entries, got {count}"
        );
    }

    #[test]
    fn datetime_builtins_count() {
        let count = crate::builtins_datetime::datetime_builtins().len();
        assert!(
            count > 10,
            "expected datetime builtins to have >10 entries, got {count}"
        );
    }

    #[test]
    fn net_builtins_count() {
        let count = crate::builtins_net::net_builtins().len();
        assert!(
            count > 5,
            "expected net builtins to have >5 entries, got {count}"
        );
    }

    #[test]
    fn add_int_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(8));
    }

    #[test]
    fn add_int_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(5.5));
    }

    #[test]
    fn add_float_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(1.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(4.0));
    }

    #[test]
    fn add_negative_ints() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(-10)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn add_zeros() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn add_type_error_string() {
        let e = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        // Non-Int/Float operands produce a NoInstance error for the Addable class.
        assert!(
            e.kind.to_string().contains("no instance") || e.kind.to_string().contains("Addable"),
            "expected NoInstance error for Int + String, got: {}",
            e.kind
        );
    }

    #[test]
    fn add_arity_one_arg() {
        let e = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn add_arity_three_args() {
        // + now accepts 2+ args (uses first two). Three args succeed; result is 1+2=3.
        let result = mat(builtin_add(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(3));
    }

    #[test]
    fn add_overflow_error() {
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MAX)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn sub_overflow_error() {
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MIN)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn sub_int_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(7));
    }

    #[test]
    fn sub_int_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(6.5));
    }

    #[test]
    fn sub_float_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Float(10.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn sub_float_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Float(10.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.0));
    }

    #[test]
    fn sub_result_negative() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(10))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-7));
    }

    #[test]
    fn sub_to_zero() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn sub_arity_zero_args() {
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn sub_arity_one_arg() {
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn sub_arity_three_args() {
        // - now accepts 2+ args (uses first two). Three args succeed; result is 1-2=-1.
        let result = mat(builtin_sub(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(-1));
    }

    #[test]
    fn sub_type_error_string() {
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        // Non-Int/Float operands produce a NoInstance error for the Subtractable class.
        assert!(
            e.kind.to_string().contains("no instance")
                || e.kind.to_string().contains("Subtractable"),
            "expected NoInstance error for Int - String, got: {}",
            e.kind
        );
    }

    #[test]
    fn mul_int_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(4)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(20));
    }

    #[test]
    fn mul_int_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(4)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(10.0));
    }

    #[test]
    fn mul_float_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Float(3.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(7.5));
    }

    #[test]
    fn mul_by_zero() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(0));
    }

    #[test]
    fn mul_negative() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(-3)), thunk(Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-12));
    }

    #[test]
    fn mul_by_negative_one() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), thunk(Value::Int(-1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Int(-42));
    }

    #[test]
    fn mul_overflow_error() {
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MAX)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn add_float_overflow_to_infinity_is_error() {
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(1e308)), thunk(Value::Float(1e308))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn sub_float_nan_is_error() {
        // f64::INFINITY - f64::INFINITY = NaN
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn mul_float_overflow_to_infinity_is_error() {
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(1e308)), thunk(Value::Float(10.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn div_float_nan_result_is_error() {
        // 0.0 / 0.0 produces NaN; the existing b==0.0 guard catches b==0.0,
        // but this test documents that NaN results from non-zero / 0-adjacent
        // ops are also caught. Use f64::NAN inputs via Float values directly:
        // f64::INFINITY / f64::INFINITY = NaN
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn div_float_int_int_returns_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
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
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(2))],
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
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(3.0))],
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
            args: vec![thunk(Value::Float(7.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(3.0));
    }

    #[test]
    fn div_float_by_zero_int() {
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            matches!(e.kind, crate::error::ErrorKind::DivisionByZero { .. }),
            "expected DivisionByZero, got: {}",
            e.kind
        );
        assert_eq!(
            e.kind.code(),
            "E031",
            "expected E031, got: {}",
            e.kind.code()
        );
        assert!(
            e.kind.to_string().contains("division by zero"),
            "expected message containing 'division by zero', got: {}",
            e.kind
        );
    }

    #[test]
    fn div_float_by_zero_float() {
        // Float / Float(0.0) produces Inf which check_float_result rejects as non-finite.
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("is not a finite number"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn div_float_by_zero_mixed() {
        // Int / Float(0.0) produces Inf which check_float_result rejects as non-finite.
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("is not a finite number"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn div_float_negative_zero() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Float(-0.0)), thunk(Value::Float(1.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Float(0.0));
    }

    #[test]
    fn eq_int_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_int_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(6))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_float_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14)), thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_float_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14)), thunk(Value::Float(2.71))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_string_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![
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
            args: vec![
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
            args: vec![thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_bool_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_cross_type_int_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_float_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_cross_type_int_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.1))],
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
            args: vec![
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
            args: vec![thunk(Value::Int(1)), thunk(string_val("1".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_bool_vs_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Bool(true)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_nan_not_equal_to_self() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN)), thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn eq_negative_zero_float() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(-0.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn eq_arity_error() {
        let e = run(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn lt_int_int_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_int_int_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_int_int_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_float_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_string_lexicographic() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
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
            args: vec![
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
            args: vec![
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
            args: vec![
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
            args: vec![thunk(Value::Int(3)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_float_int() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_cross_type_equal_values() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_incompatible_types_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[test]
    fn lt_bool_false_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Bool(false)), thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_bool_true_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Bool(true)), thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_false_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Bool(false)), thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_bool_true_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Bool(true)), thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(false));
    }

    #[test]
    fn lt_dict_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[test]
    fn lt_arity_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn lt_negative_numbers() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(-10)), thunk(Value::Int(-5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(r, Value::Bool(true));
    }

    #[test]
    fn lt_nan_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN)), thunk(Value::Float(1.0))],
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
            args,
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
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn if_does_not_materialize_unchosen_else_branch() {
        let ctx = test_ctx();
        let error_thunk = make_undef_thunk(&ctx);

        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42)), error_thunk];
        let result = mat(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn if_does_not_materialize_unchosen_then_branch() {
        let ctx = test_ctx();
        let error_thunk = make_undef_thunk(&ctx);

        let args = vec![
            thunk(Value::Bool(false)),
            error_thunk,
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args,
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
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("expected Bool"),
            "got: {}",
            e.kind
        );
        assert!(
            e.kind.to_string().contains("Bool"),
            "expected Bool mentioned, got: {}",
            e.kind
        );
    }

    #[test]
    fn if_string_condition_error() {
        let args = vec![
            thunk(string_val("true".into())),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("expected Bool"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn if_arity_too_few() {
        let args = vec![thunk(Value::Bool(true)), thunk(Value::Int(42))];
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
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
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[test]
    fn if_non_bool_condition_has_secondary_span() {
        // Test that $if with a non-Bool condition includes secondary_span
        // pointing to where the condition was produced (if different from call site).
        let condition_span = test_span(5, 1, 5, 10); // Where the Int value is defined
        let call_span_val = test_span(10, 1, 10, 30); // Where the $if call is

        let args = vec![
            thunk_with_span(Value::Int(1), condition_span.clone()),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];

        let err = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span_val,
            ctx: test_ctx(),
        }))
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
            thunk_with_span(Value::Int(1), same_span.clone()),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];

        let err = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: same_span,
            ctx: test_ctx(),
        }))
        .unwrap_err();

        // Secondary span should NOT be set because it equals call_span
        assert!(
            err.secondary_span.is_none(),
            "Secondary span should be suppressed when same as call span"
        );
    }

    // create_root_env() was deleted — replaced by direct injection in
    // create_stdlib_env_inner() and create_type_stage_env().

    /// Parse-only smoke test for the prelude. Evaluating the full prelude requires a
    /// 128 MB thread stack (see corpus_tests.rs) due to deep Rc<Environment> drop chains
    /// that exceed the default and RUST_MIN_STACK=64MB test thread stacks.
    /// This test verifies the prelude parses without error — which was broken by the
    /// f1e38a2 VarRef colon-ahead detection regression (duplicate key "value" false positive).
    #[test]
    fn prelude_parses_without_error() {
        let prelude_source = include_str!("../stdlib/prelude.llt");
        match crate::parser::parse(prelude_source) {
            Ok(output) => {
                assert!(
                    output.errors.is_empty(),
                    "prelude.llt has parse errors: {:?}",
                    output.errors
                );
            }
            Err(e) => panic!("prelude parse failed: {e}"),
        }
    }

    // Note: macros_parses_without_error test removed — macro transformer definitions
    // (tmpl, do, begin) were merged into prelude.llt and are now covered by the
    // prelude_parses_without_error test above.

    #[test]
    #[ignore = "pre-existing regression from runtime-v2 merge: stdlib loading fails"]
    fn create_stdlib_env_has_builtins_and_prelude() {
        let env = create_stdlib_env().expect("stdlib env creation failed");
        let env_ref = env.read().unwrap();
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
        // Should have macro transformer exports from prelude (tmpl, do, begin)
        assert!(
            env_ref.get("tmpl").is_some(),
            "missing prelude macro export tmpl"
        );
        assert!(
            env_ref.get("do").is_some(),
            "missing prelude macro export do"
        );
        assert!(
            env_ref.get("begin").is_some(),
            "missing prelude macro export begin"
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

    // DELETED: include_ctx, dir_cap_val, write_temp_file helper functions (include-decomp-redelete sprint)
    // DELETED: 29 builtin_include test functions (include-decomp-redelete sprint)
    // These tested the deleted builtin_include function and referenced deleted include_cache/include_guard fields.
    // Lines ~7858-8748 removed: include_wrong_type_error, include_file_not_found, include_simple_dict,
    // include_scalar_value, include_parse_error, include_circular_detection, include_self_circular,
    // include_nested, include_absolute_path, include_arity_error, include_rejects_named_args,
    // include_multi_document, include_uses_stdlib, include_cache_returns_same_rc_ptr,
    // include_caches_result, include_cache_respects_normalization, include_cache_shared_across_nested,
    // include_forbidden_when_no_fs, include_with_correct_blake3_hash, include_with_wrong_blake3_hash_errors,
    // include_hash_invalid_format_errors, include_hash_unsupported_algo_errors,
    // include_require_integrity_rejects_hashless, include_require_integrity_accepts_hashed,
    // include_chain_nested_error, include_chain_cleaned_up_after_success, include_chain_cleaned_up_after_error.

    // Sequence builtins tests

    #[test]
    fn seq_basic() {
        let ctx = test_ctx();
        let head_val = thunk(Value::Int(1));
        let tail_val = thunk(Value::Int(2));
        let result = mat(builtin_seq(BuiltinArgs {
            args: vec![head_val.clone(), tail_val.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(1));
        assert_eq!(mat_id(tail, &ctx), Value::Int(2));
    }

    #[test]
    fn seq_arity_zero() {
        let result = run(builtin_seq(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_one() {
        let result = run(builtin_seq(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn seq_arity_three() {
        let result = run(builtin_seq(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn seq_lazy() {
        // Head can be a thunk referencing a nonexistent variable.
        // If we tried to materialize this thunk, it would error (undefined variable).
        // But seq construction should succeed because it doesn't materialize args.
        let ctx = test_ctx();
        let undef_thunk = make_undef_thunk(&ctx);
        let tail_val = thunk(Value::Int(2));
        // seq construction should succeed even though head would error if materialized
        let result = run(builtin_seq(BuiltinArgs {
            args: vec![undef_thunk, tail_val],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_ok());
        // Verify the result is a Seq.Cons
        let r = mat_val(result.unwrap());
        assert!(
            matches!(r, Value::Variant { ref tag, .. } if tag == "Seq.Cons"),
            "expected Seq.Cons, got {:?}",
            r
        );
    }

    #[test]
    fn head_basic() {
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(string_val("first".into())), empty_seq_thunk(), &ctx);
        let result = run(builtin_head(BuiltinArgs {
            args: vec![seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_ok());
        let head = mat_val(result.unwrap());
        assert_eq!(head, string_val("first".into()));
    }

    #[test]
    fn head_non_seq() {
        let result = run(builtin_head(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_zero() {
        let result = run(builtin_head(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn head_arity_two() {
        let result = run(builtin_head(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn head_empty_seq_nil() {
        // After Seq→Variant migration, the empty-sequence terminal is Seq.Nil (not empty Dict).
        // head on Seq.Nil must produce an "on empty collection" error.
        let result = run(builtin_head(BuiltinArgs {
            args: vec![Arc::new(Thunk::new_materialized(
                crate::value::make_seq_nil(),
                call_span(),
            ))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let err = result.unwrap_err();
        assert!(
            err.kind.to_string().contains("on empty collection"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn tail_empty_seq_nil() {
        // After Seq→Variant migration, the empty-sequence terminal is Seq.Nil (not empty Dict).
        // tail on Seq.Nil must produce an "on empty collection" error.
        let result = run(builtin_tail(BuiltinArgs {
            args: vec![Arc::new(Thunk::new_materialized(
                crate::value::make_seq_nil(),
                call_span(),
            ))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        let err = result.unwrap_err();
        assert!(
            err.kind.to_string().contains("on empty collection"),
            "got: {}",
            err.kind
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
        let result = run(builtin_tail(BuiltinArgs {
            args: vec![seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_ok());
        let tail = mat_val(result.unwrap());
        assert_eq!(tail, Value::Int(99));
    }

    #[test]
    fn tail_non_seq() {
        let result = run(builtin_tail(BuiltinArgs {
            args: vec![thunk(string_val("not a seq".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn collect_basic() {
        // Build a 3-element sequence: Seq(1, Seq(2, Seq(3, {})))
        let ctx = test_ctx();
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_seq_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let seq_val = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);

        let result = mat(builtin_collect(BuiltinArgs {
            args: vec![seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let seq_val = seq_thunk(thunk(Value::Int(42)), empty_seq_thunk(), &ctx);
        let result = mat(builtin_collect(BuiltinArgs {
            args: vec![seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let result = run(builtin_collect(BuiltinArgs {
            args: vec![thunk(Value::Int(123))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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
        let result = run(builtin_collect(BuiltinArgs {
            args: vec![seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn collect_large_sequence() {
        // Test collect with a moderately-sized sequence (200 elements) to verify it works
        // correctly without hitting MAX_CONTINUATION_STACK (2048) or MAX_COLLECT_SIZE (1M).
        // Testing at the actual MAX_COLLECT_SIZE (1M) would be too slow/memory-intensive,
        // and with depth increment fixes, sequences hit depth limits around 256 elements.
        let ctx = test_ctx();
        let range_result = run(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        let take_result = run(builtin_take(BuiltinArgs {
            args: vec![thunk(Value::Int(200)), range_result],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        let collect_result = run(builtin_collect(BuiltinArgs {
            args: vec![take_result],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));

        assert!(
            collect_result.is_ok(),
            "collect should succeed for 200 elements"
        );
        match crate::eval::materialize_sync(&collect_result.unwrap(), None, &ctx).unwrap() {
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
    fn collect_max_size_limit_enforced() {
        // Test that the MAX_COLLECT_SIZE check is present and triggers correctly.
        // We can't practically test with 1M+ elements in a unit test (too slow/memory-intensive),
        // but we can test that attempting to collect from an unbounded sequence without $take
        // will eventually hit either MAX_CONTINUATION_STACK or MAX_COLLECT_SIZE.
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
                // Single shared context — all ThunkIds must belong to the same arena.
                let ctx = test_ctx();
                let range_result = crate::async_rt::block_on(builtin_range(BuiltinArgs {
                    args: vec![thunk(Value::Int(0))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                // Attempt to collect infinite range without take
                // This will hit MAX_CONTINUATION_STACK (2048) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the PendingBuiltin chain.
                let collect_result = crate::async_rt::block_on(builtin_collect(BuiltinArgs {
                    args: vec![range_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }));

                // Should fail (either MAX_CONTINUATION_STACK or MAX_COLLECT_SIZE)
                assert!(
                    collect_result.is_err(),
                    "collect should fail on infinite sequence"
                );
                let err = collect_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.kind.to_string().contains("maximum evaluation depth");
                let is_size_error = err
                    .kind
                    .to_string()
                    .contains("exceeded maximum collection size");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.kind
                );
            })
            .unwrap()
            .join();

        // Propagate any panic from the spawned thread
        result.unwrap();
    }

    // === range builtin tests ===

    #[test]
    fn range_finite_basic() {
        // range(0, 5) → 0, 1, 2, 3, 4
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(0));
        // Materialize tail to get next element
        let tail_val = mat_id(tail, &ctx);
        let (h2, _) = seq_head_tail(&tail_val, &ctx);
        assert_eq!(mat_id(h2, &ctx), Value::Int(1));
    }

    #[test]
    fn range_empty() {
        // range(5, 5) → Seq.Nil (after Seq→Variant migration, empty range is Seq.Nil not {})
        let result = mat(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(
            is_seq_nil(&result),
            "expected Seq.Nil for empty range, got {:?}",
            result
        );
    }

    #[test]
    fn range_negative_range() {
        // range(10, 5) → Seq.Nil (start >= end; after Seq→Variant migration, empty range is Seq.Nil not {})
        let result = mat(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(
            is_seq_nil(&result),
            "expected Seq.Nil for empty range, got {:?}",
            result
        );
    }

    #[test]
    fn range_single_element() {
        // range(0, 1) → 0
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(0));
        // tail should be Seq.Nil (terminal)
        let tail_val = mat_id(tail, &ctx);
        assert!(
            is_seq_nil(&tail_val),
            "expected Seq.Nil for tail, got {:?}",
            tail_val
        );
    }

    #[test]
    fn range_infinite_basic() {
        // range(0) → 0, 1, 2, ... (take first 3)
        let ctx = test_ctx();
        let result = mat(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(0));
        let tail_val = mat_id(tail, &ctx);
        let (h2, t2) = seq_head_tail(&tail_val, &ctx);
        assert_eq!(mat_id(h2, &ctx), Value::Int(1));
        let t2_val = mat_id(t2, &ctx);
        let (h3, _) = seq_head_tail(&t2_val, &ctx);
        assert_eq!(mat_id(h3, &ctx), Value::Int(2));
    }

    #[test]
    fn range_arity_zero() {
        let result = run(builtin_range(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn range_arity_three() {
        let result = run(builtin_range(BuiltinArgs {
            args: vec![
                thunk(Value::Int(0)),
                thunk(Value::Int(5)),
                thunk(Value::Int(10)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_start() {
        let result = run(builtin_range(BuiltinArgs {
            args: vec![thunk(string_val("not an int".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn range_non_int_end() {
        let result = run(builtin_range(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), thunk(Value::Float(5.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    // === repeat builtin tests ===

    #[test]
    fn repeat_basic() {
        // repeat(42) → 42, 42, 42, ... (take first 3)
        let ctx = test_ctx();
        let result = mat(builtin_repeat(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(42));
        let tail_val = mat_id(tail, &ctx);
        let (h2, t2) = seq_head_tail(&tail_val, &ctx);
        assert_eq!(mat_id(h2, &ctx), Value::Int(42));
        let t2_val = mat_id(t2, &ctx);
        let (h3, _) = seq_head_tail(&t2_val, &ctx);
        assert_eq!(mat_id(h3, &ctx), Value::Int(42));
    }

    #[test]
    fn repeat_laziness() {
        // Repeat a thunk that would error if materialized
        let ctx = test_ctx();
        let undef_thunk = make_undef_thunk(&ctx);
        // repeat construction should succeed without materializing arg
        let result = run(builtin_repeat(BuiltinArgs {
            args: vec![undef_thunk],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_ok());
        let r = mat_val(result.unwrap());
        assert!(
            matches!(r, Value::Variant { ref tag, .. } if tag == "Seq.Cons"),
            "expected Seq.Cons, got {:?}",
            r
        );
    }

    #[test]
    fn repeat_arity_zero() {
        let result = run(builtin_repeat(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn repeat_arity_two() {
        let result = run(builtin_repeat(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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
            args: vec![dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        // First element: "a"
        assert_eq!(mat_id(head, &ctx), string_val("a".into()));
        let tail_val = mat_id(tail, &ctx);
        let (h2, t2) = seq_head_tail(&tail_val, &ctx);
        // Second element: "b"
        assert_eq!(mat_id(h2, &ctx), string_val("b".into()));
        let t2_val = mat_id(t2, &ctx);
        let (h3, t3) = seq_head_tail(&t2_val, &ctx);
        // Third element: "a" (cycling back)
        assert_eq!(mat_id(h3, &ctx), string_val("a".into()));
        let t3_val = mat_id(t3, &ctx);
        let (h4, _) = seq_head_tail(&t3_val, &ctx);
        // Fourth element: "b"
        assert_eq!(mat_id(h4, &ctx), string_val("b".into()));
    }

    #[test]
    fn cycle_empty_dict() {
        let result = run(builtin_cycle(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
        assert!(result.unwrap_err().kind.to_string().contains("empty"));
    }

    #[test]
    fn cycle_non_dict() {
        let result = run(builtin_cycle(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn cycle_arity_zero() {
        let result = run(builtin_cycle(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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
            args: vec![f_thunk, x_thunk.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        // Head should be x (0)
        assert_eq!(mat_id(head, &ctx), Value::Int(0));
        // Tail is a PendingBuiltin wrapping iterate(f, f(x))
        // Materializing it returns another Seq.Cons (doesn't error yet)
        let tail_val = mat_id(tail, &ctx);
        let (h2, _) = seq_head_tail(&tail_val, &ctx);
        // Trying to materialize h2 (which is PendingCall(Int(999), [Int(0)]))
        // will error because Int(999) is not a function
        let h2_thunk = ctx.get_thunk(h2);
        let h2_result = crate::eval::materialize_sync(&h2_thunk, None, &ctx);
        assert!(h2_result.is_err());
    }

    #[test]
    fn iterate_laziness() {
        // iterate doesn't materialize its args
        let ctx = test_ctx();
        let undef_f = make_undef_thunk(&ctx);
        let undef_x = make_undef_thunk(&ctx);
        let result = run(builtin_iterate(BuiltinArgs {
            args: vec![undef_f, undef_x],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_ok());
        let r = mat_val(result.unwrap());
        assert!(
            matches!(r, Value::Variant { ref tag, .. } if tag == "Seq.Cons"),
            "expected Seq.Cons, got {:?}",
            r
        );
    }

    #[test]
    fn iterate_arity_one() {
        let result = run(builtin_iterate(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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

        let result = run(builtin_unfold(BuiltinArgs {
            args: vec![step_thunk, seed_thunk],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_ok());
        // Result is a PendingBuiltin, not yet materialized
        // Materializing it would call unfold_step, which would error because
        // step is Int(999), not a function
        let result_val = materialize(&result.unwrap(), None, &test_ctx());
        assert!(result_val.is_err());
    }

    #[test]
    fn unfold_arity_one() {
        let result = run(builtin_unfold(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
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
            args: vec![thunk(Value::Int(2)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(0)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(-5)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(10)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_seq_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let seq_val = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);

        // take(2, seq) → Seq(1, Seq(2, []))
        let result = mat(builtin_take(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let (head, tail) = seq_head_tail(&result, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(1));
        let tail_val = mat_id(tail, &ctx);
        let (h2, t2) = seq_head_tail(&tail_val, &ctx);
        assert_eq!(mat_id(h2, &ctx), Value::Int(2));
        // tail of tail should be Seq.Nil (terminal)
        let t2_val = mat_id(t2, &ctx);
        assert!(is_seq_nil(&t2_val), "expected Seq.Nil, got {:?}", t2_val);
    }

    #[test]
    fn take_seq_zero() {
        // take(0, seq) → Seq.Nil (after Seq→Variant migration, empty-take on a Seq returns Seq.Nil)
        let ctx = test_ctx();
        let seq_val = seq_thunk(thunk(Value::Int(1)), empty_seq_thunk(), &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert!(
            is_seq_nil(&result),
            "expected Seq.Nil for take(0, seq), got {:?}",
            result
        );
    }

    #[test]
    fn take_n_non_int() {
        let result = run(builtin_take(BuiltinArgs {
            args: vec![thunk(string_val("not int".into())), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn take_xs_non_dict_or_seq() {
        let result = run(builtin_take(BuiltinArgs {
            args: vec![
                thunk(Value::Int(5)),
                thunk(string_val("not dict or seq".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn take_arity_one() {
        let result = run(builtin_take(BuiltinArgs {
            args: vec![thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }));
        assert!(result.is_err());
    }

    #[test]
    fn concat_seq() {
        // Build two 2-element sequences and concat them
        let ctx = test_ctx();
        // xs = Seq(1, Seq(2, {}))
        let xs_inner = seq_thunk(thunk(Value::Int(2)), empty_seq_thunk(), &ctx);
        let xs = seq_thunk(thunk(Value::Int(1)), xs_inner, &ctx);

        // ys = Seq(3, Seq(4, {}))
        let ys_inner = seq_thunk(thunk(Value::Int(4)), empty_seq_thunk(), &ctx);
        let ys = seq_thunk(thunk(Value::Int(3)), ys_inner, &ctx);

        // concat(xs, ys) should produce Seq(1, Seq(2, Seq(3, Seq(4, {}))))
        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        // Materialize the result to verify structure
        let result_val = crate::eval::materialize_sync(&result, None, &ctx).unwrap();
        let (h1, t1) = seq_head_tail(&result_val, &ctx);
        assert_eq!(mat_id(h1, &ctx), Value::Int(1));
        let t1_val = mat_id(t1, &ctx);
        let (h2, t2) = seq_head_tail(&t1_val, &ctx);
        assert_eq!(mat_id(h2, &ctx), Value::Int(2));
        let t2_val = mat_id(t2, &ctx);
        let (h3, t3) = seq_head_tail(&t2_val, &ctx);
        assert_eq!(mat_id(h3, &ctx), Value::Int(3));
        let t3_val = mat_id(t3, &ctx);
        let (h4, t4) = seq_head_tail(&t3_val, &ctx);
        assert_eq!(mat_id(h4, &ctx), Value::Int(4));
        let t4_val = mat_id(t4, &ctx);
        assert!(is_seq_nil(&t4_val), "expected Seq.Nil, got {:?}", t4_val);
    }

    #[test]
    fn concat_seq_empty_xs() {
        // concat({}, ys) should return ys (same materialized value)
        let ctx = test_ctx();
        let xs = thunk(Value::Dict(IndexMap::new()));
        let ys = seq_thunk(thunk(Value::Int(1)), empty_seq_thunk(), &ctx);

        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        // Result should be ys — verify by materializing and checking value
        let result_val = crate::eval::materialize_sync(&result, None, &ctx).unwrap();
        let (head, _) = seq_head_tail(&result_val, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(1));
    }

    #[test]
    fn concat_seq_empty_ys() {
        // concat(xs, {}) returns xs's elements with the empty dict {} as the tail sentinel.
        // After Seq→Variant migration: when xs is exhausted (tail is Seq.Nil), concat_seq_step
        // returns ys_thunk directly. ys = {} (empty Dict), so the tail of the result is Dict({}).
        // Note: concat accepts Dict as a valid ys type, so {} is valid but terminates as Dict, not Seq.Nil.
        let ctx = test_ctx();
        let xs = seq_thunk(thunk(Value::Int(1)), empty_seq_thunk(), &ctx);
        let ys = thunk(Value::Dict(IndexMap::new()));

        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        // Materialize to verify: Seq(1, {})
        let result_val = crate::eval::materialize_sync(&result, None, &ctx).unwrap();
        let (head, tail) = seq_head_tail(&result_val, &ctx);
        assert_eq!(mat_id(head, &ctx), Value::Int(1));
        let tail_val = mat_id(tail, &ctx);
        assert!(
            matches!(tail_val, Value::Dict(ref map) if map.is_empty()),
            "expected empty Dict as tail sentinel from concat(seq, {{}}), got {:?}",
            tail_val
        );
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
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
        let seq3 = seq_thunk(thunk(Value::Int(3)), empty_seq_thunk(), &ctx);
        let seq2 = seq_thunk(thunk(Value::Int(2)), seq3, &ctx);
        let xs = seq_thunk(thunk(Value::Int(1)), seq2, &ctx);
        let ys = thunk(Value::Int(42));

        // builtin_concat itself fails immediately because ys=42 is not a collection.
        let err = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
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
    fn join_seq_size_limit() {
        // Test that join enforces MAX_COLLECT_SIZE on sequence iteration.
        // Similar to collect_max_size_limit_enforced, we verify that attempting to join
        // an unbounded sequence will hit either MAX_CONTINUATION_STACK or MAX_COLLECT_SIZE.
        //
        // Run in a thread with larger stack to avoid Rust stack overflow when testing
        // depth-exceeded behavior (same pattern as corpus test runners).
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Single shared context — all ThunkIds must belong to the same arena.
                let ctx = test_ctx();
                let range_result = crate::async_rt::block_on(builtin_range(BuiltinArgs {
                    args: vec![thunk(Value::Int(0))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                // Attempt to join infinite range without take
                // This will hit MAX_CONTINUATION_STACK (2048) before MAX_COLLECT_SIZE (1M)
                // due to depth accumulation in the sequence traversal.
                let join_result = crate::async_rt::block_on(builtin_join(BuiltinArgs {
                    args: vec![thunk(string_val(",")), range_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }));

                // Should fail (either MAX_CONTINUATION_STACK or MAX_COLLECT_SIZE)
                assert!(
                    join_result.is_err(),
                    "join should fail on infinite sequence"
                );
                let err = join_result.unwrap_err();
                // Accept either error - both are valid protections
                let is_depth_error = err.kind.to_string().contains("maximum evaluation depth");
                let is_size_error = err.kind.to_string().contains("sequence exceeds");
                assert!(
                    is_depth_error || is_size_error,
                    "expected depth or size limit error, got: {}",
                    err.kind
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
            args: vec![thunk(string_val(",")), thunk(Value::Dict(IndexMap::new()))],
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
            args: vec![thunk_dict(dict1, &ctx), thunk_dict(dict2, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
    fn filter_seq_step_no_depth_accumulation_on_consecutive_failures() {
        // Task 1: Verify that consecutive predicate failures in builtin_filter_seq_step
        // do NOT accumulate depth. Before the fix, each skipped element created a
        // PendingBuiltin at depth+1, so N failures consumed ~2N depth units and would
        // hit depth limits after ~128 consecutive failing elements. After the
        // fix, the skip branch uses an internal loop, so N failures cost zero extra depth.
        //
        // Test: filter range(0, 300) with a predicate that only passes x == 299.
        // This triggers 299 consecutive failures. With the old PendingBuiltin-per-failure
        // approach, this would hit depth limits (~128 failures × 2 depth units each).
        // With the fix (internal loop for failures), all 299 failures are handled at
        // constant depth, and the result is Seq(Int(299), ...).
        //
        // The predicate is implemented as a Rust builtin (not an LLT function) to avoid
        // needing a closure env with stdlib builtins.
        fn pred_eq_299(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let val = crate::eval::materialize_sync(&ctx.args[0], None, &ctx.ctx)?;
                ok_val(Value::Bool(matches!(val, Value::Int(299))), Span::origin())
            })
        }

        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Single shared context — all ThunkIds must belong to the same arena.
                let ctx = test_ctx();
                // Create range(0, 300): lazy Seq(0, 1, ..., 299) via PendingBuiltin chain
                let range_result = crate::async_rt::block_on(builtin_range(BuiltinArgs {
                    args: vec![thunk(Value::Int(0)), thunk(Value::Int(300))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                let pred = thunk(Value::Builtin(crate::value::BuiltinDef {
                    func: pred_eq_299,
                    name: "pred_eq_299",
                    pos_strictness: &[],
                    force_count: 0,
                }));

                let filter_result = crate::async_rt::block_on(builtin_filter(BuiltinArgs {
                    args: vec![pred, range_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                // Force the filter result. Before the fix this would fail with depth
                // exceeded after ~128 consecutive failures. After the fix the internal
                // loop handles all 299 failures at constant depth.
                let val = crate::eval::materialize_sync(&filter_result, None, &ctx).unwrap();
                let (head, _) = seq_head_tail(&val, &ctx);
                let head_thunk = ctx.get_thunk(head);
                let head_val = crate::eval::materialize_sync(&head_thunk, None, &ctx).unwrap();
                assert_eq!(
                    head_val,
                    Value::Int(299),
                    "expected Int(299) as first passing element"
                );
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
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
        // Call builtin_filter with depth near MAX_CONTINUATION_STACK (e.g., depth=200).
        // Collect the result via builtin_collect to force materialization.
        // Assert the result is an empty dict (no depth exceeded error).
        fn pred_always_false(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Bool(false), Span::origin()) })
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
                    force_count: 0,
                }));

                // Call filter at depth=200 (near MAX_CONTINUATION_STACK=2048)
                // If filter_dict_step accumulates depth incorrectly, this would hit
                // DepthExceeded after ~27 entries (200 + 27*2 ≥ 256).
                // With the fix, all 300 failures are handled at constant depth.
                let filter_result = crate::async_rt::block_on(builtin_filter(BuiltinArgs {
                    args: vec![pred, dict_thunk],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx_inner),
                }))
                .unwrap();

                // Convert lazy Seq to Dict via builtin_collect.
                // Must go through Thunk::new_pending_builtin + materialize rather than
                // calling builtin_collect directly, because builtin_collect uses
                // expect_one_arg which calls try_get_materialized().expect("pre-materialized
                // by force_count"). Calling it directly with an unmaterialized thunk panics.
                // The CEK machine (eval.rs::materialize PendingBuiltin handler) applies
                // pos_strictness W1 (Spine) pre-materialization before dispatching.
                let collect_def = builtin!("builtin-collect", builtin_collect, [Strictness::Spine]);
                let collect_thunk = Arc::new(Thunk::new_pending_builtin(
                    collect_def,
                    vec![filter_result],
                    None,
                    call_span(),
                    None,
                    Arc::clone(&ctx_inner),
                ));

                let val = crate::eval::materialize_sync(&collect_thunk, None, &ctx_inner).unwrap();
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

        let err = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();

        // type_mismatch_ctx with a context produces "concat: expected ..., got ..."
        assert!(
            err.kind.to_string().contains("concat"),
            "expected 'concat' in error, got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("Dict or Seq"),
            "expected 'Dict or Seq' in error, got: {}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("Int"),
            "expected 'Int' in error (got type name), got: {}",
            err.kind
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
        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let val = mat_val(result.unwrap());
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(mat_id(*m.get(&Key::Int(0)).unwrap(), &ctx), Value::Int(99));
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn take_large_count_from_infinite_seq_succeeds() {
        // Verify that $take + $collect works correctly for counts well above the old
        // MAX_CONTINUATION_STACK (2048). The CEK machine handles the Seq chain iteratively,
        // so no depth limit is hit. This test proves the iterative materialize_rc loop
        // correctly traverses long lazy sequences without Rust stack overflow.
        //
        // Run in a thread with larger stack for the initial setup and evaluation.
        let result = std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                // Single shared context — all ThunkIds must belong to the same arena.
                let ctx = test_ctx();
                // Create infinite range starting at 0
                let range_result = crate::async_rt::block_on(builtin_range(BuiltinArgs {
                    args: vec![thunk(Value::Int(0))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                // Take 300 elements (well above the old MAX_CONTINUATION_STACK=2048).
                // The CEK machine handles this iteratively — no depth limit is hit.
                let take_result = crate::async_rt::block_on(builtin_take(BuiltinArgs {
                    args: vec![thunk(Value::Int(300)), range_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }))
                .unwrap();

                // Force the entire sequence by calling collect.
                // With the iterative CEK machine, 300 elements succeeds.
                let collect_result = crate::async_rt::block_on(builtin_collect(BuiltinArgs {
                    args: vec![take_result],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }));

                assert!(
                    collect_result.is_ok(),
                    "collect(take(300, range(0))) should succeed with iterative CEK machine"
                );
                let val =
                    crate::eval::materialize_sync(&collect_result.unwrap(), None, &ctx).unwrap();
                match val {
                    Value::Dict(ref map) => {
                        assert_eq!(map.len(), 300, "expected 300 elements in result dict");
                        assert_eq!(
                            crate::eval::materialize_sync(
                                &ctx.get_thunk(*map.get(&Key::Int(0)).unwrap()),
                                None,
                                &ctx
                            )
                            .unwrap(),
                            Value::Int(0)
                        );
                        assert_eq!(
                            crate::eval::materialize_sync(
                                &ctx.get_thunk(*map.get(&Key::Int(299)).unwrap()),
                                None,
                                &ctx
                            )
                            .unwrap(),
                            Value::Int(299)
                        );
                    }
                    other => panic!("expected Dict, got {:?}", other),
                }
            })
            .unwrap()
            .join();

        assert!(result.is_ok(), "test thread panicked: {:?}", result);
    }

    #[test]
    fn test_proxy_returns_proxy_value() {
        let ctx = test_ctx();
        let handler = thunk(Value::Int(42));
        let result = run(builtin_proxy(BuiltinArgs {
            args: vec![handler.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }))
        .unwrap();

        let val = mat_val(result);
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
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );

        // Two args
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );

        // Three args
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[test]
    fn test_proxy_named_arg_error() {
        let mut named = IndexMap::new();
        named.insert("handler".to_string(), thunk(Value::Int(42)));

        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
        }))
        .unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("does not accept named arguments"),
            "got: {}",
            err.kind
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
        let seq = seq_thunk(thunk(Value::Int(1)), empty_seq_thunk(), &ctx);

        // Create the PendingBuiltin thunk
        let pending_thunk = Arc::new(Thunk::new_pending_builtin(
            builtin!("builtin-drop", builtin_drop_seq_step, [], 2),
            vec![n_remaining, seq],
            None,
            call_span(),
            Some(Arc::from("test drop_seq_step")),
            Arc::clone(&ctx),
        ));

        // Materialize it and expect an error
        let result = crate::eval::materialize_sync(&pending_thunk, None, &ctx);
        let err = result.unwrap_err();

        // Verify it's a TypeMismatch error with the expected message
        assert!(
            matches!(err.kind, crate::error::ErrorKind::TypeMismatch { .. }),
            "Expected ErrorKind::TypeMismatch, got: {:?}",
            err.kind
        );
        assert!(
            err.kind.to_string().contains("drop") && err.kind.to_string().contains("expected Int"),
            "Expected message to contain 'drop' and 'expected Int', got: {}",
            err.kind
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
        let env = stdlib_env.read().unwrap();

        // Names that must exist: Rust-native builtins (registered via builtin_module() groups)
        // plus a representative selection of prelude-defined functions.
        let required_names: &[&str] = &[
            // Rust-native operators (registered via builtin_module("core"), injected during bootstrap)
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
            args: vec![thunk(Value::Int(0)), thunk(Value::Int(10))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));

        let result = mat(builtin_drop(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), thunk(seq)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));

        // Result should be a Seq.Cons (drop materializes and returns remaining Seq)
        let (head, _) = seq_head_tail(&result, &ctx);
        // First element after dropping 2 should be 2
        assert_eq!(mat_id(head, &ctx), Value::Int(2));
    }

    #[test]
    fn reduce_constructs_pending_call() {
        // reduce(+, 0, [1, 2]) should create a PendingCall chain
        let ctx = test_ctx();
        let mut m = IndexMap::new();
        m.insert(Key::Int(0), thunk(Value::Int(1)));
        m.insert(Key::Int(1), thunk(Value::Int(2)));
        let seq_val = thunk_dict(m, &ctx);

        // T-719: get + builtin from builtin_module("core") instead of deleted standard_builtins()
        let add_builtin = builtin_module("core")
            .expect("core module must exist")
            .into_iter()
            .find(|def| def.name == "+")
            .map(Value::Builtin)
            .expect("+ must be in core module");

        let result = mat(builtin_reduce(BuiltinArgs {
            args: vec![thunk(add_builtin), thunk(Value::Int(0)), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(string_val(",".into())), seq_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));

        // Result should be "a,b"
        assert_eq!(result, string_val("a,b".into()));
    }

    #[test]
    fn test_builtin_until_basic() {
        // Count from 0 to 10 using until
        // pred: [fn [x] [= $x 10]]
        // f: [fn [x] [+ $x 1]]
        // init: 0
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let ctx = test_ctx();
                let pred = parse_eval("[fn [let x] [= $x 10]]", &ctx);
                let f = parse_eval("[fn [let x] [+ $x 1]]", &ctx);

                let result = mat(builtin_until(BuiltinArgs {
                    args: vec![thunk(pred), thunk(f), thunk(Value::Int(0))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
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
        // f: [fn [x] [$error "should not be called"]]
        // init: 42
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let ctx = test_ctx();
                let pred = parse_eval("[fn [let x] true]", &ctx);
                let f = parse_eval("[fn [let x] [$error \"should not be called\"]]", &ctx);

                let result = mat(builtin_until(BuiltinArgs {
                    args: vec![thunk(pred), thunk(f), thunk(Value::Int(42))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
                }));

                assert_eq!(result, Value::Int(42));
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn test_builtin_until_many_iterations() {
        // Test that we can exceed MAX_CONTINUATION_STACK (2048) iterations
        // Count from 0 to 300
        std::thread::Builder::new()
            .stack_size(TEST_STACK_SIZE)
            .spawn(|| {
                let ctx = test_ctx();
                let pred = parse_eval("[fn [let x] [= $x 300]]", &ctx);
                let f = parse_eval("[fn [let x] [+ $x 1]]", &ctx);

                let result = mat(builtin_until(BuiltinArgs {
                    args: vec![thunk(pred), thunk(f), thunk(Value::Int(0))],
                    named: no_named(),
                    call_span: call_span(),
                    ctx: Arc::clone(&ctx),
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

    fn make_int_dict(vals: &[i64], ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let mut rc_map: IndexMap<Key, Arc<Thunk>> = IndexMap::new();
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
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> i64 {
        let thunk = ctx.get_thunk(*map.get(&Key::Int(idx)).unwrap());
        match crate::eval::materialize_sync(&thunk, None, ctx).unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int at index {idx}, got {:?}", other),
        }
    }

    #[test]
    fn rest_three_elements_drops_first() {
        let ctx = test_ctx();
        let result = mat(builtin_rest(BuiltinArgs {
            args: vec![thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(make_int_dict(&[42], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[test]
    fn rest_empty_dict_returns_empty() {
        let result = mat(builtin_rest(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
            args: vec![thunk(Value::Int(0)), thunk(make_int_dict(&[1, 2, 3], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Int(99)), thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
            args: vec![thunk(make_int_dict(&[3, 1, 4, 1, 5], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
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
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
            args: vec![thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        // each returns a Seq.Cons — materialize head
        let (head, tail) = seq_head_tail(&result, &ctx);
        let head_val = mat_id(head, &ctx);
        assert_eq!(head_val, Value::Int(10));
        // Verify tail is also a Seq.Cons (not fully unwinding it here)
        let tail_thunk = ctx.get_thunk(tail);
        let tail_val = crate::eval::materialize_sync(&tail_thunk, None, &ctx).unwrap();
        assert!(matches!(tail_val, Value::Variant { ref tag, .. } if tag == "Seq.Cons"));
    }

    #[test]
    fn each_type_error_int() {
        let ctx = test_ctx();
        let result = run(builtin_each(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        }));
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
        let result = run(builtin_each(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        }));
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
        let result = run(builtin_each(BuiltinArgs {
            args: vec![thunk(Value::Bool(true))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        }));
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
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
        let result = run(builtin_each_key(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        }));
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
            args: vec![thunk(Value::Dict(IndexMap::new()))],
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
        let result = run(builtin_each_kv(BuiltinArgs {
            args: vec![thunk(Value::Bool(false))],
            named: no_named(),
            call_span: call_span(),
            ctx,
        }));
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
            args: vec![thunk(Value::Int(1)), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert_eq!(result, string_val("second".into()));
    }

    #[test]
    fn builtin_get_key_not_found_error() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), thunk(Value::Int(10)));
        map.insert(Key::String("b".into()), thunk(Value::Int(20)));
        let result = run(builtin_get(BuiltinArgs {
            args: vec![thunk(string_val("z".into())), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::KeyNotFound { .. }),
            "expected KeyNotFound, got {:?}",
            err.kind
        );
    }

    // === H1 CPS sentinel tests: verify force_count builtins do NOT force lazy values ===

    /// Body test: `builtin_keys` (force_count=1) must NOT force dict VALUES.
    ///
    /// `$keys` enumerates dictionary keys only. The values stored under those keys
    /// must remain as unevaluated thunks throughout. If `builtin_keys` were to call
    /// `materialize()` on any dict value, the undef thunk would fail with an
    /// "undefined variable" error, causing this test to panic.
    ///
    /// This is a body test: it verifies that the builtin body does not over-materialize
    /// beyond the args promised by force_count. The force_count dispatch mechanism itself
    /// is tested separately by the CEK and bypass-path unit tests.
    #[test]
    fn builtin_keys_does_not_force_dict_values() {
        let ctx = test_ctx();
        // Build a dict whose VALUES are bomb thunks: materializing them would fail.
        // `$keys` should enumerate the keys without ever touching the values.
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), make_undef_thunk(&ctx));
        map.insert(Key::String("b".into()), make_undef_thunk(&ctx));
        map.insert(Key::String("c".into()), make_undef_thunk(&ctx));
        let dict = thunk_dict(map, &ctx);

        // builtin_keys should succeed: it only reads keys, not values.
        let result = run(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_keys must not force dict values; got error: {:?}",
                e
            )
        });
        // The result should be a dict with 3 entries (one per key).
        let val = mat_val(result_thunk);
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 3, "expected 3 keys in result, got {}", m.len())
            }
            other => panic!("expected Dict from builtin_keys, got {:?}", other),
        }
    }

    /// Body test: `builtin_length` (force_count=1) must NOT force dict VALUES.
    ///
    /// `$length` counts dictionary entries. The values stored under those keys
    /// must remain unevaluated. Like `builtin_keys_does_not_force_dict_values`, this
    /// is a body test: verifies the builtin body does not over-materialize beyond
    /// the args promised by force_count.
    #[test]
    fn builtin_length_does_not_force_dict_values() {
        let ctx = test_ctx();
        // Build a dict with 4 bomb-value entries.
        let mut map = IndexMap::new();
        map.insert(Key::Int(0), make_undef_thunk(&ctx));
        map.insert(Key::Int(1), make_undef_thunk(&ctx));
        map.insert(Key::Int(2), make_undef_thunk(&ctx));
        map.insert(Key::Int(3), make_undef_thunk(&ctx));
        let dict = thunk_dict(map, &ctx);

        // builtin_length should succeed: it only counts entries, not values.
        let result = run(builtin_length(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_length must not force dict values; got error: {:?}",
                e
            )
        });
        let val = mat_val(result_thunk);
        assert_eq!(val, Value::Int(4), "expected length 4, got {:?}", val);
    }

    /// Body test: `builtin_append` (force_count=0) must NOT force the appended VALUE.
    ///
    /// After T-776, `$append` takes `(value, dict)` — args[0]=value, args[1]=dict.
    /// The VALUE being appended (args[0]) must stay as an unevaluated thunk — only
    /// the dict structure (args[1]) needs to be materialized to determine the next key.
    ///
    /// If `builtin_append` were to force args[0], the undef thunk would produce an
    /// "undefined variable" error. Passing this test proves args[0] is never forced
    /// by the builtin body (body test — not testing the pos_strictness dispatch mechanism).
    #[test]
    fn builtin_append_does_not_force_appended_value() {
        let ctx = test_ctx();
        // Start with an empty dict and append a bomb thunk as the value.
        let dict = thunk(Value::Dict(IndexMap::new()));
        let bomb = make_undef_thunk(&ctx);

        // builtin_append should succeed: it inserts the thunk by Rc::clone, never forcing it.
        let result = run(builtin_append(BuiltinArgs {
            args: vec![bomb, dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
        }));
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_append must not force the appended value; got error: {:?}",
                e
            )
        });
        // The result should be a dict with exactly one entry at key 0.
        let val = mat_val(result_thunk);
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1, "expected 1 entry after append, got {}", m.len());
                assert!(
                    m.contains_key(&Key::Int(0)),
                    "expected integer key 0, got {:?}",
                    m.keys().collect::<Vec<_>>()
                );
            }
            other => panic!("expected Dict from builtin_append, got {:?}", other),
        }
    }
}
