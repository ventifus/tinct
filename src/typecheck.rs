//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[cfg(test)]
use crate::ast::Pattern;
use crate::ast::{
    Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceExpression, SurfaceItem,
    SurfaceNode, SurfaceProgram, TypeAnnotationTable,
};
use crate::env::Env;
#[cfg(test)]
use crate::types::TypeEnv;
use crate::types::{generalize, InferState, Row, Type, TypeAlias, TypeError};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
pub(crate) mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;
// Special-case type refinement dispatchers for polymorphic builtins
// Path-sensitive narrowing, pattern binding extraction, overlap checking
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

/// Map from source span `(start_offset, end_offset)` to inferred type. Populated during type
/// checking so LSP hover/diagnostics can look up types without re-running inference. Offsets
/// are sufficient as keys; the full `Span` source text is not needed.
pub type TypeMap = HashMap<(usize, usize), Type>;

/// Map from variable/parameter name to its documentation string.
/// Populated during type checking by extracting `doc:` properties from annotations.
pub type DocMap = HashMap<String, String>;

// MatchArmData (old Expr-based) removed — replaced by SurfaceMatchArmData.

/// Re-export SchemeMap from types for LSP consumers.
pub use crate::types::SchemeMap;

/// Type-check a SurfaceProgram and return a TypeAnnotationTable.
///
/// This is the runtime-v2 entry point for type checking used by the eval pipeline.
/// The SurfaceProgram is unchanged (immutable); all type annotations are captured
/// in the returned table keyed by NodeId.
///
/// For the public API that returns a `TypeMap` (span-keyed, compatible with the
/// LSP and import-resolution paths), use [`typecheck_surface_program`] instead.
///
/// # Algorithm
///
/// 1. For each SurfaceDocument, walk items (expressions + declarations)
/// 2. Run type inference via `typecheck_cek::run_typecheck` (CEK machine)
/// 3. Capture inferred types in TypeAnnotationTable keyed by NodeId
///
/// The type environment is threaded across documents:
/// % bindings, %name bindings, dict-scoped let-generalization.
///
/// # Returns
///
/// Returns `(errors, table)` where:
/// - `errors`: Type errors encountered during inference (advisory — evaluation proceeds)
/// - `table`: TypeAnnotationTable mapping NodeId → Type for successfully inferred expressions
pub async fn typecheck_surface_program_annotation_table(
    program: &SurfaceProgram,
) -> (
    Vec<TypeError>,
    TypeAnnotationTable,
    crate::type_def::TyConEnv,
) {
    typecheck_surface_program_annotation_table_with_env(
        program,
        Arc::new(RwLock::new(Env::new())),
        None,
        std::collections::HashMap::new(),
        None,
    )
    .await
}

/// Type-check a `SurfaceProgram` starting from a pre-seeded type environment.
///
/// Identical to [`typecheck_surface_program_annotation_table`] but accepts an initial
/// `TypeEnv` and an optional `EvalContext`. When `eval_ctx` is provided, the type
/// normalizer can evaluate TypeStageApp nodes (e.g. `Integer → TypeNode.Int → Type::Int`)
/// using the runtime evaluator, enabling the type-stage for the init program's type-check.
pub async fn typecheck_surface_program_annotation_table_with_env(
    program: &SurfaceProgram,
    initial_env: Arc<RwLock<Env>>,
    eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
    seed_tycon_env: crate::type_def::TyConEnv,
    type_stage_map: Option<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>>,
) -> (
    Vec<TypeError>,
    TypeAnnotationTable,
    crate::type_def::TyConEnv,
) {
    let mut errors = Vec::new();
    let mut table = TypeAnnotationTable::new();
    let mut env: Arc<RwLock<Env>> = initial_env;
    let mut state = InferState::new();
    state.eval_ctx = eval_ctx;
    state.tycon_env = seed_tycon_env;
    // Always seed type_stage_map with Unknown → Type::Unknown so that `@Unknown`
    // (the gradual-typing escape hatch) resolves through the unified Step 3 path
    // in resolve_type_head. Production callers supply a full map from type-stage
    // documents; this seed is merged in regardless so `Unknown` is always available.
    // The caller-supplied entries win on conflict (or_insert semantics below).
    let mut effective_type_stage_map = {
        let mut m = std::collections::HashMap::new();
        m.insert(
            "Unknown".to_string(),
            crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::Unknown),
        );
        m
    };
    if let Some(caller_map) = type_stage_map {
        // Caller entries override the seed (production type-stage map takes precedence).
        effective_type_stage_map.extend(caller_map);
    }
    state.type_stage_map = Some(effective_type_stage_map);
    // Compute and store the resolution table for slot-indexed VarRef lookup.
    // No runtime env at the type-checker path; pass empty initial_frames for bootstrap mode.
    let (resolve_table, _frames) = crate::resolve::resolve_surface_program(program, &[]);
    state.resolution_table = Some(std::sync::Arc::new(resolve_table));

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        let (new_env, _, mut doc_errors) =
            process_document(doc, &env, &mut state, &mut table, &mut None).await;
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking propagation.
        errors.append(&mut doc_errors);
    }

    (errors, table, state.tycon_env)
}

