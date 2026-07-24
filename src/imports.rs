//! Bootstrap type environment for the type checker.
//!
//! Provides `get_builtin_core_type_env()` which parses and type-checks
//! `stdlib/builtin_core.llt` to build the initial type environment seeded
//! with core type definitions (`Handle`, `DirCap`, and other prelude-defined types).
//!
//! Include resolution for user programs is handled by the prelude's `include`
//! function, which runs the full pipeline (parse → desugar → resolve →
//! typecheck → eval) via `builtin-typecheck-doc`.

use std::cell::RefCell;
use std::sync::{Arc, RwLock};

use crate::env::Env;
use crate::typecheck::typecheck_surface_program_with_env;

// Thread-local cache for the builtin_core.llt type environment (T-1366 Rust step 2 bootstrap).
// Populated on first call to `get_builtin_core_type_env()`. Once built, all subsequent
// calls on the same thread return an `Arc::clone` without re-parsing or re-typechecking.
thread_local! {
    static BUILTIN_CORE_TYPE_ENV: RefCell<Option<Arc<RwLock<Env>>>> = const { RefCell::new(None) };
    /// TyConEnv from type-checking builtin_core.llt — contains opaque type defs like BuilderHandle.
    static BUILTIN_CORE_TYCON_ENV: RefCell<Option<crate::type_def::TyConEnv>> = const { RefCell::new(None) };
    /// Recursion guard: prevents re-entrant calls from within the typecheck of builtin_core.llt.
    static BUILDING_BUILTIN_CORE_ENV: RefCell<bool> = const { RefCell::new(false) };
}

/// Return the TyConEnv from type-checking `stdlib/builtin_core.llt`.
/// Returns None if `get_builtin_core_type_env` has not yet been called.
pub fn get_builtin_core_tycon_env() -> Option<crate::type_def::TyConEnv> {
    BUILTIN_CORE_TYCON_ENV.with(|c| c.borrow().clone())
}

/// T-1366 Rust step 2 bootstrap: type-check `stdlib/builtin_core.llt` and return the
/// resulting `TypeEnv` so that prelude-defined types, `Handle`, `builtin-raise`, etc.
/// are visible to the prelude type-checker by their bare names.
///
/// Uses `include_str!` so the file is embedded at compile time — no runtime libdir access
/// needed. The result is cached thread-locally; subsequent calls return `Arc::clone` in O(1).
///
/// Returns `None` if:
/// - A re-entrant call is detected (recursion guard).
/// - Parsing or resolution fails (rare; the file is compiled-in and known-good).
pub async fn get_builtin_core_type_env() -> Option<Arc<RwLock<Env>>> {
    // Fast path: return cached result.
    let cached = BUILTIN_CORE_TYPE_ENV.with(|c| c.borrow().clone());
    if let Some(env) = cached {
        return Some(env);
    }

    // Recursion guard: prevent re-entrant calls (e.g. from within builtin_core.llt typecheck).
    let already_building = BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow());
    if already_building {
        return None;
    }
    BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow_mut() = true);

    let result = build_builtin_core_type_env_inner().await;

    BUILDING_BUILTIN_CORE_ENV.with(|f| *f.borrow_mut() = false);

    if let Some(ref env) = result {
        BUILTIN_CORE_TYPE_ENV.with(|c| *c.borrow_mut() = Some(Arc::clone(env)));
    }

    result
}

