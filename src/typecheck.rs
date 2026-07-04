//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner-style type variable
//! unification for polymorphic function calls.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::{
    Pattern, Span, Spanned, SurfaceDeclaration, SurfaceDocument, SurfaceExpression, SurfaceItem,
    SurfaceNode, SurfaceProgram,
};
// All production inference helpers now walk SurfaceExpression natively.
// No Expr bridge needed — tests use parse_surface_expression directly.
use crate::coverage;
use crate::type_errors::{
    ConsistencyViolation, CoverageViolation, GenericTypeError, InstanceContainsUnknown, NotARecord,
    OverlappingInstancePatterns, TypeErrorTyped, TypeSpanFrame, UndefinedVariable,
    UnificationFailure,
};
use crate::types::{
    constrain, generalize, instantiate_scheme, unify, Constraint, InferState, Row, TyConDef, Type,
    TypeEnv, TypeError, TypeScheme, Variance,
};

// Split modules — annotation resolution and dict inference
#[path = "typecheck_annot.rs"]
pub(crate) mod typecheck_annot;
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

/// Re-export SchemeMap from types for LSP consumers.
pub use crate::types::SchemeMap;

/// Evaluate a PropertyDict annotation to an IndexMap<String, Value> for TyConDef.annotation.
///
/// Type-level annotations (on type aliases, constructors, fields) are evaluated at typecheck time
/// and stored in TyConDef.annotation / field_annotations. Only literal values are supported:
/// strings, ints, floats, bools. Non-literal entries (type names, function expressions) are
/// silently skipped — they are type metadata, not runtime annotation values.
///
/// This is distinct from runtime annotation evaluation (eval_annotation_property_dict in eval_dict.rs),
/// which runs in the evaluator with full EvalContext and supports thunked expressions.
pub(crate) fn eval_type_annotation_property_dict(
    annotation: &crate::ast::Annotation,
) -> Option<indexmap::IndexMap<String, crate::value::Value>> {
    use crate::ast::SurfaceExpression;
    use crate::value::{string_val, Value};

    match annotation {
        crate::ast::Annotation::PropertyDict(entries) => {
            let mut result = indexmap::IndexMap::new();
            for entry in entries {
                // Extract key as a string
                let key_str = if let Some(key_node) = &entry.node.key {
                    match &key_node.expr {
                        SurfaceExpression::Str(s) => s.clone(),
                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                        SurfaceExpression::Int(n) => n.to_string(),
                        SurfaceExpression::U64(n) => n.to_string(),
                        _ => continue, // Skip complex keys
                    }
                } else {
                    // Auto-indexed entries are not meaningful for type-level annotations
                    continue;
                };

                // Extract value as a literal
                let value = match &entry.node.value.expr {
                    SurfaceExpression::Str(s) => string_val(s),
                    SurfaceExpression::Int(n) => Value::Int(*n),
                    SurfaceExpression::U64(n) => Value::U64(*n),
                    SurfaceExpression::Float(f) => Value::Float(*f),
                    _ => {
                        // Non-literal annotation values (type names, function expressions, etc.)
                        // are skipped — they are type metadata, not runtime annotation values.
                        continue;
                    }
                };

                result.insert(key_str, value);
            }
            // B-359: return None (not Some(empty_map)) when all entries were non-literal.
            // Callers use `.is_some()` to detect "has annotation" — Some(empty) would misfire.
            if result.is_empty() {
                None
            } else {
                Some(result)
            }
        }
        _ => None,
    }
}

