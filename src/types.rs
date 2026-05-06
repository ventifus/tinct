//! Runtime type representations, type environments with scoped alias registries,
//! substitutions/unification for Hindley-Milner polymorphism,
//! and type error definitions for the type checker.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

use crate::ast::Span;

/// Row tail for Rémy-style row polymorphism (kinded row variables)
#[derive(Debug, Clone, Eq)]
pub enum RowTail {
    Empty,               // closed row — no more fields
    RowVar(String, u32), // ρ — row variable (bindable in substitution), with level for let-generalization
}

// Manual PartialEq for RowTail: RowVar compares name only, level ignored
impl PartialEq for RowTail {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RowTail::Empty, RowTail::Empty) => true,
            (RowTail::RowVar(n1, _), RowTail::RowVar(n2, _)) => n1 == n2,
            _ => false,
        }
    }
}

/// Row representation for record types (dict+tail representation)
///
/// `fields` uses `HashMap` because row field order is semantically irrelevant at the type level —
/// Rémy's commutativity equations make rows unordered. `Display` sorts field names for
/// deterministic output. Runtime `Value::Dict` keeps `IndexMap` for ordered user-visible
/// semantics; this HashMap is only at the type-inference layer.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub fields: HashMap<String, Type>, // known fields {l₁: τ₁, l₂: τ₂, ...}
    pub tail: RowTail,                 // Empty (closed) or RowVar(ρ) (open)
}

#[derive(Debug, Clone)]
pub enum Type {
    Int,
    IntLiteral(i64),
    Float,
    Str,
    StringLiteral(String),
    Bool,
    /// Supertype of both `Int` and `Float` — represents any numeric value.
    /// No `NumberLiteral` variant exists (unlike `IntLiteral`/`StringLiteral`) because:
    /// - Literals parse to concrete types (`IntLiteral` or `Float`)
    /// - `Number` only appears in user annotations (`[@Number ...]`) and subtyping relations
    /// - There is no runtime value that is "a number but neither int nor float"
    Number,
    Record(Row),
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
        variadic: bool,
    },
    Seq(Box<Type>),
    Proxy,
    #[allow(clippy::enum_variant_names)]
    TypeVar(String, u32),
    Any,
    /// Sentinel for failed sub-expression inference. Prevents cascade errors: when a
    /// sub-expression fails type inference, its result is `Error` rather than propagating
    /// the failure to parent expressions. `unify(Error, T)` is a no-op for all T (silent
    /// absorption), so parent expressions can continue inference without spurious downstream
    /// errors. `is_subtype(Error, _)` returns false; Error is not a subtype of anything.
    Error,
    /// Directory capability — wraps cap_std::fs::Dir. Created by `dir-cap` builtin or
    /// injected via CLI --cap-fs. Represents authority to access a specific directory tree.
    DirCap,
    /// Network capability — wraps host allowlist. Created by `net-cap` builtin or injected
    /// via CLI --cap-net. Represents authority to connect to specific network hosts.
    NetCap,
    /// File/stream handle — wraps Box<dyn BufRead>. Created by `open` or `connect`.
    /// Represents authority to read/write a specific open resource.
    Handle,
}

// Manual PartialEq for Type: TypeVar compares name only, level ignored
impl PartialEq for Type {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Type::Int, Type::Int) => true,
            (Type::IntLiteral(v1), Type::IntLiteral(v2)) => v1 == v2,
            (Type::Float, Type::Float) => true,
            (Type::Str, Type::Str) => true,
            (Type::StringLiteral(s1), Type::StringLiteral(s2)) => s1 == s2,
            (Type::Bool, Type::Bool) => true,
            (Type::Number, Type::Number) => true,
            (Type::Record(row1), Type::Record(row2)) => row1 == row2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                    variadic: v1,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                    variadic: v2,
                },
            ) => v1 == v2 && p1 == p2 && r1 == r2,
            (Type::Seq(e1), Type::Seq(e2)) => e1 == e2,
            (Type::Proxy, Type::Proxy) => true,
            (Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2,
            (Type::Any, Type::Any) => true,
            (Type::Error, Type::Error) => true,
            (Type::DirCap, Type::DirCap) => true,
            (Type::NetCap, Type::NetCap) => true,
            (Type::Handle, Type::Handle) => true,
            _ => false,
        }
    }
}

impl Type {
    /// Recursive without a depth guard; safe because `Type` is a finite tree (structural recursion
    /// on an algebraic data type — each recursive call descends into a strict sub-term). The
    /// occurs-check invariant (Robinson 1965) additionally ensures that substitution-applied types
    /// are acyclic.
    ///
    /// Note: `Any` acts as both top and bottom of the subtype lattice (see [S-ANY-TOP] and
    /// [S-ANY-BOT] in doc/06). The `Any` short-circuit fires at every recursive call site,
    /// including nested positions, so `Seq[Any] <: Seq[τ]` holds for all τ. This is intentional
    /// for gradual typing.
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
        // Error is not a subtype of anything (not even itself), and nothing is a subtype of Error.
        // It is a sentinel for failed inference and should not satisfy any constraint.
        // This check must come BEFORE the Any check to prevent Error <: Any returning true.
        if matches!(sub, Type::Error) || matches!(sup, Type::Error) {
            return false;
        }
        if matches!(sub, Type::Any) || matches!(sup, Type::Any) {
            return true;
        }
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::Seq(sub_elem), Type::Seq(sup_elem)) => Type::is_subtype(sub_elem, sup_elem),
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::Str) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            // Capability types: reflexive only (DirCap <: DirCap, etc.)
            // The equality check at the top of the match handles this, but we document it here.
            // All capability types are subtypes of Any (handled by Any short-circuit above).
            (Type::Record(sub_row), Type::Record(sup_row)) => {
                // All fields in sup must be present in sub with subtype field types
                let fields_ok = sup_row.fields.iter().all(|(k, sup_ty)| {
                    sub_row
                        .fields
                        .get(k)
                        .is_some_and(|sub_ty| Type::is_subtype(sub_ty, sup_ty))
                });
                if !fields_ok {
                    return false;
                }

                // Check tail constraints
                match &sup_row.tail {
                    RowTail::Empty => {
                        // Closed sup requires sub has no extra fields
                        match &sub_row.tail {
                            RowTail::Empty => {
                                // Both closed: sub must have exact same fields as sup
                                sub_row
                                    .fields
                                    .keys()
                                    .all(|k| sup_row.fields.contains_key(k))
                            }
                            RowTail::RowVar(_, _) => {
                                // Open records (RowVar tail) cannot satisfy closed record
                                // constraints — Rémy (1994). The row variable may be instantiated
                                // with additional fields that the closed supertype rejects.
                                // This is the sound PRE-unification behavior: is_subtype is called
                                // before unification binds the RowVar to Empty. After unification,
                                // the substituted type will have RowTail::Empty and the (Empty, Empty)
                                // arm applies correctly. See test_is_subtype_consistency_open_sub_closed_sup_exact_known_fields.
                                false
                            }
                        }
                    }
                    RowTail::RowVar(_, _) => {
                        // Open via row var — extra fields allowed
                        true
                    }
                }
            }
            (
                Type::Function {
                    params: sub_p,
                    ret: sub_r,
                    variadic: sv,
                },
                Type::Function {
                    params: sup_p,
                    ret: sup_r,
                    variadic: pv,
                },
            ) => {
                sv == pv
                    && sub_p.len() == sup_p.len()
                    && sub_p
                        .iter()
                        .zip(sup_p.iter())
                        .all(|(sp, pp)| Type::is_subtype(pp, sp))
                    && Type::is_subtype(sub_r, sup_r)
            }
            _ => false,
        }
    }

    pub fn collect_type_vars(&self, vars: &mut HashSet<String>) {
        match self {
            Type::TypeVar(name, _) => {
                vars.insert(name.clone());
            }
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_type_vars(vars);
                }
                // Row tail contains no type variables (only RowVar or Empty)
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for p in params {
                    p.collect_type_vars(vars);
                }
                ret.collect_type_vars(vars);
            }
            Type::Seq(elem) => elem.collect_type_vars(vars),
            _ => {}
        }
    }

    /// Returns true if the type contains any inference variables (TypeVar or RowVar).
    /// Used to determine whether a type is concrete or still under inference.
    pub fn has_inference_vars(&self) -> bool {
        match self {
            Type::TypeVar(_, _) => true,
            Type::Record(row) => {
                matches!(row.tail, RowTail::RowVar(_, _))
                    || row.fields.values().any(|ty| ty.has_inference_vars())
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => params.iter().any(|p| p.has_inference_vars()) || ret.has_inference_vars(),
            Type::Seq(elem) => elem.has_inference_vars(),
            Type::Proxy => false,
            _ => false,
        }
    }

    /// Collect row variables from RowTail positions in the type tree.
    pub fn collect_row_vars(&self, vars: &mut HashSet<String>) {
        match self {
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_row_vars(vars);
                }
                if let RowTail::RowVar(name, _) = &row.tail {
                    vars.insert(name.clone());
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for p in params {
                    p.collect_row_vars(vars);
                }
                ret.collect_row_vars(vars);
            }
            Type::Seq(elem) => elem.collect_row_vars(vars),
            _ => {}
        }
    }

    /// Collect both type variables and row variables in a single tree walk.
    /// Performance optimization: avoids allocating two HashSets and traversing the type tree twice.
    pub fn collect_all_vars(
        &self,
        type_vars: &mut HashSet<String>,
        row_vars: &mut HashSet<String>,
    ) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.insert(name.clone());
            }
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars(type_vars, row_vars);
                }
                if let RowTail::RowVar(name, _) = &row.tail {
                    row_vars.insert(name.clone());
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for p in params {
                    p.collect_all_vars(type_vars, row_vars);
                }
                ret.collect_all_vars(type_vars, row_vars);
            }
            Type::Seq(elem) => elem.collect_all_vars(type_vars, row_vars),
            _ => {}
        }
    }

    /// Fused occurs check + variable collection: checks whether `occurs_name` appears
    /// in the type tree and simultaneously collects all type vars and row vars.
    /// Returns `true` if `occurs_name` was found (infinite-type guard for U-VAR arms).
    ///
    /// This replaces the double-walk pattern of calling `type_var_occurs()` then
    /// `collect_all_vars()` separately in each U-VAR arm of `unify()`.
    pub fn collect_all_vars_check_occurs(
        &self,
        occurs_name: &str,
        type_vars: &mut HashSet<String>,
        row_vars: &mut HashSet<String>,
    ) -> bool {
        match self {
            Type::TypeVar(name, _) => {
                let found = name == occurs_name;
                type_vars.insert(name.clone());
                found
            }
            Type::Record(row) => {
                let mut found = false;
                for ty in row.fields.values() {
                    found |= ty.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                }
                if let RowTail::RowVar(name, _) = &row.tail {
                    row_vars.insert(name.clone());
                }
                found
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                let mut found = false;
                for p in params {
                    found |= p.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                }
                found |= ret.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars);
                found
            }
            Type::Seq(elem) => elem.collect_all_vars_check_occurs(occurs_name, type_vars, row_vars),
            _ => false,
        }
    }

    /// Collect type and row variables into Vecs, allowing duplicates. Cheaper than HashSet
    /// allocation; callers that need deduplication handle it via seen-set or contains_key guards.
    /// Production callers: `instantiate_at_level` and `generalize`. (The test-only `instantiate()`
    /// uses the HashSet variant `collect_all_vars` instead.)
    pub fn collect_all_vars_vec(&self, type_vars: &mut Vec<String>, row_vars: &mut Vec<String>) {
        match self {
            Type::TypeVar(name, _) => {
                type_vars.push(name.clone());
            }
            Type::Record(row) => {
                for ty in row.fields.values() {
                    ty.collect_all_vars_vec(type_vars, row_vars);
                }
                if let RowTail::RowVar(name, _) = &row.tail {
                    row_vars.push(name.clone());
                }
            }
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                for p in params {
                    p.collect_all_vars_vec(type_vars, row_vars);
                }
                ret.collect_all_vars_vec(type_vars, row_vars);
            }
            Type::Seq(elem) => elem.collect_all_vars_vec(type_vars, row_vars),
            _ => {}
        }
    }
}

/// Type scheme for let-generalization (∀α₁...αₙ. τ)
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScheme {
    pub type_vars: Vec<String>,
    pub row_vars: Vec<String>,
    pub body: Type,
}

impl TypeScheme {
    /// Create a monomorphic scheme (no quantified variables)
    pub fn mono(ty: Type) -> Self {
        Self {
            type_vars: vec![],
            row_vars: vec![],
            body: ty,
        }
    }
}

impl fmt::Display for TypeScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.type_vars.is_empty() && self.row_vars.is_empty() {
            write!(f, "{}", self.body)
        } else {
            write!(f, "∀")?;
            let mut first = true;
            for var in self.type_vars.iter().chain(self.row_vars.iter()) {
                if !first {
                    write!(f, " ")?;
                }
                write!(f, "{var}")?;
                first = false;
            }
            write!(f, ". {}", self.body)
        }
    }
}

/// Inference state for levels-based let-generalization
#[derive(Debug, Clone)]
pub struct InferState {
    /// Monotonic counter for fresh type/row variable names (_t0, _t1, ...).
    /// Uses u32 instead of u64 — assumes no single inference run creates >4B type variables.
    /// (In practice, programs with >1M type variables exhaust memory first via substitution map growth.)
    pub name_counter: u32,
    pub level: u32,
    pub levels: HashMap<String, u32>,
    /// Global accumulated substitution: collects constraints from access-chain inference
    /// and other constraint generators. Applied when resolving type variables during
    /// inference, so that constraints from `$x.field1` are visible when processing
    /// `$x.field2` in the same expression. See doc/07-type-extensions.md Part 5.
    pub subst: Substitution,
}

impl InferState {
    pub fn new() -> Self {
        Self {
            name_counter: 0,
            level: 0,
            levels: HashMap::new(),
            subst: Substitution::new(),
        }
    }

    /// Create a fresh type variable at the current level and register it in `state.levels`.
    pub fn fresh_type_var(&mut self) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }

    /// Create a fresh row variable name at the current level and register it in `state.levels`.
    pub fn fresh_row_var_name(&mut self) -> (String, u32) {
        let name = format!("_t{}", self.name_counter);
        self.name_counter = self.name_counter.saturating_add(1);
        self.levels.insert(name.clone(), self.level);
        (name, self.level)
    }

    #[cfg(test)]
    pub fn fresh_var(&mut self) -> Type {
        self.fresh_type_var()
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    pub type_map: HashMap<String, Type>, // α → τ  (kind: Type)
    pub row_map: HashMap<String, Row>,   // ρ → r  (kind: Row)
}

const MAX_APPLY_DEPTH: usize = 256;

/// Maximum size of the substitution map (combined type_map + row_map entries).
/// Prevents resource exhaustion from quadratic growth in pathological cases.
/// Raised from 10K to 50K to accommodate real-world K8s-style configs with
/// hundreds of open-record dot-accesses that each bind a fresh row variable.
pub const MAX_SUBST_SIZE: usize = 50_000;

impl Substitution {
    /// Create a new empty substitution.
    ///
    /// Performance note: `HashMap::new()` creates a map with zero capacity
    /// and performs no heap allocation until the first insert. This is optimal
    /// for fully-concrete dicts that generate no unification constraints.
    pub fn new() -> Self {
        Self {
            type_map: HashMap::new(),
            row_map: HashMap::new(),
        }
    }

    /// Check if the substitution is empty (no bindings in either map).
    /// Used to guard against unnecessary allocation in apply() operations.
    pub fn is_empty(&self) -> bool {
        self.type_map.is_empty() && self.row_map.is_empty()
    }

    /// Check if the substitution has exceeded the maximum allowed size.
    /// Returns an error if the combined size of type_map and row_map exceeds MAX_SUBST_SIZE.
    pub(crate) fn check_size(&self, span: Span) -> Result<(), TypeError> {
        let total_size = self.type_map.len() + self.row_map.len();
        if total_size > MAX_SUBST_SIZE {
            Err(TypeError::new(
                format!(
                    "type inference resource limit exceeded (substitution size {} > {}) — use fewer chained dot-accesses or add explicit type annotations to break constraint chains",
                    total_size, MAX_SUBST_SIZE
                ),
                span,
            ))
        } else {
            Ok(())
        }
    }

    pub fn apply(&self, ty: &Type) -> Type {
        if self.is_empty() {
            return ty.clone();
        }
        // Fast-path for concrete types: no type variables, so return clone immediately.
        // Avoids allocating visited_types/visited_rows HashSets for the common case.
        match ty {
            Type::Int
            | Type::Float
            | Type::Bool
            | Type::Str
            | Type::Number
            | Type::Any
            | Type::Proxy
            | Type::Error
            | Type::DirCap
            | Type::NetCap
            | Type::Handle => {
                return ty.clone();
            }
            _ => {}
        }
        let mut visited_types = HashSet::new();
        let mut visited_rows = HashSet::new();
        self.apply_type(ty, 0, &mut visited_types, &mut visited_rows)
            .into_owned()
    }

    /// Apply substitution with externally-supplied visited sets.
    /// Allows sharing visited sets across multiple apply() calls to avoid repeated allocation.
    /// The caller must clear the visited sets between uses.
    pub fn apply_with_visited(
        &self,
        ty: &Type,
        visited_types: &mut HashSet<String>,
        visited_rows: &mut HashSet<String>,
    ) -> Type {
        if self.is_empty() {
            return ty.clone();
        }
        self.apply_type(ty, 0, visited_types, visited_rows)
            .into_owned()
    }

    fn apply_type<'a>(
        &self,
        ty: &'a Type,
        depth: usize,
        visited_types: &mut HashSet<String>,
        visited_rows: &mut HashSet<String>,
    ) -> Cow<'a, Type> {
        if depth >= MAX_APPLY_DEPTH {
            return Cow::Borrowed(ty);
        }
        match ty {
            Type::TypeVar(name, level) => {
                if visited_types.contains(name) {
                    return Cow::Borrowed(ty);
                }
                match self.type_map.get(name) {
                    Some(bound) => {
                        visited_types.insert(name.clone());
                        // Reset depth to 0 when following a TypeVar binding: chain-following
                        // is cycle-protected by visited_types; depth guards structural
                        // recursion only. Resetting prevents premature truncation of
                        // long-but-shallow substitution chains (items 5/6).
                        let result = self
                            .apply_type(bound, 0, visited_types, visited_rows)
                            .into_owned();
                        visited_types.remove(name);
                        Cow::Owned(result)
                    }
                    None => Cow::Owned(Type::TypeVar(name.clone(), *level)),
                }
            }
            Type::Record(row) => {
                let applied_row = self.apply_row(row, depth + 1, visited_types, visited_rows);
                Cow::Owned(Type::Record(applied_row))
            }
            Type::Function {
                params,
                ret,
                variadic,
            } => Cow::Owned(Type::Function {
                params: params
                    .iter()
                    .map(|p| {
                        self.apply_type(p, depth + 1, visited_types, visited_rows)
                            .into_owned()
                    })
                    .collect(),
                ret: Box::new(
                    self.apply_type(ret, depth + 1, visited_types, visited_rows)
                        .into_owned(),
                ),
                variadic: *variadic,
            }),
            Type::Seq(elem) => Cow::Owned(Type::Seq(Box::new(
                self.apply_type(elem, depth + 1, visited_types, visited_rows)
                    .into_owned(),
            ))),
            // Primitive types (Int, Float, Bool, Str, etc.) have no type variables;
            // return a borrow to avoid cloning the whole type tree when substitution
            // does not apply. Cow::Borrowed eliminates the clone on the hot path.
            _ => Cow::Borrowed(ty),
        }
    }

    fn apply_row(
        &self,
        row: &Row,
        depth: usize,
        visited_types: &mut HashSet<String>,
        visited_rows: &mut HashSet<String>,
    ) -> Row {
        if depth >= MAX_APPLY_DEPTH {
            return row.clone();
        }

        // Apply substitution to field types. apply_type returns Cow<'_, Type>;
        // .into_owned() is called here because new_fields needs owned Types.
        // Primitive field types (Int, Str, etc.) avoid cloning inside apply_type
        // and only allocate here when ownership is required for the HashMap.
        let new_fields: HashMap<String, Type> = row
            .fields
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    self.apply_type(v, depth + 1, visited_types, visited_rows)
                        .into_owned(),
                )
            })
            .collect();

        // Resolve tail
        match &row.tail {
            RowTail::Empty => Row {
                fields: new_fields,
                tail: RowTail::Empty,
            },
            RowTail::RowVar(name, level) => {
                if visited_rows.contains(name) {
                    // Cycle detected: return unresolved row var
                    return Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(name.clone(), *level),
                    };
                }
                match self.row_map.get(name) {
                    Some(bound_row) => {
                        visited_rows.insert(name.clone());
                        // Reset depth to 0 when following a RowVar binding: cycle-protection
                        // is handled by visited_rows; depth guards structural recursion only.
                        let resolved = self.apply_row(bound_row, 0, visited_types, visited_rows);
                        visited_rows.remove(name);

                        // Merge fields: explicit fields (new_fields) take precedence.
                        // Duplicates CAN legitimately arise here: a row variable may
                        // have been bound (by a prior unification step or by direct
                        // construction) to a row that re-introduces a field already
                        // present in the explicit fields.  The contains_key guard
                        // ensures the explicit field always wins, matching Rémy's
                        // semantics for row-variable substitution application.
                        // See test_substitution_apply_row_var_duplicate_field.
                        let mut merged = new_fields;
                        for (key, value) in resolved.fields {
                            if !merged.contains_key(&key) {
                                merged.insert(key, value);
                            }
                        }
                        Row {
                            fields: merged,
                            tail: resolved.tail,
                        }
                    }
                    None => Row {
                        fields: new_fields,
                        tail: RowTail::RowVar(name.clone(), *level),
                    },
                }
            }
        }
    }

    /// Test-only introspection: lookup a type variable binding in the type_map.
    /// Used in type checker tests for asserting substitution contents; not called from production code.
    /// For production access to substitution results, use `apply()` instead.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&Type> {
        self.type_map.get(name)
    }
}

