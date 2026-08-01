//! Path-sensitive narrowing and overlap checking.
//!
//! This module contains the subsystem responsible for:
//! - Extracting type narrowing constraints from conditional expressions (`if`, match guards)
//! - Applying those constraints to fork the type environment for true/false branches
//! - Instance pattern type extraction and functional-dependency parameter index resolution
//! - Pattern overlap / type unification probes (side-effect-free)
//!
//! ## Annotation-based narrowing (T-1761)
//!
//! `extract_narrowings` supports two mechanisms for declaring narrowing behavior:
//!
//! 1. **`@[narrows: TypeName]` key annotation** — e.g., `foo?@[narrows: Int]:`.
//!    When `[foo? x]` is true, `x` is narrowed to `TypeName`.
//!
//! 2. **`@[is: TypeName]` parameter annotation** — e.g., `[fn [let x@[is: Int]] ...]`.
//!    When the predicate is called with a single variable argument and returns true,
//!    that variable is narrowed to `TypeName`.
//!
//! Both mechanisms produce a `Vec<Option<TypeValue>>` derived locally in `typecheck_cek.rs`
//! from `@[narrows: T]` / `@[is: T]` annotations during Pass 4 of `run_typecheck_dict`.
//! `extract_narrowings` looks up the called function in the type environment and reads
//! the narrowings from the callee's TypeValue.Scheme `narrowings` payload field. Any
//! function — not just prelude predicates — can participate in narrowing by using these
//! annotations.
//!
//! **Predicate narrowing** (`@[is: T]`, `@[narrows: T]`) is entirely annotation-driven — no
//! predicate names are hardcoded in Rust. A custom prelude can name predicates anything.
//!
//! **Structural pattern narrowing** (`=`, `and`, `has?`, `type-of`) uses four function names
//! that are **Axiom 1 protocol entries** (D-8). Rust defines the protocol; the prelude
//! implements it. Any compliant prelude must provide functions with these exact names for
//! structural narrowing to work. This is analogous to `tmpl`/`unindent` (D-3) and
//! `as-typenode` (D-7). See `doc/feature/narrowing.md` §Structural Narrowing Protocol Entries.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use super::typecheck_annot;
use crate::ast::{Span, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::error::TypeDiagnostic;
use crate::type_infer::{
    make_typevalue_float_lit, make_typevalue_int_lit, make_typevalue_repr, make_typevalue_str_lit,
    make_typevalue_unknown,
};
use crate::type_tags::*;
use crate::types::InferState;
use crate::value::Value;

/// Extract the first parameter narrowing type from a TypeValue.Scheme's narrowings payload.
///
/// TypeValue.Scheme has an optional `narrowings` field (an indexed dict: `{ 0: TypeValue | [] }`).
/// Returns `Some(narrowing_ty)` if `narrowings[0]` is a TypeValue (non-empty dict/variant).
/// Returns `None` if the scheme has no narrowings, or if `narrowings[0]` is `[]` (absent/null).
fn extract_scheme_narrowings_first(
    scheme: &Arc<crate::value::Value>,
) -> Option<Arc<crate::value::Value>> {
    use crate::value::{HashableValue, Value};
    // The scheme must be a TypeValue.Scheme variant.
    let payload = match scheme.as_ref() {
        Value::Variant {
            ctor,
            payload: Some(thunk),
            ..
        } if ctor.as_ref() == TV_SCHEME => match thunk.peek_result() {
            Some(Ok(v)) => v,
            _ => return None,
        },
        _ => return None,
    };
    // Extract the `narrowings` dict from the Scheme payload.
    let narrowings_dict = if let Value::Dict { entries, .. } = payload {
        let key = HashableValue::Str(Arc::from(FIELD_NARROWINGS));
        let thunk = entries.get(&key)?;
        match thunk.peek_result() {
            Some(Ok(Value::Dict { entries, .. })) => entries.clone(),
            _ => return None,
        }
    } else {
        return None;
    };
    // Read narrowings[0].
    let first_thunk = narrowings_dict.get(&HashableValue::Int(0))?;
    let first_val = match first_thunk.peek_result() {
        Some(Ok(v)) => v,
        _ => return None,
    };
    // An absent/null narrowing is represented as `[]` (empty Dict).
    match first_val {
        Value::Dict { entries, .. } if entries.is_empty() => None,
        other => Some(Arc::new(other.clone())),
    }
}

/// Narrowing constraints extracted from conditional expressions.
/// Each constraint refines the type of a variable in the true branch of an `if`.
#[derive(Debug, Clone)]
pub(crate) enum Narrowing {
    /// `[= var literal]` narrows `var` to the literal type.
    EqLiteral {
        var: String,
        ty: Arc<crate::value::Value>,
    },
    /// `[= [type-of var] "TypeName"]` narrows `var` to the named type.
    TypeOf {
        var: String,
        ty: Arc<crate::value::Value>,
    },
    /// `[has? var "key"]` narrows `var` to a record with at least that key.
    HasKey { var: String, key: String },
}

/// Extract narrowing constraints from a condition expression (SurfaceNode version).
/// Returns an empty vec for unrecognized patterns.
///
/// `env` is the type environment at the call site, used to look up annotation-based
/// narrowing declarations (the `narrowings` payload field in the callee's TypeValue.Scheme).
/// When a function is annotated with `@[narrows: T]` or has a first parameter annotated
/// with `@[is: T]`, calling it with a single variable argument narrows that variable to `T`
/// in the true branch. Any function registered in `env` can participate in narrowing —
/// not just hardcoded prelude predicates.
pub(crate) fn extract_narrowings(
    cond: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
) -> Vec<Narrowing> {
    match &cond.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } if named_args.is_empty() => {
            if let SurfaceExpression::VarRef { name, .. } = &func.expr {
                match name.as_str() {
                    // Pattern: [= x literal] or [= literal x]
                    "=" if args.len() == 2 => {
                        // Try both operand orderings
                        if let Some(narrowing) = try_eq_literal(&args[0], &args[1]) {
                            return vec![narrowing];
                        }
                        if let Some(narrowing) = try_eq_literal(&args[1], &args[0]) {
                            return vec![narrowing];
                        }
                        // Try type-of pattern: [= [type-of x] "TypeName"]
                        if let Some(narrowing) = try_type_of(&args[0], &args[1]) {
                            return vec![narrowing];
                        }
                        if let Some(narrowing) = try_type_of(&args[1], &args[0]) {
                            return vec![narrowing];
                        }
                    }
                    // Pattern: [has? x "key"]
                    "has?" if args.len() == 2 => {
                        if let (
                            SurfaceExpression::VarRef { name: var_name, .. },
                            SurfaceExpression::StringLiteral { content: key, .. },
                        ) = (&args[0].expr, &args[1].expr)
                        {
                            return vec![Narrowing::HasKey {
                                var: var_name.clone(),
                                key: key.clone(),
                            }];
                        }
                    }
                    // Pattern: [and cond1 cond2 ...]
                    "and" => {
                        let mut narrowings = Vec::new();
                        for arg in args {
                            narrowings.extend(extract_narrowings(arg, env));
                        }
                        return narrowings;
                    }
                    // Annotation-based narrowing (T-1761): look up the function in env.
                    // If its TypeValue.Scheme has `narrowings[0] = Some(T)`, then
                    // `[foo? x]` being true narrows `x` to `T`. This is the general
                    // mechanism — any function can declare narrowing via `@[narrows: T]`
                    // or `@[is: T]` on its first parameter.
                    _ if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            let scheme = env.read().expect("env RwLock poisoned").get_scheme(name);
                            if let Some(ref scheme_tv) = scheme {
                                // Extract narrowings[0] from TypeValue.Scheme payload if present.
                                if let Some(narrowing_ty) =
                                    extract_scheme_narrowings_first(scheme_tv)
                                {
                                    return vec![Narrowing::TypeOf {
                                        var: var_name.clone(),
                                        ty: narrowing_ty,
                                    }];
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
    Vec::new()
}

/// Try to extract an equality-literal narrowing from `[= var literal]`.
pub(crate) fn try_eq_literal(
    left: &Arc<SurfaceNode>,
    right: &Arc<SurfaceNode>,
) -> Option<Narrowing> {
    if let SurfaceExpression::VarRef { name, .. } = &left.expr {
        match &right.expr {
            SurfaceExpression::Int(n) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: make_typevalue_int_lit(*n),
            }),
            SurfaceExpression::Float(f) => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: make_typevalue_float_lit(*f),
            }),
            SurfaceExpression::StringLiteral { content: s, .. } => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: make_typevalue_str_lit(s),
            }),
            _ => None,
        }
    } else {
        None
    }
}

