//! Type instantiation, generalization, and constraint simplification.
//!
//! Pure functions for type scheme management. The `TypeEnv` struct that was
//! originally in this file has been deleted — the unified `Env` (src/env.rs)
//! replaces it.

use std::collections::{HashMap, HashSet};

use super::*;
use crate::type_tags::*;

// T-2004: Old Type-enum-based functions (instantiate_at_level, instantiate_scheme,
// generalize, generalize_with_doc) deleted. All callers now use the TypeValue-based
// functions below (instantiate_scheme_tv, generalize_tv).
//
// The old functions used Type, TypeScheme, Substitution, Kind, Row, RowTail — all
// deleted from type_def.rs in T-1986/T-1995/T-2001.

// ── TypeValue-based scheme operations (S-1003 migration) ─────────────────────
//
// These functions operate on `Arc<Value>` TypeValues (see type_infer.rs for the TypeValue
// type alias and helper constructors). They coexist with the Type-enum-based functions above
// during the incremental migration. Once all callers are migrated, the Type-enum functions
// above will be deleted.

/// Instantiate a TypeValue scheme by substituting fresh TypeVars for quantified variables.
///
/// The scheme is a `TypeValue.Scheme` variant with payload `{ vars: Dict, constraints: Dict, body: TypeValue }`.
/// Each var in `vars` is replaced by a fresh TypeValue.Var created via `ctx.fresh_typevar(name)`.
/// The substitution is applied to the body via structural TypeValue walking.
///
/// `level` is the enclosing let-binding level at the instantiation site — the caller captures
/// `state.ctx.current_level` before entering the binding and passes it here. Fresh TypeVars
/// are registered at this level in `ctx.levels` so that level-lowering during unification
/// behaves correctly.
///
/// Returns the instantiated body TypeValue (with fresh vars replacing quantified ones).
/// Returns the scheme body unchanged if the vars dict is empty (monomorphic scheme).
///
/// # Errors
/// Returns `None` if the scheme is not a well-formed TypeValue.Scheme (wrong ctor, missing payload,
/// or non-Dict payload). Callers should fall back gracefully (treat as unknown/opaque).
pub fn instantiate_scheme_tv(
    scheme: &crate::type_class::TypeValue,
    ctx: &mut crate::type_infer::InferenceContext,
    level: u32,
) -> Option<crate::type_class::TypeValue> {
    use crate::value::{HashableValue, Value};
    use std::sync::Arc;

    // Match TypeValue.Scheme { vars, constraints, body }
    let (vars_thunk, body_thunk) = match scheme.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(payload_thunk),
            ..
        } if ctor.as_ref() == TV_SCHEME => {
            // Force the payload dict synchronously (it must be settled — we constructed it).
            match payload_thunk.peek_result()? {
                Ok(Value::Dict { entries, .. }) => {
                    let vars_key = HashableValue::Str(Arc::from(FIELD_VARS));
                    let body_key = HashableValue::Str(Arc::from(FIELD_BODY));
                    let vars_thunk = entries.get(&vars_key)?.clone();
                    let body_thunk = entries.get(&body_key)?.clone();
                    (vars_thunk, body_thunk)
                }
                _ => return None,
            }
        }
        _ => return None,
    };

    // Extract vars dict: maps var-name-string → VarDecl (we only need the names here).
    let var_names: Vec<String> = match vars_thunk.peek_result()? {
        Ok(Value::Dict { entries, .. }) => entries
            .keys()
            .filter_map(|k| match k {
                HashableValue::Str(s) => Some(s.as_ref().to_string()),
                _ => None,
            })
            .collect(),
        _ => return None,
    };

    // Extract body TypeValue.
    let body: crate::type_class::TypeValue = match body_thunk.peek_result()? {
        Ok(v) => Arc::new(v.clone()),
        _ => return None,
    };

    // Monomorphic case: no type variables to instantiate — return body directly.
    if var_names.is_empty() {
        return Some(body);
    }

    // Build substitution: old_var_name → fresh TypeValue.Var
    // Temporarily set ctx.current_level to the caller's enclosing level so that
    // instantiated TypeVars are created at the correct level. We save and restore
    // current_level because instantiate_scheme_tv may be called from within a
    // save/restore context where current_level already reflects some inner scope;
    // the `level` argument is the enclosing let-level captured by the caller.
    let saved_level = ctx.current_level;
    ctx.current_level = level;
    let mut renaming: HashMap<String, crate::type_class::TypeValue> = HashMap::new();
    for var_name in &var_names {
        let fresh = ctx.fresh_typevar(var_name);
        renaming.insert(var_name.clone(), fresh);
    }
    ctx.current_level = saved_level;

    // Apply the renaming to the body TypeValue.
    Some(apply_typevalue_renaming(&body, &renaming))
}

