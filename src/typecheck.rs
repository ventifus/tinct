//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

// Pattern import deleted (T-1750)
use crate::ast::{
    Span, Spanned, SurfaceDocument, SurfaceExpression, SurfaceItem, SurfaceNode, SurfaceProgram,
    TypeAnnotationTable,
};
use crate::env::Env;
use crate::error::TypeDiagnostic;
use crate::types::{generalize, InferState, Row, Type};

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

/// Map from source span `(start_line, start_col, end_line, end_col)` to inferred type.
/// Populated during type checking so LSP hover/diagnostics can look up types without
/// re-running inference.
pub type TypeMap = HashMap<(u32, u32, u32, u32), Type>;

// MatchArmData (old Expr-based) removed — replaced by SurfaceMatchArmData.

/// Grouped surface data for a `[class ...]` declaration.
///
/// Passed to [`infer_class_decl_from_surface`] instead of individual arguments,
/// eliminating the need for `#[allow(clippy::too_many_arguments)]`.
pub(crate) struct ClassDeclSurface<'a> {
    pub name: &'a str,
    pub params: &'a [String],
    pub superclasses: &'a [(String, String)],
    pub determines: &'a [Arc<SurfaceNode>],
    pub resolver: &'a Option<Arc<SurfaceNode>>,
    pub resolver_injective: bool,
    pub structural: &'a str,
    pub span: Span,
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
    type_stage_scope: Vec<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>>,
) -> (
    Vec<TypeDiagnostic>,
    Arc<RwLock<Env>>,
    crate::type_def::TyConEnv,
) {
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
    state.type_stage_scope = type_stage_scope;
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
    let (resolve_table, frames) = crate::resolve::resolve_surface_program(program, &[root_frame]);
    state.resolution_table = Arc::new(resolve_table);
    state.resolver_frames = frames;

    let mut annotation_table = TypeAnnotationTable::new();
    for doc_spanned in &program.documents {
        let doc = &doc_spanned.node;
        let (new_env, _, mut doc_errors) =
            process_document(doc, &env, &mut state, &mut annotation_table, &mut None).await;
        env = new_env;
        errors.append(&mut doc_errors);
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
/// chain are already visible — no filtering is needed. Duplicate insertion is safe
/// (insert_at_slot and insert_scheme_named_only are idempotent for same-name, same-value).
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
                guard.insert_scheme_named_only(name.to_string(), scheme.clone());
            }
        }
        for (name, slot) in &frame.extras {
            if let Some(ref scheme) = slot.scheme {
                guard.insert_scheme_named_only(name.clone(), scheme.clone());
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
        // Wire classes into type_stage_scope so that imported declarations from loaded
        // modules are visible to annotation resolution.
        if state.type_stage_scope.is_empty() {
            state
                .type_stage_scope
                .push(std::collections::HashMap::new());
        }
        for (name, decl) in &frame.classes {
            state.type_stage_scope[0]
                .entry(name.clone())
                .or_insert(crate::type_infer::TypeStageEntry::Class(decl.clone()));
        }
        // Wire tycon_defs into type_stage_scope so that imported type constructors from
        // loaded modules resolve correctly in annotations.
        for (name, _) in &frame.tycon_defs {
            state.type_stage_scope[0].entry(name.clone()).or_insert(
                crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::TyCon(
                    name.clone(),
                )),
            );
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
) -> (Arc<RwLock<Env>>, Type, Vec<TypeDiagnostic>) {
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
            let (_, schemes, _referenced, mut errs) =
                typecheck_cek::run_typecheck_dict(entries, &current_env, state, type_map).await;
            errors.append(&mut errs);
            for (nid, ty) in state.type_annotation_table.drain() {
                table.insert(nid, ty);
            }
            let mut new_env_inner = Env::with_parent(Arc::clone(&current_env));
            for (name, scheme) in &schemes {
                new_env_inner.insert_scheme_named_only(name.clone(), scheme.clone());
            }
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
                        new_env_inner.insert_scheme_named_only(name.clone(), scheme);
                    }
                    current_env = Arc::new(RwLock::new(new_env_inner));
                }
                Type::Unknown | Type::Any => {}
                _ => errors.push(TypeDiagnostic::error(
                    "type-error",
                    format!("expected record type, got {}", ty),
                    intermediate.span.clone(),
                )),
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
        let (dict_ty, schemes, _referenced, mut dict_errs) =
            typecheck_cek::run_typecheck_dict(entries, &current_env, state, type_map).await;
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

    // Check that the last expression's type is a Dict subtype.
    //
    // A document's last expression must be a record (the exports dict). The runtime enforces
    // the same constraint via builtin-eval, but catching it here produces a clearer error
    // message and fails fast at compile time rather than at evaluation time.
    //
    // Type::Dict(_):        ok — any dict (open, closed, uniform-tail) satisfies the constraint.
    // Type::Unknown:        ok — gradual escape hatch; cannot rule out a dict statically.
    // Type::Any:            ok — top type; cannot rule out a dict.
    // Type::Var(_, _):  ok — inference variable; deferred to the constraint solver.
    //                            is_subtype returns true for TypeVars (conservative approximation).
    // Type::Error(_):       ok — cascade sentinel; an upstream error already reported.
    // Everything else:      the type is known to be a non-dict → error.
    match &result_ty {
        Type::Dict(_) | Type::Unknown | Type::Any | Type::Var(_, _) | Type::Error(_) => {}
        _ => errors.push(TypeDiagnostic::error(
            "type-error",
            format!(
                "document last expression must be a record type, got {}",
                result_ty
            ),
            last_node.span.clone(),
        )),
    }

    // Build result_env with parent=parent_env (flat env chain invariant).
    let mut result_env_inner = Env::with_parent(Arc::clone(parent_env));
    if let Some(schemes) = last_dict_schemes {
        for (name, scheme) in schemes {
            result_env_inner.insert_scheme_named_only(name, scheme);
        }
    }
    if let Some((Type::Dict(Row { fields, .. }), enc_level)) = last_record_type {
        for (name, field_ty) in fields {
            let scheme = generalize(enc_level, &field_ty, state);
            result_env_inner.insert_scheme_named_only(name, scheme);
        }
    }
    (Arc::new(RwLock::new(result_env_inner)), result_ty, errors)
}

