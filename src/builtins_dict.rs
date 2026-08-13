//! Dict/access builtins: keys, length, append, get, each, each-key, each-kv.
//!
//! These builtins operate on dictionary (Dict) values, providing primitive
//! operations for accessing and transforming key-value structures.
//!
//! **Dict primitives:**
//! - `keys`: Extract dict keys as an auto-indexed dict
//! - `length`: Count dict entries
//! - `append`: Insert value at next integer key
//! - `builtin-dict-get`: Primitive dict key lookup (errors on missing key)
//! - `builtin-has-key?`: Returns Int 1 if key exists, Int 0 if not (O(1), no value force)
//! - `builtin-dict-merge`: Right-biased dict union (used by spread-dict desugaring)
//!
//! **Dict single-step primitives (drive laziness from tinct side):**
//! - `builtin-dict-has-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-nth`: Get value at insertion-order position i (errors on out of bounds)
//! - `builtin-dict-has-key-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-key-nth`: Get key at insertion-order position i (errors on out of bounds)
//! - `builtin-dict-has-kv-nth?`: Returns Int 1 if position i is valid, Int 0 if out of bounds
//! - `builtin-dict-kv-nth`: Get {key,value} pair at position i (errors on out of bounds)
//!
//! **Transient builders:**
//! - `make-builder`: Create an empty mutable builder
//! - `builder-set`: Set key-value pair, returns builder for chaining
//! - `builder-delete`: Remove key, returns builder for chaining
//! - `builder-finish`: Take inner dict, freeze builder
//! - `builder-snapshot`: Clone inner dict without freezing
//! - `builder-has?`: Check if key exists
//! - `builder-get`: Get value by key
//!
//! All three single-step primitives are O(1) (IndexMap::get_index).
//! Tinct wrappers in prelude.llt drive laziness via recursive thunks.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::builtins::{ok_val, reject_named};
use crate::error::Diagnostic;
use crate::eval::materialize;
use crate::rust_span;
use crate::value::{string_val, BuiltinArgs, HashableValue, Thunk, Value, TYPE_ID_DICT};

const KIND: &str = "eval";

/// Convert a runtime `Value` to a `HashableValue` dict key.
/// Returns an error if the value is not hashable (Function, Handle, Seq, etc.).
fn value_to_hashable_key(
    val: &Value,
    builtin_name: &str,
    span: crate::ast::Span,
    ctx: &Arc<crate::eval::EvalContext>,
) -> crate::error::DiagnosticResult<HashableValue> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
    match val {
        Value::Int { n, .. } => crate::error::DiagnosticResult::ok(HashableValue::Int(*n)).with_diagnostics(_dr_diags),
        Value::Float { n: f, .. } => crate::error::DiagnosticResult::ok(HashableValue::Float(f.to_bits())).with_diagnostics(_dr_diags),
        Value::String {
            source, start, end, ..
        } => {
            let s = &source[*start..*end];
            crate::error::DiagnosticResult::ok(HashableValue::Str(s.into())).with_diagnostics(_dr_diags)
        }
        Value::Variant {
            type_decl_id,
            ctor_index,
            payload,
            ..
        } => {
            // General variant key — use numeric (type_decl_id, ctor_index) identity
            let hv_payload = match payload {
                None => None,
                Some(p) => {
                    let p_val = match p.peek_result() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()).with_diagnostics(_dr_diags),
                        None => {
                            return crate::error::DiagnosticResult::err(Diagnostic::error(
                                KIND,
                                format!("{builtin_name}: variant payload not materialized"),
                                span.clone(),
                            )).with_diagnostics(_dr_diags)
                        }
                    };
                    Some(Box::new({
                        let (result, diags) = value_to_hashable_key(
                            p_val,
                            builtin_name,
                            span.clone(),
                            ctx,
                        ).into_parts();
                        _dr_diags.extend(diags);
                        match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        }
                    }))
                }
            };
            crate::error::DiagnosticResult::ok(HashableValue::Variant {
                type_decl_id: *type_decl_id,
                ctor_index: *ctor_index,
                payload: hv_payload,
            }).with_diagnostics(_dr_diags)
        }
        other => crate::error::DiagnosticResult::err(Diagnostic::error(
            KIND,
            format!(
                "{}: expected {}, got {}",
                builtin_name,
                "Int, Float, String, or Variant",
                ctx.type_display_name(other.type_decl_id_of())
            ),
            span,
        )).with_diagnostics(_dr_diags),
    }
}

