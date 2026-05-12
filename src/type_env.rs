//! Type environment, instantiation, generalization, Display, type aliases,
//! class/instance environments, and type errors.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

use crate::ast::Span;

use super::*;

/// Instantiate a type by creating fresh type variables at level 0.
/// Call-site vars are created at level 0 and intentionally NOT registered in
/// `InferState.levels`. This means they are treated as level 0 = never generalize,
/// because `generalize()` only generalizes variables where `levels[var] > enclosing_level`
/// and absent variables default to 0. In contrast, `InferState::fresh_var()` always
/// registers at `state.level`, and `instantiate_at_level()` registers at the current
/// level for proper participation in generalization.
///
/// This function is test-only; production code uses `instantiate_at_level()`.
/// Returns both the instantiated type and the renaming substitution that was applied.
/// The substitution is unused by current callers but kept for testing/debugging purposes
/// (allows inspection of which type/row vars were renamed to which fresh vars).
#[cfg(test)]
pub fn instantiate(ty: &Type, counter: &mut u32) -> (Type, Substitution) {
    let mut type_vars = HashSet::new();
    let mut row_vars = HashSet::new(); // always empty under BAS
    ty.collect_all_vars(&mut type_vars, &mut row_vars);

    let mut renaming = Substitution::new();
    for var in type_vars {
        let fresh = format!("_t{counter}");
        *counter += 1;
        renaming.type_map.insert(var, Type::TypeVar(fresh, 0));
    }

    (renaming.apply(ty), renaming)
}