/// Apply a name-to-TypeValue renaming to a TypeValue, substituting TypeValue.Var names.
///
/// This is the TypeValue equivalent of `Substitution::apply()` for the migration period.
/// Only substitutes `TypeValue.Var` nodes whose name appears in `renaming`. Other variants
/// and non-settled payloads are returned as-is (Arc::clone or best-effort structural walk).
///
/// NOTE: This does a shallow walk — it does NOT recursively descend into unsettled thunks.
/// Complex TypeValues with unsettled payloads are treated as opaque (returned as-is).
/// In practice, TypeValues constructed by the type checker are always settled synchronously.
pub fn apply_typevalue_renaming(
    ty: &crate::type_class::TypeValue,
    renaming: &HashMap<String, crate::type_class::TypeValue>,
) -> crate::type_class::TypeValue {
    // typevalue_var_name is in scope via `use super::*`
    use crate::value::Value;
    use std::sync::Arc;

    match ty.as_ref() {
        Value::Variant {
            ctor,
            payload,
            type_val,
        } => {
            match ctor.as_ref() {
                TV_VAR => {
                    // If this var is in the renaming, substitute.
                    if let Some(name) = typevalue_var_name(ty) {
                        if let Some(replacement) = renaming.get(&name) {
                            return Arc::clone(replacement);
                        }
                    }
                    // Not in renaming — return as-is.
                    Arc::clone(ty)
                }
                // Unit variants — no substitution positions.
                TV_UNKNOWN | TV_NEVER | TV_TOP => Arc::clone(ty),
                // Leaf variants with opaque payloads — no TypeVar positions to substitute.
                TV_REPR | TV_INT_LIT | TV_FLOAT_LIT | TV_STR_LIT | TV_OP => Arc::clone(ty),
                // Structural variants: recursively apply to settled payload dict fields.
                _ => {
                    let Some(payload_thunk) = payload else {
                        return Arc::clone(ty);
                    };
                    // If payload is settled, apply renaming to each TypeValue-shaped field.
                    // TypeValue payloads are Dicts whose values may be:
                    //   - Variant: a direct TypeValue field (e.g., Fn.return, Neg.inner)
                    //   - Dict: a nested Dict of TypeValues (e.g., Record.fields, Union.members,
                    //           Fn.params, Fn.param-names)
                    // We recurse into both layers so TypeVars inside Record.fields etc. are found.
                    match payload_thunk.peek_result() {
                        Some(Ok(Value::Dict { entries, .. })) => {
                            // Rebuild the payload dict with renamed fields.
                            let mut new_entries = indexmap::IndexMap::new();
                            let mut changed = false;
                            for (key, val_thunk) in entries.iter() {
                                match val_thunk.peek_result() {
                                    Some(Ok(v)) if matches!(v, Value::Variant { .. }) => {
                                        // Direct TypeValue field.
                                        let field_tv: crate::type_class::TypeValue =
                                            Arc::new(v.clone());
                                        let renamed = apply_typevalue_renaming(&field_tv, renaming);
                                        if !Arc::ptr_eq(&renamed, &field_tv) {
                                            changed = true;
                                        }
                                        new_entries.insert(
                                            key.clone(),
                                            Arc::new(crate::value::Thunk::value(
                                                renamed.as_ref().clone(),
                                                crate::rust_span!(),
                                            )),
                                        );
                                    }
                                    Some(Ok(Value::Dict {
                                        entries: inner_entries,
                                        ..
                                    })) => {
                                        // Nested Dict of TypeValues (Record.fields, Union.members, etc.).
                                        // Rebuild the inner dict with renamed TypeValue entries.
                                        let mut new_inner = indexmap::IndexMap::new();
                                        let mut inner_changed = false;
                                        for (ikey, ithunk) in inner_entries.iter() {
                                            match ithunk.peek_result() {
                                                Some(Ok(iv))
                                                    if matches!(iv, Value::Variant { .. }) =>
                                                {
                                                    let inner_tv: crate::type_class::TypeValue =
                                                        Arc::new(iv.clone());
                                                    let renamed = apply_typevalue_renaming(
                                                        &inner_tv, renaming,
                                                    );
                                                    if !Arc::ptr_eq(&renamed, &inner_tv) {
                                                        inner_changed = true;
                                                        changed = true;
                                                    }
                                                    new_inner.insert(
                                                        ikey.clone(),
                                                        Arc::new(crate::value::Thunk::value(
                                                            renamed.as_ref().clone(),
                                                            crate::rust_span!(),
                                                        )),
                                                    );
                                                }
                                                Some(Ok(iv)) => {
                                                    new_inner.insert(
                                                        ikey.clone(),
                                                        Arc::new(crate::value::Thunk::value(
                                                            iv.clone(),
                                                            crate::rust_span!(),
                                                        )),
                                                    );
                                                }
                                                _ => {
                                                    new_inner
                                                        .insert(ikey.clone(), Arc::clone(ithunk));
                                                }
                                            }
                                        }
                                        let new_inner_dict = if inner_changed {
                                            Value::Dict {
                                                entries: new_inner,
                                                type_val: crate::value::unknown_type_val(),
                                            }
                                        } else {
                                            // Inner dict unchanged — reconstruct from original.
                                            match val_thunk.peek_result() {
                                                Some(Ok(v)) => v.clone(),
                                                _ => unreachable!(),
                                            }
                                        };
                                        new_entries.insert(
                                            key.clone(),
                                            Arc::new(crate::value::Thunk::value(
                                                new_inner_dict,
                                                crate::rust_span!(),
                                            )),
                                        );
                                    }
                                    Some(Ok(v)) => {
                                        // Non-Variant, non-Dict field (String, Int, etc.) — copy as-is.
                                        new_entries.insert(
                                            key.clone(),
                                            Arc::new(crate::value::Thunk::value(
                                                v.clone(),
                                                crate::rust_span!(),
                                            )),
                                        );
                                    }
                                    _ => {
                                        // Unsettled — copy the original thunk.
                                        new_entries.insert(key.clone(), Arc::clone(val_thunk));
                                    }
                                }
                            }
                            if !changed {
                                // Nothing changed — return original.
                                return Arc::clone(ty);
                            }
                            // Reconstruct the Variant with the new payload dict.
                            let new_payload_dict = Value::Dict {
                                entries: new_entries,
                                type_val: crate::value::unknown_type_val(),
                            };
                            Arc::new(Value::Variant {
                                // type_val is always unknown_type_val() for TypeValues — not semantic.
                                type_val: Arc::clone(type_val),
                                ctor: Arc::clone(ctor),
                                payload: Some(Arc::new(crate::value::Thunk::value(
                                    new_payload_dict,
                                    crate::rust_span!(),
                                ))),
                            })
                        }
                        _ => {
                            // Payload not settled or not a Dict — return as-is.
                            Arc::clone(ty)
                        }
                    }
                }
            }
        }
        _ => Arc::clone(ty),
    }
}

