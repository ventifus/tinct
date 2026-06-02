//! Dict construction and letrec scoping: `eval_dict_core`, `eval_key_core`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! All evaluation is CoreExpr-native via `eval_dict_core` / `eval_key_core`.
//! The old Expr-based `eval_dict` / `eval_key` were removed in the Parts-B+E migration.

use std::rc::Rc;
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
        } => Ok(Key::String(Rc::from(&source[*start..*end]))),
        Value::Int(n) => Ok(Key::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), span.clone()).into()),
    }
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
/// A key is static iff it is a bare-word string (`CoreExpr::Str`) or an annotated bare-word
/// (`CoreExpr::Annotated`). All other key forms (variable references, function calls, etc.)
/// are computed at runtime and are excluded from letrec scope and slot assignment.
///
/// **Invariant**: `CoreExpr::Annotated { name, .. }` always has a static `name: String` that is
/// a bare identifier parsed from source (e.g., `Fn@Number` → name="Fn"). The parser creates
/// `Annotated` only for `Token::Identifier` followed by `@` (parser.rs:3232-3254). Therefore,
/// `CoreExpr::Annotated` keys are always static and suitable for letrec scope and slot assignment.
///
/// **Must stay in sync with `resolve.rs` `surface_dict_static_keys` / `surface_node_static_keys`.**
/// Both the resolver and all three runtime insertion sites (eval_dict.rs, eval.rs Sequential,
/// eval.rs document pipeline) use this predicate so that the slot indices they assign and count
/// agree exactly.
pub(crate) fn core_expr_is_static_key(k: &CoreExpr) -> bool {
    matches!(k, CoreExpr::Str(_) | CoreExpr::Annotated { .. })
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
///
/// **B-296 constructor injection**: Unit constructors from `CoreExpr::TypeDecl` entries
/// are injected directly into `dict_env` as materialized Variant thunks. This replaces
/// the desugar-pass injection for unit constructors (field constructors still use desugar
/// pass for now).
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    parent_env: &Arc<RwLock<Environment>>,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // Task 6: Skip dict_env allocation for literal-only dicts.
    // Check if all values are literals (Int/Float/Bool/Str) - if so, we don't need letrec scoping.
    let has_non_literal = entries.iter().any(|entry| {
        !matches!(
            &entry.node.value.node,
            CoreExpr::Int(_) | CoreExpr::Float(_) | CoreExpr::Bool(_) | CoreExpr::Str(_)
        )
    });

    let dict_env = if has_non_literal {
        Some(Arc::new(RwLock::new(Environment::with_parent(Arc::clone(
            parent_env,
        )))))
    } else {
        None
    };
    let mut dict_map: IndexMap<Key, ThunkId> = IndexMap::with_capacity(entries.len());
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

    // B-296: Pre-scan for TypeDecl entries and inject unit constructors.
    // This matches the desugar-pass behavior where constructors are inserted at the
    // BEGINNING of the dict (before other entries). We inject into both dict_map
    // (as dict fields) and dict_env (as letrec bindings).
    //
    // Tracks injected names so the main loop can skip the corresponding desugar-pass
    // entries (which produce identical `Ctor: [variant "CtorName"]` bindings). Without
    // this skip, both mechanisms would insert the same key into dict_map and trigger
    // E030. T-902 will delete the desugar pass entirely, at which point
    // pre_injected_constructors will always be empty and this guard becomes a no-op.
    let mut pre_injected_constructors: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    if let Some(ref d_env) = dict_env.as_ref() {
        for entry in entries {
            if let CoreExpr::TypeDecl { unit_constructors } = &entry.node.value.node {
                for ctor_name in unit_constructors {
                    // Create materialized Variant thunk with no payload
                    let variant_thunk = Arc::new(Thunk::new_materialized(
                        Value::Variant {
                            tag: ctor_name.clone(),
                            payload: None,
                        },
                        entry.span.clone(),
                    ));
                    let thunk_id = ctx.alloc_thunk(Arc::clone(&variant_thunk));

                    // Insert into dict_map as a field entry (at current position)
                    dict_map.insert(Key::String(Rc::from(ctor_name.as_str())), thunk_id);

                    // Insert into dict_env so sibling entries can reference it
                    d_env
                        .write()
                        .unwrap()
                        .insert(ctor_name.clone(), Arc::clone(&variant_thunk));

                    // Record name so the main loop skips the desugar-pass duplicate
                    pre_injected_constructors.insert(ctor_name.clone());
                }
            }
        }
    }

    for entry in entries {
        // B-296: Skip TypeDecl entries — they don't become dict fields, only inject constructors.
        // The constructors were already injected in the pre-scan above.
        if matches!(&entry.node.value.node, CoreExpr::TypeDecl { .. }) {
            continue;
        }

        // Determine if this entry has a static key (CoreExpr::Str or CoreExpr::Annotated).
        // Must match resolve.rs Resolver::walk_expr Dict arm exactly — use the shared predicate.
        let is_static_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| core_expr_is_static_key(&k.node));

        let key = match &entry.node.key {
            Some(key_expr) => eval_key_core(key_expr, parent_env, ctx).await?,
            None => {
                let k = Key::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span.clone())
                })?;
                k
            }
        };

        // Fast path for literal values: Materialized thunks directly (Nix maybeThunk pattern).
        // Non-literal values become CoreExpr thunks pointing to dict_env.
        let thunk = match &entry.node.value.node {
            CoreExpr::Int(n) => Arc::new(Thunk::new_materialized(
                Value::Int(*n),
                entry.node.value.span.clone(),
            )),
            CoreExpr::Float(f) => Arc::new(Thunk::new_materialized(
                Value::Float(*f),
                entry.node.value.span.clone(),
            )),
            CoreExpr::Bool(b) => Arc::new(Thunk::new_materialized(
                Value::Bool(*b),
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

        // B-296 deduplication: check whether this key was already inserted into dict_map
        // by the pre-scan (TypeDecl constructor injection). If so, skip the dict_map insert
        // to avoid an E030 duplicate key error. The FlatEnv slot is still filled below so
        // that slot_idx stays aligned with the resolver's slot assignment — the desugar
        // thunk and the pre-scan's materialized Variant thunk evaluate to the same value.
        //
        // T-902 will delete the desugar pass entirely; at that point pre_injected_constructors
        // is always empty and this branch is dead code.
        let is_pre_injected = if !pre_injected_constructors.is_empty() {
            matches!(&key, Key::String(name) if pre_injected_constructors.contains(name.as_ref()))
        } else {
            false
        };

        // String keys become bindings so sibling entries can reference via $name (letrec).
        // Only insert if we have a dict_env (i.e., if there are non-literals).
        // CRITICAL: Only insert static-key entries to preserve slot alignment with the resolver.
        // Computed-key entries (even if they evaluate to strings) are NOT part of the letrec scope.
        if is_static_key {
            if let Key::String(ref name) = key {
                if let Some(ref env) = dict_env {
                    env.write()
                        .unwrap()
                        .insert(name.to_string(), Arc::clone(&thunk));
                }
            }
        }

        let thunk_id = ctx.alloc_thunk(thunk);
        if is_pre_injected {
            // The pre-scan already inserted a materialized Variant thunk for this constructor
            // into dict_map. Keep the pre-scan's thunk (avoid the unevaluated desugar thunk).
            // Still fall through to fill the FlatEnv slot for variable-reference correctness.
        } else {
            // Task 1: Move key into dict_map instead of cloning (saves Rc::from allocation per entry).
            // If duplicate, reconstruct key string from entry (rare error path).
            if dict_map.insert(key, thunk_id).is_some() {
                // key was moved; reconstruct string representation from entry for error message
                let key_str = match &entry.node.key {
                    Some(k_expr) => match &k_expr.node {
                        CoreExpr::Str(s) => s.clone(),
                        CoreExpr::Int(n) => n.to_string(),
                        CoreExpr::Annotated { name, .. } => name.clone(),
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
    // static-key count per scope. A runtime recount of CoreExpr::Str | CoreExpr::Annotated
    // entries matches the same predicate used above to compute slot_idx, so any such
    // assert_eq would be tautological (comparing a value against itself). A proper
    // resolver/runtime alignment check requires storing the resolver's slot-count per
    // env_id and comparing here; tracked as a future improvement.

    Ok(Arc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        dict_span.clone(),
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
    // Fast path for static keys — avoids thunk creation and materialization
    match &key_expr.node {
        CoreExpr::Str(s) => return Ok(Key::String(Rc::from(s.as_str()))),
        CoreExpr::Int(n) => return Ok(Key::Int(*n)),
        // Annotated keys (e.g., `name@[doc: "..."]`) always resolve to the bare name.
        // eval_core_expr for CoreExpr::Annotated already returns string_val(name);
        // skipping the thunk/materialize round-trip is both faster and avoids any
        // environment-dependent lookup that could produce a wrong result.
        CoreExpr::Annotated { name, .. } => return Ok(Key::String(Rc::from(name.as_str()))),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval_core_expr(key_expr.as_ref(), parent_env, ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}

#[cfg(test)]
mod tests {
    // Tests currently use only items from parent module's glob re-exports

    /// Test that dict keys are evaluated in the parent scope, not the dict scope.
    /// Per letrec semantics: keys see parent bindings, values see sibling bindings.
    #[test]
    fn test_key_evaluated_in_parent_scope() {
        // [x: 1  y: $x]  -- $x in value position should see sibling x
        // [outer: 1  inner: [x: 2  y: $outer]]  -- $outer in value should see parent outer
        // [outer: 1  inner: [x: 2  $outer: 3]]  -- $outer in KEY position should also see parent outer (key=1)
        let input = r#"
            [outer: 1  inner: [x: 2  $outer: 999]]
        "#;
        let result = crate::eval_source(input);
        assert!(result.is_ok(), "Should succeed: {:?}", result);
        let output = result.unwrap();
        // The key $outer evaluates to 1 (from parent scope) — Value::Int(1) → Key::Int(1).
        // Key::Int is formatted as the bare integer (e.g., "1: Int(999)"), not "Int(1): Int(999)".
        assert!(
            output.contains("1: Int(999)"),
            "Key $outer should evaluate to 1 (parent scope), got: {}",
            output
        );
    }

    /// Test that dict values are evaluated in the dict's own scope (letrec).
    /// Sibling entries are visible to each other (forward references allowed).
    #[test]
    fn test_value_evaluated_in_dict_scope() {
        // [x: 1  y: $x]  -- $x in value position should resolve to sibling x (value 1)
        let input = r#"[x: 1  y: $x]"#;
        let result = crate::eval_source(input);
        assert!(result.is_ok(), "Should succeed: {:?}", result);
        let output = result.unwrap();
        assert!(
            output.contains(r#""y": Int(1)"#),
            "y should reference sibling x (value 1), got: {}",
            output
        );
    }

    /// Test that circular dependencies are detected.
    /// [x: $y  y: $x] should produce a cycle error.
    #[test]
    fn test_circular_dependency_detection() {
        let input = r#"[x: $y  y: $x]"#;
        let result = crate::eval_source(input);
        assert!(
            result.is_err(),
            "Should fail with circular dependency error"
        );
        let err = result.unwrap_err();
        assert!(
            err.contains("circular dependency"),
            "Error should mention circular dependency, got: {}",
            err
        );
    }

    /// Test that nested dicts properly shadow outer bindings.
    /// [x: 1  inner: [x: 2  y: $x]]  -- $x in inner.y should see inner.x (2), not outer.x (1)
    #[test]
    fn test_nested_dict_shadowing() {
        let input = r#"[x: 1  inner: [x: 2  y: $x]]"#;
        let result = crate::eval_source(input);
        assert!(result.is_ok(), "Should succeed: {:?}", result);
        let output = result.unwrap();
        // The inner dict should have y: 2 (shadowed x)
        assert!(
            output.contains(r#""y": Int(2)"#),
            "inner.y should reference inner.x (2, shadowed), got: {}",
            output
        );
    }

    /// Test that a literal-only dict evaluates correctly via the no-dict_env fast path.
    /// When all dict values are literals (Int/Float/Bool/Str), `eval_dict_core` skips
    /// the letrec Environment allocation entirely (`dict_env = None`). This verifies
    /// both correctness of the fast path and that keys/values are emitted in the right order.
    #[test]
    fn test_literal_only_dict_fast_path() {
        let input = r#"[a: 1  b: 2]"#;
        let result = crate::eval_source(input);
        assert!(
            result.is_ok(),
            "literal-only dict should evaluate without error: {:?}",
            result
        );
        let output = result.unwrap();
        // Both entries must appear with the correct Display representation
        assert!(
            output.contains(r#""a": Int(1)"#),
            "expected 'a': Int(1) in output, got: {}",
            output
        );
        assert!(
            output.contains(r#""b": Int(2)"#),
            "expected 'b': Int(2) in output, got: {}",
            output
        );
    }
}
