//! Type inference machinery: InferenceContext, InferState, generalization.
//!
//! This module contains the core type inference infrastructure including
//! the InferenceContext (TypeValue substitution, levels-based let-generalization,
//! Kiselyov 2013) used throughout the S-1003 Arc<Value> migration.
//!
//! NOTE: The `Substitution` and `TypeScheme` structs were deleted as part of S-1003 T-2004.
//! The `Type` enum was deleted from `type_def.rs` in T-1986.
//! Type representations now use `Arc<Value>` (TypeValue) throughout.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use crate::ast::Span;
use crate::type_tags::*;
// TypeValue is the canonical Arc<Value> alias for tinct-side type representations.
// The definition lives in type_class.rs — re-export it here so callers can import
// TypeValue from type_infer without reaching into type_class directly.
pub use crate::type_class::TypeValue;

// ── TypeValue helper functions ────────────────────────────────────────────────

/// Create a TypeValue.Var Arc<Value> with the given name.
///
/// The level is NOT stored in the TypeValue — register it in `InferenceContext.levels`
/// separately. TypeValue.Var carries identity only (the variable's name).
pub fn make_typevar_value(name: &str) -> TypeValue {
    // Payload: a settled Dict with { name: String }
    let payload = make_dict_typevalue(&[(FIELD_NAME, make_string_typevalue(name))]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_VAR),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Unknown Arc<Value> (the gradual `?` escape hatch).
pub fn make_typevalue_unknown() -> TypeValue {
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_UNKNOWN),
        payload: None,
    })
}

/// Create a TypeValue.Never Arc<Value> (the bottom type ⊥).
pub fn make_typevalue_never() -> TypeValue {
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_NEVER),
        payload: None,
    })
}

/// Create a TypeValue.Top Arc<Value> (the top type ⊤).
pub fn make_typevalue_top() -> TypeValue {
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_TOP),
        payload: None,
    })
}

/// Map a TypeNode bare constructor name to its canonical TypeValue.
///
/// This is the **third authorized translator** listed in `doc/16b-rust-tinct-protocol.md §3`.
/// It covers the unit (no-payload) TypeNode constructors that have a direct primitive TypeValue
/// equivalent — `Int`, `Float`, `String`, `Bytes`, `Proxy`, and `Callable`. These are the
/// constructors that can appear as a *pin value* in a match pattern without any payload.
///
/// `bare_ctor` is the unqualified constructor name — the part after `"TypeNode."`. For example,
/// given the fully-qualified ctor `"TypeNode.Int"`, pass `"Int"`.
///
/// Returns `None` for unknown or payload-carrying constructors (those require async
/// `typenode_value_to_type` which reads payload fields). Returns `None` for `Dict` because the
/// runtime type check for `@Dict` is a structural record check, not a `Repr` check, and the
/// payload-free `TypeNode.Dict` variant in a pin position has no field information to inspect.
///
/// This function is **pure and synchronous** — no evaluation context or async required.
/// Its sole job is the constant table mapping bare names to `make_typevalue_*` calls.
pub fn typenode_ctor_to_typevalue(bare_ctor: &str) -> Option<TypeValue> {
    match bare_ctor {
        TN_BARE_INT => Some(make_typevalue_repr(REPR_INT)),
        TN_BARE_FLOAT => Some(make_typevalue_repr(REPR_FLOAT)),
        TN_BARE_STRING => Some(make_typevalue_repr(REPR_STRING)),
        TN_BARE_BYTES => Some(make_typevalue_repr(REPR_BYTES)),
        TN_BARE_PROXY => Some(make_typevalue_repr(REPR_PROXY)),
        TN_BARE_CALLABLE => Some(make_typevalue_fn_with_flags(
            vec![],
            make_typevalue_top(),
            Some(0), // required_count — no fixed params
            true,    // variadic — accepts any number of arguments
            Vec::new(),
        )),
        // All other TypeNode constructors either have payloads (Dict, Union, Intersect, Arrow,
        // Recursive, …) or are abstract lattice types (Top, Unknown, Never, Absent) that cannot
        // usefully appear as no-payload pin patterns in match. None signals that the caller
        // should fall through to value equality.
        _ => None,
    }
}

/// Create a TypeValue.Repr Arc<Value> for a primitive type.
///
/// `repr` is the Rust variant discriminant string, e.g. `"Value::Int"`, `"Value::Float"`.
/// The `is` field is left as an empty dict (no typeclass instance attached at construction time).
pub fn make_typevalue_repr(repr: &str) -> TypeValue {
    // Payload: { repr: String, is: [] }
    let payload = make_dict_typevalue(&[
        (FIELD_REPR, make_string_typevalue(repr)),
        (FIELD_IS, make_empty_dict_typevalue()),
    ]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_REPR),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.IntLit Arc<Value> for an integer literal type.
pub fn make_typevalue_int_lit(n: i64) -> TypeValue {
    let payload = make_dict_typevalue(&[(
        FIELD_VALUE,
        crate::value::Value::Int {
            n,
            type_val: crate::value::unknown_type_val(),
        },
    )]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_INT_LIT),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.StrLit Arc<Value> for a string literal type.
pub fn make_typevalue_str_lit(s: &str) -> TypeValue {
    let payload = make_dict_typevalue(&[(FIELD_VALUE, make_string_typevalue(s))]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_STR_LIT),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.FloatLit Arc<Value> for a float literal type.
pub fn make_typevalue_float_lit(f: f64) -> TypeValue {
    let payload = make_dict_typevalue(&[(
        FIELD_VALUE,
        crate::value::Value::Float {
            n: f,
            type_val: crate::value::unknown_type_val(),
        },
    )]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_FLOAT_LIT),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Op Arc<Value> for a type constructor name.
pub fn make_typevalue_op(name: &str) -> TypeValue {
    let payload = make_dict_typevalue(&[(FIELD_NAME, make_string_typevalue(name))]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_OP),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Var Arc<Value> — alias for `make_typevar_value`.
pub fn make_typevalue_var(name: &str) -> TypeValue {
    make_typevar_value(name)
}

/// Create a TypeValue.Fn Arc<Value> for a function type.
///
/// `params`: list of `(param_name_opt, TypeValue)` pairs for fixed parameters.
/// `ret`: the return TypeValue.
pub fn make_typevalue_fn(params: Vec<(Option<String>, TypeValue)>, ret: TypeValue) -> TypeValue {
    make_typevalue_fn_with_flags(params, ret, None, false, Vec::new())
}

/// Create a TypeValue.Fn with variadic flag, typed variadic buckets, required count, and param names stored in the payload.
///
/// This is the full-fidelity version of `make_typevalue_fn` that stores:
/// - `params`: integer-keyed dict of param types
/// - `param-names`: integer-keyed dict of param names (when names are available)
/// - `required`: integer count of required (non-default) params; absent if all params are required (= params.len())
/// - `variadic`: "true" Variant if variadic, absent if not
/// - `typed-variadics`: integer-keyed dict of `{ name: String, ty: TypeValue }` entries
///   for typed variadic buckets (e.g. `...xs@Seq[Integer]`); absent if empty
/// - `return`: the return type
pub fn make_typevalue_fn_with_flags(
    params: Vec<(Option<String>, TypeValue)>,
    ret: TypeValue,
    required_count: Option<usize>,
    variadic: bool,
    typed_variadics: Vec<(Option<String>, TypeValue)>,
) -> TypeValue {
    use crate::value::HashableValue;
    // Build params dict: { 0: TypeValue, 1: TypeValue, ... }
    let mut params_entries = indexmap::IndexMap::new();
    let mut names_entries = indexmap::IndexMap::new();
    for (i, (name, param_tv)) in params.iter().enumerate() {
        params_entries.insert(
            HashableValue::Int(i as i64),
            Arc::new(crate::value::Thunk::value(
                param_tv.as_ref().clone(),
                crate::rust_span!(),
            )),
        );
        if let Some(ref n) = name {
            names_entries.insert(
                HashableValue::Int(i as i64),
                Arc::new(crate::value::Thunk::value(
                    make_string_typevalue(n),
                    crate::rust_span!(),
                )),
            );
        }
        // Unnamed params (name=None) are left absent in names_entries so that
        // typevalue_fn_param_names returns None for that index. Named-arg matching
        // must not treat an index string ("0", "1") as an identifier name.
    }
    let params_dict = crate::value::Value::Dict {
        entries: params_entries,
        type_val: crate::value::unknown_type_val(),
    };
    let param_names_dict = crate::value::Value::Dict {
        entries: names_entries,
        type_val: crate::value::unknown_type_val(),
    };
    let mut payload_entries = indexmap::IndexMap::new();
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_PARAMS)),
        Arc::new(crate::value::Thunk::value(params_dict, crate::rust_span!())),
    );
    // Store param-names dict: only named params have entries. Unnamed params (name=None)
    // are absent from the dict so that typevalue_fn_param_names returns None for those
    // indices, preventing false named-arg matches against index strings ("0", "1").
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_PARAM_NAMES)),
        Arc::new(crate::value::Thunk::value(
            param_names_dict,
            crate::rust_span!(),
        )),
    );
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_RETURN)),
        Arc::new(crate::value::Thunk::value(
            ret.as_ref().clone(),
            crate::rust_span!(),
        )),
    );
    // Store required count if different from params.len() (i.e., function has optional params).
    // Only stored when needed to keep payload minimal for all-required functions.
    if let Some(req_count) = required_count {
        if req_count != params.len() {
            payload_entries.insert(
                HashableValue::Str(Arc::from(FIELD_REQUIRED)),
                Arc::new(crate::value::Thunk::value(
                    crate::value::Value::Int {
                        n: req_count as i64,
                        type_val: crate::value::unknown_type_val(),
                    },
                    crate::rust_span!(),
                )),
            );
        }
    }
    if variadic {
        // Store "variadic: true" as a unit Variant with ctor BOOL_TRUE ("true").
        let true_variant = crate::value::Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(BOOL_TRUE),
            payload: None,
        };
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_VARIADIC)),
            Arc::new(crate::value::Thunk::value(
                true_variant,
                crate::rust_span!(),
            )),
        );
    }
    // Store typed variadic buckets: integer-keyed dict of { name: String, ty: TypeValue }.
    // Each entry is one typed variadic param (e.g. `...xs@Seq[Integer]`).
    // Only stored when non-empty to keep the payload minimal for non-variadic functions.
    if !typed_variadics.is_empty() {
        let mut tv_entries = indexmap::IndexMap::new();
        for (i, (name, ty)) in typed_variadics.iter().enumerate() {
            // Each bucket is a dict { name: String, ty: TypeValue }.
            // name may be None for anonymous typed variadics (stored as empty string).
            let bucket_name_str = name.as_deref().unwrap_or("");
            let bucket_dict_val = make_dict_typevalue(&[
                (FIELD_NAME, make_string_typevalue(bucket_name_str)),
                (FIELD_OF, ty.as_ref().clone()),
            ]);
            tv_entries.insert(
                HashableValue::Int(i as i64),
                Arc::new(crate::value::Thunk::value(
                    bucket_dict_val,
                    crate::rust_span!(),
                )),
            );
        }
        let tv_dict = crate::value::Value::Dict {
            entries: tv_entries,
            type_val: crate::value::unknown_type_val(),
        };
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_TYPED_VARIADICS)),
            Arc::new(crate::value::Thunk::value(tv_dict, crate::rust_span!())),
        );
    }
    let payload_dict = crate::value::Value::Dict {
        entries: payload_entries,
        type_val: crate::value::unknown_type_val(),
    };
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_FN),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload_dict,
            crate::rust_span!(),
        ))),
    })
}

/// Create a RowTail.Closed Arc<Value> — the tail for a closed (fully-specified) record.
pub fn make_rowtail_closed() -> TypeValue {
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(RT_CLOSED),
        payload: None,
    })
}

