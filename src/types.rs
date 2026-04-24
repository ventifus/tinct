//! Runtime type representations, type environments with scoped alias registries,
//! substitutions/unification for Hindley-Milner polymorphism,
//! and type error definitions for the type checker.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;

#[derive(Debug, Clone, Eq)]
pub enum RowRest {
    Closed,
    Open,
    RowVar(String, u32),
}

// Manual PartialEq for RowRest: RowVar compares name only, level ignored
impl PartialEq for RowRest {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (RowRest::Closed, RowRest::Closed) => true,
            (RowRest::Open, RowRest::Open) => true,
            (RowRest::RowVar(n1, _), RowRest::RowVar(n2, _)) => n1 == n2,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Type {
    Int,
    IntLiteral(i64),
    Float,
    Str,
    StringLiteral(String),
    Bool,
    Number,
    Record(IndexMap<String, Type>, RowRest),
    Function {
        params: Vec<Type>,
        ret: Box<Type>,
    },
    Seq(Box<Type>),
    #[allow(clippy::enum_variant_names)]
    TypeVar(String, u32),
    Any,
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
            (Type::Record(f1, r1), Type::Record(f2, r2)) => f1 == f2 && r1 == r2,
            (
                Type::Function {
                    params: p1,
                    ret: r1,
                },
                Type::Function {
                    params: p2,
                    ret: r2,
                },
            ) => p1 == p2 && r1 == r2,
            (Type::Seq(e1), Type::Seq(e2)) => e1 == e2,
            (Type::TypeVar(n1, _), Type::TypeVar(n2, _)) => n1 == n2,
            (Type::Any, Type::Any) => true,
            _ => false,
        }
    }
}

impl Type {
    /// Recursive without a depth guard; safe because type nesting is bounded by the parser's MAX_DEPTH (256).
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
        if matches!(sub, Type::Any) || matches!(sup, Type::Any) {
            return true;
        }
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::Seq(sub_elem), Type::Seq(sup_elem)) => Type::is_subtype(sub_elem, sup_elem),
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::Str) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            (Type::Record(sub_fields, _sub_rest), Type::Record(sup_fields, sup_rest)) => {
                let fields_ok = sup_fields.iter().all(|(k, sup_ty)| {
                    sub_fields
                        .get(k)
                        .is_some_and(|sub_ty| Type::is_subtype(sub_ty, sup_ty))
                });
                if !fields_ok {
                    return false;
                }
                match sup_rest {
                    RowRest::Closed => sub_fields.keys().all(|k| sup_fields.contains_key(k)),
                    RowRest::Open | RowRest::RowVar(_, _) => true,
                }
            }
            (
                Type::Function {
                    params: sub_p,
                    ret: sub_r,
                },
                Type::Function {
                    params: sup_p,
                    ret: sup_r,
                },
            ) => {
                sub_p.len() == sup_p.len()
                    && sub_p
                        .iter()
                        .zip(sup_p.iter())
                        .all(|(sp, pp)| Type::is_subtype(pp, sp))
                    && Type::is_subtype(sub_r, sup_r)
            }
            _ => false,
        }
    }

    pub fn collect_type_vars(&self, vars: &mut BTreeSet<String>) {
        match self {
            Type::TypeVar(name, _) => {
                vars.insert(name.clone());
            }
            Type::Record(fields, rest) => {
                for ty in fields.values() {
                    ty.collect_type_vars(vars);
                }
                if let RowRest::RowVar(name, _) = rest {
                    vars.insert(name.clone());
                }
            }
            Type::Function { params, ret } => {
                for p in params {
                    p.collect_type_vars(vars);
                }
                ret.collect_type_vars(vars);
            }
            Type::Seq(elem) => elem.collect_type_vars(vars),
            _ => {}
        }
    }

    pub fn has_type_vars(&self) -> bool {
        match self {
            Type::TypeVar(_, _) => true,
            Type::Record(fields, rest) => {
                matches!(rest, RowRest::RowVar(_, _))
                    || fields.values().any(|ty| ty.has_type_vars())
            }
            Type::Function { params, ret } => {
                params.iter().any(|p| p.has_type_vars()) || ret.has_type_vars()
            }
            Type::Seq(elem) => elem.has_type_vars(),
            _ => false,
        }
    }
}

/// Type scheme for let-generalization (∀α₁...αₙ. τ)
#[derive(Debug, Clone, PartialEq)]
pub struct TypeScheme {
    pub vars: Vec<String>,
    pub body: Type,
}