/// Extract field annotations from a type alias body for TyConDef.field_annotations.
///
/// Walks the body SurfaceNode and collects `@[...]` annotations from named fields in constructor
/// declarations. For example, in `[Circle r@[required: true]: Float]`, extracts `{"r": {"required": true}}`.
///
/// Only processes literal annotation values (strings, ints, floats, bools) — type metadata
/// (type names, function expressions) is skipped, similar to `eval_type_annotation_property_dict`.
pub(crate) fn extract_field_annotations_from_body(
    body_node: &Arc<SurfaceNode>,
) -> indexmap::IndexMap<String, indexmap::IndexMap<String, crate::value::Value>> {
    use crate::ast::SurfaceExpression;
    use crate::value::Value;

    let mut result = indexmap::IndexMap::new();

    fn walk_expr(
        expr: &SurfaceExpression,
        result: &mut indexmap::IndexMap<String, indexmap::IndexMap<String, Value>>,
    ) {
        match expr {
            // Constructor call with named args: extract field annotations
            SurfaceExpression::Call { named_args, .. } => {
                for named_arg in named_args {
                    // Check if the field name is annotated
                    if let Some(ref field_annotation) = named_arg.node.annotation {
                        // Extract field name from the key
                        let field_name = named_arg.node.name.clone();

                        // Evaluate the annotation PropertyDict to literal values
                        if let Some(annotation_map) =
                            eval_type_annotation_property_dict(&field_annotation.node)
                        {
                            result.insert(field_name, annotation_map);
                        }
                    }
                }
            }
            // Dict with entries: check for annotated keys (record type fields)
            SurfaceExpression::Dict(entries) => {
                for entry in entries {
                    if let Some(ref key_node) = entry.node.key {
                        // Extract field name and annotation from annotated keys
                        match &key_node.expr {
                            // Annotated VarRef (annotation is now on VarRef directly).
                            SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
                                // Evaluate the annotation PropertyDict to literal values
                                if let Some(annotation_map) =
                                    eval_type_annotation_property_dict(&annotation.node)
                                {
                                    result.insert(name.clone(), annotation_map);
                                }
                            }
                            _ => {
                                // Non-annotated key: walk the value in case it's a nested type
                                walk_expr(&entry.node.value.expr, result);
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    walk_expr(&body_node.expr, &mut result);
    result
}

/// Attempt to interpret a SurfaceExpression as a compile-time literal constant value.
///
/// Returns `Some(Value)` for `Int`, `U64`, `Float`, and `Str` literals.
/// Returns `None` for type expressions, VarRef, Call, etc.
fn surface_expr_to_constant(expr: &crate::ast::SurfaceExpression) -> Option<crate::value::Value> {
    use crate::ast::SurfaceExpression;
    use crate::value::{string_val, Value};
    match expr {
        SurfaceExpression::Int(n) => Some(Value::Int(*n)),
        SurfaceExpression::U64(n) => Some(Value::U64(*n)),
        SurfaceExpression::Float(f) => Some(Value::Float(*f)),
        SurfaceExpression::Str(s) => Some(string_val(s)),
        _ => None,
    }
}

/// Extract compile-time constructor constants from a TypeAlias body for TyConDef.constructor_constants.
///
/// Walks the body SurfaceNode and collects `name: literal` entries from constructor declarations
/// (T-1357/T-1358). For example, in:
///   `DnsRcode: [type [NoError rcode: 0 description: "No Error"] [FormErr rcode: 1 ...]]`
/// extracts:
///   `{ "DnsRcode.NoError": { "rcode": Int(0), "description": Str("No Error") }, "DnsRcode.FormErr": ... }`
///
/// The disambiguation rule: named args (`field: value`) whose value is a literal → constants;
/// annotated positional args (`field@Type`) → payload fields (not collected here).
///
/// `type_name` is the unqualified type name (e.g., "DnsRcode"), used to form qualified tags.
pub(crate) fn extract_constructor_constants_from_body(
    body_node: &Arc<SurfaceNode>,
    type_name: &str,
) -> indexmap::IndexMap<String, indexmap::IndexMap<String, crate::value::Value>> {
    use crate::ast::SurfaceExpression;

    let mut result = indexmap::IndexMap::new();

    // Try to extract constants from a single constructor Call expression.
    // Inserts into `result` if this is a constructor with literal-valued named args.
    let try_extract_one = |expr: &SurfaceExpression,
                           result: &mut indexmap::IndexMap<
        String,
        indexmap::IndexMap<String, crate::value::Value>,
    >| {
        if let SurfaceExpression::Call {
            func, named_args, ..
        } = expr
        {
            if let SurfaceExpression::VarRef {
                name: ctor_name, ..
            } = &func.expr
            {
                if crate::eval::is_constructor_name(ctor_name) && !named_args.is_empty() {
                    let mut constants: indexmap::IndexMap<String, crate::value::Value> =
                        indexmap::IndexMap::new();
                    for named_arg in named_args {
                        if let Some(val) = surface_expr_to_constant(&named_arg.node.value.expr) {
                            constants.insert(named_arg.node.name.clone(), val);
                        }
                    }
                    if !constants.is_empty() {
                        let qualified = format!("{}.{}", type_name, ctor_name);
                        result.insert(qualified, constants);
                    }
                }
            }
        }
    };

    match &body_node.expr {
        SurfaceExpression::Dict(entries) => {
            // Multi-constructor body: each positional entry is a constructor.
            // Detect single-constructor-dict vs union of constructors.
            // single-ctor: first positional is uppercase VarRef AND there are keyed entries.
            let is_single_ctor_dict = entries.first().is_some_and(|first| {
                if first.node.key.is_some() {
                    return false;
                }
                let first_is_ctor = matches!(&first.node.value.expr,
                    SurfaceExpression::VarRef { name, .. }
                    if crate::eval::is_constructor_name(name));
                let has_keyed = entries[1..].iter().any(|e| e.node.key.is_some());
                first_is_ctor && has_keyed
            });
            if is_single_ctor_dict {
                // Single constructor in Dict form: `{ positional: VarRef(Ctor), keyed: literal, ... }`.
                // Collect literal-valued keyed entries as constants.
                if let Some(first) = entries.first() {
                    if let SurfaceExpression::VarRef {
                        name: ctor_name, ..
                    } = &first.node.value.expr
                    {
                        if crate::eval::is_constructor_name(ctor_name) {
                            let mut constants: indexmap::IndexMap<String, crate::value::Value> =
                                indexmap::IndexMap::new();
                            for entry in &entries[1..] {
                                if let Some(key_node) = &entry.node.key {
                                    let field_name = match &key_node.expr {
                                        SurfaceExpression::Str(s) => s.clone(),
                                        SurfaceExpression::VarRef { name, .. } => name.clone(),
                                        _ => continue,
                                    };
                                    if let Some(val) =
                                        surface_expr_to_constant(&entry.node.value.expr)
                                    {
                                        constants.insert(field_name, val);
                                    }
                                }
                            }
                            if !constants.is_empty() {
                                let qualified = format!("{}.{}", type_name, ctor_name);
                                result.insert(qualified, constants);
                            }
                        }
                    }
                }
            } else {
                // Union: each positional entry is a separate constructor expression.
                for entry in entries {
                    if entry.node.key.is_none() {
                        try_extract_one(&entry.node.value.expr, &mut result);
                    }
                }
            }
        }
        other => {
            // Single-constructor body (not wrapped in a Dict).
            try_extract_one(other, &mut result);
        }
    }

    result
}

/// Type-check a SurfaceProgram and write all type annotations inline on AST nodes.
///
/// This is the runtime-v2 entry point for type checking used by the eval pipeline.
/// Type annotations are written inline on AST nodes (TypeAnnotation OnceLock fields):
/// - TypeAssert.resolved_type: the resolved type for [@Type expr] annotations
/// - Pattern::TypeAssertPending.resolved: the resolved type for [@Type pat] patterns
/// - VarRef.call_dispatch: the instance binding name for typeclass method calls
///
/// The type environment is threaded across documents:
/// % bindings, %name bindings, dict-scoped let-generalization.
///
/// # Returns
///
/// Returns `(errors, expects_resolved, tycon_env)` where:
/// - `errors`: Type errors encountered during inference (advisory — evaluation proceeds)
/// - `expects_resolved`: Span → Type map for `--- expects: @Type` pipeline contracts
/// - `tycon_env`: Type constructor environment populated by `[type ...]` declarations.
///   Callers that run a subsequent evaluation pass and need runtime TypeAssert checking
///   (e.g. `@Boolean`, `@Color` guards) should call `ctx.set_tycon_env(tycon_env)` so that
///   `value_matches_type` can resolve user-defined nominal types. Without this, TypeAssert
///   on user-defined ADTs falls through to `None => false` at runtime. Callers that only
///   format or inspect the type-checked program (e.g. the formatter, test helpers) may
///   intentionally discard the returned `TyConEnv`.
pub async fn typecheck_surface_program_annotation_table(
    program: &SurfaceProgram,
) -> (
    Vec<TypeError>,
    HashMap<crate::ast::Span, Type>,
    crate::type_def::TyConEnv,
) {
    let mut errors = Vec::new();
    // get_builtin_core_type_env returns Arc<TypeEnv>; convert to Rc<TypeEnv> for the internal
    // type-checking chain (TypeEnv::with_parent uses Rc).
    let mut env: Rc<TypeEnv> = {
        let arc_env = crate::imports::get_builtin_core_type_env()
            .await
            .expect("S-898: builtin_core.llt type env unavailable — recursion during bootstrap is a bug, not a recoverable condition");
        Rc::new((*arc_env).clone())
    };
    let mut state = InferState::new();
    // Seed class_env and instance_env from TypeEnv (canonical source).
    {
        let env_class_env = env.build_class_env();
        for decl in env_class_env.iter_classes() {
            state.class_env.insert(decl.clone());
        }
        let env_instance_env = env.build_instance_env();
        for decl in env_instance_env.iter_instances() {
            let _ = state.instance_env.insert(decl.clone());
        }
    }

    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: indexmap::IndexMap::new(),
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
            &mut None, // annotation_table path — no span TypeMap needed
            &pipeline_type,
            &named_types,
        )
        .await;
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

    (errors, state.expects_resolved, state.tycon_env)
}

/// Type-check a `SurfaceProgram` with a given initial type environment.
///
/// This is the native-Surface implementation — it delegates to
/// [`typecheck_surface_program_with_env`] which walks `program.documents` directly via
/// [`typecheck_surface_document`] without any conversion through the old `File` AST.
/// The span-keyed [`TypeMap`] in the return tuple is always empty; callers that need
/// per-expression type information should use the [`TypeMap`] in the return tuple instead.
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics)`
///
/// The returned [`TypeMap`] is span-keyed and built during inference.
/// All type annotations are written inline on AST nodes (TypeAnnotation OnceLock fields).
pub async fn typecheck_surface_program(
    program: &SurfaceProgram,
    parent_env: Arc<TypeEnv>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
) {
    let (errors, type_map, doc_map, scheme_map, diagnostics, _state, _env) =
        typecheck_surface_program_with_env(program, parent_env, true, None, Default::default(), None).await;
    // type_map is now populated during inference (enable_scheme_map=true path).
    (errors, type_map, doc_map, scheme_map, diagnostics)
}

/// Type-check a `SurfaceProgram` with full control over scheme-map generation and the
/// prelude-load optimisation flag, returning all intermediate state.
///
/// This is the native-Surface implementation — it walks `program.documents` directly
/// via [`typecheck_surface_document`] without any conversion through the old `File` AST.
/// All type annotations are written inline on AST nodes (TypeAnnotation OnceLock fields).
///
/// # Parameters
///
/// - `program`: The surface AST to type-check.
/// - `parent_env`: Initial type environment (e.g., from `build_prelude_env()`).
/// - `enable_scheme_map`: When `true`, populates the [`SchemeMap`] for LSP hover.
///
/// # Returns
///
/// `(errors, type_map, doc_map, scheme_map, diagnostics, infer_state, final_env)`
///
/// `type_map` and `doc_map` are currently empty — all callers discard them. If a caller
/// needs span-keyed types, use [`typecheck_surface_program`] instead.
/// All type annotations are written inline on AST nodes (TypeAnnotation OnceLock fields).
#[allow(clippy::type_complexity)]
pub async fn typecheck_surface_program_with_env(
    program: &SurfaceProgram,
    parent_env: Arc<TypeEnv>,
    enable_scheme_map: bool,
    _resolution_table: Option<()>,
    instance_binding_slots: std::collections::HashMap<String, u32>,
    main_env: Option<Arc<std::sync::RwLock<crate::value::Environment>>>,
) -> (
    Vec<TypeError>,
    TypeMap,
    DocMap,
    SchemeMap,
    Vec<crate::error::TypeDiagnostic>,
    InferState,
    Arc<TypeEnv>,
) {
    let mut errors = Vec::new();
    let mut diagnostics = Vec::new();
    // Convert Arc<TypeEnv> to Rc<TypeEnv> for the internal type-checking chain,
    // which uses Rc<TypeEnv> throughout (TypeEnv::parent is Option<Rc<TypeEnv>>).
    let mut env: Rc<TypeEnv> = Rc::new((*parent_env).clone());
    let mut state = InferState::new();
    state.instance_binding_slots = instance_binding_slots;
    state.main_env = main_env;

    // Seed class_env and instance_env from TypeEnv. TypeEnv is the canonical persistent store;
    // InferState starts with empty ClassEnv/InstanceEnv (no pre-seeding). All class and instance
    // declarations flow here via the TypeEnv parent chain (prelude → builtin_core → user code).
    {
        let env_class_env = env.build_class_env();
        for decl in env_class_env.iter_classes() {
            state.class_env.insert(decl.clone());
        }
        let env_instance_env = env.build_instance_env();
        for decl in env_instance_env.iter_instances() {
            let _ = state.instance_env.insert(decl.clone());
        }
    }

    // B-345: Seed state.tycon_env from the parent TypeEnv so that [type ...] declarations
    // registered in previous type-check passes (e.g., earlier REPL turns) are visible
    // to the type checker for exhaustiveness checking, subtype checking, and variance-directed
    // subtyping in the current pass.
    {
        let mut inherited_tycon_defs = HashMap::new();
        env.collect_all_tycon_defs(&mut inherited_tycon_defs);
        for (name, def) in inherited_tycon_defs {
            state.tycon_env.entry(name).or_insert(def);
        }
    }

    if enable_scheme_map {
        state.scheme_map = Some(SchemeMap::new());
    }

    // type_map_inner accumulates span→type for all sub-expressions (for LSP hover).
    // Populated when enable_scheme_map is true (i.e., LSP path), empty otherwise.
    let mut type_map_inner = TypeMap::new();
    let mut named_types: HashMap<String, Type> = HashMap::new();
    let mut pipeline_type = Type::Record(Row {
        fields: indexmap::IndexMap::new(),
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
            &mut type_map_ref,
            &pipeline_type,
            &named_types,
        )
        .await;
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

    // Propagate any class/instance declarations registered during inference into the result env.
    // state.class_env/instance_env are derived working snapshots; new declarations made during
    // this session must be reflected in the returned TypeEnv so subsequent documents see them.
    {
        let result_env = Rc::make_mut(&mut env);
        // Classes: only propagate if not already in the parent chain (avoid re-inserting
        // parent-inherited classes whose declarations come from prelude type-checking).
        let parent_class_env = parent_env.build_class_env();
        for decl in state.class_env.iter_classes() {
            if parent_class_env.get(&decl.name).is_none() {
                result_env.insert_class(decl.clone());
            }
        }
        // Instances: propagate all from state. insert_instance is idempotent on exact
        // duplicates (same class + same determined-position types), so re-inserting
        // already-registered instances is safe and won't cause coherence errors.
        for decl in state.instance_env.iter_instances() {
            result_env.insert_instance(decl.clone());
        }
    }

    diagnostics.sort_by(|a, b| a.message.cmp(&b.message));

    (
        errors,
        type_map_inner,
        doc_map,
        scheme_map,
        diagnostics,
        state,
        // Convert Rc<TypeEnv> → Arc<TypeEnv> for the boundary: inference_env field requires Arc.
        Arc::new((*env).clone()),
    )
}

/// Type-check a single SurfaceDocument.
///
/// Mirrors the structure of `typecheck_document()` but operates on SurfaceItem instead of Expr.
/// Converts SurfaceNode back to Expr for type inference, writing results inline on AST nodes.
async fn typecheck_surface_document(
    doc: &SurfaceDocument,
    parent_env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
    pipeline_type: &Type,
    named_types: &HashMap<String, Type>,
) -> (Rc<TypeEnv>, Type, Vec<TypeError>) {
    let mut errors = Vec::new();
    let mut advisory_errors: Vec<TypeError> = Vec::new();
    // Top-level constraints accumulator for non-dict expressions in this document.
    // Scoped to the document: constraints from one non-dict expression do not leak to the next.
    let mut constraints: Vec<Constraint> = Vec::new();

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
    env.insert("%emit-channel".to_string(), Type::Any);
    // materialize: builtin force function available at runtime but not re-exported by prelude.
    // none.llt calls [materialize %] — seed here to suppress T002 for formatters.
    env.insert(
        "materialize".to_string(),
        Type::Function {
            params: vec![(None, Type::Any)],
            ret: Box::new(Type::Any),
            variadic: false,
            required_count: 1,
        },
    );

    let mut env = Rc::new(env);

    // Validate expects annotation if present (advisory errors)
    if let Some(ref expects_ann) = doc.expects {
        let mut expects_constraints: Vec<Constraint> = Vec::new();
        match resolve_annotation(
            &expects_ann.node,
            &env,
            expects_ann.span.clone(),
            state,
            &mut expects_constraints,
            &mut None,
            &mut None,
            None,
        )
        .await
        {
            Ok(expected_type) => {
                // Store resolved type for eval.rs pipeline to use in TypeAssert
                state
                    .expects_resolved
                    .insert(expects_ann.span.clone(), expected_type.clone());

                let (pipeline_type_resolved, expected_type_resolved) = if state.subst_is_empty() {
                    (pipeline_type.clone(), expected_type.clone())
                } else {
                    (
                        state.apply(pipeline_type),
                        state.apply(&expected_type),
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
                    advisory_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "Pipeline input type {} does not satisfy expects contract {}",
                            pipeline_type_resolved, expected_type_resolved
                        ),
                        span: expects_ann.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
            }
            Err(e) => advisory_errors.push(e),
        }
    }

    // Process caps: declarations if present
    if let Some(ref caps_ann) = doc.caps {
        let mut env_mut = (*env).clone();
        let mut cap_constraints: Vec<Constraint> = Vec::new();
        for (cap_name, annotation) in &caps_ann.node {
            match resolve_annotation(
                annotation,
                &env,
                caps_ann.span.clone(),
                state,
                &mut cap_constraints,
                &mut None,
                &mut None,
                None,
            )
            .await
            {
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
                    errors.push(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!("unknown native module: {}", module_name.node),
                        span: module_name.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }));
                }
            }
        }
        env = Rc::new(env_mut);
    }

    let mut result_type = Type::Record(Row {
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
                } => {
                    // Infer the class declaration to register it into state.class_env.
                    // Method schemes are pushed to state.pending_scheme_injections by the callee.
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
                    )
                    .await
                    {
                        Ok(_) => {
                            // Inline writes are already done — no drain needed.
                            // Drain pending method schemes and inject into env.
                            if !state.pending_scheme_injections.is_empty() {
                                let mut env_mut = (*env).clone();
                                for (method_name, scheme) in
                                    state.pending_scheme_injections.drain(..)
                                {
                                    env_mut.insert_scheme(method_name, scheme);
                                }
                                env = Rc::new(env_mut);
                            }
                        }
                        Err(mut errs) => {
                            errors.append(&mut errs);
                            // Inline writes are already done — no drain needed.
                            // Clear any partial injections on error to avoid leaking stale schemes.
                            state.pending_scheme_injections.clear();
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
                            // Inline writes are already done — no drain needed.
                        }
                        Err(mut errs) => {
                            errors.append(&mut errs);
                            // Inline writes are already done — no drain needed.
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
            let mut output_constraints: Vec<Constraint> = Vec::new();
            match resolve_annotation(
                &output_ann.node,
                &env,
                output_ann.span.clone(),
                state,
                &mut output_constraints,
                &mut None,
                &mut None,
                None,
            )
            .await
            {
                Ok(expected_output) => {
                    let (result_type_resolved, expected_output_resolved) = if state.subst_is_empty()
                    {
                        (result_type.clone(), expected_output.clone())
                    } else {
                        (
                            state.apply(&result_type),
                            state.apply(&expected_output),
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
                        advisory_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "Document output type {} does not match annotation {}",
                                result_type_resolved, expected_output_resolved
                            ),
                            span: output_ann.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
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
                infer_dict(entries, &env, state, type_map, surface_node.span.clone()).await;
            errors.append(&mut dict_errs);
            // Inline writes handled in infer_dict/infer_surface_expr — nothing to aggregate.
            if is_last {
                result_type = dict_ty;
                last_dict_schemes = Some(schemes);
                last_node = Some(Arc::clone(surface_node));
            } else {
                let mut new_env = TypeEnv::with_parent(&env);
                for (name, scheme) in &schemes {
                    new_env.insert_scheme(name.clone(), scheme.clone());
                }
                let mut alias_errs =
                    register_type_aliases(surface_node, &mut new_env, &env, state).await;
                errors.append(&mut alias_errs);
                env = Rc::new(new_env);
            }
        } else {
            // Non-dict expression: infer at incremented level so type variables can be
            // properly generalized when threading Record fields as schemes into the env.
            // Mirrors typecheck_document lines 1041-1112.
            let enclosing_level = state.level;
            state.level += 1;

            constraints.clear();
            match infer_surface_expr(surface_node, &env, state, &mut constraints, type_map).await {
                Ok(ty) => {
                    state.level = enclosing_level;
                    // Inline writes handled in infer_surface_expr — nothing to aggregate.
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
                                    let scheme =
                                        generalize(enclosing_level, field_ty, state, &constraints);
                                    new_env.insert_scheme(name.clone(), scheme);
                                }
                                let mut alias_errs =
                                    register_type_aliases(surface_node, &mut new_env, &env, state)
                                        .await;
                                errors.append(&mut alias_errs);
                                env = Rc::new(new_env);
                            }
                            Type::Unknown => {
                                // Gradual: dict type inference failed, skip type alias registration.
                                // Special case: if this is a static [include %libdir "X.llt"] call,
                                // inject the included module's exported bindings into scope so that
                                // subsequent expressions can reference the module's functions without
                                // false "undefined variable" warnings.
                                if let Some(module_env) =
                                    Box::pin(try_resolve_stdlib_include_env(surface_node)).await
                                {
                                    let mut new_env = TypeEnv::with_parent(&env);
                                    // Copy all module-exported bindings into the current scope.
                                    // These are the names that the bare include makes available.
                                    let mut module_names = std::collections::HashSet::new();
                                    module_env.collect_own_names(&mut module_names);
                                    for name in module_names {
                                        if let Some(scheme) = module_env.get_own(&name) {
                                            new_env.insert_scheme(name, scheme.clone());
                                        }
                                    }
                                    env = Rc::new(new_env);
                                }
                            }
                            _ => errors.push(TypeErrorTyped::NotARecord(NotARecord {
                                actual: ty.clone(),
                                span: surface_node.span.clone(),
                                notes: vec![],
                                call_stack: vec![],
                            })),
                        }
                    }
                }
                Err(mut errs) => {
                    state.level = enclosing_level;
                    errors.append(&mut errs);
                }
            }
        }
    }

    // Validate output_type annotation if present (advisory)
    if let Some(ref output_ann) = doc.output_type {
        let mut output_constraints2: Vec<Constraint> = Vec::new();
        match resolve_annotation(
            &output_ann.node,
            &env,
            output_ann.span.clone(),
            state,
            &mut output_constraints2,
            &mut None,
            &mut None,
            None,
        )
        .await
        {
            Ok(expected_output) => {
                let (result_type_resolved, expected_output_resolved) = if state.subst_is_empty() {
                    (result_type.clone(), expected_output.clone())
                } else {
                    (
                        state.apply(&result_type),
                        state.apply(&expected_output),
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
                    advisory_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                        message: format!(
                            "Document output type {} does not match annotation {}",
                            result_type_resolved, expected_output_resolved
                        ),
                        span: output_ann.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }));
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
            let scheme = generalize(enclosing_level, &field_ty, state, &constraints);
            result_env.insert_scheme(name, scheme);
        }
    }
    if let Some(ref node) = last_node {
        let _ = register_type_aliases(node, &mut result_env, &env, state).await;
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
                        // Annotated VarRef: annotation is now on VarRef directly.
                        SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
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
        SurfaceExpression::Field {
            expr: Some(inner), ..
        } => {
            extract_doc_from_surface_node(inner, doc_map, None);
        }
        SurfaceExpression::Field { expr: None, .. } => {}
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

/// Detect a static `[include %libdir "X.llt"]` call and return the module's exported
/// type environment, if available.
///
/// When the type checker encounters this pattern as an intermediate expression in a
/// sequential document, it needs to inject the included module's exported bindings into
/// scope so that subsequent expressions can reference them without false "undefined
/// variable" warnings.
///
/// Returns `None` if:
/// - The expression is not an include call.
/// - The cap variable is not `%libdir` (user-file includes via `%cwd` are not supported
///   in this path — they require file-system access via the full LSP include-resolution
///   pipeline in `imports::build_type_env_with_cap`).
/// - The path is not a string literal (dynamic includes are not statically resolvable).
/// - The module cannot be located or type-checked.
async fn try_resolve_stdlib_include_env(node: &Arc<SurfaceNode>) -> Option<Rc<TypeEnv>> {
    if let SurfaceExpression::Call { func, args, .. } = &node.expr {
        if let SurfaceExpression::VarRef { name, .. } = &func.expr {
            if name == "include" && args.len() == 2 {
                // Cap-qualified form: [include %cap "path"]
                if let SurfaceExpression::VarRef { name: cap_name, .. } = &args[0].expr {
                    if cap_name == "%libdir" {
                        if let SurfaceExpression::Str(module_path) = &args[1].expr {
                            return crate::imports::get_stdlib_module_type_env(module_path).await;
                        }
                    }
                }
            }
        }
    }
    None
}

/// Collect all NominalVariant tag names reachable from a type.
/// A type alias body such as `[Ok a] | [Err b]` resolves to `Union([NominalVariant("Ok",...),
/// NominalVariant("Err",...)])`. This function extracts `["Ok", "Err"]` so the caller can
/// check each tag against the `registered_nominal_tags` registry for W042 duplicates.
fn collect_nominal_tags(ty: &Type) -> Vec<String> {
    match ty {
        Type::NominalVariant { tag, .. } => vec![tag.clone()],
        Type::Union(members) => members.iter().flat_map(collect_nominal_tags).collect(),
        // Intersection, App, Record, and all scalar types carry no nominal tags.
        _ => vec![],
    }
}

/// Extract constructor information from a resolved type alias body.
/// For nominal ADTs (unions of NominalVariants), returns Vec<(qualified_tag, payload_arity)>.
/// - qualified_tag: "Result.Ok", "Maybe.Some", "Absent.Absent"
/// - payload_arity: 0 if fields.is_empty() (unit constructor), 1 otherwise (has payload)
///
/// Examples:
/// - `Result: [type [Ok a] [Error String]]` → `[("Result.Ok", 1), ("Result.Error", 1)]`
/// - `Maybe: [type [Some a] [None]]` → `[("Maybe.Some", 1), ("Maybe.None", 0)]`
/// - `Absent: [type Absent]` → `[("Absent.Absent", 0)]`
pub(crate) fn extract_constructors_from_type(ty: &Type, type_name: &str) -> Vec<(String, usize)> {
    match ty {
        Type::NominalVariant { tag, fields } => {
            let qualified_tag = format!("{}.{}", type_name, tag);
            let payload_arity = if fields.fields.is_empty() { 0 } else { 1 };
            vec![(qualified_tag, payload_arity)]
        }
        Type::Union(members) => members
            .iter()
            .flat_map(|m| extract_constructors_from_type(m, type_name))
            .collect(),
        _ => vec![],
    }
}

/// Build a constructor type from a NominalVariant present in a type alias body.
///
/// For each NominalVariant found in `alias_ty`:
/// - **Unit constructor** (no fields): the constructor IS the variant value, so the type
///   is `NominalVariant { tag: tag, fields: empty }` — a value, not a function (tag is the
///   unqualified form, e.g., "Ok" not "Result.Ok"; the qualified form is only the map key).
/// - **Field constructor** (has fields): the constructor is a named-argument function.
///   The type is `Function { params: [(Some(field_name), field_type), ...], ret: NominalVariant }`.
///   Fields are sorted by name for deterministic output (HashMap is unordered).
///
/// Returns a `Vec<(qualified_tag, Type)>` for each constructor found in the alias body.
pub(crate) fn extract_constructor_types(alias_ty: &Type, type_name: &str) -> Vec<(String, Type)> {
    match alias_ty {
        Type::NominalVariant { tag, fields } => {
            let qualified_tag = format!("{}.{}", type_name, tag);
            // Build the return type: the full NominalVariant with tag and fields.
            let ret_ty = Type::NominalVariant {
                tag: tag.clone(),
                fields: fields.clone(),
            };
            let ctor_ty = if fields.fields.is_empty() {
                // Unit constructor: the value IS the variant (no parameters needed).
                ret_ty
            } else {
                // Field constructor: a function that takes named arguments.
                // Sort fields by name for deterministic parameter order.
                let mut sorted_fields: Vec<(&String, &Type)> = fields.fields.iter().collect();
                sorted_fields.sort_by_key(|(name, _)| name.as_str());
                let params: Vec<(Option<String>, Type)> = sorted_fields
                    .into_iter()
                    .map(|(field_name, field_ty)| (Some(field_name.clone()), field_ty.clone()))
                    .collect();
                let required_count = params.len();
                Type::Function {
                    params,
                    ret: Box::new(ret_ty),
                    variadic: false,
                    required_count,
                }
            };
            vec![(qualified_tag, ctor_ty)]
        }
        Type::Union(members) => members
            .iter()
            .flat_map(|m| extract_constructor_types(m, type_name))
            .collect(),
        _ => vec![],
    }
}

async fn register_type_aliases(
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
        // Each entry carries (alias_name, params, body_node, declaration_span, type_annotation).
        // params is Vec<(String, Option<Spanned<Annotation>>)>: (param_name, optional variance/class annotation).
        // type_annotation is the @[...] annotation on the TypeName key (e.g. `JsonValue@[doc: "..."]`),
        // captured as a cloned Annotation for population into TyConDef.annotation in Pass 2.
        //
        // T-1052 produces `SurfaceExpression::Annotated { name, annotation }` key nodes
        // when a type alias has a top-level @[...] annotation (e.g. `JsonValue@[doc: "..."]: [type ...]`).
        // Pass 1 recognises Annotated keys and passes the annotation through. Pass 2 evaluates
        // the annotation in the type-stage evaluator (eval_type_stage_expr, T-1058, T-1053) and stores
        // the resulting Value dict in TyConDef.annotation.
        #[allow(clippy::type_complexity)]
        let mut alias_entries: Vec<(
            String,
            Vec<(String, Option<crate::ast::Spanned<crate::ast::Annotation>>)>,
            Arc<SurfaceNode>,
            Span,
            Option<crate::ast::Annotation>, // type-level @[...] annotation on the alias name
        )> = Vec::new();
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                // Recognise both plain `TypeName: [type ...]` and annotated `TypeName@[...]: [type ...]` keys.
                // The latter produces SurfaceExpression::Annotated { name, annotation } once T-1052 lands.
                let (alias_name, type_annotation): (
                    Option<String>,
                    Option<crate::ast::Annotation>,
                ) = match &key.expr {
                    SurfaceExpression::Str(name) => (Some(name.clone()), None),
                    // T-1052: `TypeName@[doc: "..." ...]` — annotated alias declaration key.
                    // Annotation is now on VarRef directly.
                    SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
                        (Some(name.clone()), Some(annotation.node.clone()))
                    }
                    // Plain VarRef key (no annotation).
                    SurfaceExpression::VarRef { name, .. } => (Some(name.clone()), None),
                    _ => (None, None),
                };
                if let Some(name) = alias_name {
                    if let SurfaceExpression::Decl(decl_box) = &entry.node.value.expr {
                        if let SurfaceDeclaration::TypeAlias { params, body } = decl_box.as_ref() {
                            alias_entries.push((
                                name.clone(),
                                params.clone(),
                                Arc::clone(body),
                                entry.node.value.span.clone(),
                                type_annotation,
                            ));
                            // Pre-register with placeholder body (T-1064: now in TyConDef)
                            // Gradual: Pre-register with placeholder during forward-reference resolution
                            let param_names: Vec<String> =
                                params.iter().map(|(n, _)| n.clone()).collect();
                            let placeholder_tycon = Arc::new(TyConDef {
                                params: param_names.clone(),
                                body: Type::Unknown,
                                constraints: vec![],
                                variance: vec![],
                                constructors: vec![],
                                builtin_type: None,
                                annotation: None,
                                field_annotations: indexmap::IndexMap::new(),
                                constructor_constants: indexmap::IndexMap::new(),
                            });
                            target_env.insert_tycon_def(name.clone(), placeholder_tycon);
                        }
                    }
                }
            }
        }

        // Pass 2: Resolve actual bodies
        for (name, params, body_node, decl_span, type_annotation) in alias_entries {
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
                // T-1122: Evaluate type-level annotation from the alias key (e.g. `Seq@[doc: "..."]`)
                // and store in TyConDef.annotation for runtime access via annotation-of builtin.
                let annotation = type_annotation
                    .as_ref()
                    .and_then(eval_type_annotation_property_dict);
                let tycon_def = Arc::new(TyConDef {
                    params: params.iter().map(|(s, _)| s.clone()).collect(),
                    body: Type::TyCon(name.clone()),
                    constraints: vec![],
                    variance: vec![Variance::Invariant; n_params],
                    constructors: vec![],
                    builtin_type: Some(discriminant),
                    annotation,
                    field_annotations: indexmap::IndexMap::new(),
                    constructor_constants: indexmap::IndexMap::new(),
                });
                target_env.insert_tycon_def(name.clone(), Arc::clone(&tycon_def));
                state.tycon_env.insert(name.clone(), tycon_def);
                continue; // Skip normal body resolution for builtin-type declarations (T-1064: type_aliases eliminated).
            }

            // Use a fresh per-alias mapping so annotation names within one type
            // alias expression (e.g., `a` in `[Fn@a [a]]`) consistently map to
            // the same fresh TypeVar. Without a mapping, every occurrence of `@a`
            // creates a distinct fresh var, breaking identity-function types.
            let mut alias_ann_map: HashMap<String, String> = HashMap::new();
            // Per-param declared variance (from @X annotation); None = infer from body.
            let mut declared_variances: Vec<Option<crate::type_def::Variance>> =
                vec![None; params.len()];
            // Class constraints from @ClassName annotations on params (T-1101).
            let mut alias_constraints: Vec<crate::type_class::Constraint> = Vec::new();
            // Pre-seed param names so they map to fresh TypeVars.
            for (idx, (p, ann)) in params.iter().enumerate() {
                let n = state.name_counter;
                let fresh = format!("_t{}", n);
                state.name_counter = n.saturating_add(1);
                state.set_level(fresh.clone(), state.level);
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
                        } else {
                            // Not a variance annotation — check if it's a class constraint (T-1101).
                            if let Some(class_decl) = state.class_env.get(name) {
                                // Build Constraint::Class for this param.
                                // Use the FRESH param name (from alias_ann_map), not the original.
                                let constraint = crate::type_class::Constraint::Class {
                                    class: std::sync::Arc::new(class_decl.clone()),
                                    vars: vec![crate::type_class::ConstraintArg::Var(
                                        fresh.clone(),
                                    )],
                                    origin_name: Some(std::sync::Arc::from(name.to_string())),
                                    origin_span: Some(ann_spanned.span.clone()),
                                };
                                alias_constraints.push(constraint);
                            } else {
                                // Unknown annotation — error
                                let err_msg = format!(
                                    "unknown type parameter annotation '@{}' — \
                                     expected a variance annotation (Covariant, Contravariant, Invariant, Phantom) \
                                     or a type class constraint",
                                    name
                                );
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: err_msg,
                                    span: ann_spanned.span.clone(),
                                    code: typecheck_diag::T021_UNKNOWN_TYPE_PARAM_ANNOTATION,
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                            }
                        }
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

            // Build the type_params_scope: maps declared param names → TypeVars.
            // Enforces explicit param scoping in TypeAlias bodies (T-1100 / T-951).
            let param_scope: HashMap<String, crate::types::Type> = params
                .iter()
                .filter_map(|(n, _)| {
                    alias_ann_map.get(n).map(|fresh| {
                        let level = state.get_level(fresh).unwrap_or(state.level);
                        (n.clone(), crate::types::Type::TypeVar(fresh.clone(), level))
                    })
                })
                .collect();

            let mut alias_constraints: Vec<Constraint> = Vec::new();
            let body_resolve_result = Box::pin(resolve_type_expr_with_guard(
                &body_node,
                target_env, // Now resolve in target_env so recursive refs are visible
                state,
                &mut alias_constraints,
                &mut Some(&mut alias_ann_map),
                &mut None,
                &mut recursion_guard,
                &name,
                0,
                Some((&param_scope, true)), // strict: TypeAlias rejects undeclared names
            ))
            .await;

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
                                code: typecheck_diag::W042_DUPLICATE_NOMINAL_TAG,
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
                    // (T-1064: type_aliases eliminated; body stored in TyConDef below)

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

                    // Extract constructor information from the resolved type body.
                    // For nominal ADTs (unions of NominalVariants), populate constructors.
                    // constructors: Vec<(qualified_tag, payload_arity)>
                    // - qualified_tag: "Result.Ok", "Maybe.Some", "Absent.Absent"
                    // - payload_arity: 0 if fields.is_empty() (unit constructor), 1 otherwise
                    let constructors = extract_constructors_from_type(&alias_ty, &name);

                    // Register TyConDef for TyCon identity checking and variance-directed subtyping.
                    // Arc::new preserves pointer identity so UNIFY-TYCON can detect cross-scope
                    // shadowing via Arc::ptr_eq when two [type Foo ...] decls exist in different
                    // scopes (B-343).
                    //
                    // T-1122 annotation population:
                    // `type_annotation` carries the @[...] annotation from the type alias key node
                    // (e.g. `JsonValue@[doc: "..." schema-id: "json-value"]: [type ...]`) and is
                    // evaluated to literal values (strings, ints, floats, bools) and stored in
                    // TyConDef.annotation for runtime access via annotation-of builtin.
                    //
                    // `field_annotations` are extracted from @[...] annotations on constructor fields
                    // (e.g., `[Circle r@[required: true]: Float]` → `{"r": {"required": true}}`).
                    let annotation = type_annotation
                        .as_ref()
                        .and_then(eval_type_annotation_property_dict);
                    let field_annotations = extract_field_annotations_from_body(&body_node);
                    let constructor_constants =
                        extract_constructor_constants_from_body(&body_node, &name);
                    let tycon_def = Arc::new(TyConDef {
                        params: remapped_params.clone(),
                        body: alias_ty.clone(),
                        constraints: alias_constraints.clone(),
                        variance: final_variances,
                        constructors: constructors.clone(),
                        builtin_type: None,
                        annotation,
                        field_annotations,
                        constructor_constants,
                    });
                    target_env.insert_tycon_def(name.clone(), Arc::clone(&tycon_def));
                    state.tycon_env.insert(name.clone(), tycon_def);

                    // T-1048: Register each constructor name with a precise type.
                    // Constructors are available via the runtime constructor dict (lower.rs T-1193)
                    // but the type checker must register them to avoid "undefined variable" warnings.
                    // Unit constructors (no fields): type is the NominalVariant value itself.
                    // Field constructors: type is Function{params: [(field_name, field_ty), ...], ret: NominalVariant}.
                    // Both qualified ("Result.Ok") and unqualified ("Ok") forms are registered:
                    // the desugar pass injects bare constructor names as runtime bindings, so the
                    // type checker must see the unqualified name to suppress "undefined variable" warnings.
                    //
                    // B-351: Constructor types must be registered as properly quantified TypeSchemes
                    // (not TypeScheme::mono) so that instantiate_scheme freshens the TypeVars on each
                    // call site. remapped_params contains the fresh TypeVar names (e.g., "_t0", "_t1")
                    // that alias_ann_map assigned to this alias's type parameters and that are baked
                    // into ctor_ty. With an empty type_vars list, two calls like [Ok value: 42] and
                    // [Ok value: "hello"] would share the same _t0 and spuriously unify Int with Str.
                    for (qualified_tag, ctor_ty) in extract_constructor_types(&alias_ty, &name) {
                        let scheme = TypeScheme {
                            type_vars: remapped_params.clone(),
                            constraints: vec![],
                            body: ctor_ty,
                            label_vars: vec![],
                            kind_vars: vec![],
                            doc: None,
                            inner_schemes: None,
                            param_narrowings: Vec::new(),
                        };
                        // Register the qualified form (e.g., "Result.Ok") for pattern matching
                        // and dot-access from the type dict (Color.Red, Option.Some, etc.).
                        target_env.insert_scheme(qualified_tag.clone(), scheme.clone());
                        // Also register the unqualified form (e.g., "Ok") so that bare constructor
                        // references in function bodies and expression position typecheck without
                        // "undefined variable" errors. The desugar pass injects bare constructor
                        // names as runtime bindings; the type checker must see the same unqualified
                        // name to avoid false positives (B-296, T-1048).
                        let bare = qualified_tag
                            .split_once('.')
                            .map(|(_, bare)| bare)
                            .unwrap_or(&qualified_tag);
                        if bare != qualified_tag {
                            target_env.insert_scheme(bare.to_string(), scheme);
                        }
                    }
                }
                Err(e) => errors.push(e),
            }
        }
    }
    errors
}

