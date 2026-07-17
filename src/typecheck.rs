//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    node_id, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceExpression,
    SurfaceItem, SurfaceNode, SurfaceProgram, TypeAnnotationTable,
};
use crate::env::Env;
use crate::types::{generalize, InferState, Row, Type, TypeAlias, TypeError, TypeScheme};
#[cfg(test)]
use crate::ast::Pattern;
#[cfg(test)]
use crate::types::TypeEnv;

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
pub(crate) mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;
// Special-case type refinement dispatchers for polymorphic builtins
// Path-sensitive narrowing, pattern binding extraction, overlap checking
#[path = "typecheck_narrow.rs"]
pub(crate) mod typecheck_narrow;
// T010/T011/T012 type quality diagnostics
#[path = "typecheck_diag.rs"]
pub(crate) mod typecheck_diag;
// Case arm and function literal type inference
#[path = "typecheck_match.rs"]
pub(crate) mod typecheck_match;
// Call and dot-access type checking
#[path = "typecheck_call.rs"]
pub(crate) mod typecheck_call;
// CEK machine for iterative type checking
#[path = "typecheck_cek.rs"]
pub(crate) mod typecheck_cek;

#[allow(unused_imports)]
use typecheck_annot::*;
#[allow(unused_imports)]
use typecheck_call::*;
#[allow(unused_imports)]
use typecheck_diag::*;
#[allow(unused_imports)]
use typecheck_dict::*;
#[allow(unused_imports)]
use typecheck_match::*;
#[allow(unused_imports)]
use typecheck_narrow::*;

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
    type_stage_env: Option<Arc<RwLock<Env>>>,
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
    state.type_stage_env = type_stage_env;
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

    let named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        // Type-stage documents are type-checked in document order.

        let (new_env, doc_output_type, mut doc_errors) = typecheck_surface_document(
            doc,
            &env,
            &mut state,
            &mut table,
            &mut None, // annotation_table path — no span TypeMap needed
            &pipeline_type,
            &named_types,
        )
        .await;
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking propagation.
        errors.append(&mut doc_errors);
        // Document names removed — named_types no longer populated from doc.name
        // Update pipeline type for next document
        pipeline_type = doc_output_type;
    }

    (errors, table, state.tycon_env)
}