/// Try to extract a type-of narrowing from `[= [type-of var] "TypeName"]`.
pub(crate) fn try_type_of(left: &Arc<SurfaceNode>, right: &Arc<SurfaceNode>) -> Option<Narrowing> {
    // Left side must be [type-of var]
    if let SurfaceExpression::Call {
        func,
        args,
        named_args,
        ..
    } = &left.expr
    {
        if named_args.is_empty() && args.len() == 1 {
            if let SurfaceExpression::VarRef {
                name: func_name, ..
            } = &func.expr
            {
                if func_name == "type-of" {
                    if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                        // Right side must be a string literal type name
                        if let SurfaceExpression::StringLiteral {
                            content: type_name, ..
                        } = &right.expr
                        {
                            let tv = match type_name.as_str() {
                                "Int" => Some(make_typevalue_repr(REPR_INT)),
                                "Float" => Some(make_typevalue_repr(REPR_FLOAT)),
                                "String" => Some(make_typevalue_repr(REPR_STRING)),
                                // Only the three primitive types above produce narrowing from
                                // type-of. All other type names (including prelude-defined types
                                // such as Bool and Seq) fall through to None — no narrowing —
                                // so the variable retains its original type. Encoding prelude
                                // names as explicit match arms would couple Rust to prelude
                                // internals (Axiom 4), so the wildcard handles all remaining cases.
                                _ => None,
                            };
                            return tv.map(|t| Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: t,
                            });
                        }
                    }
                }
            }
        }
    }
    None
}

