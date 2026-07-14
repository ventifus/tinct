//! Type normalization and Display implementations.
//!
//! This module contains normalization logic for union/intersection types
//! and Display implementations for the Type enum.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use indexmap::IndexMap;

use crate::env::Env;
use crate::type_def::Type;
use crate::type_infer::TypeVarEntry;
use crate::value::Value;

/// Normalization context for type expressions.
///
/// Tracks state for TypeStageApp reduction and caching.
#[derive(Debug, Clone)]
pub struct NormCtxt {
    /// Cache for normalized types (ground types only)
    pub cache: HashMap<Type, Type>,
    /// Current normalization depth
    pub depth: u32,
    /// Maximum normalization depth before aborting
    pub max_depth: u32,
    /// Call stack for cycle detection (resolver function names)
    pub call_stack: Vec<String>,
    /// Type-stage evaluation environment for user-defined resolver functions.
    ///
    /// Contains bindings from `--- stage: type` sections of prelude.llt.
    /// When set, `normalize()` will call user-defined resolver functions to
    /// reduce `TypeStageApp` nodes. Results are memoized in `cache`.
    ///
    /// `None` during bootstrap (when the type-stage env is being built),
    /// when type-stage env creation fails, or when resolver evaluation is
    /// not needed (e.g., in tests that only normalize concrete types).
    pub type_stage_env: Option<Arc<RwLock<Env>>>,
    /// EvalContext for accessing the FlatEnv arena and type-stage function thunks.
    ///
    /// Needed by `evaluate_resolver` to construct ThunkIds from the type_stage_flat_env_id
    /// stored in the TypeContext. `None` when normalizing outside of an evaluation context
    /// (e.g., in tests).
    pub eval_ctx: Option<Arc<crate::eval::EvalContext>>,
    /// If false, disable resolver evaluation (prevents runtime errors from propagating into type inference).
    /// Set to false inside unify() to prevent evaluation failures from causing type errors.
    pub allow_eval: bool,
}

impl NormCtxt {
    /// Create a normalization context with the given type-stage env and eval context.
    ///
    /// Production callers pass `state.type_stage_env.clone()` and `state.eval_ctx.clone()`
    /// so that resolver functions defined in `--- stage: type` sections of prelude.llt are
    /// available. Test callers and bootstrap contexts where no env has been built yet pass
    /// `None` for both, which causes `TypeStageApp` nodes to remain stuck (resolver
    /// evaluation is skipped).
    pub fn new(
        type_stage_env: Option<Arc<RwLock<Env>>>,
        eval_ctx: Option<Arc<crate::eval::EvalContext>>,
    ) -> Self {
        Self {
            cache: HashMap::new(),
            depth: 0,
            max_depth: 64,
            call_stack: Vec::new(),
            type_stage_env,
            eval_ctx,
            allow_eval: true,
        }
    }
}