/// Type-check a `SurfaceProgram` with a given initial type environment.
///
/// This is the native-Surface implementation — it delegates to
/// [`typecheck_surface_program_with_env`] which walks `program.documents` directly via
/// [`process_document`] without any conversion through the old `File` AST.
/// The span-keyed [`TypeMap`] in the return tuple is always empty; callers that need
/// per-expression type information should use the [`TypeAnnotationTable`] returned by
/// [`typecheck_surface_program_with_env`] instead.
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics)`
///
/// The returned [`TypeMap`] is span-keyed and built from the `TypeAnnotationTable` produced
/// during inference (populated per-node by `process_document`). Only top-level
/// expression nodes are inserted in the table; inner sub-expressions are included via the
/// recursive `collect_type_map_from_node` walk.
pub async fn typecheck_surface_program(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    let (errors, type_map, doc_map, scheme_map, diagnostics, _state, _env, _annotation_table) =
        typecheck_surface_program_with_env(
            program,
            parent_env,
            true,
            std::collections::HashMap::new(),
            None,
        )
        .await;
    // type_map is now populated during inference (enable_hover_map=true path).
    (errors, type_map, doc_map, scheme_map, diagnostics)
}

/// Type-check a `SurfaceProgram` with full control over scheme-map generation,
/// returning all intermediate state including a [`TypeAnnotationTable`] for the evaluator's
/// lowering pass.
///
/// This is the native-Surface implementation — it walks `program.documents` directly
/// via [`process_document`] without any conversion through the old `File` AST.
/// The [`TypeAnnotationTable`] is populated directly during inference (keyed by `NodeId`
/// of the original `Arc<SurfaceNode>`) — no span-based correlation is needed.
///
/// # Parameters
///
/// - `program`: The surface AST to type-check.
/// - `parent_env`: Initial type environment. All classes and instances visible to this
///   program must already be in `parent_env`'s chain (populated by prior type-checking runs
///   via `TypeContext`).
/// - `enable_hover_map`: When `true`, populates the [`SchemeMap`] for LSP hover.
/// - `seed_tycon_env`: Pre-populated type constructor definitions. Propagates opaque types
///   (DirCap, File, ClockCap, Handle, etc.) declared in `builtin_core.llt` to subsequent
///   module type-checks without requiring re-declaration.
/// - `eval_ctx`: Optional `EvalContext` for type-stage scope-chain lookup.
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics, infer_state, final_env, annotation_table)`
///
/// `type_map` and `doc_map` are currently empty — all callers discard them. If a caller
/// needs span-keyed types, use [`typecheck_surface_program`] instead.
#[allow(clippy::type_complexity)]
pub async fn typecheck_surface_program_with_env(
    program: &SurfaceProgram,
    parent_env: Arc<RwLock<Env>>,
    enable_hover_map: bool,
    seed_tycon_env: std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
    eval_ctx: Option<std::sync::Arc<crate::eval::EvalContext>>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
    InferState,
    Arc<RwLock<Env>>,
    TypeAnnotationTable,
) {
    let mut errors = Vec::new();
    let mut diagnostics = Vec::new();
    // Create a child Env scope for this type-checking session: reads walk through
    // to the parent (finding prelude classes/instances), writes stay in the child.
    let child_env = Arc::new(RwLock::new(Env::with_parent(Arc::clone(&parent_env))));
    let mut env: Arc<RwLock<Env>> = Arc::clone(&child_env);
    let mut state = InferState::with_env(Arc::clone(&child_env));
    state.eval_ctx = eval_ctx;
    // Seed type_stage_map with Unknown → Type::Unknown so that `@Unknown` resolves
    // through the unified Step 3 path in resolve_type_head. This function is called
    // by typecheck_source (corpus tests, LSP diagnostics) which do not evaluate
    // type-stage documents. The full production loader uses
    // typecheck_surface_program_annotation_table_with_env instead (which has its own
    // equivalent seed). The seed ensures `@Unknown` never requires a special case.
    {
        let mut seed = std::collections::HashMap::new();
        seed.insert(
            "Unknown".to_string(),
            crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::Unknown),
        );
        state.type_stage_map = Some(seed);
    }
    // Seed tycon_env from the TypeContext's accumulated TyConDefs. This propagates
    // opaque types (DirCap, File, ClockCap, Handle, etc.) declared in builtin_core.llt
    // to subsequent module type-checks (builtin_io.llt, builtin_async.llt, ...) so that
    // @DirCap and similar annotations resolve correctly without re-declaration.
    // Use or_insert so that static TyConDefs (with correct primitive bodies) are never
    // overwritten by dynamic declarations that produce nominal bodies.
    for (name, def) in seed_tycon_env {
        state.tycon_env.entry(name).or_insert(def);
    }
    // Seed the resolver from scope 0 when an eval_ctx is available — this includes builtins
    // and any host-injected names (capabilities, CLI variables, etc.). Fall back to
    // core_builtins() only in bootstrap contexts where no runtime arena exists.
    let root_frame: indexmap::IndexMap<String, u32> = if let Some(ref ctx) = state.eval_ctx {
        let arena = ctx.scope_arena.borrow();
        if !arena.scopes.is_empty() {
            arena.scopes[0]
                .slots
                .iter()
                .enumerate()
                .filter_map(|(slot, t)| {
                    Some((t.as_ref()?.span.name.as_deref()?.to_string(), slot as u32))
                })
                .collect()
        } else {
            crate::builtins_core::core_builtins()
                .iter()
                .enumerate()
                .map(|(i, def)| (def.name.to_string(), i as u32))
                .collect()
        }
    } else {
        crate::builtins_core::core_builtins()
            .iter()
            .enumerate()
            .map(|(i, def)| (def.name.to_string(), i as u32))
            .collect()
    };
    let (resolve_table, _frames) = crate::resolve::resolve_surface_program(program, &[root_frame]);
    state.resolution_table = Some(Arc::new(resolve_table));

    if enable_hover_map {
        state.scheme_map = Some(SchemeMap::new());
    }

    let mut annotation_table = TypeAnnotationTable::new();
    // type_map_inner accumulates span→type for all sub-expressions (for LSP hover).
    // Populated when enable_hover_map is true (i.e., LSP path), empty otherwise.
    let mut type_map_inner = TypeMap::new();
    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        let mut type_map_ref: Option<&mut TypeMap> = if enable_hover_map {
            Some(&mut type_map_inner)
        } else {
            None
        };

        let (new_env, _, mut doc_errors) = process_document(
            doc,
            &env,
            &mut state,
            &mut annotation_table,
            &mut type_map_ref,
        )
        .await;
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking env propagation.
        errors.append(&mut doc_errors);
    }

    // Extract scheme_map from state (populated during VarRef inference).
    let scheme_map = state.scheme_map.take().unwrap_or_default();

    // Collect diagnostics from state (e.g., T013 ambiguous constraints).
    diagnostics.append(&mut state.diagnostics);

    // Extract doc strings from the Surface AST (equivalent to extract_doc_strings on File AST).
    // Only needed when enable_hover_map is true (i.e., LSP path — doc_map is for hover).
    let doc_map = if enable_hover_map {
        let mut doc_map = DocMap::new();
        extract_doc_strings_surface(program, &mut doc_map);
        doc_map
    } else {
        DocMap::new()
    };

    // Merge the document-level Env scheme bindings back into the child Env.
    // This ensures that variables declared in this program are visible to callers
    // that hold the returned Arc<RwLock<Env>> (e.g., subsequent builtin-typecheck-doc calls).
    merge_env_schemes_into_env(&env, &child_env);

    (
        errors,
        type_map_inner,
        doc_map,
        scheme_map,
        diagnostics,
        state,
        child_env,
        annotation_table,
    )
}