impl TypeScheme {
    /// Create a monomorphic scheme (no quantified variables)
    pub fn mono(ty: Type) -> Self {
        Self {
            vars: vec![],
            body: ty,
        }
    }
}

impl fmt::Display for TypeScheme {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.vars.is_empty() {
            write!(f, "{}", self.body)
        } else {
            write!(f, "∀{}. {}", self.vars.join(" "), self.body)
        }
    }
}

/// Inference state for levels-based let-generalization
#[derive(Debug, Clone)]
pub struct InferState {
    pub name_counter: u32,
    pub level: u32,
    pub levels: HashMap<String, u32>,
}

impl InferState {
    pub fn new() -> Self {
        Self {
            name_counter: 0,
            level: 0,
            levels: HashMap::new(),
        }
    }

    /// Create a fresh type variable at the current level
    pub fn fresh_var(&mut self) -> Type {
        let name = format!("_t{}", self.name_counter);
        self.name_counter += 1;
        self.levels.insert(name.clone(), self.level);
        Type::TypeVar(name, self.level)
    }
}

impl Default for InferState {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    map: IndexMap<String, Type>,
}

const MAX_APPLY_DEPTH: usize = 256;

impl Substitution {
    pub fn new() -> Self {
        Self {
            map: IndexMap::new(),
        }
    }

    pub fn apply(&self, ty: &Type) -> Type {
        let mut visited = HashSet::new();
        self.apply_inner(ty, 0, &mut visited)
    }

    fn apply_inner(&self, ty: &Type, depth: usize, visited: &mut HashSet<String>) -> Type {
        if depth >= MAX_APPLY_DEPTH {
            return ty.clone();
        }
        match ty {
            Type::TypeVar(name, level) => {
                if visited.contains(name) {
                    return ty.clone();
                }
                match self.map.get(name) {
                    Some(bound) => {
                        visited.insert(name.clone());
                        let result = self.apply_inner(bound, depth + 1, visited);
                        visited.remove(name);
                        result
                    }
                    None => Type::TypeVar(name.clone(), *level),
                }
            }
            Type::Record(fields, rest) => {
                let new_fields: IndexMap<String, Type> = fields
                    .iter()
                    .map(|(k, v)| (k.clone(), self.apply_inner(v, depth + 1, visited)))
                    .collect();
                match rest {
                    RowRest::RowVar(name, level) => match self.map.get(name) {
                        Some(bound) => {
                            let resolved = self.apply_inner(bound, depth + 1, visited);
                            match resolved {
                                Type::Record(extra_fields, resolved_rest) => {
                                    let mut merged = new_fields;
                                    merged.extend(extra_fields);
                                    Type::Record(merged, resolved_rest)
                                }
                                Type::TypeVar(new_name, new_level) => {
                                    Type::Record(new_fields, RowRest::RowVar(new_name, new_level))
                                }
                                _ => Type::Record(new_fields, rest.clone()),
                            }
                        }
                        None => Type::Record(new_fields, RowRest::RowVar(name.clone(), *level)),
                    },
                    _ => Type::Record(new_fields, rest.clone()),
                }
            }
            Type::Function { params, ret } => Type::Function {
                params: params
                    .iter()
                    .map(|p| self.apply_inner(p, depth + 1, visited))
                    .collect(),
                ret: Box::new(self.apply_inner(ret, depth + 1, visited)),
            },
            Type::Seq(elem) => Type::Seq(Box::new(self.apply_inner(elem, depth + 1, visited))),
            _ => ty.clone(),
        }
    }

    // Used in type checker tests; not yet called from production code.
    #[cfg(test)]
    pub fn get(&self, name: &str) -> Option<&Type> {
        self.map.get(name)
    }
}

impl Default for Substitution {
    fn default() -> Self {
        Self::new()
    }
}

fn occurs_in(var_name: &str, ty: &Type) -> bool {
    match ty {
        Type::TypeVar(name, _) => name == var_name,
        Type::Record(fields, rest) => {
            fields.values().any(|t| occurs_in(var_name, t))
                || matches!(rest, RowRest::RowVar(r, _) if r == var_name)
        }
        Type::Function { params, ret } => {
            params.iter().any(|p| occurs_in(var_name, p)) || occurs_in(var_name, ret)
        }
        Type::Seq(elem) => occurs_in(var_name, elem),
        _ => false,
    }
}