/// Pre-register type aliases in `target_env` before Pass 1 inference.
///
/// Map a resolved `Type` to the dispatch tag string used in instance binding names.
///
/// This mapping must match `extract_dispatch_tags` in `lower.rs`, which reads `@Annotation`
/// names from instance arm patterns.  Annotations are written as `@Integer`, `@Float`,
/// `@String`, `@Bytes`, `@SomeType` — the strings that appear in
/// `instance_binding_name` calls.
///
/// Returns `None` for:
/// - Unbound `TypeVar` (instance not yet determined).
/// - `Unknown` / `Top` / `Error` (gradual / lattice types that don't correspond to instances).
/// - Compound types that don't map to a single dispatch tag (records, functions, unions).
///
/// `IntLiteral`/`StringLiteral` are promoted to `"Integer"`/`"String"` because instance arms
/// are always annotated with the widened type (e.g., `@Integer`, never `@42`).
pub(crate) fn type_to_dispatch_tag(ty: &Type) -> Option<String> {
    match ty {
        Type::Int | Type::IntLiteral(_) => Some("Integer".to_string()),
        Type::Float => Some("Float".to_string()),
        Type::Str | Type::StringLiteral(_) => Some("String".to_string()),
        // Bytes is a direct variant (not TyCon("Bytes")).
        Type::Bytes => Some("Bytes".to_string()),
        // TyCon: map to the type name directly. Instance arm annotations must match the declared
        // type name exactly (e.g., @SomeType not a shortened alias) for dispatch tags to align.
        Type::TyCon(name) => Some(name.clone()),
        // Unresolved inference variables and gradual types cannot be dispatched.
        Type::Var(_, _) | Type::Unknown | Type::Any | Type::Error(_) => None,
        // Compound types don't correspond to single-param dispatch tags.
        _ => None,
    }
}

