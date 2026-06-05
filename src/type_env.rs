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
        parent: None,
        creation_level: 0,
        name_counter: std::cell::Cell::new(0),
    };
    for var in type_vars {
        // First-write-wins: skip if this var was already mapped (handles duplicates from the Vec).
        if !renaming.type_map.borrow().contains_key(&var) {
            let n = state.subst.name_counter.get();
            let fresh_name = format!("_t{}", n);
            state.subst.name_counter.set(n.saturating_add(1));
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
        Type::App(f, a) => Type::App(
            Box::new(rename_single_type_var(f, old_name, fresh_name, level)),
            Box::new(rename_single_type_var(a, old_name, fresh_name, level)),
        ),
        Type::TyCon(_) => ty.clone(), // TyCon has no type variables to rename
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
    // Preserve the tail: rename type vars in Uniform key/value types too
    let new_tail = match &row.tail {
        crate::type_def::RowTail::Empty => crate::type_def::RowTail::Empty,
        crate::type_def::RowTail::Uniform { key, value } => crate::type_def::RowTail::Uniform {
            key: key
                .as_ref()
                .map(|k| Box::new(rename_single_type_var(k, old_name, fresh_name, level))),
            value: Box::new(rename_single_type_var(value, old_name, fresh_name, level)),
        },
    };
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
        tail: new_tail,
    }
}

