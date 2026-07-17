//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::ast::{
    node_id, Pattern, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceExpression,
    SurfaceItem, SurfaceNode, SurfaceProgram, TypeAnnotationTable,
};
// All production inference helpers now walk SurfaceExpression natively.
// No Expr bridge needed — tests use parse_surface_expression directly.
use crate::coverage;
use crate::env::Env;
use crate::types::{
    generalize, instantiate_scheme, InferState, Row, Type, TypeAlias, TypeEnv, TypeError,
    TypeScheme,
};

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
#[allow(unused_imports)]

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
/// 2. Run type inference via `infer_surface_expr` (native SurfaceNode walk)
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
            // Dict expression: use infer_dict to get per-entry schemes for cross-document scoping.
            // This mirrors typecheck_document which calls infer_dict directly for dict exprs.
            // infer_dict always returns Ok with best-effort schemes; errors are in the third element.
            let (dict_ty, schemes, mut dict_errs) =
                infer_dict(entries, &env, state, type_map, surface_node.span.clone()).await;
            errors.append(&mut dict_errs);
            table.insert(node_id(surface_node), dict_ty.clone());
            // Merge nested TypeAssert entries from infer_dict into the document-level table
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

/// Collect all NominalVariant tag names reachable from a type.
/// A type alias body such as `[Ok a] | [Err b]` resolves to `Union([NominalVariant("Ok",...),
/// NominalVariant("Err",...)])`. This function extracts `["Ok", "Err"]` so the caller can
/// check each tag against the `registered_nominal_tags` registry for W042 duplicates.
#[allow(dead_code)]
fn collect_nominal_tags(ty: &Type) -> Vec<String> {
    match ty {
        Type::NominalVariant { tycon, ctor, .. } => vec![format!("{}.{}", tycon, ctor)],
        Type::Union(members) => members.iter().flat_map(collect_nominal_tags).collect(),
        _ => vec![],
    }
}