/// Generalize a TypeValue into a TypeValue.Scheme by quantifying free TypeVars.
///
/// Collects all free TypeVars in `ty` whose level in `ctx.levels` is strictly greater
/// than `enclosing_level`. These are the variables introduced inside the let-binding being
/// generalized. Variables at level ≤ enclosing_level are free in the enclosing scope and
/// must NOT be quantified.
///
/// The result is a `TypeValue.Scheme` variant with:
/// - `vars` dict: each quantified var name → `VarDecl { name, kind: TypeValue.Unknown }`
/// - `constraints` dict: empty (constraint integration is future work)
/// - `body`: the original `ty` TypeValue
/// - `narrowings`: optional per-param narrowing type hints (stored as indexed dict)
/// - `doc`: optional docstring (stored as String value)
///
/// Returns a monomorphic scheme (bare TypeValue) if no free vars are generalizable,
/// to avoid wrapping non-polymorphic types in unnecessary Scheme variants.
///
/// `narrowings`: per-parameter narrowing types extracted from `@[narrows: T]` annotations.
///   Pass `&[]` when not available. Only stored when non-empty.
/// `doc`: docstring from `@[doc: "..."]` annotation. Pass `None` when not available.
pub fn generalize_tv(
    enclosing_level: u32,
    ty: &crate::type_class::TypeValue,
    ctx: &crate::type_infer::InferenceContext,
) -> crate::type_class::TypeValue {
    generalize_tv_with_meta(enclosing_level, ty, ctx, &[], None)
}