/// Merge ALL scheme bindings from an `Arc<RwLock<Env>>` chain into a target `Arc<RwLock<Env>>`.
///
/// After type-checking a program's documents, the final env is a chain of frames
/// (one per document, plus the initial parent frame). This function walks all
/// own-frame bindings and copies their schemes into `target_env` (the child Env) so that
/// callers holding the child Env can see all new bindings.
///
/// Since `target_env.parent == parent_env`, schemes that already exist in the parent
/// chain are already visible — no filtering is needed. Duplicate insertion is safe
/// (insert_scheme and insert_scheme_named_only are idempotent for same-name, same-value).
fn merge_env_schemes_into_env(source_env: &Arc<RwLock<Env>>, target_env: &Arc<RwLock<Env>>) {
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
                guard.insert_scheme(name.to_string(), scheme.clone());
            }
        }
        for (name, slot) in &frame.extras {
            if let Some(ref scheme) = slot.scheme {
                guard.insert_scheme_named_only(name.clone(), scheme.clone());
            }
        }
        for (name, alias) in frame.own_type_aliases() {
            guard.insert_type_alias(name.to_string(), alias.clone());
        }
        for (_, decl) in &frame.classes {
            guard.insert_class(decl.clone());
        }
        for (mangled, decl) in &frame.instances {
            guard.insert_instance(mangled.clone(), decl.clone());
        }
    }
}

/// Type-check a single [`SurfaceDocument`] using the CEK machine.
///
/// Replaces [`typecheck_surface_document`]. Processes all document items in source order
/// (Decls interleaved with Exprs, no pre-pass hoisting). Each intermediate item extends the
/// env for subsequent items. The last item's schemes are threaded into the result env.
///
/// # Returns
///
/// `(result_env, result_type, errors)` where:
/// - `result_env`: env containing schemes from the last dict body, exported to subsequent documents
/// - `result_type`: the type of the last expression (or empty-dict for empty documents)
/// - `errors`: type errors encountered during inference (non-fatal — env always propagated)
pub(crate) async fn process_document(
    doc: &SurfaceDocument,
    parent_env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    table: &mut TypeAnnotationTable,
    type_map: &mut Option<&mut TypeMap>,
) -> (Arc<RwLock<Env>>, Type, Vec<TypeError>) {
    let empty_dict_ty = Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    // Collect all items in source order as SurfaceNodes.
    // SurfaceItem::Expr → use the node directly.
    // SurfaceItem::Decl → synthetic node with SurfaceExpression::Decl so infer_step::Decl
    //   handles class/instance registration and TypeAlias (returns Type::Any, a no-op).
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
        let result_env_inner = Env::with_parent(Arc::clone(parent_env));
        return (
            Arc::new(RwLock::new(result_env_inner)),
            empty_dict_ty,
            Vec::new(),
        );
    }

    let mut errors = Vec::new();
    let mut current_env = Arc::clone(parent_env);
    let enclosing_level = state.level;

    // Process all intermediate items (all but the last) by extending the env.
    // This is the same logic as infer_step::Sequential: dict bodies → run_typecheck_dict
    // (preserving let-polymorphism and ctor_schemes); non-dict bodies → run_typecheck.
    let intermediates = &nodes[..nodes.len() - 1];
    for intermediate in intermediates {
        if let SurfaceExpression::Dict(entries) = &intermediate.expr {
            let (_, schemes, mut errs) = typecheck_cek::run_typecheck_dict(
                entries,
                &current_env,
                state,
                type_map,
                intermediate.span.clone(),
            )
            .await;
            errors.append(&mut errs);
            for (nid, ty) in state.type_annotation_table.drain() {
                table.insert(nid, ty);
            }
            let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
            for (name, scheme) in &schemes {
                new_env_inner.insert_scheme(name.clone(), scheme.clone());
            }
            register_type_aliases_env(intermediate, &mut new_env_inner, state, &mut errors);
            current_env = Arc::new(RwLock::new(new_env_inner));
        } else {
            // Non-dict (including Decl nodes): run_typecheck at incremented level.
            state.level += 1;
            let ty = typecheck_cek::run_typecheck(
                intermediate,
                &current_env,
                state,
                &mut errors,
                type_map,
                &mut Vec::new(),
            )
            .await;
            state.level = enclosing_level;
            for (nid, ty) in state.type_annotation_table.drain() {
                table.insert(nid, ty);
            }
            match &ty {
                Type::Dict(Row { fields, .. }) => {
                    let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
                    for (name, field_ty) in fields {
                        let scheme = generalize(enclosing_level, field_ty, state);
                        new_env_inner.insert_scheme(name.clone(), scheme);
                    }
                    register_type_aliases_env(intermediate, &mut new_env_inner, state, &mut errors);
                    current_env = Arc::new(RwLock::new(new_env_inner));
                }
                Type::Unknown | Type::Any => {}
                _ => errors.push(TypeError::not_a_record(&ty, intermediate.span.clone())),
            }
        }
    }

    // Process the last expression, preserving ctor_schemes for cross-document scoping.
    // Dict last expressions must call run_typecheck_dict directly — AfterDictPassZero
    // discards the ctor_schemes (North/South/etc.) that are essential for result_env.
    let last_node = Arc::clone(nodes.last().unwrap());
    let mut last_dict_schemes: Option<indexmap::IndexMap<String, crate::types::TypeScheme>> = None;
    let mut last_record_type: Option<(Type, u32)> = None;

    let result_ty = if let SurfaceExpression::Dict(entries) = &last_node.expr {
        let (dict_ty, schemes, mut dict_errs) = typecheck_cek::run_typecheck_dict(
            entries,
            &current_env,
            state,
            type_map,
            last_node.span.clone(),
        )
        .await;
        errors.append(&mut dict_errs);
        for (nid, ty) in state.type_annotation_table.drain() {
            table.insert(nid, ty);
        }
        last_dict_schemes = Some(schemes);
        dict_ty
    } else {
        state.level += 1;
        let ty = typecheck_cek::run_typecheck(
            &last_node,
            &current_env,
            state,
            &mut errors,
            type_map,
            &mut Vec::new(),
        )
        .await;
        state.level = enclosing_level;
        for (nid, ty) in state.type_annotation_table.drain() {
            table.insert(nid, ty);
        }
        if matches!(&ty, Type::Dict(_)) {
            last_record_type = Some((ty.clone(), enclosing_level));
        }
        ty
    };

    // Build result_env with parent=parent_env (flat env chain invariant).
    let mut result_env_inner = Env::with_parent(Arc::clone(parent_env));
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env_inner.insert_scheme(name, scheme);
        }
    }
    if let Some((Type::Dict(Row { fields, .. }), enc_level)) = last_record_type {
        for (name, field_ty) in fields {
            let scheme = generalize(enc_level, &field_ty, state);
            result_env_inner.insert_scheme(name, scheme);
        }
    }
    register_type_aliases_env(&last_node, &mut result_env_inner, state, &mut errors);

    (Arc::new(RwLock::new(result_env_inner)), result_ty, errors)
}

