//! Builtin registry, bootstrap, and slim helpers for the LLT language.
//!
//! Builtin implementations live in split files (`builtins_math.rs`, `builtins_io.rs`, etc.).
//! This file provides:
//! - `builtin_module(name)` — dispatch to per-module aggregators (`core_builtins()`, etc.)
//! - `type_env_module(name)` — dispatch to per-module type environments
//! - `build_builtins_type_env()` — combined type env for all modules
//! - `build_core_env()` — build a fresh env with only the core Rust builtins (the starting point for run_loader_pipeline)
//! - Helper functions: `ok_val`, `string_val`, `reject_named`, `require_string`, etc.
//! - Re-exports of split-file functions for test access via `use super::*`

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{CoreExpr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::rust_span;
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
use crate::eval_call::{invoke_function, CallContext};
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
}
pub(crate) use builtin;

/// Maximum collection size for $collect (1,000,000 elements).
/// Prevents memory exhaustion from infinite sequences without $take.
pub const MAX_COLLECT_SIZE: usize = 1_000_000;

/// Maximum string output size for string output builtins (`$replace`, `$str-map-chars`, `$join`) (64 MB).
/// Prevents memory exhaustion from adversarial inputs or replacement patterns.
pub(crate) const MAX_STRING_SIZE: usize = 64 * 1024 * 1024;

pub(crate) fn ok_val(v: Value, span: Span) -> EvalResult<Arc<Thunk>> {
    Ok(Arc::new(Thunk::new_materialized(v, span)))
}

/// Convert a `Value::Bytes` slice into a Seq of `Value::Int` (one per byte).
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
        let thunk = ctx.thunk_arena.lock().unwrap().get(thunk_id).clone();
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
    builtin_builder_snapshot, builtin_dict_key_nth, builtin_dict_kv_nth, builtin_dict_nth,
    builtin_get, builtin_get_optional, builtin_keys, builtin_length, builtin_make_builder,
    builtin_merge,
};

// Type/eval/meta builtins: type-of, include, error, try, apply, validate.
// Implementations live in builtins_meta.rs; re-exported here so that
// builtin_module() registration and unit tests (via `use super::*`) still work.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_meta::{
    builtin_annotation_of, builtin_apply, builtin_ast_of, builtin_big_int, builtin_blake3,
    builtin_cap_identity, builtin_decimal, builtin_eval, builtin_eval_types,
    builtin_force, builtin_gensym, builtin_include_cache_get, builtin_include_cache_put,
    builtin_llt_repr, builtin_load, builtin_macro_error, builtin_macro_injects,
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
    builtin_str_bytes, builtin_str_index_of, builtin_str_length,
    builtin_str_map_chars, builtin_str_nth_char, builtin_str_slice, builtin_str_to_lower_char,
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
        ..
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

/// Register `builtin-*` type aliases for core numeric builtins in this file (T-1102).
///
/// Covers `floor`, `round`, and `to-float`, which are implemented in `builtins.rs`
/// and not claimed by any other per-file function. Each alias copies the TypeScheme
/// from the canonical name already registered in `core_type_env`.
/// Call this AFTER `core_type_env` has run.
pub fn core_builtin_types(env: &mut crate::types::TypeEnv) {
    env.alias_types(&[
        ("builtin-floor", "floor"),
        ("builtin-round", "round"),
        ("builtin-to-float", "to-float"),
    ]);
}

// Re-exported here for test access via `use super::*`.
#[allow(unused_imports)] // used in test modules via `use super::*`
pub(crate) use crate::builtins_dict::{builtin_concat, builtin_drop, builtin_take};

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
        ..
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
                )
                .await?;
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
        ..
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
                    require_dict("last", other, args[0].span.clone(), &ctx, call_span.clone())
                        .await?;
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

