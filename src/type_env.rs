//! Type environment, instantiation, generalization, Display, type aliases,
//! class/instance environments, and type errors.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::sync::Arc;

use crate::ast::Span;

use super::*;

/// Instantiate a type by creating fresh type variables at the current level.
/// Used for CALL-POLY: when calling a polymorphic function, instantiate its type
/// at the current level to enable proper generalization (Kiselyov 2013).
///
/// This function registers fresh variables in `state.levels` so they participate in
/// level-based generalization. Without this, fresh variables would default to level 0
/// and be permanently excluded from generalization by [U-VAR-LEVEL].
///
/// **Design note:** This function intentionally freshens ALL type variables in the input type,
/// not just quantified ones (unlike `instantiate_scheme`). This is correct for CALL-POLY because
/// the input `func_ty` is a raw type from `infer_expr(func, ...)`, not a type scheme body.
/// Any type variables in `func_ty` at this point are either:
/// - Fresh variables from the function's own inference (e.g., from type annotations)
/// - Unbound inference variables that need fresh instances for this call site
///
/// Free variables from the enclosing scope would already be bound in `state.subst` and would
/// not appear as TypeVars in the input type. Per Algorithm W, instantiation only applies to
/// the syntactic type variables present in the type expression, which are all treated uniformly here.
pub fn instantiate_at_level(ty: &Type, state: &mut InferState) -> Type {
    // Use Vec instead of HashSet to avoid hash computation overhead for small types.
    // Deduplication is handled by the contains_key guard below: only the first occurrence
    // of each type/row var generates a fresh variable. Subsequent occurrences are skipped.
    let mut type_vars = Vec::new();
    ty.collect_all_vars_vec(&mut type_vars);

    // Monomorphic fast-path: if no type vars, return ty directly (saves HashMap allocation)
    if type_vars.is_empty() {
        return ty.clone();
    }

    // Collect all Operator names from the original type to preserve their kind
    let mut operator_names = HashSet::new();
    ty.collect_operator_names(&mut operator_names);

    // Use with_capacity so the HashMap internal array is allocated exactly once,
    // avoiding a resize when the type var count is known upfront (CALL-POLY hot path).
    // Note: capacity hint may be larger than actual unique count if there are duplicates,
    // but this wastes at most a few slots and is cheaper than deduplicating first.
    let renaming = Substitution {
        type_map: std::cell::RefCell::new(HashMap::with_capacity(type_vars.len())),
    };
    for var in type_vars {
        // First-write-wins: skip if this var was already mapped (handles duplicates from the Vec).
        if !renaming.type_map.borrow().contains_key(&var) {
            let fresh_name = format!("_t{}", state.name_counter);
            state.name_counter = state.name_counter.saturating_add(1);
            state.levels.insert(fresh_name.clone(), state.level);

            // If this var appears as Type::Operator in the original type, preserve the Operator kind
            if operator_names.contains(&var) {
                // Register the fresh name in kind_env as Kind::Operator
                state.kind_env.insert(fresh_name.clone(), Kind::Operator);
                renaming
                    .type_map
                    .borrow_mut()
                    .insert(var, Type::Operator(fresh_name));
            } else {
                renaming
                    .type_map
                    .borrow_mut()
                    .insert(var, Type::TypeVar(fresh_name, state.level));
            }
        }
    }

    renaming.apply(ty)
}

/// Rename a single type variable `old_name -> Type::TypeVar(fresh_name, level)` inline.
///
/// This is equivalent to `Substitution { type_map: {old_name -> TypeVar(fresh,level)},
/// row_map: {} }.apply(ty)` but avoids allocating 2 HashMaps and 2 HashSets.
/// Safe to use without cycle detection because scheme bodies from `generalize` are
/// acyclic with respect to quantified type variables (no self-referential TypeVar bindings
/// can appear in a scheme body -- TypeVars in a scheme are free variables, not bound ones).
fn rename_single_type_var(ty: &Type, old_name: &str, fresh_name: &str, level: u32) -> Type {
    match ty {
        Type::TypeVar(name, _) if name == old_name => Type::TypeVar(fresh_name.to_owned(), level),
        Type::TypeVar(_, _) => ty.clone(),
        Type::Record(row) => Type::Record(rename_single_type_var_in_row(
            row, old_name, fresh_name, level,
        )),
        Type::Function {
            params,
            ret,
            variadic,
        } => Type::Function {
            params: params
                .iter()
                .map(|(name, p_ty)| {
                    (
                        name.clone(),
                        rename_single_type_var(p_ty, old_name, fresh_name, level),
                    )
                })
                .collect(),
            ret: Box::new(rename_single_type_var(ret, old_name, fresh_name, level)),
            variadic: *variadic,
        },
        Type::Seq(elem) => Type::Seq(Box::new(rename_single_type_var(
            elem, old_name, fresh_name, level,
        ))),
        Type::Map(key, val) => Type::Map(
            Box::new(rename_single_type_var(key, old_name, fresh_name, level)),
            Box::new(rename_single_type_var(val, old_name, fresh_name, level)),
        ),
        Type::Union(members) => Type::Union(
            members
                .iter()
                .map(|m| rename_single_type_var(m, old_name, fresh_name, level))
                .collect(),
        ),
        Type::Intersection(members) => Type::Intersection(
            members
                .iter()
                .map(|m| rename_single_type_var(m, old_name, fresh_name, level))
                .collect(),
        ),
        Type::App(f, a) => Type::App(
            Box::new(rename_single_type_var(f, old_name, fresh_name, level)),
            Box::new(rename_single_type_var(a, old_name, fresh_name, level)),
        ),
        Type::Operator(name) if name == old_name => Type::Operator(fresh_name.to_owned()),
        Type::Operator(_) => ty.clone(),
        Type::Negation(inner) => Type::Negation(Box::new(rename_single_type_var(
            inner, old_name, fresh_name, level,
        ))),
        Type::TypeStageApp { fn_name, args } => Type::TypeStageApp {
            fn_name: fn_name.clone(),
            args: args
                .iter()
                .map(|arg| rename_single_type_var(arg, old_name, fresh_name, level))
                .collect(),
        },
        Type::NominalVariant { tag, fields } => Type::NominalVariant {
            tag: tag.clone(),
            fields: rename_single_type_var_in_row(fields, old_name, fresh_name, level),
        },
        // Primitives, Any, Error, Number, Proxy: no type variables inside.
        _ => ty.clone(),
    }
}

fn rename_single_type_var_in_row(row: &Row, old_name: &str, fresh_name: &str, level: u32) -> Row {
    Row {
        fields: row
            .fields
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    rename_single_type_var(v, old_name, fresh_name, level),
                )
            })
            .collect(),
    }
}

/// Instantiate a type scheme by creating fresh type variables at the given level.
/// Used for VAR-POLY: when a polymorphic binding is referenced, create fresh instances.
pub fn instantiate_scheme(scheme: &TypeScheme, level: u32, state: &mut InferState) -> Type {
    if scheme.type_vars.is_empty() {
        // Monomorphic scheme: return body directly
        return scheme.body.clone();
    }

    // Build variable renaming map (old names -> fresh names)
    let mut var_renaming: HashMap<String, String> = HashMap::new();

    // Fast path: single type variable -- avoid building Substitution (HashMap + apply HashSet).
    // Inline rename is allocation-free aside from the string format for the fresh name.
    if scheme.type_vars.len() == 1 {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(scheme.type_vars[0].clone(), fresh_name.clone());

        // Copy constraints with renamed variables
        for constraint in &scheme.constraints {
            match constraint {
                Constraint::Class { class, vars } => {
                    // Rename all vars in the constraint
                    let fresh_vars: Vec<String> = vars
                        .iter()
                        .filter_map(|v| var_renaming.get(v).cloned())
                        .collect();
                    if fresh_vars.len() == vars.len() {
                        state.constraints.push(Constraint::Class {
                            class: Arc::clone(class),
                            vars: fresh_vars,
                        });
                    }
                }
                Constraint::HasField {
                    label,
                    dict_var,
                    field_var,
                } => {
                    // Rename both dict_var and field_var
                    if let (Some(fresh_dict_var), Some(fresh_field_var)) =
                        (var_renaming.get(dict_var), var_renaming.get(field_var))
                    {
                        // Rename label variable if it's a Label::Var
                        let fresh_label = match label {
                            Label::Concrete(s) => Label::Concrete(s.clone()),
                            Label::Var(var_name) => {
                                if let Some(fresh_name) = var_renaming.get(var_name) {
                                    Label::Var(fresh_name.clone())
                                } else {
                                    // Label::Var not in var_renaming must be a free variable
                                    // from an outer scope or registered in kind_env with Kind::Label
                                    Label::Var(var_name.clone())
                                }
                            }
                        };

                        state.constraints.push(Constraint::HasField {
                            label: fresh_label,
                            dict_var: fresh_dict_var.clone(),
                            field_var: fresh_field_var.clone(),
                        });
                    }
                }
            }
        }

        // Re-register label vars in kind_env with Kind::Label
        if scheme.label_vars.contains(&scheme.type_vars[0]) {
            state.kind_env.insert(fresh_name.clone(), Kind::Label);
        }

        return rename_single_type_var(&scheme.body, &scheme.type_vars[0], &fresh_name, level);
    }

    // General path: multiple type variables -- build a full Substitution.
    let renaming = Substitution {
        type_map: std::cell::RefCell::new(HashMap::with_capacity(scheme.type_vars.len())),
    };
    for var in &scheme.type_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(var.clone(), fresh_name.clone());
        renaming
            .type_map
            .borrow_mut()
            .insert(var.clone(), Type::TypeVar(fresh_name.clone(), level));

        // Re-register label vars in kind_env with Kind::Label
        if scheme.label_vars.contains(var) {
            state.kind_env.insert(fresh_name, Kind::Label);
        }
    }

    // Copy constraints with renamed variables
    for constraint in &scheme.constraints {
        match constraint {
            Constraint::Class { class, vars } => {
                // Rename all vars in the constraint
                let fresh_vars: Vec<String> = vars
                    .iter()
                    .filter_map(|v| var_renaming.get(v).cloned())
                    .collect();
                if fresh_vars.len() == vars.len() {
                    state.constraints.push(Constraint::Class {
                        class: Arc::clone(class),
                        vars: fresh_vars,
                    });
                }
            }
            Constraint::HasField {
                label,
                dict_var,
                field_var,
            } => {
                // Rename both dict_var and field_var
                if let (Some(fresh_dict_var), Some(fresh_field_var)) =
                    (var_renaming.get(dict_var), var_renaming.get(field_var))
                {
                    // Rename label variable if it's a Label::Var
                    let fresh_label = match label {
                        Label::Concrete(s) => Label::Concrete(s.clone()),
                        Label::Var(var_name) => {
                            if let Some(fresh_name) = var_renaming.get(var_name) {
                                Label::Var(fresh_name.clone())
                            } else {
                                // Label::Var not in var_renaming must be a free variable
                                // from an outer scope or registered in kind_env with Kind::Label
                                Label::Var(var_name.clone())
                            }
                        }
                    };

                    state.constraints.push(Constraint::HasField {
                        label: fresh_label,
                        dict_var: fresh_dict_var.clone(),
                        field_var: fresh_field_var.clone(),
                    });
                }
            }
        }
    }

    renaming.apply(&scheme.body)
}

/// Simplify a set of constraints by removing redundant constraints.
///
/// A constraint C is redundant if it is entailed by another constraint in the set.
/// For example, if both `Comparable a` and `Equatable a` are present, `Equatable a`
/// is redundant because Comparable has Equatable as a superclass.
///
/// This implements the constraint simplification step from Jones (1992)
/// "Type Classes: Exploring the Design Space".
pub(crate) fn simplify_constraints(class_env: &ClassEnv, constraints: &mut Vec<Constraint>) {
    // Snapshot the constraints so retain's closure can read from a separate copy
    let snapshot = constraints.clone();
    constraints.retain(|target| {
        // Keep the target if no other constraint entails it
        !snapshot.iter().any(|other| {
            // Don't compare a constraint with itself
            other != target && entails(class_env, std::slice::from_ref(other), target)
        })
    });
}