/// Extract documentation strings from a SurfaceProgram.
///
/// Walks the Surface AST looking for `doc:` properties in `@[...]` annotations on
/// function parameters and return annotations.
fn extract_doc_strings_surface(program: &SurfaceProgram, doc_map: &mut DocMap) {
    for doc_spanned in &program.documents {
        for item in &doc_spanned.node.items {
            if let SurfaceItem::Expr(node) = item {
                extract_doc_from_surface_node(node, doc_map, None);
            }
        }
    }
}

/// Recursively extract doc strings from a SurfaceNode.
fn extract_doc_from_surface_node(
    node: &std::sync::Arc<crate::ast::SurfaceNode>,
    doc_map: &mut DocMap,
    binding_name: Option<&str>,
) {
    use crate::ast::SurfaceExpression;
    match &node.expr {
        SurfaceExpression::Fn {
            params,
            body,
            return_ann,
            ..
        } => {
            // Extract doc from return annotation (fn@[doc: "..."])
            if let Some(ann) = return_ann {
                if let Some(doc_node) = ann.node.get_property("doc") {
                    if let SurfaceExpression::StringLiteral {
                        content: doc_string,
                        ..
                    } = &doc_node.expr
                    {
                        if let Some(name) = binding_name {
                            doc_map.insert(name.to_string(), doc_string.clone());
                        }
                    }
                }
            }
            // Extract doc from each parameter annotation
            for param_spanned in params {
                if let Some(ref ann) = param_spanned.node.annotation {
                    if let Some(doc_node) = ann.node.get_property("doc") {
                        if let SurfaceExpression::StringLiteral {
                            content: doc_string,
                            ..
                        } = &doc_node.expr
                        {
                            doc_map.insert(param_spanned.node.name.clone(), doc_string.clone());
                        }
                    }
                }
            }
            extract_doc_from_surface_node(body, doc_map, None);
        }
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                let key_name: Option<String> =
                    entry.node.key.as_ref().and_then(|k| match &k.expr {
                        SurfaceExpression::VarRef {
                            name,
                            annotation: Some(ann),
                            ..
                        } => {
                            if let Some(doc_node) = ann.node.get_property("doc") {
                                if let SurfaceExpression::StringLiteral {
                                    content: doc_string,
                                    ..
                                } = &doc_node.expr
                                {
                                    doc_map.insert(name.clone(), doc_string.clone());
                                }
                            }
                            Some(name.clone())
                        }
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
                        _ => None,
                    });
                extract_doc_from_surface_node(&entry.node.value, doc_map, key_name.as_deref());
            }
        }
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            extract_doc_from_surface_node(func, doc_map, None);
            for a in args {
                extract_doc_from_surface_node(a, doc_map, None);
            }
            for na in named_args {
                extract_doc_from_surface_node(&na.node.value, doc_map, None);
            }
        }
        SurfaceExpression::TypeAssert { expr, .. } => {
            extract_doc_from_surface_node(expr, doc_map, None);
        }
        SurfaceExpression::Field { expr, .. } => {
            if let Some(target) = expr {
                extract_doc_from_surface_node(target, doc_map, None);
            }
        }
        SurfaceExpression::Pipe { lhs, rhs } => {
            extract_doc_from_surface_node(lhs, doc_map, None);
            extract_doc_from_surface_node(rhs, doc_map, None);
        }
        SurfaceExpression::Sequential(nodes) => {
            for n in nodes {
                extract_doc_from_surface_node(n, doc_map, None);
            }
        }
        _ => {}
    }
}