impl Default for Substitution {
    fn default() -> Self {
        Self::new()
    }
}

/// Row variable occurs check: does row variable ρ occur in row r?
/// Checks both the tail (direct occurrence like ρ = {..., ...ρ}) and field types
/// (nested occurrence like ρ = {x: Record({y: Int, ...ρ})})
/// Chases TypeVar bindings through `subst` to detect transitive occurrences.
fn row_var_occurs(var_name: &str, row: &Row, subst: &Substitution) -> bool {
    // Check field types for nested row variables
    let in_fields = row
        .fields
        .values()
        .any(|ty| row_var_occurs_in_type(var_name, ty, subst));
    // Check tail
    let in_tail = matches!(&row.tail, RowTail::RowVar(name, _) if name == var_name);
    in_fields || in_tail
}

/// Row variable occurs check in type: does ρ occur in type τ through Record nesting?
/// Chases TypeVar bindings through `subst` so that if α is bound to a type containing ρ,
/// the occurrence is detected. This mirrors Robinson's requirement that the occurs check
/// operates on substitution-applied types.
///
/// ## Call pattern analysis (Task 3: FTV/FRV caching feasibility)
///
/// This function is called exclusively through `row_var_occurs`, which is invoked:
///
///   1. In `unify_remainders` Cases 2, 3, 4 — exactly **once** per row-variable binding,
///      checking whether the variable being bound appears in the row it would be bound to.
///      The walk is O(|unique_fields| × depth) — unavoidable for a sound occurs check.
///
///   2. Via `row_var_occurs_pub` in `typecheck.rs` access-chain generation — once per
///      dot-access on an open record.
///
/// The optimization proposed in TODO.md (pre-collect all free row vars once per unification
/// context, then check membership) would only help if `row_var_occurs` were called in a
/// loop over the same fields with different target variables. In the current code, Cases 2
/// and 3 each check ONE variable against ONE row (one call total). Case 4 checks TWO
/// different variables against TWO different rows — a pre-collected FRV set cannot eliminate
/// either walk because they target different variables. There is no O(n×m) pattern to break.
///
/// **Decision**: no caching optimization is warranted at this call site. The occurs check
/// is already called the minimum number of times required for soundness. If future work
/// introduces a loop that calls `row_var_occurs` for each field in a large record (e.g., a
/// bulk row-compatibility check), revisit by collecting `FRV(row)` once before the loop via
/// `ty.collect_row_vars(&mut frv_set)` and replacing per-field tree walks with `frv_set.contains`.
fn row_var_occurs_in_type(var_name: &str, ty: &Type, subst: &Substitution) -> bool {
    let mut visited = HashSet::new();
    row_var_occurs_in_type_impl(var_name, ty, subst, &mut visited)
}

/// Implementation of `row_var_occurs_in_type` with cycle detection.
/// Defense-in-depth: tracks visited TypeVars to prevent unbounded recursion
/// on cyclic type_map bindings (should be impossible under correct occurs-check
/// invariants, but defended against for robustness).
fn row_var_occurs_in_type_impl(
    var_name: &str,
    ty: &Type,
    subst: &Substitution,
    visited: &mut HashSet<String>,
) -> bool {
    match ty {
        Type::Record(row) => row_var_occurs(var_name, row, subst),
        Type::Function {
            params,
            ret,
            variadic: _,
        } => {
            params
                .iter()
                .any(|p| row_var_occurs_in_type_impl(var_name, p, subst, visited))
                || row_var_occurs_in_type_impl(var_name, ret, subst, visited)
        }
        Type::Seq(elem) => row_var_occurs_in_type_impl(var_name, elem, subst, visited),
        Type::TypeVar(name, _) => {
            // Chase TypeVar binding: if α is bound to τ in subst, check τ for ρ
            // Cycle detection: if we've already visited this TypeVar, return false
            // to prevent infinite recursion on cyclic bindings (impossible under
            // correct occurs-check invariants, but defended against for robustness).
            //
            // Monotone visited set: once a TypeVar is visited, it stays visited.
            // The occurs-check result is path-independent — if ρ does not occur in
            // the resolution of α via one path, it won't occur via any other path,
            // because subst.type_map is deterministic (each name maps to exactly
            // one type). Removing on backtrack would only cause redundant re-traversal
            // without changing the result.
            if !visited.insert(name.clone()) {
                return false;
            }
            if let Some(bound) = subst.type_map.get(name) {
                row_var_occurs_in_type_impl(var_name, bound, subst, visited)
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Check if a row variable name should be hidden from display (starts with '_')
fn is_display_hidden(name: &str) -> bool {
    name.starts_with('_')
}

/// Resolve a row by following bound row variables in the substitution
fn resolve_row<'a>(row: &'a Row, subst: &Substitution) -> Cow<'a, Row> {
    match &row.tail {
        RowTail::RowVar(name, _level) => {
            if let Some(bound) = subst.row_map.get(name) {
                // Fast-path: if the original row has no fields, the resolved row is the result.
                // No need to clone and merge — return the resolved row directly.
                if row.fields.is_empty() {
                    let mut visited_types = HashSet::new();
                    let mut visited_rows = HashSet::new();
                    return Cow::Owned(subst.apply_row(
                        bound,
                        0,
                        &mut visited_types,
                        &mut visited_rows,
                    ));
                }

                // Apply the row to chase through the binding
                let mut visited_types = HashSet::new();
                let mut visited_rows = HashSet::new();
                let resolved = subst.apply_row(bound, 0, &mut visited_types, &mut visited_rows);
                // Merge fields: original fields take precedence.
                // Overlap can arise when ρ was bound by a different unification call
                // (e.g., {y: T, ...ρ} ~ {y: T, x: S} binds ρ → {x: S}, then
                // resolving {x: U, ...ρ} sees x in both the explicit and bound rows).
                let mut merged = row.fields.clone();
                for (key, value) in resolved.fields {
                    if !merged.contains_key(&key) {
                        merged.insert(key, value);
                    }
                }
                Cow::Owned(Row {
                    fields: merged,
                    tail: resolved.tail,
                })
            } else {
                Cow::Borrowed(row)
            }
        }
        RowTail::Empty => Cow::Borrowed(row),
    }
}

/// Unify two row tails
fn unify_tails(
    t1: &RowTail,
    t2: &RowTail,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    match (t1, t2) {
        (RowTail::Empty, RowTail::Empty) => Ok(()),
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _)) => {
            // No occurs check needed: resolve_row guarantees both unbound, so binding ρ₁→{…ρ₁} cannot occur (Robinson vacuous satisfaction)
            if rho1 == rho2 {
                Ok(())
            } else {
                // Bind rho1 to Row { fields: {}, tail: RowVar(rho2) }
                // Lower levels symmetrically
                //
                // The level asymmetry is safe: rho1 is bound to Row({}, RowVar(rho2)), eliminating it
                // from the constraint set. Only rho2 remains free, so only its level needs lowering to
                // prevent unsound generalization (Kiselyov 2013). However, we lower rho2's level to
                // min(rho1_level, rho2_level) to maintain the invariant that binding eliminates the
                // higher-level variable.
                let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
                let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
                state
                    .levels
                    .insert(rho2.clone(), rho2_level.min(rho1_level));

                subst.row_map.insert(
                    rho1.clone(),
                    Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar(rho2.clone(), rho2_level.min(rho1_level)),
                    },
                );
                subst.check_size(span)?;
                Ok(())
            }
        }
        (RowTail::RowVar(rho, _), RowTail::Empty) | (RowTail::Empty, RowTail::RowVar(rho, _)) => {
            // Bind rho to Row { fields: {}, tail: Empty }
            subst.row_map.insert(
                rho.clone(),
                Row {
                    fields: HashMap::new(),
                    tail: RowTail::Empty,
                },
            );
            subst.check_size(span)?;
            Ok(())
        }
    }
}

/// Lower the level of all type vars and row vars appearing in a row to min(their level, max_level).
/// Called after a row-variable binding to prevent unsound generalization of inner vars.
fn lower_row_var_levels(row: &Row, max_level: u32, state: &mut InferState) {
    // Collect both type vars and row vars in a single pass over field types
    let mut type_vars = HashSet::new();
    let mut row_vars = HashSet::new();
    for ty in row.fields.values() {
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
    }
    // Also collect the tail row var if present
    if let RowTail::RowVar(name, _) = &row.tail {
        row_vars.insert(name.clone());
    }
    // Lower all collected vars in a single pass
    for var in type_vars.iter().chain(&row_vars) {
        let current = state.levels.get(var).copied().unwrap_or(0);
        state.levels.insert(var.clone(), current.min(max_level));
    }
}

/// Public wrapper for `row_var_occurs` — used in access-chain constraint generation
/// (doc/07-type-extensions.md Part 5) to check for cyclic row bindings before binding.
pub fn row_var_occurs_pub(var_name: &str, row: &Row, subst: &Substitution) -> bool {
    row_var_occurs(var_name, row, subst)
}

/// Public wrapper for `lower_row_var_levels` — used in access-chain constraint generation
/// (doc/07-type-extensions.md Part 5) to enforce level invariants before binding a row variable.
pub fn lower_row_var_levels_pub(row: &Row, max_level: u32, state: &mut InferState) {
    lower_row_var_levels(row, max_level, state);
}

/// Case 4 of Wand (1987): both rows have unique fields and distinct RowVar tails.
///
/// Creates a fresh row variable ρ_fresh to represent the shared unknown tail, then:
///   - Binds ρ₁ → Row { fields: U₂, tail: RowVar(ρ_fresh) }
///   - Binds ρ₂ → Row { fields: U₁, tail: RowVar(ρ_fresh) }
///
/// This correctly propagates constraints: if either tail is later unified with a
/// concrete row, the binding flows through ρ_fresh to the other side.
///
/// # Soundness
///
/// Before each binding, `row_var_occurs` is called to detect would-be cyclic
/// bindings (infinite row types).  After each `row_map.insert`, `check_size` is
/// called to enforce the global substitution size limit.  Level lowering
/// (`lower_row_var_levels`) is applied to both rows before binding so that
/// inner type/row variables cannot escape their scope via the fresh tail
/// (Kiselyov 2013 §level-lowering).
fn partition_fields_and_bind(
    unique1: HashMap<String, Type>,
    rho1: &str,
    unique2: HashMap<String, Type>,
    rho2: &str,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Allocate a fresh row variable ρ_fresh to act as the shared unknown tail
    let rho_fresh_name = format!("_t{}", state.name_counter);
    state.name_counter = state.name_counter.saturating_add(1);
    let rho_fresh_level = state.level;
    state.levels.insert(rho_fresh_name.clone(), rho_fresh_level);

    let fresh_tail = RowTail::RowVar(rho_fresh_name.clone(), rho_fresh_level);

    // Build the two rows that each RowVar will be bound to.
    // ρ₁ → Row { fields: U₂, tail: ρ_fresh }
    // ρ₂ → Row { fields: U₁, tail: ρ_fresh }
    let row2_with_fresh = Row {
        fields: unique2,
        tail: fresh_tail.clone(),
    };
    let row1_with_fresh = Row {
        fields: unique1,
        tail: RowTail::RowVar(rho_fresh_name, rho_fresh_level),
    };

    // Occurs check: ρ₁ must not appear in (U₂ ∪ {ρ_fresh})
    if row_var_occurs(rho1, &row2_with_fresh, subst) {
        let rho1_display = if is_display_hidden(rho1) {
            "an anonymous open row".to_string()
        } else {
            rho1.to_string()
        };
        return Err(TypeError::new(
            format!("infinite row type: {rho1_display} occurs in its own binding"),
            span,
        ));
    }

    // Occurs check: ρ₂ must not appear in (U₁ ∪ {ρ_fresh})
    if row_var_occurs(rho2, &row1_with_fresh, subst) {
        let rho2_display = if is_display_hidden(rho2) {
            "an anonymous open row".to_string()
        } else {
            rho2.to_string()
        };
        return Err(TypeError::new(
            format!("infinite row type: {rho2_display} occurs in its own binding"),
            span,
        ));
    }

    // Level lowering: prevent inner vars from escaping their scope through the fresh tail
    let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
    let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
    lower_row_var_levels(&row2_with_fresh, rho1_level, state);
    lower_row_var_levels(&row1_with_fresh, rho2_level, state);

    // Bind ρ₁ → Row { fields: U₂, tail: ρ_fresh }
    subst.row_map.insert(rho1.to_string(), row2_with_fresh);
    subst.check_size(span)?;
    // Bind ρ₂ → Row { fields: U₁, tail: ρ_fresh }
    subst.row_map.insert(rho2.to_string(), row1_with_fresh);
    subst.check_size(span)?;

    Ok(())
}

/// Unify remainders (unique fields + tails) — implements Wand (1987) 4-case algorithm.
///
/// Soundness invariant: every binding case calls `row_var_occurs` BEFORE
/// `subst.row_map.insert` to prevent construction of infinite row types
/// (Robinson 1965, extended for rows per Rémy 1994).  Verified for Cases 2–4.
fn unify_remainders(
    unique1: HashMap<String, Type>,
    tail1: RowTail,
    unique2: HashMap<String, Type>,
    tail2: RowTail,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    let u1_empty = unique1.is_empty();
    let u2_empty = unique2.is_empty();

    // NOTE: Case 4 must be matched BEFORE Cases 2/3 to prevent shadowing
    match (&tail1, &tail2) {
        // Case 1: No unique fields on either side — unify tails directly
        (_, _) if u1_empty && u2_empty => unify_tails(&tail1, &tail2, subst, state, span),

        // Case 4: Both have unique fields and both have RowVar tails — create fresh row variable.
        // Delegates to `partition_fields_and_bind` which encapsulates the occurs checks,
        // level lowering, and dual binding logic (Wand 1987, Case 4).
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _))
            if !u1_empty && !u2_empty && rho1 != rho2 =>
        {
            partition_fields_and_bind(unique1, rho1, unique2, rho2, subst, state, span)
        }

        // Case 2: Only left has unique fields — right tail must absorb them
        // Guard: u2_empty required — when both sides have unique fields with different RowVars, Case 4 applies; this guard ensures Case 2 only fires when unique2 is genuinely empty.
        (_, RowTail::RowVar(rho2, _)) if !u1_empty && u2_empty => {
            let row_to_bind = Row {
                fields: unique1,
                tail: tail1,
            };
            if row_var_occurs(rho2, &row_to_bind, subst) {
                let rho2_display = if is_display_hidden(rho2) {
                    "an anonymous open row".to_string()
                } else {
                    rho2.clone()
                };
                return Err(TypeError::new(
                    format!("infinite row type: {rho2_display} occurs in its own binding"),
                    span,
                ));
            }
            // Lower levels of inner vars to rho2's level before binding
            let rho2_level = state.levels.get(rho2).copied().unwrap_or(0);
            lower_row_var_levels(&row_to_bind, rho2_level, state);
            subst.row_map.insert(rho2.clone(), row_to_bind);
            subst.check_size(span)?;
            Ok(())
        }

        // Case 3: Only right has unique fields — left tail must absorb them
        // Guard: u1_empty required — when both sides have unique fields with different RowVars,
        // Case 4 applies; this guard ensures Case 3 only fires when unique1 is genuinely empty.
        (RowTail::RowVar(rho1, _), _) if !u2_empty && u1_empty => {
            let row_to_bind = Row {
                fields: unique2,
                tail: tail2,
            };
            if row_var_occurs(rho1, &row_to_bind, subst) {
                let rho1_display = if is_display_hidden(rho1) {
                    "an anonymous open row".to_string()
                } else {
                    rho1.clone()
                };
                return Err(TypeError::new(
                    format!("infinite row type: {rho1_display} occurs in its own binding"),
                    span,
                ));
            }
            // Lower levels of inner vars to rho1's level before binding
            let rho1_level = state.levels.get(rho1).copied().unwrap_or(0);
            lower_row_var_levels(&row_to_bind, rho1_level, state);
            subst.row_map.insert(rho1.clone(), row_to_bind);
            subst.check_size(span)?;
            Ok(())
        }

        // Error case: closed tail cannot absorb unique fields
        (_, RowTail::Empty) if !u1_empty => Err(TypeError::new(
            format!("extra fields [{}] in closed row", {
                let mut keys: Vec<_> = unique1.keys().cloned().collect();
                keys.sort();
                keys.join(", ")
            }),
            span,
        )),
        (RowTail::Empty, _) if !u2_empty => Err(TypeError::new(
            format!("extra fields [{}] in closed row", {
                let mut keys: Vec<_> = unique2.keys().cloned().collect();
                keys.sort();
                keys.join(", ")
            }),
            span,
        )),

        // Error case: same row variable with different unique fields on BOTH sides
        // This handles {x: Int, ...rho} ~ {y: Str, ...rho} which would require
        // rho to simultaneously provide both x and y, which is impossible
        (RowTail::RowVar(rho1, _), RowTail::RowVar(rho2, _))
            if rho1 == rho2 && !u1_empty && !u2_empty =>
        {
            let mut fields: Vec<_> = unique1.keys().chain(unique2.keys()).cloned().collect();
            fields.sort();
            let rho1_display = if rho1.starts_with('_') {
                "an anonymous open row".to_string()
            } else {
                rho1.clone()
            };
            Err(TypeError::new(
                format!(
                    "incompatible fields [{}] with shared row variable {}",
                    fields.join(", "),
                    rho1_display
                ),
                span,
            ))
        }

        // All 7 pattern cases are exhaustive over (u1_empty, tail1, u2_empty, tail2); this arm is dead by invariant.
        _ => unreachable!("unify_remainders: all cases should be covered"),
    }
}

