//! Runtime type representations, type environments with scoped alias registries,
//! substitutions/unification for Hindley-Milner polymorphism,
//! and type error definitions for the type checker.

use std::collections::BTreeSet;
use std::fmt;
use std::rc::Rc;
use std::string::String as StdString; // Alias avoids shadowing by Type::String

use indexmap::IndexMap;

use crate::ast::Span;

#[derive(Debug, Clone, PartialEq)]
pub enum RowRest {
    Closed,
    Open,
    RowVar(StdString),
}

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)] // TypeVar starts with "Type"; renaming would hurt readability
pub enum Type {
    Int,
    IntLiteral(i64),
    Float,
    String,
    StringLiteral(StdString),
    Bool,
    Number,
    Record(IndexMap<StdString, Type>, RowRest),
    Function { params: Vec<Type>, ret: Box<Type> },
    TypeVar(StdString),
    Any,
}

impl Type {
    /// Recursive without a depth guard; safe because type nesting is bounded by the parser's MAX_DEPTH (256).
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
        if matches!(sub, Type::Any) || matches!(sup, Type::Any) {
            return true;
        }
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::String) => true,
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
                    RowRest::Open | RowRest::RowVar(_) => true,
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

    pub fn collect_type_vars(&self, vars: &mut BTreeSet<StdString>) {
        match self {
            Type::TypeVar(name) => {
                vars.insert(name.clone());
            }
            Type::Record(fields, rest) => {
                for ty in fields.values() {
                    ty.collect_type_vars(vars);
                }
                if let RowRest::RowVar(name) = rest {
                    vars.insert(name.clone());
                }
            }
            Type::Function { params, ret } => {
                for p in params {
                    p.collect_type_vars(vars);
                }
                ret.collect_type_vars(vars);
            }
            _ => {}
        }
    }

    pub fn has_type_vars(&self) -> bool {
        match self {
            Type::TypeVar(_) => true,
            Type::Record(fields, rest) => {
                matches!(rest, RowRest::RowVar(_)) || fields.values().any(|ty| ty.has_type_vars())
            }
            Type::Function { params, ret } => {
                params.iter().any(|p| p.has_type_vars()) || ret.has_type_vars()
            }
            _ => false,
        }
    }
}

// --- Substitution ---

#[derive(Debug, Clone, PartialEq)]
pub struct Substitution {
    map: IndexMap<StdString, Type>,
}

impl Substitution {
    pub fn new() -> Self {
        Self {
            map: IndexMap::new(),
        }
    }