/// `builtin-rest`: Returns all elements of a Dict except the first, reindexed 0..n-1.
///
/// - Takes 1 arg: a Dict. Dict path only — O(n).
/// - For Seq use the tinct-defined `tail` in prelude.
///
/// Inherently materializing: must copy all remaining entries.
pub(crate) fn builtin_rest(
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
        reject_named("rest", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Spine");
        let map = require_dict("rest", val, args[0].span.clone(), &ctx, call_span.clone()).await?;

        // Skip the first entry (index 0 by insertion order), reindex rest as 0..n-1.
        let mut result = IndexMap::with_capacity(map.len().saturating_sub(1));
        for (new_idx, (_old_key, thunk)) in map.into_iter().skip(1).enumerate() {
            let new_key = HashableValue::Int(i64::try_from(new_idx).map_err(|_| {
                EvalError::internal("collection index overflow".to_string(), call_span.clone())
            })?);
            result.insert(new_key, thunk);
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
        ..
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
        )
        .await?;

        let mut result = IndexMap::with_capacity(map.len());
        // Collect values in reverse insertion order.
        let entries: Vec<_> = map.into_iter().collect();
        for (new_idx, (_old_key, thunk)) in entries.into_iter().rev().enumerate() {
            let new_key = HashableValue::Int(i64::try_from(new_idx).map_err(|_| {
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
        (
            Value::Variant { tag: a_tag, payload: None },
            Value::Variant { tag: b_tag, payload: None },
        ) if (a_tag == "Boolean.True" || a_tag == "Boolean.False")
            && (b_tag == "Boolean.True" || b_tag == "Boolean.False") =>
        {
            a_tag.cmp(b_tag) // "Boolean.False" < "Boolean.True" lexically → false < true ✓
        }
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
        ..
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
        let map = require_dict("sort", val, dict_span, &ctx, call_span.clone()).await?;

        // Materialize all values so we can compare them.
        let mut pairs: Vec<(Value, Span)> = Vec::with_capacity(map.len());
        for (_key, thunk_id) in &map {
            let thunk = ctx.get_thunk(*thunk_id);
            let mat = materialize(&thunk, Some(&call_span), &ctx).await?;
            pairs.push((mat, thunk.span.clone()));
        }

        // Sort using comparator or natural ordering.
        if let Some((cmp_val, cmp_span)) = comparator_opt {
            // Use custom comparator function — async insertion sort (stable, correct).
            // pairs.sort_by cannot .await inside the closure, so we use an explicit loop.
            for i in 1..pairs.len() {
                let mut j = i;
                while j > 0 {
                    let (a_val, a_span) = pairs[j - 1].clone();
                    let (b_val, b_span) = pairs[j].clone();
                    let a_thunk = Arc::new(Thunk::new_materialized(a_val, a_span.clone()));
                    let b_thunk = Arc::new(Thunk::new_materialized(b_val, b_span.clone()));
                    let pos_args = vec![a_thunk, b_thunk];

                    let result_thunk = match &cmp_val {
                        Value::Function {
                            params,
                            body,
                            env: closure_env,
                            ..
                        } => {
                            invoke_function(&CallContext {
                                params,
                                body,
                                closure_env,
                                positional: &pos_args,
                                named: None,
                                default_env: closure_env,
                                call_span: call_span.clone(),
                                origin: Some(Arc::from("sort")),
                                ctx: &ctx,
                            })
                            .await?
                        }
                        Value::Builtin(def) => {
                            let builtin_args = BuiltinArgs {
                                args: pos_args,
                                named: None,
                                call_span: call_span.clone(),
                                caller_env: Arc::new(std::sync::RwLock::new(
                                    crate::value::Environment::new(),
                                )),
                                ctx: Arc::clone(&ctx),
                            };
                            (def.func)(builtin_args).await?
                        }
                        _ => {
                            return Err(EvalError::type_mismatch_ctx(
                                "sort".to_string(),
                                "Function",
                                cmp_val.type_name(),
                                cmp_span.clone(),
                            )
                            .into());
                        }
                    };

                    let result_val = materialize(&result_thunk, Some(&call_span), &ctx).await?;
                    if result_val.is_truthy() {
                        // truthy means a > b → swap
                        pairs.swap(j - 1, j);
                        j -= 1;
                    } else {
                        // falsy means a <= b → already in order
                        break;
                    }
                }
            }
        } else {
            // Use natural ordering — sync sort_by is fine here (no async needed).
            let mut sort_error: Option<Box<crate::error::EvalError>> = None;
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
            if let Some(e) = sort_error {
                return Err(e);
            }
        }

        // Build result dict with dense integer keys 0..n-1, wrapping sorted values as thunks.
        let mut result = IndexMap::with_capacity(pairs.len());
        for (new_idx, (mat_val, orig_span)) in pairs.into_iter().enumerate() {
            let new_key = HashableValue::Int(i64::try_from(new_idx).map_err(|_| {
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
        ..
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

/// Return the builtin list for a named module, or None if the name is unknown.
///
/// Modules "io", "math", "meta", "dict", "string", "seq", "async" return empty
/// lists because their builtins are injected into the prelude scope directly
/// (not via the module system). They are declared in prelude.llt's --- uses:
/// header for documentation/intent, but have no native registrations.
pub fn builtin_module(name: &str) -> Option<Vec<crate::value::BuiltinDef>> {
    match name {
        "core" => Some(crate::builtins_core::core_builtins()),
        "datetime" => Some(crate::builtins_datetime::datetime_builtins()),
        "net" => Some(crate::builtins_net::net_builtins()),
        // Declared in prelude --- uses: but have no native-level registrations.
        // Return empty so uses-scope doesn't fail.
        "io" | "math" | "meta" | "dict" | "string" | "seq" | "async" => Some(vec![]),
        _ => None,
    }
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
pub fn build_core_env() -> Arc<RwLock<crate::value::Environment>> {
    let env = Arc::new(RwLock::new(crate::value::Environment::new()));
    if let Some(defs) = builtin_module("core") {
        let mut env_write = env.write().unwrap();
        for def in defs {
            let name = def.name.to_string();
            let thunk = Arc::new(Thunk::new_materialized(
                crate::value::Value::Builtin(def),
                rust_span!(),
            ));
            env_write.insert(name, thunk);
        }
    }
    env
}

/// Build a `TypeEnv` containing type signatures for all builtin modules (core, datetime, net).
///
/// The combined env includes all registrations from `core_type_env()`, `datetime_type_env()`,
/// and `net_type_env()`. The result is a flat environment (no parent chain) suitable for use as
/// the baseline for prelude type-checking and builtin-aware type inference.
pub fn build_builtins_type_env() -> crate::types::TypeEnv {
    use crate::type_def::{Row, RowTail, TyConDef, Type, Variance};

    let mut env = crate::types::TypeEnv::new();
    crate::builtins_core::core_type_env(&mut env);
    crate::builtins_datetime::datetime_type_env(&mut env);
    env.merge(crate::builtins_net::net_type_env());

    // Register root-scope TyConDefs for all primitive type names (T-1296).
    //
    // These enable the unified type-stage env lookup path to resolve primitive type names
    // through TyCon lookup instead of a hardwired string-match bypass list.
    // The body holds the concrete primitive Type so callers that read TyConDef.body directly
    // (e.g., type display, annotation-of) see the correct underlying type.
    // builtin_type: Some(name) marks each as opaque — expand_named returns a bare TyCon leaf
    // without structural expansion (same treatment as Seq/Map/Handle in InferState::new()).
    // params: vec![] and variance: vec![] for zero-parameter primitives.

    // Zero-parameter primitives
    for (name, body) in [
        ("Int", Type::Int),
        ("Float", Type::Float),
        ("Bytes", Type::Bytes),
        ("String", Type::Str),
        ("Unknown", Type::Unknown),
        ("Any", Type::Any),
    ] {
        env.insert_tycon_def(
            name.to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some(name.to_string()),
                annotation: None,
                field_annotations: IndexMap::new(),
                constructor_constants: IndexMap::new(),
            }),
        );
    }

    // One-parameter type constructors (* → *)
    // Handle: Cap → Handle[Cap]
    env.insert_tycon_def(
        "Handle".to_string(),
        Arc::new(TyConDef {
            params: vec!["cap".to_string()],
            body: Type::handle(Type::Unknown),
            constraints: vec![],
            variance: vec![Variance::Covariant],
            constructors: vec![],
            builtin_type: Some("Handle".to_string()),
            annotation: None,
            field_annotations: IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );

    // Two-parameter type constructors (* → * → *)
    // Map: K → V → Map[K, V]
    env.insert_tycon_def(
        "Map".to_string(),
        Arc::new(TyConDef {
            params: vec!["k".to_string(), "v".to_string()],
            body: Type::map(Type::Unknown, Type::Unknown),
            constraints: vec![],
            variance: vec![Variance::Invariant, Variance::Covariant],
            constructors: vec![],
            builtin_type: Some("Map".to_string()),
            annotation: None,
            field_annotations: IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );
    // Fn: the "any callable" type — variadic function with Top return.
    env.insert_tycon_def(
        "Fn".to_string(),
        Arc::new(TyConDef {
            params: vec![],
            body: Type::Function {
                params: vec![],
                ret: Box::new(Type::Any),
                variadic: true,
                required_count: 0,
            },
            constraints: vec![],
            variance: vec![],
            constructors: vec![],
            builtin_type: Some("Fn".to_string()),
            annotation: None,
            field_annotations: IndexMap::new(),
            constructor_constants: indexmap::IndexMap::new(),
        }),
    );

    // Structural record types — Dict and Record both represent "any dict" (empty open record).
    // No builtin_type discriminant: these resolve structurally, not opaquely.
    let empty_open_record = Type::Record(Row {
        fields: indexmap::IndexMap::new(),
        tail: RowTail::Empty,
    });
    for name in ["Dict", "Record"] {
        env.insert_tycon_def(
            name.to_string(),
            Arc::new(TyConDef {
                params: vec![],
                body: empty_open_record.clone(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: None,
                annotation: None,
                field_annotations: IndexMap::new(),
                constructor_constants: IndexMap::new(),
            }),
        );
    }

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
            core_builtin_types(&mut env);
            Some(env)
        }
        "datetime" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_datetime::datetime_type_env(&mut env);
            Some(env)
        }
        "net" => Some(crate::builtins_net::net_type_env()),
        "io" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_io::io_builtin_types(&mut env);
            Some(env)
        }
        "math" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_math::math_builtin_types(&mut env);
            Some(env)
        }
        "meta" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_meta::meta_builtin_types(&mut env);
            Some(env)
        }
        "dict" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_dict::dict_builtin_types(&mut env);
            Some(env)
        }
        "string" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_string::string_builtin_types(&mut env);
            Some(env)
        }
        "seq" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_dict::dict_builtin_types(&mut env);
            Some(env)
        }
        "async" => {
            let mut env = crate::types::TypeEnv::new();
            crate::builtins_core::core_type_env(&mut env);
            crate::builtins_async::async_builtin_types(&mut env);
            Some(env)
        }
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
    use crate::value::{string_val, Environment, Strictness};

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
                let thunk = Arc::new(Thunk::new_materialized(Value::Builtin(def), rust_span!()));
                env.write().unwrap().insert(name, thunk);
            }
        }
        crate::eval::EvalContext::new_empty(base_dir, env, false)
    }

    /// Drive an async builtin to completion in tests.
    async fn run(
        f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>,
    ) -> EvalResult<Arc<Thunk>> {
        f.await
    }

    /// Async materialize wrapper for test code.
    async fn materialize_sync(
        t: &Thunk,
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
    /// Uses the stdlib environment so that builtins are available in the body.
    /// The snippet should be a complete expression (e.g. `"[fn [let] 42]"`).
    async fn parse_eval(llt_src: &str, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let parsed = crate::parser::parse(llt_src)
            .unwrap_or_else(|e| panic!("parse_eval: parse failed for {:?}: {}", llt_src, e));
        let mut program = parsed.program;
        crate::desugar::desugar_surface_program(&mut program);
        let resolve_errors = crate::resolve::resolve_surface_program(&program);
        if !resolve_errors.is_empty() {
            panic!(
                "parse_eval: resolve errors in {:?}: {:?}",
                llt_src, resolve_errors
            );
        }
        let env = Arc::clone(&ctx.config.stdlib_env);
        let thunk = crate::eval::eval_surface_file(&program, env, ctx)
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
                resolution: crate::ast::Resolution::new(), // Not set → unresolvable → CoreExpr::Error
                call_dispatch: crate::ast::CallDispatch::new(),
                annotation: None,
            },
            span: test_span(1, 1, 1, 10),
            type_guard: crate::ast::TypeAnnotation::new(),
            provenance: crate::ast::Provenance::new(),
        });
        Arc::new(Thunk::new_surface(
            node,
            Arc::new(RwLock::new(Environment::new())),
            Arc::clone(ctx),
            test_span(1, 1, 1, 10),
        ))
    }

    /// Build a materialized dict thunk whose entries are allocated into `ctx`'s arena.
    /// Accepts `IndexMap<HashableValue, Arc<Thunk>>` (convenient for test construction) and
    /// stores each as a `ThunkId` in `Value::Dict`, as the runtime requires.
    fn thunk_dict(
        map: IndexMap<HashableValue, Arc<Thunk>>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> Arc<Thunk> {
        let mut id_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(map.len());
        for (k, v) in map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Arc::new(Thunk::new_materialized(
            Value::Dict(id_map),
            test_span(1, 1, 1, 5),
        ))
    }

    /// Helper: flatten a Value (Dict or Overlay) to an `IndexMap<HashableValue, ThunkId>` for test assertions.
    async fn flatten_val(
        val: Value,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> IndexMap<HashableValue, ThunkId> {
        match val {
            Value::Dict(map) => map,
            Value::Overlay(l, r) => flatten_overlay(&l, &r, "test", ctx, test_span(1, 1, 1, 5))
                .await
                .unwrap(),
            other => panic!("expected Dict or Overlay, got {other:?}"),
        }
    }

    /// Helper: materialize the thunk identified by `id` in `ctx`'s arena.
    async fn mat_id(id: ThunkId, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let thunk = ctx.get_thunk(id);
        materialize_sync(&thunk, None, ctx).await.unwrap()
    }

    #[tokio::test]
    async fn floor_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn floor_negative_int_passthrough() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(-7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn floor_zero_int() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn floor_positive_float() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(3.7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn floor_negative_float() {
        // floor(-3.2) = -4, not -3
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(-3.2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-4));
    }

    #[tokio::test]
    async fn floor_float_exact_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn floor_float_just_below_integer() {
        let result = mat(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(2.9999999))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn floor_nan_errors() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn floor_positive_infinity_errors() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(string_val("3.5"))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn floor_bool_type_error() {
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(3.5))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_floor(BuiltinArgs {
            args: vec![thunk(Value::Float(-1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn round_negative_int_passthrough() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(-7))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn round_positive_half_rounds_up() {
        // 0.5 rounds to 1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(0.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(1));
    }

    #[tokio::test]
    async fn round_negative_half_rounds_down() {
        // -0.5 rounds to -1 (half-away-from-zero)
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-0.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-1));
    }

    #[tokio::test]
    async fn round_positive_below_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(2.4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn round_positive_above_half() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(2.6))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn round_negative_below_half() {
        // -2.4 rounds to -2
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-2.4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-2));
    }

    #[tokio::test]
    async fn round_negative_above_half() {
        // -2.6 rounds to -3
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-2.6))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-3));
    }

    #[tokio::test]
    async fn round_1_5_rounds_to_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(1.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn round_negative_1_5_rounds_to_negative_2() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-1.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-2));
    }

    #[tokio::test]
    async fn round_float_exact_integer() {
        let result = mat(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn round_nan_errors() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("NaN"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn round_positive_infinity_errors() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NEG_INFINITY))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(string_val("3.5"))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn round_bool_type_error() {
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::boolean(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_round(BuiltinArgs {
            args: vec![thunk(Value::Float(-1e19))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn to_int_valid_negative() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("-7".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-7));
    }

    #[tokio::test]
    async fn to_int_valid_zero() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("0".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn to_int_valid_large() {
        let result = mat(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("9223372036854775807".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(i64::MAX));
    }

    #[tokio::test]
    async fn to_int_invalid_float_string() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val(" 42 ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn to_int_rejects_bool_input() {
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("1".into())), thunk(string_val("2".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(3.14));
    }

    #[tokio::test]
    async fn to_float_valid_integer_string() {
        // "42" parses as 42.0
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("42".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(42.0));
    }

    #[tokio::test]
    async fn to_float_valid_negative() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("-2.5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(-2.5));
    }

    #[tokio::test]
    async fn to_float_valid_scientific_notation() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("1.5e10".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(1.5e10));
    }

    #[tokio::test]
    async fn to_float_valid_negative_exponent() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("2.5e-3".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(2.5e-3));
    }

    #[tokio::test]
    async fn to_float_valid_zero() {
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("0.0".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(0.0));
    }

    #[tokio::test]
    async fn to_float_valid_leading_dot() {
        // ".5" parses to 0.5
        let result = mat(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val(".5".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Float(0.5));
    }

    #[tokio::test]
    async fn to_float_invalid_text() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("inf".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("-inf".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("infinity".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("NaN".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn to_float_rejects_bool_input() {
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![
                thunk(string_val("1.0".into())),
                thunk(string_val("2.0".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(string_val("1.0".into())));
        let err = run(builtin_to_float(BuiltinArgs {
            args: vec![thunk(string_val("3.14".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("9223372036854775808".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("boom".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "boom");
    }

    #[tokio::test]
    async fn error_custom_message() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("division by zero".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert_eq!(err.kind.to_string(), "division by zero");
    }

    #[tokio::test]
    async fn error_type_mismatch_on_non_string() {
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn try_success_returns_ok_variant() {
        // [fn [let] 42]
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] 42]", &ctx).await;
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx).await;
                assert_eq!(payload_val, Value::Int(42));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_success_with_string_body() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] \"hello\"]", &ctx).await;
        let result = mat(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx).await;
                assert_eq!(payload_val, string_val("hello".into()));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_non_function_type_error() {
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("expected Function"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn try_non_zero_arg_function_error() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let x] $x]", &ctx).await;
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(
            err.kind.to_string().contains("zero-argument function"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn try_arity_check() {
        let err = run(builtin_try(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn try_with_builtin_success() {
        fn ok_builtin(
            _ctx: BuiltinArgs,
        ) -> Pin<Box<dyn std::future::Future<Output = EvalResult<Arc<Thunk>>>>> {
            Box::pin(async move { ok_val(Value::Int(99), rust_span!()) })
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Ok");
                let payload_val = mat_id(payload.expect("Ok should have payload"), &ctx).await;
                assert_eq!(payload_val, Value::Int(99));
            }
            _ => panic!("expected Variant(Result.Ok, ...), got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_with_builtin_failure() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Variant { tag, payload } => {
                assert_eq!(tag, "Result.Error");
                let payload_val =
                    mat_id(payload.expect("Result.Error should have payload"), &ctx).await;
                // builtin-try uses e.to_string() which includes error code and span.
                let s = format!("{payload_val}");
                assert!(s.contains("builtin error"), "error payload should contain 'builtin error', got: {s}");
            }
            _ => panic!("expected Variant(Result.Error, ...), got: {:?}", result),
        }
    }

    #[tokio::test]
    async fn try_resource_limit_exceeded_not_catchable() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        // Should propagate as error, not return err dict
        assert!(
            err.kind.to_string().contains("exceeded resource limit"),
            "expected resource limit error to propagate, got: {}",
            err.kind
        );
        assert_eq!(err.kind.code(), "E043");
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
                let a = materialize(&args[0], None, &ctx).await?; // TEST: test-only inline builtin
                let b = materialize(&args[1], None, &ctx).await?; // TEST: test-only inline builtin
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(func), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(42)), args_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(func), thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &test_ctx())
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
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &test_ctx())
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
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("Int".into()));
    }

    #[tokio::test]
    async fn type_of_float() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("Float".into()));
    }

    #[tokio::test]
    async fn type_of_string() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(string_val("hi".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("String".into()));
    }

    #[tokio::test]
    async fn type_of_dict() {
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("Dict".into()));
    }

    #[tokio::test]
    async fn type_of_function() {
        let ctx = test_ctx();
        let func = parse_eval("[fn [let] 0]", &ctx).await;
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(func)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![thunk(builtin)],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("Function".into()));
    }

    #[tokio::test]
    async fn test_type_of_variant() {
        // Nominal variants return their full qualified tag from $type-of.
        let variant = Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: "Color.Red".to_string(),
                payload: None,
            },
            test_span(1, 1, 1, 5),
        ));
        let result = mat(builtin_type_of(BuiltinArgs {
            args: vec![variant],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("Color.Red".into()));
    }

    #[tokio::test]
    async fn type_of_arity_check() {
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn merge_disjoint_keys() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(HashableValue::Str("a".into()), thunk(Value::Int(1)));
        left.insert(HashableValue::Str("b".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(HashableValue::Str("c".into()), thunk(Value::Int(3)));
        right.insert(HashableValue::Str("d".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        // builtin_merge now returns Value::Overlay; flatten to verify contents.
        let map = flatten_val(result, &ctx).await;
        assert_eq!(map.len(), 4);
        assert!(map.contains_key(&HashableValue::Str("a".into())));
        assert!(map.contains_key(&HashableValue::Str("b".into())));
        assert!(map.contains_key(&HashableValue::Str("c".into())));
        assert!(map.contains_key(&HashableValue::Str("d".into())));
    }

    #[tokio::test]
    async fn merge_overlapping_keys_right_wins() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(HashableValue::Str("x".into()), thunk(Value::Int(1)));
        left.insert(HashableValue::Str("y".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(HashableValue::Str("y".into()), thunk(Value::Int(99)));
        right.insert(HashableValue::Str("z".into()), thunk(Value::Int(3)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let map = flatten_val(result, &ctx).await;
        assert_eq!(map.len(), 3);
        let x = mat_id(map[&HashableValue::Str("x".into())], &ctx).await;
        assert_eq!(x, Value::Int(1));
        let y = mat_id(map[&HashableValue::Str("y".into())], &ctx).await;
        assert_eq!(y, Value::Int(99)); // R overrides L
        let z = mat_id(map[&HashableValue::Str("z".into())], &ctx).await;
        assert_eq!(z, Value::Int(3));
    }

    #[tokio::test]
    async fn merge_empty_dicts() {
        let ctx = test_ctx();
        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![
                thunk_dict(IndexMap::new(), &ctx),
                thunk_dict(IndexMap::new(), &ctx),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let map = flatten_val(result, &ctx).await;
        assert_eq!(map.len(), 0);
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
    async fn merge_left_empty() {
        let ctx = test_ctx();
        let mut right = IndexMap::new();
        right.insert(HashableValue::Int(0), thunk(string_val("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(IndexMap::new(), &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let map = flatten_val(result, &ctx).await;
        assert_eq!(map.len(), 1);
        let v = mat_id(map[&HashableValue::Int(0)], &ctx).await;
        assert_eq!(v, string_val("only".into()));
    }

    #[tokio::test]
    async fn merge_right_empty() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(HashableValue::Int(0), thunk(string_val("only".into())));
        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(IndexMap::new(), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let map = flatten_val(result, &ctx).await;
        assert_eq!(map.len(), 1);
        let v = mat_id(map[&HashableValue::Int(0)], &ctx).await;
        assert_eq!(v, string_val("only".into()));
    }

    #[tokio::test]
    async fn merge_preserves_thunks() {
        // With arena-based ThunkIds, verify that the values are preserved correctly
        // through a lazy overlay by materializing and comparing values.
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);
        let left_thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span.clone()));
        let right_thunk = Arc::new(Thunk::new_materialized(Value::Int(99), span));

        let mut left = IndexMap::new();
        left.insert(HashableValue::Str("a".into()), Arc::clone(&left_thunk));
        let mut right = IndexMap::new();
        right.insert(HashableValue::Str("b".into()), Arc::clone(&right_thunk));

        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        // Flatten and verify values are preserved correctly through the overlay.
        let map = flatten_val(result, &ctx).await;
        assert_eq!(
            mat_id(map[&HashableValue::Str("a".into())], &ctx).await,
            Value::Int(42)
        );
        assert_eq!(
            mat_id(map[&HashableValue::Str("b".into())], &ctx).await,
            Value::Int(99)
        );
    }

    #[tokio::test]
    async fn merge_preserves_left_order() {
        let ctx = test_ctx();
        let mut left = IndexMap::new();
        left.insert(HashableValue::Str("b".into()), thunk(Value::Int(1)));
        left.insert(HashableValue::Str("a".into()), thunk(Value::Int(2)));
        let mut right = IndexMap::new();
        right.insert(HashableValue::Str("d".into()), thunk(Value::Int(3)));
        right.insert(HashableValue::Str("c".into()), thunk(Value::Int(4)));

        let result = mat(builtin_merge(BuiltinArgs {
            args: vec![thunk_dict(left, &ctx), thunk_dict(right, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let map = flatten_val(result, &ctx).await;
        let keys: Vec<&HashableValue> = map.keys().collect();
        assert_eq!(
            keys,
            vec![
                &HashableValue::Str("b".into()),
                &HashableValue::Str("a".into()),
                &HashableValue::Str("d".into()),
                &HashableValue::Str("c".into()),
            ]
        );
    }

    #[tokio::test]
    async fn keys_wrong_arity_zero() {
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_length(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn merge_wrong_arity_one() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_merge(BuiltinArgs {
            args: vec![d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn merge_wrong_arity_three() {
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let err = run(builtin_merge(BuiltinArgs {
            args: vec![d.clone(), d.clone(), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_keys(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(5));
    }

    #[tokio::test]
    async fn length_string_empty() {
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(0));
    }

    #[tokio::test]
    async fn length_string_unicode() {
        // Multi-byte characters: length returns char count, not byte count
        let result = mat(builtin_length(BuiltinArgs {
            args: vec![thunk(string_val("\u{1F600}\u{1F601}".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(2));
    }

    #[tokio::test]
    async fn merge_first_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let result = run(builtin_merge(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), d],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        // builtin_merge itself succeeds — returns Overlay(Int(1), {})
        let overlay_thunk = result.unwrap();
        let overlay_val = materialize_sync(&overlay_thunk, None, &ctx).await.unwrap();
        // Flatten fires the type error: left side is Int, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span())
                    .await
                    .unwrap_err();
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

    #[tokio::test]
    async fn merge_second_arg_non_dict() {
        // With lazy overlay, builtin_merge succeeds (O(1) — no type check at call time).
        // The type error fires when the overlay is flattened (at access time).
        let ctx = test_ctx();
        let d = thunk_dict(IndexMap::new(), &ctx);
        let result = run(builtin_merge(BuiltinArgs {
            args: vec![d, thunk(string_val("nope".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let overlay_thunk = result.unwrap();
        let overlay_val = materialize_sync(&overlay_thunk, None, &ctx).await.unwrap();
        // Flatten fires the type error: right side is String, not Dict
        match overlay_val {
            Value::Overlay(l, r) => {
                let err = flatten_overlay(&l, &r, "merge", &ctx, call_span())
                    .await
                    .unwrap_err();
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

    #[tokio::test]
    async fn append_to_empty_dict() {
        let ctx = test_ctx();
        let empty = thunk_dict(IndexMap::new(), &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), empty],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                let val = mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await;
                assert_eq!(val, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_to_existing_list() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("a".into())));
        map.insert(HashableValue::Int(1), thunk(string_val("b".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: vec![thunk(string_val("c".into())), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = mat_id(*map.get(&HashableValue::Int(2)).unwrap(), &ctx).await;
                assert_eq!(val, string_val("c".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_to_dict_with_string_keys_only() {
        // Dict with only string keys -- next int key should be 0
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("x".into()), thunk(Value::Int(1)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(99)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let val = mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await;
                assert_eq!(val, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_to_dict_with_gap_in_int_keys() {
        // Dict with keys 0, 5 -- next key should be 6 (max + 1)
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(Value::Int(10)));
        map.insert(HashableValue::Int(5), thunk(Value::Int(50)));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(60)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                let val = mat_id(*map.get(&HashableValue::Int(6)).unwrap(), &ctx).await;
                assert_eq!(val, Value::Int(60));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_preserves_existing_entries() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(0), thunk(string_val("first".into())));
        let dict = thunk_dict(map, &ctx);
        let result = mat(builtin_append(BuiltinArgs {
            args: vec![thunk(string_val("second".into())), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let first = mat_id(*map.get(&HashableValue::Int(0)).unwrap(), &ctx).await;
                assert_eq!(first, string_val("first".into()));
                let second = mat_id(*map.get(&HashableValue::Int(1)).unwrap(), &ctx).await;
                assert_eq!(second, string_val("second".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_value_stays_as_thunk() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(map) => {
                // Verify the value was inserted correctly and materializes to the expected value.
                let id = *map.get(&HashableValue::Int(0)).unwrap();
                assert_eq!(mat_id(id, &ctx).await, Value::Int(7));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn append_wrong_arity_zero() {
        let err = run(builtin_append(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("2"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn append_wrong_arity_three() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("2"), "got: {}", err.kind);
    }

    #[tokio::test]
    async fn append_second_arg_non_dict() {
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(err.kind.to_string().contains("append"), "got: {}", err.kind);
        assert!(
            err.kind.to_string().contains("expected Dict"),
            "got: {}",
            err.kind
        );
    }

    #[tokio::test]
    async fn append_key_overflow_at_i64_max() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Int(i64::MAX), thunk(Value::Int(1)));
        let dict = thunk_dict(map, &ctx);
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), dict],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn replace_basic() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("world".into())),
                thunk(string_val("Rust".into())),
                thunk(string_val("hello world".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello Rust".into()));
    }

    #[tokio::test]
    async fn replace_multiple_occurrences() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(string_val("o".into())),
                thunk(string_val("banana".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("bonono".into()));
    }

    #[tokio::test]
    async fn replace_no_match() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("xyz".into())),
                thunk(string_val("abc".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn replace_empty_pattern() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("".into())),
                thunk(string_val("-".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("-a-b-c-".into()));
    }

    #[tokio::test]
    async fn replace_to_empty() {
        let result = mat(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("l".into())),
                thunk(string_val("".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("heo".into()));
    }

    #[tokio::test]
    async fn replace_output_size_limit_empty_pattern() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("replace: output would exceed"));
    }

    #[tokio::test]
    async fn replace_output_size_ok_normal_pattern() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        // 1000 'a' replaced with 'bb' -> 2000 'b'
        assert_eq!(result, string_val(&"b".repeat(2000)));
    }

    #[tokio::test]
    async fn trim_basic() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("  hello  ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_leading_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("   hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_trailing_only() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("hello   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_no_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_all_whitespace() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("   ".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("".into()));
    }

    #[tokio::test]
    async fn trim_tabs_and_newlines() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("\t\nhello\n\t".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("hello".into()));
    }

    #[tokio::test]
    async fn trim_empty() {
        let result = mat(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, string_val("".into()));
    }

    #[tokio::test]
    async fn replace_wrong_arity() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("a".into())), thunk(string_val("b".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(Value::Int(1)),
                thunk(string_val("b".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(Value::boolean(true)),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
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

    #[tokio::test]
    async fn replace_wrong_type_input() {
        let err = run(builtin_replace(BuiltinArgs {
            args: vec![
                thunk(string_val("a".into())),
                thunk(string_val("b".into())),
                thunk(Value::Float(3.14)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(string_val("hi".into())));
        let err = run(builtin_trim(BuiltinArgs {
            args: vec![thunk(string_val("  hello  ".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_raise(BuiltinArgs {
            args: vec![thunk(string_val("boom".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_type_of(BuiltinArgs {
            args: vec![thunk(Value::Int(42))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("x".into(), thunk(Value::Int(1)));
        let err = run(builtin_to_int(BuiltinArgs {
            args: vec![thunk(string_val("42".into()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(99)));
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(1))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(2)), thunk(Value::Int(3))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn if_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_if(BuiltinArgs {
            args: vec![
                thunk(Value::boolean(true)),
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
            ],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        named.insert("extra".into(), thunk(Value::Int(1)));
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let map = IndexMap::new();
        let err = run(builtin_length(BuiltinArgs {
            args: vec![thunk(Value::Dict(map))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn merge_rejects_named_args() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn append_rejects_named_args() {
        let mut named = IndexMap::new();
        named.insert("extra".into(), thunk(Value::Int(1)));
        let err = run(builtin_append(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new())), thunk(Value::Int(42))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = parse_eval("[fn [let] 42]", &ctx).await;
        let err = run(builtin_try(BuiltinArgs {
            args: vec![thunk(func)],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        named.insert("extra".into(), thunk(Value::Int(1)));
        let func = parse_eval("[fn [let] 42]", &ctx).await;
        let apply_result = run(builtin_apply(BuiltinArgs {
            args: vec![thunk(func), thunk(Value::Dict(IndexMap::new()))],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .expect("should return thunk");
        let err = materialize_sync(&apply_result, None, &test_ctx())
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
            count > 100,
            "expected core builtins to have >100 entries, got {count}"
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
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(8));
    }

    #[tokio::test]
    async fn add_int_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(5.5));
    }

    #[tokio::test]
    async fn add_float_int() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(5.5));
    }

    #[tokio::test]
    async fn add_float_float() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(1.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(4.0));
    }

    #[tokio::test]
    async fn add_negative_ints() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(-10)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(-7));
    }

    #[tokio::test]
    async fn add_zeros() {
        let r = mat(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(0)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn add_type_error_string() {
        let e = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        // Non-Int/Float operands produce a NoInstance error for the Addable class.
        assert!(
            e.kind.to_string().contains("no instance") || e.kind.to_string().contains("Addable"),
            "expected NoInstance error for Int + String, got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn add_arity_one_arg() {
        let e = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn add_arity_three_args() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(3));
    }

    #[tokio::test]
    async fn add_overflow_error() {
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MAX)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MIN)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(7));
    }

    #[tokio::test]
    async fn sub_int_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(6.5));
    }

    #[tokio::test]
    async fn sub_float_int() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Float(10.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(7.5));
    }

    #[tokio::test]
    async fn sub_float_float() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Float(10.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(7.0));
    }

    #[tokio::test]
    async fn sub_result_negative() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(10))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(-7));
    }

    #[tokio::test]
    async fn sub_to_zero() {
        let r = mat(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn sub_arity_zero_args() {
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn sub_arity_three_args() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(-1));
    }

    #[tokio::test]
    async fn sub_type_error_string() {
        let e = run(builtin_sub(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        // Non-Int/Float operands produce a NoInstance error for the Subtractable class.
        assert!(
            e.kind.to_string().contains("no instance")
                || e.kind.to_string().contains("Subtractable"),
            "expected NoInstance error for Int - String, got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn mul_int_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(4)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(20));
    }

    #[tokio::test]
    async fn mul_int_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(4)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(10.0));
    }

    #[tokio::test]
    async fn mul_float_int() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(10.0));
    }

    #[tokio::test]
    async fn mul_float_float() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Float(3.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(7.5));
    }

    #[tokio::test]
    async fn mul_by_zero() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn mul_negative() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(-3)), thunk(Value::Int(4))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(-12));
    }

    #[tokio::test]
    async fn mul_by_negative_one() {
        let r = mat(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(42)), thunk(Value::Int(-1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(-42));
    }

    #[tokio::test]
    async fn mul_overflow_error() {
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Int(i64::MAX)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_add(BuiltinArgs {
            args: vec![thunk(Value::Float(1e308)), thunk(Value::Float(1e308))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_sub(BuiltinArgs {
            args: vec![
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_mul(BuiltinArgs {
            args: vec![thunk(Value::Float(1e308)), thunk(Value::Float(10.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_div_float(BuiltinArgs {
            args: vec![
                thunk(Value::Float(f64::INFINITY)),
                thunk(Value::Float(f64::INFINITY)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match r {
            Value::Float(f) => assert_eq!(f, 5.0),
            other => panic!("expected Float(5.0), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn div_float_int_float() {
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(3.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Float(7.5)), thunk(Value::Float(2.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(3.0));
    }

    #[tokio::test]
    async fn div_float_by_zero_int() {
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
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

    #[tokio::test]
    async fn div_float_by_zero_float() {
        // Float / Float(0.0) produces Inf which check_float_result rejects as non-finite.
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Float(10.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let e = run(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Int(10)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_div_float(BuiltinArgs {
            args: vec![thunk(Value::Float(-0.0)), thunk(Value::Float(1.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Float(0.0));
    }

    #[tokio::test]
    async fn eq_int_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_int_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(6))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_float_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14)), thunk(Value::Float(3.14))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_float_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(3.14)), thunk(Value::Float(2.71))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_string_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![
                thunk(string_val("hello".into())),
                thunk(string_val("hello".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_string_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![
                thunk(string_val("hello".into())),
                thunk(string_val("world".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_bool_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::boolean(true)), thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_bool_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::boolean(true)), thunk(Value::boolean(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_cross_type_int_float_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_cross_type_float_int_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(5.0)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_cross_type_int_float_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_dict_structural_equality() {
        // Empty dicts are structurally equal
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_different_types_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("1".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_bool_vs_int_not_equal() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::boolean(true)), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_nan_not_equal_to_self() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN)), thunk(Value::Float(f64::NAN))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn eq_negative_zero_float() {
        let r = mat(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Float(-0.0)), thunk(Value::Float(0.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn eq_arity_error() {
        let e = run(builtin_eq(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_int_int_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_int_int_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_float_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_string_lexicographic() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(string_val("apple".into())),
                thunk(string_val("banana".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_string_lexicographic_reverse() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(string_val("banana".into())),
                thunk(string_val("apple".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_string_equal_is_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(string_val("same".into())),
                thunk(string_val("same".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_string_prefix() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(string_val("ab".into())),
                thunk(string_val("abc".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_int_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(3)), thunk(Value::Float(3.5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_float_int() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(2.5)), thunk(Value::Int(3))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_cross_type_equal_values() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(5)), thunk(Value::Float(5.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_incompatible_types_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1)), thunk(string_val("hello".into()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[tokio::test]
    async fn lt_bool_false_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::boolean(false)), thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_bool_true_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::boolean(true)), thunk(Value::boolean(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_bool_false_lt_false() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::boolean(false)), thunk(Value::boolean(false))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_bool_true_lt_true() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::boolean(true)), thunk(Value::boolean(true))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn lt_dict_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![
                thunk(Value::Dict(IndexMap::new())),
                thunk(Value::Dict(IndexMap::new())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(e.kind.to_string().contains("expected"), "got: {}", e.kind);
    }

    #[tokio::test]
    async fn lt_arity_error() {
        let e = run(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Int(-10)), thunk(Value::Int(-5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(1));
    }

    #[tokio::test]
    async fn lt_nan_float() {
        let r = mat(builtin_lt(BuiltinArgs {
            args: vec![thunk(Value::Float(f64::NAN)), thunk(Value::Float(1.0))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(r, Value::Int(0));
    }

    #[tokio::test]
    async fn if_true_returns_then_branch() {
        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn if_false_returns_else_branch() {
        let args = vec![
            thunk(Value::Int(0)),
            thunk(Value::Int(42)),
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(99));
    }

    #[tokio::test]
    async fn if_does_not_materialize_unchosen_else_branch() {
        let ctx = test_ctx();
        let error_thunk = make_undef_thunk(&ctx);

        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(42)),
            error_thunk,
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(42));
    }

    #[tokio::test]
    async fn if_does_not_materialize_unchosen_then_branch() {
        let ctx = test_ctx();
        let error_thunk = make_undef_thunk(&ctx);

        let args = vec![
            thunk(Value::Int(0)),
            error_thunk,
            thunk(Value::Int(99)),
        ];
        let result = mat(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(99));
    }

    #[tokio::test]
    async fn if_non_bool_condition_error() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
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

    #[tokio::test]
    async fn if_string_condition_error() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();
        assert!(
            e.kind.to_string().contains("expected Bool"),
            "got: {}",
            e.kind
        );
    }

    #[tokio::test]
    async fn if_arity_too_few() {
        let args = vec![thunk(Value::Int(1)), thunk(Value::Int(42))];
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn if_arity_too_many() {
        let args = vec![
            thunk(Value::Int(1)),
            thunk(Value::Int(1)),
            thunk(Value::Int(2)),
            thunk(Value::Int(3)),
        ];
        let e = run(builtin_if(BuiltinArgs {
            args,
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
    async fn if_non_bool_condition_has_secondary_span() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
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

    #[tokio::test]
    async fn if_non_bool_secondary_span_suppressed_when_same() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await
        .unwrap_err();

        // Secondary span should NOT be set because it equals call_span
        assert!(
            err.secondary_span.is_none(),
            "Secondary span should be suppressed when same as call span"
        );
    }

    /// Parse-only smoke test for the prelude. Evaluating the full prelude requires a
    #[tokio::test]
    async fn build_core_env_has_builtins() {
        let env = build_core_env();
        let env_ref = env.read().unwrap();
        // Should have core builtins
        assert!(
            env_ref.get_by_name("builtin-if").is_some(),
            "missing builtin builtin-if"
        );
        // Prelude functions are NOT in core_env — they are loaded via run_loader_pipeline.
        assert!(
            env_ref.get_by_name("map").is_none(),
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
            args: vec![thunk(Value::Int(2)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(0)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(-5)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(10)), dict_val],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let result = run(builtin_take(BuiltinArgs {
            args: vec![thunk(string_val("not int".into())), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn take_xs_non_dict_or_seq() {
        let result = run(builtin_take(BuiltinArgs {
            args: vec![
                thunk(Value::Int(5)),
                thunk(string_val("not dict or seq".into())),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn take_arity_one() {
        let result = run(builtin_take(BuiltinArgs {
            args: vec![thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn concat_dict_empty_xs_returns_ys() {
        // concat({}, ys) returns ys unchanged (empty xs short-circuit)
        let ctx = test_ctx();
        let xs = thunk(Value::Dict(IndexMap::new()));
        let mut ys_map = IndexMap::new();
        ys_map.insert(HashableValue::Int(0), thunk(Value::Int(1)));
        let ys = thunk_dict(ys_map, &ctx);

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let ys = thunk(Value::Dict(IndexMap::new()));

        let result = mat(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let xs = Arc::new(Thunk::new_materialized(
            Value::Variant {
                tag: "Color.Red".to_string(),
                payload: None,
            },
            test_span(1, 1, 1, 5),
        ));
        let ys = thunk(Value::Dict(IndexMap::new()));

        let err = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let xs = thunk(Value::Dict(IndexMap::new())); // empty dict
        let mut ys_map = IndexMap::new();
        ys_map.insert(HashableValue::Int(0), thunk(Value::Int(99)));
        let ys = thunk_dict(ys_map, &ctx);

        // Should succeed and return ys (the same thunk or an equivalent materialized form)
        let result = run(builtin_concat(BuiltinArgs {
            args: vec![xs, ys],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let handler = thunk(Value::Int(42));
        let result = run(builtin_proxy(BuiltinArgs {
            args: vec![handler.clone()],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(1)), thunk(Value::Int(2))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
                thunk(Value::Int(1)),
                thunk(Value::Int(2)),
                thunk(Value::Int(3)),
            ],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
        let mut named = IndexMap::new();
        named.insert("handler".to_string(), thunk(Value::Int(42)));

        let err = run(builtin_proxy(BuiltinArgs {
            args: vec![],
            named: Some(named),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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

        // Core builtins that must exist (from builtin_module("core"))
        let required_names: &[&str] = &[
            "builtin-if",
            "builtin-raise",
            "builtin-type-of",
            "builtin-keys",
            "builtin-get",
            "builtin-merge",
            "builtin-add",
            "builtin-sub",
            "builtin-mul",
            "builtin-div",
        ];

        for name in required_names {
            assert!(
                env.get_by_name(*name).is_some(),
                "core env is missing expected builtin: {name}"
            );
        }

        // Prelude functions must NOT be in core env (they come from run_loader_pipeline).
        assert!(
            env.get_by_name("map").is_none(),
            "map should not be in core env"
        );
        assert!(
            env.get_by_name("filter").is_none(),
            "filter should not be in core env"
        );
    }

    // -------------------------------------------------------------------------
    // Unit tests: builtin_rest, builtin_reverse, builtin_sort
    // -------------------------------------------------------------------------

    fn make_int_dict(vals: &[i64], ctx: &Arc<crate::eval::EvalContext>) -> Value {
        let mut rc_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, &v) in vals.iter().enumerate() {
            rc_map.insert(HashableValue::Int(i as i64), thunk(Value::Int(v)));
        }
        let mut id_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(rc_map.len());
        for (k, v) in rc_map {
            id_map.insert(k, ctx.alloc_thunk(v));
        }
        Value::Dict(id_map)
    }

    async fn extract_int_at(
        map: &IndexMap<HashableValue, ThunkId>,
        idx: i64,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> i64 {
        let thunk = ctx.get_thunk(*map.get(&HashableValue::Int(idx)).unwrap());
        match materialize_sync(&thunk, None, ctx).await.unwrap() {
            Value::Int(n) => n,
            other => panic!("expected Int at index {idx}, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rest_three_elements_drops_first() {
        let ctx = test_ctx();
        let result = mat(builtin_rest(BuiltinArgs {
            args: vec![thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 2);
        assert_eq!(extract_int_at(&m, 0, &ctx).await, 20);
        assert_eq!(extract_int_at(&m, 1, &ctx).await, 30);
    }

    #[tokio::test]
    async fn rest_single_element_returns_empty() {
        let ctx = test_ctx();
        let result = mat(builtin_rest(BuiltinArgs {
            args: vec![thunk(make_int_dict(&[42], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn rest_empty_dict_returns_empty() {
        let result = mat(builtin_rest(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn reverse_three_elements() {
        let ctx = test_ctx();
        let result = mat(builtin_reverse(BuiltinArgs {
            args: vec![thunk(make_int_dict(&[10, 20, 30], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 3);
        assert_eq!(extract_int_at(&m, 0, &ctx).await, 30);
        assert_eq!(extract_int_at(&m, 1, &ctx).await, 20);
        assert_eq!(extract_int_at(&m, 2, &ctx).await, 10);
    }

    #[tokio::test]
    async fn reverse_empty_dict() {
        let result = mat(builtin_reverse(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn sort_integers_ascending() {
        let ctx = test_ctx();
        let result = mat(builtin_sort(BuiltinArgs {
            args: vec![thunk(make_int_dict(&[3, 1, 4, 1, 5], &ctx))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        assert_eq!(m.len(), 5);
        let expected = [1i64, 1, 3, 4, 5];
        for (i, &exp) in expected.iter().enumerate() {
            assert_eq!(
                extract_int_at(&m, i as i64, &ctx).await,
                exp,
                "at index {i}"
            );
        }
    }

    #[tokio::test]
    async fn sort_strings_lexicographic() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        for (i, s) in ["banana", "apple", "cherry"].iter().enumerate() {
            map.insert(HashableValue::Int(i as i64), thunk(string_val(s)));
        }
        let result = mat(builtin_sort(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let m = match result {
            Value::Dict(m) => m,
            other => panic!("expected Dict, got {:?}", other),
        };
        let v0 = mat_id(*m.get(&HashableValue::Int(0)).unwrap(), &ctx).await;
        assert_eq!(v0, string_val("apple".into()));
    }

    #[tokio::test]
    async fn sort_empty_dict() {
        let result = mat(builtin_sort(BuiltinArgs {
            args: vec![thunk(Value::Dict(IndexMap::new()))],
            named: no_named(),
            call_span: call_span(),
            ctx: test_ctx(),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        match result {
            Value::Dict(m) => assert!(m.is_empty()),
            other => panic!("expected Dict, got {:?}", other),
        }
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
            args: vec![thunk_dict(map, &ctx), thunk(Value::Int(1))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert_eq!(result, Value::Int(20));
    }

    #[tokio::test]
    async fn dict_nth_out_of_bounds() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("a".into()), thunk(Value::Int(10)));
        let result = mat(builtin_dict_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), thunk(Value::Int(5))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        assert!(
            matches!(result, Value::Variant { ref tag, payload: None } if tag == "Absent.Absent")
        );
    }

    #[tokio::test]
    async fn dict_key_nth_string_key() {
        let ctx = test_ctx();
        let mut map = IndexMap::new();
        map.insert(HashableValue::Str("foo".into()), thunk(Value::Int(1)));
        map.insert(HashableValue::Str("bar".into()), thunk(Value::Int(2)));
        let result = mat(builtin_dict_key_nth(BuiltinArgs {
            args: vec![thunk_dict(map, &ctx), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk_dict(map, &ctx), thunk(Value::Int(0))],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(Value::Int(1)), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            args: vec![thunk(string_val("z".into())), thunk_dict(map, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
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

    /// Body test: `builtin_append` (force_count=0) must NOT force the appended VALUE.
    ///
    /// After T-776, `$append` takes `(value, dict)` — args[0]=value, args[1]=dict.
    /// The VALUE being appended (args[0]) must stay as an unevaluated thunk — only
    /// the dict structure (args[1]) needs to be materialized to determine the next key.
    ///
    /// If `builtin_append` were to force args[0], the undef thunk would produce an
    /// "undefined variable" error. Passing this test proves args[0] is never forced
    /// by the builtin body (body test — not testing the pos_strictness dispatch mechanism).
    #[tokio::test]
    async fn builtin_append_does_not_force_appended_value() {
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
            caller_env: Arc::new(RwLock::new(Environment::new())),
        }))
        .await;
        let result_thunk = result.unwrap_or_else(|e| {
            panic!(
                "builtin_append must not force the appended value; got error: {:?}",
                e
            )
        });
        // The result should be a dict with exactly one entry at key 0.
        let val = mat_val(result_thunk).await;
        match val {
            Value::Dict(ref m) => {
                assert_eq!(m.len(), 1, "expected 1 entry after append, got {}", m.len());
                assert!(
                    m.contains_key(&HashableValue::Int(0)),
                    "expected integer key 0, got {:?}",
                    m.keys().collect::<Vec<_>>()
                );
            }
            other => panic!("expected Dict from builtin_append, got {:?}", other),
        }
    }
}