/// Instantiate a type scheme by creating fresh type variables at the given level.
/// Used for VAR-POLY: when a polymorphic binding is referenced, create fresh instances.
///
/// Variables in `scheme.type_vars` are instantiated as `Type::TypeVar(fresh, level)`.
/// Variables in `scheme.kind_vars` with `Kind::Operator` are instantiated as
/// `Type::Operator(fresh)` and registered in `state.kind_env` with `Kind::Operator`,
/// enabling them to unify with type constructor applications via UNIFY-OPERATOR.
/// Variables in `scheme.kind_vars` with `Kind::Label` are treated identically to
/// label_vars (registered in `state.kind_env` with `Kind::Label`).
pub fn instantiate_scheme(
    scheme: &TypeScheme,
    level: u32,
    state: &mut InferState,
    origin_name: Option<&str>,
    origin_span: Option<Span>,
) -> Type {
    if scheme.type_vars.is_empty() && scheme.kind_vars.is_empty() {
        // Monomorphic scheme: return body directly
        return scheme.body.clone();
    }

    // Build variable renaming map (old names -> fresh names)
    let mut var_renaming: HashMap<String, String> = HashMap::new();

    // Fast path: single regular type variable with no kind_vars --
    // avoid building Substitution (HashMap + apply HashSet).
    // Inline rename is allocation-free aside from the string format for the fresh name.
    if scheme.type_vars.len() == 1 && scheme.kind_vars.is_empty() {
        let n = state.subst.name_counter.get();
        let fresh_name = format!("_t{}", n);
        state.subst.name_counter.set(n.saturating_add(1));
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(scheme.type_vars[0].clone(), fresh_name.clone());

        // Copy constraints with renamed variables
        for constraint in &scheme.constraints {
            match constraint {
                Constraint::Class {
                    class,
                    vars,
                    origin_name: constraint_origin_name,
                    origin_span: constraint_origin_span,
                } => {
                    // Rename all vars in the constraint
                    let fresh_vars: Vec<String> = vars
                        .iter()
                        .filter_map(|v| var_renaming.get(v).cloned())
                        .collect();
                    if fresh_vars.len() == vars.len() {
                        // Use constraint's origin info if present, otherwise use call-site origin
                        let final_origin_name = constraint_origin_name
                            .clone()
                            .or_else(|| origin_name.map(Arc::from));
                        let final_origin_span =
                            constraint_origin_span.clone().or(origin_span.clone());

                        state.constraints.push(Constraint::Class {
                            class: Arc::clone(class),
                            vars: fresh_vars,
                            origin_name: final_origin_name,
                            origin_span: final_origin_span,
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

    // General path: multiple type variables and/or kind_vars -- build a full Substitution.
    // Total capacity is type_vars + kind_vars (each kind_var also gets a renaming entry).
    let total_vars = scheme.type_vars.len() + scheme.kind_vars.len();
    let renaming = Substitution {
        type_map: std::cell::RefCell::new(HashMap::with_capacity(total_vars)),
        parent: None,
        creation_level: 0,
        name_counter: std::cell::Cell::new(0),
    };

    // Instantiate regular type variables as Type::TypeVar.
    for var in &scheme.type_vars {
        let n = state.subst.name_counter.get();
        let fresh_name = format!("_t{}", n);
        state.subst.name_counter.set(n.saturating_add(1));
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

    // Instantiate kinded variables according to their kind.
    // Kind::Operator → Type::Operator(fresh_name), registered in kind_env.
    // Kind::Label    → Type::TypeVar(fresh_name, level), registered in kind_env as Label.
    // Kind::Type     → Type::TypeVar(fresh_name, level) (same as a regular type_var).
    for (var, kind) in &scheme.kind_vars {
        let n = state.subst.name_counter.get();
        let fresh_name = format!("_t{}", n);
        state.subst.name_counter.set(n.saturating_add(1));
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(var.clone(), fresh_name.clone());

        let instantiated_type = match kind {
            Kind::Operator => {
                // Register in kind_env so that resolve_type_expr and UNIFY-OPERATOR
                // recognise the fresh variable as a type constructor, not a type.
                state.kind_env.insert(fresh_name.clone(), Kind::Operator);
                Type::Operator(fresh_name.clone())
            }
            Kind::Label => {
                state.kind_env.insert(fresh_name.clone(), Kind::Label);
                Type::TypeVar(fresh_name.clone(), level)
            }
            Kind::Type => Type::TypeVar(fresh_name.clone(), level),
            Kind::Arrow(..) => unreachable!(
                "Arrow kinds are for builtin type constructors only; \
                 builtins are pre-registered directly in kind_env and \
                 never appear in TypeScheme.kind_vars"
            ),
        };

        renaming
            .type_map
            .borrow_mut()
            .insert(var.clone(), instantiated_type);
    }

    // Copy constraints with renamed variables (from both type_vars and kind_vars)
    for constraint in &scheme.constraints {
        match constraint {
            Constraint::Class {
                class,
                vars,
                origin_name: constraint_origin_name,
                origin_span: constraint_origin_span,
            } => {
                let fresh_vars: Vec<String> = vars
                    .iter()
                    .filter_map(|v| var_renaming.get(v).cloned())
                    .collect();
                if fresh_vars.len() == vars.len() {
                    // Use constraint's origin info if present, otherwise use call-site origin
                    let final_origin_name = constraint_origin_name
                        .clone()
                        .or_else(|| origin_name.map(Arc::from));
                    let final_origin_span = constraint_origin_span.clone().or(origin_span.clone());

                    state.constraints.push(Constraint::Class {
                        class: Arc::clone(class),
                        vars: fresh_vars,
                        origin_name: final_origin_name,
                        origin_span: final_origin_span,
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
    let constraints_snap = state.constraints.clone();
    generalize_with_doc(
        level,
        ty,
        state,
        &constraints_snap,
        None,
        crate::ast::Span::origin(),
    )
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
/// When present, diagnostics report "ambiguous type variable 'x'" (without the internal name noise).
///
/// `emitted` deduplicates warnings: tracks (TypeVar name, Span) pairs already warned about.
//
// Task 4: DONE — constraint origin_span is updated to per-argument span in check_call_with_scheme
// (typecheck.rs) by collecting (param type vars → arg span) pairs during the argument loop and
// patching state.constraints[constraints_start..] after unification.
//
// Task 5: DONE — origin_name/origin_span are now used when present (see match arm below).
// Emits "argument to `{name}` has unconstrained type — {class} constraint will be silently dropped"
// and uses origin_span as the diagnostic span when available.
//
// Task 6: DONE — format_var_name now shows just the source name (e.g., 'a') without the
// "(internal: _tN)" suffix — the internal name is noise for users.
//
// Task 7: DONE — unit tests added in test_t013_origin_name_message_format and
// test_t013_fallback_message_format; corpus test in
// tests/corpus/typecheck/warnings/t013_origin_name_call.llt-eval.
fn emit_ambiguous_constraint_diagnostics(
    constraints: &[Constraint],
    subst_snapshot: &HashMap<String, Type>,
    source_names: &HashMap<String, String>,
    diagnostics: &mut Vec<crate::error::TypeDiagnostic>,
    span: crate::ast::Span,
    emitted: &mut std::collections::HashSet<(String, crate::ast::Span)>,
) {
    let is_discharged = |var_name: &str| -> bool {
        subst_snapshot
            .get(var_name)
            .map(|t| !matches!(t, Type::TypeVar(_, _) | Type::Operator(_)))
            .unwrap_or(false)
    };

    // Format a variable name with source name if available.
    // When a source name is known (e.g., the scheme's quantified name 'a'), show just
    // that name — the internal _tN name is noise for users. When no source name is
    // available, show the internal name as a last resort.
    let format_var_name = |var: &str| -> String {
        if let Some(source_name) = source_names.get(var) {
            format!("'{}'", source_name)
        } else {
            format!("'{}'", var)
        }
    };
    for c in constraints {
        match c {
            Constraint::Class {
                class,
                vars,
                origin_name,
                origin_span,
            } => {
                for var in vars {
                    if !is_discharged(var) {
                        // Use argument-level span when available (Task 4: origin_span set during
                        // instantiate_scheme at argument type-checking sites). Fall back to the
                        // call-site span passed to this function.
                        let diag_span = origin_span.clone().unwrap_or_else(|| span.clone());
                        // Deduplicate: only emit if this (var, diag_span) pair hasn't been seen
                        if emitted.insert((var.clone(), diag_span.clone())) {
                            let message = if let Some(name) = origin_name {
                                // Better message: cite the origin function and constraint class.
                                // Drops the internal TypeVar name — user doesn't know/care about _tN.
                                format!(
                                    "argument to `{}` has unconstrained type — {} constraint will be silently dropped",
                                    name, class
                                )
                            } else {
                                format!(
                                    "ambiguous type variable {} in constraint {}: appears in constraint but not in the type — constraint will be silently dropped",
                                    format_var_name(var), class
                                )
                            };
                            diagnostics.push(crate::error::TypeDiagnostic {
                                message,
                                span: diag_span,
                                code: "T013",
                                level: crate::error::DiagnosticLevel::Warn,
                            });
                        }
                    }
                }
            }
            Constraint::HasField {
                dict_var,
                label,
                field_var,
            } => {
                if !is_discharged(dict_var) {
                    // Deduplicate: only emit if this (var, span) pair hasn't been seen
                    if emitted.insert((dict_var.clone(), span.clone())) {
                        diagnostics.push(crate::error::TypeDiagnostic {
                            message: format!(
                                "ambiguous type variable {} (dict) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(dict_var)
                            ),
                            span: span.clone(),
                            code: "T013",
                            level: crate::error::DiagnosticLevel::Warn,
                        });
                    }
                }
                // Only Label::Var positions can be ambiguous. Label::Concrete strings
                // are never present in the substitution map, so checking them would
                // unconditionally fire a spurious T013 for every HasField with a
                // literal label (false-positive).
                if let Label::Var(label_var) = label {
                    if !is_discharged(label_var) {
                        // Deduplicate: only emit if this (var, span) pair hasn't been seen
                        if emitted.insert((label_var.clone(), span.clone())) {
                            diagnostics.push(crate::error::TypeDiagnostic {
                                message: format!(
                                    "ambiguous label variable {} in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                    format_var_name(label_var)
                                ),
                                span: span.clone(),
                                code: "T013",
                                level: crate::error::DiagnosticLevel::Warn,
                            });
                        }
                    }
                }
                if !is_discharged(field_var) {
                    // Deduplicate: only emit if this (var, span) pair hasn't been seen
                    if emitted.insert((field_var.clone(), span.clone())) {
                        diagnostics.push(crate::error::TypeDiagnostic {
                            message: format!(
                                "ambiguous type variable {} (field) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(field_var)
                            ),
                            span: span.clone(),
                            code: "T013",
                            level: crate::error::DiagnosticLevel::Warn,
                        });
                    }
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
///
/// The `constraints` parameter carries the type class constraints accumulated during inference
/// of the binding being generalized. Callers on the `infer_dict` path collect these constraints
/// via `std::mem::take(&mut state.constraints)` after entry inference and pass them explicitly
/// here — the constraint queue is scoped to each dict entry by the caller, not by this function.
/// Callers outside the `infer_dict` path (e.g., the `generalize` wrapper) pass
/// `&state.constraints` directly.
pub fn generalize_with_doc(
    level: u32,
    ty: &Type,
    state: &mut InferState,
    constraints: &[Constraint],
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
        if !constraints.is_empty() {
            let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();
            emit_ambiguous_constraint_diagnostics(
                constraints,
                &subst_snapshot,
                &state.type_var_source_names,
                &mut state.diagnostics,
                span,
                &mut state.t013_emitted,
            );
        }
        return TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            kind_vars: Vec::new(),
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
        if !constraints.is_empty() {
            let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();
            emit_ambiguous_constraint_diagnostics(
                constraints,
                &subst_snapshot,
                &state.type_var_source_names,
                &mut state.diagnostics,
                span,
                &mut state.t013_emitted,
            );
        }

        TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            kind_vars: Vec::new(),
            doc,
            inner_schemes: None,
        }
    } else {
        // Filter constraints: keep only those on generalized variables
        let generalizable_vars: HashSet<String> = generalizable_type_vars.iter().cloned().collect();

        // Snapshot the substitution map so the filter closure can look up TypeVar→TypeVar
        // bindings without borrowing `state` during `constraints.iter()`.
        //
        // When a fresh var "_bt0" from `instantiate_scheme` is bound to "_label_0"
        // (the actual label TypeVar from the function param) in `state.subst`, the HasField
        // constraint records "_bt0" as the label var. But "_bt0" is not in `generalizable_vars`
        // (it's a bound intermediate). We must resolve through one substitution level to find
        // the effective free variable "_label_0" before checking generalizable membership.
        let subst_snapshot: HashMap<String, Type> = state.subst.type_map.borrow().clone();

        // Helper: resolve a type variable name through the full substitution chain.
        // T5 FIX: Follow chains like α→β→γ to the end (was only doing one hop).
        let resolve_var_name = |var_name: &str| -> String {
            let mut current = var_name.to_string();
            let mut visited = HashSet::new();
            loop {
                if !visited.insert(current.clone()) {
                    // Cycle detected — return current
                    return current;
                }
                match subst_snapshot.get(&current) {
                    Some(Type::TypeVar(resolved_name, _)) => {
                        current = resolved_name.clone();
                    }
                    Some(Type::Operator(resolved_name)) => {
                        current = resolved_name.clone();
                    }
                    _ => return current,
                }
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

        // Helper: format a variable name with source name if available.
        // Show just the source name — the internal _tN name is noise for users.
        let format_var_name = |var: &str| -> String {
            if let Some(source_name) = state.type_var_source_names.get(var) {
                format!("'{}'", source_name)
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
        for c in constraints {
            match c {
                Constraint::Class {
                    class,
                    vars,
                    origin_name,
                    origin_span,
                } => {
                    // Resolve all vars through one substitution level
                    let resolved_vars: Vec<String> =
                        vars.iter().map(|v| resolve_var_name(v)).collect();
                    // Keep constraint if ALL resolved vars are generalizable
                    if resolved_vars.iter().all(|v| generalizable_vars.contains(v)) {
                        generalizable_constraints.push(Constraint::Class {
                            class: Arc::clone(class),
                            vars: resolved_vars,
                            origin_name: origin_name.clone(),
                            origin_span: origin_span.clone(),
                        });
                    } else {
                        // Diagnostic: ambiguous type variable in constraint
                        // (appears in constraint but not in the type — constraint will be silently dropped)
                        // T2 FIX: For MPTC constraints with FDs, only flag vars that are non-generalizable
                        // AND not covered by a FD whose determining positions are all generalizable.
                        for (var_idx, var) in resolved_vars.iter().enumerate() {
                            if !generalizable_vars.contains(var) && !is_discharged(var) {
                                // Check if this var is covered by a FD with all determining positions generalizable
                                let is_fd_covered = class.determines.iter().any(
                                    |(det_positions, ded_positions)| {
                                        // Is this var in a determined position?
                                        if !ded_positions.contains(&var_idx) {
                                            return false;
                                        }
                                        // Are ALL determining positions generalizable?
                                        det_positions.iter().all(|&det_idx| {
                                            resolved_vars
                                                .get(det_idx)
                                                .map(|v| generalizable_vars.contains(v))
                                                .unwrap_or(false)
                                        })
                                    },
                                );

                                if !is_fd_covered {
                                    // Deduplicate: only emit if this (var, span) pair hasn't been seen
                                    if state.t013_emitted.insert((var.clone(), span.clone())) {
                                        state.diagnostics.push(crate::error::TypeDiagnostic {
                                            message: format!(
                                                "ambiguous type variable {} in constraint {}: appears in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(var),
                                                class.name
                                            ),
                                            span: span.clone(),
                                            code: "T013",
                                            level: crate::error::DiagnosticLevel::Warn,
                                        });
                                    }
                                }
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
                                    // Deduplicate: only emit if this (var, span) pair hasn't been seen
                                    if state.t013_emitted.insert((resolved.clone(), span.clone())) {
                                        state.diagnostics.push(crate::error::TypeDiagnostic {
                                            message: format!(
                                                "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(&resolved)
                                            ),
                                            span: span.clone(),
                                            code: "T013",
                                            level: crate::error::DiagnosticLevel::Warn,
                                        });
                                    }
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
                                    // For aggregated warnings, deduplicate on dict (the first var mentioned)
                                    if state
                                        .t013_emitted
                                        .insert((effective_dict.clone(), span.clone()))
                                    {
                                        state.diagnostics.push(crate::error::TypeDiagnostic {
                                            message: format!(
                                                "ambiguous type variables {}, {} in constraint HasField: appear in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(&effective_dict),
                                                format_var_name(&effective_field)
                                            ),
                                            span: span.clone(),
                                            code: "T013",
                                            level: crate::error::DiagnosticLevel::Warn,
                                        });
                                    }
                                }
                            } else if dict_ambiguous && !is_discharged(&effective_dict) {
                                // Deduplicate: only emit if this (var, span) pair hasn't been seen
                                if state
                                    .t013_emitted
                                    .insert((effective_dict.clone(), span.clone()))
                                {
                                    state.diagnostics.push(crate::error::TypeDiagnostic {
                                        message: format!(
                                            "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&effective_dict)
                                        ),
                                        span: span.clone(),
                                        code: "T013",
                                        level: crate::error::DiagnosticLevel::Warn,
                                    });
                                }
                            } else if field_ambiguous && !is_discharged(&effective_field) {
                                // Deduplicate: only emit if this (var, span) pair hasn't been seen
                                if state
                                    .t013_emitted
                                    .insert((effective_field.clone(), span.clone()))
                                {
                                    state.diagnostics.push(crate::error::TypeDiagnostic {
                                        message: format!(
                                            "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&effective_field)
                                        ),
                                        span: span.clone(),
                                        code: "T013",
                                        level: crate::error::DiagnosticLevel::Warn,
                                    });
                                }
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
            kind_vars: Vec::new(),
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
        Type::App(f, a) => {
            collect_pretty_type_vars(f, seen);
            collect_pretty_type_vars(a, seen);
        }
        Type::TyCon(_) => {}
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
        // App covers Seq[T], Map[K V], Handle[cap] — pretty-print by recursing into
        // the structure using format_type_pretty so TypeVars get renamed.
        Type::TyCon(name) => name.clone(),
        Type::App(_, _) => {
            // Use Display for the overall structure (handles Seq/Map/Handle formatting),
            // but rename TypeVars inline by applying the rename map to the Display output.
            // This is a conservative approach: Display already formats these correctly,
            // and pretty_type_str handles _tN → letter renaming in a post-pass.
            format!("{}", ty)
        }
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
    tycon_defs: HashMap<String, TyConDef>,
    parent: Option<Rc<TypeEnv>>,
}

impl TypeEnv {
    pub fn new() -> Self {
        Self {
            bindings: HashMap::new(),
            type_aliases: HashMap::new(),
            tycon_defs: HashMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: &Rc<TypeEnv>) -> Self {
        Self {
            bindings: HashMap::new(),
            type_aliases: HashMap::new(),
            tycon_defs: HashMap::new(),
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

    pub fn insert_tycon_def(&mut self, name: String, def: TyConDef) {
        self.tycon_defs.insert(name, def);
    }

    pub fn lookup_tycon_def(&self, name: &str) -> Option<&TyConDef> {
        if let Some(def) = self.tycon_defs.get(name) {
            return Some(def);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(def) = env.tycon_defs.get(name) {
                return Some(def);
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Resolve an unqualified constructor name to its fully qualified tag (T-956).
    ///
    /// Searches all TyConDef entries visible in this environment (current frame and parent chain)
    /// for a constructor whose unqualified name (the part after the last `.`) matches `name`.
    /// Returns the qualified tag string (e.g., `"Result.Ok"` for `name == "Ok"`) from the
    /// INNERMOST frame that has a match. Returns `None` if not found in any frame.
    ///
    /// Disambiguation: uses inner-wins semantics (same as TypeEnv bindings). If multiple TyCons
    /// in the same frame have a constructor with the same unqualified name, the first match in
    /// iteration order is returned (HashMap order; determinism not guaranteed for ambiguous cases).
    pub fn resolve_constructor_tag(&self, name: &str) -> Option<String> {
        // Already qualified — no lookup needed.
        if name.contains('.') {
            return Some(name.to_string());
        }

        // Search the current frame first (inner-wins).
        for (tycon_name, def) in &self.tycon_defs {
            for (ctor_tag, _arity) in &def.constructors {
                let unqualified = ctor_tag
                    .rfind('.')
                    .map_or(ctor_tag.as_str(), |pos| &ctor_tag[pos + 1..]);
                if unqualified == name {
                    let qualified = if ctor_tag.contains('.') {
                        ctor_tag.clone()
                    } else {
                        format!("{}.{}", tycon_name, ctor_tag)
                    };
                    return Some(qualified);
                }
            }
        }

        // Walk the parent chain recursively if not found in current frame.
        self.parent
            .as_ref()
            .and_then(|p| p.resolve_constructor_tag(name))
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

    /// Collect only the binding names defined in THIS frame (no parent walk).
    ///
    /// Used by `imports::collect_names_above_baseline` to identify names introduced
    /// by the prelude (rather than inherited from the builtin baseline).
    pub fn own_type_aliases(&self) -> impl Iterator<Item = (&str, &TypeAlias)> {
        self.type_aliases.iter().map(|(k, v)| (k.as_str(), v))
    }

    pub fn collect_own_names(&self, names: &mut std::collections::HashSet<String>) {
        for name in self.bindings.keys() {
            names.insert(name.clone());
        }
    }

    /// Return the parent environment frame, if any.
    ///
    /// Used by `imports::collect_names_above_baseline` to walk the frame chain up to
    /// the builtin baseline boundary.
    pub fn parent(&self) -> Option<&Rc<TypeEnv>> {
        self.parent.as_ref()
    }

    /// Copy all bindings and type aliases from `other` into `self`.
    ///
    /// Copies only the own (non-parent) bindings and type aliases from `other`.
    /// Parent chains are not traversed. Existing entries in `self` with the same
    /// name are overwritten by entries from `other`.
    ///
    /// Used by `build_builtins_type_env()` to combine per-module type environments
    /// (core, datetime, net) into a single flat environment.
    pub fn merge(&mut self, other: TypeEnv) {
        for (name, scheme) in other.bindings {
            self.bindings.insert(name, scheme);
        }
        for (name, alias) in other.type_aliases {
            self.type_aliases.insert(name, alias);
        }
        for (name, def) in other.tycon_defs {
            self.tycon_defs.insert(name, def);
        }
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
            ("builtin-gt", ">"),
            ("builtin-gte", ">="),
            ("builtin-lte", "<="),
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
            // prelude-missing-wrappers sprint: stable aliases for previously-unwrapped builtins
            ("builtin-keys", "keys"),
            ("builtin-merge", "merge"),
            ("builtin-each", "each"),
            ("builtin-each-key", "each-key"),
            ("builtin-each-kv", "each-kv"),
            ("builtin-floor", "floor"),
            ("builtin-round", "round"),
            ("builtin-to-float", "to-float"),
            ("builtin-try", "try"),
            ("builtin-apply", "apply"),
            ("builtin-type-of", "type-of"),
            ("builtin-narrow", "narrow"),
            // builtin-from-json deleted (json-serde-removal sprint): from-json is pure tinct in stdlib/codecs/json.llt
            // builtin-privacy-primary-names sprint: new builtin-* → bare-name mappings
            ("builtin-raise", "raise"),
            ("builtin-emit", "emit"),
            ("builtin-env", "env"),
            ("builtin-str", "str"),
            ("builtin-split", "split"),
            ("builtin-trim", "trim"),
            ("builtin-str-length", "str-length"),
            ("builtin-str-slice", "str-slice"),
            ("builtin-to-int", "to-int"),
            ("builtin-append", "append"),
            ("builtin-length", "length"),
            // docgen-conformance: list-dir, load, expand exported from prelude
            ("builtin-list-dir", "list-dir"),
            ("builtin-load", "load"),
            ("builtin-expand", "expand"),
            ("builtin-eval", "eval"),
            ("builtin-eval-types", "eval-types"),
            ("builtin-blake3", "blake3"),
            ("builtin-cap-identity", "cap-identity"),
            ("builtin-include-cache-get", "include-cache-get"),
            ("builtin-include-cache-put", "include-cache-put"),
            // builtin-privacy-operators-and-io sprint: new builtin-* → bare-name mappings
            ("builtin-replace", "replace"),
            ("builtin-str-chars", "str-chars"),
            ("builtin-char-code", "char-code"),
            ("builtin-chr", "chr"),
            ("builtin-str-bytes", "str-bytes"),
            ("builtin-bytes-str", "bytes-str"),
            ("builtin-str-index-of", "str-index-of"),
            ("builtin-trim-start", "trim-start"),
            ("builtin-trim-end", "trim-end"),
            ("builtin-str-to-upper-char", "str-to-upper-char"),
            ("builtin-str-to-lower-char", "str-to-lower-char"),
            ("builtin-str-map-chars", "str-map-chars"),
            ("builtin-regex-match?", "regex-match?"),
            // math functions (pow, sqrt, sin, etc.) are NOT injected here:
            // they are stdlib/math.llt exports (require [include %libdir "math.llt"]).
            // The aliases were removed in T-826 along with the runtime injection loop.
            ("builtin-band", "band"),
            ("builtin-bor", "bor"),
            ("builtin-bxor", "bxor"),
            ("builtin-shl", "shl"),
            ("builtin-shr", "shr"),
            ("builtin-float", "float"),
            // B-168: I/O and builder builtins renamed to builtin-* prefix
            ("builtin-open", "open"),
            ("builtin-write", "write"),
            ("builtin-write-atomic", "write-atomic"),
            ("builtin-write-handle", "write-handle"),
            ("builtin-flush", "flush"),
            ("builtin-close", "close"),
            ("builtin-stat", "stat"),
            ("builtin-exists", "exists"),
            ("builtin-stat-symlink", "stat-symlink"),
            ("builtin-copy-file", "copy-file"),
            ("builtin-symlink", "symlink"),
            ("builtin-set-permissions", "set-permissions"),
            ("builtin-make-dir", "make-dir"),
            ("builtin-rename", "rename"),
            ("builtin-link", "link"),
            ("builtin-read-link", "read-link"),
            ("builtin-get-xattr", "get-xattr"),
            ("builtin-set-xattr", "set-xattr"),
            ("builtin-remove-xattr", "remove-xattr"),
            ("builtin-list-xattrs", "list-xattrs"),
            ("builtin-raw-create", "raw-create"),
            ("builtin-seek", "seek"),
            ("builtin-seek-end", "seek-end"),
            ("builtin-position", "position"),
            ("builtin-revocable", "revocable"),
            ("builtin-revoke-cap", "revoke-cap"),
            ("builtin-cap-data", "cap-data"),
            ("builtin-connect", "connect"),
            ("builtin-tls-layer", "tls-layer"),
            ("builtin-tls-peer-cert", "tls-peer-cert"),
            ("builtin-send-datagram", "send-datagram"),
            ("builtin-recv-datagram", "recv-datagram"),
            ("builtin-string-handle", "string-handle"),
            ("builtin-make-builder", "make-builder"),
            ("builtin-builder-set", "builder-set"),
            ("builtin-builder-delete", "builder-delete"),
            ("builtin-builder-finish", "builder-finish"),
            ("builtin-builder-snapshot", "builder-snapshot"),
            ("builtin-builder-has?", "builder-has?"),
            ("builtin-builder-get", "builder-get"),
            ("builtin-builder-get-or", "builder-get-or"),
            // Reactive cells (T-831)
            ("builtin-reactive-cell", "reactive-cell"),
            ("builtin-cell-get", "cell-get"),
            ("builtin-cell-set", "cell-set"),
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
    pub notes: Box<Vec<String>>,
    /// Explicit stable error code, e.g. `"T014"`. When `Some`, overrides the message-pattern
    /// dispatch in `code()`. Use `with_code()` to attach a code at the construction site.
    pub code: Option<String>,
}

impl TypeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        let span = Span {
            start: span.start,
            end: span.end,
            file: None,
        };
        Self {
            message: message.into(),
            span,
            notes: Box::new(Vec::new()),
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
    if let Some(snippet) = render_span_snippet(source, err.span.clone()) {
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
    for note in err.notes.iter() {
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
        let b_var = || Type::TypeVar("b".to_string(), 0);
        let instance = InstanceDecl {
            class_name: "Appendable".to_string(),
            instance_type: Type::seq(b_var()),
            det_positions: vec![], // Single-parameter class, no FDs
            method_types: {
                let mut methods = HashMap::new();
                methods.insert(
                    "append".to_string(),
                    Type::Function {
                        params: vec![(None, Type::seq(b_var())), (None, Type::seq(b_var()))],
                        ret: Box::new(Type::seq(b_var())),
                        variadic: false,
                    },
                );
                methods
            },
        };

        // Use a fresh InstanceEnv containing ONLY the test instance.
        // InferState::new() pre-seeds instance_env with structural instances including
        // Appendable Seq[T]. Cloning state.instance_env would therefore contain both
        // Seq[T] and Seq[b], which are equally specific for Seq[Int] and cause an
        // ambiguity error. An isolated InstanceEnv tests resolve_instance in isolation.
        let mut inst_env = crate::type_class::InstanceEnv::new();
        inst_env.insert(instance.clone()).unwrap();

        // Resolve against Seq[Int]
        let target = Type::seq(Type::Int);
        let resolved = inst_env
            .resolve_instance("Appendable", &target, &mut state)
            .expect("resolve_instance should not error");

        assert!(resolved.is_some(), "should resolve Appendable for Seq[Int]");
        let resolved = resolved.unwrap();

        // The method types should have Int substituted for b
        let append_ty = resolved.method_types.get("append");
        assert!(append_ty.is_some(), "append method should exist");

        // Check that the method signature has Seq[Int], not Seq[b]
        if let Type::Function { params, ret, .. } = append_ty.unwrap() {
            assert_eq!(params.len(), 2);
            // Both params should be Seq[Int] or Seq[_tN] (freshened)
            let p0 = &params[0].1;
            if let Some(elem) = p0.as_seq() {
                // Should be Int or a fresh type var that got unified with Int
                assert!(
                    matches!(elem, Type::Int | Type::TypeVar(..)),
                    "first param should be Seq[Int] or Seq[fresh], got {:?}",
                    elem
                );
            } else {
                panic!("expected Seq type for first param, got {:?}", p0);
            }

            let ret_ty = ret.as_ref();
            if let Some(elem) = ret_ty.as_seq() {
                assert!(
                    matches!(elem, Type::Int | Type::TypeVar(..)),
                    "return should be Seq[Int] or Seq[fresh], got {:?}",
                    elem
                );
            } else {
                panic!("expected Seq type for return, got {:?}", ret_ty);
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

    // T013 Task 5: emit_ambiguous_constraint_diagnostics origin_name path.
    // When a Constraint::Class carries origin_name, the diagnostic message should cite
    // the function name rather than the internal TypeVar name.
    #[test]
    fn test_t013_origin_name_message_format() {
        use crate::ast::{Position, Span};
        use crate::error::DiagnosticLevel;
        use crate::type_class::{ClassDecl, Constraint};
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        // Build a minimal ClassDecl for "Showable" (params=[] follows Constraint::new_by_name pattern)
        let class = Arc::new(ClassDecl {
            name: "Showable".to_string(),
            params: vec![],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Constraint with origin_name="str" (as would be set by instantiate_scheme at a VarRef)
        let arg_span = Span {
            start: Position {
                offset: 10,
                line: 1,
                column: 11,
            },
            end: Position {
                offset: 14,
                line: 1,
                column: 15,
            },
            file: None,
        };
        let constraint_with_origin = Constraint::Class {
            class: Arc::clone(&class),
            vars: vec!["_t42".to_string()],
            origin_name: Some(Arc::from("str")),
            origin_span: Some(arg_span.clone()),
        };

        // _t42 is NOT in the substitution map → not discharged → T013 fires
        let subst_snapshot: HashMap<String, Type> = HashMap::new();
        let source_names: HashMap<String, String> = HashMap::new();
        let mut diagnostics: Vec<crate::error::TypeDiagnostic> = Vec::new();
        let fallback_span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: 5,
                line: 1,
                column: 6,
            },
            file: None,
        };
        let mut emitted: HashSet<(String, Span)> = HashSet::new();

        emit_ambiguous_constraint_diagnostics(
            &[constraint_with_origin],
            &subst_snapshot,
            &source_names,
            &mut diagnostics,
            fallback_span,
            &mut emitted,
        );

        assert_eq!(diagnostics.len(), 1, "expected exactly one T013 diagnostic");
        let diag = &diagnostics[0];
        assert_eq!(diag.code, "T013");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
        // Message must cite the origin function, not the internal TypeVar name
        assert!(
            diag.message.contains("argument to `str`"),
            "expected message to cite origin function 'str'; got: {}",
            diag.message
        );
        assert!(
            diag.message.contains("Showable"),
            "expected message to cite constraint class 'Showable'; got: {}",
            diag.message
        );
        assert!(
            !diag.message.contains("_t42"),
            "expected message to omit internal TypeVar name '_t42'; got: {}",
            diag.message
        );
        // When origin_span is provided, the diagnostic span should be the argument span
        assert_eq!(
            diag.span, arg_span,
            "expected diagnostic span to use origin_span (argument location)"
        );
    }

    // T013 Task 5: when origin_name is absent, the fallback message format is preserved.
    #[test]
    fn test_t013_fallback_message_format() {
        use crate::ast::{Position, Span};
        use crate::error::DiagnosticLevel;
        use crate::type_class::{ClassDecl, Constraint};
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        let class = Arc::new(ClassDecl {
            name: "Comparable".to_string(),
            params: vec![],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
        });

        // Constraint WITHOUT origin info (annotation-driven, as in existing tests)
        let constraint_no_origin = Constraint::new(class, "_t7");

        let subst_snapshot: HashMap<String, Type> = HashMap::new();
        let source_names: HashMap<String, String> = HashMap::new();
        let mut diagnostics: Vec<crate::error::TypeDiagnostic> = Vec::new();
        let call_span = Span {
            start: Position {
                offset: 0,
                line: 1,
                column: 1,
            },
            end: Position {
                offset: 5,
                line: 1,
                column: 6,
            },
            file: None,
        };
        let mut emitted: HashSet<(String, Span)> = HashSet::new();

        emit_ambiguous_constraint_diagnostics(
            &[constraint_no_origin],
            &subst_snapshot,
            &source_names,
            &mut diagnostics,
            call_span.clone(),
            &mut emitted,
        );

        assert_eq!(diagnostics.len(), 1, "expected exactly one T013 diagnostic");
        let diag = &diagnostics[0];
        assert_eq!(diag.code, "T013");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
        // Fallback: must still contain "ambiguous type variable" and the constraint class
        assert!(
            diag.message.contains("ambiguous type variable"),
            "expected fallback message to contain 'ambiguous type variable'; got: {}",
            diag.message
        );
        assert!(
            diag.message.contains("Comparable"),
            "expected fallback message to cite 'Comparable'; got: {}",
            diag.message
        );
        // Fallback: span should be the call_span, not any argument span
        assert_eq!(
            diag.span, call_span,
            "expected fallback span to be call_span"
        );
    }

    /// A TypeScheme with a `kind_vars` entry of `Kind::Operator` must instantiate
    /// to `Type::Operator(fresh_name)`, not `Type::TypeVar(fresh_name, level)`.
    ///
    /// Concretely: the scheme `∀(f: Operator) a. f a` should produce
    /// `App(Operator("_t0"), TypeVar("_t1", 1))` when instantiated at level 1,
    /// and the fresh Operator name must be registered in `state.kind_env` so that
    /// subsequent UNIFY-OPERATOR and KIND-OPERATOR rules can recognise it as a
    /// type constructor rather than a monomorphic type variable.
    ///
    /// Mutation resistance: if `instantiate_scheme` treated `kind_vars` variables
    /// like regular `type_vars`, it would produce `TypeVar("_t0", 1)` instead of
    /// `Operator("_t0")`, and the `kind_env` entry would be absent — both
    /// assertions below would fail.
    #[test]
    fn test_instantiate_scheme_kind_var_operator_produces_type_operator() {
        use crate::types::{InferState, Kind, Type};

        let mut state = InferState::new();
        state.level = 1;

        // Build the scheme body: App(Operator("f"), TypeVar("a", 0))
        // representing the type `f a` where f is Operator-kinded.
        let scheme_body = Type::App(
            Box::new(Type::Operator("f".to_string())),
            Box::new(Type::TypeVar("a".to_string(), 0)),
        );

        // Construct a scheme: ∀(f: Operator) a. f a
        let scheme = TypeScheme {
            type_vars: vec!["a".to_string()],
            kind_vars: vec![("f".to_string(), Kind::Operator)],
            constraints: vec![],
            body: scheme_body,
            label_vars: vec![],
            doc: None,
            inner_schemes: None,
        };

        let instantiated = instantiate_scheme(&scheme, state.level, &mut state, None, None);

        // The instantiated type must be App(Operator(fresh_f), TypeVar(fresh_a, 1)).
        match instantiated {
            Type::App(ref f_ty, ref a_ty) => {
                // f must instantiate to Type::Operator, not Type::TypeVar.
                match f_ty.as_ref() {
                    Type::Operator(fresh_f) => {
                        // The fresh Operator name must be registered in kind_env with Kind::Operator.
                        assert_eq!(
                            state.kind_env.get(fresh_f.as_str()),
                            Some(&Kind::Operator),
                            "fresh Operator name '{}' must be in kind_env with Kind::Operator",
                            fresh_f
                        );
                        // Levels map should contain the fresh name so level-based
                        // generalization can track it.
                        assert!(
                            state.levels.contains_key(fresh_f.as_str()),
                            "fresh Operator name '{}' must be registered in state.levels",
                            fresh_f
                        );
                    }
                    other => panic!("expected Type::Operator for kind_var 'f', got {:?}", other),
                }
                // a must instantiate to Type::TypeVar.
                match a_ty.as_ref() {
                    Type::TypeVar(fresh_a, lv) => {
                        assert_eq!(*lv, 1, "TypeVar level must match instantiation level");
                        // a must NOT be in kind_env as Operator (it's a regular type var).
                        assert_ne!(
                            state.kind_env.get(fresh_a.as_str()),
                            Some(&Kind::Operator),
                            "regular type_var 'a' must not be Kind::Operator in kind_env"
                        );
                    }
                    other => panic!(
                        "expected Type::TypeVar for regular type_var 'a', got {:?}",
                        other
                    ),
                }
            }
            other => panic!("expected App(Operator, TypeVar), got {:?}", other),
        }
    }

    /// A monomorphic TypeScheme (both `type_vars` and `kind_vars` empty) must return
    /// its body directly without incrementing the name counter.
    ///
    /// Mutation resistance: if the early-exit check in `instantiate_scheme` only tested
    /// `type_vars.is_empty()` (not `kind_vars.is_empty()`), a scheme with only `kind_vars`
    /// would incorrectly skip freshening. Conversely, an empty scheme must skip allocation.
    #[test]
    fn test_instantiate_scheme_empty_kind_vars_monomorphic_no_freshening() {
        use crate::types::{InferState, Type};

        let mut state = InferState::new();

        let scheme = TypeScheme::mono(Type::Int);

        let result = instantiate_scheme(&scheme, state.level, &mut state, None, None);

        assert_eq!(
            result,
            Type::Int,
            "monomorphic scheme must return body unchanged"
        );
        assert_eq!(
            state.subst.name_counter.get(),
            0,
            "monomorphic instantiation must not increment name counter"
        );
    }
}