/// Generalize a type at a binding boundary by quantifying free type variables
/// whose level is strictly greater than the enclosing scope level.
/// Used for let-generalization: ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ
///
/// Defense-in-depth: applies the current substitution first, per Damas & Milner (1982).
/// Generalization must operate over the image of the substitution, not the raw type.
///
/// Diagnostics are pushed to `state.diagnostics`. Uses a synthetic span (0:0) for warnings.
/// Prefer `generalize_with_doc` when a real span is available.
pub fn generalize(level: u32, ty: &Type, state: &mut InferState) -> TypeScheme {
    generalize_with_doc(level, ty, state, None, crate::ast::Span::origin())
}

/// Emit T013 diagnostics for constraints whose type variables are ambiguous (appear in
/// the constraint but not in the surrounding type, so the constraint will be silently
/// dropped at generalization time).
///
/// Only `Label::Var` label positions are checked — a `Label::Concrete` string like `"host"`
/// is never present in the substitution map, so checking it would unconditionally return
/// `false` and produce a spurious warning for every HasField constraint with a literal label.
///
/// `subst_snapshot` is a read-only clone of the substitution map taken before this call.
/// A variable is considered "discharged" (already satisfied during unification) when it
/// is bound in the snapshot to a non-TypeVar, non-Operator type.
///
/// `source_names` maps internal TypeVar names to user-visible source names (e.g., `"_t42"` → `"x"`).
/// When present, diagnostics report "ambiguous type variable 'x' (internal: _t42)" for better readability.
fn emit_ambiguous_constraint_diagnostics(
    constraints: &[Constraint],
    subst_snapshot: &HashMap<String, Type>,
    source_names: &HashMap<String, String>,
    diagnostics: &mut Vec<crate::error::TypeDiagnostic>,
    span: crate::ast::Span,
) {
    let is_discharged = |var_name: &str| -> bool {
        subst_snapshot
            .get(var_name)
            .map(|t| !matches!(t, Type::TypeVar(_, _) | Type::Operator(_)))
            .unwrap_or(false)
    };

    // Format a variable name with source name if available
    let format_var_name = |var: &str| -> String {
        if let Some(source_name) = source_names.get(var) {
            format!("'{}' (internal: {})", source_name, var)
        } else {
            format!("'{}'", var)
        }
    };
    for c in constraints {
        match c {
            Constraint::Class { class, vars, .. } => {
                for var in vars {
                    if !is_discharged(var) {
                        diagnostics.push(crate::error::TypeDiagnostic {
                            message: format!(
                                "ambiguous type variable {} in constraint {}: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(var), class
                            ),
                            span,
                            code: "T013",
                            level: crate::error::DiagnosticLevel::Warn,
                        });
                    }
                }
            }
            Constraint::HasField {
                dict_var,
                label,
                field_var,
            } => {
                if !is_discharged(dict_var) {
                    diagnostics.push(crate::error::TypeDiagnostic {
                        message: format!(
                            "ambiguous type variable {} (dict) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                            format_var_name(dict_var)
                        ),
                        span,
                        code: "T013",
                        level: crate::error::DiagnosticLevel::Warn,
                    });
                }
                // Only Label::Var positions can be ambiguous. Label::Concrete strings
                // are never present in the substitution map, so checking them would
                // unconditionally fire a spurious T013 for every HasField with a
                // literal label (false-positive).
                if let Label::Var(label_var) = label {
                    if !is_discharged(label_var) {
                        diagnostics.push(crate::error::TypeDiagnostic {
                            message: format!(
                                "ambiguous label variable {} in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(label_var)
                            ),
                            span,
                            code: "T013",
                            level: crate::error::DiagnosticLevel::Warn,
                        });
                    }
                }
                if !is_discharged(field_var) {
                    diagnostics.push(crate::error::TypeDiagnostic {
                        message: format!(
                            "ambiguous type variable {} (field) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                            format_var_name(field_var)
                        ),
                        span,
                        code: "T013",
                        level: crate::error::DiagnosticLevel::Warn,
                    });
                }
            }
        }
    }
}

/// Generalize a type into a TypeScheme with optional documentation.
///
/// This is the core generalization function used by the type inference engine.
/// The `doc` parameter allows threading documentation strings from source annotations
/// into the TypeScheme for LSP hover display.
///
/// Ambiguous type variables (appearing in constraints but not in the type) trigger
/// diagnostic warnings pushed to `state.diagnostics`. The `span` parameter provides
/// source location for these warnings.
pub fn generalize_with_doc(
    level: u32,
    ty: &Type,
    state: &mut InferState,
    doc: Option<String>,
    span: crate::ast::Span,
) -> TypeScheme {
    // Apply substitution first -- defense-in-depth per Damas & Milner (1982).
    // Generalization must operate over the image of the substitution.
    // Without this, a bound TypeVar would be generalized incorrectly.
    let ty = &state.subst.apply(ty);

    // Early exit for monomorphic types (common case: all-concrete config dicts)
    if !ty.has_inference_vars() {
        // No type variables to generalize, but we may still have constraints.
        // Any constraint when there are no TypeVars is ambiguous (constraint variable
        // appears in constraint but not in the type).
        // Guard: skip constraints already discharged (bound to concrete type) during unification.
        if !state.constraints.is_empty() {
            let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();
            emit_ambiguous_constraint_diagnostics(
                &state.constraints,
                &subst_snapshot,
                &state.type_var_source_names,
                &mut state.diagnostics,
                span,
            );
        }
        return TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            doc,
            inner_schemes: None,
        };
    }

    let mut all_type_vars = Vec::new();
    ty.collect_all_vars_vec(&mut all_type_vars);

    // Filter: keep only vars where levels[var] > level.
    // collect_all_vars_vec may produce duplicates; deduplicate during filter using seen set.
    let mut seen = HashSet::new();
    let generalizable_type_vars: Vec<String> = all_type_vars
        .into_iter()
        .filter(|var| {
            let var_level = state.levels.get(var).copied().unwrap_or(0);
            let is_generalizable = var_level > level;
            // Deduplicate: only include var if we haven't seen it and it's generalizable
            is_generalizable && seen.insert(var.clone())
        })
        .collect();

    if generalizable_type_vars.is_empty() {
        // No type variables to generalize, but we may still have constraints.
        // Any constraint on a TypeVar when there are no generalizable TypeVars is ambiguous
        // (the TypeVar appears in the constraint but not in the type).
        // Guard: skip constraints already discharged (bound to concrete type) during unification.
        if !state.constraints.is_empty() {
            let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();
            emit_ambiguous_constraint_diagnostics(
                &state.constraints,
                &subst_snapshot,
                &state.type_var_source_names,
                &mut state.diagnostics,
                span,
            );
        }

        TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            doc,
            inner_schemes: None,
        }
    } else {
        // Filter constraints: keep only those on generalized variables
        let generalizable_vars: HashSet<String> = generalizable_type_vars.iter().cloned().collect();

        // Snapshot the substitution map so the filter closure can look up TypeVar→TypeVar
        // bindings without borrowing `state` during `state.constraints.iter()`.
        //
        // When a fresh var "_bt0" from `instantiate_scheme` is bound to "_label_0"
        // (the actual label TypeVar from the function param) in `state.subst`, the HasField
        // constraint records "_bt0" as the label var. But "_bt0" is not in `generalizable_vars`
        // (it's a bound intermediate). We must resolve through one substitution level to find
        // the effective free variable "_label_0" before checking generalizable membership.
        let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();

        // Helper: resolve a type variable name through one level of substitution.
        let resolve_var_name = |var_name: &str| -> String {
            match subst_snapshot.get(var_name) {
                Some(Type::TypeVar(resolved_name, _)) => resolved_name.clone(),
                Some(Type::Operator(resolved_name)) => resolved_name.clone(),
                _ => var_name.to_string(),
            }
        };

        // Helper: check if a variable was already discharged (bound to concrete type).
        // Returns true if the constraint was satisfied during unification.
        let is_discharged = |var_name: &str| -> bool {
            subst_snapshot
                .get(var_name)
                .map(|t| !matches!(t, Type::TypeVar(_, _) | Type::Operator(_)))
                .unwrap_or(false)
        };

        // Helper: format a variable name with source name if available
        let format_var_name = |var: &str| -> String {
            if let Some(source_name) = state.type_var_source_names.get(var) {
                format!("'{}' (internal: {})", source_name, var)
            } else {
                format!("'{}'", var)
            }
        };

        // Build generalizable constraints. For each constraint, resolve TypeVar names through one
        // level of substitution before checking generalizable membership AND before storing into
        // the TypeScheme. This handles the case where instantiate_scheme generates fresh vars
        // (e.g., "_bt0") that are immediately bound to the actual free vars (e.g., "_label_0");
        // the raw constraint names "_bt0" would not match generalizable_vars, and would not be
        // correctly renamed by instantiate_scheme at future call sites.
        let mut generalizable_constraints: Vec<Constraint> = Vec::new();
        for c in &state.constraints {
            match c {
                Constraint::Class { class, vars } => {
                    // Resolve all vars through one substitution level
                    let resolved_vars: Vec<String> =
                        vars.iter().map(|v| resolve_var_name(v)).collect();
                    // Keep constraint if ALL resolved vars are generalizable
                    if resolved_vars.iter().all(|v| generalizable_vars.contains(v)) {
                        generalizable_constraints.push(Constraint::Class {
                            class: Arc::clone(class),
                            vars: resolved_vars,
                        });
                    } else {
                        // Diagnostic: ambiguous type variable in constraint
                        // (appears in constraint but not in the type — constraint will be silently dropped)
                        for var in &resolved_vars {
                            if !generalizable_vars.contains(var) && !is_discharged(var) {
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: format!(
                                        "ambiguous type variable {} in constraint {}: appears in constraint but not in the type — constraint will be silently dropped",
                                        format_var_name(var),
                                        class.name
                                    ),
                                    span,
                                    code: "T013",
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                            }
                        }
                    }
                }
                Constraint::HasField {
                    label,
                    dict_var,
                    field_var,
                } => {
                    let effective_dict = resolve_var_name(dict_var);
                    let effective_field = resolve_var_name(field_var);
                    let effective_label = match label {
                        Label::Concrete(s) => Some(Label::Concrete(s.clone())),
                        Label::Var(var_name) => {
                            let resolved = resolve_var_name(var_name);
                            if generalizable_vars.contains(&resolved) {
                                Some(Label::Var(resolved))
                            } else {
                                // Diagnostic: ambiguous label variable
                                if !is_discharged(&resolved) {
                                    state.diagnostics.push(crate::error::TypeDiagnostic {
                                        message: format!(
                                            "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&resolved)
                                        ),
                                        span,
                                        code: "T013",
                                        level: crate::error::DiagnosticLevel::Warn,
                                    });
                                }
                                None // label not generalizable
                            }
                        }
                    };
                    if let Some(eff_label) = effective_label {
                        if generalizable_vars.contains(&effective_dict)
                            && generalizable_vars.contains(&effective_field)
                        {
                            generalizable_constraints.push(Constraint::HasField {
                                label: eff_label,
                                dict_var: effective_dict,
                                field_var: effective_field,
                            });
                        } else {
                            // Diagnostic: ambiguous dict or field variable
                            let dict_ambiguous = !generalizable_vars.contains(&effective_dict);
                            let field_ambiguous = !generalizable_vars.contains(&effective_field);

                            if dict_ambiguous && field_ambiguous {
                                // Emit one aggregated warning for both vars if at least one is not discharged
                                let dict_discharged = is_discharged(&effective_dict);
                                let field_discharged = is_discharged(&effective_field);

                                if !dict_discharged || !field_discharged {
                                    state.diagnostics.push(crate::error::TypeDiagnostic {
                                        message: format!(
                                            "ambiguous type variables {}, {} in constraint HasField: appear in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&effective_dict),
                                            format_var_name(&effective_field)
                                        ),
                                        span,
                                        code: "T013",
                                        level: crate::error::DiagnosticLevel::Warn,
                                    });
                                }
                            } else if dict_ambiguous && !is_discharged(&effective_dict) {
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: format!(
                                        "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                        format_var_name(&effective_dict)
                                    ),
                                    span,
                                    code: "T013",
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                            } else if field_ambiguous && !is_discharged(&effective_field) {
                                state.diagnostics.push(crate::error::TypeDiagnostic {
                                    message: format!(
                                        "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                        format_var_name(&effective_field)
                                    ),
                                    span,
                                    code: "T013",
                                    level: crate::error::DiagnosticLevel::Warn,
                                });
                            }
                        }
                    }
                }
            }
        }

        // Simplify constraints: remove redundant constraints entailed by others
        // For example, if both `Comparable a` and `Equatable a` are present,
        // remove `Equatable a` (it's entailed via Comparable's superclass).
        simplify_constraints(&state.class_env, &mut generalizable_constraints);

        // Collect label vars: TypeVars that are label-kinded (Kind::Label in kind_env)
        let label_vars: Vec<String> = generalizable_type_vars
            .iter()
            .filter(|var| state.kind_env.get(var.as_str()) == Some(&Kind::Label))
            .cloned()
            .collect();

        TypeScheme {
            type_vars: generalizable_type_vars,
            constraints: generalizable_constraints,
            body: ty.clone(),
            label_vars,
            doc,
            inner_schemes: None,
        }
    }
}