/// Pre-register type aliases in `target_env` before Pass 1 inference.
///
/// Scans a dict node for `Name: [type ...]` entries and inserts stub `TypeAlias` entries
/// (with `body: Type::Unknown`) so that forward references within the same dict resolve
/// correctly when Pass 2 fills in the real bodies.
pub(crate) fn register_type_aliases_env(
    node: &Arc<SurfaceNode>,
    target_env: &mut Env,
    _state: &mut InferState,
    _errors: &mut Vec<TypeError>,
) {
    if let SurfaceExpression::Dict(entries) = &node.expr {
        let mut alias_entries: Vec<(String, Vec<String>)> = Vec::new();
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let SurfaceExpression::StringLiteral { content: name, .. } = &key.expr {
                    if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                        if let SurfaceDeclaration::TypeAlias { params, .. } = decl_box.as_ref() {
                            let param_names: Vec<String> =
                                params.iter().map(|(n, _)| n.clone()).collect();
                            alias_entries.push((name.clone(), param_names.clone()));
                            target_env.insert_type_alias(
                                name.clone(),
                                TypeAlias {
                                    params: param_names,
                                    body: Type::Unknown,
                                },
                            );
                        }
                    }
                }
            }
        }
    }
}

/// Map a resolved `Type` to the dispatch tag string used in instance binding names.
///
/// This mapping must match `extract_dispatch_tags` in `lower.rs`, which reads `@Annotation`
/// names from instance arm patterns.  Annotations are written as `@Integer`, `@Float`,
/// `@String`, `@Boolean`, `@Bytes`, `@TyConName` — the strings that appear in
/// `instance_binding_name` calls.
///
/// Returns `None` for:
/// - Unbound `TypeVar` (instance not yet determined).
/// - `Unknown` / `Top` / `Error` (gradual / lattice types that don't correspond to instances).
/// - Compound types that don't map to a single dispatch tag (records, functions, unions).
///
/// `IntLiteral`/`StringLiteral` are promoted to `"Integer"`/`"String"` because instance arms
/// are always annotated with the widened type (e.g., `@Integer`, never `@42`).
fn type_to_dispatch_tag(ty: &Type) -> Option<String> {
    match ty {
        Type::Int | Type::IntLiteral(_) => Some("Integer".to_string()),
        Type::Float => Some("Float".to_string()),
        Type::Str | Type::StringLiteral(_) => Some("String".to_string()),
        // Bytes is a direct variant (not TyCon("Bytes")).
        Type::Bytes => Some("Bytes".to_string()),
        // TyCon: map to the type name directly. Instance arm annotations must match the declared
        // type name exactly (e.g., @Boolean not @Bool) for dispatch tags to align.
        Type::TyCon(name) => Some(name.clone()),
        // Unresolved inference variables and gradual types cannot be dispatched.
        Type::TypeVar(_, _) | Type::Unknown | Type::Any | Type::Error(_) => None,
        // Compound types don't correspond to single-param dispatch tags.
        _ => None,
    }
}

/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
#[allow(clippy::too_many_arguments)]
pub(crate) fn infer_class_decl_from_surface(
    name: &str,
    params: &[String],
    superclasses: &[(String, String)],
    methods: &[Spanned<crate::ast::SurfaceEntry>],
    determines: &[Arc<SurfaceNode>],
    resolver: &Option<Arc<SurfaceNode>>,
    resolver_injective: bool,
    structural: &str,
    span: Span,
    _env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    _type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::types::{ClassDecl, Kind};

    if name.is_empty() {
        return Err(vec![TypeError::new(
            "class declaration must have a name declared with [class [ClassName ...] ...]",
            span,
        )]);
    }

    // Method body validation is handled by dict inference (Pass 2, typecheck_dict.rs).
    // resolve_type_expr is async and cannot be called from sync infer_class_decl_from_surface.
    // method_signatures is populated as empty (matches existing ClassDecl construction sites).
    let _ = methods; // suppress unused warning

    let existing_param_kinds: std::collections::HashMap<String, Kind> = {
        let env_guard = state.env.read().unwrap();
        env_guard
            .get_class(name)
            .map(|existing| existing.params.iter().cloned().collect())
            .unwrap_or_default()
    };

    let mut fd_indices: Vec<(Vec<usize>, Vec<usize>)> = Vec::new();
    for fd_node in determines {
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
                return Err(vec![TypeError::new(
                    "functional dependency must be a 2-element list [[determining-vars] determined-var(s)]",
                    fd_node.span.clone(),
                )]);
            }
        }
    }

    let resolver_name = if let Some(resolver_node) = resolver {
        match &resolver_node.expr {
            SurfaceExpression::VarRef { name: n, .. } => Some(n.clone()),
            SurfaceExpression::StringLiteral { content, .. } => Some(content.clone()),
            _ => {
                return Err(vec![TypeError::new(
                    "resolver must be an identifier or string",
                    resolver_node.span.clone(),
                )]);
            }
        }
    } else {
        None
    };

    let structural_discharge = match structural {
        "closed-dict" => crate::type_class::StructuralDischarge::ClosedDict,
        _ => crate::type_class::StructuralDischarge::None,
    };

    let class_decl = ClassDecl {
        name: name.to_string(),
        params: params
            .iter()
            .map(|p| {
                let kind = existing_param_kinds.get(p).cloned().unwrap_or(Kind::Type);
                (p.clone(), kind)
            })
            .collect(),
        superclasses: superclasses
            .iter()
            .map(|(class_name, param)| (class_name.clone(), vec![param.clone()]))
            .collect(),
        determines: fd_indices,
        resolver: resolver_name,
        resolver_injective,
        structural_discharge,
        method_signatures: vec![],
    };

    state.env.write().unwrap().insert_class(class_decl.clone());
    for (param_name, kind) in &class_decl.params {
        if *kind == Kind::Operator {
            state.kind_env.insert(param_name.clone(), Kind::Operator);
        }
    }

    Ok(Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    }))
}