pub fn unify(
    a: &Type,
    b: &Type,
    subst: &mut Substitution,
    state: &mut InferState,
    span: Span,
) -> Result<(), TypeError> {
    let a = subst.apply(a);
    let b = subst.apply(b);

    if a == b {
        return Ok(());
    }

    match (&a, &b) {
        // Any-unification with level zeroing: prevent generalization of Any-touched vars
        (Type::Any, Type::TypeVar(name, _)) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::TypeVar(name, _), Type::Any) => {
            state.levels.insert(name.clone(), 0);
            Ok(())
        }
        (Type::Any, _) | (_, Type::Any) => Ok(()),

        // U-VAR-LEVEL: bind α to τ, lower levels of all β ∈ FTV(τ)
        (Type::TypeVar(name, _), _) => {
            if occurs_in(name, &b) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {b}"),
                    span,
                ));
            }
            // Symmetric level lowering: lower all type vars in b to min(their level, α's level)
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            let mut vars_in_b = BTreeSet::new();
            b.collect_type_vars(&mut vars_in_b);
            for beta in vars_in_b {
                let beta_level = state.levels.get(&beta).copied().unwrap_or(0);
                state.levels.insert(beta, beta_level.min(alpha_level));
            }
            subst.map.insert(name.clone(), b);
            Ok(())
        }
        // U-VAR-LEVEL-SYM: bind α to τ, lower levels of all β ∈ FTV(τ)
        (_, Type::TypeVar(name, _)) => {
            if occurs_in(name, &a) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {a}"),
                    span,
                ));
            }
            // Symmetric level lowering: lower all type vars in a to min(their level, α's level)
            let alpha_level = state.levels.get(name).copied().unwrap_or(0);
            let mut vars_in_a = BTreeSet::new();
            a.collect_type_vars(&mut vars_in_a);
            for beta in vars_in_a {
                let beta_level = state.levels.get(&beta).copied().unwrap_or(0);
                state.levels.insert(beta, beta_level.min(alpha_level));
            }
            subst.map.insert(name.clone(), a);
            Ok(())
        }

        // Literal-to-parent promotions
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
        (Type::IntLiteral(_), Type::Float) | (Type::Float, Type::IntLiteral(_)) => Ok(()),

        (
            Type::Function {
                params: p1,
                ret: r1,
            },
            Type::Function {
                params: p2,
                ret: r2,
            },
        ) => {
            if p1.len() != p2.len() {
                return Err(TypeError::new(
                    format!(
                        "function arity mismatch: expected {} params, got {}",
                        p1.len(),
                        p2.len()
                    ),
                    span,
                ));
            }
            for (pa, pb) in p1.iter().zip(p2.iter()) {
                unify(pa, pb, subst, state, span)?;
            }
            unify(r1, r2, subst, state, span)
        }

        (Type::Seq(elem1), Type::Seq(elem2)) => unify(elem1, elem2, subst, state, span),

        (Type::Record(f1, r1), Type::Record(f2, r2)) => {
            if matches!(r1, RowRest::Closed) && matches!(r2, RowRest::Closed) {
                let keys1: BTreeSet<&String> = f1.keys().collect();
                let keys2: BTreeSet<&String> = f2.keys().collect();
                if keys1 != keys2 {
                    return Err(TypeError::new(
                        format!(
                            "closed record field mismatch: expected [{}], got [{}]",
                            f1.keys().cloned().collect::<Vec<_>>().join(", "),
                            f2.keys().cloned().collect::<Vec<_>>().join(", ")
                        ),
                        span,
                    ));
                }
            }
            for (key, ty1) in f1 {
                if let Some(ty2) = f2.get(key) {
                    unify(ty1, ty2, subst, state, span)?;
                }
            }
            Ok(())
        }

        _ => Err(TypeError::type_mismatch(&a, &b, span)),
    }
}

pub fn instantiate(ty: &Type, counter: &mut u32) -> (Type, Substitution) {
    let mut vars = BTreeSet::new();
    ty.collect_type_vars(&mut vars);

    let mut renaming = Substitution::new();
    for var in vars {
        let fresh = format!("_t{counter}");
        *counter += 1;
        renaming.map.insert(var, Type::TypeVar(fresh, 0));
    }

    (renaming.apply(ty), renaming)
}

/// Instantiate a type scheme by creating fresh type variables at the given level.
/// Used for VAR-POLY: when a polymorphic binding is referenced, create fresh instances.
pub fn instantiate_scheme(scheme: &TypeScheme, level: u32, state: &mut InferState) -> Type {
    if scheme.vars.is_empty() {
        // Monomorphic scheme: return body directly
        return scheme.body.clone();
    }

    // Create fresh type variables at the specified level for each quantified var
    let mut renaming = Substitution::new();
    for var in &scheme.vars {
        let fresh_name = format!("_t{}", state.name_counter);
        state.name_counter += 1;
        state.levels.insert(fresh_name.clone(), level);
        renaming
            .map
            .insert(var.clone(), Type::TypeVar(fresh_name, level));
    }

    renaming.apply(&scheme.body)
}