/// Instantiate a type by creating fresh type variables at the current level.
/// Used for CALL-POLY: when calling a polymorphic function, instantiate its type
/// at the current level to enable proper generalization (Kiselyov 2013).
///
/// Unlike `instantiate()`, this function registers the fresh variables in `state.levels`
/// so they participate in level-based generalization. Without this, fresh variables
/// default to level 0 and are permanently excluded from generalization by [U-VAR-LEVEL].
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
    let mut row_vars = Vec::new(); // always empty under BAS
    ty.collect_all_vars_vec(&mut type_vars, &mut row_vars);

    // Monomorphic fast-path: if no type vars, return ty directly (saves HashMap allocation)
    if type_vars.is_empty() {
        return ty.clone();
    }

    // Use with_capacity so the HashMap internal array is allocated exactly once,
    // avoiding a resize when the type var count is known upfront (CALL-POLY hot path).
    // Note: capacity hint may be larger than actual unique count if there are duplicates,
    // but this wastes at most a few slots and is cheaper than deduplicating first.
    let mut renaming = Substitution {
        type_map: HashMap::with_capacity(type_vars.len()),
    };
    for var in type_vars {
        // First-write-wins: skip if this var was already mapped (handles duplicates from the Vec).
        if !renaming.type_map.contains_key(&var) {
            let fresh_name = format!("_t{}", state.name_counter);
            state.name_counter = state.name_counter.saturating_add(1);
            state.levels.insert(fresh_name.clone(), state.level);
            renaming
                .type_map
                .insert(var, Type::TypeVar(fresh_name, state.level));
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
                Constraint::Class { class, var } => {
                    if let Some(fresh_var) = var_renaming.get(var) {
                        state.add_constraint(class.clone(), fresh_var.clone());
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
    let mut renaming = Substitution {
        type_map: HashMap::with_capacity(scheme.type_vars.len()),
    };
    for var in &scheme.type_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(var.clone(), fresh_name.clone());
        renaming
            .type_map
            .insert(var.clone(), Type::TypeVar(fresh_name.clone(), level));

        // Re-register label vars in kind_env with Kind::Label
        if scheme.label_vars.contains(var) {
            state.kind_env.insert(fresh_name, Kind::Label);
        }
    }

    // Copy constraints with renamed variables
    for constraint in &scheme.constraints {
        match constraint {
            Constraint::Class { class, var } => {
                if let Some(fresh_var) = var_renaming.get(var) {
                    state.add_constraint(class.clone(), fresh_var.clone());
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
            other != target && entails(class_env, &[other.clone()], target)
        })
    });
}

/// Generalize a type at a binding boundary by quantifying free type variables
/// whose level is strictly greater than the enclosing scope level.
/// Used for let-generalization: ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ
///
/// Defense-in-depth: applies the current substitution first, per Damas & Milner (1982).
/// Generalization must operate over the image of the substitution, not the raw type.
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme {
    generalize_with_doc(level, ty, state, None)
}

/// Generalize a type into a TypeScheme with optional documentation.
///
/// This is the core generalization function used by the type inference engine.
/// The `doc` parameter allows threading documentation strings from source annotations
/// into the TypeScheme for LSP hover display.
pub fn generalize_with_doc(
    level: u32,
    ty: &Type,
    state: &InferState,
    doc: Option<String>,
) -> TypeScheme {
    // Apply substitution first -- defense-in-depth per Damas & Milner (1982).
    // Generalization must operate over the image of the substitution.
    // Without this, a bound TypeVar would be generalized incorrectly.
    let ty = &state.subst.apply(ty);

    // Early exit for monomorphic types (common case: all-concrete config dicts)
    if !ty.has_inference_vars() {
        return TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            doc,
        };
    }

    let mut all_type_vars = Vec::new();
    let mut all_row_vars = Vec::new(); // always empty under BAS
    ty.collect_all_vars_vec(&mut all_type_vars, &mut all_row_vars);

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
        TypeScheme {
            type_vars: Vec::new(),
            constraints: Vec::new(),
            body: ty.clone(),
            label_vars: Vec::new(),
            doc,
        }
    } else {
        // Filter constraints: keep only those on generalized variables
        let generalizable_vars: HashSet<String> = generalizable_type_vars.iter().cloned().collect();

        let mut generalizable_constraints: Vec<Constraint> = state
            .constraints
            .iter()
            .filter(|c| match c {
                Constraint::Class { var, .. } => generalizable_vars.contains(var),
                Constraint::HasField {
                    label,
                    dict_var,
                    field_var,
                } => {
                    let label_ok = match label {
                        Label::Concrete(_) => true,
                        Label::Var(var_name) => generalizable_vars.contains(var_name),
                    };
                    label_ok
                        && generalizable_vars.contains(dict_var)
                        && generalizable_vars.contains(field_var)
                }
            })
            .cloned()
            .collect();

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
        Type::TypeVar(name, _) if name.starts_with("_t") => {
            if !seen.contains(name) {
                seen.push(name.clone());
            }
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

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::IntLiteral(n) => write!(f, "{n}"),
            Type::Float => write!(f, "Float"),
            Type::Str => write!(f, "String"),
            Type::StringLiteral(s) => write!(f, "\"{s}\""),
            Type::Bool => write!(f, "Bool"),
            Type::Bytes => write!(f, "Bytes"),
            Type::Number => write!(f, "Number"),
            Type::Unknown => write!(f, "_"),
            Type::Top => write!(f, "\u{22a4}"),
            Type::TypeVar(name, _level) => write!(f, "{name}"),
            Type::Record(row) => {
                write!(f, "[")?;
                // Sort field names for deterministic output (HashMap has no insertion order).
                let mut sorted_fields: Vec<(&String, &Type)> = row.fields.iter().collect();
                sorted_fields.sort_by_key(|(k, _)| k.as_str());
                for (i, (key, ty)) in sorted_fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{key}: {ty}")?;
                }
                write!(f, "]")
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                // Parenthesize nested function types in return position for clarity
                match **ret {
                    Type::Function { .. } => write!(f, "Fn@({ret}) [")?,
                    _ => write!(f, "Fn@{ret} [")?,
                }
                for (i, (name, p_ty)) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    // Parenthesize nested function types in parameter position
                    match p_ty {
                        Type::Function { .. } => {
                            if let Some(n) = name {
                                write!(f, "{n}: ({p_ty})")?
                            } else {
                                write!(f, "({p_ty})")?
                            }
                        }
                        _ => {
                            if let Some(n) = name {
                                write!(f, "{n}: {p_ty}")?
                            } else {
                                write!(f, "{p_ty}")?
                            }
                        }
                    }
                }
                write!(f, "]")
            }
            Type::Seq(elem) => write!(f, "Seq[{elem}]"),
            Type::Map(key, val) => write!(f, "Map[{key} {val}]"),
            Type::Proxy => write!(f, "Proxy"),
            Type::Error => write!(f, "<error>"),
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
            Type::DatagramHandle => write!(f, "DatagramHandle"),
            Type::Union(members) => {
                for (i, member) in members.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    // Parenthesize nested unions (shouldn't happen after normalization, but be safe)
                    match member {
                        Type::Union(_) => write!(f, "({member})")?,
                        _ => write!(f, "{member}")?,
                    }
                }
                Ok(())
            }
            Type::Intersection(members) => {
                for (i, member) in members.iter().enumerate() {
                    if i > 0 {
                        write!(f, " & ")?;
                    }
                    // Parenthesize nested intersections and unions for clarity
                    match member {
                        Type::Intersection(_) | Type::Union(_) => write!(f, "({member})")?,
                        _ => write!(f, "{member}")?,
                    }
                }
                Ok(())
            }
            Type::Negation(inner) => {
                // Parenthesize complex inner types for clarity
                match **inner {
                    Type::Union(_) | Type::Intersection(_) | Type::Negation(_) => {
                        write!(f, "~({inner})")
                    }
                    _ => write!(f, "~{inner}"),
                }
            }
            Type::Never => write!(f, "\u{22a5}"), // ⊥ symbol
            Type::App(func, arg) => write!(f, "[{func} {arg}]"),
            Type::Operator(name) => write!(f, "{name}"),
        }
    }
}

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