/// Type-check an `if` expression with path-sensitive narrowing.
async fn infer_if(
    cond: &Arc<SurfaceNode>,
    then_expr: &Arc<SurfaceNode>,
    else_expr: &Arc<SurfaceNode>,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    // Infer the condition type (must be Bool)
    let _cond_ty = infer_surface_expr(cond, env, state, constraints, type_map).await?;

    // Extract narrowings from the condition — walks SurfaceExpression natively.
    // Uses env to look up param_narrowings on called functions' TypeSchemes.
    let narrowings = extract_narrowings(cond, env);

    // Fork the environment for the true branch
    let env_true = apply_narrowings(env, &narrowings, state);

    // Fork the environment for the false branch: apply negation narrowings (BAS false-branch).
    // For each TypeOf narrowing (type predicate on x narrows x to T in the true branch),
    // the false branch narrows x to Negation(T) — i.e., "definitely not T".
    // This enables false-branch type refinement: if x : T1 | T2 and the predicate fails,
    // then x : (T1 | T2) & ~T1 = T2 in the false branch.
    let env_false = apply_negation_narrowings(env, &narrowings, state);

    // Infer the then and else branches in their respective environments
    let then_ty = infer_surface_expr(then_expr, &env_true, state, constraints, type_map).await?;
    let else_ty = infer_surface_expr(else_expr, &env_false, state, constraints, type_map).await?;

    // Form the union of both branch types and simplify (RDNF step 1b).
    // normalize_union deduplicates — if both branches return Int, the result is Int not Union([Int, Int]).
    let raw_union = Type::normalize_union(vec![then_ty, else_ty]);
    let result_ty = Type::simplify_type(raw_union);

    Ok(result_ty)
}