/// Pretty-print a type for user-facing output (LSP hover, completions).
///
/// Differences from the internal `Display` impl:
/// - `Type::Unknown` (`_`) → `any`
/// - Empty record (`[]`) → `{}` (avoids confusion with empty list/function-call syntax)
/// - Internal TypeVars (`_t266`) renamed to short alphabetic names (`a`, `b`, …)
///   in order of first appearance; all occurrences of the same var get the same name
/// - Parameter names shown when present (`name: Type` instead of just `Type`)
pub fn pretty_type(ty: &Type) -> String {
    let mut vars_seen: Vec<String> = Vec::new();
    collect_pretty_type_vars(ty, &mut vars_seen);
    let rename: HashMap<String, String> = vars_seen
        .iter()
        .enumerate()
        .map(|(i, name)| (name.clone(), tvar_display_name(i)))
        .collect();
    format_type_pretty(ty, &rename)
}

fn collect_pretty_type_vars(ty: &Type, seen: &mut Vec<String>) {
    match ty {
        Type::TypeVar(name, _) if name.starts_with("_t") && !seen.contains(name) => {
            seen.push(name.clone());
        }
        Type::Function { params, ret, .. } => {
            for (_, p) in params {
                collect_pretty_type_vars(p, seen);
            }
            collect_pretty_type_vars(ret, seen);
        }
        Type::Record(row) => {
            for v in row.fields.values() {
                collect_pretty_type_vars(v, seen);
            }
        }
        Type::Seq(elem) => collect_pretty_type_vars(elem, seen),
        Type::Map(k, v) => {
            collect_pretty_type_vars(k, seen);
            collect_pretty_type_vars(v, seen);
        }
        Type::Union(ms) | Type::Intersection(ms) => {
            for m in ms {
                collect_pretty_type_vars(m, seen);
            }
        }
        Type::Negation(inner) => collect_pretty_type_vars(inner, seen),
        Type::TypeStageApp { fn_name: _, args } => {
            for arg in args {
                collect_pretty_type_vars(arg, seen);
            }
        }
        _ => {}
    }
}

fn format_type_pretty(ty: &Type, rename: &HashMap<String, String>) -> String {
    match ty {
        // Use the tinct annotation names for user-facing display.
        Type::Unknown => "Unknown".to_string(), // annotation: @Unknown or @_
        Type::Top => "Any".to_string(),         // annotation: @Any
        Type::Record(row) if row.fields.is_empty() => "Dict".to_string(), // annotation: @Dict
        Type::TypeVar(name, _) => rename.get(name).cloned().unwrap_or_else(|| name.clone()),
        Type::Record(row) => {
            let mut fields: Vec<_> = row.fields.iter().collect();
            fields.sort_by_key(|(k, _)| k.as_str());
            let inner = fields
                .iter()
                .map(|(k, v)| format!("{k}: {}", format_type_pretty(v, rename)))
                .collect::<Vec<_>>()
                .join(" ");
            format!("[{inner}]")
        }
        Type::Function {
            params,
            ret,
            variadic: _,
        } => {
            let ret_str = format_type_pretty(ret, rename);
            let params_str = params
                .iter()
                .map(|(name, pty)| {
                    let ty_str = match pty {
                        Type::Function { .. } => format!("({})", format_type_pretty(pty, rename)),
                        _ => format_type_pretty(pty, rename),
                    };
                    match name {
                        Some(n) if !n.is_empty() => format!("{n}: {ty_str}"),
                        _ => ty_str,
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            match **ret {
                Type::Function { .. } => format!("Fn@({ret_str}) [{params_str}]"),
                _ => format!("Fn@{ret_str} [{params_str}]"),
            }
        }
        Type::Seq(elem) => format!("Seq[{}]", format_type_pretty(elem, rename)),
        Type::Map(k, v) => format!(
            "Map[{} {}]",
            format_type_pretty(k, rename),
            format_type_pretty(v, rename)
        ),
        Type::Union(ms) => ms
            .iter()
            .map(|m| format_type_pretty(m, rename))
            .collect::<Vec<_>>()
            .join(" | "),
        Type::Intersection(ms) => ms
            .iter()
            .map(|m| format_type_pretty(m, rename))
            .collect::<Vec<_>>()
            .join(" & "),
        Type::Negation(inner) => format!("!{}", format_type_pretty(inner, rename)),
        Type::TypeStageApp { fn_name, args } => {
            let args_str = args
                .iter()
                .map(|arg| format_type_pretty(arg, rename))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{}({})", fn_name, args_str)
        }
        // All other types: fall back to Display (concrete types have no TypeVars to rename)
        other => other.to_string(),
    }
}

/// Same renaming pass applied to an already-formatted type string.
/// Useful when the type was formatted via Display (e.g. TypeScheme).
pub fn pretty_type_str(raw: &str) -> String {
    // First pass: collect _tN names in left-to-right order without duplicates.
    let mut vars: Vec<&str> = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with("_t") {
            let digit_start = i + 2;
            let digit_end = raw[digit_start..]
                .bytes()
                .position(|c| !c.is_ascii_digit())
                .map(|p| digit_start + p)
                .unwrap_or(raw.len());
            if digit_end > digit_start {
                let varname = &raw[i..digit_end];
                if !vars.contains(&varname) {
                    vars.push(varname);
                }
                i = digit_end;
                continue;
            }
        }
        i += raw[i..].chars().next().map_or(1, |c| c.len_utf8());
    }

    if vars.is_empty() {
        return raw.to_string();
    }

    // Build rename table: _tN → a, b, …, z, a1, b1, …
    let rename: HashMap<&str, String> = vars
        .iter()
        .enumerate()
        .map(|(idx, name)| (*name, tvar_display_name(idx)))
        .collect();

    // Second pass: emit the string, substituting _tN tokens.
    let mut result = String::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i..].starts_with("_t") {
            let digit_start = i + 2;
            let digit_end = raw[digit_start..]
                .bytes()
                .position(|c| !c.is_ascii_digit())
                .map(|p| digit_start + p)
                .unwrap_or(raw.len());
            if digit_end > digit_start {
                let varname = &raw[i..digit_end];
                result.push_str(&rename[varname]);
                i = digit_end;
                continue;
            }
        }
        let c = raw[i..].chars().next().unwrap();
        result.push(c);
        i += c.len_utf8();
    }
    result
}

/// Map a 0-based index to a display name: 0→a, 1→b, …, 25→z, 26→a1, 27→b1, …
fn tvar_display_name(idx: usize) -> String {
    let letter = (b'a' + (idx % 26) as u8) as char;
    if idx < 26 {
        letter.to_string()
    } else {
        format!("{}{}", letter, idx / 26)
    }
}

// Display impl for Type moved to type_normalize.rs

/// Parameterized type alias declaration.
///
/// `[type [a b] [first: a second: b]]` stores `params: ["a", "b"]` and
/// `body: Record({first: TypeVar(a), second: TypeVar(b)})`.
///
/// When instantiated (e.g., `[Pair Int String]`), build substitution
/// `{a -> Int, b -> String}` and apply to body.
#[derive(Debug, Clone)]
pub struct TypeAlias {
    pub params: Vec<String>,
    pub body: Type,
}

// ClassDecl, InstanceDecl, ClassEnv, InstanceEnv moved to type_class.rs (chr-module-split)
// Imported via façade: use super::* resolves through types.rs → type_class.rs

#[derive(Debug, Clone)]
pub struct TypeEnv {
    bindings: HashMap<String, TypeScheme>,
    type_aliases: HashMap<String, TypeAlias>,
    parent: Option<Rc<TypeEnv>>,
}