/// Type-check a `SurfaceProgram` with a given initial type environment.
///
/// This is the native-Surface implementation — it delegates to
/// [`typecheck_surface_program_with_env`] which walks `program.documents` directly via
/// [`typecheck_surface_document`] without any conversion through the old `File` AST.
/// The span-keyed [`TypeMap`] in the return tuple is always empty; callers that need
/// per-expression type information should use the [`TypeAnnotationTable`] returned by
/// [`typecheck_surface_program_with_env`] instead.
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics)`
///
/// The returned [`TypeMap`] is span-keyed and built from the `TypeAnnotationTable` produced
/// during inference (populated per-node by `typecheck_surface_document`). Only top-level
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
            None,
            None,
            std::collections::HashMap::new(),
            None,
        )
        .await;
    // type_map is now populated during inference (enable_scheme_map=true path).
    (errors, type_map, doc_map, scheme_map, diagnostics)
}

/// Type-check a `SurfaceProgram` with full control over scheme-map generation,
/// returning all intermediate state including a [`TypeAnnotationTable`] for the evaluator's
/// lowering pass.
///
/// This is the native-Surface implementation — it walks `program.documents` directly
/// via [`typecheck_surface_document`] without any conversion through the old `File` AST.
/// The [`TypeAnnotationTable`] is populated directly during inference (keyed by `NodeId`
/// of the original `Arc<SurfaceNode>`) — no span-based correlation is needed.
///
/// # Parameters
///
/// - `program`: The surface AST to type-check.
/// - `parent_env`: Initial type environment. All classes and instances visible to this
///   program must already be in `parent_env`'s chain (populated by prior type-checking runs
///   via `TypeContext`).
/// - `enable_scheme_map`: When `true`, populates the [`SchemeMap`] for LSP hover.
/// - `resolver_seed_env`: Optional env used to seed the resolver (name resolution pass).
///   When `Some`, the resolver is seeded from this env so that instance binding names
///   (ɪ-prefixed, e.g. `ɪɴꜱᴛᴀɴᴄᴇ⧼Castable∷cast⟨String,Int⟩⧽`) are visible in scope
///   and `method_to_instance` can resolve class method VarRefs. This should be the full
///   runtime eval env (from `builtin-typecheck-doc env: <env>`), which contains instance
///   bindings that the type-only `parent_env` does not. When `None`, falls back to
///   `parent_env` for resolver seeding (the prior behavior).
/// - `type_stage_env`: Optional type-stage evaluation environment. When `Some`, stored on
///   `state.type_stage_env` so that `eval_type_stage_expr` and `call_type_stage_fn` can
///   evaluate user-defined type-stage functions (e.g. TypeNode constructors, `or`, `all`).
///   Populated from `TypeContextData.type_stage_env` by `builtin-typecheck-doc`. `None` at all
///   bootstrap call sites (prelude, stdlib includes, LSP path) where the type-stage env
///   has not yet been built.
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
    enable_scheme_map: bool,
    _resolver_seed_env: Option<Arc<RwLock<Env>>>,
    type_stage_env: Option<Arc<RwLock<Env>>>,
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
    // Wire type_stage_env from the TypeContext so eval_type_stage_expr can evaluate
    // user-defined type-stage functions (TypeNode constructors, `or`, `all`, etc.).
    // When None (bootstrap/LSP paths), type_stage_env stays None — primitive types are
    // resolved directly in resolve_type_name without a type-stage env call.
    state.type_stage_env = type_stage_env;
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
    // Seed the resolver so that instance binding names (ɪ-prefixed) are visible
    // in scope and method_to_instance can resolve class method VarRefs (cast, +, -, etc.)
    // to their letrec slots. T-1576: When an eval_ctx is provided, use its scope_arena
    // to seed the resolver from FlatEnv. Otherwise use bootstrap mode (empty initial_frames).
    state.resolution_table = if let Some(ref ctx) = state.eval_ctx {
        let root_frame: indexmap::IndexMap<String, u32> = {
            let arena = ctx.scope_arena.borrow();
            arena.scopes[0]
                .iter_named()
                .filter(|(n, _)| !n.is_empty() && !n.starts_with('#'))
                .map(|(n, slot)| (n.to_string(), slot))
                .collect()
        };
        let (table, _frames) = crate::resolve::resolve_surface_program(program, &[root_frame]);
        Some(Arc::new(table))
    } else {
        let (table, _frames) = crate::resolve::resolve_surface_program(program, &[]);
        Some(Arc::new(table))
    };

    if enable_scheme_map {
        state.scheme_map = Some(SchemeMap::new());
    }

    let mut annotation_table = TypeAnnotationTable::new();
    // type_map_inner accumulates span→type for all sub-expressions (for LSP hover).
    // Populated when enable_scheme_map is true (i.e., LSP path), empty otherwise.
    let mut type_map_inner = TypeMap::new();
    let named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        // Type-stage documents are type-checked in document order alongside runtime documents.
        // Their type declarations (Boolean, Seq, etc.) register in state.tycon_env so runtime
        // documents can resolve @Boolean, @Seq annotations without any separate pass.

        let mut type_map_ref: Option<&mut TypeMap> = if enable_scheme_map {
            Some(&mut type_map_inner)
        } else {
            None
        };

        let (new_env, doc_output_type, mut doc_errors) = typecheck_surface_document(
            doc,
            &env,
            &mut state,
            &mut annotation_table,
            &mut type_map_ref,
            &pipeline_type,
            &named_types,
        )
        .await;
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking env propagation.
        errors.append(&mut doc_errors);
        // Document names removed — named_types no longer populated from doc.name
        // Update pipeline type for next document.
        pipeline_type = doc_output_type;
    }

    // Extract scheme_map from state (populated during VarRef inference).
    let scheme_map = state.scheme_map.take().unwrap_or_default();

    // Collect diagnostics from state (e.g., T013 ambiguous constraints).
    diagnostics.append(&mut state.diagnostics);

    // Scan for type quality issues (Unknown types, over-broad annotations).
    // Uses type_map_inner — only populated when enable_scheme_map is true (LSP + typecheck_surface_program path).
    // When enable_scheme_map is false (annotation-table-only path), type_map_inner is empty so
    // T010/T011/T012 diagnostics from type_map are suppressed.
    scan_type_quality(&type_map_inner, program, &mut diagnostics);

    // Always emit T011 for explicit @Unknown annotations even when enable_scheme_map=false.
    // These are unconditional: the programmer wrote @Unknown explicitly, so the warning
    // fires regardless of inferred type, and does not require a populated type_map.
    // When enable_scheme_map=true, scan_type_quality already handles T011 via the type_map;
    // we skip this scan to avoid duplicates.
    if !enable_scheme_map {
        scan_explicit_unknown_t011(program, &mut diagnostics);
    }

    // Extract doc strings from the Surface AST (equivalent to extract_doc_strings on File AST).
    // Only needed when enable_scheme_map is true (i.e., LSP path — doc_map is for hover).
    let doc_map = if enable_scheme_map {
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

/// Propagate newly discovered class and instance declarations from `state.env` into `result_env`.
///
/// After type-checking a document, `state.env` contains all class/instance declarations
/// visible during that run. This function copies declarations from `state.env` that are NOT
/// already present in `parent_env` into `result_env` so they propagate to subsequent documents.
///
/// This is the mechanism that makes classes and instances declared in one document (e.g.,
/// earlier documents) visible when type-checking the next document.
fn propagate_classes_instances_to_env(
    state: &InferState,
    parent_env: &Arc<RwLock<Env>>,
    result_env: &mut Env,
) {
    // Only propagate classes/instances declared in state.env's OWN frame (not inherited
    // from the parent chain). The parent chain's classes are already in parent_env.
    // Using all_classes() would walk state.env's parent chain (which IS parent_env),
    // causing a deadlock when combined with the parent_guard lock.
    let env_guard = state.env.read().unwrap();
    // Collect own classes/instances first (before releasing the lock).
    let own_classes: Vec<_> = env_guard.classes.values().cloned().collect();
    let own_instances: Vec<_> = env_guard
        .instances
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    drop(env_guard); // Release before acquiring parent lock to avoid deadlock.

    let parent_guard = parent_env.read().unwrap();
    // Propagate new class declarations.
    for decl in &own_classes {
        // Only propagate if this class is not already visible in the parent chain.
        if parent_guard.get_class(&decl.name).is_none() {
            result_env.insert_class(decl.clone());
        }
    }
    // Propagate new instance declarations.
    for (mangled, decl) in &own_instances {
        if parent_guard.get_instance(mangled).is_none() {
            result_env.insert_instance(mangled.clone(), decl.clone());
        }
    }
}

/// Type-check a single SurfaceDocument.
///
/// Mirrors the structure of `typecheck_document()` but operates on SurfaceItem instead of Expr.
/// Converts SurfaceNode back to Expr for type inference, then captures results in TypeAnnotationTable.
pub(crate) async fn typecheck_surface_document(
    doc: &SurfaceDocument,
    parent_env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    table: &mut TypeAnnotationTable,
    type_map: &mut Option<&mut TypeMap>,
    pipeline_type: &Type,
    named_types: &HashMap<String, Type>,
) -> (Arc<RwLock<Env>>, Type, Vec<TypeError>) {
    let mut errors = Vec::new();
    let mut advisory_errors: Vec<TypeError> = Vec::new();

    // Create environment with % and %name bindings
    let doc_env_inner = {
        let mut e = Env::with_parent(Arc::clone(parent_env));
        // Bind % (pipeline variable) with the incoming type
        e.insert("%".to_string(), pipeline_type.clone());
        // Bind all named sections as %name
        for (name, ty) in named_types {
            e.insert(format!("%{}", name), ty.clone());
        }
        e
    };
    let mut env: Arc<RwLock<Env>> = Arc::new(RwLock::new(doc_env_inner));

    // Note: expects: and caps: annotation validation requires async resolve_annotation.
    // Since typecheck_surface_document is sync, these are skipped here.
    // The async typecheck path handles them separately.
    // Note: --- uses: headers (now in doc.header) are processed by loader's uses-scope (tinct code) which
    // type-checks each builtin_*.llt file and accumulates results into the TypeContext.
    // The typechecker receives all module type schemes via tc.inference_env (the parent_env
    // passed to typecheck_surface_program_with_env). No Rust-side injection needed here.

    let mut result_type = Type::Dict(Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    // Process declarations first (TypeAlias, ClassDecl, InstanceDecl)
    // These register into env/state before expressions are type-checked.
    for item in &doc.items {
        if let SurfaceItem::Decl(decl_spanned) = item {
            match &decl_spanned.node {
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Standalone [type ...] declarations at the top level have no name
                    // (the name comes from the dict key in `MyType: [type ...]` form).
                    // Unnamed type alias decls are skipped here; named aliases in Dict
                    // expressions are registered in the pre-pass above.
                }
                SurfaceDeclaration::ClassDecl {
                    name,
                    params,
                    superclasses,
                    methods,
                    determines,
                    resolver,
                    resolver_injective,
                    structural,
                } => {
                    // Infer the class declaration to register it into state.env
                    let superclasses_flat: Vec<(String, String)> = superclasses
                        .iter()
                        .flat_map(|(sc_name, sc_params)| {
                            sc_params
                                .iter()
                                .map(|p| (sc_name.clone(), p.clone()))
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    match infer_class_decl_from_surface(
                        name,
                        params,
                        &superclasses_flat,
                        methods,
                        determines,
                        resolver,
                        *resolver_injective,
                        structural,
                        decl_spanned.span.clone(),
                        &env,
                        state,
                        &mut None,
                    ) {
                        Ok(_) => {
                            // Drain TypeAnnotationTable entries produced during ClassDecl inference
                            // to prevent them from leaking into subsequent items.
                            for (nid, ty) in state.type_annotation_table.drain() {
                                table.insert(nid, ty);
                            }
                        }
                        Err(mut errs) => {
                            errors.append(&mut errs);
                            // Drain TypeAssert entries from failed expression to prevent leaking into next iteration
                            for (nid, ty) in state.type_annotation_table.drain() {
                                table.insert(nid, ty);
                            }
                        }
                    }
                }
                SurfaceDeclaration::InstanceDecl { class_name, arms } => {
                    // Infer the instance declaration to register it
                    match infer_instance_decl_from_surface(
                        class_name,
                        arms,
                        decl_spanned.span.clone(),
                        &env,
                        state,
                        &mut None,
                    )
                    .await
                    {
                        Ok(_) => {
                            // Drain TypeAnnotationTable entries produced during InstanceDecl inference
                            // to prevent them from leaking into subsequent items.
                            for (nid, ty) in state.type_annotation_table.drain() {
                                table.insert(nid, ty);
                            }
                        }
                        Err(mut errs) => {
                            errors.append(&mut errs);
                            // Drain TypeAssert entries from failed expression to prevent leaking into next iteration
                            for (nid, ty) in state.type_annotation_table.drain() {
                                table.insert(nid, ty);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // Extract only expression items (skip declarations)
    let expr_items: Vec<_> = doc
        .items
        .iter()
        .filter_map(|item| match item {
            SurfaceItem::Expr(node) => Some(node),
            SurfaceItem::Decl(_) => None,
        })
        .collect();

    if expr_items.is_empty() {
        // Note: output_type annotation (now in doc.header) validation requires async resolve_annotation — skip.

        let mut result_env_inner = Env::with_parent(Arc::clone(parent_env));
        result_env_inner.insert("%".to_string(), result_type.clone());
        // Propagate new class/instance declarations into result_env.
        propagate_classes_instances_to_env(state, parent_env, &mut result_env_inner);

        // Always return Ok with the partial env so callers always propagate env.
        advisory_errors.append(&mut errors);
        return (
            Arc::new(RwLock::new(result_env_inner)),
            result_type,
            advisory_errors,
        );
    }

    // Tracks schemes from the last dict expression so they can be threaded into result_env.
    // Mirrors typecheck_document's `last_dict_schemes` / `last_record_type` logic.
    // IndexMap preserves insertion order so insert_scheme calls into result_env match the
    // resolver's slot assignments (surface_dict_static_keys source order).
    let mut last_dict_schemes: Option<indexmap::IndexMap<String, TypeScheme>> = None;
    // last_record_type: captures (type, enclosing_level) for the last non-dict Record result,
    // so its fields can be generalized and threaded into result_env (cross-document scoping).
    let mut last_record_type: Option<(Type, u32)> = None;
    let mut last_node: Option<Arc<SurfaceNode>> = None;

    for (i, surface_node) in expr_items.iter().enumerate() {
        let is_last = i == expr_items.len() - 1;

        if let SurfaceExpression::Dict(entries) = &surface_node.expr {
            // Dict expression: use run_typecheck_dict to get per-entry schemes for cross-document
            // scoping. run_typecheck_dict always returns best-effort schemes; errors are in the
            // third element (T-1644).
            let (dict_ty, schemes, mut dict_errs) = typecheck_cek::run_typecheck_dict(
                entries,
                &env,
                state,
                type_map,
                surface_node.span.clone(),
            )
            .await;
            errors.append(&mut dict_errs);
            table.insert(node_id(surface_node), dict_ty.clone());
            // Merge nested TypeAssert entries from run_typecheck_dict into the document-level table
            for (nid, ty) in state.type_annotation_table.drain() {
                table.insert(nid, ty);
            }
            if is_last {
                result_type = dict_ty;
                last_dict_schemes = Some(schemes);
                last_node = Some(Arc::clone(surface_node));
            } else {
                let mut new_env_inner = Env::with_parent(Arc::clone(&env));
                for (name, scheme) in &schemes {
                    new_env_inner.insert_scheme(name.clone(), scheme.clone());
                }
                register_type_aliases_env(surface_node, &mut new_env_inner, state, &mut errors);
                env = Arc::new(RwLock::new(new_env_inner));
            }
        } else {
            // Non-dict expression: infer at incremented level so type variables can be
            // properly generalized when threading Record fields as schemes into the env.
            // Mirrors typecheck_document lines 1041-1112.
            let enclosing_level = state.level;
            state.level += 1;

            // Track error count before inference to detect whether run_typecheck added errors.
            let errors_before = errors.len();
            let mut stack = Vec::new();
            let ty = typecheck_cek::run_typecheck(
                surface_node,
                &env,
                state,
                &mut errors,
                type_map,
                &mut stack,
            )
            .await;
            state.level = enclosing_level;
            let had_errors = errors.len() > errors_before;

            if had_errors {
                // Drain TypeAssert entries from failed expression to prevent leaking into next iteration
                for (nid, ty) in state.type_annotation_table.drain() {
                    table.insert(nid, ty);
                }
            } else {
                table.insert(node_id(surface_node), ty.clone());
                // Merge nested TypeAssert entries from run_typecheck into the document-level table
                for (nid, ty) in state.type_annotation_table.drain() {
                    table.insert(nid, ty);
                }
                if is_last {
                    result_type = ty.clone();
                    last_node = Some(Arc::clone(surface_node));
                    // Track last non-dict Record for cross-document field threading.
                    if matches!(&ty, Type::Dict(_)) {
                        last_record_type = Some((ty, enclosing_level));
                    }
                } else {
                    // Intermediate expressions must be record types.
                    // Mirrors typecheck_document line 1097.
                    match &ty {
                        Type::Dict(Row { fields, .. }) => {
                            let mut new_env_inner = Env::with_parent(Arc::clone(&env));
                            for (name, field_ty) in fields {
                                let scheme = generalize(enclosing_level, field_ty, state);
                                new_env_inner.insert_scheme(name.clone(), scheme);
                            }
                            register_type_aliases_env(
                                surface_node,
                                &mut new_env_inner,
                                state,
                                &mut errors,
                            );
                            env = Arc::new(RwLock::new(new_env_inner));
                        }
                        Type::Unknown => {} // Gradual: dict type inference failed, skip type alias registration
                        _ => errors.push(TypeError::not_a_record(&ty, surface_node.span.clone())),
                    }
                }
            }
        }
    }

    // Note: output_type annotation (now in doc.header) validation requires async resolve_annotation — skip.

    // Build result_env: thread last-dict schemes or last-Record fields into cross-document scope.
    // Mirrors typecheck_document lines 1116-1148.
    //
    // IMPORTANT: result_env uses parent_env as its parent, NOT env.
    // This ensures doc-local bindings (%, %name, caps, and module-from-uses) do NOT
    // propagate to subsequent documents. Only explicitly exported bindings (last-dict
    // schemes, last-Record fields, %) are propagated via result_env.bindings.
    let mut result_env_inner = Env::with_parent(Arc::clone(parent_env));
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env_inner.insert_scheme(name, scheme);
        }
    }
    // If the last expression was a non-dict Record, generalize and thread its fields.
    // Mirrors typecheck_document lines 1137-1142.
    if let Some((Type::Dict(Row { fields, .. }), enclosing_level)) = last_record_type {
        for (name, field_ty) in fields {
            let scheme = generalize(enclosing_level, &field_ty, state);
            result_env_inner.insert_scheme(name, scheme);
        }
    }
    if let Some(ref node) = last_node {
        register_type_aliases_env(node, &mut result_env_inner, state, &mut errors);
    }
    result_env_inner.insert("%".to_string(), result_type.clone());
    // Propagate new class/instance declarations into result_env.
    propagate_classes_instances_to_env(state, parent_env, &mut result_env_inner);

    // Always return the partial env — even if there are type errors.
    // This mirrors the pre-surface-migration behavior: the bridge path (typecheck_document)
    // always returned an env and propagated errors separately. Returning Err here caused
    // `typecheck_surface_program_with_env` to skip updating the accumulated env, which meant
    // stdlib bindings (map, filter, keys, …) were never inserted into final_env.
    // Non-advisory errors are merged into advisory_errors so callers still collect them via
    // the third tuple element.
    advisory_errors.append(&mut errors);
    (
        Arc::new(RwLock::new(result_env_inner)),
        result_type,
        advisory_errors,
    )
}

/// Type-check a single [`SurfaceDocument`] using the native Surface path.
///
/// This is a thin entry point that delegates to [`typecheck_surface_document`].
/// It wraps the caller-supplied `env: &TypeEnv` into a fresh `Rc<TypeEnv>` child
/// (so the caller's env is unchanged) and supplies default pipeline bookkeeping
/// (empty named-section map, empty `{}`-record pipeline type).
///
/// Results are written into `type_map` (NodeId → Type). Errors are returned as
/// `Err(Vec<TypeError>)`; advisory errors (expects:/output_type) are silently
/// discarded here — this entry point is intended for callers that only need
/// Extract documentation strings from parameter and function annotations.
///
/// Walks the AST looking for `doc:` properties in `@[...]` annotations.
/// Populates the doc_map with entries like `param_name -> "doc string"`.
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
fn register_type_aliases_env(
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
/// Called from typecheck_surface_document and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
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
/// Called from typecheck_surface_document and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
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
            variadic: exp_variadic,
            required_count: exp_required,
            ..
        },
    ) = (&node.expr, expected)
    {
        if !expected.has_inference_vars() {
            let actual_count = params.len();
            let expected_count = exp_params.len();
            let min_required = *exp_required;
            let max_allowed = if *exp_variadic {
                usize::MAX
            } else {
                expected_count
            };
            if actual_count < min_required || actual_count > max_allowed {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        if *exp_variadic {
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
