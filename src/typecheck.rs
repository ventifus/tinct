//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::type_tags::*;
// Pattern import deleted (T-1750)
use crate::ast::{
    Span, Spanned, SurfaceDocument, SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram,
};
use crate::env::Env;
use crate::error::Diagnostic;
use crate::type_infer::InferenceContext;
use crate::types::{generalize_tv, InferState};
use crate::value::{unknown_type_val, Value};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
pub(crate) mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;
// Special-case type refinement dispatchers for polymorphic builtins
// Path-sensitive narrowing and overlap checking
#[path = "typecheck_narrow.rs"]
pub(crate) mod typecheck_narrow;
// Case arm and function literal type inference
#[path = "typecheck_match.rs"]
pub(crate) mod typecheck_match;
// Call and dot-access type checking
#[path = "typecheck_call.rs"]
pub(crate) mod typecheck_call;
// CEK machine for iterative type checking
#[path = "typecheck_cek.rs"]
pub(crate) mod typecheck_cek;

use typecheck_narrow::{
    extract_param_indices, extract_pattern_types, patterns_overlap, types_can_unify,
};

/// Returns the next globally unique class declaration ID.
/// Delegates to `type_class::next_class_decl_id` — the counter is defined there so the
/// resolver's Phase 1b scan and the type-checker share the same AtomicU64.
fn next_class_decl_id() -> u64 {
    crate::type_class::next_class_decl_id()
}

/// Map from source span `(start_line, start_col, end_line, end_col)` to inferred TypeValue.
/// Populated during type checking so LSP hover/diagnostics can look up types without
/// re-running inference.
pub type TypeMap = HashMap<(u32, u32, u32, u32), Arc<Value>>;

// MatchArmData (old Expr-based) removed — replaced by SurfaceMatchArmData.

/// Grouped surface data for a `[class ...]` declaration.
///
/// Passed to [`infer_class_decl_from_surface`] instead of individual arguments,
/// eliminating the need for `#[allow(clippy::too_many_arguments)]`.
pub(crate) struct ClassDeclSurface<'a> {
    pub name: &'a str,
    pub params: &'a [String],
    pub superclasses: &'a [(String, Vec<String>)],
    pub determines: &'a [Arc<SurfaceNode>],
    pub structural: &'a str,
    pub span: Span,
    /// Optional resolver name (from `resolver:` in class metadata). Stored in ClassDecl.
    pub resolver: Option<String>,
    /// Whether the resolver is injective (from `resolver_injective:` in class metadata).
    pub resolver_injective: bool,
    /// Pre-assigned class declaration ID from the resolver's Phase 1b scan.
    /// When `Some`, the type-checker reads this ID instead of allocating a new one,
    /// ensuring the EffectPerform dispatch chain uses a consistent class_decl_id.
    /// `None` means the resolver did not run Phase 1b (e.g. bootstrap typecheck path).
    pub pre_assigned_class_decl_id: Option<&'a crate::ast::ClassDeclIdCell>,
}

