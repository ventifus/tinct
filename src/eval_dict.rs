//! Dict construction and letrec scoping: `eval_dict_core`, `eval_key_core`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! All evaluation is CoreExpr-native via `eval_dict_core` / `eval_key_core`.

use std::rc::Rc;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::ast::{Annotation, CoreEntry, CoreExpr, Span, Spanned, SurfaceExpression};
use crate::error::{EvalError, EvalResult};
use crate::value::ThunkId;
use crate::value::{string_val, HashableValue, Thunk, Value};

use super::{eval_core_expr, materialize, EvalContext};

fn value_to_key(value: &Value, span: &Span) -> EvalResult<HashableValue> {
    match value {
        Value::String {
            ref source,
            start,
            end,
        } => Ok(HashableValue::Str(Rc::from(&source[*start..*end]))),
        Value::Int(n) => Ok(HashableValue::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), span.clone()).into()),
    }
}

/// Evaluate a PropertyDict annotation to a materialized Value::Dict.
///
/// The annotation PropertyDict contains SurfaceExpression nodes that need to be evaluated
/// in the given environment context. This function evaluates each entry's key and value,
/// materializes them, and builds a concrete Value::Dict.
///
/// Used by T-1119 to evaluate `@[...]` annotations on dict key entries.
fn eval_annotation_property_dict(
    entries: &[Spanned<crate::ast::SurfaceEntry>],
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value> {
    let mut dict_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    for entry in entries {
        // Evaluate the key to get a concrete HashableValue
        let key = if let Some(key_node) = &entry.node.key {
            // Evaluate the key expression (SurfaceExpression)
            // We need to lower it to CoreExpr first, then evaluate
            // For now, handle the common case of string literals directly
            match &key_node.expr {
                SurfaceExpression::Str(s) => HashableValue::Str(Rc::from(s.as_str())),
                SurfaceExpression::Int(n) => HashableValue::Int(*n),
                // U64 values that fit in i64 are used as integer keys; larger values error.
                SurfaceExpression::U64(n) => {
                    if let Ok(i) = i64::try_from(*n) {
                        HashableValue::Int(i)
                    } else {
                        return Err(Box::new(EvalError::internal(
                            format!(
                                "u64 key {n} is too large for a dict integer key (max i64::MAX)"
                            ),
                            key_node.span.clone(),
                        )));
                    }
                }
                SurfaceExpression::VarRef { name, .. } => {
                    HashableValue::Str(Rc::from(name.as_str()))
                }
                _ => {
                    // For complex key expressions, we'd need to lower and evaluate
                    // For now, treat as an error since annotation keys should be simple
                    return Err(Box::new(EvalError::internal(
                        format!(
                            "annotation property dict keys must be string literals, int literals, or bare words, got: {:?}",
                            key_node.expr
                        ),
                        key_node.span.clone(),
                    )));
                }
            }
        } else {
            // Auto-indexed entry
            let k = HashableValue::Int(auto_index);
            auto_index = auto_index.checked_add(1).ok_or_else(|| {
                EvalError::integer_overflow(
                    "annotation dict auto-index".to_string(),
                    entry.span.clone(),
                )
            })?;
            k
        };

        // Evaluate the value expression
        // We need to lower SurfaceExpression to CoreExpr, then evaluate it
        let value_thunk = {
            // Simple path: annotations should use literal values for now (T-1124 handles full eval at fn definition)
            match &entry.node.value.expr {
                SurfaceExpression::Str(s) => Arc::new(Thunk::new_materialized(
                    string_val(s),
                    entry.node.value.span.clone(),
                )),
                SurfaceExpression::Int(n) => Arc::new(Thunk::new_materialized(
                    Value::Int(*n),
                    entry.node.value.span.clone(),
                )),
                SurfaceExpression::U64(n) => Arc::new(Thunk::new_materialized(
                    Value::U64(*n),
                    entry.node.value.span.clone(),
                )),
                SurfaceExpression::Float(f) => Arc::new(Thunk::new_materialized(
                    Value::Float(*f),
                    entry.node.value.span.clone(),
                )),
                _ => {
                    // Non-literal annotation values (VarRef type names, fn expressions, etc.)
                    // are skipped — they appear in function return-type annotations like
                    // @[return: Dict  doc: "..."] where Dict is a type name, not a runtime value.
                    // T-1124 handles expression evaluation at fn-definition time for fn annotations;
                    // dict-key annotations only carry literal metadata (strings, ints, numbers).
                    continue;
                }
            }
        };

        let thunk_id = ctx.alloc_thunk(value_thunk);
        if dict_map.insert(key, thunk_id).is_some() {
            let key_str = match &entry.node.key {
                Some(k_node) => match &k_node.expr {
                    SurfaceExpression::Str(s) => s.clone(),
                    SurfaceExpression::Int(n) => n.to_string(),
                    SurfaceExpression::U64(n) => n.to_string(),
                    SurfaceExpression::VarRef { name, .. } => name.clone(),
                    _ => "<computed key>".to_string(),
                },
                None => (auto_index - 1).to_string(),
            };
            return Err(Box::new(EvalError::duplicate_key(
                &key_str,
                entry.span.clone(),
            )));
        }
    }

    Ok(Value::Dict(dict_map))
}

// ============================================================================
// eval_dict_core / eval_key_core
//
// These functions accept `CoreEntry` / `CoreExpr` slices directly.
// Non-literal entries use Thunk::new_unevaluated_core (UnevaluatedState::CoreExpr) —
// no CoreExpr→Expr round-trip for dict values.
//
// Note: TypeAlias / ClassDecl / InstanceDecl declaration forms in dict value position
// are handled at compile time (type checker Pass 0c, typecheck_dict.rs) and skipped
// at runtime via `continue` in lower.rs (SurfaceExpression::Decl match arm). They
// produce no runtime entry — this is correct behavior. The type checker registers
// class/instance declarations found inside dicts before the SCC inference loop, so
// all declarations are visible regardless of order. FD consistency checks fire
// correctly for class/instance inside dict values (fixed in B-164).
// ============================================================================

/// Returns `true` if a dict key expression is "static" — i.e., its name is known at compile
/// time and the resolver assigned it a slot index.
///
/// A key is static iff it is a bare-word string (`CoreExpr::Str`) or an annotated var
/// (`CoreExpr::Var { annotation: Some(_) }`). All other key forms (variable references,
/// function calls, etc.) are computed at runtime and are excluded from letrec scope and
/// slot assignment.
///
/// **Invariant**: `CoreExpr::Var { annotation: Some(_), .. }` always has a static `name: String`
/// that is a bare identifier parsed from source (e.g., `Fn@Number` → name="Fn"). The parser
/// creates annotated VarRefs only for `Token::Identifier` followed by `@`. Therefore,
/// annotated Var keys are always static and suitable for letrec scope and slot assignment.
///
/// **Must stay in sync with `resolve.rs` `surface_dict_static_keys` / `surface_node_static_keys`.**
/// Both the resolver and all three runtime insertion sites (eval_dict.rs, eval.rs Sequential,
/// eval.rs document pipeline) use this predicate so that the slot indices they assign and count
/// agree exactly.
pub(crate) fn core_expr_is_static_key(k: &CoreExpr) -> bool {
    // Annotated VarRef (Var { annotation: Some(_) }) is now a static key too.
    matches!(
        k,
        CoreExpr::Str(_)
            | CoreExpr::Var {
                annotation: Some(_),
                ..
            }
    )
}

/// Evaluate a dict literal from `CoreExpr::Dict` entries with letrec semantics.
///
/// Directly accepts the `CoreEntry` slice produced by `eval_core_expr`'s Dict arm.
///
/// Semantics are identical to `eval_dict`:
/// - String-keyed entries enter `dict_env` (letrec: forward references allowed)
/// - Keys evaluated in `parent_env` (Key Isolation Invariant)
/// - Literal values (Int/Float/Bool/Str) get Materialized thunks directly (fast path)
/// - Non-literal values become CoreExpr thunks in `dict_env` (UnevaluatedState::CoreExpr)
///   — no CoreExpr→Expr round-trip for dict values.
///
/// **Constructor dict**: Constructors (unit and named-field) are produced by the lower.rs
/// pass (T-1193) as entries in the runtime constructor dict. No runtime pre-scan is needed —
/// there are no `CoreExpr::TypeDecl` entries in the lowered AST.
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    parent_env: &Arc<RwLock<crate::env::Env>>,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // Task 6: Skip dict_env allocation for literal-only dicts.
    // Check if all values are literals (Int/Float/Str) - if so, we don't need letrec scoping.
    let has_non_literal = entries.iter().any(|entry| {
        !matches!(
            &entry.node.value.node,
            CoreExpr::Int(_) | CoreExpr::U64(_) | CoreExpr::Float(_) | CoreExpr::Str(_)
        )
    });

    let dict_env = if has_non_literal {
        Some(Arc::new(RwLock::new(crate::env::Env::with_parent(
            Arc::clone(parent_env),
        ))))
    } else {
        None
    };
    let mut dict_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // Allocate a FlatEnv for this dict scope with entries.len() capacity (upper bound).
    // This avoids the count_static_keys_core() pass. May slightly over-allocate when
    // some entries have computed keys, but the single-pass is faster than counting first.
    // Only allocate if we have non-literals that need env slots.
    let env_id = if has_non_literal {
        Some(ctx.env_arena.lock().unwrap().alloc_root(entries.len()))
    } else {
        None
    };
    let mut slot_idx: u32 = 0;
    // Collect (slot_idx, thunk_id) pairs for static-key entries so we can
    // batch-acquire the arena lock once after the loop instead of once per entry.
    // The lock cannot be held across the .await in eval_key_core, so we must
    // collect first and write after.
    let mut letrec_slots: Vec<(u32, ThunkId)> = Vec::new();

    for entry in entries {
        // Determine if this entry has a static key (CoreExpr::Str or annotated Var).
        // Must match resolve.rs Resolver::walk_expr Dict arm exactly — use the shared predicate.
        let is_static_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| core_expr_is_static_key(&k.node));

        let key = match &entry.node.key {
            Some(key_expr) => eval_key_core(key_expr, parent_env, ctx).await?,
            None => {
                let k = HashableValue::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span.clone())
                })?;
                k
            }
        };

        // Fast path for literal values: Materialized thunks directly (Nix maybeThunk pattern).
        // Non-literal values become CoreExpr thunks pointing to dict_env.
        let value_thunk = match &entry.node.value.node {
            CoreExpr::Int(n) => Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                entry.node.value.span.clone(),
            )),
            CoreExpr::U64(n) => Arc::new(Thunk::new_materialized(
                Value::U64(*n),
                entry.node.value.span.clone(),
            )),
            CoreExpr::Float(f) => Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                entry.node.value.span.clone(),
            )),
            CoreExpr::Str(s) => Arc::new(Thunk::new_materialized(
                string_val(s),
                entry.node.value.span.clone(),
            )),
            // Non-literal: use UnevaluatedState::CoreExpr.
            _ => Arc::new(Thunk::new_unevaluated_core(
                Arc::clone(&entry.node.value),
                Arc::clone(
                    dict_env
                        .as_ref()
                        .expect("dict_env present for non-literals"),
                ),
                Arc::clone(ctx),
                entry.node.value.span.clone(),
            )),
        };

        // T-1119: If the key is annotated (e.g., Pi@[doc: "..."]), wrap the value in Value::Annotated.
        // The annotation PropertyDict is evaluated to a Value::Dict at dict construction time.
        let thunk = if let Some(key_expr) = &entry.node.key {
            if let CoreExpr::Var {
                annotation: Some(annotation),
                ..
            } = &key_expr.node
            {
                // Only PropertyDict annotations produce Value::Annotated at runtime.
                // Simple annotations (e.g., Pi@Number) are type-level metadata, not runtime values.
                if let Annotation::PropertyDict(ann_entries) = &annotation.node {
                    // Evaluate the annotation PropertyDict to a Value::Dict
                    let annotation_value = eval_annotation_property_dict(ann_entries, ctx)?;

                    // T-1123: Value::Annotated stores materialized inner.
                    // Only wrap in Value::Annotated when the inner value is already materialized
                    // (i.e., a literal: Int, Float, Bool, Str). For non-literal values (VarRefs,
                    // function calls, etc.), skip the wrapping and use the plain thunk. This
                    // preserves laziness: forcing a non-literal annotated entry at dict construction
                    // time would evaluate VarRefs against the current env, potentially failing if
                    // the referenced name is not yet in scope (e.g., net builtins like
                    // builtin-connect referenced in the prelude's annotated wrapper entries).
                    // The trade-off: annotation-of on a dict-key-annotated non-literal entry
                    // returns {} instead of the annotation dict — acceptable since the primary use
                    // case for annotation-of is functions (FnAnnotation.extra) and unit constructors
                    // (Value::Annotated from make-annotated), not arbitrary non-literal dict entries.
                    if let Some(inner_value) = value_thunk.try_get_materialized() {
                        // Wrap it in Value::Annotated
                        Arc::new(Thunk::new_materialized(
                            Value::Annotated {
                                inner: Box::new(inner_value),
                                annotation: Box::new(annotation_value),
                            },
                            entry.node.value.span.clone(),
                        ))
                    } else {
                        // Non-literal: skip Value::Annotated wrapping to preserve laziness
                        value_thunk
                    }
                } else {
                    // Simple/Annotated annotations are type-level only — use unannotated value
                    value_thunk
                }
            } else {
                value_thunk
            }
        } else {
            value_thunk
        };

        // String keys become bindings so sibling entries can reference via $name (letrec).
        // Only insert if we have a dict_env (i.e., if there are non-literals).
        // CRITICAL: Only insert static-key entries to preserve slot alignment with the resolver.
        // Computed-key entries (even if they evaluate to strings) are NOT part of the letrec scope.
        if is_static_key {
            if let HashableValue::Str(ref name) = key {
                if let Some(ref env) = dict_env {
                    env.write()
                        .unwrap()
                        .insert_value(name.to_string(), Arc::clone(&thunk));
                }
            }
        }

        // Task 1: Move key into dict_map instead of cloning (saves Rc::from allocation per entry).
        // If duplicate, reconstruct key string from entry (rare error path).
        let thunk_id = ctx.alloc_thunk(thunk);
        if dict_map.insert(key, thunk_id).is_some() {
            // key was moved; reconstruct string representation from entry for error message
            let key_str = match &entry.node.key {
                Some(k_expr) => match &k_expr.node {
                    CoreExpr::Str(s) => s.clone(),
                    CoreExpr::Int(n) => n.to_string(),
                    CoreExpr::U64(n) => n.to_string(),
                    CoreExpr::Var { name, .. } => name.clone(),
                    _ => "<computed key>".to_string(),
                },
                None => (auto_index - 1).to_string(),
            };
            return Err(Box::new(EvalError::duplicate_key(
                &key_str,
                entry.span.clone(),
            )));
        }

        if is_static_key && env_id.is_some() {
            letrec_slots.push((slot_idx, thunk_id));
            slot_idx += 1;
        }
    }

    // Batch-fill letrec slots: acquire the arena lock once for all static-key entries
    // instead of once per entry. This avoids repeated mutex lock/unlock overhead for
    // dicts with many string-keyed fields.
    if let Some(id) = env_id {
        if !letrec_slots.is_empty() {
            let mut arena_guard = ctx.env_arena.lock().unwrap();
            for (idx, thunk_id) in letrec_slots {
                arena_guard.fill_letrec_slot(id, idx, thunk_id);
            }
        }
    }

    // Note: a meaningful resolver/runtime drift check would compare slot_idx against
    // the slot count recorded by the resolver for this dict's env_id. That information
    // is not currently stored in EvalContext in a queryable form — the resolver writes
    // slot indices into CoreExpr::Var nodes but does not separately record the total
    // static-key count per scope. A runtime recount of CoreExpr::Str | CoreExpr::Var(annotated)
    // entries matches the same predicate used above to compute slot_idx, so any such
    // assert_eq would be tautological (comparing a value against itself). A proper
    // resolver/runtime alignment check requires storing the resolver's slot-count per
    // env_id and comparing here; tracked as a future improvement.

    Ok(Arc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        dict_span.clone(),
    )))
}

