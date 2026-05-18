//! Dict construction and letrec scoping: `eval_dict`, `eval_key`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{Entry, Expr, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, Environment, Key, Thunk, Value};

use super::{eagerly_register_constructors, eval, materialize, EvalContext};

/// Count the number of static string-keyed entries in a dict.
///
/// Static string keys are those that produce `Key::String` bindings visible to
/// sibling VarRef lookups: `Expr::Str` and `Expr::Annotated`. This count is used
/// to pre-size the FlatEnv slot vector for O(1) VarRef lookup.
///
/// Matches the static key logic in `resolve.rs::Resolver::walk_expr` (Dict arm).
#[allow(dead_code)]
fn count_static_keys(entries: &[Spanned<Entry>]) -> usize {
    entries
        .iter()
        .filter(|entry| {
            entry.node.key.as_ref().is_some_and(|key_expr| {
                matches!(
                    &key_expr.node,
                    Expr::Str(_) | Expr::Annotated { .. }
                )
            })
        })
        .count()
}

pub(crate) fn eval_dict(
    entries: &[Spanned<Entry>],
    parent_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Rc<Thunk>> {
    let dict_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // Phase 3 (arena-eval): Allocate a FlatEnv for this dict scope.
    //
    // The FlatEnv stores ThunkIds for static-key entries, enabling O(1) variable lookup
    // when VarRef nodes have been assigned (level, slot) coordinates by the resolver.
    // We use alloc_root (no parent FlatEnv) because the parent chain is still Rc-based;
    // O(1) applies only to level=0 (same-dict sibling references). Outer-scope references
    // fall back to the chain-based Environment.get_by_slot / Environment.get path.
    //
    // env_id is stored in Unevaluated thunks for future use when take_unevaluated is
    // updated to propagate it. For now it acts as scaffolding (the evaluator discards it).
    let static_key_count = count_static_keys(entries);
    let env_id = ctx.env_arena.borrow_mut().alloc_root(static_key_count);

    // Slot index counter: incremented only for static-key entries (matching resolver logic).
    // Must stay in sync with resolve.rs Resolver::walk_expr Dict arm (Expr::Str | Expr::Annotated).
    let mut slot_idx: u32 = 0;

    // Pre-pass: eagerly register nominal variant constructors from TypeAlias entries.
    //
    // Problem: `Ok: Ok` is a self-referential letrec thunk. When forced, it evaluates
    // VarRef "Ok", which looks up dict_env["Ok"] — the same `Ok: Ok` thunk — causing
    // E070 circular dependency.
    //
    // Fix: scan for TypeAlias entries *before* creating any thunks, and insert their
    // constructors into dict_env as materialized thunks. Then, in the main pass below,
    // skip dict_env.insert for keys that match a pre-registered constructor name (so the
    // `Ok: Ok` lazy thunk does NOT overwrite the constructor in dict_env). When `Ok: Ok`
    // is forced, VarRef "Ok" finds the pre-registered constructor thunk — no cycle.
    //
    // The `Ok: Ok` entry still appears in dict_map (the exported dict value), so callers
    // that include the prelude can access `Ok` as a dict field.
    let mut pre_registered_constructors: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for entry in entries {
        if let Expr::TypeAlias { params: _, body } = &entry.node.value.node {
            eagerly_register_constructors(&body.node, entry.node.value.span, &dict_env);
            for (tag, _) in super::extract_nominal_constructors(&body.node) {
                pre_registered_constructors.insert(tag);
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
            Some(key_expr) => eval_key(key_expr, parent_env, ctx)?,
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
        let thunk = match &entry.node.value.node {
            Expr::Int(n) => Rc::new(Thunk::new_materialized(
                Value::Int(*n),
                entry.node.value.span,
            )),
            Expr::Float(f) => Rc::new(Thunk::new_materialized(
                Value::Float(*f),
                entry.node.value.span,
            )),
            Expr::Bool(b) => Rc::new(Thunk::new_materialized(
                Value::Bool(*b),
                entry.node.value.span,
            )),
            Expr::Str(s) => Rc::new(Thunk::new_materialized(
                string_val(s),
                entry.node.value.span,
            )),
            _ if is_static_key => Rc::new(Thunk::new_unevaluated_with_env_id(
                Rc::clone(&entry.node.value),
                Rc::clone(&dict_env),
                env_id,
                Rc::clone(ctx),
                entry.node.value.span,
            )),
            _ => Rc::new(Thunk::new_unevaluated(
                Rc::clone(&entry.node.value),
                Rc::clone(&dict_env),
                Rc::clone(ctx),
                entry.node.value.span,
            )),
        };

        // String keys become bindings so sibling entries can reference via $name.
        // Exception: skip dict_env.insert for keys that were pre-registered as nominal
        // variant constructors in the pre-pass above. Those entries (e.g. `Ok: Ok`) must
        // resolve `Ok` to the pre-registered constructor thunk, not to the `Ok: Ok` thunk
        // itself (which would create a circular dependency / E070 error).
        if let Key::String(ref name) = key {
            if !pre_registered_constructors.contains(name.as_str()) {
                dict_env
                    .borrow_mut()
                    .insert(name.clone(), Rc::clone(&thunk));
            }
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
                .borrow_mut()
                .fill_letrec_slot(env_id, slot_idx, thunk_id);
            slot_idx += 1;
        }
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        *dict_span,
    )))
}

pub(crate) fn eval_key(
    key_expr: &Spanned<Expr>,
    parent_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
) -> EvalResult<Key> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval(Rc::new(key_expr.clone()), Rc::clone(parent_env), ctx)?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx)?;
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