/// Apply narrowings for a branch by inserting refined types into `state.narrowing_map`.
///
/// Narrowing is a TYPE REFINEMENT of an existing slot binding, not a new binding.
/// The env is unchanged — callers no longer need the return value.
/// Scoping is handled by AfterBlock: `saved_narrowing_map` is stored in the continuation
/// and restored when AfterBlock fires, exactly like `saved_use_def`.
pub(crate) fn apply_narrowings(
    env: &Arc<RwLock<Env>>,
    narrowings: &[Narrowing],
    state: &mut crate::type_infer::InferState,
) {
    for narrowing in narrowings {
        let (var, refined_tv) = match narrowing {
            Narrowing::EqLiteral { var, ty } => (var, Arc::clone(ty)),
            Narrowing::TypeOf { var, ty } => (var, Arc::clone(ty)),
            Narrowing::HasKey { var, key } => {
                let fresh_field_tv = state.fresh_type_var(&crate::rust_span!());
                let mut fields: indexmap::IndexMap<String, Arc<Value>> = indexmap::IndexMap::new();
                fields.insert(key.clone(), fresh_field_tv);
                let current_tv = env.read().unwrap().get_scheme(var);
                if let Some(existing_tv) = current_tv {
                    if let Some(existing_fields) =
                        crate::typecheck::extract_record_fields_pub(&existing_tv)
                    {
                        for (k, v) in existing_fields {
                            fields.entry(k).or_insert(v);
                        }
                    }
                }
                (
                    var,
                    crate::typecheck::make_typevalue_record_pub(fields, None),
                )
            }
        };

        // Find the binding's BindingId via its definition span, then insert the refinement.
        if let Some(def_span) = Env::find_def_span_by_name(env, var) {
            let id = crate::type_infer::BindingId {
                def_span,
                name: var.clone(),
            };
            state.narrowing_map.insert(id, refined_tv);
        }
        // If no definition_span found: the binding has no tinct source (e.g. a builtin
        // injected without a span). Narrowings on such bindings are silently dropped —
        // they cannot be tracked via BindingId.
    }
}