impl TypeEnv {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            type_aliases: HashMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: &Rc<TypeEnv>) -> Self {
        Self {
            bindings: HashMap::new(),
            type_aliases: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    pub fn get(&self, name: &str) -> Option<&TypeScheme> {
        self.lookup(name).map(|(scheme, _)| scheme)
    }

    /// Look up a binding in the CURRENT frame only (does not walk the parent chain).
    ///
    /// Used by `imports::extract_bindings_from_file_with_fallback` to check whether
    /// `merge_env_bindings_into` already inserted a binding into the flat output env,
    /// without accidentally matching builtins in a parent env.
    pub fn get_own(&self, name: &str) -> Option<&TypeScheme> {
        self.bindings.get(name)
    }

    pub fn get_type_alias(&self, name: &str) -> Option<&TypeAlias> {
        self.lookup_type_alias(name).map(|(alias, _)| alias)
    }

    pub(crate) fn lookup(&self, name: &str) -> Option<(&TypeScheme, &HashMap<String, TypeScheme>)> {
        if let Some(scheme) = self.bindings.get(name) {
            return Some((scheme, &self.bindings));
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(scheme) = env.bindings.get(name) {
                return Some((scheme, &env.bindings));
            }
            current = env.parent.as_deref();
        }
        None
    }

    fn lookup_type_alias(&self, name: &str) -> Option<(&TypeAlias, &HashMap<String, TypeAlias>)> {
        if let Some(alias) = self.type_aliases.get(name) {
            return Some((alias, &self.type_aliases));
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(alias) = env.type_aliases.get(name) {
                return Some((alias, &env.type_aliases));
            }
            current = env.parent.as_deref();
        }
        None
    }

    pub fn insert(&mut self, name: String, ty: Type) {
        self.bindings.insert(name, TypeScheme::mono(ty));
    }

    pub fn insert_scheme(&mut self, name: String, scheme: TypeScheme) {
        self.bindings.insert(name, scheme);
    }

    pub fn insert_type_alias(&mut self, name: String, alias: TypeAlias) {
        self.type_aliases.insert(name, alias);
    }

    /// Collect all binding names visible from this environment (including parent scopes).
    ///
    /// Walks the scope chain and inserts every bound name into `names`. Used by
    /// `imports::merge_env_bindings_into` to enumerate what the prelude introduced.
    pub fn collect_all_names(&self, names: &mut std::collections::HashSet<String>) {
        for name in self.bindings.keys() {
            names.insert(name.clone());
        }
        if let Some(ref parent) = self.parent {
            parent.collect_all_names(names);
        }
    }

    /// Create a `TypeEnv` pre-registered with builtin function type signatures.
    ///
    /// This enables the type checker to validate user code that calls builtins.
    /// Polymorphic parameters use `Any` as the escape hatch; precise return types
    /// are specified where known.
    ///
    /// **Type signature conventions:**
    /// - `Any -> Any -> T`: binary operator returning type `T`
    /// - `Any -> T`: unary operator returning type `T`
    /// - `Fn@Any [Any]`: higher-order function (e.g. map, filter) with `Any` for callbacks
    ///
    /// **Coverage:** All builtins from `standard_builtins()` (src/builtins.rs)
    pub fn with_builtins() -> Self {
        let mut env = Self::new();

        // Helper to create DirCap intersection with capability flags.
        // Creates Intersection([DirCap, Flag1Record, Flag2Record, ...]) where each flag
        // is a unique singleton record type registered as a type alias.
        let _dircap_with_flags = |flags: &[&str]| -> Type {
            let mut members = vec![Type::DirCap];
            for flag in flags {
                let mut fields = HashMap::new();
                fields.insert(
                    format!("__cap_flag_{}", flag.to_lowercase()),
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                );
                members.push(Type::Record(Row { fields }));
            }
            Type::normalize_intersection(members)
        };

        // Create ClassDecl instances for arithmetic operators.
        // These match the declarations registered in InferState::new().
        use std::sync::Arc;
        let addable_class = Arc::new(ClassDecl {
            name: "Addable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
        });

        let subtractable_class = Arc::new(ClassDecl {
            name: "Subtractable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
        });

        let multipliable_class = Arc::new(ClassDecl {
            name: "Multipliable".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
        });

        let divisible_class = Arc::new(ClassDecl {
            name: "Divisible".to_string(),
            params: vec![
                ("a".to_string(), Kind::Type),
                ("b".to_string(), Kind::Type),
                ("c".to_string(), Kind::Type),
            ],
            superclasses: vec![],
            determines: vec![(vec![0, 1], vec![2])], // (a,b) → c
            resolver: None,
            resolver_injective: false,
        });

        let equatable_class = Arc::new(ClassDecl {
            name: "Equatable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        let comparable_class = Arc::new(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![("a".to_string(), Kind::Type)],
            superclasses: vec![("Equatable".to_string(), vec!["a".to_string()])],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Addition: Addable a b c => a -> b -> c
        // Multi-parameter type class with functional dependency (a,b) → c
        env.insert_scheme(
            "+".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&addable_class),
                    vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                }],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("c".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Subtraction: Subtractable a b c => a -> b -> c
        env.insert_scheme(
            "-".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&subtractable_class),
                    vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                }],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("c".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Multiplication: Multipliable a b c => a -> b -> c
        env.insert_scheme(
            "*".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&multipliable_class),
                    vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                }],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("c".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Division: Divisible a b c => a -> b -> c
        env.insert_scheme(
            "/".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                constraints: vec![Constraint::Class {
                    class: Arc::clone(&divisible_class),
                    vars: vec!["a".to_string(), "b".to_string(), "c".to_string()],
                }],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("c".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Equality: Equatable a => a -> a -> Bool
        env.insert_scheme(
            "=".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new(Arc::clone(&equatable_class), "a")],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("a".to_string(), 0)),
                    ],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Less-than: Comparable a => a -> a -> Bool
        env.insert_scheme(
            "<".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new(Arc::clone(&comparable_class), "a")],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("a".to_string(), 0)),
                    ],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Control flow: if takes Bool and two branches, returns Top (type depends on branches)
        env.insert(
            "if".to_string(),
            Type::Function {
                params: vec![
                    (Some("condition".to_string()), Type::Bool),
                    (Some("then_".to_string()), Type::Top),
                    (Some("else_".to_string()), Type::Top),
                ],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );

        // Dict primitives
        env.insert(
            "keys".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                )],
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );
        env.insert(
            "length".to_string(),
            Type::Function {
                // builtin_length dispatches on Value::Dict, Value::String, Value::Bytes,
                // and integer-keyed Dicts (which are represented as Seq at the type level).
                params: vec![(
                    None,
                    Type::Union(vec![
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                        Type::Str,
                        Type::Bytes,
                        Type::Seq(Box::new(Type::Top)),
                    ]),
                )],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
        env.insert(
            "merge".to_string(),
            Type::Function {
                params: vec![
                    (
                        Some("dict1".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ),
                    (
                        Some("dict2".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ),
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "append".to_string(),
            Type::Function {
                params: vec![
                    (
                        Some("dict".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ),
                    (Some("value".to_string()), Type::Top),
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );

        // String operations: Showable a => a -> Str
        env.insert_scheme(
            "str".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new_by_name("Showable", "a")],
                body: Type::Function {
                    params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                    ret: Box::new(Type::Str),
                    variadic: true,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        env.insert(
            "split".to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Str)],
                // split returns an integer-keyed Dict of Strings. Typed as Seq[Str] so
                // that `[get N [split sep s]]` returns Str via check_get's Seq[T] arm.
                // (Seq[Str] and Dict[Int→Str] are both valid views; the Seq arm in check_get
                // handles integer indexing and returns the element type Str.)
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );
        env.insert(
            "replace".to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Str), (None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        for name in ["trim", "trim-start", "trim-end"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Str),
                    variadic: false,
                },
            );
        }

        // str-to-upper-char / str-to-lower-char: String → String (single-char primitives)
        for name in ["str-to-upper-char", "str-to-lower-char"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Str),
                    variadic: false,
                },
            );
        }

        // str-map-chars: (String → String) → String → String
        env.insert(
            "str-map-chars".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Str)],
                            ret: Box::new(Type::Str),
                            variadic: false,
                        },
                    ),
                    (None, Type::Str),
                ],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );

        // str-index-of: String → String → Int
        env.insert(
            "str-index-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Str)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );

        // regex-match?: String → String → Bool
        env.insert(
            "regex-match?".to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Str)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // String operations returning Bool
        for name in ["starts-with?", "ends-with?", "str-contains?"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Str), (None, Type::Str)],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            );
        }

        // str-chars: String → Seq
        env.insert(
            "str-chars".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );

        // char-code: String → Int
        env.insert(
            "char-code".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );

        // chr: Int → String
        env.insert(
            "chr".to_string(),
            Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );

        // str-bytes: String → Bytes
        env.insert(
            "str-bytes".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Bytes),
                variadic: false,
            },
        );

        // bytes-str: Bytes → String
        env.insert(
            "bytes-str".to_string(),
            Type::Function {
                params: vec![(None, Type::Bytes)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );

        // bytes: variadic Bytes → Bytes (concat)
        env.insert(
            "bytes".to_string(),
            Type::Function {
                params: vec![],
                ret: Box::new(Type::Bytes),
                variadic: true,
            },
        );

        // bytes-find: Bytes → Bytes → Int
        env.insert(
            "bytes-find".to_string(),
            Type::Function {
                params: vec![(None, Type::Bytes), (None, Type::Bytes)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );

        // bytes-of: Seq → Bytes (or Dict → Bytes)
        env.insert(
            "bytes-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)], // Accepts Seq or Dict
                ret: Box::new(Type::Bytes),
                variadic: false,
            },
        );

        // bytes-equal?: Bytes → Bytes → Bool
        env.insert(
            "bytes-equal?".to_string(),
            Type::Function {
                params: vec![(None, Type::Bytes), (None, Type::Bytes)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // ct-equal?: Bytes → Bytes → Bool
        env.insert(
            "ct-equal?".to_string(),
            Type::Function {
                params: vec![(None, Type::Bytes), (None, Type::Bytes)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // str-slice: String → Int → Int → String
        env.insert(
            "str-slice".to_string(),
            Type::Function {
                params: vec![(None, Type::Str), (None, Type::Int), (None, Type::Int)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );

        // str-length: String → Int
        env.insert(
            "str-length".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );

        // Numeric operations
        for name in ["floor", "round"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Number)],
                    ret: Box::new(Type::Int),
                    variadic: false,
                },
            );
        }

        // Math functions: 1-arg (Number -> Float)
        for name in [
            "sqrt", "log", "log2", "log10", "exp", "sin", "cos", "tan", "asin", "acos", "atan",
        ] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Number)],
                    ret: Box::new(Type::Float),
                    variadic: false,
                },
            );
        }

        // Math functions: 2-arg (Number -> Number -> Float)
        for name in ["pow", "atan2"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Number), (None, Type::Number)],
                    ret: Box::new(Type::Float),
                    variadic: false,
                },
            );
        }

        // Float predicates (Float -> Bool)
        for name in ["nan?", "inf?", "finite?"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Float)],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            );
        }

        // Bitwise operations (Int -> Int -> Int)
        for name in ["band", "bor", "bxor", "shl", "shr"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Int), (None, Type::Int)],
                    ret: Box::new(Type::Int),
                    variadic: false,
                },
            );
        }

        // Parsing
        env.insert(
            "to-int".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
        env.insert(
            "to-float".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Float),
                variadic: false,
            },
        );

        // Evaluation control
        env.insert(
            "deep-materialize".to_string(),
            Type::Function {
                // Genuinely unknown: deep-materialize accepts any thunk/expression and returns an
                // arbitrary value whose type is not knowable at compile time.
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "materialize".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );
        env.insert(
            "raise".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Never),
                variadic: false,
            },
        );
        // try: takes 1 arg — a zero-argument function. Returns Ok(v) or Error(String).
        // Runtime (builtins_meta.rs:builtin_try) enforces exactly 1 arg.
        //
        // Return type is Top rather than a structural union `{ok:T}|{err:Str}` because:
        // 1. The runtime now returns nominal Value::Variant { tag: "Ok"/"Error" }, not a struct dict.
        // 2. A structural union would cause T004 "non-exhaustive match" when user code matches
        //    on constructor patterns `[Ok v]` / `[Error msg]` — the coverage checker would see
        //    DictKey("ok")/DictKey("err") in the sig but Variant("Ok")/Variant("Error") in arms.
        // 3. Top avoids triggering exhaustiveness checking (Type::Union guard in infer_match).
        //
        // See builtin-type-audit sprint: try return type (TODO.md)
        env.insert(
            "try".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Function {
                        params: vec![],
                        ret: Box::new(Type::Top),
                        variadic: false,
                    },
                )],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );
        env.insert(
            "apply".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Top)],
                            ret: Box::new(Type::Top),
                            variadic: false,
                        },
                    ),
                    (
                        None,
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ),
                ],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );

        // Convergence loop: until(pred, f, init) applies f until pred holds
        // ∀T. (T → Bool) → (T → T) → T → T
        env.insert_scheme(
            "until".to_string(),
            TypeScheme {
                type_vars: vec!["T".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![
                        (
                            None,
                            Type::Function {
                                params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                                ret: Box::new(Type::Bool),
                                variadic: false,
                            },
                        ),
                        (
                            None,
                            Type::Function {
                                params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                                ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                                variadic: false,
                            },
                        ),
                        (None, Type::TypeVar("T".to_string(), 0)),
                    ],
                    ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Type introspection — these accept any value (Top), return Str
        env.insert(
            "type-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "llt-repr".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "tag-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "variant".to_string(),
            Type::Function {
                params: vec![],
                // Genuinely unknown: Returns a Variant, but we don't have Type::Variant yet
                ret: Box::new(Type::Unknown),
                variadic: true, // 1 arg (unit variant: tag) or 2 args (tag + payload)
            },
        );
        env.insert(
            "eval-ast".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );
        env.insert(
            "gensym".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Str),
                variadic: true, // 0 or 1 args (optional prefix)
            },
        );
        env.insert(
            "decimal".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                // Genuinely unknown: Returns Decimal type (not yet in Type enum)
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "big-int".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                // Genuinely unknown: Returns BigInt type (not yet in Type enum)
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // Type predicates — accept any value (Top), return Bool
        env.insert(
            "int?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "float?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "num?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "str?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bool?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bytes?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "null?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "dict?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "fn?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "record?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "map?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        // builtin-*? aliases — stable names used by prelude to re-export type predicates
        // without creating circular self-references in letrec dicts.
        for alias in &[
            "builtin-int?",
            "builtin-float?",
            "builtin-str?",
            "builtin-bool?",
            "builtin-null?",
            "builtin-dict?",
            "builtin-fn?",
            "builtin-seq?",
        ] {
            env.insert(
                alias.to_string(),
                Type::Function {
                    params: vec![(None, Type::Top)],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            );
        }

        // I/O
        env.insert(
            "emit".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "env".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                // Returns Str when set; Null (empty dict) when unset, --no-env active, or not in allowlist
                ret: Box::new(Type::normalize_union(vec![
                    Type::Str,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                ])),
                variadic: false,
            },
        );
        env.insert(
            "open".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "slurp".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))],
                // Returns Str for text handles ("r"); Bytes for binary handles ("rb")
                ret: Box::new(Type::normalize_union(vec![Type::Str, Type::Bytes])),
                variadic: false,
            },
        );
        env.insert(
            "lines".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))],
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );
        env.insert(
            "narrow".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::DirCap),
                variadic: false,
            },
        );
        env.insert(
            "write".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "write-atomic".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "revocable".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("cap".to_string(), Type::DirCap),
                        (
                            "revoke".to_string(),
                            Type::Function {
                                params: vec![],
                                ret: Box::new(Type::Record(Row {
                                    fields: HashMap::new(),
                                })), // Null
                                variadic: false,
                            },
                        ),
                    ]),
                })),
                variadic: false,
            },
        );
        env.insert(
            "revoke-cap".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap)],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "connect".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Top), // NetCap or DirCap (UnixStream/UnixDatagram)
                    // Transport variant: Tcp, Udp, UnixStream, UnixDatagram, NamedPipe, Icmp
                    // Constructors are typed via prelude [type [Tcp] [Udp] ...] declaration.
                    // Top instead of Unknown: any value is accepted structurally (callers pass a
                    // Transport variant), but Unknown would silently bypass boundary guards.
                    // Transport type is narrowed via prelude nominal variants (Tcp, Udp, etc.)
                    // registered at runtime; the type parameter uses Top to accept them
                    // structurally; narrowing to a specific Union type is possible once
                    // Type::Variant or a union of runtime-registered types is expressible statically.
                    (None, Type::Top), // Transport variant
                    (None, Type::Str), // host
                    (None, Type::Int), // port
                ],
                // Returns Handle (stream) or DatagramHandle (datagram) depending on transport.
                ret: Box::new(Type::normalize_union(vec![
                    Type::Handle(Box::new(Type::Unknown)),
                    Type::DatagramHandle,
                ])),
                variadic: false,
            },
        );
        // Datagram socket builtins
        env.insert(
            "send-datagram".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::DatagramHandle),                                // socket
                    (None, Type::normalize_union(vec![Type::Str, Type::Bytes])), // String or Bytes
                ],
                ret: Box::new(Type::Record(crate::types::Row {
                    fields: std::collections::HashMap::new(),
                })), // null
                variadic: false,
            },
        );
        env.insert(
            "recv-datagram".to_string(),
            Type::Function {
                params: vec![(None, Type::DatagramHandle)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("data".to_string(), Type::Bytes),
                        ("addr".to_string(), Type::Str),
                        ("port".to_string(), Type::Int),
                    ]),
                })),
                variadic: false,
            },
        );
        // TLS builtins
        env.insert(
            "tls-layer".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Handle(Box::new(Type::Unknown))),
                    (None, Type::Str),
                    // TODO(unknown-elimination): opts is an open record {alpn?: Seq(Str), ...}.
                    // Use an open Record with RowVar tail once opts-dict pattern is established.
                    (None, Type::Top), // opts dict — any record or null
                ],
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "tls-peer-cert".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("subject".to_string(), Type::Str),
                        ("issuer".to_string(), Type::Str),
                        ("sans".to_string(), Type::Seq(Box::new(Type::Str))),
                    ]),
                })),
                variadic: false,
            },
        );
        // spki-pin is now implemented in stdlib/net.llt (pure dict construction, no Rust needed)
        // HTTP-sessions stubs — return Unknown until full implementation lands
        env.insert(
            "quic-session".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::NetCap), // cap
                    (None, Type::Str),    // host
                    (None, Type::Int),    // port
                    // TODO(unknown-elimination): opts is an open record {alpn?: Seq(Str), cert?: ...}.
                    (None, Type::Top), // opts dict — any record or null
                ],
                ret: Box::new(Type::QuicSession),
                variadic: false,
            },
        );
        env.insert(
            "quic-open-stream".to_string(),
            Type::Function {
                params: vec![(None, Type::QuicSession)],
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))), // Returns a bidirectional stream Handle
                variadic: false,
            },
        );
        env.insert(
            "quic-open-datagram".to_string(),
            Type::Function {
                params: vec![(None, Type::QuicSession)],
                ret: Box::new(Type::QuicDatagramHandle),
                variadic: false,
            },
        );
        env.insert(
            "http2-session".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::NetCap), // capability
                    (None, Type::Str),    // base_url (scheme://host:port)
                    // TODO(unknown-elimination): opts is an open record.
                    (None, Type::Top), // opts dict — any record or null
                ],
                ret: Box::new(Type::Http2Session),
                variadic: false,
            },
        );
        env.insert(
            "http3-session".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::QuicSession), // QUIC session
                    // TODO(unknown-elimination): opts is an open record.
                    (None, Type::Top), // opts dict — any record or null
                ],
                ret: Box::new(Type::Http3Session),
                variadic: false,
            },
        );
        env.insert(
            "http-request".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Union(vec![Type::Http2Session, Type::Http3Session]),
                    ), // Http2Session or Http3Session
                    (None, Type::Str), // method
                    (None, Type::Str), // path
                    // TODO(unknown-elimination): headers is {Str: Str} — a Map(Str,Str).
                    (None, Type::Top), // headers dict — any record or null
                    // TODO(unknown-elimination): body is Bytes | Null.
                    (
                        None,
                        Type::normalize_union(vec![
                            Type::Bytes,
                            Type::Record(Row {
                                fields: HashMap::new(),
                            }), // Null
                        ]),
                    ), // body
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("status".to_string(), Type::Int),
                        (
                            "headers".to_string(),
                            Type::Map(Box::new(Type::Str), Box::new(Type::Str)),
                        ),
                        ("body".to_string(), Type::Bytes),
                    ]),
                })),
                variadic: false,
            },
        );
        env.insert(
            "icmp-ping".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::NetCap), // cap
                    (None, Type::Str),    // host
                    (None, Type::Int),    // timeout_ms
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("rtt_ms".to_string(), Type::Int),
                        ("success".to_string(), Type::Bool),
                    ]),
                })),
                variadic: false,
            },
        );
        env.insert(
            "cap-data".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Handle(Box::new(Type::Unknown))),
                    (None, Type::Str),
                ],
                // Genuinely unknown: cap-data returns the value stored in the Handle's
                // capability map, which can be any type (cap name is a dynamic string key).
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // has-cap? is now implemented in stdlib/io.llt as [not [null? [cap-data h cap]]]
        env.insert(
            "write-handle".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Handle(Box::new(Type::Unknown))),
                    (None, Type::normalize_union(vec![Type::Str, Type::Bytes])), // String or Bytes
                ],
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))), // Returns WriteHandle
                variadic: false,
            },
        );
        env.insert(
            "flush".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))], // WriteHandle
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))),        // Returns WriteHandle
                variadic: false,
            },
        );
        env.insert(
            "close".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))], // WriteHandle
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "raw-create".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))), // Returns WriteHandle
                variadic: false,
            },
        );
        env.insert(
            "seek".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Handle(Box::new(Type::Unknown))),
                    (None, Type::Int),
                ], // Handle, byte offset
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))), // Returns Handle for chaining
                variadic: false,
            },
        );
        env.insert(
            "seek-end".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))], // Handle
                ret: Box::new(Type::Handle(Box::new(Type::Unknown))), // Returns Handle for chaining
                variadic: false,
            },
        );
        env.insert(
            "position".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle(Box::new(Type::Unknown)))], // Handle
                ret: Box::new(Type::Int),                                    // Current byte offset
                variadic: false,
            },
        );
        env.insert(
            "list-dir".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Seq(Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("name".to_string(), Type::Str),
                        ("kind".to_string(), Type::Str),
                        ("size".to_string(), Type::Int),
                    ]),
                })))),
                variadic: false,
            },
        );
        env.insert(
            "stat".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("name".to_string(), Type::Str),
                        ("kind".to_string(), Type::Str),
                        ("size".to_string(), Type::Int),
                    ]),
                })),
                variadic: false,
            },
        );
        env.insert(
            "make-dir".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty)
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "builtin-remove".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty)
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "rename".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty)
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "link".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str), (None, Type::Str)],
                // Null -- Type::Record(Row::Empty)
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "read-link".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Str), // Returns target path as String
                variadic: false,
            },
        );
        env.insert(
            "from-json".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                // Genuinely unknown: JSON parse output can be any JSON value (object, array,
                // string, number, bool, null). A precise type requires schema information.
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // include was removed in include-decomp-prelude sprint. No type entry needed.

        // Sequences: primitives (registered as builtin-NAME; prelude exports unwrapped names)
        env.insert(
            "builtin-seq".to_string(),
            Type::Function {
                params: vec![(None, Type::Top), (None, Type::Top)],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );
        // builtin-head: ∀T. Seq(T) → T
        env.insert_scheme(
            "builtin-head".to_string(),
            TypeScheme {
                type_vars: vec!["T".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(None, Type::Seq(Box::new(Type::TypeVar("T".to_string(), 0))))],
                    ret: Box::new(Type::TypeVar("T".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        // builtin-tail: ∀T. Seq(T) → Seq(T)
        env.insert_scheme(
            "builtin-tail".to_string(),
            TypeScheme {
                type_vars: vec!["T".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(None, Type::Seq(Box::new(Type::TypeVar("T".to_string(), 0))))],
                    ret: Box::new(Type::Seq(Box::new(Type::TypeVar("T".to_string(), 0)))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        // builtin-collect: ∀T. Seq(T) → Dict
        // TODO(unknown-elimination): Make Seq param ∀T once collect returns a typed Dict.
        env.insert(
            "builtin-collect".to_string(),
            Type::Function {
                // Genuinely unknown: The Unknown in Seq(Unknown) should ideally be TypeVar("T")
                // but the Dict return type already erases element type information, so the
                // polymorphism buys nothing here.
                params: vec![(None, Type::Seq(Box::new(Type::Unknown)))],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "seq?".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // Sequences: generators (registered as builtin-NAME; prelude exports unwrapped names)
        env.insert(
            "builtin-range".to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Int)],
                ret: Box::new(Type::Seq(Box::new(Type::Int))),
                variadic: false,
            },
        );
        // builtin-repeat: ∀T. T → Seq(T)
        // Each call site gets its own fresh T, so [repeat 42] infers Seq(Int)
        // rather than Seq(Top).
        env.insert_scheme(
            "builtin-repeat".to_string(),
            TypeScheme {
                type_vars: vec!["T".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(None, Type::TypeVar("T".to_string(), 0))],
                    ret: Box::new(Type::Seq(Box::new(Type::TypeVar("T".to_string(), 0)))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        env.insert(
            "builtin-cycle".to_string(),
            Type::Function {
                params: vec![(None, Type::Seq(Box::new(Type::Top)))],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );
        env.insert(
            "builtin-iterate".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Top)],
                            ret: Box::new(Type::Top),
                            variadic: false,
                        },
                    ),
                    (None, Type::Top),
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );
        env.insert(
            "builtin-unfold".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Top)],
                            ret: Box::new(Type::Top),
                            variadic: false,
                        },
                    ),
                    (None, Type::Top),
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );

        // Sequences: transforms
        // map: truly needs HKT (∀f a b. Mappable f ⇒ (a→b)→f a→f b) — requires Type::App/Operator
        // resolution not yet ready. Dual-dispatch at runtime (Dict|Seq), Unknown for now.
        // TODO(unknown-elimination): Replace with Mappable f => (a → b) → f a → f b once
        // instance resolution works.
        env.insert(
            "map".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Top), // callback: any function
                    // Genuinely unknown: collection is Dict or Seq, can't express yet
                    (None, Type::Unknown),
                ],
                // Genuinely unknown: returns same shape as input (HKT needed for precision)
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // filter: ∀a. (a → Bool) → Seq a → Seq a
        // Seq-specific for now. Runtime also accepts Dict, but we can't express Dict|Seq
        // without union subtyping. Dict callers will hit a type error; they should use
        // [filter pred [each dict]] explicitly.
        env.insert_scheme(
            "filter".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![
                        (
                            Some("pred".to_string()),
                            Type::Function {
                                params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                                ret: Box::new(Type::Bool),
                                variadic: false,
                            },
                        ),
                        (
                            Some("xs".to_string()),
                            Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0))),
                        ),
                    ],
                    ret: Box::new(Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0)))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        env.insert(
            "take".to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Seq(Box::new(Type::Top)))],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );
        env.insert(
            "drop".to_string(),
            Type::Function {
                params: vec![(None, Type::Int), (None, Type::Seq(Box::new(Type::Top)))],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );

        // Sequences: reductions
        // reduce: ∀a b. (b → a → b) → b → Seq a → b
        // Seq-specific for now. Runtime also accepts Dict, but we can't express Dict|Seq
        // without union subtyping. Dict callers will hit a type error; they should use
        // [reduce fn init [each dict]] explicitly.
        env.insert_scheme(
            "reduce".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string()],
                constraints: vec![],
                body: Type::Function {
                    params: vec![
                        (
                            Some("fn_".to_string()),
                            Type::Function {
                                params: vec![
                                    (None, Type::TypeVar("b".to_string(), 0)),
                                    (None, Type::TypeVar("a".to_string(), 0)),
                                ],
                                ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                                variadic: false,
                            },
                        ),
                        (Some("init".to_string()), Type::TypeVar("b".to_string(), 0)),
                        (
                            Some("xs".to_string()),
                            Type::Seq(Box::new(Type::TypeVar("a".to_string(), 0))),
                        ),
                    ],
                    ret: Box::new(Type::TypeVar("b".to_string(), 0)),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );
        env.insert(
            "builtin-join".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Str),
                    // Genuinely unknown: join stringifies any element type via stringify().
                    (None, Type::Seq(Box::new(Type::Unknown))),
                ],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        // builtin-concat: Appendable a, Appendable b => a -> b -> Unknown
        // Each argument must be Appendable (Record/Seq), but both args need not be the
        // same concrete type (e.g. two dicts with different field types are both Appendable).
        // Return type is genuinely unknown: output shape depends on runtime values (field merge).
        // TODO(unknown-elimination): Return type could be a TypeVar with an Appendable constraint
        // once the type system supports output-shape inference from structural merges.
        env.insert_scheme(
            "builtin-concat".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string()],
                constraints: vec![
                    Constraint::new_by_name("Appendable", "a"),
                    Constraint::new_by_name("Appendable", "b"),
                ],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::Unknown), // Genuinely unknown: merge shape not inferrable
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // List operations (registered as builtin-NAME; prelude exports unwrapped names)
        // builtin-rest: Dict -> Dict (removes first entry, reindexes)
        env.insert(
            "builtin-rest".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                )],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        // builtin-cons: Top -> Dict -> Dict (prepends element, reindexes)
        env.insert(
            "builtin-cons".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Top), // element to prepend — any value
                    (
                        None,
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ),
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        // builtin-reverse: Dict -> Dict (reverses insertion order, reindexes)
        env.insert(
            "builtin-reverse".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Record(Row {
                        fields: HashMap::new(),
                    }),
                )],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        // builtin-sort: Dict -> Dict (natural ordering)
        //            OR (a -> a -> Bool) -> Dict -> Dict (custom comparator)
        // Variadic to accept both 1-arg and 2-arg call forms without arity errors.
        // First param is Top (accepts either Dict or comparator Fn).
        // TODO(unknown-elimination): Replace with two overloaded TypeSchemes or a union param
        // once the type system supports overloaded/multi-arity signatures cleanly.
        env.insert(
            "builtin-sort".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: true,
            },
        );

        // Proxy: takes a (Str → Top) handler, returns a Proxy value.
        // The handler receives field names as strings and can return any value.
        env.insert(
            "proxy".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Function {
                        params: vec![(None, Type::Str)],
                        ret: Box::new(Type::Top),
                        variadic: false,
                    },
                )],
                ret: Box::new(Type::Proxy),
                variadic: false,
            },
        );

        // Capability and handle types: register as type aliases so @DirCap, @NetCap, @Handle
        // are valid in user annotations.
        env.insert_type_alias(
            "DirCap".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::DirCap,
            },
        );
        env.insert_type_alias(
            "NetCap".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::NetCap,
            },
        );
        env.insert_type_alias(
            "Handle".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::Handle(Box::new(Type::Unknown)),
            },
        );
        env.insert_type_alias(
            "QuicSession".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::QuicSession,
            },
        );
        env.insert_type_alias(
            "Http2Session".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::Http2Session,
            },
        );
        env.insert_type_alias(
            "Http3Session".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::Http3Session,
            },
        );
        env.insert_type_alias(
            "QuicDatagramHandle".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::QuicDatagramHandle,
            },
        );
        env.insert_type_alias(
            "DatagramHandle".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::DatagramHandle,
            },
        );

        // Capability flags: singleton unit types for DirCap permission markers.
        // Each flag is a unique record type with a distinctive marker field.
        // These are used in intersection types to express fine-grained capabilities:
        // e.g., Intersection([DirCap, Readable, Writable]) for a read-write DirCap.
        for flag_name in [
            "Readable",
            "Writable",
            "Listable",
            "Statable",
            "Appendable",
            "Deletable",
            "Renameable",
        ] {
            let mut fields = HashMap::new();
            // Use a distinctive marker field name that won't conflict with user data.
            // The field name encodes the flag identity to ensure structural uniqueness.
            fields.insert(
                format!("__cap_flag_{}", flag_name.to_lowercase()),
                Type::Record(Row {
                    fields: HashMap::new(),
                }),
            );
            env.insert_type_alias(
                flag_name.to_string(),
                TypeAlias {
                    params: vec![],
                    body: Type::Record(Row { fields }),
                },
            );
        }

        // builtin-get: registered directly. 'get' is a prelude wrapper (not a Rust builtin
        // type), so it is absent from this env when the alias loop below runs. Registering
        // builtin-get here gives the type checker enough information to avoid false
        // "undefined variable" errors in stdlib/prelude.llt.
        //
        // NOTE: A Label-polymorphic scheme ∀(l:Label) d a. HasField l d a ⇒ l → d → a would be
        // more precise, but causes O(N²) blowup in prelude type-checking: the prelude's private dict
        // has ~35 direct `builtin-get` calls (for integer and string key access), each generating 3
        // fresh TypeVars + 1 HasField constraint. With ~100 TypeVar entries added to state.subst and
        // the O(N²) merge loop in typecheck_dict.rs, the prelude type-check hangs.
        //
        // Precision is preserved for `get` and `get?` via the `check_get` special-form dispatcher
        // (typecheck.rs:1476-1488), which intercepts `name == "get"` or `"get?"` calls and handles
        // label TypeVars directly via `resolve_has_field`. The `check_get` dispatcher is also applied
        // to `builtin-get` calls (typecheck.rs) to handle label TypeVar keys in prelude wrappers.
        env.insert_scheme(
            "builtin-get".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    // Genuinely unknown: conservative fallback signature (any dict-like, any key, any result).
                    // The type checker special-cases get calls for precise Map/Record typing.
                    params: vec![(None, Type::Unknown), (None, Type::Unknown)],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // get?: registered directly. It is a Rust builtin (builtin_get_optional) that returns
        // the value at the key or Null (empty dict) if missing. The type checker special-cases get?
        // for Map and Record args to produce precise Union(V|Null) return types.
        env.insert_scheme(
            "get?".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    // Genuinely unknown: conservative fallback signature Unknown → Unknown → Union(Unknown, Null).
                    // The type checker special-cases get? calls for precise Map/Record typing.
                    params: vec![(None, Type::Unknown), (None, Type::Unknown)],
                    ret: Box::new(Type::normalize_union(vec![
                        Type::Unknown,
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }),
                    ])),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Date-time: timestamps and durations
        env.insert(
            "parse-timestamp".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Timestamp),
                variadic: false,
            },
        );
        env.insert(
            "format-timestamp".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "timestamp->unix".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
        env.insert(
            "unix->timestamp".to_string(),
            Type::Function {
                params: vec![(None, Type::Int)],
                ret: Box::new(Type::Timestamp),
                variadic: false,
            },
        );
        env.insert(
            "now".to_string(),
            Type::Function {
                params: vec![(None, Type::ClockCap)],
                ret: Box::new(Type::Timestamp),
                variadic: false,
            },
        );
        env.insert(
            "fixed-clock".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp)],
                ret: Box::new(Type::ClockCap),
                variadic: false,
            },
        );
        env.insert(
            "timestamp-add".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp), (None, Type::Duration)],
                ret: Box::new(Type::Timestamp),
                variadic: false,
            },
        );
        env.insert(
            "timestamp-diff".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp), (None, Type::Timestamp)],
                ret: Box::new(Type::Duration),
                variadic: false,
            },
        );
        for name in ["timestamp<?", "timestamp>?", "timestamp=?"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Timestamp), (None, Type::Timestamp)],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            );
        }
        for name in [
            "timestamp-year",
            "timestamp-month",
            "timestamp-day",
            "timestamp-hour",
            "timestamp-minute",
            "timestamp-second",
        ] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Timestamp)],
                    ret: Box::new(Type::Int),
                    variadic: false,
                },
            );
        }
        env.insert(
            "timestamp-parts".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("year".to_string(), Type::Int),
                        ("month".to_string(), Type::Int),
                        ("day".to_string(), Type::Int),
                        ("hour".to_string(), Type::Int),
                        ("minute".to_string(), Type::Int),
                        ("second".to_string(), Type::Int),
                    ]),
                })),
                variadic: false,
            },
        );
        for name in [
            "duration-nanos",
            "duration-seconds",
            "duration-minutes",
            "duration-hours",
            "duration-days",
        ] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Int)],
                    ret: Box::new(Type::Duration),
                    variadic: false,
                },
            );
        }
        for name in ["duration->seconds", "duration->nanos"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Duration)],
                    ret: Box::new(Type::Int),
                    variadic: false,
                },
            );
        }
        env.insert(
            "load-tz".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Timezone),
                variadic: false,
            },
        );
        env.insert(
            "timestamp-in-tz".to_string(),
            Type::Function {
                params: vec![(None, Type::Timestamp), (None, Type::Timezone)],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::from([
                        ("year".to_string(), Type::Int),
                        ("month".to_string(), Type::Int),
                        ("day".to_string(), Type::Int),
                        ("hour".to_string(), Type::Int),
                        ("minute".to_string(), Type::Int),
                        ("second".to_string(), Type::Int),
                        ("offset-seconds".to_string(), Type::Int),
                        ("tz-name".to_string(), Type::Str),
                    ]),
                })),
                variadic: false,
            },
        );
        env.insert(
            "local->timestamp".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Int),
                    (None, Type::Int),
                    (None, Type::Int),
                    (None, Type::Int),
                    (None, Type::Int),
                    (None, Type::Int),
                    (None, Type::Timezone),
                ],
                ret: Box::new(Type::Timestamp),
                variadic: false,
            },
        );
        env.insert(
            "local-tz-name".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );

        // builtin-first: Dict|String|Bytes -> Any (returns first element, char, or byte-as-Int)
        // TODO(unknown-elimination): Input should be Dict|Str|Bytes union; return type depends on
        // input (element type for Dict, Str for String, Int for Bytes). Requires union input types
        // and type-indexed return — defer to unknown-elimination sprint.
        env.insert(
            "builtin-first".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)], // Top: accepts Dict, Str, or Bytes
                ret: Box::new(Type::Unknown),    // Genuinely unknown: depends on input type
                variadic: false,
            },
        );
        // builtin-last: Dict|String|Bytes -> Any (returns last element, char, or byte-as-Int)
        // TODO(unknown-elimination): same as builtin-first — see above.
        env.insert(
            "builtin-last".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)], // Top: accepts Dict, Str, or Bytes
                ret: Box::new(Type::Unknown),    // Genuinely unknown: depends on input type
                variadic: false,
            },
        );

        // HashAlgorithm: type alias for the supported hash algorithm identifiers.
        // Represented as a union of string literals (variant tags are strings at the type level
        // until Type::Variant is added in a future type-extension sprint).
        // Used as the algorithm argument to hash and SPKI pin functions.
        env.insert_type_alias(
            "HashAlgorithm".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::normalize_union(vec![
                    Type::StringLiteral("Sha256".to_string()),
                    Type::StringLiteral("Sha384".to_string()),
                    Type::StringLiteral("Sha512".to_string()),
                    Type::StringLiteral("Sha3-256".to_string()),
                    Type::StringLiteral("Sha3-384".to_string()),
                    Type::StringLiteral("Sha3-512".to_string()),
                    Type::StringLiteral("Blake3".to_string()),
                ]),
            },
        );

        // Transport type and variant constants (Tcp, Udp, UnixStream, UnixDatagram, NamedPipe, Icmp)
        // are now registered by the prelude's [type [Tcp] [Udp] ...] declaration.
        // No manual type alias needed — the [type ...] declaration creates the Transport
        // type and registers each constructor as a Value::Variant during prelude evaluation.

        // Url: type alias for the record type returned by the `url` builtin.
        // Allows @Url annotations in user code without "undefined type" errors.
        env.insert_type_alias(
            "Url".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::Record(Row {
                    fields: HashMap::new(),
                }),
            },
        );

        // URI parsing builtins: uri, url, urn
        // These return Dict types — the type system doesn't yet support precise row types
        // for the returned dicts, so we use a generic Dict type.
        for name in ["uri", "url", "urn"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Record(Row {
                        fields: HashMap::new(),
                    })),
                    variadic: false,
                },
            );
        }

        // Iteration builtins: each, each-key, each-kv
        // Runtime: 1-arg functions that convert Dict → Seq
        // each: Dict a → Seq a (values)
        // each-key: Dict → Seq (Int | Str) (keys)
        // each-kv: Dict a → Seq [key: Int | Str, value: a] (key-value pairs)

        // each: Record → Seq Top
        // Dict values are heterogeneous, so the honest return type is Seq Top.
        // Previously ∀a. Record → Seq a with phantom `a` (no occurrence in params),
        // which allowed any element type to be inferred without checking.
        env.insert_scheme(
            "each".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(
                        Some("xs".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }), // Dict (open record)
                    )],
                    ret: Box::new(Type::Seq(Box::new(Type::Top))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // each-key: Record → Seq (Int | Str)
        env.insert_scheme(
            "each-key".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(
                        Some("xs".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }), // Dict (open record)
                    )],
                    ret: Box::new(Type::Seq(Box::new(Type::normalize_union(vec![
                        Type::Int,
                        Type::Str,
                    ])))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // each-kv: Record → Seq [key: Int | Str, value: Top]
        // Dict values are heterogeneous, so the value field is Top.
        // Previously ∀a. Record → Seq [key: Int | Str, value: a] with phantom `a`.
        env.insert_scheme(
            "each-kv".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(
                        Some("xs".to_string()),
                        Type::Record(Row {
                            fields: HashMap::new(),
                        }), // Dict (open record)
                    )],
                    ret: Box::new(Type::Seq(Box::new({
                        let mut kv_fields = HashMap::new();
                        kv_fields.insert(
                            "key".to_string(),
                            Type::normalize_union(vec![Type::Int, Type::Str]),
                        );
                        kv_fields.insert("value".to_string(), Type::Top);
                        Type::Record(Row { fields: kv_fields })
                    }))),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
                inner_schemes: None,
            },
        );

        // Type constructors
        // Map with Unknown K/V is the unparameterized Map type — used when the user writes
        // `Map` without type arguments. Parameterized Map[K V] is handled by the type alias below.
        // Genuinely unknown until the user supplies type arguments via @[Map Str Int].
        env.insert(
            "Map".to_string(),
            Type::Map(Box::new(Type::Unknown), Box::new(Type::Unknown)),
        );
        // Map[K V] as a parameterized type alias: allows @[Map Str Int] annotations.
        // The alias params "k" and "v" are substituted by instantiate_type_alias when applied.
        env.insert_type_alias(
            "Map".to_string(),
            TypeAlias {
                params: vec!["k".to_string(), "v".to_string()],
                body: Type::Map(
                    Box::new(Type::TypeVar("k".to_string(), 0)),
                    Box::new(Type::TypeVar("v".to_string(), 0)),
                ),
            },
        );

        env
    }

    /// Inject `builtin-*` aliases into this type environment.
    ///
    /// These aliases map `builtin-lt` → `<`, `builtin-add` → `+`, etc.
    /// They are used by `stdlib/prelude.llt` to call Rust primitives by stable
    /// names that cannot be shadowed by user code.
    ///
    /// **Only call this when type-checking prelude itself.** User code does NOT
    /// have `builtin-*` names in scope — they are private to the prelude evaluation
    /// layer. Adding them here for user type-checking would allow the type checker
    /// to accept code that the evaluator would reject with "undefined variable".
    pub fn inject_builtin_aliases(&mut self) {
        for (alias, canonical) in [
            ("builtin-lt", "<"),
            ("builtin-eq", "="),
            ("builtin-add", "+"),
            ("builtin-sub", "-"),
            ("builtin-mul", "*"),
            ("builtin-div", "/"),
            ("builtin-if", "if"),
            ("builtin-filter", "filter"),
            ("builtin-map", "map"),
            ("builtin-reduce", "reduce"),
            ("builtin-take", "take"),
            ("builtin-drop", "drop"),
            ("builtin-eval-ast", "eval-ast"),
            ("builtin-gensym", "gensym"),
            ("builtin-llt-repr", "llt-repr"),
            ("builtin-tag-of", "tag-of"),
            ("builtin-variant", "variant"),
            ("builtin-decimal", "decimal"),
            ("builtin-big-int", "big-int"),
            ("builtin-proxy", "proxy"),
        ] {
            if let Some(scheme) = self.get(canonical).cloned() {
                self.insert_scheme(alias.to_string(), scheme);
            }
        }
    }
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeError {
    pub message: String,
    pub span: Span,
    /// Extra `= note:` lines attached at the error-generation site (e.g. "caused by" context).
    pub notes: Vec<String>,
    /// Explicit stable error code, e.g. `"T014"`. When `Some`, overrides the message-pattern
    /// dispatch in `code()`. Use `with_code()` to attach a code at the construction site.
    pub code: Option<String>,
}

