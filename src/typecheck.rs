//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner-style type variable
//! unification for polymorphic function calls.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{
    node_id, Pattern, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceExpression,
    SurfaceItem, SurfaceNode, SurfaceProgram, TypeAnnotationTable,
};
// All production inference helpers now walk SurfaceExpression natively.
// No Expr bridge needed — tests use parse_surface_expression directly.
use crate::coverage;
use crate::types::{
    generalize, instantiate_scheme, unify, InferState, Row, TyConDef, Type, TypeAlias, TypeEnv,
    TypeError, TypeScheme, Variance,
};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
mod typecheck_annot;
#[path = "typecheck_dict.rs"]
mod typecheck_dict;
// Special-case type refinement dispatchers for polymorphic builtins
#[path = "typecheck_special.rs"]
pub(crate) mod typecheck_special;
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

use typecheck_annot::*;
use typecheck_call::*;
use typecheck_diag::*;
use typecheck_dict::*;
use typecheck_match::*;
use typecheck_narrow::*;
use typecheck_special::*;

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
pub fn typecheck_surface_program_annotation_table(
    program: &SurfaceProgram,
) -> (
    Vec<TypeError>,
    TypeAnnotationTable,
    HashMap<crate::ast::Span, Type>,
) {
    let mut errors = Vec::new();
    let mut table = TypeAnnotationTable::new();
    let mut env = crate::imports::build_prelude_env();
    let mut state = InferState::new();
    // Seed with prelude instances so constraint checking works for dynamically registered classes.
    crate::imports::seed_infer_state_from_prelude_cache(&mut state);

    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        // Skip type-stage documents — they are handled separately by create_type_stage_env()
        // and should not be type-checked in the runtime pipeline.
        if doc.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

        let (new_env, doc_output_type, mut doc_errors) = typecheck_surface_document(
            doc,
            &env,
            &mut state,
            &mut table,
            &mut None, // annotation_table path — no span TypeMap needed
            &pipeline_type,
            &named_types,
        );
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking propagation.
        errors.append(&mut doc_errors);
        // Store named section type if this document has a name
        if let Some(ref name) = doc.name {
            named_types.insert(name.clone(), doc_output_type.clone());
        }
        // Update pipeline type for next document
        pipeline_type = doc_output_type;
    }

    (errors, table, state.expects_resolved)
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
pub fn typecheck_surface_program(
    program: &SurfaceProgram,
    parent_env: Rc<TypeEnv>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    let (errors, type_map, doc_map, scheme_map, diagnostics, _state, _env, _annotation_table) =
        typecheck_surface_program_with_env(program, parent_env, true, false);
    // type_map is now populated during inference (enable_scheme_map=true path).
    (errors, type_map, doc_map, scheme_map, diagnostics)
}