/// Type-check a `SurfaceProgram` — minimal bootstrap entry point.
///
/// Iterates over `program.documents`, calling [`process_document`] for each with the
/// accumulated env, and collects all diagnostics. Cross-document env threading is handled
/// here because the bootstrap path has no init program to own that logic.
///
/// This is the only Rust-level typecheck entry point. Three bootstrap callers need it:
/// - `imports::build_builtin_core_envs_inner` — typecheck builtin_core.llt
/// - `lib::run_loader_pipeline` — typecheck the init program (loader.llt)
/// - `formatter::format_source` — typecheck the formatter script
///
/// All other type-checking goes through the init program via `builtin-typecheck-doc`.
///
/// # Returns
///
/// `(diagnostics, final_env, tycon_env)` where:
/// - `diagnostics`: All type errors and quality warnings encountered during inference.
/// - `final_env`: Env containing schemes from the last document, exported for callers
///   that need to inspect the resulting type environment.
/// - `tycon_env`: TyConEnv accumulated during inference (type constructor definitions).
pub async fn typecheck_program_bootstrap(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
    eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
    seed_tycon_env: crate::type_def::TyConEnv,
    type_stage_data: crate::type_infer::TypeStageData,
) -> (Vec<Diagnostic>, Arc<RwLock<Env>>, crate::type_def::TyConEnv) {
    let mut errors = Vec::new();
    // Create a child Env scope for this type-checking session: reads walk through
    // to the parent (finding prelude classes/instances), writes stay in the child.
    let child_env = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&parent_env))));
    let mut env: Arc<RwLock<Env>> = Arc::clone(&child_env);
    let mut state = InferState::with_env(Arc::clone(&child_env));
    state.eval_ctx = eval_ctx;
    // Install the caller-supplied type-stage scope. The scope must be provided by the
    // caller (obtained from get_builtin_core_type_stage_scope() or the bootstrap eval).
    // This function must NOT call get_builtin_core_type_stage_scope() internally —
    // build_builtin_core_envs_inner() calls this function, so calling it here would
    // create circular recursion.
    state.type_stage_scope = type_stage_data.scope;
    state.type_stage_fns = type_stage_data.fns;
    state.type_stage_type_vars = type_stage_data.type_vars;
    // Seed tycon_env from the TypeContext's accumulated TyConDefs. This propagates
    // opaque types (DirCap, File, ClockCap, Handle, etc.) declared in builtin_core.llt
    // to subsequent module type-checks (builtin_io.llt, builtin_async.llt, ...) so that
    // @DirCap and similar annotations resolve correctly without re-declaration.
    // Use or_insert so that static TyConDefs (with correct primitive bodies) are never
    // overwritten by dynamic declarations that produce nominal bodies.
    for (name, def) in seed_tycon_env {
        state.tycon_env.entry(name).or_insert(def);
    }
    // Seed the resolver from the full root_group when an eval context is available,
    // so all builtin slots match the runtime. Falls back to core_builtins() only when
    // no eval context is provided (type-only paths without an evaluator).
    let root_frame: indexmap::IndexMap<String, u32> = if let Some(ctx) = &state.eval_ctx {
        ctx.root_group_resolver_map()
    } else {
        crate::builtins_core::core_builtins()
            .iter()
            .enumerate()
            .map(|(i, def)| (def.name.to_string(), i as u32))
            .collect()
    };
    // Keep a reference to root_frame for seeding child_env below.
    // The root_frame contains the full accumulated group including TypeNode and other
    // type-stage entries at their absolute LGM slot positions.
    let root_frame_for_ts = root_frame.clone();
    let (resolve_table, frames) = crate::resolve::resolve_surface_program_with_classes(
        program,
        &[root_frame],
        std::collections::HashMap::new(),
    );
    state.resolution_table = Arc::new(resolve_table);
    state.resolver_frames = frames;

    // Seed child_env with all root-group entries so that get_scheme_at(N, slot) correctly
    // finds them at depth N via normal parent-chain traversal. Three priority levels:
    // 1. Type-stage entries — their actual TypeValues (highest priority).
    // 2. Entries with proper schemes from parent_env's chain (prelude, builtins).
    // 3. Unknown for any remaining slot in root_frame_for_ts (capability vars, etc.).
    {
        use crate::type_infer::{make_typevalue_op, make_typevalue_unknown};
        let mut child_inner = child_env.write().unwrap();
        let mut covered: std::collections::HashSet<usize> = std::collections::HashSet::new();

        // Type-stage entries get their actual TypeValues (highest priority).
        // type_stage_scope holds resolved TypeValues; Function entries in type_stage_fns;
        // TypeVar entries in type_stage_type_vars.
        for (name, tv) in state.type_stage_scope.iter().flat_map(|m| m.iter()) {
            let abs_slot = root_frame_for_ts.get(name.as_str()).copied().or_else(|| {
                state
                    .resolver_frames
                    .iter()
                    .find_map(|(f, _kind)| f.get(name.as_str()).copied())
            });
            if let Some(slot) = abs_slot {
                let idx = slot as usize;
                child_inner.insert_at_slot(idx, name.clone(), Arc::clone(tv), None);
                covered.insert(idx);
            } else {
                // No resolver slot for this type-stage name — append at end so name-based
                // lookup (get_scheme via slot_index) still works.
                let idx = child_inner.slots.len();
                child_inner.insert_at_slot(idx, name.clone(), Arc::clone(tv), None);
            }
        }
        for (name, _thunk) in &state.type_stage_fns {
            let tv = make_typevalue_op(name);
            let abs_slot = root_frame_for_ts.get(name.as_str()).copied().or_else(|| {
                state
                    .resolver_frames
                    .iter()
                    .find_map(|(f, _kind)| f.get(name.as_str()).copied())
            });
            if let Some(slot) = abs_slot {
                let idx = slot as usize;
                child_inner.insert_at_slot(idx, name.clone(), Arc::clone(&tv), None);
                covered.insert(idx);
            } else {
                // No resolver slot for this type-stage fn — append at end.
                let idx = child_inner.slots.len();
                child_inner.insert_at_slot(idx, name.clone(), tv, None);
            }
        }
        for (name, _kind) in &state.type_stage_type_vars {
            let tv = make_typevalue_op(name);
            let abs_slot = root_frame_for_ts.get(name.as_str()).copied().or_else(|| {
                state
                    .resolver_frames
                    .iter()
                    .find_map(|(f, _kind)| f.get(name.as_str()).copied())
            });
            if let Some(slot) = abs_slot {
                let idx = slot as usize;
                child_inner.insert_at_slot(idx, name.clone(), Arc::clone(&tv), None);
                covered.insert(idx);
            } else {
                // No resolver slot for this type-stage type var — append at end.
                let idx = child_inner.slots.len();
                child_inner.insert_at_slot(idx, name.clone(), tv, None);
            }
        }

        // Walk parent_env chain from outermost to innermost (innermost wins on conflict).
        // Only copy entries that belong to the runtime root frame (builtins + caps), remapping
        // to their correct runtime slot positions. parent_env (builtin_env_base) was resolved
        // with an empty root frame (initial_offset=0), so its type-stage entries (Integer, Float,
        // etc.) occupy slots 0, 1, 2, ... which collide with runtime builtin slots. We ONLY copy
        // entries that root_frame_for_ts knows about, at the slot root_frame_for_ts assigns them.
        // Type-stage names not in root_frame_for_ts are skipped — they are accessible via
        // get_scheme(name) through the parent chain's slot_index, and via type_stage_scope for
        // annotation resolution.
        let mut chain: Vec<Arc<RwLock<Env>>> = Vec::new();
        {
            let mut cursor = Some(Arc::clone(&parent_env));
            while let Some(arc) = cursor {
                chain.push(Arc::clone(&arc));
                cursor = arc.read().unwrap().parent.as_ref().map(Arc::clone);
            }
        }
        for frame in chain.iter().rev() {
            let frame_read = frame.read().unwrap();
            for (_, slot_entry) in frame_read.slots.iter().enumerate() {
                if let Some((name, env_slot)) = slot_entry {
                    if let Some(ref scheme) = env_slot.scheme {
                        // Remap to the runtime slot; skip names not in the runtime root frame.
                        if let Some(&rt_slot) = root_frame_for_ts.get(name.as_str()) {
                            let target_idx = rt_slot as usize;
                            if !covered.contains(&target_idx) {
                                child_inner.insert_at_slot(
                                    target_idx,
                                    name.clone(),
                                    Arc::clone(scheme),
                                    None,
                                );
                                covered.insert(target_idx);
                            }
                        }
                    }
                }
            }
        }

        // Fill remaining root-frame slots with Unknown (pure-Rust builtins, capability vars).
        for (name, &slot) in &root_frame_for_ts {
            let idx = slot as usize;
            if !covered.contains(&idx) {
                child_inner.insert_at_slot(idx, name.clone(), make_typevalue_unknown(), None);
            }
        }
    }

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        // Skip type-stage documents — they are evaluated before type-checking runs and their
        // entries are already in state.type_stage_scope / child_env via the caller-supplied
        // type_stage_data. Re-processing them through process_document produces resolver-slot-miss
        // errors because child_env is seeded for runtime slots, not type-stage eval slots.
        if doc.header.get("stage").is_some_and(|stage_node| {
            matches!(
                &stage_node.expr,
                crate::ast::SurfaceExpression::StringLiteral { content, .. }
                if content == "type"
            )
        }) {
            continue;
        }
        let (new_env, _, mut doc_errors) = process_document(doc, &env, &mut state, &mut None).await;
        errors.append(&mut doc_errors);
        env = new_env;
    }

    // Include state-level diagnostics (e.g. unknown-type warnings from unification).
    errors.append(&mut state.diagnostics);

    // Merge the document-level Env scheme bindings back into the child Env so callers
    // holding the returned Arc<RwLock<Env>> see all new bindings.
    merge_env_schemes_into_env(&env, &child_env, &mut state);
    state.invalidate_env_caches();

    (errors, child_env, state.tycon_env)
}

