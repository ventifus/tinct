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
use crate::error::Diagnostic;
use crate::value::{string_val, EvalFrame, HashableValue, Thunk, Value, TYPE_ID_DICT};

use super::{materialize, EvalContext};
use crate::eval_core::eval_core_expr;

fn value_to_key(
    value: &Value,
    span: &Span,
    ctx: &Arc<EvalContext>,
) -> Result<HashableValue, Box<crate::error::Diagnostic>> {
    match value {
        Value::String {
            ref source,
            start,
            end,
            ..
        } => Ok(HashableValue::Str(Arc::from(&source[*start..*end]))),
        Value::Int { n, .. } => Ok(HashableValue::Int(*n)),
        Value::Float { n, .. } => Ok(HashableValue::Float(n.to_bits())),
        Value::Variant {
            type_decl_id,
            ctor_index,
            payload,
            ..
        } => {
            let hv_payload = match payload {
                None => None,
                Some(p) => {
                    let p_val = match p.peek_result() {
                        Some(Ok(v)) => v,
                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                        None => {
                            return Err(Diagnostic::internal(
                                "variant payload not materialized".to_string(),
                                span.clone(),
                            )
                            .into())
                        }
                    };
                    Some(Box::new(value_to_key(p_val, span, ctx)?))
                }
            };
            Ok(HashableValue::Variant {
                type_decl_id: *type_decl_id,
                ctor_index: *ctor_index,
                payload: hv_payload,
            })
        }
        _ => Err(Diagnostic::type_mismatch(
            "String, Int, Float, or Variant",
            &ctx.type_display_name(value.type_decl_id_of()),
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
) -> Result<Value, Box<crate::error::Diagnostic>> {
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
                        return Err(Box::new(Diagnostic::internal(
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
                    return Err(Box::new(Diagnostic::user_error(
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
                Diagnostic::integer_overflow(
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
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            SurfaceExpression::U64(n) => Value::U64 {
                n: *n,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_U64,
                )),
            },
            SurfaceExpression::Float(f) => Value::Float {
                n: *f,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_FLOAT,
                )),
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
                let display_names = ctx.type_display_names_snapshot();
                let core_expr =
                    crate::lower::lower_inner(&entry.node.value, &mut lower_diags, &display_names);
                let has_errors = lower_diags
                    .iter()
                    .any(|d| d.level == crate::error::DiagnosticLevel::Err);
                ctx.runtime_diagnostics
                    .lock()
                    .expect("runtime_diagnostics mutex poisoned")
                    .extend(lower_diags);
                if has_errors {
                    return Err(Box::new(crate::error::Diagnostic::error(
                        "lower-error",
                        "expression lowering failed",
                        entry.node.value.span.clone(),
                    )));
                }
                match eval_core_expr(&core_expr, &EvalFrame::empty(), ctx).await {
                    Ok(thunk) => materialize(&thunk, Some(&entry.node.value.span), ctx).await?,
                    Err(e) if e.kind == "unimplemented" => {
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
                Diagnostic::duplicate_key(&key_str, entry.span.clone())
                    .with_secondary_span(prev_span, "previously defined here"),
            ));
        }
    }

    Ok(Value::Dict {
        entries: dict_map,
        type_decl_id: TYPE_ID_DICT,
        type_val: Some(crate::type_infer::make_typevalue_nominal(
            crate::value::TYPE_ID_DICT,
        )),
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
// are handled at compile time (type checker class/instance pre-registration step, typecheck_dict.rs) and skipped
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
        CoreExpr::Str { .. }
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
) -> Result<Arc<Thunk>, Box<crate::error::Diagnostic>> {
    // EvalFrame-based letrec state is initialized here; the legacy Arc<RwLock<Env>> has been removed.
    let mut dict_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::with_capacity(entries.len());
    let mut auto_index: i64 = 0;

    // EvalFrame-based letrec: static-key entry thunks are placed into scope_vec at their
    // assigned_slot positions for Dispatch(_, slot) variable lookup. The frame is built
    // once after the loop and patched into each non-literal CoreExpr thunk via try_claim/reset.
    //
    // ALL non-literal thunks (both static-key and computed-key) are patched with the
    // letrec frame so that computed-key entry values can access function parameters
    // and static-key siblings. For dicts with no static keys (e.g.,
    // [$k: v] where $k is a computed key), the letrec frame is created with an empty
    // group but still carries params from the outer frame.
    //
    // Clone parent scope: static-key entries are placed at their resolver-assigned slot
    // positions. Computed-key entries (no static key name) are appended sequentially.
    let mut scope_vec: Vec<Arc<Thunk>> = outer_frame.group.as_ref().clone();
    // Collect the inner CoreExpr thunk for each non-literal entry (both static and computed key),
    // so we can patch its frame after the group Vec is assembled.
    let mut core_expr_thunks: Vec<Option<Arc<Thunk>>> = Vec::new();
    // Whether any non-literal entries were collected (controls whether we need a letrec frame).
    // ALL non-literal thunks (static-key AND computed-key) need the letrec frame so that
    // computed-key values (e.g., [$k: v]) can access function parameters and closures.
    let mut has_non_literal = false;

    for entry in entries {
        // Keys are evaluated in the parent frame (Key Isolation Invariant): key expressions
        // must not see letrec sibling bindings. Pass outer_frame so computed keys can
        // reference outer-scope variables.
        let key = match &entry.node.key {
            Some(key_expr) => eval_key_core(key_expr, outer_frame, ctx).await?,
            None => {
                let k = HashableValue::Int(auto_index);
                auto_index = auto_index.checked_add(1).ok_or_else(|| {
                    Diagnostic::integer_overflow("dict auto-index".to_string(), entry.span.clone())
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
                CoreExpr::Int { value: n, .. } => (
                    Arc::new(Thunk::value(
                        Value::Int {
                            n: *n,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_INT,
                            )),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::U64 { value: n, .. } => (
                    Arc::new(Thunk::value(
                        Value::BigInt {
                            n: num_bigint::BigInt::from(*n),
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_BIGINT,
                            )),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::Float { value: f, .. } => (
                    Arc::new(Thunk::value(
                        Value::Float {
                            n: *f,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_FLOAT,
                            )),
                        },
                        entry_span,
                    )),
                    None,
                ),
                CoreExpr::Str { value: s, .. } => {
                    (Arc::new(Thunk::value(string_val(s), entry_span)), None)
                }
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
                                type_val: None,
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
                    CoreExpr::Str { value: s, .. } => s.clone(),
                    CoreExpr::Int { value: n, .. } => n.to_string(),
                    CoreExpr::U64 { value: n, .. } => n.to_string(),
                    CoreExpr::Var { name, .. } => name.clone(),
                    _ => "<computed key>".to_string(),
                },
                None => (auto_index - 1).to_string(),
            };
            return Err(Box::new(
                Diagnostic::duplicate_key(&key_str, entry.span.clone())
                    .with_secondary_span(prev_span, "previously defined here"),
            ));
        }

        // Uniform slot assignment: every entry gets a real slot in scope_vec.
        {
            let slot = entry.node.assigned_slot as usize;
            if slot < scope_vec.len() {
                scope_vec[slot] = Arc::clone(&thunk);
            } else {
                // Gaps in scope_vec can occur when the resolver assigns non-sequential slots
                // (e.g., slot 0 for entry 'a', slot 2 for entry 'c' if slot 1 is reserved for
                // a parent-scope binding that is later overridden). Fill gaps with empty dicts
                // to maintain slot indexing invariant: scope_vec[N] must exist for all N < len.
                // Gap entries are never accessed by correct code — they exist only to preserve
                // the direct-indexing property (VarAddr::Dispatch(_, slot) -> scope_vec[slot]).
                scope_vec.resize_with(slot + 1, || {
                    Arc::new(Thunk::value(
                        Value::Dict {
                            entries: IndexMap::new(),
                            type_decl_id: TYPE_ID_DICT,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_DICT,
                            )),
                        },
                        crate::rust_span!(),
                    ))
                });
                scope_vec[slot] = Arc::clone(&thunk);
            }
        }
        // Track all non-literal thunks (both static and computed key) for frame patching.
        core_expr_thunks.push(core_expr_thunk);
    }

    // Phase 2: build the shared letrec EvalFrame and patch it into each non-literal
    // CoreExpr thunk.
    //
    // group: scope_vec already contains the parent scope entries plus this dict's own
    // static-key entries placed at their assigned_slot positions. This ensures
    // LGM(absolute_slot) lookups resolve correctly for both:
    //   - Cross-dict refs (e.g., LGM(N) for a root or prior-dict entry) → scope_vec[N]
    //   - Self-refs within this dict → scope_vec[assigned_slot]
    //
    // definition_scope: for document-level dicts, carry the outer_frame.group so that fns
    // defined in this dict can access cross-dict names via frame.group[slot] at fn-creation
    // time. For fn-body dicts, carry the outer frame's definition scope unchanged.
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
        // Build group from scope_vec which already contains outer scope + this dict's entries
        // placed at their assigned_slot positions.
        let group: crate::value::Scope = std::sync::Arc::new(scope_vec);

        // Inherit outer_frame.params so that function parameters remain accessible
        // from within intermediate dict bodies inside a function. Without this, an
        // intermediate dict `[x: [fn-param-ref]]` inside `[fn [let p] [x: p] body]`
        // would fail because the dict's letrec_frame has empty params.
        let letrec_frame = std::sync::Arc::new(EvalFrame {
            group,
            params: std::sync::Arc::clone(&outer_frame.params),
            caller_evidence: std::sync::Arc::clone(&outer_frame.caller_evidence),
            type_inst: outer_frame.type_inst.clone(),
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
            type_decl_id: TYPE_ID_DICT,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_DICT,
            )),
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
) -> Result<HashableValue, Box<crate::error::Diagnostic>> {
    // Direct key evaluation for statically-known keys — avoids thunk creation and materialization
    match &key_expr.node {
        CoreExpr::Str { value: s, .. } => return Ok(HashableValue::Str(Arc::from(s.as_str()))),
        CoreExpr::Int { value: n, .. } => return Ok(HashableValue::Int(*n)),
        CoreExpr::Float { value: f, .. } => return Ok(HashableValue::Float(f.to_bits())),
        // U64 keys that fit in i64 are used as integer keys; larger values error.
        CoreExpr::U64 { value: n, .. } => {
            if let Ok(i) = i64::try_from(*n) {
                return Ok(HashableValue::Int(i));
            }
            return Err(Diagnostic::internal(
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
    value_to_key(&value, &key_expr.span, ctx)
}