/// Type-check a `SurfaceProgram` with full control over scheme-map generation and the
/// prelude-load optimisation flag, returning all intermediate state including a
/// [`TypeAnnotationTable`] for the evaluator's lowering pass.
///
/// This is the native-Surface implementation — it walks `program.documents` directly
/// via [`typecheck_surface_document`] without any conversion through the old `File` AST.
/// The [`TypeAnnotationTable`] is populated directly during inference (keyed by `NodeId`
/// of the original `Arc<SurfaceNode>`) — no span-based correlation is needed.
///
/// # Parameters
///
/// - `program`: The surface AST to type-check.
/// - `parent_env`: Initial type environment (e.g., from `build_prelude_env()`).
/// - `enable_scheme_map`: When `true`, populates the [`SchemeMap`] for LSP hover.
/// - `in_prelude_load`: When `true`, skips instance-method body inference (prelude optimisation).
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics, infer_state, final_env, annotation_table)`
///
/// `type_map` and `doc_map` are currently empty — all callers discard them. If a caller
/// needs span-keyed types, use [`typecheck_surface_program`] instead.
#[allow(clippy::type_complexity)]
pub fn typecheck_surface_program_with_env(
    program: &SurfaceProgram,
    parent_env: Rc<TypeEnv>,
    enable_scheme_map: bool,
    in_prelude_load: bool,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
    InferState,
    Rc<TypeEnv>,
    TypeAnnotationTable,
) {
    let mut errors = Vec::new();
    let mut diagnostics = Vec::new();
    let mut env = parent_env;
    let mut state = InferState::new();
    // Seed with prelude instances so constraint checking works for dynamically registered classes.
    crate::imports::seed_infer_state_from_prelude_cache(&mut state);

    state.in_prelude_load = in_prelude_load;

    if enable_scheme_map {
        state.scheme_map = Some(SchemeMap::new());
    }

    let mut annotation_table = TypeAnnotationTable::new();
    // type_map_inner accumulates span→type for all sub-expressions (for LSP hover).
    // Populated when enable_scheme_map is true (i.e., LSP path), empty otherwise.
    let mut type_map_inner = TypeMap::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });

    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;

        // Skip type-stage documents — handled separately by create_type_stage_env().
        if doc.stage == Some(crate::ast::Stage::Type) {
            continue;
        }

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
        );
        env = new_env;
        // Collect all errors (type errors + advisory) without blocking env propagation.
        errors.append(&mut doc_errors);
        // Store named section type if this document has a name.
        if let Some(ref name) = doc.name {
            named_types.insert(name.clone(), doc_output_type.clone());
        }
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

    (
        errors,
        type_map_inner,
        doc_map,
        scheme_map,
        diagnostics,
        state,
        env,
        annotation_table,
    )
}

/// Type-check a single SurfaceDocument.
///
/// Mirrors the structure of `typecheck_document()` but operates on SurfaceItem instead of Expr.
/// Converts SurfaceNode back to Expr for type inference, then captures results in TypeAnnotationTable.
fn typecheck_surface_document(
    doc: &SurfaceDocument,
    parent_env: &Rc<TypeEnv>,
    state: &mut InferState,
    table: &mut TypeAnnotationTable,
    type_map: &mut Option<&mut TypeMap>,
    pipeline_type: &Type,
    named_types: &HashMap<String, Type>,
) -> (Rc<TypeEnv>, Type, Vec<TypeError>) {
    let mut errors = Vec::new();
    let mut advisory_errors: Vec<TypeError> = Vec::new();

    // Create environment with % and %name bindings
    let mut env = TypeEnv::with_parent(parent_env);

    // Bind % (pipeline variable) with the incoming type
    env.insert("%".to_string(), pipeline_type.clone());

    // Bind all named sections as %name
    for (name, ty) in named_types {
        env.insert(format!("%{}", name), ty.clone());
    }

    // Seed runtime-injected names that are not in the prelude type env.
    // %emit-channel: raw emit channel injected for all programs by eval-program (loader.llt).
    // Typed as Top to avoid T002 false positives in output formatters (e.g. none.llt).
    // See B-307: removed once %emit-channel gets proper stdlib-only scoping.
    env.insert("%emit-channel".to_string(), Type::Top);
    // materialize: builtin force function available at runtime but not re-exported by prelude.
    // none.llt calls [materialize %] — seed here to suppress T002 for formatters.
    env.insert(
        "materialize".to_string(),
        Type::Function {
            params: vec![(None, Type::Top)],
            ret: Box::new(Type::Top),
            variadic: false,
        },
    );

    let mut env = Rc::new(env);

    // Validate expects annotation if present (advisory errors)
    if let Some(ref expects_ann) = doc.expects {
        match resolve_annotation(
            &expects_ann.node,
            &env,
            expects_ann.span.clone(),
            state,
            &mut None,
            &mut None,
        ) {
            Ok(expected_type) => {
                // Store resolved type for eval.rs pipeline to use in TypeAssert
                state
                    .expects_resolved
                    .insert(expects_ann.span.clone(), expected_type.clone());

                let (pipeline_type_resolved, expected_type_resolved) = if state.subst.is_empty() {
                    (pipeline_type.clone(), expected_type.clone())
                } else {
                    (
                        state.subst.apply(pipeline_type),
                        state.subst.apply(&expected_type),
                    )
                };
                let passes = Type::is_subtype(
                    &pipeline_type_resolved,
                    &expected_type_resolved,
                    Some(&state.tycon_env),
                ) || ((contains_unknown_or_top(&pipeline_type_resolved)
                    || contains_unknown_or_top(&expected_type_resolved))
                    && Type::is_consistent(&pipeline_type_resolved, &expected_type_resolved));
                if !passes {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Pipeline input type {} does not satisfy expects contract {}",
                            pipeline_type_resolved, expected_type_resolved
                        ),
                        expects_ann.span.clone(),
                    ));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    // Process caps: declarations if present
    if let Some(ref caps_ann) = doc.caps {
        let mut env_mut = (*env).clone();
        for (cap_name, annotation) in &caps_ann.node {
            match resolve_annotation(
                annotation,
                &env,
                caps_ann.span.clone(),
                state,
                &mut None,
                &mut None,
            ) {
                Ok(cap_type) => {
                    env_mut.insert(format!("%{}", cap_name), cap_type);
                }
                Err(e) => {
                    errors.push(e);
                }
            }
        }
        env = Rc::new(env_mut);
    }

    // Process uses: pragma if present
    // Inject module-specific type signatures into the doc-local environment.
    // These bindings are available for type-checking THIS document's expressions,
    // but do NOT propagate to subsequent documents via result_env (module bindings
    // are doc-local, matching the runtime's `builtin_module()` injection behavior).
    if let Some(ref uses) = doc.uses {
        let mut env_mut = (*env).clone();
        for module_name in &uses.node {
            match crate::builtins::type_env_module(&module_name.node) {
                Some(module_env) => {
                    env_mut.merge(module_env);
                }
                None => {
                    // Emit a diagnostic for unknown native modules.
                    // The runtime will also catch this when it attempts to call
                    // builtin_module(), but we flag it statically here too.
                    errors.push(TypeError::new(
                        format!("unknown native module: {}", module_name.node),
                        module_name.span.clone(),
                    ));
                }
            }
        }
        env = Rc::new(env_mut);
    }

    let mut result_type = Type::Record(Row {
        fields: HashMap::new(),
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
                } => {
                    // Infer the class declaration to register it into state.class_env
                    match infer_class_decl_from_surface(
                        name,
                        params,
                        superclasses,
                        methods,
                        determines,
                        resolver,
                        *resolver_injective,
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
                    ) {
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
        // Validate output annotation even for empty document (advisory)
        if let Some(ref output_ann) = doc.output_type {
            match resolve_annotation(
                &output_ann.node,
                &env,
                output_ann.span.clone(),
                state,
                &mut None,
                &mut None,
            ) {
                Ok(expected_output) => {
                    let (result_type_resolved, expected_output_resolved) = if state.subst.is_empty()
                    {
                        (result_type.clone(), expected_output.clone())
                    } else {
                        (
                            state.subst.apply(&result_type),
                            state.subst.apply(&expected_output),
                        )
                    };
                    let passes = Type::is_subtype(
                        &result_type_resolved,
                        &expected_output_resolved,
                        Some(&state.tycon_env),
                    ) || ((contains_unknown_or_top(&result_type_resolved)
                        || contains_unknown_or_top(&expected_output_resolved))
                        && Type::is_consistent(&result_type_resolved, &expected_output_resolved));
                    if !passes {
                        advisory_errors.push(TypeError::new(
                            format!(
                                "Document output type {} does not match annotation {}",
                                result_type_resolved, expected_output_resolved
                            ),
                            output_ann.span.clone(),
                        ));
                    }
                }
                Err(e) => advisory_errors.push(e),
            }
        }

        let mut result_env = TypeEnv::with_parent(&env);
        result_env.insert("%".to_string(), result_type.clone());

        // Always return Ok with the partial env so callers always propagate env.
        advisory_errors.append(&mut errors);
        return (Rc::new(result_env), result_type, advisory_errors);
    }

    // Tracks schemes from the last dict expression so they can be threaded into result_env.
    // Mirrors typecheck_document's `last_dict_schemes` / `last_record_type` logic.
    let mut last_dict_schemes: Option<HashMap<String, TypeScheme>> = None;
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
                infer_dict(entries, &env, state, type_map, surface_node.span.clone());
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
                let mut new_env = TypeEnv::with_parent(&env);
                for (name, scheme) in &schemes {
                    new_env.insert_scheme(name.clone(), scheme.clone());
                }
                let mut alias_errs = register_type_aliases(surface_node, &mut new_env, &env, state);
                errors.append(&mut alias_errs);
                env = Rc::new(new_env);
            }
        } else {
            // Non-dict expression: infer at incremented level so type variables can be
            // properly generalized when threading Record fields as schemes into the env.
            // Mirrors typecheck_document lines 1041-1112.
            let enclosing_level = state.level;
            state.level += 1;

            match infer_surface_expr(surface_node, &env, state, type_map) {
                Ok(ty) => {
                    state.level = enclosing_level;
                    table.insert(node_id(surface_node), ty.clone());
                    // Merge nested TypeAssert entries from infer_surface_expr into the document-level table
                    for (nid, ty) in state.type_annotation_table.drain() {
                        table.insert(nid, ty);
                    }
                    if is_last {
                        result_type = ty.clone();
                        last_node = Some(Arc::clone(surface_node));
                        // Track last non-dict Record for cross-document field threading.
                        if matches!(&ty, Type::Record(_)) {
                            last_record_type = Some((ty, enclosing_level));
                        }
                    } else {
                        // Intermediate expressions must be record types.
                        // Mirrors typecheck_document line 1097.
                        match &ty {
                            Type::Record(Row { fields, .. }) => {
                                let mut new_env = TypeEnv::with_parent(&env);
                                for (name, field_ty) in fields {
                                    let scheme = generalize(enclosing_level, field_ty, state);
                                    new_env.insert_scheme(name.clone(), scheme);
                                }
                                let mut alias_errs =
                                    register_type_aliases(surface_node, &mut new_env, &env, state);
                                errors.append(&mut alias_errs);
                                env = Rc::new(new_env);
                            }
                            Type::Unknown => {} // Gradual: dict type inference failed, skip type alias registration
                            _ => {
                                errors.push(TypeError::not_a_record(&ty, surface_node.span.clone()))
                            }
                        }
                    }
                }
                Err(mut errs) => {
                    state.level = enclosing_level;
                    errors.append(&mut errs);
                    // Drain TypeAssert entries from failed expression to prevent leaking into next iteration
                    for (nid, ty) in state.type_annotation_table.drain() {
                        table.insert(nid, ty);
                    }
                }
            }
        }
    }

    // Validate output_type annotation if present (advisory)
    if let Some(ref output_ann) = doc.output_type {
        match resolve_annotation(
            &output_ann.node,
            &env,
            output_ann.span.clone(),
            state,
            &mut None,
            &mut None,
        ) {
            Ok(expected_output) => {
                let (result_type_resolved, expected_output_resolved) = if state.subst.is_empty() {
                    (result_type.clone(), expected_output.clone())
                } else {
                    (
                        state.subst.apply(&result_type),
                        state.subst.apply(&expected_output),
                    )
                };
                let passes = Type::is_subtype(
                    &result_type_resolved,
                    &expected_output_resolved,
                    Some(&state.tycon_env),
                ) || ((contains_unknown_or_top(&result_type_resolved)
                    || contains_unknown_or_top(&expected_output_resolved))
                    && Type::is_consistent(&result_type_resolved, &expected_output_resolved));
                if !passes {
                    advisory_errors.push(TypeError::new(
                        format!(
                            "Document output type {} does not match annotation {}",
                            result_type_resolved, expected_output_resolved
                        ),
                        output_ann.span.clone(),
                    ));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    // Build result_env: thread last-dict schemes or last-Record fields into cross-document scope.
    // Mirrors typecheck_document lines 1116-1148.
    //
    // IMPORTANT: result_env uses parent_env as its parent, NOT env.
    // This ensures doc-local bindings (%, %name, caps, and module-from-uses) do NOT
    // propagate to subsequent documents. Only explicitly exported bindings (last-dict
    // schemes, last-Record fields, %) are propagated via result_env.bindings.
    let mut result_env = TypeEnv::with_parent(parent_env);
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env.insert_scheme(name, scheme);
        }
    }
    // If the last expression was a non-dict Record, generalize and thread its fields.
    // Mirrors typecheck_document lines 1137-1142.
    if let Some((Type::Record(Row { fields, .. }), enclosing_level)) = last_record_type {
        for (name, field_ty) in fields {
            let scheme = generalize(enclosing_level, &field_ty, state);
            result_env.insert_scheme(name, scheme);
        }
    }
    if let Some(ref node) = last_node {
        let _ = register_type_aliases(node, &mut result_env, &env, state);
    }
    result_env.insert("%".to_string(), result_type.clone());

    // Always return the partial env — even if there are type errors.
    // This mirrors the pre-surface-migration behavior: the bridge path (typecheck_document)
    // always returned an env and propagated errors separately. Returning Err here caused
    // `typecheck_surface_program_with_env` to skip updating the accumulated env, which meant
    // the prelude's bindings (map, filter, keys, …) were never inserted into final_env.
    // Non-advisory errors are merged into advisory_errors so callers still collect them via
    // the third tuple element.
    advisory_errors.append(&mut errors);
    (Rc::new(result_env), result_type, advisory_errors)
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
/// type-error diagnostics, not pipeline-type threading.
///
/// - Walks `SurfaceItem::Expr` via `infer_surface_expr` (native SurfaceNode walk)
/// - Walks `SurfaceItem::Decl` for `TypeAlias`, `ClassDecl`, `InstanceDecl`
///   Pass 0c pre-scan via `SurfaceExpression::Decl` is already handled by `typecheck_surface_document`.
#[allow(dead_code)]
pub(crate) fn typecheck_surface_document_native(
    doc: &SurfaceDocument,
    state: &mut InferState,
    type_map: &mut TypeAnnotationTable,
    env: &TypeEnv,
) -> Result<(), Vec<TypeError>> {
    let parent_env = Rc::new(env.clone());
    let pipeline_type = Type::Record(Row {
        fields: HashMap::new(),
        tail: crate::type_def::RowTail::Empty,
    });
    let named_types = HashMap::new();

    let (_env, _ty, errors) = typecheck_surface_document(
        doc,
        &parent_env,
        state,
        type_map,
        &mut None, // no span TypeMap for this entry point
        &pipeline_type,
        &named_types,
    );
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

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
                    if let SurfaceExpression::Str(doc_string) = &doc_node.expr {
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
                        if let SurfaceExpression::Str(doc_string) = &doc_node.expr {
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
                        SurfaceExpression::Annotated { name, annotation } => {
                            if let Some(doc_node) = annotation.node.get_property("doc") {
                                if let SurfaceExpression::Str(doc_string) = &doc_node.expr {
                                    doc_map.insert(name.clone(), doc_string.clone());
                                }
                            }
                            Some(name.clone())
                        }
                        SurfaceExpression::VarRef { name, .. } => Some(name.clone()),
                        SurfaceExpression::Str(s) => Some(s.clone()),
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
        SurfaceExpression::DotAccess { expr, .. } => {
            extract_doc_from_surface_node(expr, doc_map, None);
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
fn collect_nominal_tags(ty: &Type) -> Vec<String> {
    match ty {
        Type::NominalVariant { tag, .. } => vec![tag.clone()],
        Type::Union(members) => members.iter().flat_map(collect_nominal_tags).collect(),
        // Intersection, Seq, Record, and all scalar types carry no nominal tags.
        _ => vec![],
    }
}

fn register_type_aliases(
    node: &Arc<SurfaceNode>,
    target_env: &mut TypeEnv,
    _resolve_env: &TypeEnv,
    state: &mut InferState,
) -> Vec<TypeError> {
    let mut errors = Vec::new();
    if let SurfaceExpression::Dict(entries) = &node.expr {
        // Two-pass registration to support recursive type aliases:
        // Pass 1: Pre-register all aliases with placeholder bodies (Unknown)
        // Pass 2: Resolve actual bodies (now recursive references can be looked up)

        // Pass 1: Collect alias names and pre-register placeholders
        // Each entry carries (alias_name, params, body_node, declaration_span).
        // params is Vec<(String, Option<Spanned<Annotation>>)>: (param_name, optional variance/class annotation).
        #[allow(clippy::type_complexity)]
        let mut alias_entries: Vec<(
            String,
            Vec<(String, Option<crate::ast::Spanned<crate::ast::Annotation>>)>,
            Arc<SurfaceNode>,
            Span,
        )> = Vec::new();
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let SurfaceExpression::Str(name) = &key.expr {
                    if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                        if let SurfaceDeclaration::TypeAlias { params, body } = decl_box.as_ref() {
                            alias_entries.push((
                                name.clone(),
                                params.clone(),
                                Arc::clone(body),
                                entry.node.value.span.clone(),
                            ));
                            // Pre-register with placeholder body
                            // Gradual: Pre-register with placeholder during forward-reference resolution
                            let param_names: Vec<String> =
                                params.iter().map(|(n, _)| n.clone()).collect();
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

        // Pass 2: Resolve actual bodies
        for (name, params, body_node, decl_span) in alias_entries {
            // [builtin-type "X"] detection (T-957): if the body is a `[builtin-type "X"]` call,
            // create a TyConDef with the builtin discriminant and skip normal body resolution.
            let builtin_type_discriminant: Option<String> = {
                match &body_node.expr {
                    SurfaceExpression::Call {
                        func,
                        args,
                        named_args,
                        implied: true,
                    } if named_args.is_empty() && args.len() == 1 => {
                        if let SurfaceExpression::VarRef {
                            name: func_name, ..
                        } = &func.expr
                        {
                            if func_name == "builtin-type" {
                                if let SurfaceExpression::Str(discriminant) = &args[0].expr {
                                    Some(discriminant.clone())
                                } else {
                                    None
                                }
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            };

            if let Some(discriminant) = builtin_type_discriminant {
                let n_params = params.len();
                let tycon_def = TyConDef {
                    variance: vec![Variance::Invariant; n_params],
                    constructors: vec![],
                    builtin_type: Some(discriminant),
                };
                target_env.insert_tycon_def(name.clone(), tycon_def.clone());
                state.tycon_env.insert(name.clone(), tycon_def);
                // Register a zero-param type alias for annotation resolution.
                target_env.insert_type_alias(
                    name.clone(),
                    TypeAlias {
                        params: vec![],
                        body: Type::TyCon(name.clone()),
                    },
                );
                continue; // Skip normal body resolution for builtin-type declarations.
            }

            // Use a fresh per-alias mapping so annotation names within one type
            // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
            // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
            // creates a distinct fresh var, breaking identity-function types.
            let mut alias_ann_map: HashMap<String, String> = HashMap::new();
            // Per-param declared variance (from @X annotation); None = infer from body.
            let mut declared_variances: Vec<Option<crate::type_def::Variance>> =
                vec![None; params.len()];
            // Pre-seed param names so they map to fresh TypeVars.
            for (idx, (p, ann)) in params.iter().enumerate() {
                let n = state.subst.name_counter.get();
                let fresh = format!("_t{}", n);
                state.subst.name_counter.set(n.saturating_add(1));
                state.levels.insert(fresh.clone(), state.level);
                alias_ann_map.insert(p.clone(), fresh.clone());
                // Process variance annotation if present (T-953).
                // ann is now Option<Spanned<Annotation>> — extract the Simple name for variance lookup.
                if let Some(ann_spanned) = ann {
                    let ann_name = match &ann_spanned.node {
                        crate::ast::Annotation::Simple(name) => Some(name.as_str()),
                        _ => None,
                    };
                    if let Some(name) = ann_name {
                        if let Some(v) = typecheck_annot::annotation_to_variance(name) {
                            declared_variances[idx] = Some(v);
                        }
                        // If annotation_to_variance returns None: could be a typeclass constraint.
                        // Class constraint processing is deferred to T-953 Phase B (class lookup).
                        // For now: if not a variance annotation, leave as None (will become Invariant).
                    }
                }
            }

            // Create a recursion guard for this alias resolution.
            // Seed with the current alias name so that any reference to `name` encountered
            // while resolving the body (including inside keyed record dicts like
            // `[value: Int  next: Name]`) is treated as a recursive back-reference and
            // returns a fresh TypeVar instead of the Unknown Pass-1 placeholder.
            let mut recursion_guard = HashSet::new();
            recursion_guard.insert(name.clone());

            // Set type_params_scope so that resolve_type_name enforces explicit param scoping
            // (T-951): inside a TypeAlias body, lowercase names are TypeVars only if declared.
            let param_names_set: HashSet<String> = params.iter().map(|(n, _)| n.clone()).collect();
            state.type_params_scope = Some(param_names_set);

            let body_resolve_result = resolve_type_expr_with_guard(
                &body_node,
                target_env, // Now resolve in target_env so recursive refs are visible
                state,
                &mut Some(&mut alias_ann_map),
                &mut None,
                &mut recursion_guard,
                &name,
                0,
            );

            // Always clear type_params_scope after body resolution, regardless of success/failure.
            state.type_params_scope = None;

            match body_resolve_result {
                Ok(alias_ty) => {
                    // W042: check each NominalVariant tag name in the resolved body against
                    // the global registry. Two separate [type ...] declarations with the same
                    // tag name are ambiguous at match sites — the second definition shadows the
                    // first in runtime pattern matching but both contribute to the type's union.
                    for tag in collect_nominal_tags(&alias_ty) {
                        // Copy the span out before any mutable borrow of state, so Rust's borrow
                        // checker sees the immutable borrow end before the push below.
                        let prev = state.registered_nominal_tags.get(tag.as_str()).cloned();
                        if let Some(prev_span) = prev {
                            state.diagnostics.push(crate::error::TypeDiagnostic {
                                message: format!(
                                    "duplicate nominal tag name '{tag}': previously defined at \
                                     {}:{} — tag names must be unique across [type ...] declarations",
                                    prev_span.start.line, prev_span.start.column,
                                ),
                                span: decl_span.clone(),
                                code: "W042",
                                level: crate::error::DiagnosticLevel::Warn,
                            });
                        } else {
                            state.registered_nominal_tags.insert(tag, decl_span.clone());
                        }
                    }

                    // Use the fresh names assigned to params
                    let remapped_params: Vec<String> = params
                        .iter()
                        .map(|(p, _)| alias_ann_map.get(p).cloned().unwrap())
                        .collect();
                    // Update with actual body
                    target_env.insert_type_alias(
                        name.clone(),
                        TypeAlias {
                            params: remapped_params.clone(),
                            body: alias_ty.clone(),
                        },
                    );

                    // Polarity analysis (T-952): infer variance for each param from the alias body.
                    // Then merge with declared variances from @X annotations (declared wins).
                    // The type_env used for TyCon lookup in nested App nodes.
                    let inferred_variances =
                        typecheck_annot::infer_variance(&alias_ty, &remapped_params, target_env);
                    let final_variances: Vec<Variance> = declared_variances
                        .iter()
                        .zip(inferred_variances.iter())
                        .map(|(decl, inferred)| decl.unwrap_or(*inferred))
                        .collect();

                    // Register TyConDef for TyCon identity checking and variance-directed subtyping.
                    let tycon_def = TyConDef {
                        variance: final_variances,
                        constructors: vec![],
                        builtin_type: None,
                    };
                    target_env.insert_tycon_def(name.clone(), tycon_def.clone());
                    state.tycon_env.insert(name.clone(), tycon_def);
                }
                Err(e) => errors.push(e),
            }
        }
    }
    errors
}

/// Type-check an `if` expression with path-sensitive narrowing.
fn infer_if(
    cond: &Arc<SurfaceNode>,
    then_expr: &Arc<SurfaceNode>,
    else_expr: &Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Infer the condition type (must be Bool)
    let _cond_ty = infer_surface_expr(cond, env, state, type_map)?;

    // Extract narrowings from the condition — walks SurfaceExpression natively.
    let narrowings = extract_narrowings(cond);

    // Fork the environment for the true branch
    let env_true = apply_narrowings(env, &narrowings, state);

    // Fork the environment for the false branch: apply negation narrowings (BAS false-branch).
    // For each TypeOf narrowing (e.g., [int? x] → x : Int in true branch),
    // the false branch narrows x to Negation(Int) — i.e., "definitely not Int".
    // This enables false-branch type refinement: if x : Int | Str and [int? x] fails,
    // then x : (Int | Str) & ~Int = Str in the false branch.
    let env_false = apply_negation_narrowings(env, &narrowings, state);

    // Infer the then and else branches in their respective environments
    let then_ty = infer_surface_expr(then_expr, &env_true, state, type_map)?;
    let else_ty = infer_surface_expr(else_expr, &env_false, state, type_map)?;

    // Form the union of both branch types and simplify (RDNF step 1b).
    // normalize_union deduplicates — if both branches return Int, the result is Int not Union([Int, Int]).
    let raw_union = Type::normalize_union(vec![then_ty, else_ty]);
    let result_ty = Type::simplify_type(raw_union);

    Ok(result_ty)
}

/// Type-infer a SurfaceNode expression.
///
/// Natively walks SurfaceExpression variants without converting to Expr.
/// Recursive calls use `infer_surface_expr` for child SurfaceNodes.
/// Bridge to check_* functions (via surface_node_to_expr) will be eliminated in Phase 4.
pub(crate) fn infer_surface_expr(
    node: &std::sync::Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    let result = match &node.expr {
        SurfaceExpression::Int(n) => Ok(Type::IntLiteral(*n)),
        SurfaceExpression::Float(_) => Ok(Type::Float),
        SurfaceExpression::Bool(_) => Ok(Type::Bool),
        SurfaceExpression::Str(s) => Ok(Type::StringLiteral(s.clone())),

        SurfaceExpression::VarRef { name, .. } => {
            if let Some(scheme) = env.get(name) {
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
                    scheme,
                    state.level,
                    state,
                    Some(name.as_str()),
                    Some(node.span.clone()),
                ))
            } else {
                let mut err = TypeError::undefined_variable(name, node.span.clone());
                if let Some(cause_span) = state.failed_bindings.get(name.as_str()) {
                    err.notes.push(format!(
                        "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                        cause_span.start.line, cause_span.start.column
                    ));
                }
                Err(vec![err])
            }
        }

        SurfaceExpression::Dict(entries) => {
            let (ty, _schemes, errs) = infer_dict(entries, env, state, type_map, node.span.clone());
            if errs.is_empty() {
                Ok(ty)
            } else {
                Err(errs)
            }
        }

        SurfaceExpression::DotAccess {
            expr: target,
            field,
        } => {
            // check_dot_access now takes Arc<SurfaceNode> directly
            check_dot_access(target, field, env, node.span.clone(), state, type_map)
        }

        SurfaceExpression::Pipe { .. } => {
            unreachable!("Pipe should be desugared before type checking")
        }

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
                return Ok(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                }));
            }

            let mut current_env = Rc::clone(env);

            for (i, seq_expr) in exprs.iter().enumerate() {
                let is_last = i == exprs.len() - 1;

                if is_last {
                    // Last expression: return its type
                    return infer_surface_expr(seq_expr, &current_env, state, type_map);
                }

                // Intermediate expression: infer and extract record bindings.
                // For Dict expressions, call infer_dict directly to get TypeSchemes
                // (infer_surface_expr discards them via TypeScheme::mono()).
                if let SurfaceExpression::Dict(entries) = &seq_expr.expr {
                    let (dict_ty, schemes, dict_errs) = infer_dict(
                        entries,
                        &current_env,
                        state,
                        type_map,
                        seq_expr.span.clone(),
                    );
                    if !dict_errs.is_empty() {
                        return Err(dict_errs);
                    }

                    if let Type::Record(_) = &dict_ty {
                        let mut child_env = TypeEnv::with_parent(&current_env);

                        // Insert schemes (preserving polymorphism) for entries
                        // that have generalized TypeSchemes from infer_dict.
                        // Fall back to mono() for any field in the Record type
                        // that doesn't have a scheme (e.g., auto-indexed entries).
                        for (field_name, scheme) in &schemes {
                            child_env.insert_scheme(field_name.clone(), scheme.clone());
                        }

                        current_env = Rc::new(child_env);
                    } else {
                        return Err(vec![TypeError::new(
                            format!(
                                "sequential expression requires intermediate expressions to be dicts, got {}",
                                dict_ty
                            ),
                            seq_expr.span.clone(),
                        )]);
                    }
                } else {
                    let enclosing_level = state.level;
                    let expr_ty = infer_surface_expr(seq_expr, &current_env, state, type_map)?;

                    // Extract record fields to extend the type environment.
                    // Generalize each field type at the enclosing level so that
                    // a call expression returning a polymorphic record (e.g. a
                    // function that returns `[id: fn [x@a] $x]`) preserves
                    // let-polymorphism for downstream bindings.  Without
                    // generalization, `id` would be inserted as a monomorphic
                    // entry and could only be used at a single type.
                    if let Type::Record(row) = expr_ty {
                        let mut child_env = TypeEnv::with_parent(&current_env);

                        for (field_name, field_ty) in &row.fields {
                            let scheme = generalize(enclosing_level, field_ty, state);
                            child_env.insert_scheme(field_name.clone(), scheme);
                        }

                        current_env = Rc::new(child_env);
                    }
                    // Non-record intermediate expressions (e.g., side-effect calls returning Top)
                    // contribute nothing to scope but are valid — runtime evaluates them for
                    // side effects. Only record-typed intermediates extend the type environment.
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
            // Special case: `if` is a type-level special form with path-sensitive narrowing
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                if name == "if" && args.len() == 3 && named_args.is_empty() {
                    return infer_if(&args[0], &args[1], &args[2], env, state, type_map);
                }

                // Special case: `get-in` is a type-level special form that unfolds into
                // repeated `get` calls for nested dict access.
                if name == "get-in" && named_args.is_empty() {
                    return check_get_in(args, named_args, env, node.span.clone(), state, type_map);
                }

                // Special case: `open` synthesizes a precise Handle(cap_row) return type when
                // capability flag arguments are statically known VarRefs (e.g., Readable, Writable).
                if name == "open" && named_args.is_empty() && args.len() >= 2 {
                    return check_open(args, env, node.span.clone(), state, type_map);
                }

                // Special case: `connect` synthesizes a precise return type based on transport variant.
                if name == "connect" && named_args.is_empty() && args.len() == 4 {
                    let _ = infer_surface_expr(func, env, state, type_map); // Record func type for LSP hover
                    return check_connect(args, env, node.span.clone(), state, type_map);
                }

                // Special case: `map` synthesizes a precise return type for Seq input with callback.
                if name == "map" && named_args.is_empty() && args.len() == 2 {
                    let _ = infer_surface_expr(func, env, state, type_map); // Record func type for LSP hover
                    return check_map(args, env, node.span.clone(), state, type_map);
                }

                // Special case: `builtin-map` is an alias for `map` — dispatch to check_map.
                if name == "builtin-map" && named_args.is_empty() && args.len() == 2 {
                    let _ = infer_surface_expr(func, env, state, type_map); // Record func type for LSP hover
                    return check_map(args, env, node.span.clone(), state, type_map);
                }

                // Special case: `tls-layer` preserves input handle's capability row.
                if name == "tls-layer" && named_args.is_empty() && args.len() == 3 {
                    let _ = infer_surface_expr(func, env, state, type_map); // Record func type for LSP hover
                    return check_tls_layer(args, env, node.span.clone(), state, type_map);
                }
            }

            // Special case: do-infer sentinel — inferred [do] form monad resolution.
            if let SurfaceExpression::DotAccess {
                expr: da_target,
                field: da_field,
            } = &func.expr
            {
                if let SurfaceExpression::VarRef { name, .. } = &da_target.expr {
                    if name.starts_with("ℊꜱʏᴍ⧼do-infer⧽") && named_args.is_empty() {
                        return check_do_infer(
                            da_field,
                            name,
                            args,
                            named_args,
                            env,
                            node.span.clone(),
                            state,
                            type_map,
                        );
                    }
                }
            }

            // Special case: if func is a VarRef to a polymorphic scheme, pass the scheme
            // directly to avoid double instantiation (VAR-POLY followed by CALL-POLY).
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                // Monomorphic recursion check (same logic as infer_expr)
                if state.current_function.as_ref() == Some(name) {
                    let fn_resolved_ty = env
                        .get(name)
                        .map(|scheme| state.subst.apply(&scheme.body))
                        .unwrap_or_else(|| state.fresh_type_var());

                    match &fn_resolved_ty {
                        Type::Function {
                            params,
                            ret,
                            variadic,
                        } => {
                            // Monomorphic recursion: the function's type is already known.
                            let variadic = *variadic;
                            let params = params.clone();
                            let ret = ret.clone();

                            let total_supplied = args.len() + named_args.len();
                            let min_required = if variadic && !params.is_empty() {
                                params.len() - 1
                            } else {
                                params.len()
                            };
                            if total_supplied < min_required
                                || (!variadic && total_supplied != params.len())
                            {
                                return Err(vec![TypeError::new(
                                    format!(
                                        "arity mismatch: expected {}{} argument(s), got {}",
                                        if variadic { "at least " } else { "" },
                                        min_required,
                                        total_supplied,
                                    ),
                                    node.span.clone(),
                                )]);
                            }

                            for (arg, (_param_name, param_ty)) in args.iter().zip(params.iter()) {
                                let arg_ty = infer_surface_expr(arg, env, state, type_map)?;
                                let mut subst = std::mem::take(&mut state.subst);
                                let unify_result =
                                    unify(&arg_ty, param_ty, &mut subst, state, arg.span.clone());
                                state.subst = subst;
                                if let Err(uerr) = unify_result {
                                    return Err(vec![uerr]);
                                }
                            }
                            for na in named_args {
                                let _ = infer_surface_expr(&na.node.value, env, state, type_map)?;
                            }
                            return Ok(state.subst.apply(&ret));
                        }
                        _ => {
                            // TypeVar or other non-Function type: allow speculatively.
                            for arg in args {
                                let _ = infer_surface_expr(arg, env, state, type_map)?;
                            }
                            for na in named_args {
                                let _ = infer_surface_expr(&na.node.value, env, state, type_map)?;
                            }
                            return Ok(state.fresh_type_var());
                        }
                    }
                }

                match env.get(name) {
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
                        // Polymorphic scheme: optimize by instantiating once in check_call_with_scheme
                        check_call_with_scheme(
                            scheme,
                            func.span.clone(),
                            Some(name.as_str()), // func_name for T013 origin diagnostics
                            args,
                            named_args,
                            env,
                            node.span.clone(),
                            state,
                            type_map,
                        )
                    }
                    Some(_) => {
                        // Monomorphic: use check_call (now takes SurfaceNode directly)
                        check_call(
                            func,
                            args,
                            named_args,
                            env,
                            node.span.clone(),
                            state,
                            type_map,
                        )
                    }
                    None => {
                        // Special handling for $proxy builtin: produces Type::Proxy
                        if name == "proxy" {
                            // Infer arguments for type map population
                            for arg in args {
                                let _ = infer_surface_expr(arg, env, state, type_map)?;
                            }
                            for na in named_args {
                                let _ = infer_surface_expr(&na.node.value, env, state, type_map)?;
                            }
                            Ok(Type::Proxy)
                        } else {
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
                }
            } else {
                // Non-VarRef func: use check_call with SurfaceNode directly
                check_call(
                    func,
                    args,
                    named_args,
                    env,
                    node.span.clone(),
                    state,
                    type_map,
                )
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
            infer_fn(
                return_ann,
                &params_converted,
                body,
                env,
                node.span.clone(),
                state,
                type_map,
            )
        }

        SurfaceExpression::TypeAssert {
            annotation,
            expr: inner,
        } => {
            // resolve_type_assert returns Ok(type) as the authoritative result.
            // Surface AST stores type information in TypeAnnotationTable (keyed by NodeId),
            // not in the AST node itself.
            let result =
                resolve_type_assert(annotation, inner, env, node.span.clone(), state, type_map);
            // Populate TypeAnnotationTable so lower.rs can produce CoreExpr::TypeAssert
            // with the statically-resolved type (or Type::Unknown for errors/macros).
            if let Ok(ref ty) = result {
                let id = crate::ast::node_id(node);
                state.type_annotation_table.insert(id, ty.clone());
            }
            result
        }

        SurfaceExpression::Annotated { name, annotation } => {
            // Create per-annotation-scope mappings for type and row variables.
            let mut ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
            let mut row_ann_mapping: Option<HashMap<String, String>> = Some(HashMap::new());
            let mut ann_mapping_opt = ann_mapping.as_mut();
            let mut row_ann_mapping_opt = row_ann_mapping.as_mut();
            resolve_annotated(
                name,
                annotation,
                env,
                node.span.clone(),
                state,
                &mut ann_mapping_opt,
                &mut row_ann_mapping_opt,
            )
            .map_err(|e| vec![e])
        }

        SurfaceExpression::Quote(_inner) => {
            // [quote expr] produces a dict representing the AST.
            Ok(Type::Record(Row {
                fields: HashMap::new(),
                tail: crate::type_def::RowTail::Empty,
            }))
        }

        SurfaceExpression::Unquote(inner) => {
            // [unquote expr] evaluates expr and returns its type.
            infer_surface_expr(inner, env, state, type_map)
        }

        SurfaceExpression::UnquoteSplice(inner) => {
            // [unquote-splice expr] expects expr to be a list (Dict with integer keys).
            let inner_ty = infer_surface_expr(inner, env, state, type_map)?;

            let expected_list_ty = Type::Record(Row {
                fields: HashMap::new(),
                tail: crate::type_def::RowTail::Empty,
            });

            let mut subst = std::mem::take(&mut state.subst);
            let result = unify(
                &inner_ty,
                &expected_list_ty,
                &mut subst,
                state,
                inner.span.clone(),
            );
            state.subst = subst;
            result.map_err(|_e| {
                vec![TypeError::new(
                    format!("unquote-splice expects a list (Dict), got {}", inner_ty),
                    inner.span.clone(),
                )]
            })?;

            Ok(expected_list_ty)
        }

        SurfaceExpression::Match { scrutinee, arms } => {
            // Infer scrutinee type — needed for exhaustiveness checking.
            let scrutinee_ty = infer_surface_expr(scrutinee, env, state, type_map)?;
            let scrutinee_ty = state.subst.apply(&scrutinee_ty);

            // I-Case3 (BAS match narrowing): maintain a "remaining scrutinee" type that
            // accumulates negations as Constructor/TypeAssert arms are processed.
            let mut remaining_scrutinee = scrutinee_ty.clone();
            let mut arm_result_types: Vec<Type> = Vec::new();

            for arm in arms {
                // Compute the arm-local scrutinee type from I-Case3.
                let arm_scrutinee_ty = match &arm.pattern.node {
                    Pattern::Constructor { tag, .. } => {
                        // When remaining_scrutinee is already a NominalVariant for this tag,
                        // use it directly — no intersection needed. The intersection (I-Case3)
                        // is only meaningful when narrowing a Union to one constructor.
                        // Intersecting NominalVariant("Circle",{r:Int}) with NominalVariant("Circle",{})
                        // loses the real field types from the original NominalVariant.
                        if matches!(&remaining_scrutinee, Type::NominalVariant { tag: t, .. } if t == tag)
                        {
                            remaining_scrutinee.clone()
                        } else {
                            let tag_ty = Type::NominalVariant {
                                tag: tag.clone(),
                                fields: crate::type_def::Row {
                                    fields: std::collections::HashMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            };
                            let members = vec![remaining_scrutinee.clone(), tag_ty];
                            Type::normalize_intersection(members)
                        }
                    }
                    Pattern::TypeAssertPending { .. } | Pattern::TypeAssert { .. } => {
                        // Type assertion patterns: treat as wildcard for scrutinee narrowing.
                        // Full narrowing is deferred to the elaboration pass.
                        remaining_scrutinee.clone()
                    }
                    Pattern::TypeTag(_) => {
                        // TypeTag patterns (Int:, Str:, etc.) — no scrutinee narrowing yet.
                        // Full type-dispatch narrowing is a future enhancement.
                        remaining_scrutinee.clone()
                    }
                    Pattern::Wildcard | Pattern::Variable(_) => remaining_scrutinee.clone(),
                    _ => scrutinee_ty.clone(),
                };

                // Elaboration pass: resolve TypeAssertPending → TypeAssert before collecting
                // pattern bindings. This ensures collect_pattern_bindings sees resolved_type
                // (not the raw annotation) when computing variable types for arm body checking.
                let elaborated_pat = elaborate_pattern(&arm.pattern.node, env, state)?;

                let mut pat_bindings: Vec<(String, Type)> = Vec::new();
                collect_pattern_bindings(&elaborated_pat, &arm_scrutinee_ty, &mut pat_bindings);
                let arm_env = if pat_bindings.is_empty() {
                    env.clone()
                } else {
                    let mut child = TypeEnv::with_parent(env);
                    for (name, ty) in pat_bindings {
                        child.insert(name, ty);
                    }
                    Rc::new(child)
                };

                // Type-check guard if present, and apply is: predicate narrowing.
                let arm_env = if let Some(guard) = &arm.guard {
                    let _guard_ty = infer_surface_expr(guard, &arm_env, state, type_map)?;
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
                let arm_ty = infer_surface_expr(&arm.body, &arm_env, state, type_map)?;
                arm_result_types.push(arm_ty);

                // Update remaining_scrutinee for subsequent arms (I-Case3 negation accumulation).
                if arm.guard.is_none() {
                    match &arm.pattern.node {
                        Pattern::Constructor { tag, .. } => {
                            let neg_tag = Type::Negation(Box::new(Type::NominalVariant {
                                tag: tag.clone(),
                                fields: crate::type_def::Row {
                                    fields: std::collections::HashMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                },
                            }));
                            remaining_scrutinee = Type::normalize_intersection(vec![
                                remaining_scrutinee.clone(),
                                neg_tag,
                            ]);
                        }
                        Pattern::Wildcard | Pattern::Variable(_) => {
                            remaining_scrutinee = Type::Never;
                        }
                        _ => {}
                    }
                }
            }

            // Exhaustiveness checking (Maranget 2007).
            let sig = match &scrutinee_ty {
                Type::Union(members) => {
                    coverage::ConstructorSignature::from_union(members, &state.tycon_env)
                }
                Type::NominalVariant { tag, fields } => Some(
                    coverage::ConstructorSignature::from_nominal_variant(tag, fields),
                ),
                Type::Bool => Some(coverage::ConstructorSignature {
                    constructors: vec![
                        (coverage::ConstructorTag::LiteralBool(true), 0),
                        (coverage::ConstructorTag::LiteralBool(false), 0),
                    ],
                }),
                // User-defined type constructors: look up TyConDef.constructors in tycon_env.
                // Handles both bare TyCon(name) scrutinees and App(TyCon(name), arg) forms
                // (e.g., `Seq[Int]` or a user-defined parameterized type).
                // TyConDef.constructors is empty until T-1003 populates it; in that case
                // we return None so coverage checking is skipped (no false non-exhaustiveness).
                ty @ Type::TyCon(_) | ty @ Type::App(_, _) => {
                    // Extract the root TyCon name from TyCon(name) or App(TyCon(name), _).
                    let tycon_name = match ty {
                        Type::TyCon(n) => Some(n.as_str()),
                        Type::App(f, _) => match f.as_ref() {
                            Type::TyCon(n) => Some(n.as_str()),
                            _ => None,
                        },
                        _ => None,
                    };
                    match tycon_name.and_then(|name| state.tycon_env.get(name)) {
                        Some(def) if !def.constructors.is_empty() => {
                            // Constructors are known — build a sig so coverage can be checked.
                            // Arity clamped to 0/1 (Pattern::Constructor has one binding slot).
                            let ctors = def
                                .constructors
                                .iter()
                                .map(|(tag, arity)| {
                                    let clamped = if *arity == 0 { 0 } else { 1 };
                                    (coverage::ConstructorTag::Variant(tag.clone()), clamped)
                                })
                                .collect();
                            Some(coverage::ConstructorSignature {
                                constructors: ctors,
                            })
                        }
                        // Constructors not yet populated (pending T-1003) or TyCon not found —
                        // skip coverage checking to avoid false non-exhaustiveness warnings.
                        _ => None,
                    }
                }
                _ => None,
            };

            if let Some(sig) = sig {
                let coverage_patterns: Vec<coverage::CoveragePattern> = arms
                    .iter()
                    .map(|arm| coverage::ast_pattern_to_coverage(&arm.pattern.node))
                    .collect();
                let has_guards: Vec<bool> = arms.iter().map(|arm| arm.guard.is_some()).collect();
                let result = coverage::check_coverage(
                    &coverage_patterns,
                    &sig,
                    &has_guards,
                );
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
                // Empty match is uninhabited — no arms to execute means no value can be produced.
                Type::Never
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
                } => infer_class_decl_from_surface(
                    name,
                    params,
                    superclasses,
                    methods,
                    determines,
                    resolver,
                    resolver_injective,
                    node.span.clone(),
                    env,
                    state,
                    type_map,
                ),
                SurfaceDeclaration::InstanceDecl {
                    ref class_name,
                    ref arms,
                } => infer_instance_decl_from_surface(
                    class_name,
                    arms,
                    node.span.clone(),
                    env,
                    state,
                    type_map,
                ),
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Extract body from decl_box directly to avoid borrow issues with **decl_box.
                    if let SurfaceDeclaration::TypeAlias { ref body, .. } = **decl_box {
                        expand_type_alias(body, env, state).map_err(|e| vec![e])
                    } else {
                        unreachable!()
                    }
                }
                SurfaceDeclaration::MacroDecl { .. } => Err(vec![TypeError::new(
                    "MacroDecl should be removed by expansion pass before typechecking (internal error)",
                    node.span.clone(),
                )]),
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

        SurfaceExpression::LetDecl { bindings } => {
            // LetDecl in value position is always an error (only valid in binding contexts).
            let msg = if bindings.len() > 1 {
                "multi-element [let ...] pattern not yet supported — use single binding".to_string()
            } else {
                "binding declaration [let ...] is not valid in expression position".to_string()
            };
            Err(vec![TypeError::new(msg, node.span.clone())])
        }

        SurfaceExpression::CaseArm { pattern, body } => {
            typecheck_case_arm(pattern, body, &Type::Unknown, env, state, type_map)
        }

        SurfaceExpression::Placeholder => {
            // Gradual: placeholder (`...`) is the explicit gradual typing escape hatch.
            Ok(Type::Unknown)
        }

        SurfaceExpression::Rest(_) => Err(vec![TypeError::new(
            "rest marker (...) is only valid inside type expressions",
            node.span.clone(),
        )]),

        SurfaceExpression::PatternDecl { .. } => {
            // PatternDecl should never appear in value positions (only in instance arms)
            Err(vec![TypeError::new(
                "pattern declaration is only valid in instance match arms",
                node.span.clone(),
            )])
        }

        SurfaceExpression::Error(span) => Err(vec![TypeError::new(
            format!(
                "syntax error at {}:{} (cannot typecheck error node)",
                span.start.line, span.start.column
            ),
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
            Err(_) => {
                map.insert(key, Type::Error);
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
    span: Span,
    env: &Rc<TypeEnv>,
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

    for method in methods {
        let _method_name = match &method.node.key {
            Some(key_node) => match &key_node.expr {
                SurfaceExpression::Str(s) => s.clone(),
                SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                _ => {
                    return Err(vec![TypeError::new(
                        "class method name must be a string or identifier",
                        key_node.span.clone(),
                    )]);
                }
            },
            None => {
                return Err(vec![TypeError::new(
                    "class method must have a name",
                    method.span.clone(),
                )]);
            }
        };
        // Method values in class declarations are type signatures ([fn params body] form).
        // If resolve_type_expr fails, use Error to cascade the failure rather than going gradual.
        // Dead code: current implementation doesn't use this result, but if it did, Error is
        // correct for failed inference (not Unknown which would activate gradual typing).
        let _method_type = resolve_type_expr(&method.node.value, env, state, &mut None, &mut None)
            .unwrap_or(crate::types::Type::Error);
    }

    let existing_param_kinds: std::collections::HashMap<String, Kind> = state
        .class_env
        .get(name)
        .map(|existing| existing.params.iter().cloned().collect())
        .unwrap_or_default();

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
            SurfaceExpression::Str(s) => Some(s.clone()),
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
    };

    state.class_env.insert(class_decl.clone());
    for (param_name, kind) in &class_decl.params {
        if *kind == Kind::Operator {
            state.kind_env.insert(param_name.clone(), Kind::Operator);
        }
    }

    Ok(Type::Record(Row {
        fields: HashMap::new(),
        tail: crate::type_def::RowTail::Empty,
    }))
}

/// Type alias for match arm type data (Surface version): (param_types, span, entries).
type SurfaceMatchArmData<'a> = (Vec<Type>, Span, &'a Vec<Spanned<crate::ast::SurfaceEntry>>);

/// Type-check an [instance ...] declaration from SurfaceDeclaration::InstanceDecl fields.
/// Called from infer_surface_expr (Decl arm) and typecheck_surface_document — no Expr bridge needed.
fn infer_instance_decl_from_surface(
    class_name: &str,
    arms: &[(Arc<SurfaceNode>, Vec<Spanned<crate::ast::SurfaceEntry>>)],
    span: Span,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(Type::Record(Row {
            fields: HashMap::new(),
            tail: crate::type_def::RowTail::Empty,
        }));
    }

    let (param_count, has_fds, fd_list, param_names) = {
        let class_decl = state.class_env.get(class_name).ok_or_else(|| {
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
            for (pattern_types, arm_span, _) in &arm_data {
                for &det_idx in determined_indices {
                    if !determining_indices.contains(&det_idx) {
                        if let Type::TypeVar(det_name, _) = &pattern_types[det_idx] {
                            let same_var_in_determining =
                                determining_indices.iter().any(|&det_pos| {
                                    matches!(&pattern_types[det_pos], Type::TypeVar(n, _) if n == det_name)
                                });
                            if !same_var_in_determining {
                                let param_name = param_names
                                    .get(det_idx)
                                    .map(|s| s.as_str())
                                    .unwrap_or("<unknown>");
                                return Err(vec![TypeError::new(
                                    format!(
                                        "coverage violation for class '{}': determined parameter '{}' (variable '{}') does not appear in any determining position",
                                        class_name, param_name, det_name
                                    ),
                                    arm_span.clone(),
                                )
                                .with_code("T016")]);
                            }
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
            Type::Record(Row {
                fields: pattern_types
                    .iter()
                    .enumerate()
                    .map(|(i, ty)| (i.to_string(), ty.clone()))
                    .collect(),
                tail: crate::type_def::RowTail::Empty,
            })
        };

        let mut method_types = HashMap::new();

        if !state.in_prelude_load {
            for method in *methods {
                let method_name = match &method.node.key {
                    Some(key_node) => match &key_node.expr {
                        SurfaceExpression::Str(s) => s.clone(),
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
                    infer_surface_expr(&method.node.value, env, state, type_map)?;
                method_types.insert(method_name, method_impl_type);
            }
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

        // Structural overlap check: detect instances whose head types unify even if
        // their string keys differ (e.g., `[Seq a]` vs `[Seq Int]`).
        // Clone the instance_env to satisfy the borrow checker — check_structural_overlap
        // takes &self (read-only) but state is also needed mutably for freshening.
        // This follows the same clone pattern used in resolve_instance callers.
        // The check is skipped during prelude loading (in_prelude_load) because prelude
        // instances are registered in a separate session before user code is type-checked,
        // and cross-session overlap is detected when user code re-declares prelude instances
        // (string-key dedup handles exact duplicates; structural overlap in the prelude itself
        // is an authoring error caught by the prelude test suite).
        if !state.in_prelude_load {
            let inst_env_snapshot = state.instance_env.clone();
            if let Err(msg) = inst_env_snapshot.check_structural_overlap(&instance_decl, state) {
                // Structural overlap is advisory (warning), not a blocking error.
                // User code may deliberately re-declare instances that the prelude defines
                // (e.g., `[instance Equatable [pattern [Int]]: ...]` in user code); the
                // InstanceEnv::insert deduplicates by key so no actual duplicate is registered.
                state.diagnostics.push(crate::error::TypeDiagnostic {
                    message: msg,
                    span: span.clone(),
                    code: "W043",
                    level: crate::error::DiagnosticLevel::Warn,
                });
            }
        }

        if let Err(msg) = state.instance_env.insert(instance_decl) {
            return Err(vec![TypeError::new(msg, span.clone())]);
        }
    }

    Ok(Type::Record(Row {
        fields: HashMap::new(),
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
fn contains_unknown_or_top(ty: &Type) -> bool {
    match ty {
        Type::Unknown | Type::Top => true,
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
        Type::App(f, arg) => contains_unknown_or_top(f) || contains_unknown_or_top(arg),
        Type::TyCon(_) => false,
        Type::Record(row) => row.fields.values().any(contains_unknown_or_top),
        Type::Union(members) => members.iter().any(contains_unknown_or_top),
        _ => false,
    }
}

/// This function is used at checking positions where the expected type is fully concrete
/// (no type variables): CALL-MONO arguments, concrete return annotations (no TypeVars), and TypeAssert.
pub(super) fn check_surface_expr(
    node: &Arc<SurfaceNode>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    // Lambda checking mode: when checking a function expression against a function type,
    // propagate expected parameter types into the lambda.
    // Only applies when expected type is fully concrete after applying state.subst
    // (no unbound type variables) per doc/06 §[CHECK-FN].
    if let SurfaceExpression::Fn {
        return_ann,
        params,
        body,
        ..
    } = &node.expr
    {
        if let Type::Function { .. } = expected {
            // Apply current substitution before checking for TypeVars — TypeVars that are
            // already bound in state.subst are effectively resolved. Without this, lambda
            // checking mode is blocked by TypeVars that have known types, falling through
            // to the less precise synthesize+subsume path.
            // Per Algorithm W (Damas & Milner, 1982): substitutions must be applied before
            // inspecting types, maintaining the substitution threading invariant.
            let resolved_expected = if state.subst.is_empty() {
                expected.clone()
            } else {
                state.subst.apply(expected)
            };
            // Only use lambda checking mode if expected type is fully concrete after applying subst
            if let Type::Function {
                params: ref expected_params,
                ret: ref expected_ret,
                variadic: ref expected_variadic,
            } = resolved_expected
            {
                // Skip lambda checking mode for the "any function" top type
                // (Function{params:[], ret:Top, variadic:true}).  That type is the top
                // of the function lattice and accepts any lambda — applying the arity
                // check (params.len() != 0) would incorrectly reject non-zero-param
                // lambdas like `fn [let x] x`.  Instead, fall through to the
                // synthesize+subsume path, which uses is_consistent_subtype to verify
                // that the concrete lambda type is ~<: any-function (always true).
                let is_any_function_expected = expected_params.is_empty() && *expected_variadic;
                if !resolved_expected.has_inference_vars() && !is_any_function_expected {
                    // Create a fresh annotation mapping for this lambda to prevent
                    // cross-contamination of type variables.
                    // Only allocate if any param has an annotation or there's a return annotation.
                    let has_annotations =
                        params.iter().any(|p| p.node.annotation.is_some()) || return_ann.is_some();
                    let mut ann_mapping = if has_annotations {
                        Some(HashMap::new())
                    } else {
                        None
                    };
                    let mut ann_mapping_opt = ann_mapping.as_mut();
                    // row_ann_mapping tracks named row variables per lambda scope (kinded separation).
                    let mut row_ann_mapping = if has_annotations {
                        Some(HashMap::new())
                    } else {
                        None
                    };
                    let mut row_ann_mapping_opt = row_ann_mapping.as_mut();

                    // Arity check
                    if params.len() != expected_params.len() {
                        return Err(vec![TypeError::new(
                            format!(
                                "arity mismatch: expected {} arguments, got {}",
                                expected_params.len(),
                                params.len()
                            ),
                            node.span.clone(),
                        )]);
                    }

                    // Build parameter types: use expected types for unannotated params.
                    // For annotated params, verify the annotation is compatible with the expected
                    // type: expected_ty must be a subtype of the annotation (contravariant check).
                    // Example: expected Fn(Int→...) but param declared @String → Int <: String is
                    // false → error, because callers will pass Int but the body expects String.
                    let param_types: Vec<Type> = params
                        .iter()
                        .zip(expected_params.iter())
                        .map(|(p, (_, expected_ty))| match &p.node.annotation {
                            Some(ann) => {
                                let resolved = resolve_annotation(
                                    &ann.node,
                                    env,
                                    ann.span.clone(),
                                    state,
                                    &mut ann_mapping_opt,
                                    &mut row_ann_mapping_opt,
                                )?;
                                // Contravariant check: expected param type must be subtype of annotation.
                                // When annotation contains type variables, use unification mode instead of
                                // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                                if resolved.has_inference_vars() {
                                    let mut subst = std::mem::take(&mut state.subst);
                                    let result =
                                        unify(expected_ty, &resolved, &mut subst, state, ann.span.clone());
                                    state.subst = subst;
                                    result.map_err(|_e| {
                                        TypeError::new(
                                            format!("parameter annotation {resolved} is more restrictive than required type {expected_ty}"),
                                            ann.span.clone()
                                        )
                                    })?;
                                } else {
                                    // Apply substitution before consistency check
                                    let (expected_ty_resolved, resolved_ty) = if state.subst.is_empty() {
                                        (expected_ty.clone(), resolved.clone())
                                    } else {
                                        (state.subst.apply(expected_ty), state.subst.apply(&resolved))
                                    };
                                    let sub_passes = Type::is_subtype(&expected_ty_resolved, &resolved_ty, Some(&state.tycon_env))
                                        || ((contains_unknown_or_top(&expected_ty_resolved)
                                            || contains_unknown_or_top(&resolved_ty))
                                            && Type::is_consistent(&expected_ty_resolved, &resolved_ty));
                                    if !sub_passes {
                                        return Err(TypeError::new(
                                            format!("parameter annotation {resolved_ty} is more restrictive than required type {expected_ty_resolved}"),
                                            ann.span.clone()
                                        ));
                                    }
                                }
                                Ok(resolved)
                            }
                            None => Ok(expected_ty.clone()),
                        })
                        .collect::<Result<_, _>>()
                        .map_err(|e| vec![e])?;

                    // Build function environment with parameter bindings
                    let mut fn_env = TypeEnv::with_parent(env);
                    for (param, ty) in params.iter().zip(param_types.iter()) {
                        if param.node.variadic {
                            // Variadic rest-parameter is typed as Seq(T) where T is a fresh type var.
                            // This allows type checking on operations over the rest sequence
                            // (e.g., [length xs] infers Seq(T) → Int).
                            let elem_var = state.fresh_type_var();
                            fn_env.insert(param.node.name.clone(), Type::seq(elem_var));
                        } else {
                            fn_env.insert(param.node.name.clone(), ty.clone());
                        }
                    }
                    let fn_env = Rc::new(fn_env);

                    // Check body against expected return type (or infer if no return annotation)
                    match return_ann {
                        Some(ann) => {
                            let declared = resolve_annotation(
                                &ann.node,
                                env,
                                ann.span.clone(),
                                state,
                                &mut ann_mapping_opt,
                                &mut row_ann_mapping_opt,
                            )
                            .map_err(|e| vec![e])?;
                            // Check that declared return type is compatible with expected.
                            // When declared contains type variables, use unification mode instead of
                            // is_subtype (C65 fix pattern: TypeVars only match reflexively in is_subtype).
                            if declared.has_inference_vars() {
                                let mut subst = std::mem::take(&mut state.subst);
                                let result = unify(
                                    &declared,
                                    expected_ret,
                                    &mut subst,
                                    state,
                                    ann.span.clone(),
                                );
                                state.subst = subst;
                                result.map_err(|_e| {
                                    vec![TypeError::type_mismatch(
                                        expected_ret,
                                        &declared,
                                        node.span.clone(),
                                    )]
                                })?;
                            } else {
                                // Apply substitution before consistency check
                                let (declared_resolved, expected_ret_resolved) =
                                    if state.subst.is_empty() {
                                        (declared.clone(), expected_ret.clone())
                                    } else {
                                        (
                                            state.subst.apply(&declared),
                                            Box::new(state.subst.apply(expected_ret)),
                                        )
                                    };
                                let sub_passes =
                                    Type::is_subtype(
                                        &declared_resolved,
                                        &expected_ret_resolved,
                                        Some(&state.tycon_env),
                                    ) || ((contains_unknown_or_top(&declared_resolved)
                                        || contains_unknown_or_top(&expected_ret_resolved))
                                        && Type::is_consistent(
                                            &declared_resolved,
                                            &expected_ret_resolved,
                                        ));
                                if !sub_passes {
                                    return Err(vec![TypeError::type_mismatch(
                                        &expected_ret_resolved,
                                        &declared_resolved,
                                        node.span.clone(),
                                    )]);
                                }
                            }
                            // Check body against declared return type
                            check_surface_expr(body, &declared, &fn_env, state, type_map)?;
                        }
                        None => {
                            // No return annotation: check body against expected return type.
                            // Apply state.subst to expected_ret — parameter inference
                            // (annotation unification above) may have added NEW bindings to
                            // state.subst that target TypeVars in expected_ret. The initial
                            // state.subst.apply at the guard resolved pre-existing bindings,
                            // but annotation unification can create new ones.
                            //
                            // Currently a no-op: the !has_inference_vars() guard ensures expected_ret
                            // (from the resolved type) has no TypeVars. Annotation unification
                            // binds annotation-fresh TypeVars, not expected_ret TypeVars. Retained
                            // as a safety net per Algorithm W substitution threading invariant.
                            let applied_ret = if state.subst.type_map.borrow().is_empty() {
                                *expected_ret.clone()
                            } else {
                                state.subst.apply(expected_ret)
                            };
                            check_surface_expr(body, &applied_ret, &fn_env, state, type_map)?;
                        }
                    }

                    // Record the function type in the type map — use the resolved
                    // (subst-applied) type so the map contains concrete types.
                    // In lambda checking mode, type_map records the expected function type
                    // (resolved_expected), not the synthesized type. This is correct
                    // bidirectional semantics for LSP hover: the lambda's type is determined
                    // by the checking context, not inferred from the body alone.
                    if let Some(ref mut map) = type_map {
                        let key = (node.span.start.offset, node.span.end.offset);
                        map.insert(key, resolved_expected.clone());
                    }

                    return Ok(());
                }
            }
        }
    }

    // Default: synthesize then check via infer_surface_expr
    let actual = infer_surface_expr(node, env, state, type_map)?;
    // Apply state.subst to both types before comparison — access-chain constraints
    // may have bound TypeVars in state.subst. Without substitution, the comparison
    // uses stale TypeVars.
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let (actual, expected_resolved) = if state.subst.is_empty() {
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
        // Expected type contains TypeVars — use unification to bind them.
        // This is the CALL-POLY path: the function is polymorphic, and we need to
        // instantiate type variables based on the argument types.
        let mut subst = std::mem::take(&mut state.subst);
        let result = unify(
            &actual,
            &expected_resolved,
            &mut subst,
            state,
            node.span.clone(),
        );
        state.subst = subst;
        result.map_err(|e| vec![e])
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
        let (actual_resolved, expected_final) = if state.subst.is_empty() {
            (actual.clone(), expected_resolved.clone())
        } else {
            (
                state.subst.apply(&actual),
                state.subst.apply(&expected_resolved),
            )
        };

        let passes = Type::is_subtype(&actual_resolved, &expected_final, Some(&state.tycon_env))
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

#[cfg(test)]
#[path = "typecheck_tests.rs"]
mod tests;
