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

use super::{eval, materialize, EvalContext};

pub(crate) fn eval_dict(
    entries: &[Spanned<Entry>],
    parent_env: &Rc<RefCell<Environment>>,
    ctx: &Rc<EvalContext>,
    dict_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let dict_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    for entry in entries {
        let key = match &entry.node.key {
            // Keys are evaluated in the parent scope, not dict_env, because key
            // expressions must not see sibling bindings. This prevents keys from
            // depending on values that are still unevaluated thunks and keeps
            // key evaluation deterministic regardless of entry order.
            Some(key_expr) => eval_key(key_expr, parent_env, ctx, depth)?,
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
                // TODO(ast-rc): `entry.node.value` is `Spanned<Expr>` (owned), so
                // `Rc::new(...clone())` deep-clones the AST subtree on every
                // eval_dict invocation.  Eliminating this requires migrating
                // `Entry::value` to `Rc<Spanned<Expr>>` in ast.rs and updating
                // the parser, formatter, typecheck, lsp/analysis, and all eval
                // call sites (~36 occurrences across 6 files).  Deferred to a
                // dedicated AST-RC migration sprint.
                Rc::clone(&entry.node.value),
                Rc::clone(&dict_env),
                Rc::clone(ctx),
                entry.node.value.span,
            )),
        };

        // String keys become bindings so sibling entries can reference via $name
        if let Key::String(ref name) = key {
            dict_env
                .borrow_mut()
                .insert(name.clone(), Rc::clone(&thunk));
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
    depth: usize,
) -> EvalResult<Key> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval(
        Rc::new(key_expr.clone()),
        Rc::clone(parent_env),
        ctx,
        depth + 1,
    )?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx, depth + 1)?;
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
