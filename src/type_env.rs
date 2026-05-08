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
    let mut row_vars = HashSet::new();
    ty.collect_all_vars(&mut type_vars, &mut row_vars);

    let mut renaming = Substitution::new();
    for var in type_vars {
        let fresh = format!("_t{counter}");
        *counter += 1;
        renaming.type_map.insert(var, Type::TypeVar(fresh, 0));
    }

    for var in row_vars {
        let fresh = format!("_t{counter}");
        *counter += 1;
        renaming.row_map.insert(
            var,
            Row {
                fields: HashMap::new(),
                tail: RowTail::RowVar(fresh, 0),
            },
        );
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
pub fn instantiate_at_level(ty: &Type, state: &mut InferState) -> Type {
    // Use Vec instead of HashSet to avoid hash computation overhead for small types.
    // Deduplication is handled by the contains_key guard below: only the first occurrence
    // of each type/row var generates a fresh variable. Subsequent occurrences are skipped.
    let mut type_vars = Vec::new();
    let mut row_vars = Vec::new();
    ty.collect_all_vars_vec(&mut type_vars, &mut row_vars);

    // Monomorphic fast-path: if no type/row vars, return ty directly (saves 2 HashMap allocations)
    if type_vars.is_empty() && row_vars.is_empty() {
        return ty.clone();
    }

    // Use with_capacity so the HashMap internal arrays are allocated exactly once,
    // avoiding a resize when the type/row var counts are known upfront (CALL-POLY hot path).
    // Note: capacity hint may be larger than actual unique count if there are duplicates,
    // but this wastes at most a few slots and is cheaper than deduplicating first.
    let mut renaming = Substitution {
        type_map: HashMap::with_capacity(type_vars.len()),
        row_map: HashMap::with_capacity(row_vars.len()),
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

    for var in row_vars {
        if !renaming.row_map.contains_key(&var) {
            let fresh_name = format!("_t{}", state.name_counter);
            state.name_counter = state.name_counter.saturating_add(1);
            state.levels.insert(fresh_name.clone(), state.level);
            renaming.row_map.insert(
                var,
                Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar(fresh_name, state.level),
                },
            );
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
                .map(|p| rename_single_type_var(p, old_name, fresh_name, level))
                .collect(),
            ret: Box::new(rename_single_type_var(ret, old_name, fresh_name, level)),
            variadic: *variadic,
        },
        Type::Seq(elem) => Type::Seq(Box::new(rename_single_type_var(
            elem, old_name, fresh_name, level,
        ))),
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
pub fn instantiate_scheme(scheme: &TypeScheme, level: u32, state: &mut InferState) -> Type {
    if scheme.type_vars.is_empty() && scheme.row_vars.is_empty() {
        // Monomorphic scheme: return body directly
        return scheme.body.clone();
    }

    // Build variable renaming map (old names -> fresh names)
    let mut var_renaming: HashMap<String, String> = HashMap::new();

    // Fast path: single type variable, no row variables -- avoid building Substitution
    // (2 HashMaps) and the apply() HashSet pair. Inline rename is allocation-free
    // aside from the string format for the fresh name.
    if scheme.type_vars.len() == 1 && scheme.row_vars.is_empty() {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(scheme.type_vars[0].clone(), fresh_name.clone());

        // Copy constraints with renamed variables
        for constraint in &scheme.constraints {
            if let Some(fresh_var) = var_renaming.get(&constraint.var) {
                state.add_constraint(constraint.class.clone(), fresh_var.clone());
            }
        }

        return rename_single_type_var(&scheme.body, &scheme.type_vars[0], &fresh_name, level);
    }

    // General path: multiple variables or row variables -- build a full Substitution.
    // Create fresh type variables at the specified level for each quantified var
    let mut renaming = Substitution {
        type_map: HashMap::with_capacity(scheme.type_vars.len()),
        row_map: HashMap::with_capacity(scheme.row_vars.len()),
    };
    for var in &scheme.type_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(var.clone(), fresh_name.clone());
        renaming
            .type_map
            .insert(var.clone(), Type::TypeVar(fresh_name, level));
    }

    // Create fresh row variables -- row vars bind to Row, not Type
    // Row variables and type variables share the same naming counter (`_t{n}`)
    for var in &scheme.row_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        var_renaming.insert(var.clone(), fresh_name.clone());
        renaming.row_map.insert(
            var.clone(),
            Row {
                fields: HashMap::new(),
                tail: RowTail::RowVar(fresh_name, level),
            },
        );
    }

    // Copy constraints with renamed variables
    for constraint in &scheme.constraints {
        if let Some(fresh_var) = var_renaming.get(&constraint.var) {
            state.add_constraint(constraint.class.clone(), fresh_var.clone());
        }
    }

    renaming.apply(&scheme.body)
}

/// Generalize a type at a binding boundary by quantifying free type variables
/// whose level is strictly greater than the enclosing scope level.
/// Used for let-generalization: ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ
///
/// Defense-in-depth: applies the current substitution first, per Damas & Milner (1982).
/// Generalization must operate over the image of the substitution, not the raw type.
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme {
    // Apply substitution first -- defense-in-depth per Damas & Milner (1982).
    // Generalization must operate over the image of the substitution.
    // Without this, a bound TypeVar would be generalized incorrectly.
    let ty = &state.subst.apply(ty);

    // Early exit for monomorphic types (common case: all-concrete config dicts)
    if !ty.has_inference_vars() {
        return TypeScheme::mono(ty.clone());
    }

    let mut all_type_vars = Vec::new();
    let mut all_row_vars = Vec::new();
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

    seen.clear();
    let generalizable_row_vars: Vec<String> = all_row_vars
        .into_iter()
        .filter(|var| {
            let var_level = state.levels.get(var).copied().unwrap_or(0);
            let is_generalizable = var_level > level;
            is_generalizable && seen.insert(var.clone())
        })
        .collect();

    if generalizable_type_vars.is_empty() && generalizable_row_vars.is_empty() {
        TypeScheme::mono(ty.clone())
    } else {
        // Filter constraints: keep only those on generalized variables
        let generalizable_vars: HashSet<String> = generalizable_type_vars
            .iter()
            .chain(generalizable_row_vars.iter())
            .cloned()
            .collect();

        let generalizable_constraints: Vec<Constraint> = state
            .constraints
            .iter()
            .filter(|c| generalizable_vars.contains(&c.var))
            .cloned()
            .collect();

        TypeScheme {
            type_vars: generalizable_type_vars,
            row_vars: generalizable_row_vars,
            constraints: generalizable_constraints,
            body: ty.clone(),
        }
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
            Type::Number => write!(f, "Number"),
            Type::Unknown => write!(f, "?"),
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
                match &row.tail {
                    RowTail::Empty => {}
                    RowTail::RowVar(name, _level) => {
                        if !row.fields.is_empty() {
                            write!(f, " ")?;
                        }
                        // Hide generated names (starting with _) -- display as bare "..."
                        if name.starts_with('_') {
                            write!(f, "...")?;
                        } else {
                            write!(f, "...{name}")?;
                        }
                    }
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
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    // Parenthesize nested function types in parameter position
                    match p {
                        Type::Function { .. } => write!(f, "({p})")?,
                        _ => write!(f, "{p}")?,
                    }
                }
                write!(f, "]")
            }
            Type::Seq(elem) => write!(f, "Seq[{elem}]"),
            Type::Proxy => write!(f, "Proxy"),
            Type::Error => write!(f, "<error>"),
            Type::DirCap => write!(f, "DirCap"),
            Type::NetCap => write!(f, "NetCap"),
            Type::Handle => write!(f, "Handle"),
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
#[allow(dead_code)] // Scaffolding for future type class implementation
pub struct ClassDecl {
    /// Class name (e.g., "Equatable")
    pub name: String,
    /// Type parameters with their kinds (e.g., [("a", Kind::Type)])
    pub params: Vec<(String, Kind)>,
    /// Superclass constraints (e.g., ["Ord"])
    pub superclasses: Vec<String>,
    /// Method signatures: method_name -> type scheme
    pub methods: HashMap<String, TypeScheme>,
}

/// Type class instance declaration
/// Example: `[instance [Equatable Int] eq: [fn [x y] [= x y]]]`
#[derive(Debug, Clone)]
#[allow(dead_code)] // Scaffolding for future type class implementation
pub struct InstanceDecl {
    /// Class name (e.g., "Equatable")
    pub class_name: String,
    /// Instance type (e.g., Int, or type constructor application)
    pub instance_type: Type,
    /// Method implementations: method_name -> inferred type
    /// (The actual dictionary value is stored in eval::ClassDictionary)
    pub method_types: HashMap<String, Type>,
}

/// Class environment: global registry of type class declarations
/// Scoped like TypeEnv (supports shadowing in nested scopes)
#[derive(Debug, Clone)]
#[allow(dead_code)] // Scaffolding for future type class implementation
pub struct ClassEnv {
    classes: HashMap<String, ClassDecl>,
    parent: Option<Rc<ClassEnv>>,
}

impl ClassEnv {
    pub fn new() -> Self {
        Self {
            classes: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for future type class implementation
    pub fn with_parent(parent: &Rc<ClassEnv>) -> Self {
        Self {
            classes: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    #[allow(dead_code)] // Scaffolding for future type class implementation
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

    #[allow(dead_code)] // Scaffolding for future type class implementation
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
#[allow(dead_code)] // Scaffolding for future type class implementation
pub struct InstanceEnv {
    instances: HashMap<(String, String), InstanceDecl>,
    parent: Option<Rc<InstanceEnv>>,
}

impl InstanceEnv {
    pub fn new() -> Self {
        Self {
            instances: HashMap::new(),
            parent: None,
        }
    }

    #[allow(dead_code)] // Scaffolding for future type class implementation
    pub fn with_parent(parent: &Rc<InstanceEnv>) -> Self {
        Self {
            instances: HashMap::new(),
            parent: Some(Rc::clone(parent)),
        }
    }

    /// Look up an instance by class name and type.
    /// Returns the instance declaration if found.
    #[allow(dead_code)] // Scaffolding for future type class implementation
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
    #[allow(dead_code)] // Scaffolding for future type class implementation
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
    /// **Coverage:** All 57 builtins from `standard_builtins()` (src/builtins.rs)
    pub fn with_builtins() -> Self {
        let mut env = Self::new();

        // Arithmetic: Numeric a => a -> a -> a
        // Constrained polymorphic type variables allow precise typing of overloaded operations.
        for name in ["+", "-", "*"] {
            env.insert_scheme(
                name.to_string(),
                TypeScheme {
                    type_vars: vec!["a".to_string()],
                    row_vars: vec![],
                    constraints: vec![Constraint::new("Numeric", "a")],
                    body: Type::Function {
                        params: vec![
                            Type::TypeVar("a".to_string(), 0),
                            Type::TypeVar("a".to_string(), 0),
                        ],
                        ret: Box::new(Type::TypeVar("a".to_string(), 0)),
                        variadic: false,
                    },
                },
            );
        }

        // Division: Numeric a => a -> a -> Float
        env.insert_scheme(
            "/".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                row_vars: vec![],
                constraints: vec![Constraint::new("Numeric", "a")],
                body: Type::Function {
                    params: vec![
                        Type::TypeVar("a".to_string(), 0),
                        Type::TypeVar("a".to_string(), 0),
                    ],
                    ret: Box::new(Type::Float),
                    variadic: false,
                },
            },
        );

        // Equality: Equatable a => a -> a -> Bool
        env.insert_scheme(
            "=".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                row_vars: vec![],
                constraints: vec![Constraint::new("Equatable", "a")],
                body: Type::Function {
                    params: vec![
                        Type::TypeVar("a".to_string(), 0),
                        Type::TypeVar("a".to_string(), 0),
                    ],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            },
        );

        // Less-than: Comparable a => a -> a -> Bool
        env.insert_scheme(
            "<".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                row_vars: vec![],
                constraints: vec![Constraint::new("Comparable", "a")],
                body: Type::Function {
                    params: vec![
                        Type::TypeVar("a".to_string(), 0),
                        Type::TypeVar("a".to_string(), 0),
                    ],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            },
        );

        // Control flow: if takes Bool, returns Any (type depends on branches)
        env.insert(
            "if".to_string(),
            Type::Function {
                params: vec![Type::Bool, Type::Unknown, Type::Unknown],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );

        // Dict primitives
        env.insert(
            "keys".to_string(),
            Type::Function {
                params: vec![Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_dict".to_string(), 0),
                })],
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );
        env.insert(
            "length".to_string(),
            Type::Function {
                params: vec![Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_dict".to_string(), 0),
                })],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
        env.insert(
            "merge".to_string(),
            Type::Function {
                params: vec![
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_merge_a".to_string(), 0),
                    }),
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_merge_b".to_string(), 0),
                    }),
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_merge_r".to_string(), 0),
                })),
                variadic: false,
            },
        );
        env.insert(
            "append".to_string(),
            Type::Function {
                params: vec![
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_append_a".to_string(), 0),
                    }),
                    Type::Unknown,
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_append_r".to_string(), 0),
                })),
                variadic: false,
            },
        );

        // String operations: Showable a => a -> Str
        env.insert_scheme(
            "str".to_string(),
            TypeScheme {
                type_vars: vec!["a".to_string()],
                row_vars: vec![],
                constraints: vec![Constraint::new("Showable", "a")],
                body: Type::Function {
                    params: vec![Type::TypeVar("a".to_string(), 0)],
                    ret: Box::new(Type::Str),
                    variadic: true,
                },
            },
        );
        for name in ["split", "replace"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![Type::Str, Type::Str],
                    ret: Box::new(if name == "split" {
                        Type::Seq(Box::new(Type::Str))
                    } else {
                        Type::Str
                    }),
                    variadic: false,
                },
            );
        }
        for name in ["upper", "lower", "trim"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![Type::Str],
                    ret: Box::new(Type::Str),
                    variadic: false,
                },
            );
        }

        // Numeric operations
        for name in ["floor", "round"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![Type::Number],
                    ret: Box::new(Type::Int),
                    variadic: false,
                },
            );
        }

        // Parsing
        env.insert(
            "to-int".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Int),
                variadic: false,
            },
        );
        env.insert(
            "to-float".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Float),
                variadic: false,
            },
        );

        // Evaluation control
        env.insert(
            "eval".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "error".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "try".to_string(),
            Type::Function {
                params: vec![Type::Unknown, Type::Unknown],
                ret: Box::new(Type::normalize_union(vec![
                    // [ok: a] variant
                    Type::Record(Row {
                        fields: {
                            let mut f = HashMap::new();
                            f.insert("ok".to_string(), Type::Unknown);
                            f
                        },
                        tail: RowTail::Empty,
                    }),
                    // [err: Str] variant
                    Type::Record(Row {
                        fields: {
                            let mut f = HashMap::new();
                            f.insert("err".to_string(), Type::Str);
                            f
                        },
                        tail: RowTail::Empty,
                    }),
                ])),
                variadic: false,
            },
        );
        env.insert(
            "apply".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_dict".to_string(), 0),
                    }),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );

        // Convergence loop: until(pred, f, init) applies f until pred holds
        env.insert(
            "until".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Bool),
                        variadic: false,
                    },
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Unknown,
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );

        // Type introspection
        env.insert(
            "type-of".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "tag-of".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "variant".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Unknown), // Returns a Variant, but we don't have Type::Variant yet
                variadic: false,
            },
        );
        env.insert(
            "int?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "float?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "num?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "str?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bool?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "null?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "dict?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "fn?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // I/O
        env.insert(
            "emit".to_string(),
            Type::Function {
                params: vec![Type::Str],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                })),
                variadic: false,
            },
        );
        env.insert(
            "env".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Unknown), // returns Str or Null
                variadic: false,
            },
        );
        env.insert(
            "dir-cap".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::DirCap),
                variadic: false,
            },
        );
        env.insert(
            "open".to_string(),
            Type::Function {
                params: vec![Type::DirCap, Type::Str, Type::Str],
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        env.insert(
            "slurp".to_string(),
            Type::Function {
                params: vec![Type::Handle],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "lines".to_string(),
            Type::Function {
                params: vec![Type::Handle],
                ret: Box::new(Type::Seq(Box::new(Type::Str))),
                variadic: false,
            },
        );
        env.insert(
            "narrow".to_string(),
            Type::Function {
                params: vec![Type::DirCap, Type::Str],
                ret: Box::new(Type::DirCap),
                variadic: false,
            },
        );
        env.insert(
            "write".to_string(),
            Type::Function {
                params: vec![Type::DirCap, Type::Str, Type::Str],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                })),
                variadic: false,
            },
        );
        env.insert(
            "write-atomic".to_string(),
            Type::Function {
                params: vec![Type::DirCap, Type::Str, Type::Str],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                })),
                variadic: false,
            },
        );
        env.insert(
            "revocable".to_string(),
            Type::Function {
                params: vec![Type::DirCap],
                ret: Box::new(Type::Unknown), // returns dict with cap and revoke fields
                variadic: false,
            },
        );
        env.insert(
            "revoke-cap".to_string(),
            Type::Function {
                params: vec![Type::DirCap],
                // Null -- Type::Record(Row::Empty), see doc/whatif/null-semantics.md
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                })),
                variadic: false,
            },
        );
        env.insert(
            "net-cap".to_string(),
            Type::Function {
                params: vec![Type::Unknown], // accepts Seq/Dict/Str of allowlist entries
                ret: Box::new(Type::NetCap),
                variadic: false,
            },
        );
        env.insert(
            "connect".to_string(),
            Type::Function {
                params: vec![Type::NetCap, Type::Str, Type::Int],
                ret: Box::new(Type::Handle),
                variadic: false,
            },
        );
        env.insert(
            "from-json".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "include".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );

        // Sequences: primitives
        env.insert(
            "seq".to_string(),
            Type::Function {
                params: vec![Type::Unknown, Type::Unknown],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "head".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "tail".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "collect".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_dict".to_string(), 0),
                })),
                variadic: false,
            },
        );
        env.insert(
            "seq?".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // Sequences: generators
        env.insert(
            "range".to_string(),
            Type::Function {
                params: vec![Type::Int, Type::Int],
                ret: Box::new(Type::Seq(Box::new(Type::Int))),
                variadic: false,
            },
        );
        env.insert(
            "repeat".to_string(),
            Type::Function {
                params: vec![Type::Unknown],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "cycle".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "iterate".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Unknown,
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "unfold".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Unknown,
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
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
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Unknown,
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "filter".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown],
                        ret: Box::new(Type::Bool),
                        variadic: false,
                    },
                    Type::Unknown,
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "take".to_string(),
            Type::Function {
                params: vec![Type::Int, Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );
        env.insert(
            "drop".to_string(),
            Type::Function {
                params: vec![Type::Int, Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );

        // Sequences: reductions
        env.insert(
            "reduce".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Unknown, Type::Unknown],
                        ret: Box::new(Type::Unknown),
                        variadic: false,
                    },
                    Type::Unknown,
                    Type::Seq(Box::new(Type::Unknown)),
                ],
                ret: Box::new(Type::Unknown),
                variadic: false,
            },
        );
        env.insert(
            "join".to_string(),
            Type::Function {
                params: vec![Type::Str, Type::Seq(Box::new(Type::Unknown))],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "concat".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Seq(Box::new(Type::Unknown))))],
                ret: Box::new(Type::Seq(Box::new(Type::Unknown))),
                variadic: false,
            },
        );

        // List operations (moved from LLT stdlib to Rust for performance)
        // rest: Dict -> Dict (removes first entry, reindexes)
        env.insert(
            "rest".to_string(),
            Type::Function {
                params: vec![Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_rest_a".to_string(), 0),
                })],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_rest_r".to_string(), 0),
                })),
                variadic: false,
            },
        );
        // cons: Any -> Dict -> Dict (prepends element, reindexes)
        env.insert(
            "cons".to_string(),
            Type::Function {
                params: vec![
                    Type::Unknown,
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_cons_a".to_string(), 0),
                    }),
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_cons_r".to_string(), 0),
                })),
                variadic: false,
            },
        );
        // reverse: Dict -> Dict (reverses insertion order, reindexes)
        env.insert(
            "reverse".to_string(),
            Type::Function {
                params: vec![Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_reverse_a".to_string(), 0),
                })],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_reverse_r".to_string(), 0),
                })),
                variadic: false,
            },
        );
        // sort: Dict -> Dict (natural ordering, O(n log n))
        env.insert(
            "sort".to_string(),
            Type::Function {
                params: vec![Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_sort_a".to_string(), 0),
                })],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_sort_r".to_string(), 0),
                })),
                variadic: false,
            },
        );

        // Proxy
        env.insert(
            "proxy".to_string(),
            Type::Function {
                params: vec![Type::Function {
                    params: vec![Type::Str],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                }],
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

        // builtin-get: registered directly. 'get' is a prelude wrapper (not a Rust builtin
        // type), so it is absent from this env when the alias loop below runs. Registering
        // builtin-get here gives the type checker enough information to avoid false
        // "undefined variable" errors in stdlib/prelude.llt.
        env.insert_scheme(
            "builtin-get".to_string(),
            TypeScheme {
                type_vars: vec![],
                row_vars: vec![],
                constraints: vec![],
                body: Type::Function {
                    params: vec![Type::Unknown, Type::Unknown],
                    ret: Box::new(Type::Unknown),
                    variadic: false,
                },
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
}

impl TypeError {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Self {
            message: message.into(),
            span,
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
}

impl fmt::Display for TypeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at {}", self.message, self.span)
    }
}

impl std::error::Error for TypeError {}