impl TypeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
            notes: Vec::new(),
            code: None,
        }
    }

    /// Builder method: attach an explicit error code and return `self`.
    ///
    /// The explicit code takes priority over the message-pattern dispatch in `code()`.
    pub fn with_code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }

    pub fn type_mismatch(expected: &Type, got: &Type, span: Span) -> Self {
        Self::new(format!("cannot unify {expected} with {got}"), span)
    }

    pub fn field_not_found(field: &str, record_type: &Type, span: Span) -> Self {
        Self::new(format!("field '{field}' not found in {record_type}"), span)
    }

    pub fn not_a_record(ty: &Type, span: Span) -> Self {
        Self::new(format!("expected record type, got {ty}"), span)
    }

    pub fn not_a_function(ty: &Type, span: Span) -> Self {
        Self::new(format!("expected function type, got {ty}"), span)
    }

    pub fn undefined_variable(name: &str, span: Span) -> Self {
        // Emit name as-is -- `%`-prefixed refs include `%`; plain identifiers display without sigil.
        Self::new(format!("undefined variable: {name}"), span)
    }

    pub fn undefined_type(name: &str, span: Span) -> Self {
        Self::new(format!("undefined type: {name}"), span)
    }

    pub fn kind_mismatch(expected_kind: &str, got: &str, span: Span) -> Self {
        Self::new(
            format!("kind mismatch: expected `{expected_kind}`, got {got}"),
            span,
        )
    }

    /// Returns the stable type error code for this error.
    ///
    /// If an explicit code was attached via `with_code()`, it is returned directly.
    /// Otherwise the code is derived from the error message:
    ///
    /// - T001: arity mismatch (wrong number of arguments at call site)
    /// - T002: undefined variable or undefined type
    /// - T003: cannot unify / type mismatch / field not found / not a function / not a record
    /// - T004: type assert failure (annotation-site mismatch)
    /// - T014: overlapping CHR instance patterns (disjointness violation)
    /// - T015: CHR instance consistency violation (FD disagreement between arms)
    /// - T016: CHR instance coverage violation (determined var absent from determining positions)
    /// - T091: kind mismatch (expected `* → *`, got concrete type, etc.)
    /// - T000: other type errors not covered above
    pub fn code(&self) -> &str {
        if let Some(ref explicit) = self.code {
            return explicit.as_str();
        }
        let msg = &self.message;
        if msg.starts_with("arity mismatch") {
            "T001"
        } else if msg.starts_with("undefined variable") || msg.starts_with("undefined type") {
            "T002"
        } else if msg.starts_with("cannot unify")
            || msg.starts_with("field '")
            || msg.starts_with("expected record type")
            || msg.starts_with("expected function type")
            || msg.starts_with("type mismatch")
        {
            "T003"
        } else if msg.contains("type assert") || msg.starts_with("non-exhaustive match") {
            "T004"
        } else if msg.starts_with("overlapping instance patterns") {
            "T014"
        } else if msg.starts_with("consistency violation") {
            "T015"
        } else if msg.starts_with("coverage violation") {
            "T016"
        } else if msg.starts_with("kind mismatch") {
            "T091"
        } else {
            "T000"
        }
    }
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.message, self.span)
    }
}