/// Walk an elaborated pattern alongside the original unelaborated pattern, writing
/// resolved types inline on `TypeAssertPending` nodes in the stored AST.
///
/// `elaborated` is the output of `elaborate_pattern`; `original` is the unelaborated
/// pattern from the stored AST (the one that will remain in the `SurfaceMatchArm`).
///
/// When we find a `TypeAssert` in `elaborated` that corresponds to a `TypeAssertPending`
/// in `original`, we write the resolved type directly to `original.resolved` (a TypeAnnotation
/// OnceLock). The lowerer reads this field to convert `TypeAssertPending → TypeAssert`.
///
/// Recursion mirrors the structure of `elaborate_pattern` exactly so the parallel walk
/// stays in sync. Sub-patterns (inner, Or branches, Constructor binding, Dict fields,
/// cons head/tail) are walked in the same order as `elaborate_pattern`.
fn record_pattern_elaborations(elaborated: &Pattern, original: &Pattern) {
    match (elaborated, original) {
        // The key case: TypeAssertPending was resolved to TypeAssert.
        // Write the resolved type inline on the original pattern's `resolved` field.
        (
            Pattern::TypeAssert {
                resolved_type,
                inner: elab_inner,
            },
            Pattern::TypeAssertPending {
                resolved: orig_resolved,
                inner: orig_inner,
                ..
            },
        ) => {
            orig_resolved.set(Some(resolved_type.clone()));
            // Recurse into inner sub-pattern if present.
            if let (Some(elab_box), Some(orig_box)) = (elab_inner, orig_inner) {
                record_pattern_elaborations(&elab_box.node, &orig_box.node);
            }
        }

        // TypeAssert in both: already elaborated — recurse into inner.
        (
            Pattern::TypeAssert {
                inner: elab_inner, ..
            },
            Pattern::TypeAssert {
                inner: orig_inner, ..
            },
        ) => {
            if let (Some(elab_box), Some(orig_box)) = (elab_inner, orig_inner) {
                record_pattern_elaborations(&elab_box.node, &orig_box.node);
            }
        }

        // Constructor: builtin-type constructors may become TypeAssert in elaborated.
        // Pattern::Constructor has no annotation span so nothing can be recorded in pattern_types.
        // Recurse into inner binding to catch nested TypeAssertPending patterns.
        (
            Pattern::TypeAssert {
                inner: elab_inner, ..
            },
            Pattern::Constructor {
                binding: orig_binding,
                ..
            },
        ) => {
            if let (Some(elab_box), Some(orig_box)) = (elab_inner, orig_binding) {
                record_pattern_elaborations(&elab_box.node, &orig_box.node);
            }
        }

        // Constructor keeping its form: elaborate inner binding.
        (
            Pattern::Constructor {
                binding: elab_binding,
                ..
            },
            Pattern::Constructor {
                binding: orig_binding,
                ..
            },
        ) => {
            if let (Some(elab_box), Some(orig_box)) = (elab_binding, orig_binding) {
                record_pattern_elaborations(&elab_box.node, &orig_box.node);
            }
        }

        // Or-pattern: walk each branch pair.
        (Pattern::Or(elab_branches), Pattern::Or(orig_branches)) => {
            for (elab_branch, orig_branch) in elab_branches.iter().zip(orig_branches.iter()) {
                record_pattern_elaborations(&elab_branch.node, &orig_branch.node);
            }
        }

        // Dict pattern: walk each field pair.
        (
            Pattern::Dict {
                fields: elab_fields,
                ..
            },
            Pattern::Dict {
                fields: orig_fields,
                ..
            },
        ) => {
            for ((_, elab_spanned), (_, orig_spanned)) in elab_fields.iter().zip(orig_fields.iter())
            {
                record_pattern_elaborations(&elab_spanned.node, &orig_spanned.node);
            }
        }

        // Leaf patterns (Variable, Wildcard, Literal, Pin): no sub-patterns.
        _ => {}
    }
}

/// Extract a `CoveragePattern` from a `SurfaceExpression::CaseArm` pattern expression.
///
/// In 3-arg `[case [let bindings] pattern body]` arms, the `SurfaceMatchArm.pattern` is a
/// `Pattern::Wildcard` sentinel — the actual structural pattern is the `pattern` field of
/// the `CaseArm` body expression. This function inspects that pattern expression and
/// extracts a constructor tag when possible, so exhaustiveness checking sees the real
/// pattern instead of a wildcard.
///
/// Returns `Some(CoveragePattern::Constructor { ... })` when the pattern expression is:
/// - A constructor call: `[Shape.Circle p]` → `Call { func: DotAccess, args: [p] }`
/// - A bare constructor reference: `Shape.Circle` → `DotAccess`
///
/// Returns `None` for wildcard/lowercase-name patterns (the caller should fall back to
/// `CoveragePattern::Wildcard`).
///
/// `tycon_env` is used to qualify unqualified constructor tags (e.g., `Circle` → `Shape.Circle`)
/// so they match the qualified forms in the constructor signature. This mirrors the B-341
/// qualification applied in `elaborate_pattern` for `Pattern::Constructor`.
fn extract_case_arm_coverage_pattern(
    pattern_expr: &SurfaceExpression,
    tycon_env: &crate::types::TyConEnv,
) -> Option<coverage::CoveragePattern> {
    // Determine whether the pattern head is a constructor call (with payload args)
    // or a bare constructor reference (tag-only match).
    //
    // Four cases:
    // 1. `[Shape.Circle p]`      → Call with positional args → constructor with payload
    // 2. `[Shape.Circle r: v]`   → Call with named args only → constructor with payload
    // 3. `[Shape.Circle]`        → Call with no args → constructor without payload (zero-arg call)
    // 4. `Shape.Circle`          → DotAccess or VarRef → constructor without payload (bare ref)
    let (head_expr, has_payload) = match pattern_expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } if !args.is_empty() || !named_args.is_empty() => (&func.expr, true),
        SurfaceExpression::Call { func, .. } => (&func.expr, false),
        other => (other, false),
    };

    // Extract the constructor tag string from the head expression.
    let tag = crate::ast::flatten_dot_access_to_tag(head_expr)?;

    // Only uppercase-initial names are constructor patterns; lowercase names are
    // variable bindings or wildcards, which should remain CoveragePattern::Wildcard.
    if !tag.chars().next().is_some_and(|c| c.is_uppercase()) {
        return None;
    }

    // Qualify unqualified constructor tags using tycon_env (mirrors B-341 in coverage.rs).
    // If the tag contains a dot, it's already qualified (e.g., "Shape.Circle").
    // If not, look up the TyCon that defines this constructor and qualify it.
    let qualified_tag = if tag.contains('.') {
        tag
    } else {
        coverage::qualify_nominal_tag(&tag, tycon_env)
    };

    // Produce a Constructor coverage pattern. Sub-patterns follow the same convention
    // as ast_pattern_to_coverage for Pattern::Constructor:
    // - binding: Some → vec![Wildcard] (payload slot present)
    // - binding: None → vec![]       (sentinel for normalize_constructor_arities to fix up)
    let sub_patterns = if has_payload {
        vec![coverage::CoveragePattern::Wildcard]
    } else {
        vec![]
    };

    Some(coverage::CoveragePattern::Constructor {
        tag: coverage::ConstructorTag::Variant(qualified_tag),
        sub_patterns,
    })
}

