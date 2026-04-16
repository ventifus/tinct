//! Core evaluation module: lazy evaluation with letrec dict scoping, document
//! pipelines, function evaluation, and `$_` implicit lambda desugaring.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Document, Entry, Expr, File, NamedArg, Param, Position, Span, Spanned};
use crate::error::EvalError;
use crate::value::{Environment, Key, Thunk, ThunkState, Value};

pub(crate) const MAX_EVAL_DEPTH: usize = 256;
const RANGE_KEY_TYPE_ERROR: &str = "range access requires comparable key types";
const DEFAULT_ANNOTATION_KEY: &str = "default";

/// Check whether `k` falls in the half-open range `[start, end)`.
/// `None` bounds are treated as unbounded (i.e. negative/positive infinity).
/// Returns an error when `k` is not comparable with the bound (mixed key types).
fn key_in_range(
    k: &Key,
    start: Option<&Key>,
    end: Option<&Key>,
    span: Span,
) -> Result<bool, Box<EvalError>> {
    let after_start = match start {
        Some(s) => {
            let ord = k
                .partial_cmp(s)
                .ok_or_else(|| EvalError::new(RANGE_KEY_TYPE_ERROR, span))?;
            ord != std::cmp::Ordering::Less
        }
        None => true,
    };
    let before_end = match end {
        Some(e) => {
            let ord = k
                .partial_cmp(e)
                .ok_or_else(|| EvalError::new(RANGE_KEY_TYPE_ERROR, span))?;
            ord == std::cmp::Ordering::Less
        }
        None => true,
    };
    Ok(after_start && before_end)
}

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
            expr.span,
        )
        .into());
    }
    // $_ implicit lambda desugaring: if the expression directly contains VarRef("_")
    // and `_` is not already bound in the current environment (i.e., we're not inside
    // the body of a lambda that already captured `_`), wrap it in [fn [_] <expr>].
    if should_desugar_underscore(&expr.node) && env.borrow().get("_").is_none() {
        let lambda = wrap_in_lambda(expr);
        return eval(&lambda, env, depth + 1);
    }

    match &expr.node {
        Expr::Int(n) => Ok(Rc::new(Thunk::new_materialized(Value::Int(*n), expr.span))),
        Expr::Float(f) => Ok(Rc::new(Thunk::new_materialized(
            Value::Float(*f),
            expr.span,
        ))),
        Expr::Bool(b) => Ok(Rc::new(Thunk::new_materialized(Value::Bool(*b), expr.span))),
        Expr::Str(s) => Ok(Rc::new(Thunk::new_materialized(
            Value::String(s.clone()),
            expr.span,
        ))),
        Expr::VarRef(name) => {
            let found = env.borrow().get(name);
            match found {
                Some(thunk) => Ok(thunk),
                None => {
                    Err(EvalError::new(format!("undefined variable: ${name}"), expr.span).into())
                }
            }
        }
        Expr::Dict(entries) => eval_dict(entries, &env, &expr.span, depth + 1),
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
            // Evaluate as identity; the type checker (typecheck.rs) validates the assertion.
            eval(inner, env, depth + 1)
        }
        Expr::Annotated { name, .. } => {
            // Evaluate as the bare string; the type checker (typecheck.rs) interprets annotations.
            Ok(Rc::new(Thunk::new_materialized(
                Value::String(name.clone()),
                expr.span,
            )))
        }
        Expr::Fn {
            return_ann,
            params,
            body,
        } => {
            let fn_params: Vec<Param> = params.iter().map(|p| p.node.clone()).collect();
            Ok(Rc::new(Thunk::new_materialized(
                Value::Function {
                    params: Rc::new(fn_params),
                    body: Rc::new(*body.clone()),
                    env: Rc::clone(&env),
                    return_ann: return_ann.clone(),
                },
                expr.span,
            )))
        }
        Expr::Call {
            func,
            args,
            named_args,
        } => eval_call(func, args, named_args, &env, &expr.span, depth),
        Expr::TypeAlias(_inner) => {
            // Type aliases are handled by the type checker (typecheck.rs). Evaluate as unit (empty dict).
            Ok(Rc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                expr.span,
            )))
        }
    }
}

// --- Document Evaluation (scope chains) ---

