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
        // avoiding Unevaluated → Materialized state transition overhead (Nix maybeThunk pattern)
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
        if dict_map
            .insert(key.clone(), ctx.alloc_thunk(thunk))
            .is_some()
        {
            return Err(Box::new(EvalError::duplicate_key(
                &key.to_string(),
                entry.span,
            )));
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