/// Normalize a type expression.
///
/// Performs the following steps in order:
/// 1. Apply current substitution to resolve bound TypeVars
/// 2. If the type is TypeStageApp { fn_name, args }:
///    - Normalize each arg recursively
///    - If all args are ground (no TypeVars) and no cycle detected, attempt reduction:
///      a. Call `evaluate_resolver()` to invoke the type-stage function.
///         Results are memoized by the outer `cache` (TypeStageApp → resolved type).
///      c. If evaluation fails, return stuck TypeStageApp
///         (caller can retry via deferred_equalities)
///    - If depth exceeded or cycle detected, return stuck TypeStageApp
/// 3. Cache the result (only for ground types)
///
/// Returns the normalized type.
#[allow(clippy::doc_overindented_list_items)] // multi-level numbered sub-list requires deeper indentation
pub fn normalize<'a>(
    ty: &'a Type,
    type_vars: &'a IndexMap<String, TypeVarEntry>,
    ctx: &'a mut NormCtxt,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Type> + 'a>> {
    Box::pin(async move {
        // Step 1: Apply current substitution
        let ty_substituted = crate::types::apply_substitution(ty, type_vars);

        // Check cache before computing (only for ground types)
        if !ty_substituted.has_inference_vars() {
            if let Some(cached) = ctx.cache.get(&ty_substituted) {
                return cached.clone();
            }
        }

        // Step 2: TypeStageApp reduction
        let result = match &ty_substituted {
            Type::TypeStageApp { fn_name, args } => {
                // Depth guard: if we've exceeded max depth, return stuck
                if ctx.depth >= ctx.max_depth {
                    return ty_substituted.clone();
                }

                // Cycle detection: if fn_name is already in the call stack, return stuck
                if ctx.call_stack.contains(fn_name) {
                    return ty_substituted.clone();
                }

                // Normalize each arg recursively
                ctx.depth += 1;
                let mut normalized_args: Vec<Type> = Vec::with_capacity(args.len());
                for arg in args.iter() {
                    normalized_args.push(Box::pin(normalize(arg, type_vars, ctx)).await);
                }
                ctx.depth -= 1;

                // Check if all args are ground
                let all_ground = normalized_args.iter().all(|arg| !arg.has_inference_vars());

                if all_ground {
                    // All args are ground — attempt reduction via resolver cache lookup
                    // Push fn_name to call stack for cycle detection
                    ctx.call_stack.push(fn_name.clone());

                    let result = if ctx.allow_eval {
                        if let (Some(env), Some(eval_ctx)) =
                            (ctx.type_stage_env.clone(), ctx.eval_ctx.clone())
                        {
                            if let Some(resolved) =
                                evaluate_resolver(fn_name, &normalized_args, &env, &eval_ctx).await
                            {
                                resolved
                            } else {
                                // Resolver evaluation failed — return stuck TypeStageApp
                                Type::TypeStageApp {
                                    fn_name: fn_name.clone(),
                                    args: normalized_args,
                                }
                            }
                        } else {
                            // No type-stage env or eval_ctx available — return stuck TypeStageApp
                            Type::TypeStageApp {
                                fn_name: fn_name.clone(),
                                args: normalized_args,
                            }
                        }
                    } else {
                        // allow_eval is false (inside unify) — return stuck TypeStageApp to prevent
                        // runtime errors from propagating into type inference
                        Type::TypeStageApp {
                            fn_name: fn_name.clone(),
                            args: normalized_args,
                        }
                    };

                    // Pop fn_name from call stack
                    ctx.call_stack.pop();

                    result
                } else {
                    // Not all args are ground — return stuck TypeStageApp with normalized args
                    Type::TypeStageApp {
                        fn_name: fn_name.clone(),
                        args: normalized_args,
                    }
                }
            }
            _ => ty_substituted.clone(),
        };

        // Step 3: Cache the result (only for ground types)
        if !result.has_inference_vars() {
            ctx.cache.insert(ty_substituted.clone(), result.clone());
        }

        result
    }) // end Box::pin(async move {
}

// normalize_union and normalize_intersection moved to impl Type in type_def.rs

/// Convert a `Type` to a TypeNode `Value` (T-1061).
///
/// Resolver functions receive TypeNode Variant values as arguments.
///
/// Handles primitive types. Complex types (App, Union, Record, etc.) return `None`.
/// Called directly by `evaluate_resolver`.
fn type_to_typenode(ty: &Type) -> Option<Value> {
    // Build a leaf TypeNode Variant (no payload) for the given tag name.
    let leaf = |tag: &str| -> Value {
        Value::Variant {
            tag: tag.to_string(),
            payload: None,
        }
    };

    match ty {
        Type::Int | Type::IntLiteral(_) => Some(leaf("TypeNode.Int")),
        Type::Float => Some(leaf("TypeNode.Float")),
        Type::Str | Type::StringLiteral(_) => Some(leaf("TypeNode.String")),
        Type::Unknown => Some(leaf("TypeNode.Unknown")),
        Type::Never => Some(leaf("TypeNode.Never")),
        Type::Any => Some(leaf("TypeNode.Any")),
        // Complex types — arithmetic resolvers never receive these.
        // Return None so evaluate_resolver returns None (resolver returns None → Unknown fallback).
        _ => None,
    }
}

