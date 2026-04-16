//! Type checker: infers types from AST expressions, resolves type aliases,
//! and validates type assertions and annotations.

use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::*;
use crate::types::*;

// --- Public API ---

pub fn typecheck_file(file: &File) -> Result<(), Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::new());

    for doc in &file.documents {
        match typecheck_document(doc, &env) {
            Ok(new_env) => env = new_env,
            Err(mut doc_errors) => errors.append(&mut doc_errors),
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

// --- Document-level type inference ---

fn typecheck_document(
    doc: &Spanned<Document>,
    parent_env: &Rc<TypeEnv>,
) -> Result<Rc<TypeEnv>, Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::with_parent(Rc::clone(parent_env)));
    let mut result_type = Type::Record(IndexMap::new());

    let exprs = &doc.node.expressions;
    if exprs.is_empty() {
        let mut result_env = TypeEnv::with_parent(Rc::clone(&env));
        result_env.insert("$".to_string(), Type::Record(IndexMap::new()));
        return Ok(Rc::new(result_env));
    }

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;
        match infer_expr(expr, &env) {
            Ok(ty) => {
                if is_last {
                    result_type = ty;
                } else {
                    match &ty {
                        Type::Record(fields) => {
                            let mut new_env = TypeEnv::with_parent(Rc::clone(&env));
                            for (name, field_ty) in fields {
                                new_env.insert(name.clone(), field_ty.clone());
                            }
                            register_type_aliases(expr, &mut new_env, &env);
                            env = Rc::new(new_env);
                        }
                        Type::Any => {}
                        _ => errors.push(TypeError::not_a_record(&ty, expr.span)),
                    }
                }
            }
            Err(e) => errors.push(e),
        }
    }

    let mut result_env = TypeEnv::with_parent(env);
    result_env.insert("$".to_string(), result_type);

    if errors.is_empty() {
        Ok(Rc::new(result_env))
    } else {
        Err(errors)
    }
}

fn register_type_aliases(
    expr: &Spanned<Expr>,
    target_env: &mut TypeEnv,
    resolve_env: &TypeEnv,
) {
    if let Expr::Dict(entries) = &expr.node {
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let Expr::Str(name) = &key.node {
                    if let Expr::TypeAlias(inner) = &entry.node.value.node {
                        if let Ok(alias_ty) = resolve_type_expr(inner, resolve_env) {
                            target_env.insert_type_alias(name.clone(), alias_ty);
                        }
                    }
                }
            }
        }
    }
}

// --- Expression type inference ---

fn infer_expr(expr: &Spanned<Expr>, env: &Rc<TypeEnv>) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Int(n) => Ok(Type::IntLiteral(*n)),
        Expr::Float(_) => Ok(Type::Float),
        Expr::Bool(_) => Ok(Type::Bool),
        Expr::Str(s) => Ok(Type::StringLiteral(s.clone())),

        Expr::VarRef(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| TypeError::undefined_variable(name, expr.span)),

        Expr::Dict(entries) => infer_dict(entries, env),

        Expr::DotAccess {
            expr: target,
            field,
        } => check_dot_access(target, field, env, expr.span),

        Expr::BracketAccess { expr: target, key } => {
            check_bracket_access(target, key, env, expr.span)
        }

        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => check_range_access(target, start, end, env, expr.span),

        Expr::Call {
            func,
            args,
            named_args,
        } => check_call(func, args, named_args, env, expr.span),

        Expr::Fn {
            return_ann,
            params,
            body,
        } => infer_fn(return_ann, params, body, env, expr.span),

        Expr::TypeAlias(inner) => expand_type_alias(inner, env),

        Expr::TypeAssert {
            annotation,
            expr: inner,
        } => resolve_type_assert(annotation, inner, env, expr.span),

        Expr::Annotated { name, annotation } => resolve_annotated(name, annotation, env, expr.span),
    }
}

// --- Record type construction ---

