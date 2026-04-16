// Core evaluation module

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Expr, Span, Spanned};
use crate::error::EvalError;
use crate::value::{Environment, Key, Thunk, ThunkState, Value};

const MAX_EVAL_DEPTH: usize = 256;
const RANGE_KEY_TYPE_ERROR: &str = "range access requires comparable key types";

// --- Evaluation ---

/// Wrap an AST expression in a thunk. Literals produce immediately materialized
/// thunks; dicts produce materialized thunks whose values are unevaluated;
/// var refs look up the environment chain.
///
/// `depth` tracks recursion depth to prevent stack overflow. Callers should
/// pass 0 for top-level evaluation.
pub fn eval(
    expr: &Spanned<Expr>,
    env: Rc<RefCell<Environment>>,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum evaluation depth exceeded ({MAX_EVAL_DEPTH})"),
            expr.span.clone(),
        )
        .into());
    }
    match &expr.node {
        Expr::Int(n) => Ok(Rc::new(Thunk::new_materialized(
            Value::Int(*n),
            expr.span.clone(),
        ))),
        Expr::Float(f) => Ok(Rc::new(Thunk::new_materialized(
            Value::Float(*f),
            expr.span.clone(),
        ))),
        Expr::Bool(b) => Ok(Rc::new(Thunk::new_materialized(
            Value::Bool(*b),
            expr.span.clone(),
        ))),
        Expr::Str(s) => Ok(Rc::new(Thunk::new_materialized(
            Value::String(s.clone()),
            expr.span.clone(),
        ))),
        Expr::VarRef(name) => {
            let found = env.borrow().get(name);
            match found {
                Some(thunk) => Ok(thunk),
                None => Err(EvalError::new(
                    format!("undefined variable: ${name}"),
                    expr.span.clone(),
                )
                .into()),
            }
        }
        Expr::Dict(entries) => eval_dict(entries, &env, &expr.span, depth),
        Expr::DotAccess {
            expr: target,
            field,
        } => eval_dot_access(target, field, &env, &expr.span, depth),
        Expr::BracketAccess {
            expr: target,
            key: key_expr,
        } => eval_bracket_access(target, key_expr, &env, &expr.span, depth),
        Expr::RangeAccess {
            expr: target,
            start,
            end,
        } => eval_range_access(
            target,
            start.as_deref(),
            end.as_deref(),
            &env,
            &expr.span,
            depth,
        ),
        Expr::TypeAssert { expr: inner, .. } => {
            // Phase 1c: evaluate as identity. Type checker enforces in Phase 2a.
            eval(inner, env, depth + 1)
        }
        Expr::Annotated { name, .. } => {
            // Phase 1c: evaluate as the bare string. Type checker interprets in Phase 2a.
            Ok(Rc::new(Thunk::new_materialized(
                Value::String(name.clone()),
                expr.span.clone(),
            )))
        }
        Expr::Call { .. } => {
            Err(EvalError::new("not yet implemented: Call", expr.span.clone()).into())
        }
        Expr::Fn { .. } => Err(EvalError::new("not yet implemented: Fn", expr.span.clone()).into()),
        Expr::TypeAlias(_) => {
            Err(EvalError::new("not yet implemented: TypeAlias", expr.span.clone()).into())
        }
    }
}

// --- Dict Construction (letrec) ---

fn eval_dict(
    entries: &[Spanned<crate::ast::Entry>],
    parent_env: &Rc<RefCell<Environment>>,
    dict_span: &Span,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    let dict_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        parent_env,
    ))));
    let mut dict_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
    let mut auto_index: i64 = 0;

    for entry in entries {
        let key = match &entry.node.key {
            // Keys are evaluated in the parent scope, not dict_env, because key
            // expressions must not see sibling bindings. This prevents keys from
            // depending on values that are still unevaluated thunks and keeps
            // key evaluation deterministic regardless of entry order.
            Some(key_expr) => eval_key(key_expr, parent_env, depth)?,
            None => {
                let k = Key::Int(auto_index);
                auto_index += 1;
                k
            }
        };

        if dict_map.contains_key(&key) {
            return Err(EvalError::new(format!("duplicate key: {key}"), entry.span.clone()).into());
        }

        let thunk = Rc::new(Thunk::new_unevaluated(
            entry.node.value.clone(),
            Rc::clone(&dict_env),
            entry.node.value.span.clone(),
        ));

        // String keys become bindings so sibling entries can reference via $name
        if let Key::String(ref name) = key {
            dict_env
                .borrow_mut()
                .insert(name.clone(), Rc::clone(&thunk));
        }

        dict_map.insert(key, thunk);
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(dict_map),
        dict_span.clone(),
    )))
}