/// Unify two rows using field partitioning
fn unify_rows(
    row1: &Row,
    row2: &Row,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Step 1: Resolve bound row variables
    let resolved1 = resolve_row(row1, subst).into_owned();
    let resolved2 = resolve_row(row2, subst).into_owned();

    // Fast-path: both rows are closed and have identical key sets — the common case
    // for checking an inferred closed record against an annotated closed record.
    // Skip all partition allocation and proceed directly to per-field unification.
    if resolved1.tail == RowTail::Empty
        && resolved2.tail == RowTail::Empty
        && resolved1.fields.len() == resolved2.fields.len()
        && resolved1
            .fields
            .keys()
            .all(|k| resolved2.fields.contains_key(k))
    {
        for (key, ty1) in &resolved1.fields {
            let ty2 = &resolved2.fields[key];
            unify(ty1, ty2, subst, state, span)?;
        }
        return Ok(());
    }

    // Step 2: Partition fields into shared and unique
    let keys1: HashSet<&String> = resolved1.fields.keys().collect();
    let keys2: HashSet<&String> = resolved2.fields.keys().collect();
    let shared: Vec<&String> = keys1.intersection(&keys2).copied().collect();

    let unique1: HashMap<String, Type> = resolved1
        .fields
        .iter()
        .filter(|(k, _)| !keys2.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    let unique2: HashMap<String, Type> = resolved2
        .fields
        .iter()
        .filter(|(k, _)| !keys1.contains(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    // Step 3: Unify shared field types
    for key in shared {
        let ty1 = &resolved1.fields[key];
        let ty2 = &resolved2.fields[key];
        unify(ty1, ty2, subst, state, span)?;
    }

    // Step 3.5: Re-resolve tails after shared-field unification
    // Step 3's recursive unify() calls may have bound row variables that appear
    // as resolved1.tail or resolved2.tail (e.g., when unifying nested Record types
    // that share a row variable with the outer row's tail). Passing stale tails to
    // Step 4 would cause unify_remainders to overwrite the Step-3 binding, violating
    // the Robinson (1965) substitution-threading invariant.
    //
    // Fast-path: both tails are already Empty — re-resolution is a no-op (resolve_row
    // with RowTail::Empty returns the row unchanged). Skip the two resolve_row calls and
    // the Step 3.6 re-partition allocations; proceed directly to unify_remainders.
    if resolved1.tail == RowTail::Empty && resolved2.tail == RowTail::Empty {
        return unify_remainders(
            unique1,
            resolved1.tail.clone(),
            unique2,
            resolved2.tail.clone(),
            subst,
            state,
            span,
        );
    }

    let re_resolved1 = resolve_row(
        &Row {
            fields: unique1,
            tail: resolved1.tail.clone(),
        },
        subst,
    )
    .into_owned();
    let re_resolved2 = resolve_row(
        &Row {
            fields: unique2,
            tail: resolved2.tail.clone(),
        },
        subst,
    )
    .into_owned();

    // Step 3.6: Re-partition after re-resolution
    // Re-resolution may surface new fields from row variable bindings that overlap
    // with the other side's unique fields. These must be unified as shared fields
    // before passing the truly unique remainders to unify_remainders.
    let rekeys1: HashSet<&String> = re_resolved1.fields.keys().collect();
    let rekeys2: HashSet<&String> = re_resolved2.fields.keys().collect();
    let new_shared: Vec<&String> = rekeys1.intersection(&rekeys2).copied().collect();

    if !new_shared.is_empty() {
        // New shared fields surfaced by re-resolution — unify them and re-partition.
        // Delegate to unify_rows which handles the full resolve-partition-unify-remainder
        // cycle. Terminates because each recursive entry requires Step 3 to have bound
        // at least one row variable (surfacing new_shared fields), strictly reducing the
        // number of unbound row variables. The occurs check prevents cyclic bindings.
        unify_rows(&re_resolved1, &re_resolved2, subst, state, span)
    } else {
        // Step 4: Unify remainders with re-resolved tails (no new shared fields)
        unify_remainders(
            re_resolved1.fields,
            re_resolved1.tail,
            re_resolved2.fields,
            re_resolved2.tail,
            subst,
            state,
            span,
        )
    }
}

/// Lower levels of all type/row variables in `ty` to min(their level, cap_level).
/// Performs occurs check simultaneously: returns true if `occurs_name` appears in the tree.
/// No allocation — directly updates `state.levels` in a single recursive walk.
fn lower_levels_check_occurs(
    ty: &Type,
    occurs_name: &str,
    cap_level: u32,
    state: &mut InferState,
) -> bool {
    match ty {
        Type::TypeVar(name, _) => {
            let found = name == occurs_name;
            let current_level = state.levels.get(name).copied().unwrap_or(0);
            state
                .levels
                .insert(name.clone(), current_level.min(cap_level));
            found
        }
        Type::Record(row) => {
            let mut found = false;
            for ty in row.fields.values() {
                found |= lower_levels_check_occurs(ty, occurs_name, cap_level, state);
            }
            if let RowTail::RowVar(name, _) = &row.tail {
                let current_level = state.levels.get(name).copied().unwrap_or(0);
                state
                    .levels
                    .insert(name.clone(), current_level.min(cap_level));
            }
            found
        }
        Type::Function {
            params,
            ret,
            variadic: _,
        } => {
            let mut found = false;
            for p in params {
                found |= lower_levels_check_occurs(p, occurs_name, cap_level, state);
            }
            found |= lower_levels_check_occurs(ret, occurs_name, cap_level, state);
            found
        }
        Type::Seq(elem) => lower_levels_check_occurs(elem, occurs_name, cap_level, state),
        _ => false,
    }
}

/// Unify two types under Robinson's algorithm extended with Rémy-style row polymorphism
/// and Damas-Milner level-based let-generalization.
///
/// ## Robinson unification invariant
///
/// At entry, `subst` is the current accumulated substitution. The first action is to
/// apply `subst` to both `a` and `b`, producing fully-resolved forms. This is the
/// standard Robinson step: chasing all already-bound type variables before dispatching
/// on structure, so that e.g. a previously unified `α ↦ Int` is visible here.
///
/// Recursive calls (e.g. for Function param/return types, or for row fields) receive
/// the *same* `subst` and do their own `apply()` at the top of each call. This is
/// correct: sub-terms extracted from the already-applied forms (`a`/`b` above) are
/// themselves fully resolved w.r.t. the substitution at the time of extraction.
/// However, each recursive call may *extend* `subst` by binding new variables, and
/// those bindings will be visible to subsequent sibling recursive calls (since `subst`
/// is `&mut`). This is the standard incremental Robinson behaviour — not a bug.
///
/// ## Level-lowering invariant
///
/// When binding α ↦ τ (U-VAR-LEVEL), all free type/row variables β in τ have their
/// levels lowered to `min(level(β), level(α))`. This preserves the Damas-Milner
/// generalization invariant: a variable can only be generalized if its level exceeds
/// the enclosing `let`-level, so lowering prevents premature generalization of
/// variables that escape through the binding.
pub fn unify(
    a: &Type,
    b: &Type,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    // Apply current substitution to both sides (Robinson step: chase bound vars).
    // Shared visited sets avoid redundant allocation across both apply() calls.
    let mut visited_types = HashSet::new();
    let mut visited_rows = HashSet::new();
    let a = subst.apply_with_visited(a, &mut visited_types, &mut visited_rows);
    visited_types.clear();
    visited_rows.clear();
    let b = subst.apply_with_visited(b, &mut visited_types, &mut visited_rows);

    if a == b {
        return Ok(());
    }

    // Robinson (1965) invariant: after unifying X and Y, `subst` is extended with at most one
    // new binding (the TypeVar arm inserts exactly one entry into subst.type_map). Subsequent
    // calls to `unify` operate on the extended substitution via the `apply_with_visited` calls
    // at the top of each recursive invocation — those calls chase the substitution chain and
    // return fully-walked types before the match. We do NOT re-apply `subst` to already-unified
    // terms between match arms because (a) the occurs check prevents cycles, so there are no
    // self-referential chains to chase, and (b) each arm receives pre-applied operands (a, b)
    // that are already walk-complete with respect to the substitution at entry time.
    match (&a, &b) {
        // Error absorption: unify(Error, T) = Ok(()) for all T.
        // Error is a sentinel for failed sub-expression inference; absorbing it silently
        // prevents cascade errors in parent expressions. No substitution is modified —
        // Error carries no information that should propagate to type variables.
        (Type::Error, _) | (_, Type::Error) => Ok(()),

        // Any-unification with level zeroing: prevent generalization of Any-touched vars
        (Type::Any, Type::TypeVar(name, _)) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Any) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::Any, other) | (other, Type::Any) => {
            // Zero levels of all type/row vars in the non-Any side to prevent
            // over-generalization. E.g., unify(Any, Fn(TypeVar("b",3) → Int))
            // must zero b's level so it won't be generalized.
            let mut type_vars = HashSet::new();
            let mut row_vars = HashSet::new();
            other.collect_all_vars(&mut type_vars, &mut row_vars);
            for var in type_vars.iter().chain(row_vars.iter()) {
                state.levels.insert(var.clone(), 0);
            }
            Ok(())
        }

        // U-VAR-LEVEL: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (Type::TypeVar(name, _), _) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            // lower_levels_check_occurs returns true if `name` appears in the type tree
            // (infinite-type guard), and simultaneously lowers all var levels to cap_level.
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            if lower_levels_check_occurs(&b, name, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {b}"),
                    span,
                ));
            }
            subst.type_map.insert(name.clone(), b);
            subst.check_size(span)?;
            Ok(())
        }
        // U-VAR-LEVEL-SYM: bind α to τ, lower levels of all β ∈ FTV(τ) and all ρ ∈ FRV(τ)
        (_, Type::TypeVar(name, _)) => {
            // Fused occurs check + level lowering: one tree walk, zero HashSet allocations.
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            if lower_levels_check_occurs(&a, name, alpha_level, state) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {a}"),
                    span,
                ));
            }
            subst.type_map.insert(name.clone(), a);
            subst.check_size(span)?;
            Ok(())
        }

        // Literal-to-parent promotions
        // Note: These rules are bidirectional (IntLiteral ↔ Int) for unification symmetry.
        // In a pure subtyping system, only IntLiteral <: Int would hold (not vice versa).
        // Bidirectional promotion simplifies unification but reduces diagnostic precision:
        // unify(Int, IntLiteral(42)) succeeds, whereas is_subtype(Int, IntLiteral(42)) = false.
        (Type::IntLiteral(_), Type::Int | Type::Number) | (Type::Int, Type::Number) => Ok(()),
        (Type::Int | Type::Number, Type::IntLiteral(_)) | (Type::Number, Type::Int) => Ok(()),
        (Type::Float, Type::Number) | (Type::Number, Type::Float) => Ok(()),
        (Type::StringLiteral(_), Type::Str) | (Type::Str, Type::StringLiteral(_)) => Ok(()),
        (Type::IntLiteral(v1), Type::IntLiteral(v2)) => {
            if v1 == v2 {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(
                    &Type::IntLiteral(*v1),
                    &Type::IntLiteral(*v2),
                    span,
                ))
            }
        }
        (Type::StringLiteral(s1), Type::StringLiteral(s2)) => {
            if s1 == s2 {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(
                    &Type::StringLiteral(s1.clone()),
                    &Type::StringLiteral(s2.clone()),
                    span,
                ))
            }
        }
        (
            Type::Function {
                params: p1,
                ret: r1,
                variadic: v1,
            },
            Type::Function {
                params: p2,
                ret: r2,
                variadic: v2,
            },
        ) => {
            if p1.len() != p2.len() {
                return Err(TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        p1.len(),
                        p2.len()
                    ),
                    span,
                ));
            }
            if v1 != v2 {
                return Err(TypeError::new(
                    format!(
                        "variadic mismatch: {} vs {}",
                        if *v1 { "variadic" } else { "non-variadic" },
                        if *v2 { "variadic" } else { "non-variadic" }
                    ),
                    span,
                ));
            }
            // Robinson invariant: sub-terms are passed without explicit apply() because
            // every recursive unify() call re-applies the accumulated substitution at its
            // own entry (via apply_with_visited at the top of this function). Bindings
            // from earlier parameter unifications are therefore visible to later ones via
            // the shared `subst` — this is correct Robinson (1965) unification.
            for (pa, pb) in p1.iter().zip(p2.iter()) {
                unify(pa, pb, subst, state, span)?;
            }
            unify(r1, r2, subst, state, span)
        }

        (Type::Seq(elem1), Type::Seq(elem2)) => unify(elem1, elem2, subst, state, span),

        (Type::Proxy, Type::Proxy) => Ok(()),

        // Capability types: reflexive unification only
        (Type::DirCap, Type::DirCap) => Ok(()),
        (Type::NetCap, Type::NetCap) => Ok(()),
        (Type::Handle, Type::Handle) => Ok(()),

        // Record unification: delegate to row unification
        (Type::Record(row1), Type::Record(row2)) => unify_rows(row1, row2, subst, state, span),

        // [U-SUBSUME]: concrete type subsumption fallback (Pierce & Turner 2000)
        // When both sides are ground types (no type variables), check the subtype
        // relation in both directions. Bidirectional because unification is symmetric —
        // the original actual/expected roles are lost after structural decomposition.
        // The substitution is not modified (no variables to bind).
        _ if !a.has_inference_vars() && !b.has_inference_vars() => {
            if Type::is_subtype(&a, &b) || Type::is_subtype(&b, &a) {
                Ok(())
            } else {
                Err(TypeError::type_mismatch(&a, &b, span))
            }
        }

        _ => Err(TypeError::type_mismatch(&a, &b, span)),
    }
}

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
/// can appear in a scheme body — TypeVars in a scheme are free variables, not bound ones).
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

    // Fast path: single type variable, no row variables — avoid building Substitution
    // (2 HashMaps) and the apply() HashSet pair. Inline rename is allocation-free
    // aside from the string format for the fresh name.
    if scheme.type_vars.len() == 1 && scheme.row_vars.is_empty() {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        return rename_single_type_var(&scheme.body, &scheme.type_vars[0], &fresh_name, level);
    }

    // General path: multiple variables or row variables — build a full Substitution.
    // Create fresh type variables at the specified level for each quantified var
    let mut renaming = Substitution {
        type_map: HashMap::with_capacity(scheme.type_vars.len()),
        row_map: HashMap::with_capacity(scheme.row_vars.len()),
    };
    for var in &scheme.type_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        renaming
            .type_map
            .insert(var.clone(), Type::TypeVar(fresh_name, level));
    }

    // Create fresh row variables — row vars bind to Row, not Type
    // Row variables and type variables share the same naming counter (`_t{n}`)
    for var in &scheme.row_vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter = state.name_counter.saturating_add(1);
        state.levels.insert(fresh_name.clone(), level);
        renaming.row_map.insert(
            var.clone(),
            Row {
                fields: HashMap::new(),
                tail: RowTail::RowVar(fresh_name, level),
            },
        );
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
    // Apply substitution first — defense-in-depth per Damas & Milner (1982).
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
        TypeScheme {
            type_vars: generalizable_type_vars,
            row_vars: generalizable_row_vars,
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
            Type::Any => write!(f, "Any"),
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
                        // Hide generated names (starting with _) — display as bare "..."
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
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    bindings: HashMap<String, TypeScheme>,
    type_aliases: HashMap<String, Type>,
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
        self.lookup(|env| env.bindings.get(name))
    }

    pub fn get_type_alias(&self, name: &str) -> Option<&Type> {
        self.lookup_type_alias(|env| env.type_aliases.get(name))
    }

    pub(crate) fn lookup(
        &self,
        field: impl Fn(&TypeEnv) -> Option<&TypeScheme>,
    ) -> Option<&TypeScheme> {
        if let Some(scheme) = field(self) {
            return Some(scheme);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(scheme) = field(env) {
                return Some(scheme);
            }
            current = env.parent.as_deref();
        }
        None
    }

    fn lookup_type_alias(&self, field: impl Fn(&TypeEnv) -> Option<&Type>) -> Option<&Type> {
        if let Some(ty) = field(self) {
            return Some(ty);
        }
        let mut current = self.parent.as_deref();
        while let Some(env) = current {
            if let Some(ty) = field(env) {
                return Some(ty);
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

    pub fn insert_type_alias(&mut self, name: String, ty: Type) {
        self.type_aliases.insert(name, ty);
    }

    /// Create a `TypeEnv` pre-registered with builtin function type signatures.
    ///
    /// This enables the type checker to validate user code that calls builtins.
    /// Polymorphic parameters use `Any` as the escape hatch; precise return types
    /// are specified where known.
    ///
    /// **Type signature conventions:**
    /// - `Any → Any → T`: binary operator returning type `T`
    /// - `Any → T`: unary operator returning type `T`
    /// - `Fn@Any [Any]`: higher-order function (e.g. map, filter) with `Any` for callbacks
    ///
    /// **Coverage:** All 57 builtins from `standard_builtins()` (src/builtins.rs)
    pub fn with_builtins() -> Self {
        let mut env = Self::new();

        // Arithmetic: Number → Number → Number (auto-promotion handled at runtime)
        // We use Number instead of Any to allow the type checker to catch non-numeric arguments
        for name in ["+", "-", "*"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![Type::Number, Type::Number],
                    ret: Box::new(Type::Number),
                    variadic: false,
                },
            );
        }

        // Division: always returns Float
        env.insert(
            "/".to_string(),
            Type::Function {
                params: vec![Type::Number, Type::Number],
                ret: Box::new(Type::Float),
                variadic: false,
            },
        );

        // Comparison: Any → Any → Bool (cross-type comparison allowed at runtime)
        for name in ["=", "<"] {
            env.insert(
                name.to_string(),
                Type::Function {
                    params: vec![Type::Any, Type::Any],
                    ret: Box::new(Type::Bool),
                    variadic: false,
                },
            );
        }

        // Control flow: if takes Bool, returns Any (type depends on branches)
        env.insert(
            "if".to_string(),
            Type::Function {
                params: vec![Type::Bool, Type::Any, Type::Any],
                ret: Box::new(Type::Any),
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
                    Type::Any,
                ],
                ret: Box::new(Type::Record(Row {
                    fields: HashMap::new(),
                    tail: RowTail::RowVar("_append_r".to_string(), 0),
                })),
                variadic: false,
            },
        );

        // String operations
        env.insert(
            "str".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Str),
                variadic: true,
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
                params: vec![Type::Any],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "error".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "try".to_string(),
            Type::Function {
                params: vec![Type::Any, Type::Any],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "apply".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Record(Row {
                        fields: HashMap::new(),
                        tail: RowTail::RowVar("_dict".to_string(), 0),
                    }),
                ],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );

        // Convergence loop: until(pred, f, init) applies f until pred holds
        env.insert(
            "until".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Bool),
                        variadic: false,
                    },
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Any,
                ],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );

        // Type introspection
        env.insert(
            "type-of".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "int?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "float?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "num?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "str?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "bool?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "null?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "dict?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );
        env.insert(
            "fn?".to_string(),
            Type::Function {
                params: vec![Type::Any],
                ret: Box::new(Type::Bool),
                variadic: false,
            },
        );

        // I/O
        env.insert(
            "emit".to_string(),
            Type::Function {
                params: vec![Type::Str],
                // Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md
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
                ret: Box::new(Type::Any), // returns Str or Null
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
                // Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md
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
                // Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md
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
                ret: Box::new(Type::Any), // returns dict with cap and revoke fields
                variadic: false,
            },
        );
        env.insert(
            "revoke-cap".to_string(),
            Type::Function {
                params: vec![Type::DirCap],
                // Null — Type::Record(Row::Empty), see doc/whatif/null-semantics.md
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
                params: vec![Type::Any], // accepts Seq/Dict/Str of allowlist entries
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
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "include".to_string(),
            Type::Function {
                params: vec![Type::Str],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );

        // Sequences: primitives
        env.insert(
            "seq".to_string(),
            Type::Function {
                params: vec![Type::Any, Type::Any],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "head".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "tail".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "collect".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Any))],
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
                params: vec![Type::Any],
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
                params: vec![Type::Any],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "cycle".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "iterate".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Any,
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "unfold".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Any,
                ],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );

        // Sequences: transforms
        env.insert(
            "map".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Any,
                ],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "filter".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any],
                        ret: Box::new(Type::Bool),
                        variadic: false,
                    },
                    Type::Any,
                ],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "take".to_string(),
            Type::Function {
                params: vec![Type::Int, Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );
        env.insert(
            "drop".to_string(),
            Type::Function {
                params: vec![Type::Int, Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );

        // Sequences: reductions
        env.insert(
            "reduce".to_string(),
            Type::Function {
                params: vec![
                    Type::Function {
                        params: vec![Type::Any, Type::Any],
                        ret: Box::new(Type::Any),
                        variadic: false,
                    },
                    Type::Any,
                    Type::Seq(Box::new(Type::Any)),
                ],
                ret: Box::new(Type::Any),
                variadic: false,
            },
        );
        env.insert(
            "join".to_string(),
            Type::Function {
                params: vec![Type::Str, Type::Seq(Box::new(Type::Any))],
                ret: Box::new(Type::Str),
                variadic: false,
            },
        );
        env.insert(
            "concat".to_string(),
            Type::Function {
                params: vec![Type::Seq(Box::new(Type::Seq(Box::new(Type::Any))))],
                ret: Box::new(Type::Seq(Box::new(Type::Any))),
                variadic: false,
            },
        );

        // List operations (moved from LLT stdlib to Rust for performance)
        // rest: Dict → Dict (removes first entry, reindexes)
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
        // cons: Any → Dict → Dict (prepends element, reindexes)
        env.insert(
            "cons".to_string(),
            Type::Function {
                params: vec![
                    Type::Any,
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
        // reverse: Dict → Dict (reverses insertion order, reindexes)
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
        // sort: Dict → Dict (natural ordering, O(n log n))
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
                    ret: Box::new(Type::Any),
                    variadic: false,
                }],
                ret: Box::new(Type::Proxy),
                variadic: false,
            },
        );

        // Capability and handle types: register as type aliases so @DirCap, @NetCap, @Handle
        // are valid in user annotations.
        env.insert_type_alias("DirCap".to_string(), Type::DirCap);
        env.insert_type_alias("NetCap".to_string(), Type::NetCap);
        env.insert_type_alias("Handle".to_string(), Type::Handle);

        // builtin-* aliases: same types as canonical counterparts.
        // Used by stdlib/prelude to call builtins when canonical names may be shadowed.
        for (alias, canonical) in [
            ("builtin-get", "get"),
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

    /// Extend a TypeEnv with prelude names, binding each to its inferred type.
    ///
    /// This suppresses "undefined variable" type errors for prelude functions
    /// in LSP contexts where the prelude AST is not in scope but the prelude
    /// environment is loaded at runtime.
    ///
    /// Takes a TypeMap from the prelude index and extracts types for top-level
    /// bindings. Falls back to Type::Any for names without type information.
    pub fn with_prelude_types(
        &self,
        name_to_span: &std::collections::HashMap<String, crate::ast::Span>,
        type_map: &crate::typecheck::TypeMap,
    ) -> Self {
        let mut env = self.clone();
        for (name, span) in name_to_span {
            let key = (span.start.offset, span.end.offset);
            let ty = type_map.get(&key).cloned().unwrap_or(Type::Any);
            env.insert(name.clone(), ty);
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
        // Emit name as-is — `%`-prefixed refs include `%`; plain identifiers display without sigil.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::test_span;

    // Helper to create closed records in tests
    fn closed_record(fields: HashMap<String, Type>) -> Type {
        Type::Record(Row {
            fields,
            tail: RowTail::Empty,
        })
    }

    // Helper to create open records with row variable
    fn row_var_record(fields: HashMap<String, Type>, var_name: &str, level: u32) -> Type {
        Type::Record(Row {
            fields,
            tail: RowTail::RowVar(var_name.to_string(), level),
        })
    }

    #[test]
    fn test_display_primitives() {
        assert_eq!(format!("{}", Type::Int), "Int");
        assert_eq!(format!("{}", Type::Float), "Float");
        assert_eq!(format!("{}", Type::Str), "String");
        assert_eq!(format!("{}", Type::Bool), "Bool");
        assert_eq!(format!("{}", Type::Number), "Number");
        assert_eq!(format!("{}", Type::Any), "Any");
    }

    #[test]
    fn test_display_int_literal() {
        assert_eq!(format!("{}", Type::IntLiteral(42)), "42");
    }

    #[test]
    fn test_display_string_literal() {
        assert_eq!(
            format!("{}", Type::StringLiteral("hello".into())),
            "\"hello\""
        );
    }

    #[test]
    fn test_display_type_var() {
        assert_eq!(format!("{}", Type::TypeVar("a".into(), 0)), "a");
    }

    #[test]
    fn test_display_record() {
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        fields.insert("age".into(), Type::Int);
        // Fields are sorted alphabetically for deterministic output (HashMap has no insertion order)
        assert_eq!(
            format!("{}", closed_record(fields)),
            "[age: Int name: String]"
        );
    }

    #[test]
    fn test_display_record_empty() {
        assert_eq!(format!("{}", closed_record(HashMap::new())), "[]");
    }

    #[test]
    fn test_display_record_open() {
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!("{}", row_var_record(fields, "_open", 0)),
            "[name: String ...]"
        );
    }

    #[test]
    fn test_display_record_open_empty() {
        assert_eq!(
            format!("{}", row_var_record(HashMap::new(), "_open", 0)),
            "[...]"
        );
    }

    #[test]
    fn test_display_record_row_var() {
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!("{}", row_var_record(fields, "rest", 0)),
            "[name: String ...rest]"
        );
    }

    #[test]
    fn test_display_function() {
        let ty = Type::Function {
            params: vec![Type::Int, Type::Str],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@Bool [Int String]");
    }

    #[test]
    fn test_display_function_no_params() {
        let ty = Type::Function {
            params: vec![],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert_eq!(format!("{ty}"), "Fn@Int []");
    }

    #[test]
    fn test_subtype_same() {
        assert!(Type::is_subtype(&Type::Int, &Type::Int));
        assert!(Type::is_subtype(&Type::Str, &Type::Str));
    }

    #[test]
    fn test_subtype_any_bypass() {
        assert!(Type::is_subtype(&Type::Any, &Type::Int));
        assert!(Type::is_subtype(&Type::Int, &Type::Any));
        assert!(Type::is_subtype(&Type::Any, &Type::Any));
    }

    #[test]
    fn test_subtype_int_literal() {
        assert!(Type::is_subtype(
            &Type::IntLiteral(42),
            &Type::IntLiteral(42)
        ));
        assert!(!Type::is_subtype(
            &Type::IntLiteral(42),
            &Type::IntLiteral(99)
        ));
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::Int));
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::Number));
        assert!(!Type::is_subtype(&Type::Int, &Type::IntLiteral(42)));
    }

    #[test]
    fn test_subtype_string_literal() {
        assert!(Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::StringLiteral("a".into())
        ));
        assert!(!Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::StringLiteral("b".into())
        ));
        assert!(Type::is_subtype(
            &Type::StringLiteral("a".into()),
            &Type::Str
        ));
        assert!(!Type::is_subtype(
            &Type::Str,
            &Type::StringLiteral("a".into())
        ));
    }

    #[test]
    fn test_subtype_number() {
        assert!(Type::is_subtype(&Type::Int, &Type::Number));
        assert!(Type::is_subtype(&Type::Float, &Type::Number));
        assert!(!Type::is_subtype(&Type::Number, &Type::Int));
        assert!(!Type::is_subtype(&Type::Str, &Type::Number));
    }

    #[test]
    fn test_subtype_record_structural() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);
        sub.insert("extra".into(), Type::Bool);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_record_missing_field() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(!Type::is_subtype(&closed_record(sub), &closed_record(sup),));
    }

    #[test]
    fn test_subtype_closed_record_extra_field_rejected() {
        // Closed sub with extra field should NOT be subtype of closed sup
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[a: Int, b: Int] should NOT be subtype of [a: Int] (Closed)"
        );
    }

    #[test]
    fn test_subtype_closed_record_same_fields_ok() {
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] should be subtype of [a: Int] (both Closed)"
        );
    }

    #[test]
    fn test_subtype_closed_to_row_var() {
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = closed_record(sub_fields);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = row_var_record(sup_fields, "r", 0);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] (Closed) should be subtype of [a: Int ...r] (RowVar)"
        );
    }

    #[test]
    fn test_subtype_row_var_to_closed() {
        let mut sub_fields = HashMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = row_var_record(sub_fields, "r", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[a: Int, b: Int ...r] (RowVar) should NOT be subtype of [a: Int] (Closed)"
        );
    }

    #[test]
    fn test_subtype_function_covariant_return() {
        let sub = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
            variadic: false,
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_is_subtype_open_record_not_subtype_of_closed() {
        // Sound pre-unification: open record with RowVar tail cannot satisfy a closed supertype.
        // Rémy (1994): the row variable may instantiate with additional fields the closed type rejects.
        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let open = row_var_record(a_fields.clone(), "r", 0);
        let closed = closed_record(a_fields);

        assert!(
            !Type::is_subtype(&open, &closed),
            "[a:Int ...r] (RowVar) should NOT be subtype of [a:Int] (closed)"
        );
    }

    #[test]
    fn test_is_subtype_closed_record_subtype_of_closed() {
        // Closed record with exact same fields IS a subtype of a closed record.
        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let sub = closed_record(a_fields.clone());
        let sup = closed_record(a_fields);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a:Int] (closed) should be subtype of [a:Int] (closed) — same fields"
        );
    }

    #[test]
    fn test_is_subtype_open_record_subtype_of_open() {
        // Open record (RowVar tail) IS a subtype of another open record with the same fields.
        // Both have RowVar tails — the sup is open so extra fields are acceptable.
        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);
        let sub = row_var_record(fields.clone(), "r1", 0);
        let sup = row_var_record(fields, "r2", 0);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a:Int ...r1] (RowVar) should be subtype of [a:Int ...r2] (RowVar) — sup is open"
        );
    }

    #[test]
    fn test_subtype_function_contravariant_params() {
        let sub = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_function_arity_mismatch() {
        let sub = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let sup = Type::Function {
            params: vec![Type::Int, Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(!Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_different_kinds() {
        assert!(!Type::is_subtype(&Type::Int, &Type::Str));
        assert!(!Type::is_subtype(&Type::Bool, &Type::Float));
        assert!(!Type::is_subtype(
            &Type::Int,
            &closed_record(HashMap::new())
        ));
    }

    #[test]
    fn test_subtype_type_var() {
        assert!(Type::is_subtype(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("a".into(), 0)
        ));
        assert!(!Type::is_subtype(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("b".into(), 0)
        ));
    }

    #[test]
    fn test_subtype_nested_record() {
        let mut inner_sub = HashMap::new();
        inner_sub.insert("x".into(), Type::Int);
        inner_sub.insert("y".into(), Type::Int);
        let mut outer_sub = HashMap::new();
        outer_sub.insert("point".into(), closed_record(inner_sub));

        let mut inner_sup = HashMap::new();
        inner_sup.insert("x".into(), Type::Number);
        let mut outer_sup = HashMap::new();
        outer_sup.insert("point".into(), row_var_record(inner_sup, "_open", 0));

        assert!(Type::is_subtype(
            &closed_record(outer_sub),
            &row_var_record(outer_sup, "_open", 0)
        ));
    }

    #[test]
    fn test_subtype_number_reflexive() {
        assert!(Type::is_subtype(&Type::Number, &Type::Number));
    }

    #[test]
    fn test_subtype_closed_sub_open_sup() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_closed_sub_closed_sup_extra_fields_rejected() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(!Type::is_subtype(&closed_record(sub), &closed_record(sup),));
    }

    #[test]
    fn test_subtype_closed_exact_match() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(&closed_record(sub), &closed_record(sup),));
    }

    #[test]
    fn test_subtype_open_sub_open_sup() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &row_var_record(sub, "_open", 0),
            &row_var_record(sup, "_open", 0),
        ));
    }

    #[test]
    fn test_subtype_row_var_behaves_like_open() {
        let mut sub = HashMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = HashMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &closed_record(sub),
            &row_var_record(sup, "r", 0),
        ));
    }

    #[test]
    fn test_subtype_open_sub_closed_sup_fewer_fields_rejected() {
        // Open sub with FEWER known fields than Closed sup must be rejected.
        // Old code: sub_fields ⊆ sup_fields → true (wrong).
        // New code: bidirectional check → sup field "age" not in sub → false (correct).
        //
        // sub: [name: Str | Open]  (may have additional unknown fields)
        // sup: [name: Str, age: Int | Closed]  (must have exactly name + age)
        let mut sub_fields = HashMap::new();
        sub_fields.insert("name".into(), Type::Str);
        let sub = row_var_record(sub_fields, "_open", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("name".into(), Type::Str);
        sup_fields.insert("age".into(), Type::Int);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[name: Str | Open] should NOT be subtype of [name: Str, age: Int | Closed]: \
             sub is Open so may lack 'age'"
        );
    }

    #[test]
    fn test_subtype_open_sub_closed_sup_extra_fields_rejected() {
        // Open sub with MORE known fields than Closed sup must be rejected.
        // sub's extra field "age" is not in sup → bidirectional check fails.
        //
        // sub: [name: Str, age: Int | Open]
        // sup: [name: Str | Closed]
        let mut sub_fields = HashMap::new();
        sub_fields.insert("name".into(), Type::Str);
        sub_fields.insert("age".into(), Type::Int);
        let sub = row_var_record(sub_fields, "_open", 0);

        let mut sup_fields = HashMap::new();
        sup_fields.insert("name".into(), Type::Str);
        let sup = closed_record(sup_fields);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[name: Str, age: Int | Open] should NOT be subtype of [name: Str | Closed]: \
             sub has extra field 'age' not in Closed sup"
        );
    }

    /// Function subtyping is contravariant in params and covariant in return.
    /// Transitivity: if P <: Q and Q <: R, then P <: R.
    ///
    /// P = Fn(Number → Int)
    /// Q = Fn(Int → Int)
    /// R = Fn(Int → Number)
    ///
    /// P <: Q: contravariant param (Int <: Number ✓), covariant return (Int <: Int ✓).
    /// Q <: R: contravariant param (Int <: Int ✓),  covariant return (Int <: Number ✓).
    /// P <: R: contravariant param (Int <: Number ✓), covariant return (Int <: Number ✓).
    #[test]
    fn test_function_variance_transitivity() {
        let p = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let q = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let r = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Number),
            variadic: false,
        };

        assert!(
            Type::is_subtype(&p, &q),
            "P <: Q should hold (contravariant param, covariant return)"
        );
        assert!(
            Type::is_subtype(&q, &r),
            "Q <: R should hold (covariant return Int <: Number)"
        );
        assert!(
            Type::is_subtype(&p, &r),
            "P <: R should hold by transitivity"
        );
    }

    /// Function subtyping is NOT symmetric: the contravariance of params means
    /// Fn(A → B) <: Fn(A' → B') does not imply Fn(A' → B') <: Fn(A → B).
    ///
    /// This is a sanity check that the transitivity test above is testing
    /// a genuine directional constraint, not accidental reflexivity.
    #[test]
    fn test_function_variance_not_symmetric() {
        // Fn(Number → Int) <: Fn(Int → Int) but NOT vice versa
        let broader_param = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let narrower_param = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        assert!(
            Type::is_subtype(&broader_param, &narrower_param),
            "Fn(Number → Int) should be a subtype of Fn(Int → Int)"
        );
        assert!(!Type::is_subtype(&narrower_param, &broader_param),
            "Fn(Int → Int) should NOT be a subtype of Fn(Number → Int): param Number is not a subtype of Int");
    }

    #[test]
    fn test_has_inference_vars_primitive() {
        assert!(!Type::Int.has_inference_vars());
        assert!(!Type::Str.has_inference_vars());
        assert!(!Type::Any.has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_type_var() {
        assert!(Type::TypeVar("a".into(), 0).has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_function() {
        let with = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::Int),
            variadic: false,
        };
        let without = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        assert!(with.has_inference_vars());
        assert!(!without.has_inference_vars());
    }

    #[test]
    fn test_has_inference_vars_record() {
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        assert!(closed_record(fields).has_inference_vars());
    }

    #[test]
    fn test_collect_type_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut vars = HashSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
        assert_eq!(vars.len(), 2);
    }

    #[test]
    fn test_collect_all_vars() {
        // TypeVar produces type_vars only
        let ty = Type::TypeVar("a".into(), 0);
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("a"));
        assert!(row_vars.is_empty());

        // Record with RowVar tail produces both type_vars and row_vars
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("t1".into(), 0));
        fields.insert("y".into(), Type::Int);
        let ty = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("r1".into(), 0),
        });
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("t1"));
        assert!(row_vars.contains("r1"));
        assert_eq!(type_vars.len(), 1);
        assert_eq!(row_vars.len(), 1);

        // Function type produces type_vars from params and return
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("c".into(), 0)),
            variadic: false,
        };
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("a"));
        assert!(type_vars.contains("b"));
        assert!(type_vars.contains("c"));
        assert!(row_vars.is_empty());
        assert_eq!(type_vars.len(), 3);

        // Seq type produces type_vars from element type
        let ty = Type::Seq(Box::new(Type::TypeVar("elem".into(), 0)));
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        ty.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.contains("elem"));
        assert!(row_vars.is_empty());

        // Ground types produce empty sets
        for ty in [
            Type::Int,
            Type::Str,
            Type::Bool,
            Type::Float,
            Type::Number,
            Type::Any,
        ] {
            let mut type_vars = HashSet::new();
            let mut row_vars = HashSet::new();
            ty.collect_all_vars(&mut type_vars, &mut row_vars);
            assert!(type_vars.is_empty());
            assert!(row_vars.is_empty());
        }
    }

    #[test]
    fn test_env_get_current() {
        let mut env = TypeEnv::new();
        env.insert("x".into(), Type::Int);
        assert_eq!(env.get("x").map(|s| &s.body), Some(&Type::Int));
    }

    #[test]
    fn test_env_get_parent() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let child = TypeEnv::with_parent(&parent_rc);
        assert_eq!(child.get("x").map(|s| &s.body), Some(&Type::Int));
    }

    #[test]
    fn test_env_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        child.insert("x".into(), Type::Str);
        assert_eq!(child.get("x").map(|s| &s.body), Some(&Type::Str));
    }

    #[test]
    fn test_env_missing() {
        let env = TypeEnv::new();
        assert_eq!(env.get("x"), None);
    }

    #[test]
    fn test_env_type_alias() {
        let mut env = TypeEnv::new();
        let mut fields = HashMap::new();
        fields.insert("name".into(), Type::Str);
        env.insert_type_alias("Person".into(), closed_record(fields.clone()));
        assert_eq!(env.get_type_alias("Person"), Some(&closed_record(fields)));
    }

    #[test]
    fn test_env_type_alias_parent() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias("Base".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let child = TypeEnv::with_parent(&parent_rc);
        assert_eq!(child.get_type_alias("Base"), Some(&Type::Int));
    }

    #[test]
    fn test_env_type_alias_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias("T".into(), Type::Int);
        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        child.insert_type_alias("T".into(), Type::Str);
        assert_eq!(child.get_type_alias("T"), Some(&Type::Str));
    }

    #[test]
    fn test_with_builtins_registers_all_builtins() {
        let env = TypeEnv::with_builtins();

        // Arithmetic
        assert!(env.get("+").is_some());
        assert!(env.get("-").is_some());
        assert!(env.get("*").is_some());
        assert!(env.get("/").is_some());

        // Comparison
        assert!(env.get("=").is_some());
        assert!(env.get("<").is_some());

        // Control flow
        assert!(env.get("if").is_some());

        // Dict primitives
        assert!(env.get("keys").is_some());
        assert!(env.get("length").is_some());
        assert!(env.get("merge").is_some());
        assert!(env.get("append").is_some());

        // Sequences
        assert!(env.get("map").is_some());
        assert!(env.get("filter").is_some());
        assert!(env.get("reduce").is_some());

        // List operations (moved from LLT to Rust)
        assert!(env.get("rest").is_some());
        assert!(env.get("cons").is_some());
        assert!(env.get("reverse").is_some());
        assert!(env.get("sort").is_some());
    }

    #[test]
    fn test_with_builtins_arithmetic_signature() {
        let env = TypeEnv::with_builtins();
        let add_scheme = env.get("+").expect("+ should be registered");
        match &add_scheme.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], Type::Number);
                assert_eq!(params[1], Type::Number);
                assert_eq!(&**ret, &Type::Number);
            }
            other => panic!("expected Function type for +, got {other}"),
        }
    }

    #[test]
    fn test_with_builtins_division_returns_float() {
        let env = TypeEnv::with_builtins();
        let div_scheme = env.get("/").expect("/ should be registered");
        match &div_scheme.body {
            Type::Function { ret, .. } => {
                assert_eq!(&**ret, &Type::Float);
            }
            other => panic!("expected Function type for /, got {other}"),
        }
    }

    #[test]
    fn test_with_builtins_comparison_signature() {
        let env = TypeEnv::with_builtins();
        let eq_scheme = env.get("=").expect("= should be registered");
        match &eq_scheme.body {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_eq!(params.len(), 2);
                assert_eq!(params[0], Type::Any);
                assert_eq!(params[1], Type::Any);
                assert_eq!(&**ret, &Type::Bool);
            }
            other => panic!("expected Function type for =, got {other}"),
        }
    }

    #[test]
    fn test_type_error_display() {
        let span = test_span(3, 5, 3, 10);
        let err = TypeError::new("oops", span);
        assert_eq!(format!("{err}"), "oops at 3:5-3:10");
    }

    #[test]
    fn test_type_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::type_mismatch(&Type::Int, &Type::Str, span);
        assert_eq!(err.message, "cannot unify Int with String");
    }

    #[test]
    fn test_type_error_field_not_found() {
        let span = test_span(1, 1, 1, 5);
        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);
        let err = TypeError::field_not_found("b", &closed_record(fields), span);
        assert_eq!(err.message, "field 'b' not found in [a: Int]");
    }

    #[test]
    fn test_type_error_undefined_variable() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::undefined_variable("x", span);
        assert_eq!(err.message, "undefined variable: x");
    }

    #[test]
    fn test_type_error_undefined_type() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::undefined_type("Foo", span);
        assert_eq!(err.message, "undefined type: Foo");
    }

    #[test]
    fn test_type_error_not_a_record() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::not_a_record(&Type::Int, span);
        assert_eq!(err.message, "expected record type, got Int");
    }

    #[test]
    fn test_type_error_not_a_function() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::not_a_function(&Type::Str, span);
        assert_eq!(err.message, "expected function type, got String");
    }

    #[test]
    fn test_substitution_empty_apply() {
        let subst = Substitution::new();
        assert_eq!(subst.apply(&Type::Int), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_bound() {
        let mut subst = Substitution::new();
        subst.type_map.insert("a".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_apply_chain() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.type_map.insert("b".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_idempotence() {
        // Verify that applying a substitution multiple times produces the same result.
        // This validates the claim in doc/05-type-annotations.md that substitution
        // application is idempotent.
        let mut subst = Substitution::new();
        subst
            .type_map
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.type_map.insert("b".into(), Type::Int);

        let ty = Type::TypeVar("a".into(), 0);
        let result_once = subst.apply(&ty);
        let result_twice = subst.apply(&result_once);

        // Both applications should produce the same result: Int
        assert_eq!(result_once, Type::Int);
        assert_eq!(result_twice, Type::Int);
        assert_eq!(result_once, result_twice);
    }

    #[test]
    fn test_substitution_apply_in_function() {
        let mut subst = Substitution::new();
        subst.type_map.insert("a".into(), Type::Int);
        subst.type_map.insert("b".into(), Type::Str);
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::TypeVar("b".into(), 0)),
            variadic: false,
        };
        assert_eq!(
            subst.apply(&ty),
            Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::Str),
                variadic: false,
            }
        );
    }

    #[test]
    fn test_substitution_apply_in_record() {
        let mut subst = Substitution::new();
        subst.type_map.insert("a".into(), Type::Int);
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        fields.insert("y".into(), Type::Str);
        let ty = closed_record(fields);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::Str);
        assert_eq!(subst.apply(&ty), closed_record(expected));
    }

    #[test]
    fn test_substitution_leaves_unbound_alone() {
        let mut subst = Substitution::new();
        subst.type_map.insert("a".into(), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("b".into(), 0)),
            Type::TypeVar("b".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_self_reference_cycle() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .insert("a".into(), Type::TypeVar("a".into(), 0));
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_indirect_cycle() {
        let mut subst = Substitution::new();
        subst
            .type_map
            .insert("a".into(), Type::TypeVar("b".into(), 0));
        subst
            .type_map
            .insert("b".into(), Type::TypeVar("a".into(), 0));
        // When we apply starting from "a", we get "a" back because:
        // a -> b (with a visited) -> a (already visited, return TypeVar("a"))
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_unify_identical_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Int, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Str, &Type::Str, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Bool, &Type::Bool, &mut subst, &mut state, span).is_ok());
    }

    #[test]
    fn test_unify_typevar_with_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.get("a"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_concrete_with_typevar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::Int,
            &Type::TypeVar("a".into(), 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.get("a"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_two_typevars() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("b".into(), 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        let resolved = subst.apply(&Type::TypeVar("a".into(), 0));
        assert_eq!(resolved, subst.apply(&Type::TypeVar("b".into(), 0)));
    }

    #[test]
    fn test_unify_typevar_already_bound_compatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
    }

    #[test]
    fn test_unify_typevar_already_bound_incompatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Str,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::TypeVar("b".into(), 0)),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        unify(&f1, &f2, &mut subst, &mut state, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into(), 0)), Type::Str);
    }

    #[test]
    fn test_unify_function_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![Type::Int, Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let result = unify(&f1, &f2, &mut subst, &mut state, span);
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("arity mismatch"));
    }

    #[test]
    fn test_unify_record_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::TypeVar("a".into(), 0));
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_unify_closed_record_extra_fields_rejected() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("extra fields"));
    }

    #[test]
    fn test_unify_open_record_extra_fields_accepted() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        unify(
            &row_var_record(f1, "_open", 0),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        // Verify that _open was bound to {y: Str, Empty} — not just is_ok()
        let binding = subst.row_map.get("_open").expect("_open should be bound");
        assert_eq!(binding.tail, RowTail::Empty);
        assert_eq!(binding.fields.get("y"), Some(&Type::Str));
        assert_eq!(binding.fields.len(), 1, "only 'y' should be in the binding");
    }

    #[test]
    fn test_unify_any_with_anything() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Any, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Str, &Type::Any, &mut subst, &mut state, span).is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_int() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Int,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
        assert!(unify(
            &Type::Int,
            &Type::IntLiteral(99),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_number() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Number,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_string_literal_with_string() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::StringLiteral("hi".into()),
            &Type::Str,
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
        assert!(unify(
            &Type::Str,
            &Type::StringLiteral("lo".into()),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_int_literal_different_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::IntLiteral(1),
            &Type::IntLiteral(2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("cannot unify"));
    }

    #[test]
    fn test_unify_int_literal_same_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::IntLiteral(42),
            &mut subst,
            &mut state,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_string_literal_different_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::StringLiteral("hello".into()),
            &Type::StringLiteral("world".into()),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("cannot unify"));
    }

    #[test]
    fn test_unify_string_literal_same_values() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::StringLiteral("hello".into()),
            &Type::StringLiteral("hello".into()),
            &mut subst,
            &mut state,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_incompatible_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(&Type::Int, &Type::Str, &mut subst, &mut state, span);
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_int_with_bool() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(&Type::Int, &Type::Bool, &mut subst, &mut state, span).is_err());
    }

    #[test]
    fn test_unify_int_literal_float_fails() {
        // Regression guard: IntLiteral is not a subtype of Float (different branches of the
        // numeric lattice: IntLiteral <: Int <: Number vs Float <: Number). The unsound
        // `(IntLiteral, Float)` promotion arm was removed; this test ensures it stays gone.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::IntLiteral(42),
            &Type::Float,
            &mut subst,
            &mut state,
            span
        )
        .is_err());
    }

    #[test]
    fn test_unify_float_with_int_literal_fails() {
        // Regression guard: symmetric case — Float is not a supertype of IntLiteral.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        assert!(unify(
            &Type::Float,
            &Type::IntLiteral(42),
            &mut subst,
            &mut state,
            span
        )
        .is_err());
    }

    #[test]
    fn test_unify_subsume_positive_path() {
        // [U-SUBSUME] positive-path coverage note:
        //
        // The [U-SUBSUME] arm fires for concrete (no type-var) pairs not matched by any
        // prior structural or explicit-promotion arm. With the current type vocabulary,
        // every valid subtype relationship already has a fast-path explicit arm:
        //   IntLiteral <: Int | Number  (unify line 1075)
        //   Int <: Number               (unify line 1075)
        //   Float <: Number             (unify line 1077)
        //   StringLiteral <: Str        (unify line 1078)
        //
        // This means [U-SUBSUME]'s positive branch (is_subtype returns true) is
        // unreachable with the current set of types.  The arm is a future extension
        // point: when a new subtype relationship is added to is_subtype() without a
        // corresponding explicit arm, [U-SUBSUME] will catch it automatically.
        //
        // The NEGATIVE branch (both concrete, neither is a subtype of the other) IS
        // exercised: pairs like (Int, Bool) or (Float, Bool) fall through all explicit
        // arms and reach [U-SUBSUME], which correctly rejects them.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Negative path through [U-SUBSUME]: concrete types with no subtype relation.
        // Neither Int <: Bool nor Bool <: Int, so [U-SUBSUME] rejects correctly.
        assert!(unify(&Type::Int, &Type::Bool, &mut subst, &mut state, span).is_err());

        // Another negative path through [U-SUBSUME]: Float <: Bool is also false.
        assert!(unify(&Type::Float, &Type::Bool, &mut subst, &mut state, span).is_err());
    }

    #[test]
    fn test_instantiate_no_vars() {
        let ty = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, ty);
        assert_eq!(counter, 0);
    }

    #[test]
    fn test_instantiate_with_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        assert!(!matches!(&result, Type::Function { params, .. }
            if params[0] == Type::TypeVar("a".into(), 0)));
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => assert_eq!(params[0], **ret),
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_multiple_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
            variadic: false,
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 2);
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                assert_ne!(params[0], params[1]);
                assert_eq!(params[0], **ret);
            }
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_counter_increments() {
        let ty = Type::TypeVar("x".into(), 0);
        let mut counter = 5;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, Type::TypeVar("_t5".into(), 0));
        assert_eq!(counter, 6);
    }

    #[test]
    fn test_unify_nested_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f1 = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::Function {
                params: vec![Type::TypeVar("a".into(), 0)],
                ret: Box::new(Type::TypeVar("b".into(), 0)),
                variadic: false,
            }),
            variadic: false,
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::Str),
                variadic: false,
            }),
            variadic: false,
        };
        unify(&f1, &f2, &mut subst, &mut state, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into(), 0)), Type::Str);
    }

    #[test]
    fn test_unify_occurs_check_direct() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Function {
                params: vec![Type::TypeVar("a".into(), 0)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_nested() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &closed_record(fields),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_reverse() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Function {
                params: vec![Type::TypeVar("a".into(), 0)],
                ret: Box::new(Type::Int),
                variadic: false,
            },
            &Type::TypeVar("a".into(), 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_substitution_apply_row_var_to_record() {
        let mut subst = Substitution::new();
        let mut extra = HashMap::new();
        extra.insert("y".into(), Type::Str);
        subst.row_map.insert(
            "r".into(),
            Row {
                fields: extra,
                tail: RowTail::Empty,
            },
        );

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields, "r", 0);
        let result = subst.apply(&ty);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::Str);
        assert_eq!(result, closed_record(expected));
    }

    #[test]
    fn test_substitution_apply_row_var_to_row_var() {
        let mut subst = Substitution::new();
        // Bind row variable "r" to a row with just a row variable "s" tail
        let empty_fields = HashMap::new();
        subst.row_map.insert(
            "r".into(),
            Row {
                fields: empty_fields,
                tail: RowTail::RowVar("s".into(), 0),
            },
        );

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields, "r", 0);
        let result = subst.apply(&ty);

        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        assert_eq!(result, row_var_record(expected, "s", 0));
    }

    #[test]
    fn test_substitution_apply_row_var_unbound() {
        let subst = Substitution::new();
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields.clone(), "r", 0);
        let result = subst.apply(&ty);
        assert_eq!(result, row_var_record(fields, "r", 0));
    }

    #[test]
    fn test_substitution_apply_row_var_duplicate_field() {
        // When a row variable binding contains a key that also exists in the original record,
        // the original (explicit) field must take precedence over the row-variable-bound field.
        let mut subst = Substitution::new();
        // r is bound to { x: Str, z: Bool } — 'x' collides with original record
        let mut extra = HashMap::new();
        extra.insert("x".into(), Type::Str); // collides with original x: Int
        extra.insert("z".into(), Type::Bool);
        subst.row_map.insert(
            "r".into(),
            Row {
                fields: extra,
                tail: RowTail::Empty,
            },
        );

        // original record: { x: Int ...r }
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields, "r", 0);

        let result = subst.apply(&ty);

        // Expected: { x: Int, z: Bool } — x stays Int (explicit wins), z is spliced in
        let mut expected = HashMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("z".into(), Type::Bool);
        assert_eq!(result, closed_record(expected));
    }

    #[test]
    fn test_unify_closed_records_same_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::Str);
        assert!(unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_closed_records_different_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("b".into(), Type::Int);
        let result = unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("extra fields"));
    }

    #[test]
    fn test_display_seq() {
        assert_eq!(format!("{}", Type::Seq(Box::new(Type::Int))), "Seq[Int]");
        assert_eq!(
            format!("{}", Type::Seq(Box::new(Type::TypeVar("a".into(), 0)))),
            "Seq[a]"
        );
    }

    #[test]
    fn test_subtype_seq_covariant() {
        assert!(Type::is_subtype(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Seq(Box::new(Type::Number)),
        ));
        assert!(!Type::is_subtype(
            &Type::Seq(Box::new(Type::Number)),
            &Type::Seq(Box::new(Type::Int)),
        ));
    }

    #[test]
    fn test_subtype_seq_same() {
        assert!(Type::is_subtype(
            &Type::Seq(Box::new(Type::Str)),
            &Type::Seq(Box::new(Type::Str)),
        ));
    }

    #[test]
    fn test_subtype_seq_vs_other() {
        assert!(!Type::is_subtype(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Int,
        ));
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::Seq(Box::new(Type::Int)),
        ));
    }

    #[test]
    fn test_has_inference_vars_seq() {
        assert!(Type::Seq(Box::new(Type::TypeVar("a".into(), 0))).has_inference_vars());
        assert!(!Type::Seq(Box::new(Type::Int)).has_inference_vars());
    }

    #[test]
    fn test_collect_type_vars_seq() {
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        let mut vars = HashSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_substitution_apply_seq() {
        let mut subst = Substitution::new();
        subst.type_map.insert("a".into(), Type::Int);
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        assert_eq!(subst.apply(&ty), Type::Seq(Box::new(Type::Int)));
    }

    #[test]
    fn test_unify_seq_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        unify(
            &Type::Seq(Box::new(Type::TypeVar("a".into(), 0))),
            &Type::Seq(Box::new(Type::Int)),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_unify_seq_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Seq(Box::new(Type::Str)),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_seq_vs_non_seq() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::Seq(Box::new(Type::Int)),
            &Type::Int,
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_occurs_check_seq() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Seq(Box::new(Type::TypeVar("a".into(), 0))),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_instantiate_seq() {
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        match &result {
            Type::Seq(elem) => assert_eq!(**elem, Type::TypeVar("_t0".into(), 0)),
            _ => panic!("expected Seq"),
        }
    }

    // --- TypeVar/RowVar level semantics ---

    #[test]
    fn test_typevar_eq_ignores_level() {
        // [U-REFL]: same name = equal regardless of level
        assert_eq!(Type::TypeVar("a".into(), 0), Type::TypeVar("a".into(), 5));
    }

    #[test]
    fn test_u_refl_fast_path_level_blind() {
        // Verify that unify() returns Ok(()) via the [U-REFL] fast path (line: `if a == b`)
        // when both sides are the same TypeVar name but with different levels.
        // TypeVar PartialEq is name-only, so ("a", level=0) == ("a", level=3), triggering
        // the fast path before any match arm is reached. The substitution must remain empty.
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 3);
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::TypeVar("a".into(), 3),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "same-name TypeVar with different levels should unify via [U-REFL]"
        );
        assert!(
            subst.type_map.is_empty(),
            "fast path must not bind anything in the substitution"
        );
    }

    #[test]
    fn test_typevar_neq_different_name() {
        assert_ne!(Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0));
    }

    #[test]
    fn test_typevar_display_hides_level() {
        assert_eq!(format!("{}", Type::TypeVar("a".into(), 5)), "a");
    }

    #[test]
    fn test_rowvar_eq_ignores_level() {
        assert_eq!(
            RowTail::RowVar("r".into(), 0),
            RowTail::RowVar("r".into(), 7)
        );
    }

    #[test]
    fn test_rowvar_neq_different_name() {
        assert_ne!(
            RowTail::RowVar("r".into(), 0),
            RowTail::RowVar("s".into(), 0)
        );
    }

    #[test]
    fn test_rowvar_display_hides_level() {
        // RowVar appears in record display as "...name"
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields, "r", 99);
        assert_eq!(format!("{ty}"), "[x: Int ...r]");
    }

    #[test]
    fn test_rowtail_eq_empty_both() {
        assert_eq!(RowTail::Empty, RowTail::Empty);
    }

    #[test]
    fn test_rowtail_neq_empty_vs_rowvar() {
        assert_ne!(RowTail::Empty, RowTail::RowVar("x".into(), 0));
        assert_ne!(RowTail::RowVar("x".into(), 0), RowTail::Empty);
    }

    // --- TypeScheme ---

    #[test]
    fn test_type_scheme_mono_empty_vars() {
        let scheme = TypeScheme::mono(Type::Int);
        assert!(scheme.type_vars.is_empty());
        assert!(scheme.row_vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_type_scheme_mono_wraps_body() {
        let body = Type::Function {
            params: vec![Type::Str],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let scheme = TypeScheme::mono(body.clone());
        assert!(scheme.type_vars.is_empty());
        assert!(scheme.row_vars.is_empty());
        assert_eq!(scheme.body, body);
    }

    #[test]
    fn test_type_scheme_display_monomorphic() {
        let scheme = TypeScheme::mono(Type::Int);
        assert_eq!(format!("{scheme}"), "Int");
    }

    #[test]
    fn test_type_scheme_display_polymorphic() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into(), "b".into()],
            row_vars: vec![],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
                ret: Box::new(Type::TypeVar("a".into(), 0)),
                variadic: false,
            },
        };
        assert_eq!(format!("{scheme}"), "∀a b. Fn@a [a b]");
    }

    #[test]
    fn test_type_scheme_display_single_var() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        assert_eq!(format!("{scheme}"), "∀a. a");
    }

    #[test]
    fn test_type_scheme_partial_eq_same() {
        let s1 = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        let s2 = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_different_vars() {
        let s1 = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::Int,
        };
        let s2 = TypeScheme {
            type_vars: vec!["b".into()],
            row_vars: vec![],
            body: Type::Int,
        };
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_different_body() {
        let s1 = TypeScheme::mono(Type::Int);
        let s2 = TypeScheme::mono(Type::Str);
        assert_ne!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_mono_vs_poly() {
        let s1 = TypeScheme::mono(Type::TypeVar("a".into(), 0));
        let s2 = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        assert_ne!(s1, s2);
    }

    // --- InferState ---

    #[test]
    fn test_infer_state_new_defaults() {
        let state = InferState::new();
        assert_eq!(state.name_counter, 0);
        assert_eq!(state.level, 0);
        assert!(state.levels.is_empty());
    }

    #[test]
    fn test_infer_state_fresh_var_increments_counter() {
        let mut state = InferState::new();
        state.fresh_var();
        assert_eq!(state.name_counter, 1);
        state.fresh_var();
        assert_eq!(state.name_counter, 2);
    }

    #[test]
    fn test_infer_state_fresh_var_registers_in_levels() {
        let mut state = InferState::new();
        let tv = state.fresh_var();
        // The var name should appear in the levels map at the current level
        match &tv {
            Type::TypeVar(name, level) => {
                assert_eq!(*level, 0);
                assert_eq!(state.levels.get(name.as_str()), Some(&0));
            }
            _ => panic!("expected TypeVar"),
        }
    }

    #[test]
    fn test_infer_state_fresh_var_returns_type_var_at_current_level() {
        let mut state = InferState::new();
        state.level = 3;
        let tv = state.fresh_var();
        match tv {
            Type::TypeVar(name, level) => {
                assert_eq!(level, 3);
                assert_eq!(name, "_t0");
                assert_eq!(state.levels.get("_t0"), Some(&3));
            }
            _ => panic!("expected TypeVar"),
        }
    }

    #[test]
    fn test_infer_state_fresh_var_sequential_names() {
        let mut state = InferState::new();
        let tv0 = state.fresh_var();
        let tv1 = state.fresh_var();
        match (&tv0, &tv1) {
            (Type::TypeVar(n0, _), Type::TypeVar(n1, _)) => {
                assert_eq!(n0, "_t0");
                assert_eq!(n1, "_t1");
            }
            _ => panic!("expected TypeVars"),
        }
    }

    // --- TypeEnv::insert_scheme ---

    #[test]
    fn test_env_insert_scheme_stores_and_retrieves() {
        let mut env = TypeEnv::new();
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        env.insert_scheme("f".into(), scheme.clone());
        assert_eq!(env.get("f"), Some(&scheme));
    }

    #[test]
    fn test_env_insert_scheme_shadows_parent() {
        let mut parent = TypeEnv::new();
        let parent_scheme = TypeScheme::mono(Type::Int);
        parent.insert_scheme("x".into(), parent_scheme);

        let parent_rc = Rc::new(parent);
        let mut child = TypeEnv::with_parent(&parent_rc);
        let child_scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        child.insert_scheme("x".into(), child_scheme.clone());

        // Child shadows parent: child scheme should be returned
        assert_eq!(child.get("x"), Some(&child_scheme));
    }

    // --- instantiate_scheme ---

    #[test]
    fn test_instantiate_scheme_monomorphic() {
        let scheme = TypeScheme::mono(Type::Int);
        let mut state = InferState::new();
        state.level = 2;
        let result = instantiate_scheme(&scheme, 2, &mut state);
        assert_eq!(result, Type::Int);
        assert_eq!(state.name_counter, 0); // No fresh vars created
    }

    #[test]
    fn test_instantiate_scheme_polymorphic() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into(), "b".into()],
            row_vars: vec![],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 0)],
                ret: Box::new(Type::TypeVar("b".into(), 0)),
                variadic: false,
            },
        };
        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        // Should get fresh variables at level 3
        match &result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                match &params[0] {
                    Type::TypeVar(name, level) => {
                        assert_eq!(*level, 3);
                        assert!(name.starts_with("_t"));
                        assert_eq!(state.levels.get(name.as_str()), Some(&3));
                    }
                    _ => panic!("expected TypeVar in params"),
                }
                match &**ret {
                    Type::TypeVar(name, level) => {
                        assert_eq!(*level, 3);
                        assert!(name.starts_with("_t"));
                        assert_eq!(state.levels.get(name.as_str()), Some(&3));
                    }
                    _ => panic!("expected TypeVar in return"),
                }
            }
            _ => panic!("expected Function"),
        }
        assert_eq!(state.name_counter, 2); // Two fresh vars created
    }

    #[test]
    fn test_instantiate_scheme_creates_independent_instances() {
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        let mut state = InferState::new();

        let inst1 = instantiate_scheme(&scheme, 1, &mut state);
        let inst2 = instantiate_scheme(&scheme, 1, &mut state);

        // Should be different fresh variables
        assert_ne!(inst1, inst2);
    }

    #[test]
    fn test_instantiate_at_level_registers_vars_in_levels() {
        // Create a type scheme with a polymorphic variable
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::TypeVar("a".into(), 0),
        };
        let mut state = InferState::new();
        state.level = 2;

        // Instantiate the scheme
        let result = instantiate_scheme(&scheme, 2, &mut state);

        // The result should be a fresh type variable
        match result {
            Type::TypeVar(name, _) => {
                // Verify the fresh variable is registered in levels at the current level
                assert_eq!(
                    state.levels.get(&name),
                    Some(&2),
                    "instantiate_at_level must register fresh vars in state.levels at current level"
                );
            }
            other => panic!("expected TypeVar, got {other:?}"),
        }
    }

    #[test]
    fn test_instantiate_at_level_monomorphic_fast_path() {
        let mut state = InferState::new();
        let before_counter = state.name_counter;

        let result = instantiate_at_level(&Type::Int, &mut state);

        assert_eq!(result, Type::Int);
        assert_eq!(
            state.name_counter, before_counter,
            "monomorphic fast-path must not increment name_counter"
        );
    }

    // --- generalize ---

    #[test]
    fn test_generalize_no_vars() {
        let state = InferState::new();
        let ty = Type::Int;
        let scheme = generalize(0, &ty, &state);
        assert!(scheme.type_vars.is_empty());
        assert!(scheme.row_vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_generalize_var_at_higher_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);
        let ty = Type::TypeVar("a".into(), 2);
        let scheme = generalize(1, &ty, &state);
        assert_eq!(scheme.type_vars, vec!["a"]);
        assert!(scheme.row_vars.is_empty());
        assert_eq!(scheme.body, ty);
    }

    #[test]
    fn test_generalize_var_at_same_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        let ty = Type::TypeVar("a".into(), 1);
        let scheme = generalize(1, &ty, &state);
        // Level 1 is NOT > 1, so should not generalize
        assert!(scheme.type_vars.is_empty());
        assert!(scheme.row_vars.is_empty());
    }

    #[test]
    fn test_generalize_var_at_lower_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 0);
        let ty = Type::TypeVar("a".into(), 0);
        let scheme = generalize(1, &ty, &state);
        // Level 0 is NOT > 1, so should not generalize
        assert!(scheme.type_vars.is_empty());
        assert!(scheme.row_vars.is_empty());
    }

    #[test]
    fn test_generalize_multiple_vars_mixed_levels() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);
        state.levels.insert("b".into(), 1);
        state.levels.insert("c".into(), 3);
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 2), Type::TypeVar("b".into(), 1)],
            ret: Box::new(Type::TypeVar("c".into(), 3)),
            variadic: false,
        };
        let scheme = generalize(1, &ty, &state);
        // Only a (level 2 > 1) and c (level 3 > 1) should be generalized
        // b is at level 1, not > 1
        assert_eq!(scheme.type_vars.len(), 2);
        assert!(scheme.type_vars.contains(&"a".into()));
        assert!(scheme.type_vars.contains(&"c".into()));
        assert!(!scheme.type_vars.contains(&"b".into()));
        assert!(scheme.row_vars.is_empty());
    }

    #[test]
    fn test_generalize_row_vars() {
        let mut state = InferState::new();
        state.levels.insert("r".into(), 2);
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = row_var_record(fields, "r", 2);
        let scheme = generalize(1, &ty, &state);
        assert_eq!(scheme.row_vars, vec!["r"]);
        assert!(scheme.type_vars.is_empty());
    }

    #[test]
    fn test_generalize_applies_subst_before_collecting() {
        // Defense-in-depth test: generalize() must apply substitution first.
        // Without this, a TypeVar bound in state.subst would be incorrectly generalized.
        let mut state = InferState::new();

        // Create a type variable "a" at level 2 (higher than enclosing level 1)
        state.levels.insert("a".into(), 2);

        // Bind "a" to Int in the substitution
        state.subst.type_map.insert("a".into(), Type::Int);

        // Create a type containing the bound variable
        let ty = Type::TypeVar("a".into(), 2);

        // Generalize at level 1
        let scheme = generalize(1, &ty, &state);

        // The variable should NOT be generalized because it's bound to Int.
        // After applying substitution, the type is Int (no free vars).
        assert!(
            scheme.type_vars.is_empty(),
            "Bound TypeVar should not be generalized after substitution application"
        );
        assert!(scheme.row_vars.is_empty());
        assert_eq!(
            scheme.body,
            Type::Int,
            "Generalized type should be Int, not TypeVar"
        );
    }

    // --- level lowering in unify ---

    #[test]
    fn test_unify_level_lowering_symmetric() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        state.levels.insert("b".into(), 3);

        let mut subst = Substitution::new();
        // Unify a (level 1) with b (level 3)
        unify(
            &Type::TypeVar("a".into(), 1),
            &Type::TypeVar("b".into(), 3),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // b should be lowered to min(3, 1) = 1
        assert_eq!(state.levels.get("b"), Some(&1));
    }

    #[test]
    fn test_unify_level_lowering_in_complex_type() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        state.levels.insert("b".into(), 3);
        state.levels.insert("c".into(), 4);

        let mut subst = Substitution::new();
        let complex = Type::Function {
            params: vec![Type::TypeVar("b".into(), 3)],
            ret: Box::new(Type::TypeVar("c".into(), 4)),
            variadic: false,
        };

        // Unify a (level 1) with complex type containing b (3) and c (4)
        unify(
            &Type::TypeVar("a".into(), 1),
            &complex,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Both b and c should be lowered to 1
        assert_eq!(state.levels.get("b"), Some(&1));
        assert_eq!(state.levels.get("c"), Some(&1));
    }

    #[test]
    fn test_unify_any_with_typevar_zeros_level() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 3);

        let mut subst = Substitution::new();
        unify(
            &Type::Any,
            &Type::TypeVar("a".into(), 3),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Level should be set to 0 to prevent generalization
        assert_eq!(state.levels.get("a"), Some(&0));
    }

    #[test]
    fn test_unify_typevar_with_any_zeros_level() {
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);

        let mut subst = Substitution::new();
        unify(
            &Type::TypeVar("a".into(), 2),
            &Type::Any,
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // Level should be set to 0 to prevent generalization
        assert_eq!(state.levels.get("a"), Some(&0));
    }

    #[test]
    fn test_unify_any_with_function_zeros_contained_vars() {
        // unify(Any, Fn(TypeVar("b",3) → Int)) must zero b's level
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("b".into(), 3);

        let fn_ty = Type::Function {
            params: vec![Type::TypeVar("b".into(), 3)],
            ret: Box::new(Type::Int),
            variadic: false,
        };

        let mut subst = Substitution::new();
        unify(&Type::Any, &fn_ty, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("b"),
            Some(&0),
            "TypeVar inside Fn unified with Any must have level zeroed"
        );
    }

    #[test]
    fn test_unify_any_with_record_zeros_contained_vars() {
        // unify(Any, Record({x: TypeVar("c",2), ...ρ})) must zero both c and ρ
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("c".into(), 2);
        state.levels.insert("rho".into(), 2);

        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("c".into(), 2));
        let rec_ty = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("rho".into(), 2),
        });

        let mut subst = Substitution::new();
        unify(&Type::Any, &rec_ty, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("c"),
            Some(&0),
            "TypeVar inside Record unified with Any must have level zeroed"
        );
        assert_eq!(
            state.levels.get("rho"),
            Some(&0),
            "RowVar inside Record unified with Any must have level zeroed"
        );
    }

    #[test]
    fn test_unify_complex_with_any_zeros_contained_vars() {
        // Symmetric: unify(Fn(TypeVar("d",4) → Seq(TypeVar("e",4))), Any)
        let span = test_span(1, 1, 1, 5);
        let mut state = InferState::new();
        state.levels.insert("d".into(), 4);
        state.levels.insert("e".into(), 4);

        let fn_ty = Type::Function {
            params: vec![Type::TypeVar("d".into(), 4)],
            ret: Box::new(Type::Seq(Box::new(Type::TypeVar("e".into(), 4)))),
            variadic: false,
        };

        let mut subst = Substitution::new();
        unify(&fn_ty, &Type::Any, &mut subst, &mut state, span).unwrap();

        assert_eq!(
            state.levels.get("d"),
            Some(&0),
            "TypeVar in param unified with Any must have level zeroed"
        );
        assert_eq!(
            state.levels.get("e"),
            Some(&0),
            "TypeVar in Seq return unified with Any must have level zeroed"
        );
    }

    // --- Task 4: instantiate_scheme with row var body ---

    #[test]
    fn test_instantiate_scheme_with_row_var_body() {
        // Create a TypeScheme whose body is Record(fields, RowRest::RowVar("r", 1))
        // with row_vars: vec!["r"]
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let scheme = TypeScheme {
            type_vars: vec![],
            row_vars: vec!["r".into()],
            body: row_var_record(fields.clone(), "r", 1),
        };

        let mut state = InferState::new();
        state.level = 2;
        let result = instantiate_scheme(&scheme, 2, &mut state);

        // Verify the result has a FRESH RowVar (not the original "r")
        match result {
            Type::Record(Row {
                fields: result_fields,
                tail: row_rest,
            }) => {
                assert_eq!(result_fields, fields);
                match row_rest {
                    RowTail::RowVar(name, level) => {
                        // NOTE: This test may EXPOSE a bug where RowVars are instantiated as TypeVars
                        // The correct behavior is: RowVar → fresh RowVar
                        // If this fails, it documents a known issue with the current instantiate_scheme
                        assert!(
                            name.starts_with("_t"),
                            "row var should be freshly renamed, got {}",
                            name
                        );
                        assert_ne!(
                            name, "r",
                            "row var should not be the original 'r', got {}",
                            name
                        );
                        assert_eq!(level, 2, "row var should be at level 2");
                        assert_eq!(
                            state.levels.get(&name),
                            Some(&2),
                            "fresh row var should be registered in levels at level 2"
                        );
                    }
                    RowTail::Empty => panic!("expected RowVar in result, got Closed"),
                }
            }
            other => panic!("expected Record, got {:?}", other),
        }
    }

    // --- Task 5: instantiate_scheme leaves free vars unchanged ---

    #[test]
    fn test_instantiate_scheme_leaves_free_vars_unchanged() {
        // Create a TypeScheme with type_vars: vec!["a"] and body Function { params: [TypeVar("a", 1)], ret: TypeVar("b", 1) }
        // Only "a" is quantified; "b" is free
        let scheme = TypeScheme {
            type_vars: vec!["a".into()],
            row_vars: vec![],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 1)],
                ret: Box::new(Type::TypeVar("b".into(), 1)),
                variadic: false,
            },
        };

        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        match result {
            Type::Function {
                params,
                ret,
                variadic: _,
            } => {
                // "a" should get a fresh name (e.g., "_t0")
                match &params[0] {
                    Type::TypeVar(a_name, a_level) => {
                        assert!(
                            a_name.starts_with("_t"),
                            "quantified var 'a' should be renamed to fresh var, got {}",
                            a_name
                        );
                        assert_ne!(
                            a_name, "a",
                            "quantified var should not be 'a', got {}",
                            a_name
                        );
                        assert_eq!(*a_level, 3);
                    }
                    other => panic!("expected TypeVar in params, got {:?}", other),
                }

                // "b" should remain unchanged (it's free, not quantified)
                match ret.as_ref() {
                    Type::TypeVar(b_name, b_level) => {
                        assert_eq!(
                            b_name, "b",
                            "free var 'b' should be unchanged, got {}",
                            b_name
                        );
                        assert_eq!(*b_level, 1, "free var level should be unchanged");
                    }
                    other => panic!("expected TypeVar in return, got {:?}", other),
                }
            }
            other => panic!("expected Function, got {:?}", other),
        }
    }

    // --- Row unification algorithm tests (Cases 2/3/4 and occurs checks) ---

    /// Case 4: both sides have unique fields and both tails are RowVar
    /// Unify {a: Int, ...rho1} with {b: Str, ...rho2} → fresh row var created with dual bindings
    #[test]
    fn test_unify_remainders_case4_both_unique_both_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("b".into(), Type::Str);

        unify(
            &row_var_record(f1, "rho1", 0),
            &row_var_record(f2, "rho2", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho1 should be bound to {b: Str, ..._t0} (unique2 + fresh tail)
        let rho1_binding = subst.row_map.get("rho1").expect("rho1 should be bound");
        assert_eq!(rho1_binding.fields.get("b"), Some(&Type::Str));
        assert_eq!(rho1_binding.fields.len(), 1);
        let rho1_tail_name = match &rho1_binding.tail {
            RowTail::RowVar(name, _) => name.clone(),
            other => panic!("expected RowVar tail for rho1, got {:?}", other),
        };

        // rho2 should be bound to {a: Int, ..._t0} (unique1 + same fresh tail)
        let rho2_binding = subst.row_map.get("rho2").expect("rho2 should be bound");
        assert_eq!(rho2_binding.fields.get("a"), Some(&Type::Int));
        assert_eq!(rho2_binding.fields.len(), 1);
        let rho2_tail_name = match &rho2_binding.tail {
            RowTail::RowVar(name, _) => name.clone(),
            other => panic!("expected RowVar tail for rho2, got {:?}", other),
        };

        // Both bindings must share the same fresh row variable
        assert_eq!(
            rho1_tail_name, rho2_tail_name,
            "rho1 and rho2 bindings should share the same fresh row variable"
        );
    }

    /// Case 2: left has unique fields, right tail is RowVar, right has no unique fields
    /// Unify {a: Int, b: Str} (closed) with {a: Int, ...rho} → rho binds to {b: Str, Empty}
    #[test]
    fn test_unify_remainders_case2_left_unique_right_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to {b: Str, Empty}
        let binding = subst.row_map.get("rho").expect("rho should be bound");
        assert_eq!(binding.tail, RowTail::Empty);
        assert_eq!(binding.fields.get("b"), Some(&Type::Str));
        assert_eq!(binding.fields.len(), 1);
    }

    /// Case 3: right has unique fields, left tail is RowVar, left has no unique fields
    /// Unify {a: Int, ...rho} with {a: Int, b: Str} (closed) → rho binds to {b: Str, Empty}
    #[test]
    fn test_unify_remainders_case3_right_unique_left_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::Str);

        unify(
            &row_var_record(f1, "rho", 0),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to {b: Str, Empty}
        let binding = subst.row_map.get("rho").expect("rho should be bound");
        assert_eq!(binding.tail, RowTail::Empty);
        assert_eq!(binding.fields.get("b"), Some(&Type::Str));
        assert_eq!(binding.fields.len(), 1);
    }

    /// Soundness test from Finding 1: unifying closed {a: Int, b: Str} with open {a: Int, c: Bool, ...rho}
    /// must FAIL — unique2 has {c: Bool} but tail1 is Empty, so no way to absorb it.
    #[test]
    fn test_unify_closed_vs_open_unique_both_sides_fails() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("c".into(), Type::Bool);

        let result = unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "should fail: closed left cannot absorb unique2 {{c: Bool}}"
        );
        assert!(
            result.unwrap_err().message.contains("extra fields"),
            "error should mention extra fields"
        );
    }

    /// Row occurs check: direct tail cycle — ρ unified with row whose tail is ρ
    /// Setup: unify {a: Int, b: Str, ...rho} with {a: Int, b: Str, ...rho} is trivially ok,
    /// but unify {a: Int, b: Str, ...rho} with {a: Int, ...rho} (unique1={b:Str}, tail2=rho)
    /// creates binding rho → {b: Str, ...rho} — direct tail cycle, must fail.
    #[test]
    fn test_row_occurs_check_direct_tail_cycle() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        // Left: closed {a: Int, b: Str, ...rho}, right: open {a: Int, ...rho}
        // After shared field extraction: unique1={b:Str}, tail1=RowVar(rho), unique2={}, tail2=RowVar(rho)
        // Case 2: u1 non-empty, u2 empty, tail2=RowVar(rho) → bind rho → {b:Str, ...rho} — CYCLE!
        let result = unify(
            &row_var_record(f1, "rho", 0),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "should fail: rho binds to {{b: Str, ...rho}} which is an infinite row"
        );
        assert!(
            result.unwrap_err().message.contains("infinite row type"),
            "error should mention infinite row type"
        );
    }

    /// Row occurs check: nested-in-field cycle — ρ unified with row containing a field of type Record(ρ)
    /// Setup: left = {a: Int, x: Record({...rho})} (closed), right = {a: Int, ...rho} (open)
    /// After extracting shared field a: unique1={x:Record({...rho})}, tail1=Empty, unique2={}, tail2=RowVar(rho)
    /// Case 2: bind rho → {x: Record({...rho}), Empty} — rho occurs in a field type, infinite row!
    #[test]
    fn test_row_occurs_check_nested_in_field_cycle() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Build {x: Record({...rho}), a: Int} closed
        let nested_fields = HashMap::new(); // empty fields, tail is rho
        let nested_record = Type::Record(Row {
            fields: nested_fields,
            tail: RowTail::RowVar("rho".into(), 0),
        });
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("x".into(), nested_record);

        // Build {a: Int, ...rho}
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        let result = unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "should fail: rho occurs in field type Record with rho in tail"
        );
        assert!(
            result.unwrap_err().message.contains("infinite row type"),
            "error should mention infinite row type"
        );
    }

    /// Row variable name hiding in occurs-check errors: variables starting with `_` are
    /// displayed as "an anonymous open row" rather than their raw internal name.
    /// This tests the branch added in unify_remainders at the occurs-check sites.
    ///
    /// Setup mirrors test_row_occurs_check_direct_tail_cycle but uses `_open3` as the
    /// row variable name — the `_` prefix triggers the display-hiding branch.
    #[test]
    fn test_row_occurs_check_anonymous_row_display() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        // Left: {a: Int, b: Str, ..._open3}, right: {a: Int, ..._open3}
        // After shared field extraction: unique1={b:Str}, unique2={}, tail2=RowVar(_open3)
        // Case 2: bind _open3 → {b: Str, ..._open3} — CYCLE; error should say "an anonymous open row"
        let result = unify(
            &row_var_record(f1, "_open3", 0),
            &row_var_record(f2, "_open3", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "should fail: _open3 binds to {{b: Str, ..._open3}} — infinite row"
        );
        let err = result.unwrap_err();
        assert!(
            err.message.contains("infinite row type"),
            "error should mention infinite row type, got: {err}"
        );
        assert!(
            err.message.contains("an anonymous open row"),
            "error should display '_open3' as 'an anonymous open row', got: {err}"
        );
        assert!(
            !err.message.contains("_open3"),
            "raw internal name '_open3' must not appear in the error, got: {err}"
        );
    }

    /// Row occurs check chases TypeVar bindings through the substitution.
    /// If α is bound to Record({x: Int, ...ρ}), then row_var_occurs_in_type("ρ", TypeVar("α"), &subst)
    /// must return true — the row variable ρ is transitively reachable through α's binding.
    /// This prevents construction of infinite row types via indirect TypeVar references.
    /// See Robinson (1965): the occurs check must operate on substitution-applied types.
    #[test]
    fn test_row_occurs_check_chases_typevar_bindings() {
        let mut subst = Substitution::new();

        // Bind α → Record({x: Int}, RowVar("rho"))
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::Int);
        let bound_type = Type::Record(Row {
            fields,
            tail: RowTail::RowVar("rho".into(), 0),
        });
        subst.type_map.insert("alpha".into(), bound_type);

        // row_var_occurs_in_type("rho", TypeVar("alpha"), &subst) should be true
        let tv_alpha = Type::TypeVar("alpha".into(), 0);
        assert!(
            row_var_occurs_in_type("rho", &tv_alpha, &subst),
            "should detect rho transitively through alpha's binding"
        );

        // Negative case: unbound TypeVar should not claim to contain any row var
        let tv_beta = Type::TypeVar("beta".into(), 0);
        assert!(
            !row_var_occurs_in_type("rho", &tv_beta, &subst),
            "unbound TypeVar should not contain any row var"
        );

        // Negative case: bound TypeVar whose binding does NOT contain the target row var
        subst.type_map.insert(
            "gamma".into(),
            Type::Record(Row {
                fields: HashMap::new(),
                tail: RowTail::Empty,
            }),
        );
        let tv_gamma = Type::TypeVar("gamma".into(), 0);
        assert!(
            !row_var_occurs_in_type("rho", &tv_gamma, &subst),
            "TypeVar bound to type without rho should return false"
        );

        // row_var_occurs (row-level) should also chase through TypeVar fields
        let mut row_fields = HashMap::new();
        row_fields.insert("y".into(), Type::TypeVar("alpha".into(), 0));
        let row = Row {
            fields: row_fields,
            tail: RowTail::Empty,
        };
        assert!(
            row_var_occurs("rho", &row, &subst),
            "row_var_occurs should detect rho in field type via TypeVar chasing"
        );
    }

    // --- unify_tails binding tests (exercised via unify on records with no unique fields) ---

    /// Both tails are RowVar with the same name — must succeed (same variable, trivially ok)
    #[test]
    fn test_unify_tails_both_rowvar_same_name() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("x".into(), Type::Int);

        // Both have the same shared field and same row var — Case 1 → unify_tails(rho, rho) → Ok
        unify(
            &row_var_record(f1, "rho", 0),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should NOT be bound — same name is handled as reflexivity
        assert!(
            !subst.row_map.contains_key("rho"),
            "same-name RowVar unification should not create a binding"
        );
    }

    /// Both tails are RowVar with different names — rho1 must bind to rho2
    #[test]
    fn test_unify_tails_both_rowvar_different_names() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields on either side, different row vars → Case 1 → unify_tails(rho1, rho2)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &row_var_record(f1, "rho1", 0),
            &row_var_record(f2, "rho2", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho1 should be bound to Row { fields: {}, tail: RowVar("rho2") }
        let binding = subst.row_map.get("rho1").expect("rho1 should be bound");
        assert_eq!(binding.fields.len(), 0);
        assert_eq!(binding.tail, RowTail::RowVar("rho2".into(), 0));
    }

    /// Both tails are RowVar with different levels — test level minimization
    #[test]
    fn test_unify_tails_both_rowvar_level_minimization() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Set different levels for the two row variables
        state.levels.insert("rho1".into(), 2);
        state.levels.insert("rho2".into(), 4);

        // No unique fields on either side, different row vars → Case 1 → unify_tails(rho1, rho2)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &row_var_record(f1, "rho1", 2),
            &row_var_record(f2, "rho2", 4),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho1 should be bound to Row { fields: {}, tail: RowVar("rho2", 2) }
        let binding = subst.row_map.get("rho1").expect("rho1 should be bound");
        assert_eq!(binding.fields.len(), 0);
        assert_eq!(
            binding.tail,
            RowTail::RowVar("rho2".into(), 2),
            "tail should use min(2, 4) = 2"
        );

        // rho2's level in state.levels should be lowered to min(2, 4) = 2
        assert_eq!(
            state.levels.get("rho2").copied(),
            Some(2),
            "rho2 level should be lowered to min(rho1_level, rho2_level) = min(2, 4) = 2"
        );
    }

    /// RowVar vs Empty — RowVar must bind to Row { fields: {}, tail: Empty }
    #[test]
    fn test_unify_tails_rowvar_vs_empty() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields, left is open (rho), right is closed → Case 1 → unify_tails(rho, Empty)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &row_var_record(f1, "rho", 0),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to Row { fields: {}, tail: Empty }
        let binding = subst.row_map.get("rho").expect("rho should be bound");
        assert_eq!(binding.fields.len(), 0);
        assert_eq!(binding.tail, RowTail::Empty);
    }

    /// Both tails are Empty — must succeed with no bindings created
    #[test]
    fn test_unify_tails_both_empty() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &closed_record(f1),
            &closed_record(f2),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        assert!(
            subst.row_map.is_empty(),
            "no row bindings should be created"
        );
    }

    // =========================================================================
    // Consistency tests: unify() vs is_subtype() for all RowTail combinations
    //
    // Core invariant: if unify(A, B) succeeds producing substitution S, then
    //   is_subtype(S(A), S(B)) must hold (A <: B direction or B <: A).
    //
    // Contrapositive: when unify fails, the pre-unification is_subtype is also
    // false (or the asymmetry is documented as intentional).
    //
    // RowTail pair cases covered:
    //   1a/1b/1c  (Empty, Empty)           — both closed
    //   2 / 2b    (Empty, RowVar)          — closed sub, open sup
    //   3 / 3b/3c (RowVar, Empty)          — open sub, closed sup [conservative]
    //   4 / 4b    (RowVar(r1), RowVar(r2)) — different row vars
    //   5 / 5b    (RowVar(r), RowVar(r))   — same row var
    //   + field numeric promotion, nested record nesting
    // =========================================================================

    /// Case 1a: (Empty, Empty) identical fields — unify succeeds, subtype holds.
    ///
    /// A = [a: Int]  (closed)
    /// B = [a: Int]  (closed)
    ///
    /// unify: no bindings. S(A) = A, S(B) = B. is_subtype(A, B) = true.
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_identical() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);

        let a = closed_record(fields.clone());
        let b = closed_record(fields);

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify(A, B) succeeds for identical closed records"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify(A, B) succeeds for identical closed records"
        );
    }

    /// Case 1b: (Empty, Empty) sub has extra field — unify FAILS, is_subtype false.
    ///
    /// A = [a: Int, b: Str]  (closed)
    /// B = [a: Int]          (closed)
    ///
    /// Extra field "b" in A; closed B cannot absorb it. unify and is_subtype both reject.
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_extra_field() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = closed_record(b_fields);

        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "unify([a:Int,b:Str](closed), [a:Int](closed)) should fail"
        );

        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int,b:Str](closed) should NOT be subtype of [a:Int](closed)"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[a:Int](closed) should NOT be subtype of [a:Int,b:Str](closed)"
        );
    }

    /// Case 1c: (Empty, Empty) field type mismatch — unify FAILS, is_subtype false both ways.
    ///
    /// A = [a: Int]  (closed)
    /// B = [a: Str]  (closed)
    #[test]
    fn test_is_subtype_consistency_closed_vs_closed_field_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Str);
        let b = closed_record(b_fields);

        assert!(
            unify(&a, &b, &mut subst, &mut state, span).is_err(),
            "unify([a:Int](closed), [a:Str](closed)) should fail"
        );
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int] should NOT be subtype of [a:Str]"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[a:Str] should NOT be subtype of [a:Int]"
        );
    }

    /// Case 2: (Empty, RowVar) — closed sub, open sup with same fields.
    ///
    /// A = [a: Int]        (closed)
    /// B = [a: Int, ...r]  (open, RowVar "r")
    ///
    /// unify: no unique fields -> Case 1 -> unify_tails(Empty, RowVar(r)) -> r binds to Empty.
    /// Pre-unification: is_subtype(A, B) = true (sup is open RowVar — always lenient).
    /// Post-substitution: S(B) = [a: Int] = S(A), subtype holds both ways.
    #[test]
    fn test_is_subtype_consistency_closed_sub_open_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = closed_record(a_fields.clone());
        let b = row_var_record(a_fields, "r", 0);

        // Pre-unification: sup is open -> lenient
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int](closed) should be subtype of [a:Int ...r](RowVar): sup is open"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r_binding = subst
            .row_map
            .get("r")
            .expect("row var 'r' should be bound after unifying with closed record");
        assert_eq!(
            r_binding.tail,
            RowTail::Empty,
            "r should bind to closed tail after unifying with Empty"
        );
        assert_eq!(r_binding.fields.len(), 0, "r should have no extra fields");

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(
            sb, sa,
            "S([a:Int ...r]) should equal [a:Int] after r binds to Empty"
        );
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify succeeds"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify succeeds (symmetric post-bind)"
        );
    }

    /// Case 2b: (Empty, RowVar) — closed sub with extra fields, open sup with fewer fields.
    ///
    /// A = [a: Int, b: Str]  (closed)
    /// B = [a: Int, ...r]    (open, RowVar "r")
    ///
    /// unify: "b" unique to A, B's "r" tail absorbs it (Case 2). r binds to {b: Str, Empty}.
    /// is_subtype(A, B) = true (sup is RowVar — open tail leniency).
    /// Post-substitution: S(B) = [a: Int, b: Str] = S(A).
    #[test]
    fn test_is_subtype_consistency_closed_sub_with_extra_open_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = row_var_record(b_fields, "r", 0);

        // Pre-unification: sup is RowVar -> lenient
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int,b:Str](closed) should be subtype of [a:Int ...r](RowVar): sup is open"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r_binding = subst.row_map.get("r").expect("row var 'r' should be bound");
        assert_eq!(r_binding.tail, RowTail::Empty, "r tail should be Empty");
        assert_eq!(
            r_binding.fields.get("b"),
            Some(&Type::Str),
            "r should absorb field 'b: Str'"
        );

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(
            sb, sa,
            "S(B) should equal S(A) after successful unification"
        );
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify succeeds"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify succeeds"
        );
    }

    /// Case 3: (RowVar, Empty) — open sub, closed sup with extra field.
    ///
    /// A = [a: Int, ...r]    (open, RowVar "r")
    /// B = [a: Int, b: Str]  (closed)
    ///
    /// Pre-unification is_subtype(A, B): CONSERVATIVE — B has "b" not in A's known fields.
    /// Bidirectional field check fails (sup has field "b" absent from sub's known set).
    /// So is_subtype(A, B) = false before unification.
    ///
    /// unify: "b" unique to B; A's tail "r" absorbs it (Case 3). r binds to {b: Str, Empty}.
    /// Post-substitution: S(A) = [a: Int, b: Str] = S(B). Subtype holds both ways.
    ///
    /// KEY: unify succeeds, but is_subtype(A, B) is false PRE-substitution.
    /// The consistency guarantee applies only AFTER substitution.
    #[test]
    fn test_is_subtype_consistency_open_sub_closed_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields, "r", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        b_fields.insert("b".into(), Type::Str);
        let b = closed_record(b_fields);

        // Pre-unification: conservative — B has "b" not in A's known fields
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...r] (RowVar) should NOT be subtype of [a:Int,b:Str] (closed): \
             sub might lack 'b' — conservative treatment for unbound RowVar"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r_binding = subst.row_map.get("r").expect("r should be bound");
        assert_eq!(r_binding.tail, RowTail::Empty);
        assert_eq!(r_binding.fields.get("b"), Some(&Type::Str));

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(
            sa, sb,
            "S(A) should equal S(B) after binding r to {{b: Str, Empty}}"
        );
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after substitution"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after substitution (symmetric)"
        );
    }

    /// Case 3b: (RowVar, Empty) — open sub with exact known fields matches closed sup.
    ///
    /// A = [a: Int, ...r]  (open)
    /// B = [a: Int]        (closed)
    ///
    /// Conservative is_subtype: A's known fields exactly match B's — bidirectional check passes.
    /// is_subtype(A, B) = true.
    /// unify: no unique fields -> Case 1 -> unify_tails(RowVar(r), Empty) -> r binds to Empty.
    #[test]
    fn test_is_subtype_consistency_open_sub_closed_sup_exact_known_fields() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields.clone(), "r", 0);
        let b = closed_record(a_fields);

        // Sound pre-unification check: open record (RowVar tail) cannot satisfy closed record
        // constraint (Rémy 1994). The row variable may be instantiated with additional fields.
        // Post-unification (after r binds to Empty via unify()), the types are equal and
        // is_subtype holds — verified below.
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...r] (RowVar) should NOT be subtype of [a:Int] (closed) pre-unification: \
             the row variable may be instantiated with additional fields that the closed type rejects"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r_binding = subst.row_map.get("r").expect("r should be bound");
        assert_eq!(r_binding.tail, RowTail::Empty);
        assert_eq!(r_binding.fields.len(), 0);

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);
        assert_eq!(sa, sb, "S(A) should equal S(B) after binding r to Empty");

        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify (symmetric)"
        );
    }

    /// Case 3c: (RowVar, Empty) — open sub with EXTRA known fields, closed sup.
    ///
    /// A = [a: Int, b: Str, ...r]  (open)
    /// B = [a: Int]                (closed)
    ///
    /// is_subtype: A has extra known "b" not in closed B -> bidirectional check fails.
    /// unify: "b" unique to A, closed B cannot absorb it -> error.
    /// Both agree: rejected.
    #[test]
    fn test_is_subtype_consistency_open_sub_extra_fields_closed_sup() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        a_fields.insert("b".into(), Type::Str);
        let a = row_var_record(a_fields, "r", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("a".into(), Type::Int);
        let b = closed_record(b_fields);

        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int,b:Str ...r] should NOT be subtype of [a:Int](closed): extra known field"
        );

        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "unify([a:Int,b:Str ...r], [a:Int](closed)) should fail: closed row cannot absorb 'b'"
        );
        assert!(
            result.unwrap_err().message.contains("extra fields"),
            "error should mention extra fields"
        );
    }

    /// Case 4: (RowVar(r1), RowVar(r2)) — both open with distinct unique fields (Wand Case 4).
    ///
    /// A = [a: Int, ...r1]  (open, row var r1)
    /// B = [b: Str, ...r2]  (open, row var r2)
    ///
    /// unify creates fresh rho_fresh. Binds:
    ///   r1 -> {b: Str, tail: RowVar(rho_fresh)}
    ///   r2 -> {a: Int, tail: RowVar(rho_fresh)}
    ///
    /// Post-substitution: S(A) = S(B) = [a: Int, b: Str, ...rho_fresh].
    ///
    /// Pre-unification is_subtype:
    /// - is_subtype(A, B): sup B has field "b"; sub A does not have "b" in known fields.
    ///   The fields_ok check fails: not all sup fields are in sub. Returns FALSE.
    ///   Open-tail leniency (RowVar in sup) only governs extra fields in sub beyond sup,
    ///   NOT missing fields in sub that sup requires. The field presence check comes first.
    /// - is_subtype(B, A): sup A has field "a"; sub B does not have "a". Returns FALSE.
    ///
    /// This is NOT a bug: unify succeeds because row variables can absorb missing fields
    /// (r1 will absorb "b", r2 will absorb "a"). But is_subtype is a pure predicate operating
    /// on the pre-unification types — without mutation, it cannot infer what row vars will hold.
    ///
    /// Post-substitution: S(A) = S(B) = [a: Int, b: Str, ...rho_fresh]. Subtype holds both ways.
    #[test]
    fn test_is_subtype_consistency_both_open_different_vars_case4() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields, "r1", 0);

        let mut b_fields = HashMap::new();
        b_fields.insert("b".into(), Type::Str);
        let b = row_var_record(b_fields, "r2", 0);

        // Pre-unification: is_subtype checks "all sup fields present in sub" first.
        // A's known fields {a} don't include B's required field "b" -> fields_ok fails -> FALSE.
        // B's known fields {b} don't include A's required field "a" -> fields_ok fails -> FALSE.
        // The RowVar-tail leniency only allows extra fields in sub beyond sup's requirements;
        // it cannot supply missing required fields.
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...r1] should NOT be subtype of [b:Str ...r2]: \
             sub is missing required sup field 'b' (fields_ok fails before tail check)"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[b:Str ...r2] should NOT be subtype of [a:Int ...r1]: \
             sub is missing required sup field 'a' (fields_ok fails before tail check)"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r1_binding = subst.row_map.get("r1").expect("r1 should be bound");
        assert!(
            matches!(r1_binding.tail, RowTail::RowVar(_, _)),
            "r1 tail should be a fresh RowVar (Case 4)"
        );
        assert_eq!(
            r1_binding.fields.get("b"),
            Some(&Type::Str),
            "r1 should absorb field 'b: Str' from unique2"
        );

        let r2_binding = subst.row_map.get("r2").expect("r2 should be bound");
        assert!(
            matches!(r2_binding.tail, RowTail::RowVar(_, _)),
            "r2 tail should be the same fresh RowVar"
        );
        assert_eq!(
            r2_binding.fields.get("a"),
            Some(&Type::Int),
            "r2 should absorb field 'a: Int' from unique1"
        );

        // Both bindings must share the SAME fresh row var
        let r1_fresh = match &r1_binding.tail {
            RowTail::RowVar(name, _) => name.clone(),
            RowTail::Empty => panic!("r1 tail should be RowVar"),
        };
        let r2_fresh = match &r2_binding.tail {
            RowTail::RowVar(name, _) => name.clone(),
            RowTail::Empty => panic!("r2 tail should be RowVar"),
        };
        assert_eq!(
            r1_fresh, r2_fresh,
            "r1 and r2 must share the same fresh row variable in Case 4"
        );

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);
        assert_eq!(
            sa, sb,
            "S(A) and S(B) should be equal after Case 4 unification"
        );

        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must hold after unify succeeds"
        );
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) must hold after unify succeeds"
        );
    }

    /// Case 4b: (RowVar(r1), RowVar(r2)) — both open, shared field only (Wand Case 1 path).
    ///
    /// A = [a: Int, ...r1]  (open)
    /// B = [a: Int, ...r2]  (open)
    ///
    /// No unique fields -> Case 1 -> unify_tails(r1, r2).
    /// r1 binds to Row { fields: {}, tail: RowVar(r2) }.
    /// Post-substitution: S(A) = S(B) = [a: Int, ...r2].
    #[test]
    fn test_is_subtype_consistency_both_open_different_vars_case1() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("a".into(), Type::Int);
        let a = row_var_record(a_fields.clone(), "r1", 0);
        let b = row_var_record(a_fields, "r2", 0);

        // Pre-unification: RowVar tails -> lenient both ways
        assert!(
            Type::is_subtype(&a, &b),
            "[a:Int ...r1] should be subtype of [a:Int ...r2]: sup is open RowVar"
        );
        assert!(
            Type::is_subtype(&b, &a),
            "[a:Int ...r2] should be subtype of [a:Int ...r1]: sup is open RowVar"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r1_binding = subst
            .row_map
            .get("r1")
            .expect("r1 should be bound (Case 1 tail unify)");
        assert_eq!(
            r1_binding.fields.len(),
            0,
            "r1 binding should have no extra fields"
        );
        assert_eq!(
            r1_binding.tail,
            RowTail::RowVar("r2".into(), 0),
            "r1 tail should point to r2"
        );

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(sa, sb, "S(A) should equal S(B) after r1 binds to r2");
        assert!(Type::is_subtype(&sa, &sb), "S(A) <: S(B) after unify");
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) after unify (symmetric)"
        );
    }

    /// Case 5: (RowVar(r), RowVar(r)) — same row var, same fields — reflexive.
    ///
    /// A = [a: Int, ...rho]  (open, row var rho)
    /// B = [a: Int, ...rho]  (open, same row var rho)
    ///
    /// unify: shared "a" only, no unique fields -> Case 1 -> unify_tails(rho, rho) -> reflexive.
    /// No binding created. is_subtype(A, B) = true by a==b structural equality.
    #[test]
    fn test_is_subtype_consistency_same_rowvar_same_fields() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut fields = HashMap::new();
        fields.insert("a".into(), Type::Int);

        let a = row_var_record(fields.clone(), "rho", 0);
        let b = row_var_record(fields, "rho", 0);

        assert!(Type::is_subtype(&a, &b), "A == B structurally, so A <: B");
        assert!(Type::is_subtype(&b, &a), "A == B structurally, so B <: A");

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        assert!(
            !subst.row_map.contains_key("rho"),
            "same row var unification should not create a binding (reflexive)"
        );

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(sa, sb, "S(A) == S(B) for same-var records");
        assert!(Type::is_subtype(&sa, &sb), "S(A) <: S(B)");
        assert!(Type::is_subtype(&sb, &sa), "S(B) <: S(A)");
    }

    /// Case 5b: (RowVar(r), RowVar(r)) — same row var, different unique fields.
    ///
    /// A = [a: Int, ...rho]  (open, row var rho)
    /// B = [b: Str, ...rho]  (open, same row var rho)
    ///
    /// Both unify AND is_subtype reject this combination.
    ///
    /// is_subtype: sup (B or A) has field "b"/"a" that is not in the sub's known fields.
    ///   fields_ok fails before the tail check. Returns FALSE in both directions.
    ///
    /// unify: rejects because rho cannot simultaneously provide both "a" (unique to A)
    ///   and "b" (unique to B) — that would be unsound.
    ///
    /// Both functions agree: this is an invalid combination.
    ///
    /// Note: "open-tail leniency" (RowVar in sup allows extra sub fields) does NOT apply
    /// when the sub is MISSING a required sup field. The fields_ok check runs first.
    #[test]
    fn test_is_subtype_consistency_same_rowvar_different_unique_asymmetry() {
        let mut fields_a = HashMap::new();
        fields_a.insert("a".into(), Type::Int);
        let a = row_var_record(fields_a, "rho", 0);

        let mut fields_b = HashMap::new();
        fields_b.insert("b".into(), Type::Str);
        let b = row_var_record(fields_b, "rho", 0);

        // is_subtype: sub is missing required sup field -> fields_ok fails -> FALSE both ways
        assert!(
            !Type::is_subtype(&a, &b),
            "[a:Int ...rho] should NOT be subtype of [b:Str ...rho]: \
             sub is missing required sup field 'b' (fields_ok fails)"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[b:Str ...rho] should NOT be subtype of [a:Int ...rho]: \
             sub is missing required sup field 'a' (fields_ok fails)"
        );

        // unify: also rejects — rho cannot simultaneously have field 'a' and field 'b'
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let result = unify(&a, &b, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "unify([a:Int ...rho], [b:Str ...rho]) should fail: \
             rho cannot simultaneously have field 'a' and field 'b'"
        );
        assert!(
            result.unwrap_err().message.contains("incompatible fields"),
            "error should mention incompatible fields"
        );
    }

    /// Numeric promotion through record fields — unify more permissive than is_subtype.
    ///
    /// A = [x: Int]    (closed)
    /// B = [x: Number] (closed)
    ///
    /// is_subtype: A <: B (Int <: Number). B <:/ A.
    /// unify: succeeds via promotion rules (Int ~ Number).
    /// Post-substitution: S(A) = [x: Int], S(B) = [x: Number] — asymmetric subtype preserved.
    ///
    /// Documents the intentional asymmetry: unify is bidirectional, is_subtype is directional.
    #[test]
    fn test_is_subtype_consistency_field_numeric_promotion_closed_closed() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut a_fields = HashMap::new();
        a_fields.insert("x".into(), Type::Int);
        let a = closed_record(a_fields);

        let mut b_fields = HashMap::new();
        b_fields.insert("x".into(), Type::Number);
        let b = closed_record(b_fields);

        assert!(
            Type::is_subtype(&a, &b),
            "[x:Int] should be subtype of [x:Number]: Int <: Number"
        );
        assert!(
            !Type::is_subtype(&b, &a),
            "[x:Number] should NOT be subtype of [x:Int]: Number !<: Int"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        // Directional subtype preserved post-unification
        assert!(
            Type::is_subtype(&sa, &sb),
            "S(A) <: S(B) must still hold after unify (Int <: Number)"
        );
        // unify is more permissive than <: for promotions
        assert!(
            !Type::is_subtype(&sb, &sa),
            "S(B) <:/ S(A): unify is more permissive than <: for promotions"
        );
    }

    /// Nested record consistency — RowVar in nested field type.
    ///
    /// A = [point: [x: Int, y: Int] (closed)]  (closed outer)
    /// B = [point: [x: Int, ...r]]              (open inner, closed outer)
    ///
    /// is_subtype(A, B): inner sup is RowVar -> extra 'y' allowed -> true.
    /// unify: inner row var 'r' absorbs "y: Int".
    /// Post-substitution: S(A) = S(B) (inner 'r' bound to {y: Int, Empty}).
    #[test]
    fn test_is_subtype_consistency_nested_record_field() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut inner_a = HashMap::new();
        inner_a.insert("x".into(), Type::Int);
        inner_a.insert("y".into(), Type::Int);
        let mut outer_a = HashMap::new();
        outer_a.insert("point".into(), closed_record(inner_a));
        let a = closed_record(outer_a);

        let mut inner_b = HashMap::new();
        inner_b.insert("x".into(), Type::Int);
        let mut outer_b = HashMap::new();
        outer_b.insert("point".into(), row_var_record(inner_b, "r", 0));
        let b = closed_record(outer_b);

        assert!(
            Type::is_subtype(&a, &b),
            "[point:[x:Int,y:Int]](closed) should be subtype of [point:[x:Int ...r]](closed): \
             inner sup is RowVar so extra 'y' in sub is allowed"
        );

        unify(&a, &b, &mut subst, &mut state, span).unwrap();

        let r_binding = subst.row_map.get("r").expect("r should be bound");
        assert_eq!(r_binding.tail, RowTail::Empty);
        assert_eq!(r_binding.fields.get("y"), Some(&Type::Int));

        let sa = subst.apply(&a);
        let sb = subst.apply(&b);

        assert_eq!(sa, sb, "S(A) should equal S(B) after nested unification");
        assert!(Type::is_subtype(&sa, &sb), "S(A) <: S(B) post-unification");
        assert!(
            Type::is_subtype(&sb, &sa),
            "S(B) <: S(A) post-unification (symmetric)"
        );
    }

    /// Same row variable with different unique fields should fail
    /// This catches the soundness bug: unifying {x: Int, ...rho} with {y: Str, ...rho}
    /// would silently succeed before the fix, but it's unsound because rho cannot
    /// simultaneously provide both x and y fields.
    #[test]
    fn test_unify_same_rho_different_unique_fields_errors() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("y".into(), Type::Str);

        // Both have different unique fields but share the same row variable
        let result = unify(
            &row_var_record(f1, "rho", 0),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        );

        assert!(
            result.is_err(),
            "should fail: same row variable with different unique fields is unsound"
        );
        let err_msg = result.unwrap_err().message;
        assert!(
            err_msg.contains("incompatible fields") && err_msg.contains("rho"),
            "error should mention incompatible fields and the row variable name, got: {}",
            err_msg
        );
    }

    /// Same row variable with asymmetric unique-field counts should fail.
    /// This tests: {x: Int, z: Bool, ...rho} ~ {y: Str, ...rho}
    /// The left side has TWO unique fields (x, z) and the right has ONE (y).
    /// All three fields are exclusive to one side, so rho cannot simultaneously
    /// provide x, y, and z — the error arm must fire even with mismatched cardinalities.
    #[test]
    fn test_unify_same_rowvar_asymmetric_unique_field_counts_errors() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        f1.insert("z".into(), Type::Bool);
        let mut f2 = HashMap::new();
        f2.insert("y".into(), Type::Str);

        // Left has two unique fields, right has one — all three are side-exclusive
        let result = unify(
            &row_var_record(f1, "rho", 0),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        );

        assert!(
            result.is_err(),
            "should fail: same row variable with asymmetric unique fields (2 vs 1) is unsound"
        );
        let err_msg = result.unwrap_err().message;
        assert!(
            err_msg.contains("incompatible fields") && err_msg.contains("rho"),
            "error should mention incompatible fields and the row variable name, got: {}",
            err_msg
        );
    }

    /// Test that lower_row_var_levels in unify_remainders Case 2 prevents over-generalization.
    /// This verifies the Kiselyov (2013) level-based let-polymorphism mechanism: inner row vars
    /// at level 3 should have their level lowered to the outer row var's level when bound,
    /// preventing them from being generalized at the wrong scope.
    #[test]
    fn test_lower_row_var_levels_prevents_generalization() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Create rho_inner at level 3, rho_outer at level 1
        state.levels.insert("rho_inner".into(), 3);
        state.levels.insert("rho_outer".into(), 1);

        // Build left = {x: Int, ...rho_inner}, right = {...rho_outer}
        // This triggers Case 2: left has unique field {x}, right has no unique fields
        let mut f1 = HashMap::new();
        f1.insert("x".into(), Type::Int);
        let left = row_var_record(f1, "rho_inner", 3);

        let f2 = HashMap::new();
        let right = row_var_record(f2, "rho_outer", 1);

        // Unify them
        unify(&left, &right, &mut subst, &mut state, span).unwrap();

        // Case 2 binds rho_outer → {x: Int, ...rho_inner}
        // lower_row_var_levels should lower rho_inner's level from 3 to min(3, 1) = 1
        let rho_inner_level = state.levels.get("rho_inner").copied().unwrap_or(0);
        assert_eq!(
            rho_inner_level, 1,
            "rho_inner level should be lowered from 3 to 1 (rho_outer's level)"
        );

        // Now generalize at level 1 — rho_inner should NOT be generalized
        // because its level is now 1, which is NOT > 1
        let binding = subst
            .row_map
            .get("rho_outer")
            .expect("rho_outer should be bound");
        let bound_type = Type::Record(binding.clone());
        let scheme = generalize(1, &bound_type, &state);

        assert!(
            !scheme.row_vars.contains(&"rho_inner".to_string()),
            "rho_inner should NOT be generalized: its level is now 1, not > 1"
        );
    }

    /// Test the symmetric direction of unify_tails: (Empty, RowVar) vs the already-tested (RowVar, Empty).
    /// Both should bind the RowVar to Row { fields: {}, tail: Empty }.
    #[test]
    fn test_unify_tails_empty_vs_rowvar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // No unique fields, left is closed (Empty), right is open (rho)
        let f1 = HashMap::new();
        let f2 = HashMap::new();
        unify(
            &closed_record(f1),
            &row_var_record(f2, "rho", 0),
            &mut subst,
            &mut state,
            span,
        )
        .unwrap();

        // rho should be bound to Row { fields: {}, tail: Empty }
        let binding = subst.row_map.get("rho").expect("rho should be bound");
        assert_eq!(binding.tail, RowTail::Empty);
        assert_eq!(binding.fields.len(), 0);
    }

    /// Test that shared-field unification bindings are not overwritten by stale tail references.
    ///
    /// Scenario: ρ appears both as the tail of an outer row AND inside a nested Record field.
    /// Step 3 (shared-field unification) binds ρ via the nested record.
    /// Step 4 must re-resolve the outer tail to see that binding, rather than using the
    /// pre-Step-3 stale RowVar(ρ) reference that would overwrite the binding.
    ///
    /// Row1: {a: Record({x: Int, ...ρ}), ...ρ}
    /// Row2: {a: Record({x: Int, y: Str}), z: Bool}
    ///
    /// Step 3 binds ρ → {y: Str, ∅} from inner record unification.
    /// Without the fix, Step 4 would overwrite ρ → {z: Bool, ∅}, losing the y: Str constraint.
    /// With the fix, Step 4 re-resolves ρ, sees it's already bound to {y: Str, ∅}, and the
    /// outer row resolves to {a: ..., y: Str} vs {a: ..., z: Bool} — correctly producing an error.
    ///
    /// Formal model: Robinson (1965) substitution-threading invariant — bindings from
    /// earlier unification steps must be visible to later steps.
    #[test]
    fn test_reresolution_after_shared_field_unification() {
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.level = 1;
        state.levels.insert("rho".into(), 1);
        let span = test_span(1, 1, 1, 1);

        // Row1: {a: Record({x: Int, ...ρ}), ...ρ}
        let inner1 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int)]),
            tail: RowTail::RowVar("rho".into(), 1),
        });
        let row1 = Row {
            fields: HashMap::from([("a".into(), inner1)]),
            tail: RowTail::RowVar("rho".into(), 1),
        };

        // Row2: {a: Record({x: Int, y: Str}), z: Bool}
        let inner2 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int), ("y".into(), Type::Str)]),
            tail: RowTail::Empty,
        });
        let row2 = Row {
            fields: HashMap::from([("a".into(), inner2), ("z".into(), Type::Bool)]),
            tail: RowTail::Empty,
        };

        // Unifying these should FAIL because:
        // - Inner unification binds ρ → {y: Str, ∅}
        // - So outer row1 expands to {a: ..., y: Str}
        // - Outer row2 has {a: ..., z: Bool}
        // - {y: Str} vs {z: Bool} with both tails closed → error
        let result = unify(
            &Type::Record(row1),
            &Type::Record(row2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_err(),
            "should fail: ρ bound by inner unification prevents outer tail from absorbing z: Bool"
        );
    }

    /// Test that re-resolution works correctly when the row variable binding is compatible.
    ///
    /// Row1: {a: Record({x: Int, ...ρ}), ...ρ}
    /// Row2: {a: Record({x: Int, y: Str}), y: Str}
    ///
    /// Step 3 binds ρ → {y: Str, ∅} from inner record unification.
    /// After re-resolution, outer row1 becomes {a: ..., y: Str, ∅}.
    /// Outer row2 is {a: ..., y: Str, ∅}.
    /// The newly-surfaced y: Str fields match — unification should succeed.
    #[test]
    fn test_reresolution_compatible_binding() {
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.level = 1;
        state.levels.insert("rho".into(), 1);
        let span = test_span(1, 1, 1, 1);

        // Row1: {a: Record({x: Int, ...ρ}), ...ρ}
        let inner1 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int)]),
            tail: RowTail::RowVar("rho".into(), 1),
        });
        let row1 = Row {
            fields: HashMap::from([("a".into(), inner1)]),
            tail: RowTail::RowVar("rho".into(), 1),
        };

        // Row2: {a: Record({x: Int, y: Str}), y: Str}
        let inner2 = Type::Record(Row {
            fields: HashMap::from([("x".into(), Type::Int), ("y".into(), Type::Str)]),
            tail: RowTail::Empty,
        });
        let row2 = Row {
            fields: HashMap::from([("a".into(), inner2), ("y".into(), Type::Str)]),
            tail: RowTail::Empty,
        };

        // Should succeed: ρ → {y: Str, ∅} from inner, then outer y: Str matches
        let result = unify(
            &Type::Record(row1),
            &Type::Record(row2),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "should succeed: ρ bound to {{y: Str}} by inner, outer y: Str matches. Got: {:?}",
            result.err()
        );

        // Verify ρ is bound to {y: Str, ∅}
        let rho_binding = subst.row_map.get("rho").expect("rho should be bound");
        assert_eq!(rho_binding.fields.get("y"), Some(&Type::Str));
        assert_eq!(rho_binding.tail, RowTail::Empty);
    }

    /// Test multi-hop TypeVar chase in row_var_occurs_in_type.
    /// If α → Record({x: TypeVar(β)}) and β → Record({z: Int, ...ρ}),
    /// then row_var_occurs_in_type("ρ", TypeVar("α"), &subst) should return true.
    /// This tests that the recursive chase works through multiple TypeVar bindings.
    #[test]
    fn test_multi_hop_typevar_chase_in_row_occurs() {
        let mut subst = Substitution::new();

        // Bind β → Record({z: Int}, RowVar("rho"))
        let mut beta_fields = HashMap::new();
        beta_fields.insert("z".into(), Type::Int);
        let beta_bound = Type::Record(Row {
            fields: beta_fields,
            tail: RowTail::RowVar("rho".into(), 0),
        });
        subst.type_map.insert("beta".into(), beta_bound);

        // Bind α → Record({x: TypeVar("beta")})
        let mut alpha_fields = HashMap::new();
        alpha_fields.insert("x".into(), Type::TypeVar("beta".into(), 0));
        let alpha_bound = Type::Record(Row {
            fields: alpha_fields,
            tail: RowTail::Empty,
        });
        subst.type_map.insert("alpha".into(), alpha_bound);

        // row_var_occurs_in_type should chase: α → Record({x: β}) → β → Record({...ρ})
        // and detect that ρ is transitively reachable through α's binding
        let tv_alpha = Type::TypeVar("alpha".into(), 0);
        assert!(
            row_var_occurs_in_type("rho", &tv_alpha, &subst),
            "should detect rho through multi-hop TypeVar chase: alpha → beta → rho"
        );
    }

    #[test]
    fn test_row_occurs_visited_set_early_return() {
        // Exercises the cycle-guard branch in row_var_occurs_in_type_impl:
        // when a TypeVar has already been visited, the function returns false
        // immediately rather than recursing into a cycle.
        //
        // This tests defense-in-depth: cyclic type_map bindings (alpha → alpha)
        // should be impossible under correct occurs-check invariants, but the
        // visited set prevents infinite recursion if they occur.
        let mut subst = Substitution::new();

        // Create a cyclic binding: alpha → TypeVar("alpha")
        subst
            .type_map
            .insert("alpha".into(), Type::TypeVar("alpha".into(), 0));

        // row_var_occurs_in_type should hit the visited-set early return
        // on the second encounter of "alpha" and return false (not hang).
        let tv_alpha = Type::TypeVar("alpha".into(), 0);
        assert!(
            !row_var_occurs_in_type("rho", &tv_alpha, &subst),
            "cyclic TypeVar binding should not cause infinite recursion; \
             visited set should catch the cycle and return false"
        );

        // Also test via a field containing the cyclic TypeVar — the visited
        // set must propagate correctly through row_var_occurs → field iteration
        // → row_var_occurs_in_type_impl.
        let mut fields = HashMap::new();
        fields.insert("x".into(), Type::TypeVar("alpha".into(), 0));
        let row = Row {
            fields,
            tail: RowTail::Empty,
        };
        assert!(
            !row_var_occurs("rho", &row, &subst),
            "row_var_occurs should handle cyclic TypeVar in field types"
        );
    }

    #[test]
    fn test_max_subst_size_limit_type_vars() {
        // Create enough type variable bindings to exceed MAX_SUBST_SIZE
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Create MAX_SUBST_SIZE + 1 type variables and try to unify them
        // This should trigger the size limit
        for i in 0..=MAX_SUBST_SIZE {
            let var = Type::TypeVar(format!("t{}", i), 0);
            let concrete = Type::Int;
            let result = unify(&var, &concrete, &mut subst, &mut state, span);

            if i <= MAX_SUBST_SIZE - 1 {
                // Should succeed for bindings within the limit
                assert!(result.is_ok(), "unify should succeed for binding {}", i);
            } else {
                // Should fail when exceeding the limit
                assert!(
                    result.is_err(),
                    "unify should fail when exceeding MAX_SUBST_SIZE"
                );
                if let Err(e) = result {
                    assert!(
                        e.message.contains("type inference resource limit exceeded"),
                        "error message should mention inference limit, got: {}",
                        e.message
                    );
                }
            }
        }
    }

    #[test]
    fn test_max_subst_size_limit_row_vars() {
        // Create enough row variable bindings to exceed MAX_SUBST_SIZE
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Create MAX_SUBST_SIZE + 1 row variables and try to unify them
        for i in 0..=MAX_SUBST_SIZE {
            let row1 = Row {
                fields: HashMap::new(),
                tail: RowTail::RowVar(format!("rho{}", i), 0),
            };
            let row2 = Row {
                fields: HashMap::new(),
                tail: RowTail::Empty,
            };
            let rec1 = Type::Record(row1);
            let rec2 = Type::Record(row2);
            let result = unify(&rec1, &rec2, &mut subst, &mut state, span);

            if i <= MAX_SUBST_SIZE - 1 {
                // Should succeed for bindings within the limit
                assert!(result.is_ok(), "unify should succeed for row binding {}", i);
            } else {
                // Should fail when exceeding the limit
                assert!(
                    result.is_err(),
                    "unify should fail when exceeding MAX_SUBST_SIZE"
                );
                if let Err(e) = result {
                    assert!(
                        e.message.contains("type inference resource limit exceeded"),
                        "error message should mention inference limit, got: {}",
                        e.message
                    );
                }
            }
        }
    }

    #[test]
    fn test_max_subst_size_combined_types_and_rows() {
        // Test that the limit applies to the combined size of type_map and row_map
        let mut state = InferState::new();
        let mut subst = Substitution::new();
        let span = Span::origin();

        // Add half the limit in type variables
        let halfway = MAX_SUBST_SIZE / 2;
        for i in 0..halfway {
            let var = Type::TypeVar(format!("t{}", i), 0);
            let concrete = Type::Int;
            let result = unify(&var, &concrete, &mut subst, &mut state, span);
            assert!(
                result.is_ok(),
                "type var unify should succeed for binding {}",
                i
            );
        }

        // Now add row variables until we exceed the combined limit
        for i in 0..=MAX_SUBST_SIZE {
            let row1 = Row {
                fields: HashMap::new(),
                tail: RowTail::RowVar(format!("rho{}", i), 0),
            };
            let row2 = Row {
                fields: HashMap::new(),
                tail: RowTail::Empty,
            };
            let rec1 = Type::Record(row1);
            let rec2 = Type::Record(row2);
            let result = unify(&rec1, &rec2, &mut subst, &mut state, span);

            let total_size = halfway + i + 1;
            if total_size <= MAX_SUBST_SIZE {
                // Should succeed while under the combined limit
                assert!(
                    result.is_ok(),
                    "unify should succeed at total size {}",
                    total_size
                );
            } else {
                // Should fail when combined size exceeds limit
                assert!(
                    result.is_err(),
                    "unify should fail when combined size {} exceeds MAX_SUBST_SIZE",
                    total_size
                );
                if let Err(e) = result {
                    assert!(
                        e.message.contains("type inference resource limit exceeded"),
                        "error message should mention inference limit, got: {}",
                        e.message
                    );
                }
                break;
            }
        }
    }

    // --- Type::Error sentinel ---

    #[test]
    fn test_error_display() {
        assert_eq!(format!("{}", Type::Error), "<error>");
    }

    #[test]
    fn test_error_eq() {
        assert_eq!(Type::Error, Type::Error);
        assert_ne!(Type::Error, Type::Int);
        assert_ne!(Type::Error, Type::Any);
    }

    #[test]
    fn test_error_is_not_subtype_of_anything() {
        assert!(!Type::is_subtype(&Type::Error, &Type::Int));
        assert!(!Type::is_subtype(&Type::Error, &Type::Str));
        assert!(!Type::is_subtype(&Type::Error, &Type::Any));
        assert!(!Type::is_subtype(&Type::Error, &Type::Error));
        assert!(!Type::is_subtype(&Type::Int, &Type::Error));
        assert!(!Type::is_subtype(&Type::Any, &Type::Error));
    }

    #[test]
    fn test_error_has_no_inference_vars() {
        assert!(!Type::Error.has_inference_vars());
    }

    #[test]
    fn test_error_collect_vars_empty() {
        let mut type_vars = HashSet::new();
        let mut row_vars = HashSet::new();
        Type::Error.collect_all_vars(&mut type_vars, &mut row_vars);
        assert!(type_vars.is_empty());
        assert!(row_vars.is_empty());
    }

    #[test]
    fn test_unify_error_with_any_type_succeeds() {
        // unify(Error, T) = Ok(()) for all T — error absorption
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Error with concrete types
        assert!(unify(&Type::Error, &Type::Int, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Int, &Type::Error, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Str, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Bool, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Any, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Any, &Type::Error, &mut subst, &mut state, span).is_ok());
        assert!(unify(&Type::Error, &Type::Error, &mut subst, &mut state, span).is_ok());

        // Substitution must not be modified — Error carries no binding information
        assert!(
            subst.is_empty(),
            "unify(Error, T) must not create any bindings in the substitution"
        );
    }

    #[test]
    fn test_unify_error_with_typevar_does_not_bind() {
        // unify(Error, TypeVar) = Ok(()) — Error absorbs without binding the TypeVar
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);

        let result = unify(
            &Type::Error,
            &Type::TypeVar("a".into(), 1),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_ok());
        // TypeVar "a" must not be bound — Error does not carry type information
        assert!(
            subst.type_map.is_empty(),
            "TypeVar must not be bound when unified with Error"
        );
    }

    #[test]
    fn test_apply_preserves_error() {
        // Substitution::apply must pass Error through unchanged
        let subst = Substitution::new();
        assert_eq!(subst.apply(&Type::Error), Type::Error);

        let mut subst_with_binding = Substitution::new();
        subst_with_binding.type_map.insert("a".into(), Type::Int);
        assert_eq!(subst_with_binding.apply(&Type::Error), Type::Error);
    }

    /// Case 5: unify_remainders with display-hiding row variable.
    /// Tests that unification succeeds when one of the row variables has a `_` prefix,
    /// triggering the display-hiding branch in error messages and Display formatting.
    #[test]
    fn test_unify_remainders_case5_display_hiding() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();

        // Create two rows with the same field set: {a: Int}
        // Left: {a: Int, ...rho1}, right: {a: Int, ..._hidden2}
        // The `_hidden2` row var has a `_` prefix → display-hiding behavior
        let mut f1 = HashMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = HashMap::new();
        f2.insert("a".into(), Type::Int);

        // Unify should succeed: shared field {a: Int}, no unique fields → Case 1
        let result = unify(
            &row_var_record(f1, "rho1", 0),
            &row_var_record(f2, "_hidden2", 0),
            &mut subst,
            &mut state,
            span,
        );
        assert!(
            result.is_ok(),
            "unification should succeed when row var has _ prefix, got: {:?}",
            result.unwrap_err()
        );

        // The tails should unify: rho1 and _hidden2 should bind together
        // (Case 1 in unify_remainders: no unique fields → unify_tails)
        // unify_tails(RowVar("rho1"), RowVar("_hidden2")) binds the lower-level one
        // to the higher-level one (or chooses based on name ordering if same level)
        assert!(
            subst.row_map.get("rho1").is_some() || subst.row_map.get("_hidden2").is_some(),
            "at least one row var should be bound after tail unification"
        );
    }

    // --- variadic flag in PartialEq and unify ---

    #[test]
    fn test_function_partial_eq_includes_variadic() {
        // variadic=true and variadic=false must not be equal even with identical params/ret.
        let f_variadic = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_non_variadic = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert_ne!(
            f_variadic, f_non_variadic,
            "Fn(Int→Bool, variadic=true) must not equal Fn(Int→Bool, variadic=false)"
        );
        // Same variadic flag must still be equal.
        let f_variadic2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        assert_eq!(
            f_variadic, f_variadic2,
            "Fn(Int→Bool, variadic=true) must equal itself"
        );
    }

    #[test]
    fn test_unify_variadic_mismatch_error() {
        // unify(Fn(variadic=true), Fn(variadic=false)) must return a TypeError
        // containing "variadic mismatch".
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let f_variadic = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_non_variadic = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        let result = unify(&f_variadic, &f_non_variadic, &mut subst, &mut state, span);
        assert!(
            result.is_err(),
            "unify(variadic=true, variadic=false) must return Err"
        );
        assert!(
            result.unwrap_err().message.contains("variadic mismatch"),
            "error message must contain 'variadic mismatch'"
        );
    }

    #[test]
    fn test_is_subtype_variadic_mismatch() {
        // is_subtype must return false when variadic flags differ.
        let f_v = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: true,
        };
        let f_nv = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
            variadic: false,
        };
        assert!(
            !Type::is_subtype(&f_v, &f_nv),
            "variadic must not be subtype of non-variadic"
        );
        assert!(
            !Type::is_subtype(&f_nv, &f_v),
            "non-variadic must not be subtype of variadic"
        );
    }
}