fn infer_dict(entries: &[Spanned<Entry>], env: &Rc<TypeEnv>) -> Result<Type, TypeError> {
    let mut dict_env = TypeEnv::with_parent(Rc::clone(env));
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    // Pass 0+1: resolve key names and bind all resolved keys to Any.
    // Literal keys are extracted directly. Computed keys are resolved via type
    // inference in the parent env. Unresolvable computed keys get None.
    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env);
        let is_alias = matches!(&entry.node.value.node, Expr::TypeAlias(_));
        if let Some(ref name) = key_name {
            dict_env.insert(name.clone(), Type::Any);
        }
        key_entries.push((key_name, is_alias));
    }

    // Pass 2: register type aliases sequentially (each sees previously registered siblings).
    // Cycles cannot occur: each alias is resolved against only the *previously* registered
    // aliases (not itself), so forward references fail in resolve_type_expr and are silently
    // dropped by the `if let Ok(...)` guard. A self-referencing alias like `T: [type T]`
    // would look up "T" before it is registered, get an undefined-type error, and be skipped.
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            if let Some(name) = key_name {
                if let Expr::TypeAlias(inner) = &entry.node.value.node {
                    if let Ok(alias_ty) = resolve_type_expr(inner, &dict_env) {
                        dict_env.insert_type_alias(name.clone(), alias_ty);
                    }
                }
            }
        }
    }

    let dict_env = Rc::new(dict_env);

    // Pass 3: infer value types (accumulate errors, substitute Any for failures).
    // Entries with unresolvable computed keys are type-checked but excluded from
    // the Record's fields since the field name is not statically known.
    let mut fields = IndexMap::new();
    let mut errors = Vec::new();
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias {
            continue;
        }
        match infer_expr(&entry.node.value, &dict_env) {
            Ok(value_ty) => {
                if let Some(name) = key_name {
                    fields.insert(name.clone(), value_ty);
                }
            }
            Err(e) => {
                errors.push(e);
                if let Some(name) = key_name {
                    fields.insert(name.clone(), Type::Any);
                }
            }
        }
    }

    if let Some(first) = errors.into_iter().next() {
        Err(first)
    } else {
        Ok(Type::Record(fields))
    }
}

fn entry_key_name(entry: &Entry, auto_index: &mut i64, env: &Rc<TypeEnv>) -> Option<String> {
    match &entry.key {
        Some(key_expr) => match &key_expr.node {
            Expr::Str(s) => Some(s.clone()),
            Expr::Int(n) => Some(n.to_string()),
            _ => match infer_expr(key_expr, env) {
                Ok(Type::StringLiteral(s)) => Some(s),
                Ok(Type::IntLiteral(n)) => Some(n.to_string()),
                _ => None,
            },
        },
        None => {
            let name = auto_index.to_string();
            *auto_index += 1;
            Some(name)
        }
    }
}

// --- Access chain type checking ---

fn check_dot_access(
    target: &Spanned<Expr>,
    field: &str,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let target_ty = infer_expr(target, env)?;
    match &target_ty {
        Type::Record(fields) => fields
            .get(field)
            .cloned()
            .ok_or_else(|| TypeError::field_not_found(field, &target_ty, span)),
        Type::Any => Ok(Type::Any),
        _ => Err(TypeError::not_a_record(&target_ty, span)),
    }
}

fn check_bracket_access(
    target: &Spanned<Expr>,
    key: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let target_ty = infer_expr(target, env)?;
    let key_ty = infer_expr(key, env)?;

    match &target_ty {
        Type::Record(fields) => match &key.node {
            Expr::Str(s) => fields
                .get(s)
                .cloned()
                .ok_or_else(|| TypeError::field_not_found(s, &target_ty, span)),
            Expr::Int(n) => {
                let key_str = n.to_string();
                fields
                    .get(&key_str)
                    .cloned()
                    .ok_or_else(|| TypeError::field_not_found(&key_str, &target_ty, span))
            }
            _ => match &key_ty {
                Type::StringLiteral(s) => fields
                    .get(s.as_str())
                    .cloned()
                    .ok_or_else(|| TypeError::field_not_found(s, &target_ty, span)),
                Type::IntLiteral(n) => {
                    let key_str = n.to_string();
                    fields
                        .get(&key_str)
                        .cloned()
                        .ok_or_else(|| TypeError::field_not_found(&key_str, &target_ty, span))
                }
                Type::String | Type::Int | Type::Any => Ok(Type::Any),
                _ => Err(TypeError::new(
                    format!("bracket access key must be String or Int, got {key_ty}"),
                    span,
                )),
            },
        },
        Type::Any => Ok(Type::Any),
        _ => Err(TypeError::not_a_record(&target_ty, span)),
    }
}

