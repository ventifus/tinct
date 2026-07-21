//! Path-sensitive narrowing and overlap checking.
//!
//! This module contains the subsystem responsible for:
//! - Extracting type narrowing constraints from conditional expressions (`if`, match guards)
//! - Applying those constraints to fork the type environment for true/false branches
//! - Instance pattern type extraction and functional-dependency parameter index resolution
//! - Pattern overlap / type unification probes (side-effect-free)
//!
//! ## Prelude coupling (B-507)
//!
//! `extract_narrowings` recognizes specific predicate function names (`int?`, `str?`,
//! `bool?`, `seq?`, etc.) by string match. This couples the narrower to the prelude's
//! naming conventions. The principled fix is annotation-based narrowing: predicate
//! functions would declare their narrowing behavior via annotations (e.g.,
//! `@[narrows: [type: Int]]`), and the type checker would read these annotations at
//! call sites. This requires design work tracked separately.
//!
//! The primitive types narrowed to (`Type::Int`, `Type::Float`, `Type::Str`) are Rust-level
//! and prelude-agnostic. Predicates for prelude-defined types (`bool?`, `seq?`) currently
//! narrow to `Type::Unknown` — the principled fix (annotation-based narrowing) is tracked
//! as T-1761, which would allow predicates to declare their narrowing behavior via annotations.

use std::sync::{Arc, RwLock};