/// Evaluate a document: a sequence of expressions forming a scope chain.
///
/// Each intermediate expression is materialized and must produce a `Value::Dict`.
/// The dict's string-keyed entries become bindings in a new child environment that
/// serves as the scope for the next expression. The last expression is returned
/// as-is (lazy, any type). An empty document returns an empty dict.
pub fn eval_document(
    doc: &Spanned<Document>,
    env: Rc<RefCell<Environment>>,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    let exprs = &doc.node.expressions;

    if exprs.is_empty() {
        return Ok(Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            doc.span,
        )));
    }

    let mut current_env = env;

    for (i, expr) in exprs.iter().enumerate() {
        let is_last = i == exprs.len() - 1;

        if is_last {
            // Last expression: return its thunk as-is (lazy, any type)
            return eval(expr, current_env, depth);
        }

        // Intermediate expression: materialize and extract dict bindings
        let thunk = eval(expr, Rc::clone(&current_env), depth)?;
        let value = materialize(&thunk, Some(&expr.span), depth)?;

        match value {
            Value::Dict(map) => {
                let child_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
                    &current_env,
                ))));
                for (key, val_thunk) in &map {
                    // Only string keys become scope bindings; int keys are positional, not named.
                    if let Key::String(name) = key {
                        child_env
                            .borrow_mut()
                            .insert(name.clone(), Rc::clone(val_thunk));
                    }
                }
                current_env = child_env;
            }
            _ => {
                return Err(EvalError::type_mismatch("Dict", value.type_name(), expr.span).into());
            }
        }
    }

    unreachable!("document has expressions but loop did not return")
}

// --- File Evaluation (multi-document pipeline) ---

fn empty_dict_thunk(span: Span) -> Rc<Thunk> {
    Rc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span))
}

fn origin_span() -> Span {
    let pos = Position {
        offset: 0,
        line: 1,
        column: 1,
    };
    Span::new(pos, pos)
}

/// Evaluate a file: one or more documents separated by `---`.
///
/// Documents are totally isolated -- they share no scope. Data flows between
/// documents via `$$` (the variable `$`), which is injected into each
/// document's root scope containing the previous document's output.
///
/// - For the first document, `$$` is an empty dict.
/// - For subsequent documents, `$$` is the previous document's result thunk
///   (lazy -- no materialization at the `---` boundary).
/// - The last document's result is the file's output.
/// - An empty file (zero documents) returns an empty dict.
pub fn eval_file(
    file: &File,
    env: Rc<RefCell<Environment>>,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    // $$ starts as empty dict for first document
    let mut prev_output = empty_dict_thunk(origin_span());

    for doc in &file.documents {
        // Each document gets a fresh scope with only $$ bound
        let doc_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&env))));
        doc_env
            .borrow_mut()
            .insert("$".to_string(), Rc::clone(&prev_output));

        let result = eval_document(doc, doc_env, depth)?;
        prev_output = result; // lazy: no materialization at boundary
    }

    Ok(prev_output)
}

// --- Dict Construction (letrec) ---

fn eval_dict(
    entries: &[Spanned<Entry>],
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
            return Err(EvalError::new(format!("duplicate key: {key}"), entry.span).into());
        }

        let thunk = Rc::new(Thunk::new_unevaluated(
            entry.node.value.clone(),
            Rc::clone(&dict_env),
            entry.node.value.span,
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
        *dict_span,
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
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), *span).into()),
    }
}

// --- Function Call ---

/// Evaluate a call expression: materialize the function, bind arguments, wrap body as thunk.
fn eval_call(
    func_expr: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<RefCell<Environment>>,
    call_span: &Span,
    depth: usize,
) -> Result<Rc<Thunk>, Box<EvalError>> {
    // Evaluate and materialize the function
    let func_thunk = eval(func_expr, Rc::clone(env), depth + 1)?;
    let func_val = materialize(&func_thunk, Some(call_span), depth + 1)?;

    match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            let call_env = bind_args(
                &params,
                args,
                named_args,
                env,
                &closure_env,
                call_span,
                depth,
            )?;
            // Wrap the body as an unevaluated thunk in the call environment
            Ok(Rc::new(Thunk::new_unevaluated(
                (*body).clone(),
                call_env,
                *call_span,
            )))
        }
        Value::Builtin { func, .. } => {
            // Evaluate positional args as thunks (lazy)
            let mut pos_thunks = Vec::with_capacity(args.len());
            for arg in args {
                pos_thunks.push(eval(arg, Rc::clone(env), depth + 1)?);
            }
            // Evaluate named args as thunks
            let mut named_thunks = IndexMap::new();
            for na in named_args {
                let thunk = eval(&na.node.value, Rc::clone(env), depth + 1)?;
                named_thunks.insert(na.node.name.clone(), thunk);
            }
            let result = func(&pos_thunks, &named_thunks)?;
            Ok(Rc::new(Thunk::new_materialized(result, *call_span)))
        }
        _ => Err(
            EvalError::type_mismatch("Function or Builtin", func_val.type_name(), *call_span)
                .into(),
        ),
    }
}

