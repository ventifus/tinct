//! Type normalization and Display implementations.
//!
//! This module contains normalization logic for TypeValues and Display implementations.

use std::sync::Arc;

// TypeVarEntry deleted in S-1003 T-2004
use crate::type_tags::*;
use crate::value::{Value, TYPE_ID_DICT};
// Type import removed — Type enum deleted in S-1003 T-1986.
// All functions that used &Type have been replaced with Arc<Value> equivalents.

// type_to_typenode and typenode_leaf_to_type: deleted in S-1003 (Type enum deleted).
// Use typenode_leaf_to_typevalue / make_typevalue_* functions instead.

/// Call a type-stage resolver function strictly (no lazy evaluation machinery).
///
/// Type resolver functions (e.g. Seq, Result — parameterized type constructors) are pure
/// functions that take TypeNode values and return TypeNode values. They don't need the full
/// lazy call frame — arguments are concrete values, not thunks that might diverge.
///
/// The call frame is allocated as a child of the closure scope so variable lookup works
/// correctly, and is dropped after the result is obtained. No scope 0 mutation occurs.
///
/// Returns:
/// - `Ok(Some(ty))` — the resolver produced a recognized TypeNode and it was converted.
/// - `Ok(None)` — the resolver value is not applicable (wrong shape, args mismatch, etc.).
/// - `Err(e)` — materialization of the resolver body failed with an evaluation error.
/// Call a resolver function or TypeNode value with TypeValue arguments.
///
/// After S-1003 migration: args are `Arc<Value>` TypeValues instead of `Type`.
/// Returns `Ok(Some(tv))` where `tv` is a TypeValue, or `Ok(None)` if the resolver
/// is not applicable.
pub(crate) async fn call_strict_resolver(
    resolver_val: Value,
    args: &[Arc<Value>],
    eval_ctx: &Arc<crate::eval::EvalContext>,
    type_name_to_id: &std::collections::HashMap<String, u64>,
) -> Result<Option<Arc<Value>>, Box<crate::error::Diagnostic>> {
    // Leaf value (TypeNode Variant): convert to TypeValue directly without calling.
    if let Some(tv) = typenode_leaf_to_typevalue(&resolver_val) {
        if args.is_empty() {
            return Ok(Some(tv));
        }
        // Leaf with args: apply them as TypeValue.App chain.
        let mut result = tv;
        for arg in args {
            result = make_typevalue_app(result, Arc::clone(arg));
        }
        return Ok(Some(result));
    }

    if args.is_empty() {
        return Ok(None);
    }

    // Must be a parameterized type constructor (Function).
    let (clause, definition_scope) = match resolver_val {
        Value::Function {
            clauses,
            definition_scope,
            ..
        } => {
            let clause = clauses
                .first()
                .expect("Value::Function must have at least one clause")
                .clone();
            (clause, definition_scope)
        }
        _ => return Ok(None),
    };

    if clause.params.len() != args.len() {
        return Ok(None);
    }

    // Pass TypeValue args directly as TypeNode values (TypeValue IS the TypeNode after migration).
    // Each arg is already an Arc<Value> TypeValue — use it directly as the param thunk value.
    let param_thunks: Vec<Arc<crate::value::Thunk>> = args
        .iter()
        .zip(clause.params.iter())
        .map(|(tv, param)| {
            let span =
                crate::rust_span!().with_name(std::sync::Arc::from(param.node.name.as_str()));
            // TypeValue IS the TypeNode — pass it directly.
            Arc::new(crate::value::Thunk::value(Value::clone(tv.as_ref()), span))
        })
        .collect();
    let call_chain = {
        let mut table = crate::symbol_table::SymbolTable::new();
        for (i, _thunk) in param_thunks.iter().enumerate() {
            let name = clause
                .params
                .get(i)
                .map(|p| p.node.name.clone())
                .unwrap_or_else(|| format!("_param{i}"));
            table.insert(name, i as u32, None);
        }
        for (i, thunk) in param_thunks.into_iter().enumerate() {
            table.set_value(i as u32, thunk);
        }
        let new_frame = std::sync::Arc::new(table);
        let mut chain = definition_scope.clone();
        chain.insert(0, new_frame);
        chain
    };

    // Evaluate the function body in the call frame and force the result.
    let body_thunk = Arc::new(crate::value::Thunk::core_expr(
        Arc::clone(&clause.body),
        call_chain,
        Arc::clone(eval_ctx),
        crate::rust_span!(),
    ));
    let result_val = crate::eval::materialize(&body_thunk, None, eval_ctx).await?;

    // The result is already a TypeValue — typenode_value_to_type returns Arc<Value> TypeValue.
    crate::typecheck::typecheck_annot::typenode_value_to_type(
        &result_val,
        eval_ctx,
        &[],
        type_name_to_id,
    )
    .await
}