    pub fn apply(&self, ty: &Type) -> Type {
        match ty {
            Type::TypeVar(name) => match self.map.get(name) {
                Some(bound) => self.apply(bound),
                None => ty.clone(),
            },
            Type::Record(fields, rest) => {
                let new_fields: IndexMap<StdString, Type> = fields
                    .iter()
                    .map(|(k, v)| (k.clone(), self.apply(v)))
                    .collect();
                match rest {
                    RowRest::RowVar(name) => match self.map.get(name) {
                        Some(bound) => {
                            let resolved = self.apply(bound);
                            match resolved {
                                Type::Record(extra_fields, resolved_rest) => {
                                    let mut merged = new_fields;
                                    merged.extend(extra_fields);
                                    Type::Record(merged, resolved_rest)
                                }
                                Type::TypeVar(new_name) => {
                                    Type::Record(new_fields, RowRest::RowVar(new_name))
                                }
                                _ => Type::Record(new_fields, rest.clone()),
                            }
                        }
                        None => Type::Record(new_fields, rest.clone()),
                    },
                    _ => Type::Record(new_fields, rest.clone()),
                }
            }
            Type::Function { params, ret } => Type::Function {
                params: params.iter().map(|p| self.apply(p)).collect(),
                ret: Box::new(self.apply(ret)),
            },
            _ => ty.clone(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&Type> {
        self.map.get(name)
    }
}

impl Default for Substitution {
    fn default() -> Self {
        Self::new()
    }
}

// --- Unification ---

fn occurs_in(var_name: &str, ty: &Type) -> bool {
    match ty {
        Type::TypeVar(name) => name == var_name,
        Type::Record(fields, rest) => {
            fields.values().any(|t| occurs_in(var_name, t))
                || matches!(rest, RowRest::RowVar(r) if r == var_name)
        }
        Type::Function { params, ret } => {
            params.iter().any(|p| occurs_in(var_name, p)) || occurs_in(var_name, ret)
        }
        _ => false,
    }
}

pub fn unify(a: &Type, b: &Type, subst: &mut Substitution, span: Span) -> Result<(), TypeError> {
    let a = subst.apply(a);
    let b = subst.apply(b);

    if a == b {
        return Ok(());
    }

    match (&a, &b) {
        (Type::Any, _) | (_, Type::Any) => Ok(()),

        (Type::TypeVar(name), _) => {
            if occurs_in(name, &b) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {b}"),
                    span,
                ));
            }
            subst.map.insert(name.clone(), b);
            Ok(())
        }
        (_, Type::TypeVar(name)) => {
            if occurs_in(name, &a) {
                return Err(TypeError::new(
                    format!("infinite type: {name} occurs in {a}"),
                    span,
                ));
            }
            subst.map.insert(name.clone(), a);
            Ok(())
        }

        // Literal-to-parent promotions
        (Type::IntLiteral(_), Type::Int | Type::Number) | (Type::Int, Type::Number) => Ok(()),
        (Type::Int | Type::Number, Type::IntLiteral(_)) | (Type::Number, Type::Int) => Ok(()),
        (Type::Float, Type::Number) | (Type::Number, Type::Float) => Ok(()),
        (Type::StringLiteral(_), Type::String) | (Type::String, Type::StringLiteral(_)) => Ok(()),
        (Type::IntLiteral(_), Type::IntLiteral(_)) => Ok(()),
        (Type::StringLiteral(_), Type::StringLiteral(_)) => Ok(()),
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
                unify(pa, pb, subst, span)?;
            }
            unify(r1, r2, subst, span)
        }

        (Type::Record(f1, r1), Type::Record(f2, r2)) => {
            if matches!(r1, RowRest::Closed) && matches!(r2, RowRest::Closed) {
                let keys1: BTreeSet<&StdString> = f1.keys().collect();
                let keys2: BTreeSet<&StdString> = f2.keys().collect();
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
                    unify(ty1, ty2, subst, span)?;
                }
            }
            Ok(())
        }

        _ => Err(TypeError::type_mismatch(&a, &b, span)),
    }
}

// --- Instantiation ---

pub fn instantiate(ty: &Type, counter: &mut u32) -> (Type, Substitution) {
    let mut vars = BTreeSet::new();
    ty.collect_type_vars(&mut vars);

    let mut renaming = Substitution::new();
    for var in vars {
        let fresh = format!("_t{counter}");
        *counter += 1;
        renaming.map.insert(var, Type::TypeVar(fresh));
    }

    (renaming.apply(ty), renaming)
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::Int => write!(f, "Int"),
            Type::IntLiteral(n) => write!(f, "{n}"),
            Type::Float => write!(f, "Float"),
            Type::String => write!(f, "String"),
            Type::StringLiteral(s) => write!(f, "\"{s}\""),
            Type::Bool => write!(f, "Bool"),
            Type::Number => write!(f, "Number"),
            Type::Any => write!(f, "Any"),
            Type::TypeVar(name) => write!(f, "{name}"),
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
                    RowRest::RowVar(name) => {
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
        }
    }
}

// --- TypeEnv ---

#[derive(Debug, Clone)]
pub struct TypeEnv {
    bindings: IndexMap<StdString, Type>,
    type_aliases: IndexMap<StdString, Type>,
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

    pub fn get(&self, name: &str) -> Option<&Type> {
        self.lookup(|env| env.bindings.get(name))
    }

    pub fn get_type_alias(&self, name: &str) -> Option<&Type> {
        self.lookup(|env| env.type_aliases.get(name))
    }