/// Bind call arguments to function parameters. Returns the new call environment.
///
/// Handles positional args, named args (params with `default:` annotation),
/// and variadic params (`...name`).
fn bind_args(
    params: &[Param],
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    caller_env: &Rc<RefCell<Environment>>,
    closure_env: &Rc<RefCell<Environment>>,
    call_span: &Span,
    depth: usize,
) -> Result<Rc<RefCell<Environment>>, Box<EvalError>> {
    let call_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
        closure_env,
    ))));

    // Separate the variadic param (if any) from regular params
    let (regular_params, variadic_param) = split_variadic(params);

    // Count required positional params (those without default: annotation)
    let required_count = regular_params
        .iter()
        .filter(|p| get_default(p).is_none())
        .count();
    let max_positional = regular_params.len();

    if variadic_param.is_some() {
        // With variadic: need at least required_count positional args
        if args.len() < required_count {
            return Err(EvalError::arity_mismatch(required_count, args.len(), *call_span).into());
        }
    } else {
        // Without variadic: positional args must be between required_count and max_positional
        if args.len() < required_count {
            return Err(EvalError::arity_mismatch(required_count, args.len(), *call_span).into());
        }
        if args.len() > max_positional {
            return Err(EvalError::arity_mismatch(max_positional, args.len(), *call_span).into());
        }
    }

    // Bind positional args to regular params
    for (i, param) in regular_params.iter().enumerate() {
        let thunk = if i < args.len() {
            // Positional arg provided
            eval(&args[i], Rc::clone(caller_env), depth + 1)?
        } else if let Some(default_val) = get_default(param) {
            // Check if a named arg was provided for this param
            if let Some(na) = named_args.iter().find(|na| na.node.name == param.name) {
                eval(&na.node.value, Rc::clone(caller_env), depth + 1)?
            } else {
                // Use default
                eval(&default_val, Rc::clone(caller_env), depth + 1)?
            }
        } else {
            // This shouldn't happen due to arity check above
            return Err(EvalError::arity_mismatch(required_count, args.len(), *call_span).into());
        };
        call_env.borrow_mut().insert(param.name.clone(), thunk);
    }

    // Check for named args that target params already bound positionally
    for na in named_args {
        if let Some(idx) = regular_params.iter().position(|p| p.name == na.node.name) {
            if idx < args.len() {
                return Err(EvalError::new(
                    format!(
                        "parameter '{}' received both positional and named argument",
                        na.node.name
                    ),
                    *call_span,
                )
                .into());
            }
        }
    }

    // Handle named args that weren't consumed by positional binding
    for na in named_args {
        let already_bound = call_env.borrow().bindings.contains_key(&na.node.name);
        if !already_bound {
            // Check that the named arg corresponds to a param with default:
            let is_valid_param = regular_params
                .iter()
                .any(|p| p.name == na.node.name && get_default(p).is_some());
            if !is_valid_param {
                return Err(EvalError::new(
                    format!("unexpected named argument: {}", na.node.name),
                    *call_span,
                )
                .into());
            }
            let thunk = eval(&na.node.value, Rc::clone(caller_env), depth + 1)?;
            call_env.borrow_mut().insert(na.node.name.clone(), thunk);
        }
    }

    // Bind variadic param: collect remaining positional args into a dict with int keys
    if let Some(var_param) = variadic_param {
        let mut var_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
        for (i, arg) in args.iter().enumerate().skip(max_positional) {
            let thunk = eval(arg, Rc::clone(caller_env), depth + 1)?;
            var_map.insert(Key::Int(i as i64 - max_positional as i64), thunk);
        }
        let var_thunk = Rc::new(Thunk::new_materialized(Value::Dict(var_map), *call_span));
        call_env
            .borrow_mut()
            .insert(var_param.name.clone(), var_thunk);
    }

    Ok(call_env)
}

/// Split params into (regular, optional variadic).
fn split_variadic(params: &[Param]) -> (&[Param], Option<&Param>) {
    match params.last() {
        Some(p) if p.variadic => (&params[..params.len() - 1], Some(p)),
        _ => (params, None),
    }
}

/// Extract the default value expression from a param's annotation, if present.
/// default: is specified via PropertyDict annotation with a "default" key.
fn get_default(param: &Param) -> Option<Spanned<Expr>> {
    param
        .annotation
        .as_ref()
        .and_then(|ann| ann.node.get_property(DEFAULT_ANNOTATION_KEY))
        .cloned()
}

// --- Implicit Lambda ($_ desugaring) ---

/// Check if an expression directly contains VarRef("_") (not nested in inner brackets).
fn contains_direct_underscore(expr: &Expr) -> bool {
    match expr {
        Expr::VarRef(name) => name == "_",
        // Access chains on $_ count as direct (e.g., $_.name)
        Expr::DotAccess { expr: inner, .. } => contains_direct_underscore(&inner.node),
        Expr::BracketAccess { expr: inner, .. } => contains_direct_underscore(&inner.node),
        Expr::RangeAccess { expr: inner, .. } => contains_direct_underscore(&inner.node),
        // Dict/Call/Fn create a new bracket boundary -- $_ inside them is NOT direct
        Expr::Dict(_) | Expr::Call { .. } | Expr::Fn { .. } => false,
        // Literals, TypeAlias, TypeAssert, Annotated cannot contain $_
        _ => false,
    }
}

