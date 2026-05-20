//! Dict construction and letrec scoping: `eval_dict`, `eval_key`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{Entry, Expr, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, Environment, Key, Thunk, Value};

use super::{eval, extract_nominal_constructors, materialize, EvalContext, VARIANT_TAG_MARKER};

/// Count the number of static string-keyed entries in a dict.
///
/// Static string keys are those that produce `Key::String` bindings visible to
/// sibling VarRef lookups: `Expr::Str` and `Expr::Annotated`. This count is used
/// to pre-size the FlatEnv slot vector for O(1) VarRef lookup.
///
/// Matches the static key logic in `resolve.rs::Resolver::walk_expr` (Dict arm).
#[allow(dead_code)] // arena-phase3 scaffolding: will be used when FlatEnv allocation is re-enabled
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
    let mut constructor_precomputed: std::collections::HashMap<String, Rc<Thunk>> =
        std::collections::HashMap::new();

    for entry in entries {
        if let Expr::TypeAlias { params: _, body } = &entry.node.value.node {
            let span = entry.node.value.span;
            for (tag, has_payload) in extract_nominal_constructors(&body.node) {
                let constructor_value = if has_payload {
                    let constructor_env =
                        Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&dict_env))));
                    constructor_env.borrow_mut().insert(
                        VARIANT_TAG_MARKER.to_string(),
                        Rc::new(Thunk::new_materialized(string_val(&tag), span)),
                    );
                    let param = Param {
                        name: "payload".to_string(),
                        annotation: None,
                        variadic: false,
                    };
                    let body_expr = Rc::new(Spanned::new(
                        Expr::VarRef {
                            name: "payload".to_string(),
                            escaped: false,
                            resolved: RefCell::new(None),
                        },
                        span,
                    ));
                    Value::Function {
                        params: Rc::new(vec![param]),
                        body: body_expr,
                        env: constructor_env,
                        env_id: None,
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
                    Rc::new(Thunk::new_materialized(constructor_value, span)),
                );
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
        // avoiding Unevaluated → Materialized state transition overhead (Nix maybeThunk pattern).
        //
        // Special case: if this entry's string key is a pre-computed constructor (e.g. `Ok: Ok`),
        // substitute the materialized constructor thunk directly. This breaks the circular
        // dependency where forcing `Ok: Ok` → VarRef("Ok") → dict_env["Ok"] → same thunk → cycle.
        let thunk = if let Key::String(ref name) = key {
            if let Some(ctor_thunk) = constructor_precomputed.get(name.as_str()) {
                // Use the pre-computed materialized constructor thunk directly.
                Rc::clone(ctor_thunk)
            } else {
                match &entry.node.value.node {
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
                }
            }
        } else {
            match &entry.node.value.node {
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
            }
        };

        // String keys become bindings so sibling entries can reference via $name.
        // All string-keyed entries are inserted into dict_env in AST order (preserving
        // slot index consistency with the resolver). Constructor re-exports like `Ok: Ok`
        // get the pre-computed materialized constructor thunk (set above), so they don't
        // overwrite the constructor with a self-referential unevaluated thunk.
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
        let thunk_id = ctx.alloc_thunk(thunk);
        if dict_map.insert(key.clone(), thunk_id).is_some() {
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