/// Merge ALL scheme bindings from an `Arc<RwLock<Env>>` chain into a target `Arc<RwLock<Env>>`.
///
/// After type-checking a program's documents, the final env is a chain of frames
/// (one per document, plus the initial parent frame). This function walks all
/// own-frame bindings and copies their schemes into `target_env` (the child Env) so that
/// callers holding the child Env can see all new bindings.
///
/// Since `target_env.parent == parent_env`, schemes that already exist in the parent
/// chain are already visible — no filtering is needed.
fn merge_env_schemes_into_env(
    source_env: &Arc<RwLock<Env>>,
    target_env: &Arc<RwLock<Env>>,
    state: &mut crate::type_infer::InferState,
) {
    // Collect frames from innermost to outermost, stopping when we reach target_env
    // to avoid reading and writing the same RwLock simultaneously (deadlock prevention).
    let target_ptr = Arc::as_ptr(target_env);
    let mut frames: Vec<Arc<RwLock<Env>>> = Vec::new();
    let mut current = Some(Arc::clone(source_env));
    while let Some(arc) = current {
        // Stop if we have reached the target (we'd hold both read and write locks on it).
        if Arc::as_ptr(&arc) == target_ptr {
            break;
        }
        let parent = arc.read().unwrap().parent.as_ref().map(Arc::clone);
        frames.push(arc);
        current = parent;
    }
    if frames.is_empty() {
        return; // source_env IS target_env or is a child of it; nothing to merge.
    }
    let mut guard = target_env.write().unwrap();
    // Walk frames from outermost to innermost so inner frames override outer.
    for frame_arc in frames.iter().rev() {
        let frame = frame_arc.read().unwrap();
        for (name, slot) in frame.iter_slots() {
            if let Some(ref scheme) = slot.scheme {
                // Find existing slot in target or append a new one.
                let target_slot = if let Some(&pos) = guard.slot_index.get(name) {
                    pos
                } else {
                    guard.slots.len()
                };
                guard.insert_at_slot(
                    target_slot,
                    name.to_string(),
                    scheme.clone(),
                    slot.definition_span.clone(),
                );
            }
        }
        for (_, decl) in &frame.classes {
            guard.insert_class(decl.clone());
        }
        for (mangled, decl) in &frame.instances {
            guard.insert_instance(mangled.clone(), decl.clone());
        }
        for (name, def) in &frame.tycon_defs {
            guard.insert_tycon_def(name.clone(), Arc::clone(def));
        }
        // Wire tycon_defs into type_stage_scope so that imported type constructors from
        // loaded modules resolve correctly in annotations.
        if state.type_stage_scope.is_empty() {
            state
                .type_stage_scope
                .push(std::collections::HashMap::new());
        }
        for (name, _) in &frame.tycon_defs {
            state.type_stage_scope[0]
                .entry(name.clone())
                .or_insert_with(|| crate::type_infer::make_typevalue_op(name));
        }
    }
}

/// Type-check a single [`SurfaceDocument`] using the CEK machine.
///
/// Routes all document items through the CEK's Sequential arm via `run_typecheck` — the single
/// canonical path for sequential expression evaluation. For single-item documents, the node
/// is evaluated directly (no Sequential wrapper).
///
/// After `run_typecheck`, ctor_schemes (ADT constructor TypeSchemes) are recovered from the
/// diff of `state.tycon_env` against a pre-run snapshot: `run_typecheck_dict` does not return
/// ctor_schemes for the terminal dict (the DictPassZero caller discards the schemes return),
/// so we reconstruct them here for result_env.
///
/// # Returns
///
/// `(result_env, result_type, errors)` where:
/// - `result_env`: env containing schemes from the last dict body, exported to subsequent documents
/// - `result_type`: the type of the last expression (or empty-dict for empty documents)
/// - `errors`: type errors encountered during inference (non-fatal — env always propagated)
/// Extract fields from a TypeValue.Record as `Vec<(field_name, TypeValue)>`.
/// Public alias for use by typecheck_narrow.rs.
pub(crate) fn extract_record_fields_pub(tv: &Arc<Value>) -> Option<Vec<(String, Arc<Value>)>> {
    extract_record_fields(tv)
}
///
/// Returns `Some(fields)` for TypeValue.Record variants where the payload can be read
/// synchronously (settled thunks). Returns `None` for non-Record TypeValues or unsettled payloads.
fn extract_record_fields(tv: &Arc<Value>) -> Option<Vec<(String, Arc<Value>)>> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(payload_thunk),
            ..
        } if ctor.as_ref() == TV_RECORD => match payload_thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let fields_key = HashableValue::Str(Arc::from(FIELD_FIELDS));
                let fields_thunk = entries.get(&fields_key)?;
                match fields_thunk.peek_result()? {
                    Ok(Value::Dict {
                        entries: field_entries,
                        ..
                    }) => {
                        let mut result = Vec::new();
                        for (k, v_thunk) in field_entries {
                            if let HashableValue::Str(name) = k {
                                if let Some(Ok(field_val)) = v_thunk.peek_result() {
                                    result.push((
                                        name.as_ref().to_string(),
                                        Arc::new(field_val.clone()),
                                    ));
                                }
                            }
                        }
                        Some(result)
                    }
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Public alias for use by typecheck_narrow.rs.
pub(crate) fn make_typevalue_record_pub(
    fields: indexmap::IndexMap<String, Arc<Value>>,
    tail: Option<Arc<Value>>,
) -> Arc<Value> {
    make_typevalue_record(fields, tail)
}

/// Construct a TypeValue.Record Arc<Value> from an IndexMap of field name → TypeValue pairs
/// and an optional tail TypeValue. `tail: None` means closed record (RowTail.Closed).
fn make_typevalue_record(
    fields: indexmap::IndexMap<String, Arc<Value>>,
    tail: Option<Arc<Value>>,
) -> Arc<Value> {
    use crate::value::{HashableValue, Thunk};
    // Build the fields dict: { fieldname: TypeValue, ... }
    let mut fields_entries: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
        indexmap::IndexMap::new();
    for (name, tv) in fields {
        fields_entries.insert(
            HashableValue::Str(Arc::from(name.as_str())),
            Arc::new(Thunk::value(Value::clone(tv.as_ref()), crate::rust_span!())),
        );
    }
    let fields_dict_val = Value::Dict {
        entries: fields_entries,
        type_val: unknown_type_val(),
    };
    // tail: None → closed record → RowTail.Closed variant
    let tail_val = tail
        .map(|tv| Value::clone(tv.as_ref()))
        .unwrap_or_else(|| crate::type_infer::make_rowtail_closed().as_ref().clone());
    // Build payload dict: { fields: Dict, tail: TypeValue }
    let mut payload_entries: indexmap::IndexMap<HashableValue, Arc<Thunk>> =
        indexmap::IndexMap::new();
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_FIELDS)),
        Arc::new(Thunk::value(fields_dict_val, crate::rust_span!())),
    );
    payload_entries.insert(
        HashableValue::Str(Arc::from(FIELD_TAIL)),
        Arc::new(Thunk::value(tail_val, crate::rust_span!())),
    );
    let payload_dict = Value::Dict {
        entries: payload_entries,
        type_val: unknown_type_val(),
    };
    Arc::new(Value::Variant {
        type_val: unknown_type_val(),
        type_decl_id: 0,
        ctor: Arc::from(TV_RECORD),
        payload: Some(Arc::new(Thunk::value(payload_dict, crate::rust_span!()))),
    })
}