/// Check if a Call expression has any direct $_ references in its args/named_args.
fn call_has_direct_underscore(args: &[Spanned<Expr>], named_args: &[Spanned<NamedArg>]) -> bool {
    args.iter().any(|a| contains_direct_underscore(&a.node))
        || named_args
            .iter()
            .any(|na| contains_direct_underscore(&na.node.value.node))
}

/// Determine if an expression should be desugared into an implicit lambda.
/// This applies when the expression directly contains $_ and is NOT itself
/// a bare VarRef("_") (which would be just looking up the variable).
fn should_desugar_underscore(expr: &Expr) -> bool {
    match expr {
        // A bare $_ is just a variable reference, not an implicit lambda
        Expr::VarRef(_) => false,
        // Access chains rooted at $_ → implicit lambda
        Expr::DotAccess { expr: inner, .. }
        | Expr::BracketAccess { expr: inner, .. }
        | Expr::RangeAccess { expr: inner, .. } => contains_direct_underscore(&inner.node),
        // Call with $_ in args → implicit lambda
        Expr::Call {
            args, named_args, ..
        } => call_has_direct_underscore(args, named_args),
        // Dict with $_ in entries → implicit lambda
        Expr::Dict(entries) => entries
            .iter()
            .any(|e| contains_direct_underscore(&e.node.value.node)),
        _ => false,
    }
}

/// Wrap an expression in `[fn [_] <expr>]`.
fn wrap_in_lambda(expr: &Spanned<Expr>) -> Spanned<Expr> {
    Spanned::new(
        Expr::Fn {
            return_ann: None,
            params: vec![Spanned::new(
                Param {
                    name: "_".to_string(),
                    annotation: None,
                    variadic: false,
                },
                expr.span,
            )],
            body: Box::new(expr.clone()),
        },
        expr.span,
    )
}

// --- Access Chain Helpers ---