/// Evaluate a dict key from a `CoreExpr` node, returning a concrete `HashableValue`.
///
/// Fast path for literal keys (Str/Int) avoids creating temporary thunks.
/// General path materializes the expression via `eval_core_expr`.
pub(crate) async fn eval_key_core(
    key_expr: &Arc<Spanned<CoreExpr>>,
    parent_env: &Arc<RwLock<crate::env::Env>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<HashableValue> {
    // Fast path for static keys — avoids thunk creation and materialization
    match &key_expr.node {
        CoreExpr::Str(s) => return Ok(HashableValue::Str(Rc::from(s.as_str()))),
        CoreExpr::Int(n) => return Ok(HashableValue::Int(*n)),
        // U64 keys that fit in i64 are used as integer keys; larger values error.
        CoreExpr::U64(n) => {
            if let Ok(i) = i64::try_from(*n) {
                return Ok(HashableValue::Int(i));
            }
            return Err(EvalError::internal(
                format!("u64 key {n} is too large for a dict integer key (max i64::MAX)"),
                key_expr.span.clone(),
            )
            .into());
        }
        // Annotated keys (e.g., `name@[doc: "..."]`) always resolve to the bare name.
        // The key is a Var { annotation: Some(_) } — the name field is the bare identifier.
        // Skip the thunk/materialize round-trip to use the name directly.
        CoreExpr::Var {
            name,
            annotation: Some(_),
            ..
        } => {
            return Ok(HashableValue::Str(Rc::from(name.as_str())));
        }
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete HashableValue keys
    let thunk = eval_core_expr(key_expr.as_ref(), parent_env, ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}