// --- Key Evaluation ---

fn eval_key(
    key_expr: &Spanned<Expr>,
    parent_env: &Rc<RefCell<Environment>>,
    depth: usize,
) -> Result<Key, Box<EvalError>> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: evaluate and materialize
    let thunk = eval(key_expr, Rc::clone(parent_env), depth + 1)?;
    let value = materialize(&thunk, Some(&key_expr.span), depth + 1)?;
    value_to_key(&value, &key_expr.span)
}

fn value_to_key(value: &Value, span: &Span) -> Result<Key, Box<EvalError>> {
    match value {
        Value::String(s) => Ok(Key::String(s.clone())),
        Value::Int(n) => Ok(Key::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), span.clone()).into()),
    }
}

// --- Access Chains ---

/// DotAccess: materialize target, look up string key in dict.
fn eval_dot_access(
    target: &Spanned<Expr>,
    field: &str,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    let target_thunk = eval(target, Rc::clone(env), depth + 1)?;
    let target_val = materialize(&target_thunk, Some(access_span), depth + 1)?;
    match target_val {
        Value::Dict(map) => {
            let key = Key::String(field.to_string());
            match map.get(&key) {
                Some(thunk) => Ok(Rc::clone(thunk)),
                None => Err(EvalError::key_not_found(field, access_span.clone()).into()),
            }
        }
        _ => Err(
            EvalError::type_mismatch("Dict", target_val.type_name(), access_span.clone()).into(),
        ),
    }
}

/// BracketAccess: materialize target, evaluate key, look up in dict.
fn eval_bracket_access(
    target: &Spanned<Expr>,
    key_expr: &Spanned<Expr>,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    let target_thunk = eval(target, Rc::clone(env), depth + 1)?;
    let target_val = materialize(&target_thunk, Some(access_span), depth + 1)?;
    match target_val {
        Value::Dict(map) => {
            let key = eval_key(key_expr, env, depth)?;
            match map.get(&key) {
                Some(thunk) => Ok(Rc::clone(thunk)),
                None => Err(EvalError::key_not_found(&key.to_string(), access_span.clone()).into()),
            }
        }
        _ => Err(
            EvalError::type_mismatch("Dict", target_val.type_name(), access_span.clone()).into(),
        ),
    }
}

/// RangeAccess: materialize target, filter dict entries by key range.
/// Range is [start, end) -- start inclusive, end exclusive.
/// Mixed-type keys (some Int, some String) produce an error.
fn eval_range_access(
    target: &Spanned<Expr>,
    start: Option<&Spanned<Expr>>,
    end: Option<&Spanned<Expr>>,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    let target_thunk = eval(target, Rc::clone(env), depth + 1)?;
    let target_val = materialize(&target_thunk, Some(access_span), depth + 1)?;
    match target_val {
        Value::Dict(map) => {
            let start_key = start.map(|e| eval_key(e, env, depth)).transpose()?;
            let end_key = end.map(|e| eval_key(e, env, depth)).transpose()?;

            let mut result: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
            for (k, v) in &map {
                let include = match (&start_key, &end_key) {
                    (Some(s), Some(e)) => {
                        let ge_start = k.partial_cmp(s).ok_or_else(|| {
                            EvalError::new(RANGE_KEY_TYPE_ERROR, access_span.clone())
                        })?;
                        let lt_end = k.partial_cmp(e).ok_or_else(|| {
                            EvalError::new(RANGE_KEY_TYPE_ERROR, access_span.clone())
                        })?;
                        ge_start != std::cmp::Ordering::Less && lt_end == std::cmp::Ordering::Less
                    }
                    (Some(s), None) => {
                        let cmp = k.partial_cmp(s).ok_or_else(|| {
                            EvalError::new(RANGE_KEY_TYPE_ERROR, access_span.clone())
                        })?;
                        cmp != std::cmp::Ordering::Less
                    }
                    (None, Some(e)) => {
                        let cmp = k.partial_cmp(e).ok_or_else(|| {
                            EvalError::new(RANGE_KEY_TYPE_ERROR, access_span.clone())
                        })?;
                        cmp == std::cmp::Ordering::Less
                    }
                    (None, None) => true, // unbounded: include everything
                };
                if include {
                    result.insert(k.clone(), Rc::clone(v));
                }
            }

            Ok(Rc::new(Thunk::new_materialized(
                Value::Dict(result),
                access_span.clone(),
            )))
        }
        _ => Err(
            EvalError::type_mismatch("Dict", target_val.type_name(), access_span.clone()).into(),
        ),
    }
}