/// Type alias for match arm type data (Surface version): (param_types, span, entries).
type SurfaceMatchArmData<'a> = (Vec<Type>, Span, &'a Vec<Spanned<crate::ast::SurfaceEntry>>);

/// Type-check an [instance ...] declaration from SurfaceDeclaration::InstanceDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
pub(crate) async fn infer_instance_decl_from_surface(
    class_name: &str,
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<crate::ast::SurfaceEntry>>)],
    span: Span,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(Type::Dict(Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        }));
    }

    let (param_count, has_fds, fd_list, _param_names) = {
        let class_decl = state
            .env
            .read()
            .unwrap()
            .get_class(class_name)
            .ok_or_else(|| {
                vec![TypeError::new(
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
        let pattern_types = extract_pattern_types(pattern_node, env, state)?;

        if pattern_types.len() != param_count {
            return Err(vec![TypeError::new(
                format!(
                    "instance pattern has {} type parameters but class '{}' expects {}",
                    pattern_types.len(),
                    class_name,
                    param_count
                ),
                pattern_node.span.clone(),
            )]);
        }

        if pattern_types.iter().any(|ty| matches!(ty, Type::Unknown)) {
            return Err(vec![TypeError::new(
                format!(
                    "instance pattern for class '{}' contains Unknown types — all pattern positions must have concrete type annotations (use a@Type syntax)",
                    class_name
                ),
                pattern_node.span.clone(),
            )
            .with_code("T017")]);
        }

        arm_data.push((pattern_types, pattern_node.span.clone(), methods));
    }

    for i in 0..arm_data.len() {
        for j in (i + 1)..arm_data.len() {
            let (types_i, span_i, _) = &arm_data[i];
            let (types_j, span_j, _) = &arm_data[j];

            if patterns_overlap(types_i, types_j, state)? {
                let error = TypeError::new(
                    format!(
                        "overlapping instance patterns for class '{}': arm at line {} and arm at line {} could both match the same types",
                        class_name,
                        span_i.start.line,
                        span_j.start.line
                    ),
                    span_j.clone(),
                )
                .with_code("T014");
                return Err(vec![error]);
            }
        }
    }

    if has_fds {
        for (determining_indices, determined_indices) in &fd_list {
            for (pattern_types, _arm_span, _) in &arm_data {
                for &det_idx in determined_indices {
                    if !determining_indices.contains(&det_idx) {
                        // T016: only check concrete determined types, not TypeVars.
                        // For polymorphic (TypeVar) determined types, coverage cannot be
                        // verified statically without full annotation resolution. TypeVar
                        // determined types are legitimate for catch-all instances.
                        // T016 is meaningful only when the determined type is a concrete
                        // type that can be compared against the determining positions.
                        if let Type::TypeVar(det_name, _) = &pattern_types[det_idx] {
                            let same_var_in_determining =
                                determining_indices.iter().any(|&det_pos| {
                                    matches!(&pattern_types[det_pos], Type::TypeVar(n, _) if n == det_name)
                                });
                            // Only fire T016 when the TypeVar names match but come from different
                            // sources — i.e., the same named TypeVar appears in determining positions
                            // but at the WRONG index, indicating a concrete coverage violation.
                            // When all positions are fresh (non-matching) TypeVars, this is a
                            // polymorphic catch-all — skip T016.
                            let _ = (det_name, same_var_in_determining); // suppress unused warnings
                                                                         // Skip T016 for TypeVar determined types (polymorphic catch-all)
                        }
                    }
                }
            }

            for i in 0..arm_data.len() {
                for j in (i + 1)..arm_data.len() {
                    let (types_i, span_i, _) = &arm_data[i];
                    let (types_j, span_j, _) = &arm_data[j];

                    let determining_i: Vec<Type> = determining_indices
                        .iter()
                        .map(|&idx| types_i[idx].clone())
                        .collect();
                    let determining_j: Vec<Type> = determining_indices
                        .iter()
                        .map(|&idx| types_j[idx].clone())
                        .collect();

                    if types_can_unify(&determining_i, &determining_j, state)? {
                        let determined_i: Vec<Type> = determined_indices
                            .iter()
                            .map(|&idx| types_i[idx].clone())
                            .collect();
                        let determined_j: Vec<Type> = determined_indices
                            .iter()
                            .map(|&idx| types_j[idx].clone())
                            .collect();

                        if !types_can_unify(&determined_i, &determined_j, state)? {
                            let error = TypeError::new(
                                format!(
                                    "consistency violation for class '{}': arm at line {} and arm at line {} have overlapping determining positions but incompatible determined types",
                                    class_name,
                                    span_i.start.line,
                                    span_j.start.line
                                ),
                                span_j.clone(),
                            )
                            .with_code("T015");
                            return Err(vec![error]);
                        }
                    }
                }
            }
        }
    }

    for (pattern_types, _arm_span, methods) in &arm_data {
        let inst_type = if pattern_types.len() == 1 {
            pattern_types[0].clone()
        } else {
            Type::Dict(Row {
                fields: pattern_types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| (i.to_string(), ty.clone()))
                    .collect(),
                tail: crate::type_def::RowTail::Empty,
            })
        };

        // Extract type tags for instance binding name generation.
        // Only concrete uppercase type names contribute to the binding name — TypeVars and
        // Unknown are filtered out (same semantics as lower.rs:extract_dispatch_tags).
        let type_args: Vec<String> = pattern_types
            .iter()
            .filter_map(|ty| type_to_dispatch_tag(ty))
            .collect();

        let mut method_types = HashMap::new();

        for method in *methods {
            let method_name = match &method.node.key {
                Some(key_node) => match &key_node.expr {
                    SurfaceExpression::StringLiteral { content, .. } => content.clone(),
                    SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                    _ => {
                        return Err(vec![TypeError::new(
                            "instance method name must be a string or identifier",
                            key_node.span.clone(),
                        )]);
                    }
                },
                None => {
                    return Err(vec![TypeError::new(
                        "instance method must have a name",
                        method.span.clone(),
                    )]);
                }
            };

            let mut method_errors: Vec<TypeError> = Vec::new();
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
            if !method_errors.is_empty() {
                return Err(method_errors);
            }
            method_types.insert(method_name.clone(), method_impl_type.clone());

            // Insert TypeScheme for the ɪ-prefixed binding name so that VarRef resolution
            // can find the method type. This mirrors what lower.rs does at runtime:
            // lower.rs creates a dict entry with key `ɪɴꜱᴛᴀɴᴄᴇ⧼Class∷method⟨T⟩⧽` and the
            // type checker must insert a matching TypeScheme at that name.
            let type_args_str: Vec<&str> = type_args.iter().map(|s| s.as_str()).collect();
            let binding_name =
                crate::type_def::instance_binding_name(class_name, &method_name, &type_args_str);

            let scheme = generalize(state.level, &method_impl_type, state);

            // Insert into the parent dict env. The env parameter is the dict's environment,
            // so inserting here makes the ɪ-prefixed binding visible to the same scope as
            // the instance declaration itself (letrec scope).
            env.write().unwrap().insert_scheme(binding_name, scheme);
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
        let mangled = format!(
            "ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽",
            instance_decl.class_name, instance_decl.instance_type
        );
        state
            .env
            .write()
            .unwrap()
            .insert_instance(mangled, instance_decl);
    }

    Ok(Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    }))
}

/// Check if a type contains Unknown or Top anywhere in its structure.
///
/// Used for the gradual typing fallback: when Unknown/Top appears anywhere in a type,
/// subsumption uses `is_consistent` instead of `is_subtype` to maintain the gradual guarantee.
pub(crate) fn contains_unknown_or_top(ty: &Type) -> bool {
    match ty {
        Type::Unknown | Type::Any => true,
        // TypeVar is treated as gradual (like Unknown) in the subsumption check.
        // An unresolved TypeVar represents an unknown type that could be anything.
        // Internal TypeVars from annotated params, `instantiate_scheme`, and
        // `fresh_type_var` used in pass-1 positions can appear during body checking
        // before the substitution has resolved them. Without this arm, TypeVars in
        // an actual type would fall through to `_ => false` in is_subtype, causing
        // false subsumption failures against concrete expected types like Number or Str.
        Type::TypeVar(_, _) => true,
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, t)| contains_unknown_or_top(t)) || contains_unknown_or_top(ret)
        }
        Type::App(f, a) => contains_unknown_or_top(f) || contains_unknown_or_top(a),
        Type::Dict(row) => row.fields.values().any(contains_unknown_or_top),
        Type::Union(members) => members.iter().any(contains_unknown_or_top),
        _ => false,
    }
}