impl std::error::Error for TypeError {}

/// Format a `TypeError` into the Rust-style diagnostic format with source context.
///
/// Produces output like:
/// ```text
/// error[T003]: cannot unify Int with String
///  --> 1:5
///   |
///  1 | [call $+ 1 "hello"]
///    |          ^
/// ```
///
/// When `source` is empty or the span is synthetic (`Span::origin()`),
/// only the header line is emitted (no snippet).
///
/// `file_name` is shown in the ` --> file:line:col` line. Pass `"-"` for stdin input.
pub fn format_type_error(err: &TypeError, source: &str, file_name: &str) -> String {
    use crate::error::render_span_snippet;

    let code = err.code();
    let line = err.span.start.line;
    let col = err.span.start.column;

    // Header: error[Txxx]: message
    let mut out = format!("error[{code}]: {}\n", err.message);

    // Location: --> file:line:col
    out.push_str(&format!(" --> {file_name}:{line}:{col}\n"));

    // Snippet: source context with caret
    if let Some(snippet) = render_span_snippet(source, err.span) {
        out.push_str("  |\n");
        out.push_str(&snippet);
    }

    // Contextual notes (pattern-matched); suppressed when attached notes already explain the cause
    let note = type_error_note(err);
    if let Some(n) = note {
        out.push('\n');
        out.push_str(&n);
    }

    // Attached notes added at error-generation time (e.g. "caused by" for cascade T002s)
    for note in &err.notes {
        out.push('\n');
        out.push_str(note);
    }

    out
}