/// Type-infer a SurfaceNode expression.
///
/// Natively walks SurfaceExpression variants without converting to Expr.
/// Recursive calls use `infer_surface_expr` for child SurfaceNodes.
/// Bridge to check_* functions (via surface_node_to_expr) will be eliminated in Phase 4.
pub(crate) fn infer_surface_expr<'a>(
    node: &'a std::sync::Arc<SurfaceNode>,
    env: &'a Rc<TypeEnv>,
    state: &'a mut InferState,
    constraints: &'a mut Vec<Constraint>,
    type_map: &'a mut Option<&mut TypeMap>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Type, Vec<TypeError>>> + 'a>> {
    Box::pin(async move {
        let result = match &node.expr {
            SurfaceExpression::Int(n) => Ok(Type::IntLiteral(*n)),
            // U64 literals: no dedicated U64 type yet — treated as Int at the type level.
            // Values that exceed i64::MAX are only representable as U64 at runtime; the type
            // system does not distinguish signedness. Tracked for future Type::U64 addition.
            SurfaceExpression::U64(_) => Ok(Type::Int),
            SurfaceExpression::Float(_) => Ok(Type::Float),
            SurfaceExpression::Str(s) => Ok(Type::StringLiteral(s.clone())),

            // Annotated VarRef (name@Type): handled by the arm at the end of this match.
            // NOTE: this arm is for plain VarRef (no annotation) only.
            // The annotated arm below handles VarRef { annotation: Some(_) }.
            // Since Rust matches arms in order, we must use annotation: None here.
            SurfaceExpression::VarRef {
                name, resolution, annotation: None, ..
            } => {
                // Fast path: if the resolver wrote de Bruijn coordinates into the VarRef,
                // use slot-indexed lookup (O(1)) rather than the HashMap-based `get()`.
                // Falls back to name-based `get()` when coords are absent (bootstrap, LSP,
                // unresolved references) or when the slot lookup returns None (resolver
                // assigned coords before the type checker inserted the scheme).
                let scheme = if let Some(Some((level, slot))) = resolution.get() {
                    env.get_type_at(level, slot).or_else(|| env.get(name))
                } else {
                    env.get(name)
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
                        scheme,
                        state.level,
                        state,
                        constraints,
                        Some(name.as_str()),
                        Some(node.span.clone()),
                    ))
                } else {
                    let mut err = TypeErrorTyped::UndefinedVariable(UndefinedVariable {
                        name: name.to_string(),
                        span: node.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    });
                    if let Some(cause_span) = state.failed_bindings.get(name.as_str()) {
                        err.add_note(format!(
                        "  = note: `{name}` could not be defined because its definition at {}:{} failed type checking",
                        cause_span.start.line, cause_span.start.column
                    ));
                    }
                    Err(vec![err])
                }
            }

            SurfaceExpression::Dict(entries) => {
                let (ty, _schemes, errs) =
                    infer_dict(entries, env, state, type_map, node.span.clone()).await;
                if errs.is_empty() {
                    Ok(ty)
                } else {
                    Err(errs)
                }
            }

            SurfaceExpression::Field {
                expr: Some(target),
                field,
                ..
            } => {
                // check_dot_access now takes Arc<SurfaceNode> directly
                check_dot_access(
                    target,
                    field,
                    env,
                    node.span.clone(),
                    state,
                    constraints,
                    type_map,
                )
                .await
            }

            // Leading-dot form: `.name` with no preceding expression.
            // The resolver assigned parent-scope de Bruijn coordinates; at the type level,
            // treat this identically to a VarRef: look up the name in the type environment.
            // The type checker uses a flat env (not de Bruijn leveled), so it will find the
            // name at whatever scope level it exists. For type checking purposes, this is
            // correct — it will produce the right type for the outer binding regardless of
            // any shadowing by a same-dict key.
            SurfaceExpression::Field {
                expr: None,
                field: crate::ast::DotKey::Ident(name),
                ..
            } => {
                if let Some(scheme) = env.get(name) {
                    Ok(instantiate_scheme(
                        scheme,
                        state.level,
                        state,
                        constraints,
                        Some(name.as_str()),
                        Some(node.span.clone()),
                    ))
                } else {
                    // Not in scope at typecheck time — produce Unknown (gradual typing).
                    Ok(Type::Unknown)
                }
            }

            SurfaceExpression::Field {
                expr: None,
                field: crate::ast::DotKey::Int(_),
                ..
            } => {
                // `.N` with no target is rejected at parse time; this is a safety fallback.
                Ok(Type::Unknown)
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
                        fields: indexmap::IndexMap::new(),
                        tail: crate::type_def::RowTail::Empty,
                    }));
                }

                let mut current_env = Rc::clone(env);

                for (i, seq_expr) in exprs.iter().enumerate() {
                    let is_last = i == exprs.len() - 1;

                    if is_last {
                        // Last expression: return its type
                        return infer_surface_expr(
                            seq_expr,
                            &current_env,
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
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
                        )
                        .await;
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
                            return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "sequential expression requires intermediate expressions to be dicts, got {}",
                                dict_ty
                            ),
                            span: seq_expr.span.clone(),
                            notes: vec![], call_stack: vec![],
                        })]);
                        }
                    } else {
                        let enclosing_level = state.level;
                        let expr_ty = infer_surface_expr(
                            seq_expr,
                            &current_env,
                            state,
                            constraints,
                            type_map,
                        )
                        .await?;

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
                                let scheme =
                                    generalize(enclosing_level, field_ty, state, constraints);
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
                        return infer_if(
                            &args[0],
                            &args[1],
                            &args[2],
                            env,
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }

                    // Special case: `get-in` is a type-level special form that unfolds into
                    // repeated `get` calls for nested dict access.
                    if name == "get-in" && named_args.is_empty() {
                        return check_get_in(
                            args,
                            named_args,
                            env,
                            node.span.clone(),
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }

                    // Special case: `open` synthesizes a precise Handle(cap_row) return type when
                    // capability flag arguments are statically known VarRefs (e.g., Readable, Writable).
                    if name == "open" && named_args.is_empty() && args.len() >= 2 {
                        return check_open(
                            args,
                            env,
                            node.span.clone(),
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }

                    // Special case: `connect` synthesizes a precise return type based on transport variant.
                    if name == "connect" && named_args.is_empty() && args.len() == 4 {
                        let _ = infer_surface_expr(func, env, state, constraints, type_map).await; // Record func type for LSP hover
                        return check_connect(
                            args,
                            env,
                            node.span.clone(),
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }

                    // Special case: `map` infers argument types for side effects on type_map.
                    if name == "map" && named_args.is_empty() && args.len() == 2 {
                        let _ = infer_surface_expr(func, env, state, constraints, type_map).await; // Record func type for LSP hover
                        return check_map(
                            args,
                            env,
                            node.span.clone(),
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }

                    // Special case: `tls-layer` preserves input handle's capability row.
                    if name == "tls-layer" && named_args.is_empty() && args.len() == 3 {
                        let _ = infer_surface_expr(func, env, state, constraints, type_map).await; // Record func type for LSP hover
                        return check_tls_layer(
                            args,
                            env,
                            node.span.clone(),
                            state,
                            constraints,
                            type_map,
                        )
                        .await;
                    }
                }

                // Special case: do-infer sentinel — inferred [do] form monad resolution.
                // Only applies when DotAccess has a target (not a leading-dot).
                if let SurfaceExpression::Field {
                    expr: Some(da_target),
                    field: da_field,
                    ..
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
                                constraints,
                                type_map,
                            )
                            .await;
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
                            .map(|scheme| state.apply(&scheme.body))
                            .unwrap_or_else(|| state.fresh_type_var());

                        match &fn_resolved_ty {
                            Type::Function {
                                params,
                                ret,
                                variadic,
                                required_count,
                            } => {
                                // Monomorphic recursion: the function's type is already known.
                                let variadic = *variadic;
                                let required_count = *required_count;
                                let params = params.clone();
                                let ret = ret.clone();

                                let total_supplied = args.len() + named_args.len();
                                // B-349: resolved — see required_count field in Type::Function.
                                // min_required is the number of params without default values.
                                // For variadic functions, the last (variadic) param is never required.
                                let min_required = if variadic && !params.is_empty() {
                                    required_count.saturating_sub(1)
                                } else {
                                    required_count
                                };
                                if total_supplied < min_required
                                    || (!variadic && total_supplied > params.len())
                                {
                                    return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                                        message: format!(
                                            "arity mismatch: expected {}{} argument(s), got {}",
                                            if variadic { "at least " } else { "" },
                                            min_required,
                                            total_supplied,
                                        ),
                                        span: node.span.clone(),
                                        notes: vec![],
                                        call_stack: vec![],
                                    })]);
                                }

                                for (arg, (_param_name, param_ty)) in args.iter().zip(params.iter())
                                {
                                    let arg_ty =
                                        infer_surface_expr(arg, env, state, constraints, type_map)
                                            .await?;
                                    // Argument-passing is a subtype relationship: arg_ty <: param_ty.
                                    // Use constrain() (directional) rather than unify() (symmetric) so
                                    // that C-Var1/2 fire with the correct polarity. When param_ty is
                                    // Union([τ, α]), C-Var1 rewrites to arg_ty & ~τ ≤ α — the TypeVar
                                    // in the param absorbs what the argument provides beyond τ.
                                    //
                                    // Save bounds before constrain(): if constrain() fails internally
                                    // after C-Var1/2 has already pushed a bound, the partial bound
                                    // must not leak into the error recovery state (SOUND-2).
                                    let saved_bounds = state.bounds.clone();
                                    let constrain_result = Box::pin(constrain(
                                        &arg_ty,
                                        param_ty,
                                        state,
                                        constraints,
                                        arg.span.clone(),
                                    ))
                                    .await;
                                    if let Err(uerr) = constrain_result {
                                        state.bounds = saved_bounds;
                                        return Err(vec![uerr]);
                                    }
                                }
                                for na in named_args {
                                    let _ = infer_surface_expr(
                                        &na.node.value,
                                        env,
                                        state,
                                        constraints,
                                        type_map,
                                    )
                                    .await?;
                                }
                                return Ok(state.apply(&ret));
                            }
                            _ => {
                                // TypeVar or other non-Function type: allow speculatively.
                                for arg in args {
                                    let _ =
                                        infer_surface_expr(arg, env, state, constraints, type_map)
                                            .await?;
                                }
                                for na in named_args {
                                    let _ = infer_surface_expr(
                                        &na.node.value,
                                        env,
                                        state,
                                        constraints,
                                        type_map,
                                    )
                                    .await?;
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
                            // Snapshot the substitution before call inference so we can detect
                            // which type vars were resolved by this call (for call_dispatch).
                            let constraints_before = constraints.len();
                            // Polymorphic scheme: optimize by instantiating once in check_call_with_scheme
                            let call_result = check_call_with_scheme(
                                scheme,
                                func.span.clone(),
                                Some(name.as_str()), // func_name for T013 origin diagnostics
                                args,
                                named_args,
                                env,
                                node.span.clone(),
                                state,
                                constraints,
                                type_map,
                            )
                            .await;

                            // Compile-time instance dispatch: if this call resolved a typeclass
                            // method (scheme has exactly one Class constraint that is now ground),
                            // record the instance binding name in call_dispatch so lower.rs can
                            // rewrite the call to the instance binding directly.
                            //
                            // We look at the NEW constraints added by check_call_with_scheme
                            // (from constraints_before onwards). When all constraint vars are
                            // resolved in state.subst to concrete types, the instance binding
                            // name is determinable.
                            if call_result.is_ok() {
                                // Find the class constraints introduced by this call.
                                // When all constraint type vars are resolved to concrete types,
                                // record the pre-computed instance binding name in call_dispatch
                                // so lower.rs can rewrite the function reference directly.
                                for c in constraints[constraints_before..].iter() {
                                    if let Constraint::Class { class, vars, .. } = c {
                                        // Resolve each constraint var to a concrete type name.
                                        // None means unresolved — abort: can't determine instance.
                                        let resolved_args: Option<Vec<String>> = vars
                                            .iter()
                                            .map(|v| {
                                                let ty = match v {
                                                    crate::type_class::ConstraintArg::Var(
                                                        var_name,
                                                    ) => state
                                                        .apply(&Type::TypeVar(var_name.clone(), 0)),
                                                    crate::type_class::ConstraintArg::Ground(
                                                        ty,
                                                    ) => ty.clone(),
                                                };
                                                // Map concrete types to canonical string names.
                                                // These names must match what extract_dispatch_tags
                                                // produces from instance arm patterns (e.g., `[let a@Int]`
                                                // → "Int"). The annotation name IS the dispatch tag.
                                                match &ty {
                                                    Type::TypeVar(_, _) => None, // unresolved
                                                    Type::Int | Type::IntLiteral(_) => {
                                                        Some("Int".to_string())
                                                    }
                                                    Type::Float => Some("Float".to_string()),
                                                    Type::Str | Type::StringLiteral(_) => {
                                                        Some("String".to_string())
                                                    }
                                                    Type::Bytes => Some("Bytes".to_string()),
                                                    // TyCon: map to the annotation name used in instance patterns.
                                                    // "Boolean" in type system = "Bool" in instance annotations.
                                                    // Other TyCons: use their name as-is.
                                                    Type::TyCon(n) if n == "Boolean" => {
                                                        Some("Bool".to_string())
                                                    }
                                                    Type::TyCon(n) => Some(n.clone()),
                                                    // NominalVariant: tag is "TypeName.CtorName";
                                                    // extract "TypeName" to match instance annotation
                                                    // patterns like [let a@Point].
                                                    Type::NominalVariant { tag, .. } => {
                                                        Some(tag.split('.').next().unwrap_or(tag).to_string())
                                                    }
                                                    // Union of NominalVariants: the TyCon name is the
                                                    // common prefix of all tags. For a type like
                                                    // Result (= Ok | Err), all tags share "Result".
                                                    Type::Union(members) => {
                                                        // Try to extract a common TyCon name from
                                                        // NominalVariant tags in the union.
                                                        let mut tycon_name: Option<&str> = None;
                                                        let mut all_nominal = true;
                                                        for m in members {
                                                            if let Type::NominalVariant { tag, .. } = m {
                                                                let name = tag.split('.').next().unwrap_or(tag);
                                                                match tycon_name {
                                                                    None => tycon_name = Some(name),
                                                                    Some(existing) if existing == name => {}
                                                                    _ => { all_nominal = false; break; }
                                                                }
                                                            } else {
                                                                all_nominal = false;
                                                                break;
                                                            }
                                                        }
                                                        if all_nominal {
                                                            tycon_name.map(|n| n.to_string())
                                                        } else {
                                                            Some(format!("{ty}"))
                                                        }
                                                    }
                                                    other => Some(format!("{other}")),
                                                }
                                            })
                                            .collect();

                                        if let Some(type_args) = resolved_args {
                                            let type_arg_refs: Vec<&str> =
                                                type_args.iter().map(|s| s.as_str()).collect();
                                            let binding_name =
                                                crate::type_def::instance_binding_name(
                                                    &class.name,
                                                    name,
                                                    &type_arg_refs,
                                                );
                                            // Write (level, slot) to call_dispatch using the
                                            // Write (level, slot) to call_dispatch:
                                            // - level: from func VarRef's resolution, which the
                                            //   resolver assigned via any instance binding in the
                                            //   same scope frame (all share the same level)
                                            // - slot: from instance_binding_slots for the
                                            //   specific instance binding the type checker chose
                                            if let crate::ast::SurfaceExpression::VarRef {
                                                resolution,
                                                call_dispatch,
                                                ..
                                            } = &func.expr
                                            {
                                                if let Some(&slot) =
                                                    state.instance_binding_slots.get(&binding_name)
                                                {
                                                    if let Some(Some((level, _))) =
                                                        resolution.get()
                                                    {
                                                        call_dispatch.set(level, slot);
                                                    }
                                                }
                                            }
                                        }
                                        // Only process the first Class constraint (the primary dispatch).
                                        break;
                                    }
                                }
                            }

                            call_result.map_err(|mut errs| {
                                // B-374/B-379: push call-site frame onto each error so the
                                // user sees WHERE the call was made, even when the error span
                                // originates inside a prelude function or macro body.
                                //
                                // Frame is skipped only when the error's primary span already
                                // IS the call-site span AND from the same file — i.e., the
                                // error already identifies the call expression directly (e.g.
                                // arity mismatch uses the call span itself).  When the error
                                // originates in a different file (prelude, included file) we
                                // always push the frame, even if byte offsets happen to match
                                // (B-379: prelude byte offset ≠ caller context).
                                let frame = TypeSpanFrame::call(name, node.span.clone());
                                for err in &mut errs {
                                    let err_span = err.span();
                                    let same_pos = err_span.start.offset == node.span.start.offset
                                        && err_span.end.offset == node.span.end.offset;
                                    let same_file =
                                        err_span.file.is_none() && node.span.file.is_none();
                                    if !same_pos || !same_file {
                                        err.push_frame(frame.clone());
                                    }
                                }
                                errs
                            })
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
                                constraints,
                                type_map,
                            )
                            .await
                            .map_err(|mut errs| {
                                // B-374/B-379: push call-site frame for monomorphic named calls.
                                let frame = TypeSpanFrame::call(name, node.span.clone());
                                for err in &mut errs {
                                    let err_span = err.span();
                                    let same_pos = err_span.start.offset == node.span.start.offset
                                        && err_span.end.offset == node.span.end.offset;
                                    let same_file =
                                        err_span.file.is_none() && node.span.file.is_none();
                                    if !same_pos || !same_file {
                                        err.push_frame(frame.clone());
                                    }
                                }
                                errs
                            })
                        }
                        None => {
                            // Special handling for $proxy builtin: produces Type::Proxy
                            if name == "proxy" {
                                // Infer arguments for type map population
                                for arg in args {
                                    let _ =
                                        infer_surface_expr(arg, env, state, constraints, type_map)
                                            .await?;
                                }
                                for na in named_args {
                                    let _ = infer_surface_expr(
                                        &na.node.value,
                                        env,
                                        state,
                                        constraints,
                                        type_map,
                                    )
                                    .await?;
                                }
                                Ok(Type::Proxy)
                            } else {
                                let mut err =
                                    TypeErrorTyped::UndefinedVariable(UndefinedVariable {
                                        name: name.to_string(),
                                        span: func.span.clone(),
                                        notes: vec![],
                                        call_stack: vec![],
                                    });
                                if let Some(cause_span) = state.failed_bindings.get(name.as_str()) {
                                    err.add_note(format!(
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
                        constraints,
                        type_map,
                    )
                    .await
                    .map_err(|mut errs| {
                        // B-374: push anonymous call-site frame so the call chain is visible
                        // even for lambda/inline-function call errors.
                        let frame = TypeSpanFrame::call_anon(node.span.clone());
                        for err in &mut errs {
                            let err_span = err.span();
                            let same_pos = err_span.start.offset == node.span.start.offset
                                && err_span.end.offset == node.span.end.offset;
                            let same_file = err_span.file.is_none() && node.span.file.is_none();
                            if !same_pos || !same_file {
                                err.push_frame(frame.clone());
                            }
                        }
                        errs
                    })
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
                    constraints,
                    type_map,
                )
                .await
            }

            SurfaceExpression::TypeAssert {
                annotation,
                expr: inner,
                resolved_type: resolved_type_ann,
            } => {
                // resolve_type_assert returns Ok(type) as the authoritative result.
                // Write the resolved type inline on the AST node for the lowerer to read.
                let result = resolve_type_assert(
                    annotation,
                    inner,
                    env,
                    node.span.clone(),
                    state,
                    constraints,
                    type_map,
                )
                .await;
                // Write inline so lower.rs can produce CoreExpr::TypeAssert
                // with the statically-resolved type (or None for errors/macros).
                if let Ok(ref ty) = result {
                    resolved_type_ann.set(Some(ty.clone()));
                }
                result
            }

            // Annotated VarRef (name@Type): annotation is now on VarRef directly.
            SurfaceExpression::VarRef { name, annotation: Some(annotation), .. } => {
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
                    constraints,
                    &mut ann_mapping_opt,
                    &mut row_ann_mapping_opt,
                    None,
                )
                .await
                .map_err(|e| vec![e])
            }

            SurfaceExpression::Quote(inner) => {
                // [quote expr] produces an Expr.* AST node value.
                // The specific Expr variant is determined from the surface form of inner.
                // Walk inner in quotation context so unquote/unquote-splice forms are
                // type-checked in normal context, but other sub-expressions are not
                // type-inferred (preventing Task/Channel leakage through quote bodies).
                let result_ty = expr_type_for_quote(inner, env);
                check_in_quote_context(inner, env, state, constraints, type_map).await;
                if let Some(ref mut tm) = type_map {
                    tm.insert(
                        (node.span.start.offset, node.span.end.offset),
                        result_ty.clone(),
                    );
                }
                Ok(result_ty)
            }

            SurfaceExpression::Unquote(inner) => {
                // [unquote expr] evaluates expr and returns its type.
                infer_surface_expr(inner, env, state, constraints, type_map).await
            }

            SurfaceExpression::UnquoteSplice(inner) => {
                // [unquote-splice expr] expects expr to be a list (Dict with integer keys).
                let inner_ty = infer_surface_expr(inner, env, state, constraints, type_map).await?;

                let expected_list_ty = Type::Record(Row {
                    fields: indexmap::IndexMap::new(),
                    tail: crate::type_def::RowTail::Empty,
                });

                let result = Box::pin(unify(
                    &inner_ty,
                    &expected_list_ty,
                    state,
                    constraints,
                    inner.span.clone(),
                ))
                .await;
                result.map_err(|_e| {
                    vec![TypeErrorTyped::Generic(GenericTypeError {
                        message: format!("unquote-splice expects a list (Dict), got {}", inner_ty),
                        span: inner.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    })]
                })?;

                Ok(expected_list_ty)
            }

            SurfaceExpression::Match { scrutinee, arms } => {
                // Infer scrutinee type — needed for exhaustiveness checking.
                let scrutinee_ty =
                    infer_surface_expr(scrutinee, env, state, constraints, type_map).await?;
                let scrutinee_ty = state.apply(&scrutinee_ty);
                // TyCon expansion (T-1272): expand a named type constructor to its body before
                // pattern narrowing. This enables `collect_pattern_bindings` to extract payload
                // types from constructor patterns when the scrutinee is typed as a TyCon.
                //
                // Example: `ann: TyCon("Annotation")` matched against `[Annotation.PropertyDict p]`
                // — without expansion, `collect_pattern_bindings` falls to `_ => Unknown` since
                // it only handles Union/NominalVariant/Record, not TyCon. With expansion,
                // `scrutinee_ty` becomes `Union([Simple, PropertyDict, Annotated])` and the
                // constructor pattern correctly narrows `p` to `{parts: Map Int Any, ...}`.
                //
                // One-level expansion only. Builtin TyCons have non-TyCon bodies (Int→Type::Int,
                // Map→App(...), etc.), so they fall through match arm dispatch without looping.
                // Gradual: unknown TyCons (not in tycon_env) are left as-is.
                let scrutinee_ty = {
                    // TyCon expansion: look up body before consuming scrutinee_ty.
                    let expanded = if let Type::TyCon(name) = &scrutinee_ty {
                        state
                            .tycon_env
                            .get(name.as_str())
                            .map(|def| def.body.clone())
                    } else {
                        None
                    };
                    expanded.unwrap_or(scrutinee_ty)
                };

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
                                        fields: indexmap::IndexMap::new(),
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
                        Pattern::Wildcard | Pattern::Pin(..) => remaining_scrutinee.clone(),
                        // T-1140: Predicate patterns — no static scrutinee narrowing.
                        // The predicate is opaque; we cannot determine what types it accepts.
                        Pattern::Predicate(_) => remaining_scrutinee.clone(),
                        _ => scrutinee_ty.clone(),
                    };

                    // Elaboration pass: resolve TypeAssertPending → TypeAssert before collecting
                    // pattern bindings. This ensures collect_pattern_bindings sees resolved_type
                    // (not the raw annotation) when computing variable types for arm body checking.
                    let elaborated_pat =
                        elaborate_pattern(&arm.pattern.node, env, state, &arm.pattern.span).await?;

                    // Persist elaboration: record annotation-span → resolved type in the
                    // Write resolved types inline on TypeAssertPending nodes so lower.rs
                    // can convert TypeAssertPending → TypeAssert in CoreMatchArm patterns (B-338).
                    record_pattern_elaborations(&elaborated_pat, &arm.pattern.node);

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
                        let _guard_ty =
                            infer_surface_expr(guard, &arm_env, state, constraints, type_map)
                                .await?;
                        // extract_narrowings walks SurfaceExpression natively — pass guard directly.
                        // Uses arm_env to look up param_narrowings on called functions.
                        let guard_narrowings = extract_narrowings(guard, &arm_env);
                        if guard_narrowings.is_empty() {
                            arm_env
                        } else {
                            apply_narrowings(&arm_env, &guard_narrowings, state)
                        }
                    } else {
                        arm_env
                    };
                    let arm_ty =
                        infer_surface_expr(&arm.body, &arm_env, state, constraints, type_map)
                            .await?;
                    arm_result_types.push(arm_ty);

                    // Update remaining_scrutinee for subsequent arms (I-Case3 negation accumulation).
                    if arm.guard.is_none() {
                        match &arm.pattern.node {
                            Pattern::Constructor { tag, .. } => {
                                let neg_tag = Type::Negation(Box::new(Type::NominalVariant {
                                    tag: tag.clone(),
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
                let sig = match &scrutinee_ty {
                    Type::Union(members) => {
                        coverage::ConstructorSignature::from_union(members, &state.tycon_env)
                    }
                    Type::NominalVariant { tag, fields } => {
                        Some(coverage::ConstructorSignature::from_nominal_variant(
                            tag,
                            fields,
                            &state.tycon_env,
                        ))
                    }
                    // Boolean is now a TyCon — handled via TyCon lookup below.
                    // User-defined type constructors: look up TyConDef.constructors in tycon_env.
                    // Handles both bare TyCon(name) scrutinees and App(TyCon(name), arg) forms
                    // (e.g., a user-defined parameterized type).
                    // TyConDef.constructors is populated by T-1036 for nominal ADTs declared in
                    // prelude.llt. T-1003 (S-852) handles TypeEnv.tycon_defs population for
                    // user-defined [type ...] declarations. If constructors is empty, we return
                    // None so coverage checking is skipped (no false non-exhaustiveness warnings).
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
                            // TyCon not found in tycon_env or has no declared constructors —
                            // skip coverage checking to avoid false non-exhaustiveness warnings.
                            _ => None,
                        }
                    }
                    _ => None,
                };

                if let Some(sig) = sig {
                    let coverage_patterns: Vec<coverage::CoveragePattern> = arms
                        .iter()
                        .map(|arm| {
                            // For [case ...] arms, the SurfaceMatchArm.pattern is a Wildcard
                            // sentinel — the actual structural pattern is inside the CaseArm body.
                            // Extract the constructor tag from the CaseArm pattern expression
                            // so exhaustiveness checking sees the real pattern.
                            if matches!(arm.pattern.node, Pattern::Wildcard) {
                                if let SurfaceExpression::CaseArm { pattern, .. } = &arm.body.expr {
                                    if let Some(cp) = extract_case_arm_coverage_pattern(
                                        &pattern.expr,
                                        &state.tycon_env,
                                    ) {
                                        return cp;
                                    }
                                }
                            }
                            // Qualify any unqualified Variant tags so they match the qualified
                            // constructor tags in the sig (e.g., "None" → "Option.None").
                            coverage::qualify_coverage_pattern(
                                coverage::ast_pattern_to_coverage(
                                    &arm.pattern.node,
                                    Some(&state.tycon_env),
                                ),
                                &state.tycon_env,
                            )
                        })
                        .collect();
                    let has_guards: Vec<bool> = arms
                        .iter()
                        .map(|arm| {
                            if arm.guard.is_some() {
                                return true;
                            }
                            // Guard-expression case arms are opaque to coverage analysis
                            // (Karachalias et al. 2015, §2.4): they may not always fire,
                            // so they must be treated as guarded arms in the coverage matrix.
                            // A [case ...] arm has Pattern::Wildcard as the SurfaceMatchArm.pattern;
                            // the actual pattern is inside the CaseArm body. If
                            // extract_case_arm_coverage_pattern returned None for that body pattern,
                            // it is a guard expression (lowercase/operator head), not a constructor.
                            // Plain `_` wildcards also return None but are genuine non-guarded wildcards;
                            // they are distinguishable because a CaseArm body with a VarRef("_") pattern
                            // is a true wildcard while a Call-headed body is a guard expression.
                            if matches!(arm.pattern.node, Pattern::Wildcard) {
                                if let SurfaceExpression::CaseArm { pattern, .. } = &arm.body.expr {
                                    if extract_case_arm_coverage_pattern(
                                        &pattern.expr,
                                        &state.tycon_env,
                                    )
                                    .is_none()
                                    {
                                        // None returned from extract means: not a constructor.
                                        // Check that it's not the literal _ wildcard — a bare wildcard
                                        // VarRef with name "_" is a non-guarded wildcard, not a guard.
                                        let is_bare_wildcard = matches!(
                                            &pattern.expr,
                                            SurfaceExpression::VarRef { name, .. } if name == "_"
                                        );
                                        if !is_bare_wildcard {
                                            return true;
                                        }
                                    }
                                }
                            }
                            false
                        })
                        .collect();
                    let result = coverage::check_coverage(&coverage_patterns, &sig, &has_guards);
                    let mut match_errors: Vec<TypeError> = Vec::new();

                    if !result.exhaustive {
                        let witnesses = coverage::format_witnesses(&result.uncovered);
                        match_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "non-exhaustive match: missing coverage for {}",
                                witnesses
                            ),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        }));
                    }
                    for &idx in &result.redundant {
                        match_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                        message:
                            "unreachable match arm: this pattern is already covered by prior arms"
                                .to_string(),
                        span: arms[idx].pattern.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }));
                    }
                    for &idx in &result.inaccessible {
                        match_errors.push(TypeErrorTyped::Generic(GenericTypeError {
                        message:
                            "inaccessible match arm: reachable only via diverging (bottom) values"
                                .to_string(),
                        span: arms[idx].pattern.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    }));
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
                } => {
                    // Method schemes are pushed to state.pending_scheme_injections by the callee.
                    // The caller (infer_dict Pass 0c) drains and injects them into dict_env.
                    let ty = infer_class_decl_from_surface(
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
                    ).await?;
                    Ok(ty)
                }
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
                ).await,
                SurfaceDeclaration::TypeAlias { .. } => {
                    // Extract body from decl_box directly to avoid borrow issues with **decl_box.
                    if let SurfaceDeclaration::TypeAlias { ref body, .. } = **decl_box {
                        expand_type_alias(body, env, state).await.map_err(|e| vec![e])
                    } else {
                        unreachable!()
                    }
                }
                SurfaceDeclaration::Splice(..) => Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "Splice should be removed by expansion pass before typechecking (internal error)".to_string(),
                    span: node.span.clone(),
                    notes: vec![], call_stack: vec![],
                })]),
                SurfaceDeclaration::SyntaxClass { .. } => Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "SyntaxClass should be removed by expansion pass before typechecking (internal error)".to_string(),
                    span: node.span.clone(),
                    notes: vec![], call_stack: vec![],
                })]),
            }
            }

            SurfaceExpression::LetDecl { bindings } => {
                // LetDecl in value position is always an error (only valid in binding contexts).
                let msg = if bindings.len() > 1 {
                    "multi-element [let ...] pattern not yet supported — use single binding"
                        .to_string()
                } else {
                    "binding declaration [let ...] is not valid in expression position".to_string()
                };
                Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: msg,
                    span: node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })])
            }

            SurfaceExpression::CaseArm {
                let_bindings,
                pattern: _,
                body,
            } => {
                // let_bindings is the [let ...] binding node; typecheck_case_arm uses it
                // to bind names before checking the body.
                typecheck_case_arm(
                    let_bindings,
                    body,
                    &Type::Unknown,
                    env,
                    state,
                    constraints,
                    type_map,
                )
                .await
            }

            SurfaceExpression::Placeholder => {
                // Gradual: placeholder (`...`) is the explicit gradual typing escape hatch.
                Ok(Type::Unknown)
            }

            SurfaceExpression::Rest(..) => Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                message: "rest marker (...) is only valid inside type expressions".to_string(),
                span: node.span.clone(),
                notes: vec![],
                call_stack: vec![],
            })]),

            SurfaceExpression::PatternDecl { .. } => {
                // PatternDecl should never appear in value positions (only in instance arms)
                Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "pattern declaration is only valid in instance match arms".to_string(),
                    span: node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })])
            }

            SurfaceExpression::Error(span) => {
                Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: format!(
                        "syntax error at {}:{} (cannot typecheck error node)",
                        span.start.line, span.start.column
                    ),
                    span: node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })])
            }
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
                Err(errs) => {
                    map.insert(key, Type::error_with(errs.clone()));
                }
            }
        }

        result
    }) // end Box::pin(async move {
}