/// Evaluate a resolver by Arc<Thunk> — materializes the thunk then delegates to `call_strict_resolver`.
///
/// Returns:
/// - `Ok(Some(tv))` — the resolver produced a TypeValue.
/// - `Ok(None)` — the resolver value is not applicable (wrong shape, args mismatch, etc.).
/// - `Err(e)` — materialization of the thunk or the resolver body failed with an evaluation error.
pub(crate) async fn evaluate_resolver_with_thunk(
    thunk: Arc<crate::value::Thunk>,
    args: &[Arc<Value>],
    eval_ctx: &Arc<crate::eval::EvalContext>,
    type_name_to_id: &std::collections::HashMap<String, u64>,
) -> Result<Option<Arc<Value>>, Box<crate::error::Diagnostic>> {
    let resolver_val = crate::eval::materialize(&thunk, None, eval_ctx).await?;
    call_strict_resolver(resolver_val, args, eval_ctx, type_name_to_id).await
}

/// Create a TypeValue.App { op: TypeValue, arg: TypeValue } — type constructor application.
pub(crate) fn make_typevalue_app(op: Arc<Value>, arg: Arc<Value>) -> Arc<Value> {
    use crate::value::{HashableValue, Thunk};
    let mut entries = indexmap::IndexMap::new();
    entries.insert(
        HashableValue::Str(Arc::from(FIELD_OP)),
        Arc::new(Thunk::value(Value::clone(op.as_ref()), crate::rust_span!())),
    );
    entries.insert(
        HashableValue::Str(Arc::from(FIELD_ARG)),
        Arc::new(Thunk::value(
            Value::clone(arg.as_ref()),
            crate::rust_span!(),
        )),
    );
    let payload = Value::Dict {
        entries,
        type_decl_id: TYPE_ID_DICT,
        type_val: None,
    };
    Arc::new(Value::Variant {
        type_decl_id: crate::value::TYPE_ID_TYPE_VALUE,
        ctor_index: TV_APP,
        payload: Some(Arc::new(crate::value::Thunk::value(
            payload,
            crate::rust_span!(),
        ))),
        type_val: None,
    })
}