/// Generate contextual `= note:` and `= help:` lines for well-known type error patterns.
///
/// Returns a formatted string with note and/or help lines, each prefixed with `  = `.
fn type_error_note(err: &TypeError) -> Option<String> {
    let msg = &err.message;

    if msg.starts_with("arity mismatch") {
        Some("  = note: check that you are passing the correct number of arguments".to_string())
    } else if msg.starts_with("undefined variable") {
        // When a caused-by note is attached (cascade from a failed definition), suppress the
        // generic "not defined in any enclosing scope" note — it would be misleading.
        if !err.notes.is_empty() {
            return None;
        }

        // Extract the variable name from "undefined variable: <name>"
        let name = msg
            .strip_prefix("undefined variable: ")
            .unwrap_or("")
            .trim();
        let mut lines = Vec::new();

        if name.is_empty() {
            lines.push("  = note: variable is not defined in any enclosing scope".to_string());
        } else {
            lines.push(format!(
                "  = note: `{name}` is not defined in any enclosing scope at this point"
            ));
            lines.push("  = help: if this name is defined later in the document, group definitions using a function scope: [call [fn [let] ...]]".to_string());
        }

        Some(lines.join("\n"))
    } else if msg.starts_with("cannot unify") {
        // Extract types from "cannot unify A with B"
        let rest = msg.strip_prefix("cannot unify ").unwrap_or("");
        if let Some(idx) = rest.find(" with ") {
            let expected = &rest[..idx];
            let got = &rest[idx + 6..];
            let mut lines = Vec::new();

            lines.push(format!(
                "  = note: expected `{expected}`\n           found `{got}`"
            ));

            // Add conversion hints for common type mismatches
            // "cannot unify A with B" means expected A, got B
            // So we suggest converting B (got) to A (expected)
            let help_msg = match (expected, got) {
                // Expected Int/Number, got String → convert String to Int/Float
                (e, "String") if e.contains("Int") || e.contains("Number") => {
                    Some("  = help: convert with [int <expr>] or [float <expr>]")
                }
                // Expected String, got Int/Number → convert Int/Number to String
                ("String", g) if g.contains("Int") || g.contains("Number") => {
                    Some("  = help: convert with [str <expr>]")
                }
                // Expected String, got Float
                ("String", g) if g.contains("Float") => Some("  = help: convert with [str <expr>]"),
                // Expected Float, got String
                (e, "String") if e.contains("Float") => {
                    Some("  = help: convert with [float <expr>]")
                }
                // Expected String, got Bool
                ("String", "Bool") => Some("  = help: convert with [if <expr> \"true\" \"false\"]"),
                // Expected Bool, got String
                ("Bool", "String") => Some("  = help: convert with [not [call $= \"\" <expr>]]"),
                // Expected Int/Number, got Bool → convert Bool to Int
                (e, "Bool") if e.contains("Int") || e.contains("Number") => {
                    Some("  = help: convert with [if <expr> 1 0]")
                }
                // Expected Float, got Bool
                (e, "Bool") if e.contains("Float") => {
                    Some("  = help: convert with [if <expr> 1.0 0.0]")
                }
                // Expected Bool, got Int/Number → convert Int/Number to Bool
                ("Bool", g) if g.contains("Int") || g.contains("Number") => {
                    Some("  = help: convert with [not [call $= 0 <expr>]]")
                }
                // Expected Bool, got Float
                ("Bool", g) if g.contains("Float") => {
                    Some("  = help: convert with [not [call $= 0.0 <expr>]]")
                }
                _ => None,
            };

            if let Some(help) = help_msg {
                lines.push(help.to_string());
            }

            Some(lines.join("\n"))
        } else {
            None
        }
    } else {
        None
    }
}

