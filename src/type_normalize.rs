//! Type normalization and Display implementations.
//!
//! This module contains normalization logic for union/intersection types
//! and Display implementations for the Type enum.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, RwLock};

use crate::type_def::Type;
use crate::types::Substitution;
use crate::value::{Environment, Thunk, Value};

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
    /// Resolver result cache: (resolver_name, [arg_types]) -> result_type
    /// Pre-populated with arithmetic resolver results (Add/Sub/Mul/Div).
    /// Key is (resolver function name, arg types), value is the resolved type.
    pub resolver_cache: HashMap<(String, Vec<Type>), Type>,
    /// Type-stage evaluation environment for user-defined resolver functions.
    ///
    /// Contains bindings from `--- stage: type` sections of prelude.llt.
    /// When set, `normalize()` will call user-defined resolver functions to
    /// reduce `TypeStageApp` nodes that are not in the static resolver cache.
    ///
    /// `None` during bootstrap (when the type-stage env is being built) or
    /// when type-stage env creation fails.
    pub type_stage_env: Option<Arc<RwLock<Environment>>>,
    /// If false, disable resolver evaluation (prevents runtime errors from propagating into type inference).
    /// Set to false inside unify() to prevent evaluation failures from causing type errors.
    pub allow_eval: bool,
}