/// Extract type parameters from an instance pattern declaration.
///
/// The PatternDecl stores the inner bracket `[a@Int b@Float]` as a single `SurfaceExpression::Dict`
/// binding (auto-indexed entries). This function recursively extracts types from either:
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket; extracts each auto-indexed entry
/// - `SurfaceExpression::VarRef { annotation: Some(ann), .. }` — `a@Type` form; resolves via
///   `typecheck_annot::resolve_annotation`
/// - `SurfaceExpression::VarRef { .. }` — bare identifier; treated as a fresh TypeVar
/// Map from declared TypeVar name to its TypeValue Arc — populated by `bind:` in instance patterns.
pub(crate) type InstanceBindVars = HashMap<String, Arc<crate::value::Value>>;

pub(crate) async fn extract_pattern_types(
    pattern_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
) -> Result<Vec<Arc<crate::value::Value>>, Vec<TypeDiagnostic>> {
    match &pattern_node.expr {
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            // Pre-pass: find `bind: [a b c]` entries and register TypeVars.
            // Inside [let ...], `bind: [a]` or `bind: [a b c]` declares TypeVars that can be
            // referenced as type annotations on subsequent params (e.g. target@a source@a).
            // The `:` in a LetDecl stores the value as a "default" annotation on a VarRef,
            // so `bind: [a]` creates VarRef{name:"bind", annotation:PropertyDict{default:[a]}}.
            let mut bind_vars: InstanceBindVars = HashMap::new();
            let mut skip_indices: std::collections::HashSet<usize> =
                std::collections::HashSet::new();

            for (i, binding) in bindings.iter().enumerate() {
                if let SurfaceExpression::VarRef {
                    name, annotation, ..
                } = &binding.expr
                {
                    if name.as_str() == "bind" {
                        if let Some(ann) = annotation {
                            // Use PropertyDict match to detect bind: regardless of key format.
                            // `bind: [a]` stores as PropertyDict regardless of whether the
                            // "default" key is StringLiteral or VarRef.
                            if let crate::ast::Annotation::PropertyDict(entries) = &ann.node {
                                // Extract the value from the first entry (the [a b c] node)
                                if let Some(default_node) = entries.first().map(|e| &e.node.value) {
                                    process_instance_bind_list(
                                        default_node,
                                        state,
                                        &mut bind_vars,
                                        binding.span.clone(),
                                    )
                                    .map_err(|e| vec![e])?;
                                    skip_indices.insert(i);
                                }
                            }
                        }
                    }
                }
            }

            let mut types = Vec::new();
            for (i, binding) in bindings.iter().enumerate() {
                if skip_indices.contains(&i) {
                    continue;
                }
                extract_binding_types(binding, env, state, &mut types, &bind_vars).await?;
            }
            Ok(types)
        }
        _ => Err(vec![TypeDiagnostic::error(
            "type-error",
            "instance arm pattern must be a [pattern [...]] or [let ...] declaration",
            pattern_node.span.clone(),
        )]),
    }
}

