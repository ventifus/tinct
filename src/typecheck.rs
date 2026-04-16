//! Type checker: infers types from AST expressions, resolves type aliases,
//! validates type assertions, and performs Hindley-Milner style type variable
//! unification for polymorphic function calls.

use std::cell::Cell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::*;
use crate::types::*;

// --- Public API ---

pub fn typecheck_file(file: &File) -> Result<(), Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::new());
    let counter = Cell::new(0u32);

    for doc in &file.documents {
        match typecheck_document(doc, &env, &counter) {
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
    counter: &Cell<u32>,
) -> Result<Rc<TypeEnv>, Vec<TypeError>> {
    let mut errors = Vec::new();
    let mut env = Rc::new(TypeEnv::with_parent(Rc::clone(parent_env)));
    let mut result_type = Type::Record(IndexMap::new(), RowRest::Closed);

    let exprs = &doc.node.expressions;
    if exprs.is_empty() {
        let mut result_env = TypeEnv::with_parent(Rc::clone(&env));
        result_env.insert(
            "$".to_string(),
            Type::Record(IndexMap::new(), RowRest::Closed),
        );
        return Ok(Rc::new(result_env));
    }

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;
        match infer_expr(expr, &env, counter) {
            Ok(ty) => {
                if is_last {
                    result_type = ty;
                } else {
                    match &ty {
                        Type::Record(fields, _) => {
                            let mut new_env = TypeEnv::with_parent(Rc::clone(&env));
                            for (name, field_ty) in fields {
                                new_env.insert(name.clone(), field_ty.clone());
                            }
                            let mut alias_errs =
                                register_type_aliases(expr, &mut new_env, &env);
                            errors.append(&mut alias_errs);
                            env = Rc::new(new_env);
                        }
                        Type::Any => {}
                        _ => errors.push(TypeError::not_a_record(&ty, expr.span)),
                    }
                }
            }
            Err(mut errs) => errors.append(&mut errs),
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
) -> Vec<TypeError> {
    let mut errors = Vec::new();
    if let Expr::Dict(entries) = &expr.node {
        for entry in entries {
            if let Some(ref key) = entry.node.key {
                if let Expr::Str(name) = &key.node {
                    if let Expr::TypeAlias(inner) = &entry.node.value.node {
                        match resolve_type_expr(inner, resolve_env) {
                            Ok(alias_ty) => {
                                target_env.insert_type_alias(name.clone(), alias_ty);
                            }
                            Err(e) => errors.push(e),
                        }
                    }
                }
            }
        }
    }
    errors
}

// --- Expression type inference ---

fn infer_expr(
    expr: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    match &expr.node {
        Expr::Int(n) => Ok(Type::IntLiteral(*n)),
        Expr::Float(_) => Ok(Type::Float),
        Expr::Bool(_) => Ok(Type::Bool),
        Expr::Str(s) => Ok(Type::StringLiteral(s.clone())),

        Expr::VarRef(name) => env
            .get(name)
            .cloned()
            .ok_or_else(|| vec![TypeError::undefined_variable(name, expr.span)]),

        Expr::Dict(entries) => infer_dict(entries, env, counter),

        Expr::DotAccess {
            expr: target,
            field,
        } => check_dot_access(target, field, env, expr.span, counter),

        Expr::BracketAccess { expr: target, key } => {
            check_bracket_access(target, key, env, expr.span, counter)
        }

        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => check_range_access(target, start, end, env, expr.span, counter),

        Expr::Call {
            func,
            args,
            named_args,
        } => check_call(func, args, named_args, env, expr.span, counter),

        Expr::Fn {
            return_ann,
            params,
            body,
        } => infer_fn(return_ann, params, body, env, expr.span, counter),

        Expr::TypeAlias(inner) => expand_type_alias(inner, env).map_err(|e| vec![e]),

        Expr::TypeAssert {
            annotation,
            expr: inner,
        } => resolve_type_assert(annotation, inner, env, expr.span, counter),

        Expr::Annotated { name, annotation } => {
            resolve_annotated(name, annotation, env, expr.span).map_err(|e| vec![e])
        }

        Expr::Rest(_) => Err(vec![TypeError::new(
            "rest marker (...) is only valid inside type expressions",
            expr.span,
        )]),
    }
}

// --- Record type construction ---

fn infer_dict(
    entries: &[Spanned<Entry>],
    env: &Rc<TypeEnv>,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let mut dict_env = TypeEnv::with_parent(Rc::clone(env));
    let mut key_entries: Vec<(Option<String>, bool)> = Vec::new();
    let mut auto_index: i64 = 0;

    for entry in entries {
        let key_name = entry_key_name(&entry.node, &mut auto_index, env, counter);
        let is_alias = matches!(&entry.node.value.node, Expr::TypeAlias(_));
        if let Some(ref name) = key_name {
            dict_env.insert(name.clone(), Type::Any);
        }
        key_entries.push((key_name, is_alias));
    }

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

    let mut fields = IndexMap::new();
    let mut errors = Vec::new();
    for ((key_name, is_alias), entry) in key_entries.iter().zip(entries.iter()) {
        if *is_alias || matches!(&entry.node.value.node, Expr::Rest(_)) {
            continue;
        }
        match infer_expr(&entry.node.value, &dict_env, counter) {
            Ok(value_ty) => {
                if let Some(name) = key_name {
                    fields.insert(name.clone(), value_ty);
                }
            }
            Err(mut errs) => {
                errors.append(&mut errs);
                if let Some(name) = key_name {
                    fields.insert(name.clone(), Type::Any);
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(Type::Record(fields, RowRest::Closed))
    } else {
        Err(errors)
    }
}

fn entry_key_name(
    entry: &Entry,
    auto_index: &mut i64,
    env: &Rc<TypeEnv>,
    counter: &Cell<u32>,
) -> Option<String> {
    match &entry.key {
        Some(key_expr) => match &key_expr.node {
            Expr::Str(s) => Some(s.clone()),
            Expr::Int(n) => Some(n.to_string()),
            _ => match infer_expr(key_expr, env, counter) {
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
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, counter)?;
    match &target_ty {
        Type::Record(fields, rest) => match fields.get(field) {
            Some(ty) => Ok(ty.clone()),
            None if matches!(rest, RowRest::Open | RowRest::RowVar(_)) => Ok(Type::Any),
            None => Err(vec![TypeError::field_not_found(field, &target_ty, span)]),
        },
        Type::Any => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

fn check_bracket_access(
    target: &Spanned<Expr>,
    key: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, counter)?;
    let key_ty = infer_expr(key, env, counter)?;

    match &target_ty {
        Type::Record(fields, rest) => {
            let is_open = matches!(rest, RowRest::Open | RowRest::RowVar(_));
            let lookup = |field_name: &str| -> Result<Type, Vec<TypeError>> {
                match fields.get(field_name) {
                    Some(ty) => Ok(ty.clone()),
                    None if is_open => Ok(Type::Any),
                    None => Err(vec![TypeError::field_not_found(
                        field_name, &target_ty, span,
                    )]),
                }
            };
            match &key.node {
                Expr::Str(s) => lookup(s),
                Expr::Int(n) => lookup(&n.to_string()),
                _ => match &key_ty {
                    Type::StringLiteral(s) => lookup(s.as_str()),
                    Type::IntLiteral(n) => lookup(&n.to_string()),
                    Type::String | Type::Int | Type::Any => Ok(Type::Any),
                    _ => Err(vec![TypeError::new(
                        format!("bracket access key must be String or Int, got {key_ty}"),
                        span,
                    )]),
                },
            }
        }
        Type::Any => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

fn check_range_access(
    target: &Spanned<Expr>,
    start: &Option<Box<Spanned<Expr>>>,
    end: &Option<Box<Spanned<Expr>>>,
    env: &Rc<TypeEnv>,
    span: Span,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let target_ty = infer_expr(target, env, counter)?;

    for bound in [start, end].into_iter().flatten() {
        let bound_ty = infer_expr(bound, env, counter)?;
        if !matches!(
            bound_ty,
            Type::Int | Type::IntLiteral(_) | Type::String | Type::StringLiteral(_) | Type::Any
        ) {
            return Err(vec![TypeError::new(
                format!("range bound must be Int or String, got {bound_ty}"),
                bound.span,
            )]);
        }
    }

    match &target_ty {
        Type::Record(..) | Type::Any => Ok(target_ty),
        _ => Err(vec![TypeError::not_a_record(&target_ty, span)]),
    }
}

// --- Call type checking ---

fn check_call(
    func: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<TypeEnv>,
    span: Span,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let func_ty = infer_expr(func, env, counter)?;

    let arg_types: Vec<Type> = args
        .iter()
        .map(|a| infer_expr(a, env, counter))
        .collect::<Result<_, _>>()?;
    for na in named_args {
        let _ = infer_expr(&na.node.value, env, counter)?;
    }

    match &func_ty {
        Type::Function { params, ret } => {
            if !func_ty.has_type_vars() {
                return Ok(*ret.clone());
            }

            if params.len() != arg_types.len() {
                return Err(vec![TypeError::new(
                    format!(
                        "arity mismatch: expected {} arguments, got {}",
                        params.len(),
                        arg_types.len()
                    ),
                    span,
                )]);
            }

            let mut cnt = counter.get();
            let (inst_ty, _) = instantiate(&func_ty, &mut cnt);
            counter.set(cnt);

            let (inst_params, inst_ret) = match &inst_ty {
                Type::Function { params, ret } => (params, ret),
                _ => unreachable!(),
            };

            if !params.is_empty() {
                let mut subst = Substitution::new();
                for (param_ty, arg_ty) in inst_params.iter().zip(arg_types.iter()) {
                    unify(param_ty, arg_ty, &mut subst, span).map_err(|e| vec![e])?;
                }
                Ok(subst.apply(inst_ret))
            } else {
                Ok(*ret.clone())
            }
        }
        Type::Any => Ok(Type::Any),
        _ => Err(vec![TypeError::not_a_function(&func_ty, span)]),
    }
}

// --- Function type inference ---

fn infer_fn(
    return_ann: &Option<Spanned<Annotation>>,
    params: &[Spanned<Param>],
    body: &Spanned<Expr>,
    env: &Rc<TypeEnv>,
    span: Span,
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let param_types: Vec<Type> = params
        .iter()
        .map(|p| match &p.node.annotation {
            Some(ann) => resolve_annotation(&ann.node, env, ann.span),
            None => Ok(Type::Any),
        })
        .collect::<Result<_, _>>()
        .map_err(|e| vec![e])?;

    let mut fn_env = TypeEnv::with_parent(Rc::clone(env));
    for (param, ty) in params.iter().zip(param_types.iter()) {
        if param.node.variadic {
            fn_env.insert(
                param.node.name.clone(),
                Type::Record(IndexMap::new(), RowRest::Closed),
            );
        } else {
            fn_env.insert(param.node.name.clone(), ty.clone());
        }
    }
    let fn_env = Rc::new(fn_env);

    let ret_type = match return_ann {
        Some(ann) => {
            let declared =
                resolve_annotation(&ann.node, env, ann.span).map_err(|e| vec![e])?;
            let inferred = infer_expr(body, &fn_env, counter)?;
            if !Type::is_subtype(&inferred, &declared) {
                return Err(vec![TypeError::type_mismatch(&declared, &inferred, span)]);
            }
            declared
        }
        None => infer_expr(body, &fn_env, counter)?,
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
    counter: &Cell<u32>,
) -> Result<Type, Vec<TypeError>> {
    let expected =
        resolve_annotation(&annotation.node, env, annotation.span).map_err(|e| vec![e])?;
    let actual = infer_expr(inner, env, counter)?;

    if !Type::is_subtype(&actual, &expected) {
        return Err(vec![TypeError::type_mismatch(&expected, &actual, span)]);
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
    let ret = resolve_annotation_as_type(ann, env, span)?;
    Ok(Type::Function {
        params: vec![],
        ret: Box::new(ret),
    })
}

/// Resolve an annotation in a context where a type expression is expected.
/// Unlike `resolve_annotation`, a PropertyDict is interpreted as a type expression
/// (record type or function type) rather than a property bag.
fn resolve_annotation_as_type(
    ann: &Annotation,
    env: &TypeEnv,
    span: Span,
) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => resolve_type_name(name, env, span),
        Annotation::PropertyDict(entries) => resolve_type_dict(entries, env, span),
    }
}

// --- Annotation and type name resolution ---

fn resolve_annotation(ann: &Annotation, env: &TypeEnv, span: Span) -> Result<Type, TypeError> {
    match ann {
        Annotation::Simple(name) => resolve_type_name(name, env, span),
        Annotation::PropertyDict(entries) => {
            if let Some(type_val) = ann.get_property("type") {
                resolve_type_expr_value(type_val, env)
            } else {
                resolve_property_dict_as_record(entries, env, span)
            }
        }
    }
}

fn resolve_property_dict_as_record(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
) -> Result<Type, TypeError> {
    resolve_type_dict(entries, env, span).or(Ok(Type::Any))
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
        Expr::Dict(entries) => resolve_type_dict(entries, env, expr.span),
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

fn resolve_type_dict(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
) -> Result<Type, TypeError> {
    if let Some(fn_type) = try_resolve_fn_type_expr(entries, env, span)? {
        return Ok(fn_type);
    }

    let mut fields = IndexMap::new();
    let mut rest = RowRest::Closed;
    for entry in entries {
        if let Expr::Rest(name) = &entry.node.value.node {
            rest = match name {
                None => RowRest::Open,
                Some(n) => RowRest::RowVar(n.clone()),
            };
            continue;
        }
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
    Ok(Type::Record(fields, rest))
}

/// Detect `[Fn@Return [ParamTypes]]` -- a Dict with two auto-indexed entries
/// where the first is `Annotated { name: "Fn", ... }` and the second is a Dict
/// containing the parameter type list.
fn try_resolve_fn_type_expr(
    entries: &[Spanned<Entry>],
    env: &TypeEnv,
    span: Span,
) -> Result<Option<Type>, TypeError> {
    let first = match entries.first() {
        Some(e) if e.node.key.is_none() => e,
        _ => return Ok(None),
    };

    let (ann_node, ann_span) = match &first.node.value.node {
        Expr::Annotated { name, annotation } if name == "Fn" => (&annotation.node, annotation.span),
        _ => return Ok(None),
    };

    if entries.len() != 2 {
        return Err(TypeError::new(
            format!(
                "function type [Fn@Return [Params]] requires exactly 2 entries, got {}",
                entries.len()
            ),
            span,
        ));
    }

    let second = &entries[1];
    if second.node.key.is_some() {
        return Err(TypeError::new(
            "function type parameter list must be auto-indexed",
            second.span,
        ));
    }

    let ret = resolve_annotation_as_type(ann_node, env, ann_span)?;

    let param_entries = match &second.node.value.node {
        Expr::Dict(entries) => entries,
        _ => {
            return Err(TypeError::new(
                "function type parameter list must be a bracket expression",
                second.node.value.span,
            ))
        }
    };

    let mut params = Vec::new();
    for entry in param_entries {
        if entry.node.key.is_some() {
            return Err(TypeError::new(
                "function type parameters must be auto-indexed type names",
                entry.span,
            ));
        }
        params.push(resolve_type_expr(&entry.node.value, env)?);
    }

    Ok(Some(Type::Function {
        params,
        ret: Box::new(ret),
    }))
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
        let counter = Cell::new(0u32);
        let expr = &file.node.documents[0].node.expressions[0];
        infer_expr(expr, &env, &counter).unwrap()
    }

    fn doc_env(input: &str) -> Rc<TypeEnv> {
        let file = crate::parse(input).unwrap();
        let env = Rc::new(TypeEnv::new());
        let counter = Cell::new(0u32);
        typecheck_document(&file.node.documents[0], &env, &counter).unwrap()
    }

    fn result_type(input: &str) -> Type {
        let env = doc_env(input);
        env.get("$").cloned().unwrap()
    }

    fn result_field(input: &str, field: &str) -> Type {
        match result_type(input) {
            Type::Record(fields, _) => fields.get(field).cloned().unwrap(),
            other => panic!("expected Record for $$, got {other}"),
        }
    }

    fn file_env(input: &str) -> Rc<TypeEnv> {
        let file = crate::parse(input).unwrap();
        let mut env = Rc::new(TypeEnv::new());
        let counter = Cell::new(0u32);
        for doc in &file.node.documents {
            env = typecheck_document(doc, &env, &counter).unwrap();
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
            Type::Record(fields, _) => {
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
            Type::Record(fields, _) => {
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
            Type::Record(fields, _) => {
                let inner = fields.get("outer").unwrap();
                match inner {
                    Type::Record(inner_fields, _) => {
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
            Type::Record(fields, _) => {
                assert_eq!(fields.get("a"), Some(&Type::Any));
                assert_eq!(fields.get("b"), Some(&Type::IntLiteral(42)));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    // -- Dict error accumulation --

    #[test]
    fn test_dict_multiple_errors() {
        let errors = check_err("[a: $undefined1  b: 42  c: $undefined2]");
        assert_eq!(errors.len(), 2, "should return all errors, got: {errors:?}");
        assert!(
            errors[0].message.contains("$undefined1"),
            "first error should be about $undefined1, got: {}",
            errors[0].message
        );
        assert!(
            errors[1].message.contains("$undefined2"),
            "second error should be about $undefined2, got: {}",
            errors[1].message
        );

        // Also verify via direct infer_expr call
        let file = crate::parse("[a: $undefined1  b: 42  c: $undefined2]").unwrap();
        let env = Rc::new(TypeEnv::new());
        let counter = Cell::new(0u32);
        let expr = &file.node.documents[0].node.expressions[0];
        let errs = infer_expr(expr, &env, &counter).unwrap_err();
        assert_eq!(errs.len(), 2, "infer_expr should return all dict errors");
        assert!(errs[0].message.contains("$undefined1"));
        assert!(errs[1].message.contains("$undefined2"));
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
        assert!(matches!(ty, Type::Record(..)));
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
        let ty = result_field(
            "[Person: [type [name: String  age: Number]]]\n[p: [@Person [name: Alice  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(fields, _) => {
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
            Type::Record(fields, _) => {
                let y = fields.get("y").expect("field 'y' should exist");
                assert!(
                    matches!(y, Type::Record(..)),
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
            Type::Record(fields, _) => {
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

    // -- resolve_property_dict_as_record fallback paths --

    #[test]
    fn test_property_dict_non_str_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Int(42))),
                value: sp(Expr::Str("Int".into())),
            },
            span,
        )]);
        assert_eq!(resolve_annotation(&ann, &env, span).unwrap(), Type::Any);
    }

    #[test]
    fn test_property_dict_no_key_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: None,
                value: sp(Expr::Str("Int".into())),
            },
            span,
        )]);
        assert_eq!(resolve_annotation(&ann, &env, span).unwrap(), Type::Any);
    }

    #[test]
    fn test_property_dict_unresolvable_type_falls_back_to_any() {
        let env = Rc::new(TypeEnv::new());
        let span = crate::test_util::test_span(1, 1, 1, 10);
        let sp = |node: Expr| Spanned::new(node, span);
        let ann = Annotation::PropertyDict(vec![Spanned::new(
            Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Str("NoSuchType".into())),
            },
            span,
        )]);
        assert_eq!(resolve_annotation(&ann, &env, span).unwrap(), Type::Any);
    }

    // -- Type alias in scope --

    #[test]
    fn test_type_alias_in_scope_chain() {
        let ty = result_field(
            "[Coord: [type [x: Number  y: Number]]]\n[p: [@Coord [x: 1  y: 2]]]",
            "p",
        );
        match ty {
            Type::Record(fields, _) => {
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
        let errors = check_err("[type [$var: Int]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("type record keys must be bare words")));
    }

    #[test]
    fn test_type_expr_auto_indexed_entries() {
        let errors = check_err("[type [Int String]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("auto-indexed entries not supported")));
    }

    #[test]
    fn test_annotation_type_value_invalid_expr() {
        let errors = check_err("[fn [x@[type: 42]] $x]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("invalid type in annotation")));
    }

    #[test]
    fn test_bracket_access_bool_key() {
        let errors = check_err("[data: [x: 1]  flag: true]\n[result: $data[$flag]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("bracket access key must be String or Int")));
    }

    #[test]
    fn test_annotated_non_fn_resolves_annotation() {
        let ty = infer("Config@Number");
        assert_eq!(ty, Type::Number);
    }

    // -- Fn@Return [Params] type expression --

    #[test]
    fn test_fn_type_one_param() {
        let ty = result_field(
            "[Mapper: [type [Fn@b [a]]]]\n[x: [@Mapper [fn [v] $v]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into())]);
                assert_eq!(*ret, Type::TypeVar("b".into()));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_two_params() {
        let ty = result_field(
            "[BinOp: [type [Fn@c [a b]]]]\n[x: [@BinOp [fn [p q] $p]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(
                    params,
                    vec![Type::TypeVar("a".into()), Type::TypeVar("b".into())]
                );
                assert_eq!(*ret, Type::TypeVar("c".into()));
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_types() {
        let ty = result_field(
            "[Add: [type [Fn@Number [Number Number]]]]\n[x: [@Add [fn [a@Number b@Number] $a]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number, Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_concrete_return_typevar_param() {
        let ty = result_field(
            "[Pred: [type [Fn@Bool [a]]]]\n[x: [@Pred [fn [v] true]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into())]);
                assert_eq!(*ret, Type::Bool);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_higher_order() {
        let ty = result_field(
            "[HO: [type [Fn@[Fn@c [b]] [a]]]]\n[x: [@HO [fn [v] [fn [w] $w]]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::TypeVar("a".into())]);
                match *ret {
                    Type::Function {
                        params: inner_params,
                        ret: inner_ret,
                    } => {
                        assert_eq!(inner_params, vec![Type::TypeVar("b".into())]);
                        assert_eq!(*inner_ret, Type::TypeVar("c".into()));
                    }
                    other => panic!("expected inner Function, got {other}"),
                }
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_missing_param_list() {
        let errors = check_err("[type [Fn@b]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_extra_entries() {
        let errors = check_err("[type [Fn@b [a] extra]]");
        assert!(errors
            .iter()
            .any(|e| e.message.contains("requires exactly 2 entries")));
    }

    #[test]
    fn test_fn_type_param_list_not_bracket() {
        let errors = check_err("[type [Fn@b a]]");
        assert!(errors.iter().any(|e| e
            .message
            .contains("parameter list must be a bracket expression")));
    }

    #[test]
    fn test_fn_type_standalone_fn_annotation() {
        let ty = infer("Fn@Number");
        match ty {
            Type::Function { params, ret } => {
                assert!(params.is_empty());
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_in_type_assert() {
        let ty = result_field(
            "[F: [type [Fn@Number [Number]]]]\n[x: [@F [fn [n@Number] $n]]]",
            "x",
        );
        match ty {
            Type::Function { params, ret } => {
                assert_eq!(params, vec![Type::Number]);
                assert_eq!(*ret, Type::Number);
            }
            other => panic!("expected Function, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_display_round_trip() {
        let ty = Type::Function {
            params: vec![Type::TypeVar("a".into()), Type::TypeVar("b".into())],
            ret: Box::new(Type::TypeVar("c".into())),
        };
        assert_eq!(format!("{ty}"), "Fn@c [a b]");
    }

    // -- Polymorphic call unification --

    #[test]
    fn test_call_polymorphic_identity() {
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id 42]]", "result"),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_identity_string() {
        assert_eq!(
            result_field("[id: [fn [x@a] $x]]\n[result: [call $id hello]]", "result"),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_two_type_vars() {
        assert_eq!(
            result_field(
                "[f: [fn [x@a y@b] $y]]\n[result: [call $f 42 hello]]",
                "result"
            ),
            Type::StringLiteral("hello".into()),
        );
    }

    #[test]
    fn test_call_polymorphic_type_var_in_return_only() {
        assert_eq!(
            result_field(
                "[first: [fn [x@a y@b] $x]]\n[result: [call $first 42 hello]]",
                "result"
            ),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_multiple_calls_different_types() {
        let ty = result_type("[id: [fn [x@a] $x]]\n[r1: [call $id 42]  r2: [call $id hello]]");
        match ty {
            Type::Record(fields, _) => {
                assert_eq!(fields.get("r1"), Some(&Type::IntLiteral(42)));
                assert_eq!(fields.get("r2"), Some(&Type::StringLiteral("hello".into())));
            }
            other => panic!("expected Record, got {other}"),
        }
    }

    #[test]
    fn test_call_monomorphic_no_unification() {
        assert_eq!(
            result_field(
                "[f: [fn@Number [x@Number] $x]]\n[result: [call $f 42]]",
                "result"
            ),
            Type::Number,
        );
    }

    #[test]
    fn test_call_polymorphic_arity_mismatch_error() {
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42]]");
        assert!(
            errors.iter().any(|e| e.message.contains("arity mismatch")),
            "expected arity mismatch error, got: {:?}",
            errors
        );
    }

    #[test]
    fn test_call_unification_error() {
        let errors = check_err("[f: [fn [x@a y@a] $x]]\n[result: [call $f 42 hello]]");
        assert!(
            errors.iter().any(|e| e.message.contains("type mismatch")),
            "expected type mismatch error, got: {:?}",
            errors
        );
    }

    // -- Polymorphic call with named args --

    #[test]
    fn test_call_polymorphic_with_named_arg() {
        // Polymorphic function called with positional args and a named arg override.
        // Named args are type-checked but don't participate in type var unification.
        assert_eq!(
            result_field(
                "[f: [fn [x@a y@b] $x]]\n[result: [call $f 42 hello y: 77]]",
                "result"
            ),
            Type::IntLiteral(42),
        );
    }

    #[test]
    fn test_call_polymorphic_named_arg_bad_value_errors() {
        // A named arg whose value references an undefined variable should produce
        // a type error even in a polymorphic call context.
        let errors = check_err("[f: [fn [x@a y@b] $x]]\n[result: [call $f 42 hello y: $missing]]");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("undefined variable")),
            "expected undefined variable error from named arg, got: {:?}",
            errors
        );
    }

    // -- Function type expression with param list --

    #[test]
    fn test_fn_type_expr_with_params() {
        let env = doc_env("[Identity: [type [Fn@a [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Identity");
        assert!(alias.is_some(), "Identity alias should be registered");
        match alias.unwrap() {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::TypeVar("a".into())]);
                assert_eq!(**ret, Type::TypeVar("a".into()));
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_multi_params() {
        let env = doc_env("[Mapper: [type [Fn@b [a b]]]]\n[x: 1]");
        let alias = env.get_type_alias("Mapper").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(
                    params,
                    &vec![Type::TypeVar("a".into()), Type::TypeVar("b".into())]
                );
                assert_eq!(**ret, Type::TypeVar("b".into()));
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_concrete_params() {
        let env = doc_env("[Add: [type [Fn@Number [Number Number]]]]\n[x: 1]");
        let alias = env.get_type_alias("Add").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::Number, Type::Number]);
                assert_eq!(**ret, Type::Number);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    #[test]
    fn test_fn_type_expr_predicate() {
        let env = doc_env("[Pred: [type [Fn@Bool [a]]]]\n[x: 1]");
        let alias = env.get_type_alias("Pred").unwrap();
        match alias {
            Type::Function { params, ret } => {
                assert_eq!(params, &vec![Type::TypeVar("a".into())]);
                assert_eq!(**ret, Type::Bool);
            }
            other => panic!("expected Function type alias, got {other}"),
        }
    }

    // -- Row polymorphism --

    #[test]
    fn test_type_expr_open_record() {
        let ty = result_field(
            "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  age: 30]]]",
            "p",
        );
        match ty {
            Type::Record(fields, RowRest::Open) => {
                assert_eq!(fields.get("name"), Some(&Type::String));
            }
            other => panic!("expected open Record, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_row_var_record() {
        let ty = result_field(
            "[WithName: [type [name: String ...rest]]]\n[p: [@WithName [name: Alice]]]",
            "p",
        );
        match ty {
            Type::Record(fields, RowRest::RowVar(name)) => {
                assert_eq!(fields.get("name"), Some(&Type::String));
                assert_eq!(name, "rest");
            }
            other => panic!("expected record with row var, got {other}"),
        }
    }

    #[test]
    fn test_type_expr_closed_record() {
        let ty = result_field(
            "[Closed: [type [name: String]]]\n[p: [@Closed [name: Alice]]]",
            "p",
        );
        match ty {
            Type::Record(_, RowRest::Closed) => {}
            other => panic!("expected closed Record, got {other}"),
        }
    }

    #[test]
    fn test_type_assert_open_record_accepts_extra_fields() {
        check("[@[name: String ...] [name: Alice  age: 30]]").unwrap();
    }

    #[test]
    fn test_type_assert_closed_record_rejects_extra_fields() {
        let errors = check_err("[@[name: String] [name: Alice  age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_type_assert_open_record_requires_fields() {
        let errors = check_err("[@[name: String ...] [age: 30]]");
        assert!(!errors.is_empty());
        assert!(errors[0].message.contains("type mismatch"));
    }

    #[test]
    fn test_dot_access_on_open_record_known_field() {
        assert_eq!(
            result_field(
                "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice  age: 30]]]\n[result: $p.name]",
                "result",
            ),
            Type::String,
        );
    }

    #[test]
    fn test_dot_access_on_open_record_unknown_field() {
        assert_eq!(
            result_field(
                "[Open: [type [name: String ...]]]\n[p: [@Open [name: Alice]]]\n[result: $p.unknown]",
                "result",
            ),
            Type::Any,
        );
    }

    #[test]
    fn test_data_dict_always_closed() {
        let ty = infer("[a: 1  b: 2]");
        match ty {
            Type::Record(_, RowRest::Closed) => {}
            other => panic!("expected closed Record for data dict, got {other}"),
        }
    }

    #[test]
    fn test_rest_in_data_dict_ignored() {
        let ty = infer("[a: 1 ...]");
        match ty {
            Type::Record(fields, RowRest::Closed) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields.get("a"), Some(&Type::IntLiteral(1)));
            }
            other => panic!("expected closed Record, got {other}"),
        }
    }
}