    fn lookup(&self, field: impl Fn(&TypeEnv) -> Option<&Type>) -> Option<&Type> {
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

    pub fn insert(&mut self, name: StdString, ty: Type) {
        self.bindings.insert(name, ty);
    }

    pub fn insert_type_alias(&mut self, name: StdString, ty: Type) {
        self.type_aliases.insert(name, ty);
    }
}

impl Default for TypeEnv {
    fn default() -> Self {
        Self::new()
    }
}

// --- TypeError ---

#[derive(Debug, Clone, PartialEq)]
pub struct TypeError {
    pub message: StdString,
    pub span: Span,
}

impl TypeError {
    pub fn new(message: impl Into<StdString>, span: Span) -> Self {
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

    // --- Type::Display ---

    #[test]
    fn test_display_primitives() {
        assert_eq!(format!("{}", Type::Int), "Int");
        assert_eq!(format!("{}", Type::Float), "Float");
        assert_eq!(format!("{}", Type::String), "String");
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
        assert_eq!(format!("{}", Type::TypeVar("a".into())), "a");
    }

    #[test]
    fn test_display_record() {
        let mut fields = IndexMap::new();
        fields.insert("name".into(), Type::String);
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
        fields.insert("name".into(), Type::String);
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
        fields.insert("name".into(), Type::String);
        assert_eq!(
            format!("{}", Type::Record(fields, RowRest::RowVar("rest".into()))),
            "[name: String  ...rest]"
        );
    }

    #[test]
    fn test_display_function() {
        let ty = Type::Function {
            params: vec![Type::Int, Type::String],
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

    // --- Type::is_subtype ---

    #[test]
    fn test_subtype_same() {
        assert!(Type::is_subtype(&Type::Int, &Type::Int));
        assert!(Type::is_subtype(&Type::String, &Type::String));
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
            &Type::String
        ));
        assert!(!Type::is_subtype(
            &Type::String,
            &Type::StringLiteral("a".into())
        ));
    }

    #[test]
    fn test_subtype_number() {
        assert!(Type::is_subtype(&Type::Int, &Type::Number));
        assert!(Type::is_subtype(&Type::Float, &Type::Number));
        assert!(!Type::is_subtype(&Type::Number, &Type::Int));
        assert!(!Type::is_subtype(&Type::String, &Type::Number));
    }

    #[test]
    fn test_subtype_record_structural() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);
        sub.insert("age".into(), Type::Int);
        sub.insert("extra".into(), Type::Bool);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);
        sup.insert("age".into(), Type::Int);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_record_missing_field() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);
        sup.insert("age".into(), Type::Int);

        assert!(!Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
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
        assert!(!Type::is_subtype(&Type::Int, &Type::String));
        assert!(!Type::is_subtype(&Type::Bool, &Type::Float));
        assert!(!Type::is_subtype(
            &Type::Int,
            &Type::Record(IndexMap::new(), RowRest::Closed)
        ));
    }

    #[test]
    fn test_subtype_type_var() {
        assert!(Type::is_subtype(
            &Type::TypeVar("a".into()),
            &Type::TypeVar("a".into())
        ));
        assert!(!Type::is_subtype(
            &Type::TypeVar("a".into()),
            &Type::TypeVar("b".into())
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
        sub.insert("name".into(), Type::String);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_closed_sub_closed_sup_extra_fields_rejected() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);

        assert!(!Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
    }

    #[test]
    fn test_subtype_closed_exact_match() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::Closed),
        ));
    }

    #[test]
    fn test_subtype_open_sub_open_sup() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Open),
            &Type::Record(sup, RowRest::Open),
        ));
    }

    #[test]
    fn test_subtype_row_var_behaves_like_open() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);
        sub.insert("age".into(), Type::Int);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);

        assert!(Type::is_subtype(
            &Type::Record(sub, RowRest::Closed),
            &Type::Record(sup, RowRest::RowVar("r".into())),
        ));
    }

    // --- Type::has_type_vars ---

    #[test]
    fn test_has_type_vars_primitive() {
        assert!(!Type::Int.has_type_vars());
        assert!(!Type::String.has_type_vars());
        assert!(!Type::Any.has_type_vars());
    }

    #[test]
    fn test_has_type_vars_type_var() {
        assert!(Type::TypeVar("a".into()).has_type_vars());
    }

    #[test]
    fn test_has_type_vars_function() {
        let with = Type::Function {
            params: vec![Type::TypeVar("a".into())],
            ret: Box::new(Type::Int),
        };
        let without = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::String),
        };
        assert!(with.has_type_vars());
        assert!(!without.has_type_vars());
    }

    #[test]
    fn test_has_type_vars_record() {
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into()));
        assert!(Type::Record(fields, RowRest::Closed).has_type_vars());
    }

    // --- Type::collect_type_vars ---

    #[test]
    fn test_collect_type_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into()), Type::TypeVar("b".into())],
            ret: Box::new(Type::TypeVar("a".into())),
        };
        let mut vars = BTreeSet::new();
        ty.collect_type_vars(&mut vars);
        assert!(vars.contains("a"));
        assert!(vars.contains("b"));
        assert_eq!(vars.len(), 2);
    }

    // --- TypeEnv ---

    #[test]
    fn test_env_get_current() {
        let mut env = TypeEnv::new();
        env.insert("x".into(), Type::Int);
        assert_eq!(env.get("x"), Some(&Type::Int));
    }

    #[test]
    fn test_env_get_parent() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let child = TypeEnv::with_parent(Rc::new(parent));
        assert_eq!(child.get("x"), Some(&Type::Int));
    }

    #[test]
    fn test_env_shadow() {
        let mut parent = TypeEnv::new();
        parent.insert("x".into(), Type::Int);
        let mut child = TypeEnv::with_parent(Rc::new(parent));
        child.insert("x".into(), Type::String);
        assert_eq!(child.get("x"), Some(&Type::String));
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
        fields.insert("name".into(), Type::String);
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
        child.insert_type_alias("T".into(), Type::String);
        assert_eq!(child.get_type_alias("T"), Some(&Type::String));
    }

    // --- TypeError ---

    #[test]
    fn test_type_error_display() {
        let span = test_span(3, 5, 3, 10);
        let err = TypeError::new("oops", span);
        assert_eq!(format!("{err}"), "oops at 3:5-3:10");
    }

    #[test]
    fn test_type_error_type_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let err = TypeError::type_mismatch(&Type::Int, &Type::String, span);
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
        let err = TypeError::not_a_function(&Type::String, span);
        assert_eq!(err.message, "expected function type, got String");
    }

    // --- Substitution ---

    #[test]
    fn test_substitution_empty_apply() {
        let subst = Substitution::new();
        assert_eq!(subst.apply(&Type::Int), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("a".into())),
            Type::TypeVar("a".into())
        );
    }

    #[test]
    fn test_substitution_apply_bound() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into())), Type::Int);
    }

    #[test]
    fn test_substitution_apply_chain() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::TypeVar("b".into()));
        subst.map.insert("b".into(), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("a".into())), Type::Int);
    }

    #[test]
    fn test_substitution_apply_in_function() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        subst.map.insert("b".into(), Type::String);
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into())],
            ret: Box::new(Type::TypeVar("b".into())),
        };
        assert_eq!(
            subst.apply(&ty),
            Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::String),
            }
        );
    }

    #[test]
    fn test_substitution_apply_in_record() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into()));
        fields.insert("y".into(), Type::String);
        let ty = Type::Record(fields, RowRest::Closed);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::String);
        assert_eq!(subst.apply(&ty), Type::Record(expected, RowRest::Closed));
    }

    #[test]
    fn test_substitution_leaves_unbound_alone() {
        let mut subst = Substitution::new();
        subst.map.insert("a".into(), Type::Int);
        assert_eq!(
            subst.apply(&Type::TypeVar("b".into())),
            Type::TypeVar("b".into())
        );
    }

    // --- Unification ---

    #[test]
    fn test_unify_identical_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(&Type::Int, &Type::Int, &mut subst, span).is_ok());
        assert!(unify(&Type::String, &Type::String, &mut subst, span).is_ok());
        assert!(unify(&Type::Bool, &Type::Bool, &mut subst, span).is_ok());
    }

    #[test]
    fn test_unify_typevar_with_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        unify(&Type::TypeVar("a".into()), &Type::Int, &mut subst, span).unwrap();
        assert_eq!(subst.get("a"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_concrete_with_typevar() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        unify(&Type::Int, &Type::TypeVar("a".into()), &mut subst, span).unwrap();
        assert_eq!(subst.get("a"), Some(&Type::Int));
    }

    #[test]
    fn test_unify_two_typevars() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        unify(
            &Type::TypeVar("a".into()),
            &Type::TypeVar("b".into()),
            &mut subst,
            span,
        )
        .unwrap();
        let resolved = subst.apply(&Type::TypeVar("a".into()));
        assert_eq!(resolved, subst.apply(&Type::TypeVar("b".into())));
    }

    #[test]
    fn test_unify_typevar_already_bound_compatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        unify(&Type::TypeVar("a".into()), &Type::Int, &mut subst, span).unwrap();
        unify(&Type::TypeVar("a".into()), &Type::Int, &mut subst, span).unwrap();
    }

    #[test]
    fn test_unify_typevar_already_bound_incompatible() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        unify(&Type::TypeVar("a".into()), &Type::Int, &mut subst, span).unwrap();
        let result = unify(&Type::TypeVar("a".into()), &Type::String, &mut subst, span);
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let f1 = Type::Function {
            params: vec![Type::TypeVar("a".into())],
            ret: Box::new(Type::TypeVar("b".into())),
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::String),
        };
        unify(&f1, &f2, &mut subst, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into())), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into())), Type::String);
    }

    #[test]
    fn test_unify_function_arity_mismatch() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let f1 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Bool),
        };
        let f2 = Type::Function {
            params: vec![Type::Int, Type::Int],
            ret: Box::new(Type::Bool),
        };
        let result = unify(&f1, &f2, &mut subst, span);
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
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::TypeVar("a".into()));
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            span,
        )
        .unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into())), Type::Int);
    }

    #[test]
    fn test_unify_closed_record_extra_fields_rejected() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::String);
        let result = unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
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
        let mut f1 = IndexMap::new();
        f1.insert("x".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("x".into(), Type::Int);
        f2.insert("y".into(), Type::String);
        assert!(unify(
            &Type::Record(f1, RowRest::Open),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_any_with_anything() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(&Type::Any, &Type::Int, &mut subst, span).is_ok());
        assert!(unify(&Type::String, &Type::Any, &mut subst, span).is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_int() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(&Type::IntLiteral(42), &Type::Int, &mut subst, span).is_ok());
        assert!(unify(&Type::Int, &Type::IntLiteral(99), &mut subst, span).is_ok());
    }

    #[test]
    fn test_unify_int_literal_with_number() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(&Type::IntLiteral(42), &Type::Number, &mut subst, span).is_ok());
    }

    #[test]
    fn test_unify_string_literal_with_string() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(
            &Type::StringLiteral("hi".into()),
            &Type::String,
            &mut subst,
            span
        )
        .is_ok());
        assert!(unify(
            &Type::String,
            &Type::StringLiteral("lo".into()),
            &mut subst,
            span
        )
        .is_ok());
    }

    #[test]
    fn test_unify_incompatible_concrete() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let result = unify(&Type::Int, &Type::String, &mut subst, span);
        assert!(result.is_err());
    }

    #[test]
    fn test_unify_int_with_bool() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        assert!(unify(&Type::Int, &Type::Bool, &mut subst, span).is_err());
    }

    // --- Instantiation ---

    #[test]
    fn test_instantiate_no_vars() {
        let ty = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::String),
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, ty);
        assert_eq!(counter, 0);
    }

    #[test]
    fn test_instantiate_with_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into())],
            ret: Box::new(Type::TypeVar("a".into())),
        };
        let mut counter = 0;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(counter, 1);
        assert!(!matches!(&result, Type::Function { params, .. }
            if params[0] == Type::TypeVar("a".into())));
        match &result {
            Type::Function { params, ret } => assert_eq!(params[0], **ret),
            _ => panic!("expected Function"),
        }
    }

    #[test]
    fn test_instantiate_multiple_vars() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into()), Type::TypeVar("b".into())],
            ret: Box::new(Type::TypeVar("a".into())),
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
        let ty = Type::TypeVar("x".into());
        let mut counter = 5;
        let (result, _) = instantiate(&ty, &mut counter);
        assert_eq!(result, Type::TypeVar("_t5".into()));
        assert_eq!(counter, 6);
    }

    #[test]
    fn test_unify_nested_function_types() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let f1 = Type::Function {
            params: vec![Type::TypeVar("a".into())],
            ret: Box::new(Type::Function {
                params: vec![Type::TypeVar("a".into())],
                ret: Box::new(Type::TypeVar("b".into())),
            }),
        };
        let f2 = Type::Function {
            params: vec![Type::Int],
            ret: Box::new(Type::Function {
                params: vec![Type::Int],
                ret: Box::new(Type::String),
            }),
        };
        unify(&f1, &f2, &mut subst, span).unwrap();
        assert_eq!(subst.apply(&Type::TypeVar("a".into())), Type::Int);
        assert_eq!(subst.apply(&Type::TypeVar("b".into())), Type::String);
    }

    // --- Occurs check (M1) ---

    #[test]
    fn test_unify_occurs_check_direct() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let result = unify(
            &Type::TypeVar("a".into()),
            &Type::Function {
                params: vec![Type::TypeVar("a".into())],
                ret: Box::new(Type::Int),
            },
            &mut subst,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_nested() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::TypeVar("a".into()));
        let result = unify(
            &Type::TypeVar("a".into()),
            &Type::Record(fields, RowRest::Closed),
            &mut subst,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    #[test]
    fn test_unify_occurs_check_reverse() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let result = unify(
            &Type::Function {
                params: vec![Type::TypeVar("a".into())],
                ret: Box::new(Type::Int),
            },
            &Type::TypeVar("a".into()),
            &mut subst,
            span,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().message.contains("infinite type"));
    }

    // --- RowVar substitution (M2) ---

    #[test]
    fn test_substitution_apply_row_var_to_record() {
        let mut subst = Substitution::new();
        let mut extra = IndexMap::new();
        extra.insert("y".into(), Type::String);
        subst
            .map
            .insert("r".into(), Type::Record(extra, RowRest::Closed));

        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into()));
        let result = subst.apply(&ty);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        expected.insert("y".into(), Type::String);
        assert_eq!(result, Type::Record(expected, RowRest::Closed));
    }

    #[test]
    fn test_substitution_apply_row_var_to_type_var() {
        let mut subst = Substitution::new();
        subst.map.insert("r".into(), Type::TypeVar("s".into()));

        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields, RowRest::RowVar("r".into()));
        let result = subst.apply(&ty);

        let mut expected = IndexMap::new();
        expected.insert("x".into(), Type::Int);
        assert_eq!(result, Type::Record(expected, RowRest::RowVar("s".into())));
    }

    #[test]
    fn test_substitution_apply_row_var_unbound() {
        let subst = Substitution::new();
        let mut fields = IndexMap::new();
        fields.insert("x".into(), Type::Int);
        let ty = Type::Record(fields.clone(), RowRest::RowVar("r".into()));
        let result = subst.apply(&ty);
        assert_eq!(result, Type::Record(fields, RowRest::RowVar("r".into())));
    }

    // --- Closed record key matching (M5) ---

    #[test]
    fn test_unify_closed_records_same_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut f1 = IndexMap::new();
        f1.insert("a".into(), Type::Int);
        f1.insert("b".into(), Type::String);
        let mut f2 = IndexMap::new();
        f2.insert("a".into(), Type::Int);
        f2.insert("b".into(), Type::String);
        assert!(unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            span,
        )
        .is_ok());
    }

    #[test]
    fn test_unify_closed_records_different_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut subst = Substitution::new();
        let mut f1 = IndexMap::new();
        f1.insert("a".into(), Type::Int);
        let mut f2 = IndexMap::new();
        f2.insert("b".into(), Type::Int);
        let result = unify(
            &Type::Record(f1, RowRest::Closed),
            &Type::Record(f2, RowRest::Closed),
            &mut subst,
            span,
        );
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .message
            .contains("closed record field mismatch"));
    }
}
