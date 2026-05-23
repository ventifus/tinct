//! Dict construction and letrec scoping: `eval_dict`, `eval_key`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! Two variants are provided:
//! - `eval_dict` / `eval_key`: accept old `Entry` / `Expr` types (used by Expr path)
//! - `eval_dict_core` / `eval_key_core`: accept `CoreEntry` / `CoreExpr` (used by E2+)
//!   Eliminates the CoreExpr→Expr round-trip in `eval_core_expr`'s Dict arm.

use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{CoreEntry, CoreExpr, Entry, Expr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, Environment, Key, Thunk, Value};

use super::{
    eval, eval_core_expr, extract_nominal_constructors, materialize, EvalContext,
    VARIANT_TAG_MARKER,
};

/// Count the number of static string-keyed entries in a dict.
///
/// Static string keys are those that produce `Key::String` bindings visible to
/// sibling VarRef lookups: `Expr::Str` and `Expr::Annotated`. This count is used
/// to pre-size the FlatEnv slot vector for O(1) VarRef lookup.
///
/// Matches the static key logic in `resolve.rs::Resolver::walk_expr` (Dict arm).
fn count_static_keys(entries: &[Spanned<Entry>]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry.node.key.as_ref().is_some_and(|key_expr| {
                matches!(&key_expr.node, Expr::Str(_) | Expr::Annotated { .. })
            })
        })
        .count()
}