/// Process a `bind: [a b c]` value node from an instance [let ...] pattern.
///
/// Registers each TypeVar name in `bind_vars` with a fresh TypeValue Arc.
/// Mirrors the `bind:` handling in `typecheck_annot.rs` for fn@ annotations.
fn process_instance_bind_list(
    node: &Arc<SurfaceNode>,
    state: &mut InferState,
    bind_vars: &mut InstanceBindVars,
    span: Span,
) -> Result<(), TypeDiagnostic> {
    // Collect (name, span) pairs from the Call form [a b c] or bare VarRef [a]
    let name_spans: Vec<(String, Span)> = match &node.expr {
        SurfaceExpression::Call {
            func,
            args,
            named_args,
            ..
        } => {
            if !named_args.is_empty() {
                return Err(TypeDiagnostic::error(
                    "type-error",
                    "instance bind: list must contain only bare names",
                    node.span.clone(),
                ));
            }
            let mut v = Vec::new();
            match &func.expr {
                SurfaceExpression::VarRef { name, .. } => v.push((name.clone(), func.span.clone())),
                _ => {
                    return Err(TypeDiagnostic::error(
                        "type-error",
                        "instance bind: entries must be bare TypeVar names",
                        func.span.clone(),
                    ))
                }
            }
            for arg in args {
                match &arg.expr {
                    SurfaceExpression::VarRef { name, .. } => {
                        v.push((name.clone(), arg.span.clone()))
                    }
                    _ => {
                        return Err(TypeDiagnostic::error(
                            "type-error",
                            "instance bind: entries must be bare TypeVar names",
                            arg.span.clone(),
                        ))
                    }
                }
            }
            v
        }
        SurfaceExpression::VarRef { name, .. } => vec![(name.clone(), node.span.clone())],
        _ => {
            return Err(TypeDiagnostic::error(
                "type-error",
                "instance bind: must be a list of bare TypeVar names like [a b c]",
                span,
            ))
        }
    };

    for (name, name_span) in name_spans {
        if !name.starts_with(|c: char| c.is_lowercase()) {
            return Err(TypeDiagnostic::error(
                "type-error",
                format!(
                    "instance bind: TypeVar name '{}' must start with lowercase",
                    name
                ),
                name_span,
            ));
        }
        let level = state.ctx.current_level;
        let (fresh_id, fresh_tv) =
            state.fresh_type_var_with(Some(name.as_str()), Some(level), "Type", &name_span);
        // Mark as protected: instance bind: TypeVars are explicit polymorphic params.
        state.ctx.protected_vars.insert(fresh_id.clone());
        bind_vars.insert(name, fresh_tv);
    }
    Ok(())
}