use crate::ast::{Annotation, Span, SurfaceExpression, SurfaceNode};
use crate::env::Env;
use crate::error::TypeDiagnostic;
use crate::types::{InferState, Row, Type, TypeScheme};

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
pub(crate) fn extract_narrowings(cond: &Arc<SurfaceNode>) -> Vec<Narrowing> {
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
                            narrowings.extend(extract_narrowings(arg));
                        }
                        return narrowings;
                    }
                    // Pattern: [int? x], [str? x], [dict? x], [bool? x], [float? x],
                    // [fn? x], [null? x], [seq? x], [num? x]
                    "int?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Int,
                            }];
                        }
                    }
                    "str?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Str,
                            }];
                        }
                    }
                    "dict?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // dict? narrows to open record with no fields
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Dict(Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                }),
                            }];
                        }
                    }
                    "bool?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // T-1761: principled annotation-based narrowing needed.
                            // Boolean is a prelude-defined type; Rust must not hardcode its name.
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Unknown,
                            }];
                        }
                    }
                    "float?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Float,
                            }];
                        }
                    }
                    "fn?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Function {
                                    params: vec![],
                                    ret: Box::new(Type::Unknown),
                                    typed_variadics: vec![],
                                    rest: Some(Box::new(("rest".to_string(), Type::Unknown))),
                                    required_count: 0,
                                },
                            }];
                        }
                    }
                    "null?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // null? narrows to empty closed record
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Dict(Row {
                                    fields: indexmap::IndexMap::new(),
                                    tail: crate::type_def::RowTail::Empty,
                                }),
                            }];
                        }
                    }
                    "seq?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // T-1761: principled annotation-based narrowing needed.
                            // Seq is a prelude-defined type; Rust must not hardcode its name.
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::Unknown,
                            }];
                        }
                    }
                    "num?" if args.len() == 1 => {
                        if let SurfaceExpression::VarRef { name: var_name, .. } = &args[0].expr {
                            // num? narrows to Int | Float
                            return vec![Narrowing::TypeOf {
                                var: var_name.clone(),
                                ty: Type::normalize_union(vec![Type::Int, Type::Float]),
                            }];
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
            // T-1761: annotation-based narrowing needed for principled bool narrowing.
            SurfaceExpression::VarRef { name: ref n, .. } if n == "true" || n == "false" => {
                Some(Narrowing::EqLiteral {
                    var: name.clone(),
                    ty: Type::Unknown,
                })
            }
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
                                // T-1761: "Bool" and "Seq" are prelude-defined types;
                                // Rust must not hardcode their TyCon names. Use Unknown
                                // until annotation-based narrowing is implemented.
                                "Bool" | "Seq" => Some(Type::Unknown),
                                "Number" => {
                                    Some(Type::normalize_union(vec![Type::Int, Type::Float]))
                                }
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
/// - `SurfaceExpression::Annotated { annotation, .. }` — `a@Type` form; resolves the annotation
/// - `SurfaceExpression::VarRef { .. }` — bare identifier; treated as `Type::Unknown`
///
/// Resolve a type annotation synchronously for use in instance pattern extraction.
///
/// Returns a concrete type for primitives and known names, or a fresh TypeVar for
/// complex/unresolvable annotations. Never returns `Type::Unknown` — that would
/// trigger T017 for annotated patterns that simply have unresolvable type names.
fn resolve_annotation_sync(ann: &crate::ast::Spanned<Annotation>, state: &mut InferState) -> Type {
    fn resolve_name(name: &str) -> Type {
        match name {
            "Int" | "Integer" => Type::Int,
            "Float" => Type::Float,
            "String" | "Str" => Type::Str,
            "Bytes" => Type::Bytes,
            "Any" => Type::Any,
            "Unknown" => Type::Unknown,
            other => Type::TyCon(other.to_string()),
        }
    }

    match &ann.node {
        Annotation::Simple(name) => resolve_name(name),
        Annotation::Quote => {
            // The quoting annotation does not constrain to a statically-known type;
            // no narrowing can be derived from it.
            state.fresh_type_var(&ann.span)
        }
        Annotation::PropertyDict(entries) => {
            // User-written @[type: T  default: ...] form — extract the type from the `type:` key.
            for entry in entries {
                let key_is_type = entry.node.key.as_ref().is_some_and(|k| {
                    matches!(&k.expr, SurfaceExpression::StringLiteral { content: s, .. } if s == "type")
                        || matches!(&k.expr, SurfaceExpression::VarRef { name, .. } if name == "type")
                });
                if key_is_type {
                    if let SurfaceExpression::VarRef { name, .. } = &entry.node.value.expr {
                        return resolve_name(name);
                    }
                    // Complex type expression (e.g., [List a]) — fall back to fresh TypeVar
                    return state.fresh_type_var(&ann.span);
                }
            }
            state.fresh_type_var(&ann.span)
        }
        Annotation::Annotated(name, _) => resolve_name(name),
    }
}

pub(crate) fn extract_pattern_types(
    pattern_node: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
) -> Result<Vec<Type>, Vec<TypeDiagnostic>> {
    match &pattern_node.expr {
        SurfaceExpression::PatternDecl { bindings } | SurfaceExpression::LetDecl { bindings } => {
            let mut types = Vec::new();
            for binding in bindings {
                extract_binding_types(binding, env, state, &mut types)?;
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
/// - `SurfaceExpression::Call { func, args, .. }` — implied call `[Type]` or `[Type arg1 arg2]`; infers the call type
/// - `SurfaceExpression::Annotated { annotation, .. }` — `a@Type` form
/// - `SurfaceExpression::VarRef { .. }` — bare identifier → `Type::Unknown`
/// - `SurfaceExpression::Placeholder(..)` — wildcard `_` → `Type::Unknown`
#[allow(clippy::only_used_in_recursion)]
pub(crate) fn extract_binding_types(
    binding: &Arc<SurfaceNode>,
    env: &Arc<RwLock<Env>>,
    state: &mut InferState,
    types: &mut Vec<Type>,
) -> Result<(), Vec<TypeDiagnostic>> {
    match &binding.expr {
        // Binding bracket [a@Int b@Float] parsed as auto-indexed Dict (old syntax for multi-param).
        // Named-key dicts like [key: k  value: v] represent a SINGLE structural type (a record),
        // not multiple independent type parameters. Only auto-indexed (keyless) dicts expand.
        SurfaceExpression::Dict(entries) => {
            let all_keyless = entries.iter().all(|e| e.node.key.is_none());
            if all_keyless {
                for entry in entries {
                    extract_binding_types(&entry.node.value, env, state, types)?;
                }
            } else {
                // Named-key dict: single compound type (structural/record type)
                types.push(state.fresh_type_var(&binding.span));
            }
        }
        // Inner binding bracket [let a@Int b@Float] (new unified-bindings syntax)
        SurfaceExpression::LetDecl { bindings } => {
            for sub_binding in bindings {
                extract_binding_types(sub_binding, env, state, types)?;
            }
        }
        // Implied call [Int] or [Result String] — treat as a type name reference.
        // [Int] is parsed as Call { func: VarRef("Int"), args: [], implied: true }.
        // Try to resolve the func as a type annotation; fall back to Unknown on failure.
        SurfaceExpression::Call {
            func,
            args,
            implied: true,
            ..
        } if args.is_empty() => {
            // resolve_annotation is async; push Unknown as fallback in sync context
            types.push(Type::Unknown);
        }
        // Multi-arg implied call [Result String] or other complex type expressions:
        // treat as Unknown (full parametric type resolution is future work).
        SurfaceExpression::Call { .. } => {
            types.push(Type::Unknown);
        }
        // a@Type form: VarRef with annotation
        SurfaceExpression::VarRef {
            annotation: Some(ann),
            ..
        } => {
            // Resolve simple annotations synchronously to avoid T017 false positives.
            // complex annotations fall back to a fresh TypeVar (not Unknown) so T017 is suppressed.
            let ty = resolve_annotation_sync(ann, state);
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
}

/// Check if two pattern type lists could overlap (unify).
///
/// This is a pure probe: it saves and restores all mutable fields of `state`
/// that `unify` touches (levels, constraints, kind_env) so that overlap testing
/// never leaks side-effects into the global inference state.
pub(crate) fn patterns_overlap(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, Vec<TypeDiagnostic>> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_levels = state.levels.clone();
    let saved_constraints = state.constraints.clone();
    let saved_kind_env = state.kind_env.clone();
    let saved_deferred = state.deferred_equalities.clone();
    // Also save subst: improve_functional_dependency writes directly to
    // state.subst (via std::mem::take/replace) rather than through temp_subst.
    let saved_subst = state.subst.clone();

    // Use a temporary substitution so state.subst is also unaffected.
    let _temp_subst = state.subst.clone();
    let overlaps = types_a.iter().zip(types_b.iter()).all(|(ty_a, ty_b)| {
        // Gradual: Unknown is the gradual-typing wildcard for unannotated pattern bindings.
        // Treat Unknown as distinct from any concrete type: a position with Unknown
        // cannot be used to establish overlap (it carries no type information).
        if matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown) {
            return false; // non-overlapping at this position — Unknown is not concrete
        }
        // unify is async; use structural equality as conservative approximation
        ty_a == ty_b
    });

    // Restore all mutated fields.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;

    Ok(overlaps)
}

/// Probe whether two type slices can unify (for consistency checks).
/// Returns true if all pairs successfully unify. Side-effect-free — restores state after probe.
pub(crate) fn types_can_unify(
    types_a: &[Type],
    types_b: &[Type],
    state: &mut InferState,
) -> Result<bool, Vec<TypeDiagnostic>> {
    if types_a.len() != types_b.len() {
        return Ok(false);
    }

    // Early bailout: if top-level constructors clearly differ, skip expensive unification.
    for (ty_a, ty_b) in types_a.iter().zip(types_b.iter()) {
        match (ty_a, ty_b) {
            // Clearly disjoint constructors
            (Type::Int, Type::Str)
            | (Type::Int, Type::Float)
            | (Type::Str, Type::Float)
            | (Type::Str, Type::Int)
            | (Type::Float, Type::Int)
            | (Type::Float, Type::Str) => return Ok(false),
            _ => {}
        }
    }

    // Save every field that unify() may touch so this probe is side-effect-free.
    let saved_levels = state.levels.clone();
    let saved_constraints = state.constraints.clone();
    let saved_kind_env = state.kind_env.clone();
    let saved_deferred = state.deferred_equalities.clone();
    let saved_subst = state.subst.clone();

    // Use a temporary substitution for the probe.
    // Note: this probe uses a separate temp_subst; constraint checking via
    // check_constraints_on_var may miss bindings from the probe. This is acceptable
    // for instance consistency checks where types are typically concrete annotations,
    // but would need to be addressed for general-purpose unification probes.
    let _temp_subst = state.subst.clone();
    let can_unify = types_a.iter().zip(types_b.iter()).all(|(ty_a, ty_b)| {
        ty_a == ty_b || matches!(ty_a, Type::Unknown) || matches!(ty_b, Type::Unknown)
    });

    // Restore all mutated fields.
    state.levels = saved_levels;
    state.constraints = saved_constraints;
    state.kind_env = saved_kind_env;
    state.deferred_equalities = saved_deferred;
    state.subst = saved_subst;

    Ok(can_unify)
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
