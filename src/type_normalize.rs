//! Type normalization and Display implementations.
//!
//! This module contains normalization logic for union/intersection types
//! and Display implementations for the Type enum.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::type_def::Type;
use crate::types::Substitution;
use crate::value::{Environment, Key, Thunk, Value};

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
    pub type_stage_env: Option<Rc<RefCell<Environment>>>,
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
///         from the prelude (e.g. AddResult, DivResult) and cache the result
///      c. If evaluation fails (fn not found, runtime error, unknown kind), return
///         stuck TypeStageApp — caller can retry later via deferred_equalities
///    - If depth exceeded or cycle detected, return stuck TypeStageApp
/// 3. Cache the result (only for ground types)
///
/// Returns the normalized type.
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
                        if let Some(resolved) = evaluate_resolver(fn_name, &normalized_args, &env) {
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

/// Helper: allocate a string value as a materialized thunk in `ctx`.
fn alloc_str(s: &str, ctx: &Rc<crate::eval::EvalContext>) -> crate::arena::ThunkId {
    ctx.alloc_thunk(Rc::new(Thunk::new_materialized(
        string_val(s),
        crate::ast::Span::origin(),
    )))
}

/// Convert a `Type` to its type-dict `Value` representation.
///
/// Each type is represented as a dict with a `kind:` key, following the schema
/// in `doc/feature/chr-unification.md §Type-Stage Resolvers`:
///
/// | Type      | Dict                                      |
/// |-----------|-------------------------------------------|
/// | `Int`     | `[kind: "named" name: "Int"]`             |
/// | `Float`   | `[kind: "named" name: "Float"]`           |
/// | `Str`     | `[kind: "named" name: "Str"]`             |
/// | `Bool`    | `[kind: "named" name: "Bool"]`            |
/// | `Number`  | `[kind: "named" name: "Number"]`          |
/// | `Unknown` | `[kind: "named" name: "Unknown"]`         |
/// | `Seq T`   | `[kind: "seq" element: <type-dict>]`      |
/// | `Map K V` | `[kind: "map" key: <K> value: <V>]`       |
///
/// Returns `None` if the type cannot be represented as a type-dict (e.g., TypeVar, Error).
pub(crate) fn type_to_dict(ty: &Type, ctx: &Rc<crate::eval::EvalContext>) -> Option<Value> {
    let origin = crate::ast::Span::origin();

    match ty {
        // Named scalar types
        Type::Int | Type::IntLiteral(_) => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Int", ctx));
            Some(Value::Dict(dict))
        }
        Type::Float => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Float", ctx));
            Some(Value::Dict(dict))
        }
        Type::Str | Type::StringLiteral(_) => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Str", ctx));
            Some(Value::Dict(dict))
        }
        Type::Bool => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Bool", ctx));
            Some(Value::Dict(dict))
        }
        Type::Number => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Number", ctx));
            Some(Value::Dict(dict))
        }
        Type::Unknown => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Unknown", ctx));
            Some(Value::Dict(dict))
        }
        Type::Top => {
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("named", ctx));
            dict.insert(Key::String("name".into()), alloc_str("Top", ctx));
            Some(Value::Dict(dict))
        }
        Type::Seq(elem) => {
            let elem_dict = type_to_dict(elem, ctx)?;
            let elem_id = ctx.alloc_thunk(Rc::new(Thunk::new_materialized(elem_dict, origin)));
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("seq", ctx));
            dict.insert(Key::String("element".into()), elem_id);
            Some(Value::Dict(dict))
        }
        Type::Map(k, v) => {
            let k_dict = type_to_dict(k, ctx)?;
            let v_dict = type_to_dict(v, ctx)?;
            let k_id = ctx.alloc_thunk(Rc::new(Thunk::new_materialized(k_dict, origin)));
            let v_id = ctx.alloc_thunk(Rc::new(Thunk::new_materialized(v_dict, origin)));
            let mut dict = IndexMap::new();
            dict.insert(Key::String("kind".into()), alloc_str("map", ctx));
            dict.insert(Key::String("key".into()), k_id);
            dict.insert(Key::String("value".into()), v_id);
            Some(Value::Dict(dict))
        }
        // TypeVar, Error, Function, Record, etc. — not representable as type-dicts
        _ => None,
    }
}

/// Helper: create a `Value::String` from a static string slice.
fn string_val(s: &str) -> Value {
    let src: Rc<str> = Rc::from(s);
    let len = src.len();
    Value::String {
        source: src,
        start: 0,
        end: len,
    }
}

