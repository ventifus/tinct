//! Type environment, instantiation, generalization, Display, type aliases,
//! class/instance environments, and type errors.

use std::collections::{HashMap, HashSet};

use indexmap::IndexMap;
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
pub fn instantiate_at_level(ty: &Type, state: &mut InferState, span: &crate::ast::Span) -> Type {
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
    // Build source-name frequency map: if two vars share the same source name, we must use
    // the full concrete name as the instantiation source to avoid InferState collisions.
    // Build source-name frequency counts (owned to avoid borrow conflicts).
    let source_counts: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for var in &type_vars {
            let src = crate::type_infer::InferState::typevar_source_only(var).to_owned();
            *m.entry(src).or_insert(0) += 1;
        }
        m
    };

    let renaming = Substitution {
        type_map: std::cell::RefCell::new(HashMap::with_capacity(type_vars.len())),
    };
    for var in type_vars {
        // First-write-wins: skip if this var was already mapped (handles duplicates from the Vec).
        if !renaming.type_map.borrow().contains_key(&var) {
            // Use the abstract source name when unique; fall back to full concrete name on collision.
            let src = crate::type_infer::InferState::typevar_source_only(&var).to_owned();
            let effective_src = if source_counts.get(&src).copied().unwrap_or(0) == 1 {
                src
            } else {
                var.clone() // collision: preserve full concrete name for within-scope uniqueness
            };
            let kind = if operator_names.contains(&var) {
                Kind::Operator
            } else {
                Kind::Type
            };
            let fresh_name =
                crate::type_infer::InferState::typevar_name(&effective_src, &kind, span);
            let lvl = state.level;
            state.levels.insert(fresh_name.clone(), lvl);
            state.type_vars.insert(
                fresh_name.clone(),
                crate::type_infer::TypeVarEntry::blank(lvl, kind.clone()),
            );

            if matches!(kind, Kind::Operator) {
                state.kind_env.insert(fresh_name.clone(), Kind::Operator);
                renaming
                    .type_map
                    .borrow_mut()
                    .insert(var, Type::Operator(fresh_name));
            } else {
                renaming
                    .type_map
                    .borrow_mut()
                    .insert(var, Type::TypeVar(fresh_name, lvl));
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
        Type::Dict(row) => Type::Dict(rename_single_type_var_in_row(
            row, old_name, fresh_name, level,
        )),
        Type::Function {
            params,
            ret,
            typed_variadics,
            rest,
            required_count,
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
            typed_variadics: typed_variadics
                .iter()
                .map(|(n, t)| {
                    (
                        n.clone(),
                        rename_single_type_var(t, old_name, fresh_name, level),
                    )
                })
                .collect(),
            rest: rest.as_ref().map(|b| {
                Box::new((
                    b.0.clone(),
                    rename_single_type_var(&b.1, old_name, fresh_name, level),
                ))
            }),
            required_count: *required_count,
        },
        // Type::Seq and Type::Map don't exist as variants; they are represented as
        // Type::App(TyCon("Seq"), elem) and Type::App(App(TyCon("Map"), key), val).
        // The App arm below handles these recursively.
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
        Type::NominalVariant {
            tycon,
            ctor,
            fields,
        } => Type::NominalVariant {
            tycon: tycon.clone(),
            ctor: ctor.clone(),
            fields: rename_single_type_var_in_row(fields, old_name, fresh_name, level),
        },
        // Recursive(μvar.body): rename inside body; do NOT rename the μ-binder itself.
        Type::Recursive { var, body } => Type::Recursive {
            var: var.clone(),
            body: Box::new(rename_single_type_var(body, old_name, fresh_name, level)),
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
        tail: row.tail.clone(),
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
    span: &Span,
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
        // Single var → always unique, use abstract source name.
        let src = crate::type_infer::InferState::typevar_source_only(&scheme.type_vars[0]);
        let fresh_name = crate::type_infer::InferState::typevar_name(src, &Kind::Type, span);
        state.levels.insert(fresh_name.clone(), level);
        state.type_vars.insert(
            fresh_name.clone(),
            crate::type_infer::TypeVarEntry::blank(level, Kind::Type),
        );
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
                    // Rename Var positions in the constraint; pass Ground positions through.
                    let fresh_vars: Vec<crate::type_class::ConstraintArg> = vars
                        .iter()
                        .map(|v| match v {
                            crate::type_class::ConstraintArg::Var(name) => {
                                if let Some(fresh) = var_renaming.get(name.as_str()) {
                                    crate::type_class::ConstraintArg::Var(fresh.clone())
                                } else {
                                    v.clone()
                                }
                            }
                            crate::type_class::ConstraintArg::Ground(_) => v.clone(),
                        })
                        .collect();
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
    };

    // Build source-name frequency map across all scheme vars (type_vars + kind_vars).
    // Vars sharing a source name must fall back to the full concrete name to avoid collisions.
    let source_counts: HashMap<String, usize> = {
        let mut m: HashMap<String, usize> = HashMap::new();
        for var in scheme
            .type_vars
            .iter()
            .chain(scheme.kind_vars.iter().map(|(v, _)| v))
        {
            let src = crate::type_infer::InferState::typevar_source_only(var).to_owned();
            *m.entry(src).or_insert(0) += 1;
        }
        m
    };

    // Instantiate regular type variables as Type::TypeVar.
    for var in &scheme.type_vars {
        let src = crate::type_infer::InferState::typevar_source_only(var).to_owned();
        let effective_src = if source_counts.get(&src).copied().unwrap_or(0) == 1 {
            src.as_str()
        } else {
            var.as_str()
        };
        let label = scheme.label_vars.contains(var);
        let kind = if label { Kind::Label } else { Kind::Type };
        let fresh_name = crate::type_infer::InferState::typevar_name(effective_src, &kind, span);
        state.levels.insert(fresh_name.clone(), level);
        state.type_vars.insert(
            fresh_name.clone(),
            crate::type_infer::TypeVarEntry::blank(level, kind.clone()),
        );
        var_renaming.insert(var.clone(), fresh_name.clone());
        renaming
            .type_map
            .borrow_mut()
            .insert(var.clone(), Type::TypeVar(fresh_name.clone(), level));
        if label {
            state.kind_env.insert(fresh_name, Kind::Label);
        }
    }

    // Instantiate kinded variables according to their kind.
    // Kind::Operator → Type::Operator(fresh_name), registered in kind_env.
    // Kind::Label    → Type::TypeVar(fresh_name, level), registered in kind_env as Label.
    // Kind::Type     → Type::TypeVar(fresh_name, level) (same as a regular type_var).
    for (var, kind) in &scheme.kind_vars {
        let src = crate::type_infer::InferState::typevar_source_only(var).to_owned();
        let effective_src = if source_counts.get(&src).copied().unwrap_or(0) == 1 {
            src.as_str()
        } else {
            var.as_str()
        };
        let fresh_name = crate::type_infer::InferState::typevar_name(effective_src, kind, span);
        state.levels.insert(fresh_name.clone(), level);
        state.type_vars.insert(
            fresh_name.clone(),
            crate::type_infer::TypeVarEntry::blank(level, kind.clone()),
        );
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
            Kind::Arrow(_, _) => {
                // Higher-kinded type constructors: treated as Operator for instantiation purposes.
                state.kind_env.insert(fresh_name.clone(), kind.clone());
                Type::Operator(fresh_name.clone())
            }
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
                let fresh_vars: Vec<crate::type_class::ConstraintArg> = vars
                    .iter()
                    .map(|v| match v {
                        crate::type_class::ConstraintArg::Var(name) => {
                            if let Some(fresh) = var_renaming.get(name.as_str()) {
                                crate::type_class::ConstraintArg::Var(fresh.clone())
                            } else {
                                v.clone()
                            }
                        }
                        crate::type_class::ConstraintArg::Ground(_) => v.clone(),
                    })
                    .collect();
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
    generalize_with_doc(
        level,
        ty,
        state,
        None,
        crate::ast::Span::rust_source(file!(), line!()),
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
                for var_arg in vars {
                    // Only process Var positions — Ground types are concrete and never ambiguous.
                    let var = match var_arg {
                        crate::type_class::ConstraintArg::Var(name) => name.as_str(),
                        crate::type_class::ConstraintArg::Ground(_) => continue,
                    };
                    if !is_discharged(var) {
                        // Use argument-level span when available (Task 4: origin_span set during
                        // instantiate_scheme at argument type-checking sites). Fall back to the
                        // call-site span passed to this function.
                        let diag_span = origin_span.clone().unwrap_or_else(|| span.clone());
                        // Deduplicate: only emit if this (var, diag_span) pair hasn't been seen
                        if emitted.insert((var.to_owned(), diag_span.clone())) {
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
                                level: crate::error::DiagnosticLevel::Warn,
                                kind: "ambiguous-constraint",
                                message,
                                spans: vec![(diag_span, String::new())],
                                notes: vec![],
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
                            level: crate::error::DiagnosticLevel::Warn,
                            kind: "ambiguous-constraint",
                            message: format!(
                                "ambiguous type variable {} (dict) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(dict_var)
                            ),
                            spans: vec![(span.clone(), String::new())],
                            notes: vec![],
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
                                level: crate::error::DiagnosticLevel::Warn,
                                kind: "ambiguous-constraint",
                                message: format!(
                                    "ambiguous label variable {} in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                    format_var_name(label_var)
                                ),
                                spans: vec![(span.clone(), String::new())],
                                notes: vec![],
                            });
                        }
                    }
                }
                if !is_discharged(field_var) {
                    // Deduplicate: only emit if this (var, span) pair hasn't been seen
                    if emitted.insert((field_var.clone(), span.clone())) {
                        diagnostics.push(crate::error::TypeDiagnostic {
                            level: crate::error::DiagnosticLevel::Warn,
                            kind: "ambiguous-constraint",
                            message: format!(
                                "ambiguous type variable {} (field) in HasField constraint: appears in constraint but not in the type — constraint will be silently dropped",
                                format_var_name(field_var)
                            ),
                            spans: vec![(span.clone(), String::new())],
                            notes: vec![],
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
/// **Constraint scoping contract**: Callers must manually save and restore `state.constraints`
/// around generalize calls when constraint scoping is required. This function does NOT manage
/// constraint scoping itself — it filters constraints by TypeVar membership but does not
/// preserve or restore the original constraint set. If the caller needs to isolate constraints
/// for a nested scope (e.g., a let-binding that should not leak constraints to the outer scope),
/// the caller must use `std::mem::take(&mut state.constraints)` before generalize and restore
/// afterward. See dict inference passes 1-4 for the canonical pattern.
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
            param_narrowings: Vec::new(),
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
            param_narrowings: Vec::new(),
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
        for c in &state.constraints {
            match c {
                Constraint::Class {
                    class,
                    vars,
                    origin_name,
                    origin_span,
                } => {
                    // Resolve Var positions through substitution; keep Ground positions as-is.
                    let resolved_args: Vec<crate::type_class::ConstraintArg> = vars
                        .iter()
                        .map(|v| match v {
                            crate::type_class::ConstraintArg::Var(name) => {
                                crate::type_class::ConstraintArg::Var(resolve_var_name(name))
                            }
                            crate::type_class::ConstraintArg::Ground(_) => v.clone(),
                        })
                        .collect();
                    // For generalizable check, extract resolved Var names only
                    let resolved_vars: Vec<String> = resolved_args
                        .iter()
                        .filter_map(|a| match a {
                            crate::type_class::ConstraintArg::Var(name) => Some(name.clone()),
                            crate::type_class::ConstraintArg::Ground(_) => None,
                        })
                        .collect();
                    // Keep constraint if ALL resolved Var positions are generalizable
                    if resolved_vars.iter().all(|v| generalizable_vars.contains(v)) {
                        generalizable_constraints.push(Constraint::Class {
                            class: Arc::clone(class),
                            vars: resolved_args,
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
                                            level: crate::error::DiagnosticLevel::Warn,
                                            kind: "ambiguous-constraint",
                                            message: format!(
                                                "ambiguous type variable {} in constraint {}: appears in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(var),
                                                class.name
                                            ),
                                            spans: vec![(span.clone(), String::new())],
                                            notes: vec![],
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
                                            level: crate::error::DiagnosticLevel::Warn,
                                            kind: "ambiguous-constraint",
                                            message: format!(
                                                "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(&resolved)
                                            ),
                                            spans: vec![(span.clone(), String::new())],
                                            notes: vec![],
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
                                            level: crate::error::DiagnosticLevel::Warn,
                                            kind: "ambiguous-constraint",
                                            message: format!(
                                                "ambiguous type variables {}, {} in constraint HasField: appear in constraint but not in the type — constraint will be silently dropped",
                                                format_var_name(&effective_dict),
                                                format_var_name(&effective_field)
                                            ),
                                            spans: vec![(span.clone(), String::new())],
                                            notes: vec![],
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
                                        level: crate::error::DiagnosticLevel::Warn,
                                        kind: "ambiguous-constraint",
                                        message: format!(
                                            "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&effective_dict)
                                        ),
                                        spans: vec![(span.clone(), String::new())],
                                        notes: vec![],
                                    });
                                }
                            } else if field_ambiguous && !is_discharged(&effective_field) {
                                // Deduplicate: only emit if this (var, span) pair hasn't been seen
                                if state
                                    .t013_emitted
                                    .insert((effective_field.clone(), span.clone()))
                                {
                                    state.diagnostics.push(crate::error::TypeDiagnostic {
                                        level: crate::error::DiagnosticLevel::Warn,
                                        kind: "ambiguous-constraint",
                                        message: format!(
                                            "ambiguous type variable {} in constraint HasField: appears in constraint but not in the type — constraint will be silently dropped",
                                            format_var_name(&effective_field)
                                        ),
                                        spans: vec![(span.clone(), String::new())],
                                        notes: vec![],
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
        let class_env_snapshot = state.build_class_env_snapshot().clone();
        simplify_constraints(&class_env_snapshot, &mut generalizable_constraints);

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
            param_narrowings: Vec::new(),
        }
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
    /// Source-order entries — inserted via `insert_scheme` for bindings that correspond
    /// to static dict/fn-param keys visible to the resolver.  `get_index(slot)` gives the
    /// TypeScheme for that resolver-assigned slot.  IndexMap preserves insertion order and
    /// updates-in-place on duplicate keys, so the slot index of an existing entry is stable.
    slotted: IndexMap<String, TypeScheme>,
    /// Name-only entries — inserted via `insert_scheme_named_only` for bindings that are
    /// NOT assigned a slot by the resolver (class-method injections, ADT constructor type
    /// information during Pass 2, narrowing overrides, etc.).  Looked up by name only.
    extras: HashMap<String, TypeScheme>,
    type_aliases: HashMap<String, TypeAlias>,
    parent: Option<Arc<TypeEnv>>,
    /// Class declarations registered in this scope frame.
    /// Populated by `insert_class` during type-checking; walked by `get_class` and
    /// `build_class_env`. Classes in parent frames are visible to children (inner wins).
    classes: IndexMap<String, ClassDecl>,
    /// Instance declarations registered in this scope frame, keyed by the mangled instance
    /// binding name (e.g. `ɪɴꜱᴛᴀɴᴄᴇ⧼...⧽`). Populated by `insert_instance` during
    /// type-checking; walked by `get_instance`, `all_instances`, and `build_instance_env`.
    instances: IndexMap<String, InstanceDecl>,
    /// Type constructor definitions registered in this scope frame.
    /// Used by `resolve_constructor_tag` to map unqualified constructor names to their
    /// fully-qualified form (e.g. "Ok" → "Result.Ok" when Result is in scope).
    tycon_defs: HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
}

#[allow(dead_code)]
impl TypeEnv {
    pub fn new() -> Self {
        Self {
            slotted: IndexMap::new(),
            extras: HashMap::new(),
            type_aliases: HashMap::new(),
            parent: None,
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            tycon_defs: HashMap::new(),
        }
    }

    pub fn with_parent(parent: &Arc<TypeEnv>) -> Self {
        Self {
            slotted: IndexMap::new(),
            extras: HashMap::new(),
            type_aliases: HashMap::new(),
            parent: Some(Arc::clone(parent)),
            classes: IndexMap::new(),
            instances: IndexMap::new(),
            tycon_defs: HashMap::new(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&TypeScheme> {
        // Look in slotted first, then extras, then walk parent chain.
        if let Some(scheme) = self.slotted.get(name) {
            return Some(scheme);
        }
        if let Some(scheme) = self.extras.get(name) {
            return Some(scheme);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(scheme) = env.slotted.get(name) {
                return Some(scheme);
            }
            if let Some(scheme) = env.extras.get(name) {
                return Some(scheme);
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Slot-indexed lookup with key verification: walk `level` parent frames (0 = current),
    /// look up `slot` in the target frame's `slotted` IndexMap, and return the TypeScheme
    /// ONLY if the key at that slot matches `expected_name`.
    ///
    /// The name check is the same safety net used by the runtime `Environment::get_by_slot`:
    /// if the resolver's slot assignment diverges from the TypeEnv's insertion order (e.g.,
    /// due to non-source-order insertions in sequential-dict envs), the slot lookup returns
    /// `None` and the caller falls back to name-based `get(name)`.
    ///
    /// Returns `None` if:
    /// - The parent chain is shallower than `level`.
    /// - The target frame has fewer than `slot + 1` slotted entries.
    /// - The key at `slot` does not equal `expected_name` (slot-shift detected).
    ///
    /// Call sites MUST fall back to `get(name)` when this returns `None`.
    pub fn get_type_at(&self, level: u32, slot: u32, expected_name: &str) -> Option<&TypeScheme> {
        if level == 0 {
            if let Some((key, scheme)) = self.slotted.get_index(slot as usize) {
                if key == expected_name {
                    return Some(scheme);
                }
            }
            return None;
        }
        // Walk the parent chain using the same borrow-chain pattern as get().
        let mut steps = level;
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            steps -= 1;
            if steps == 0 {
                if let Some((key, scheme)) = env.slotted.get_index(slot as usize) {
                    if key == expected_name {
                        return Some(scheme);
                    }
                }
                return None;
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Look up a binding in the CURRENT frame only (does not walk the parent chain).
    ///
    /// Used by `imports::extract_bindings_from_file_with_fallback` to check whether
    /// `merge_env_bindings_into` already inserted a binding into the flat output env,
    /// without accidentally matching builtins in a parent env.
    pub fn get_own(&self, name: &str) -> Option<&TypeScheme> {
        self.slotted.get(name).or_else(|| self.extras.get(name))
    }

    pub fn get_type_alias(&self, name: &str) -> Option<&TypeAlias> {
        self.lookup_type_alias(name).map(|(alias, _)| alias)
    }

    #[allow(dead_code)]
    pub(crate) fn lookup(&self, name: &str) -> Option<&TypeScheme> {
        self.get(name)
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

    /// Insert a type (monomorphic scheme) into the slotted IndexMap.
    /// The slot index of this entry is `slotted.len() - 1` after insertion.
    /// If an entry with this name already exists in `slotted`, the value is updated
    /// in-place and the slot index is preserved (IndexMap semantics).
    pub fn insert(&mut self, name: String, ty: Type) {
        self.slotted.insert(name, TypeScheme::mono(ty));
    }

    /// Insert a TypeScheme into the slotted IndexMap.
    /// Use this for entries that correspond to static dict keys or fn params visible
    /// to the resolver — entries where `slot = get_index_of(name)` is meaningful.
    /// If the entry already exists, the value is updated in-place (slot preserved).
    pub fn insert_scheme(&mut self, name: String, scheme: TypeScheme) {
        self.slotted.insert(name, scheme);
    }

    /// Insert a TypeScheme into the extras HashMap (name-only, no slot).
    /// Use this for entries NOT assigned a slot by the resolver:
    /// - ADT constructor type information (from `inject_adt_constructor_schemes`)
    /// - Class method injections (from `pending_scheme_injections`)
    /// - Narrowing overrides
    /// - Builtin bindings
    ///
    /// These entries are visible via `get(name)` but are never reached via
    /// `get_type_at(level, slot)`.
    pub fn insert_scheme_named_only(&mut self, name: String, scheme: TypeScheme) {
        self.extras.insert(name, scheme);
    }

    pub fn insert_type_alias(&mut self, name: String, alias: TypeAlias) {
        self.type_aliases.insert(name, alias);
    }

    /// Look up a user-defined type constructor definition by name.
    pub fn lookup_tycon_def(
        &self,
        name: &str,
    ) -> Option<std::sync::Arc<crate::type_def::TyConDef>> {
        self.tycon_defs.get(name).cloned()
    }

    /// Register a TyConDef in the type environment (stub — TyCon defs are stored in InferState.tycon_env,
    /// not in TypeEnv; this method exists for call-site compatibility and is a no-op here).
    #[allow(unused_variables)]
    pub fn insert_tycon_def(
        &mut self,
        name: String,
        def: std::sync::Arc<crate::type_def::TyConDef>,
    ) {
        self.tycon_defs.insert(name, def);
    }

    /// Register alias type schemes (copy the scheme from canonical to alias names).
    pub fn alias_types(&mut self, pairs: &[(&str, &str)]) {
        for &(alias, canonical) in pairs {
            if let Some(scheme) = self.get(canonical).cloned() {
                self.insert_scheme_named_only(alias.to_string(), scheme);
            }
        }
    }

    /// Look up the qualified form of a constructor tag.
    /// E.g., "Ok" → "Result.Ok" if Result is in scope.
    /// Returns the qualified tag "TypeName.CtorName" if found, None otherwise.
    pub fn resolve_constructor_tag(&self, tag: &str) -> Option<String> {
        // Search own tycon_defs for a constructor matching the unqualified name
        for (tycon_name, def) in &self.tycon_defs {
            for (ctor_tag, _arity) in &def.constructors {
                // ctor_tag is fully qualified: "TypeName.CtorName"
                // Extract the unqualified part after the last dot
                if let Some(unqualified) = ctor_tag.rfind('.').map(|pos| &ctor_tag[pos + 1..]) {
                    if unqualified == tag {
                        return Some(ctor_tag.clone());
                    }
                }
                // Also accept exact match for constructors already in qualified form
                if ctor_tag == tag {
                    return Some(format!("{}.{}", tycon_name, tag));
                }
            }
        }
        // Walk parent chain
        if let Some(ref parent) = self.parent {
            return parent.resolve_constructor_tag(tag);
        }
        None
    }

    /// Collect all binding names visible from this environment (including parent scopes).
    ///
    /// Walks the scope chain and inserts every bound name into `names`. Used by
    /// `imports::merge_env_bindings_into` to enumerate what the prelude introduced.
    pub fn collect_all_names(&self, names: &mut std::collections::HashSet<String>) {
        for name in self.slotted.keys() {
            names.insert(name.clone());
        }
        for name in self.extras.keys() {
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
        for name in self.slotted.keys() {
            names.insert(name.clone());
        }
        for name in self.extras.keys() {
            names.insert(name.clone());
        }
    }

    /// Iterate over the slot-indexed scheme entries in THIS frame only (no parent walk).
    pub fn iter_slotted(&self) -> impl Iterator<Item = (&str, &TypeScheme)> {
        self.slotted.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate over the extras (name-only) scheme entries in THIS frame only (no parent walk).
    pub fn iter_extras(&self) -> impl Iterator<Item = (&str, &TypeScheme)> {
        self.extras.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Iterate over the class declarations in THIS frame only (no parent walk).
    pub fn own_classes(&self) -> impl Iterator<Item = &ClassDecl> {
        self.classes.values()
    }

    /// Iterate over the instance declarations in THIS frame only (no parent walk).
    pub fn own_instances(&self) -> impl Iterator<Item = (&str, &InstanceDecl)> {
        self.instances.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Return the parent environment frame, if any.
    ///
    /// Used by `imports::collect_names_above_baseline` to walk the frame chain up to
    /// the builtin baseline boundary.
    pub fn parent(&self) -> Option<&Arc<TypeEnv>> {
        self.parent.as_ref()
    }

    // ---- Class and instance registration ----

    /// Insert a class declaration into this frame, keyed by `decl.name`.
    /// If a class with the same name already exists in this frame, it is overwritten.
    pub fn insert_class(&mut self, decl: ClassDecl) {
        self.classes.insert(decl.name.clone(), decl);
    }

    /// Insert an instance declaration into this frame, keyed by `mangled_name`.
    /// The mangled name is the instance binding name used in the runtime env
    /// (e.g. `ɪɴꜱᴛᴀɴᴄᴇ⧼Addable Int Float Float⧽`). Idempotent: if an entry
    /// with this key already exists, it is overwritten.
    pub fn insert_instance(&mut self, mangled_name: String, decl: InstanceDecl) {
        self.instances.insert(mangled_name, decl);
    }

    /// Look up a class declaration by name, walking the parent chain (inner wins).
    /// Returns a clone so the caller is not restricted by borrow lifetimes.
    pub fn get_class(&self, name: &str) -> Option<ClassDecl> {
        if let Some(decl) = self.classes.get(name) {
            return Some(decl.clone());
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(decl) = env.classes.get(name) {
                return Some(decl.clone());
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Look up an instance declaration by mangled name, walking the parent chain (inner wins).
    pub fn get_instance(&self, mangled: &str) -> Option<InstanceDecl> {
        if let Some(decl) = self.instances.get(mangled) {
            return Some(decl.clone());
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(decl) = env.instances.get(mangled) {
                return Some(decl.clone());
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Collect all class declarations from the full parent chain, root-first.
    /// Later (inner) frames with the same class name overwrite earlier (outer) ones.
    /// Returns one `ClassDecl` per unique class name.
    pub fn all_classes(&self) -> Vec<ClassDecl> {
        // Collect into a BTreeMap to deduplicate by name (inner wins via overwrite).
        let mut map: std::collections::BTreeMap<String, ClassDecl> =
            std::collections::BTreeMap::new();
        self.collect_all_classes_into(&mut map);
        map.into_values().collect()
    }

    fn collect_all_classes_into(&self, map: &mut std::collections::BTreeMap<String, ClassDecl>) {
        // Walk parent chain first (root-to-leaf), then overwrite with self (inner wins).
        if let Some(ref parent) = self.parent {
            parent.collect_all_classes_into(map);
        }
        for (name, decl) in &self.classes {
            map.insert(name.clone(), decl.clone());
        }
    }

    /// Collect all instance declarations from the full parent chain.
    /// Returns `(mangled_name, InstanceDecl)` pairs. Inner frames overwrite outer for
    /// the same mangled key.
    pub fn all_instances(&self) -> Vec<(String, InstanceDecl)> {
        let mut map: std::collections::BTreeMap<String, InstanceDecl> =
            std::collections::BTreeMap::new();
        self.collect_all_instances_into(&mut map);
        map.into_iter().collect()
    }

    fn collect_all_instances_into(
        &self,
        map: &mut std::collections::BTreeMap<String, InstanceDecl>,
    ) {
        if let Some(ref parent) = self.parent {
            parent.collect_all_instances_into(map);
        }
        for (mangled, decl) in &self.instances {
            map.insert(mangled.clone(), decl.clone());
        }
    }

    /// Build a `ClassEnv` from this TypeEnv's class chain (root-first).
    /// Used to seed `state.env` with class declarations from a TypeEnv chain.
    pub fn build_class_env(&self) -> ClassEnv {
        let mut env = ClassEnv::new();
        for decl in self.all_classes() {
            env.insert(decl);
        }
        env
    }

    /// Build an `InstanceEnv` from this TypeEnv's instance chain (root-first).
    /// Used to seed `state.env` with instance declarations from a TypeEnv chain.
    pub fn build_instance_env(&self) -> InstanceEnv {
        let mut env = InstanceEnv::new();
        for (_mangled, decl) in self.all_instances() {
            let _ = env.insert(decl);
        }
        env
    }

    // ---- End class/instance registration ----

    /// Copy all bindings and type aliases from `other` into `self`.
    ///
    /// Copies only the own (non-parent) bindings and type aliases from `other`.
    /// Parent chains are not traversed. Existing entries in `self` with the same
    /// name are overwritten by entries from `other`.
    ///
    /// Used by `build_builtins_type_env()` to combine per-module type environments
    /// (core, datetime, net) into a single flat environment.
    pub fn merge(&mut self, other: TypeEnv) {
        for (name, scheme) in other.slotted {
            self.slotted.insert(name, scheme);
        }
        for (name, scheme) in other.extras {
            self.extras.insert(name, scheme);
        }
        for (name, alias) in other.type_aliases {
            self.type_aliases.insert(name, alias);
        }
        for (name, decl) in other.classes {
            self.classes.insert(name, decl);
        }
        for (mangled, decl) in other.instances {
            self.instances.insert(mangled, decl);
        }
    }
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

// Tests for type_error_note helper function deleted in T-1724.
// The function was removed as part of TypeError → TypeDiagnostic migration.
// TypeDiagnostic formatting will be implemented in T-1725.

#[cfg(test)]
mod help_suggestion_tests {
    use super::*;

    #[tokio::test]
    async fn test_resolve_instance_freshens_type_vars() {
        use crate::types::{InferState, InstanceDecl};
        use std::collections::HashMap;

        // Test that resolve_instance freshens type variables in the instance type
        // and applies the unification substitution to method types.

        let mut state = InferState::new();

        // Create an instance: Appendable [Seq b]
        // Method: append: [Fn@[Seq b] [[Seq b] [Seq b]]]
        let seq_b = Type::App(
            Box::new(Type::TyCon("Seq".to_string())),
            Box::new(Type::TypeVar("b".to_string(), 0)),
        );
        let instance = InstanceDecl {
            class_name: "Appendable".to_string(),
            instance_type: seq_b.clone(),
            det_positions: vec![], // Single-parameter class, no FDs
            method_types: {
                let mut methods = HashMap::new();
                methods.insert(
                    "append".to_string(),
                    Type::Function {
                        params: vec![(None, seq_b.clone()), (None, seq_b.clone())],
                        ret: Box::new(seq_b.clone()),
                        typed_variadics: vec![],
                        rest: None,
                        required_count: 2,
                    },
                );
                methods
            },
        };

        // Register the instance in state.env
        let mangled = format!(
            "ɪɴꜱᴛᴀɴᴄᴇ⧼{} {}⧽",
            instance.class_name, instance.instance_type
        );
        state
            .env
            .write()
            .unwrap()
            .insert_instance(mangled, instance.clone());
        state.invalidate_env_caches();

        // Resolve against Seq[Int]
        let target = Type::App(
            Box::new(Type::TyCon("Seq".to_string())),
            Box::new(Type::Int),
        );
        // Build a temporary InstanceEnv snapshot to avoid borrow checker conflict
        let inst_env = state.build_instance_env_snapshot().clone();
        let resolved = inst_env
            .resolve_instance("Appendable", &target, &mut state)
            .await;

        assert!(resolved.is_ok(), "resolve_instance should not error");
        let resolved = resolved.unwrap();
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
                Type::App(head, elem) if matches!(head.as_ref(), Type::TyCon(n) if n == "Seq") => {
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
                Type::App(head, elem) if matches!(head.as_ref(), Type::TyCon(n) if n == "Seq") => {
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
        let instantiated = instantiate_at_level(&original_ty, &mut state, &crate::rust_span!());

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
        use crate::ast::Span;
        use crate::error::DiagnosticLevel;
        use crate::type_class::{ClassDecl, Constraint};
        use std::collections::{HashMap, HashSet};
        use std::sync::Arc;

        // Build a minimal ClassDecl for "Castable" (params=[] follows Constraint::new_by_name pattern)
        let class = Arc::new(ClassDecl {
            name: "Castable".to_string(),
            params: vec![],
            superclasses: vec![],
            determines: vec![],
            resolver: None,
            resolver_injective: false,
            structural_discharge: crate::type_class::StructuralDischarge::None,
            method_signatures: vec![],
        });

        // Constraint with origin_name="cast" (as would be set by instantiate_scheme at a VarRef)
        let arg_span = Span {
            file: crate::rust_span!().file,
            start_line: 1,
            start_col: 11,
            end_line: 1,
            end_col: 15,
            name: None,
        };
        let constraint_with_origin = Constraint::Class {
            class: Arc::clone(&class),
            vars: vec![crate::type_class::ConstraintArg::Var("_t42".to_string())],
            origin_name: Some(Arc::from("cast")),
            origin_span: Some(arg_span.clone()),
        };

        // _t42 is NOT in the substitution map → not discharged → T013 fires
        let subst_snapshot: HashMap<String, Type> = HashMap::new();
        let source_names: HashMap<String, String> = HashMap::new();
        let mut diagnostics: Vec<crate::error::TypeDiagnostic> = Vec::new();
        let fallback_span = Span {
            file: crate::rust_span!().file,
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 6,
            name: None,
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
        assert_eq!(diag.kind, "ambiguous-constraint");
        assert_eq!(diag.level, DiagnosticLevel::Warn);
        // Message must cite the origin function, not the internal TypeVar name
        assert!(
            diag.message.contains("argument to `cast`"),
            "expected message to cite origin function 'cast'; got: {}",
            diag.message
        );
        assert!(
            diag.message.contains("Castable"),
            "expected message to cite constraint class 'Castable'; got: {}",
            diag.message
        );
        assert!(
            !diag.message.contains("_t42"),
            "expected message to omit internal TypeVar name '_t42'; got: {}",
            diag.message
        );
        // When origin_span is provided, the diagnostic span should be the argument span
        assert_eq!(
            diag.primary_span(),
            &arg_span,
            "expected diagnostic span to use origin_span (argument location)"
        );
    }

    // T013 Task 5: when origin_name is absent, the fallback message format is preserved.
    #[test]
    fn test_t013_fallback_message_format() {
        use crate::ast::Span;
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
            structural_discharge: crate::type_class::StructuralDischarge::None,
            method_signatures: vec![],
        });

        // Constraint WITHOUT origin info (annotation-driven, as in existing tests)
        let constraint_no_origin = Constraint::Class {
            class,
            vars: vec![crate::type_class::ConstraintArg::Var("_t7".to_string())],
            origin_name: None,
            origin_span: None,
        };

        let subst_snapshot: HashMap<String, Type> = HashMap::new();
        let source_names: HashMap<String, String> = HashMap::new();
        let mut diagnostics: Vec<crate::error::TypeDiagnostic> = Vec::new();
        let call_span = Span {
            file: crate::rust_span!().file,
            start_line: 1,
            start_col: 1,
            end_line: 1,
            end_col: 6,
            name: None,
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
        assert_eq!(diag.kind, "ambiguous-constraint");
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
            diag.primary_span(),
            &call_span,
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
            param_narrowings: Vec::new(),
        };

        let instantiated = instantiate_scheme(
            &scheme,
            state.level,
            &mut state,
            None,
            None,
            &crate::rust_span!(),
        );

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
        let type_vars_before = state.type_vars.len();

        let scheme = TypeScheme::mono(Type::Int);

        let result = instantiate_scheme(
            &scheme,
            state.level,
            &mut state,
            None,
            None,
            &crate::rust_span!(),
        );

        assert_eq!(
            result,
            Type::Int,
            "monomorphic scheme must return body unchanged"
        );
        assert_eq!(
            state.type_vars.len(),
            type_vars_before,
            "monomorphic instantiation must not create new type variables"
        );
    }
}