/// Type class declaration (Wadler & Blott 1989)
/// Example: `[class [Equatable a] eq: [Fn@Bool [a a]]]`
#[derive(Debug, Clone)]
pub struct ClassDecl {
    /// Class name (e.g., "Equatable")
    pub name: String,
    /// Type parameters with their kinds (e.g., [("a", Kind::Type)])
    #[allow(dead_code)]
    // Written during registration, read during constraint solving (future work)
    pub params: Vec<(String, Kind)>,
    /// Superclass constraints as (class_name, param_name) tuples.
    /// Example: ("Functor", "f") means this class extends Functor with parameter f.
    #[allow(dead_code)]
    // Written during registration, read during constraint solving (future work)
    pub superclasses: Vec<(String, String)>,
    /// Method signatures: method_name -> type scheme
    #[allow(dead_code)]
    // Written during registration, read during method type checking (future work)
    pub methods: HashMap<String, TypeScheme>,
}

/// Type class instance declaration
/// Example: `[instance [Equatable Int] eq: [fn [x y] [= x y]]]`
#[derive(Debug, Clone)]
pub struct InstanceDecl {
    /// Class name (e.g., "Equatable")
    pub class_name: String,
    /// Instance type (e.g., Int, or type constructor application)
    pub instance_type: Type,
    /// Method implementations: method_name -> inferred type
    /// (The actual dictionary value is stored in eval::ClassDictionary)
    #[allow(dead_code)]
    // Written during registration, read during dictionary construction (future work)
    pub method_types: HashMap<String, Type>,
}

/// Class environment: global registry of type class declarations
/// Scoped like TypeEnv (supports shadowing in nested scopes)
#[derive(Debug, Clone)]
pub struct ClassEnv {
    classes: HashMap<String, ClassDecl>,
    #[allow(dead_code)] // Scaffolding for scoped class environments (future work)
    parent: Option<Rc<ClassEnv>>,
}