// --- Materialization ---

/// Force a thunk to its concrete value. Memoizes the result so subsequent
/// calls return the cached value. Detects cycles via the InProgress sentinel.
///
/// # Side effects
///
/// Mutates the thunk's internal state via `RefCell`: transitions from
/// `Unevaluated` to `InProgress` to `Materialized`. Subsequent calls
/// return the cached value without further mutation.
///
/// `mat_span` is the span of the expression that triggered materialization
/// (e.g., an access chain). Attached to errors so users can see both where
/// a value was defined and where it was forced.
pub fn materialize(
    thunk: &Thunk,
    mat_span: Option<&Span>,
    depth: usize,
) -> Result<Value, Box<EvalError>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum evaluation depth exceeded ({MAX_EVAL_DEPTH})"),
            thunk.span.clone(),
        )
        .into());
    }

    // Check current state without taking ownership
    {
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => return Ok(v.clone()),
            ThunkState::InProgress => {
                let mut err = EvalError::circular_dependency("thunk", thunk.span.clone());
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(span.clone());
                }
                return Err(err.into());
            }
            ThunkState::Unevaluated { .. } => {} // continue below
        }
    }

    // Take the unevaluated data, atomically setting state to InProgress
    let (expr, env) = thunk
        .take_unevaluated()
        .expect("state must be Unevaluated after check");

    // Evaluate and recursively materialize
    let result_thunk = eval(&expr, env, depth + 1).map_err(|mut e| {
        if e.materialization_span.is_none() {
            if let Some(span) = mat_span {
                e.materialization_span = Some(span.clone());
            }
        }
        e
    })?;
    let value = materialize(&result_thunk, mat_span, depth + 1).map_err(|mut e| {
        if e.materialization_span.is_none() {
            if let Some(span) = mat_span {
                e.materialization_span = Some(span.clone());
            }
        }
        e
    })?;

    // Memoize
    thunk.transition(|_| ThunkState::Materialized(value.clone()));

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::*;
    use crate::test_util::{sp, test_span};
    use crate::value::*;

    fn empty_env() -> Rc<RefCell<Environment>> {
        Rc::new(RefCell::new(Environment::new()))
    }

    // --- Literal Evaluation ---

    #[test]
    fn test_eval_int() {
        let expr = sp(Expr::Int(42));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_float() {
        let expr = sp(Expr::Float(3.14));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_eval_bool() {
        let expr = sp(Expr::Bool(true));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_eval_str() {
        let expr = sp(Expr::Str("hello".into()));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    // --- VarRef Lookup ---

    #[test]
    fn test_varref_found() {
        let env = empty_env();
        let span = test_span(1, 1, 1, 5);
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(Value::Int(99), span)),
        );

        let expr = sp(Expr::VarRef("x".into()));
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_varref_parent_scope() {
        let parent = empty_env();
        let span = test_span(1, 1, 1, 5);
        parent.borrow_mut().insert(
            "y".into(),
            Rc::new(Thunk::new_materialized(Value::Int(77), span)),
        );

        let child = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&parent))));
        let expr = sp(Expr::VarRef("y".into()));
        let thunk = eval(&expr, child, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(77));
    }

    #[test]
    fn test_varref_not_found() {
        let expr = sp(Expr::VarRef("missing".into()));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("undefined variable: $missing"),
            "got: {}",
            err.message
        );
    }

    // --- Simple Dict ---

    #[test]
    fn test_simple_dict() {
        // [x: 1  y: hello]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::Str("hello".into())),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                assert_eq!(materialize(x_thunk, None, 0).unwrap(), Value::Int(1));
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(
                    materialize(y_thunk, None, 0).unwrap(),
                    Value::String("hello".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Auto-indexed Dict ---

    #[test]
    fn test_auto_indexed_dict() {
        let entries = vec![
            sp(Entry {
                key: None,
                value: sp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(30)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 3);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap(),
                    Value::Int(10)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap(),
                    Value::Int(20)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, 0).unwrap(),
                    Value::Int(30)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Mixed Keyed + Auto-indexed ---

    #[test]
    fn test_mixed_keyed_and_auto_indexed() {
        // [name: hello  42  flag: true  99]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: sp(Expr::Str("hello".into())),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("flag".into()))),
                value: sp(Expr::Bool(true)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(99)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 4);
                assert_eq!(
                    materialize(map.get(&Key::String("name".into())).unwrap(), None, 0).unwrap(),
                    Value::String("hello".into())
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap(),
                    Value::Int(42)
                );
                assert_eq!(
                    materialize(map.get(&Key::String("flag".into())).unwrap(), None, 0).unwrap(),
                    Value::Bool(true)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap(),
                    Value::Int(99)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Dict Letrec ---

    #[test]
    fn test_dict_letrec_sibling_reference() {
        // [x: 5  y: $x]
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(5)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::VarRef("x".into())),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(5));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_dict_letrec_forward_reference() {
        // [y: $x  x: 10] -- y references x which is defined after y
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::VarRef("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(10)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Cycle Detection ---

    #[test]
    fn test_cycle_detection() {
        // [x: $x] -- x references itself
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                let err = materialize(x_thunk, None, 0).unwrap_err();
                assert!(
                    err.message.contains("circular dependency"),
                    "got: {}",
                    err.message
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Nested Dict Scope ---

    #[test]
    fn test_nested_dict_sees_outer_bindings() {
        // [x: 42  inner: [y: $x]]
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: sp(Expr::VarRef("x".into())),
        })];
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(42)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("inner".into()))),
                value: sp(Expr::Dict(inner_entries)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let outer = materialize(&thunk, None, 0).unwrap();

        match outer {
            Value::Dict(outer_map) => {
                let inner_thunk = outer_map.get(&Key::String("inner".into())).unwrap();
                let inner_val = materialize(inner_thunk, None, 0).unwrap();
                match inner_val {
                    Value::Dict(inner_map) => {
                        let y_thunk = inner_map.get(&Key::String("y".into())).unwrap();
                        assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    // --- Duplicate Key ---

    #[test]
    fn test_duplicate_key_error() {
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(2)),
            }),
        ];
        let expr = sp(Expr::Dict(entries));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("duplicate key: x"),
            "got: {}",
            err.message
        );
    }

    // --- Unimplemented Expr ---

    #[test]
    fn test_unimplemented_call() {
        let expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![],
            named_args: vec![],
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("not yet implemented: Call"),
            "got: {}",
            err.message
        );
    }

    // --- DotAccess ---

    fn dict_with_entries(entries: Vec<(&str, Value)>) -> Spanned<Expr> {
        let ast_entries = entries
            .into_iter()
            .map(|(k, v)| {
                let value_expr = match v {
                    Value::Int(n) => Expr::Int(n),
                    Value::String(s) => Expr::Str(s),
                    Value::Bool(b) => Expr::Bool(b),
                    Value::Float(f) => Expr::Float(f),
                    _ => panic!("unsupported value type in test helper"),
                };
                sp(Entry {
                    key: Some(sp(Expr::Str(k.into()))),
                    value: sp(value_expr),
                })
            })
            .collect();
        sp(Expr::Dict(ast_entries))
    }

    #[test]
    fn test_dot_access() {
        // [name: hello].name -> "hello"
        let dict = dict_with_entries(vec![("name", Value::String("hello".into()))]);
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();

        // Bind the dict to $d in the environment
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            field: "name".into(),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_dot_access_missing_key() {
        let dict = dict_with_entries(vec![("x", Value::Int(1))]);
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            field: "missing".into(),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("key not found: missing"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_dot_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("x".into()))),
            field: "foo".into(),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
    }

    // --- BracketAccess ---

    #[test]
    fn test_bracket_access_int_key() {
        // [10 20 30][1] -> 20
        let entries = vec![
            sp(Entry {
                key: None,
                value: sp(Expr::Int(10)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(20)),
            }),
            sp(Entry {
                key: None,
                value: sp(Expr::Int(30)),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            key: Box::new(sp(Expr::Int(1))),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[test]
    fn test_bracket_access_string_key() {
        let dict = dict_with_entries(vec![("name", Value::String("alice".into()))]);
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            key: Box::new(sp(Expr::Str("name".into()))),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("alice".into()));
    }

    #[test]
    fn test_bracket_access_missing_key() {
        let dict = dict_with_entries(vec![("a", Value::Int(1))]);
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            key: Box::new(sp(Expr::Str("z".into()))),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("key not found: z"),
            "got: {}",
            err.message
        );
    }

    // --- RangeAccess ---

    #[test]
    fn test_range_access_both_bounds() {
        // [0: a  1: b  2: c  3: d  4: e][2..4] -> [2: c  3: d]
        let entries: Vec<_> = (0..5)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: sp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            start: Some(Box::new(sp(Expr::Int(2)))),
            end: Some(Box::new(sp(Expr::Int(4)))),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::Int(2)).unwrap(), None, 0).unwrap(),
                    Value::String("v2".into())
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(3)).unwrap(), None, 0).unwrap(),
                    Value::String("v3".into())
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_start_only() {
        // [0: a  1: b  2: c][1..] -> [1: b  2: c]
        let entries: Vec<_> = (0..3)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: sp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            start: Some(Box::new(sp(Expr::Int(1)))),
            end: None,
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert!(map.contains_key(&Key::Int(1)));
                assert!(map.contains_key(&Key::Int(2)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_end_only() {
        // [0: a  1: b  2: c][..2] -> [0: a  1: b]
        let entries: Vec<_> = (0..3)
            .map(|i| {
                sp(Entry {
                    key: Some(sp(Expr::Int(i))),
                    value: sp(Expr::Str(format!("v{i}"))),
                })
            })
            .collect();
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            start: None,
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert!(map.contains_key(&Key::Int(0)));
                assert!(map.contains_key(&Key::Int(1)));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_unbounded() {
        // [0: a  1: b][..] -> all entries
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Int(0))),
                value: sp(Expr::Str("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Int(1))),
                value: sp(Expr::Str("b".into())),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            start: None,
            end: None,
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 2),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_range_access_mixed_keys_error() {
        // [0: a  name: b][0..1] -> error (mixed Int and String keys)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Int(0))),
                value: sp(Expr::Str("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: sp(Expr::Str("b".into())),
            }),
        ];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("d".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(1)))),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("comparable key types"),
            "got: {}",
            err.message
        );
    }

    // --- TypeAssert (identity) ---

    #[test]
    fn test_type_assert_identity() {
        // [@Number 42] -> 42
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    // --- Annotated (bare string) ---

    #[test]
    fn test_annotated_bare_string() {
        // Config@ConfigType -> "Config"
        let expr = sp(Expr::Annotated {
            name: "Config".into(),
            annotation: sp(Annotation::Simple("ConfigType".into())),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("Config".into()));
    }

    // --- Chained access ---

    #[test]
    fn test_chained_dot_access() {
        // [outer: [inner: 99]].outer.inner -> 99
        let inner_entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("inner".into()))),
            value: sp(Expr::Int(99)),
        })];
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("outer".into()))),
            value: sp(Expr::Dict(inner_entries)),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "d".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        // $d.outer.inner
        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::VarRef("d".into()))),
                field: "outer".into(),
            })),
            field: "inner".into(),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    // --- Depth limit ---

    #[test]
    fn test_eval_depth_limit() {
        let expr = sp(Expr::Int(42));
        let err = eval(&expr, empty_env(), MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message.contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_materialize_depth_limit() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(1), span);
        let err = materialize(&thunk, None, MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message.contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message
        );
    }

    // --- Materialization span ---

    #[test]
    fn test_materialization_span_on_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();

        // Extract x's thunk from the dict
        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // Materialize x with a known materialization span
        let mat_span = test_span(5, 1, 5, 5);
        let err = materialize(&x_thunk, Some(&mat_span), 0).unwrap_err();
        assert!(
            err.message.contains("undefined variable: $missing"),
            "got: {}",
            err.message
        );
        assert_eq!(
            err.materialization_span,
            Some(mat_span),
            "materialization span should be the access site"
        );
    }

    #[test]
    fn test_cycle_has_materialization_span() {
        // [x: $x] -- force x with a known materialization site
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        match val {
            Value::Dict(map) => {
                let x_thunk = map.get(&Key::String("x".into())).unwrap();
                let mat_span = test_span(10, 1, 10, 5);
                let err = materialize(x_thunk, Some(&mat_span), 0).unwrap_err();
                assert!(err.message.contains("circular dependency"));
                assert_eq!(err.materialization_span, Some(mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- BracketAccess on non-dict ---

    #[test]
    fn test_bracket_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::BracketAccess {
            expr: Box::new(sp(Expr::VarRef("x".into()))),
            key: Box::new(sp(Expr::Int(0))),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
    }

    // --- RangeAccess on non-dict ---

    #[test]
    fn test_range_access_on_non_dict() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::String("hello".into()),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("x".into()))),
            start: Some(Box::new(sp(Expr::Int(0)))),
            end: Some(Box::new(sp(Expr::Int(2)))),
        });
        let err = eval(&expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected Dict"),
            "got: {}",
            err.message
        );
    }

    // --- RangeAccess with string keys ---

    #[test]
    fn test_range_access_string_keys() {
        // [a: 1  b: 2  c: 3  d: 4]["b".."d"] -> [b: 2  c: 3]
        let dict = dict_with_entries(vec![
            ("a", Value::Int(1)),
            ("b", Value::Int(2)),
            ("c", Value::Int(3)),
            ("d", Value::Int(4)),
        ]);
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "dd".into(),
            Rc::new(Thunk::new_materialized(dict_val, test_span(1, 1, 1, 10))),
        );

        let expr = sp(Expr::RangeAccess {
            expr: Box::new(sp(Expr::VarRef("dd".into()))),
            start: Some(Box::new(sp(Expr::Str("b".into())))),
            end: Some(Box::new(sp(Expr::Str("d".into())))),
        });
        let thunk = eval(&expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::String("b".into())).unwrap(), None, 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::String("c".into())).unwrap(), None, 0).unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- Unimplemented Fn ---

    #[test]
    fn test_unimplemented_fn() {
        let expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Box::new(sp(Expr::VarRef("x".into()))),
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("not yet implemented: Fn"),
            "got: {}",
            err.message
        );
    }

    // --- Unimplemented TypeAlias ---

    #[test]
    fn test_unimplemented_type_alias() {
        let expr = sp(Expr::TypeAlias(Box::new(sp(Expr::VarRef("MyType".into())))));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("not yet implemented: TypeAlias"),
            "got: {}",
            err.message
        );
    }

    // --- value_to_key with invalid types ---

    #[test]
    fn test_value_to_key_invalid_type_bool() {
        // A dict with a Bool key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Bool(true))),
            value: sp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String or Int"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Bool"), "got: {}", err.message);
    }

    #[test]
    fn test_value_to_key_invalid_type_float() {
        // A dict with a Float key expression should fail in eval_key -> value_to_key
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Float(3.14))),
            value: sp(Expr::Int(1)),
        })];
        let expr = sp(Expr::Dict(entries));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("expected String or Int"),
            "got: {}",
            err.message
        );
        assert!(err.message.contains("got Float"), "got: {}", err.message);
    }
}
