//! Dict construction and letrec scoping: `eval_dict_core`, `eval_key_core`, `value_to_key`.
//!
//! Dict entries are evaluated lazily with letrec semantics: string-keyed entries
//! become bindings visible to sibling values (forward references are allowed because
//! values are thunks). Keys are evaluated in the parent scope.
//!
//! All evaluation is CoreExpr-native via `eval_dict_core` / `eval_key_core`.

use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::{Annotation, CoreEntry, CoreExpr, Span, Spanned, SurfaceExpression};
use crate::error::{EvalError, EvalResult};
use crate::value::{string_val, EvalFrame, HashableValue, Thunk, Value};

use super::{materialize, EvalContext};
use crate::eval_core::eval_core_expr;

fn value_to_key(value: &Value, span: &Span) -> EvalResult<HashableValue> {
    match value {
        Value::String {
            ref source,
            start,
            end,
            ..
        } => Ok(HashableValue::Str(Arc::from(&source[*start..*end]))),
        Value::Int { n, .. } => Ok(HashableValue::Int(*n)),
        Value::Float { n, .. } => Ok(HashableValue::Float(n.to_bits())),
        Value::Variant { ctor, payload, .. } => {
            let tag = ctor.as_ref();
            let hv_payload = match payload {
                None => None,
                Some(p) => {
                    let p_val = match p.peek_result() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                        None => {
                            return Err(EvalError::internal(
                                "variant payload not materialized".to_string(),
                                span.clone(),
                            )
                            .into())
                        }
                    };
                    Some(Box::new(value_to_key(p_val, span)?))
                }
            };
            Ok(HashableValue::Variant {
                tag: tag.into(),
                payload: hv_payload,
            })
        }
        _ => Err(EvalError::type_mismatch(
            "String, Int, Float, or Variant",
            value.type_name(),
            span.clone(),
        )
        .into()),
    }
}

fn extract_property_dict_from_annotation(
    ann: &Annotation,
) -> Option<&Vec<Spanned<crate::ast::SurfaceEntry>>> {
    match ann {
        Annotation::PropertyDict(entries) => Some(entries),
        Annotation::Annotated(outer, _) => extract_property_dict_from_annotation(outer),
        _ => None,
    }
}