/// Generalize a type at a binding boundary by quantifying free type variables
/// whose level is strictly greater than the enclosing scope level.
/// Used for let-generalization: ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme {
    let mut all_vars = BTreeSet::new();
    ty.collect_type_vars(&mut all_vars);

    // Filter: keep only vars where levels[var] > level
    let generalizable: Vec<String> = all_vars
        .into_iter()
        .filter(|var| {
            let var_level = state.levels.get(var).copied().unwrap_or(0);
            var_level > level
        })
        .collect();

    if generalizable.is_empty() {
        TypeScheme::mono(ty.clone())
    } else {
        TypeScheme {
            vars: generalizable,
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
            Type::Record(fields, rest) => {
                write!(f, "[")?;
                for (i, (key, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    write!(f, "{key}: {ty}")?;
                }
                match rest {
                    RowRest::Closed => {}
                    RowRest::Open => {
                        if !fields.is_empty() {
                            write!(f, "  ")?;
                        }
                        write!(f, "...")?;
                    }
                    RowRest::RowVar(name, _level) => {
                        if !fields.is_empty() {
                            write!(f, "  ")?;
                        }
                        write!(f, "...{name}")?;
                    }
                }
                write!(f, "]")
            }
            Type::Function { params, ret } => {
                write!(f, "Fn@{ret} [")?;
                for (i, p) in params.iter().enumerate() {
                    if i > 0 {
                        write!(f, " ")?;
                    }
                    write!(f, "{p}")?;
                }
                write!(f, "]")
            }
            Type::Seq(elem) => write!(f, "Seq[{elem}]"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TypeEnv {
    bindings: IndexMap<String, TypeScheme>,
    type_aliases: IndexMap<String, Type>,
    parent: Option<Rc<TypeEnv>>,
}

impl TypeEnv {
    pub fn new() -> Self {
        Self {
            bindings: IndexMap::new(),
            type_aliases: IndexMap::new(),
            parent: None,
        }
    }

    pub fn with_parent(parent: Rc<TypeEnv>) -> Self {
        Self {
            bindings: IndexMap::new(),
            type_aliases: IndexMap::new(),
            parent: Some(parent),
        }
    }

    pub fn get(&self, name: &str) -> Option<&TypeScheme> {
        self.lookup(|env| env.bindings.get(name))
    }

    pub fn get_type_alias(&self, name: &str) -> Option<&Type> {
        self.lookup_type_alias(|env| env.type_aliases.get(name))
    }

    fn lookup(&self, field: impl Fn(&TypeEnv) -> Option<&TypeScheme>) -> Option<&TypeScheme> {
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
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq)]
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
        Self::new(
            format!("type mismatch: expected {expected}, got {got}"),
            span,
        )
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
        Self::new(format!("undefined variable: ${name}"), span)
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
        let mut fields = IndexMap::new();
        fields.insert("name".into(), Type::Str);
        fields.insert("age".into(), Type::Int);
        assert_eq!(
            format!("{}", Type::Record(fields, RowRest::Closed)),
            "[name: String  age: Int]"
        );
    }

    #[test]
    fn test_display_record_empty() {
        assert_eq!(
            format!("{}", Type::Record(IndexMap::new(), RowRest::Closed)),
            "[]"
        );
    }

    #[test]
    fn test_display_record_open() {
        let mut fields = IndexMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!("{}", Type::Record(fields, RowRest::Open)),
            "[name: String  ...]"
        );
    }

    #[test]
    fn test_display_record_open_empty() {
        assert_eq!(
            format!("{}", Type::Record(IndexMap::new(), RowRest::Open)),
            "[...]"
        );
    }

    #[test]
    fn test_display_record_row_var() {
        let mut fields = IndexMap::new();
        fields.insert("name".into(), Type::Str);
        assert_eq!(
            format!(
                "{}",
                Type::Record(fields, RowRest::RowVar("rest".into(), 0))
            ),
            "[name: String  ...rest]"
        );
    }

    #[test]
    fn test_display_function() {
        let ty = Type::Function {
            params: vec![Type::Int, Type::Str],
            ret: Box::new(Type::Bool),
        };
        assert_eq!(format!("{ty}"), "Fn@Bool [Int String]");
    }

    #[test]
    fn test_display_function_no_params() {
        let ty = Type::Function {
            params: vec![],
            ret: Box::new(Type::Int),
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
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);
        sub.insert("extra".into(), Type::Bool);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_record_missing_field() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);
        sup.insert("age".into(), Type::Int);

        assert!(!Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
    }

    #[test]
    fn test_subtype_closed_record_extra_field_rejected() {
        // Closed sub with extra field should NOT be subtype of closed sup
        let mut sub_fields = IndexMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = Type::Record(sub_fields, RowRest::Closed);

        let mut sup_fields = IndexMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = Type::Record(sup_fields, RowRest::Closed);

        assert!(
            !Type::is_subtype(&sub, &sup),
            "[a: Int, b: Int] should NOT be subtype of [a: Int] (Closed)"
        );
    }

    #[test]
    fn test_subtype_closed_record_same_fields_ok() {
        let mut sub_fields = IndexMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = Type::Record(sub_fields, RowRest::Closed);

        let mut sup_fields = IndexMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = Type::Record(sup_fields, RowRest::Closed);

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] should be subtype of [a: Int] (both Closed)"
        );
    }

    #[test]
    fn test_subtype_closed_to_row_var() {
        let mut sub_fields = IndexMap::new();
        sub_fields.insert("a".into(), Type::Int);
        let sub = Type::Record(sub_fields, RowRest::Closed);

        let mut sup_fields = IndexMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = Type::Record(sup_fields, RowRest::RowVar("r".into(), 0));

        assert!(
            Type::is_subtype(&sub, &sup),
            "[a: Int] (Closed) should be subtype of [a: Int ...r] (RowVar)"
        );
    }

    #[test]
    fn test_subtype_row_var_to_closed() {
        let mut sub_fields = IndexMap::new();
        sub_fields.insert("a".into(), Type::Int);
        sub_fields.insert("b".into(), Type::Int);
        let sub = Type::Record(sub_fields, RowRest::RowVar("r".into(), 0));

        let mut sup_fields = IndexMap::new();
        sup_fields.insert("a".into(), Type::Int);
        let sup = Type::Record(sup_fields, RowRest::Closed);

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
        };
        let sup = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Number),
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_function_contravariant_params() {
        let sub = Type::Function {
            params: vec![Type::Number],
            ret: Box::new(Type::Bool),
        };
        let sup = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
        };
        assert!(Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_function_arity_mismatch() {
        let sub = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
        };
        let sup = Type::Function {
            params: vec![Type::Int, Type::Int],
            ret: Box::new(Type::Bool),
        };
        assert!(!Type::is_subtype(&sub, &sup));
    }

    #[test]
    fn test_subtype_different_kinds() {
        assert!(!Type::is_subtype(&Type::Int, &Type::Str));
        assert!(!Type::is_subtype(&Type::Bool, &Type::Float));
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::Record(IndexMap::new(), RowRest::Closed)
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
        let mut inner_sub = IndexMap::new();
        inner_sub.insert("x".into(), Type::Int);
        inner_sub.insert("y".into(), Type::Int);
        let mut outer_sub = IndexMap::new();
        outer_sub.insert("point".into(), Type::Record(inner_sub, RowRest::Closed));

        let mut inner_sup = IndexMap::new();
        inner_sup.insert("x".into(), Type::Number);
        let mut outer_sup = IndexMap::new();
        outer_sup.insert("point".into(), Type::Record(inner_sup, RowRest::Open));

        assert!(Type::is_subtype(
            &Type::Record(outer_sub, RowRest::Closed),
            &Type::Record(outer_sup, RowRest::Open)
        ));
    }

    #[test]
    fn test_subtype_number_reflexive() {
        assert!(Type::is_subtype(&Type::Number, &Type::Number));
    }

    #[test]
    fn test_subtype_closed_sub_open_sup() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_closed_sub_closed_sup_extra_fields_rejected() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(!Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
    }

    #[test]
    fn test_subtype_closed_exact_match() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
    }

    #[test]
    fn test_subtype_open_sub_open_sup() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Open),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_row_var_behaves_like_open() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::Str);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::Str);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::RowVar("r".into(), 0)),
        ));
    }

    #[test]
    fn test_has_type_vars_primitive() {
        assert!(!Type::Int.has_type_vars());
        assert!(!Type::Str.has_type_vars());
        assert!(!Type::Any.has_type_vars());
    }

    #[test]
    fn test_has_type_vars_type_var() {
        assert!(Type::TypeVar("a".into(), 0).has_type_vars());
    }

    #[test]
    fn test_has_type_vars_function() {
        let with = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::Int),
        };
        let without = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
        };
        assert!(with.has_type_vars());
        assert!(!without.has_type_vars());
    }

    #[test]
    fn test_has_type_vars_record() {
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        assert!(Type::Record(fields, RowRest::Closed).has_type_vars());
    }

    #[test]
    fn test_collect_type_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
        };
        let mut vars = BTreeSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
        assert_eq!(vars.len(), 2);
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
        let child = TypeEnv::with_parent(Rc::new(parent));
        assert_eq!(child.get("x").map(|s| &s.body), Some(&Type::Int));
    }

    #[test]
    fn test_env_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let mut child = TypeEnv::with_parent(Rc::new(parent));
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
        let mut fields = IndexMap::new();
        fields.insert("name".into(), Type::Str);
        env.insert_type_alias(
            "Person".into(),
            Type::Record(fields.clone(), RowRest::Closed),
        );
        assert_eq!(
            env.get_type_alias("Person"),
            Some(&Type::Record(fields, RowRest::Closed))
        );
    }

    #[test]
    fn test_env_type_alias_parent() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias("Base".into(), Type::Int);
        let child = TypeEnv::with_parent(Rc::new(parent));
        assert_eq!(child.get_type_alias("Base"), Some(&Type::Int));
    }

    #[test]
    fn test_env_type_alias_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert_type_alias("T".into(), Type::Int);
        let mut child = TypeEnv::with_parent(Rc::new(parent));
        child.insert_type_alias("T".into(), Type::Str);
        assert_eq!(child.get_type_alias("T"), Some(&Type::Str));
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
        assert_eq!(err.message, "type mismatch: expected Int, got String");
    }

    #[test]
    fn test_type_error_field_not_found() {
        let span = test_span(1, 1, 1, 5);
        let mut fields = IndexMap::new();
        fields.insert("a".into(), Type::Int);
        let err = TypeError::field_not_found("b", &Type::Record(fields, RowRest::Closed), span);
        assert_eq!(err.message, "field 'b' not found in [a: Int]");
    }

    #[test]
    fn test_type_error_undefined_variable() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::undefined_variable("x", span);
        assert_eq!(err.message, "undefined variable: $x");
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
        subst.map.insert("a".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_apply_chain() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.map.insert("b".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into(), 0)), Type::Int);
    }

    #[test]
    fn test_substitution_apply_in_function() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        subst.map.insert("b".into(), Type::Str);
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0)],
            ret: Box::new(Type::TypeVar("b".into(), 0)),
        };
        assert_eq!(
            subst.apply(&ty),
            Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::Str),
            }
        );
    }

    #[test]
    fn test_substitution_apply_in_record() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        fields.insert("y".into(), Type::Str);
        let ty = Type::Record(fields, RowRest::Closed);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::Str);
        assert_eq!(subst.apply(&ty), Type::Record(expected, RowRest::Closed));
    }

    #[test]
    fn test_substitution_leaves_unbound_alone() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("b".into(), 0)),
            Type::TypeVar("b".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_self_reference_cycle() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::TypeVar("a".into(), 0));
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into(), 0)),
            Type::TypeVar("a".into(), 0)
        );
    }

    #[test]
    fn test_substitution_apply_indirect_cycle() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::TypeVar("b".into(), 0));
        subst.map.insert("b".into(), Type::TypeVar("a".into(), 0));
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
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
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
        };
        let f2 = Type::Function {
            params: vec![Type::Int, Type::Int],
            ret: Box::new(Type::Bool),
        };
        let result = unify(&f1, &f2, &mut subst, &mut state, span);
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("function arity mismatch"));
    }

    #[test]
    fn test_unify_record_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::TypeVar("a".into(), 0));
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
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
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        let result = unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("closed record field mismatch"));
    }

    #[test]
    fn test_unify_open_record_extra_fields_accepted() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::Str);
        assert!(unify(
            &Type::Record(f1, RowRest::Open),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            &mut state,
            span,
        )
        .is_ok());
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
        assert!(result.unwrap_err().message.contains("type mismatch"));
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
        assert!(result.unwrap_err().message.contains("type mismatch"));
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
    fn test_instantiate_no_vars() {
        let ty = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Str),
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
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        assert!(!matches!(&result, Type::Function { params, .. }
            if params[0] == Type::TypeVar("a".into(), 0)));
        match &result {
            Type::Function { params, ret } => assert_eq!(params[0], **ret),
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_multiple_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
            ret: Box::new(Type::TypeVar("a".into(), 0)),
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 2);
        match &result {
            Type::Function { params, ret } => {
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
            }),
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::Str),
            }),
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
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into(), 0));
        let result = unify(
            &Type::TypeVar("a".into(), 0),
            &Type::Record(fields, RowRest::Closed),
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
        let mut extra = IndexMap::new();
        extra.insert("y".into(), Type::Str);
        subst
            .map
            .insert("r".into(), Type::Record(extra, RowRest::Closed));

        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into(), 0));
        let result = subst.apply(&ty);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::Str);
        assert_eq!(result, Type::Record(expected, RowRest::Closed));
    }

    #[test]
    fn test_substitution_apply_row_var_to_type_var() {
        let mut subst = Substitution::new();
        subst.map.insert("r".into(), Type::TypeVar("s".into(), 0));

        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into(), 0));
        let result = subst.apply(&ty);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        assert_eq!(
            result,
            Type::Record(expected, RowRest::RowVar("s".into(), 0))
        );
    }

    #[test]
    fn test_substitution_apply_row_var_unbound() {
        let subst = Substitution::new();
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields.clone(), RowRest::RowVar("r".into(), 0));
        let result = subst.apply(&ty);
        assert_eq!(result, Type::Record(fields, RowRest::RowVar("r".into(), 0)));
    }

    #[test]
    fn test_unify_closed_records_same_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut state = InferState::new();
        let mut f1 = IndexMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::Str);
        let mut f2 = IndexMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::Str);
        assert!(unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
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
        let mut f1 = IndexMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("b".into(), Type::Int);
        let result = unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            &mut state,
            span,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("closed record field mismatch"));
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
    fn test_has_type_vars_seq() {
        assert!(Type::Seq(Box::new(Type::TypeVar("a".into(), 0))).has_type_vars());
        assert!(!Type::Seq(Box::new(Type::Int)).has_type_vars());
    }

    #[test]
    fn test_collect_type_vars_seq() {
        let ty = Type::Seq(Box::new(Type::TypeVar("a".into(), 0)));
        let mut vars = BTreeSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert_eq!(vars.len(), 1);
    }

    #[test]
    fn test_substitution_apply_seq() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
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
            RowRest::RowVar("r".into(), 0),
            RowRest::RowVar("r".into(), 7)
        );
    }

    #[test]
    fn test_rowvar_neq_different_name() {
        assert_ne!(
            RowRest::RowVar("r".into(), 0),
            RowRest::RowVar("s".into(), 0)
        );
    }

    #[test]
    fn test_rowvar_display_hides_level() {
        // RowVar appears in record display as "...name"
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into(), 99));
        assert_eq!(format!("{ty}"), "[x: Int  ...r]");
    }

    // --- TypeScheme ---

    #[test]
    fn test_type_scheme_mono_empty_vars() {
        let scheme = TypeScheme::mono(Type::Int);
        assert!(scheme.vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_type_scheme_mono_wraps_body() {
        let body = Type::Function {
            params: vec![Type::Str],
            ret: Box::new(Type::Bool),
        };
        let scheme = TypeScheme::mono(body.clone());
        assert!(scheme.vars.is_empty());
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
            vars: vec!["a".into(), "b".into()],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 0), Type::TypeVar("b".into(), 0)],
                ret: Box::new(Type::TypeVar("a".into(), 0)),
            },
        };
        assert_eq!(format!("{scheme}"), "∀a b. Fn@a [a b]");
    }

    #[test]
    fn test_type_scheme_display_single_var() {
        let scheme = TypeScheme {
            vars: vec!["a".into()],
            body: Type::TypeVar("a".into(), 0),
        };
        assert_eq!(format!("{scheme}"), "∀a. a");
    }

    #[test]
    fn test_type_scheme_partial_eq_same() {
        let s1 = TypeScheme {
            vars: vec!["a".into()],
            body: Type::TypeVar("a".into(), 0),
        };
        let s2 = TypeScheme {
            vars: vec!["a".into()],
            body: Type::TypeVar("a".into(), 0),
        };
        assert_eq!(s1, s2);
    }

    #[test]
    fn test_type_scheme_partial_eq_different_vars() {
        let s1 = TypeScheme {
            vars: vec!["a".into()],
            body: Type::Int,
        };
        let s2 = TypeScheme {
            vars: vec!["b".into()],
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
            vars: vec!["a".into()],
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
            vars: vec!["a".into()],
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

        let mut child = TypeEnv::with_parent(Rc::new(parent));
        let child_scheme = TypeScheme {
            vars: vec!["a".into()],
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
            vars: vec!["a".into(), "b".into()],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 0)],
                ret: Box::new(Type::TypeVar("b".into(), 0)),
            },
        };
        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        // Should get fresh variables at level 3
        match &result {
            Type::Function { params, ret } => {
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
            vars: vec!["a".into()],
            body: Type::TypeVar("a".into(), 0),
        };
        let mut state = InferState::new();

        let inst1 = instantiate_scheme(&scheme, 1, &mut state);
        let inst2 = instantiate_scheme(&scheme, 1, &mut state);

        // Should be different fresh variables
        assert_ne!(inst1, inst2);
    }

    // --- generalize ---

    #[test]
    fn test_generalize_no_vars() {
        let state = InferState::new();
        let ty = Type::Int;
        let scheme = generalize(0, &ty, &state);
        assert!(scheme.vars.is_empty());
        assert_eq!(scheme.body, Type::Int);
    }

    #[test]
    fn test_generalize_var_at_higher_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 2);
        let ty = Type::TypeVar("a".into(), 2);
        let scheme = generalize(1, &ty, &state);
        assert_eq!(scheme.vars, vec!["a"]);
        assert_eq!(scheme.body, ty);
    }

    #[test]
    fn test_generalize_var_at_same_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 1);
        let ty = Type::TypeVar("a".into(), 1);
        let scheme = generalize(1, &ty, &state);
        // Level 1 is NOT > 1, so should not generalize
        assert!(scheme.vars.is_empty());
    }

    #[test]
    fn test_generalize_var_at_lower_level() {
        let mut state = InferState::new();
        state.levels.insert("a".into(), 0);
        let ty = Type::TypeVar("a".into(), 0);
        let scheme = generalize(1, &ty, &state);
        // Level 0 is NOT > 1, so should not generalize
        assert!(scheme.vars.is_empty());
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
        };
        let scheme = generalize(1, &ty, &state);
        // Only a (level 2 > 1) and c (level 3 > 1) should be generalized
        // b is at level 1, not > 1
        assert_eq!(scheme.vars.len(), 2);
        assert!(scheme.vars.contains(&"a".into()));
        assert!(scheme.vars.contains(&"c".into()));
        assert!(!scheme.vars.contains(&"b".into()));
    }

    #[test]
    fn test_generalize_row_vars() {
        let mut state = InferState::new();
        state.levels.insert("r".into(), 2);
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into(), 2));
        let scheme = generalize(1, &ty, &state);
        assert_eq!(scheme.vars, vec!["r"]);
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

    // --- Task 4: instantiate_scheme with row var body ---

    #[test]
    fn test_instantiate_scheme_with_row_var_body() {
        // Create a TypeScheme whose body is Record(fields, RowRest::RowVar("r", 1))
        // with vars: vec!["r"]
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let scheme = TypeScheme {
            vars: vec!["r".into()],
            body: Type::Record(fields.clone(), RowRest::RowVar("r".into(), 1)),
        };

        let mut state = InferState::new();
        state.level = 2;
        let result = instantiate_scheme(&scheme, 2, &mut state);

        // Verify the result has a FRESH RowVar (not the original "r")
        match result {
            Type::Record(result_fields, row_rest) => {
                assert_eq!(result_fields, fields);
                match row_rest {
                    RowRest::RowVar(name, level) => {
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
                    RowRest::Closed => panic!("expected RowVar in result, got Closed"),
                    RowRest::Open => panic!("expected RowVar in result, got Open"),
                }
            }
            other => panic!("expected Record, got {:?}", other),
        }
    }

    // --- Task 5: instantiate_scheme leaves free vars unchanged ---

    #[test]
    fn test_instantiate_scheme_leaves_free_vars_unchanged() {
        // Create a TypeScheme with vars: vec!["a"] and body Function { params: [TypeVar("a", 1)], ret: TypeVar("b", 1) }
        // Only "a" is quantified; "b" is free
        let scheme = TypeScheme {
            vars: vec!["a".into()],
            body: Type::Function {
                params: vec![Type::TypeVar("a".into(), 1)],
                ret: Box::new(Type::TypeVar("b".into(), 1)),
            },
        };

        let mut state = InferState::new();
        state.level = 3;
        let result = instantiate_scheme(&scheme, 3, &mut state);

        match result {
            Type::Function { params, ret } => {
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
}