pub(crate) async fn eval_dict(
    entries: &[Spanned<Entry>],
    parent_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    let dict_env = Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // Phase 3 (arena-eval): Allocate a FlatEnv for this dict scope.
    //
    // The FlatEnv stores ThunkIds for static-key entries, enabling O(1) variable lookup
    // when VarRef nodes have been assigned (level, slot) coordinates by the resolver.
    // We use alloc_root (no parent FlatEnv) because the parent chain is still Rc-based;
    // O(1) applies only to level=0 (same-dict sibling references, De Bruijn level 0 = current scope).
    // Outer-scope references use level > 0, walking N parent hops via Environment.get_by_slot.
    //
    // env_id is stored in Unevaluated thunks for future use when take_unevaluated is
    // updated to propagate it. For now it acts as scaffolding (the evaluator discards it).
    let static_key_count = count_static_keys(entries);
    let env_id = ctx.env_arena.lock().unwrap().alloc_root(static_key_count);

    // Slot index counter: incremented only for static-key entries (matching resolver logic).
    // Must stay in sync with resolve.rs Resolver::walk_expr Dict arm (Expr::Str | Expr::Annotated).
    let mut slot_idx: u32 = 0;

    // Pre-pass: pre-COMPUTE (but do NOT insert) nominal variant constructor thunks.
    //
    // Problem: `Ok: Ok` is a self-referential letrec thunk. When forced, it evaluates
    // VarRef "Ok", which looks up dict_env["Ok"] — the same `Ok: Ok` thunk — causing
    // E070 circular dependency.
    //
    // Fix (slot-safe): compute constructor thunks in a side table keyed by tag name.
    // In the main pass below, when a re-export entry like `Ok: Ok` is processed,
    // substitute the pre-computed materialized thunk instead of an unevaluated thunk,
    // and INSERT it normally into dict_env at the correct (AST-order) position.
    //
    // This preserves the invariant that dict_env.bindings insertion order matches the
    // resolver's slot assignments (both iterate entries in AST order). The previous
    // approach inserted constructors into dict_env BEFORE the main loop (at positions
    // 0..N), shifting all subsequent entries by N and breaking slot-based lookup.
    //
    // The TypeAlias entry itself (`Result: [type ...]`) must come before the re-export
    // entries (`Ok: Ok`) in the AST — this is enforced by convention in the prelude.
    // When `Result:` is processed in the main pass, `Expr::TypeAlias` evaluation
    // (eval_step) will also insert constructors into dict_env — this is consistent
    // with the pre-computed thunks and doesn't cause slot mismatches (IndexMap::insert
    // with an existing key updates in-place without changing the position).

    // Early-exit guard: skip constructor pre-computation if there are no TypeAlias entries.
    // Most dicts don't have TypeAlias, so avoid the O(n) constructor-building loop below.
    let has_type_alias = entries
        .iter()
        .any(|entry| matches!(&entry.node.value.node, Expr::TypeAlias { .. }));

    let mut constructor_precomputed: std::collections::HashMap<String, Arc<Thunk>> =
        std::collections::HashMap::new();

    if has_type_alias {
        for entry in entries {
            if let Expr::TypeAlias { params: _, body } = &entry.node.value.node {
                let span = entry.node.value.span;
                for (tag, has_payload) in extract_nominal_constructors(&body.node) {
                    let constructor_value = if has_payload {
                        let constructor_env =
                            Arc::new(RwLock::new(Environment::with_parent(Arc::clone(&dict_env))));
                        constructor_env.write().unwrap().insert(
                            VARIANT_TAG_MARKER.to_string(),
                            Arc::new(Thunk::new_materialized(string_val(&tag), span)),
                        );
                        let param = Param {
                            name: "payload".to_string(),
                            annotation: None,
                            variadic: false,
                        };
                        // The body is never evaluated for variant constructors:
                        // invoke_function early-returns via the VARIANT_TAG_MARKER path.
                        // We store CoreExpr::FreeVar as a placeholder that matches the type.
                        let body_expr =
                            Arc::new(Spanned::new(CoreExpr::FreeVar("payload".to_string()), span));
                        Value::Function {
                            params: Rc::new(vec![param]),
                            body: body_expr,
                            env: constructor_env,
                            annotation: None,
                        }
                    } else {
                        Value::Variant {
                            tag: tag.clone(),
                            payload: None,
                        }
                    };
                    constructor_precomputed.insert(
                        tag,
                        Arc::new(Thunk::new_materialized(constructor_value, span)),
                    );
                }
            }
        }
    }

    for entry in entries {
        // Determine if this entry has a static key (Expr::Str or Expr::Annotated).
        // This must match resolve.rs Resolver::walk_expr Dict arm exactly.
        let is_static_key = entry.node.key.as_ref().is_some_and(|key_expr| {
            matches!(&key_expr.node, Expr::Str(_) | Expr::Annotated { .. })
        });

        let key = match &entry.node.key {
            // Keys are evaluated in the parent scope, not dict_env, because key
            // expressions must not see sibling bindings. This prevents keys from
            // depending on values that are still unevaluated thunks and keeps
            // key evaluation deterministic regardless of entry order.
            Some(key_expr) => eval_key(key_expr, parent_env, ctx).await?,
            None => {
                let k = Key::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span)
                })?;
                k
            }
        };

        // Fast path for literal values: create Materialized thunks directly,
        // avoiding Unevaluated → Materialized state transition overhead (Nix maybeThunk pattern).
        // Non-literal unevaluated thunks for static-key entries get env_id for future O(1) lookup.
        //
        // Special case: if this entry's string key is a pre-computed constructor (e.g. `Ok: Ok`),
        // substitute the materialized constructor thunk directly. This breaks the circular
        // dependency where forcing `Ok: Ok` → VarRef("Ok") → dict_env["Ok"] → same thunk → cycle.
        // The constructor thunk is inserted into dict_env at the CORRECT AST-order position,
        // preserving slot index consistency with the resolver.
        let thunk = if let Key::String(ref name) = key {
            if let Some(ctor_thunk) = constructor_precomputed.get(name.as_str()) {
                // Use the pre-computed materialized constructor thunk directly.
                Arc::clone(ctor_thunk)
            } else {
                match &entry.node.value.node {
                    Expr::Int(n) => Arc::new(Thunk::new_materialized(
                        Value::Int(*n),
                        entry.node.value.span,
                    )),
                    Expr::Float(f) => Arc::new(Thunk::new_materialized(
                        Value::Float(*f),
                        entry.node.value.span,
                    )),
                    Expr::Bool(b) => Arc::new(Thunk::new_materialized(
                        Value::Bool(*b),
                        entry.node.value.span,
                    )),
                    Expr::Str(s) => Arc::new(Thunk::new_materialized(
                        string_val(s),
                        entry.node.value.span,
                    )),
                    _ if is_static_key => Arc::new(Thunk::new_unevaluated_with_env_id(
                        Rc::clone(&entry.node.value),
                        Arc::clone(&dict_env),
                        env_id,
                        Arc::clone(ctx),
                        entry.node.value.span,
                    )),
                    _ => Arc::new(Thunk::new_unevaluated(
                        Rc::clone(&entry.node.value),
                        Arc::clone(&dict_env),
                        Arc::clone(ctx),
                        entry.node.value.span,
                    )),
                }
            }
        } else {
            match &entry.node.value.node {
                Expr::Int(n) => Arc::new(Thunk::new_materialized(
                    Value::Int(*n),
                    entry.node.value.span,
                )),
                Expr::Float(f) => Arc::new(Thunk::new_materialized(
                    Value::Float(*f),
                    entry.node.value.span,
                )),
                Expr::Bool(b) => Arc::new(Thunk::new_materialized(
                    Value::Bool(*b),
                    entry.node.value.span,
                )),
                Expr::Str(s) => Arc::new(Thunk::new_materialized(
                    string_val(s),
                    entry.node.value.span,
                )),
                _ if is_static_key => Arc::new(Thunk::new_unevaluated_with_env_id(
                    Rc::clone(&entry.node.value),
                    Arc::clone(&dict_env),
                    env_id,
                    Arc::clone(ctx),
                    entry.node.value.span,
                )),
                _ => Arc::new(Thunk::new_unevaluated(
                    Rc::clone(&entry.node.value),
                    Arc::clone(&dict_env),
                    Arc::clone(ctx),
                    entry.node.value.span,
                )),
            }
        };

        // String keys become bindings so sibling entries can reference via $name.
        // All string-keyed entries are inserted into dict_env in AST order (preserving
        // slot index consistency with the resolver). Constructor re-exports like `Ok: Ok`
        // get the pre-computed materialized constructor thunk (set above), so they don't
        // overwrite the constructor with a self-referential unevaluated thunk.
        if let Key::String(ref name) = key {
            dict_env
                .write()
                .unwrap()
                .insert(name.clone(), Arc::clone(&thunk));
        }

        // Check for duplicate keys using insert(), which returns Some(old_value) if present.
        // This fuses the contains_key + insert operations into a single lookup.
        // Note: ctx.alloc_thunk() and dict_env are updated before duplicate detection.
        // Both are abandoned when Err is returned; the arena allocation is a minor leak
        // on duplicate-key error paths (benign since the arena is dropped with the context).
        let thunk_id = ctx.alloc_thunk(thunk);
        if dict_map.insert(key.clone(), thunk_id).is_some() {
            return Err(Box::new(EvalError::duplicate_key(
                &key.to_string(),
                entry.span,
            )));
        }

        // Fill the FlatEnv slot for this static-key entry.
        // Slot indices must match those assigned by resolve.rs exactly (sequential,
        // incremented only for Expr::Str | Expr::Annotated key entries).
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