/// Convert a `HashableValue` key back to a runtime `Value`.
fn hashable_value_to_value(hv: &HashableValue) -> Value {
    match hv {
        HashableValue::Int(n) => Value::Int {
            n: *n,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_INT,
            )),
        },
        HashableValue::Float(bits) => Value::Float {
            n: f64::from_bits(*bits),
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_FLOAT,
            )),
        },
        HashableValue::Str(s) => string_val(s),
        HashableValue::Dict(pairs) => {
            let mut map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(pairs.len());
            for (k, v) in pairs {
                let v_val = hashable_value_to_value(v);
                map.insert(k.clone(), Arc::new(Thunk::value(v_val, rust_span!())));
            }
            Value::Dict {
                entries: map,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }
        }
        HashableValue::Variant {
            type_decl_id,
            ctor_index,
            payload,
        } => {
            let payload_thunk = payload.as_ref().map(|p| {
                let val = hashable_value_to_value(p);
                Arc::new(Thunk::value(val, rust_span!()))
            });
            Value::Variant {
                type_decl_id: *type_decl_id,
                ctor_index: *ctor_index,
                payload: payload_thunk,
                type_val: Some(crate::type_infer::make_typevalue_nominal(*type_decl_id)),
            }
        }
    }
}

/// `keys`: Takes 1 arg (a Dict). Returns a Dict with integer keys `0..n`
/// mapping to the key values (Int keys become Int values, String keys become
/// String values). Insertion order is preserved.
/// Inherently materializing: must access IndexMap to enumerate keys.
pub(crate) fn builtin_keys(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("keys", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }
        // arg[0] is pre-forced by force_count.
        let thunk0 = args[0].clone();
        let val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "keys",
                val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let origin = call_span.clone();
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(map.len());
        for (i, (key, _)) in map.iter().enumerate() {
            let key_value = hashable_value_to_value(key);
            let thunk = Arc::new(Thunk::value(key_value, origin.clone()));
            let idx_key = match i64::try_from(i) {
                Ok(v) => v,
                Err(_) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    "collection index overflow".to_string(),
                    call_span.clone(),
                )).with_diagnostics(_dr_diags),
            };
            result.insert(
                HashableValue::Int(idx_key),
                thunk,
            );
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `length`: Takes 1 arg (a Dict, String, or Bytes). Returns an Int with the number of entries/characters/bytes.
/// Dual-dispatch: Dict returns entry count, String returns character count, Bytes returns byte count.
/// Inherently materializing: must access IndexMap to count entries or count UTF-8 characters or bytes.
pub(crate) fn builtin_length(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("length", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }
        // arg[0] is pre-forced by force_count.
        let thunk0 = args[0].clone();
        let val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match val {
            Value::String {
                source, start, end, ..
            } => {
                let s = &source[start..end];
                let len = s.chars().count();
                let len_i64 = match i64::try_from(len) {
                    Ok(v) => v,
                    Err(_) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        "length: string length exceeds i64::MAX".to_string(),
                        call_span.clone(),
                    )).with_diagnostics(_dr_diags),
                };
                ok_val(
                    Value::Int {
                        n: len_i64,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            Value::Bytes { start, end, .. } => {
                let len = end - start;
                let len_i64 = match i64::try_from(len) {
                    Ok(v) => v,
                    Err(_) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        "length: byte length exceeds i64::MAX".to_string(),
                        call_span.clone(),
                    )).with_diagnostics(_dr_diags),
                };
                ok_val(
                    Value::Int {
                        n: len_i64,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            _ => {
                let map = {
                    let (result, diags) = crate::builtins::require_dict(
                        "length",
                        val.clone(),
                        thunk0.span.clone(),
                        &ctx,
                        call_span.clone(),
                    )
                    .await.into_parts();
                    _dr_diags.extend(diags);
                    match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    }
                };
                ok_val(
                    Value::Int {
                        n: map.len() as i64,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
        }
    })
}

/// `builtin-dict-get`: Rust primitive for keyed access on Dict, Variant, and Proxy values.
///
/// Takes 2 args: a key (Int or String) and a target value.
/// Returns the value at that key, or errors if the key is not found.
///
/// Handles:
/// - Dict: O(1) key lookup
/// - Variant (with payload): auto-unpacks payload dict and looks up key; falls back to
///   TyConDef constructor constants (T-1358) if not found in payload
/// Dict-only key lookup. Type error on non-Dict values.
/// Used by `[get key dict]` via the Indexable typeclass in prelude.
pub(crate) fn builtin_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-dict-get", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = {
            let (result, diags) = value_to_hashable_key(&key_val, "builtin-dict-get", thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let thunk1 = args[1].clone();
        let target_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let target_span = thunk1.span.clone();

        match target_val {
            Value::Dict { entries: map, .. } => {
                let key_str = key.to_string();
                match map.get(&key) {
                    Some(thunk) => crate::error::DiagnosticResult::ok(Arc::clone(thunk)).with_diagnostics(_dr_diags),
                    None => {
                        let mut err = Diagnostic::error(
                            KIND,
                            format!("key not found: {}", key_str),
                            target_span,
                        );
                        err.labels.push(crate::error::SpanLabel {
                            span: call_span,
                            message: "accessed here".to_string(),
                        });
                        err.notes.push(format!("dict has {} entries", map.len(),));
                        crate::error::DiagnosticResult::err(err).with_diagnostics(_dr_diags)
                    }
                }
            }
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "type mismatch: expected {}, got {}",
                    "Dict",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                target_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-has-key?`: Check whether a key exists in a dict.
///
/// Takes 2 args: a key (Int or String) and a dict.
/// Returns `Int 1` if the key is present, `Int 0` if absent.
/// Does NOT force the value at the key — O(1) spine-only lookup.
/// Prelude composes this with `builtin-dict-get` to implement `get?` without Rust knowing Absent.
pub(crate) fn builtin_has_key(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-has-key?", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // Materialize the key
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = {
            let (result, diags) = value_to_hashable_key(&key_val, "builtin-has-key?", thunk0.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Materialize the dict (spine only, not values)
        let thunk1 = args[1].clone();
        let dict_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-has-key?",
                dict_val,
                thunk1.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let exists = if map.contains_key(&key) { 1i64 } else { 0i64 };
        ok_val(
            Value::Int {
                n: exists,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-dict-has-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// O(1) — used by prelude step functions to drive laziness without constructing Absent.
/// `Dict a -> Int -> Int`
pub(crate) fn builtin_dict_has_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-dict-has-nth?", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-has-nth?",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        if idx < 0 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "builtin-dict-has-nth?: dict index must be non-negative".to_string(),
                call_span,
            )).with_diagnostics(_dr_diags);
        }
        let idx_usize = match usize::try_from(idx) {
            Ok(i) => i,
            Err(_overflow) => {
                // idx > usize::MAX on this platform; no dict can have this many entries.
                return ok_val(
                    Value::Int {
                        n: 0,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags);
            }
        };
        let exists = i64::from(map.get_index(idx_usize).is_some());
        ok_val(
            Value::Int {
                n: exists,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-dict-nth`: Get the value at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Errors if `i` is out of bounds.
/// O(1) — drives laziness from the tinct side via prelude step functions that
/// guard with `builtin-dict-has-nth?` before calling this.
/// `Dict a -> Int -> a`
pub(crate) fn builtin_dict_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-dict-nth", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-nth",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        let entry = match usize::try_from(idx) {
            Ok(i) => map.get_index(i),
            Err(_) => None, // idx exceeds addressable range — not in dict
        };
        match entry {
            Some((_, thunk)) => crate::error::DiagnosticResult::ok(Arc::clone(thunk)).with_diagnostics(_dr_diags),
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-dict-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-dict-has-key-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// Identical to `builtin-dict-has-nth?` but paired with `builtin-dict-key-nth`.
/// `Dict a -> Int -> Int`
pub(crate) fn builtin_dict_has_key_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named(
                "builtin-dict-has-key-nth?",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-has-key-nth?",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        if idx < 0 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "builtin-dict-has-key-nth?: dict index must be non-negative".to_string(),
                call_span,
            )).with_diagnostics(_dr_diags);
        }
        let idx_usize = match usize::try_from(idx) {
            Ok(i) => i,
            Err(_overflow) => {
                // idx > usize::MAX on this platform; no dict can have this many entries.
                return ok_val(
                    Value::Int {
                        n: 0,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags);
            }
        };
        let exists = i64::from(map.get_index(idx_usize).is_some());
        ok_val(
            Value::Int {
                n: exists,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-dict-key-nth`: Get the key at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Errors if `i` is out of bounds.
/// Prelude guards with `builtin-dict-has-key-nth?` before calling this.
/// `Dict a -> Int -> Key`
pub(crate) fn builtin_dict_key_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-dict-key-nth", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-key-nth",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        let entry = match usize::try_from(idx) {
            Ok(i) => map.get_index(i),
            Err(_) => None, // idx exceeds addressable range — not in dict
        };
        match entry {
            Some((key, _)) => {
                let key_val = hashable_value_to_value(key);
                ok_val(key_val, call_span).with_diagnostics(_dr_diags)
            }
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-dict-key-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-dict-has-kv-nth?`: Check whether insertion-order position `i` is valid in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns `Int 1` if position `i` exists, `Int 0` if out of bounds.
/// Identical to `builtin-dict-has-nth?` but paired with `builtin-dict-kv-nth`.
/// `Dict a -> Int -> Int`
pub(crate) fn builtin_dict_has_kv_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named(
                "builtin-dict-has-kv-nth?",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-has-kv-nth?",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        if idx < 0 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "builtin-dict-has-kv-nth?: dict index must be non-negative".to_string(),
                call_span,
            )).with_diagnostics(_dr_diags);
        }
        let idx_usize = match usize::try_from(idx) {
            Ok(i) => i,
            Err(_overflow) => {
                // idx > usize::MAX on this platform; no dict can have this many entries.
                return ok_val(
                    Value::Int {
                        n: 0,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags);
            }
        };
        let exists = i64::from(map.get_index(idx_usize).is_some());
        ok_val(
            Value::Int {
                n: exists,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-dict-kv-nth`: Get the key-value pair at insertion-order position `i` in a Dict.
///
/// Takes 2 args: (dict, i: Int). Returns a `[key: K  value: V]` dict at that position.
/// Errors if `i` is out of bounds. Prelude guards with `builtin-dict-has-kv-nth?`.
/// `Dict a -> Int -> [key: Key  value: a]`
pub(crate) fn builtin_dict_kv_nth(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-dict-kv-nth", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }
        let thunk0 = args[0].clone();
        let dict_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-kv-nth",
                dict_val,
                thunk0.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let idx = match idx {
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
                )).with_diagnostics(_dr_diags)
            }
        };
        let entry = match usize::try_from(idx) {
            Ok(i) => map.get_index(i),
            Err(_) => None, // idx exceeds addressable range — not in dict
        };
        match entry {
            Some((key, val_thunk)) => {
                let key_val = hashable_value_to_value(key);
                let mut kv: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                let key_thunk_val = {
                    let (result, diags) = ok_val(key_val, call_span.clone()).into_parts();
                    _dr_diags.extend(diags);
                    match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    }
                };
                kv.insert(
                    HashableValue::Str("key".into()),
                    key_thunk_val,
                );
                kv.insert(HashableValue::Str("value".into()), Arc::clone(val_thunk));
                ok_val(
                    Value::Dict {
                        entries: kv,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-dict-kv-nth: index {idx} out of bounds (dict has {} entries)",
                    map.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builder-get-or`: Atomically get-or-insert in a builder.
///
/// Takes 3 args: key (Int or String), default_value (any), builder.
/// If `key` exists, returns the existing value. Otherwise inserts `default_value` at `key`
/// and returns it. Single mutex acquisition — no race between has? and set.
/// Returns the builder for chaining (NOT the looked-up value — returns builder so callers
/// can chain further operations).
///
/// NOTE: Returns the looked-up/inserted ThunkId value (not the builder).
/// Usage pattern: `[builder-set k [cons x [builder-get-or k [] b]] b]`
pub(crate) fn builtin_builder_get_or(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-get-or", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = match key_val {
            Value::Int { n, .. } => HashableValue::Int(n),
            Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-get-or",
                        "Int, Float, or String (for key)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // args[1] (default value) is NOT materialized — inserted as Arc<Thunk> if key absent.
        let default_thunk = args[1].clone();

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let thunk2 = args[2].clone();
        let builder_val = {
            let (result, diags) = thunk2.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-get-or",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk2.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Atomic get-or-insert: single mutex acquisition
        let result_thunk = match builder.get_or(key, default_thunk) {
            Ok(v) => v,
            Err(_) => return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span)
                    .with_note(format!("operation: {}", "builder-get-or"))
            ).with_diagnostics(_dr_diags),
        };

        crate::error::DiagnosticResult::ok(result_thunk).with_diagnostics(_dr_diags)
    })
}

/// `build-dict`: Efficiently construct a dict from a Seq or Dict of key-value pairs.
///
/// Takes 1 arg (a Seq or Dict where each element is a dict with `key` and `value` fields).
/// Returns a new flat Dict with those entries.
///
/// - **Seq input:** Each element should be `[key: K, value: V]` (like what `each-kv` returns).
///   Forces the key (to extract it), keeps value lazy. Builds an IndexMap. O(n).
/// - **Dict input:** Copies all entries into a new flat IndexMap. O(n).
///
/// Pre-allocates the IndexMap when size is known.
///
/// This replaces O(n^2) merge-accumulation in dict-building stdlib functions like
/// `from-entries`, `map-entries`, `walk`, `transpose`, `collect-kv`, etc.
pub(crate) fn builtin_build_dict(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("build-dict", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] is pre-materialized by force_count=1.
        let thunk0 = args[0].clone();
        let val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match val {
            // Dict input: build from key-value pair dicts in insertion order.
            // Each entry value must be a dict with "key" and "value" fields.
            Value::Dict {
                entries: ref map, ..
            } => {
                let mut result: IndexMap<HashableValue, Arc<Thunk>> =
                    IndexMap::with_capacity(map.len());
                for (_idx, entry_thunk) in map {
                    let entry_val = {
                        let (result, diags) = materialize(entry_thunk, None, &ctx).await.into_parts();
                        _dr_diags.extend(diags);
                        match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        }
                    };
                    let entry_map = {
                        let (result, diags) = crate::builtins::require_dict(
                            "build-dict entry",
                            entry_val,
                            entry_thunk.span.clone(),
                            &ctx,
                            call_span.clone(),
                        )
                        .await.into_parts();
                        _dr_diags.extend(diags);
                        match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        }
                    };
                    let key_thunk = match entry_map
                        .get(&HashableValue::Str("key".into()))
                    {
                        Some(v) => v,
                        None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!("key not found: {}", "key"),
                            call_span.clone(),
                        )).with_diagnostics(_dr_diags),
                    };
                    let value_thunk = match entry_map
                        .get(&HashableValue::Str("value".into()))
                    {
                        Some(v) => v,
                        None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!("key not found: {}", "value"),
                            call_span.clone(),
                        )).with_diagnostics(_dr_diags),
                    };
                    let key_val = {
                        let (result, diags) = materialize(key_thunk, None, &ctx).await.into_parts();
                        _dr_diags.extend(diags);
                        match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        }
                    };
                    let key = match key_val {
                        Value::Int { n, .. } => HashableValue::Int(n),
                        Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
                        Value::String {
                            ref source,
                            start,
                            end,
                            ..
                        } => HashableValue::Str((&source[start..end]).into()),
                        other => {
                            return crate::error::DiagnosticResult::err(Diagnostic::error(
                                KIND,
                                format!(
                                    "{}: expected {}, got {}",
                                    "build-dict",
                                    "Int, Float, or String (for key)",
                                    ctx.type_display_name(other.type_decl_id_of())
                                ),
                                key_thunk.span.clone(),
                            )).with_diagnostics(_dr_diags)
                        }
                    };
                    result.insert(key, Arc::clone(value_thunk));
                }
                ok_val(
                    Value::Dict {
                        entries: result,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }

            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "build-dict",
                    "Dict of [key: K  value: V] pairs",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                thunk0.span.clone(),
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `make-builder`: Create an empty transient builder for efficient mutable dict construction.
/// Takes 0 args. Returns a new Builder.
pub(crate) fn builtin_make_builder(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if !args.is_empty() {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(0),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }
        // Optional named arg: capacity: <Int> — pre-allocates the inner IndexMap.
        // Any other named arg is rejected.
        let capacity: usize = if let Some(ref named_map) = named {
            let cap_thunk = named_map.get("capacity").map(Arc::clone);
            // Reject unexpected named args (all except "capacity").
            let unexpected: IndexMap<String, Arc<Thunk>> = named_map
                .iter()
                .filter(|(k, _)| k.as_str() != "capacity")
                .map(|(k, v)| (k.clone(), Arc::clone(v)))
                .collect();
            if !unexpected.is_empty() {
                let (result, diags) = reject_named("make-builder", Some(&unexpected), call_span.clone()).into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(()) => {}
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            }
            if let Some(cap_thunk) = cap_thunk {
                let cap_val = {
                    let (result, diags) = materialize(&cap_thunk, None, &ctx).await.into_parts();
                    _dr_diags.extend(diags);
                    match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    }
                };
                match cap_val {
                    Value::Int { n, .. } if n >= 0 => n as usize,
                    Value::Int { n, .. } => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "{}: expected {}, got {}",
                                "make-builder",
                                "non-negative Int",
                                format!("Int({})", n)
                            ),
                            cap_thunk.span.clone(),
                        )).with_diagnostics(_dr_diags)
                    }
                    other => {
                        return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!(
                                "{}: expected {}, got {}",
                                "make-builder",
                                "Int",
                                ctx.type_display_name(other.type_decl_id_of())
                            ),
                            cap_thunk.span.clone(),
                        )).with_diagnostics(_dr_diags)
                    }
                }
            } else {
                0
            }
        } else {
            0
        };
        let builder = if capacity > 0 {
            crate::value::Builder::with_capacity(capacity)
        } else {
            crate::value::Builder::new()
        };
        ok_val(Value::Builder(Arc::new(builder)), call_span).with_diagnostics(_dr_diags)
    })
}