/// Look up `name` in `frames` (innermost-first) and return its slot as `usize`, or `None`.
///
/// Used to assign resolver-correct slots to bindings that need slot insertion but were not
/// yet inserted via `insert_at_slot` at the point of the call.
pub(crate) fn find_slot_in_frames(
    frames: &[(indexmap::IndexMap<String, u32>, crate::resolve::FrameKind)],
    name: &str,
) -> Option<usize> {
    for (frame, _kind) in frames.iter().rev() {
        if let Some(&slot) = frame.get(name) {
            return Some(slot as usize);
        }
    }
    None
}

pub(crate) async fn process_document(
    doc: &SurfaceDocument,
    parent_env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> (Arc<RwLock<Env>>, Arc<Value>, Vec<Diagnostic>) {
    // Empty record TypeValue: TypeValue.Record { fields: {}, tail: [] (closed) }
    let empty_dict_ty = make_typevalue_record(indexmap::IndexMap::new(), None);

    // Collect all items in source order as SurfaceNodes.
    // SurfaceItem::Expr → use the node directly.
    // SurfaceItem::Decl → synthetic node with SurfaceExpression::Decl so infer_step::Decl
    //   handles class/instance/type-decl registration (TypeAlias deleted in S-1003).
    let nodes: Vec<Arc<SurfaceNode>> = doc
        .items
        .iter()
        .map(|item| match item {
            SurfaceItem::Expr(node) => Arc::clone(node),
            SurfaceItem::Decl(d) => Arc::new(SurfaceNode::new(
                SurfaceExpression::Decl(Box::new(d.node.clone())),
                d.span.clone(),
            )),
        })
        .collect();

    if nodes.is_empty() {
        // Empty document — return the parent env unchanged so the caller's env chain
        // continues from where it left off. Creating a fresh Env::with_parent here
        // would produce an env with 0 slots, breaking slot-based lookups in subsequent
        // documents that expect to traverse back to the properly seeded root env.
        return (Arc::clone(parent_env), empty_dict_ty, Vec::new());
    }

    let enclosing_level = state.ctx.current_level;
    let last_span = nodes.last().unwrap().span.clone();

    // Snapshot tycon_env keys before the run.
    // ctor_schemes for ADT types declared in the last Dict expression are not propagated
    // by the DictPassZero handler (it discards the schemes return from run_typecheck_dict).
    // We reconstruct them by diffing tycon_env against this snapshot: new zero-arity TyCon
    // entries represent types declared here.
    let tycon_keys_before: std::collections::HashSet<String> =
        state.tycon_env.keys().cloned().collect();

    // Route all document nodes through the CEK machine's Sequential arm — the single
    // canonical path for sequential expression evaluation. For single-item documents,
    // use the node directly (no Sequential wrapper needed).
    let doc_node = if nodes.len() == 1 {
        Arc::clone(&nodes[0])
    } else {
        let span = nodes[0].span.clone();
        Arc::new(SurfaceNode::new(SurfaceExpression::Sequential(nodes), span))
    };

    let mut errors = Vec::new();
    let result_ty = typecheck_cek::run_typecheck(
        &doc_node,
        parent_env,
        state,
        &mut errors,
        type_map,
        &mut Vec::new(),
    )
    .await;

    // Check that the last expression's type is a Dict subtype.
    //
    // A document's last expression must be a record (the exports dict). The runtime enforces
    // the same constraint via builtin-eval, but catching it here produces a clearer error
    // message and fails fast at compile time rather than at evaluation time.
    //
    // TypeValue.Record:   ok — any record type satisfies the constraint.
    // TypeValue.Unknown:  ok — gradual escape hatch; cannot rule out a dict statically.
    // TypeValue.Top:      ok — top type; cannot rule out a dict.
    // TypeValue.Var:      ok — inference variable; deferred to the constraint solver.
    // TypeValue.Op/App:   ok — named type constructor (may be a Dict alias).
    // Everything else:    the type is known to be a non-dict → error.
    {
        let is_dict_ok = match result_ty.as_ref() {
            Value::Variant { ctor, .. } => {
                let c = ctor.as_ref();
                c == TV_RECORD
                    || c == TV_UNKNOWN
                    || c == TV_TOP
                    || c == TV_VAR
                    || c == TV_OP
                    || c == TV_APP
            }
            // Bootstrap sentinel (empty dict = Unknown during bootstrap)
            Value::Dict { entries, .. } if entries.is_empty() => true,
            _ => false,
        };
        if !is_dict_ok {
            errors.push(Diagnostic::error(
                "type-error",
                format!(
                    "document last expression must be a record type, got {:?}",
                    result_ty
                ),
                last_span,
            ));
        }
    }

    // Build result_env with parent=parent_env (flat env chain invariant).
    // process_document invariant: result_env is always parented to parent_env, not to the
    // internal intermediate envs created by the CEK's Sequential arm.
    let mut result_env_inner = Env::with_parent(Arc::clone(parent_env));

    // Extract schemes from the last expression's result type.
    // The last Dict goes through DictPassZero → run_typecheck_dict, which produces mono field
    // types (not generalized at the call site). Generalize at enclosing_level here —
    // run_typecheck_dict restores state.ctx.current_level to enclosing_level before returning, so TypeVars
    // at level > enclosing_level are the ones from this document's inference and are safe to
    // generalize.
    //
    // Note: TyConDef entries (e.g., `Direction: [type North South]`) are excluded from
    // the record's field_types in run_typecheck_dict. They are NOT in result_ty.fields
    // and are handled separately below via the tycon_env diff.
    if let Some(fields) = extract_record_fields(&result_ty) {
        for (name, field_tv) in fields {
            let tv = generalize_tv(enclosing_level, &field_tv, &state.ctx);
            // Use resolver-assigned slot if available; otherwise append at end.
            let slot = find_slot_in_frames(&state.resolver_frames, &name)
                .unwrap_or_else(|| result_env_inner.slots.len());
            result_env_inner.insert_at_slot(slot, name, tv, None);
        }
    }

    // Reconstruct schemes for zero-arity ADT types declared in this document.
    // TypeAlias entries have is_alias=true in run_typecheck_dict and are excluded from
    // resolved_field_types, so they are not in result_ty.fields. The DictPassZero handler
    // also discards the schemes return from run_typecheck_dict. We recover both by diffing
    // state.tycon_env against the pre-run snapshot:
    //   1. The type name itself (e.g., "Direction") → adt_value_type(body) — the constructor dict
    //   2. Each constructor name (e.g., "North", "South") → the NominalVariant type
    for (name, def) in &state.tycon_env {
        if !tycon_keys_before.contains(name) && def.params.is_empty() {
            let value_ty = typecheck_cek::adt_value_type(&def.body);
            // Use resolver-assigned slot if available; otherwise append at end.
            let slot = find_slot_in_frames(&state.resolver_frames, name)
                .unwrap_or_else(|| result_env_inner.slots.len());
            result_env_inner.insert_at_slot(slot, name.clone(), Arc::clone(&value_ty), None);
            // Insert each constructor name with its variant TypeValue.
            if let Some(ctor_fields) = extract_record_fields(&value_ty) {
                for (ctor_name, ctor_tv) in ctor_fields {
                    let ctor_slot = find_slot_in_frames(&state.resolver_frames, &ctor_name)
                        .unwrap_or_else(|| result_env_inner.slots.len());
                    result_env_inner.insert_at_slot(ctor_slot, ctor_name, ctor_tv, None);
                }
            }
        }
    }

    (Arc::new(RwLock::new(result_env_inner)), result_ty, errors)
}

/// Pre-register type aliases in `target_env` before Pass 1 inference.
///
/// Map a resolved TypeValue (Arc<Value>) to the dispatch tag string used in instance binding names.
///
/// This mapping must match `extract_dispatch_tags` in `lower.rs`, which reads `@Annotation`
/// names from instance arm patterns.  Annotations are written as `@Integer`, `@Float`,
/// `@String`, `@Bytes`, `@SomeType` — the strings that appear in
/// `instance_binding_name` calls.
///
/// Returns `None` for:
/// - Unbound `TypeValue.Var` (instance not yet determined).
/// - `TypeValue.Unknown` / `TypeValue.Top` (gradual / lattice types that don't correspond to instances).
/// - Compound types that don't map to a single dispatch tag (records, functions, unions).
///
/// Literal types (IntLit, FloatLit, StrLit) are promoted to their widened tags
/// (`"Integer"`, `"Float"`, `"String"`) because instance arms are always annotated
/// with the widened type (e.g., `@Integer`, never `@42`).
pub(crate) fn type_to_dispatch_tag(tv: &Arc<Value>) -> Option<String> {
    match tv.as_ref() {
        Value::Variant { ctor, .. } => match ctor.as_ref() {
            TV_REPR => {
                // Extract the repr string from the payload to determine the dispatch tag.
                let repr = typevalue_repr_string(tv)?;
                match repr.as_str() {
                    REPR_INT => Some(DISPATCH_INTEGER.to_string()),
                    REPR_FLOAT => Some(DISPATCH_FLOAT.to_string()),
                    REPR_STRING => Some(DISPATCH_STRING.to_string()),
                    REPR_BYTES => Some(DISPATCH_BYTES.to_string()),
                    _ => None,
                }
            }
            TV_INT_LIT => Some(DISPATCH_INTEGER.to_string()),
            TV_FLOAT_LIT => Some(DISPATCH_FLOAT.to_string()),
            TV_STR_LIT => Some(DISPATCH_STRING.to_string()),
            // TypeValue.Op: a named type constructor → dispatch on its name.
            TV_OP => {
                // Extract name from payload dict.
                typevalue_op_name(tv).map(|n| n.to_string())
            }
            // Inference variables and gradual types cannot be dispatched.
            TV_VAR | TV_UNKNOWN | TV_TOP | TV_NEVER => None,
            // Compound types (Record, Fn, Union, App, etc.) don't map to single dispatch tags.
            _ => None,
        },
        // Bootstrap sentinel (empty dict = Unknown) → not dispatchable.
        _ => None,
    }
}

/// Extract the `repr` string from a TypeValue.Repr variant payload (synchronously).
fn typevalue_repr_string(tv: &Arc<Value>) -> Option<String> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_REPR => match thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let key = HashableValue::Str(Arc::from(FIELD_REPR));
                match entries.get(&key)?.peek_result()? {
                    Ok(Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Extract the `name` string from a TypeValue.Op variant payload (synchronously).
fn typevalue_op_name(tv: &Arc<Value>) -> Option<String> {
    use crate::value::HashableValue;
    match tv.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_OP => match thunk.peek_result()? {
            Ok(Value::Dict { entries, .. }) => {
                let key = HashableValue::Str(Arc::from(FIELD_NAME));
                match entries.get(&key)?.peek_result()? {
                    Ok(Value::String {
                        source, start, end, ..
                    }) => Some(source[*start..*end].to_string()),
                    _ => None,
                }
            }
            _ => None,
        },
        _ => None,
    }
}

/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
pub(crate) fn infer_class_decl_from_surface(
    decl: &ClassDeclSurface<'_>,
    state: &mut InferState,
) -> Result<Arc<Value>, Vec<Diagnostic>> {
    use crate::types::ClassDecl;
    // After S-1003: Kind deleted, params use Arc<Value> TypeValue kind.

    let ClassDeclSurface {
        name,
        params,
        superclasses,
        determines,
        structural,
        span,
        resolver,
        resolver_injective,
        pre_assigned_class_decl_id,
    } = decl;
    let span = span.clone();

    if name.is_empty() {
        return Err(vec![Diagnostic::error(
            "type-error",
            "class declaration must have a name declared with [class [ClassName ...] ...]",
            span,
        )]);
    }

    // Method body validation is handled by dict inference (Pass 2, typecheck_dict.rs).
    // resolve_type_expr is async and cannot be called from sync infer_class_decl_from_surface.
    // method_signatures is populated as empty (matches existing ClassDecl construction sites).

    // After S-1003: ClassDecl.params is Vec<(String, Arc<Value>)> where Arc<Value> is the kind TypeValue.
    let existing_param_kinds: std::collections::HashMap<String, Arc<Value>> = {
        let env_guard = state.env.read().unwrap();
        match env_guard.get_class(name) {
            Some(existing) => existing.params.iter().cloned().collect(),
            None => std::collections::HashMap::new(), // Class not yet registered, treat as zero params.
        }
    };

    let mut fd_indices: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for fd_node in *determines {
        match &fd_node.expr {
            SurfaceExpression::Dict(entries) if entries.len() == 2 => {
                let determining = &entries[0].node.value;
                let determining_indices =
                    extract_param_indices(determining, params, fd_node.span.clone())?;
                let determined = &entries[1].node.value;
                let determined_indices =
                    extract_param_indices(determined, params, fd_node.span.clone())?;
                fd_indices.push((determining_indices, determined_indices));
            }
            _ => {
                return Err(vec![Diagnostic::error("type-error",
                    "functional dependency must be a 2-element list [[determining-vars] determined-var(s)]",
                    fd_node.span.clone(),
                )]);
            }
        }
    }

    let structural_discharge = match *structural {
        STRUCTURAL_CLOSED_DICT => crate::type_class::StructuralDischarge::ClosedDict,
        _ => crate::type_class::StructuralDischarge::None,
    };

    // Use the pre-assigned class_decl_id if the resolver's Phase 1b scan set one.
    // This ensures the EffectPerform dispatch chain uses the same ID that was written
    // into VarAddr::EffectPerform during resolution. Fall back to a fresh ID for
    // the bootstrap typecheck path (where Phase 1b does not run).
    let class_decl_id = pre_assigned_class_decl_id
        .and_then(|cell| cell.get())
        .unwrap_or_else(next_class_decl_id);

    let class_decl = ClassDecl {
        class_decl_id,
        name: name.to_string(),
        params: params
            .iter()
            .map(|p| {
                // After S-1003: default kind is TypeValue.Op{name:"Type"} (kind *).
                let kind = existing_param_kinds
                    .get(p)
                    .cloned()
                    .unwrap_or_else(|| crate::type_infer::make_typevalue_op(KIND_TYPE));
                (p.clone(), kind)
            })
            .collect(),
        superclasses: superclasses
            .iter()
            .map(|(class_name, params)| (class_name.clone(), params.clone()))
            .collect(),
        determines: fd_indices,
        resolver: resolver.clone(),
        resolver_injective: *resolver_injective,
        structural_discharge,
        method_signatures: vec![],
    };

    state.env.write().unwrap().insert_class(class_decl.clone());
    state.invalidate_env_caches();
    // Class entries are stored in state.env (class registry).
    // resolve_type_head looks up classes via state.env.read().get_class(name).
    // No insertion into type_stage_scope needed.
    // After S-1003: Kind deleted, kind_env removed from InferState.
    // Operator-kinded params are now tracked as TypeValue kinds in ClassDecl.params.
    // No further action needed — the kind information is already in class_decl.params.

    // Populate non_injective_resolvers in the InferenceContext so that unify() can look up
    // injectivity for TV_APP~TV_APP without requiring access to the full Env.
    if !class_decl.resolver_injective {
        if let Some(ref resolver_name) = class_decl.resolver {
            state
                .ctx
                .non_injective_resolvers
                .insert(resolver_name.clone());
        }
    }

    Ok(make_typevalue_record(indexmap::IndexMap::new(), None))
}

/// Type alias for match arm type data (Surface version): (param_types, span, entries).
type SurfaceMatchArmData<'a> = (
    Vec<Arc<Value>>,
    Span,
    &'a Vec<Spanned<crate::ast::SurfaceEntry>>,
);

/// Type-check an [instance ...] declaration from SurfaceDeclaration::InstanceDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
pub(crate) async fn infer_instance_decl_from_surface(
    class_name: &str,
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<crate::ast::SurfaceEntry>>)],
    span: Span,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Arc<Value>, Vec<Diagnostic>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(make_typevalue_record(indexmap::IndexMap::new(), None));
    }

    let (param_count, has_fds, fd_list, param_names) = {
        let class_decl = state
            .env
            .read()
            .unwrap()
            .get_class(class_name)
            .ok_or_else(|| {
                vec![Diagnostic::error(
                    "type-error",
                    format!("unknown class '{}'", class_name),
                    span.clone(),
                )]
            })?;
        (
            class_decl.params.len(),
            !class_decl.determines.is_empty(),
            class_decl.determines.clone(),
            class_decl
                .params
                .iter()
                .map(|(n, _)| n.clone())
                .collect::<Vec<_>>(),
        )
    };

    let mut arm_data: Vec<SurfaceMatchArmData> = Vec::new();

    for (pattern_node, methods) in arms {
        let pattern_types = extract_pattern_types(pattern_node, env, state).await?;

        if pattern_types.len() != param_count {
            return Err(vec![Diagnostic::error(
                "type-error",
                format!(
                    "instance pattern has {} type parameters but class '{}' expects {}",
                    pattern_types.len(),
                    class_name,
                    param_count
                ),
                pattern_node.span.clone(),
            )]);
        }

        // Allow TV_VAR positions (declared TypeVars from bind: in [let ...]).
        // Reject only Unknown (empty dict or TV_UNKNOWN): those indicate missing annotations.
        if pattern_types.iter().any(|tv| {
            matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_UNKNOWN)
                || matches!(tv.as_ref(), Value::Dict { entries, .. } if entries.is_empty())
        }) {
            return Err(vec![Diagnostic::error("type-error",
                format!(
                    "instance pattern for class '{}' contains Unknown types — all pattern positions must have concrete type annotations (use a@Type syntax) or TypeVars declared via bind: in [let ...]",
                    class_name
                ),
                pattern_node.span.clone(),
            )]);
        }

        arm_data.push((pattern_types, pattern_node.span.clone(), methods));
    }

    for i in 0..arm_data.len() {
        for j in (i + 1)..arm_data.len() {
            let (types_i, span_i, _) = &arm_data[i];
            let (types_j, span_j, _) = &arm_data[j];

            if patterns_overlap(types_i, types_j, state).await? {
                let error = Diagnostic::error("type-error",
                    format!(
                        "overlapping instance patterns for class '{}': arm at line {} and arm at line {} could both match the same types",
                        class_name,
                        span_i.start_line,
                        span_j.start_line
                    ),
                    span_j.clone(),
                );
                return Err(vec![error]);
            }
        }
    }

    if has_fds {
        for (determining_indices, determined_indices) in &fd_list {
            for i in 0..arm_data.len() {
                for j in (i + 1)..arm_data.len() {
                    let (types_i, span_i, _) = &arm_data[i];
                    let (types_j, span_j, _) = &arm_data[j];

                    let determining_i: Vec<Arc<Value>> = determining_indices
                        .iter()
                        .map(|&idx| Arc::clone(&types_i[idx]))
                        .collect();
                    let determining_j: Vec<Arc<Value>> = determining_indices
                        .iter()
                        .map(|&idx| Arc::clone(&types_j[idx]))
                        .collect();

                    if types_can_unify(&determining_i, &determining_j, state).await? {
                        let determined_i: Vec<Arc<Value>> = determined_indices
                            .iter()
                            .map(|&idx| Arc::clone(&types_i[idx]))
                            .collect();
                        let determined_j: Vec<Arc<Value>> = determined_indices
                            .iter()
                            .map(|&idx| Arc::clone(&types_j[idx]))
                            .collect();

                        if !types_can_unify(&determined_i, &determined_j, state).await? {
                            let error = Diagnostic::error("type-error",
                                format!(
                                    "consistency violation for class '{}': arm at line {} and arm at line {} have overlapping determining positions but incompatible determined types",
                                    class_name,
                                    span_i.start_line,
                                    span_j.start_line
                                ),
                                span_j.clone(),
                            );
                            return Err(vec![error]);
                        }
                    }
                }
            }
        }
    }

    for (pattern_types, _arm_span, methods) in &arm_data {
        let inst_type = if pattern_types.len() == 1 {
            Arc::clone(&pattern_types[0])
        } else {
            let fields: indexmap::IndexMap<String, Arc<Value>> = pattern_types
                .iter()
                .enumerate()
                .map(|(i, tv)| (i.to_string(), Arc::clone(tv)))
                .collect();
            make_typevalue_record(fields, None)
        };

        // Extract type tags for instance binding name generation.
        // Only concrete uppercase type names contribute to the binding name — TypeVars and
        // Unknown are filtered out (same semantics as lower.rs:extract_dispatch_tags).
        let type_args: Vec<String> = pattern_types
            .iter()
            .filter_map(type_to_dispatch_tag)
            .collect();

        let mut method_types = HashMap::new();

        // Inject class type parameter bindings into type_stage_scope so that
        // method body annotations referencing the class's type parameters resolve to the
        // concrete pattern types for this arm.
        //
        // For example: class `Equatable [let a]` with method `=: [Fn@Boolean [a a]]` and
        // arm pattern `[let a@Integer]` (pattern_types = [Type::Int]) → inject `a → Int`.
        // Any annotation `@a` inside the method body now resolves to `Type::Int` instead
        // of the catch-all behavior. For the arm `[pattern [Int]]` where the pattern type
        // is already resolved to `Type::Int` (via the zero-arg Call fix in extract_binding_types),
        // this also injects the correct concrete type.
        //
        // Implementation: push a new innermost type_stage_scope frame (index 0) containing
        // the param_name → TypeValue (pattern_type) bindings. Pop it after all
        // methods in this arm are checked. This is a temporary scope — it does NOT leak into
        // other arms or the surrounding type environment.
        let param_type_frame: std::collections::HashMap<String, crate::type_infer::TypeValue> = {
            let mut frame = std::collections::HashMap::new();
            for (name, tv) in param_names.iter().zip(pattern_types.iter()) {
                // Skip TypeValue.Var — unresolved type variable, not a concrete pattern type.
                let is_var =
                    matches!(tv.as_ref(), Value::Variant { ctor, .. } if ctor.as_ref() == TV_VAR);
                if !is_var {
                    frame.insert(name.clone(), Arc::clone(tv));
                }
            }
            frame
        };
        let pushed_frame = !param_type_frame.is_empty();
        if pushed_frame {
            state.type_stage_scope.insert(0, param_type_frame.clone());
        }

        for method in *methods {
            let method_name = match &method.node.key {
                Some(key_node) => match &key_node.expr {
                    SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                    SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                    _ => {
                        if pushed_frame {
                            state.type_stage_scope.remove(0);
                        }
                        return Err(vec![Diagnostic::error(
                            "type-error",
                            "instance method name must be a string or identifier",
                            key_node.span.clone(),
                        )]);
                    }
                },
                None => {
                    if pushed_frame {
                        state.type_stage_scope.remove(0);
                    }
                    return Err(vec![Diagnostic::error(
                        "type-error",
                        "instance method must have a name",
                        method.span.clone(),
                    )]);
                }
            };

            // Bidirectional type checking — look up the class method's polymorphic
            // signature, instantiate it with the concrete pattern types, and set
            // state.expected_fn_params so infer_fn_push_cont can type unannotated params.
            //
            // Example: class Equatable [let a] with signature eq: [Fn@Boolean [a a]].
            // For instance [let a@Int], method_sig = Fn(a,a)→Bool → instantiate a→Int
            // → Fn(Int,Int)→Bool → expected_fn_params = [Int, Int].
            // Unannotated params x, y in `[fn [let x y] ...]` then get Type::Int instead
            // of Type::Unknown.
            //
            // ClassDecl.method_signatures is populated by the CEK pass when the [class ...]
            // declaration is processed. By the time [instance ...] arms are checked, the
            // class body has already been type-checked, so method_signatures is available.
            {
                // Build a TypeValue substitution from the param_type_frame, mapping class
                // param names (e.g., "a") to the concrete pattern TypeValues (e.g., Int).
                // param_type_frame contains TypeValue directly for each concrete param.
                let type_subst: HashMap<String, Arc<Value>> = param_type_frame
                    .iter()
                    .map(|(name, tv)| (name.clone(), Arc::clone(tv)))
                    .collect();

                // Look up this method's polymorphic signature in the class.
                let method_sig_opt =
                    state
                        .env
                        .read()
                        .unwrap()
                        .get_class(class_name)
                        .and_then(|cd| {
                            cd.method_signatures
                                .iter()
                                .find(|(n, _)| n == &method_name)
                                .map(|(_, tv)| Arc::clone(tv))
                        });

                if let Some(poly_sig) = method_sig_opt {
                    // Apply the substitution to the polymorphic method type to get the
                    // specialized (monomorphic) signature for this instance arm.
                    let concrete_sig =
                        crate::types::apply_typevalue_renaming(&poly_sig, &type_subst);

                    // Extract the param types from the concrete TypeValue.Fn.
                    if let Some((param_types, _ret)) =
                        crate::type_infer::typevalue_fn_params_and_ret(&concrete_sig)
                    {
                        if !param_types.is_empty() {
                            state.expected_fn_params = Some(param_types);
                        }
                    }
                }
                // If method_signatures is empty (class body not yet processed or no signature),
                // or if the method is not found, skip — unannotated params fall back to Unknown.
            }

            let mut method_errors: Vec<Diagnostic> = Vec::new();
            let mut method_stack = Vec::new();
            let method_impl_type = Box::pin(typecheck_cek::run_typecheck(
                &method.node.value,
                env,
                state,
                &mut method_errors,
                type_map,
                &mut method_stack,
            ))
            .await;
            // Clear expected_fn_params in case run_typecheck didn't consume it
            // (e.g. the method body is not a fn expression).
            state.expected_fn_params = None;
            if !method_errors.is_empty() {
                if pushed_frame {
                    state.type_stage_scope.remove(0);
                }
                return Err(method_errors);
            }

            // T-2145 Feature 1: Validate instance method params against class declaration signature.
            //
            // For each instance method, check that its parameter types are compatible with
            // the class method signature (after substituting class type parameters with the
            // instance's concrete pattern types). Emit a warning if they don't unify.
            //
            // This is a TYPECHECK-TIME validation — the runtime dispatch handles correctness,
            // but catching param mismatches here improves the developer experience.
            {
                // Look up the class method signature.
                let class_method_sig_opt = state
                    .env
                    .read()
                    .unwrap()
                    .get_class(class_name)
                    .and_then(|cd| {
                        cd.method_signatures
                            .iter()
                            .find(|(n, _)| n == &method_name)
                            .map(|(_, tv)| Arc::clone(tv))
                    });

                if let Some(poly_sig) = class_method_sig_opt {
                    // Substitute class params with the concrete instance pattern types.
                    let type_subst: HashMap<String, Arc<Value>> = param_type_frame
                        .iter()
                        .map(|(name, tv)| (name.clone(), Arc::clone(tv)))
                        .collect();
                    let concrete_class_sig =
                        crate::types::apply_typevalue_renaming(&poly_sig, &type_subst);

                    // Extract param types from both the class signature and the instance method.
                    let class_params_opt =
                        crate::type_infer::typevalue_fn_params_and_ret(&concrete_class_sig);
                    let method_params_opt =
                        crate::type_infer::typevalue_fn_params_and_ret(&method_impl_type);

                    match (class_params_opt, method_params_opt) {
                        (Some((class_params, _class_ret)), Some((method_params, _method_ret))) => {
                            // Check that the instance method has the same arity as the class signature.
                            if class_params.len() != method_params.len() {
                                state.diagnostics.push(Diagnostic::warn(
                                    "type-warning",
                                    format!(
                                        "instance method '{}' for class '{}' has {} parameters, but class signature expects {}",
                                        method_name,
                                        class_name,
                                        method_params.len(),
                                        class_params.len()
                                    ),
                                    method.span.clone(),
                                ));
                            } else {
                                // Check that each param type unifies with the class signature's param type.
                                // Use a fresh InferenceContext snapshot to avoid polluting the global state.
                                let mut validation_ctx = InferenceContext::from_snapshot(
                                    state.ctx.subst.clone(),
                                    state.ctx.levels.clone(),
                                    state.ctx.current_level,
                                    state.ctx.tycon_env.clone(),
                                );
                                let mut validation_constraints = Vec::new();

                                for (i, (class_param, method_param)) in
                                    class_params.iter().zip(method_params.iter()).enumerate()
                                {
                                    let unify_result = Box::pin(crate::types::unify(
                                        class_param,
                                        method_param,
                                        &mut validation_ctx,
                                        &mut validation_constraints,
                                        method.span.clone(),
                                        0,
                                    ))
                                    .await;

                                    if let Err(err) = unify_result {
                                        state.diagnostics.push(
                                            Diagnostic::warn(
                                                "type-warning",
                                                format!(
                                                    "instance method '{}' parameter {} has type incompatible with class signature",
                                                    method_name, i
                                                ),
                                                method.span.clone(),
                                            )
                                            .with_note(format!(
                                                "class signature expects: {}",
                                                crate::eval::format_type_for_assert(
                                                    class_param
                                                )
                                            ))
                                            .with_note(format!(
                                                "instance method has: {}",
                                                crate::eval::format_type_for_assert(
                                                    method_param
                                                )
                                            ))
                                            .with_note(format!("unification failed: {}", err.message)),
                                        );
                                    }
                                }
                            }
                        }
                        (None, Some(_)) => {
                            // Instance method is a function, but class signature is not.
                            state.diagnostics.push(Diagnostic::warn(
                                "type-warning",
                                format!(
                                    "instance method '{}' for class '{}' is a function, but class signature is not",
                                    method_name, class_name
                                ),
                                method.span.clone(),
                            ));
                        }
                        (Some(_), None) => {
                            // Class signature is a function, but instance method is not.
                            state.diagnostics.push(Diagnostic::warn(
                                "type-warning",
                                format!(
                                    "instance method '{}' for class '{}' is not a function, but class signature expects one",
                                    method_name, class_name
                                ),
                                method.span.clone(),
                            ));
                        }
                        (None, None) => {
                            // Neither is a function — this is OK (methods can be non-function values).
                            // No validation needed.
                        }
                    }
                }
                // If class_method_sig is None, the class has no signature for this method —
                // this is OK (the method might be added dynamically). No validation needed.
            }

            method_types.insert(method_name.clone(), method_impl_type.clone());

            // Insert TypeValue for the ɪ-prefixed binding name so that VarRef resolution
            // can find the method type. This mirrors what lower.rs does at runtime:
            // lower.rs creates a dict entry with key `ɪɴꜱᴛᴀɴᴄᴇ⧼Class∷method⟨T⟩⧽` and the
            // type checker must insert a matching TypeValue at that name.
            let type_args_str: Vec<&str> = type_args.iter().map(|s| s.as_str()).collect();
            let binding_name =
                crate::type_def::instance_binding_name(class_name, &method_name, &type_args_str);

            let scheme = generalize_tv(state.ctx.current_level, &method_impl_type, &state.ctx);

            // Insert into the parent dict env at the resolver-assigned slot, or append.
            // The ɪ-prefixed binding name is assigned a slot by the resolver since lower.rs
            // creates a concrete dict entry for it.
            {
                let mut env_write = env.write().unwrap();
                let slot = find_slot_in_frames(&state.resolver_frames, &binding_name)
                    .unwrap_or_else(|| env_write.slots.len());
                env_write.insert_at_slot(slot, binding_name, scheme, None);
            }
        }

        // Pop the type param scope frame pushed before method body checking.
        if pushed_frame {
            state.type_stage_scope.remove(0);
        }

        let det_positions: Vec<usize> = {
            let mut seen = std::collections::HashSet::new();
            let mut positions = Vec::new();
            for (det_indices, _) in &fd_list {
                for &idx in det_indices {
                    if seen.insert(idx) {
                        positions.push(idx);
                    }
                }
            }
            positions.sort_unstable();
            positions
        };

        let instance_decl = InstanceDecl {
            class_name: class_name.to_string(),
            instance_type: inst_type,
            det_positions,
            method_types,
        };

        // Structural overlap is async (calls unify) and cannot be checked from this sync function.
        // Exact duplicates are caught by the mangled-key dedup in env.insert_instance.
        // After S-1003: instance_type is Arc<Value>. Use the dispatch tag strings for mangling.
        // type_args contains the dispatch tag strings derived from pattern_types.
        let type_args_for_mangling: Vec<&str> = type_args.iter().map(String::as_str).collect();
        let mangled = format!(
            "ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽",
            instance_decl.class_name,
            type_args_for_mangling.join(",")
        );
        state
            .env
            .write()
            .unwrap()
            .insert_instance(mangled, instance_decl);
        state.invalidate_env_caches();
    }

    Ok(make_typevalue_record(indexmap::IndexMap::new(), None))
}

// contains_unknown_or_top removed — check_surface_expr now uses constrain() which handles
// gradual types internally (Unknown absorption in unify). The helper is no longer needed.

#[cfg(test)]
#[path = "typecheck_tests.rs"]
mod tests;