impl ClassEnv {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for scoped class environments (future work)
    pub fn with_parent(parent: &Rc<ClassEnv>) -> Self {
        Self {
            classes: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    /// Look up a class declaration by name, checking parent scopes if necessary.
    pub fn get(&self, name: &str) -> Option<&ClassDecl> {
        if let Some(class) = self.classes.get(name) {
            return Some(class);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(class) = env.classes.get(name) {
                return Some(class);
            }
            current = env.parent.as_deref();
        }
        None
    }

    pub fn insert(&mut self, class_decl: ClassDecl) {
        self.classes.insert(class_decl.name.clone(), class_decl);
    }
}

impl Default for ClassEnv {
    fn default() -> Self {
        Self::new()
    }
}

/// Instance environment: global registry of type class instances
/// Key is (class_name, instance_type_string) to allow fast lookup
#[derive(Debug, Clone)]
pub struct InstanceEnv {
    instances: HashMap<(String, String), InstanceDecl>,
    #[allow(dead_code)] // Scaffolding for scoped instance environments (future work)
    parent: Option<Rc<InstanceEnv>>,
}

impl InstanceEnv {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for scoped instance environments (future work)
    pub fn with_parent(parent: &Rc<InstanceEnv>) -> Self {
        Self {
            instances: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    /// Look up an instance by class name and type.
    /// Returns the instance declaration if found.
    #[allow(dead_code)] // Instance lookup used during dictionary construction (future work)
    pub fn get(&self, class_name: &str, ty: &Type) -> Option<&InstanceDecl> {
        let key = (class_name.to_string(), ty.to_string());
        if let Some(inst) = self.instances.get(&key) {
            return Some(inst);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(inst) = env.instances.get(&key) {
                return Some(inst);
            }
            current = env.parent.as_deref();
        }
        None
    }

    /// Insert an instance. Returns an error if an overlapping instance already exists.
    pub fn insert(&mut self, inst: InstanceDecl) -> Result<(), String> {
        let key = (inst.class_name.clone(), inst.instance_type.to_string());
        if self.instances.contains_key(&key) {
            return Err(format!(
                "overlapping instance for {} {}",
                inst.class_name, inst.instance_type
            ));
        }
        self.instances.insert(key, inst);
        Ok(())
    }

    /// Resolve an instance for the given class and target type.
    /// Attempts to unify each registered instance's head type with the target type.
    /// Returns the matching instance declaration if found, or None if no match.
    ///
    /// This is a simple unification-based resolution: it tries each instance in order
    /// and returns the first that unifies with the target type. More sophisticated
    /// resolution (with backtracking, overlapping instance detection, or instance
    /// selection based on specificity) is deferred to future work.
    pub fn resolve_instance(
        &self,
        class_name: &str,
        target_type: &Type,
        state: &mut InferState,
    ) -> Option<&InstanceDecl> {
        // Collect all instances for this class
        let mut candidates = Vec::new();

        // Check local instances
        for ((cname, _), inst) in &self.instances {
            if cname == class_name {
                candidates.push(inst);
            }
        }

        // Check parent instances
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            for ((cname, _), inst) in &env.instances {
                if cname == class_name {
                    candidates.push(inst);
                }
            }
            current = env.parent.as_deref();
        }

        // Try to unify with each candidate
        for inst in candidates {
            // Create a fresh substitution for this unification attempt
            let mut temp_subst = state.subst.clone();

            // Attempt unification
            if unify(
                &inst.instance_type,
                target_type,
                &mut temp_subst,
                state,
                Span::origin(),
            )
            .is_ok()
            {
                return Some(inst);
            }
        }

        None
    }
}

impl Default for InstanceEnv {
    fn default() -> Self {
        Self::new()
    }
}

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

        // Arithmetic: Numeric a => a -> a -> a
        // Constrained polymorphic type variables allow precise typing of overloaded operations.
        for name in ["+", "-", "*"] {
            env.insert_scheme(
                name.to_string(),
                TypeScheme {
                    type_vars: vec!["a".to_string()],
                    constraints: vec![Constraint::new("Numeric", "a")],
                    body: Type::Function {
                        params: vec![
                            (None, Type::TypeVar("a".to_string(), 0)),
                            (None, Type::TypeVar("a".to_string(), 0)),
                        ],
                        ret: Box::new(Type::TypeVar("a".to_string(), 0)),
                        variadic: false,
                    },
                    label_vars: vec![],
                    doc: None,
                },
            );
        }

        // Division: Numeric a => a -> a -> Float
        env.insert_scheme(
            "/".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new("Numeric", "a")],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("a".to_string(), 0)),
                    ],
                    ret: Box::new(Type::Float),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
            },
        );

        // Equality: Equatable a => a -> a -> Bool
        env.insert_scheme(
            "=".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new("Equatable", "a")],
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
            },
        );

        // Less-than: Comparable a => a -> a -> Bool
        env.insert_scheme(
            "<".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                constraints: vec![Constraint::new("Comparable", "a")],
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
                constraints: vec![Constraint::new("Showable", "a")],
                body: Type::Function {
                    params: vec![(None, Type::TypeVar("a".to_string(), 0))],
                    ret: Box::new(Type::Str),
                    variadic: true,
                },
                label_vars: vec![],
                doc: None,
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
        for name in ["upper", "lower", "trim"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Str)],
                    ret: Box::new(Type::Str),
                    variadic: false,
                },
            );
        }

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
            "eval".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "force".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Top),
                variadic: false,
            },
        );
        env.insert(
            "error".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Never),
                variadic: false,
            },
        );
        // try: takes 1 arg — a zero-argument function. Returns Ok(v) or Err(String).
        // Runtime (builtins_meta.rs:builtin_try) enforces exactly 1 arg.
        //
        // Return type is Top rather than a structural union `{ok:T}|{err:Str}` because:
        // 1. The runtime now returns nominal Value::Variant { tag: "Ok"/"Err" }, not a struct dict.
        // 2. A structural union would cause T004 "non-exhaustive match" when user code matches
        //    on constructor patterns `[Ok v]` / `[Err msg]` — the coverage checker would see
        //    DictKey("ok")/DictKey("err") in the sig but Variant("Ok")/Variant("Err") in arms.
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
        env.insert(
            "until".to_string(),
            Type::Function {
                params: vec![
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Unknown)],
                            ret: Box::new(Type::Bool),
                            variadic: false,
                        },
                    ),
                    (
                        None,
                        Type::Function {
                            params: vec![(None, Type::Unknown)],
                            ret: Box::new(Type::Unknown),
                            variadic: false,
                        },
                    ),
                    (None, Type::Unknown),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );

        // Type introspection
        env.insert(
            "type-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "llt-repr".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "tag-of".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "variant".to_string(),
            Type::Function {
                params: vec![(None, Type::Str)],
                ret: Box::new(Type::Unknown), // Returns a Variant, but we don't have Type::Variant yet
                variadic: false,
            },
        );
        env.insert(
            "int?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "float?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "num?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "str?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bool?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bytes?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "null?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "dict?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "fn?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "record?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "map?".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

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
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        env.insert(
            "slurp".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)],
                // Returns Str for text handles ("r"); Bytes for binary handles ("rb")
                ret: Box::new(Type::normalize_union(vec![Type::Str, Type::Bytes])),
                variadic: false,
            },
        );
        env.insert(
            "lines".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)],
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
                ret: Box::new(Type::Unknown), // returns dict with cap and revoke fields
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
                    (None, Type::Unknown), // NetCap or DirCap (UnixStream/UnixDatagram)
                    (None, Type::Unknown), // Transport variant (Tcp, Udp, UnixStream, etc.)
                    (None, Type::Str),     // host
                    (None, Type::Int),     // port
                ],
                ret: Box::new(Type::Unknown), // Handle or DatagramHandle depending on transport
                variadic: false,
            },
        );
        // Datagram socket builtins
        env.insert(
            "send-datagram".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::DatagramHandle), // socket
                    (None, Type::Unknown),        // String or Bytes
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
                ret: Box::new(Type::Unknown), // Dict {data: Bytes}
                variadic: false,
            },
        );
        // TLS builtins
        env.insert(
            "tls-layer".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Handle),
                    (None, Type::Str),
                    (None, Type::Unknown), // opts dict
                ],
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        env.insert(
            "tls-peer-cert".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)],
                ret: Box::new(Type::Unknown), // Returns Dict with subject, issuer, sans, etc.
                variadic: false,
            },
        );
        env.insert(
            "spki-pin".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown), (None, Type::Bytes)], // HashAlgorithm variant, Bytes fingerprint
                ret: Box::new(Type::Unknown), // Returns Dict {algorithm, fingerprint}
                variadic: false,
            },
        );
        env.insert(
            "http-get".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown), (None, Type::Str)], // HttpConn, path String
                ret: Box::new(Type::Unknown), // Returns Dict {status, headers, body}
                variadic: false,
            },
        );
        env.insert(
            "socks5-connect".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Str), (None, Type::Int)], // Handle, host, port
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        env.insert(
            "proxy-connect".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Str), (None, Type::Int)], // Handle, host, port
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        // HTTP-sessions stubs — return Unknown until full implementation lands
        env.insert(
            "quic-session".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::NetCap),  // cap
                    (None, Type::Str),     // host
                    (None, Type::Int),     // port
                    (None, Type::Unknown), // opts dict
                ],
                ret: Box::new(Type::QuicSession),
                variadic: false,
            },
        );
        env.insert(
            "quic-open-stream".to_string(),
            Type::Function {
                params: vec![(None, Type::QuicSession)],
                ret: Box::new(Type::Handle), // Returns a bidirectional stream Handle
                variadic: false,
            },
        );
        env.insert(
            "quic-open-datagram".to_string(),
            Type::Function {
                params: vec![(None, Type::QuicSession)],
                ret: Box::new(Type::Unknown), // Returns a datagram channel
                variadic: false,
            },
        );
        env.insert(
            "http2-session".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::NetCap),  // capability
                    (None, Type::Str),     // base_url (scheme://host:port)
                    (None, Type::Unknown), // opts dict
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
                    (None, Type::Unknown),     // opts dict
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
                    (None, Type::Str),     // method
                    (None, Type::Str),     // path
                    (None, Type::Unknown), // headers dict
                    (None, Type::Unknown), // body (Bytes or Null)
                ],
                ret: Box::new(Type::Unknown), // Returns Dict {status, headers, body}
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
                ret: Box::new(Type::Unknown), // Returns Dict {rtt_ms, success}
                variadic: false,
            },
        );
        env.insert(
            "cap-data".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Str)],
                ret: Box::new(Type::Unknown), // Returns the cap value (can be any type)
                variadic: false,
            },
        );
        env.insert(
            "has-cap?".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Str)],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "write-handle".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Unknown)], // Handle/WriteHandle, String or Bytes
                ret: Box::new(Type::Handle),                               // Returns WriteHandle
                variadic: false,
            },
        );
        env.insert(
            "flush".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)], // WriteHandle
                ret: Box::new(Type::Handle),        // Returns WriteHandle
                variadic: false,
            },
        );
        env.insert(
            "close".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)], // WriteHandle
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                })),
                variadic: false,
            },
        );
        env.insert(
            "seek".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle), (None, Type::Int)], // Handle, byte offset
                ret: Box::new(Type::Handle), // Returns Handle for chaining
                variadic: false,
            },
        );
        env.insert(
            "seek-end".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)], // Handle
                ret: Box::new(Type::Handle),        // Returns Handle for chaining
                variadic: false,
            },
        );
        env.insert(
            "position".to_string(),
            Type::Function {
                params: vec![(None, Type::Handle)], // Handle
                ret: Box::new(Type::Int),           // Current byte offset
                variadic: false,
            },
        );
        env.insert(
            "list-dir".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Unknown), // Returns Seq of metadata Dicts
                variadic: false,
            },
        );
        env.insert(
            "stat".to_string(),
            Type::Function {
                params: vec![(None, Type::DirCap), (None, Type::Str)],
                ret: Box::new(Type::Unknown), // Returns metadata Dict
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
            "remove".to_string(),
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
            "copy".to_string(),
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
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // include: accepts 2–3 positional args (runtime: builtins_meta.rs:builtin_include).
        //   [include $cap "path"]          — 2 args: DirCap + path
        //   [include $cap "path" "hash"]   — 3 args: DirCap + path + hash
        // First arg is Unknown (DirCap); variadic covers 2- and 3-arg forms.
        env.insert(
            "include".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Unknown),
                variadic: true,
            },
        );

        // Sequences: primitives (registered as builtin-NAME; prelude exports unwrapped names)
        env.insert(
            "builtin-seq".to_string(),
            Type::Function {
                params: vec![(None, Type::Top), (None, Type::Top)],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
            },
        );
        env.insert(
            "builtin-head".to_string(),
            Type::Function {
                params: vec![(None, Type::Seq(Box::new(Type::Unknown)))],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "builtin-tail".to_string(),
            Type::Function {
                params: vec![(None, Type::Seq(Box::new(Type::Unknown)))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "builtin-collect".to_string(),
            Type::Function {
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
                params: vec![(None, Type::Unknown)],
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
        env.insert(
            "builtin-repeat".to_string(),
            Type::Function {
                params: vec![(None, Type::Top)],
                ret: Box::new(Type::Seq(Box::new(Type::Top))),
                variadic: false,
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
        // Note: Mappable constraint requires higher-kinded types (Phase 3 / D1 scope).
        // For now, these remain typed as Unknown.
        env.insert(
            "map".to_string(),
            Type::Function {
                params: vec![
                    (
                        Some("fn_".to_string()),
                        Type::Function {
                            params: vec![(None, Type::Unknown)],
                            ret: Box::new(Type::Unknown),
                            variadic: false,
                        },
                    ),
                    (Some("seq".to_string()), Type::Unknown),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "filter".to_string(),
            Type::Function {
                params: vec![
                    (
                        Some("pred".to_string()),
                        Type::Function {
                            params: vec![(None, Type::Unknown)],
                            ret: Box::new(Type::Bool),
                            variadic: false,
                        },
                    ),
                    (Some("seq".to_string()), Type::Unknown),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
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
        env.insert(
            "reduce".to_string(),
            Type::Function {
                params: vec![
                    (
                        Some("fn_".to_string()),
                        Type::Function {
                            params: vec![(None, Type::Unknown), (None, Type::Unknown)],
                            ret: Box::new(Type::Unknown),
                            variadic: false,
                        },
                    ),
                    (Some("init".to_string()), Type::Unknown),
                    (Some("seq".to_string()), Type::Seq(Box::new(Type::Unknown))),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "builtin-join".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Str),
                    (None, Type::Seq(Box::new(Type::Unknown))),
                ],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        // builtin-concat: Appendable a, Appendable b => a -> b -> Unknown
        // Each argument must be Appendable (Record/Seq), but both args need not be the
        // same concrete type (e.g. two dicts with different field types are both Appendable).
        // Return type is Unknown because the output shape depends on runtime values.
        // This causes a type warning when concat is called with a non-Appendable (e.g. Int).
        env.insert_scheme(
            "builtin-concat".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string(), "b".to_string()],
                constraints: vec![
                    Constraint::new("Appendable", "a"),
                    Constraint::new("Appendable", "b"),
                ],
                body: Type::Function {
                    params: vec![
                        (None, Type::TypeVar("a".to_string(), 0)),
                        (None, Type::TypeVar("b".to_string(), 0)),
                    ],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
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
        // builtin-cons: Any -> Dict -> Dict (prepends element, reindexes)
        env.insert(
            "builtin-cons".to_string(),
            Type::Function {
                params: vec![
                    (None, Type::Unknown),
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
        // builtin-sort: Dict -> Dict (natural ordering, O(n log n))
        env.insert(
            "builtin-sort".to_string(),
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

        // Proxy
        env.insert(
            "proxy".to_string(),
            Type::Function {
                params: vec![(
                    None,
                    Type::Function {
                        params: vec![(None, Type::Str)],
                        ret: Box::new(Type::Unknown),
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
                body: Type::Handle,
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
            "DatagramHandle".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::DatagramHandle,
            },
        );

        // builtin-get: registered directly. 'get' is a prelude wrapper (not a Rust builtin
        // type), so it is absent from this env when the alias loop below runs. Registering
        // builtin-get here gives the type checker enough information to avoid false
        // "undefined variable" errors in stdlib/prelude.llt.
        env.insert_scheme(
            "builtin-get".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    params: vec![(None, Type::Unknown), (None, Type::Unknown)],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                },
                label_vars: vec![],
                doc: None,
            },
        );

        // get?: registered directly. It is a Rust builtin (builtin_get_optional) that returns
        // the value at the key or Null (empty dict) if missing. Conservative type is
        // Unknown → Unknown → Union(Unknown, Null). The type checker special-cases get? for
        // Map and Record args to produce precise Union(V|Null) return types.
        env.insert_scheme(
            "get?".to_string(),
            TypeScheme {
                type_vars: vec![],
                constraints: vec![],
                body: Type::Function {
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
                ret: Box::new(Type::Unknown), // Returns Dict with year/month/day/hour/minute/second
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
                ret: Box::new(Type::Unknown), // Returns Dict with year/month/day/hour/minute/second/offset-seconds/tz-name
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
        env.insert(
            "builtin-first".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        // builtin-last: Dict|String|Bytes -> Any (returns last element, char, or byte-as-Int)
        env.insert(
            "builtin-last".to_string(),
            Type::Function {
                params: vec![(None, Type::Unknown)],
                ret: Box::new(Type::Unknown),
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

        // Transport: type alias for network transport variants (Tcp, Udp, UnixStream, UnixDatagram, NamedPipe, Icmp).
        // Represented as a union of string literals until Type::Variant exists.
        env.insert_type_alias(
            "Transport".to_string(),
            TypeAlias {
                params: vec![],
                body: Type::normalize_union(vec![
                    Type::StringLiteral("Tcp".to_string()),
                    Type::StringLiteral("Udp".to_string()),
                    Type::StringLiteral("UnixStream".to_string()),
                    Type::StringLiteral("UnixDatagram".to_string()),
                    Type::StringLiteral("NamedPipe".to_string()),
                    Type::StringLiteral("Icmp".to_string()),
                ]),
            },
        );

        // Transport variant constants: Tcp, Udp, UnixStream, UnixDatagram, NamedPipe, Icmp.
        // Runtime builtins.rs inserts these as Value::Variant { tag, payload: None }.
        // The type system has no Type::Variant yet, so we use Unknown to allow passing
        // these values to tls-connect / connect without a type error in --strict mode.
        for tag in [
            "Tcp",
            "Udp",
            "UnixStream",
            "UnixDatagram",
            "NamedPipe",
            "Icmp",
        ] {
            env.insert(tag.to_string(), Type::Unknown);
        }

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

        // builtin-* aliases: same types as canonical counterparts.
        // Used by stdlib/prelude to call builtins when canonical names may be shadowed.
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
        ] {
            if let Some(scheme) = env.get(canonical).cloned() {
                env.insert_scheme(alias.to_string(), scheme);
            }
        }

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
        // These have complex polymorphic types (lazy sequence transformers with callback functions),
        // so we register them as Unknown to avoid false "undefined variable" warnings in LSP.
        // Their runtime types are enforced by the builtin implementations in src/builtins.rs.
        for name in ["each", "each-key", "each-kv"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![(None, Type::Unknown)],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                },
            );
        }

        // Type constructors
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
}

impl TypeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
            notes: Vec::new(),
        }
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

    /// Returns the stable type error code for this error, based on message classification.
    ///
    /// Codes are parallel to the runtime E0xx codes:
    /// - T001: arity mismatch (wrong number of arguments at call site)
    /// - T002: undefined variable or undefined type
    /// - T003: cannot unify / type mismatch / field not found / not a function / not a record
    /// - T004: type assert failure (annotation-site mismatch)
    /// - T091: kind mismatch (expected `* → *`, got concrete type, etc.)
    /// - T000: other type errors not covered above
    pub fn code(&self) -> &'static str {
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
            lines.push("  = help: if this name is defined later in the document, group definitions using a function scope: [call [fn [] ...]]".to_string());
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
}