/// Inner implementation of `get_builtin_core_type_env`.
///
/// Parses `stdlib/builtin_core.llt` (embedded at compile time via `include_str!`),
/// runs the full pipeline (desugar → resolve → typecheck), and returns the
/// resulting `Arc<RwLock<Env>>` with the new type declarations merged on top of
/// `build_builtins_type_env_arc()` as the parent.
async fn build_builtin_core_type_env_inner() -> Option<Arc<RwLock<Env>>> {
    // Embedded source — no libdir access needed at runtime.
    let source = include_str!("../stdlib/builtin_core.llt");
    let sf: Arc<str> = Arc::from("stdlib/builtin_core.llt");

    // Parse — extract .program from ParseOutput
    let program =
        crate::desugar::desugar_program_full(&crate::parser::parse(source, sf).ok()?.program);

    // Empty parent — builtin_core.llt is the source of truth. Primitives are hardcoded
    // in resolve_type_name; types declared within the file resolve via state.tycon_env.
    let parent_env = Arc::new(RwLock::new(crate::env::Env::new()));

    // Create EvalContext for type-stage evaluation. No filesystem access needed —
    // type-stage eval only constructs TypeNode values in memory (no IO builtins).
    let type_stage_eval_ctx = crate::eval::EvalContext::new_empty();

    // ONE resolve pass on the FULL program with the eval root frame as the outer scope.
    // This sets OnceLock coordinates on ALL nodes (including type-stage ones) relative to
    // the EvalContext's root group, exactly as run_loader_pipeline does. The resulting
    // scope frames are threaded into eval_ctx_with_frames so the evaluator can resolve
    // VarRefs at any level — including unit constructor accesses like TypeNode.Int which
    // require a working outer-frame lookup chain.
    let eval_ctx_with_frames: std::sync::Arc<crate::eval::EvalContext> = {
        let root_frame = type_stage_eval_ctx.root_group_resolver_map();
        let (_table, new_frames) =
            crate::resolve::resolve_surface_program(&program, std::slice::from_ref(&root_frame));
        let all_frames: Vec<indexmap::IndexMap<String, u32>> =
            std::iter::once(root_frame).chain(new_frames).collect();
        type_stage_eval_ctx.with_scope_frames(std::sync::Arc::new(all_frames))
    };

    // Filter type-stage documents from the already-resolved full program.
    // The OnceLocks are already set by the resolve above — no separate resolve needed.
    let ts_docs: Vec<_> = program
        .documents
        .iter()
        .filter(|d| {
            d.node.header.get("stage").is_some_and(|stage_node| {
                matches!(
                    &stage_node.expr,
                    crate::ast::SurfaceExpression::StringLiteral { content, .. }
                    if content == "type"
                )
            })
        })
        .cloned()
        .collect();

    // Build type_stage_scope by evaluating the type-stage documents with the correct context.
    // eval_ctx_with_frames has the scope frames from the resolve pass, so VarRefs at any
    // nesting level resolve correctly — including unit constructors (TypeNode.Int) which
    // require outer-group frame lookup, not just builtins.
    let type_stage_scope: Vec<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>> =
        if ts_docs.is_empty() {
            Vec::new()
        } else {
            let ts_program = crate::ast::SurfaceProgram { documents: ts_docs };
            let ts_thunk = crate::eval::eval_surface_file(&ts_program, &eval_ctx_with_frames).await.ok()?;
            let ts_val = crate::eval::materialize(&ts_thunk, None, &eval_ctx_with_frames).await.ok()?;
            match ts_val {
                crate::value::Value::Dict(entries) => {
                    let mut map = std::collections::HashMap::new();
                    for (key, thunk) in &entries {
                        if let crate::value::HashableValue::Str(name) = key {
                            let val = crate::eval::materialize(thunk, None, &eval_ctx_with_frames).await.ok()?;
                            if let Some(ty) =
                                crate::type_normalize::typenode_leaf_to_type(&val)
                            {
                                map.insert(
                                    name.to_string(),
                                    crate::type_infer::TypeStageEntry::Resolved(ty),
                                );
                            } else if let Some(kind) =
                                crate::type_normalize::typenode_typevar_kind(&val)
                            {
                                map.insert(
                                    name.to_string(),
                                    crate::type_infer::TypeStageEntry::TypeVar(kind),
                                );
                            } else if matches!(val, crate::value::Value::Function { .. }) {
                                map.insert(
                                    name.to_string(),
                                    crate::type_infer::TypeStageEntry::Function(
                                        std::sync::Arc::clone(thunk),
                                    ),
                                );
                            }
                        }
                    }
                    vec![map]
                }
                _ => Vec::new(),
            }
        };

    // Typecheck with builtins env as parent.
    // enable_hover_map=false (no LSP hover needed for bootstrap).
    let (_diagnostics, _type_map, _doc_map, _scheme_map, mut state, final_env, _annot) =
        typecheck_surface_program_with_env(
            &program,
            parent_env,
            false,                            // enable_hover_map
            std::collections::HashMap::new(), // seed_tycon_env: empty at bootstrap
            None,                             // eval_ctx: no EvalContext at bootstrap
            Some(type_stage_scope),           // type_stage_scope from evaluating type-stage docs
        )
        .await;

    // Register opaque builtin TyConDefs. These types are declared as TypeNode leaf
    // constructors in the type-stage (not as [type X] in the runtime dict), so the
    // typecheck pass does not create their TyConDefs automatically.
    // builtin_type: Some(discriminant) enables value_matches_type dispatch.
    // or_insert_with: if a [type X] declaration already registered an entry (e.g., during
    // migration), keep it; once tinct-side is fully migrated, all entries will be absent.
    use crate::type_def::TyConDef;
    let opaque_types: &[(&str, &str)] = &[
        ("Program", "Program"),
        ("Document", "Document"),
        ("TypeContext", "TypeContext"),
        ("DirCap", "DirCap"),
        ("NetCap", "NetCap"),
        ("Handle", "Handle"),
        ("File", "File"),
        ("BuilderHandle", "BuilderHandle"),
        ("Task", "Task"),
        ("Channel", "Channel"),
        ("Context", "Context"),
        ("ReactiveCell", "ReactiveCell"),
        ("ClockCap", "ClockCap"),
        ("Timezone", "Timezone"),
        ("Decimal", "Decimal"),
        ("BigInt", "BigInt"),
        ("QuicSession", "QuicSession"),
        ("QuicDatagramHandle", "QuicDatagramHandle"),
        ("Http2Session", "Http2Session"),
        ("Http3Session", "Http3Session"),
        ("Uri", "Uri"),
        ("Urn", "Urn"),
    ];
    for (name, discriminant) in opaque_types {
        state.tycon_env.entry(name.to_string()).or_insert_with(|| {
            std::sync::Arc::new(TyConDef {
                params: vec![],
                body: crate::types::Type::Unknown,
                constraints: vec![],
                variance: vec![],
                constructors: vec![],
                builtin_type: Some(discriminant.to_string()),
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            })
        });
    }

    // Cache the tycon_env so callers can seed it into downstream typechecks.
    BUILTIN_CORE_TYCON_ENV.with(|c| {
        *c.borrow_mut() = Some(state.tycon_env.clone());
    });

    // `final_env` is the child Env containing parent bindings plus new type declarations.
    Some(final_env)
}
