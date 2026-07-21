//! Dict construction and letrec scoping: `eval_dict_core`, `eval_key_core`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! All evaluation is CoreExpr-native via `eval_dict_core` / `eval_key_core`.

use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{Annotation, CoreEntry, CoreExpr, Span, Spanned, SurfaceExpression};
use crate::error::{EvalError, EvalResult};
use crate::value::ThunkId;
use crate::value::{string_val, HashableValue, Thunk, Value};

use super::{materialize, EvalContext};
use crate::eval_core::eval_core_expr;

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
                SurfaceExpression::StringLiteral {
                    content: s,
                    delimiter,
                    ..
                } => {
                    let processed = if delimiter.len() == 1 {
                        crate::lower::process_escapes(s, delimiter)
                    } else {
                        s.clone()
                    };
                    HashableValue::Str(Rc::from(processed.as_str()))
                }
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
            // Simple path: annotations should use literal values for now (T-1124 handles full eval at fn definition).
            // See T-1620 for completing full expression evaluation for annotation property dict values.
            match &entry.node.value.expr {
                SurfaceExpression::StringLiteral {
                    content: s,
                    delimiter,
                    ..
                } => {
                    let processed = if delimiter.len() == 1 {
                        crate::lower::process_escapes(s, delimiter)
                    } else {
                        s.clone()
                    };
                    Arc::new(Thunk::value(
                        string_val(&processed),
                        entry.node.value.span.clone(),
                    ))
                }
                SurfaceExpression::Int(n) => {
                    Arc::new(Thunk::value(Value::Int(*n), entry.node.value.span.clone()))
                }
                SurfaceExpression::U64(n) => {
                    Arc::new(Thunk::value(Value::U64(*n), entry.node.value.span.clone()))
                }
                SurfaceExpression::Float(f) => Arc::new(Thunk::value(
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

        let thunk_id = ctx.alloc_thunk(0, value_thunk);
        if dict_map.insert(key, thunk_id).is_some() {
            let key_str = match &entry.node.key {
                Some(k_node) => match &k_node.expr {
                    SurfaceExpression::StringLiteral { content: s, .. } => s.clone(),
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
// Non-literal entries use Thunk::core_expr (UnevaluatedState::CoreExpr) —
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
/// Semantics: letrec scoping with FlatEnv display chain (see arena.rs).
/// - String-keyed entries allocated in child FlatEnv (letrec: forward references allowed)
/// - Keys evaluated in parent scope (Key Isolation Invariant)
/// - Literal values are pre-materialized at dict construction time — no environment lookup needed since literals are scope-independent
/// - Non-literal values become CoreExpr thunks in `dict_env` (UnevaluatedState::CoreExpr)
///   — no CoreExpr→Expr round-trip for dict values.
///
/// **Constructor dict**: Constructors (unit and named-field) are produced by the lower.rs
/// pass (T-1193) as entries in the runtime constructor dict. No runtime pre-scan is needed —
/// there are no `CoreExpr::TypeDecl` entries in the lowered AST.
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    parent_env_id: u32,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // dict_env (legacy Arc<RwLock<Env>>) removed — T-1557. FlatEnv env_id used instead.
    let mut dict_map: IndexMap<HashableValue, ThunkId> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // Always allocate a FlatEnv for every dict scope.
    // The resolver assigns (level, slot) coordinates to ALL entries in ALL dict scopes,
    // regardless of whether values are literals. Skipping allocation for literal-only dicts
    // would shorten the display chain, causing VarRefs from nested scopes to resolve to
    // the wrong level. Use alloc_child so the display vector inherits all ancestor scopes —
    // this is required for VarRef dispatch at level > 0 (cross-scope variable references).
    let env_id = ctx
        .scope_arena
        .borrow_mut()
        .alloc_child(crate::arena::ScopeId(parent_env_id), entries.len());
    let mut slot_idx: u32 = 0;
    // Collect (slot_idx, thunk_id, name) tuples for static-key entries so we can
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
            Some(key_expr) => eval_key_core(key_expr, parent_env_id, ctx).await?,
            None => {
                let k = HashableValue::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    EvalError::integer_overflow("dict auto-index".to_string(), entry.span.clone())
                })?;
                k
            }
        };

        // Attach the binding name to the entry's span for blame tracking.
        // When the key is a string (named binding), the name goes on the thunk's span so it
        // is visible for stack traces, scope-frame reconstruction, and type-stage lookup.
        let entry_span = if let HashableValue::Str(ref name) = key {
            entry
                .node
                .value
                .span
                .clone()
                .with_name(std::sync::Arc::from(name.as_ref()))
        } else {
            entry.node.value.span.clone()
        };

        // Literals are pre-materialized — scope-independent values need no CoreExpr deferral.
        // Non-literal values become CoreExpr thunks pointing to dict_env.
        let value_thunk = match &entry.node.value.node {
            CoreExpr::Int(n) => Arc::new(Thunk::value(Value::Int(*n), entry_span)),
            CoreExpr::U64(n) => Arc::new(Thunk::value(
                Value::BigInt(num_bigint::BigInt::from(*n)),
                entry_span,
            )),
            CoreExpr::Float(f) => Arc::new(Thunk::value(Value::Float(*f), entry_span)),
            CoreExpr::Str(s) => Arc::new(Thunk::value(string_val(s), entry_span)),
            // Non-literal: use UnevaluatedState::CoreExpr with the FlatEnv dict scope.
            _ => Arc::new(Thunk::core_expr(
                Arc::clone(&entry.node.value),
                env_id.0,
                Arc::clone(ctx),
                entry_span,
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
                    // See T-1621 for completing Value::Annotated wrapping for non-literal entries.
                    if let Some(inner_val) = value_thunk.try_get_materialized() {
                        let span = value_thunk.span.clone();
                        Arc::new(Thunk::value(
                            Value::Annotated {
                                inner: Box::new(inner_val),
                                annotation: Box::new(annotation_value),
                            },
                            span,
                        ))
                    } else {
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

        // Values go into arena slots (T-1557: Env is type-metadata only).
        // The letrec scope is maintained via arena reserve_slot + fill_slot calls below.
        let thunk_id = ctx.alloc_thunk(0, thunk);
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

        if is_static_key {
            letrec_slots.push((slot_idx, thunk_id));
            slot_idx += 1;
        }
    }

    // Batch-fill letrec slots: acquire the arena borrow once for all static-key entries
    // instead of once per entry. This avoids repeated borrow overhead for
    // dicts with many string-keyed fields.
    //
    // Two-phase letrec: first reserve all named slots in the child scope (in order),
    // then fill each reserved slot from the corresponding source ThunkId.
    // This ensures child scope slots exist before any are filled, maintaining letrec semantics.
    if !letrec_slots.is_empty() {
        let mut arena_guard = ctx.scope_arena.borrow_mut();
        for (idx, _thunk_id) in &letrec_slots {
            let reserved_idx = arena_guard.reserve_slot(env_id);
            debug_assert_eq!(
                reserved_idx, *idx,
                "letrec slot index must match reservation order"
            );
        }
        for (idx, thunk_id) in letrec_slots {
            arena_guard.fill_slot(env_id, idx, thunk_id);
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

    Ok(Arc::new(Thunk::value(
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
    parent_env_id: u32,
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
    // General path: must materialize because IndexMap requires concrete HashableValue keys.
    // Key expressions evaluate in the parent scope (parent_env_id).
    let thunk = eval_core_expr(key_expr.as_ref(), parent_env_id, ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use indexmap::IndexMap;

    use crate::ast::{SurfaceDocument, SurfaceItem, SurfaceProgram};
    use crate::error::EvalResult;
    use crate::eval::EvalContext;
    use crate::value::{HashableValue, Thunk, ThunkId, Value};

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        EvalContext::new(base_dir, false)
    }

    fn empty_env() -> Arc<RwLock<crate::env::Env>> {
        Arc::new(RwLock::new(crate::env::Env::new()))
    }

    /// Parse and evaluate a surface expression string using the resolver so
    /// $name variable references are correctly dispatched via de Bruijn slots.
    async fn eval_str(
        src: &str,
        env: Arc<RwLock<crate::env::Env>>,
        ctx: &Arc<EvalContext>,
    ) -> EvalResult<Arc<Thunk>> {
        use crate::ast::Spanned;
        use crate::resolve::resolve_surface_program;
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        let span = node.span.clone();
        let doc = SurfaceDocument {
            header: IndexMap::new(),
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
        };
        let program = SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), span)],
        };
        let mut program = program;
        crate::desugar::desugar_program_full(&mut program);
        let root_frame: IndexMap<String, u32> = crate::builtins_core::core_builtins()
            .iter()
            .enumerate()
            .map(|(i, def)| (def.name.to_string(), i as u32))
            .collect();
        let _ = env;
        let (_table, _frames) = resolve_surface_program(&program, &[root_frame]);
        crate::eval_surface_file(&program, ctx).await
    }

    async fn materialize(
        thunk: &Arc<Thunk>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::eval::materialize(thunk, None, ctx).await
    }

    async fn mat_id(id: ThunkId, ctx: &Arc<EvalContext>) -> crate::error::EvalResult<Value> {
        let thunk = ctx.get_thunk(id);
        materialize(&thunk, ctx).await
    }

    /// Empty dict `[]` evaluates to an empty Value::Dict.
    #[tokio::test]
    async fn test_empty_dict() {
        let ctx = test_ctx();
        let thunk = eval_str("[]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();
        match val {
            Value::Dict(map) => assert!(map.is_empty(), "empty dict must have zero entries"),
            other => panic!("expected empty Dict, got: {other:?}"),
        }
    }

    /// `[x: 42  y: x]` — letrec scope: y can see sibling x.
    ///
    /// The dict uses letrec semantics: `y`'s value expression `x` must resolve
    /// to sibling `x`'s value. This exercises the FlatEnv child-scope allocation
    /// in eval_dict_core without requiring any builtins.
    #[tokio::test]
    async fn test_dict_letrec_value_scope() {
        let ctx = test_ctx();
        let thunk = eval_str("[x: 42  y: x]", empty_env(), &ctx).await.unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict(map) = val else {
            panic!("expected Dict, got: {val:?}");
        };

        let x_id = *map
            .get(&HashableValue::Str("x".into()))
            .expect("key 'x' must exist");
        let y_id = *map
            .get(&HashableValue::Str("y".into()))
            .expect("key 'y' must exist");

        let x_val = mat_id(x_id, &ctx).await.unwrap();
        let y_val = mat_id(y_id, &ctx).await.unwrap();

        assert_eq!(x_val, Value::Int(42), "x must be 42");
        assert_eq!(
            y_val,
            Value::Int(42),
            "y must equal x (letrec sibling scope)"
        );
    }

    /// Dict key names do not leak into sibling value scope as bare names.
    ///
    /// `[k: "found"  v: 42]` — the value `42` must not accidentally resolve
    /// to `k`'s value. This guards against key names escaping into sibling scopes.
    /// After evaluation, `v` must be exactly `Int(42)`, not `String("found")`.
    #[tokio::test]
    async fn test_dict_key_not_in_value_scope() {
        let ctx = test_ctx();
        let thunk = eval_str("[k: \"found\"  v: 42]", empty_env(), &ctx)
            .await
            .unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict(map) = val else {
            panic!("expected Dict, got: {val:?}");
        };

        // There must be exactly two keys: "k" and "v"
        assert_eq!(map.len(), 2, "dict must have exactly 2 entries");

        let k_id = *map
            .get(&HashableValue::Str("k".into()))
            .expect("key 'k' must exist");
        let v_id = *map
            .get(&HashableValue::Str("v".into()))
            .expect("key 'v' must exist");

        let k_val = mat_id(k_id, &ctx).await.unwrap();
        let v_val = mat_id(v_id, &ctx).await.unwrap();

        assert_eq!(
            k_val,
            crate::value::string_val("found"),
            "k must be 'found'"
        );
        // v must be Int(42) — not the string value of k, proving key names don't
        // bleed into sibling value scopes.
        assert_eq!(
            v_val,
            Value::Int(42),
            "v must be Int(42), not the value of k"
        );
    }
}