/// Type-check a surface node in quotation context (inside `[quote ...]`).
/// Return the `Expr.*` variant type for a quoted surface expression.
///
/// Inspects the surface form without any type inference — the result type of `[quote expr]`
/// is determined structurally from expr's syntax, not from its value type.
///
/// Looks up the constructor function for each Expr variant in the type environment and
/// extracts its return type (the `NominalVariant { tag: "Expr.Call", ... }` etc.).
/// Falls back to the bare "Expr" type if the specific variant isn't registered yet,
/// and to `Type::Unknown` if Expr itself isn't in scope (e.g. pre-prelude type stage).
fn expr_type_for_quote(_inner: &SurfaceNode, _env: &Rc<TypeEnv>) -> Type {
    // The correct return type for [quote expr] is an Expr.* NominalVariant — e.g.
    // Expr.Call for a call expression, Expr.VarRef for a name, etc. However,
    // returning NominalVariant types (even with empty fields) changes constraint graph
    // interactions in a way that produces non-deterministic type error reporting across
    // prelude. The simpler Record({}) type binds TypeVars (e.g. builtin-if's `a`)
    // to a concrete stable value without triggering those interactions.
    //
    // TODO: return the proper Expr.* NominalVariant once the constraint graph instability
    // is understood. The instability is in how NominalVariant unification differs from
    // Record unification in the constraint solver — a separate issue from quote typing.
    Type::Record(crate::type_def::Row {
        fields: indexmap::IndexMap::new(),
        tail: crate::type_def::RowTail::Empty,
    })
}