/// Convert a TypeNode leaf Variant value to a TypeValue (Arc<Value>).
/// Used by `call_strict_resolver` after S-1003 migration.
/// Parallel to `typenode_leaf_to_type` but returns TypeValue instead of Type.
pub(crate) fn typenode_leaf_to_typevalue(val: &Value) -> Option<Arc<Value>> {
    use crate::type_infer::{
        make_typevalue_never, make_typevalue_nominal, make_typevalue_top, make_typevalue_unknown,
    };
    let (type_decl_id, ctor_index) = match val {
        Value::Variant {
            type_decl_id,
            ctor_index,
            ..
        } => (*type_decl_id, *ctor_index),
        _ => return None,
    };
    // Only handle TypeNode variants (type_decl_id == TYPE_ID_TYPE_NODE).
    if type_decl_id != crate::value::TYPE_ID_TYPE_NODE {
        return None;
    }
    match ctor_index {
        TN_INT => Some(make_typevalue_nominal(crate::value::TYPE_ID_INT)),
        TN_FLOAT => Some(make_typevalue_nominal(crate::value::TYPE_ID_FLOAT)),
        TN_STRING => Some(make_typevalue_nominal(crate::value::TYPE_ID_STRING)),
        TN_BYTES => Some(make_typevalue_nominal(crate::value::TYPE_ID_BYTES)),
        TN_NEVER => Some(make_typevalue_never()),
        TN_UNKNOWN => Some(make_typevalue_unknown()),
        TN_TOP => Some(make_typevalue_top()),
        TN_ABSENT => {
            // Empty closed record TypeValue
            let fields: indexmap::IndexMap<String, Arc<Value>> = indexmap::IndexMap::new();
            Some(crate::typecheck::make_typevalue_record_pub(fields, None))
        }
        _ => None,
    }
}

/// Extract the kind name from a TypeNode.TypeVar sentinel value.
///
/// After S-1003: `Kind` enum is deleted. This function returns `Option<String>` (the kind name).
/// `TypeNode.TypeVar kind: "Operator"` and `TypeNode.TypeVar kind: "Label"` are produced
/// by builtin_core.llt's type-stage section for the `Operator` and `Label` type names.
/// The returned kind name is used to populate `InferState::type_stage_type_vars`.
///
/// Returns `Ok(Some(kind_name))` for recognised kind strings, `Ok(None)` for unrecognised values,
/// and `Err(e)` when a thunk inside the TypeVar payload has settled with an evaluation error.
pub(crate) fn typenode_typevar_kind(
    val: &Value,
) -> Result<Option<String>, std::sync::Arc<crate::error::Diagnostic>> {
    // Only matches Value::Variant { ctor_index: TN_TYPE_VAR, payload: Some(thunk) }
    // where the thunk resolves to Value::Dict containing kind: "Operator" | "Label".
    let (type_decl_id, ctor_index, payload_opt) = match val {
        Value::Variant {
            type_decl_id,
            ctor_index,
            payload,
            ..
        } => (*type_decl_id, *ctor_index, payload),
        _ => return Ok(None),
    };
    if type_decl_id != crate::value::TYPE_ID_TYPE_NODE || ctor_index != TN_TYPE_VAR {
        return Ok(None);
    }
    // The payload is an Arc<Thunk> that resolves to a Dict with a "kind" string field.
    let payload_thunk = match payload_opt.as_ref() {
        Some(t) => t,
        None => return Ok(None),
    };
    let payload_val = match payload_thunk.peek_result() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(std::sync::Arc::clone(e)),
        None => return Ok(None),
    };
    let dict = match payload_val {
        Value::Dict { entries: d, .. } => d,
        _ => return Ok(None),
    };
    // Extract the "kind" field value.
    let kind_key = crate::value::HashableValue::Str(std::sync::Arc::from(TN_FIELD_KIND));
    let kind_thunk = match dict.get(&kind_key) {
        Some(t) => t,
        None => return Ok(None),
    };
    let kind_val = match kind_thunk.peek_result() {
        Some(Ok(v)) => v,
        Some(Err(e)) => return Err(std::sync::Arc::clone(e)),
        None => return Ok(None),
    };
    let kind_str = match kind_val {
        Value::String {
            source, start, end, ..
        } => &source[*start..*end],
        _ => return Ok(None),
    };
    match kind_str {
        KIND_OPERATOR | KIND_LABEL | KIND_TYPE | KIND_ARROW => Ok(Some(kind_str.to_string())),
        _ => Ok(None),
    }
}

// T-1986/S-1003: impl fmt::Display for Type deleted — Type enum no longer exists.
// TypeValue formatting is handled by typevalue_display_string() and related helpers
// in this file and in type_infer.rs.