/// Full-fidelity generalize_tv that stores narrowing hints and docstring in the Scheme payload.
pub fn generalize_tv_with_meta(
    enclosing_level: u32,
    ty: &crate::type_class::TypeValue,
    ctx: &crate::type_infer::InferenceContext,
    narrowings: &[Option<crate::type_class::TypeValue>],
    doc: Option<&str>,
) -> crate::type_class::TypeValue {
    // make_typevalue_unknown and TypeValue are in scope via `use super::*`
    use crate::value::{HashableValue, Value};
    use std::sync::Arc;

    // Collect free TypeVars and filter by level > enclosing_level.
    let free = ctx.free_vars(ty);
    let mut seen = HashSet::new();
    let generalizable: Vec<String> = free
        .into_iter()
        .filter(|name| {
            let level = ctx.get_level(name);
            level > enclosing_level && seen.insert(name.clone())
        })
        .collect();

    // Monomorphic case: no generalizable variables — return ty directly.
    if generalizable.is_empty() {
        return Arc::clone(ty);
    }

    // Build vars dict: var_name → VarDecl { name: String, kind: TypeValue.Unknown }
    let vars_dict = {
        let mut entries = indexmap::IndexMap::new();
        for var_name in &generalizable {
            // VarDecl payload dict: { name: String, kind: TypeValue.Unknown }
            let var_decl_payload = {
                let mut d = indexmap::IndexMap::new();
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_NAME)),
                    Arc::new(crate::value::Thunk::value(
                        Value::String {
                            source: Arc::from(var_name.as_str()),
                            start: 0,
                            end: var_name.len(),
                            type_val: crate::value::unknown_type_val(),
                        },
                        crate::rust_span!(),
                    )),
                );
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_KIND)),
                    Arc::new(crate::value::Thunk::value(
                        make_typevalue_unknown().as_ref().clone(),
                        crate::rust_span!(),
                    )),
                );
                Value::Dict {
                    entries: d,
                    type_val: crate::value::unknown_type_val(),
                }
            };
            let var_decl = Value::Variant {
                type_val: crate::value::unknown_type_val(),
                ctor: Arc::from(TV_VAR_DECL),
                payload: Some(Arc::new(crate::value::Thunk::value(
                    var_decl_payload,
                    crate::rust_span!(),
                ))),
            };
            entries.insert(
                HashableValue::Str(Arc::from(var_name.as_str())),
                Arc::new(crate::value::Thunk::value(var_decl, crate::rust_span!())),
            );
        }
        Value::Dict {
            entries,
            type_val: crate::value::unknown_type_val(),
        }
    };

    // Build constraints dict: empty (future work: include typeclass constraints).
    let constraints_dict = Value::Dict {
        entries: indexmap::IndexMap::new(),
        type_val: crate::value::unknown_type_val(),
    };

    // Build narrowings dict: { 0: TypeValue | [], 1: TypeValue | [], ... }
    // Only stored when narrowings is non-empty to avoid bloating simple schemes.
    let narrowings_dict_opt = if narrowings.is_empty() {
        None
    } else {
        let mut narrowing_entries = indexmap::IndexMap::new();
        for (i, tv_opt) in narrowings.iter().enumerate() {
            let tv_val = match tv_opt {
                Some(tv) => tv.as_ref().clone(),
                None => Value::Dict {
                    entries: indexmap::IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
            };
            narrowing_entries.insert(
                HashableValue::Int(i as i64),
                Arc::new(crate::value::Thunk::value(tv_val, crate::rust_span!())),
            );
        }
        Some(Value::Dict {
            entries: narrowing_entries,
            type_val: crate::value::unknown_type_val(),
        })
    };

    // Build TypeValue.Scheme payload dict: { vars, constraints, body, narrowings?, doc? }
    let scheme_payload = {
        let mut entries = indexmap::IndexMap::new();
        entries.insert(
            HashableValue::Str(Arc::from(FIELD_VARS)),
            Arc::new(crate::value::Thunk::value(vars_dict, crate::rust_span!())),
        );
        entries.insert(
            HashableValue::Str(Arc::from(FIELD_CONSTRAINTS)),
            Arc::new(crate::value::Thunk::value(
                constraints_dict,
                crate::rust_span!(),
            )),
        );
        entries.insert(
            HashableValue::Str(Arc::from(FIELD_BODY)),
            Arc::new(crate::value::Thunk::value(
                ty.as_ref().clone(),
                crate::rust_span!(),
            )),
        );
        if let Some(narrowings_dict) = narrowings_dict_opt {
            entries.insert(
                HashableValue::Str(Arc::from(FIELD_NARROWINGS)),
                Arc::new(crate::value::Thunk::value(
                    narrowings_dict,
                    crate::rust_span!(),
                )),
            );
        }
        if let Some(doc_str) = doc {
            let doc_val = Value::String {
                source: Arc::from(doc_str),
                start: 0,
                end: doc_str.len(),
                type_val: crate::value::unknown_type_val(),
            };
            entries.insert(
                HashableValue::Str(Arc::from(FIELD_DOC)),
                Arc::new(crate::value::Thunk::value(doc_val, crate::rust_span!())),
            );
        }
        Value::Dict {
            entries,
            type_val: crate::value::unknown_type_val(),
        }
    };

    Arc::new(Value::Variant {
        type_val: crate::value::unknown_type_val(),
        ctor: Arc::from(TV_SCHEME),
        payload: Some(Arc::new(crate::value::Thunk::value(
            scheme_payload,
            crate::rust_span!(),
        ))),
    })
}