///
/// In quotation context, sub-expressions are AST templates — they are NOT function calls,
/// NOT variable lookups, NOT evaluated expressions. `builtin-if`, `builtin-task`, etc.
/// are just names embedded in syntax. This prevents calls inside quote bodies from leaking
/// their return types (e.g. `builtin-task`'s `Task` type) into the constraint graph.
///
/// `[unquote x]` and `[unquote-splice xs]` switch back to NORMAL evaluation context:
/// `x`/`xs` are inferred as regular expressions. Errors in `x`/`xs` propagate normally.
fn check_in_quote_context<'a>(
    node: &'a Arc<SurfaceNode>,
    env: &'a Rc<TypeEnv>,
    state: &'a mut InferState,
    constraints: &'a mut Vec<Constraint>,
    type_map: &'a mut Option<&mut TypeMap>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        match &node.expr {
            // Unquote/UnquoteSplice: the inner expr is in normal evaluation context,
            // but calling infer_surface_expr here mutates state.subst/constraints via
            // side effects, causing non-deterministic constraint solving for the enclosing
            // macro body. Until state isolation for speculative inference is available,
            // we recurse in quote context (treating the inner expr as an AST template).
            // This means unquote args are not type-checked — a known limitation.
            // TODO: use a cloned/forked state to safely infer unquote args.
            SurfaceExpression::Unquote(inner) => {
                Box::pin(check_in_quote_context(inner, env, state, constraints, type_map)).await;
            }
            SurfaceExpression::UnquoteSplice(inner) => {
                Box::pin(check_in_quote_context(inner, env, state, constraints, type_map)).await;
            }

            // Call in quote context: func and args are AST template positions.
            // Recurse into all children in quote context (they are not evaluated).
            SurfaceExpression::Call { func, args, named_args, .. } => {
                Box::pin(check_in_quote_context(func, env, state, constraints, type_map)).await;
                for arg in args.iter() {
                    Box::pin(check_in_quote_context(arg, env, state, constraints, type_map)).await;
                }
                for na in named_args.iter() {
                    Box::pin(check_in_quote_context(&na.node.value, env, state, constraints, type_map)).await;
                }
            }

            // Dict entries may contain unquote forms — recurse into all children.
            SurfaceExpression::Dict(entries) => {
                for entry in entries.iter() {
                    if let Some(ref key) = entry.node.key {
                        Box::pin(check_in_quote_context(key, env, state, constraints, type_map)).await;
                    }
                    Box::pin(check_in_quote_context(&entry.node.value, env, state, constraints, type_map)).await;
                }
            }

            // Fn body may contain unquote forms — recurse into body.
            SurfaceExpression::Fn { body, .. } => {
                Box::pin(check_in_quote_context(body, env, state, constraints, type_map)).await;
            }

            SurfaceExpression::Sequential(exprs) => {
                for e in exprs.iter() {
                    Box::pin(check_in_quote_context(e, env, state, constraints, type_map)).await;
                }
            }

            // Nested quote: recurse in quote context (double-quoting is still quoted).
            SurfaceExpression::Quote(inner) => {
                Box::pin(check_in_quote_context(inner, env, state, constraints, type_map)).await;
            }

            // Field access in quote context: recurse into the target expression.
            SurfaceExpression::Field { expr: Some(target), .. } => {
                Box::pin(check_in_quote_context(target, env, state, constraints, type_map)).await;
            }

            // Match in quote context: recurse into scrutinee and arm bodies.
            SurfaceExpression::Match { scrutinee, arms } => {
                Box::pin(check_in_quote_context(scrutinee, env, state, constraints, type_map)).await;
                for arm in arms.iter() {
                    Box::pin(check_in_quote_context(&arm.body, env, state, constraints, type_map)).await;
                }
            }

            // Atoms in quote context (VarRef, literals, TypeAssert, Placeholder, etc.)
            // are just AST node structure — no type inference needed, no unquote inside.
            _ => {}
        }
    })
}