/// Create a RowTail.Var Arc<Value> — the tail for an open record with a polymorphic row variable.
///
/// The payload is a Dict with a single "name" field containing the RowVar name string,
/// mirroring TypeValue.Var { name: String } structure for consistency with `typevalue_var_name`.
///
/// RowVar names participate in the same level-tracking and substitution infrastructure
/// as TypeVar names. Register the name in `ctx.levels` before calling `make_rowtail_var`.
#[cfg(test)]
pub fn make_rowtail_var(name: &str) -> TypeValue {
    let payload = make_dict_typevalue_from_value(&[(FIELD_NAME, make_string_typevalue(name))]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(RT_VAR),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Extract the RowVar name from a RowTail.Var TypeValue.
///
/// RowTail.Var payload is a Dict with { name: String }. Returns `None` if the TypeValue
/// is not a RowTail.Var or if the payload is unsettled/malformed.
pub fn extract_rowtail_var_name(tv: &TypeValue) -> Option<String> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == RT_VAR => match thunk.peek_result() {
            Some(Ok(crate::value::Value::Dict { entries, .. })) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_NAME));
                let name_thunk = entries.get(&key)?;
                match name_thunk.peek_result()? {
                    Ok(crate::value::Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Create a RowTail.Uniform Arc<Value> — the tail for an open record where all additional
/// fields share the given `value_type`. Pass `make_typevalue_top()` for "any field value".
pub fn make_rowtail_uniform(value_type: TypeValue) -> TypeValue {
    make_rowtail_uniform_with_key_type(value_type, None)
}

/// Create a RowTail.Uniform with an optional key-type constraint.
///
/// When `key_type` is `Some(ty)`, the RowTail.Uniform payload includes a `key-type` field
/// that constrains the key type of additional map entries. This enables typed-key maps
/// (e.g., `[key-type: String  value-type: Int]` → a map from String keys to Int values).
///
/// The key-type field uses the `RT_FIELD_KEY_TYPE` constant ("key-type") from type_tags.
pub fn make_rowtail_uniform_with_key_type(
    value_type: TypeValue,
    key_type: Option<TypeValue>,
) -> TypeValue {
    let mut fields: Vec<(&str, crate::value::Value)> =
        vec![(RT_FIELD_VALUE_TYPE, value_type.as_ref().clone())];
    if let Some(kt) = key_type {
        fields.push((RT_FIELD_KEY_TYPE, kt.as_ref().clone()));
    }
    let payload = make_dict_typevalue_from_value(&fields);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(RT_UNIFORM),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Record Arc<Value> for a record/dict type.
///
/// `fields`: map of field name → TypeValue for the known fields.
/// `tail`: the row tail — must be a RowTail Variant constructed by `make_rowtail_*`:
///   - `None` → closed record (RowTail.Closed, no additional fields allowed)
///   - `Some(make_rowtail_closed())` → explicit closed record (equivalent to None)
///   - `Some(make_rowtail_uniform(value_type))` → open record; all extra fields have `value_type`
///
/// IMPORTANT: Do NOT pass a raw TypeValue (e.g., TypeValue.Top) as the tail — `bas.rs`
/// dispatch expects a RowTail Variant. Raw TypeValues in the tail field will fall through
/// to the "closed" branch in `is_record_subtype`, silently producing incorrect subtype checks.
pub fn make_typevalue_record(
    fields: indexmap::IndexMap<String, TypeValue>,
    tail: Option<TypeValue>,
) -> TypeValue {
    use crate::value::HashableValue;

    // Validate tail is a RowTail Variant (Closed/Var/Uniform).
    if let Some(ref t) = tail {
        let ctor = typevalue_ctor(t);
        assert!(
            matches!(ctor, Some(RT_CLOSED) | Some(RT_VAR) | Some(RT_UNIFORM)),
            "make_typevalue_record: tail must be a RowTail Variant (Closed/Var/Uniform), got {:?}",
            ctor
        );
    }

    // Build fields dict
    let mut fields_entries = indexmap::IndexMap::new();
    for (name, tv) in fields {
        fields_entries.insert(
            HashableValue::Str(Arc::from(name.as_str())),
            Arc::new(crate::value::Thunk::value(
                tv.as_ref().clone(),
                crate::rust_span!(),
            )),
        );
    }
    let fields_dict = crate::value::Value::Dict {
        entries: fields_entries,
        type_val: crate::value::unknown_type_val(),
    };
    // Build tail: None (closed) → RowTail.Closed variant
    let tail_val = match tail {
        Some(t) => t.as_ref().clone(),
        None => make_rowtail_closed().as_ref().clone(),
    };
    let mut payload_entries = indexmap::IndexMap::new();
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_FIELDS)),
        Arc::new(crate::value::Thunk::value(fields_dict, crate::rust_span!())),
    );
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_TAIL)),
        Arc::new(crate::value::Thunk::value(tail_val, crate::rust_span!())),
    );
    let payload_dict = crate::value::Value::Dict {
        entries: payload_entries,
        type_val: crate::value::unknown_type_val(),
    };
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_RECORD),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload_dict,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Union Arc<Value> for a union of types.
///
/// Members are ordered positionally (0, 1, 2, ...) in the payload dict.
pub fn make_typevalue_union(members: Vec<TypeValue>) -> TypeValue {
    use crate::value::HashableValue;
    let mut members_entries = indexmap::IndexMap::new();
    for (i, tv) in members.iter().enumerate() {
        members_entries.insert(
            HashableValue::Int(i as i64),
            Arc::new(crate::value::Thunk::value(
                tv.as_ref().clone(),
                crate::rust_span!(),
            )),
        );
    }
    let members_dict = crate::value::Value::Dict {
        entries: members_entries,
        type_val: crate::value::unknown_type_val(),
    };
    let payload = make_dict_typevalue_from_value(&[(FIELD_MEMBERS, members_dict)]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_UNION),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Inter Arc<Value> for an intersection of types.
pub fn make_typevalue_intersection(members: Vec<TypeValue>) -> TypeValue {
    use crate::value::HashableValue;
    let mut members_entries = indexmap::IndexMap::new();
    for (i, tv) in members.iter().enumerate() {
        members_entries.insert(
            HashableValue::Int(i as i64),
            Arc::new(crate::value::Thunk::value(
                tv.as_ref().clone(),
                crate::rust_span!(),
            )),
        );
    }
    let members_dict = crate::value::Value::Dict {
        entries: members_entries,
        type_val: crate::value::unknown_type_val(),
    };
    let payload = make_dict_typevalue_from_value(&[(FIELD_MEMBERS, members_dict)]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_INTER),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Neg Arc<Value> for a negation type.
pub fn make_typevalue_negation(inner: TypeValue) -> TypeValue {
    let payload = make_dict_typevalue_from_value(&[(FIELD_OF, inner.as_ref().clone())]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_NEG),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.App Arc<Value> for a type application.
pub fn make_typevalue_app(op: TypeValue, arg: TypeValue) -> TypeValue {
    let payload = make_dict_typevalue_from_value(&[
        (FIELD_OP, op.as_ref().clone()),
        (FIELD_ARG, arg.as_ref().clone()),
    ]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_APP),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.NominalVariant Arc<Value> for a nominal variant type.
///
/// `tycon`: the type constructor name string.
/// `ctor`: the constructor tag string (qualified, e.g., "Color.Red").
/// `fields`: a TypeValue.Record or empty dict for unit constructors.
pub fn make_typevalue_nominal_variant(tycon: &str, ctor: &str, fields: TypeValue) -> TypeValue {
    let payload = make_dict_typevalue_from_value(&[
        (FIELD_TYCON, make_string_typevalue(tycon)),
        (FIELD_CTOR, make_string_typevalue(ctor)),
        (FIELD_FIELDS, fields.as_ref().clone()),
    ]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_NOMINAL_VARIANT),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.Recursive Arc<Value> for a recursive type (de Bruijn).
///
/// De Bruijn contract: the `body` must use `TypeValue.RecursiveRef { depth: 0 }` (created by
/// `make_typevalue_recursive_ref(0)`) wherever the type self-refers. Do NOT use a TypeValue.Var
/// for the self-reference — Recursive types are de Bruijn indexed, not name-bound. A free TypeVar
/// in `body` will remain unbound by this constructor, producing a type with a dangling variable
/// rather than the intended equirecursive self-reference.
///
/// For nested Recursive types (μ inside μ), the inner Recursive's body uses depth=0 for its own
/// binder and depth=1 (or higher) to refer to the outer binder. `substitute_recursive_ref` handles
/// de Bruijn shifting correctly when unfolding nested types.
///
/// **Exception**: `typecheck_annot.rs` uses `TypeValue.Var` for the bound variable name during
/// initial alias expansion (before the alias body is fully constructed). The Var is replaced by
/// `TypeValue.RecursiveRef { depth: 0 }` by `substitute_rec_ref` after the body is materialized.
/// This is the only permitted use of `TypeValue.Var` inside a Recursive body; all other callers
/// must satisfy the de Bruijn contract above.
pub fn make_typevalue_recursive(body: TypeValue) -> TypeValue {
    let payload = make_dict_typevalue_from_value(&[(FIELD_BODY, body.as_ref().clone())]);
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_RECURSIVE),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Create a TypeValue.RecursiveRef Arc<Value> for a de Bruijn depth reference within a Recursive type.
/// `depth = 0` refers to the immediately enclosing TypeValue.Recursive binder.
#[cfg(test)]
pub fn make_typevalue_recursive_ref(depth: i64) -> TypeValue {
    use crate::value::HashableValue;
    let mut entries = indexmap::IndexMap::new();
    entries.insert(
        HashableValue::Str(Arc::from(FIELD_DEPTH)),
        Arc::new(crate::value::Thunk::value(
            crate::value::Value::Int {
                n: depth,
                type_val: crate::value::unknown_type_val(),
            },
            crate::rust_span!(),
        )),
    );
    let payload = crate::value::Value::Dict {
        entries,
        type_val: crate::value::unknown_type_val(),
    };
    Arc::new(crate::value::Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_RECURSIVE_REF),
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
    })
}

/// Normalize a union: flatten nested unions and deduplicate.
/// If members is empty, returns TypeValue.Never.
/// If members has one element, returns that element.
/// Otherwise wraps in TypeValue.Union.
pub fn typevalue_normalize_union(mut members: Vec<TypeValue>) -> TypeValue {
    // Flatten nested unions
    let mut flat: Vec<TypeValue> = Vec::with_capacity(members.len());
    while let Some(m) = members.pop() {
        if let Some(TV_UNION) = typevalue_ctor(&m) {
            // Flatten — extract members from payload dict
            if let Some(inner) = typevalue_extract_list_members(&m) {
                for im in inner {
                    flat.push(im);
                }
            } else {
                flat.push(m);
            }
        } else {
            flat.push(m);
        }
    }
    flat.reverse();
    // Deduplicate by structural equality (reflexivity check via ptr_eq, then typevalue_eq for unit
    // variants, TypeValue.Var by name, TypeValue.Repr by repr string).
    let mut deduped: Vec<TypeValue> = Vec::new();
    for tv in flat {
        if !deduped.iter().any(|existing| typevalue_eq(existing, &tv)) {
            deduped.push(tv);
        }
    }
    match deduped.len() {
        0 => make_typevalue_never(),
        1 => deduped.into_iter().next().unwrap(),
        _ => make_typevalue_union(deduped),
    }
}

/// Normalize an intersection: flatten nested intersections and deduplicate.
/// If members is empty, returns TypeValue.Top.
/// If members has one element, returns that element.
/// Otherwise wraps in TypeValue.Inter.
pub fn typevalue_normalize_intersection(mut members: Vec<TypeValue>) -> TypeValue {
    let mut flat: Vec<TypeValue> = Vec::with_capacity(members.len());
    while let Some(m) = members.pop() {
        if let Some(TV_INTER) = typevalue_ctor(&m) {
            if let Some(inner) = typevalue_extract_list_members(&m) {
                for im in inner {
                    flat.push(im);
                }
            } else {
                flat.push(m);
            }
        } else {
            flat.push(m);
        }
    }
    flat.reverse();
    // Deduplicate by structural equality (reflexivity check via ptr_eq, then typevalue_eq for unit
    // variants, TypeValue.Var by name, TypeValue.Repr by repr string).
    let mut deduped: Vec<TypeValue> = Vec::new();
    for tv in flat {
        if !deduped.iter().any(|existing| typevalue_eq(existing, &tv)) {
            deduped.push(tv);
        }
    }
    match deduped.len() {
        0 => make_typevalue_top(),
        1 => deduped.into_iter().next().unwrap(),
        _ => make_typevalue_intersection(deduped),
    }
}

/// Extract field name → TypeValue pairs from a TypeValue.Record payload.
/// Returns an empty IndexMap if the payload cannot be read.
pub fn typevalue_record_fields_pub(tv: &TypeValue) -> indexmap::IndexMap<String, TypeValue> {
    use crate::value::HashableValue;
    let mut result = indexmap::IndexMap::new();
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => thunk,
        _ => return result,
    };
    let fields_thunk = match payload_thunk.peek_result() {
        Some(Ok(crate::value::Value::Dict { entries, .. })) => {
            let key = HashableValue::Str(Arc::from(FIELD_FIELDS));
            match entries.get(&key) {
                Some(t) => t.clone(),
                None => return result,
            }
        }
        _ => return result,
    };
    match fields_thunk.peek_result() {
        Some(Ok(crate::value::Value::Dict { entries, .. })) => {
            for (k, v_thunk) in entries.iter() {
                if let HashableValue::Str(name) = k {
                    if let Some(Ok(val)) = v_thunk.peek_result() {
                        result.insert(name.as_ref().to_string(), Arc::new(val.clone()));
                    }
                }
            }
        }
        _ => {}
    }
    result
}

/// Extract the params and return TypeValues from a TypeValue.Fn payload.
/// Returns `None` if the TypeValue is not a settled TypeValue.Fn.
/// The params are returned as `Vec<TypeValue>` (positional, in dict key order).
pub fn typevalue_fn_params_and_ret(tv: &TypeValue) -> Option<(Vec<TypeValue>, TypeValue)> {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return None,
    };
    match payload_thunk.peek_result()? {
        Ok(crate::value::Value::Dict { entries, .. }) => {
            let params_key = HashableValue::Str(Arc::from(FIELD_PARAMS));
            let ret_key = HashableValue::Str(Arc::from(FIELD_RETURN));
            let params_thunk = entries.get(&params_key)?;
            let ret_thunk = entries.get(&ret_key)?;
            let params: Vec<TypeValue> = match params_thunk.peek_result()? {
                Ok(crate::value::Value::Dict {
                    entries: params_entries,
                    ..
                }) => {
                    let mut result = Vec::new();
                    for i in 0..params_entries.len() {
                        let key = HashableValue::Int(i as i64);
                        if let Some(p_thunk) = params_entries.get(&key) {
                            if let Some(Ok(pv)) = p_thunk.peek_result() {
                                result.push(Arc::new(pv.clone()));
                            }
                        }
                    }
                    result
                }
                _ => Vec::new(),
            };
            let ret: TypeValue = match ret_thunk.peek_result()? {
                Ok(rv) => Arc::new(rv.clone()),
                _ => return None,
            };
            Some((params, ret))
        }
        _ => None,
    }
}

/// Public version of `typevalue_extract_list_members` for use by typecheck_cek.rs.
/// Extracts the members list from a TypeValue.Union or TypeValue.Inter payload.
pub fn typevalue_extract_members_pub(tv: &TypeValue) -> Option<Vec<TypeValue>> {
    typevalue_extract_list_members(tv)
}

/// Extract optional param names from a TypeValue.Fn payload.
///
/// TypeValue.Fn stores param names under the "param-names" key (a Dict with Int keys
/// mapping to String values). Only named params have entries; unnamed params are absent.
/// Returns a Vec of length `count` where each position is `Some(name)` for named params
/// and `None` for unnamed params. Returns a vec of `None`s of length `count` if the
/// "param-names" key is absent (TypeValue.Fn without name storage).
///
/// The `count` parameter must equal the number of positional params extracted from the
/// same TypeValue.Fn — callers derive it from `typevalue_fn_params_and_ret`.
pub fn typevalue_fn_param_names(tv: &TypeValue, count: usize) -> Vec<Option<String>> {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return vec![None; count],
    };
    let payload_val = match payload_thunk.peek_result() {
        Some(Ok(v)) => v,
        _ => return vec![None; count],
    };
    let entries = match payload_val {
        crate::value::Value::Dict { entries, .. } => entries,
        _ => return vec![None; count],
    };
    let names_key = HashableValue::Str(Arc::from(FIELD_PARAM_NAMES));
    let names_thunk = match entries.get(&names_key) {
        Some(t) => t,
        None => return vec![None; count],
    };
    let names_entries = match names_thunk.peek_result() {
        Some(Ok(crate::value::Value::Dict { entries, .. })) => entries,
        _ => return vec![None; count],
    };
    // Iterate all indices up to `count`, returning None for absent (unnamed) params.
    // We do NOT stop at the first gap — unnamed params may appear at any position.
    (0..count as i64)
        .map(|i| {
            let key = HashableValue::Int(i);
            match names_entries.get(&key) {
                Some(thunk) => match thunk.peek_result() {
                    Some(Ok(crate::value::Value::String {
                        source, start, end, ..
                    })) => Some(source[*start..*end].to_string()),
                    _ => None,
                },
                None => None,
            }
        })
        .collect()
}

