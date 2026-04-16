//! Runtime type representations, type environments with scoped alias registries,
//! and type error definitions for the type checker.

use std::fmt;
use std::rc::Rc;
use std::string::String as StdString;

use indexmap::IndexMap;

use crate::ast::Span;

#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::enum_variant_names)]
pub enum Type {
    Int,
    IntLiteral(i64),
    Float,
    String,
    StringLiteral(StdString),
    Bool,
    Number,
    Record(IndexMap<StdString, Type>),
    Function { params: Vec<Type>, ret: Box<Type> },
    TypeVar(StdString),
    Any,
}

impl Type {
    pub fn is_subtype(sub: &Type, sup: &Type) -> bool {
        if matches!(sub, Type::Any) || matches!(sup, Type::Any) {
            return true;
        }
        match (sub, sup) {
            (a, b) if a == b => true,
            (Type::IntLiteral(_), Type::Int | Type::Number) => true,
            (Type::StringLiteral(_), Type::String) => true,
            (Type::Int | Type::Float, Type::Number) => true,
            (Type::Record(sub_fields), Type::Record(sup_fields)) => {
                sup_fields.iter().all(|(k, sup_ty)| {
                    sub_fields
                        .get(k)
                        .map_or(false, |sub_ty| Type::is_subtype(sub_ty, sup_ty))
                })
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
            Type::Record(fields) => {
                write!(f, "[")?;
                for (i, (key, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, "  ")?;
                    }
                    write!(f, "{key}: {ty}")?;
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

    // -- Type::Display --

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
        assert_eq!(format!("{}", Type::StringLiteral("hello".into())), "\"hello\"");
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
            format!("{}", Type::Record(fields)),
            "[name: String  age: Int]"
        );
    }

    #[test]
    fn test_display_record_empty() {
        assert_eq!(format!("{}", Type::Record(IndexMap::new())), "[]");
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

    // -- Type::is_subtype --

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
        assert!(Type::is_subtype(&Type::IntLiteral(42), &Type::IntLiteral(42)));
        assert!(!Type::is_subtype(&Type::IntLiteral(42), &Type::IntLiteral(99)));
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
        assert!(Type::is_subtype(&Type::StringLiteral("a".into()), &Type::String));
        assert!(!Type::is_subtype(&Type::String, &Type::StringLiteral("a".into())));
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

        assert!(Type::is_subtype(&Type::Record(sub), &Type::Record(sup),));
    }

    #[test]
    fn test_subtype_record_missing_field() {
        let mut sub = IndexMap::new();
        sub.insert("name".into(), Type::String);

        let mut sup = IndexMap::new();
        sup.insert("name".into(), Type::String);
        sup.insert("age".into(), Type::Int);

        assert!(!Type::is_subtype(&Type::Record(sub), &Type::Record(sup),));
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
            &Type::Record(IndexMap::new())
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
        outer_sub.insert("point".into(), Type::Record(inner_sub));

        let mut inner_sup = IndexMap::new();
        inner_sup.insert("x".into(), Type::Number);
        let mut outer_sup = IndexMap::new();
        outer_sup.insert("point".into(), Type::Record(inner_sup));

        assert!(Type::is_subtype(
            &Type::Record(outer_sub),
            &Type::Record(outer_sup)
        ));
    }

    #[test]
    fn test_subtype_number_reflexive() {
        assert!(Type::is_subtype(&Type::Number, &Type::Number));
    }

    // -- TypeEnv --

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
        env.insert_type_alias("Person".into(), Type::Record(fields.clone()));
        assert_eq!(env.get_type_alias("Person"), Some(&Type::Record(fields)));
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

    // -- TypeError --

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
        let err = TypeError::field_not_found("b", &Type::Record(fields), span);
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
}