/// Evaluate a user-defined type-stage resolver function.
///
/// Looks up `fn_name` in the type-stage environment, calls it with the given
/// `Type` arguments (converted to TypeNode Variant values per T-1061) and converts
/// the result back to a `Type` via `typenode_value_to_type`.
///
/// Returns `None` if any step fails:
/// - Resolver not found in env
/// - Argument type cannot be represented as a TypeNode value
/// - Runtime error during evaluation
/// - Result cannot be converted back to a `Type`
pub(crate) async fn evaluate_resolver(
    fn_name: &str,
    args: &[Type],
    env: &Arc<RwLock<Env>>,
    eval_ctx: &Arc<crate::eval::EvalContext>,
) -> Option<Type> {
    // Step 1: Walk the Env parent chain to find fn_name and its depth.
    //
    // Depth 0 = leaf Env (the one pointed to by type_stage_flat_env_id).
    // Depth 1 = its parent, depth 2 = grandparent, etc.
    //
    // This handles the general case where type-stage docs span multiple sequential
    // dicts (each creating a child Env). The leaf Env only holds the LAST doc's
    // bindings; earlier docs are in ancestor Envs.
    let (slot_index, depth) = {
        let env_read = env.read().unwrap();
        if let Some(idx) = env_read.slots.get_index_of(fn_name) {
            // Found in the leaf Env (depth 0)
            (idx, 0usize)
        } else {
            // Walk the parent chain
            let mut current_parent = env_read.parent.as_ref().map(Arc::clone);
            drop(env_read);
            let mut depth = 1usize;
            loop {
                let parent_arc = current_parent?;
                let parent_read = parent_arc.read().unwrap();
                if let Some(idx) = parent_read.slots.get_index_of(fn_name) {
                    break (idx, depth);
                }
                depth += 1;
                current_parent = parent_read.parent.as_ref().map(Arc::clone);
                drop(parent_read);
            }
        }
    };

    // Step 2: Get the type_stage_flat_env_id from the TypeContext.
    // This is the FlatEnv EnvId of the leaf (last-evaluated) type-stage doc.
    let type_stage_flat_env_id = {
        let tc_guard = eval_ctx.type_context.lock().unwrap();
        let tc_data = tc_guard.as_ref()?;
        tc_data.type_stage_flat_env_id?
    };

    // Step 3: Construct a ThunkId for the resolver function.
    //
    // Walk the parent chain `depth` hops from the leaf FlatEnv to reach the ancestor scope
    // that owns the resolver function at `slot_index`.
    //
    // ThunkId.scope_id is a u32 (raw ScopeArena index), not a ScopeId wrapper.
    //
    // Invariant — Env parent chain depth equals FlatEnv parent chain depth:
    //   Each builtin-eval call allocates exactly one FlatEnv (via builtin-extend-env with
    //   flat-env: set to the prior call's flat-env-id). The Env chain grows one hop per
    //   builtin-eval call, and the FlatEnv parent chain grows one hop per alloc_child call.
    //   These two parallel chains are always kept in sync by the loader: every
    //   builtin-eval → builtin-extend-env pair produces exactly one Env hop and one
    //   FlatEnv parent hop. Therefore walking `depth` parent hops from the leaf always
    //   reaches the FlatEnv at Env depth `depth`.
    //
    //   This mapping would break only if builtin-extend-env were called without flat-env:
    //   (causing an alloc_root instead of alloc_child, severing the parent chain). The
    //   loader and test-loader always pass flat-env: — verified by code review of
    //   loader.llt and test-loader.llt. The type-stage evaluation path does not omit
    //   flat-env: for any document after the initial bootstrap.
    let resolver_thunk_id = {
        let arena_borrow = eval_ctx.scope_arena.borrow();
        match arena_borrow.walk_parent_chain(type_stage_flat_env_id, depth) {
            Err(_) => {
                // Depth exceeds parent chain — fn_name not reachable
                return None;
            }
            Ok(target_env_id) => crate::arena::ThunkId {
                scope_id: target_env_id.0,
                slot: slot_index as u32,
            },
        }
    };

    // Step 4: Get the resolver function thunk from the arena
    let resolver_thunk = eval_ctx.scope_arena.borrow().get_thunk(resolver_thunk_id);

    // Step 5: Convert Type args to TypeNode values
    let type_args: Vec<Value> = args.iter().filter_map(|ty| type_to_typenode(ty)).collect();
    if type_args.len() != args.len() {
        // At least one type couldn't be converted to a TypeNode value
        return None;
    }

    // Step 6: Allocate arg ThunkIds in the arena and call the resolver
    let arg_thunk_ids: Vec<crate::arena::ThunkId> = type_args
        .into_iter()
        .map(|val| {
            eval_ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                val,
                crate::rust_span!(),
            )))
        })
        .collect();

    // Materialize the resolver thunk to get the Function value
    let resolver_val = crate::eval::materialize(&resolver_thunk, None, eval_ctx)
        .await
        .ok()?;

    // Dispatch: resolver must be a Function
    let (params, body, closure_env_id) = match resolver_val {
        Value::Function {
            params,
            body,
            closure_env_id,
            ..
        } => (params, body, closure_env_id),
        _ => return None,
    };

    // Call the resolver function via invoke_function
    use crate::eval_call::{invoke_function, CallContext};
    let call_ctx = CallContext {
        params: &params,
        body: &body,
        closure_env_id,
        positional: &arg_thunk_ids,
        named: None,
        default_env_id: closure_env_id,
        call_span: crate::rust_span!(),
        origin: None,
        ctx: eval_ctx,
    };
    let result_thunk = invoke_function(&call_ctx).await.ok()?;

    // Force the result
    let result_val = crate::eval::materialize(&result_thunk, None, eval_ctx)
        .await
        .ok()?;

    // Step 7: Convert result TypeNode Value back to Type
    match &result_val {
        Value::Variant { tag, payload: None } => {
            match tag.as_str() {
                "TypeNode.Int" | "TypeNode.Integer" => Some(Type::Int),
                "TypeNode.Float" => Some(Type::Float),
                "TypeNode.String" | "TypeNode.Str" => Some(Type::Str),
                // TypeNode.Bool has no direct Type equivalent — fall through to None
                "TypeNode.Never" => Some(Type::Never),
                // TypeNode.Unknown → Type::Unknown (gradual ?); TypeNode.Any → Type::Any (top).
                // Distinct semantics: Unknown is the gradual type (bottom of the info lattice),
                // Any is the unconstrained top (τ <: Any for all τ). Keep them separate.
                "TypeNode.Unknown" => Some(Type::Unknown),
                "TypeNode.Any" => Some(Type::Any),
                _ => None,
            }
        }
        _ => None,
    }
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Integer"),
            Type::IntLiteral(n) => write!(f, "{}", n),
            Type::Float => write!(f, "Float"),
            Type::Str => write!(f, "String"),
            Type::StringLiteral(s) => write!(f, "\"{}\"", s),
            Type::Bytes => write!(f, "Bytes"),
            Type::Dict(row) => {
                write!(f, "[")?;
                let mut sorted: Vec<_> = row.fields.iter().collect();
                sorted.sort_by_key(|(k, _)| *k);
                for (i, (key, ty)) in sorted.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}: {}", key, ty)?;
                }
                // Display RowTail::Uniform as `...@[Dict K V]` where K is the key type
                // (Any when unconstrained). Mirrors tinct's annotation convention and uses
                // Dict — a type programmers already know — rather than Map (not user-visible).
                if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                    if !row.fields.is_empty() {
                        write!(f, " ")?;
                    }
                    let key_str = key
                        .as_ref()
                        .map(|k| format!("{}", k))
                        .unwrap_or_else(|| "Any".to_string());
                    write!(f, "...@[Dict {} {}]", key_str, value)?;
                }
                write!(f, "]")
            }
            Type::Function {
                params,
                ret,
                variadic,
                required_count: _,
            } => {
                // Parenthesize nested function return types for clarity
                match **ret {
                    Type::Function { .. } => write!(f, "Fn@({}) [", ret)?,
                    _ => write!(f, "Fn@{} [", ret)?,
                }
                for (i, (name_opt, param_ty)) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    // Parenthesize nested function types in parameter position
                    match param_ty {
                        Type::Function { .. } => {
                            if let Some(name) = name_opt {
                                write!(f, "{}: ({})", name, param_ty)?
                            } else {
                                write!(f, "({})", param_ty)?
                            }
                        }
                        _ => {
                            if let Some(name) = name_opt {
                                write!(f, "{}: {}", name, param_ty)?
                            } else {
                                write!(f, "{}", param_ty)?
                            }
                        }
                    }
                }
                if *variadic {
                    write!(f, " ...")?;
                }
                write!(f, "]")
            }
            Type::Proxy => write!(f, "Proxy"),
            Type::TypeVar(name, _level) => write!(f, "{}", name),
            Type::Unknown => write!(f, "Unknown"),
            Type::Any => write!(f, "Any"),
            Type::Error(errs) => {
                if let Some(first) = errs.first() {
                    write!(f, "<error: {}>", first.message())
                } else {
                    write!(f, "<error>")
                }
            }
            Type::Union(types) => {
                // Use tinct annotation syntax: [or T1 T2 ...]
                write!(f, "[or")?;
                for ty in types.iter() {
                    write!(f, " {}", ty)?;
                }
                write!(f, "]")
            }
            Type::Intersection(types) => {
                // Use tinct annotation syntax: [all T1 T2 ...]
                write!(f, "[all")?;
                for ty in types.iter() {
                    write!(f, " {}", ty)?;
                }
                write!(f, "]")
            }
            Type::DirCap => write!(f, "DirCap"),
            Type::NetCap => write!(f, "NetCap"),
            Type::Uri => write!(f, "Uri"),
            Type::Timestamp => write!(f, "Timestamp"),
            Type::Duration => write!(f, "Duration"),
            Type::ClockCap => write!(f, "ClockCap"),
            Type::Timezone => write!(f, "Timezone"),
            Type::QuicSession => write!(f, "QuicSession"),
            Type::Http2Session => write!(f, "Http2Session"),
            Type::Http3Session => write!(f, "Http3Session"),
            Type::QuicDatagramHandle => write!(f, "QuicDatagramHandle"),
            Type::DatagramHandle => write!(f, "DatagramHandle"),
            Type::Negation(inner) => {
                // Parenthesize complex inner types for clarity
                match **inner {
                    Type::Union(_) | Type::Intersection(_) | Type::Negation(_) => {
                        write!(f, "~({})", inner)
                    }
                    _ => write!(f, "~{}", inner),
                }
            }
            Type::Never => write!(f, "Never"),
            Type::NominalVariant { tag, .. } => write!(f, "{}", tag),
            Type::App(func, arg) => {
                if let Some((k, v)) = self.as_map() {
                    return write!(f, "Map[{} {}]", k, v);
                }
                if let Some(cap) = self.as_handle() {
                    if matches!(cap, Type::Unknown) {
                        return write!(f, "Handle");
                    }
                    return write!(f, "Handle[{}]", cap);
                }

                // General case: TyCon applications show as Name[Arg, ...] instead of App[TyCon(Name) Arg].
                // Collect the full curried spine: App(App(...App(TyCon(name), a1)..., an-1), an).
                {
                    let mut args_rev: Vec<&Type> = vec![arg.as_ref()];
                    let mut cur: &Type = func.as_ref();
                    loop {
                        match cur {
                            Type::App(inner_func, inner_arg) => {
                                args_rev.push(inner_arg.as_ref());
                                cur = inner_func.as_ref();
                            }
                            Type::TyCon(name) => {
                                args_rev.reverse();
                                write!(f, "{name}[")?;
                                for (i, a) in args_rev.iter().enumerate() {
                                    if i > 0 {
                                        write!(f, ", ")?;
                                    }
                                    write!(f, "{a}")?;
                                }
                                return write!(f, "]");
                            }
                            _ => break,
                        }
                    }
                }
                write!(f, "[{} {}]", func, arg)
            }
            Type::TyCon(name) => write!(f, "{}", name),
            Type::Operator(name) => write!(f, "{}", name),
            Type::TypeStageApp { fn_name, args } => {
                write!(f, "{}(", fn_name)?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", arg)?;
                }
                write!(f, ")")
            }
            // S-860: equirecursive-types-core
            // Display as `μVarName.Body`, extracting the human-readable alias name from the
            // gensym'd binder (e.g., "𝜇ꜱʏᴍ⧼IntList⧽42" → displayed as "μIntList").
            // This matches the notation used in doc/whatif/equirecursive-types.md.
            Type::Recursive { var, body } => {
                // Extract the alias name from the gensym tag: "𝜇ꜱʏᴍ⧼NAME⧽N" → "NAME".
                // Falls back to the full var name if the format doesn't match.
                let display_name = var
                    .strip_prefix('𝜇')
                    .and_then(|s| s.strip_prefix("ꜱʏᴍ⧼"))
                    .and_then(|s| s.find('⧽').map(|i| &s[..i]))
                    .unwrap_or(var.as_str());
                write!(f, "μ{}.{}", display_name, body)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Kind;
    use std::collections::HashSet;

    /// Helper: create an empty type_vars map for testing
    fn empty_type_vars() -> IndexMap<String, TypeVarEntry> {
        IndexMap::new()
    }

    async fn norm(
        ty: &Type,
        type_vars: &IndexMap<String, TypeVarEntry>,
        ctx: &mut NormCtxt,
    ) -> Type {
        normalize(ty, type_vars, ctx).await
    }

    /// Test: normalize(Int, type_vars, ctx) returns Int unchanged
    #[tokio::test]
    async fn test_normalize_identity_concrete_type() {
        let tv = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);
        let ty = Type::Int;
        let result = norm(&ty, &tv, &mut ctx).await;
        assert_eq!(result, Type::Int);
    }

    /// Test: normalize(TypeVar("a"), type_vars, ctx) returns the bound type if "a" is bound
    #[tokio::test]
    async fn test_normalize_substitution() {
        let mut tv = IndexMap::new();
        {
            let mut entry = TypeVarEntry::blank(0, Kind::Type);
            entry.binding = Some(Type::Str);
            tv.insert("a".to_string(), entry);
        }
        let mut ctx = NormCtxt::new(None, None);
        let ty = Type::TypeVar("a".to_string(), 0);
        let result = norm(&ty, &tv, &mut ctx).await;
        assert_eq!(result, Type::Str);
    }

    /// Test: normalize() cache - second call returns cached result
    #[tokio::test]
    async fn test_normalize_cache() {
        let tv = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);
        let ty = Type::Int;

        // First call - populates cache
        let result1 = norm(&ty, &tv, &mut ctx).await;
        assert_eq!(result1, Type::Int);

        // Second call - should return cached result
        let result2 = norm(&ty, &tv, &mut ctx).await;
        assert_eq!(result2, Type::Int);

        // Verify cache entry exists (use concrete Int, not Seq)
        assert!(ctx.cache.contains_key(&Type::Int));
    }

    /// Test: normalize() cycle detection - TypeStageApp with fn_name already in call_stack returns stuck
    #[tokio::test]
    async fn test_normalize_cycle_detection() {
        let tv = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);

        // Manually push "Recursive" to call_stack to simulate cycle
        ctx.call_stack.push("Recursive".to_string());

        let ty = Type::TypeStageApp {
            fn_name: "Recursive".to_string(),
            args: vec![Type::Int],
        };

        let result = norm(&ty, &tv, &mut ctx).await;

        // Should return stuck (unchanged) due to cycle detection
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "Recursive".to_string(),
                args: vec![Type::Int],
            }
        );
    }

    /// Test: normalize() depth guard - depth > max_depth returns stuck
    #[tokio::test]
    async fn test_normalize_depth_guard() {
        let tv = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);

        // Set depth to max_depth
        ctx.depth = ctx.max_depth;

        let ty = Type::TypeStageApp {
            fn_name: "Deep".to_string(),
            args: vec![Type::Int],
        };

        let result = norm(&ty, &tv, &mut ctx).await;

        // Should return stuck (unchanged) due to depth exceeded
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "Deep".to_string(),
                args: vec![Type::Int],
            }
        );
    }

    /// Test: has_type_stage_app() returns true for TypeStageApp
    #[tokio::test]
    async fn test_has_type_stage_app_true_for_type_stage_app() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert!(ty.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns false for concrete types
    #[tokio::test]
    async fn test_has_type_stage_app_false_for_concrete() {
        assert!(!Type::Int.has_type_stage_app());
        assert!(!Type::Str.has_type_stage_app());
        assert!(!Type::Float.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns true for App(TyCon, TypeStageApp)
    #[tokio::test]
    async fn test_has_type_stage_app_true_for_app_containing_type_stage_app() {
        let ty = Type::App(
            Box::new(Type::TyCon("Box".into())),
            Box::new(Type::TypeStageApp {
                fn_name: "AddResult".to_string(),
                args: vec![Type::Int, Type::Float],
            }),
        );
        assert!(ty.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns false for App(TyCon, Int)
    #[tokio::test]
    async fn test_has_type_stage_app_false_for_app_of_concrete() {
        let ty = Type::App(Box::new(Type::TyCon("Box".into())), Box::new(Type::Int));
        assert!(!ty.has_type_stage_app());
    }

    /// Test: TypeStageApp PartialEq - same fn_name+args equal
    #[tokio::test]
    async fn test_type_stage_app_partial_eq_same() {
        let ty1 = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let ty2 = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert_eq!(ty1, ty2);
    }

    /// Test: TypeStageApp PartialEq - different fn_name not equal
    #[tokio::test]
    async fn test_type_stage_app_partial_eq_different_fn_name() {
        let ty1 = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let ty2 = Type::TypeStageApp {
            fn_name: "SubResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert_ne!(ty1, ty2);
    }

    /// Test: TypeStageApp PartialEq - different args not equal
    #[tokio::test]
    async fn test_type_stage_app_partial_eq_different_args() {
        let ty1 = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let ty2 = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Float, Type::Int],
        };
        assert_ne!(ty1, ty2);
    }

    /// Test: TypeStageApp Display format
    #[tokio::test]
    async fn test_type_stage_app_display() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "AddResult(Integer, Float)");
    }

    /// Test: TypeStageApp Display format with single arg
    #[tokio::test]
    async fn test_type_stage_app_display_single_arg() {
        let ty = Type::TypeStageApp {
            fn_name: "Singleton".to_string(),
            args: vec![Type::Int],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "Singleton(Integer)");
    }

    /// Test: TypeStageApp Display format with no args
    #[tokio::test]
    async fn test_type_stage_app_display_no_args() {
        let ty = Type::TypeStageApp {
            fn_name: "Nullary".to_string(),
            args: vec![],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "Nullary()");
    }

    /// Test: normalize() with non-ground TypeStageApp args returns stuck
    #[tokio::test]
    async fn test_normalize_type_stage_app_non_ground_args() {
        let subst = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::TypeVar("a".to_string(), 0), Type::Float],
        };
        let result = norm(&ty, &subst, &mut ctx).await;
        // Non-ground args - should return TypeStageApp with normalized args (but stuck)
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "AddResult".to_string(),
                args: vec![Type::TypeVar("a".to_string(), 0), Type::Float],
            }
        );
    }

    /// Test: NormCtxt::new() initializes with correct defaults
    #[tokio::test]
    async fn test_norm_ctxt_new_defaults() {
        let ctx = NormCtxt::new(None, None);

        assert!(ctx.cache.is_empty());
        assert_eq!(ctx.depth, 0);
        assert_eq!(ctx.max_depth, 64);
        assert!(ctx.call_stack.is_empty());
    }

    /// Test: cache only stores ground types
    #[tokio::test]
    async fn test_normalize_cache_only_ground_types() {
        let subst = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);

        // Normalize a type with inference variables
        let ty_with_var = Type::App(
            Box::new(Type::TyCon("Box".into())),
            Box::new(Type::TypeVar("a".to_string(), 0)),
        );
        let _result1 = norm(&ty_with_var, &subst, &mut ctx).await;

        // Cache should NOT contain this type (has inference vars)
        assert!(!ctx.cache.contains_key(&ty_with_var));

        // Normalize a ground type
        let ty_ground = Type::App(Box::new(Type::TyCon("Box".into())), Box::new(Type::Int));
        let _result2 = norm(&ty_ground, &subst, &mut ctx).await;

        // Cache SHOULD contain this type (ground)
        assert!(ctx.cache.contains_key(&ty_ground));
    }

    /// Test: TypeStageApp collect_type_vars
    #[tokio::test]
    async fn test_type_stage_app_collect_type_vars() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![
                Type::TypeVar("a".to_string(), 0),
                Type::Int,
                Type::TypeVar("b".to_string(), 1),
            ],
        };

        let mut vars = HashSet::new();
        ty.collect_type_vars(&mut vars);

        assert_eq!(vars.len(), 2);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
    }

    /// Test: TypeStageApp has_inference_vars
    #[tokio::test]
    async fn test_type_stage_app_has_inference_vars_true() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::TypeVar("a".to_string(), 0), Type::Int],
        };
        assert!(ty.has_inference_vars());
    }

    /// Test: TypeStageApp has_inference_vars false for ground args
    #[tokio::test]
    async fn test_type_stage_app_has_inference_vars_false() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert!(!ty.has_inference_vars());
    }

    /// Test: unknown resolver returns stuck TypeStageApp (no env → allow_eval path skipped)
    #[tokio::test]
    async fn test_unknown_resolver_returns_stuck() {
        let subst = empty_type_vars();
        let mut ctx = NormCtxt::new(None, None);
        let ty = Type::TypeStageApp {
            fn_name: "UnknownResolver".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let result = norm(&ty, &subst, &mut ctx).await;
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "UnknownResolver".to_string(),
                args: vec![Type::Int, Type::Float],
            }
        );
    }
}