/// Recursively extract type(s) from a single pattern binding expression.
///
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket `[a@Int b@Float]` (old syntax); expands entries
/// - `SurfaceExpression::LetDecl { bindings }` — inner binding bracket `[let a@Int b@Float]` (new syntax); expands bindings
/// - `SurfaceExpression::Call { func, args, .. }` — implied call `[Type]` or `[Type arg1 arg2]`; zero-arg attempts type resolution, multi-arg uses fresh TypeVar
/// - `SurfaceExpression::VarRef { annotation: Some(ann), .. }` — `a@Type` form; resolved via `resolve_annotation`
/// - `SurfaceExpression::VarRef { .. }` — bare identifier → fresh TypeVar (not Unknown, to suppress T017)
/// - `SurfaceExpression::Placeholder(..)` — wildcard `_` → Unknown (gradual escape hatch)
///
/// Recursive async functions must return a `BoxFuture` to be object-safe.
pub(crate) fn extract_binding_types<'a>(
    binding: &'a Arc<SurfaceNode>,
    env: &'a Arc<RwLock<Env>>,
    state: &'a mut InferState,
    types: &'a mut Vec<Arc<crate::value::Value>>,
    bind_vars: &'a InstanceBindVars,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), Vec<TypeDiagnostic>>> + Send + 'a>>
{
    Box::pin(async move {
        match &binding.expr {
            // Binding bracket [a@Int b@Float] parsed as auto-indexed Dict (old syntax for multi-param).
            // Named-key dicts like [key: k  value: v] represent a SINGLE structural type (a record),
            // not multiple independent type parameters. Only auto-indexed (keyless) dicts expand.
            SurfaceExpression::Dict(entries) => {
                let all_keyless = entries.iter().all(|e| e.node.key.is_none());
                if all_keyless {
                    for entry in entries {
                        extract_binding_types(&entry.node.value, env, state, types, bind_vars)
                            .await?;
                    }
                } else {
                    // Named-key dict: single compound type (structural/record type)
                    // After migration, fresh_type_var returns Arc<Value> (TypeValue.Var).
                    types.push(state.fresh_type_var(&binding.span));
                }
            }
            // Inner binding bracket [let a@Int b@Float] (new unified-bindings syntax)
            SurfaceExpression::LetDecl { bindings } => {
                for sub_binding in bindings {
                    extract_binding_types(sub_binding, env, state, types, bind_vars).await?;
                }
            }
            // Implied call form in pattern position.
            //
            // Zero-arg case `[Int]`, `[String]`, `[Boolean]` — the name is a type constructor
            // with no arguments. Try to resolve the func name as a type annotation via
            // `resolve_annotation`. If the name is registered in `type_stage_scope` or
            // `tycon_env` (e.g. `Int`, `String`, `Boolean`), we get the concrete type back.
            // This enables `[pattern [Int]]` to resolve to TypeValue.Repr{repr:"Value::Int"}
            // rather than a fresh TypeVar, making the instance pattern concrete.
            //
            // Multi-arg case `[Result String]`, `[Channel Int]` — parametric type application.
            // Uses a fresh TypeVar so that unification can still find the correct type and
            // T017 ("contains Unknown") does not fire. The type constructor is looked up at
            // call sites during type class resolution.
            SurfaceExpression::Call {
                func,
                args,
                named_args,
                ..
            } if args.is_empty() && named_args.is_empty() => {
                // Zero-arg implied call: attempt type-name resolution.
                let resolved = if let SurfaceExpression::VarRef {
                    name,
                    escaped: false,
                    ..
                } = &func.expr
                {
                    let ann = crate::ast::Annotation::Simple(name.clone());
                    let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
                    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
                    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
                    let tv = typecheck_annot::resolve_annotation(
                        &ann,
                        func.span.clone(),
                        &mut *state,
                        &mut constraints,
                        &mut ann_m,
                        &mut row_m,
                        None,
                    )
                    .await
                    .map_err(|e| vec![e])?;
                    Some(tv)
                } else {
                    None
                };
                types.push(match resolved {
                    Some(tv) => tv,
                    None => state.fresh_type_var(&binding.span),
                });
            }
            SurfaceExpression::Call { .. } => {
                // Multi-arg parametric call: use a fresh TypeVar.
                types.push(state.fresh_type_var(&binding.span));
            }
            // a@Type form: VarRef with annotation — resolve via typecheck_annot::resolve_annotation.
            // If the annotation is a Simple name that's in ann_mapping (declared via bind:),
            // look up the TypeVar ID and resolve it from the type state rather than creating
            // a new Unknown. This enables `target@a source@a` to share the same TypeVar.
            SurfaceExpression::VarRef {
                annotation: Some(ann),
                ..
            } => {
                // Simple annotation whose name is in bind_vars (declared via bind:).
                // Return the same TypeVar Arc so that `target@a source@a` share one TypeVar,
                // enforcing that both positions must have equal types.
                let tv = if let crate::ast::Annotation::Simple(ann_name) = &ann.node {
                    if let Some(bound_tv) = bind_vars.get(ann_name) {
                        Arc::clone(bound_tv)
                    } else {
                        let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
                        let mut ann_m: Option<&mut HashMap<String, String>> = None;
                        let mut row_m: Option<&mut HashMap<String, String>> = None;
                        typecheck_annot::resolve_annotation(
                            &ann.node,
                            ann.span.clone(),
                            &mut *state,
                            &mut constraints,
                            &mut ann_m,
                            &mut row_m,
                            None,
                        )
                        .await
                        .map_err(|e| vec![e])?
                    }
                } else {
                    let mut constraints: Vec<Arc<crate::value::Value>> = Vec::new();
                    let mut ann_m: Option<&mut HashMap<String, String>> = None;
                    let mut row_m: Option<&mut HashMap<String, String>> = None;
                    typecheck_annot::resolve_annotation(
                        &ann.node,
                        ann.span.clone(),
                        &mut *state,
                        &mut constraints,
                        &mut ann_m,
                        &mut row_m,
                        None,
                    )
                    .await
                    .map_err(|e| vec![e])?
                };
                types.push(tv);
            }
            // Bare identifier in pattern position: represents a type variable (any type).
            // Use a fresh TypeVar rather than Unknown.
            SurfaceExpression::VarRef { .. } => {
                types.push(state.fresh_type_var(&binding.span));
            }
            // Gradual: wildcard placeholder
            SurfaceExpression::Placeholder(..) => {
                types.push(make_typevalue_unknown());
            }
            _ => {
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
                    "pattern binding must be in form 'a@Type', bare identifier, or [let ...]",
                    binding.span.clone(),
                )]);
            }
        }
        Ok(())
    })
}