/// Check that an expression has a compatible type with the expected type.
///
/// Uses bidirectional type checking: synthesize the expression's type via `run_typecheck` (CEK),
/// then check subsumption via `is_subtype(actual, expected)`.
///
/// Per doc/06-type-inference.md §Bidirectional Typing, this is the [SUB] rule:
/// if `Γ ⊢ e ⇒ σ` and `σ <: τ`, then `Γ ⊢ e ⇐ τ`.
///
/// Special case for lambdas (doc/06 §[CHECK-FN]): when `node` is a `Fn` expression and
/// `expected` is a concrete `Function` type, arity is checked before synthesis.
/// Used at checking positions where the expected type is fully concrete: TypeAssert and
/// default-value validation. Called from `typecheck_annot::resolve_type_assert`.
pub(crate) async fn check_surface_expr(
    node: &Arc<SurfaceNode>,
    expected: &Type,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    // Lambda checking mode (CHECK-FN) — arity check.
    // When the node is a Fn expression and expected is a concrete Function type, check
    // that the lambda's param count matches the expected param count. This is a necessary
    // arity check even when full bidirectional propagation is not available (sync context).
    if let (
        SurfaceExpression::Fn { params, .. },
        Type::Function {
            params: exp_params,
            typed_variadics: exp_tv,
            rest: exp_rest,
            required_count: exp_required,
            ..
        },
    ) = (&node.expr, expected)
    {
        if !expected.has_inference_vars() {
            let actual_count = params.len();
            let expected_count = exp_params.len();
            let min_required = *exp_required;
            let exp_variadic = !exp_tv.is_empty() || exp_rest.is_some();
            let max_allowed = if exp_variadic {
                usize::MAX
            } else {
                expected_count
            };
            if actual_count < min_required || actual_count > max_allowed {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        if exp_variadic {
                            format!("at least {}", min_required)
                        } else {
                            expected_count.to_string()
                        },
                        actual_count
                    ),
                    node.span.clone(),
                )]);
            }
        }
    }

    // Default: synthesize then check via run_typecheck (CEK machine).
    // Full lambda checking mode (CHECK-FN) that propagates expected parameter types into a lambda
    // requires async annotation resolution (resolve_annotation). Lambda inference is handled
    // correctly by the CEK machine's AfterFnBody continuation.
    // Fall through to synthesize+subsume.
    let mut local_errors: Vec<TypeError> = Vec::new();
    let mut local_stack = Vec::new();
    let actual = Box::pin(typecheck_cek::run_typecheck(
        node,
        env,
        state,
        &mut local_errors,
        type_map,
        &mut local_stack,
    ))
    .await;
    if !local_errors.is_empty() {
        return Err(local_errors);
    }
    // Apply state.subst to both types before comparison — access-chain constraints
    // may have bound TypeVars in state.subst. Without substitution, the comparison
    // uses stale TypeVars.
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let (actual, expected_resolved) = if state.subst.type_map.borrow().is_empty() {
        (actual, expected.clone())
    } else {
        (state.subst.apply(&actual), state.subst.apply(expected))
    };

    // Unified CALL-MONO/CALL-POLY path: eliminates verdict divergence between monomorphic
    // and polymorphic function calls. When expected type has TypeVars, use unification to
    // bind them (CALL-POLY). When expected type is concrete, use subsumption (CALL-MONO).
    // This ensures identical literal pairs get consistent verdicts regardless of whether
    // the function type has inference vars.
    if expected_resolved.has_inference_vars() {
        // Expected type contains TypeVars — use consistent subtyping (gradual).
        // Full unification is async (unify calls are async) and cannot be performed from
        // this sync function. is_consistent_subtype handles TypeVar positions as gradual (?),
        // which is the correct behavior for the check context (unknown ≡ accept anything).
        if !Type::is_consistent_subtype(&actual, &expected_resolved) {
            return Err(vec![TypeError::type_mismatch(
                &expected_resolved,
                &actual,
                node.span.clone(),
            )]);
        }
        Ok(())
    } else {
        // Expected type is concrete — use subsumption with gradual typing fallback.
        // This is the CALL-MONO path: the function type is fully known, so we check
        // that the argument type is a subtype of the parameter type.
        //
        // Use is_subtype for standard HM subsumption. When is_subtype fails and either type
        // contains Unknown (gradual ?) anywhere in its structure, fall back to is_consistent.
        // The gradual guarantee requires that making types less precise (adding ?) never
        // causes new type errors (Siek & Taha 2006). We only use the consistency fallback
        // when Unknown is present, because is_consistent is symmetric (Number ~ Int) while
        // is_subtype is directional (Int <: Number but NOT Number <: Int).

        // Apply substitution before consistency check
        let (actual_resolved, expected_final) = if state.subst.type_map.borrow().is_empty() {
            (actual.clone(), expected_resolved.clone())
        } else {
            (
                state.subst.apply(&actual),
                state.subst.apply(&expected_resolved),
            )
        };

        let tycon_env = state.tycon_env_ref();
        let passes = Type::is_subtype(&actual_resolved, &expected_final, Some(tycon_env))
            || ((contains_unknown_or_top(&actual_resolved)
                || contains_unknown_or_top(&expected_final))
                && Type::is_consistent(&actual_resolved, &expected_final));
        if !passes {
            Err(vec![TypeError::type_mismatch(
                &expected_final,
                &actual_resolved,
                node.span.clone(),
            )])
        } else {
            Ok(())
        }
    }
}