impl NormCtxt {
    /// Create an empty normalization context with default limits.
    /// Populates `type_stage_env` from the cached type-stage environment (built once per thread).
    pub fn new() -> Self {
        Self {
            cache: HashMap::new(),
            depth: 0,
            max_depth: 64,
            call_stack: Vec::new(),
            resolver_cache: HashMap::new(),
            type_stage_env: crate::imports::build_type_stage_env(),
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
///      a. Check `resolver_cache` (memoized results from previous LLT calls this run)
///      b. On cache miss, call `evaluate_resolver()` to invoke the type-stage function
///         from the prelude (e.g. `AddResult`, `DivResult`) and cache the result.
///      c. If evaluation fails, return stuck TypeStageApp
///         (caller can retry via deferred_equalities)
///    - If depth exceeded or cycle detected, return stuck TypeStageApp
/// 3. Cache the result (only for ground types)
///
/// Returns the normalized type.
#[allow(clippy::doc_overindented_list_items)] // multi-level numbered sub-list requires deeper indentation
pub fn normalize(ty: &Type, subst: &Substitution, ctx: &mut NormCtxt) -> Type {
    // Step 1: Apply current substitution
    let ty_substituted = subst.apply(ty);

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
            let normalized_args: Vec<Type> =
                args.iter().map(|arg| normalize(arg, subst, ctx)).collect();
            ctx.depth -= 1;

            // Check if all args are ground
            let all_ground = normalized_args.iter().all(|arg| !arg.has_inference_vars());

            if all_ground {
                // All args are ground — attempt reduction via resolver cache lookup
                // Push fn_name to call stack for cycle detection
                ctx.call_stack.push(fn_name.clone());

                // Check resolver_cache for pre-populated results (arithmetic resolvers)
                let cache_key = (fn_name.clone(), normalized_args.clone());
                let result = if let Some(resolved_type) = ctx.resolver_cache.get(&cache_key) {
                    // Cache hit: return the resolved type directly
                    resolved_type.clone()
                } else if ctx.allow_eval {
                    // allow_eval is true — try evaluating resolver from type-stage env
                    if let Some(env) = ctx.type_stage_env.clone() {
                        // Cache miss — try evaluating user-defined resolver from type-stage env
                        // sync bridge — tracked for async migration
                        if let Some(resolved) = crate::async_rt::block_on_anywhere(
                            evaluate_resolver(fn_name, &normalized_args, &env),
                        ) {
                            // Insert into resolver_cache so subsequent calls are fast
                            ctx.resolver_cache.insert(cache_key, resolved.clone());
                            resolved
                        } else {
                            // Resolver evaluation failed — return stuck TypeStageApp
                            Type::TypeStageApp {
                                fn_name: fn_name.clone(),
                                args: normalized_args,
                            }
                        }
                    } else {
                        // No type-stage env available — return stuck TypeStageApp
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
}

// normalize_union and normalize_intersection moved to impl Type in type_def.rs

/// Convert a `Type` to a TypeNode `Value` (T-1061).
///
/// After the type-stage combinator migration, resolver functions (`AddResult`, etc.)
/// receive TypeNode Variant values as arguments, not kind-keyed dicts.
///
/// Handles primitive types that appear as arithmetic resolver arguments.
/// Complex types (Seq, Map, Union, Record, etc.) return `None` — arithmetic
/// resolvers never receive those as arguments.
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
        Type::Bool => Some(leaf("TypeNode.Bool")),
        Type::Unknown => Some(leaf("TypeNode.Unknown")),
        Type::Never => Some(leaf("TypeNode.Never")),
        // Number (supertype of Int and Float) — no direct TypeNode equivalent;
        // use Unknown so the resolver produces a conservative result.
        Type::Number => Some(leaf("TypeNode.Unknown")),
        Type::Top => Some(leaf("TypeNode.Unknown")),
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
    env: &Arc<RwLock<Environment>>,
) -> Option<Type> {
    // Look up the resolver function thunk
    let fn_thunk = env.read().unwrap().get(fn_name)?;

    // Create a minimal EvalContext for type-stage evaluation.
    // We use new_empty() to avoid inheriting stale stdlib ThunkId caches — the
    // type-stage env was built with its own bootstrap EvalContext and ThunkArena.
    // AMBIENT-OK: Type-stage evaluation uses CWD as base_dir (no file I/O).
    #[allow(clippy::disallowed_methods)]
    let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).ok()?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, Arc::clone(env), false);

    // Materialize the function value
    let fn_val = crate::eval::materialize(&fn_thunk, None, &ctx).await.ok()?;

    // Convert each Type arg to a TypeNode Variant Value (T-1061).
    // Resolver functions now receive TypeNode Variants, not kind-keyed dicts.
    let arg_thunks: Vec<Arc<Thunk>> = args
        .iter()
        .map(|ty| {
            let typenode_val = type_to_typenode(ty)?;
            Some(Arc::new(Thunk::new_materialized(
                typenode_val,
                crate::ast::Span::origin(),
            )))
        })
        .collect::<Option<Vec<_>>>()?;

    // Dispatch to the function
    let result_thunk = match fn_val {
        Value::Function {
            ref params,
            ref body,
            env: ref closure_env,
            ..
        } => {
            let call_ctx = crate::eval_call::CallContext {
                params,
                body,
                closure_env,
                positional: &arg_thunks,
                named: None,
                default_env: closure_env,
                call_span: crate::ast::Span::origin(),
                origin: None,
                ctx: &ctx,
            };
            crate::eval_call::invoke_function(&call_ctx).await.ok()?
        }
        // Builtin resolvers are not expected — all resolvers are LLT-defined functions.
        _ => return None,
    };

    // Force evaluation (materialize the lazy result)
    let result_val = crate::eval::materialize(&result_thunk, None, &ctx)
        .await
        .ok()?;

    // Convert the TypeNode Variant result back to a Type (T-1061).
    // typenode_value_to_type handles both TypeNode Variants (new path) and
    // kind-keyed dicts (fallback for any pre-migration resolver code).
    crate::typecheck::typecheck_annot::typenode_value_to_type_pub(&result_val, &ctx)
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::IntLiteral(n) => write!(f, "{}", n),
            Type::Float => write!(f, "Float"),
            Type::Str => write!(f, "String"),
            Type::StringLiteral(s) => write!(f, "\"{}\"", s),
            Type::Bool => write!(f, "Bool"),
            Type::Bytes => write!(f, "Bytes"),
            Type::Number => write!(f, "Number"),
            Type::Record(row) => {
                write!(f, "[")?;
                let mut sorted: Vec<_> = row.fields.iter().collect();
                sorted.sort_by_key(|(k, _)| *k);
                for (i, (key, ty)) in sorted.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{}: {}", key, ty)?;
                }
                // Display RowTail::Uniform as `_ : V` or `_@K : V`
                if let crate::type_def::RowTail::Uniform { key, value } = &row.tail {
                    if !row.fields.is_empty() {
                        write!(f, " ")?;
                    }
                    if let Some(k) = key {
                        write!(f, "_@{} : {}", k, value)?;
                    } else {
                        write!(f, "_ : {}", value)?;
                    }
                }
                write!(f, "]")
            }
            Type::Function {
                params,
                ret,
                variadic,
            } => {
                // Parenthesize nested function types in return position for clarity
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
            Type::Unknown => write!(f, "_"),
            Type::Top => write!(f, "Top"),
            Type::Error => write!(f, "<error>"),
            Type::Union(types) => {
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    // Parenthesize nested unions (shouldn't happen after normalization, but be safe)
                    match ty {
                        Type::Union(_) => write!(f, "({})", ty)?,
                        _ => write!(f, "{}", ty)?,
                    }
                }
                Ok(())
            }
            Type::Intersection(types) => {
                for (i, ty) in types.iter().enumerate() {
                    if i > 0 {
                        write!(f, " & ")?;
                    }
                    // Parenthesize nested intersections and unions for clarity
                    match ty {
                        Type::Intersection(_) | Type::Union(_) => write!(f, "({})", ty)?,
                        _ => write!(f, "{}", ty)?,
                    }
                }
                Ok(())
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
                // Pretty-print common builtin type constructors in their familiar syntax
                if let Some(elem) = self.as_seq() {
                    return write!(f, "Seq[{}]", elem);
                }
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
    use std::collections::HashSet;

    /// Helper: create an empty substitution for testing
    fn empty_subst() -> Substitution {
        Substitution::new()
    }

    /// Test: normalize(Int, subst, ctx) returns Int unchanged
    #[test]
    fn test_normalize_identity_concrete_type() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::Int;
        let result = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result, Type::Int);
    }

    /// Test: normalize(TypeVar("a"), subst, ctx) returns the bound type if "a" is in subst
    #[test]
    fn test_normalize_substitution() {
        let subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".to_string(), Type::Str);
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeVar("a".to_string(), 0);
        let result = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result, Type::Str);
    }