/// Shared probe helper: attempt to unify each pair of TypeValues from `types_a` and `types_b`.
///
/// Saves all mutable InferState fields that `unify` may touch, runs the probe, then
/// restores them unconditionally — so this function is always side-effect-free.
///
/// Returns `Ok(true)` if every pair unified, `Ok(false)` if any pair failed to unify or
/// the slices have different lengths, and `Err` only on an internal diagnostic that cannot
/// be represented as a simple bool result (currently unreachable in practice).
async fn try_unify_probe(
    types_a: &[Arc<crate::value::Value>],
    types_b: &[Arc<crate::value::Value>],
    state: &mut InferState,
) -> Result<bool, TypeDiagnostic> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_constraints = state.constraints.clone();
    let saved_deferred = state.deferred_equalities.clone();
    let saved_ctx = state.ctx.clone();
    let saved_diagnostics = state.diagnostics.clone();

    // Attempt actual unification for each TypeValue pair.
    let mut unified = true;
    let span = crate::rust_span!();
    for (tv_a, tv_b) in types_a.iter().zip(types_b.iter()) {
        let mut temp_constraints = Vec::new();
        match crate::types::unify(
            tv_a,
            tv_b,
            &mut state.ctx,
            &mut temp_constraints,
            span.clone(),
            0,
        )
        .await
        {
            Ok(()) => {
                // This pair can unify — continue.
            }
            Err(_td) => {
                // Probe unification failure: the TypeDiagnostic means these two TypeValues
                // cannot unify — there is no overlap. State is restored unconditionally below.
                unified = false;
                break;
            }
        }
    }

    // Restore all mutated fields unconditionally.
    state.constraints = saved_constraints;
    state.deferred_equalities = saved_deferred;
    state.ctx = saved_ctx;
    state.diagnostics = saved_diagnostics;

    Ok(unified)
}

/// Check if two pattern TypeValue lists could overlap (unify).
///
/// Delegates to `try_unify_probe`, which is always side-effect-free.
pub(crate) async fn patterns_overlap(
    types_a: &[Arc<crate::value::Value>],
    types_b: &[Arc<crate::value::Value>],
    state: &mut InferState,
) -> Result<bool, Vec<TypeDiagnostic>> {
    try_unify_probe(types_a, types_b, state)
        .await
        .map_err(|e| vec![e])
}

/// Probe whether two TypeValue slices can unify (for consistency checks).
/// Returns true if all pairs successfully unify. Side-effect-free — restores state after probe.
///
/// Delegates to `try_unify_probe`, which is always side-effect-free.
pub(crate) async fn types_can_unify(
    types_a: &[Arc<crate::value::Value>],
    types_b: &[Arc<crate::value::Value>],
    state: &mut InferState,
) -> Result<bool, Vec<TypeDiagnostic>> {
    try_unify_probe(types_a, types_b, state)
        .await
        .map_err(|e| vec![e])
}