pub(crate) async fn eval_key(
    key_expr: &Spanned<Expr>,
    parent_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Key> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval(Rc::new(key_expr.clone()), Arc::clone(parent_env), ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}

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
// CoreExpr path: eval_dict_core / eval_key_core
//
// These functions accept `CoreEntry` / `CoreExpr` slices directly, eliminating
// the CoreExpr→Expr round-trip that `eval_core_expr`'s Dict arm previously
// performed (collecting a Vec<Spanned<Entry>> before calling eval_dict).
//
// Non-literal entries use Thunk::new_unevaluated_core (UnevaluatedState::CoreExpr) —
// no CoreExpr→Expr round-trip for dict values.
// TODO(parts-e): once the Expr-based eval_dict/eval_key are fully retired, delete them.
//
// Note: TypeAlias constructor pre-computation is OMITTED here intentionally.
// CoreExpr::Dict entries never contain CoreExpr::TypeAlias (declaration forms
// become CoreExpr::Error during expr_to_core_expr / lowering). The TypeAlias
// pre-pass in eval_dict is dead code post-E1; tracked as a pre-existing
// regression in TODO.md (runtime-v2-fix-class-instance-in-dict).
// ============================================================================

/// Count static string-keyed entries in a CoreEntry slice.
///
/// Static string keys are those that produce `Key::String` bindings visible to
/// sibling VarRef lookups: `CoreExpr::Str` and `CoreExpr::Annotated`. Must stay
/// in sync with `count_static_keys` and the resolver's walk_expr Dict arm.
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