/// Heuristically resolve which monad library a `Type` value corresponds to.
///
/// Used in `[do ...]` inference to determine which monad dict to inject when
/// the call's head type is known.  Returns the lowercase monad dict name
/// (e.g. `"result"`, `"option"`) or `None` if no match can be found.
///
/// Rules:
/// - `Type::Operator(name)` → if name is `"Result"`, return `"result"` (extensible to other monads)
/// - `Type::Record` with an `"ok"` or `"err"` field → `"result"`
/// - `Type::Union` → first member that matches one of the above rules
/// - Anything else → `None`
#[cfg(test)]
pub(crate) fn resolve_monad_from_type(ty: &Type, _state: &InferState) -> Option<String> {
    match ty {
        Type::Operator(name) => {
            let lower = name.to_lowercase();
            if lower == "result" {
                Some("result".to_string())
            } else {
                None
            }
        }
        Type::Dict(Row { fields, .. }) => {
            if fields.contains_key("ok") || fields.contains_key("err") {
                Some("result".to_string())
            } else {
                None
            }
        }
        Type::Union(members) => members
            .iter()
            .find_map(|m| resolve_monad_from_type(m, _state)),
        _ => None,
    }
}

/// Heuristically resolve which monad library an implied-call `SurfaceNode` targets.
///
/// Inspects the function position of an implied call expression to determine which
/// monad dict the constructor belongs to.  Returns the lowercase monad dict name
/// (e.g. `"result"`) or `None` if the node is not an implied constructor call or the
/// constructor cannot be resolved.
///
/// Rules:
/// 1. Node must be a `SurfaceExpression::Call { implied: true, ... }`.
/// 2. If the function position is a dot-access chain (e.g. `Result.Ok`), extract the
///    leading type name and return `Some(name.to_lowercase())`.
/// 3. If the function position is a plain `VarRef` (e.g. `Ok`), look up the qualified
///    tag via `type_env.resolve_constructor_tag(name)`, extract the TyCon prefix, and
///    return `Some(tycon.to_lowercase())`.
/// 4. Otherwise return `None`.
#[cfg(test)]
pub(crate) fn resolve_monad_from_surface(
    node: &std::sync::Arc<SurfaceNode>,
    type_env: &TypeEnv,
) -> Option<String> {
    let SurfaceExpression::Call {
        func,
        implied: true,
        ..
    } = &node.expr
    else {
        return None;
    };

    // Rule 2: dot-access chain like `Result.Ok`
    if let Some(tag) = crate::ast::flatten_dot_access_to_tag(&func.expr) {
        if let Some(dot_pos) = tag.find('.') {
            let tycon = &tag[..dot_pos];
            return Some(tycon.to_lowercase());
        }
    }

    // Rule 3: plain VarRef like `Ok` — resolve via type_env
    if let SurfaceExpression::VarRef { name, .. } = &func.expr {
        if let Some(qualified) = type_env.resolve_constructor_tag(name) {
            if let Some(dot_pos) = qualified.find('.') {
                let tycon = &qualified[..dot_pos];
                return Some(tycon.to_lowercase());
            }
        }
    }

    None
}

#[cfg(test)]
#[path = "typecheck_tests.rs"]
mod tests;