/// Extract parameter indices from a functional dependency variable list.
/// Accepts a single param name (VarRef/Str), a Dict list [a b c], or an implied
/// Call `[a b]` (which the parser produces when `a` is in head position).
/// Returns Vec<usize> of indices into the class params list.
pub(crate) fn extract_param_indices(
    node: &Arc<SurfaceNode>,
    params: &[String],
    span: Span,
) -> Result<Vec<usize>, Vec<TypeDiagnostic>> {
    let mut indices = Vec::new();

    match &node.expr {
        // Single param: a@Type or just "a"
        SurfaceExpression::VarRef { name, .. }
        | SurfaceExpression::StringLiteral { content: name, .. } => {
            if let Some(idx) = params.iter().position(|p| p == name) {
                indices.push(idx);
            } else {
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
                    format!("functional dependency references unknown param '{}'", name),
                    span,
                )]);
            }
        }
        // Multiple params as auto-indexed Dict: produced when bracket contains
        // a literal/annotated head (e.g. `[a@Int b]` → Dict with auto-indexed entries)
        SurfaceExpression::Dict(entries) => {
            for entry in entries {
                let param_name = match &entry.node.value.expr {
                    SurfaceExpression::VarRef { name, .. } => name,
                    SurfaceExpression::StringLiteral { content: s, .. } => s,
                    _ => {
                        return Err(vec![TypeDiagnostic::error(
                            "type-error",
                            "functional dependency param must be an identifier or string",
                            entry.span.clone(),
                        )]);
                    }
                };

                if let Some(idx) = params.iter().position(|p| p == param_name) {
                    indices.push(idx);
                } else {
                    return Err(vec![TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "functional dependency references unknown param '{}'",
                            param_name
                        ),
                        entry.span.clone(),
                    )]);
                }
            }
        }
        // Multiple params as implied Call: produced when bracket has identifier in head
        // position, e.g. `[a b]` → Call { func: VarRef("a"), args: [VarRef("b")] }
        SurfaceExpression::Call {
            func,
            args,
            implied: true,
            ..
        } => {
            // Extract the function (head param)
            let head_name = match &func.expr {
                SurfaceExpression::VarRef { name, .. } => name,
                SurfaceExpression::StringLiteral { content: s, .. } => s,
                _ => {
                    return Err(vec![TypeDiagnostic::error(
                        "type-error",
                        "functional dependency param must be an identifier or string",
                        func.span.clone(),
                    )])
                }
            };
            if let Some(idx) = params.iter().position(|p| p == head_name) {
                indices.push(idx);
            } else {
                return Err(vec![TypeDiagnostic::error(
                    "type-error",
                    format!(
                        "functional dependency references unknown param '{}'",
                        head_name
                    ),
                    func.span.clone(),
                )]);
            }
            // Extract the remaining args
            for arg in args {
                let arg_name = match &arg.expr {
                    SurfaceExpression::VarRef { name, .. } => name,
                    SurfaceExpression::StringLiteral { content: s, .. } => s,
                    _ => {
                        return Err(vec![TypeDiagnostic::error(
                            "type-error",
                            "functional dependency param must be an identifier or string",
                            arg.span.clone(),
                        )])
                    }
                };
                if let Some(idx) = params.iter().position(|p| p == arg_name) {
                    indices.push(idx);
                } else {
                    return Err(vec![TypeDiagnostic::error(
                        "type-error",
                        format!(
                            "functional dependency references unknown param '{}'",
                            arg_name
                        ),
                        arg.span.clone(),
                    )]);
                }
            }
        }
        _ => {
            return Err(vec![TypeDiagnostic::error(
                "type-error",
                "functional dependency variables must be an identifier or list",
                span,
            )]);
        }
    }

    Ok(indices)
}
