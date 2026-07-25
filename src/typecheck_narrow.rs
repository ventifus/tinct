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
//! Both mechanisms store `TypeScheme.param_narrowings[0] = Some(T)` during Pass 4 of
//! `run_typecheck_dict`. `extract_narrowings` looks up the called function in the
//! type environment and reads `param_narrowings`. Any function — not just prelude
//! predicates — can participate in narrowing by using these annotations.
//!
//! **Predicate narrowing** (`@[is: T]`, `@[narrows: T]`) is entirely annotation-driven — no
//! predicate names are hardcoded in Rust. A custom prelude can name predicates anything.
//!
//! **Structural pattern narrowing** (`=`, `and`, `has?`, `type-of`) uses four function names
//! that are **Axiom 1 protocol entries** (D-8). Rust defines the protocol; the prelude
//! implements it. Any compliant prelude must provide functions with these exact names for
//! structural narrowing to work. This is analogous to `tmpl`/`unindent` (D-3) and
//! `as-typenode` (D-7). See `doc/feature/narrowing.md` §Structural Narrowing Protocol Entries.

use std::sync::{Arc, RwLock};

use super::typecheck_annot;
use crate::ast::{Span, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::error::TypeDiagnostic;
use crate::types::{Constraint, InferState, Row, Type, TypeScheme};

/// Narrowing constraints extracted from conditional expressions.
/// Each constraint refines the type of a variable in the true branch of an `if`.
#[derive(Debug, Clone)]
pub(crate) enum Narrowing {
    /// `[= var literal]` narrows `var` to the literal type.
    EqLiteral { var: String, ty: Type },
    /// `[= [type-of var] "TypeName"]` narrows `var` to the named type.
    TypeOf { var: String, ty: Type },
    /// `[has? var "key"]` narrows `var` to a record with at least that key.
    HasKey { var: String, key: String },
}

/// Extract narrowing constraints from a condition expression (SurfaceNode version).
/// Returns an empty vec for unrecognized patterns.
///
/// `env` is the type environment at the call site, used to look up annotation-based
/// narrowing declarations (`param_narrowings` on the callee's TypeScheme). When a
/// function is annotated with `@[narrows: T]` or has a first parameter annotated with
/// `@[is: T]`, calling it with a single variable argument narrows that variable to `T`
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
                    // If its TypeScheme has `param_narrowings[0] = Some(T)`, then
                    // `[foo? x]` being true narrows `x` to `T`. This is the general
                    // mechanism — any function can declare narrowing via `@[narrows: T]`
                    // or `@[is: T]` on its first parameter.
                    _ if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            let scheme = env.read().ok().and_then(|e| e.get_scheme(name));
                            if let Some(scheme) = scheme {
                                if let Some(Some(narrow_ty)) = scheme.param_narrowings.first() {
                                    return vec![Narrowing::TypeOf {
                                        var: var_name.clone(),
                                        ty: narrow_ty.clone(),
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
                ty: Type::IntLiteral(*n),
            }),
            SurfaceExpression::StringLiteral { content: s, .. } => Some(Narrowing::EqLiteral {
                var: name.clone(),
                ty: Type::StringLiteral(s.clone()),
            }),
            // true/false are plain identifiers in tinct — no native boolean type.
            // No narrowing: x retains its original type. Emitting Unknown would
            // degrade type checking (Axiom 4: no prelude-specific behavior in Rust).
            SurfaceExpression::VarRef { name: ref n, .. } if n == "true" || n == "false" => None,
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
                            let ty = match type_name.as_str() {
                                "Int" => Some(Type::Int),
                                "Float" => Some(Type::Float),
                                "String" => Some(Type::Str),
                                // "Bool" and "Seq" are prelude-defined types — Rust must
                                // not hardcode their TyCon names (Axiom 4). Return None
                                // (no narrowing) so the variable retains its original type.
                                // Emitting Unknown would actively degrade type checking.
                                // Predicate narrowing (@[is: T]) handles these via prelude
                                // annotations instead (B-546).
                                "Bool" | "Seq" => None,
                                _ => None,
                            };
                            return ty.map(|t| Narrowing::TypeOf {
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

/// Apply narrowings to a type environment, creating a refined environment for the true branch.
pub(crate) fn apply_narrowings(
    env: &Arc<RwLock<Env>>,
    narrowings: &[Narrowing],
    state: &mut InferState,
) -> Arc<RwLock<Env>> {
    if narrowings.is_empty() {
        return Arc::clone(env);
    }

    let mut new_env_inner = Env::with_parent(Arc::clone(env));

    for narrowing in narrowings {
        match narrowing {
            Narrowing::EqLiteral { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed.
                // Use insert_scheme_named_only: narrowing frames are not resolver scopes,
                // so their entries must not occupy slotted positions.
                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(ty.clone()));
            }
            Narrowing::TypeOf { var, ty } => {
                // BAS: all tails are Empty — no row var registration needed.
                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(ty.clone()));
            }
            Narrowing::HasKey { var, key } => {
                // Get the current type of the variable (if any)
                let current_ty = env
                    .read()
                    .unwrap()
                    .get_scheme(var)
                    .map(|scheme| scheme.body);

                // Create a record type with at least the given key
                let mut fields: indexmap::IndexMap<String, Type> = indexmap::IndexMap::new();
                let fresh_type_var = state.fresh_type_var(&crate::rust_span!());
                fields.insert(key.clone(), fresh_type_var);

                // BAS: all tails are Empty. Merge existing record fields if present.
                // Width subtyping handles the openness — the record is known to have the
                // key at runtime, and may have additional fields beyond those annotated.
                let new_ty = if let Some(Type::Dict(current_row)) = current_ty {
                    // Merge existing fields with the new constraint
                    for (k, v) in current_row.fields {
                        fields.insert(k, v);
                    }
                    Type::Dict(Row {
                        fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                } else {
                    // Create a fresh record with just the key constraint
                    Type::Dict(Row {
                        fields,
                        tail: crate::type_def::RowTail::Empty,
                    })
                };

                new_env_inner.insert_scheme_named_only(var.clone(), TypeScheme::mono(new_ty));
            }
        }
    }

    Arc::new(RwLock::new(new_env_inner))
}

/// Extract type parameters from an instance pattern declaration.
///
/// The PatternDecl stores the inner bracket `[a@Int b@Float]` as a single `SurfaceExpression::Dict`
/// binding (auto-indexed entries). This function recursively extracts types from either:
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket; extracts each auto-indexed entry
/// - `SurfaceExpression::VarRef { annotation: Some(ann), .. }` — `a@Type` form; resolves via
///   `typecheck_annot::resolve_annotation`
/// - `SurfaceExpression::VarRef { .. }` — bare identifier; treated as a fresh TypeVar
pub(crate) async fn extract_pattern_types(
    pattern_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
) -> Result<Vec<Type>, Vec<TypeDiagnostic>> {
    match &pattern_node.expr {
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            let mut types = Vec::new();
            for binding in bindings {
                extract_binding_types(binding, env, state, &mut types).await?;
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

/// Recursively extract type(s) from a single pattern binding expression.
///
/// - `SurfaceExpression::Dict(entries)` — inner binding bracket `[a@Int b@Float]` (old syntax); expands entries
/// - `SurfaceExpression::LetDecl { bindings }` — inner binding bracket `[let a@Int b@Float]` (new syntax); expands bindings
/// - `SurfaceExpression::Call { func, args, .. }` — implied call `[Type]` or `[Type arg1 arg2]`; treated as Unknown
/// - `SurfaceExpression::VarRef { annotation: Some(ann), .. }` — `a@Type` form; resolved via `resolve_annotation`
/// - `SurfaceExpression::VarRef { .. }` — bare identifier → fresh TypeVar (not Unknown, to suppress T017)
/// - `SurfaceExpression::Placeholder(..)` — wildcard `_` → `Type::Unknown`
///
/// Recursive async functions must return a `BoxFuture` to be object-safe.
pub(crate) fn extract_binding_types<'a>(
    binding: &'a Arc<SurfaceNode>,
    env: &'a Arc<RwLock<Env>>,
    state: &'a mut InferState,
    types: &'a mut Vec<Type>,
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
                        extract_binding_types(&entry.node.value, env, state, types).await?;
                    }
                } else {
                    // Named-key dict: single compound type (structural/record type)
                    types.push(state.fresh_type_var(&binding.span));
                }
            }
            // Inner binding bracket [let a@Int b@Float] (new unified-bindings syntax)
            SurfaceExpression::LetDecl { bindings } => {
                for sub_binding in bindings {
                    extract_binding_types(sub_binding, env, state, types).await?;
                }
            }
            // Implied call form in pattern position.
            //
            // Zero-arg case `[Int]`, `[String]`, `[Boolean]` — the name is a type constructor
            // with no arguments. Try to resolve the func name as a type annotation via
            // `resolve_annotation`. If the name is registered in `type_stage_scope` or
            // `tycon_env` (e.g. `Int`, `String`, `Boolean`), we get the concrete type back.
            // This enables `[pattern [Int]]` to resolve to `Type::Int` rather than a fresh
            // TypeVar, making the instance pattern concrete.
            //
            // Multi-arg case `[Result String]`, `[Channel Int]` — parametric type application.
            // Full resolution of these is future work; use a fresh TypeVar so that unification
            // can still find the correct type and T017 ("contains Unknown") does not fire.
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
                    let mut constraints: Vec<Constraint> = Vec::new();
                    let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
                    let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
                    let ann_result = typecheck_annot::resolve_annotation(
                        &ann,
                        func.span.clone(),
                        &mut *state,
                        &mut constraints,
                        &mut ann_m,
                        &mut row_m,
                        None,
                    )
                    .await;
                    match ann_result {
                        Ok(ty) => Some(ty),
                        Err(_) => None,
                    }
                } else {
                    None
                };
                types.push(resolved.unwrap_or_else(|| state.fresh_type_var(&binding.span)));
            }
            SurfaceExpression::Call { .. } => {
                // Multi-arg parametric call — fresh TypeVar for now.
                types.push(state.fresh_type_var(&binding.span));
            }
            // a@Type form: VarRef with annotation — resolve via typecheck_annot::resolve_annotation.
            // A fresh TypeVar (not Unknown) is used on failure so T017 is suppressed for annotated
            // patterns with complex/unresolvable type names.
            SurfaceExpression::VarRef {
                annotation: Some(ann),
                ..
            } => {
                let mut constraints: Vec<Constraint> = Vec::new();
                let mut ann_m: Option<&mut std::collections::HashMap<String, String>> = None;
                let mut row_m: Option<&mut std::collections::HashMap<String, String>> = None;
                let ty = match typecheck_annot::resolve_annotation(
                    &ann.node,
                    ann.span.clone(),
                    &mut *state,
                    &mut constraints,
                    &mut ann_m,
                    &mut row_m,
                    None,
                )
                .await
                {
                    Ok(t) => t,
                    Err(_) => state.fresh_type_var(&ann.span),
                };
                types.push(ty);
            }
            // Bare identifier in pattern position: represents a type variable (any type).
            // Use a fresh TypeVar rather than Unknown so that:
            // - T017 ("contains Unknown types") doesn't fire for intentional type variables
            // - T016 coverage violations are still correctly detected (TypeVars in determined
            //   positions that don't appear in determining positions still trigger T016)
            SurfaceExpression::VarRef { .. } => {
                types.push(state.fresh_type_var(&binding.span));
            }
            // Gradual: wildcard placeholder
            SurfaceExpression::Placeholder(..) => {
                types.push(Type::Unknown);
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

/// Shared probe helper: attempt to unify each pair of types from `types_a` and `types_b`.
///
/// Saves all 8 mutable InferState fields that `unify` may touch, runs the probe, then
/// restores them unconditionally — so this function is always side-effect-free.
///
/// Returns `Ok(true)` if every pair unified, `Ok(false)` if any pair failed to unify or
/// the slices have different lengths, and `Err` only on an internal diagnostic that cannot
/// be represented as a simple bool result (currently unreachable in practice).
async fn try_unify_probe(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, TypeDiagnostic> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_levels = state.levels.clone();
    let saved_constraints = state.constraints.clone();
    let saved_kind_env = state.kind_env.clone();
    let saved_deferred = state.deferred_equalities.clone();
    let saved_subst = state.subst.clone();
    let saved_type_vars = state.type_vars.clone();
    let saved_bounds = state.bounds.clone();
    let saved_dispatch_obligations = state.dispatch_obligations.clone();
    let saved_diagnostics = state.diagnostics.clone();

    // Attempt actual unification for each type pair.
    let mut unified = true;
    let span = crate::rust_span!();
    for (ty_a, ty_b) in types_a.iter().zip(types_b.iter()) {
        let mut temp_constraints = Vec::new();
        match crate::types::unify(ty_a, ty_b, state, &mut temp_constraints, span.clone(), 0).await {
            Ok(()) => {
                // This pair can unify — continue.
            }
            Err(_) => {
                // Unification failed — no overlap.
                unified = false;
                break;
            }
        }
    }

    // Restore all mutated fields unconditionally.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;
    state.type_vars = saved_type_vars;
    state.bounds = saved_bounds;
    state.dispatch_obligations = saved_dispatch_obligations;
    state.diagnostics = saved_diagnostics;

    Ok(unified)
}

/// Check if two pattern type lists could overlap (unify).
///
/// Delegates to `try_unify_probe`, which is always side-effect-free: it saves and
/// restores all mutable InferState fields that `unify` touches so overlap testing
/// never leaks side-effects into the global inference state.
pub(crate) async fn patterns_overlap(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, Vec<TypeDiagnostic>> {
    try_unify_probe(types_a, types_b, state)
        .await
        .map_err(|e| vec![e])
}

/// Probe whether two type slices can unify (for consistency checks).
/// Returns true if all pairs successfully unify. Side-effect-free — restores state after probe.
///
/// Delegates to `try_unify_probe`, which is always side-effect-free.
pub(crate) async fn types_can_unify(
    types_a: &[Type],
    types_b: &[Type],
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