// ── TypeValue-based function tests (S-1003 migration) ────────────────────────

#[cfg(test)]
mod typevalue_scheme_tests {
    use super::*;
    use crate::type_infer::{
        make_typevalue_unknown, make_typevar_value, typevalue_ctor, typevalue_var_name,
        InferenceContext,
    };

    /// generalize_tv with a TypeValue.Var at level > enclosing_level produces a
    /// TypeValue.Scheme that quantifies over that variable.
    ///
    /// Mutation resistance: if generalize_tv failed to filter by level, it would either
    /// include all vars (even low-level ones) or no vars (even high-level ones).
    #[test]
    fn test_generalize_tv_creates_scheme() {
        let mut ctx = InferenceContext::new();
        ctx.current_level = 2;

        // Create a TypeVar at level 2
        let tv = ctx.fresh_typevar("a");
        assert!(
            typevalue_var_name(&tv).is_some(),
            "fresh_typevar must have a name"
        );

        // Generalize at enclosing_level = 1: level 2 > 1, so this var IS generalized
        let scheme = generalize_tv(1, &tv, &ctx);

        // Result must be a TypeValue.Scheme
        assert_eq!(
            typevalue_ctor(&scheme),
            Some(TV_SCHEME),
            "generalize_tv must produce TypeValue.Scheme when vars are generalizable"
        );
    }