fn check_range_access(
    target: &Spanned<Expr>,
    start: &Option<Box<Spanned<Expr>>>,
    end: &Option<Box<Spanned<Expr>>>,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let target_ty = infer_expr(target, env)?;

    for bound in [start, end].into_iter().flatten() {
        let bound_ty = infer_expr(bound, env)?;
        if !matches!(
            bound_ty,
            Type::Int | Type::IntLiteral(_) | Type::String | Type::StringLiteral(_) | Type::Any
        ) {
            return Err(TypeError::new(
                format!("range bound must be Int or String, got {bound_ty}"),
                bound.span,
            ));
        }
    }

    match &target_ty {
        Type::Record(_) | Type::Any => Ok(target_ty),
        _ => Err(TypeError::not_a_record(&target_ty, span)),
    }
}

// --- Call type checking ---

fn check_call(
    func: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let func_ty = infer_expr(func, env)?;

    for arg in args {
        let _ = infer_expr(arg, env)?;
    }
    for na in named_args {
        let _ = infer_expr(&na.node.value, env)?;
    }

    match &func_ty {
        Type::Function { ret, .. } => Ok(*ret.clone()),
        Type::Any => Ok(Type::Any),
        _ => Err(TypeError::not_a_function(&func_ty, span)),
    }
}

// --- Function type inference ---

fn infer_fn(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<Param>],
    body: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let param_types: Vec<Type> = params
        .iter()
        .map(|p| match &p.node.annotation {
            Some(ann) => resolve_annotation(&ann.node, env, ann.span),
            None => Ok(Type::Any),
        })
        .collect::<Result<_, _>>()?;

    let mut fn_env = TypeEnv::with_parent(Rc::clone(env));
    for (param, ty) in params.iter().zip(param_types.iter()) {
        if param.node.variadic {
            fn_env.insert(param.node.name.clone(), Type::Record(IndexMap::new()));
        } else {
            fn_env.insert(param.node.name.clone(), ty.clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            let declared = resolve_annotation(&ann.node, env, ann.span)?;
            let inferred = infer_expr(body, &fn_env)?;
            if !Type::is_subtype(&inferred, &declared) {
                return Err(TypeError::type_mismatch(&declared, &inferred, span));
            }
            declared
        }
        None => infer_expr(body, &fn_env)?,
    };

    Ok(Type::Function {
        params: param_types,
        ret: Box::new(ret_type),
    })
}

// --- Type alias expansion ---

fn expand_type_alias(inner: &Spanned<Expr>, env: &Rc<TypeEnv>) -> Result<Type, TypeError> {
    let _ = resolve_type_expr(inner, env)?;
    Ok(Type::Any)
}

// --- TypeAssert enforcement ---

fn resolve_type_assert(
    annotation: &Spanned<Annotation>,
    inner: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    let expected = resolve_annotation(&annotation.node, env, annotation.span)?;
    let actual = infer_expr(inner, env)?;

    if !Type::is_subtype(&actual, &expected) {
        return Err(TypeError::type_mismatch(&expected, &actual, span));
    }

    Ok(expected)
}

// --- Annotated node interpretation ---

fn resolve_annotated(
    name: &str,
    annotation: &Spanned<Annotation>,
    env: &Rc<TypeEnv>,
    span: Span,
) -> Result<Type, TypeError> {
    if name == "Fn" {
        resolve_fn_type(&annotation.node, env, annotation.span)
    } else {
        resolve_annotation(&annotation.node, env, span)
    }
}

fn resolve_fn_type(ann: &Annotation, env: &TypeEnv, span: Span) -> Result<Type, TypeError> {
    let ret = resolve_annotation(ann, env, span)?;
    Ok(Type::Function {
        params: vec![],
        ret: Box::new(ret),
    })
}

// --- Annotation and type name resolution ---

fn resolve_annotation(ann: &Annotation, env: &TypeEnv, span: Span) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => resolve_type_name(name, env, span),
        Annotation::PropertyDict(_) => {
            if let Some(type_val) = ann.get_property("type") {
                resolve_type_expr_value(type_val, env)
            } else {
                Ok(Type::Any)
            }
        }
    }
}

fn resolve_type_name(name: &str, env: &TypeEnv, span: Span) -> Result<Type, TypeError> {
    match name {
        "Int" => Ok(Type::Int),
        "Float" => Ok(Type::Float),
        "String" => Ok(Type::String),
        "Bool" => Ok(Type::Bool),
        "Number" => Ok(Type::Number),
        "Any" => Ok(Type::Any),
        _ => {
            if name.starts_with(|c: char| c.is_lowercase()) {
                Ok(Type::TypeVar(name.to_string()))
            } else {
                env.get_type_alias(name)
                    .cloned()
                    .ok_or_else(|| TypeError::undefined_type(name, span))
            }
        }
    }
}