/// Evaluate a target expression, materialize, and return the inner IndexMap if
/// it's a Dict, otherwise return a type-mismatch error. Shared by all access
/// chain functions (dot, bracket, range).
fn eval_as_dict(
    target: &Spanned<Expr>,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> Result<IndexMap<Key, Rc<Thunk>>, Box<EvalError>> {
    let target_thunk = eval(target, Rc::clone(env), depth + 1)?;
    let target_val = materialize(&target_thunk, Some(access_span), depth + 1)?;
    match target_val {
        Value::Dict(map) => Ok(map),
        _ => Err(EvalError::type_mismatch("Dict", target_val.type_name(), *access_span).into()),
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
    let map = eval_as_dict(target, env, access_span, depth)?;
    let key = Key::String(field.to_string());
    match map.get(&key) {
        Some(thunk) => Ok(Rc::clone(thunk)),
        None => Err(EvalError::key_not_found(field, *access_span).into()),
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
    let map = eval_as_dict(target, env, access_span, depth)?;
    let key = eval_key(key_expr, env, depth)?;
    match map.get(&key) {
        Some(thunk) => Ok(Rc::clone(thunk)),
        None => Err(EvalError::key_not_found(&key.to_string(), *access_span).into()),
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
    let map = eval_as_dict(target, env, access_span, depth)?;
    let start_key = start.map(|e| eval_key(e, env, depth)).transpose()?;
    let end_key = end.map(|e| eval_key(e, env, depth)).transpose()?;

    let mut result: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
    for (k, v) in &map {
        if key_in_range(k, start_key.as_ref(), end_key.as_ref(), *access_span)? {
            result.insert(k.clone(), Rc::clone(v));
        }
    }

    Ok(Rc::new(Thunk::new_materialized(
        Value::Dict(result),
        *access_span,
    )))
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
            thunk.span,
        )
        .into());
    }

    // Check current state without taking ownership
    {
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => return Ok(v.clone()),
            ThunkState::InProgress => {
                let mut err = EvalError::circular_dependency("thunk", thunk.span);
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(*span);
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

    // Evaluate and recursively materialize. On failure, restore the thunk
    // to Unevaluated so it can be retried (avoids thunk poisoning where a
    // failed thunk permanently reports "circular dependency").
    let attach_mat_span = |mut e: Box<EvalError>| -> Box<EvalError> {
        if e.materialization_span.is_none() {
            if let Some(span) = mat_span {
                e.materialization_span = Some(*span);
            }
        }
        e
    };
    let result = (|| {
        let result_thunk = eval(&expr, Rc::clone(&env), depth + 1).map_err(attach_mat_span)?;
        materialize(&result_thunk, mat_span, depth + 1).map_err(attach_mat_span)
    })();

    match result {
        Ok(value) => {
            thunk.transition(|_| ThunkState::Materialized(value.clone()));
            Ok(value)
        }
        Err(e) => {
            // Restore thunk so it can be retried
            thunk.transition(|_| ThunkState::Unevaluated { expr, env });
            Err(e)
        }
    }
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

    // --- Thunk Poisoning Prevention ---

    #[test]
    fn test_thunk_retryable_after_error() {
        // [x: $missing] -- materializing x fails because $missing is undefined.
        // After failure, the thunk must be restored to Unevaluated, not left
        // as InProgress. A second materialize attempt should produce the same
        // "undefined variable" error, NOT "circular dependency".
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First attempt: should fail with "undefined variable"
        let err1 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err1.message.contains("undefined variable: $missing"),
            "first attempt: got: {}",
            err1.message
        );

        // Second attempt: should produce the SAME error, not "circular dependency"
        let err2 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err2.message.contains("undefined variable: $missing"),
            "second attempt should not be poisoned, got: {}",
            err2.message
        );
        assert!(
            !err2.message.contains("circular dependency"),
            "thunk was poisoned: got circular dependency on retry"
        );
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

    // --- Fn Evaluation ---

    #[test]
    fn test_fn_creates_function_value() {
        // [fn [x] $x] → Function
        let expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![sp(Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            })],
            body: Box::new(sp(Expr::VarRef("x".into()))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_fn_captures_closure_env() {
        // outer: 42 is in env, [fn [] $outer] should capture it
        let env = empty_env();
        env.borrow_mut().insert(
            "outer".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let fn_expr = sp(Expr::Fn {
            return_ann: None,
            params: vec![],
            body: Box::new(sp(Expr::VarRef("outer".into()))),
        });
        let fn_thunk = eval(&fn_expr, Rc::clone(&env), 0).unwrap();
        let fn_val = materialize(&fn_thunk, None, 0).unwrap();

        // Call it: [call $f]
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![],
            named_args: vec![],
        });
        let result_thunk = eval(&call_expr, env, 0).unwrap();
        let result = materialize(&result_thunk, None, 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    // --- Call Evaluation ---

    #[test]
    fn test_call_simple() {
        // Define identity function and call it
        // f: [fn [x] $x]
        // [call $f 42]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(42))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_multiple_args() {
        // f: [fn [a b] $b]  -- returns second arg
        // [call $f 10 20] → 20
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("b".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(10)), sp(Expr::Int(20))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(20));
    }

    #[test]
    fn test_call_on_non_function() {
        let env = empty_env();
        env.borrow_mut().insert(
            "x".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(42),
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("x".into()))),
            args: vec![],
            named_args: vec![],
        });
        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("type mismatch"),
            "got: {}",
            err.message
        );
        assert!(
            err.message.contains("Function or Builtin"),
            "got: {}",
            err.message
        );
    }

    // --- Arity Checking ---

    #[test]
    fn test_call_too_few_args() {
        // f: [fn [x y] $x]
        // [call $f 1] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: None,
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![],
        });
        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_too_many_args() {
        // f: [fn [x] $x]
        // [call $f 1 2] → arity mismatch
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1)), sp(Expr::Int(2))],
            named_args: vec![],
        });
        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("arity mismatch"),
            "got: {}",
            err.message
        );
    }

    // --- Named Arguments ---

    #[test]
    fn test_call_named_arg_with_default() {
        // f: [fn [x  y@[default: 99]] [result: $y]]
        // [call $f 1] → y defaults to 99
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: sp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("y".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        // Call without named arg -- y should default to 99
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, Rc::clone(&env), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

    #[test]
    fn test_call_named_arg_overridden() {
        // f: [fn [x  y@[default: 99]] $y]
        // [call $f 1 y: 42] → y = 42
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: sp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("y".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: sp(Expr::Int(42)),
            })],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_call_unexpected_named_arg() {
        // f: [fn [x] $x]
        // [call $f 1 z: 2] → error: unexpected named argument
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![sp(NamedArg {
                name: "z".into(),
                value: sp(Expr::Int(2)),
            })],
        });
        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(
            err.message.contains("unexpected named argument: z"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn test_call_duplicate_positional_and_named_error() {
        // f: [fn [x y@[default: 99]] $y]
        // [call $f 1 2 y: 42] → error: y received both positional and named argument
        let env = empty_env();
        let default_entry = sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: sp(Expr::Int(99)),
        });
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![default_entry]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("y".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1)), sp(Expr::Int(2))],
            named_args: vec![sp(NamedArg {
                name: "y".into(),
                value: sp(Expr::Int(42)),
            })],
        });
        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(
            err.message
                .contains("received both positional and named argument"),
            "got: {}",
            err.message
        );
    }

    // --- Variadic Parameters ---

    #[test]
    fn test_call_variadic() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1 2 3] → rest = Dict({0: 2, 1: 3})
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "rest".into(),
                    annotation: None,
                    variadic: true,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("rest".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1)), sp(Expr::Int(2)), sp(Expr::Int(3))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::Int(0)).unwrap(), None, 0).unwrap(),
                    Value::Int(2)
                );
                assert_eq!(
                    materialize(map.get(&Key::Int(1)).unwrap(), None, 0).unwrap(),
                    Value::Int(3)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_call_variadic_empty() {
        // f: [fn [x ...rest] $rest]
        // [call $f 1] → rest = Dict({})
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "rest".into(),
                    annotation: None,
                    variadic: true,
                },
            ]),
            body: Rc::new(sp(Expr::VarRef("rest".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    // --- Builtin Calls ---

    #[test]
    fn test_call_builtin() {
        fn add_builtin(
            args: &[Rc<Thunk>],
            _named: &IndexMap<String, Rc<Thunk>>,
        ) -> Result<Value, Box<EvalError>> {
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Value::Int(x + y)),
                _ => panic!("test expects Int args"),
            }
        }
        let env = empty_env();
        env.borrow_mut().insert(
            "add".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin {
                    name: "add",
                    func: add_builtin,
                },
                test_span(1, 1, 1, 5),
            )),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("add".into()))),
            args: vec![sp(Expr::Int(3)), sp(Expr::Int(4))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(7));
    }

    // --- TypeAlias ---

    #[test]
    fn test_type_alias_returns_empty_dict() {
        let expr = sp(Expr::TypeAlias(Box::new(sp(Expr::VarRef("MyType".into())))));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    // --- $_ Implicit Lambda ---

    #[test]
    fn test_underscore_access_chain_becomes_lambda() {
        // $_.name → [fn [_] $_.name]
        // Evaluating this should produce a Function, not look up $_
        let expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("_".into()))),
            field: "name".into(),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_call_becomes_lambda() {
        // [call $f $_] where $f is in scope → should produce a lambda
        // The outer [call ...] contains $_ directly → wraps in [fn [_] [call $f $_]]
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![sp(Expr::VarRef("_".into()))],
            named_args: vec![],
        });
        let thunk = eval(&call_expr, Rc::clone(&env), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_lambda_callable() {
        // Create $_.name as a lambda, then call it with a dict that has name: "alice"
        let env = empty_env();
        // Build the $_.name expression → becomes [fn [_] $_.name]
        let getter_expr = sp(Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("_".into()))),
            field: "name".into(),
        });
        let getter_thunk = eval(&getter_expr, Rc::clone(&env), 0).unwrap();
        let getter_val = materialize(&getter_thunk, None, 0).unwrap();
        env.borrow_mut().insert(
            "getter".into(),
            Rc::new(Thunk::new_materialized(getter_val, test_span(1, 1, 1, 10))),
        );

        // Call it with [name: alice]
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("getter".into()))),
            args: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("name".into()))),
                value: sp(Expr::Str("alice".into())),
            })]))],
            named_args: vec![],
        });
        let result_thunk = eval(&call_expr, env, 0).unwrap();
        let result = materialize(&result_thunk, None, 0).unwrap();
        assert_eq!(result, Value::String("alice".into()));
    }

    #[test]
    fn test_underscore_in_dict_entry() {
        // [a: $_.name] → desugars to [fn [_] [a: $_.name]]
        // Dict with $_ in a value position should desugar to an implicit lambda
        let expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::VarRef("_".into()))),
                field: "name".into(),
            }),
        })]));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ dict desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_underscore_in_named_arg() {
        // [call $f x: $_] → desugars to [fn [_] [call $f x: $_]]
        // Call with $_ in a named arg value should desugar to an implicit lambda
        let env = empty_env();
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
            return_ann: None,
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 10))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("f".into()))),
            args: vec![],
            named_args: vec![sp(NamedArg {
                name: "x".into(),
                value: sp(Expr::VarRef("_".into())),
            })],
        });
        let thunk = eval(&call_expr, Rc::clone(&env), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "_");
            }
            other => panic!("expected Function from $_ named arg desugaring, got {other:?}"),
        }
    }

    #[test]
    fn test_bare_underscore_is_not_lambda() {
        // $_ alone is just a VarRef, not an implicit lambda
        // It should fail with "undefined variable" if not in scope
        let expr = sp(Expr::VarRef("_".into()));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message.contains("undefined variable: $_"),
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

    // --- Document Evaluation ---

    #[test]
    fn test_eval_document_single_expression() {
        // A document with one dict expression returns that dict
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(1)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::Int(2)),
            }),
        ];
        let doc = sp(Document {
            expressions: vec![sp(Expr::Dict(entries))],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                assert_eq!(
                    materialize(map.get(&Key::String("x".into())).unwrap(), None, 0).unwrap(),
                    Value::Int(1)
                );
                assert_eq!(
                    materialize(map.get(&Key::String("y".into())).unwrap(), None, 0).unwrap(),
                    Value::Int(2)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain() {
        // Two expressions: expr 1 defines x, expr 2 references $x
        // Expr 1: [x: 10]
        // Expr 2: [y: $x]
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::Int(10)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("y".into()))),
            value: sp(Expr::VarRef("x".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_shadowing() {
        // Expr 1: [x: 1]
        // Expr 2: [x: 2  y: $x]
        // y should be 2 (local letrec wins over parent scope)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(2)),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::VarRef("x".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_intermediate_non_dict_error() {
        // Two expressions where expr 1 is a literal (not a dict). Should error.
        let expr1 = sp(Expr::Int(42));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::Int(1)),
        })]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2],
        });
        let err = eval_document(&doc, empty_env(), 0).unwrap_err();
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

    #[test]
    fn test_eval_document_empty() {
        // A document with zero expressions returns an empty dict
        let doc = sp(Document {
            expressions: vec![],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 0);
            }
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_three_expressions() {
        // Three expressions chaining scope:
        // Expr 1: [a: 1]
        // Expr 2: [b: 2]
        // Expr 3: [sum_ref_a: $a  sum_ref_b: $b]
        // Expr 3 should see both $a (from expr 1 via grandparent) and $b (from expr 2 via parent)
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("a".into()))),
            value: sp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("b".into()))),
            value: sp(Expr::Int(2)),
        })]));
        let expr3 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("ref_a".into()))),
                value: sp(Expr::VarRef("a".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("ref_b".into()))),
                value: sp(Expr::VarRef("b".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2, expr3],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let ref_a = map.get(&Key::String("ref_a".into())).unwrap();
                assert_eq!(materialize(ref_a, None, 0).unwrap(), Value::Int(1));
                let ref_b = map.get(&Key::String("ref_b".into())).unwrap();
                assert_eq!(materialize(ref_b, None, 0).unwrap(), Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_inherits_parent_env() {
        // A document evaluated with a pre-populated parent env.
        // The document's expressions should see the parent's bindings.
        let parent_env = empty_env();
        parent_env.borrow_mut().insert(
            "external".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(999),
                test_span(1, 1, 1, 5),
            )),
        );

        let expr = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("local".into()))),
            value: sp(Expr::VarRef("external".into())),
        })]));
        let doc = sp(Document {
            expressions: vec![expr],
        });
        let thunk = eval_document(&doc, parent_env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let local = map.get(&Key::String("local".into())).unwrap();
                assert_eq!(materialize(local, None, 0).unwrap(), Value::Int(999));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_single_non_dict_expression() {
        // A document with a single Int expression (not a dict).
        // The last expression can be any type.
        let doc = sp(Document {
            expressions: vec![sp(Expr::Int(42))],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_eval_document_integer_keys_skipped_in_scope_chain() {
        // Expr 1: [10 20 30] (auto-indexed: keys Int(0), Int(1), Int(2))
        // Expr 2: [result: 99]
        // Integer keys from expr 1 should not become scope bindings.
        let expr1 = sp(Expr::Dict(vec![
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
        ]));
        let expr2 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("result".into()))),
            value: sp(Expr::Int(99)),
        })]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(materialize(result_thunk, None, 0).unwrap(), Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_document_scope_chain_plus_letrec() {
        // Expr 1: [x: 1]
        // Expr 2: [y: $x  z: $y]
        // y references x from the scope chain, z references y via letrec.
        // Verify z resolves to 1.
        let expr1 = sp(Expr::Dict(vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::Int(1)),
        })]));
        let expr2 = sp(Expr::Dict(vec![
            sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::VarRef("x".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("z".into()))),
                value: sp(Expr::VarRef("y".into())),
            }),
        ]));
        let doc = sp(Document {
            expressions: vec![expr1, expr2],
        });
        let thunk = eval_document(&doc, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let z_thunk = map.get(&Key::String("z".into())).unwrap();
                assert_eq!(materialize(z_thunk, None, 0).unwrap(), Value::Int(1));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    // --- File Evaluation ---

    #[test]
    fn test_eval_file_single_document() {
        // A file with one document containing [x: 1]. Verify x=1.
        let doc = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(1)),
            })]))],
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                assert_eq!(map.len(), 1);
                assert_eq!(
                    materialize(map.get(&Key::String("x".into())).unwrap(), None, 0).unwrap(),
                    Value::Int(1)
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_dollar_dollar_is_empty_for_first_doc() {
        // A file with one document containing [prev: $$].
        // $$ is VarRef("$"), should resolve to empty dict for first doc.
        let doc = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("prev".into()))),
                value: sp(Expr::VarRef("$".into())),
            })]))],
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let prev_thunk = map.get(&Key::String("prev".into())).unwrap();
                let prev_val = materialize(prev_thunk, None, 0).unwrap();
                match prev_val {
                    Value::Dict(inner) => assert_eq!(inner.len(), 0),
                    other => panic!("expected empty Dict for $$, got {other:?}"),
                }
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_dollar_dollar_pipeline() {
        // Doc 1: [x: 10]
        // Doc 2: [y: $$.x]  (access previous doc's x via $$)
        // Verify y=10.
        let doc1 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(10)),
            })]))],
        });
        let doc2 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::VarRef("$".into()))),
                    field: "x".into(),
                }),
            })]))],
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                assert_eq!(materialize(y_thunk, None, 0).unwrap(), Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_non_dict_dollar_dollar() {
        // Doc 1: 42 (a bare Int, not a dict)
        // Doc 2: [prev: $$]
        // Verify that prev resolves to Int(42).
        let doc1 = sp(Document {
            expressions: vec![sp(Expr::Int(42))],
        });
        let doc2 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("prev".into()))),
                value: sp(Expr::VarRef("$".into())),
            })]))],
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let prev_thunk = map.get(&Key::String("prev".into())).unwrap();
                assert_eq!(materialize(prev_thunk, None, 0).unwrap(), Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_dollar_dollar_lazy() {
        // Verify that $$ is lazy: Doc 1 contains a value that would error if
        // materialized. Doc 2 accesses a DIFFERENT key from $$, so the error
        // value is never forced.
        // Doc 1: [good: 1  bad: $missing]
        // Doc 2: [result: $$.good]
        // Verify result=1.
        let doc1 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![
                sp(Entry {
                    key: Some(sp(Expr::Str("good".into()))),
                    value: sp(Expr::Int(1)),
                }),
                sp(Entry {
                    key: Some(sp(Expr::Str("bad".into()))),
                    value: sp(Expr::VarRef("missing".into())),
                }),
            ]))],
        });
        let doc2 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("result".into()))),
                value: sp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::VarRef("$".into()))),
                    field: "good".into(),
                }),
            })]))],
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(materialize(result_thunk, None, 0).unwrap(), Value::Int(1));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_three_documents() {
        // Three documents piped:
        // Doc 1: [a: 1]
        // Doc 2: [b: $$.a  c: 2]
        // Doc 3: [result: $$.b]
        // Verify result=1 (piped through two boundaries).
        let doc1 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("a".into()))),
                value: sp(Expr::Int(1)),
            })]))],
        });
        let doc2 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![
                sp(Entry {
                    key: Some(sp(Expr::Str("b".into()))),
                    value: sp(Expr::DotAccess {
                        expr: Box::new(sp(Expr::VarRef("$".into()))),
                        field: "a".into(),
                    }),
                }),
                sp(Entry {
                    key: Some(sp(Expr::Str("c".into()))),
                    value: sp(Expr::Int(2)),
                }),
            ]))],
        });
        let doc3 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("result".into()))),
                value: sp(Expr::DotAccess {
                    expr: Box::new(sp(Expr::VarRef("$".into()))),
                    field: "b".into(),
                }),
            })]))],
        });
        let file = File {
            documents: vec![doc1, doc2, doc3],
        };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let result_thunk = map.get(&Key::String("result".into())).unwrap();
                assert_eq!(materialize(result_thunk, None, 0).unwrap(), Value::Int(1));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_documents_isolated() {
        // Verify documents don't share scope:
        // Doc 1: [x: 42]
        // Doc 2: [y: $x]  (NOT $$.x, just $x -- should fail)
        let doc1 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("x".into()))),
                value: sp(Expr::Int(42)),
            })]))],
        });
        let doc2 = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("y".into()))),
                value: sp(Expr::VarRef("x".into())),
            })]))],
        });
        let file = File {
            documents: vec![doc1, doc2],
        };
        // eval_file succeeds (dict is lazy), but materializing y should fail
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let y_thunk = map.get(&Key::String("y".into())).unwrap();
                let err = materialize(y_thunk, None, 0).unwrap_err();
                assert!(
                    err.message.contains("undefined variable: $x"),
                    "got: {}",
                    err.message
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_empty() {
        // A file with zero documents. Should return an empty dict.
        let file = File { documents: vec![] };
        let thunk = eval_file(&file, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => assert_eq!(map.len(), 0),
            other => panic!("expected empty Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_eval_file_inherits_env() {
        // A file evaluated with a pre-populated parent env.
        // Document expressions should see the parent's bindings.
        let parent_env = empty_env();
        parent_env.borrow_mut().insert(
            "external".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(777),
                test_span(1, 1, 1, 5),
            )),
        );

        let doc = sp(Document {
            expressions: vec![sp(Expr::Dict(vec![sp(Entry {
                key: Some(sp(Expr::Str("val".into()))),
                value: sp(Expr::VarRef("external".into())),
            })]))],
        });
        let file = File {
            documents: vec![doc],
        };
        let thunk = eval_file(&file, parent_env, 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        match val {
            Value::Dict(map) => {
                let val_thunk = map.get(&Key::String("val".into())).unwrap();
                assert_eq!(materialize(val_thunk, None, 0).unwrap(), Value::Int(777));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }
}