/// Check if a TypeValue.Fn is variadic (has the "variadic" field set to "true").
///
/// Returns false if the "variadic" key is absent (non-variadic function) or if the
/// TypeValue is not a TypeValue.Fn.
pub fn typevalue_fn_is_variadic(tv: &TypeValue) -> bool {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return false,
    };
    let payload_val = match payload_thunk.peek_result() {
        Some(Ok(v)) => v,
        _ => return false,
    };
    let entries = match payload_val {
        crate::value::Value::Dict { entries, .. } => entries,
        _ => return false,
    };
    let key = HashableValue::Str(Arc::from(FIELD_VARIADIC));
    match entries.get(&key).and_then(|t| t.peek_result()) {
        Some(Ok(crate::value::Value::Variant { ctor, .. })) => ctor.as_ref() == BOOL_TRUE,
        _ => false,
    }
}

/// Extract typed variadic buckets from a TypeValue.Fn payload.
///
/// Returns the typed variadic buckets stored under the `FIELD_TYPED_VARIADICS` key,
/// in declaration order. Each bucket is `(name, ty)` where `name` is the param name
/// (empty string if anonymous) and `ty` is the Seq[T] element type.
///
/// Returns an empty Vec if the TypeValue is not a TypeValue.Fn, the payload is
/// malformed, or no typed-variadics key is present (non-variadic function or a
/// variadic function declared without typed buckets).
pub fn typevalue_fn_typed_variadics(tv: &TypeValue) -> Vec<(String, TypeValue)> {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return Vec::new(),
    };
    let payload_val = match payload_thunk.peek_result() {
        Some(Ok(v)) => v,
        _ => return Vec::new(),
    };
    let outer_entries = match payload_val {
        crate::value::Value::Dict { entries, .. } => entries,
        _ => return Vec::new(),
    };
    let tv_key = HashableValue::Str(Arc::from(FIELD_TYPED_VARIADICS));
    let tv_thunk = match outer_entries.get(&tv_key) {
        Some(t) => t,
        None => return Vec::new(), // absent = no typed variadics
    };
    let tv_entries = match tv_thunk.peek_result() {
        Some(Ok(crate::value::Value::Dict { entries, .. })) => entries,
        _ => return Vec::new(),
    };
    // Collect by integer index in ascending order.
    let mut indexed: Vec<(i64, (String, TypeValue))> = Vec::new();
    for (k, bucket_thunk) in tv_entries.iter() {
        if let HashableValue::Int(idx) = k {
            if let Some(Ok(crate::value::Value::Dict {
                entries: bucket_entries,
                ..
            })) = bucket_thunk.peek_result()
            {
                // Extract name: String
                let name_key = HashableValue::Str(Arc::from(FIELD_NAME));
                let name = match bucket_entries.get(&name_key).and_then(|t| t.peek_result()) {
                    Some(Ok(crate::value::Value::String {
                        source, start, end, ..
                    })) => source[*start..*end].to_string(),
                    _ => String::new(),
                };
                // Extract ty: TypeValue (stored under FIELD_OF)
                let of_key = HashableValue::Str(Arc::from(FIELD_OF));
                if let Some(ty_thunk) = bucket_entries.get(&of_key) {
                    if let Some(Ok(ty_val)) = ty_thunk.peek_result() {
                        indexed.push((*idx, (name, Arc::new(ty_val.clone()))));
                    }
                }
            }
        }
    }
    indexed.sort_by_key(|(idx, _)| *idx);
    indexed.into_iter().map(|(_, bucket)| bucket).collect()
}

/// Extract the required parameter count from a TypeValue.Fn payload.
///
/// Returns the number of required (non-default) fixed parameters. If the "required" key
/// is absent, all params are required (returns params.len()).
///
/// Returns `None` if the TypeValue is not a settled TypeValue.Fn.
pub fn typevalue_fn_required_count(tv: &TypeValue) -> Option<usize> {
    use crate::value::HashableValue;
    let payload_thunk = match tv.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_FN => thunk,
        _ => return None,
    };
    let payload_val = match payload_thunk.peek_result() {
        Some(Ok(v)) => v,
        _ => return None,
    };
    let entries = match payload_val {
        crate::value::Value::Dict { entries, .. } => entries,
        _ => return None,
    };

    // Check if "required" field is present.
    let req_key = HashableValue::Str(Arc::from(FIELD_REQUIRED));
    if let Some(req_thunk) = entries.get(&req_key) {
        if let Some(Ok(crate::value::Value::Int { n, .. })) = req_thunk.peek_result() {
            return Some(*n as usize);
        }
    }

    // "required" field absent: all params are required. Count params.
    let params_key = HashableValue::Str(Arc::from(FIELD_PARAMS));
    if let Some(params_thunk) = entries.get(&params_key) {
        if let Some(Ok(crate::value::Value::Dict {
            entries: params_entries,
            ..
        })) = params_thunk.peek_result()
        {
            return Some(params_entries.len());
        }
    }

    None
}

