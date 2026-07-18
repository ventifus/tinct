//! Type normalization and Display implementations.
//!
//! This module contains normalization logic for union/intersection types
//! and Display implementations for the Type enum.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use indexmap::IndexMap;

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
    /// EvalContext for accessing the scope arena (ScopeArena) and type-stage function thunks.
    pub eval_ctx: Option<Arc<crate::eval::EvalContext>>,
    /// Pre-built map from type-stage name → ThunkId for O(1) resolver lookup.
    /// Populated from `InferState.type_stage_map` by production callers in type_unify.rs.
    /// `None` in test/bootstrap contexts — TypeStageApp nodes remain stuck.
    pub type_stage_map: Option<std::collections::HashMap<String, crate::type_infer::TypeStageEntry>>,
    /// If false, disable resolver evaluation (prevents runtime errors from propagating into type inference).
    /// Set to false inside unify() to prevent evaluation failures from causing type errors.
    pub allow_eval: bool,
}

impl NormCtxt {
    /// Create a normalization context with the given eval context.
    ///
    /// Production callers pass `state.eval_ctx.clone()` so that resolver functions
    /// are looked up via the type-stage scope chain. Test callers and bootstrap contexts
    /// pass `None`, which causes `TypeStageApp` nodes to remain stuck.
    pub fn new(eval_ctx: Option<Arc<crate::eval::EvalContext>>) -> Self {
        Self {
            cache: HashMap::new(),
            depth: 0,
            max_depth: 64,
            call_stack: Vec::new(),
            eval_ctx,
            type_stage_map: None,
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
///      a. Look up `fn_name` in the type-stage scope chain via `ctx.eval_ctx`.
///      b. Materialize the resolver thunk and call `call_strict_resolver`.
///      c. If evaluation fails or the name is not found, return stuck TypeStageApp
///         (caller can retry via deferred_equalities)
///    - If depth exceeded or cycle detected, return stuck TypeStageApp
/// 3. Cache the result (only for ground types)
///
/// Returns the normalized type.
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
                        if let (Some(ref map), Some(eval_ctx)) =
                            (&ctx.type_stage_map, ctx.eval_ctx.clone())
                        {
                            // O(1) lookup in the pre-built type_stage_map.
                            let resolved = if let Some(entry) = map.get(fn_name) {
                                match entry {
                                    crate::type_infer::TypeStageEntry::Function(thunk_id) => {
                                        evaluate_resolver_with_thunk(
                                            *thunk_id,
                                            &normalized_args,
                                            &eval_ctx,
                                        )
                                        .await
                                    }
                                    crate::type_infer::TypeStageEntry::Resolved(ty) => {
                                        if normalized_args.is_empty() {
                                            Some(ty.clone())
                                        } else {
                                            let mut result = ty.clone();
                                            for arg in &normalized_args {
                                                result = Type::App(
                                                    Box::new(result),
                                                    Box::new(arg.clone()),
                                                );
                                            }
                                            Some(result)
                                        }
                                    }
                                }
                            } else {
                                None
                            };
                            resolved.unwrap_or_else(|| Type::TypeStageApp {
                                fn_name: fn_name.clone(),
                                args: normalized_args,
                            })
                        } else {
                            // No type_stage_map or eval_ctx — return stuck TypeStageApp
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
/// Handles primitive types. Complex types (App, Union, Record, etc.) return `None`.
fn type_to_typenode(ty: &Type) -> Option<Value> {
    // Build a leaf TypeNode Variant (no payload) for the given constructor name.
    // tycon is always "TypeNode" for all TypeNode variants.
    let leaf = |ctor: &str| -> Value {
        Value::Variant {
            tycon: "TypeNode".to_string(),
            ctor: ctor.to_string(),
            payload: None,
        }
    };

    match ty {
        Type::Int | Type::IntLiteral(_) => Some(leaf("Int")),
        Type::Float => Some(leaf("Float")),
        Type::Str | Type::StringLiteral(_) => Some(leaf("String")),
        Type::Unknown => Some(leaf("Unknown")),
        Type::Never => Some(leaf("Never")),
        Type::Any => Some(leaf("Any")),
        _ => None,
    }
}

/// Call a type-stage resolver function strictly (no lazy evaluation machinery).
///
/// Type resolver functions (e.g. Seq, Result — parameterized type constructors) are pure
/// functions that take TypeNode values and return TypeNode values. They don't need the full
/// lazy call frame — arguments are concrete values, not thunks that might diverge.
///
/// The call frame is allocated as a child of the closure scope so variable lookup works
/// correctly, and is dropped after the result is obtained. No scope 0 mutation occurs.
pub(crate) async fn call_strict_resolver(
    resolver_val: Value,
    args: &[crate::type_def::Type],
    eval_ctx: &Arc<crate::eval::EvalContext>,
) -> Option<crate::type_def::Type> {
    // Leaf value: convert directly without calling anything.
    if let Some(ty) = typenode_leaf_to_type(&resolver_val) {
        if args.is_empty() {
            return Some(ty);
        }
        // Leaf with args: apply them as App chain.
        let mut result = ty;
        for arg in args {
            result = crate::type_def::Type::App(Box::new(result), Box::new(arg.clone()));
        }
        return Some(result);
    }

    if args.is_empty() {
        return None;
    }

    // Must be a parameterized type constructor (Function).
    let (params, body, closure_env_id) = match resolver_val {
        Value::Function {
            params,
            body,
            closure_env_id,
            ..
        } => (params, body, closure_env_id),
        _ => return None,
    };

    if params.len() != args.len() {
        return None;
    }

    // Convert Type args to TypeNode values.
    let type_args: Vec<Value> = args.iter().filter_map(|ty| type_to_typenode(ty)).collect();
    if type_args.len() != args.len() {
        return None;
    }

    // Allocate a call frame as a child of the closure scope.
    // Arg thunks go directly into this frame under the param names — no scope 0 mutation.
    // Function arguments are rebindings: we push each arg value into the slot the resolver
    // assigned to each param.
    let call_frame = {
        let mut arena = eval_ctx.scope_arena.borrow_mut();
        let frame_id = arena.alloc_child(crate::arena::ScopeId(closure_env_id), params.len());
        for (param, val) in params.iter().zip(type_args.into_iter()) {
            let span = crate::rust_span!().with_name(std::sync::Arc::from(param.name.as_str()));
            arena.push_slot(frame_id, Arc::new(crate::value::Thunk::value(val, span)));
        }
        frame_id
    }; // borrow_mut dropped here

    // Evaluate the function body in the call frame and force the result.
    let body_thunk = Arc::new(crate::value::Thunk::core_expr(
        Arc::clone(&body),
        call_frame.0,
        Arc::clone(eval_ctx),
        crate::rust_span!(),
    ));
    let result_val = crate::eval::materialize(&body_thunk, None, eval_ctx)
        .await
        .ok();

    // Drop the call frame: release the arg thunks now that the result is obtained.
    eval_ctx.scope_arena.borrow_mut().drop_scope(call_frame);

    typenode_leaf_to_type(&result_val?)
}

/// Evaluate a resolver by ThunkId — materializes the thunk then delegates to `call_strict_resolver`.
/// Used by the `type_stage_map` Function path (Step 3 in `resolve_type_head`) which has a pre-located ThunkId.
pub(crate) async fn evaluate_resolver_with_thunk(
    thunk_id: crate::arena::ThunkId,
    args: &[crate::type_def::Type],
    eval_ctx: &Arc<crate::eval::EvalContext>,
) -> Option<crate::type_def::Type> {
    let resolver_thunk = eval_ctx.scope_arena.borrow().get_thunk(thunk_id);
    let resolver_val = crate::eval::materialize(&resolver_thunk, None, eval_ctx)
        .await
        .ok()?;
    call_strict_resolver(resolver_val, args, eval_ctx).await
}

/// Convert a TypeNode variant value to a Type.
/// Handles leaf constructors (no payload) and the Dict constructor (any payload → open Dict).
pub(crate) fn typenode_leaf_to_type(val: &Value) -> Option<Type> {
    let tag = match val {
        Value::Variant { tycon, ctor, .. } => format!("{}.{}", tycon, ctor),
        _ => return None,
    };
    match tag.as_str() {
        "TypeNode.Int" => Some(Type::Int),
        "TypeNode.Float" => Some(Type::Float),
        "TypeNode.String" => Some(Type::Str),
        "TypeNode.Bytes" => Some(Type::Bytes),
        "TypeNode.Never" => Some(Type::Never),
        "TypeNode.Unknown" => Some(Type::Unknown),
        "TypeNode.Top" => Some(Type::Any),
        "TypeNode.Proxy" => Some(Type::Proxy),
        // Dict with any payload → open structural dict (any keys, any values)
        "TypeNode.Dict" => Some(Type::Dict(crate::types::Row {
            fields: indexmap::IndexMap::new(),
            tail: crate::type_def::RowTail::Uniform {
                key: None,
                value: Box::new(Type::Any),
            },
        })),
        // Opaque builtin types — each maps to a TyCon that value_matches_type dispatches
        // via TyConDef.builtin_type. The discriminant string here must match the string
        // registered in build_builtin_core_type_env_inner and the arm in value_matches_type.
        "TypeNode.Program" => Some(Type::TyCon("Program".to_string())),
        "TypeNode.Document" => Some(Type::TyCon("Document".to_string())),
        "TypeNode.TypeContext" => Some(Type::TyCon("TypeContext".to_string())),
        "TypeNode.DirCap" => Some(Type::TyCon("DirCap".to_string())),
        "TypeNode.NetCap" => Some(Type::TyCon("NetCap".to_string())),
        "TypeNode.Handle" => Some(Type::TyCon("Handle".to_string())),
        "TypeNode.File" => Some(Type::TyCon("File".to_string())),
        "TypeNode.BuilderHandle" => Some(Type::TyCon("BuilderHandle".to_string())),
        "TypeNode.Task" => Some(Type::TyCon("Task".to_string())),
        "TypeNode.Channel" => Some(Type::TyCon("Channel".to_string())),
        "TypeNode.Context" => Some(Type::TyCon("Context".to_string())),
        "TypeNode.ReactiveCell" => Some(Type::TyCon("ReactiveCell".to_string())),
        "TypeNode.ClockCap" => Some(Type::TyCon("ClockCap".to_string())),
        "TypeNode.Timezone" => Some(Type::TyCon("Timezone".to_string())),
        "TypeNode.Decimal" => Some(Type::TyCon("Decimal".to_string())),
        "TypeNode.BigInt" => Some(Type::TyCon("BigInt".to_string())),
        "TypeNode.QuicSession" => Some(Type::TyCon("QuicSession".to_string())),
        "TypeNode.Http2Session" => Some(Type::TyCon("Http2Session".to_string())),
        "TypeNode.Http3Session" => Some(Type::TyCon("Http3Session".to_string())),
        "TypeNode.Uri" => Some(Type::TyCon("Uri".to_string())),
        "TypeNode.Urn" => Some(Type::TyCon("Urn".to_string())),
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
                typed_variadics,
                rest,
                ret,
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
                for (name, ty) in typed_variadics {
                    write!(f, " ...{}@{}", name, ty)?;
                }
                if let Some(r) = rest {
                    write!(f, " ...{}", r.0)?;
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
            Type::NominalVariant { tycon, ctor, .. } => {
                if tycon.is_empty() {
                    write!(f, "{}", ctor)
                } else {
                    write!(f, "{}.{}", tycon, ctor)
                }
            }
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
        let mut ctx = NormCtxt::new(None);
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
        let mut ctx = NormCtxt::new(None);
        let ty = Type::TypeVar("a".to_string(), 0);
        let result = norm(&ty, &tv, &mut ctx).await;
        assert_eq!(result, Type::Str);
    }

    /// Test: normalize() cache - second call returns cached result
    #[tokio::test]
    async fn test_normalize_cache() {
        let tv = empty_type_vars();
        let mut ctx = NormCtxt::new(None);
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
        let mut ctx = NormCtxt::new(None);

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
        let mut ctx = NormCtxt::new(None);

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
        let mut ctx = NormCtxt::new(None);
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
        let ctx = NormCtxt::new(None);

        assert!(ctx.cache.is_empty());
        assert_eq!(ctx.depth, 0);
        assert_eq!(ctx.max_depth, 64);
        assert!(ctx.call_stack.is_empty());
    }

    /// Test: cache only stores ground types
    #[tokio::test]
    async fn test_normalize_cache_only_ground_types() {
        let subst = empty_type_vars();
        let mut ctx = NormCtxt::new(None);

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
        let mut ctx = NormCtxt::new(None);
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