    /// generalize_tv with a TypeValue.Var at level ≤ enclosing_level returns the
    /// bare TypeValue (monomorphic, no scheme wrapper).
    ///
    /// Mutation resistance: if generalize_tv ignored levels and always wrapped,
    /// the ctor check below would return Scheme instead of Var.
    #[test]
    fn test_generalize_tv_monomorphic_no_scheme() {
        let mut ctx = InferenceContext::new();
        ctx.current_level = 0;

        // Create a TypeVar at level 0
        let tv = ctx.fresh_typevar("a");

        // Generalize at enclosing_level = 0: level 0 is NOT > 0, so NOT generalized
        let result = generalize_tv(0, &tv, &ctx);

        // Must be returned as-is (not wrapped in Scheme)
        assert_ne!(
            typevalue_ctor(&result),
            Some(TV_SCHEME),
            "generalize_tv must not wrap non-generalizable vars in Scheme"
        );
        // Must still be a TypeValue.Var (same as input)
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_VAR),
            "generalize_tv must return the bare TypeValue for monomorphic types"
        );
    }

    /// generalize_tv on Unknown (no vars) returns Unknown unchanged.
    #[test]
    fn test_generalize_tv_unknown_passthrough() {
        let ctx = InferenceContext::new();
        let unknown = make_typevalue_unknown();

        let result = generalize_tv(0, &unknown, &ctx);
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_UNKNOWN),
            "generalize_tv must return Unknown unchanged"
        );
    }

    /// instantiate_scheme_tv on a well-formed TypeValue.Scheme creates fresh TypeVars.
    ///
    /// We manually construct a TypeValue.Scheme and verify that the instantiated result
    /// has a fresh TypeVar where the quantified var was.
    #[test]
    fn test_instantiate_scheme_tv_creates_fresh_vars() {
        use crate::value::{HashableValue, Value};
        use std::sync::Arc;

        let mut ctx = InferenceContext::new();

        // Manually construct: TypeValue.Scheme { vars: {"a": VarDecl{name:"a", kind:Unknown}}, constraints: {}, body: TypeValue.Var{name:"a"} }
        let var_decl_payload = Value::Dict {
            entries: {
                let mut d = indexmap::IndexMap::new();
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_NAME)),
                    Arc::new(crate::value::Thunk::value(
                        Value::String {
                            source: Arc::from("a"),
                            start: 0,
                            end: 1,
                            type_val: crate::value::unknown_type_val(),
                        },
                        crate::rust_span!(),
                    )),
                );
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_KIND)),
                    Arc::new(crate::value::Thunk::value(
                        make_typevalue_unknown().as_ref().clone(),
                        crate::rust_span!(),
                    )),
                );
                d
            },
            type_val: crate::value::unknown_type_val(),
        };
        let var_decl = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(TV_VAR_DECL),
            payload: Some(Arc::new(crate::value::Thunk::value(
                var_decl_payload,
                crate::rust_span!(),
            ))),
        };

        let vars_dict = Value::Dict {
            entries: {
                let mut d = indexmap::IndexMap::new();
                d.insert(
                    HashableValue::Str(Arc::from("a")),
                    Arc::new(crate::value::Thunk::value(var_decl, crate::rust_span!())),
                );
                d
            },
            type_val: crate::value::unknown_type_val(),
        };
        let constraints_dict = Value::Dict {
            entries: indexmap::IndexMap::new(),
            type_val: crate::value::unknown_type_val(),
        };
        let body_var = make_typevar_value("a"); // body is TypeValue.Var{name:"a"}

        let scheme_payload = Value::Dict {
            entries: {
                let mut d = indexmap::IndexMap::new();
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_VARS)),
                    Arc::new(crate::value::Thunk::value(vars_dict, crate::rust_span!())),
                );
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_CONSTRAINTS)),
                    Arc::new(crate::value::Thunk::value(
                        constraints_dict,
                        crate::rust_span!(),
                    )),
                );
                d.insert(
                    HashableValue::Str(Arc::from(FIELD_BODY)),
                    Arc::new(crate::value::Thunk::value(
                        body_var.as_ref().clone(),
                        crate::rust_span!(),
                    )),
                );
                d
            },
            type_val: crate::value::unknown_type_val(),
        };

        let scheme: crate::type_class::TypeValue = Arc::new(Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from(TV_SCHEME),
            payload: Some(Arc::new(crate::value::Thunk::value(
                scheme_payload,
                crate::rust_span!(),
            ))),
        });

        // Instantiate
        let instantiated = instantiate_scheme_tv(&scheme, &mut ctx, 0);
        assert!(
            instantiated.is_some(),
            "instantiate_scheme_tv must succeed on well-formed Scheme"
        );
        let result = instantiated.unwrap();

        // Result should be a TypeValue.Var (the body was a TypeValue.Var, substituted with fresh)
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_VAR),
            "instantiated body must be a TypeValue.Var"
        );

        // The fresh var name must be DIFFERENT from "a" (it was freshened by ctx)
        let fresh_name = typevalue_var_name(&result).unwrap();
        assert_ne!(
            fresh_name, "a",
            "instantiated var must have a fresh name, not the original 'a'"
        );
    }

    /// apply_typevalue_renaming on a TypeValue.Unknown (no vars) returns the original unchanged.
    #[test]
    fn test_apply_typevalue_renaming_passthrough() {
        use std::collections::HashMap;

        let unknown = make_typevalue_unknown();
        let renaming: HashMap<String, crate::type_class::TypeValue> = HashMap::new();

        let result = apply_typevalue_renaming(&unknown, &renaming);

        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_UNKNOWN),
            "apply_typevalue_renaming on Unknown must return Unknown"
        );
    }

    /// apply_typevalue_renaming substitutes matching TypeValue.Var names.
    #[test]
    fn test_apply_typevalue_renaming_substitutes_var() {
        use std::collections::HashMap;
        use std::sync::Arc;

        let var_a = make_typevar_value("a");
        let replacement = make_typevalue_unknown();

        let mut renaming: HashMap<String, crate::type_class::TypeValue> = HashMap::new();
        renaming.insert("a".to_string(), Arc::clone(&replacement));

        let result = apply_typevalue_renaming(&var_a, &renaming);

        // Should be Unknown (the replacement)
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_UNKNOWN),
            "apply_typevalue_renaming must substitute matching var 'a' with Unknown"
        );
    }

    /// Fix 3: generalize_tv on a TypeValue.Record with a TypeVar field must produce a Scheme
    /// that quantifies over the TypeVar in the field.
    ///
    /// Previously collect_free_vars_inner only walked one level deep and missed TypeVars
    /// inside Record.fields (which is a Dict-valued field, not a Variant-valued field).
    #[test]
    fn test_generalize_tv_on_record_with_typevar_field() {
        use crate::type_infer::{make_typevalue_record, InferenceContext};
        use std::sync::Arc;

        let mut ctx = InferenceContext::new();
        ctx.current_level = 2;

        // Create a TypeVar "a" at level 2.
        let tv_a = ctx.fresh_typevar("a");
        let var_name = typevalue_var_name(&tv_a).expect("fresh_typevar must produce a named var");

        // Build TypeValue.Record { fields: { x: TypeValue.Var("a") } }
        let record = make_typevalue_record(
            indexmap::IndexMap::from([(var_name.clone(), Arc::clone(&tv_a))]),
            None,
        );

        // generalize at level 1: "a" is at level 2 > 1, so it MUST be quantified.
        let scheme = generalize_tv(1, &record, &ctx);

        // Result must be TypeValue.Scheme (not bare Record).
        assert_eq!(
            typevalue_ctor(&scheme),
            Some(TV_SCHEME),
            "generalize_tv on Record with TypeVar field must produce TypeValue.Scheme — \
             collect_free_vars_inner must recurse into Record.fields Dict"
        );
    }

    /// Fix 4: apply_typevalue_renaming on a TypeValue.Record with a TypeVar field must
    /// substitute the TypeVar inside Record.fields.
    ///
    /// Previously only the outer payload Dict's Variant entries were visited; Dict entries
    /// (like Record.fields) were not recursed into.
    #[test]
    fn test_apply_typevalue_renaming_substitutes_into_record_field() {
        use crate::type_infer::{make_typevalue_record, make_typevalue_repr};
        use crate::type_tags::REPR_INT;
        use std::collections::HashMap;
        use std::sync::Arc;

        let var_a = make_typevar_value("a");

        // TypeValue.Record { fields: { x: TypeValue.Var("a") } }
        let record = make_typevalue_record(
            indexmap::IndexMap::from([("x".to_string(), Arc::clone(&var_a))]),
            None,
        );

        // Rename "a" → TypeValue.Repr(Int)
        let replacement = make_typevalue_repr(REPR_INT);
        let renaming: HashMap<String, crate::type_class::TypeValue> =
            [("a".to_string(), Arc::clone(&replacement))]
                .into_iter()
                .collect();

        let result = apply_typevalue_renaming(&record, &renaming);

        // Result must be a TypeValue.Record (outer Variant unchanged).
        assert_eq!(
            typevalue_ctor(&result),
            Some(TV_RECORD),
            "apply_typevalue_renaming must preserve the Record wrapper"
        );

        // Extract the field "x" and verify it was renamed to Repr(Int).
        let fields = crate::type_infer::typevalue_record_fields_pub(&result);
        let x_field = fields
            .get("x")
            .expect("renamed Record must still have field 'x'");
        assert_eq!(
            typevalue_ctor(x_field),
            Some(TV_REPR),
            "field 'x' in renamed Record must be TypeValue.Repr — \
             apply_typevalue_renaming must recurse into Record.fields Dict"
        );
    }

    /// Round-trip test: generalize_tv then instantiate_scheme_tv on a TypeValue.Fn.
    ///
    /// This test verifies the complete generalization/instantiation cycle:
    /// 1. Create a TypeValue.Fn whose return type is a free TypeValue.Var.
    /// 2. Call generalize_tv — result must be TypeValue.Scheme containing the Var in its vars dict.
    /// 3. Call instantiate_scheme_tv — result must be a TypeValue.Fn whose return type is a
    ///    fresh TypeValue.Var with a different name than the original.
    #[test]
    fn test_generalize_instantiate_round_trip_fn() {
        use crate::type_infer::{
            make_typevalue_fn, make_typevalue_repr, typevalue_fn_params_and_ret, InferenceContext,
        };
        use crate::type_tags::REPR_INT;
        use std::sync::Arc;

        let mut ctx = InferenceContext::new();
        ctx.current_level = 1;

        // Create a free TypeValue.Var "a" at level 1.
        let tv_a = ctx.fresh_typevar("a");
        let original_name =
            typevalue_var_name(&tv_a).expect("fresh_typevar must produce a named var");

        // Build TypeValue.Fn { params: [TypeValue.Repr(Int)], return: TypeValue.Var("a") }
        let int_repr = make_typevalue_repr(REPR_INT);
        let params = vec![(None, Arc::clone(&int_repr))];
        let fn_ty = make_typevalue_fn(params, Arc::clone(&tv_a));

        // Step 1: generalize at level 0 — "a" at level 1 > 0, so it must be quantified.
        let scheme = generalize_tv(0, &fn_ty, &ctx);

        assert_eq!(
            typevalue_ctor(&scheme),
            Some(TV_SCHEME),
            "generalize_tv on TypeValue.Fn with free TypeVar must produce TypeValue.Scheme"
        );

        // Step 2: verify the scheme's vars dict contains the original TypeVar name.
        let scheme_payload = match scheme.as_ref() {
            crate::value::Value::Variant {
                payload: Some(p), ..
            } => match p.peek_result() {
                Some(Ok(crate::value::Value::Dict { entries, .. })) => entries.clone(),
                _ => panic!("scheme payload must be a settled Dict"),
            },
            _ => panic!("scheme must be a Variant"),
        };
        let vars_key = crate::value::HashableValue::Str(Arc::from(FIELD_VARS));
        let vars_thunk = scheme_payload
            .get(&vars_key)
            .expect("TypeValue.Scheme must have 'vars' field");
        let vars_dict = match vars_thunk.peek_result() {
            Some(Ok(crate::value::Value::Dict { entries, .. })) => entries.clone(),
            _ => panic!("vars must be a settled Dict"),
        };
        let var_key = crate::value::HashableValue::Str(Arc::from(original_name.clone()));
        assert!(
            vars_dict.contains_key(&var_key),
            "TypeValue.Scheme vars dict must contain the original TypeVar name '{}'",
            original_name
        );

        // Step 3: instantiate the scheme — must produce a TypeValue.Fn with a fresh return TypeVar.
        let mut ctx2 = InferenceContext::new();
        let instantiated = instantiate_scheme_tv(&scheme, &mut ctx2, 0);
        assert!(
            instantiated.is_some(),
            "instantiate_scheme_tv must succeed on well-formed scheme"
        );
        let inst_fn = instantiated.unwrap();

        // The instantiated type must still be a TypeValue.Fn.
        assert_eq!(
            typevalue_ctor(&inst_fn),
            Some(TV_FN),
            "instantiate_scheme_tv of a TypeValue.Fn scheme must produce TypeValue.Fn"
        );

        // Extract the return type from the instantiated Fn and verify it is a fresh TypeVar.
        let (_, ret_ty) = typevalue_fn_params_and_ret(&inst_fn)
            .expect("instantiated TypeValue.Fn must have params and return type");
        assert_eq!(
            typevalue_ctor(&ret_ty),
            Some(TV_VAR),
            "return type of instantiated Fn must be a TypeValue.Var"
        );
        let fresh_name = typevalue_var_name(&ret_ty).expect("return TypeVar must have a name");
        assert_ne!(
            fresh_name, original_name,
            "instantiated return TypeVar must have a fresh name different from '{}'",
            original_name
        );
    }
}