    /// Test: normalize(TypeStageApp with ground args) returns resolver result from cache
    #[test]
    fn test_normalize_type_stage_app_ground_args() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new(); // pre-populated with arithmetic resolver cache
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        // NormCtxt::new() pre-populates resolver_cache; AddResult(Int, Float) -> Float
        assert_eq!(result, Type::Float);
    }

    /// Test: normalize() cache - second call returns cached result
    #[test]
    fn test_normalize_cache() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::Int;

        // First call - populates cache
        let result1 = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result1, Type::Int);

        // Second call - should return cached result
        let result2 = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result2, Type::Int);

        // Verify cache entry exists (use concrete Int, not Seq)
        assert!(ctx.cache.contains_key(&Type::Int));
    }

    /// Test: normalize() cycle detection - TypeStageApp with fn_name already in call_stack returns stuck
    #[test]
    fn test_normalize_cycle_detection() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();

        // Manually push "Recursive" to call_stack to simulate cycle
        ctx.call_stack.push("Recursive".to_string());

        let ty = Type::TypeStageApp {
            fn_name: "Recursive".to_string(),
            args: vec![Type::Int],
        };

        let result = normalize(&ty, &subst, &mut ctx);

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
    #[test]
    fn test_normalize_depth_guard() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();

        // Set depth to max_depth
        ctx.depth = ctx.max_depth;

        let ty = Type::TypeStageApp {
            fn_name: "Deep".to_string(),
            args: vec![Type::Int],
        };

        let result = normalize(&ty, &subst, &mut ctx);

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
    #[test]
    fn test_has_type_stage_app_true_for_type_stage_app() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert!(ty.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns false for concrete types
    #[test]
    fn test_has_type_stage_app_false_for_concrete() {
        assert!(!Type::Int.has_type_stage_app());
        assert!(!Type::Str.has_type_stage_app());
        assert!(!Type::Bool.has_type_stage_app());
        assert!(!Type::Float.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns true for Seq(TypeStageApp)
    #[test]
    fn test_has_type_stage_app_true_for_seq_containing_type_stage_app() {
        let ty = Type::seq(Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        });
        assert!(ty.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns false for Seq(Int)
    #[test]
    fn test_has_type_stage_app_false_for_seq_of_concrete() {
        let ty = Type::seq(Type::Int);
        assert!(!ty.has_type_stage_app());
    }

    /// Test: TypeStageApp PartialEq - same fn_name+args equal
    #[test]
    fn test_type_stage_app_partial_eq_same() {
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
    #[test]
    fn test_type_stage_app_partial_eq_different_fn_name() {
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
    #[test]
    fn test_type_stage_app_partial_eq_different_args() {
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
    #[test]
    fn test_type_stage_app_display() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "AddResult(Int, Float)");
    }

    /// Test: TypeStageApp Display format with single arg
    #[test]
    fn test_type_stage_app_display_single_arg() {
        let ty = Type::TypeStageApp {
            fn_name: "Singleton".to_string(),
            args: vec![Type::Int],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "Singleton(Int)");
    }

    /// Test: TypeStageApp Display format with no args
    #[test]
    fn test_type_stage_app_display_no_args() {
        let ty = Type::TypeStageApp {
            fn_name: "Nullary".to_string(),
            args: vec![],
        };
        let formatted = format!("{}", ty);
        assert_eq!(formatted, "Nullary()");
    }

    /// Test: normalize() with non-ground TypeStageApp args returns stuck
    #[test]
    fn test_normalize_type_stage_app_non_ground_args() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::TypeVar("a".to_string(), 0), Type::Float],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        // Non-ground args - should return TypeStageApp with normalized args (but stuck)
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "AddResult".to_string(),
                args: vec![Type::TypeVar("a".to_string(), 0), Type::Float],
            }
        );
    }

    /// Test: normalize() recursively normalizes TypeStageApp args then resolves from cache
    #[test]
    fn test_normalize_type_stage_app_recursive_arg_normalization() {
        let subst = Substitution::new();
        subst
            .type_map
            .borrow_mut()
            .insert("a".to_string(), Type::Int);
        let mut ctx = NormCtxt::new(); // pre-populated with arithmetic resolver cache

        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::TypeVar("a".to_string(), 0), Type::Float],
        };

        let result = normalize(&ty, &subst, &mut ctx);

        // Args normalized (TypeVar("a") -> Int), then cache hit: AddResult(Int, Float) -> Float
        assert_eq!(result, Type::Float);
    }

    /// Test: NormCtxt::new() initializes with correct defaults
    #[test]
    fn test_norm_ctxt_new_defaults() {
        let ctx = NormCtxt::new();

        assert!(ctx.cache.is_empty());
        assert_eq!(ctx.depth, 0);
        assert_eq!(ctx.max_depth, 64);
        assert!(ctx.call_stack.is_empty());
        assert!(ctx.resolver_cache.is_empty());
    }

    /// Test: cache only stores ground types
    #[test]
    fn test_normalize_cache_only_ground_types() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();

        // Normalize a type with inference variables
        let ty_with_var = Type::seq(Type::TypeVar("a".to_string(), 0));
        let _result1 = normalize(&ty_with_var, &subst, &mut ctx);

        // Cache should NOT contain this type (has inference vars)
        assert!(!ctx.cache.contains_key(&ty_with_var));

        // Normalize a ground type
        let ty_ground = Type::seq(Type::Int);
        let _result2 = normalize(&ty_ground, &subst, &mut ctx);

        // Cache SHOULD contain this type (ground)
        assert!(ctx.cache.contains_key(&ty_ground));
    }

    /// Test: TypeStageApp collect_type_vars
    #[test]
    fn test_type_stage_app_collect_type_vars() {
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
    #[test]
    fn test_type_stage_app_has_inference_vars_true() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::TypeVar("a".to_string(), 0), Type::Int],
        };
        assert!(ty.has_inference_vars());
    }

    /// Test: TypeStageApp has_inference_vars false for ground args
    #[test]
    fn test_type_stage_app_has_inference_vars_false() {
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        assert!(!ty.has_inference_vars());
    }

    /// Test: AddResult(Int, Int) resolves to Int via LLT type-stage function
    #[test]
    fn test_resolver_cache_add_int_int() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Int],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result, Type::Int);
    }

    /// Test: AddResult(Int, Float) resolves to Float via LLT type-stage function
    #[test]
    fn test_resolver_cache_add_int_float() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result, Type::Float);
    }

    /// Test: DivResult(Int, Int) resolves to Float via LLT type-stage function
    #[test]
    fn test_resolver_cache_div_int_int() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeStageApp {
            fn_name: "DivResult".to_string(),
            args: vec![Type::Int, Type::Int],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        assert_eq!(result, Type::Float);
    }

    /// Test: unknown resolver returns stuck TypeStageApp
    #[test]
    fn test_resolver_cache_miss_unknown_resolver() {
        let subst = empty_subst();
        let mut ctx = NormCtxt::new();
        let ty = Type::TypeStageApp {
            fn_name: "UnknownResolver".to_string(),
            args: vec![Type::Int, Type::Float],
        };
        let result = normalize(&ty, &subst, &mut ctx);
        // Should return stuck TypeStageApp (cache miss)
        assert_eq!(
            result,
            Type::TypeStageApp {
                fn_name: "UnknownResolver".to_string(),
                args: vec![Type::Int, Type::Float],
            }
        );
    }
}
