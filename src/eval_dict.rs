//! Dict construction and letrec scoping: `eval_dict_core`, `eval_key_core`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! All evaluation is CoreExpr-native via `eval_dict_core` / `eval_key_core`.
//! The old Expr-based `eval_dict` / `eval_key` were removed in the Parts-B+E migration.

use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{CoreEntry, CoreExpr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, Environment, Key, Thunk, Value};

use super::{eval_core_expr, materialize, EvalContext};

fn value_to_key(value: &Value, span: &Span) -> EvalResult<Key> {
    match value {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(Key::String(source[*start..*end].to_string())),
        Value::Int(n) => Ok(Key::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), *span).into()),
    }
}

// ============================================================================
// eval_dict_core / eval_key_core
//
// These functions accept `CoreEntry` / `CoreExpr` slices directly.
// Non-literal entries use Thunk::new_unevaluated_core (UnevaluatedState::CoreExpr) —
// no CoreExpr→Expr round-trip for dict values.
//
// Note: TypeAlias constructor pre-computation is OMITTED intentionally.
// CoreExpr::Dict entries never contain CoreExpr::TypeAlias (declaration forms
// become CoreExpr::Error during expr_to_core_expr / lowering). The TypeAlias
// pre-pass from the old eval_dict is dead code post-E1; tracked as a pre-existing
// regression in TODO.md (runtime-v2-fix-class-instance-in-dict).
// ============================================================================

/// Count static string-keyed entries in a CoreEntry slice.
///
/// Static string keys are those that produce `Key::String` bindings visible to
/// sibling VarRef lookups: `CoreExpr::Str` and `CoreExpr::Annotated`. Must stay
/// in sync with the resolver's walk_expr Dict arm.
fn count_static_keys_core(entries: &[Spanned<CoreEntry>]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry
                .node
                .key
                .as_ref()
                .is_some_and(|k| matches!(&k.node, CoreExpr::Str(_) | CoreExpr::Annotated { .. }))
        })
        .count()
}

/// Evaluate a dict literal from `CoreExpr::Dict` entries with letrec semantics.
///
/// Directly accepts the `CoreEntry` slice produced by `eval_core_expr`'s Dict arm,
/// avoiding the Vec<Spanned<Entry>> allocation previously required by `eval_dict`.
///
/// Semantics are identical to `eval_dict`:
/// - String-keyed entries enter `dict_env` (letrec: forward references allowed)
/// - Keys evaluated in `parent_env` (Key Isolation Invariant)
/// - Literal values (Int/Float/Bool/Str) get Materialized thunks directly (fast path)
/// - Non-literal values become CoreExpr thunks in `dict_env` (UnevaluatedState::CoreExpr)
///   — no CoreExpr→Expr round-trip for dict values.
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    parent_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    let dict_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // Allocate a FlatEnv for this dict scope (same logic as eval_dict).
    let static_key_count = count_static_keys_core(entries);
    let env_id = ctx.env_arena.lock().unwrap().alloc_root(static_key_count);
    let mut slot_idx: u32 = 0;

    for entry in entries {
        // Determine if this entry has a static key (CoreExpr::Str or CoreExpr::Annotated).
        // Must match resolve.rs Resolver::walk_expr Dict arm exactly.
        let is_static_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| matches!(&k.node, CoreExpr::Str(_) | CoreExpr::Annotated { .. }));

        let key = match &entry.node.key {
            Some(key_expr) => eval_key_core(key_expr, parent_env, ctx).await?,
            None => {
                let k = Key::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span)
                })?;
                k
            }
        };

        // Fast path for literal values: Materialized thunks directly (Nix maybeThunk pattern).
        // Non-literal values become CoreExpr thunks pointing to dict_env.
        let thunk = match &entry.node.value.node {
            CoreExpr::Int(n) => Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                entry.node.value.span,
            )),
            CoreExpr::Float(f) => Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                entry.node.value.span,
            )),
            CoreExpr::Bool(b) => Arc::new(Thunk::new_materialized(
                Value::Bool(*b),
                entry.node.value.span,
            )),
            CoreExpr::Str(s) => Arc::new(Thunk::new_materialized(
                string_val(s),
                entry.node.value.span,
            )),
            // Non-literal: use UnevaluatedState::CoreExpr — no CoreExpr→Expr round-trip.
            // TODO(parts-e): Thunk::new_unevaluated_core_with_env_id (for the env_id fast path).
            _ => Arc::new(Thunk::new_unevaluated_core(
                Arc::clone(&entry.node.value),
                Arc::clone(&dict_env),
                Arc::clone(ctx),
                entry.node.value.span,
            )),
        };

        // String keys become bindings so sibling entries can reference via $name (letrec).
        if let Key::String(ref name) = key {
            dict_env
                .write()
                .unwrap()
                .insert(name.clone(), Arc::clone(&thunk));
        }

        let thunk_id = ctx.alloc_thunk(thunk);
        if dict_map.insert(key.clone(), thunk_id).is_some() {
            return Err(Box::new(EvalError::duplicate_key(
                &key.to_string(),
                entry.span,
            )));
        }

        if is_static_key {
            ctx.env_arena
                .lock()
                .unwrap()
                .fill_letrec_slot(env_id, slot_idx, thunk_id);
            slot_idx += 1;
        }
    }

    Ok(Arc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        *dict_span,
    )))
}

/// Evaluate a dict key from a `CoreExpr` node, returning a concrete `Key`.
///
/// Fast path for literal keys (Str/Int) avoids creating temporary thunks.
/// General path materializes the expression via `eval_core_expr`.
pub(crate) async fn eval_key_core(
    key_expr: &Arc<Spanned<CoreExpr>>,
    parent_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Key> {
    // Fast path for literal keys
    match &key_expr.node {
        CoreExpr::Str(s) => return Ok(Key::String(s.clone())),
        CoreExpr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval_core_expr(key_expr.as_ref(), parent_env, ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}