/// Convert a type-dict `Value` back to a `Type`.
///
/// Inverse of `type_to_dict`. Handles the `kind:` schema from
/// `doc/feature/chr-unification.md §Type-Stage Resolvers`.
///
/// Returns `None` if the dict cannot be converted (unknown kind, missing fields, wrong shape).
pub(crate) fn dict_to_type(val: &Value, ctx: &Rc<crate::eval::EvalContext>) -> Option<Type> {
    let dict = match val {
        Value::Dict(d) => d,
        _ => return None,
    };

    // Get kind field
    let kind_id = dict.get(&Key::String("kind".into()))?;
    let kind_thunk = ctx.get_thunk(*kind_id);
    let kind_val = crate::eval::materialize(&kind_thunk, None, ctx).ok()?;
    let kind = match kind_val.as_str() {
        Some(s) => s.to_string(),
        None => return None,
    };

    match kind.as_str() {
        "named" => {
            let name_id = dict.get(&Key::String("name".into()))?;
            let name_thunk = ctx.get_thunk(*name_id);
            let name_val = crate::eval::materialize(&name_thunk, None, ctx).ok()?;
            let name = name_val.as_str()?.to_string();
            match name.as_str() {
                "Int" => Some(Type::Int),
                "Float" => Some(Type::Float),
                "Str" | "String" => Some(Type::Str),
                "Bool" => Some(Type::Bool),
                "Number" => Some(Type::Number),
                "Unknown" | "_" => Some(Type::Unknown),
                "Top" => Some(Type::Top),
                _ => None, // Unknown named type — can't represent
            }
        }
        "seq" => {
            let elem_id = dict.get(&Key::String("element".into()))?;
            let elem_thunk = ctx.get_thunk(*elem_id);
            let elem_val = crate::eval::materialize(&elem_thunk, None, ctx).ok()?;
            let elem_ty = dict_to_type(&elem_val, ctx)?;
            Some(Type::Seq(Box::new(elem_ty)))
        }
        "map" => {
            let k_id = dict.get(&Key::String("key".into()))?;
            let v_id = dict.get(&Key::String("value".into()))?;
            let k_val = crate::eval::materialize(&ctx.get_thunk(*k_id), None, ctx).ok()?;
            let v_val = crate::eval::materialize(&ctx.get_thunk(*v_id), None, ctx).ok()?;
            let k_ty = dict_to_type(&k_val, ctx)?;
            let v_ty = dict_to_type(&v_val, ctx)?;
            Some(Type::Map(Box::new(k_ty), Box::new(v_ty)))
        }
        _ => None, // Unknown kind — can't convert
    }
}

/// Evaluate a user-defined type-stage resolver function.
///
/// Looks up `fn_name` in the type-stage environment, calls it with the given
/// `Type` arguments (converted to type-dict values), and converts the result
/// back to a `Type`.
///
/// Returns `None` if any step fails:
/// - Resolver not found in env
/// - Argument type cannot be represented as a type-dict
/// - Runtime error during evaluation
/// - Result cannot be converted back to a `Type`
pub(crate) fn evaluate_resolver(
    fn_name: &str,
    args: &[Type],
    env: &Rc<RefCell<Environment>>,
) -> Option<Type> {
    // Look up the resolver function thunk
    let fn_thunk = env.borrow().get(fn_name)?;

    // Create a minimal EvalContext for type-stage evaluation.
    // We use new_empty() to avoid inheriting stale stdlib ThunkId caches — the
    // type-stage env was built with its own bootstrap EvalContext and ThunkArena.
    let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority()).ok()?;
    let ctx = crate::eval::EvalContext::new_empty(base_dir, Rc::clone(env), false);

    // Materialize the function value
    let fn_val = crate::eval::materialize(&fn_thunk, None, &ctx).ok()?;

    // Convert each Type arg to a type-dict Value
    let arg_thunks: Vec<Rc<Thunk>> = args
        .iter()
        .map(|ty| {
            let dict_val = type_to_dict(ty, &ctx)?;
            Some(Rc::new(Thunk::new_materialized(
                dict_val,
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
            crate::eval_call::invoke_function(&call_ctx).ok()?
        }
        // Builtin resolvers are not expected — all resolvers are LLT-defined functions.
        _ => return None,
    };

    // Force evaluation (materialize the lazy result)
    let result_val = crate::eval::materialize(&result_thunk, None, &ctx).ok()?;

    // Convert result dict back to Type
    dict_to_type(&result_val, &ctx)
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
            Type::Seq(elem) => write!(f, "Seq[{}]", elem),
            Type::Map(key, val) => write!(f, "Map[{} {}]", key, val),
            Type::Proxy => write!(f, "Proxy"),
            Type::TypeVar(name, _level) => write!(f, "{}", name),
            Type::Unknown => write!(f, "_"),
            Type::Top => write!(f, "\u{22a4}"),
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
            Type::Handle => write!(f, "Handle"),
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
            Type::Never => write!(f, "\u{22a5}"), // ⊥ symbol
            Type::NominalVariant { tag, .. } => write!(f, "{}", tag),
            Type::App(func, arg) => write!(f, "[{} {}]", func, arg),
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

        // Verify cache entry exists
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
        let ty = Type::Seq(Box::new(Type::TypeStageApp {
            fn_name: "AddResult".to_string(),
            args: vec![Type::Int, Type::Float],
        }));
        assert!(ty.has_type_stage_app());
    }

    /// Test: has_type_stage_app() returns false for Seq(Int)
    #[test]
    fn test_has_type_stage_app_false_for_seq_of_concrete() {
        let ty = Type::Seq(Box::new(Type::Int));
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
        let ty_with_var = Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0)));
        let _result1 = normalize(&ty_with_var, &subst, &mut ctx);

        // Cache should NOT contain this type (has inference vars)
        assert!(!ctx.cache.contains_key(&ty_with_var));

        // Normalize a ground type
        let ty_ground = Type::Seq(Box::new(Type::Int));
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