/// `builder-set`: Set a key-value pair in a builder. Returns the builder for chaining.
/// Takes 3 args: key (Int or String), value (any), builder.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_set(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-set", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = match key_val {
            Value::Int { n, .. } => HashableValue::Int(n),
            Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-set",
                        "Int, Float, or String (for key)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // args[1] (value) is NOT materialized — pass the Arc<Thunk> directly to the builder
        let value_thunk = args[1].clone();

        // args[2] (builder) is pre-forced by W1 pos_strictness[2]=Seq scan
        let thunk2 = args[2].clone();
        let builder_val = {
            let (result, diags) = thunk2.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-set",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk2.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Set the key-value pair
        match builder.set(key, value_thunk) {
            Ok(()) => {}
            Err(_) => return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span)
                    .with_note(format!("operation: {}", "builder-set"))
            ).with_diagnostics(_dr_diags),
        }

        // Return the builder for chaining
        crate::error::DiagnosticResult::ok(args[2].clone()).with_diagnostics(_dr_diags)
    })
}

/// `builder-delete`: Remove a key from a builder. Returns the builder for chaining.
/// Takes 2 args: key (Int or String), builder.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_delete(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-delete", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = match key_val {
            Value::Int { n, .. } => HashableValue::Int(n),
            Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-delete",
                        "Int, Float, or String (for key)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-delete",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk1.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Delete the key
        match builder.delete(&key) {
            Ok(()) => {}
            Err(_) => return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span)
                    .with_note(format!("operation: {}", "builder-delete"))
            ).with_diagnostics(_dr_diags),
        }

        // Return the builder for chaining
        crate::error::DiagnosticResult::ok(args[1].clone()).with_diagnostics(_dr_diags)
    })
}