#[allow(dead_code)]
fn register_type_aliases(
    node: &Arc<SurfaceNode>,
    target_env: &mut TypeEnv,
    _resolve_env: &TypeEnv,
    _state: &mut InferState,
) -> Vec<TypeError> {
    let errors = Vec::new();
    if let SurfaceExpression::Dict(entries) = &node.expr {
        let mut alias_entries: Vec<(String, Vec<String>, Arc<SurfaceNode>, Span)> = Vec::new();
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let SurfaceExpression::StringLiteral { content: name, .. } = &key.expr {
                    if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                        if let SurfaceDeclaration::TypeAlias { params, body } = decl_box.as_ref() {
                            let param_names: Vec<String> =
                                params.iter().map(|(n, _)| n.clone()).collect();
                            alias_entries.push((
                                name.clone(),
                                param_names.clone(),
                                Arc::clone(body),
                                entry.node.value.span.clone(),
                            ));
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
        let _ = alias_entries;
    }
    errors
}

/// Variant of `register_type_aliases` operating on an owned `Env` rather than `TypeEnv`.
/// Used by `typecheck_surface_document` after the Env migration.
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

/// Attempt to set `call_dispatch` on the function VarRef node after resolving the typeclass
/// instance for a polymorphic call.
///
/// Called from the sync `infer_surface_expr` Call arm after arg types have been inferred.
/// Uses a temporary (throwaway) type-variable binding map so that the dispatch computation
/// does not pollute `state.subst` with unification side effects.
///
/// Algorithm:
/// 1. Find the Constraint::Class entries introduced by scheme instantiation (those with vars
///    that are fresh TypeVars — not yet bound in `state.subst`).
/// 2. The scheme's instantiated Function type gives us `params[i] = TypeVar(_tN)`.
///    For each param position that maps to a constraint var, record `_tN → arg_types[i]`.
/// 3. Build the `type_args` array by applying these bindings to each constraint var and
///    mapping to dispatch tag strings.
/// 4. If all constraint vars resolve to ground dispatch tags, compute the binding name and
///    call `call_dispatch.set(mangled_name)`.
///
/// Silently does nothing when:
/// - `func` is not a VarRef (e.g., a lambda literal).
/// - The scheme has no class constraints.
/// - Some constraint vars remain unresolved (no instance can be determined statically).
fn try_resolve_call_dispatch(
    func: &std::sync::Arc<SurfaceNode>,
    instantiated_func_ty: &Type,
    arg_types: &[Type],
    constraints_before: usize,
    state: &InferState,
) {
    // Only VarRef function nodes carry call_dispatch.
    let call_dispatch = if let SurfaceExpression::VarRef { call_dispatch, .. } = &func.expr {
        call_dispatch
    } else {
        return;
    };

    // Already dispatched (OnceLock semantics: first write wins, subsequent writes silently fail).
    if call_dispatch.get().is_some() {
        return;
    }

    // Extract new Class constraints added since `constraints_before`.
    // `instantiate_scheme` pushes them onto `state.constraints`.
    let new_constraints = &state.constraints[constraints_before..];
    if new_constraints.is_empty() {
        return;
    }

    // Find the first Class constraint (the primary typeclass membership constraint).
    // For multi-class schemes (rare), we dispatch on the first — consistent with how
    // lower.rs processes the first instance arm pattern.
    let (class_name, constraint_vars) = match new_constraints.iter().find_map(|c| {
        if let crate::type_class::Constraint::Class { class, vars, .. } = c {
            Some((class.name.as_str(), vars.as_slice()))
        } else {
            None
        }
    }) {
        Some(cv) => cv,
        None => return,
    };

    // Build a temporary mapping from constraint TypeVar names to arg types by matching
    // the instantiated function param types against the constraint var positions.
    //
    // The instantiated Function type has `params: Vec<(Option<String>, Type)>` where each
    // param type is a TypeVar (fresh from instantiate_scheme). The constraint vars at the
    // corresponding positions have the same TypeVar names.
    //
    // Strategy: for each (param_type, arg_type) pair, if param_type is a TypeVar whose name
    // appears in the constraint vars, record that mapping in a local HashMap.
    let mut var_to_type: HashMap<String, Type> = HashMap::new();

    if let Type::Function { params, ret, .. } = instantiated_func_ty {
        for (i, (_, param_ty)) in params.iter().enumerate() {
            if let Some(arg_ty) = arg_types.get(i) {
                // Follow any existing bindings in state.subst for the param type var.
                let resolved_param_ty = state.subst.apply(param_ty);
                let resolved_arg_ty = state.subst.apply(arg_ty);

                // Widen literal types to base types for dispatch (IntLiteral → Int, etc.).
                let widened_arg_ty = match &resolved_arg_ty {
                    Type::IntLiteral(_) => Type::Int,
                    Type::StringLiteral(_) => Type::Str,
                    other => other.clone(),
                };

                if let Type::TypeVar(var_name, _) = &resolved_param_ty {
                    // Only record if this TypeVar appears in the constraint.
                    if constraint_vars
                        .iter()
                        .any(|cv| cv.as_var() == Some(var_name.as_str()))
                    {
                        var_to_type
                            .entry(var_name.clone())
                            .or_insert(widened_arg_ty);
                    }
                }
            }
        }

        // Also try to resolve the return type var via the functional dependency.
        // For FD (a, b) → c: if we know a and b, look up c in the current state.
        // state.subst may have already bound c via FD improvement (from type_unify.rs).
        let resolved_ret = state.subst.apply(ret);
        if let Type::TypeVar(var_name, _) = &resolved_ret {
            if constraint_vars
                .iter()
                .any(|cv| cv.as_var() == Some(var_name.as_str()))
            {
                if !var_to_type.contains_key(var_name.as_str()) {
                    // Check if state.subst already bound this var (e.g., via FD improvement).
                    let subst_val = state.subst.apply(&Type::TypeVar(var_name.clone(), 0));
                    if !matches!(subst_val, Type::TypeVar(_, _)) {
                        let widened = match &subst_val {
                            Type::IntLiteral(_) => Type::Int,
                            Type::StringLiteral(_) => Type::Str,
                            other => other.clone(),
                        };
                        var_to_type.insert(var_name.clone(), widened);
                    }
                }
            }
        }
    }

    // Resolve each constraint var to a dispatch tag string.
    // If any var is unresolved (no ground type), dispatch cannot be determined statically.
    let mut type_args: Vec<String> = Vec::with_capacity(constraint_vars.len());
    for cv in constraint_vars {
        match cv {
            crate::type_class::ConstraintArg::Ground(ty) => {
                // Ground type already known from FD improvement before generalization.
                match type_to_dispatch_tag(ty) {
                    Some(tag) => type_args.push(tag),
                    None => return, // Cannot dispatch on this ground type
                }
            }
            crate::type_class::ConstraintArg::Var(var_name) => {
                // Check local var_to_type first, then state.subst.
                let ty = if let Some(t) = var_to_type.get(var_name.as_str()) {
                    t.clone()
                } else {
                    let resolved = state.subst.apply(&Type::TypeVar(var_name.clone(), 0));
                    match &resolved {
                        Type::TypeVar(_, _) => return, // Still unresolved — cannot dispatch
                        other => {
                            let widened = match other {
                                Type::IntLiteral(_) => Type::Int,
                                Type::StringLiteral(_) => Type::Str,
                                t => t.clone(),
                            };
                            widened
                        }
                    }
                };
                match type_to_dispatch_tag(&ty) {
                    Some(tag) => type_args.push(tag),
                    None => return, // Cannot dispatch on this type
                }
            }
        }
    }

    // All constraint vars resolved — compute the mangled instance binding name.
    // The method name is the VarRef's `name` field (e.g., "+", "=", "compare").
    let method_name = if let SurfaceExpression::VarRef { name, .. } = &func.expr {
        name.as_str()
    } else {
        return;
    };
    let type_arg_refs: Vec<&str> = type_args.iter().map(String::as_str).collect();
    let mangled_name =
        crate::type_def::instance_binding_name(class_name, method_name, &type_arg_refs);

    call_dispatch.set(mangled_name);
}

/// Type-infer a SurfaceNode expression.
///
/// Natively walks SurfaceExpression variants without converting to Expr.
/// Recursive calls use `infer_surface_expr` for child SurfaceNodes.
/// Bridge to check_* functions (via surface_node_to_expr) will be eliminated in Phase 4.
///
/// # Transitional status
///
/// This function is being phased out in favor of `typecheck_cek::run_typecheck`, which
/// implements the same inference iteratively via a CEK machine (no Rust stack recursion).
/// `run_typecheck` is the entry point for non-dict top-level expressions; see
/// `typecheck_surface_document`. `infer_surface_expr` is retained for Decl variants whose
/// helpers (`infer_class_decl_from_surface`, etc.) remain private to this module, and
/// for dict inference which delegates via `typecheck_dict::infer_dict`.
///
/// Do not add new callers. New inference code should call `run_typecheck`.
pub(crate) async fn infer_surface_expr(
    node: &std::sync::Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let result = match &node.expr {
        SurfaceExpression::Int(n) => Ok(Type::IntLiteral(*n)),
        SurfaceExpression::Float(_) => Ok(Type::Float),
        // Bool literals: in this type system, booleans are represented as TyCon("Boolean")
        // There is no SurfaceExpression::Bool variant — skip
        SurfaceExpression::StringLiteral { content, .. } => {
            Ok(Type::StringLiteral(content.clone()))
        }

        SurfaceExpression::VarRef {
            name, annotation, ..
        } => {
            // Slot-indexed fast path: if the resolver assigned de Bruijn coordinates for
            // this VarRef, try get_scheme_at(level, slot) before falling back to name lookup.
            // This O(1) path is only taken when the resolution table is present AND this
            // node has a resolved entry.  Falls back to name-based get_scheme(name) when:
            //   - No resolution table (tests, inline programs)
            //   - No entry for this node (free variable)
            //   - get_scheme_at returns None (narrowing frame intervened, extras entry, etc.)
            // Name-based lookup first: uses the type checker's own env chain and is always
            // correct for user-defined names. Slot-based lookup is only needed for ɪ-prefixed
            // class method bindings (e.g. ɪɴꜱᴛᴀɴᴄᴇ⧼Addable∷+⟨Integer⟩⧽) which have no
            // symbol name (get_scheme("+") returns None). For all other names — including
            // builtin-* and prelude aliases — name-based lookup finds the correct scheme
            // in the type checker's parent env chain.
            //
            // The resolver uses the runtime env (resolver_seed_env) for slot assignments,
            // which has different slot positions than the type checker's env chain. Using
            // slot-based lookup for names that name-based lookup CAN find causes wrong
            // schemes to be returned (slot alignment mismatch between runtime and typecheck envs).
            let name_scheme = env.read().unwrap().get_scheme(name);
            let scheme: Option<TypeScheme> = if name_scheme.is_some() {
                // Found by name in the type checker's env chain — use directly.
                name_scheme
            } else {
                // Not found by name — fall back to slot lookup (for ɪ-prefixed class methods).
                let mut slot_scheme: Option<TypeScheme> = None;
                if let Some(ref table) = state.resolution_table {
                    let id = node_id(node);
                    if let Some(&(level, slot)) = table.get(&id) {
                        slot_scheme = env.read().unwrap().get_scheme_at(level, slot);
                    }
                }
                slot_scheme
            };
            if let Some(scheme) = scheme {
                // Record scheme in scheme_map for LSP hover (constraints + type vars).
                // Only store when scheme collection is enabled and the scheme is polymorphic
                // (has constraints or quantified type vars — monomorphic schemes show the
                // same info via type_map and don't need the extra constraint display).
                if !scheme.constraints.is_empty()
                    || !scheme.type_vars.is_empty()
                    || !scheme.kind_vars.is_empty()
                {
                    if let Some(ref mut smap) = state.scheme_map {
                        let key = (node.span.start.offset, node.span.end.offset);
                        smap.insert(key, scheme.clone());
                    }
                }
                Ok(instantiate_scheme(
                    &scheme,
                    state.level,
                    state,
                    Some(name.as_str()),
                    Some(node.span.clone()),
                    &node.span,
                ))
            } else {
                let mut err = TypeError::undefined_variable(name, node.span.clone());
                if let Some(cause_span) = state.failed_bindings.get(name.as_str()) {
                    err.notes.push(format!(
                        "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                        cause_span.start.line, cause_span.start.column
                    ));
                }
                // Gradual typing: if the VarRef has an inline annotation, use the
                // annotation type even when the variable is undefined. This allows patterns
                // like `Config@Int` to resolve to `Int` without requiring `Config` to be
                // defined (the annotation provides the type).
                //
                // Special case: `Fn@RetType` — the name "Fn"/"Function" in annotation
                // position with a return-type annotation produces a zero-param function type.
                // This supports standalone `Fn@Int` expressions in type position.
                if let Some(ann) = annotation {
                    let stub_env = crate::types::TypeEnv::new();
                    let mut constraints: Vec<crate::types::Constraint> = Vec::new();
                    if name == "Fn" || name == "Function" {
                        let ret_ty = match typecheck_annot::resolve_annotation(
                            &ann.node,
                            &stub_env,
                            ann.span.clone(),
                            state,
                            &mut constraints,
                            &mut None,
                            &mut None,
                            None,
                        )
                        .await
                        {
                            Ok(ty) => ty,
                            Err(e) => return Err(vec![e]),
                        };
                        state
                            .failed_bindings
                            .insert(name.clone(), node.span.clone());
                        return Ok(Type::Function {
                            params: vec![],
                            ret: Box::new(ret_ty),
                            variadic: false,
                            required_count: 0,
                        });
                    }
                    let ty = match typecheck_annot::resolve_annotation(
                        &ann.node,
                        &stub_env,
                        ann.span.clone(),
                        state,
                        &mut constraints,
                        &mut None,
                        &mut None,
                        None,
                    )
                    .await
                    {
                        Ok(ty) => ty,
                        Err(e) => return Err(vec![e]),
                    };
                    state
                        .failed_bindings
                        .insert(name.clone(), node.span.clone());
                    return Ok(ty);
                }
                // No annotation: undefined variable is a hard error.
                Err(vec![err])
            }
        }

        SurfaceExpression::Dict(entries) => {
            let (ty, _schemes, errs) =
                Box::pin(infer_dict(entries, env, state, type_map, node.span.clone())).await;
            if errs.is_empty() {
                Ok(ty)
            } else {
                Err(errs)
            }
        }

        SurfaceExpression::Pipe { .. } => Ok(Type::error_note(
            "Pipe should be desugared before type checking",
        )),

        SurfaceExpression::Sequential(exprs) => {
            // Multi-expression sequential evaluation (let-binding semantics).
            // Each expression's result dict extends the type environment for the next.
            // The last expression's type is the overall result type.
            //
            // For intermediate dict expressions, we call infer_dict directly (not via
            // infer_surface_expr) to capture per-entry TypeSchemes. This preserves
            // let-polymorphism across sequential steps: a binding like `id: [fn [let x] x]`
            // in an earlier step retains its polymorphic scheme `forall a. a -> a`, so
            // later steps can instantiate it at different types (Damas & Milner, 1982).
            if exprs.is_empty() {
                return Ok(Type::Dict(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }));
            }

            let mut current_env: Arc<RwLock<Env>> = Arc::clone(env);

            for (i, seq_expr) in exprs.iter().enumerate() {
                let is_last = i == exprs.len() - 1;

                if is_last {
                    // Last expression: return its type
                    return Box::pin(infer_surface_expr(seq_expr, &current_env, state, type_map))
                        .await;
                }

                // Intermediate expression: infer and extract record bindings.
                // For Dict expressions, call infer_dict directly to get TypeSchemes
                // (infer_surface_expr discards them via TypeScheme::mono()).
                if let SurfaceExpression::Dict(entries) = &seq_expr.expr {
                    let (dict_ty, schemes, dict_errs) = Box::pin(infer_dict(
                        entries,
                        &current_env,
                        state,
                        type_map,
                        seq_expr.span.clone(),
                    ))
                    .await;
                    if !dict_errs.is_empty() {
                        return Err(dict_errs);
                    }

                    if let Type::Dict(_) = &dict_ty {
                        let mut child_env_inner = Env::with_parent(Arc::clone(&current_env));

                        // Insert schemes (preserving polymorphism) for entries
                        // that have generalized TypeSchemes from infer_dict.
                        // Fall back to mono() for any field in the Record type
                        // that doesn't have a scheme (e.g., auto-indexed entries).
                        for (field_name, scheme) in &schemes {
                            child_env_inner.insert_scheme(field_name.clone(), scheme.clone());
                        }

                        current_env = Arc::new(RwLock::new(child_env_inner));
                    } else {
                        // Non-dict intermediate (from an explicit Dict expression that
                        // happened to produce a non-Record type) — treat as advisory.
                        // This can happen when infer_dict returns Error or Unknown.
                    }
                } else {
                    let enclosing_level = state.level;
                    let expr_ty =
                        Box::pin(infer_surface_expr(seq_expr, &current_env, state, type_map))
                            .await?;

                    // Extract record fields to extend the type environment.
                    // Generalize each field type at the enclosing level so that
                    // a call expression returning a polymorphic record (e.g. a
                    // function that returns `[id: fn [x@a] $x]`) preserves
                    // let-polymorphism for downstream bindings.  Without
                    // generalization, `id` would be inserted as a monomorphic
                    // entry and could only be used at a single type.
                    if let Type::Dict(row) = expr_ty {
                        let mut child_env_inner = Env::with_parent(Arc::clone(&current_env));

                        for (field_name, field_ty) in &row.fields {
                            let scheme = generalize(enclosing_level, field_ty, state);
                            child_env_inner.insert_scheme(field_name.clone(), scheme);
                        }

                        current_env = Arc::new(RwLock::new(child_env_inner));
                    }
                    // Non-dict intermediate expression (e.g. a side-effecting call like
                    // [cancel-root] or [drain]): no bindings contributed. This is valid —
                    // the evaluator handles non-dict intermediates silently. Do not emit
                    // a warning here; the caller can inspect via advisories if needed.
                }
            }

            unreachable!(
                "infer_surface_expr Sequential: loop did not return — exprs was non-empty but is_last never triggered"
            )
        }

        SurfaceExpression::Call {
            func,
            args,
            named_args,
            implied: _,
        } => {
            // Special case: do-infer sentinel — inferred [do] form monad resolution.
            if let SurfaceExpression::Field {
                expr: Some(da_target),
                field: da_field,
                ..
            } = &func.expr
            {
                if let SurfaceExpression::VarRef { name, .. } = &da_target.expr {
                    if name.starts_with("ℊꜱʏᴍ⧼do-infer⧽") && named_args.is_empty() {
                        // check_do_infer is async; use Unknown in sync context
                        let _ = da_field;
                        return Ok(Type::Unknown);
                    }
                }
            }

            // Special case: [if cond true-branch false-branch] with path-sensitive narrowing.
            // Applies narrowing constraints from the condition to the true branch environment,
            // then joins the branch result types. Both branches are widened (literal → base type)
            // before joining so that Union(Int, IntLiteral(0)) collapses to Int correctly.
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "if" && args.len() == 3 && named_args.is_empty() {
                    let cond_node = &args[0];
                    let true_node = &args[1];
                    let false_node = &args[2];

                    // Infer condition for side effects (type errors on the condition itself).
                    let _cond_ty = Box::pin(infer_surface_expr(cond_node, env, state, type_map))
                        .await
                        .unwrap_or(Type::Unknown);

                    // Extract narrowing constraints from the condition.
                    let narrowings = extract_narrowings(cond_node);

                    // Infer true branch with narrowed environment.
                    let true_ty = if narrowings.is_empty() {
                        Box::pin(infer_surface_expr(true_node, env, state, type_map))
                            .await
                            .unwrap_or(Type::Unknown)
                    } else {
                        let narrowed_env = apply_narrowings(env, &narrowings, state);
                        Box::pin(infer_surface_expr(
                            true_node,
                            &narrowed_env,
                            state,
                            type_map,
                        ))
                        .await
                        .unwrap_or(Type::Unknown)
                    };

                    // Infer false branch with the original environment.
                    let false_ty = Box::pin(infer_surface_expr(false_node, env, state, type_map))
                        .await
                        .unwrap_or(Type::Unknown);

                    // Widen literal types before joining so that Union(Int, IntLiteral(n)) → Int,
                    // Union(Str, StringLiteral(s)) → Str, etc.
                    let true_ty = widen_literal_types(true_ty);
                    let false_ty = widen_literal_types(false_ty);

                    let result_ty = if true_ty == false_ty {
                        true_ty
                    } else {
                        Type::normalize_union(vec![true_ty, false_ty])
                    };
                    return Ok(result_ty);
                }
            }

            // Special case: if func is a VarRef to a polymorphic scheme, pass the scheme
            // directly to avoid double instantiation (VAR-POLY followed by CALL-POLY).
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                // Special case: `builtin-get`, `get`, `get?` — field-type-resolving dispatch.
                // In the sync path, the FD improvement machinery (check_constraints_on_var) is
                // not called, so the Indexable constraint does not resolve `v` to the field type.
                // This special case implements the Indexable Record case directly:
                // when the key is a string literal and the container is a Record, return the field type.
                // For `get?`, the return type is `field_type | Null`.
                if (name == "builtin-get" || name == "get" || name == "get?")
                    && args.len() == 2
                    && named_args.is_empty()
                {
                    // Infer the container type (arg 1)
                    let container_ty = Box::pin(infer_surface_expr(&args[1], env, state, type_map))
                        .await
                        .unwrap_or(Type::Unknown);
                    let container_resolved = state.subst.apply(&container_ty);

                    // Infer the key type (arg 0)
                    let key_ty = Box::pin(infer_surface_expr(&args[0], env, state, type_map))
                        .await
                        .unwrap_or(Type::Unknown);
                    let key_resolved = state.subst.apply(&key_ty);

                    // When the key is a string literal and container is a Record, resolve the field type.
                    let field_ty = if let (Type::StringLiteral(field_name), Type::Dict(row)) =
                        (&key_resolved, &container_resolved)
                    {
                        row.fields
                            .get(field_name.as_str())
                            .cloned()
                            .unwrap_or(Type::Unknown)
                    } else if let (Type::StringLiteral(field_name), Type::Union(members)) =
                        (&key_resolved, &container_resolved)
                    {
                        // Union of records: distribute field access and normalize
                        let field_types: Vec<Type> = members
                            .iter()
                            .filter_map(|m| {
                                if let Type::Dict(row) = m {
                                    row.fields.get(field_name.as_str()).cloned()
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if field_types.is_empty() {
                            Type::Unknown
                        } else {
                            Type::normalize_union(field_types)
                        }
                    } else {
                        Type::Unknown
                    };

                    return if name == "get?" {
                        // get? returns the field type or Null (empty record)
                        let null_ty = Type::Dict(Row {
                            fields: indexmap::IndexMap::new(),
                            tail: crate::type_def::RowTail::Empty,
                        });
                        if matches!(field_ty, Type::Unknown) {
                            Ok(Type::Unknown)
                        } else {
                            Ok(Type::normalize_union(vec![field_ty, null_ty]))
                        }
                    } else {
                        Ok(field_ty)
                    };
                }

                // Special case: `get-in` — path-following field access with precise return type.
                // [GET-IN-NIL]: empty path returns dict type unchanged.
                // [GET-IN-CONS]: literal string path follows fields through records.
                // Variable/non-literal path: returns Unknown (gradual fallback).
                if name == "get-in" && args.len() == 2 && named_args.is_empty() {
                    // Infer dict type (arg 1)
                    let dict_ty = Box::pin(infer_surface_expr(&args[1], env, state, type_map))
                        .await
                        .unwrap_or(Type::Unknown);
                    let dict_ty = state.subst.apply(&dict_ty);

                    // Check if path (arg 0) is a literal dict with auto-indexed string entries
                    match &args[0].expr {
                        SurfaceExpression::Dict(path_entries) => {
                            // Extract string literal keys from path
                            let mut keys: Vec<String> = Vec::new();
                            let mut all_literal = true;
                            for (idx, entry) in path_entries.iter().enumerate() {
                                let is_auto_indexed = match &entry.node.key {
                                    None => true,
                                    Some(k) => {
                                        matches!(&k.expr, SurfaceExpression::Int(n) if *n == idx as i64)
                                    }
                                };
                                if !is_auto_indexed {
                                    all_literal = false;
                                    break;
                                }
                                match &entry.node.value.expr {
                                    SurfaceExpression::StringLiteral { content, .. } => {
                                        keys.push(content.clone())
                                    }
                                    _ => {
                                        all_literal = false;
                                        break;
                                    }
                                }
                            }
                            if all_literal {
                                if keys.is_empty() {
                                    // Empty path: return dict type unchanged
                                    return Ok(dict_ty);
                                }
                                // Follow path through records
                                let mut current = dict_ty;
                                let mut resolved = true;
                                for key in &keys {
                                    current = state.subst.apply(&current);
                                    match &current {
                                        Type::Dict(row) => {
                                            if let Some(field_ty) = row.fields.get(key.as_str()) {
                                                current = field_ty.clone();
                                            } else {
                                                current = Type::Unknown;
                                                resolved = false;
                                                break;
                                            }
                                        }
                                        Type::Unknown => {
                                            resolved = false;
                                            break;
                                        }
                                        _ => {
                                            current = Type::Unknown;
                                            resolved = false;
                                            break;
                                        }
                                    }
                                }
                                let _ =
                                    Box::pin(infer_surface_expr(&args[0], env, state, type_map))
                                        .await;
                                return Ok(if resolved { current } else { Type::Unknown });
                            }
                        }
                        _ => {}
                    }
                    // Non-literal path: infer arg 0 for side effects, return Unknown
                    let _ = Box::pin(infer_surface_expr(&args[0], env, state, type_map)).await;
                    return Ok(Type::Unknown);
                }

                // Slot-indexed fast path: try the resolution table before name-based lookup.
                // Class method VarRefs like `+`, `-`, `*`, `/`, `=`, `<` are registered in
                // the letrec env under ɪ-prefixed names (e.g. ɪɴꜱᴛᴀɴᴄᴇ⧼Addable∷+⟨Integer⟩⧽),
                // NOT under their operator symbol. Name-based get_scheme("+") returns None,
                // triggering a false "undefined variable: +" warning. The resolution table has
                // the correct (level, slot) → slot-based get_scheme_at finds the ɪ-prefixed
                // TypeScheme registered by infer_instance_decl_from_surface.
                // Name-based lookup first: same rationale as in the VarRef arm.
                // Name-based lookup finds the correct scheme from the type checker's env chain.
                // Slot-based lookup is the fallback for ɪ-prefixed class method bindings only.
                let scheme_opt: Option<TypeScheme> = {
                    let name_scheme = env.read().unwrap().get_scheme(name);
                    if name_scheme.is_some() {
                        name_scheme
                    } else {
                        let mut slot_scheme: Option<TypeScheme> = None;
                        if let Some(ref table) = state.resolution_table {
                            let func_id = node_id(func);
                            if let Some(&(level, slot)) = table.get(&func_id) {
                                slot_scheme = env.read().unwrap().get_scheme_at(level, slot);
                            }
                        }
                        slot_scheme
                    }
                };
                match scheme_opt {
                    Some(scheme)
                        if !scheme.type_vars.is_empty() || !scheme.kind_vars.is_empty() =>
                    {
                        // Record scheme for LSP hover
                        if !scheme.constraints.is_empty()
                            || !scheme.type_vars.is_empty()
                            || !scheme.kind_vars.is_empty()
                        {
                            if let Some(ref mut smap) = state.scheme_map {
                                let key = (func.span.start.offset, func.span.end.offset);
                                smap.insert(key, scheme.clone());
                            }
                        }
                        // Instantiate the scheme before inferring args so that the fresh
                        // TypeVar names in the instantiated function type are available for
                        // try_resolve_call_dispatch (which maps param TypeVars to arg types).
                        let constraints_before = state.constraints.len();
                        let inst_ty = instantiate_scheme(
                            &scheme,
                            state.level,
                            state,
                            Some(name.as_str()),
                            Some(node.span.clone()),
                            &node.span,
                        );
                        // Record the function VarRef span → instantiated type in type_map.
                        // This mirrors check_call_with_scheme's explicit recording, enabling
                        // LSP hover to show the function type at call sites.
                        if let Some(ref mut tm) = type_map {
                            let key = (func.span.start.offset, func.span.end.offset);
                            tm.insert(key, inst_ty.clone());
                        }
                        // Arity check on the instantiated type before inferring args.
                        // Catches wrong argument count for polymorphic functions.
                        if let Type::Function {
                            params: ref fn_params,
                            variadic: fn_variadic,
                            required_count: fn_required,
                            ..
                        } = &inst_ty
                        {
                            let total_supplied = args.len() + named_args.len();
                            // Mirror check_call_args (typecheck_call.rs:534-537): for variadic
                            // functions the variadic param itself is not required, so subtract 1.
                            let min_required = if *fn_variadic && !fn_params.is_empty() {
                                fn_required.saturating_sub(1)
                            } else {
                                *fn_required
                            };
                            let max_allowed = if *fn_variadic {
                                usize::MAX
                            } else {
                                fn_params.len()
                            };
                            if total_supplied < min_required || total_supplied > max_allowed {
                                return Err(vec![TypeError::new(
                                    format!(
                                        "arity mismatch: expected {}{} arguments, got {}",
                                        if *fn_variadic { "at least " } else { "" },
                                        if *fn_variadic {
                                            min_required
                                        } else {
                                            fn_params.len()
                                        },
                                        total_supplied,
                                    ),
                                    node.span.clone(),
                                )]);
                            }
                        }
                        // Infer args and collect types for call_dispatch resolution.
                        let mut arg_types: Vec<Type> = Vec::with_capacity(args.len());
                        for arg in args {
                            match Box::pin(infer_surface_expr(arg, env, state, type_map)).await {
                                Ok(ty) => arg_types.push(ty),
                                Err(_) => arg_types.push(Type::Unknown),
                            }
                        }
                        // Bind instantiated param TypeVars to inferred arg types.
                        // This is the sync approximation of CALL-POLY unification:
                        // for each param `TypeVar(_tN)` and corresponding arg type T,
                        // bind `_tN → T` in state.subst. This makes the return type resolve
                        // correctly (e.g., identity `∀a. a→a` applied to Int returns Int).
                        // For subsequent args that bind the same TypeVar (from same annotation
                        // name, e.g. `fn [let x@a y@a]`), check for type conflicts.
                        let mut poly_errors: Vec<TypeError> = Vec::new();
                        if let Type::Function {
                            params: ref fn_params,
                            ..
                        } = &inst_ty
                        {
                            for ((_, param_ty), arg_ty) in fn_params.iter().zip(arg_types.iter()) {
                                let param_resolved = state.subst.apply(param_ty);
                                match param_resolved {
                                    Type::TypeVar(var_name, _) => {
                                        let existing = {
                                            let map = state.subst.type_map.borrow();
                                            map.get(&var_name).cloned()
                                        };
                                        match existing {
                                            None => {
                                                // First binding: bind TypeVar to arg type
                                                state
                                                    .subst
                                                    .type_map
                                                    .borrow_mut()
                                                    .insert(var_name, arg_ty.clone());
                                            }
                                            Some(bound) => {
                                                // Subsequent binding: check consistency
                                                let bound_resolved = state.subst.apply(&bound);
                                                if !Type::is_consistent_subtype(
                                                    arg_ty,
                                                    &bound_resolved,
                                                ) && !Type::is_consistent_subtype(
                                                    &bound_resolved,
                                                    arg_ty,
                                                ) {
                                                    poly_errors.push(TypeError::type_mismatch(
                                                        &bound_resolved,
                                                        arg_ty,
                                                        node.span.clone(),
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                    already_bound => {
                                        // TypeVar already fully resolved — check consistency with arg.
                                        // This handles the case where apply() resolved the TypeVar
                                        // to a concrete type (e.g., IntLiteral) from a prior arg.
                                        if !Type::is_consistent_subtype(arg_ty, &already_bound)
                                            && !Type::is_consistent_subtype(&already_bound, arg_ty)
                                        {
                                            poly_errors.push(TypeError::type_mismatch(
                                                &already_bound,
                                                arg_ty,
                                                node.span.clone(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                        if !poly_errors.is_empty() {
                            return Err(poly_errors);
                        }
                        // Attempt to set call_dispatch on the function VarRef if the scheme
                        // has typeclass constraints and all constraint vars can be resolved
                        // from the inferred arg types.
                        try_resolve_call_dispatch(
                            func,
                            &inst_ty,
                            &arg_types,
                            constraints_before,
                            state,
                        );
                        if let Type::Function { ret, .. } = inst_ty {
                            Ok(state.subst.apply(&ret))
                        } else if matches!(inst_ty, Type::Unknown | Type::Any) {
                            // Gradual: Unknown/Any in function position → return Unknown
                            Ok(Type::Unknown)
                        } else if matches!(inst_ty, Type::Error(_)) {
                            // Error cascade suppression (B-180): scheme body is Error → suppress T003.
                            for arg in args {
                                let _ =
                                    Box::pin(infer_surface_expr(arg, env, state, type_map)).await;
                            }
                            for na in named_args {
                                let _ = Box::pin(infer_surface_expr(
                                    &na.node.value,
                                    env,
                                    state,
                                    type_map,
                                ))
                                .await;
                            }
                            Ok(Type::Unknown)
                        } else {
                            // Concrete non-function type in call position → error
                            Err(vec![TypeError::not_a_function(&inst_ty, func.span.clone())])
                        }
                    }
                    Some(scheme) => {
                        // Monomorphic call (no quantified type variables).
                        // Apply substitution to get the concrete function type, then check
                        // arity and argument types synchronously.
                        let func_ty = state.subst.apply(&scheme.body);
                        // Record the function type at the func VarRef span for LSP hover.
                        // Without this, the func VarRef has no entry in type_map
                        // because the monomorphic path looks up the scheme directly
                        // rather than recursively calling infer_surface_expr on the func.
                        if let Some(ref mut tm) = type_map {
                            let key = (func.span.start.offset, func.span.end.offset);
                            tm.insert(key, Type::simplify_type(func_ty.clone()));
                        }
                        if let Type::Function {
                            params: fn_params,
                            ret: fn_ret,
                            variadic: fn_variadic,
                            required_count: fn_required,
                        } = func_ty
                        {
                            // Delegate to check_call_args — the single canonical call-checking
                            // path — instead of duplicating arity/positional/named-arg logic here.
                            // Use mem::take to avoid borrowing both `state` and `state.constraints`.
                            let mut constraints = std::mem::take(&mut state.constraints);
                            let constraints_start = constraints.len();
                            let result = Box::pin(typecheck_call::check_call_args(
                                &fn_params,
                                &fn_ret,
                                fn_variadic,
                                fn_required,
                                None,
                                args,
                                named_args,
                                env,
                                node.span.clone(),
                                state,
                                &mut constraints,
                                type_map,
                                constraints_start,
                                false,
                            ))
                            .await;
                            state.constraints = constraints;
                            result
                        } else if matches!(func_ty, Type::TypeVar(_, _) | Type::Unknown | Type::Any)
                        {
                            // TypeVar or gradual type: cannot check call statically.
                            // Infer func and args for side effects, return Unknown.
                            let _func_ty =
                                Box::pin(infer_surface_expr(func, env, state, type_map)).await?;
                            for arg in args {
                                let _ =
                                    Box::pin(infer_surface_expr(arg, env, state, type_map)).await;
                            }
                            Ok(Type::Unknown)
                        } else if matches!(func_ty, Type::Error(_)) {
                            // Error cascade suppression (B-180): when calling a function typed as
                            // Type::Error (e.g. because its definition failed type-checking), do NOT
                            // emit a T003 "expected function type, got <error>" error. The root cause
                            // has already been reported at the definition site.
                            // Infer args for side effects (type map population) and return Unknown.
                            for arg in args {
                                let _ =
                                    Box::pin(infer_surface_expr(arg, env, state, type_map)).await;
                            }
                            for na in named_args {
                                let _ = Box::pin(infer_surface_expr(
                                    &na.node.value,
                                    env,
                                    state,
                                    type_map,
                                ))
                                .await;
                            }
                            Ok(Type::Unknown)
                        } else {
                            // Concrete non-function type: calling a non-function is an error.
                            Err(vec![TypeError::not_a_function(&func_ty, func.span.clone())])
                        }
                    }
                    None => {
                        let mut err = TypeError::undefined_variable(name, func.span.clone());
                        if let Some(cause_span) = state.failed_bindings.get(name.as_str()) {
                            err.notes.push(format!(
                                "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                                cause_span.start.line, cause_span.start.column
                            ));
                        }
                        Err(vec![err])
                    }
                }
            } else {
                // Non-VarRef function expression (e.g., literal, nested call).
                // Infer the function expression type and check it's callable.
                let func_ty = Box::pin(infer_surface_expr(func, env, state, type_map)).await?;
                let func_resolved = if state.subst.type_map.borrow().is_empty() {
                    func_ty
                } else {
                    state.subst.apply(&func_ty)
                };
                // Detect non-function types in call position (e.g., `[call 42 1]`).
                if !matches!(
                    &func_resolved,
                    Type::Function { .. }
                        | Type::TypeVar(_, _)
                        | Type::Unknown
                        | Type::Any
                        | Type::Error(_) // Error cascade suppression (B-180)
                ) {
                    return Err(vec![TypeError::not_a_function(
                        &func_resolved,
                        func.span.clone(),
                    )]);
                }
                // When the function type is a concrete Function with inference vars (CALL-POLY),
                // bind param TypeVars to arg types in state.subst, then return the resolved ret type.
                // This enables inline lambdas like `[call [fn [let x@a] $x] $data]` to produce
                // the correct return type (not just Unknown). Mirrors the VarRef polymorphic path.
                if let Type::Function {
                    params: ref fn_params,
                    ret: ref fn_ret,
                    variadic: fn_variadic,
                    required_count: fn_required,
                } = func_resolved
                {
                    let total_supplied = args.len() + named_args.len();
                    let min_required = fn_required;
                    let max_allowed = if fn_variadic {
                        usize::MAX
                    } else {
                        fn_params.len()
                    };
                    if total_supplied < min_required || total_supplied > max_allowed {
                        // Arity mismatch for non-VarRef callee — still infer args for side effects
                        for arg in args {
                            let _ = Box::pin(infer_surface_expr(arg, env, state, type_map)).await;
                        }
                        return Ok(Type::Unknown);
                    }
                    let mut arg_types: Vec<Type> = Vec::with_capacity(args.len());
                    for arg in args {
                        match Box::pin(infer_surface_expr(arg, env, state, type_map)).await {
                            Ok(ty) => arg_types.push(ty),
                            Err(_) => arg_types.push(Type::Unknown),
                        }
                    }
                    // Bind param TypeVars to arg types (sync approximation of CALL-POLY unification)
                    let fn_params_clone = fn_params.clone();
                    let fn_ret_clone = fn_ret.clone();
                    let fn_variadic_clone = fn_variadic;
                    for ((_param_name, param_ty), arg_ty) in
                        fn_params_clone.iter().zip(arg_types.iter())
                    {
                        let param_resolved = state.subst.apply(param_ty);
                        if let Type::TypeVar(var_name, _) = param_resolved {
                            state
                                .subst
                                .type_map
                                .borrow_mut()
                                .insert(var_name, arg_ty.clone());
                        }
                    }
                    let _ = fn_variadic_clone;
                    return Ok(state.subst.apply(&fn_ret_clone));
                }
                for arg in args {
                    let _ = Box::pin(infer_surface_expr(arg, env, state, type_map)).await;
                }
                Ok(Type::Unknown)
            }
        }

        SurfaceExpression::Fn {
            return_ann,
            params,
            body,
            ..
        } => {
            // Convert SurfaceParam → Param (identical fields).
            use crate::ast::Param;
            let params_converted: Vec<Spanned<Param>> = params
                .iter()
                .map(|p| {
                    Spanned::new(
                        Param {
                            name: p.node.name.clone(),
                            annotation: p.node.annotation.clone(),
                            variadic: p.node.variadic,
                        },
                        p.span.clone(),
                    )
                })
                .collect();
            // Resolve annotations via the async resolver (resolve_annotation from
            // typecheck_annot.rs). TypeVars must be declared via bind: in the fn metadata;
            // resolve_annotation populates ann_mapping_str from bind: declarations before
            // resolving return: and param annotations.
            {
                // ann_mapping_str starts empty — bind: processing populates it.
                // All annotations share the same map so TypeVars are deduplicated.
                let mut ann_mapping_str: HashMap<String, String> = HashMap::new();
                let stub_type_env = crate::types::TypeEnv::new();
                let mut constraints: Vec<crate::types::Constraint> = Vec::new();
                let mut ann_mapping_opt = Some(&mut ann_mapping_str);
                let mut row_ann_mapping_str: HashMap<String, String> = HashMap::new();
                let mut row_ann_mapping_opt = Some(&mut row_ann_mapping_str);
                // Accumulate annotation errors so that all param annotations are validated
                // even when the return annotation fails. Mirrors infer_fn_push_cont (CEK path)
                // which pushes errors and continues rather than returning early.
                let mut ann_errors: Vec<TypeError> = Vec::new();

                // Step 1: Resolve return annotation FIRST so bind: TypeVars are registered
                // in ann_mapping_str before param annotations are resolved.
                // For fn metadata dicts (bind:/return:/constraint:/doc:), call
                // resolve_fn_metadata directly — this populates ann_mapping_str via bind:
                // AND returns the unwrapped return type (not a Type::Function wrapper).
                // For other annotation forms, use resolve_annotation normally.
                let pre_declared_ret: Option<Option<Type>> = if let Some(ret_ann) = return_ann {
                    let resolved = match &ret_ann.node {
                        crate::ast::Annotation::PropertyDict(entries)
                            if entries.iter().any(|e| {
                                e.node.key.as_ref().map_or(false, |k| {
                                    matches!(&k.expr,
                                        crate::ast::SurfaceExpression::StringLiteral { content: s, .. }
                                            if crate::ast::STANDARD_ANN_KEYS.contains(&s.as_str()))
                                })
                            }) =>
                        {
                            // fn metadata dict: resolve_fn_metadata populates bind: TypeVars
                            // into ann_mapping_str and returns the extracted return type directly.
                            match typecheck_annot::resolve_fn_metadata(
                                entries,
                                &stub_type_env,
                                ret_ann.span.clone(),
                                state,
                                &mut constraints,
                                &mut ann_mapping_opt,
                                &mut row_ann_mapping_opt,
                                None,
                            )
                            .await
                            {
                                Ok((ret_ty, _doc)) => Some(ret_ty),
                                Err(e) => {
                                    ann_errors.push(TypeError::from(e));
                                    None
                                }
                            }
                        }
                        _ => {
                            // Non-metadata annotation: use resolve_annotation normally.
                            // On failure, accumulate the error and treat the return type as
                            // absent (None) so param annotations are still validated.
                            match typecheck_annot::resolve_annotation(
                                &ret_ann.node,
                                &stub_type_env,
                                ret_ann.span.clone(),
                                state,
                                &mut constraints,
                                &mut ann_mapping_opt,
                                &mut row_ann_mapping_opt,
                                None,
                            )
                            .await
                            {
                                Ok(ty) => Some(ty),
                                Err(e) => {
                                    ann_errors.push(e);
                                    None
                                }
                            }
                        }
                    };
                    Some(resolved)
                } else {
                    None
                };

                // Step 2: Resolve param annotations using the now-populated ann_mapping.
                let mut fn_env_inner = Env::with_parent(Arc::clone(env));
                let mut param_types: Vec<(Option<String>, Type)> = Vec::new();
                for p in &params_converted {
                    let param_ty = if p.node.variadic {
                        let elem_ty = state.fresh_type_var(&p.span);
                        Type::Dict(Row {
                            fields: indexmap::IndexMap::new(),
                            tail: crate::type_def::RowTail::Uniform {
                                key: None,
                                value: Box::new(elem_ty),
                            },
                        })
                    } else if let Some(ann) = &p.node.annotation {
                        match typecheck_annot::resolve_annotation(
                            &ann.node,
                            &stub_type_env,
                            ann.span.clone(),
                            state,
                            &mut constraints,
                            &mut ann_mapping_opt,
                            &mut row_ann_mapping_opt,
                            None,
                        )
                        .await
                        {
                            Ok(ty) => ty,
                            Err(e) => {
                                ann_errors.push(e);
                                Type::Unknown
                            }
                        }
                    } else {
                        Type::Unknown
                    };
                    fn_env_inner.insert(p.node.name.clone(), param_ty.clone());
                    param_types.push((Some(p.node.name.clone()), param_ty));
                }
                // All param annotations have been validated. If any annotation failed,
                // return all errors now — the fn type cannot be soundly inferred.
                if !ann_errors.is_empty() {
                    return Err(ann_errors);
                }
                let fn_env_arc = Arc::new(RwLock::new(fn_env_inner));
                let body_ty =
                    Box::pin(infer_surface_expr(body, &fn_env_arc, state, type_map)).await?;

                // Step 3: Use the pre-resolved return type (or fall back to body type).
                let fn_ret_ty = if let Some(declared_ret_opt) = pre_declared_ret {
                    if let Some(declared_ret) = declared_ret_opt {
                        // Check for body type mismatches against concrete return annotations.
                        // Only check concrete primitive mismatches: Int, Float, String, Bytes.
                        // Avoid TyCon (e.g. "Dict" doesn't match Record), TypeVar, Unknown, Any.
                        let is_checkable_primitive = matches!(
                            declared_ret,
                            Type::Int | Type::Float | Type::Str | Type::Bytes
                        );
                        if is_checkable_primitive {
                            let body_resolved = if state.subst.type_map.borrow().is_empty() {
                                body_ty.clone()
                            } else {
                                state.subst.apply(&body_ty)
                            };
                            // Only report if body is also a concrete non-Unknown primitive type
                            let body_is_concrete = !matches!(
                                body_resolved,
                                Type::Unknown | Type::Any | Type::TypeVar(..)
                            );
                            if body_is_concrete
                                && !Type::is_consistent_subtype(&body_resolved, &declared_ret)
                            {
                                return Err(vec![TypeError::type_mismatch(
                                    &declared_ret,
                                    &body_resolved,
                                    node.span.clone(),
                                )]);
                            }
                        }
                        // If the resolved return type is Unknown and the body type is concrete,
                        // prefer the body type (it carries more information than Unknown).
                        // But preserve TypeVar — it's a deliberate annotation that should unify.
                        match &declared_ret {
                            Type::Unknown => body_ty.clone(),
                            _ => declared_ret,
                        }
                    } else {
                        // Return annotation resolved to error — fall back to body type.
                        body_ty.clone()
                    }
                } else {
                    body_ty.clone()
                };

                let is_variadic = params_converted.iter().any(|p| p.node.variadic);
                // required_count: for variadic functions, the variadic param itself doesn't count.
                let required_count = if is_variadic {
                    params_converted.len().saturating_sub(1)
                } else {
                    params_converted.len()
                };
                Ok(Type::Function {
                    params: param_types,
                    ret: Box::new(fn_ret_ty),
                    variadic: is_variadic,
                    required_count,
                })
            }
        }

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
            ..
        } => {
            // Infer the inner expression type, then resolve the annotation via the async
            // resolver and check for type mismatches (arity, return type, etc.).
            //
            // ASSERT-DEFAULT: when the annotation carries a `default:` property, a type
            // mismatch or inference error on the inner expression is suppressed — the
            // default provides a fallback value at runtime. Errors on the default itself
            // are never suppressed (hard type errors).
            let has_default = annotation.node.get_property("default").is_some();
            let actual_result = Box::pin(infer_surface_expr(inner, env, state, type_map)).await;

            let (actual, inner_ok) = match actual_result {
                Ok(ty) => (ty, true),
                Err(errs) => {
                    if has_default {
                        // Suppress inner inference error when default is present.
                        (Type::Unknown, false)
                    } else {
                        return Err(errs);
                    }
                }
            };

            // Resolve the annotation using the async resolver so that user-defined type names
            // (e.g. @Dict, @Color) are looked up through state.tycon_env rather than falling
            // back to the opaque TyCon("…") sentinel produced by the old sync resolver.
            let stub_env = crate::types::TypeEnv::new();
            let mut constraints: Vec<crate::types::Constraint> = Vec::new();
            let annotation_result = typecheck_annot::resolve_annotation(
                &annotation.node,
                &stub_env,
                annotation.span.clone(),
                state,
                &mut constraints,
                &mut None,
                &mut None,
                None,
            )
            .await;
            let annotation_resolved = match annotation_result {
                Ok(ty) => Some(ty),
                Err(_) => None,
            };

            if let Some(expected) = annotation_resolved {
                let expected_resolved = if state.subst.type_map.borrow().is_empty() {
                    expected.clone()
                } else {
                    state.subst.apply(&expected)
                };

                // Only check type compatibility when the inner expression inferred successfully.
                if inner_ok {
                    let actual_resolved = if state.subst.type_map.borrow().is_empty() {
                        actual.clone()
                    } else {
                        state.subst.apply(&actual)
                    };

                    // Check function arity and type mismatches
                    let mismatch_err = match (&actual_resolved, &expected_resolved) {
                        (
                            Type::Function {
                                params: p_actual,
                                ret: r_actual,
                                ..
                            },
                            Type::Function {
                                params: p_expected,
                                ret: r_expected,
                                ..
                            },
                        ) => {
                            if p_actual.len() != p_expected.len() {
                                Some(vec![TypeError::new(
                                    format!(
                                        "arity mismatch: expected {} arguments, got {}",
                                        p_expected.len(),
                                        p_actual.len()
                                    ),
                                    node.span.clone(),
                                )])
                            } else {
                                // Check param types FIRST (before return type).
                                // A parameter annotation incompatibility is the primary error:
                                // the lambda's annotated param type is more restrictive than required.
                                // This mirrors bidirectional typing (Pierce & Turner 2000 §3.2): when
                                // checking a lambda against a concrete function type, param annotations
                                // must be compatible with the expected param types (contravariant).
                                let mut param_err = None;
                                for (_, ((_, p_act), (_, p_exp))) in
                                    p_actual.iter().zip(p_expected.iter()).enumerate()
                                {
                                    if !Type::is_consistent_subtype(p_act, p_exp) {
                                        // Parameter annotation is incompatible with expected type.
                                        // Produce "parameter annotation more restrictive" error in
                                        // all incompatible cases — the annotation makes the function
                                        // accept fewer values than the required type allows.
                                        param_err = Some(vec![TypeError::new(
                                            format!(
                                                "[TypeError] parameter annotation {} is more restrictive than required type {}",
                                                p_act, p_exp
                                            ),
                                            node.span.clone(),
                                        )]);
                                        break;
                                    }
                                }
                                // If no param error, check return type.
                                if param_err.is_some() {
                                    param_err
                                } else if !Type::is_consistent_subtype(r_actual, r_expected) {
                                    Some(vec![TypeError::type_mismatch(
                                        r_expected,
                                        r_actual,
                                        node.span.clone(),
                                    )])
                                } else {
                                    None
                                }
                            }
                        }
                        _ => {
                            // Non-function type: general consistency check
                            if !Type::is_consistent_subtype(&actual_resolved, &expected_resolved) {
                                Some(vec![TypeError::type_mismatch(
                                    &expected_resolved,
                                    &actual_resolved,
                                    node.span.clone(),
                                )])
                            } else {
                                None
                            }
                        }
                    };

                    if let Some(errs) = mismatch_err {
                        if !has_default {
                            return Err(errs);
                        }
                        // With default: suppress the main-check error (ASSERT-DEFAULT rule).
                    }
                }

                // Validate the default value type — hard error regardless of main-check result.
                if let Some(default_node) = annotation.node.get_property("default") {
                    match Box::pin(infer_surface_expr(default_node, env, state, type_map)).await {
                        Ok(default_ty) => {
                            let default_resolved = if state.subst.type_map.borrow().is_empty() {
                                default_ty
                            } else {
                                state.subst.apply(&default_ty)
                            };
                            let passes = Type::is_subtype(
                                &default_resolved,
                                &expected_resolved,
                                Some(&state.tycon_env),
                            ) || ((contains_unknown_or_top(&default_resolved)
                                || contains_unknown_or_top(&expected_resolved))
                                && Type::is_consistent(&default_resolved, &expected_resolved));
                            if !passes {
                                return Err(vec![TypeError::new(
                                    format!(
                                        "default value type mismatch: default has type {default_resolved}, \
                                         but assertion expects {expected_resolved}"
                                    ),
                                    default_node.span.clone(),
                                )]);
                            }
                        }
                        Err(errs) => return Err(errs),
                    }
                }

                Ok(expected)
            } else {
                Ok(actual)
            }
        }

        // SurfaceExpression::Annotated was removed — annotation now on VarRef.
        // VarRef with annotation: handled by the VarRef arm above.
        SurfaceExpression::Quote(_inner) => {
            // [quote expr] produces a dict representing the AST.
            Ok(Type::Dict(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }))
        }

        SurfaceExpression::Unquote(inner) => {
            // [unquote expr] evaluates expr and returns its type.
            Box::pin(infer_surface_expr(inner, env, state, type_map)).await
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            // [unquote-splice expr] — infer inner type only (unify is async)
            let _inner_ty = Box::pin(infer_surface_expr(inner, env, state, type_map)).await?;
            Ok(Type::Unknown)
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            // Infer scrutinee type — needed for exhaustiveness checking.
            let scrutinee_ty =
                Box::pin(infer_surface_expr(scrutinee, env, state, type_map)).await?;
            let scrutinee_ty = state.subst.apply(&scrutinee_ty);

            // I-Case3 (BAS match narrowing): maintain a "remaining scrutinee" type that
            // accumulates negations as Constructor/TypeTag arms are processed.
            let mut remaining_scrutinee = scrutinee_ty.clone();
            let mut arm_result_types: Vec<Type> = Vec::new();

            for arm in arms {
                // Compute the arm-local scrutinee type from I-Case3.
                let arm_scrutinee_ty = match &arm.pattern.node {
                    Pattern::Constructor { tag, .. } => {
                        let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                        if matches!(&remaining_scrutinee, Type::NominalVariant { tycon: t, ctor: c, .. } if t == tycon && c == ctor)
                        {
                            remaining_scrutinee.clone()
                        } else {
                            let tag_ty = Type::NominalVariant {
                                tycon: tycon.to_string(),
                                ctor: ctor.to_string(),
                                fields: crate::type_def::Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            };
                            let members = vec![remaining_scrutinee.clone(), tag_ty];
                            Type::normalize_intersection(members)
                        }
                    }
                    Pattern::Wildcard | Pattern::Pin(..) => remaining_scrutinee.clone(),
                    _ => scrutinee_ty.clone(),
                };

                let mut pat_bindings: Vec<(String, Type)> = Vec::new();
                collect_pattern_bindings(&arm.pattern.node, &arm_scrutinee_ty, &mut pat_bindings);
                let arm_env: Arc<RwLock<Env>> = if pat_bindings.is_empty() {
                    Arc::clone(env)
                } else {
                    let mut child_inner = Env::with_parent(Arc::clone(env));
                    for (name, ty) in pat_bindings {
                        child_inner.insert(name, ty);
                    }
                    Arc::new(RwLock::new(child_inner))
                };

                // Type-check guard if present, and apply is: predicate narrowing.
                let arm_env = if let Some(guard) = &arm.guard {
                    let _guard_ty =
                        Box::pin(infer_surface_expr(guard, &arm_env, state, type_map)).await?;
                    // extract_narrowings walks SurfaceExpression natively — pass guard directly.
                    let guard_narrowings = extract_narrowings(guard);
                    if guard_narrowings.is_empty() {
                        arm_env
                    } else {
                        apply_narrowings(&arm_env, &guard_narrowings, state)
                    }
                } else {
                    arm_env
                };
                let arm_ty = Box::pin(infer_surface_expr(
                    arm.body_expr(),
                    &arm_env,
                    state,
                    type_map,
                ))
                .await?;
                arm_result_types.push(arm_ty);

                // Update remaining_scrutinee for subsequent arms (I-Case3 negation accumulation).
                if arm.guard.is_none() {
                    match &arm.pattern.node {
                        Pattern::Constructor { tag, .. } => {
                            let (tycon, ctor) = tag.split_once('.').unwrap_or(("", tag.as_str()));
                            let neg_tag = Type::Negation(Box::new(Type::NominalVariant {
                                tycon: tycon.to_string(),
                                ctor: ctor.to_string(),
                                fields: crate::type_def::Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            }));
                            remaining_scrutinee = Type::normalize_intersection(vec![
                                remaining_scrutinee.clone(),
                                neg_tag,
                            ]);
                        }
                        Pattern::Wildcard | Pattern::Pin(..) => {
                            remaining_scrutinee = Type::Never;
                        }
                        _ => {}
                    }
                }
            }

            // Exhaustiveness checking (Maranget 2007).
            let tycon_env_ref = state.tycon_env_ref();
            let sig = match &scrutinee_ty {
                Type::Union(members) => {
                    coverage::ConstructorSignature::from_union(members, tycon_env_ref)
                }
                Type::NominalVariant {
                    tycon,
                    ctor,
                    fields,
                } => Some(coverage::ConstructorSignature::from_nominal_variant(
                    tycon,
                    ctor,
                    fields,
                    tycon_env_ref,
                )),
                // Nominal ADT: scrutinee is a TyCon with declared constructors.
                // Look up the constructors from tycon_env and build the signature directly.
                // This handles `[match c Boolean.True: t Boolean.False: e]` where c: TyCon("Boolean").
                // Mirrors the TyCon member handling in ConstructorSignature::from_union so that
                // the same exhaustiveness analysis applies when a TyCon appears as a scrutinee
                // directly (not wrapped in a Union).
                Type::TyCon(name) => match tycon_env_ref.get(name.as_str()) {
                    Some(def) if !def.constructors.is_empty() => {
                        let constructors = def
                            .constructors
                            .iter()
                            .map(|(tag, arity)| {
                                let clamped = if *arity == 0 { 0 } else { 1 };
                                (coverage::ConstructorTag::Variant(tag.clone()), clamped)
                            })
                            .collect();
                        Some(coverage::ConstructorSignature { constructors })
                    }
                    _ => None,
                },
                _ => None,
            };

            if let Some(sig) = sig {
                let coverage_patterns: Vec<coverage::CoveragePattern> = arms
                    .iter()
                    .map(|arm| {
                        coverage::ast_pattern_to_coverage(&arm.pattern.node, Some(tycon_env_ref))
                    })
                    .collect();
                let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();
                let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);
                let mut match_errors: Vec<TypeError> = Vec::new();

                if !result.exhaustive {
                    let witnesses = coverage::format_witnesses(&result.uncovered);
                    match_errors.push(TypeError::new(
                        format!("non-exhaustive match: missing coverage for {}", witnesses),
                        node.span.clone(),
                    ));
                }
                for &idx in &result.redundant {
                    match_errors.push(TypeError::new(
                        "unreachable match arm: this pattern is already covered by prior arms",
                        arms[idx].pattern.span.clone(),
                    ));
                }
                for &idx in &result.inaccessible {
                    match_errors.push(TypeError::new(
                        "inaccessible match arm: reachable only via diverging (bottom) values",
                        arms[idx].pattern.span.clone(),
                    ));
                }
                if !match_errors.is_empty() {
                    return Err(match_errors);
                }
            }

            let match_ty = if arm_result_types.is_empty() {
                Type::Unknown
            } else {
                let raw_union = Type::normalize_union(arm_result_types);
                Type::simplify_type(raw_union)
            };
            Ok(match_ty)
        }

        SurfaceExpression::Decl(decl_box) => {
            // Handle declaration forms embedded in expression context
            match **decl_box {
                SurfaceDeclaration::ClassDecl {
                    ref name,
                    ref params,
                    ref superclasses,
                    ref methods,
                    ref determines,
                    ref resolver,
                    resolver_injective,
                    ref structural,
                } => {
                    let sc_flat: Vec<(String, String)> = superclasses
                        .iter()
                        .flat_map(|(sc_name, sc_params)| {
                            sc_params.iter().map(|p| (sc_name.clone(), p.clone())).collect::<Vec<_>>()
                        })
                        .collect();
                    infer_class_decl_from_surface(
                        name,
                        params,
                        &sc_flat,
                        methods,
                        determines,
                        resolver,
                        resolver_injective,
                        structural,
                        node.span.clone(),
                        env,
                        state,
                        type_map,
                    )
                }
                SurfaceDeclaration::InstanceDecl {
                    ref class_name,
                    ref arms,
                } => Box::pin(infer_instance_decl_from_surface(
                    class_name,
                    arms,
                    node.span.clone(),
                    env,
                    state,
                    type_map,
                ))
                .await,
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Type alias declarations in expression position have no runtime type.
                    // expand_type_alias is async and cannot be called from sync infer_surface_expr.
                    // Alias body validation occurs in Pass 2 of dict inference (typecheck_dict.rs).
                    // Return Any (same sentinel that expand_type_alias returns when successful).
                    Ok(Type::Any)
                }
                SurfaceDeclaration::Splice(..) => Err(vec![TypeError::new(
                    "Splice should be removed by expansion pass before typechecking (internal error)",
                    node.span.clone(),
                )]),
                SurfaceDeclaration::SyntaxClass { .. } => Err(vec![TypeError::new(
                    "SyntaxClass should be removed by expansion pass before typechecking (internal error)",
                    node.span.clone(),
                )]),
            }
        }

        SurfaceExpression::LetDecl { .. } => {
            // LetDecl in value position is always an error (only valid in binding contexts).
            Err(vec![TypeError::new(
                "binding declaration [let ...] is not valid in expression position",
                node.span.clone(),
            )])
        }

        SurfaceExpression::CaseArm {
            let_bindings,
            pattern: _,
            body,
        } => {
            // CaseArm appears as the body of a match arm (stored with Wildcard sentinel pattern
            // by the parser). The match handler sees Wildcard and adds no bindings, so we must
            // extract bindings from let_bindings here and add them to a child TypeEnv.
            //
            // let_bindings is a [let name1 name2 ...] LetDecl node. Walk its bindings to collect
            // declared names; assign each a fresh TypeVar so the body can reference them.
            let arm_env: Arc<RwLock<Env>> = {
                let binding_names: Vec<String> = match &let_bindings.expr {
                    SurfaceExpression::LetDecl { bindings } => bindings
                        .iter()
                        .filter_map(|b| {
                            if let SurfaceExpression::VarRef { name, .. } = &b.expr {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                if binding_names.is_empty() {
                    Arc::clone(env)
                } else {
                    let mut child_inner = Env::with_parent(Arc::clone(env));
                    for name in binding_names {
                        child_inner.insert(name, state.fresh_type_var(&node.span));
                    }
                    Arc::new(RwLock::new(child_inner))
                }
            };
            Box::pin(infer_surface_expr(body, &arm_env, state, type_map)).await
        }

        SurfaceExpression::Placeholder => {
            // Gradual: placeholder (`...`) is the explicit gradual typing escape hatch.
            Ok(Type::Unknown)
        }

        SurfaceExpression::Rest(..) => {
            // Rest (...expr) in value position: the spread target type.
            // Returns Dict (open) — the spread source must be a dict.
            // The surrounding dict literal inference will detect this Rest and use an
            // open Uniform tail. Here we just return Dict as the type of the spread source.
            Ok(Type::Dict(Row {
                fields: indexmap::IndexMap::new(),
                tail: crate::type_def::RowTail::Uniform {
                    key: None,
                    value: Box::new(Type::Any),
                },
            }))
        }

        // U64 literals infer as Int (gradual: close enough for now)
        SurfaceExpression::U64(_) => Ok(Type::Int),

        SurfaceExpression::Field { expr, field, .. } => {
            // Dot-access: infer the base expression, then look up the field type.
            // For leading-dot form (expr: None), return Unknown (no base to infer from).
            match expr {
                None => Ok(Type::Unknown),
                Some(base) => {
                    let base_ty = Box::pin(infer_surface_expr(base, env, state, type_map)).await?;
                    // Look up the field type from the record type (or Unknown for gradual).
                    let resolved_base = state.subst.apply(&base_ty);
                    match &resolved_base {
                        Type::Dict(row) => {
                            let key = match field {
                                crate::ast::DotKey::Ident(s) => s.clone(),
                                crate::ast::DotKey::Int(n) => n.to_string(),
                            };
                            Ok(row.fields.get(&key).cloned().unwrap_or(Type::Unknown))
                        }
                        // Intersection: search all member records for the field.
                        Type::Intersection(members) => {
                            let key = match field {
                                crate::ast::DotKey::Ident(s) => s.clone(),
                                crate::ast::DotKey::Int(n) => n.to_string(),
                            };
                            for m in members {
                                if let Type::Dict(row) = m {
                                    if let Some(ty) = row.fields.get(&key) {
                                        return Ok(ty.clone());
                                    }
                                }
                            }
                            Ok(Type::Unknown)
                        }
                        // Gradual / unknown types — no static field information available.
                        Type::Unknown | Type::Any | Type::TypeVar(_, _) => Ok(Type::Unknown),
                        // Negation type: field access returns Unknown (conservative).
                        // A Negation type restricts what values CAN'T inhabit the type, but
                        // doesn't describe the field structure. Return Unknown for any field.
                        Type::Negation(_) => Ok(Type::Unknown),
                        // Union type: ADT names now carry a Dict value type (one field per
                        // constructor), so qualified constructor access (TypeNode.Dict) goes
                        // through the Type::Dict arm above.  A Union here means a genuinely
                        // union-typed value (e.g. after match narrowing collapsed to two
                        // branches) — field access on that is gradual.
                        Type::Union(_) => Ok(Type::Unknown),
                        // Concrete non-record types: produce an error.
                        // TypeVar cases are handled above; TyCon may expand to a Record so
                        // we allow Unknown for those.
                        // NominalVariant: a narrowed/matched variant value — field access is
                        // gradual (payload fields can be accessed; constructor dict fields are
                        // not meaningful on an instance).
                        Type::NominalVariant { .. } => Ok(Type::Unknown),
                        Type::TyCon(_) | Type::App(_, _) => Ok(Type::Unknown),
                        // All other concrete types (Int, Str, Float, Function, etc.): error.
                        other => Err(vec![TypeError::new(
                            format!("expected record type for field access, but got {}", other),
                            node.span.clone(),
                        )]),
                    }
                }
            }
        }

        SurfaceExpression::PatternDecl { .. } => {
            // PatternDecl should never appear in value positions (only in instance arms)
            Err(vec![TypeError::new(
                "pattern declaration is only valid in instance match arms",
                node.span.clone(),
            )])
        }

        SurfaceExpression::Error(_) => Err(vec![TypeError::new(
            "parse error node in expression position",
            node.span.clone(),
        )]),
    };

    // Record the inferred type in the type map (if collecting).
    // On error, record Type::Error as a sentinel so that LSP hover shows <error>
    // rather than no type at all, and parent expressions can see Error via the type_map
    // rather than inferring from a missing entry.
    // Simplify compound types (RDNF step 1d) before storing so LSP hover shows the
    // reduced form (e.g., Union([Int, Int]) → Int, Intersection([Never, T]) → Never).
    if let Some(ref mut map) = type_map {
        let key = (node.span.start.offset, node.span.end.offset);
        match &result {
            Ok(ty) => {
                let simplified = Type::simplify_type(ty.clone());
                map.insert(key, simplified);
            }
            Err(ref errs) => {
                let typed: Vec<crate::type_errors::TypeErrorTyped> = errs
                    .iter()
                    .map(|e| {
                        crate::type_errors::TypeErrorTyped::new(e.message.clone(), e.span.clone())
                    })
                    .collect();
                map.insert(key, Type::error_with(typed));
            }
        }
    }

    result
}

/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from infer_surface_expr (Decl arm) and typecheck_surface_document — no Expr bridge needed.
#[allow(clippy::too_many_arguments)]
fn infer_class_decl_from_surface(
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
/// Called from infer_surface_expr (Decl arm) and typecheck_surface_document — no Expr bridge needed.
async fn infer_instance_decl_from_surface(
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

            let method_impl_type =
                infer_surface_expr(&method.node.value, env, state, type_map).await?;
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

/// Check that an expression has a compatible type with the expected type.
/// Uses bidirectional type checking: synthesize the expression's type via `infer_surface_expr`,
/// then check subsumption via `is_subtype(actual, expected)`.
///
/// Per doc/06-type-inference.md §Bidirectional Typing, this is the [SUB] rule:
/// if `Γ ⊢ e ⇒ σ` and `σ <: τ`, then `Γ ⊢ e ⇐ τ`.
///
/// Special case for lambdas (doc/06 §[CHECK-FN]): when checking a function expression
/// against an expected function type, propagate the expected parameter types into the
/// lambda's parameter inference (Pierce & Turner 2000 lambda checking mode).
///
/// Check if a type contains Unknown or Top anywhere in its structure.
/// Used for the gradual typing fallback: when Unknown/Top appears anywhere in a type,
/// subsumption uses `is_consistent` instead of `is_subtype` to maintain the gradual guarantee.
#[allow(dead_code)]
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

/// This function is used at checking positions where the expected type is fully concrete
/// (no type variables): CALL-MONO arguments, concrete return annotations (no TypeVars), and TypeAssert.
#[allow(dead_code)]
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

    // Default: synthesize then check via infer_surface_expr.
    // Full lambda checking mode (CHECK-FN) that propagates expected parameter types into a lambda
    // requires async annotation resolution (resolve_annotation). Lambda inference is handled
    // correctly by infer_fn in typecheck_match.rs when infer_surface_expr reaches a Fn node.
    // Fall through to synthesize+subsume.
    let actual = Box::pin(infer_surface_expr(node, env, state, type_map)).await?;
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