fn resolve_type_expr_value(expr: &Spanned<Expr>, env: &TypeEnv) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Str(name) | Expr::VarRef(name) => resolve_type_name(name, env, expr.span),
        _ => Err(TypeError::new(
            format!("invalid type in annotation: {}", expr.node),
            expr.span,
        )),
    }
}

fn resolve_type_expr(expr: &Spanned<Expr>, env: &TypeEnv) -> Result<Type, TypeError> {
    match &expr.node {
        Expr::Str(name) | Expr::VarRef(name) => resolve_type_name(name, env, expr.span),
        Expr::Dict(entries) => {
            let mut fields = IndexMap::new();
            for entry in entries {
                let key = match &entry.node.key {
                    Some(k) => match &k.node {
                        Expr::Str(s) => s.clone(),
                        _ => {
                            return Err(TypeError::new(
                                "type record keys must be bare words",
                                k.span,
                            ))
                        }
                    },
                    None => {
                        return Err(TypeError::new(
                            "auto-indexed entries not supported in type expressions",
                            entry.span,
                        ))
                    }
                };
                let ty = resolve_type_expr(&entry.node.value, env)?;
                fields.insert(key, ty);
            }
            Ok(Type::Record(fields))
        }
        Expr::Annotated { name, annotation } => {
            if name == "Fn" {
                resolve_fn_type(&annotation.node, env, annotation.span)
            } else {
                resolve_annotation(&annotation.node, env, expr.span)
            }
        }
        _ => Err(TypeError::new(
            format!("invalid type expression: {}", expr.node),
            expr.span,
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(input: &str) -> Result<(), Vec<TypeError>> {
        let file = crate::parse(input).unwrap();
        typecheck_file(&file.node)
    }

    fn check_err(input: &str) -> Vec<TypeError> {
        check(input).unwrap_err()
    }

    fn infer(input: &str) -> Type {
        let file = crate::parse(input).unwrap();
        let env = Rc::new(TypeEnv::new());
        let expr = &file.node.documents[0].node.expressions[0];
        infer_expr(expr, &env).unwrap()
    }

    fn doc_env(input: &str) -> Rc<TypeEnv> {
        let file = crate::parse(input).unwrap();
        let env = Rc::new(TypeEnv::new());
        typecheck_document(&file.node.documents[0], &env).unwrap()
    }

    fn result_type(input: &str) -> Type {
        let env = doc_env(input);
        env.get("$").cloned().unwrap()
    }

    fn result_field(input: &str, field: &str) -> Type {
        match result_type(input) {
            Type::Record(fields) => fields.get(field).cloned().unwrap(),
            other => panic!("expected Record for $$, got {other}"),
        }
    }

    /// Like `doc_env` but processes all documents, returning the final env.
    fn file_env(input: &str) -> Rc<TypeEnv> {
        let file = crate::parse(input).unwrap();
        let mut env = Rc::new(TypeEnv::new());
        for doc in &file.node.documents {
            env = typecheck_document(doc, &env).unwrap();
        }
        env
    }

    // -- Literal inference --

    #[test]
    fn test_literal_int() {
        assert_eq!(infer("42"), Type::IntLiteral(42));
    }

    #[test]
    fn test_literal_float() {
        assert_eq!(infer("3.14"), Type::Float);
    }

    #[test]
    fn test_literal_bool() {
        assert_eq!(infer("true"), Type::Bool);
    }

    #[test]
    fn test_literal_string() {
        assert_eq!(infer("hello"), Type::StringLiteral("hello".into()));
    }

    // -- VarRef --

    #[test]
    fn test_varref_in_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_varref_undefined() {
        let errors = check_err("$x");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("undefined variable: $x"));
    }

    // -- Record construction --

    #[test]
    fn test_dict_simple() {
        let ty = infer("[a: 1  b: hello  c: true]");
        match ty {
            Type::Record(fields) => {
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
                assert_eq!(fields.get("b"), Some(&Type::StringLiteral("hello".into())));
                assert_eq!(fields.get("c"), Some(&Type::Bool));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_auto_indexed() {
        let ty = infer("[foo bar baz]");
        match ty {
            Type::Record(fields) => {
                assert_eq!(fields.get("0"), Some(&Type::StringLiteral("foo".into())));
                assert_eq!(fields.get("1"), Some(&Type::StringLiteral("bar".into())));
                assert_eq!(fields.get("2"), Some(&Type::StringLiteral("baz".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_nested() {
        let ty = infer("[outer: [inner: 42]]");
        match ty {
            Type::Record(fields) => {
                let inner = fields.get("outer").unwrap();
                match inner {
                    Type::Record(inner_fields) => {
                        assert_eq!(inner_fields.get("inner"), Some(&Type::IntLiteral(42)));
                    }
                    other => panic!("expected Record, got {other}"),
                }
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_ref() {
        let ty = infer("[a: $b  b: 42]");
        match ty {
            Type::Record(fields) => {
                assert_eq!(fields.get("a"), Some(&Type::Any));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // -- Dict error accumulation --

    #[test]
    fn test_dict_multiple_errors() {
        // Multiple entries reference undefined variables outside the dict scope.
        // With error accumulation, the type checker continues past the first error
        // and reports the first while still checking the rest.
        let errors = check_err("[a: $undefined1  b: 42  c: $undefined2]");
        // infer_dict now processes all entries and returns the first error.
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("undefined variable"));

        // Verify that the first error reported is about the first undefined var.
        let file = crate::parse("[a: $undefined1  b: 42  c: $undefined2]").unwrap();
        let env = Rc::new(TypeEnv::new());
        let expr = &file.node.documents[0].node.expressions[0];
        let err = infer_expr(expr, &env).unwrap_err();
        assert!(
            err.message.contains("$undefined1"),
            "first error should be about $undefined1, got: {}",
            err.message
        );
    }

    // -- Dot access --

    #[test]
    fn test_dot_access_found() {
        assert_eq!(
            result_field(
                "[person: [name: Andrew  age: 30]]\n[result: $person.name]",
                "result"
            ),
            Type::StringLiteral("Andrew".into()),
        );
    }

    #[test]
    fn test_dot_access_missing_field() {
        let errors = check_err("[person: [name: Andrew]]\n[result: $person.age]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("field 'age' not found")));
    }

    #[test]
    fn test_dot_access_non_record() {
        let errors = check_err("[x: 42]\n[result: $x.field]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected record type")));
    }

    // -- Bracket access --

    #[test]
    fn test_bracket_access_string_key() {
        assert_eq!(
            result_field("[data: [name: hello]]\n[result: $data[name]]", "result"),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_bracket_access_int_key() {
        assert_eq!(
            result_field("[list: [a b c]]\n[result: $list[0]]", "result"),
            Type::StringLiteral("a".into()),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_literal() {
        assert_eq!(
            result_field("[data: [x: 1]  key: x]\n[result: $data[$key]]", "result"),
            Type::IntLiteral(1),
        );
    }

    #[test]
    fn test_bracket_access_dynamic_key_non_literal() {
        // $key has type Any (forward ref resolves to Any), so result is Any
        assert_eq!(
            result_field("[result: $data[$key]  data: [x: 1]  key: x]", "result"),
            Type::Any,
        );
    }

    // -- Range access --

    #[test]
    fn test_range_access_record() {
        let ty = result_field(
            "[data: [a: 1  b: 2  c: 3]]\n[result: $data[0..2]]",
            "result",
        );
        assert!(matches!(ty, Type::Record(_)));
    }

    #[test]
    fn test_range_access_invalid_bound() {
        let errors = check_err("[flag: true  data: [a: 1  b: 2]]\n[result: $data[$flag..2]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("range bound must be Int or String")));
    }

    // -- TypeAssert --

    #[test]
    fn test_type_assert_pass() {
        let ty = infer("[@Number 42]");
        assert_eq!(ty, Type::Number);
    }

    #[test]
    fn test_type_assert_fail() {
        let errors = check_err("[@Number hello]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_type_assert_int_not_string() {
        let errors = check_err("[@String 42]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    // -- TypeAlias --

    #[test]
    fn test_type_alias_record() {
        // Use a scope chain so the alias is registered and then referenced via @Person
        let ty = result_field(
            "[Person: [type [name: String  age: Number]]]\n[p: [@Person [name: Alice  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(fields) => {
                assert_eq!(fields.get("name"), Some(&Type::String));
                assert_eq!(fields.get("age"), Some(&Type::Number));
            }
            other => panic!("expected Record type from Person alias, got {other}"),
        }
    }

    // -- Function inference --

    #[test]
    fn test_fn_unannotated() {
        let ty = infer("[fn [x] 42]");
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Any]);
                assert_eq!(*ret, Type::IntLiteral(42));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_annotated_params() {
        let ty = infer("[fn [x@Number] $x]");
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_match() {
        let ty = infer("[fn@Number [x@Number] $x]");
        match ty {
            Type::Function { ret, .. } => assert_eq!(*ret, Type::Number),
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_return_annotation_mismatch() {
        let errors = check_err("[fn@String [x@Number] $x]");
        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("type mismatch"));
    }

    // -- Call --

    #[test]
    fn test_call_returns_function_ret_type() {
        assert_eq!(
            result_field("[f: [fn@Number [] 42]]\n[result: [call $f]]", "result"),
            Type::Number,
        );
    }

    #[test]
    fn test_call_non_function() {
        let errors = check_err("[x: 42]\n[result: [call $x]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("expected function type")));
    }

    // -- Document scope chain --

    #[test]
    fn test_scope_chain() {
        assert_eq!(result_field("[x: 42]\n[y: $x]", "y"), Type::IntLiteral(42));
    }

    #[test]
    fn test_intermediate_non_dict_error() {
        let errors = check_err("42\n[x: 1]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("expected record type"));
    }

    // -- $$ pipeline --

    #[test]
    fn test_pipeline_dollar_dollar() {
        let env = file_env("[x: 42]\n---\n[y: $$]");
        let result = env.get("$").cloned().unwrap();
        match result {
            Type::Record(fields) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert!(
                    matches!(y, Type::Record(_)),
                    "expected $$ to be Record, got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    #[test]
    fn test_pipeline_dollar_dollar_type() {
        let env = file_env("[x: 1]\n---\n[y: $$.x]");
        let result = env.get("$").cloned().unwrap();
        match result {
            Type::Record(fields) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert_eq!(
                    *y,
                    Type::IntLiteral(1),
                    "expected $$.x to propagate IntLiteral(1), got {y}"
                );
            }
            other => panic!("expected Record result, got {other}"),
        }
    }

    // -- Annotation resolution --

    #[test]
    fn test_annotation_simple() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        assert_eq!(
            resolve_annotation(&Annotation::Simple("Int".into()), &env, span).unwrap(),
            Type::Int,
        );
    }

    #[test]
    fn test_annotation_type_var() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 5);
        assert_eq!(
            resolve_annotation(&Annotation::Simple("a".into()), &env, span).unwrap(),
            Type::TypeVar("a".into()),
        );
    }

    #[test]
    fn test_annotation_property_dict_with_type() {
        let ty = infer("[fn [x@[type: Number  default: 0]] $x]");
        match ty {
            Type::Function { params, .. } => assert_eq!(params, vec![Type::Number]),
            other => panic!("expected Function, got {other}"),
        }
    }

    // -- Type alias in scope --

    #[test]
    fn test_type_alias_in_scope_chain() {
        let ty = result_field(
            "[Coord: [type [x: Number  y: Number]]]\n[p: [@Coord [x: 1  y: 2]]]",
            "p",
        );
        match ty {
            Type::Record(fields) => {
                assert_eq!(fields.get("x"), Some(&Type::Number));
                assert_eq!(fields.get("y"), Some(&Type::Number));
                assert_eq!(fields.len(), 2);
            }
            other => panic!("expected Record type from Coord alias, got {other}"),
        }
    }

    // -- Error branch coverage --

    #[test]
    fn test_type_expr_non_bare_word_key() {
        // resolve_type_expr: "type record keys must be bare words"
        // $var key in a type expression is VarRef, not Str.
        // Must be a standalone type expr (not inside a dict, which silently
        // swallows resolve_type_expr errors in alias registration).
        let errors = check_err("[type [$var: Int]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("type record keys must be bare words")));
    }

    #[test]
    fn test_type_expr_auto_indexed_entries() {
        // resolve_type_expr: "auto-indexed entries not supported in type expressions"
        // Standalone type expr to avoid silent error swallowing in dict alias pass.
        let errors = check_err("[type [Int String]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("auto-indexed entries not supported")));
    }

    #[test]
    fn test_annotation_type_value_invalid_expr() {
        // resolve_type_expr_value: invalid type in annotation (non-Str/VarRef)
        let errors = check_err("[fn [x@[type: 42]] $x]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("invalid type in annotation")));
    }

    #[test]
    fn test_bracket_access_bool_key() {
        // check_bracket_access: "bracket access key must be String or Int"
        let errors = check_err("[data: [x: 1]  flag: true]\n[result: $data[$flag]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("bracket access key must be String or Int")));
    }

    #[test]
    fn test_annotated_non_fn_resolves_annotation() {
        // resolve_annotated: non-"Fn" path falls through to resolve_annotation
        let ty = infer("Config@Number");
        assert_eq!(ty, Type::Number);
    }
}