/// Evaluate a PropertyDict annotation to a materialized Value::Dict.
///
/// The annotation PropertyDict contains SurfaceExpression nodes that need to be evaluated
/// in the given environment context. This function evaluates each entry's key and value,
/// materializes them, and builds a concrete Value::Dict.
///
/// Non-literal values (VarRefs, calls, fn expressions) are lowered to CoreExpr and
/// evaluated in `parent_env_id` — the same environment as the enclosing dict's keys.
///
/// Used to evaluate `@[...]` annotations on dict key entries.
async fn eval_annotation_property_dict(
    entries: &[Spanned<crate::ast::SurfaceEntry>],
    _parent_env_id: u32,
    ctx: &Arc<EvalContext>,
) -> EvalResult<Value> {
    let mut dict_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    for entry in entries {
        // Evaluate the key to get a concrete HashableValue.
        // Annotation property dict keys must be literal values or bare words — complex
        // key expressions (calls, fn expressions, pipes) are not valid annotation syntax.
        let key = if let Some(key_node) = &entry.node.key {
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
                    HashableValue::Str(Arc::from(processed.as_str()))
                }
                SurfaceExpression::Int(n) => HashableValue::Int(*n),
                SurfaceExpression::Float(f) => HashableValue::Float(f.to_bits()),
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
                    HashableValue::Str(Arc::from(name.as_str()))
                }
                _ => {
                    // Annotation property dict keys must be string literals, int literals,
                    // or bare words. Complex key expressions (calls, fn, pipes) are not
                    // valid annotation syntax.
                    return Err(Box::new(EvalError::user_error(
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

        // Evaluate the value expression: literal values directly, complex expressions via lower+eval+materialize.
        let value = match &entry.node.value.expr {
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
                string_val(&processed)
            }
            SurfaceExpression::Int(n) => Value::Int {
                n: *n,
                type_val: crate::value::unknown_type_val(),
            },
            SurfaceExpression::U64(n) => Value::U64 {
                n: *n,
                type_val: crate::value::unknown_type_val(),
            },
            SurfaceExpression::Float(f) => Value::Float {
                n: *f,
                type_val: crate::value::unknown_type_val(),
            },
            // Type-level VarRef with resolution Some(None) → produce string identity.
            // This handles @[return: String], @[return: a], @[is: Int], etc.
            SurfaceExpression::VarRef {
                name, resolution, ..
            } if resolution.get() == Some(None) => string_val(name),
            _ => {
                // Non-literal annotation values are lowered to CoreExpr and evaluated.
                // This handles VarRefs (type names like Dict, String), fn expressions, and calls.
                let mut lower_diags = Vec::new();
                let core_expr = crate::lower::lower_inner(
                    &entry.node.value,
                    &mut lower_diags,
                    ctx.scope_frames.as_ref().map(|v| v.as_slice()),
                );
                {
                    let (info_diags, other_diags): (Vec<_>, Vec<_>) = lower_diags
                        .into_iter()
                        .partition(|d| d.level == crate::error::DiagnosticLevel::Info);
                    for d in info_diags {
                        ctx.runtime_diagnostics
                            .lock()
                            .expect("runtime_diagnostics mutex poisoned")
                            .push(d);
                    }
                    if let Some(err) =
                        crate::eval_materialize::lower_errors_to_eval_error(other_diags)
                    {
                        return Err(err);
                    }
                }
                match eval_core_expr(&core_expr, &EvalFrame::empty(), ctx).await {
                    Ok(thunk) => materialize(&thunk, Some(&entry.node.value.span), ctx).await?,
                    Err(e) if matches!(&e.kind, crate::error::ErrorKind::Unimplemented { .. }) => {
                        // Placeholder from nested type expressions → skip this key.
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
        };
        let value_thunk = Arc::new(Thunk::value(value, entry.node.value.span.clone()));

        if let Some(old_thunk) = dict_map.insert(key, Arc::clone(&value_thunk)) {
            let prev_span = old_thunk.span.clone();
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
            return Err(Box::new(
                EvalError::duplicate_key(&key_str, entry.span.clone())
                    .with_secondary_span(prev_span, "previously defined here"),
            ));
        }
    }

    Ok(Value::Dict {
        entries: dict_map,
        type_val: crate::value::unknown_type_val(),
    })
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
// correctly for class/instance inside dict values.
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
/// pass as entries in the runtime constructor dict. No pre-scan of individual dict entries
/// is needed — `CoreExpr::TypeDecl` (when present) wraps the entire constructor dict and
/// is handled by `eval_core_expr` before the dict is returned, not inside `eval_dict_core`.
pub(crate) async fn eval_dict_core(
    entries: &[Spanned<CoreEntry>],
    outer_frame: &Arc<EvalFrame>,
    ctx: &Arc<EvalContext>,
    dict_span: &Span,
) -> EvalResult<Arc<Thunk>> {
    // EvalFrame-based letrec state is initialized here; the legacy Arc<RwLock<Env>> has been removed.
    let mut dict_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // EvalFrame-based letrec: static-key entry thunks are collected into group vector
    // for LetrecGroupMember variable lookup. The frame is built once after the loop and
    // patched into each non-literal CoreExpr thunk via try_claim/reset.
    //
    // ALL non-literal thunks (both static-key and computed-key) are patched with the
    // letrec frame so that computed-key entry values can access function parameters,
    // closure captures, and static-key siblings. For dicts with no static keys (e.g.,
    // [$k: v] where $k is a computed key), the letrec frame is created with an empty
    // group but still carries params and closure_env from the outer frame.
    //
    // Collect the outer thunk (what goes in the dict map / letrec group) for each static key.
    // Position in this Vec corresponds to the LetrecGroupMember slot index assigned by the
    // resolver — the i-th static-key entry goes at index i.
    let mut letrec_slots: Vec<Arc<Thunk>> = Vec::new();
    // Collect the inner CoreExpr thunk for each non-literal entry (both static and computed key),
    // so we can patch its frame after the group Vec is assembled.
    let mut core_expr_thunks: Vec<Option<Arc<Thunk>>> = Vec::new();
    // Whether any non-literal entries were collected (controls whether we need a letrec frame).
    // ALL non-literal thunks (static-key AND computed-key) need the letrec frame so that
    // computed-key values (e.g., [$k: v]) can access function parameters and closures.
    let mut has_non_literal = false;

    for entry in entries {
        // Determine if this entry has a static key (CoreExpr::Str or annotated Var).
        // Must match resolve.rs Resolver::walk_expr Dict arm exactly — use the shared predicate.
        let is_static_key = entry
            .node
            .key
            .as_ref()
            .is_some_and(|k| core_expr_is_static_key(&k.node));

        // Keys are evaluated in the parent frame (Key Isolation Invariant): key expressions
        // must not see letrec sibling bindings. Pass outer_frame so computed keys can
        // reference outer-scope ClosureCapture variables.
        let key = match &entry.node.key {
            Some(key_expr) => eval_key_core(key_expr, outer_frame, ctx).await?,
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
        // Non-literal values become CoreExpr thunks. For static-key entries these thunks are
        // created with EvalFrame::empty() as a placeholder; the real letrec frame (containing
        // the group Vec) is patched in after all entries have been processed.
        //
        // `core_expr_thunk` tracks the inner CoreExpr thunk for static-key non-literal entries
        // so we can replace its frame after the group Vec is assembled.
        let (value_thunk, core_expr_thunk): (Arc<Thunk>, Option<Arc<Thunk>>) =
            match &entry.node.value.node {
                CoreExpr::Int(n) => (
                    Arc::new(Thunk::value(
                        Value::Int {
                            n: *n,
                            type_val: crate::value::unknown_type_val(),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::U64(n) => (
                    Arc::new(Thunk::value(
                        Value::BigInt {
                            n: num_bigint::BigInt::from(*n),
                            type_val: crate::value::unknown_type_val(),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::Float(f) => (
                    Arc::new(Thunk::value(
                        Value::Float {
                            n: *f,
                            type_val: crate::value::unknown_type_val(),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::Str(s) => (Arc::new(Thunk::value(string_val(s), entry_span)), None),
                // Non-literal: create with EvalFrame::empty() as a placeholder.
                // The real letrec frame is patched in below via try_claim/reset after
                // the group Vec is assembled. ALL non-literal thunks are patched (both
                // static-key and computed-key) so that computed-key values can reference
                // function parameters, closures, and static-key siblings correctly.
                _ => {
                    let t = Arc::new(Thunk::core_expr(
                        Arc::clone(&entry.node.value),
                        EvalFrame::empty(),
                        Arc::clone(ctx),
                        entry_span,
                    ));
                    // Track for patching regardless of key kind — both static-key and
                    // computed-key non-literal entries need the letrec frame.
                    let t2 = Some(Arc::clone(&t));
                    has_non_literal = true;
                    (t, t2)
                }
            };

        // If the key is annotated (e.g., Pi@[doc: "..."]), wrap the value in Value::Annotated.
        // The annotation PropertyDict is evaluated to a Value::Dict at dict construction time.
        let thunk = if let Some(key_expr) = &entry.node.key {
            if let CoreExpr::Var {
                annotation: Some(annotation),
                ..
            } = &key_expr.node
            {
                // PropertyDict annotations (including those wrapped in Annotated) produce
                // Value::Annotated at runtime. Simple annotations (e.g., Pi@Number) are
                // type-level metadata, not runtime values.
                if let Some(ann_entries) = extract_property_dict_from_annotation(&annotation.node) {
                    // Evaluate the annotation PropertyDict to a Value::Dict
                    let annotation_value =
                        eval_annotation_property_dict(ann_entries, 0u32, ctx).await?;

                    // Wrap value in Value::Annotated.
                    // For literals (already materialized): wrap immediately in Value::Annotated.
                    // For non-literals (functions, VarRefs, calls): create a deferred
                    // AnnotatedWrap thunk that forces the inner value when accessed and wraps
                    // it in Value::Annotated { annotation, inner: forced_inner }.
                    // This preserves laziness for non-literal annotated entries.
                    if let Some(Ok(inner_val)) =
                        value_thunk.peek_result().map(|r| r.map(|v| v.clone()))
                    {
                        let span = value_thunk.span.clone();
                        Arc::new(Thunk::value(
                            Value::Annotated {
                                inner: Box::new(inner_val),
                                annotation: Box::new(annotation_value),
                            },
                            span,
                        ))
                    } else {
                        let span = value_thunk.span.clone();
                        let inner_id = value_thunk;
                        Arc::new(Thunk::annotated_wrap(
                            inner_id,
                            annotation_value,
                            Arc::clone(ctx),
                            span,
                        ))
                    }
                } else {
                    value_thunk
                }
            } else {
                value_thunk
            }
        } else {
            value_thunk
        };

        // Values go into the dict map (Dict stores Arc<Thunk> directly).
        if let Some(old_thunk) = dict_map.insert(key, Arc::clone(&thunk)) {
            let prev_span = old_thunk.span.clone();
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
            return Err(Box::new(
                EvalError::duplicate_key(&key_str, entry.span.clone())
                    .with_secondary_span(prev_span, "previously defined here"),
            ));
        }

        if is_static_key {
            letrec_slots.push(Arc::clone(&thunk));
        }
        // Track all non-literal thunks (both static and computed key) for frame patching.
        core_expr_thunks.push(core_expr_thunk);
    }

    // Phase 2: build the shared letrec EvalFrame and patch it into each non-literal
    // CoreExpr thunk.
    //
    // group: outer_frame.group (accumulated_group containing root entries + prior dict entries)
    // EXTENDED with this dict's own letrec_slots. This ensures LGM(absolute_slot) lookups
    // resolve correctly for both:
    //   - Cross-dict refs (e.g., LGM(N) for a root or prior-dict entry) → outer_frame.group[N]
    //   - Self-refs within this dict (e.g., LGM(outer_frame.group.len() + i)) → letrec_slots[i]
    //
    // closure_env: for document-level dicts (outer_frame.closure_env.is_empty()), carry
    // the outer_frame.group so that fns defined in this dict can capture cross-dict names via
    // frame.group[slot] at fn-creation time. For fn-body dicts (closure_env non-empty),
    // carry outer_frame.closure_env unchanged — the fn's own captures are already materialized.
    //
    // Patch is safe here: the thunks were just created in this function and have not been
    // shared with any other task yet (they are not in the dict map until after patch, and
    // dict_map is local to this function until the Ok(...) return at the end). try_claim()
    // atomically takes the UnevaluatedState out of the Mutex; reset() puts the updated
    // state back in. If try_claim() returns None the thunk is already settled (can only
    // happen for literals, which have core_expr_thunk=None and are skipped).
    //
    // Always create the letrec frame when there are any non-literal thunks (both
    // static-key and computed-key). This ensures computed-key values (e.g., [$k: v]
    // in make-entry) can access function parameters and closures via the frame.
    if has_non_literal {
        // Build extended group: outer group (accumulated) + this dict's letrec slots.
        let group: std::sync::Arc<crate::value::GroupSpine> = outer_frame
            .group
            .extend(letrec_slots.iter().cloned().collect());

        // closure_env: document-level dicts carry the outer group for fn captures;
        // fn-body dicts carry the fn's closure_env unchanged.
        let closure_env = if outer_frame.closure_env.is_empty() {
            std::sync::Arc::clone(&outer_frame.group)
        } else {
            std::sync::Arc::clone(&outer_frame.closure_env)
        };

        // Inherit outer_frame.params so that function parameters remain accessible
        // from within intermediate dict bodies inside a function. Without this, an
        // intermediate dict `[x: [fn-param-ref]]` inside `[fn [let p] [x: p] body]`
        // would fail because the dict's letrec_frame has empty params.
        let letrec_frame = std::sync::Arc::new(EvalFrame {
            closure_env,
            group,
            params: std::sync::Arc::clone(&outer_frame.params),
        });

        for maybe_core_thunk in &core_expr_thunks {
            let Some(core_thunk) = maybe_core_thunk else {
                continue; // literal — no frame to patch
            };
            // Atomically extract the unevaluated state, swap in the real frame, restore.
            if let Some(state) = core_thunk.try_claim() {
                match state {
                    crate::value::UnevaluatedState::CoreExpr {
                        expr, ctx: t_ctx, ..
                    } => {
                        core_thunk.reset(crate::value::UnevaluatedState::CoreExpr {
                            expr,
                            frame: std::sync::Arc::clone(&letrec_frame),
                            ctx: t_ctx,
                        });
                    }
                    other => {
                        // Should not happen: we only put CoreExpr thunks in core_expr_thunks.
                        core_thunk.reset(other);
                    }
                }
            }
            // If try_claim returns None the thunk is already settled; nothing to patch.
        }
    }

    Ok(Arc::new(Thunk::value(
        Value::Dict {
            entries: dict_map,
            type_val: crate::value::unknown_type_val(),
        },
        dict_span.clone(),
    )))
}

/// Evaluate a dict key from a `CoreExpr` node, returning a concrete `HashableValue`.
///
/// Direct key evaluation for literal keys (string/int literals) avoids creating temporary thunks.
/// General path materializes the expression via `eval_core_expr`.
pub(crate) async fn eval_key_core(
    key_expr: &Arc<Spanned<CoreExpr>>,
    frame: &Arc<EvalFrame>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<HashableValue> {
    // Direct key evaluation for statically-known keys — avoids thunk creation and materialization
    match &key_expr.node {
        CoreExpr::Str(s) => return Ok(HashableValue::Str(Arc::from(s.as_str()))),
        CoreExpr::Int(n) => return Ok(HashableValue::Int(*n)),
        CoreExpr::Float(f) => return Ok(HashableValue::Float(f.to_bits())),
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
            return Ok(HashableValue::Str(Arc::from(name.as_str())));
        }
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete HashableValue keys.
    // Key expressions evaluate in the parent frame.
    let thunk = eval_core_expr(key_expr.as_ref(), frame, ctx).await?;
    let value = materialize(&thunk, Some(&key_expr.span), ctx).await?;
    value_to_key(&value, &key_expr.span)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use indexmap::IndexMap;

    use crate::ast::{SurfaceDocument, SurfaceItem, SurfaceProgram};
    use crate::error::EvalResult;
    use crate::eval::EvalContext;
    use crate::value::{HashableValue, Thunk, Value};

    fn test_ctx() -> Arc<EvalContext> {
        EvalContext::new()
    }

    /// Parse and evaluate a surface expression string using the resolver so
    /// $name variable references are correctly dispatched via de Bruijn slots.
    async fn eval_str(src: &str, ctx: &Arc<EvalContext>) -> EvalResult<Arc<Thunk>> {
        use crate::ast::Spanned;
        use crate::resolve::resolve_surface_program;
        let node = crate::parser::parse_surface_expression(src)
            .unwrap_or_else(|e| panic!("parse_surface_expression({src:?}) failed: {e:?}"));
        let span = node.span.clone();
        let doc = SurfaceDocument {
            header: IndexMap::new(),
            items: vec![SurfaceItem::Expr(Arc::clone(&node))],
        };
        let program = crate::desugar::desugar_program_full(&SurfaceProgram {
            documents: vec![Spanned::new(Arc::new(doc), span)],
        });
        let root_frame = ctx.root_group_resolver_map();
        let (_table, _frames) = resolve_surface_program(&program, &[root_frame]);
        crate::eval_surface_file(&program, ctx).await
    }

    async fn materialize(
        thunk: &Arc<Thunk>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        crate::eval::materialize(thunk, None, ctx).await
    }

    async fn mat_thunk(
        thunk: &Arc<Thunk>,
        ctx: &Arc<EvalContext>,
    ) -> crate::error::EvalResult<Value> {
        materialize(thunk, ctx).await
    }

    /// Empty dict `[]` evaluates to an empty Value::Dict.
    #[tokio::test]
    async fn test_empty_dict() {
        let ctx = test_ctx();
        let thunk = eval_str("[]", &ctx).await.unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();
        match val {
            Value::Dict { entries: map, .. } => {
                assert!(map.is_empty(), "empty dict must have zero entries")
            }
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
        let thunk = eval_str("[x: 42  y: x]", &ctx).await.unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict { entries: map, .. } = val else {
            panic!("expected Dict, got: {val:?}");
        };

        let x_thunk = map
            .get(&HashableValue::Str("x".into()))
            .expect("key 'x' must exist");
        let y_thunk = map
            .get(&HashableValue::Str("y".into()))
            .expect("key 'y' must exist");

        let x_val = mat_thunk(x_thunk, &ctx).await.unwrap();
        let y_val = mat_thunk(y_thunk, &ctx).await.unwrap();

        assert_eq!(
            x_val,
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val()
            },
            "x must be 42"
        );
        assert_eq!(
            y_val,
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val()
            },
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
        let thunk = eval_str("[k: \"found\"  v: 42]", &ctx).await.unwrap();
        let val = materialize(&thunk, &ctx).await.unwrap();

        let Value::Dict { entries: map, .. } = val else {
            panic!("expected Dict, got: {val:?}");
        };

        // There must be exactly two keys: "k" and "v"
        assert_eq!(map.len(), 2, "dict must have exactly 2 entries");

        let k_thunk = map
            .get(&HashableValue::Str("k".into()))
            .expect("key 'k' must exist");
        let v_thunk = map
            .get(&HashableValue::Str("v".into()))
            .expect("key 'v' must exist");

        let k_val = mat_thunk(k_thunk, &ctx).await.unwrap();
        let v_val = mat_thunk(v_thunk, &ctx).await.unwrap();

        assert_eq!(
            k_val,
            crate::value::string_val("found"),
            "k must be 'found'"
        );
        // v must be Int(42) — not the string value of k, proving key names don't
        // bleed into sibling value scopes.
        assert_eq!(
            v_val,
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val()
            },
            "v must be Int(42), not the value of k"
        );
    }
}