/// `builder-finish`: Take the inner dict from a builder, freezing it permanently.
/// Takes 1 arg: builder. Returns a Dict.
/// Errors if the builder is already frozen.
pub(crate) fn builtin_builder_finish(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-finish", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (builder) is pre-forced by force_count
        let thunk0 = args[0].clone();
        let builder_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-finish",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Take the inner dict, freezing the builder
        let dict = match builder.finish() {
            Ok(v) => v,
            Err(_) => return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span.clone())
                    .with_note(format!("operation: {}", "builder-finish"))
            ).with_diagnostics(_dr_diags),
        };

        ok_val(
            Value::Dict {
                entries: dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builder-snapshot`: Clone the inner dict without freezing the builder.
/// Takes 1 arg: builder. Returns a Dict.
/// Errors if the builder is frozen.
pub(crate) fn builtin_builder_snapshot(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-snapshot", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (builder) is pre-forced by force_count
        let thunk0 = args[0].clone();
        let builder_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-snapshot",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Clone the inner dict
        let dict = match builder.snapshot() {
            Ok(v) => v,
            Err(_) => return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span.clone())
                    .with_note(format!("operation: {}", "builder-snapshot"))
            ).with_diagnostics(_dr_diags),
        };

        ok_val(
            Value::Dict {
                entries: dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builder-has?`: Check if a key exists in a builder.
/// Takes 2 args: key (Int or String), builder. Returns Bool.
pub(crate) fn builtin_builder_has(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-has?", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = match key_val {
            Value::Int { n, .. } => HashableValue::Int(n),
            Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-has?",
                        "Int, Float, or String (for key)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-has?",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk1.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Frozen builder: has? is an error, not a silent false
        if builder.is_frozen() {
            return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span.clone())
                    .with_note(format!("operation: {}", "builder-has?"))
            ).with_diagnostics(_dr_diags);
        }

        // Check if the key exists
        let has = builder.has(&key);
        ok_val(
            Value::Int {
                n: if has { 1 } else { 0 },
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builder-get`: Get a value from a builder by key.
/// Takes 2 args: key (Int or String), builder. Returns the value or errors if key not found.
pub(crate) fn builtin_builder_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builder-get", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // args[0] (key) is pre-forced by pos_strictness[0]=Seq via W1 scan
        let thunk0 = args[0].clone();
        let key_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let key = match key_val {
            Value::Int { n, .. } => HashableValue::Int(n),
            Value::Float { n: f, .. } => HashableValue::Float(f.to_bits()),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                HashableValue::Str(s.into())
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-get",
                        "Int, Float, or String (for key)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // args[1] (builder) is pre-forced by force_count
        let thunk1 = args[1].clone();
        let builder_val = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let builder = match builder_val {
            Value::Builder(b) => b,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builder-get",
                        "Builder",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk1.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // Frozen builder: get is an error distinct from key-not-found
        if builder.is_frozen() {
            return crate::error::DiagnosticResult::err(
                Diagnostic::error(KIND, "builder already finished", call_span.clone())
                    .with_note(format!("operation: {}", "builder-get"))
            ).with_diagnostics(_dr_diags);
        }

        // Get the value
        match builder.get(&key) {
            Some(thunk) => crate::error::DiagnosticResult::ok(thunk).with_diagnostics(_dr_diags),
            None => {
                let key_str = key.to_string();
                crate::error::DiagnosticResult::err(
                    Diagnostic::error(KIND, format!("key not found: {}", key_str), call_span)
                ).with_diagnostics(_dr_diags)
            }
        }
    })
}

/// `builtin-get-by-field`: Reverse lookup on a type-level lookup table (T-1378).
///
/// Takes 3 args:
/// - `field-name`: String — the constant field to match on (e.g. `"rcode"`)
/// - `field-value`: Any — the target constant value (e.g. `Int(2)`)
/// - `type-dict`: Dict — the runtime constructor dict produced by `[type ...]`
///   (e.g. `DnsRcode` = `{"NoError": Variant("DnsRcode.NoError"), ...}`)
///
/// Searches `TyConDef.constructor_constants` for the first variant whose
/// `field-name` constant equals `field-value`. Returns that variant or
/// errors if no match.
///
/// `[get rcode: 2 DnsRcode]` desugars to `[builtin-get-by-field "rcode" 2 DnsRcode]`
/// via the `get` wrapper in prelude.
pub(crate) fn builtin_get_by_field(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("builtin-get-by-field", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        // arg[0]: field-name (String) — pre-forced by pos_strictness
        let thunk0 = args[0].clone();
        let field_name_val = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let field_name: String = match field_name_val {
            Value::String {
                ref source,
                start,
                end,
                ..
            } => source[start..end].to_string(),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-get-by-field",
                        "String",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk0.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        // arg[1]: field-value (Any) — pre-forced by pos_strictness
        let field_value = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // arg[2]: type-dict (Dict of Variants) — pre-forced to Spine by pos_strictness.
        // Eagerly collect all (unqualified-name -> ThunkId) pairs from the dict so we
        // hold no borrow across the upcoming await points.
        let thunk2 = args[2].clone();
        let type_dict_val = {
            let (result, diags) = thunk2.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        // Collect (string-key -> Arc<Thunk>) pairs; skip integer-keyed entries.
        let dict_entries: Vec<(String, Arc<Thunk>)> = match &type_dict_val {
            Value::Dict { entries: map, .. } => map
                .iter()
                .filter_map(|(k, thunk)| match k {
                    HashableValue::Str(s) => Some((s.to_string(), Arc::clone(thunk))),
                    _ => None,
                })
                .collect(),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-get-by-field",
                        "Dict",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    thunk2.span.clone(),
                )).with_diagnostics(_dr_diags)
            }
        };

        if dict_entries.is_empty() {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "builtin-get-by-field: type dict is empty".to_string(),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        // Get the type_decl_id from the first Variant in the dict.
        let mut variant_type_id: Option<u64> = None;
        for (_name, entry_thunk) in &dict_entries {
            let entry_val = {
                let (result, diags) = materialize(entry_thunk, Some(&call_span), &ctx).await.into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            };
            if let Value::Variant { type_decl_id, .. } = entry_val {
                variant_type_id = Some(type_decl_id);
                break;
            }
        }

        let variant_type_id = match variant_type_id {
            Some(id) => id,
            None => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    "builtin-get-by-field: type dict contains no Variant values".to_string(),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        let tycon_env = match ctx.tycon_env.get() {
            Some(env) => env,
            None => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-get-by-field: type info not available for type {} \
                         (--no-typecheck mode)",
                        ctx.type_display_name(variant_type_id)
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        let def = match tycon_env.get(&variant_type_id) {
            Some(d) => d,
            None => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-get-by-field: type {} not found in type environment",
                        ctx.type_display_name(variant_type_id)
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        // Scan constructor_constants for the first variant where
        // constants[field_name] == field_value.
        // constructor_constants: IndexMap<u32 (ctor_index), IndexMap<String (field), Value>>
        for (ctor_idx, constants) in &def.constructor_constants {
            if let Some(constant_val) = constants.get(field_name.as_ref() as &str) {
                if *constant_val == field_value {
                    // Found a match. Look up the unqualified ctor name from the ctor_index.
                    let empty = std::collections::HashMap::new();
                    let tycon_env = ctx.tycon_env().unwrap_or(&empty);
                    let unqualified =
                        crate::type_tags::ctor_display_name(variant_type_id, *ctor_idx, tycon_env);
                    let bare_name = unqualified
                        .split_once('.')
                        .map(|(_, c)| c)
                        .unwrap_or(&unqualified);
                    // Find the matching ThunkId in our pre-collected dict_entries.
                    if let Some((_name, thunk)) = dict_entries.iter().find(|(n, _)| n == bare_name)
                    {
                        return crate::error::DiagnosticResult::ok(Arc::clone(thunk)).with_diagnostics(_dr_diags);
                    }
                }
            }
        }

        // No match found — error
        crate::error::DiagnosticResult::err(Diagnostic::error(
            KIND,
            format!(
                "builtin-get-by-field: no constructor of {} has field \"{field_name}\" \
                 matching the given value",
                ctx.type_display_name(variant_type_id)
            ),
            call_span,
        )).with_diagnostics(_dr_diags)
    })
}

/// `builtin-dict-merge`: Right-biased dict union.
///
/// Takes two Dict values and returns a new Dict containing all entries from `a`, with entries
/// from `b` added or overriding any same-keyed entries from `a`. Entries unique to `a` appear
/// first (in their original order), followed by entries from `b` (in their original order).
///
/// This is the primitive used by the spread-dict lowering (`[...rest  key: val]`). It is
/// registered as a core builtin so that spread-dict desugaring is independent of the prelude.
///
/// Args: (a: Dict, b: Dict) — both pre-materialized to Spine.
pub(crate) fn builtin_dict_merge(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        {
            let (result, diags) = reject_named("builtin-dict-merge", named.as_ref(), call_span.clone()).into_parts();
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
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
        }

        let thunk_a = args[0].clone();
        let val_a = {
            let (result, diags) = thunk_a.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map_a = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-merge",
                val_a,
                thunk_a.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let thunk_b = args[1].clone();
        let val_b = {
            let (result, diags) = thunk_b.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let map_b = {
            let (result, diags) = crate::builtins::require_dict(
                "builtin-dict-merge",
                val_b,
                thunk_b.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Right-biased union: start with all of a's entries, then overlay b.
        // Keys present in both take b's value. Keys only in a are preserved.
        // Keys only in b are appended after a's keys.
        let mut result: IndexMap<HashableValue, Arc<Thunk>> =
            IndexMap::with_capacity(map_a.len() + map_b.len());
        for (k, v) in &map_a {
            result.insert(k.clone(), Arc::clone(v));
        }
        for (k, v) in &map_b {
            result.insert(k.clone(), Arc::clone(v));
        }

        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

// -- Collection operations (moved from builtins_seq_xform.rs and builtins_seq_reduce.rs
//    in T-1380 -- Dict-only dispatch; Seq paths implemented as pure tinct in prelude.llt) ----

/// `builtin-take`: Take the first n elements from a Dict by position.
///
/// Dict path only (T-1380). For Seq inputs, use the pure-tinct `take` wrapper in prelude.llt.
/// Returns a Dict with the first n entries preserving keys.
///
/// Args: (n, xs)
pub(crate) fn builtin_take(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        {
            let (result, diags) = reject_named("take", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }

        let n = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let n_int = match n {
            Value::Int { n: i, .. } => i,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "take",
                        "Int",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        let thunk1 = args[1].clone();
        let xs = {
            let (result, diags) = thunk1.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        if n_int <= 0 {
            return ok_val(
                Value::Dict {
                    entries: IndexMap::new(),
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags);
        }

        match xs {
            Value::Dict {
                entries: ref map, ..
            } => {
                let taken: IndexMap<HashableValue, Arc<Thunk>> = map
                    .iter()
                    .take(n_int as usize)
                    .map(|(k, v)| (k.clone(), Arc::clone(v)))
                    .collect();
                ok_val(
                    Value::Dict {
                        entries: taken,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "take",
                    "Dict",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-drop`: Drop the first n elements from a Dict by position.
///
/// Dict path only (T-1380). For Seq inputs, use the pure-tinct `drop` wrapper in prelude.llt.
/// Returns a Dict with the remaining entries after skipping first n, preserving keys.
///
/// Args: (n, xs)
pub(crate) fn builtin_drop(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        {
            let (result, diags) = reject_named("drop", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }

        let n = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let n_int = match n {
            Value::Int { n: i, .. } => i,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "drop",
                        "Int",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        if n_int <= 0 {
            return crate::error::DiagnosticResult::ok(args[1].clone()).with_diagnostics(_dr_diags);
        }

        let xs = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match xs {
            Value::Dict {
                entries: ref map, ..
            } => {
                let dropped: IndexMap<HashableValue, Arc<Thunk>> = map
                    .iter()
                    .skip(n_int as usize)
                    .map(|(k, v)| (k.clone(), Arc::clone(v)))
                    .collect();
                ok_val(
                    Value::Dict {
                        entries: dropped,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "drop",
                    "Dict",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-concat`: Concatenate two collections (Dict or Seq).
///
/// Moved from builtins_seq_reduce.rs to builtins_dict.rs in T-1380.
/// Seq path is preserved for the type-stage evaluator (TypeNode.children uses builtin-concat
/// on Seq values). For user code, the `concat` prelude wrapper dispatches to concat-seq
/// (pure tinct) for Seq inputs.
///
/// - For Seq: lazily chain xs and ys (O(1) initial, O(n) on materialization).
/// - For Dict: eagerly materialize both dicts and merge them with integer reindexing.
///
/// Args: (xs, ys)
pub(crate) fn builtin_concat(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        {
            let (result, diags) = reject_named("concat", named.as_ref(), call_span.clone()).into_parts();
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
            )).with_diagnostics(_dr_diags);
        }

        let thunk0 = args[0].clone();
        let thunk1 = args[1].clone();
        let xs_span = thunk0.span.clone();
        let ys_span = thunk1.span.clone();
        let xs = {
            let (result, diags) = thunk0.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let ys_thunk = thunk1;

        match xs {
            Value::Dict {
                entries: ref xs_map,
                ..
            } => {
                // Dict path: eagerly merge with integer reindexing.
                if xs_map.is_empty() {
                    return crate::error::DiagnosticResult::ok(ys_thunk).with_diagnostics(_dr_diags);
                }

                let ys = {
                    let (result, diags) = materialize(&ys_thunk, None, &ctx).await.into_parts();
                    _dr_diags.extend(diags);
                    match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    }
                };
                match ys {
                    Value::Dict {
                        entries: ref ys_map,
                        ..
                    } => {
                        let mut result: IndexMap<HashableValue, Arc<Thunk>> =
                            IndexMap::with_capacity(xs_map.len() + ys_map.len());
                        let mut idx = 0i64;
                        for (_key, thunk) in xs_map {
                            result.insert(HashableValue::Int(idx), Arc::clone(thunk));
                            idx = match idx.checked_add(1) {
                                Some(v) => v,
                                None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                                    KIND,
                                    format!("{}: integer overflow", "concat"),
                                    call_span.clone(),
                                )).with_diagnostics(_dr_diags),
                            };
                        }
                        for (_key, thunk) in ys_map {
                            result.insert(HashableValue::Int(idx), Arc::clone(thunk));
                            idx = match idx.checked_add(1) {
                                Some(v) => v,
                                None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                                    KIND,
                                    format!("{}: integer overflow", "concat"),
                                    call_span.clone(),
                                )).with_diagnostics(_dr_diags),
                            };
                        }
                        ok_val(
                            Value::Dict {
                                entries: result,
                                type_decl_id: TYPE_ID_DICT,
                                type_val: Some(crate::type_infer::make_typevalue_nominal(
                                    crate::value::TYPE_ID_DICT,
                                )),
                            },
                            call_span,
                        ).with_diagnostics(_dr_diags)
                    }
                    other => crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!(
                            "{}: expected {}, got {}",
                            "concat",
                            "Dict",
                            ctx.type_display_name(other.type_decl_id_of())
                        ),
                        ys_span,
                    )
                    .with_materialization_span(call_span)).with_diagnostics(_dr_diags),
                }
            }
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "concat",
                    "Dict",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                xs_span,
            )
            .with_materialization_span(call_span)).with_diagnostics(_dr_diags),
        }
    })
}