/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from process_document (via CEK Sequential) and typecheck_cek::run_typecheck_dict — no Expr bridge needed.
pub(crate) fn infer_class_decl_from_surface(
    decl: &ClassDeclSurface<'_>,
    state: &mut InferState,
) -> Result<Type, Vec<TypeDiagnostic>> {
    use crate::types::{ClassDecl, Kind};

    let ClassDeclSurface {
        name,
        params,
        superclasses,
        determines,
        resolver,
        resolver_injective,
        structural,
        span,
        ..
    } = decl;
    let span = span.clone();

    if name.is_empty() {
        return Err(vec![TypeDiagnostic::error(
            "type-error",
            "class declaration must have a name declared with [class [ClassName ...] ...]",
            span,
        )]);
    }

    // Method body validation is handled by dict inference (Pass 2, typecheck_dict.rs).
    // resolve_type_expr is async and cannot be called from sync infer_class_decl_from_surface.
    // method_signatures is populated as empty (matches existing ClassDecl construction sites).

    let existing_param_kinds: std::collections::HashMap<String, Kind> = {
        let env_guard = state.env.read().unwrap();
        env_guard
            .get_class(name)
            .map(|existing| existing.params.iter().cloned().collect())
            .unwrap_or_default()
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
                return Err(vec![TypeDiagnostic::error("type-error",
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
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
                    "resolver must be an identifier or string",
                    resolver_node.span.clone(),
                )]);
            }
        }
    } else {
        None
    };

    let structural_discharge = match *structural {
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
        resolver_injective: *resolver_injective,
        structural_discharge,
        method_signatures: vec![],
    };

    state.env.write().unwrap().insert_class(class_decl.clone());
    state.invalidate_env_caches();
    // Wire class declarations into type_stage_scope so resolve_type_head
    // can find class names via the scope chain. or_insert preserves type-stage
    // entries (type-stage has priority over runtime-declared classes).
    if state.type_stage_scope.is_empty() {
        state
            .type_stage_scope
            .push(std::collections::HashMap::new());
    }
    state.type_stage_scope[0]
        .entry(class_decl.name.clone())
        .or_insert(crate::type_infer::TypeStageEntry::Class(class_decl.clone()));
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
) -> Result<Type, Vec<TypeDiagnostic>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(Type::Dict(Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        }));
    }

    let (param_count, has_fds, fd_list, param_names) = {
        let class_decl = state
            .env
            .read()
            .unwrap()
            .get_class(class_name)
            .ok_or_else(|| {
                vec![TypeDiagnostic::error(
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
            return Err(vec![TypeDiagnostic::error(
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

        if pattern_types.iter().any(|ty| matches!(ty, Type::Unknown)) {
            return Err(vec![TypeDiagnostic::error("type-error",
                format!(
                    "instance pattern for class '{}' contains Unknown types — all pattern positions must have concrete type annotations (use a@Type syntax)",
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
                let error = TypeDiagnostic::error("type-error",
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

                    let determining_i: Vec<Type> = determining_indices
                        .iter()
                        .map(|&idx| types_i[idx].clone())
                        .collect();
                    let determining_j: Vec<Type> = determining_indices
                        .iter()
                        .map(|&idx| types_j[idx].clone())
                        .collect();

                    if types_can_unify(&determining_i, &determining_j, state).await? {
                        let determined_i: Vec<Type> = determined_indices
                            .iter()
                            .map(|&idx| types_i[idx].clone())
                            .collect();
                        let determined_j: Vec<Type> = determined_indices
                            .iter()
                            .map(|&idx| types_j[idx].clone())
                            .collect();

                        if !types_can_unify(&determined_i, &determined_j, state).await? {
                            let error = TypeDiagnostic::error("type-error",
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
        // the param_name → TypeStageEntry::Resolved(pattern_type) bindings. Pop it after all
        // methods in this arm are checked. This is a temporary scope — it does NOT leak into
        // other arms or the surrounding type environment.
        let param_type_frame: std::collections::HashMap<String, crate::type_infer::TypeStageEntry> = {
            let mut frame = std::collections::HashMap::new();
            for (name, ty) in param_names.iter().zip(pattern_types.iter()) {
                if !matches!(ty, Type::Var(..)) {
                    frame.insert(
                        name.clone(),
                        crate::type_infer::TypeStageEntry::Resolved(ty.clone()),
                    );
                }
            }
            frame
        };
        let pushed_frame = !param_type_frame.is_empty();
        if pushed_frame {
            state.type_stage_scope.insert(0, param_type_frame);
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
                        return Err(vec![TypeDiagnostic::error(
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
                    return Err(vec![TypeDiagnostic::error(
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
            {
                let class_method_sig: Option<Type> = {
                    let env_guard = state.env.read().unwrap();
                    env_guard.get_class(class_name).and_then(|cd| {
                        cd.method_signatures
                            .iter()
                            .find(|(n, _)| n == &method_name)
                            .map(|(_, ty)| ty.clone())
                    })
                };
                if let Some(sig) = class_method_sig {
                    // Build a temporary substitution: class param name → concrete pattern type.
                    // This is purely local — no state mutation.
                    let tmp_subst = crate::type_infer::Substitution::new();
                    for (pname, pty) in param_names.iter().zip(pattern_types.iter()) {
                        if !matches!(pty, Type::Var(..)) {
                            tmp_subst
                                .type_map
                                .borrow_mut()
                                .insert(pname.clone(), pty.clone());
                        }
                    }
                    let specialized = tmp_subst.apply(&sig);
                    // Extract fixed param types from the specialized Function type.
                    if let Type::Function {
                        params: fn_params, ..
                    } = specialized
                    {
                        let expected: Vec<Type> =
                            fn_params.iter().map(|(_, ty)| ty.clone()).collect();
                        if !expected.is_empty() {
                            state.expected_fn_params = Some(expected);
                        }
                    }
                }
            }

            let mut method_errors: Vec<TypeDiagnostic> = Vec::new();
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
            // All dict bindings go to extras via insert_scheme_named_only; infer_var_ref
            // finds them through get_extras_scheme when the slot lookup fails.
            env.write()
                .unwrap()
                .insert_scheme_named_only(binding_name, scheme);
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
        let mangled = format!(
            "ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽",
            instance_decl.class_name, instance_decl.instance_type
        );
        state
            .env
            .write()
            .unwrap()
            .insert_instance(mangled, instance_decl);
        state.invalidate_env_caches();
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
        Type::Var(_, _) => true,
        Type::Function { params, ret, .. } => {
            params.iter().any(|(_, t)| contains_unknown_or_top(t)) || contains_unknown_or_top(ret)
        }
        Type::App(f, a) => contains_unknown_or_top(f) || contains_unknown_or_top(a),
        Type::Dict(row) => row.fields.values().any(contains_unknown_or_top),
        Type::Union(members) => members.iter().any(contains_unknown_or_top),
        _ => false,
    }
}

#[cfg(test)]
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
/// default-value validation. Called from test code only.
pub(crate) async fn check_surface_expr(
    node: &Arc<SurfaceNode>,
    expected: &Type,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeDiagnostic>> {
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
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
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
    let mut local_errors: Vec<TypeDiagnostic> = Vec::new();
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
        if !Type::is_consistent_subtype(&actual, &expected_resolved, Some(&state.tycon_env)) {
            return Err(vec![TypeDiagnostic::error(
                "unification-failure",
                format!("cannot unify {} with {}", &expected_resolved, &actual),
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
            Err(vec![TypeDiagnostic::error(
                "unification-failure",
                format!("cannot unify {} with {}", &expected_final, &actual_resolved),
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
