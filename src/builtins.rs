//! Builtin registry, bootstrap, and slim helpers for the LLT language.
//!
//! Builtin implementations live in split files (`builtins_math.rs`, `builtins_io.rs`, etc.).
//! This file provides:
//! - `builtin_module(name)` — dispatch to per-module aggregators (`core_builtins()`, etc.)
//! - `type_env_module(name)` — dispatch to per-module type environments
//! - `build_core_env()` — build a fresh `crate::env::Env` with all core Rust builtins (runtime values + type schemes)
//! - Helper functions: `ok_val`, `string_val`, `reject_named`, `require_string`, etc.
//! - Re-exports of split-file functions for test access via `use super::*`

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
#[allow(unused_imports)] // used in test modules via `use super::*`
use crate::value::Strictness;
use crate::value::ThunkId;
// Circular module dependency: this module imports `invoke_function` and `materialize` from eval.rs.
// eval.rs calls builtins via function pointers stored in `Value::Builtin`.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
// SAFETY: builtins.rs and eval.rs have a circular dependency at the value level — builtins call
// materialize/invoke_function (eval.rs), and eval calls builtin_module() (builtins.rs). This is
// safe because the dependency is at function-call level, not at module initialization level.
// Rust modules can call each other's pub functions after initialization without deadlock.
use crate::eval::materialize;
use crate::value::{BuiltinArgs, HashableValue, Thunk, Value};

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
    // 5-arg form: with strictness array, force_count, and param names (param names ignored — BuiltinDef no longer stores them)
    ($name:literal, $func:expr, [$($strictness:expr),* $(,)?], $force_count:expr, [$($param:literal),* $(,)?]) => {{
        const S: &[crate::value::Strictness] = &[$($strictness),*];
        crate::value::BuiltinDef {
            func: $func as crate::value::BuiltinFn,
            name: $name,
            pos_strictness: S,
            force_count: $force_count,
        }
    }};
    // 6-arg form: with strictness, force_count, param names, and named kwargs (param/named names ignored — BuiltinDef no longer stores them)
    ($name:literal, $func:expr, [$($strictness:expr),* $(,)?], $force_count:expr, [$($param:literal),* $(,)?], [$($named:literal),* $(,)?]) => {{
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

pub(crate) fn ok_val(v: Value, span: Span) -> EvalResult<Arc<Thunk>> {
    Ok(Arc::new(Thunk::value(v, span)))
}

/// Convert a `Value::Bytes` slice into a lazy collection of `Value::Int` (one per byte).
///
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

/// Helper: get a pre-materialized single positional argument, enforcing exact arity of 1
/// and rejecting named arguments. Used by many single-arg builtins with force_count=1.
pub(crate) fn expect_one_arg(
    name: &str,
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<Value> {
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    let thunk0 = ctx.get_thunk(args[0]);
    Ok(thunk0
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
pub(crate) async fn flatten_overlay(
    left: &ThunkId,
    right: &ThunkId,
    name: &str,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<IndexMap<HashableValue, ThunkId>> {
    use crate::value::ThunkId;

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
    let mut layers: Vec<IndexMap<HashableValue, ThunkId>> = Vec::new();

    while let Some(thunk_id) = work_stack.pop() {
        let thunk = ctx.get_thunk(thunk_id);
        let val = materialize(&thunk, Some(&call_span), ctx).await?;
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
    let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(total_cap);
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
pub(crate) async fn require_dict(
    name: &str,
    value: Value,
    def_span: Span,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<IndexMap<HashableValue, ThunkId>> {
    match value {
        Value::Dict(map) => Ok(map),
        Value::Overlay(l, r) => flatten_overlay(&l, &r, name, ctx, call_span).await,
        Value::Variant { payload, .. } => {
            // Auto-unpack variant payload — consistent with DotAccess behavior
            match payload {
                Some(payload_id) => {
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_val = materialize(&payload_thunk, Some(&call_span), ctx).await?;
                    // Recursively try to extract dict from payload
                    Box::pin(require_dict(name, payload_val, def_span, ctx, call_span)).await
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
    named: Option<&IndexMap<String, ThunkId>>,
    call_span: Span,
) -> EvalResult<()> {
    if named.map(|n| !n.is_empty()).unwrap_or(false) {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }
    Ok(())
}

// Arithmetic and comparison builtins: +, -, *, /, =, <.
// Implementations live in builtins_math.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_math::{
    builtin_acos,
    builtin_asin,
    builtin_atan,
    builtin_atan2,
    builtin_band,
    builtin_bor,
    builtin_bxor,
    builtin_cos,
    builtin_div_float,
    builtin_eq_float,
    builtin_eq_int,
    builtin_eq_string,
    builtin_exp,
    builtin_finite_check,
    builtin_float,
    builtin_float_add,
    builtin_float_gt,
    builtin_float_gte,
    builtin_float_mul,
    builtin_float_sub,
    builtin_inf_check,
    // Monomorphic typed variants.
    builtin_int_add,
    builtin_int_gt,
    builtin_int_gte,
    builtin_int_mul,
    builtin_int_sub,
    builtin_int_to_float,
    builtin_log,
    builtin_log10,
    builtin_log2,
    builtin_lt,
    builtin_lte,
    builtin_mul,
    builtin_nan_check,
    builtin_pow,
    builtin_shl,
    builtin_shr,
    builtin_sin,
    builtin_sqrt,
    builtin_str_gt,
    builtin_str_gte,
    builtin_tan,
};

// Dict/access builtins: keys, length, merge, get, each, each-key, each-kv, build-dict.
// Implementations live in builtins_dict.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_dict::{
    builtin_build_dict, builtin_builder_delete, builtin_builder_finish, builtin_builder_get,
    builtin_builder_get_or, builtin_builder_has, builtin_builder_set, builtin_builder_snapshot,
    builtin_dict_key_nth, builtin_dict_kv_nth, builtin_dict_nth, builtin_get, builtin_keys,
    builtin_length, builtin_make_builder,
};

// Type/eval/meta builtins: type-of, include, error, try, apply, validate.
// Implementations live in builtins_meta.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_meta::{
    builtin_annotation_of, builtin_apply, builtin_ast_of, builtin_big_int, builtin_blake3,
    builtin_cap_identity, builtin_decimal, builtin_eval, builtin_eval_types, builtin_force,
    builtin_gensym, builtin_llt_repr, builtin_macro_error, builtin_macro_injects,
    builtin_make_annotated, builtin_raise, builtin_span_of, builtin_tag_of, builtin_try,
    builtin_type_of, builtin_until, builtin_validate,
};

// String builtins: str, split, replace, trim, trim-start, trim-end,
// str-length, str-index-of, str-slice, str-chars, char-code, chr, str-bytes, bytes-str,
// str-to-upper-char, str-to-lower-char, str-map-chars, regex-match?.
// Note: upper/lower are no longer Rust builtins; they live in stdlib/strings.llt and
// are implemented using str-map-chars + str-to-upper-char / str-to-lower-char.
// Implementations live in builtins_string.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_string::{
    builtin_bytes_str, builtin_char_code, builtin_chr, builtin_regex_match, builtin_replace,
    builtin_str_byte_count, builtin_str_bytes, builtin_str_has_nth_byte, builtin_str_index_of,
    builtin_str_length, builtin_str_map_chars, builtin_str_nth_byte, builtin_str_nth_char,
    builtin_str_slice, builtin_str_to_lower_char, builtin_str_to_upper_char, builtin_trim,
    builtin_trim_end, builtin_trim_start,
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
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<Arc<Thunk>> {
    let val = expect_one_arg(name, args, named, ctx, call_span.clone())?;
    let arg0_span = ctx.get_thunk(args[0]).span.clone();
    match val {
        Value::Int(n) => ok_val(Value::Int(n), call_span),
        Value::Float(f) => {
            if !f.is_finite() {
                return Err(EvalError::float_not_finite(name.to_string(), f, arg0_span).into());
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
            arg0_span,
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
        ..
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
        ..
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
        ..
    } = ctx_arg;
    if args.is_empty() {
        return Box::pin(async move { Err(EvalError::arity_mismatch(1, 0, call_span).into()) });
    }
    Box::pin(async move {
        let val = expect_one_arg("to-int", &args, named.as_ref(), &ctx, call_span.clone())?;
        let arg0_span = ctx.get_thunk(args[0]).span.clone();
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
        ..
    } = ctx_arg;
    if args.is_empty() {
        return Box::pin(async move { Err(EvalError::arity_mismatch(1, 0, call_span).into()) });
    }
    Box::pin(async move {
        let val = expect_one_arg("to-float", &args, named.as_ref(), &ctx, call_span.clone())?;
        let arg0_span = ctx.get_thunk(args[0]).span.clone();
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

// Re-exported here for test access via `use super::*`.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_dict::{builtin_concat, builtin_drop, builtin_take};

pub(crate) fn builtin_proxy(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx: _,
        ..
    } = ctx_arg;
    Box::pin(async move {
        reject_named("proxy", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        Ok(Arc::new(Thunk::value(
            Value::Proxy { handler: args[0] },
            call_span,
        )))
    })
}

/// Return the builtin list for a named module, or None if the name is unknown.
///
/// All Rust builtins are registered in "core" regardless of their conceptual domain.
/// Modules "io", "math", "meta", "string", "async" return empty runtime lists
/// because their Rust implementations live in core_builtins(). The --- uses: header
/// for these modules exists only to load their builtin_*.llt type declarations.
pub fn builtin_module(name: &str) -> Option<Vec<crate::value::BuiltinDef>> {
    let defs = match name {
        "core" => Some(crate::builtins_core::core_builtins()),
        "datetime" => Some(crate::builtins_datetime::datetime_builtins()),
        "net" => Some(crate::builtins_net::net_builtins()),
        "io" => Some(crate::builtins_io::io_builtins()),
        "math" => Some(crate::builtins_math::math_builtins()),
        "meta" => Some(crate::builtins_meta::meta_builtins()),
        "string" => Some(crate::builtins_string::string_builtins()),
        "async" => Some(crate::builtins_async::async_builtins()),
        _ => return None,
    }?;

    Some(defs)
}

/// Build a fresh environment seeded with only the core Rust builtins.
///
/// This is the initial env that Rust provides to loader.llt. All other names
/// (%programs, %args, %cwd, %libdir, %stdout, etc.) are injected by the caller
/// before calling `run_loader_pipeline`. Loader.llt is then evaluated exactly once
/// with the complete initial environment.
///
/// This is the single correct execution path for bootstrapping. There is no
/// pre-evaluation of loader.llt; the caller drives the full pipeline via
/// `run_loader_pipeline`.
pub fn build_core_env() -> Arc<RwLock<crate::env::Env>> {
    // T-1557: Env is type-metadata only. Runtime values are stored in FlatEnv/arena.
    // Insert each builtin name into the slotted IndexMap so the resolver can assign
    // de Bruijn (level, slot) coordinates. The actual Value::Builtin thunks are placed
    // in the root scope (slot 0, 1, 2, …) by EvalContext::new_scope_arena, in the SAME
    // iteration order as core_builtins(). The two orderings must stay in sync.
    use crate::builtins_core::core_builtins;
    let env = crate::env::Env::new();
    let env = Arc::new(RwLock::new(env));
    {
        let mut env_write = env.write().unwrap();
        for def in core_builtins() {
            env_write.insert_slot_name_only(def.name.to_string());
        }
    }
    env
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
    use crate::rust_span;
    use crate::test_util::test_span;
    use crate::value::{string_val, Strictness};

    /// Helper: wrap a Value in a materialized Thunk (Arc).
    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::value(val, test_span(1, 1, 1, 5)))
    }

    /// Helper: allocate a Value as a ThunkId in the given ctx's arena.
    fn alloc(val: Value, ctx: &Arc<crate::eval::EvalContext>) -> ThunkId {
        ctx.alloc_thunk(0, thunk(val))
    }

    fn no_named() -> Option<IndexMap<String, ThunkId>> {
        None
    }

    fn call_span() -> Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new_empty(base_dir, false)
    }

    /// Drive an async builtin to completion in tests.
    async fn run(
        f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>,
    ) -> EvalResult<Arc<Thunk>> {
        f.await
    }

    /// Async materialize wrapper for test code.
    async fn materialize_sync(
        t: &Arc<Thunk>,
        s: Option<&crate::ast::Span>,
        c: &Arc<crate::eval::EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        materialize(t, s, c).await
    }

    async fn mat(f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>) -> Value {
        materialize_sync(&run(f).await.unwrap(), None, &test_ctx())
            .await
            .unwrap()
    }

    /// Materialize an already-resolved thunk (for `result: EvalResult<Arc<Thunk>>` cases).
    async fn mat_val(t: Arc<Thunk>) -> Value {
        materialize_sync(&t, None, &test_ctx()).await.unwrap()
    }

    /// Parse and evaluate an LLT snippet, returning the result value.
    ///
    fn test_file(src: &str) -> Arc<crate::ast::SourceFile> {
        Arc::new(crate::ast::SourceFile {
            path: Arc::from(file!()),
            content: Arc::from(src),
        })
    }

    /// Uses the stdlib environment so that builtins are available in the body.
    /// The snippet should be a complete expression (e.g. `"[fn [let] 42]"`).
    async fn parse_eval(llt_src: &str, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let parsed = crate::parser::parse(llt_src, test_file(llt_src))
            .unwrap_or_else(|e| panic!("parse_eval: parse failed for {:?}: {}", llt_src, e));
        let mut program = parsed.program;
        crate::desugar::desugar_program_full(&mut program);
        // Seed resolver from FlatEnv so builtin names resolve to de Bruijn coords.
        let root_frame: indexmap::IndexMap<String, u32> = crate::builtins_core::core_builtins()
            .iter()
            .enumerate()
            .map(|(i, def)| (def.name.to_string(), i as u32))
            .collect();
        let (_table, _frames) = crate::resolve::resolve_surface_program(&program, &[root_frame]);
        let thunk = crate::eval::eval_surface_file(&program, ctx)
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "parse_eval: eval_surface_file failed for {:?}: {}",
                    llt_src, e
                )
            });
        materialize_sync(&thunk, None, ctx)
            .await
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
                resolution: crate::ast::Resolution::new(), // Not set → unresolvable → CoreExpr::Placeholder + LowerDiagnostic
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
            span: test_span(1, 1, 1, 10),
            type_guard: crate::ast::TypeAnnotation::new(),
            provenance: crate::ast::Provenance::new(),
        });
        Arc::new(Thunk::surface(
            node,
            Arc::new(std::collections::HashMap::new()),
            Arc::new(std::collections::HashMap::new()),
            0, // root scope
            Arc::clone(ctx),
            test_span(1, 1, 1, 10),
        ))
    }

    /// Build a materialized dict thunk whose entries are allocated into `ctx`'s arena.
    /// Accepts `IndexMap<HashableValue, Arc<Thunk>>` (convenient for test construction) and
    /// stores each as a `ThunkId` in `Value::Dict`, as the runtime requires.
    /// Returns a `ThunkId` so the result can be used directly in `BuiltinArgs.args`.
    fn thunk_dict(
        map: IndexMap<HashableValue, Arc<Thunk>>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> ThunkId {
        let mut id_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(0, v));
        }
        ctx.alloc_thunk(
            0,
            Arc::new(Thunk::value(Value::Dict(id_map), test_span(1, 1, 1, 5))),
        )
    }

    /// Helper: materialize the thunk identified by `id` in `ctx`'s arena.
    async fn mat_id(id: ThunkId, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let thunk = ctx.get_thunk(id);
        materialize_sync(&thunk, None, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn floor_int_passthrough() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn floor_negative_int_passthrough() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Int(-7), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn floor_zero_int() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn floor_positive_float() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(3.7), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn floor_negative_float() {
        // floor(-3.2) = -4, not -3
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(-3.2), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-4));
    }

    #[tokio::test]
    async fn floor_float_exact_integer() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(5.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn floor_float_just_below_integer() {
        let ctx = test_ctx();
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(2.9999999), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn floor_nan_errors() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::NAN), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn floor_positive_infinity_errors() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::INFINITY), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_negative_infinity_errors() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::NEG_INFINITY), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_string_type_error() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(string_val("3.5"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_string_type_error_non_numeric() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(string_val("x"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_dict_type_error() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Dict(IndexMap::new()), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_wrong_arity_two() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(3.5), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_large_positive_float_out_of_range() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(1e19), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn floor_large_negative_float_out_of_range() {
        let ctx = test_ctx();
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![alloc(Value::Float(-1e19), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_int_passthrough() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn round_negative_int_passthrough() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Int(-7), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn round_positive_half_rounds_up() {
        // 0.5 rounds to 1 (half-away-from-zero)
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(0.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(1));
    }

    #[tokio::test]
    async fn round_negative_half_rounds_down() {
        // -0.5 rounds to -1 (half-away-from-zero)
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(-0.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-1));
    }

    #[tokio::test]
    async fn round_positive_below_half() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(2.4), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn round_positive_above_half() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(2.6), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn round_negative_below_half() {
        // -2.4 rounds to -2
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(-2.4), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-2));
    }

    #[tokio::test]
    async fn round_negative_above_half() {
        // -2.6 rounds to -3
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(-2.6), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-3));
    }

    #[tokio::test]
    async fn round_1_5_rounds_to_2() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(1.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn round_negative_1_5_rounds_to_negative_2() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(-1.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-2));
    }

    #[tokio::test]
    async fn round_float_exact_integer() {
        let ctx = test_ctx();
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(5.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn round_nan_errors() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::NAN), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn round_positive_infinity_errors() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::INFINITY), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_negative_infinity_errors() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(f64::NEG_INFINITY), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_string_type_error() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(string_val("3.5"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_string_type_error_non_numeric() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(string_val("x"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Int or Float"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_wrong_arity_two() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_large_positive_float_out_of_range() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(1e19), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn round_large_negative_float_out_of_range() {
        let ctx = test_ctx();
        let err = run(builtin_round(BuiltinArgs {
            args: vec![alloc(Value::Float(-1e19), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("out of range for Int"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_valid_positive() {
        let ctx = test_ctx();
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("42"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn to_int_valid_negative() {
        let ctx = test_ctx();
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("-7"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn to_int_valid_zero() {
        let ctx = test_ctx();
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("0"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn to_int_valid_large() {
        let ctx = test_ctx();
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("9223372036854775807"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[tokio::test]
    async fn to_int_invalid_float_string() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("3.14"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_invalid_text() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_invalid_empty() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val(""), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_invalid_with_spaces() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val(" 42 "), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_rejects_int_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn to_int_rejects_float_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(Value::Float(3.14), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_rejects_float_whole_number() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(Value::Float(1.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_rejects_dict_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(Value::Dict(IndexMap::new()), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_wrong_arity_two() {
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("1"), &ctx), alloc(string_val("2"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_valid_decimal() {
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("3.14"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(3.14));
    }

    #[tokio::test]
    async fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("42"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(42.0));
    }

    #[tokio::test]
    async fn to_float_valid_negative() {
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("-2.5"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(-2.5));
    }

    #[tokio::test]
    async fn to_float_valid_scientific_notation() {
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("1.5e10"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[tokio::test]
    async fn to_float_valid_negative_exponent() {
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("2.5e-3"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[tokio::test]
    async fn to_float_valid_zero() {
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("0.0"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(0.0));
    }

    #[tokio::test]
    async fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let ctx = test_ctx();
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val(".5"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Float(0.5));
    }

    #[tokio::test]
    async fn to_float_invalid_text() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_invalid_empty() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val(""), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_inf() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("inf"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_negative_inf() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("-inf"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_infinity() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("infinity"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_nan() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("NaN"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite number"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_int_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_float_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(Value::Float(3.14), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_dict_input() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(Value::Dict(IndexMap::new()), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_wrong_arity_two() {
        let ctx = test_ctx();
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![
                alloc(string_val("1.0"), &ctx),
                alloc(string_val("2.0"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_float_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(string_val("1.0"), &ctx));
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![alloc(string_val("3.14"), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_overflow() {
        // One past i64::MAX
        let ctx = test_ctx();
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("9223372036854775808"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("cannot parse"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn error_raises_with_message() {
        let ctx = test_ctx();
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![alloc(string_val("boom"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "boom");
    }

    #[tokio::test]
    async fn error_custom_message() {
        let ctx = test_ctx();
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![alloc(string_val("division by zero"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "division by zero");
    }

    #[tokio::test]
    async fn error_type_mismatch_on_non_string() {
        let ctx = test_ctx();
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
        assert!(err.kind.to_string().contains("String"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn error_arity_check() {
        let ctx = test_ctx();
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn try_success_returns_ok_dict() {
        // Any thunk forced by builtin-try wraps its value in {ok: ...}.
        let ctx = test_ctx();
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                let ok_tid = map
                    .get(&HashableValue::Str("ok".into()))
                    .copied()
                    .expect("success dict must have 'ok' key");
                let ok_val_result = mat_id(ok_tid, &ctx).await;
                assert_eq!(ok_val_result, Value::Int(42));
            }
            _ => panic!("expected Dict {{ok: ...}}, got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_success_with_string_body() {
        let ctx = test_ctx();
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![alloc(string_val("hello".into()), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                let ok_tid = map
                    .get(&HashableValue::Str("ok".into()))
                    .copied()
                    .expect("success dict must have 'ok' key");
                let ok_val_result = mat_id(ok_tid, &ctx).await;
                assert_eq!(ok_val_result, string_val("hello".into()));
            }
            _ => panic!("expected Dict {{ok: ...}}, got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_any_value_is_valid() {
        // Any value — including a function — is valid input. It is wrapped in {ok: ...}, not called.
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] x]", &ctx).await;
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![alloc(func, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        if let Value::Dict(map) = result {
            assert!(
                map.contains_key(&HashableValue::Str("ok".into())),
                "function value should be wrapped in {{ok: ...}}"
            );
        } else {
            panic!("expected Dict {{ok: ...}}");
        }
    }

    #[tokio::test]
    async fn try_arity_check() {
        let ctx = test_ctx();
        let err = run(builtin_try(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn try_pending_builtin_success() {
        // A PendingBuiltin thunk is forced by builtin-try; successful result wrapped in {ok: ...}.
        fn ok_builtin(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Int(99), rust_span!()) })
        }
        let ctx = test_ctx();
        let ok_def = builtin!("ok", ok_builtin, [], 0);
        let pending_id = ctx.alloc_thunk(
            0,
            Arc::new(Thunk::builtin_call(
                ok_def,
                vec![],
                None,
                call_span(),
                0,
                Arc::clone(&ctx),
            )),
        );
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![pending_id],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                let ok_tid = map
                    .get(&HashableValue::Str("ok".into()))
                    .copied()
                    .expect("success dict must have 'ok' key");
                let ok_val_result = mat_id(ok_tid, &ctx).await;
                assert_eq!(ok_val_result, Value::Int(99));
            }
            _ => panic!("expected Dict {{ok: ...}}, got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_catches_error_from_pending_builtin() {
        // Errors from forcing a PendingBuiltin thunk are caught and returned as {error: ...}.
        fn err_builtin(
            ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move {
                let call_span = ctx.call_span;
                Err(EvalError::internal("builtin error".to_string(), call_span).into())
            })
        }
        let ctx = test_ctx();
        let err_def = builtin!("fail", err_builtin, [], 0);
        let pending_id = ctx.alloc_thunk(
            0,
            Arc::new(Thunk::builtin_call(
                err_def,
                vec![],
                None,
                call_span(),
                0,
                Arc::clone(&ctx),
            )),
        );
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![pending_id],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                let err_tid = map
                    .get(&HashableValue::Str("error".into()))
                    .copied()
                    .expect("failure dict must have 'error' key");
                let err_val = mat_id(err_tid, &ctx).await;
                let s = format!("{err_val}");
                assert!(
                    s.contains("builtin error"),
                    "error value should contain 'builtin error', got: {s}"
                );
            }
            _ => panic!("expected Dict {{error: ...}}, got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_resource_limit_exceeded_not_catchable() {
        // ResourceLimitExceeded errors should NOT be caught — they must propagate.
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
        let ctx = test_ctx();
        let rl_def = builtin!("resource_fail", resource_limit_builtin, [], 0);
        let pending_id = ctx.alloc_thunk(
            0,
            Arc::new(Thunk::builtin_call(
                rl_def,
                vec![],
                None,
                call_span(),
                0,
                Arc::clone(&ctx),
            )),
        );
        let err = run(builtin_try(BuiltinArgs {
            args: vec![pending_id],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("exceeded resource limit"),
            "expected resource limit error to propagate, got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn apply_single_arg() {
        // [fn [x] $x] applied to [42]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx).await;
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Int(0), thunk(Value::Int(42)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn apply_multiple_args_returns_first() {
        // [fn [a b] $a] applied to [10, 20]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let a b] $a]", &ctx).await;
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Int(0), thunk(Value::Int(10)));
                m.insert(HashableValue::Int(1), thunk(Value::Int(20)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(10));
    }

    #[tokio::test]
    async fn apply_multiple_args_returns_second() {
        // [fn [a b] $b] applied to [10, 20]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let a b] $b]", &ctx).await;
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Int(0), thunk(Value::Int(10)));
                m.insert(HashableValue::Int(1), thunk(Value::Int(20)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(20));
    }

    #[tokio::test]
    async fn apply_with_builtin() {
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
                let thunk0 = ctx.get_thunk(args[0]);
                let thunk1 = ctx.get_thunk(args[1]);
                let a = materialize(&thunk0, None, &ctx).await?; // TEST: test-only inline builtin
                let b = materialize(&thunk1, None, &ctx).await?; // TEST: test-only inline builtin
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
                m.insert(HashableValue::Int(0), thunk(Value::Int(3)));
                m.insert(HashableValue::Int(1), thunk(Value::Int(4)));
                m
            },
            &ctx,
        );

        let result = mat(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(7));
    }

    #[tokio::test]
    async fn apply_arity_mismatch() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x y] $x]", &ctx).await;
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Int(0), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );

        let apply_thunk = run(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_thunk, None, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("missing argument for required parameter"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn apply_non_function_type_error() {
        let ctx = test_ctx();
        let args_val = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Int(0), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );

        let apply_thunk = run(builtin_apply(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_thunk, None, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Function"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn apply_non_dict_args_type_error() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx).await;
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn apply_wrong_arity() {
        let ctx = test_ctx();
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn type_of_int() {
        let ctx = test_ctx();
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Int".into()));
    }

    #[tokio::test]
    async fn type_of_float() {
        let ctx = test_ctx();
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(Value::Float(3.14), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Float".into()));
    }

    #[tokio::test]
    async fn type_of_string() {
        let ctx = test_ctx();
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(string_val("hi"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("String".into()));
    }

    #[tokio::test]
    async fn type_of_dict() {
        let ctx = test_ctx();
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(Value::Dict(IndexMap::new()), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Dict".into()));
    }

    #[tokio::test]
    async fn type_of_function() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] 0]", &ctx).await;
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(func, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Function".into()));
    }

    #[tokio::test]
    async fn type_of_builtin_returns_function() {
        fn dummy(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Int(0), rust_span!()) })
        }
        let builtin = Value::Builtin(crate::value::BuiltinDef {
            func: dummy,
            name: "dummy",
            pos_strictness: &[],
            force_count: 0,
        });
        let ctx = test_ctx();
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![alloc(builtin, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Function".into()));
    }

    #[tokio::test]
    async fn test_type_of_variant() {
        // type-of on a Variant returns the tycon name — the tinct-level type.
        // Color.Red has tinct type Color, not "Variant" (a Rust impl detail).
        let ctx = test_ctx();
        let variant = alloc(
            Value::Variant {
                tycon: Arc::from("Color"),
                ctor: Arc::from("Red"),
                payload: None,
            },
            &ctx,
        );
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![variant],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("Color".into()));
    }

    #[tokio::test]
    async fn type_of_arity_check() {
        let ctx = test_ctx();
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn keys_empty_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keys_int_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("a".into())));
        map.insert(HashableValue::Int(1), thunk(string_val("b".into())));
        map.insert(HashableValue::Int(2), thunk(string_val("c".into())));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                for i in 0..3 {
                    let val = mat_id(keys_map[&HashableValue::Int(i)], &ctx).await;
                    assert_eq!(val, Value::Int(i));
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keys_string_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(
            HashableValue::Str("name".into()),
            thunk(string_val("Alice".into())),
        );
        map.insert(HashableValue::Str("age".into()), thunk(Value::Int(30)));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 2);
                let k0 = mat_id(keys_map[&HashableValue::Int(0)], &ctx).await;
                assert_eq!(k0, string_val("name".into()));
                let k1 = mat_id(keys_map[&HashableValue::Int(1)], &ctx).await;
                assert_eq!(k1, string_val("age".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keys_mixed_key_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("first".into())));
        map.insert(
            HashableValue::Str("label".into()),
            thunk(string_val("second".into())),
        );
        map.insert(HashableValue::Int(5), thunk(string_val("third".into())));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(keys_map) => {
                assert_eq!(keys_map.len(), 3);
                let k0 = mat_id(keys_map[&HashableValue::Int(0)], &ctx).await;
                assert_eq!(k0, Value::Int(0));
                let k1 = mat_id(keys_map[&HashableValue::Int(1)], &ctx).await;
                assert_eq!(k1, string_val("label".into()));
                let k2 = mat_id(keys_map[&HashableValue::Int(2)], &ctx).await;
                assert_eq!(k2, Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn keys_preserves_insertion_order() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("z".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(2)));
        map.insert(HashableValue::Str("m".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map, &ctx);

        let result = mat(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(keys_map) => {
                let k0 = mat_id(keys_map[&HashableValue::Int(0)], &ctx).await;
                let k1 = mat_id(keys_map[&HashableValue::Int(1)], &ctx).await;
                let k2 = mat_id(keys_map[&HashableValue::Int(2)], &ctx).await;
                assert_eq!(k0, string_val("z".into()));
                assert_eq!(k1, string_val("a".into()));
                assert_eq!(k2, string_val("m".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn length_empty_dict() {
        let ctx = test_ctx();
        let dict = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn length_non_empty_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("b".into()), thunk(Value::Int(2)));
        map.insert(HashableValue::Str("c".into()), thunk(Value::Int(3)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn length_int_keyed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("x".into())));
        map.insert(HashableValue::Int(1), thunk(string_val("y".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn builtin_def_strictness_array_validity() {
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

    #[tokio::test]
    async fn keys_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn keys_wrong_arity_two() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![d, d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn length_wrong_arity_zero() {
        let ctx = test_ctx();
        let err = run(builtin_length(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn length_wrong_arity_two() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_length(BuiltinArgs {
            args: vec![d, d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn keys_non_dict_int() {
        let ctx = test_ctx();
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn keys_non_dict_string() {
        let ctx = test_ctx();
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("keys"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("got String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn length_string() {
        // length now supports String inputs (returns character count)
        let ctx = test_ctx();
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn length_string_empty() {
        let ctx = test_ctx();
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![alloc(string_val(""), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn length_string_unicode() {
        // Multi-byte characters: length returns char count, not byte count
        let ctx = test_ctx();
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![alloc(string_val("\u{1F600}\u{1F601}"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn replace_basic() {
        let ctx = test_ctx();
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("world"), &ctx),
                alloc(string_val("Rust"), &ctx),
                alloc(string_val("hello world"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello Rust".into()));
    }

    #[tokio::test]
    async fn replace_multiple_occurrences() {
        let ctx = test_ctx();
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("a"), &ctx),
                alloc(string_val("o"), &ctx),
                alloc(string_val("banana"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("bonono".into()));
    }

    #[tokio::test]
    async fn replace_no_match() {
        let ctx = test_ctx();
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("xyz"), &ctx),
                alloc(string_val("abc"), &ctx),
                alloc(string_val("hello"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn replace_empty_pattern() {
        let ctx = test_ctx();
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val(""), &ctx),
                alloc(string_val("-"), &ctx),
                alloc(string_val("abc"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("-a-b-c-".into()));
    }

    #[tokio::test]
    async fn replace_to_empty() {
        let ctx = test_ctx();
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("l"), &ctx),
                alloc(string_val(""), &ctx),
                alloc(string_val("hello"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("heo".into()));
    }

    #[tokio::test]
    async fn replace_output_size_limit_empty_pattern() {
        // Empty pattern with large replacement should error.
        // 1000 chars input, 100k chars replacement -> output would be ~100MB.
        let ctx = test_ctx();
        let input = "a".repeat(1000);
        let replacement = "x".repeat(100_000);
        let result = run(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val(""), &ctx),
                alloc(string_val(&replacement), &ctx),
                alloc(string_val(&input), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("replace: output would exceed"));
    }

    #[tokio::test]
    async fn replace_output_size_ok_normal_pattern() {
        // Normal pattern replacement should succeed even with moderate sizes.
        let ctx = test_ctx();
        let input = "a".repeat(1000);
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("a"), &ctx),
                alloc(string_val("bb"), &ctx),
                alloc(string_val(&input), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        // 1000 'a' replaced with 'bb' -> 2000 'b'
        assert_eq!(result, string_val(&"b".repeat(2000)));
    }

    #[tokio::test]
    async fn trim_basic() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("  hello  "), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_leading_only() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("   hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_trailing_only() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("hello   "), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_no_whitespace() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_all_whitespace() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("   "), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("".into()));
    }

    #[tokio::test]
    async fn trim_tabs_and_newlines() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("\t\nhello\n\t"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_empty() {
        let ctx = test_ctx();
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val(""), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("".into()));
    }

    #[tokio::test]
    async fn replace_wrong_arity() {
        let ctx = test_ctx();
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![alloc(string_val("a"), &ctx), alloc(string_val("b"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn trim_wrong_arity() {
        let ctx = test_ctx();
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("a"), &ctx), alloc(string_val("b"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn replace_wrong_type_pattern() {
        let ctx = test_ctx();
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(Value::Int(1), &ctx),
                alloc(string_val("b"), &ctx),
                alloc(string_val("abc"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn replace_wrong_type_replacement() {
        let ctx = test_ctx();
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("a"), &ctx),
                alloc(Value::Dict(IndexMap::new()), &ctx),
                alloc(string_val("abc"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected String"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn replace_wrong_type_input() {
        let ctx = test_ctx();
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("a"), &ctx),
                alloc(string_val("b"), &ctx),
                alloc(Value::Float(3.14), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn trim_wrong_type() {
        let ctx = test_ctx();
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![alloc(Value::Float(3.14), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
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

    #[tokio::test]
    async fn trim_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(string_val("hi"), &ctx));
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![alloc(string_val("  hello  "), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn error_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![alloc(string_val("boom"), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn type_of_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn to_int_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![alloc(string_val("42"), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn replace_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("x".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                alloc(string_val("a"), &ctx),
                alloc(string_val("b"), &ctx),
                alloc(string_val("abc"), &ctx),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn add_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(99), &ctx));
        let err = run(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn sub_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(3), &ctx), alloc(Value::Int(1), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn mul_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(2), &ctx), alloc(Value::Int(3), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn div_float_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Int(3), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn eq_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_eq_int(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn lt_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let err = run(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn keys_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let dict = thunk_dict(
            {
                let mut m = IndexMap::new();
                m.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
                m
            },
            &ctx,
        );
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: Some(named),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn length_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let map = IndexMap::new();
        let err = run(builtin_length(BuiltinArgs {
            args: vec![alloc(Value::Dict(map), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn try_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let func = parse_eval("[fn [let] 42]", &ctx).await;
        let err = run(builtin_try(BuiltinArgs {
            args: vec![alloc(func, &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn apply_rejects_named_args() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("extra".into(), alloc(Value::Int(1), &ctx));
        let func = parse_eval("[fn [let] 42]", &ctx).await;
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![alloc(func, &ctx), alloc(Value::Dict(IndexMap::new()), &ctx)],
            named: Some(named),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &ctx)
            .await
            .unwrap_err();
        assert!(
            err.kind.to_string().contains("named arguments"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn core_builtins_count() {
        let count = crate::builtins_core::core_builtins().len();
        assert!(
            count > 75,
            "expected core builtins to have >75 entries, got {count}"
        );
    }

    #[tokio::test]
    async fn datetime_builtins_count() {
        let count = crate::builtins_datetime::datetime_builtins().len();
        assert!(
            count > 10,
            "expected datetime builtins to have >10 entries, got {count}"
        );
    }

    #[tokio::test]
    async fn net_builtins_count() {
        let count = crate::builtins_net::net_builtins().len();
        assert!(
            count > 5,
            "expected net builtins to have >5 entries, got {count}"
        );
    }

    #[tokio::test]
    async fn add_int_int() {
        let ctx = test_ctx();
        let r = mat(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(3), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(8));
    }

    #[tokio::test]
    async fn add_int_float() {
        // Int+Float is now two steps: int-to-float then float-add.
        // Test: [3 as Float] + 2.5 = 5.5
        let ctx = test_ctx();
        let r = mat(builtin_float_add(BuiltinArgs {
            args: vec![
                alloc(Value::Float(3.0), &ctx),
                alloc(Value::Float(2.5), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(5.5));
    }

    #[tokio::test]
    async fn add_float_float() {
        let ctx = test_ctx();
        let r = mat(builtin_float_add(BuiltinArgs {
            args: vec![
                alloc(Value::Float(1.5), &ctx),
                alloc(Value::Float(2.5), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(4.0));
    }

    #[tokio::test]
    async fn add_negative_ints() {
        let ctx = test_ctx();
        let r = mat(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(-10), &ctx), alloc(Value::Int(3), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(-7));
    }

    #[tokio::test]
    async fn add_zeros() {
        let ctx = test_ctx();
        let r = mat(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(0), &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn add_type_error_string() {
        let ctx = test_ctx();
        let e = run(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        // Non-Int/Float operands produce a TypeMismatch error.
        assert!(
            matches!(&e.kind, crate::error::ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch error for Int + String, got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn add_arity_one_arg() {
        let ctx = test_ctx();
        let e = run(builtin_int_add(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn add_overflow_error() {
        let ctx = test_ctx();
        let err = run(builtin_int_add(BuiltinArgs {
            args: vec![
                alloc(Value::Int(i64::MAX), &ctx),
                alloc(Value::Int(1), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn sub_overflow_error() {
        let ctx = test_ctx();
        let err = run(builtin_int_sub(BuiltinArgs {
            args: vec![
                alloc(Value::Int(i64::MIN), &ctx),
                alloc(Value::Int(1), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn sub_int_int() {
        let ctx = test_ctx();
        let r = mat(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Int(3), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(7));
    }

    #[tokio::test]
    async fn sub_float_float() {
        let ctx = test_ctx();
        let r = mat(builtin_float_sub(BuiltinArgs {
            args: vec![
                alloc(Value::Float(10.5), &ctx),
                alloc(Value::Float(3.5), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(7.0));
    }

    #[tokio::test]
    async fn sub_result_negative() {
        let ctx = test_ctx();
        let r = mat(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(3), &ctx), alloc(Value::Int(10), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(-7));
    }

    #[tokio::test]
    async fn sub_to_zero() {
        let ctx = test_ctx();
        let r = mat(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn sub_arity_zero_args() {
        let ctx = test_ctx();
        let e = run(builtin_int_sub(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn sub_arity_one_arg() {
        let ctx = test_ctx();
        let e = run(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn sub_type_error_string() {
        let ctx = test_ctx();
        let e = run(builtin_int_sub(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        // Non-Int/Float operands produce a TypeMismatch error.
        assert!(
            matches!(&e.kind, crate::error::ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch error for Int - String, got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn mul_int_int() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(4), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(20));
    }

    #[tokio::test]
    async fn mul_int_float() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(4), &ctx), alloc(Value::Float(2.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(10.0));
    }

    #[tokio::test]
    async fn mul_float_int() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Float(2.5), &ctx), alloc(Value::Int(4), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(10.0));
    }

    #[tokio::test]
    async fn mul_float_float() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![
                alloc(Value::Float(2.5), &ctx),
                alloc(Value::Float(3.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(7.5));
    }

    #[tokio::test]
    async fn mul_by_zero() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn mul_negative() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(-3), &ctx), alloc(Value::Int(4), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(-12));
    }

    #[tokio::test]
    async fn mul_by_negative_one() {
        let ctx = test_ctx();
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![alloc(Value::Int(42), &ctx), alloc(Value::Int(-1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(-42));
    }

    #[tokio::test]
    async fn mul_overflow_error() {
        let ctx = test_ctx();
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![
                alloc(Value::Int(i64::MAX), &ctx),
                alloc(Value::Int(2), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("integer overflow"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn add_float_overflow_to_infinity_is_error() {
        let ctx = test_ctx();
        let err = run(builtin_float_add(BuiltinArgs {
            args: vec![
                alloc(Value::Float(1e308), &ctx),
                alloc(Value::Float(1e308), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn sub_float_nan_is_error() {
        // f64::INFINITY - f64::INFINITY = NaN
        let ctx = test_ctx();
        let err = run(builtin_float_sub(BuiltinArgs {
            args: vec![
                alloc(Value::Float(f64::INFINITY), &ctx),
                alloc(Value::Float(f64::INFINITY), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn mul_float_overflow_to_infinity_is_error() {
        let ctx = test_ctx();
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![
                alloc(Value::Float(1e308), &ctx),
                alloc(Value::Float(10.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn div_float_nan_result_is_error() {
        // 0.0 / 0.0 produces NaN; the existing b==0.0 guard catches b==0.0,
        // but this test documents that NaN results from non-zero / 0-adjacent
        // ops are also caught. Use f64::NAN inputs via Float values directly:
        // f64::INFINITY / f64::INFINITY = NaN
        let ctx = test_ctx();
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(f64::INFINITY), &ctx),
                alloc(Value::Float(f64::INFINITY), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("is not a finite"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn div_float_int_int_returns_float() {
        let ctx = test_ctx();
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Int(3), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn div_float_int_int_exact_returns_float() {
        let ctx = test_ctx();
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Int(2), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        match r {
            Value::Float(f) => assert_eq!(f, 5.0),
            other => panic!("expected Float(5.0), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn div_float_int_float() {
        let ctx = test_ctx();
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Float(3.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        match r {
            Value::Float(f) => {
                assert!((f - 10.0 / 3.0).abs() < 1e-10, "expected ~3.333, got {f}")
            }
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn div_float_float_float() {
        let ctx = test_ctx();
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(7.5), &ctx),
                alloc(Value::Float(2.5), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(3.0));
    }

    #[tokio::test]
    async fn div_float_by_zero_int() {
        let ctx = test_ctx();
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            matches!(e.kind, crate::error::ErrorKind::DivisionByZero { .. }),
            "expected DivisionByZero, got: {}",
            e.kind
        );
        assert!(
            e.kind.to_string().contains("division by zero"),
            "expected message containing 'division by zero', got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn div_float_by_zero_float() {
        // Float / Float(0.0) produces Inf which check_float_result rejects as non-finite.
        let ctx = test_ctx();
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(10.0), &ctx),
                alloc(Value::Float(0.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("is not a finite number"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn div_float_by_zero_mixed() {
        // Int / Float(0.0) produces Inf which check_float_result rejects as non-finite.
        let ctx = test_ctx();
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), alloc(Value::Float(0.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("is not a finite number"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn div_float_negative_zero() {
        let ctx = test_ctx();
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(-0.0), &ctx),
                alloc(Value::Float(1.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Float(0.0));
    }

    #[tokio::test]
    async fn eq_int_int_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_int(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_int_int_not_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_int(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Int(6), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    // ── builtin-eq-float tests ──────────────────────────────────────────────────────────
    #[tokio::test]
    async fn eq_float_float_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(3.14), &ctx),
                alloc(Value::Float(3.14), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_float_float_not_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(3.14), &ctx),
                alloc(Value::Float(2.71), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_nan_not_equal_to_self() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(f64::NAN), &ctx),
                alloc(Value::Float(f64::NAN), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_negative_zero_float() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_float(BuiltinArgs {
            args: vec![
                alloc(Value::Float(-0.0), &ctx),
                alloc(Value::Float(0.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    // ── builtin-eq-string tests ─────────────────────────────────────────────────────────
    #[tokio::test]
    async fn eq_string_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_string(BuiltinArgs {
            args: vec![
                alloc(string_val("hello"), &ctx),
                alloc(string_val("hello"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_string_not_equal() {
        let ctx = test_ctx();
        let r = mat(builtin_eq_string(BuiltinArgs {
            args: vec![
                alloc(string_val("hello"), &ctx),
                alloc(string_val("world"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_arity_error() {
        let ctx = test_ctx();
        let e = run(builtin_eq_int(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn lt_int_int_true() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(3), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_int_int_false() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Int(3), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_int_int_equal_is_false() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_float_float() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(Value::Float(2.5), &ctx),
                alloc(Value::Float(3.5), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_string_lexicographic() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(string_val("apple"), &ctx),
                alloc(string_val("banana"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_string_lexicographic_reverse() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(string_val("banana"), &ctx),
                alloc(string_val("apple"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_string_equal_is_false() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(string_val("same"), &ctx),
                alloc(string_val("same"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_string_prefix() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(string_val("ab"), &ctx),
                alloc(string_val("abc"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_int_float() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(3), &ctx), alloc(Value::Float(3.5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_float_int() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Float(2.5), &ctx), alloc(Value::Int(3), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_equal_values() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx), alloc(Value::Float(5.0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_incompatible_types_error() {
        let ctx = test_ctx();
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(string_val("hello"), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[tokio::test]
    async fn lt_int_false_lt_true() {
        // Int(0) < Int(1) — canonical false < true
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(0), &ctx), alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_int_true_not_lt_false() {
        // Int(1) < Int(0) is false — canonical true is not less than false
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_int_false_not_lt_false() {
        // Int(0) < Int(0) is false
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(0), &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_int_true_not_lt_true() {
        // Int(1) < Int(1) is false
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_dict_error() {
        let ctx = test_ctx();
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(Value::Dict(IndexMap::new()), &ctx),
                alloc(Value::Dict(IndexMap::new()), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[tokio::test]
    async fn lt_arity_error() {
        let ctx = test_ctx();
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("arity mismatch"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn lt_negative_numbers() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![alloc(Value::Int(-10), &ctx), alloc(Value::Int(-5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_nan_float() {
        let ctx = test_ctx();
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                alloc(Value::Float(f64::NAN), &ctx),
                alloc(Value::Float(1.0), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    /// Parse-only smoke test for the prelude. Evaluating the full prelude requires a
    #[tokio::test]
    async fn build_core_env_has_builtins() {
        let env = build_core_env();
        let env_ref = env.read().unwrap();
        // After T-1557, Env is type-metadata only. Check that builtin names are
        // registered in the slotted IndexMap (so the resolver can assign coordinates).
        let slot_names = env_ref.slot_names();
        assert!(
            slot_names.iter().any(|n| n == "builtin-raise"),
            "missing builtin builtin-raise in core env slots"
        );
        // Prelude functions are NOT in core_env — they are loaded via run_loader_pipeline.
        assert!(
            !slot_names.iter().any(|n| n == "map"),
            "map should not be in core env (requires run_loader_pipeline)"
        );
    }

    // === take builtin tests ===

    #[tokio::test]
    async fn take_dict_basic() {
        // take(2, [a: 1, b: 2, c: 3]) → [a: 1, b: 2]
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("b".into()), thunk(Value::Int(2)));
        map.insert(HashableValue::Str("c".into()), thunk(Value::Int(3)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: vec![alloc(Value::Int(2), &ctx), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Str("a".into())).unwrap(), &ctx).await,
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Str("b".into())).unwrap(), &ctx).await,
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn take_dict_zero() {
        // take(0, dict) → []
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: vec![alloc(Value::Int(0), &ctx), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn take_dict_negative() {
        // take(-5, dict) → []
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: vec![alloc(Value::Int(-5), &ctx), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) if map.is_empty() => {} // Success
            other => panic!("expected empty dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn take_dict_more_than_length() {
        // take(10, [a: 1, b: 2]) → [a: 1, b: 2]
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("b".into()), thunk(Value::Int(2)));
        let dict_val = thunk_dict(map, &ctx);

        let result = mat(builtin_take(BuiltinArgs {
            args: vec![alloc(Value::Int(10), &ctx), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn take_n_non_int() {
        let ctx = test_ctx();
        let result = run(builtin_take(BuiltinArgs {
            args: vec![
                alloc(string_val("not int"), &ctx),
                alloc(Value::Int(1), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn take_xs_non_dict_or_seq() {
        let ctx = test_ctx();
        let result = run(builtin_take(BuiltinArgs {
            args: vec![
                alloc(Value::Int(5), &ctx),
                alloc(string_val("not dict or seq"), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn take_arity_one() {
        let ctx = test_ctx();
        let result = run(builtin_take(BuiltinArgs {
            args: vec![alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn concat_dict_empty_xs_returns_ys() {
        // concat({}, ys) returns ys unchanged (empty xs short-circuit)
        let ctx = test_ctx();
        let xs = alloc(Value::Dict(IndexMap::new()), &ctx);
        let mut ys_map = IndexMap::new();
        ys_map.insert(HashableValue::Int(0), thunk(Value::Int(1)));
        let ys = thunk_dict(ys_map, &ctx);

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await,
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn concat_dict_empty_ys() {
        // concat({0:1, 1:2}, {}) → {0:1, 1:2} (empty ys is a no-op via reindexing)
        let ctx = test_ctx();
        let mut xs_map = IndexMap::new();
        xs_map.insert(HashableValue::Int(0), thunk(Value::Int(1)));
        xs_map.insert(HashableValue::Int(1), thunk(Value::Int(2)));
        let xs = thunk_dict(xs_map, &ctx);
        let ys = alloc(Value::Dict(IndexMap::new()), &ctx);

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await,
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(1)).unwrap(), &ctx).await,
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn concat_dict() {
        // concat([1, 2], [3, 4]) -> [1, 2, 3, 4] with integer reindexing
        let ctx = test_ctx();
        let mut xs_map = IndexMap::new();
        xs_map.insert(HashableValue::Int(0), thunk(Value::Int(1)));
        xs_map.insert(HashableValue::Int(1), thunk(Value::Int(2)));
        let xs = thunk_dict(xs_map, &ctx);

        let mut ys_map = IndexMap::new();
        ys_map.insert(HashableValue::Int(0), thunk(Value::Int(3)));
        ys_map.insert(HashableValue::Int(1), thunk(Value::Int(4)));
        let ys = thunk_dict(ys_map, &ctx);

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;

        match result {
            Value::Dict(ref map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await,
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(1)).unwrap(), &ctx).await,
                    Value::Int(2)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(2)).unwrap(), &ctx).await,
                    Value::Int(3)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(3)).unwrap(), &ctx).await,
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn concat_variant_xs_is_type_error() {
        // concat(Variant, anything) → type error: builtin_concat is Dict-only.
        let ctx = test_ctx();
        let xs = alloc(
            Value::Variant {
                tycon: Arc::from("Color"),
                ctor: Arc::from("Red"),
                payload: None,
            },
            &ctx,
        );
        let ys = alloc(Value::Dict(IndexMap::new()), &ctx);

        let err = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();

        assert!(
            matches!(err.kind, crate::error::ErrorKind::TypeMismatch { .. }),
            "expected TypeMismatch for Variant xs, got {:?}",
            err.kind
        );
    }

    #[tokio::test]
    async fn concat_dict_basic() {
        // Task 4: Test $concat with two small dicts to verify correct behavior
        // This exercises the checked_add call site that prevents integer overflow
        let ctx = test_ctx();
        let mut dict1 = IndexMap::new();
        dict1.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        dict1.insert(HashableValue::Str("b".into()), thunk(Value::Int(2)));

        let mut dict2 = IndexMap::new();
        dict2.insert(HashableValue::Str("c".into()), thunk(Value::Int(3)));
        dict2.insert(HashableValue::Str("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![thunk_dict(dict1, &ctx), thunk_dict(dict2, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;

        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                // All values should be reindexed with integer keys 0, 1, 2, 3
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await,
                    Value::Int(1)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(1)).unwrap(), &ctx).await,
                    Value::Int(2)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(2)).unwrap(), &ctx).await,
                    Value::Int(3)
                );
                assert_eq!(
                    mat_id(*map.get(&HashableValue::Int(3)).unwrap(), &ctx).await,
                    Value::Int(4)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn concat_empty_xs_dict_ys_valid_dict_succeeds() {
        // Task 2: When xs is empty Dict and ys is a valid Dict, concat should succeed.
        let ctx = test_ctx();
        let xs = alloc(Value::Dict(IndexMap::new()), &ctx); // empty dict
        let mut ys_map = IndexMap::new();
        ys_map.insert(HashableValue::Int(0), thunk(Value::Int(99)));
        let ys = thunk_dict(ys_map, &ctx);

        // Should succeed and return ys (the same thunk or an equivalent materialized form)
        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;

        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let val = mat_val(result.unwrap()).await;
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1);
                assert_eq!(
                    mat_id(*m.get(&HashableValue::Int(0)).unwrap(), &ctx).await,
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_proxy_returns_proxy_value() {
        let ctx = test_ctx();
        let handler = alloc(Value::Int(42), &ctx);
        let result = run(builtin_proxy(BuiltinArgs {
            args: vec![handler],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap();

        let val = mat_val(result).await;
        match val {
            Value::Proxy { handler: h } => {
                // Verify the handler thunk contains the expected value.
                let handler_val = mat_id(h, &ctx).await;
                assert_eq!(handler_val, Value::Int(42));
            }
            other => panic!("expected Proxy, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_proxy_arity_error() {
        // Zero args
        let ctx = test_ctx();
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );

        // Two args
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), alloc(Value::Int(2), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );

        // Three args
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![
                alloc(Value::Int(1), &ctx),
                alloc(Value::Int(2), &ctx),
                alloc(Value::Int(3), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("arity mismatch"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn test_proxy_named_arg_error() {
        let ctx = test_ctx();
        let mut named = IndexMap::new();
        named.insert("handler".to_string(), alloc(Value::Int(42), &ctx));

        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: Some(named),
            call_span: call_span(),
            ctx,
            caller_env_id: 0,
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind
                .to_string()
                .contains("does not accept named arguments"),
            "got: {}",
            err.kind
        );
    }

    /// Verify that `build_core_env()` returns an env with all core builtins.
    ///
    /// This is a wholeness test for the core env layer. Prelude functions are NOT
    /// expected here — they are loaded via `run_loader_pipeline`.
    #[tokio::test]
    async fn test_core_env_wholeness() {
        let core_env = build_core_env();
        let env = core_env.read().unwrap();

        // After T-1557, Env is type-metadata only. Verify builtin names are present
        // in the slotted IndexMap (resolver coordinate assignment).
        let slot_names = env.slot_names();

        // Core builtins that must exist in the slotted IndexMap.
        // These are the names that the resolver maps to (level, slot) coordinates
        // so that eval can look them up in the root FlatEnv.
        let required_names: &[&str] = &[
            "builtin-raise",
            "builtin-type-of",
            "builtin-keys",
            "builtin-get",
            "builtin-int-add",
            "builtin-float-add",
            "builtin-int-sub",
            "builtin-int-mul",
            "builtin-dict-length",
        ];

        for name in required_names {
            assert!(
                slot_names.iter().any(|n| n == name),
                "core env slots are missing expected builtin: {name}"
            );
        }

        // Prelude functions must NOT be in core env (they come from run_loader_pipeline).
        assert!(
            !slot_names.iter().any(|n| n == "map"),
            "map should not be in core env slots"
        );
        assert!(
            !slot_names.iter().any(|n| n == "filter"),
            "filter should not be in core env slots"
        );
    }

    // ========== dict-nth/dict-key-nth/dict-kv-nth tests ==========

    #[tokio::test]
    async fn dict_nth_in_bounds() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(10)));
        map.insert(HashableValue::Str("b".into()), thunk(Value::Int(20)));
        map.insert(HashableValue::Str("c".into()), thunk(Value::Int(30)));
        let result = mat(builtin_dict_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), alloc(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, Value::Int(20));
    }

    #[tokio::test]
    async fn dict_nth_out_of_bounds() {
        // builtin-dict-nth now errors on out-of-bounds access.
        // Prelude step functions guard with builtin-dict-has-nth? before calling this.
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(10)));
        let result = run(builtin_dict_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), alloc(Value::Int(5), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert!(
            result.is_err(),
            "builtin-dict-nth must error on out-of-bounds index"
        );
    }

    #[tokio::test]
    async fn dict_key_nth_string_key() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("foo".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("bar".into()), thunk(Value::Int(2)));
        let result = mat(builtin_dict_key_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert!(matches!(result, Value::String { .. }));
    }

    #[tokio::test]
    async fn dict_kv_nth_returns_kv_pair() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("x".into()), thunk(Value::Int(42)));
        let result = mat(builtin_dict_kv_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), alloc(Value::Int(0), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        match result {
            Value::Dict(m) => {
                assert!(m.contains_key(&HashableValue::Str("key".into())));
                assert!(m.contains_key(&HashableValue::Str("value".into())));
            }
            other => panic!("expected Dict kv pair, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn builtin_get_int_key_auto_indexed_dict() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("first".into())));
        map.insert(HashableValue::Int(1), thunk(string_val("second".into())));
        map.insert(HashableValue::Int(2), thunk(string_val("third".into())));
        let result = mat(builtin_get(BuiltinArgs {
            args: vec![alloc(Value::Int(1), &ctx), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        assert_eq!(result, string_val("second".into()));
    }

    #[tokio::test]
    async fn builtin_get_key_not_found_error() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(10)));
        map.insert(HashableValue::Str("b".into()), thunk(Value::Int(20)));
        let result = run(builtin_get(BuiltinArgs {
            args: vec![alloc(string_val("z"), &ctx), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
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
    #[tokio::test]
    async fn builtin_keys_does_not_force_dict_values() {
        let ctx = test_ctx();
        // Build a dict whose VALUES are bomb thunks: materializing them would fail.
        // `$keys` should enumerate the keys without ever touching the values.
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), make_undef_thunk(&ctx));
        map.insert(HashableValue::Str("b".into()), make_undef_thunk(&ctx));
        map.insert(HashableValue::Str("c".into()), make_undef_thunk(&ctx));
        let dict = thunk_dict(map, &ctx);

        // builtin_keys should succeed: it only reads keys, not values.
        let result = run(builtin_keys(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_keys must not force dict values; got error: {:?}",
                e
            )
        });
        // The result should be a dict with 3 entries (one per key).
        let val = mat_val(result_thunk).await;
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
    #[tokio::test]
    async fn builtin_length_does_not_force_dict_values() {
        let ctx = test_ctx();
        // Build a dict with 4 bomb-value entries.
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), make_undef_thunk(&ctx));
        map.insert(HashableValue::Int(1), make_undef_thunk(&ctx));
        map.insert(HashableValue::Int(2), make_undef_thunk(&ctx));
        map.insert(HashableValue::Int(3), make_undef_thunk(&ctx));
        let dict = thunk_dict(map, &ctx);

        // builtin_length should succeed: it only counts entries, not values.
        let result = run(builtin_length(BuiltinArgs {
            args: vec![dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env_id: 0,
        }))
        .await;
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_length must not force dict values; got error: {:?}",
                e
            )
        });
        let val = mat_val(result_thunk).await;
        assert_eq!(val, Value::Int(4), "expected length 4, got {:?}", val);
    }
}