#[cfg(test)]
mod help_suggestion_tests {
    use super::*;
    use crate::test_util::test_span;

    #[test]
    fn test_arity_mismatch_generic_help() {
        let err = TypeError::new(
            "arity mismatch: expected 2 argument(s), got 1 (1 positional, 0 named)",
            test_span(1, 1, 1, 10),
        );
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: check that you are passing the correct number of arguments"));
    }

    #[test]
    fn test_arity_mismatch_help() {
        let err = TypeError::new(
            "arity mismatch: expected 1 argument(s), got 0 (0 positional, 0 named)",
            test_span(1, 1, 1, 10),
        );
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: check that you are passing the correct number of arguments"));
    }

    #[test]
    fn test_undefined_variable_help() {
        let err = TypeError::new("undefined variable: myvar", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(
            note.contains("= note: `myvar` is not defined in any enclosing scope at this point")
        );
        assert!(note.contains("= help: if this name is defined later in the document, group definitions using a function scope"));
    }

    #[test]
    fn test_type_mismatch_string_to_int_help() {
        // "cannot unify Int with String" means expected Int, got String
        // Should suggest converting String to Int
        let err = TypeError::new("cannot unify Int with String", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `Int`"));
        assert!(note.contains("found `String`"));
        assert!(note.contains("= help: convert with [int <expr>] or [float <expr>]"));
    }

    #[test]
    fn test_type_mismatch_int_to_string_help() {
        // "cannot unify String with Int" means expected String, got Int
        // Should suggest converting Int to String
        let err = TypeError::new("cannot unify String with Int", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `String`"));
        assert!(note.contains("found `Int`"));
        assert!(note.contains("= help: convert with [str <expr>]"));
    }

    #[test]
    fn test_type_mismatch_number_to_string_help() {
        // "cannot unify String with Number" means expected String, got Number
        // Should suggest converting Number to String
        let err = TypeError::new("cannot unify String with Number", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= help: convert with [str <expr>]"));
    }

    #[test]
    fn test_type_mismatch_float_to_string_help() {
        // "cannot unify String with Float" means expected String, got Float
        let err = TypeError::new("cannot unify String with Float", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `String`"));
        assert!(note.contains("found `Float`"));
        assert!(note.contains("= help: convert with [str <expr>]"));
    }

    #[test]
    fn test_type_mismatch_string_to_float_help() {
        // "cannot unify Float with String" means expected Float, got String
        let err = TypeError::new("cannot unify Float with String", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `Float`"));
        assert!(note.contains("found `String`"));
        assert!(note.contains("= help: convert with [float <expr>]"));
    }

    #[test]
    fn test_type_mismatch_bool_to_string_help() {
        // "cannot unify String with Bool" means expected String, got Bool
        let err = TypeError::new("cannot unify String with Bool", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `String`"));
        assert!(note.contains("found `Bool`"));
        assert!(note.contains("= help: convert with [if <expr> \"true\" \"false\"]"));
    }

    #[test]
    fn test_type_mismatch_string_to_bool_help() {
        // "cannot unify Bool with String" means expected Bool, got String
        let err = TypeError::new("cannot unify Bool with String", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `Bool`"));
        assert!(note.contains("found `String`"));
        assert!(note.contains("= help: convert with [not [call $= \"\" <expr>]]"));
    }

    #[test]
    fn test_type_mismatch_bool_to_float_help() {
        // "cannot unify Float with Bool" means expected Float, got Bool
        let err = TypeError::new("cannot unify Float with Bool", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `Float`"));
        assert!(note.contains("found `Bool`"));
        assert!(note.contains("= help: convert with [if <expr> 1.0 0.0]"));
    }

    #[test]
    fn test_type_mismatch_float_to_bool_help() {
        // "cannot unify Bool with Float" means expected Bool, got Float
        let err = TypeError::new("cannot unify Bool with Float", test_span(1, 1, 1, 10));
        let note = type_error_note(&err);
        assert!(note.is_some());
        let note = note.unwrap();
        assert!(note.contains("= note: expected `Bool`"));
        assert!(note.contains("found `Float`"));
        assert!(note.contains("= help: convert with [not [call $= 0.0 <expr>]]"));
    }

    #[test]
    fn test_resolve_instance_freshens_type_vars() {
        use crate::types::{InferState, InstanceDecl};
        use std::collections::HashMap;

        // Test that resolve_instance freshens type variables in the instance type
        // and applies the unification substitution to method types.

        let mut state = InferState::new();

        // Create an instance: Appendable [Seq b]
        // Method: append: [Fn@[Seq b] [[Seq b] [Seq b]]]
        let instance = InstanceDecl {
            class_name: "Appendable".to_string(),
            instance_type: Type::Seq(Box::new(Type::TypeVar("b".to_string(), 0))),
            det_positions: vec![], // Single-parameter class, no FDs
            method_types: {
                let mut methods = HashMap::new();
                methods.insert(
                    "append".to_string(),
                    Type::Function {
                        params: vec![
                            (None, Type::Seq(Box::new(Type::TypeVar("b".to_string(), 0)))),
                            (None, Type::Seq(Box::new(Type::TypeVar("b".to_string(), 0)))),
                        ],
                        ret: Box::new(Type::Seq(Box::new(Type::TypeVar("b".to_string(), 0)))),
                        variadic: false,
                    },
                );
                methods
            },
        };

        // Register the instance
        state.instance_env.insert(instance.clone()).unwrap();

        // Resolve against Seq[Int]
        let target = Type::Seq(Box::new(Type::Int));
        // Clone to avoid borrowing state both mutably and immutably
        let inst_env = state.instance_env.clone();
        let resolved = inst_env.resolve_instance("Appendable", &target, &mut state);

        assert!(resolved.is_some(), "should resolve Appendable for Seq[Int]");
        let resolved = resolved.unwrap();

        // The method types should have Int substituted for b
        let append_ty = resolved.method_types.get("append");
        assert!(append_ty.is_some(), "append method should exist");

        // Check that the method signature has Seq[Int], not Seq[b]
        if let Type::Function { params, ret, .. } = append_ty.unwrap() {
            assert_eq!(params.len(), 2);
            // Both params should be Seq[Int] or Seq[_tN] (freshened)
            match &params[0].1 {
                Type::Seq(elem) => {
                    // Should be Int or a fresh type var that got unified with Int
                    assert!(
                        matches!(elem.as_ref(), Type::Int | Type::TypeVar(..)),
                        "first param should be Seq[Int] or Seq[fresh], got {:?}",
                        elem
                    );
                }
                other => panic!("expected Seq type for first param, got {:?}", other),
            }

            match ret.as_ref() {
                Type::Seq(elem) => {
                    assert!(
                        matches!(elem.as_ref(), Type::Int | Type::TypeVar(..)),
                        "return should be Seq[Int] or Seq[fresh], got {:?}",
                        elem
                    );
                }
                other => panic!("expected Seq type for return, got {:?}", other),
            }
        } else {
            panic!("append should have Function type, got {:?}", append_ty);
        }
    }

    #[test]
    fn test_instantiate_at_level_preserves_operator_kind() {
        use crate::types::{InferState, Kind, Type};

        // Create a type containing an Operator variable: App(Operator("m"), Int)
        let original_ty = Type::App(
            Box::new(Type::Operator("m".to_string())),
            Box::new(Type::Int),
        );

        let mut state = InferState::new();
        state.kind_env.insert("m".to_string(), Kind::Operator);

        // Instantiate at level 1
        state.level = 1;
        let instantiated = instantiate_at_level(&original_ty, &mut state);

        // The result should be App(Operator(fresh_name), Int), not App(TypeVar(fresh_name), Int)
        match instantiated {
            Type::App(f, a) => {
                // f should be Operator, not TypeVar
                match f.as_ref() {
                    Type::Operator(fresh_name) => {
                        // Check that the fresh name was registered in kind_env with Kind::Operator
                        assert_eq!(
                            state.kind_env.get(fresh_name.as_str()),
                            Some(&Kind::Operator)
                        );
                    }
                    other => panic!("Expected Operator after instantiation, got {:?}", other),
                }
                // a should still be Int
                assert_eq!(a.as_ref(), &Type::Int);
            }
            other => panic!("Expected App type, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_single_type_var_handles_operator() {
        use crate::types::Type;

        // Test renaming an Operator variable
        let original_ty = Type::App(
            Box::new(Type::Operator("m".to_string())),
            Box::new(Type::Int),
        );

        let renamed = rename_single_type_var(&original_ty, "m", "fresh_m", 1);

        match renamed {
            Type::App(f, a) => {
                match f.as_ref() {
                    Type::Operator(name) => {
                        assert_eq!(name, "fresh_m");
                    }
                    other => panic!("Expected Operator(fresh_m), got {:?}", other),
                }
                assert_eq!(a.as_ref(), &Type::Int);
            }
            other => panic!("Expected App type, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_single_type_var_handles_app() {
        use crate::types::Type;

        // Test that App recurses into both children
        let original_ty = Type::App(
            Box::new(Type::TypeVar("a".to_string(), 0)),
            Box::new(Type::TypeVar("a".to_string(), 0)),
        );

        let renamed = rename_single_type_var(&original_ty, "a", "b", 1);

        match renamed {
            Type::App(f, arg) => {
                match f.as_ref() {
                    Type::TypeVar(name, level) => {
                        assert_eq!(name, "b");
                        assert_eq!(*level, 1);
                    }
                    other => panic!("Expected TypeVar(b, 1), got {:?}", other),
                }
                match arg.as_ref() {
                    Type::TypeVar(name, level) => {
                        assert_eq!(name, "b");
                        assert_eq!(*level, 1);
                    }
                    other => panic!("Expected TypeVar(b, 1), got {:?}", other),
                }
            }
            other => panic!("Expected App type, got {:?}", other),
        }
    }

    #[test]
    fn test_rename_single_type_var_handles_negation() {
        use crate::types::Type;

        // Test that Negation recurses into inner type
        let original_ty = Type::Negation(Box::new(Type::TypeVar("a".to_string(), 0)));

        let renamed = rename_single_type_var(&original_ty, "a", "b", 1);

        match renamed {
            Type::Negation(inner) => match inner.as_ref() {
                Type::TypeVar(name, level) => {
                    assert_eq!(name, "b");
                    assert_eq!(*level, 1);
                }
                other => panic!("Expected TypeVar(b, 1), got {:?}", other),
            },
            other => panic!("Expected Negation type, got {:?}", other),
        }
    }
}