/// Extract list members from a TypeValue.Union or TypeValue.Inter payload.
/// Returns None if the payload is not a settled dict with integer-keyed entries.
fn typevalue_extract_list_members(tv: &TypeValue) -> Option<Vec<TypeValue>> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => {
            match thunk.peek_result()? {
                Ok(crate::value::Value::Dict { entries, .. }) => {
                    // Look for "members" key
                    let members_key = HashableValue::Str(Arc::from(FIELD_MEMBERS));
                    let members_thunk = entries.get(&members_key)?;
                    match members_thunk.peek_result()? {
                        Ok(crate::value::Value::Dict {
                            entries: members_entries,
                            ..
                        }) => {
                            // Mirror bas.rs::payload_members: sort by integer key to
                            // guarantee numeric order regardless of IndexMap insertion order.
                            let mut indexed: Vec<(i64, Arc<crate::value::Value>)> = Vec::new();
                            for (k, v_thunk) in members_entries.iter() {
                                if let HashableValue::Int(idx) = k {
                                    if let Some(Ok(v)) = v_thunk.peek_result() {
                                        indexed.push((*idx, Arc::new(v.clone())));
                                    }
                                }
                            }
                            indexed.sort_by_key(|(i, _)| *i);
                            Some(indexed.into_iter().map(|(_, v)| v).collect())
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

// ── Private Value construction helpers ───────────────────────────────────────

/// Construct a `Value::Dict` from a slice of `(key, Value)` pairs where Values are already built.
fn make_dict_typevalue_from_value(fields: &[(&str, crate::value::Value)]) -> crate::value::Value {
    use crate::value::HashableValue;
    let mut entries = indexmap::IndexMap::new();
    for (key, val) in fields {
        entries.insert(
            HashableValue::Str(Arc::from(*key)),
            Arc::new(crate::value::Thunk::value(val.clone(), crate::rust_span!())),
        );
    }
    crate::value::Value::Dict {
        entries,
        type_val: crate::value::unknown_type_val(),
    }
}

/// Construct a `Value::String` from a Rust `&str`.
fn make_string_typevalue(s: &str) -> crate::value::Value {
    crate::value::Value::String {
        source: Arc::from(s),
        start: 0,
        end: s.len(),
        type_val: crate::value::unknown_type_val(),
    }
}

/// Construct an empty `Value::Dict` for use as a TypeValue field.
fn make_empty_dict_typevalue() -> crate::value::Value {
    crate::value::Value::Dict {
        entries: indexmap::IndexMap::new(),
        type_val: crate::value::unknown_type_val(),
    }
}

/// Construct a `Value::Dict` from a slice of `(key, value)` pairs.
///
/// Keys are string keys; values are already-constructed `Value` objects that will be
/// wrapped in settled thunks.
fn make_dict_typevalue(fields: &[(&str, crate::value::Value)]) -> crate::value::Value {
    use crate::value::HashableValue;
    let mut entries = indexmap::IndexMap::new();
    for (key, val) in fields {
        entries.insert(
            HashableValue::Str(Arc::from(*key)),
            Arc::new(crate::value::Thunk::value(val.clone(), crate::rust_span!())),
        );
    }
    crate::value::Value::Dict {
        entries,
        type_val: crate::value::unknown_type_val(),
    }
}

// ── TypeValue inspection helpers ─────────────────────────────────────────────

/// Check if an `Arc<Value>` is a `TypeValue.Var` and return its name.
///
/// Returns `None` if the value is not a TypeValue.Var or if the name payload
/// cannot be read synchronously (thunk not yet settled).
pub fn typevalue_var_name(v: &TypeValue) -> Option<String> {
    match v.as_ref() {
        crate::value::Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_VAR => {
            // Inspect the settled payload dict to extract `name`.
            // peek_result returns Option<Result<&Value, &Arc<EvalError>>>.
            match thunk.peek_result() {
                Some(Ok(crate::value::Value::Dict { entries, .. })) => {
                    let key = crate::value::HashableValue::Str(Arc::from(FIELD_NAME));
                    let name_thunk = entries.get(&key)?;
                    match name_thunk.peek_result()? {
                        Ok(crate::value::Value::String {
                            source, start, end, ..
                        }) => Some(source[*start..*end].to_string()),
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Extract a single named field from a TypeValue variant's payload dict.
///
/// Returns `None` if the TypeValue has no payload, if the payload is not a settled dict,
/// or if the named field is absent.
pub fn typevalue_payload_field(tv: &TypeValue, field: &str) -> Option<TypeValue> {
    match tv.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(field));
                let field_thunk = entries.get(&key)?;
                match field_thunk.peek_result()? {
                    Ok(v) => Some(Arc::new(v.clone())),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Check if a TypeValue is a TypeValue.Op (type operator/constructor name).
pub fn typevalue_is_op(v: &TypeValue) -> bool {
    typevalue_ctor(v) == Some(TV_OP)
}

/// Return the constructor tag string of a TypeValue.
///
/// Returns `None` if the value is not a `Value::Variant`.
/// Delegates to `crate::type_class::typevalue_ctor` — canonical implementation lives there.
pub fn typevalue_ctor(v: &TypeValue) -> Option<&str> {
    crate::type_class::typevalue_ctor(v)
}

/// Extract the `name` string from a TypeValue.Op payload.
/// Returns None if not a TypeValue.Op or payload cannot be read.
pub fn typevalue_op_name(v: &TypeValue) -> Option<String> {
    if typevalue_ctor(v) != Some(TV_OP) {
        return None;
    }
    match v.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let key = crate::value::HashableValue::Str(Arc::from(FIELD_NAME));
                let name_thunk = entries.get(&key)?;
                match name_thunk.peek_result()? {
                    Ok(crate::value::Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract (tycon, ctor) strings from a TypeValue.NominalVariant payload.
/// Returns None if not a NominalVariant or payload unreadable.
pub fn typevalue_nominal_variant_tag(v: &TypeValue) -> Option<(String, String)> {
    if typevalue_ctor(v) != Some(TV_NOMINAL_VARIANT) {
        return None;
    }
    match v.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result()? {
            Ok(crate::value::Value::Dict { entries, .. }) => {
                let tycon_key = crate::value::HashableValue::Str(Arc::from(FIELD_TYCON));
                let ctor_key = crate::value::HashableValue::Str(Arc::from(FIELD_CTOR));
                // Use ? to propagate None (missing or non-string field) instead of
                // silently defaulting to empty string.
                let tycon_str = entries.get(&tycon_key).and_then(|t| {
                    match t.peek_result()? {
                        Ok(crate::value::Value::String {
                            source, start, end, ..
                        }) => Some(source[*start..*end].to_string()),
                        Ok(_) => None, // unexpected type
                        Err(e) => panic!(
                            "invariant: TypeValue.NominalVariant tycon thunk has error: {}",
                            e
                        ),
                    }
                })?;
                let ctor_str = entries.get(&ctor_key).and_then(|t| {
                    match t.peek_result()? {
                        Ok(crate::value::Value::String {
                            source, start, end, ..
                        }) => Some(source[*start..*end].to_string()),
                        Ok(_) => None, // unexpected type
                        Err(e) => panic!(
                            "invariant: TypeValue.NominalVariant ctor thunk has error: {}",
                            e
                        ),
                    }
                })?;
                Some((tycon_str, ctor_str))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Check if a TypeValue.NominalVariant has a non-empty fields record.
pub fn typevalue_nominal_variant_has_fields(v: &TypeValue) -> bool {
    if typevalue_ctor(v) != Some(TV_NOMINAL_VARIANT) {
        return false;
    }
    match v.as_ref() {
        crate::value::Value::Variant {
            payload: Some(thunk),
            ..
        } => match thunk.peek_result() {
            Some(Ok(crate::value::Value::Dict { entries, .. })) => {
                let fields_key = crate::value::HashableValue::Str(Arc::from(FIELD_FIELDS));
                if let Some(fields_thunk) = entries.get(&fields_key) {
                    match fields_thunk.peek_result() {
                        Some(Ok(crate::value::Value::Dict {
                            entries: f_entries, ..
                        })) => !f_entries.is_empty(),
                        _ => false,
                    }
                } else {
                    false
                }
            }
            _ => false,
        },
        _ => false,
    }
}

/// Check structural equality of two TypeValues.
///
/// Reflexivity: pointer-identical TypeValues are definitionally equal. For unit
/// variants (no payload), equality is determined by constructor tag. For `TypeValue.Var`,
/// equality is by name. For TypeValue.Repr, equality is by repr string.
/// For all other variants, this is conservative: falls back to pointer equality
/// (no thunk forcing — structural walking would require async).
///
/// Available in production (not test-only) so normalize_union/intersection can use it
/// for structural deduplication.
pub(crate) fn typevalue_eq(a: &TypeValue, b: &TypeValue) -> bool {
    // Reflexivity: pointer-identical TypeValues are definitionally equal.
    if Arc::ptr_eq(a, b) {
        return true;
    }
    match (a.as_ref(), b.as_ref()) {
        // Unit variants: no payload — equality by constructor tag.
        (
            crate::value::Value::Variant {
                ctor: ca,
                payload: None,
                ..
            },
            crate::value::Value::Variant {
                ctor: cb,
                payload: None,
                ..
            },
        ) => ca.as_ref() == cb.as_ref(),
        // TypeValue.Var: equality by name.
        (
            crate::value::Value::Variant {
                ctor: ca,
                payload: Some(_),
                ..
            },
            crate::value::Value::Variant {
                ctor: cb,
                payload: Some(_),
                ..
            },
        ) if ca.as_ref() == TV_VAR && cb.as_ref() == TV_VAR => {
            typevalue_var_name(a) == typevalue_var_name(b)
        }
        // TypeValue.Repr: equality by repr string.
        (
            crate::value::Value::Variant {
                ctor: ca,
                payload: Some(pa),
                ..
            },
            crate::value::Value::Variant {
                ctor: cb,
                payload: Some(pb),
                ..
            },
        ) if ca.as_ref() == TV_REPR && cb.as_ref() == TV_REPR => {
            // Extract repr string from each payload dict.
            let repr_str = |thunk: &crate::value::Thunk| -> Option<String> {
                if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                    let key = crate::value::HashableValue::Str(Arc::from(FIELD_REPR));
                    if let Some(Ok(crate::value::Value::String {
                        source, start, end, ..
                    })) = entries.get(&key).and_then(|t| t.peek_result())
                    {
                        return Some(source[*start..*end].to_string());
                    }
                }
                None
            };
            repr_str(pa) == repr_str(pb)
        }
        // Conservative: all other variants use pointer equality (already checked above).
        _ => false,
    }
}

// ── InferenceContext ─────────────────────────────────────────────────────────

/// Monotonic inference context for TypeValue-based type inference (S-1003 migration).
///
/// This is the replacement for `Substitution` (TypeVar → TypeValue bindings) and the
/// `levels` field in `InferState` (TypeVar creation-time levels). It is designed to
/// coexist with the Type-enum-based `InferState` during the incremental migration.
///
/// ## Monotonicity invariant
/// `bind()` enforces that each TypeVar is bound at most once — bindings are monotonic.
/// Once a TypeVar is bound, its binding never changes. This is the Robinson unification
/// invariant: each unification step either fails (occurs check) or adds exactly one new binding.
///
/// ## Level semantics (Kiselyov 2013)
/// TypeValue.Var does NOT carry a level (unlike `Type::Var(name, level)`). Levels live
/// in `self.levels: HashMap<VarName, u32>`. Level lowering mutates context entries rather
/// than TypeValues — Arc<Value> is stable and immutable. This is the key architectural
/// difference from the old Type-enum approach.
///
/// ## Access control
/// - Static type-checker holds `&mut InferenceContext` during inference passes (mutable).
/// - At runtime (read-only), wrap in `Arc<InferenceContext>` to prevent mutation.
#[derive(Debug, Clone)]
pub struct InferenceContext {
    /// Current binding level. Increased when entering a let-binding's RHS, decreased
    /// when leaving. Used by `fresh_typevar()` to set the creation-time level.
    pub current_level: u32,
    /// TypeVar name → creation-time level. This is the authoritative source for level
    /// lookups. Mutated by level lowering, NOT by TypeValue objects themselves.
    pub levels: HashMap<String, u32>,
    /// TypeVar name → TypeValue binding. Monotonic: each name appears at most once.
    /// Query via `lookup(name)`. Bind via `bind(name, ty)`.
    /// Restricted to `pub(crate)` to enforce the monotonicity invariant — all insertions
    /// must go through `bind()`, which checks for double-binding. Direct field assignment
    /// is only permitted via `restore_subst()`, which is a whole-substitution rollback
    /// (save/restore pattern, not a monotonic insertion).
    pub(crate) subst: HashMap<String, TypeValue>,
    /// Monotonic counter for fresh TypeVar name generation.
    /// Incremented by `fresh_typevar()`. Never decremented.
    gensym_counter: u64,
    /// Type constructor environment: name → TyConDef.
    /// Used by BAS subtyping to look up variance annotations for type constructor applications.
    /// Optional: callers that don't need TyCon variance can leave this empty.
    pub tycon_env: HashMap<String, Arc<crate::type_def::TyConDef>>,
    /// Deferred equality pairs for non-injective resolver FDs.
    ///
    /// Added by `unify()` when two `TV_APP(F, ...)` nodes with non-injective F are compared.
    /// Equal outputs don't imply equal inputs for non-injective F, so pairwise unification
    /// of args is unsound. Instead the pair is queued here.
    ///
    /// Drained by `run_fd_improvement_fixpoint` after each constraint push:
    /// - Apply substitution to both sides.
    /// - If both are ground (no free TypeVars), unify them directly.
    /// - Otherwise, put back (retry next fixpoint iteration).
    pub resolver_deferred: Vec<(Arc<crate::value::Value>, Arc<crate::value::Value>)>,
    /// Directional lower bounds accumulated by `constrain(sub, α)`.
    ///
    /// When `constrain(sub, α)` is called and α is a free TypeVar, `sub` is pushed here
    /// rather than binding α = sub via equality. This preserves directionality:
    /// "sub <: α" means α must be AT LEAST as general as sub. Multiple lower bounds from
    /// different call sites widen α rather than conflict.
    ///
    /// At bound resolution time (generalization or explicit solve), α is bound to the
    /// JOIN of all its lower bounds (computed via `typevalue_normalize_union`). This is
    /// sound because JOIN(lbs) is the least type that all lower bounds are subtypes of.
    ///
    /// When `constrain(α, sup)` fires (α is the sub), α is bound to `sup` as an upper
    /// bound. Any existing lower bounds for α are checked: each `lb <: sup` must hold.
    pub lower_bounds: HashMap<String, Vec<TypeValue>>,
    /// Directional upper bounds accumulated by `constrain(α, sup)`.
    ///
    /// When `constrain(α, sup)` is called and α is a free TypeVar, `sup` is pushed here
    /// rather than binding α = sup via equality. This preserves directionality:
    /// "α <: sup" means α must be AT MOST as general as sup. Multiple upper bounds from
    /// different call sites narrow α rather than conflict.
    ///
    /// At bound resolution time, α is bound eagerly when exactly one upper bound exists,
    /// after verifying all accumulated lower bounds are subtypes of that upper bound (B-705).
    /// When multiple upper bounds exist, they accumulate here until the TypeVar is bound
    /// from a lower-bound constraint that checks each.
    pub upper_bounds: HashMap<String, Vec<TypeValue>>,
    /// Directional lower bounds accumulated for RowVars by `constrain_record()`.
    ///
    /// When a RowVar ρ appears as the sup tail in `constrain_record(sub_row, sup_row)`,
    /// the sub tail is pushed here as a lower bound for ρ. This preserves directionality:
    /// "sub_row <: {... ρ}" means ρ must accommodate at least the sub tail. Multiple lower
    /// bounds from different call sites widen ρ rather than conflict.
    ///
    /// When exactly one lower bound exists, ρ is bound eagerly to that bound. When multiple
    /// lower bounds exist, they accumulate here until resolved from a subsequent binding site.
    pub row_lower_bounds: HashMap<String, Vec<TypeValue>>,
    /// TypeVars declared via `bind:` annotations (explicit polymorphic type parameters).
    /// These are protected from `lower_var_level` calls triggered by `constrain(Unknown, α)`.
    /// Without protection, Unknown flowing into a `bind:` TypeVar lowers its level to 0,
    /// preventing generalization even though the TypeVar is explicitly declared as a
    /// polymorphic type parameter (B-681 root cause).
    pub protected_vars: HashSet<String>,
    /// Resolver names whose associated class has `resolver_injective = false`.
    ///
    /// Populated by `infer_class_decl_from_surface` when a ClassDecl with a non-injective
    /// resolver is inserted into the environment. Used by `unify()` in the TV_APP~TV_APP
    /// arm: when two `App(F, _)` types with the same op name `F` are compared, injectivity
    /// determines whether arguments can be unified pairwise.
    ///
    /// - **Injective F**: `unify(arg_a, arg_b)` is sound — equal results imply equal args.
    /// - **Non-injective F**: `F(a) == F(b)` does not imply `a == b`. Args are pushed to
    ///   `resolver_deferred` instead, where they are retried once both sides are ground.
    pub non_injective_resolvers: HashSet<String>,
}

impl InferenceContext {
    /// Create a new empty InferenceContext at level 0.
    pub fn new() -> Self {
        Self {
            current_level: 0,
            levels: HashMap::new(),
            subst: HashMap::new(),
            gensym_counter: 0,
            tycon_env: HashMap::new(),
            resolver_deferred: Vec::new(),
            lower_bounds: HashMap::new(),
            upper_bounds: HashMap::new(),
            row_lower_bounds: HashMap::new(),
            protected_vars: HashSet::new(),
            non_injective_resolvers: HashSet::new(),
        }
    }

    /// Create an InferenceContext seeded with a TyConEnv.
    ///
    /// Used by callers (e.g., eval.rs) that need BAS subtyping with variance information
    /// but do not have an active inference session (no live TypeVars or substitution).
    pub fn with_tycon_env(tycon_env: HashMap<String, Arc<crate::type_def::TyConDef>>) -> Self {
        Self {
            current_level: 0,
            levels: HashMap::new(),
            subst: HashMap::new(),
            gensym_counter: 0,
            tycon_env,
            resolver_deferred: Vec::new(),
            lower_bounds: HashMap::new(),
            upper_bounds: HashMap::new(),
            row_lower_bounds: HashMap::new(),
            protected_vars: HashSet::new(),
            non_injective_resolvers: HashSet::new(),
        }
    }

    /// Create an InferenceContext from explicit substitution/level/tycon_env components.
    ///
    /// Used by callers that need a snapshot of a live inference session's state for
    /// a read-only check (e.g., BAS subtyping inside typecheck_cek.rs). The `gensym_counter`
    /// is initialized to 0 — callers using this context for read-only checks never call
    /// `fresh_typevar()`, so the counter value is irrelevant.
    pub fn from_snapshot(
        subst: HashMap<String, TypeValue>,
        levels: HashMap<String, u32>,
        current_level: u32,
        tycon_env: HashMap<String, Arc<crate::type_def::TyConDef>>,
    ) -> Self {
        Self {
            current_level,
            levels,
            subst,
            gensym_counter: 0,
            tycon_env,
            resolver_deferred: Vec::new(),
            lower_bounds: HashMap::new(),
            upper_bounds: HashMap::new(),
            row_lower_bounds: HashMap::new(),
            protected_vars: HashSet::new(),
            non_injective_resolvers: HashSet::new(),
        }
    }

    /// Bind a TypeVar name to a TypeValue.
    ///
    /// Enforces monotonicity: returns `Err` if the variable is already bound.
    /// Callers discovering a conflict during unification should emit a type error
    /// diagnostic rather than overwriting.
    pub fn bind(
        &mut self,
        name: String,
        val: TypeValue,
    ) -> Result<(), crate::error::TypeDiagnostic> {
        if self.subst.contains_key(&name) {
            return Err(crate::error::TypeDiagnostic::error(
                "inference-internal",
                format!(
                    "type variable '{}' bound twice — monotonicity invariant violated",
                    name
                ),
                crate::ast::Span::rust_source(file!(), line!()),
            ));
        }
        self.subst.insert(name, val);
        Ok(())
    }

    /// Create a fresh TypeValue.Var at the current level.
    ///
    /// The level is recorded in `self.levels` before the TypeValue is returned.
    /// `prefix` is a human-readable prefix used in diagnostic messages.
    pub fn fresh_typevar(&mut self, prefix: &str) -> TypeValue {
        let name = format!("{}__{}", prefix, self.gensym_counter);
        self.gensym_counter += 1;
        // Register the creation-time level BEFORE returning the TypeValue.
        self.levels.insert(name.clone(), self.current_level);
        make_typevar_value(&name)
    }

    /// Lower the level of a TypeVar to at most `max_level`.
    ///
    /// Used by the Kiselyov (2013) level-lowering algorithm: when TypeVar α at level ℓα
    /// is unified with TypeVar β at level ℓβ where ℓβ < ℓα, α must be lowered to ℓβ to
    /// prevent unsound generalization. This mutates `self.levels` — TypeValue.Var is stable.
    pub fn lower_var_level(&mut self, name: &str, max_level: u32) {
        if self.protected_vars.contains(name) {
            return;
        }
        if let Some(level) = self.levels.get_mut(name) {
            if *level > max_level {
                *level = max_level;
            }
        }
    }

    /// Get the level of a TypeVar, or 0 if not registered.
    ///
    /// Level 0 is the safe default for unregistered names (e.g., μ-binder variables in
    /// recursive types, which are not inference TypeVars).
    pub fn get_level(&self, name: &str) -> u32 {
        self.levels.get(name).copied().unwrap_or(0)
    }

    /// Record a lower bound for a free TypeVar.
    ///
    /// Called by `constrain(sub, α)` when α is free. Instead of binding α = sub via
    /// equality (which would be unsound for directional subtype constraints), the lower
    /// bound is accumulated here. Multiple lower bounds are widened at solve time via
    /// `resolve_lower_bounds()`.
    pub fn add_lower_bound(&mut self, name: &str, ty: TypeValue) {
        self.lower_bounds
            .entry(name.to_string())
            .or_default()
            .push(ty);
    }

    /// Drain and return all accumulated lower bounds for a TypeVar.
    ///
    /// Used when binding α from an upper-bound constraint (`constrain(α, sup)`): the
    /// caller verifies each lower bound satisfies `lb <: sup`. Also used by
    /// `resolve_lower_bounds()` to compute the JOIN.
    pub fn take_lower_bounds(&mut self, name: &str) -> Vec<TypeValue> {
        match self.lower_bounds.remove(name) {
            Some(v) => v,
            None => vec![], // Key absent means no bounds were ever accumulated.
        }
    }

    /// Record an upper bound for a free TypeVar.
    ///
    /// Called by `constrain(α, sup)` when α is free. Instead of binding α = sup via
    /// equality (which would prevent accumulating multiple upper bounds), the upper
    /// bound is accumulated here. When exactly one upper bound exists, `constrain`
    /// binds α eagerly after verifying all lower bounds. Multiple upper bounds accumulate
    /// and are checked against lower bounds at the binding site.
    pub fn add_upper_bound(&mut self, name: &str, ty: TypeValue) {
        self.upper_bounds
            .entry(name.to_string())
            .or_default()
            .push(ty);
    }

    /// Drain and return all accumulated upper bounds for a TypeVar.
    ///
    /// Used when binding α from a lower-bound constraint (`constrain(sub, α)`): the
    /// caller verifies each upper bound satisfies `sub <: ub`.
    pub fn take_upper_bounds(&mut self, name: &str) -> Vec<TypeValue> {
        match self.upper_bounds.remove(name) {
            Some(v) => v,
            None => vec![], // Key absent means no bounds were ever accumulated.
        }
    }

    /// Record a lower bound for a free RowVar.
    ///
    /// Called by `constrain_record()` when a RowVar ρ appears as the sup tail. Instead
    /// of binding ρ = sub_tail via equality, the sub tail is accumulated here. When
    /// exactly one lower bound exists, `constrain_record` binds ρ eagerly. Multiple
    /// lower bounds accumulate here until resolved from a subsequent binding site.
    pub fn add_row_lower_bound(&mut self, name: &str, tail: TypeValue) {
        self.row_lower_bounds
            .entry(name.to_string())
            .or_default()
            .push(tail);
    }

    /// Drain and return all accumulated lower bounds for a RowVar.
    pub fn take_row_lower_bounds(&mut self, name: &str) -> Vec<TypeValue> {
        match self.row_lower_bounds.remove(name) {
            Some(v) => v,
            None => vec![], // Key absent means no bounds were ever accumulated.
        }
    }

    /// Walk a TypeValue and apply the current substitution, resolving bound TypeVars.
    ///
    /// Follows TypeVar binding chains to fixpoint (cycle-safe via a visited set).
    /// Also recursively substitutes into TypeValue.Record's RowTail.Uniform payload
    /// (the value-type and key-type fields), since those payloads are created with
    /// `Thunk::value(...)` and are synchronously peekable via `peek_result()`.
    /// Other compound TypeValue variants (Fn, Union, Inter, App) are returned as-is —
    /// TypeVar components within those are resolved when the compound type is itself
    /// unified or instantiated.
    pub fn apply_subst(&self, ty: &TypeValue) -> TypeValue {
        self.apply_subst_inner(ty, &mut HashSet::new())
    }

    fn apply_subst_inner(&self, ty: &TypeValue, visited: &mut HashSet<String>) -> TypeValue {
        // Follow TypeVar binding chains to fixpoint.
        if let Some(name) = typevalue_var_name(ty) {
            if visited.contains(&name) {
                // Cycle detected — return the TypeVar itself.
                return Arc::clone(ty);
            }
            if let Some(bound) = self.subst.get(&name) {
                visited.insert(name);
                return self.apply_subst_inner(bound, visited);
            }
            // Unbound TypeVar — return as-is.
            return Arc::clone(ty);
        }

        // Special case: TypeValue.Record with RowTail.Uniform tail.
        // The Uniform tail contains value-type and optional key-type fields that may
        // contain TypeVars. We must recursively apply substitution to those fields.
        if let Some(TV_RECORD) = typevalue_ctor(ty) {
            // Extract the tail field from the Record payload.
            let tail_opt = self.extract_tail_for_subst(ty);
            if let Some(tail) = tail_opt {
                if let Some(RT_UNIFORM) = typevalue_ctor(&tail) {
                    // Extract value-type and key-type from the Uniform payload.
                    let (value_type_opt, key_type_opt) = self.extract_uniform_fields(&tail);

                    // Apply substitution to value-type and key-type.
                    let new_value_type = if let Some(ref vt) = value_type_opt {
                        Some(self.apply_subst_inner(vt, visited))
                    } else {
                        None
                    };
                    let new_key_type = if let Some(ref kt) = key_type_opt {
                        Some(self.apply_subst_inner(kt, visited))
                    } else {
                        None
                    };

                    // Check if substitution changed anything.
                    let changed = match (&value_type_opt, &new_value_type) {
                        (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
                        _ => false,
                    } || match (&key_type_opt, &new_key_type) {
                        (Some(old), Some(new)) => !Arc::ptr_eq(old, new),
                        _ => false,
                    };

                    if changed {
                        // Reconstruct the Record with the substituted tail.
                        return self.reconstruct_record_with_new_tail(
                            ty,
                            new_value_type,
                            new_key_type,
                        );
                    }
                }
            }
        }

        // Binding chains for TypeVars are resolved above. Other TypeValues (Fn, Union, Inter,
        // App) are returned as-is — their TypeVar components are resolved when those compound
        // types are themselves unified or instantiated.
        Arc::clone(ty)
    }

    /// Extract the tail field from a TypeValue.Record payload.
    pub(crate) fn extract_tail_for_subst(&self, tv: &TypeValue) -> Option<TypeValue> {
        use crate::value::HashableValue;
        match tv.as_ref() {
            crate::value::Value::Variant {
                payload: Some(thunk),
                ..
            } => {
                if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                    let tail_key = HashableValue::Str(Arc::from(FIELD_TAIL));
                    if let Some(tail_thunk) = entries.get(&tail_key) {
                        if let Some(Ok(tail_val)) = tail_thunk.peek_result() {
                            return Some(Arc::new(tail_val.clone()));
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// Extract value-type and key-type fields from a RowTail.Uniform payload.
    pub(crate) fn extract_uniform_fields(
        &self,
        tail: &TypeValue,
    ) -> (Option<TypeValue>, Option<TypeValue>) {
        use crate::value::HashableValue;
        match tail.as_ref() {
            crate::value::Value::Variant {
                ctor,
                payload: Some(thunk),
                ..
            } if ctor.as_ref() == RT_UNIFORM => match thunk.peek_result() {
                Some(Ok(crate::value::Value::Dict { entries, .. })) => {
                    let value_type_key = HashableValue::Str(Arc::from(RT_FIELD_VALUE_TYPE));
                    let key_type_key = HashableValue::Str(Arc::from(RT_FIELD_KEY_TYPE));

                    let value_type = entries.get(&value_type_key).and_then(|t| {
                        t.peek_result().and_then(|r| match r {
                            Ok(v) => Some(Arc::new(v.clone())),
                            Err(e) => panic!(
                                "type field thunk in error state (invariant violation): {:?}",
                                e
                            ),
                        })
                    });

                    let key_type = entries.get(&key_type_key).and_then(|t| {
                        t.peek_result().and_then(|r| match r {
                            Ok(v) => Some(Arc::new(v.clone())),
                            Err(e) => panic!(
                                "type field thunk in error state (invariant violation): {:?}",
                                e
                            ),
                        })
                    });

                    return (value_type, key_type);
                }
                Some(Err(e)) => panic!(
                    "TypeValue thunk in error state (invariant violation): TypeValue thunks \
                         created via Thunk::value() must never be in error state: {:?}",
                    e
                ),
                _ => return (None, None),
            },
            _ => (None, None),
        }
    }

    /// Extract only the key-type field from a RowTail.Uniform payload.
    ///
    /// This is a convenience wrapper around `extract_uniform_fields` that returns only the
    /// key-type. Used by occurs checks and unification when only the key-type is needed.
    pub(crate) fn extract_uniform_key_type(&self, tail: &TypeValue) -> Option<TypeValue> {
        let (_, key_type) = self.extract_uniform_fields(tail);
        key_type
    }

    /// Reconstruct a TypeValue.Record with a new RowTail.Uniform tail.
    fn reconstruct_record_with_new_tail(
        &self,
        original_record: &TypeValue,
        new_value_type: Option<TypeValue>,
        new_key_type: Option<TypeValue>,
    ) -> TypeValue {
        use crate::value::HashableValue;

        // Extract the existing fields dict from the original Record.
        let fields_dict = match original_record.as_ref() {
            crate::value::Value::Variant {
                payload: Some(thunk),
                ..
            } => {
                if let Some(Ok(crate::value::Value::Dict { entries, .. })) = thunk.peek_result() {
                    let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                    entries.get(&fields_key).and_then(|t| {
                        t.peek_result().and_then(|r| match r {
                            Ok(v) => Some(v.clone()),
                            Err(e) => panic!(
                                "type field thunk in error state (invariant violation): {:?}",
                                e
                            ),
                        })
                    })
                } else {
                    None
                }
            }
            _ => None,
        };

        // Build new RowTail.Uniform with substituted fields.
        let new_tail = if let Some(vt) = new_value_type {
            make_rowtail_uniform_with_key_type(vt, new_key_type)
        } else {
            panic!(
                "invariant violation: new_value_type must be Some when reconstructing Uniform tail"
            )
        };

        // Reconstruct the Record with the existing fields and new tail.
        let mut payload_entries = indexmap::IndexMap::new();
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_FIELDS)),
            Arc::new(crate::value::Thunk::value(
                fields_dict.expect("invariant violation: TypeValue.Record payload must have a fields entry when reconstructing Uniform tail"),
                crate::rust_span!(),
            )),
        );
        payload_entries.insert(
            HashableValue::Str(Arc::from(FIELD_TAIL)),
            Arc::new(crate::value::Thunk::value(
                new_tail.as_ref().clone(),
                crate::rust_span!(),
            )),
        );

        let payload_dict = crate::value::Value::Dict {
            entries: payload_entries,
            type_val: crate::value::unknown_type_val(),
        };

        Arc::new(crate::value::Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(TV_RECORD),
            payload: Some(Arc::new(crate::value::Thunk::value(
                payload_dict,
                crate::rust_span!(),
            ))),
        })
    }

    /// Collect all free TypeVar names directly reachable from a TypeValue.
    ///
    /// "Free" means the TypeVar has no binding in `self.subst`. Follows binding chains
    /// for bound TypeVars. Inspects settled dict payloads recursively for TypeValue-shaped
    /// fields. Non-settled thunks are treated as opaque.
    pub fn free_vars(&self, ty: &TypeValue) -> Vec<String> {
        let mut result = Vec::new();
        let mut visited = HashSet::new();
        self.collect_free_vars_inner(ty, &mut result, &mut visited);
        result
    }

    /// Helper to collect free vars from a Value (which may be a Variant, Dict, or other).
    fn collect_free_vars_from_value(
        &self,
        v: &crate::value::Value,
        result: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        match v {
            crate::value::Value::Variant { .. } => {
                // This is a TypeValue — recurse via collect_free_vars_inner.
                let tv: TypeValue = Arc::new(v.clone());
                self.collect_free_vars_inner(&tv, result, visited);
            }
            crate::value::Value::Dict { entries, .. } => {
                // Dict may contain TypeValues or nested Dicts — recurse into all entries.
                for (_key, thunk) in entries.iter() {
                    if let Some(Ok(inner_val)) = thunk.peek_result() {
                        self.collect_free_vars_from_value(inner_val, result, visited);
                    }
                }
            }
            _ => {
                // Other values (Int, String, Bool, etc.) — no TypeVars inside.
            }
        }
    }

    fn collect_free_vars_inner(
        &self,
        ty: &TypeValue,
        result: &mut Vec<String>,
        visited: &mut HashSet<String>,
    ) {
        match ty.as_ref() {
            crate::value::Value::Variant { ctor, payload, .. } => {
                match ctor.as_ref() {
                    TV_VAR => {
                        if let Some(name) = typevalue_var_name(ty) {
                            if visited.insert(name.clone()) {
                                if let Some(bound) = self.subst.get(&name) {
                                    self.collect_free_vars_inner(bound, result, visited);
                                } else {
                                    result.push(name);
                                }
                            }
                        }
                    }
                    // Leaf/opaque variants — no TypeVar positions inside.
                    // TV_PHANTOM: zero-payload unit constructor.
                    // TV_RECURSIVE_REF: payload { depth: Integer } — no TypeVar positions.
                    // TV_ERROR: Rust-internal sentinel; never contains inference vars.
                    // TV_STAGE_APP: deferred computation sentinel; payload fields are not TypeVar containers.
                    TV_UNKNOWN | TV_NEVER | TV_TOP | TV_REPR | TV_INT_LIT | TV_FLOAT_LIT
                    | TV_STR_LIT | TV_OP | TV_PHANTOM | TV_RECURSIVE_REF | TV_ERROR
                    | TV_STAGE_APP => {}
                    _ => {
                        // Structural variants: inspect settled payload dicts recursively.
                        if let Some(thunk) = payload {
                            if let Some(Ok(crate::value::Value::Dict { entries, .. })) =
                                thunk.peek_result()
                            {
                                for (_key, val_thunk) in entries.iter() {
                                    if let Some(Ok(val)) = val_thunk.peek_result() {
                                        self.collect_free_vars_from_value(val, result, visited);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Look up a TypeVar binding by name (test helper).
    ///
    /// Returns `Some(TypeValue)` if the variable is bound in the substitution, `None` otherwise.
    /// Production code should use `apply_subst` which follows chains to fixpoint.
    #[cfg(test)]
    pub fn lookup(&self, name: &str) -> Option<TypeValue> {
        self.subst.get(name).cloned()
    }
}

impl Default for InferenceContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Return `true` if `tv` contains any free (unbound) TypeVar under `ctx`'s substitution.
///
/// Uses the same shallow walk as `InferenceContext::free_vars` — follows the top-level
/// TypeVar chain and inspects settled payload dicts one level deep.
/// A TypeValue is "ground" (concrete, no free TypeVars) when this returns `false`.
///
/// Used by `run_fd_improvement_fixpoint` to decide whether a deferred resolver equality
/// is ready to be unified: unification proceeds only when both sides are ground.
pub fn has_free_type_vars_ctx(tv: &Arc<crate::value::Value>, ctx: &InferenceContext) -> bool {
    !ctx.free_vars(tv).is_empty()
}

/// Maps expression spans `(start_offset, end_offset)` to the TypeValue of the variable
/// referenced there. Only populated for `VarRef` expressions that resolve to a polymorphic
/// scheme (TypeValue.Scheme variants). Used by LSP hover to display
/// constraints (e.g., `Equatable a => Fn@Bool [a a]`).
///
/// Stored in `InferState.scheme_map` during inference, then extracted and returned as part
/// of the type-checking result for LSP consumers.
pub type SchemeMap = HashMap<(u32, u32, u32, u32), Arc<crate::value::Value>>;

/// Bundled type-stage data: resolved TypeValues, parameterized type constructor thunks,
/// and TypeVar kind annotations. Replaces the now-deleted `TypeStageEntry` enum.
///
/// - `scope`: resolved TypeValues, one frame per type-stage document.
///   Vec[0] = innermost (highest priority); Vec[N-1] = outermost.
/// - `fns`: parameterized type constructor thunks (e.g., Seq, Result).
/// - `type_vars`: TypeVar kind annotations (e.g., Operator → "Operator", Label → "Label").
#[derive(Debug, Clone, Default)]
pub struct TypeStageData {
    pub scope: Vec<std::collections::HashMap<String, TypeValue>>,
    pub fns: std::collections::HashMap<String, std::sync::Arc<crate::value::Thunk>>,
    pub type_vars: std::collections::HashMap<String, String>,
}

impl TypeStageData {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return true if no type-stage entries exist in any frame.
    pub fn is_empty(&self) -> bool {
        self.scope.iter().all(|m| m.is_empty()) && self.fns.is_empty() && self.type_vars.is_empty()
    }

    /// Check whether a name is present in any frame.
    pub fn contains_key(&self, name: &str) -> bool {
        self.scope.iter().any(|m| m.contains_key(name))
            || self.fns.contains_key(name)
            || self.type_vars.contains_key(name)
    }

    /// Get the resolved TypeValue for a name (checking all scope frames).
    /// Does NOT check fns or type_vars — those have separate lookup methods.
    pub fn get_resolved(&self, name: &str) -> Option<&TypeValue> {
        self.scope.iter().find_map(|m| m.get(name))
    }

    /// Prepend a new scope frame (innermost wins).
    pub fn push_front(&mut self, frame: std::collections::HashMap<String, TypeValue>) {
        self.scope.insert(0, frame);
    }
}

/// Unique identity of a binding: the source span where the binding was declared
/// plus its string name. Two bindings with the same name at different source
/// locations have different def_spans — uniqueness is guaranteed by the source
/// position, which is stable across all code paths (unlike Arc frame pointers,
/// which differ between dict_env, scc_env, and new_env_inner allocations).
///
/// Used as keys in `InferState.use_def` and as the type of `InferState.current_binding`.
/// Defined here (in type_infer.rs) because `InferState` holds fields of this type —
/// the dependency direction must be: low-level infrastructure (type_infer) defines
/// the type, high-level strategy (typecheck_cek) uses it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct BindingId {
    /// Source span of the binding's declaration site.
    /// Stable across all code paths because it comes from the parsed AST.
    pub def_span: crate::ast::Span,
    pub name: String,
}

/// Inference state for levels-based let-generalization (S-1003 TypeValue migration).
///
/// After S-1003 T-2004: All type representations use `Arc<Value>` (TypeValue).
/// `TypeScheme`, `Substitution`, `Type`, and `Kind` have been deleted.
/// TypeVar substitution lives in `InferenceContext.subst`.
#[derive(Debug, Clone)]
pub struct InferState {
    /// Accumulated type class constraints on type variables.
    /// Constraints are `Arc<Value>` ConstraintDecl variants (see type_class.rs for construction helpers).
    /// Generated when overloaded builtins are called with type variables.
    /// During generalization, constraints on generalized variables are included in the TypeValue.Scheme.
    pub constraints: Vec<Arc<crate::value::Value>>,
    /// Unified environment: the canonical store for classes, instances, type schemes, and values.
    /// Class/instance lookups go through `state.env.read().unwrap().get_class(name)` and
    /// `state.env.read().unwrap().get_instance(mangled)`.
    pub env: Arc<RwLock<crate::env::Env>>,
    /// Names of bindings that failed type inference, mapping to the span of the failed binding.
    /// Used to annotate downstream T002 "undefined variable" errors with a "caused by" note
    /// that points to the failed definition site instead of just saying "not in scope".
    pub failed_bindings: HashMap<String, Span>,
    /// Span-keyed map from VarRef sites to the TypeValue of the variable they reference.
    /// Only populated when the caller enables scheme collection (non-None). Used by LSP hover
    /// to display type class constraints alongside the instantiated type.
    ///
    /// Enabled by setting this to `Some(SchemeMap::new())` before running inference.
    pub scheme_map: Option<SchemeMap>,
    /// Expected return type of the currently-inferring function (if annotated).
    /// Set by `infer_fn_push_cont` (CEK) when entering a function body with an explicit return annotation,
    /// cleared when exiting. Used for inferred [do] macro to determine which monad to use.
    pub expected_return: Option<TypeValue>,
    /// Expected parameter types for the next `fn` expression to be inferred.
    /// Set by `infer_instance_decl_from_surface` before calling `run_typecheck` on an instance
    /// method body, using the class method's specialized signature. Consumed and cleared by
    /// `infer_fn_push_cont` when it processes the params — it is single-use per fn invocation.
    ///
    /// Index i = expected TypeValue for the i-th fixed (non-variadic) parameter.
    /// When `Some`, unannotated params use `expected_fn_params[i]` instead of TypeValue.Unknown.
    /// When `None` (the default), unannotated params fall back to TypeValue.Unknown as before.
    pub expected_fn_params: Option<Vec<TypeValue>>,
    /// Accumulated type diagnostics (warnings, hints).
    /// Populated during type inference and generalization, extracted by the type checker.
    pub diagnostics: Vec<crate::error::TypeDiagnostic>,
    /// Deferred equality constraints for stuck TypeStageApp applications.
    /// When a TypeStageApp has non-ground arguments or cannot be reduced, equality
    /// constraints involving it are deferred here. After each round of unification,
    /// process_deferred_equalities attempts to resolve them.
    /// Actively written to (type_unify.rs) and saved/restored during branch inference (typecheck.rs).
    pub deferred_equalities: Vec<(TypeValue, TypeValue)>,
    /// Source names for type variables: internal TypeVar name → user-visible source name.
    /// When a function parameter `x` has an inferred TypeVar `_t42`, this maps `"_t42"` → `"x"`.
    /// Used by T013 diagnostics to report "ambiguous type variable 'x'" (the internal _tN
    /// name is hidden — it is noise for users). Only populated for parameters and let-bindings
    /// where a source name exists.
    pub type_var_source_names: HashMap<String, String>,
    /// Resolution table for slot-indexed TypeEnv lookups.
    ///
    /// The CEK machine's VarRef handler uses the resolved (level, slot) coordinates to
    /// call `env.get_scheme_at(level, slot)` — O(1) per-level. This is the single
    /// authority for VarRef type resolution (no name-based fallback for user bindings).
    ///
    /// Always populated: entry points run the resolver before type-checking. Tests that
    /// construct InferState directly get an empty table (no VarRef resolution → all lookups
    /// return None from slots).
    pub resolution_table: std::sync::Arc<crate::ast::ResolutionTable>,
    /// Unified scope frames from the resolver pass: both Dict letrec frames and
    /// BlockBody sequential injection frames, in injection order. Each frame maps
    /// binding names to their absolute resolver-assigned slot numbers.
    /// Populated from `resolve_surface_program`; used by `find_slot_in_frames` to assign
    /// correct slot positions when inserting bindings that don't yet have one.
    pub resolver_frames: Vec<indexmap::IndexMap<String, u32>>,
    /// EvalContext from tinct's evaluation pipeline — passed in when type-checking runs
    /// within a program evaluation (e.g. via builtin-typecheck). Used by resolve_type_head
    /// to materialize type-stage thunks without ambient filesystem access. Never created
    /// inside the type checker; always provided by the caller that has proper capabilities.
    pub eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
    /// Type-stage scope chain: pre-computed resolved TypeValues from type-stage evaluation.
    /// Vec[0] = innermost (highest priority); Vec[N-1] = outermost.
    /// Each frame maps type names to their resolved TypeValue.
    /// Populated by builtin-tc-update-type-stage-env (T-1803) and builtin_typecheck_doc
    /// write-back. Empty Vec means no type-stage types are available.
    ///
    /// Function entries (parameterized type constructors like Seq, Result) are NOT stored
    /// here — they live in `type_stage_fns`. TypeVar entries are NOT stored here — they
    /// live in `type_stage_type_vars`. Class entries are NOT stored here — they are looked
    /// up via `state.env`.
    pub type_stage_scope: Vec<std::collections::HashMap<String, TypeValue>>,
    /// Parameterized type constructor thunks from type-stage evaluation.
    /// Maps type name → function thunk for constructors like Seq, Result that take type
    /// arguments and return a TypeNode when called.
    pub type_stage_fns: std::collections::HashMap<String, std::sync::Arc<crate::value::Thunk>>,
    /// TypeVar kind annotations from type-stage evaluation.
    /// Maps type name → kind string (e.g., "Operator", "Label", "Type") for names that
    /// were declared as type-variable markers in the type-stage section (e.g., `Operator`,
    /// `Label` in builtin_core.llt).
    pub type_stage_type_vars: std::collections::HashMap<String, String>,
    /// Type constructor environment.
    /// Maps type constructor names to their TyConDef.
    pub tycon_env: std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
    /// Current FD improvement recursion depth. Passed by `&mut` to `try_fd_improvement`,
    /// which increments it on entry and decrements it on exit (RAII-style depth guard).
    /// Capped at 32 to prevent infinite fixpoint loops in pathological cases
    /// (e.g., cyclic FDs or degenerate instance sets).
    pub fd_depth: u32,
    /// The BindingId of the dict binding currently being type-checked (T-2060/T-2071 use-def liveness).
    ///
    /// Set to `Some(id)` before type-checking each dict entry's value expression, cleared
    /// to `None` after. When a VarRef resolves `name2` while `current_binding` is `Some(id1)`,
    /// `use_def[id1].insert(id2)` records the dependency edge.
    ///
    /// BindingId = (def_span, name) — uniqueness guaranteed by the source span,
    /// which is stable across all Arc frame allocations (unlike frame pointers which
    /// differ between dict_env, scc_env, and new_env_inner). T-2071 fix.
    ///
    /// Reset to `None` at the start of each Sequential's intermediate dict processing.
    pub current_binding: Option<BindingId>,
    /// Use-def liveness graph for Sequential intermediate dict bindings (T-2060/T-2071).
    ///
    /// `use_def[A]` = set of BindingIds that binding A's value expression directly references.
    /// Populated during dict entry inference via `current_binding` tracking.
    ///
    /// The AfterSequentialFinal handler performs BFS on this graph (starting from names
    /// directly referenced by the final expression) to compute the live set, then emits
    /// lost-binding warnings for intermediate bindings not reachable from the live set.
    ///
    /// Saved and restored (via std::mem::take) at the start of each Sequential so that
    /// nested Sequentials do not corrupt the enclosing Sequential's liveness graph. The
    /// saved value is carried in AfterSequentialFinal and restored when it fires.
    pub use_def: std::collections::HashMap<BindingId, std::collections::HashSet<BindingId>>,
    /// Narrowing refinements for the current branch scope (T-2083).
    ///
    /// Maps BindingId → narrowed TypeValue for bindings whose type has been refined
    /// by a type guard (e.g., `[if [int? x] ...]` narrows `x` to Int in the true branch).
    ///
    /// Narrowing is a TYPE REFINEMENT of an existing slot binding, not a new binding.
    /// `infer_var_ref` checks this map after a successful slot lookup and returns the
    /// narrowed TypeValue if one is present.
    ///
    /// Scoped via AfterBlock: `saved_narrowing_map` is stored in the AfterBlock continuation
    /// when pushed before a match arm body, and restored when AfterBlock fires. Same
    /// mechanism as `saved_use_def` and `saved_current_binding`.
    pub narrowing_map: std::collections::HashMap<BindingId, TypeValue>,
    /// The innermost env frame whose bindings resolve to VarAddr::Parameter(i) (T-2084).
    ///
    /// Set by `infer_fn_push_cont` (to fn_env_arc) and by MatchScrutinee/MatchArm arm
    /// setup (to the case arm env frame). `infer_var_ref` uses this directly for
    /// VarAddr::Parameter lookup instead of the broken level=2 depth hack.
    ///
    /// Saved/restored via `AfterBlock.saved_parameter_frame` so nested functions and
    /// nested matches correctly restore the enclosing frame when the block exits.
    pub current_parameter_frame: Option<std::sync::Arc<std::sync::RwLock<crate::env::Env>>>,
    /// TypeValue substitution and level tracking (S-1003 TypeValue-native inference context).
    /// Replaces the deleted `Substitution` and `type_vars` fields.
    pub ctx: InferenceContext,
}

impl InferState {
    pub fn new() -> Self {
        Self::with_env(Arc::new(RwLock::new(crate::env::Env::new())))
    }

    /// Create a new InferState with the given unified environment.
    ///
    /// All class/instance lookups go through `state.env`. The env must already contain
    /// all classes and instances visible during this type-checking run (seeded from parent
    /// environments via `Env::with_parent` chains).
    pub fn with_env(env: Arc<RwLock<crate::env::Env>>) -> Self {
        Self {
            constraints: Vec::new(),
            env,
            failed_bindings: HashMap::new(),
            scheme_map: None,
            expected_return: None,
            expected_fn_params: None,
            diagnostics: Vec::new(),
            deferred_equalities: Vec::new(),
            type_var_source_names: HashMap::new(),
            resolution_table: std::sync::Arc::new(std::collections::HashMap::new()),
            resolver_frames: Vec::new(),
            eval_ctx: None,
            type_stage_scope: Vec::new(),
            type_stage_fns: std::collections::HashMap::new(),
            type_stage_type_vars: std::collections::HashMap::new(),
            tycon_env: std::collections::HashMap::new(),
            fd_depth: 0,
            current_binding: None,
            use_def: std::collections::HashMap::new(),
            narrowing_map: std::collections::HashMap::new(),
            current_parameter_frame: None,
            ctx: InferenceContext::new(),
        }
    }

    /// Add a type class constraint to an explicit constraint accumulator.
    /// Used by the new InferState API (HEAD~1 style) where constraints are passed explicitly.
    /// Falls back gracefully: if the class is not in `env`, just skips the constraint.
    pub fn add_constraint_to(
        &mut self,
        constraints: &mut Vec<Arc<crate::value::Value>>,
        class_name: impl Into<String>,
        var: impl Into<String>,
    ) {
        let class_name = class_name.into();
        let var_name = var.into();
        let env_arc = Arc::clone(&self.env);
        let env_guard = env_arc.read().unwrap();
        if env_guard.get_class(&class_name).is_some() {
            // Build a ConstraintDecl TypeValue via make_constraint_decl.
            let class_tv = crate::type_class::make_type_op(&class_name);
            let var_tv = make_typevar_value(&var_name);
            let c = crate::type_class::make_constraint_decl(class_tv, vec![var_tv]);
            constraints.push(c);
        }
        // Unknown classes are deferred — instance resolution will report an error.
    }

    // ── TypeVar name generation ──────────────────────────────────────────────────

    /// Generate a TypeVar name from a source name, kind name string, and source span.
    ///
    /// Format:
    ///   kind != "Label":  `{source}⧼{file}:{line}:{col}⧽`   e.g. `a⧼main.llt:42:7⧽`
    ///   kind == "Label":  `ʟᴀʙᴇʟ∷{source}⧼{file}:{line}:{col}⧽`
    ///
    /// The span MUST always have file/line/col information — tinct source positions come from
    /// the parsed AST; Rust-internal creation sites use `rust_span!()` to embed the Rust
    /// source location. No 0:0 or empty-file spans are permitted.
    pub fn typevar_name(source: &str, kind: &str, span: &Span) -> String {
        let file = span.file.as_ref();
        let line = span.start_line;
        let col = span.start_col;
        if kind == "Label" {
            format!("ʟᴀʙᴇʟ∷{}⧼{}:{}:{}⧽", source, file, line, col)
        } else {
            format!("{}⧼{}:{}:{}⧽", source, file, line, col)
        }
    }

    /// Create a fresh TypeVar at the given (or current) level.
    ///
    /// Returns `(name, TypeValue)` where TypeValue is a `TypeValue.Var` with the given name.
    /// The name embeds `source_name` and the call site `span` as a human-readable hint, plus
    /// a gensym counter suffix to guarantee uniqueness across multiple calls at the same source
    /// position (e.g., polymorphic functions called multiple times).
    ///
    /// TypeVar identity is the full name including the counter. Two TypeVars created from the
    /// same annotation at the same source position are distinct because their counters differ.
    pub fn fresh_type_var_with(
        &mut self,
        source_name: Option<&str>,
        level: Option<u32>,
        kind: &str,
        span: &Span,
    ) -> (String, TypeValue) {
        let src = source_name.unwrap_or("?");
        let lvl = level.unwrap_or(self.ctx.current_level);
        let base = Self::typevar_name(src, kind, span);
        let name = format!("{}__{}", base, self.ctx.gensym_counter);
        self.ctx.gensym_counter += 1;
        self.ctx.levels.insert(name.clone(), lvl);
        let tv = make_typevar_value(&name);
        (name, tv)
    }

    /// Convenience: fresh TypeVar using the current level. Pass a real span.
    pub fn fresh_type_var(&mut self, span: &Span) -> TypeValue {
        self.fresh_type_var_with(None, None, "Type", span).1
    }

    /// Invalidate the cached env snapshots (no-op — caches were removed).
    pub fn invalidate_env_caches(&mut self) {}

    /// Compact the levels map by removing entries for TypeVars that have been bound in ctx.
    /// This prevents unbounded growth of the levels HashMap during long inference sessions.
    ///
    /// Call this periodically after unification rounds (e.g., at the end of infer_dict).
    pub fn compact_levels(&mut self) {
        self.ctx
            .levels
            .retain(|name, _level| !self.ctx.subst.contains_key(name));
    }

    /// Check if the substitution has no bindings (all type variables are free).
    pub fn subst_is_empty(&self) -> bool {
        self.ctx.subst.is_empty()
    }

    /// Apply the current substitution to a TypeValue, resolving bound type variables.
    pub fn apply(&self, ty: &TypeValue) -> TypeValue {
        self.ctx.apply_subst(ty)
    }

    /// Return a reference to the type constructor environment.
    pub fn tycon_env_ref(
        &self,
    ) -> &std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>> {
        &self.tycon_env
    }

    /// Set the level for a TypeVar name.
    pub fn set_level(&mut self, name: impl Into<String>, level: u32) {
        self.ctx.levels.insert(name.into(), level);
    }

    /// Get the level of a TypeVar name.
    pub fn get_level(&self, name: &str) -> Option<u32> {
        self.ctx.levels.get(name).copied()
    }

    /// Look up a TypeVar binding by name (test helper).
    ///
    /// Delegates to `self.ctx.subst`. Returns `Some(TypeValue)` if bound, `None` otherwise.
    /// Production code should use `self.apply()` which follows chains to fixpoint.
    #[cfg(test)]
    pub fn lookup_binding(&self, name: &str) -> Option<TypeValue> {
        self.ctx.subst.get(name).cloned()
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── InferenceContext tests ─────────────────────────────────────────────────

    /// InferenceContext::fresh_typevar creates a TypeValue.Var with a unique name,
    /// registers the level in ctx.levels, and increments the gensym counter.
    ///
    /// Mutation resistance: if fresh_typevar returned the same name twice or did not
    /// register the level, these assertions would fail.
    #[test]
    fn test_inference_context_fresh_typevar_unique() {
        let mut ctx = InferenceContext::new();
        ctx.current_level = 2;

        let tv0 = ctx.fresh_typevar("a");
        let tv1 = ctx.fresh_typevar("a");

        // Both are TypeValue.Var
        let name0 = typevalue_var_name(&tv0).expect("tv0 must be TypeValue.Var");
        let name1 = typevalue_var_name(&tv1).expect("tv1 must be TypeValue.Var");

        // Names must be different (gensym counter distinguishes them)
        assert_ne!(name0, name1, "fresh TypeVars must have distinct names");

        // Both must have their level registered as 2
        assert_eq!(ctx.get_level(&name0), 2, "tv0 level must be 2");
        assert_eq!(ctx.get_level(&name1), 2, "tv1 level must be 2");
    }

    /// InferenceContext::bind enforces monotonicity: binding the same name twice returns Err.
    ///
    /// Mutation resistance: if bind() permitted overwriting, the second call would succeed
    /// and the assert_true(result.is_err()) would fail.
    #[test]
    fn test_inference_context_bind_monotonic() {
        let mut ctx = InferenceContext::new();
        let tv = ctx.fresh_typevar("x");
        let name = typevalue_var_name(&tv).unwrap();

        // First bind: must succeed
        let r1 = ctx.bind(name.clone(), make_typevalue_unknown());
        assert!(r1.is_ok(), "first bind must succeed");

        // Second bind to the same name: must fail (monotonicity)
        let r2 = ctx.bind(name.clone(), make_typevalue_never());
        assert!(r2.is_err(), "second bind must fail — monotonicity violated");
    }

    /// InferenceContext::apply_subst follows binding chains.
    ///
    /// If α → β → TypeValue.Unknown, then apply_subst on α must return TypeValue.Unknown.
    ///
    /// Mutation resistance: if apply_subst only followed one level of indirection,
    /// it would return the β TypeValue.Var rather than Unknown.
    #[test]
    fn test_inference_context_apply_subst_chain() {
        let mut ctx = InferenceContext::new();
        let alpha = ctx.fresh_typevar("alpha");
        let beta = ctx.fresh_typevar("beta");
        let alpha_name = typevalue_var_name(&alpha).unwrap();
        let beta_name = typevalue_var_name(&beta).unwrap();

        // Bind α → β
        ctx.bind(alpha_name.clone(), std::sync::Arc::clone(&beta))
            .unwrap();
        // Bind β → TypeValue.Unknown
        ctx.bind(beta_name.clone(), make_typevalue_unknown())
            .unwrap();

        // apply_subst on α should follow α → β → Unknown and return Unknown
        let result = ctx.apply_subst(&alpha);
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_UNKNOWN),
            "apply_subst must follow binding chains to fixpoint"
        );
    }

    /// InferenceContext::lower_var_level lowers the level of a TypeVar.
    ///
    /// Mutation resistance: if lower_var_level were a no-op, the level would remain 5
    /// and the assert_eq!(ctx.get_level(&name), 2) would fail.
    #[test]
    fn test_inference_context_lower_var_level() {
        let mut ctx = InferenceContext::new();
        ctx.current_level = 5;
        let tv = ctx.fresh_typevar("t");
        let name = typevalue_var_name(&tv).unwrap();

        assert_eq!(ctx.get_level(&name), 5, "initial level must be 5");

        ctx.lower_var_level(&name, 2);
        assert_eq!(ctx.get_level(&name), 2, "level must be lowered to 2");

        // Lowering to a higher level is a no-op
        ctx.lower_var_level(&name, 7);
        assert_eq!(
            ctx.get_level(&name),
            2,
            "level must not be raised by lower_var_level"
        );
    }

    /// make_typevalue_unknown, make_typevalue_never, make_typevalue_top return unit variants
    /// with the correct ctor tags.
    #[test]
    fn test_typevalue_unit_constructors() {
        let unknown = make_typevalue_unknown();
        let never = make_typevalue_never();
        let top = make_typevalue_top();

        assert_eq!(
            typevalue_ctor(&unknown),
            Some(TV_UNKNOWN),
            "unknown must have ctor TypeValue.Unknown"
        );
        assert_eq!(
            typevalue_ctor(&never),
            Some(TV_NEVER),
            "never must have ctor TypeValue.Never"
        );
        assert_eq!(
            typevalue_ctor(&top),
            Some(TV_TOP),
            "top must have ctor TypeValue.Top"
        );
    }

    /// typevalue_eq on unit variants uses ctor-tag equality.
    #[test]
    fn test_typevalue_eq_unit_variants() {
        let u1 = make_typevalue_unknown();
        let u2 = make_typevalue_unknown();
        let n = make_typevalue_never();

        // Same ctor → equal
        assert!(
            typevalue_eq(&u1, &u2),
            "two Unknown TypeValues must be equal"
        );
        // Different ctor → not equal
        assert!(
            !typevalue_eq(&u1, &n),
            "Unknown and Never must not be equal"
        );
    }

    /// typevalue_eq on TypeValue.Var uses name equality.
    #[test]
    fn test_typevalue_eq_var_by_name() {
        let tv1 = make_typevar_value("foo");
        let tv2 = make_typevar_value("foo");
        let tv3 = make_typevar_value("bar");

        assert!(
            typevalue_eq(&tv1, &tv2),
            "two Vars with same name must be equal"
        );
        assert!(
            !typevalue_eq(&tv1, &tv3),
            "Vars with different names must not be equal"
        );
    }

    /// make_typevalue_repr creates a TypeValue.Repr variant with the correct repr field.
    #[test]
    fn test_make_typevalue_repr() {
        let repr = make_typevalue_repr(REPR_INT);
        assert_eq!(
            typevalue_ctor(&repr),
            Some(TV_REPR),
            "repr must have ctor TypeValue.Repr"
        );
        // The variant has a non-None payload
        match repr.as_ref() {
            crate::value::Value::Variant { payload, .. } => {
                assert!(payload.is_some(), "TypeValue.Repr must have a payload");
            }
            _ => panic!("expected Value::Variant"),
        }
    }

    /// make_typevalue_int_lit creates a TypeValue.IntLit with a settled Int payload.
    #[test]
    fn test_make_typevalue_int_lit() {
        let lit = make_typevalue_int_lit(42);
        assert_eq!(typevalue_ctor(&lit), Some(TV_INT_LIT));
        match lit.as_ref() {
            crate::value::Value::Variant {
                payload: Some(thunk),
                ..
            } => {
                // Payload thunk must be settled (we used Thunk::value)
                assert!(thunk.is_settled(), "IntLit payload must be settled");
                // Payload dict must have 'value' key with Int(42)
                match thunk.peek_result() {
                    Some(Ok(crate::value::Value::Dict { entries, .. })) => {
                        let key =
                            crate::value::HashableValue::Str(std::sync::Arc::from(FIELD_VALUE));
                        let val_thunk = entries
                            .get(&key)
                            .expect("IntLit dict must have 'value' key");
                        match val_thunk.peek_result() {
                            Some(Ok(crate::value::Value::Int { n: 42, .. })) => {}
                            other => panic!("expected Int(42), got {:?}", other),
                        }
                    }
                    other => panic!("expected settled Dict payload, got {:?}", other),
                }
            }
            _ => panic!("expected Value::Variant with Some payload"),
        }
    }

    /// InferenceContext::free_vars correctly identifies unbound TypeVars.
    #[test]
    fn test_inference_context_free_vars() {
        let mut ctx = InferenceContext::new();

        // Two unbound TypeVars
        let alpha = ctx.fresh_typevar("alpha");
        let beta = ctx.fresh_typevar("beta");
        let alpha_name = typevalue_var_name(&alpha).unwrap();
        let beta_name = typevalue_var_name(&beta).unwrap();

        // Both are free in an unbound alpha TypeValue
        let free = ctx.free_vars(&alpha);
        assert!(free.contains(&alpha_name), "alpha must be free in itself");
        assert!(
            !free.contains(&beta_name),
            "beta must not appear when walking alpha"
        );

        // Bind alpha → Unknown; now alpha is not free (it's bound)
        ctx.bind(alpha_name.clone(), make_typevalue_unknown())
            .unwrap();
        let free_after = ctx.free_vars(&alpha);
        assert!(
            !free_after.contains(&alpha_name),
            "bound alpha must not appear in free_vars"
        );
    }

    /// `compact_levels()` removes entries for TypeVars that have been bound in ctx.subst,
    /// while keeping entries for unbound TypeVars.
    ///
    /// Mutation resistance: if `compact_levels()` were a no-op, the unified var
    /// would still be present in `state.ctx.levels` after the call.
    #[test]
    fn test_compact_levels_removes_unified_var() {
        use crate::ast::Span;
        let mut state = InferState::new();

        // Create two fresh TypeVars using span-based names.
        let span_a = Span::rust_source(file!(), line!());
        let span_b = Span::rust_source(file!(), line!() + 1);
        let tv0 = state.fresh_type_var(&span_a); // registers name in levels at level 0
        let tv1 = state.fresh_type_var(&span_b); // registers name in levels at level 0

        let name0 = typevalue_var_name(&tv0).expect("tv0 must be TypeValue.Var");
        let name1 = typevalue_var_name(&tv1).expect("tv1 must be TypeValue.Var");

        assert!(
            state.ctx.levels.contains_key(&name0),
            "tv0 should be in levels before compaction"
        );
        assert!(
            state.ctx.levels.contains_key(&name1),
            "tv1 should be in levels before compaction"
        );

        // Bind tv0 → Unknown via ctx.subst (simulates what unification does).
        state
            .ctx
            .subst
            .insert(name0.clone(), make_typevalue_unknown());

        // compact_levels() should remove tv0 (now in ctx.subst) but keep tv1 (unbound).
        state.compact_levels();

        assert!(
            !state.ctx.levels.contains_key(&name0),
            "tv0 should be removed from levels after compaction (it is unified)"
        );
        assert!(
            state.ctx.levels.contains_key(&name1),
            "tv1 should remain in levels after compaction (it is still unbound)"
        );
    }

    /// `compact_levels()` is a no-op when no TypeVars have been unified.
    /// All registered TypeVars remain in `levels`.
    #[test]
    fn test_compact_levels_preserves_unbound_vars() {
        use crate::ast::Span;
        let mut state = InferState::new();
        let span_a = Span::rust_source(file!(), line!());
        let span_b = Span::rust_source(file!(), line!() + 1);
        state.fresh_type_var(&span_a);
        state.fresh_type_var(&span_b);

        let count_before = state.ctx.levels.len();
        state.compact_levels();
        let count_after = state.ctx.levels.len();

        assert_eq!(
            count_before, count_after,
            "compact_levels() must not remove unbound TypeVars"
        );
    }
}