/// Type-check a [class ...] declaration from SurfaceDeclaration::ClassDecl fields.
/// Called from infer_surface_expr (Decl arm) and typecheck_surface_document — no Expr bridge needed.
///
/// Method schemes are NOT returned in the `Result` — they are pushed to
/// `state.pending_scheme_injections` so both call sites use the same injection channel.
/// Callers must drain `state.pending_scheme_injections` and insert into the active TypeEnv
/// after this function returns.
#[allow(clippy::too_many_arguments)]
async fn infer_class_decl_from_surface(
    name: &str,
    params: &[String],
    superclasses: &[(String, Vec<String>)],
    methods: &[Spanned<crate::ast::SurfaceEntry>],
    determines: &[Arc<SurfaceNode>],
    resolver: &Option<Arc<SurfaceNode>>,
    resolver_injective: bool,
    span: Span,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    _type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::types::{ClassDecl, Kind, TypeScheme};

    if name.is_empty() {
        return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
            message: "class declaration must have a name declared with [class [ClassName ...] ...]"
                .to_string(),
            span,
            notes: vec![],
            call_stack: vec![],
        })]);
    }

    // Step 1: Introduce class parameter names as lexically scoped type bindings.
    // The declared params (e.g. "a", "b", "c") are bound for the duration of the
    // class body. Using declared names as TypeVar names keeps TypeScheme bodies
    // consistent with type_vars: ["a", "b", "c"].
    // strict=false: unlike TypeAlias, class method annotations may introduce
    // additional fresh TypeVars beyond the class params — those fall through to
    // ann_mapping / fresh_type_var creation without being rejected.
    let class_param_scope: HashMap<String, crate::types::Type> = params
        .iter()
        .map(|p| {
            // Register class type param in unified TypeVar table if not already present
            if state.type_vars.get(p.as_str()).is_none() {
                state.set_level(p.clone(), state.level);
            }
            state.type_var_source_names.insert(p.clone(), p.clone());
            (
                p.clone(),
                crate::types::Type::TypeVar(p.clone(), state.level),
            )
        })
        .collect();

    // Step 2: Collect method signatures from the class body.
    let mut collected_method_sigs: Vec<(String, crate::types::Type)> = Vec::new();
    for method in methods {
        let method_name = match &method.node.key {
            Some(key_node) => match &key_node.expr {
                SurfaceExpression::Str(s) => s.clone(),
                SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                _ => {
                    return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                        message: "class method name must be a string or identifier".to_string(),
                        span: key_node.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    })]);
                }
            },
            None => {
                return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "class method must have a name".to_string(),
                    span: method.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })]);
            }
        };
        let mut method_constraints: Vec<Constraint> = Vec::new();
        let mut ann_map_ref: HashMap<String, String> = HashMap::new();
        let method_type = resolve_type_expr(
            &method.node.value,
            env,
            state,
            &mut method_constraints,
            &mut Some(&mut ann_map_ref),
            &mut None,
            Some((&class_param_scope, false)), // not strict: allow extra TypeVars
        )
        .await
        .unwrap_or_else(|_| crate::type_def::Type::error_cascade());

        collected_method_sigs.push((method_name, method_type));
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
                return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "functional dependency must be a 2-element list [[determining-vars] determined-var(s)]".to_string(),
                    span: fd_node.span.clone(),
                    notes: vec![], call_stack: vec![],
                })]);
            }
        }
    }

    let resolver_name = if let Some(resolver_node) = resolver {
        match &resolver_node.expr {
            SurfaceExpression::VarRef { name: n, .. } => Some(n.clone()),
            SurfaceExpression::Str(s) => Some(s.clone()),
            _ => {
                return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                    message: "resolver must be an identifier or string".to_string(),
                    span: resolver_node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                })]);
            }
        }
    } else {
        None
    };

    // Step 3: Build ClassDecl with collected method signatures.
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
            .map(|(class_name, params)| (class_name.clone(), params.clone()))
            .collect(),
        determines: fd_indices,
        resolver: resolver_name,
        resolver_injective,
        method_signatures: collected_method_sigs,
    };

    // Step 4 (S-886): Wrap in Arc to keep alive for scheme construction.
    let class_decl_arc = std::sync::Arc::new(class_decl);
    state.class_env.insert((*class_decl_arc).clone());
    for (param_name, kind) in &class_decl_arc.params {
        if *kind == Kind::Operator {
            state.set_kind(param_name.clone(), Kind::Operator);
        }
    }

    // Step 5 (S-886): Build a TypeScheme for each method signature and push to
    // state.pending_scheme_injections. The caller drains this vec and inserts the schemes into
    // the active TypeEnv so subsequent entries see the method types.
    // CRITICAL: use Arc::clone(&class_decl_arc) which has the real FD in `determines`.
    // Do NOT use Constraint::new_by_name — it creates a stub ClassDecl with empty FD.
    let type_vars: Vec<String> = class_decl_arc
        .params
        .iter()
        .map(|(n, _)| n.clone())
        .collect();
    for (method_name, method_type) in &class_decl_arc.method_signatures {
        let scheme = TypeScheme {
            type_vars: type_vars.clone(),
            constraints: vec![Constraint::Class {
                class: std::sync::Arc::clone(&class_decl_arc),
                vars: type_vars
                    .iter()
                    .map(|n| crate::types::ConstraintArg::Var(n.clone()))
                    .collect(),
                origin_name: Some(std::sync::Arc::from(method_name.as_str())),
                origin_span: None,
            }],
            body: method_type.clone(),
            label_vars: vec![],
            kind_vars: vec![],
            doc: None,
            inner_schemes: None,
            param_narrowings: Vec::new(),
        };
        state
            .pending_scheme_injections
            .push((method_name.clone(), scheme));
    }

    Ok(Type::Record(Row {
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
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<Type, Vec<TypeError>> {
    use crate::types::InstanceDecl;

    if arms.is_empty() {
        return Ok(Type::Record(Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Empty,
        }));
    }

    let (param_count, has_fds, fd_list, param_names) = {
        let class_decl = state.class_env.get(class_name).ok_or_else(|| {
            vec![TypeErrorTyped::Generic(GenericTypeError {
                message: format!("unknown class '{}'", class_name),
                span: span.clone(),
                notes: vec![],
                call_stack: vec![],
            })]
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
            return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                message: format!(
                    "instance pattern has {} type parameters but class '{}' expects {}",
                    pattern_types.len(),
                    class_name,
                    param_count
                ),
                span: pattern_node.span.clone(),
                notes: vec![],
                call_stack: vec![],
            })]);
        }

        if pattern_types.iter().any(|ty| matches!(ty, Type::Unknown)) {
            return Err(vec![TypeErrorTyped::InstanceContainsUnknown(
                InstanceContainsUnknown {
                    span: pattern_node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                },
            )]);
        }

        arm_data.push((pattern_types, pattern_node.span.clone(), methods));
    }

    for i in 0..arm_data.len() {
        for j in (i + 1)..arm_data.len() {
            let (types_i, span_i, _) = &arm_data[i];
            let (types_j, span_j, _) = &arm_data[j];

            if patterns_overlap(types_i, types_j, state).await? {
                let error = TypeErrorTyped::OverlappingInstancePatterns(OverlappingInstancePatterns {
                    span: span_j.clone(),
                    notes: vec![format!(
                        "overlapping instance patterns for class '{}': arm at line {} and arm at line {} could both match the same types",
                        class_name,
                        span_i.start.line,
                        span_j.start.line
                    )],
                    call_stack: vec![],
                });
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
                                return Err(vec![TypeErrorTyped::CoverageViolation(CoverageViolation {
                                    span: arm_span.clone(),
                                    notes: vec![format!(
                                        "coverage violation for class '{}': determined parameter '{}' (variable '{}') does not appear in any determining position",
                                        class_name, param_name, det_name
                                    )],
                                    call_stack: vec![],
                                })]);
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
                            let error = TypeErrorTyped::ConsistencyViolation(ConsistencyViolation {
                                span: span_j.clone(),
                                notes: vec![format!(
                                    "consistency violation for class '{}': arm at line {} and arm at line {} have overlapping determining positions but incompatible determined types",
                                    class_name,
                                    span_i.start.line,
                                    span_j.start.line
                                )],
                                call_stack: vec![],
                            });
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

        for method in *methods {
            let method_name = match &method.node.key {
                Some(key_node) => match &key_node.expr {
                    SurfaceExpression::Str(s) => s.clone(),
                    SurfaceExpression::VarRef { name: n, .. } => n.clone(),
                    _ => {
                        return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                            message: "instance method name must be a string or identifier"
                                .to_string(),
                            span: key_node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })]);
                    }
                },
                None => {
                    return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                        message: "instance method must have a name".to_string(),
                        span: method.span.clone(),
                        notes: vec![],
                        call_stack: vec![],
                    })]);
                }
            };

            let mut local_constraints = Vec::new();
            let method_impl_type = infer_surface_expr(
                &method.node.value,
                env,
                state,
                &mut local_constraints,
                type_map,
            )
            .await?;
            method_types.insert(method_name, method_impl_type);
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
        // their string keys differ (e.g., `[F a]` vs `[F Int]`).
        // Clone the instance_env to satisfy the borrow checker — check_structural_overlap
        // takes &self (read-only) but state is also needed mutably for freshening.
        // This follows the same clone pattern used in resolve_instance callers.
        {
            let inst_env_snapshot = state.instance_env.clone();
            if let Err(msg) = inst_env_snapshot
                .check_structural_overlap(&instance_decl, state)
                .await
            {
                // Structural overlap is advisory (warning), not a blocking error.
                // User code may deliberately re-declare instances that the prelude defines
                // (e.g., `[instance Equatable [pattern [Int]]: ...]` in user code); the
                // InstanceEnv::insert deduplicates by key so no actual duplicate is registered.
                state.diagnostics.push(crate::error::TypeDiagnostic {
                    message: msg,
                    span: span.clone(),
                    code: typecheck_diag::W043_INSTANCE_OVERLAP,
                    level: crate::error::DiagnosticLevel::Warn,
                });
            }
        }

        if let Err(msg) = state.instance_env.insert(instance_decl) {
            return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                message: msg,
                span: span.clone(),
                notes: vec![],
                call_stack: vec![],
            })]);
        }
    }

    Ok(Type::Record(Row {
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
fn contains_unknown_or_top(ty: &Type) -> bool {
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
        Type::App(f, arg) => contains_unknown_or_top(f) || contains_unknown_or_top(arg),
        Type::TyCon(_) => false,
        Type::Record(row) => row.fields.values().any(contains_unknown_or_top),
        Type::Union(members) => members.iter().any(contains_unknown_or_top),
        _ => false,
    }
}

/// This function is used at checking positions where the expected type is fully concrete
/// (no type variables): CALL-MONO arguments, concrete return annotations (no TypeVars), and TypeAssert.
pub(super) async fn check_surface_expr(
    node: &Arc<SurfaceNode>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    constraints: &mut Vec<Constraint>,
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
            let resolved_expected = if state.subst_is_empty() {
                expected.clone()
            } else {
                state.apply(expected)
            };
            // Only use lambda checking mode if expected type is fully concrete after applying subst
            if let Type::Function {
                params: ref expected_params,
                ret: ref expected_ret,
                variadic: ref expected_variadic,
                required_count: _,
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
                        return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                            message: format!(
                                "arity mismatch: expected {} arguments, got {}",
                                expected_params.len(),
                                params.len()
                            ),
                            span: node.span.clone(),
                            notes: vec![],
                            call_stack: vec![],
                        })]);
                    }

                    // Build parameter types: use expected types for unannotated params.
                    // For annotated params, verify the annotation is compatible with the expected
                    // type: expected_ty must be a subtype of the annotation (contravariant check).
                    // Example: expected Fn(Int→...) but param declared @String → Int <: String is
                    // false → error, because callers will pass Int but the body expects String.
                    let mut param_types: Vec<Type> = Vec::with_capacity(params.len());
                    for (p, (_, expected_ty)) in params.iter().zip(expected_params.iter()) {
                        let param_ty = match &p.node.annotation {
                            Some(ann) => {
                                let resolved = resolve_annotation(
                                    &ann.node,
                                    env,
                                    ann.span.clone(),
                                    state,
                                    constraints,
                                    &mut ann_mapping_opt,
                                    &mut row_ann_mapping_opt,
                                    None,
                                )
                                .await
                                .map_err(|e| vec![e])?;
                                // Contravariant check: expected param type must be subtype of annotation.
                                // When annotation contains type variables, use unification mode (not is_subtype)
                                // to actually BIND the TypeVars via constraint solving. is_subtype returns true
                                // for any TypeVar (conservative approximation — see is_subtype_bas docstring),
                                // so it would silently accept without binding, leaving TypeVars unresolved.
                                if resolved.has_inference_vars() {
                                    let mut check_fn_constraints: Vec<Constraint> = Vec::new();
                                    let result = Box::pin(unify(
                                        expected_ty,
                                        &resolved,
                                        state,
                                        &mut check_fn_constraints,
                                        ann.span.clone(),
                                    ))
                                    .await;
                                    result.map_err(|_e| {
                                        vec![TypeErrorTyped::Generic(GenericTypeError {
                                            message: format!("parameter annotation {resolved} is more restrictive than required type {expected_ty}"),
                                            span: ann.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        })]
                                    })?;
                                } else {
                                    // Apply substitution before consistency check
                                    let (expected_ty_resolved, resolved_ty) =
                                        if state.subst_is_empty() {
                                            (expected_ty.clone(), resolved.clone())
                                        } else {
                                            (
                                                state.apply(expected_ty),
                                                state.apply(&resolved),
                                            )
                                        };
                                    let sub_passes =
                                        Type::is_subtype(
                                            &expected_ty_resolved,
                                            &resolved_ty,
                                            Some(&state.tycon_env),
                                        ) || ((contains_unknown_or_top(&expected_ty_resolved)
                                            || contains_unknown_or_top(&resolved_ty))
                                            && Type::is_consistent(
                                                &expected_ty_resolved,
                                                &resolved_ty,
                                            ));
                                    if !sub_passes {
                                        return Err(vec![TypeErrorTyped::Generic(GenericTypeError {
                                            message: format!("parameter annotation {resolved_ty} is more restrictive than required type {expected_ty_resolved}"),
                                            span: ann.span.clone(),
                                            notes: vec![], call_stack: vec![],
                                        })]);
                                    }
                                }
                                resolved
                            }
                            None => expected_ty.clone(),
                        };
                        param_types.push(param_ty);
                    }

                    // Build function environment with parameter bindings
                    let mut fn_env = TypeEnv::with_parent(env);
                    for (param, ty) in params.iter().zip(param_types.iter()) {
                        if param.node.variadic {
                            let elem_var = state.fresh_type_var();
                            fn_env.insert(
                                param.node.name.clone(),
                                Type::Record(crate::type_def::Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Uniform { key: None, value: Box::new(elem_var) },
                                }),
                            );
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
                                constraints,
                                &mut ann_mapping_opt,
                                &mut row_ann_mapping_opt,
                                None,
                            )
                            .await
                            .map_err(|e| vec![e])?;
                            // Check that declared return type is compatible with expected.
                            // When declared contains type variables, use unification mode (not is_subtype)
                            // to actually BIND the TypeVars via constraint solving. is_subtype returns true
                            // for any TypeVar (conservative approximation — see is_subtype_bas docstring),
                            // so it would silently accept without binding, leaving TypeVars unresolved.
                            if declared.has_inference_vars() {
                                let mut ret_constraints: Vec<Constraint> = Vec::new();
                                let result = Box::pin(unify(
                                    &declared,
                                    expected_ret,
                                    state,
                                    &mut ret_constraints,
                                    ann.span.clone(),
                                ))
                                .await;
                                result.map_err(|_e| {
                                    vec![TypeErrorTyped::UnificationFailure(UnificationFailure {
                                        expected: (**expected_ret).clone(),
                                        got: declared.clone(),
                                        span: node.span.clone(),
                                        notes: vec![],
                                        call_stack: vec![],
                                    })]
                                })?;
                            } else {
                                // Apply substitution before consistency check
                                let (declared_resolved, expected_ret_resolved) =
                                    if state.subst_is_empty() {
                                        (declared.clone(), (**expected_ret).clone())
                                    } else {
                                        (
                                            state.apply(&declared),
                                            state.apply(expected_ret),
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
                                    return Err(vec![TypeErrorTyped::UnificationFailure(
                                        UnificationFailure {
                                            expected: expected_ret_resolved.clone(),
                                            got: declared_resolved.clone(),
                                            span: node.span.clone(),
                                            notes: vec![],
                                            call_stack: vec![],
                                        },
                                    )]);
                                }
                            }
                            // Check body against declared return type
                            Box::pin(check_surface_expr(
                                body,
                                &declared,
                                &fn_env,
                                state,
                                constraints,
                                type_map,
                            ))
                            .await?;
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
                            let applied_ret = if state.subst_is_empty() {
                                *expected_ret.clone()
                            } else {
                                state.apply(expected_ret)
                            };
                            Box::pin(check_surface_expr(
                                body,
                                &applied_ret,
                                &fn_env,
                                state,
                                constraints,
                                type_map,
                            ))
                            .await?;
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
    let actual = infer_surface_expr(node, env, state, constraints, type_map).await?;
    // Apply state.subst to both types before comparison — access-chain constraints
    // may have bound TypeVars in state.subst. Without substitution, the comparison
    // uses stale TypeVars.
    // Guard: skip allocation when subst is empty (common case for concrete programs).
    let (actual, expected_resolved) = if state.subst_is_empty() {
        (actual, expected.clone())
    } else {
        (state.apply(&actual), state.apply(expected))
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
        //
        let result = Box::pin(unify(
            &actual,
            &expected_resolved,
            state,
            constraints,
            node.span.clone(),
        ))
        .await;
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
        let (actual_resolved, expected_final) = if state.subst_is_empty() {
            (actual.clone(), expected_resolved.clone())
        } else {
            (
                state.apply(&actual),
                state.apply(&expected_resolved),
            )
        };

        let passes = Type::is_subtype(&actual_resolved, &expected_final, Some(&state.tycon_env))
            || ((contains_unknown_or_top(&actual_resolved)
                || contains_unknown_or_top(&expected_final))
                && Type::is_consistent(&actual_resolved, &expected_final));
        if !passes {
            Err(vec![TypeErrorTyped::UnificationFailure(
                UnificationFailure {
                    expected: expected_final.clone(),
                    got: actual_resolved.clone(),
                    span: node.span.clone(),
                    notes: vec![],
                    call_stack: vec![],
                },
            )])
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "typecheck_tests.rs"]
mod tests;
