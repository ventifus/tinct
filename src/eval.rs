//! Core evaluation module: lazy evaluation with letrec dict scoping, document
//! pipelines, function evaluation, and `$_` implicit lambda desugaring.

use std::borrow::Cow;
use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::{Annotation, Document, Entry, Expr, File, NamedArg, Param, Span, Spanned};
use crate::error::{EvalError, EvalResult};
// Circular module dependency: this module calls builtins via function pointers stored in `Value::Builtin`.
// builtins.rs imports `invoke_function` and `materialize` from this module.
// This bidirectional dependency is safe because neither module's initialization depends on the other.
use crate::value::{Environment, Key, Thunk, ThunkState, Value};

/// Maximum evaluation depth (256). Limits nesting of eval/materialize calls to prevent stack overflow.
pub const MAX_EVAL_DEPTH: usize = 256;
/// Shared error message for range access with mixed key types (used in `key_in_range`).
const RANGE_KEY_TYPE_ERROR: &str = "range access requires comparable key types";
const DEFAULT_ANNOTATION_KEY: &str = "default";

/// Check whether `k` falls in the half-open range `[start, end)`.
/// `None` bounds are treated as unbounded (i.e. negative/positive infinity).
/// Returns an error when `k` is not comparable with the bound (mixed key types).
fn key_in_range(k: &Key, start: Option<&Key>, end: Option<&Key>, span: Span) -> EvalResult<bool> {
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
) -> EvalResult<Rc<Thunk>> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum evaluation depth exceeded ({MAX_EVAL_DEPTH})"),
            expr.span,
        )
        .into());
    }
    // $_ implicit lambda desugaring: if the expression directly contains VarRef("_")
    // and `_` is not already bound in the current environment, wrap it in [fn [_] <expr>].
    if should_desugar_underscore(&expr.node) && env.borrow().get("_").is_none() {
        let lambda = wrap_in_lambda(expr);
        return eval(&lambda, env, depth + 1);
    }

    match &expr.node {
        // Literals and closures are already computed values, so we wrap them in
        // immediately-materialized thunks instead of Unevaluated thunks. This avoids
        // the overhead of wrapping, then unwrapping, then re-evaluating on first access.
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
        Expr::TypeAssert {
            expr: inner,
            annotation,
        } => {
            let thunk = eval(inner, Rc::clone(&env), depth + 1)?;
            let value = materialize(&thunk, Some(&expr.span), depth + 1)?;

            let expected_type = match &annotation.node {
                Annotation::Simple(name) => Some(name.as_str()),
                Annotation::PropertyDict(_) => {
                    annotation.node.get_property("type").and_then(|type_expr| {
                        match &type_expr.node {
                            Expr::Str(s) => Some(s.as_str()),
                            _ => None,
                        }
                    })
                }
            };

            if let Some(expected) = expected_type {
                let actual = value.type_name();
                let matches = if expected == "Number" {
                    actual == "Int" || actual == "Float"
                } else {
                    actual == expected
                };
                if !matches {
                    if let Some(default_expr) = annotation.node.get_property(DEFAULT_ANNOTATION_KEY)
                    {
                        return eval(default_expr, env, depth + 1);
                    }
                    return Err(EvalError::new(
                        format!("type assertion failed: expected {expected}, got {actual}"),
                        expr.span,
                    )
                    .into());
                }
            }

            Ok(Rc::new(Thunk::new_materialized(value, expr.span)))
        }
        Expr::Annotated { name, .. } => {
            // Evaluate as the bare string; the type checker (typecheck.rs) interprets annotations.
            Ok(Rc::new(Thunk::new_materialized(
                Value::String(name.clone()),
                expr.span,
            )))
        }
        Expr::Fn { params, body, .. } => {
            let fn_params: Vec<Param> = params.iter().map(|p| p.node.clone()).collect();
            Ok(Rc::new(Thunk::new_materialized(
                Value::Function {
                    params: Rc::new(fn_params),
                    body: Rc::new(body.as_ref().clone()),
                    env: Rc::clone(&env),
                },
                expr.span,
            )))
        }
        Expr::Call {
            func,
            args,
            named_args,
        } => eval_call(func, args, named_args, &env, &expr.span, depth),
        Expr::TypeAlias(_inner) => Ok(Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            expr.span,
        ))),
        Expr::Rest(_) => Err(EvalError::new(
            "rest marker (...) is only valid inside type expressions",
            expr.span,
        )
        .into()),
    }
}

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
) -> EvalResult<Rc<Thunk>> {
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
///
/// **Note:** If your LLT code uses `$include`, you must call [`crate::set_include_context`]
/// before calling this function and [`crate::clear_include_context`] after it returns.
pub fn eval_file(
    file: &File,
    env: Rc<RefCell<Environment>>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    eval_file_with_input(file, env, None, depth)
}

/// Evaluate a parsed [`File`], optionally injecting an initial `$$` value for the first document.
///
/// When `initial_input` is `Some(thunk)`, that thunk becomes `$$` for the first
/// document instead of the default empty dict. This supports the CLI's stdin
/// JSON injection: `cat data.json | llt eval file.llt`.
///
/// **Note:** If your LLT code uses `$include`, you must call [`crate::set_include_context`]
/// before calling this function and [`crate::clear_include_context`] after it returns.
pub fn eval_file_with_input(
    file: &File,
    env: Rc<RefCell<Environment>>,
    initial_input: Option<Rc<Thunk>>,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    // $$ starts as the provided input, or empty dict if none given
    let mut prev_output = initial_input.unwrap_or_else(|| {
        Rc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            Span::origin(),
        ))
    });

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

fn eval_dict(
    entries: &[Spanned<Entry>],
    parent_env: &Rc<RefCell<Environment>>,
    dict_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
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
                // Overflow unreachable: memory exhaustion prevents a single dict from reaching i64::MAX entries.
                auto_index += 1;
                k
            }
        };

        if dict_map.contains_key(&key) {
            return Err(EvalError::new(format!("duplicate key: {key}"), entry.span).into());
        }

        let thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(entry.node.value.clone()),
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

fn eval_key(
    key_expr: &Spanned<Expr>,
    parent_env: &Rc<RefCell<Environment>>,
    depth: usize,
) -> EvalResult<Key> {
    // Fast path for literal keys (avoids creating temporary thunks)
    match &key_expr.node {
        Expr::Str(s) => return Ok(Key::String(s.clone())),
        Expr::Int(n) => return Ok(Key::Int(*n)),
        _ => {}
    }
    // General path: must materialize because IndexMap requires concrete Key values
    let thunk = eval(key_expr, Rc::clone(parent_env), depth + 1)?;
    let value = materialize(&thunk, Some(&key_expr.span), depth + 1)?;
    value_to_key(&value, &key_expr.span)
}

fn value_to_key(value: &Value, span: &Span) -> EvalResult<Key> {
    match value {
        Value::String(s) => Ok(Key::String(s.clone())),
        Value::Int(n) => Ok(Key::Int(*n)),
        _ => Err(EvalError::type_mismatch("String or Int", value.type_name(), *span).into()),
    }
}

/// Extract a human-readable label from a function expression for stack frames.
fn func_label(expr: &Expr) -> Cow<'static, str> {
    Cow::Owned(format!("call {}", func_path(expr)))
}

fn func_path(expr: &Expr) -> String {
    match expr {
        Expr::VarRef(name) => format!("${name}"),
        Expr::DotAccess { expr: inner, field } => format!("{}.{field}", func_path(&inner.node)),
        _ => "<anonymous>".to_string(),
    }
}

/// Evaluate a call expression: materialize the function, bind arguments, wrap body as thunk.
fn eval_call(
    func_expr: &Spanned<Expr>,
    args: &[Spanned<Expr>],
    named_args: &[Spanned<NamedArg>],
    env: &Rc<RefCell<Environment>>,
    call_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    // Evaluate and materialize the function
    let func_thunk = eval(func_expr, Rc::clone(env), depth + 1)?;
    let func_val = materialize(&func_thunk, Some(call_span), depth + 1)?;

    // Wrap arguments as unevaluated thunks (lazy). This ensures expressions
    // like $xs[$i] in unselected $if branches are never evaluated.
    // For builtins, these thunks are wrapped again in PendingBuiltin (the builtin
    // call is deferred until its result is materialized). For LLT functions,
    // invoke_function binds these thunks lazily in the closure environment.
    let pos_thunks: Vec<Rc<Thunk>> = args
        .iter()
        .map(|arg| {
            Rc::new(Thunk::new_unevaluated(
                Rc::new((*arg).clone()),
                Rc::clone(env),
                arg.span,
            ))
        })
        .collect();
    let mut named_thunks = IndexMap::new();
    for na in named_args {
        named_thunks.insert(
            na.node.name.clone(),
            Rc::new(Thunk::new_unevaluated(
                Rc::new(na.node.value.clone()),
                Rc::clone(env),
                na.node.value.span,
            )),
        );
    }

    let label = func_label(&func_expr.node);

    match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => invoke_function(&CallContext {
            params: &params,
            body: &body,
            closure_env: &closure_env,
            positional: &pos_thunks,
            named: &named_thunks,
            default_env: env,
            call_span: *call_span,
            depth,
            origin: label.clone(),
        })
        .map_err(|mut e| {
            e.push_frame(label, *call_span);
            e
        }),
        Value::Builtin { func, .. } => Ok(Rc::new(Thunk::new_pending_builtin(
            func,
            pos_thunks,
            named_thunks,
            depth + 1,
            *call_span,
            label,
        ))),
        _ => Err(EvalError::type_mismatch("Function", func_val.type_name(), *call_span).into()),
    }
}

/// Arguments for invoking a user-defined function.
///
/// `default_env` is the environment used to evaluate default expressions for
/// optional params. For normal calls this is the caller's environment; for
/// `apply` it is the closure environment.
pub struct CallContext<'a> {
    pub params: &'a [Param],
    pub body: &'a Rc<Spanned<Expr>>,
    pub closure_env: &'a Rc<RefCell<Environment>>,
    pub positional: &'a [Rc<Thunk>],
    pub named: &'a IndexMap<String, Rc<Thunk>>,
    pub default_env: &'a Rc<RefCell<Environment>>,
    pub call_span: Span,
    pub depth: usize,
    /// Label for stack traces (e.g. "call $f"). Set by `eval_call`
    /// when the function expression has a recognizable name.
    pub origin: Cow<'static, str>,
}

/// Invoke a user-defined function with pre-evaluated thunks.
///
/// Binds positional and named args to function params (respecting defaults and
/// variadics), then wraps the body as an unevaluated thunk. This is the shared
/// call path for both `eval_call` and `builtin_apply`.
pub fn invoke_function(ctx: &CallContext) -> EvalResult<Rc<Thunk>> {
    let call_env = bind_args_thunks(
        ctx.params,
        ctx.positional,
        ctx.named,
        ctx.default_env,
        ctx.closure_env,
        &ctx.call_span,
        ctx.depth,
    )?;
    let mut thunk = Thunk::new_unevaluated(Rc::clone(ctx.body), call_env, ctx.call_span);
    if !ctx.origin.is_empty() {
        thunk = thunk.with_origin(ctx.origin.clone());
    }
    Ok(Rc::new(thunk))
}

/// Bind pre-evaluated thunks to function parameters. Returns the new call environment.
///
/// Handles positional args, named args (params with `default:` annotation),
/// and variadic params (`...name`).
fn bind_args_thunks(
    params: &[Param],
    positional: &[Rc<Thunk>],
    named: &IndexMap<String, Rc<Thunk>>,
    default_env: &Rc<RefCell<Environment>>,
    closure_env: &Rc<RefCell<Environment>>,
    call_span: &Span,
    depth: usize,
) -> EvalResult<Rc<RefCell<Environment>>> {
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

    // Both variadic and non-variadic require at least required_count positional args
    if positional.len() < required_count {
        return Err(EvalError::arity_mismatch(required_count, positional.len(), *call_span).into());
    }

    // Without variadic: positional args must not exceed max_positional
    if variadic_param.is_none() && positional.len() > max_positional {
        return Err(EvalError::arity_mismatch(max_positional, positional.len(), *call_span).into());
    }

    // Bind positional args to regular params
    for (i, param) in regular_params.iter().enumerate() {
        let thunk = if i < positional.len() {
            // Positional arg provided
            Rc::clone(&positional[i])
        } else if let Some(default_val) = get_default(param) {
            // Check if a named arg was provided for this param
            if let Some(named_thunk) = named.get(&param.name) {
                Rc::clone(named_thunk)
            } else {
                // Use default
                eval(&default_val, Rc::clone(default_env), depth + 1)?
            }
        } else {
            // This shouldn't happen due to arity check above
            return Err(
                EvalError::arity_mismatch(required_count, positional.len(), *call_span).into(),
            );
        };
        call_env.borrow_mut().insert(param.name.clone(), thunk);
    }

    // Check for named args that target params already bound positionally
    for (name, _) in named {
        if let Some(idx) = regular_params.iter().position(|p| &p.name == name) {
            if idx < positional.len() {
                return Err(EvalError::new(
                    format!(
                        "parameter '{}' received both positional and named argument",
                        name
                    ),
                    *call_span,
                )
                .into());
            }
        }
    }

    // Handle named args that weren't consumed by positional binding
    for (name, thunk) in named {
        let already_bound = call_env.borrow().bindings.contains_key(name);
        if !already_bound {
            // Check that the named arg corresponds to a param with default:
            let is_valid_param = regular_params
                .iter()
                .any(|p| &p.name == name && get_default(p).is_some());
            if !is_valid_param {
                return Err(EvalError::new(
                    format!("unexpected named argument: {}", name),
                    *call_span,
                )
                .into());
            }
            call_env.borrow_mut().insert(name.clone(), Rc::clone(thunk));
        }
    }

    // Bind variadic param: collect remaining positional args into a dict with int keys
    if let Some(var_param) = variadic_param {
        let mut var_map: IndexMap<Key, Rc<Thunk>> = IndexMap::new();
        for (i, thunk) in positional.iter().enumerate().skip(max_positional) {
            var_map.insert(
                Key::Int(i64::try_from(i - max_positional).expect("collection too large")),
                Rc::clone(thunk),
            );
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

/// Evaluate a target expression, materialize, and return the inner IndexMap if
/// it's a Dict, otherwise return a type-mismatch error. Shared by all access
/// chain functions (dot, bracket, range).
fn eval_as_dict(
    target: &Spanned<Expr>,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> EvalResult<IndexMap<Key, Rc<Thunk>>> {
    // Must materialize target to obtain IndexMap for key lookup
    let target_thunk = eval(target, Rc::clone(env), depth + 1)?;
    let target_val = materialize(&target_thunk, Some(access_span), depth + 1)?;
    match target_val {
        Value::Dict(map) => Ok(map),
        _ => Err(EvalError::type_mismatch("Dict", target_val.type_name(), *access_span).into()),
    }
}

/// DotAccess: materialize target, look up string key in dict.
fn eval_dot_access(
    target: &Spanned<Expr>,
    field: &str,
    env: &Rc<RefCell<Environment>>,
    access_span: &Span,
    depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let map = eval_as_dict(target, env, access_span, depth).map_err(|mut e| {
        e.push_frame(format!("accessing .{field}"), *access_span);
        e
    })?;
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
) -> EvalResult<Rc<Thunk>> {
    let map = eval_as_dict(target, env, access_span, depth).map_err(|mut e| {
        e.push_frame("accessing [..]", *access_span);
        e
    })?;
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
) -> EvalResult<Rc<Thunk>> {
    let map = eval_as_dict(target, env, access_span, depth).map_err(|mut e| {
        e.push_frame("accessing [..:..]", *access_span);
        e
    })?;
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

/// Decorate an EvalError with materialization context (span and stack frames).
///
/// Attaches the materialization-site span if provided, and adds stack frames
/// for the materialization context and the thunk's origin label. Avoids duplicate
/// frames by checking if the span is already in the stack.
fn attach_materialization_context(
    mut err: Box<EvalError>,
    mat_span: Option<&Span>,
    origin: &str,
    thunk_span: Span,
) -> Box<EvalError> {
    if let Some(span) = mat_span {
        if err.materialization_span.is_none() {
            err.materialization_span = Some(*span);
        } else if err.materialization_span != Some(*span)
            && !err.stack.iter().any(|f| f.span == *span)
        {
            // Only push a frame if the span differs from the existing
            // materialization span and isn't already in the stack (avoids
            // duplicate frames when the same span propagates through
            // nested materialize calls).
            err.push_frame("materialized", *span);
        }
    }
    if !origin.is_empty()
        && !err
            .stack
            .iter()
            .any(|f| f.span == thunk_span && f.label == origin)
    {
        err.push_frame(origin, thunk_span);
    }
    err
}

/// Force a thunk to its concrete value, memoizing the result.
///
/// On first materialization, evaluates the thunk and caches the result (or error).
/// Subsequent calls return the cached value without re-evaluation. This implements
/// call-by-need semantics: lazy evaluation with sharing.
///
/// # ThunkState transitions
///
/// - `Materialized`: returns cached value immediately
/// - `Failed`: returns cached error (with updated materialization_span)
/// - `InProgress`: returns circular dependency error
/// - `Unevaluated`: evaluates expr in env, memoizes result or error
/// - `PendingBuiltin`: calls builtin with args, memoizes result or error
/// - `PendingCall`: materializes func, invokes it with args, memoizes result or error
///
/// # Side effects
///
/// Mutates the thunk's internal state via `RefCell`. On success, transitions to
/// `Materialized`. On failure, transitions to `Failed` (caching the error).
///
/// `mat_span` is the span of the expression that triggered materialization
/// (e.g., an access chain). Attached to errors so users can see both where
/// a value was defined and where it was forced.
pub fn materialize(thunk: &Thunk, mat_span: Option<&Span>, depth: usize) -> EvalResult<Value> {
    if depth > MAX_EVAL_DEPTH {
        let mut err = EvalError::depth_exceeded(MAX_EVAL_DEPTH, thunk.span);
        if let Some(span) = mat_span {
            err = err.with_materialization_span(*span);
        }
        return Err(err.into());
    }

    // Read origin before checking state (InProgress may not preserve it)
    let origin = thunk.origin.clone();
    let thunk_span = thunk.span;

    {
        let state = thunk.state();
        match &*state {
            ThunkState::Materialized(v) => return Ok(v.clone()),
            ThunkState::Failed(ref err) => {
                let mut cloned = (**err).clone();
                let mut should_update_cache = false;
                if let Some(span) = mat_span {
                    if cloned.materialization_span.is_none() {
                        // First access via Failed path (edge case: error cached without mat_span)
                        cloned.materialization_span = Some(*span);
                        should_update_cache = true;
                    } else if cloned.materialization_span != Some(*span)
                        && !cloned.stack.iter().any(|f| f.span == *span)
                    {
                        // Different access site: add as stack frame, preserve original mat_span
                        cloned.push_frame("materialized", *span);
                        should_update_cache = true;
                    }
                }
                // Update cached error if we modified it
                if should_update_cache {
                    drop(state);
                    thunk.set_state(ThunkState::Failed(Box::new(cloned.clone())));
                }
                return Err(Box::new(cloned));
            }
            ThunkState::InProgress => {
                let label = if origin.is_empty() { "thunk" } else { &origin };
                let mut err = EvalError::circular_dependency(label, thunk.span);
                if let Some(span) = mat_span {
                    err = err.with_materialization_span(*span);
                }
                let err_boxed: Box<EvalError> = err.into();
                drop(state);
                thunk.cache_failure(&err_boxed);
                return Err(err_boxed);
            }
            ThunkState::Unevaluated { .. }
            | ThunkState::PendingBuiltin { .. }
            | ThunkState::PendingCall { .. } => {}
        }
    }

    let decorate = |e| attach_materialization_context(e, mat_span, &origin, thunk_span);

    if let Some((expr, env)) = thunk.take_unevaluated() {
        let result = eval(&expr, Rc::clone(&env), depth + 1)
            .and_then(|result_thunk| materialize(&result_thunk, mat_span, depth + 1))
            .map_err(&decorate);

        match result {
            Ok(value) => {
                thunk.set_state(ThunkState::Materialized(value.clone()));
                Ok(value)
            }
            Err(e) => {
                thunk.cache_failure(&e);
                Err(e)
            }
        }
    } else if let Some((func, args, named, pending_depth, call_span)) = thunk.take_pending_builtin()
    {
        let builtin_args = crate::value::BuiltinArgs {
            args: &args,
            named: &named,
            depth: pending_depth,
            call_span,
        };
        match func(builtin_args).map_err(&decorate) {
            Ok(result_thunk) => {
                // Fast path: if the builtin already materialized its result, skip recursion.
                if let Some(value) = result_thunk.try_get_materialized() {
                    thunk.set_state(ThunkState::Materialized(value.clone()));
                    Ok(value)
                } else {
                    match materialize(&result_thunk, mat_span, depth + 1).map_err(&decorate) {
                        Ok(value) => {
                            thunk.set_state(ThunkState::Materialized(value.clone()));
                            Ok(value)
                        }
                        Err(e) => {
                            thunk.cache_failure(&e);
                            Err(e)
                        }
                    }
                }
            }
            Err(e) => {
                thunk.cache_failure(&e);
                Err(e)
            }
        }
    } else if let Some((func_thunk, args, named, call_span)) = thunk.take_pending_call() {
        // Materialize the function thunk to determine if it's a Function or Builtin
        let func_value =
            match materialize(&func_thunk, Some(&call_span), depth + 1).map_err(&decorate) {
                Ok(v) => v,
                Err(e) => {
                    thunk.cache_failure(&e);
                    return Err(e);
                }
            };

        match func_value {
            Value::Function { params, body, env } => {
                // Build CallContext and invoke the function
                let ctx = CallContext {
                    params: &params,
                    body: &body,
                    closure_env: &env,
                    positional: &args,
                    named: &named,
                    default_env: &env, // Use closure env for defaults
                    call_span,
                    depth,
                    origin: origin.clone(),
                };

                match invoke_function(&ctx).map_err(&decorate) {
                    Ok(result_thunk) => {
                        // Materialize the result and memoize
                        match materialize(&result_thunk, mat_span, depth + 1).map_err(&decorate) {
                            Ok(value) => {
                                thunk.set_state(ThunkState::Materialized(value.clone()));
                                Ok(value)
                            }
                            Err(e) => {
                                thunk.cache_failure(&e);
                                Err(e)
                            }
                        }
                    }
                    Err(e) => {
                        thunk.cache_failure(&e);
                        Err(e)
                    }
                }
            }
            Value::Builtin { func, .. } => {
                let builtin_args = crate::value::BuiltinArgs {
                    args: &args,
                    named: &named,
                    depth,
                    call_span,
                };
                match func(builtin_args).map_err(&decorate) {
                    Ok(result_thunk) => {
                        if let Some(value) = result_thunk.try_get_materialized() {
                            thunk.set_state(ThunkState::Materialized(value.clone()));
                            Ok(value)
                        } else {
                            match materialize(&result_thunk, mat_span, depth + 1).map_err(&decorate)
                            {
                                Ok(value) => {
                                    thunk.set_state(ThunkState::Materialized(value.clone()));
                                    Ok(value)
                                }
                                Err(e) => {
                                    thunk.cache_failure(&e);
                                    Err(e)
                                }
                            }
                        }
                    }
                    Err(e) => {
                        thunk.cache_failure(&e);
                        Err(e)
                    }
                }
            }
            other => {
                let err =
                    EvalError::type_mismatch("Function or Builtin", other.type_name(), call_span);
                let decorated = decorate(Box::new(err));
                thunk.cache_failure(&decorated);
                Err(decorated)
            }
        }
    } else {
        unreachable!("state must be Unevaluated, PendingBuiltin, or PendingCall (Materialized returns early, Failed returns early, InProgress returns early)")
    }
}

/// Recursively force all thunks in a value tree.
///
/// - Primitives (Int, Float, String, Bool) are returned as-is.
/// - Dict values are fully materialized: each thunk entry is forced via
///   [`materialize`], then deep-materialized recursively. The returned Dict
///   wraps every value as [`Thunk::new_materialized`].
/// - Seq values are fully materialized: both head and tail thunks are forced
///   and recursively deep-materialized.
/// - Functions (user-defined and builtins) are returned as-is -- they are
///   opaque values, not collections to traverse.
///
/// `depth` is checked against [`MAX_EVAL_DEPTH`] to prevent stack overflow on
/// deeply nested structures. Cycle detection is performed using a visited set
/// to prevent infinite loops on cyclic structures (e.g., mutual dict references).
pub fn deep_materialize(val: &Value, depth: usize) -> EvalResult<Value> {
    use std::collections::HashSet;
    let mut visited = HashSet::new();
    deep_materialize_impl(val, depth, &mut visited)
}

fn deep_materialize_impl(
    val: &Value,
    depth: usize,
    visited: &mut std::collections::HashSet<*const Thunk>,
) -> EvalResult<Value> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::new(
            format!("maximum evaluation depth exceeded ({MAX_EVAL_DEPTH})"),
            Span::origin(),
        )
        .into());
    }
    match val {
        Value::Dict(map) => {
            let mut result = IndexMap::new();
            for (key, thunk) in map {
                // Check for cycles using raw pointer identity
                let thunk_ptr = Rc::as_ptr(thunk);
                if visited.contains(&thunk_ptr) {
                    // Cycle detected: return thunk as-is without deep forcing
                    result.insert(key.clone(), Rc::clone(thunk));
                    continue;
                }
                visited.insert(thunk_ptr);
                let v = materialize(thunk, None, depth)?;
                let forced = deep_materialize_impl(&v, depth + 1, visited)?;
                result.insert(
                    key.clone(),
                    Rc::new(Thunk::new_materialized(forced, thunk.span)),
                );
            }
            Ok(Value::Dict(result))
        }
        Value::Seq { head, tail } => {
            // Materialize and deep-force head
            let head_ptr = Rc::as_ptr(head);
            if visited.contains(&head_ptr) {
                // Cycle in head: keep as-is
                return Ok(Value::Seq {
                    head: Rc::clone(head),
                    tail: Rc::clone(tail),
                });
            }
            visited.insert(head_ptr);
            let head_val = materialize(head, None, depth)?;
            let head_forced = deep_materialize_impl(&head_val, depth + 1, visited)?;

            // Materialize and deep-force tail
            let tail_ptr = Rc::as_ptr(tail);
            if visited.contains(&tail_ptr) {
                // Cycle in tail: keep as-is
                return Ok(Value::Seq {
                    head: Rc::new(Thunk::new_materialized(head_forced, head.span)),
                    tail: Rc::clone(tail),
                });
            }
            visited.insert(tail_ptr);
            let tail_val = materialize(tail, None, depth)?;
            let tail_forced = deep_materialize_impl(&tail_val, depth + 1, visited)?;

            Ok(Value::Seq {
                head: Rc::new(Thunk::new_materialized(head_forced, head.span)),
                tail: Rc::new(Thunk::new_materialized(tail_forced, tail.span)),
            })
        }
        // Primitives and functions are already fully materialized
        other => Ok(other.clone()),
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
            err.message().contains("undefined variable: $missing"),
            "got: {}",
            err.message()
        );
    }

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
                    err.message().contains("circular dependency"),
                    "got: {}",
                    err.message()
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_cycle_detection_transitions_to_failed() {
        // When a thunk detects a circular dependency (InProgress state),
        // it should cache the error in Failed state, not leave it in InProgress.
        // Subsequent materializations should return the cached error.
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("x".into())),
        })];
        let expr = sp(Expr::Dict(entries));
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();

        let x_thunk = match val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should detect the cycle and fail
        let err1 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err1.message().contains("circular dependency"),
            "first error: got: {}",
            err1.message()
        );

        // Check that the thunk is now in Failed state, not stuck in InProgress
        match &*x_thunk.state() {
            ThunkState::Failed(cached_err) => {
                assert!(
                    cached_err.message().contains("circular dependency"),
                    "cached error should mention circular dependency, got: {}",
                    cached_err.message()
                );
            }
            other => panic!("expected Failed state after cycle detection, got {other:?}"),
        }

        // Second materialization: should return the cached circular dependency error
        let err2 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err2.message().contains("circular dependency"),
            "second error: got: {}",
            err2.message()
        );
    }

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
            err1.message().contains("undefined variable: $missing"),
            "first attempt: got: {}",
            err1.message()
        );

        // Second attempt: should produce the SAME error, not "circular dependency"
        let err2 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err2.message().contains("undefined variable: $missing"),
            "second attempt should not be poisoned, got: {}",
            err2.message()
        );
        assert!(
            !err2.message().contains("circular dependency"),
            "thunk was poisoned: got circular dependency on retry"
        );
    }

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
            err.message().contains("duplicate key: x"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("Function"), "got: {}", err.message());
    }

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
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
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
            err.message().contains("arity mismatch"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("unexpected named argument: z"),
            "got: {}",
            err.message()
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
            err.message()
                .contains("received both positional and named argument"),
            "got: {}",
            err.message()
        );
    }

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

    #[test]
    fn test_call_builtin() {
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
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

    #[test]
    fn test_rest_marker_anonymous_errors() {
        let expr = sp(Expr::Rest(None));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_rest_marker_named_errors() {
        let expr = sp(Expr::Rest(Some("x".into())));
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("rest marker (...) is only valid inside type expressions"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("undefined variable: $_"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("key not found: missing"),
            "got: {}",
            err.message()
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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("key not found: z"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("comparable key types"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_int_passes() {
        // [@Int 42] -> 42
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_string_passes() {
        // [@String hello] -> "hello"
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_type_assert_number_accepts_int() {
        // [@Number 42] -> 42 (Number accepts Int)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_number_accepts_float() {
        // [@Number 3.14] -> 3.14 (Number accepts Float)
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Number".into())),
            expr: Box::new(sp(Expr::Float(3.14))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Float(3.14));
    }

    #[test]
    fn test_type_assert_int_fails_on_string() {
        // [@Int hello] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Int".into())),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_string_fails_on_int() {
        // [@String 42] -> error
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("String".into())),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected String, got Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_bool_passes() {
        // [@Bool true] -> true
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::Simple("Bool".into())),
            expr: Box::new(sp(Expr::Bool(true))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Bool(true));
    }

    #[test]
    fn test_type_assert_property_dict_with_type() {
        // [@[type: Int] 42] -> 42
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: sp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_property_dict_type_mismatch() {
        // [@[type: Int] hello] -> error
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: sp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_property_dict_without_type_passes() {
        // [@[default: 0] hello] -> "hello" (no type key, no check performed)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("default".into()))),
            value: sp(Expr::Int(0)),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::String("hello".into()));
    }

    #[test]
    fn test_type_assert_default_not_used_on_match() {
        // [@[type: Int  default: 0] 42] -> 42 (type matches, default not used)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: sp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));
    }

    #[test]
    fn test_type_assert_default_used_on_mismatch() {
        // [@[type: Int  default: 0] hello] -> 0 (type mismatch, returns default)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: sp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(0)),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let thunk = eval(&expr, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(0));
    }

    #[test]
    fn test_type_assert_property_dict_no_default_errors_on_mismatch() {
        // [@[type: Int] hello] -> error (no default, mismatch is an error)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("type".into()))),
            value: sp(Expr::Str("Int".into())),
        })];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let err = eval(&expr, empty_env(), 0).unwrap_err();
        assert!(
            err.message()
                .contains("type assertion failed: expected Int, got String"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_type_assert_number_default_int_passes_string_triggers() {
        // [@[type: Number  default: -1] 42] -> 42 (Int passes Number check)
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: sp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(-1)),
            }),
        ];
        let expr_pass = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Int(42))),
        });
        let thunk = eval(&expr_pass, empty_env(), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(42));

        // [@[type: Number  default: -1] "nope"] -> -1 (String fails Number, returns default)
        let entries2 = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: sp(Expr::Str("Number".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::Int(-1)),
            }),
        ];
        let expr_fail = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries2)),
            expr: Box::new(sp(Expr::Str("nope".into()))),
        });
        let thunk2 = eval(&expr_fail, empty_env(), 0).unwrap();
        let val2 = materialize(&thunk2, None, 0).unwrap();
        assert_eq!(val2, Value::Int(-1));
    }

    #[test]
    fn test_type_assert_default_accesses_outer_scope() {
        // [@[type: Int  default: $fallback] hello] with fallback=99 -> 99
        let entries = vec![
            sp(Entry {
                key: Some(sp(Expr::Str("type".into()))),
                value: sp(Expr::Str("Int".into())),
            }),
            sp(Entry {
                key: Some(sp(Expr::Str("default".into()))),
                value: sp(Expr::VarRef("fallback".into())),
            }),
        ];
        let expr = sp(Expr::TypeAssert {
            annotation: sp(Annotation::PropertyDict(entries)),
            expr: Box::new(sp(Expr::Str("hello".into()))),
        });
        let env = empty_env();
        env.borrow_mut().insert(
            "fallback".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(99),
                test_span(1, 1, 1, 1),
            )),
        );
        let thunk = eval(&expr, Rc::clone(&env), 0).unwrap();
        let val = materialize(&thunk, None, 0).unwrap();
        assert_eq!(val, Value::Int(99));
    }

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

    #[test]
    fn test_eval_depth_limit() {
        let expr = sp(Expr::Int(42));
        let err = eval(&expr, empty_env(), MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_materialize_depth_limit() {
        let span = test_span(1, 1, 1, 5);
        let thunk = Thunk::new_materialized(Value::Int(1), span);
        let err = materialize(&thunk, None, MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("undefined variable: $missing"),
            "got: {}",
            err.message()
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
                assert!(err.message().contains("circular dependency"));
                assert_eq!(err.materialization_span, Some(mat_span));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String or Int"),
            "got: {}",
            err.message()
        );
        assert!(err.message().contains("got Bool"), "got: {}", err.message());
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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected String or Int"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("got Float"),
            "got: {}",
            err.message()
        );
    }

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
            err.message().contains("type mismatch"),
            "got: {}",
            err.message()
        );
        assert!(
            err.message().contains("expected Dict"),
            "got: {}",
            err.message()
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
        // Expr 3: [ref_a: $a  ref_b: $b]
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
                    err.message().contains("undefined variable: $x"),
                    "got: {}",
                    err.message()
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

    #[test]
    fn test_deep_materialize_int() {
        let val = Value::Int(42);
        let result = deep_materialize(&val, 0).unwrap();
        assert_eq!(result, Value::Int(42));
    }

    #[test]
    fn test_deep_materialize_float() {
        let val = Value::Float(3.14);
        let result = deep_materialize(&val, 0).unwrap();
        assert_eq!(result, Value::Float(3.14));
    }

    #[test]
    fn test_deep_materialize_string() {
        let val = Value::String("hello".into());
        let result = deep_materialize(&val, 0).unwrap();
        assert_eq!(result, Value::String("hello".into()));
    }

    #[test]
    fn test_deep_materialize_bool() {
        let val = Value::Bool(true);
        let result = deep_materialize(&val, 0).unwrap();
        assert_eq!(result, Value::Bool(true));
    }

    #[test]
    fn test_deep_materialize_empty_dict() {
        let val = Value::Dict(IndexMap::new());
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => assert!(map.is_empty()),
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_flat_dict() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let a = materialize(&map[&Key::String("a".into())], None, 0).unwrap();
                assert_eq!(a, Value::Int(1));
                let b = materialize(&map[&Key::String("b".into())], None, 0).unwrap();
                assert_eq!(b, Value::Int(2));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_nested_dict() {
        let span = test_span(1, 1, 1, 5);
        let mut inner = IndexMap::new();
        inner.insert(
            Key::String("y".into()),
            Rc::new(Thunk::new_materialized(Value::Int(42), span)),
        );
        let mut outer = IndexMap::new();
        outer.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(inner), span)),
        );
        let val = Value::Dict(outer);
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(outer_map) => {
                let x_val = materialize(&outer_map[&Key::String("x".into())], None, 0).unwrap();
                match x_val {
                    Value::Dict(inner_map) => {
                        let y_val =
                            materialize(&inner_map[&Key::String("y".into())], None, 0).unwrap();
                        assert_eq!(y_val, Value::Int(42));
                    }
                    other => panic!("expected inner Dict, got {other:?}"),
                }
            }
            other => panic!("expected outer Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_forces_unevaluated_thunks() {
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(99), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(expr, env, span));

        let mut map = IndexMap::new();
        map.insert(Key::String("val".into()), unevaluated);
        let val = Value::Dict(map);

        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                let v = materialize(&map[&Key::String("val".into())], None, 0).unwrap();
                assert_eq!(v, Value::Int(99));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_function_passthrough() {
        let span = test_span(1, 1, 1, 5);
        let val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(Expr::Int(0), span)),
            env: Rc::new(RefCell::new(Environment::new())),
        };
        let result = deep_materialize(&val, 0).unwrap();
        // Functions are opaque -- returned as-is
        match result {
            Value::Function { params, .. } => {
                assert_eq!(params.len(), 1);
                assert_eq!(params[0].name, "x");
            }
            other => panic!("expected Function, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_builtin_passthrough() {
        fn dummy(_ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            Ok(Rc::new(Thunk::new_materialized(
                Value::Int(0),
                test_span(1, 1, 1, 1),
            )))
        }
        let val = Value::Builtin {
            name: "test",
            func: dummy,
        };
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Builtin { name, .. } => assert_eq!(name, "test"),
            other => panic!("expected Builtin, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_depth_limit() {
        let err = deep_materialize(&Value::Int(1), MAX_EVAL_DEPTH + 1).unwrap_err();
        assert!(
            err.message().contains("maximum evaluation depth exceeded"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_deep_materialize_depth_just_under() {
        // At the limit should still succeed for a leaf value
        let result = deep_materialize(&Value::Int(1), MAX_EVAL_DEPTH);
        assert!(result.is_ok());
    }

    #[test]
    fn test_deep_materialize_dict_with_int_keys() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::Int(0),
            Rc::new(Thunk::new_materialized(Value::String("zero".into()), span)),
        );
        map.insert(
            Key::Int(1),
            Rc::new(Thunk::new_materialized(Value::String("one".into()), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                assert_eq!(map.len(), 2);
                let v0 = materialize(&map[&Key::Int(0)], None, 0).unwrap();
                assert_eq!(v0, Value::String("zero".into()));
                let v1 = materialize(&map[&Key::Int(1)], None, 0).unwrap();
                assert_eq!(v1, Value::String("one".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_preserves_key_order() {
        let span = test_span(1, 1, 1, 5);
        let mut map = IndexMap::new();
        map.insert(
            Key::String("c".into()),
            Rc::new(Thunk::new_materialized(Value::Int(3), span)),
        );
        map.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Int(1), span)),
        );
        map.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Int(2), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                let keys: Vec<&Key> = map.keys().collect();
                assert_eq!(
                    keys,
                    vec![
                        &Key::String("c".into()),
                        &Key::String("a".into()),
                        &Key::String("b".into()),
                    ]
                );
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_dict_containing_function() {
        // Dict with a function value -- function should pass through, not be traversed
        let span = test_span(1, 1, 1, 5);
        let func_val = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(Spanned::new(Expr::Int(0), span)),
            env: Rc::new(RefCell::new(Environment::new())),
        };
        let mut map = IndexMap::new();
        map.insert(
            Key::String("f".into()),
            Rc::new(Thunk::new_materialized(func_val, span)),
        );
        map.insert(
            Key::String("v".into()),
            Rc::new(Thunk::new_materialized(Value::Int(10), span)),
        );
        let val = Value::Dict(map);
        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                let f = materialize(&map[&Key::String("f".into())], None, 0).unwrap();
                assert!(matches!(f, Value::Function { .. }));
                let v = materialize(&map[&Key::String("v".into())], None, 0).unwrap();
                assert_eq!(v, Value::Int(10));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_three_levels_deep() {
        let span = test_span(1, 1, 1, 5);

        // Build [a: [b: [c: 99]]]
        let mut level3 = IndexMap::new();
        level3.insert(
            Key::String("c".into()),
            Rc::new(Thunk::new_materialized(Value::Int(99), span)),
        );
        let mut level2 = IndexMap::new();
        level2.insert(
            Key::String("b".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(level3), span)),
        );
        let mut level1 = IndexMap::new();
        level1.insert(
            Key::String("a".into()),
            Rc::new(Thunk::new_materialized(Value::Dict(level2), span)),
        );
        let val = Value::Dict(level1);

        let result = deep_materialize(&val, 0).unwrap();
        // Navigate three levels deep
        match result {
            Value::Dict(l1) => {
                let a = materialize(&l1[&Key::String("a".into())], None, 0).unwrap();
                match a {
                    Value::Dict(l2) => {
                        let b = materialize(&l2[&Key::String("b".into())], None, 0).unwrap();
                        match b {
                            Value::Dict(l3) => {
                                let c =
                                    materialize(&l3[&Key::String("c".into())], None, 0).unwrap();
                                assert_eq!(c, Value::Int(99));
                            }
                            other => panic!("expected level 3 Dict, got {other:?}"),
                        }
                    }
                    other => panic!("expected level 2 Dict, got {other:?}"),
                }
            }
            other => panic!("expected level 1 Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_result_thunks_are_materialized() {
        // Verify that after deep_materialize, all thunks in the result dict
        // are in the Materialized state (not Unevaluated or PendingBuiltin)
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(7), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let unevaluated = Rc::new(Thunk::new_unevaluated(expr, env, span));

        let mut map = IndexMap::new();
        map.insert(Key::String("x".into()), unevaluated);
        let val = Value::Dict(map);

        let result = deep_materialize(&val, 0).unwrap();
        match result {
            Value::Dict(map) => {
                let thunk = &map[&Key::String("x".into())];
                // The thunk in the result should be in Materialized state
                assert!(matches!(
                    &*thunk.state(),
                    ThunkState::Materialized(Value::Int(7))
                ));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_seq() {
        // Verify that deep_materialize forces both head and tail of Seq
        let span = test_span(1, 1, 1, 5);
        let head_expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let head_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::clone(&head_expr),
            Rc::clone(&env),
            span,
        ));

        let tail_expr = Rc::new(Spanned::new(Expr::Str("tail".into()), span));
        let tail_thunk = Rc::new(Thunk::new_unevaluated(tail_expr, Rc::clone(&env), span));

        let seq = Value::Seq {
            head: head_thunk,
            tail: tail_thunk,
        };

        let result = deep_materialize(&seq, 0).unwrap();
        match result {
            Value::Seq { head, tail } => {
                // Both head and tail should be materialized
                let head_val = &*head.state();
                assert!(matches!(head_val, ThunkState::Materialized(Value::Int(42))));

                let tail_val = &*tail.state();
                assert!(matches!(
                    tail_val,
                    ThunkState::Materialized(Value::String(s)) if s == "tail"
                ));
            }
            other => panic!("expected Seq, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_seq_depth_limit() {
        // Build a deeply nested Seq structure exceeding MAX_EVAL_DEPTH
        let span = test_span(1, 1, 1, 1);
        let mut current = Rc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span));

        // Create MAX_EVAL_DEPTH + 2 nested Seq values
        for _ in 0..MAX_EVAL_DEPTH + 2 {
            let seq = Value::Seq {
                head: Rc::new(Thunk::new_materialized(Value::Int(1), span)),
                tail: Rc::clone(&current),
            };
            current = Rc::new(Thunk::new_materialized(seq, span));
        }

        let outer_seq = materialize(&current, None, 0).unwrap();
        let err = deep_materialize(&outer_seq, 0).unwrap_err();
        assert!(err.message().contains("maximum evaluation depth exceeded"));
    }

    // ── Stack trace / call stack reconstruction tests ──────────────────

    #[test]
    fn test_call_error_has_stack_frame_with_function_name() {
        // [f: [fn [x] $missing]; result: [call $f 1]]
        // Calling $f with body that references $missing should produce a
        // stack frame with "call $f".
        let env = empty_env();
        let fn_span = test_span(1, 1, 1, 20);
        let fn_val = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::VarRef("missing".into()),
                test_span(1, 15, 1, 23),
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, fn_span)),
        );

        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::VarRef("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11))],
                named_args: vec![],
            },
            call_span,
        );

        let thunk = eval(&call_expr, env, 0).unwrap();
        let err = materialize(&thunk, None, 0).unwrap_err();
        assert!(
            err.message().contains("undefined variable: $missing"),
            "got: {}",
            err.message()
        );
        // The stack should contain a frame for "call $f"
        assert!(
            err.stack.iter().any(|f| f.label == "call $f"),
            "expected 'call $f' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_nested_call_produces_multi_frame_stack() {
        // inner: [fn [x] $missing]
        // outer: [fn [y] [call $inner $y]]
        // [call $outer 1]
        //
        // Error should show both call sites in the stack.
        let env = empty_env();

        // Inner function
        let inner_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::VarRef("missing".into()),
                test_span(1, 20, 1, 28),
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "inner".into(),
            Rc::new(Thunk::new_materialized(inner_fn, test_span(1, 1, 1, 30))),
        );

        // Outer function: body is [call $inner $y]
        let inner_call_span = test_span(2, 15, 2, 30);
        let outer_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "y".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(Spanned::new(
                Expr::Call {
                    func: Box::new(Spanned::new(
                        Expr::VarRef("inner".into()),
                        test_span(2, 21, 2, 26),
                    )),
                    args: vec![Spanned::new(
                        Expr::VarRef("y".into()),
                        test_span(2, 28, 2, 29),
                    )],
                    named_args: vec![],
                },
                inner_call_span,
            )),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "outer".into(),
            Rc::new(Thunk::new_materialized(outer_fn, test_span(2, 1, 2, 35))),
        );

        // Evaluate [call $outer 1]
        let outer_call_span = test_span(3, 1, 3, 20);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::VarRef("outer".into()),
                    test_span(3, 7, 3, 12),
                )),
                args: vec![Spanned::new(Expr::Int(1), test_span(3, 14, 3, 15))],
                named_args: vec![],
            },
            outer_call_span,
        );

        let thunk = eval(&call_expr, env, 0).unwrap();
        let err = materialize(&thunk, None, 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $missing"));

        // Should have frames for both call sites
        let labels: Vec<&str> = err.stack.iter().map(|f| f.label.as_str()).collect();
        assert!(
            labels.contains(&"call $inner"),
            "expected 'call $inner' in stack, got: {labels:?}"
        );
        assert!(
            labels.contains(&"call $outer"),
            "expected 'call $outer' in stack, got: {labels:?}"
        );
        // Inner call should appear before outer call (innermost first)
        let inner_pos = labels.iter().position(|l| *l == "call $inner").unwrap();
        let outer_pos = labels.iter().position(|l| *l == "call $outer").unwrap();
        assert!(
            inner_pos < outer_pos,
            "inner call frame should come before outer: {labels:?}"
        );
    }

    #[test]
    fn test_dot_access_error_has_access_frame() {
        // When dot access fails because the target evaluation itself errors,
        // the error should include a frame indicating the access context.
        //
        // [a: $missing]
        // $a.x  -- accessing .x should add a frame
        let env = empty_env();

        // Put a dict with a broken value in the env
        let dict_span = test_span(1, 1, 1, 20);
        let mut dict_map = IndexMap::new();
        let bad_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(Spanned::new(
                Expr::VarRef("missing".into()),
                test_span(1, 8, 1, 15),
            )),
            Rc::clone(&env),
            test_span(1, 8, 1, 15),
        ));
        dict_map.insert(Key::String("x".into()), bad_thunk);

        env.borrow_mut().insert(
            "a".into(),
            Rc::new(Thunk::new_materialized(Value::Dict(dict_map), dict_span)),
        );

        // Now access $a.x -- this should succeed (returns the thunk), but
        // materializing the result should fail
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::VarRef("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: "x".into(),
            },
            access_span,
        );

        let thunk = eval(&access_expr, env, 0).unwrap();
        let mat_span = test_span(3, 1, 3, 10);
        let err = materialize(&thunk, Some(&mat_span), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $missing"));
        // The materialization span should be set
        assert!(err.materialization_span.is_some());
    }

    #[test]
    fn test_dot_access_on_erroring_target_has_frame() {
        // $nonexistent.field -- the target itself fails, and the error
        // should include an "accessing .field" frame.
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::VarRef("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                field: "field".into(),
            },
            access_span,
        );

        let err = eval(&access_expr, env, 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $nonexistent"));
        // Should have an "accessing .field" frame
        assert!(
            err.stack.iter().any(|f| f.label == "accessing .field"),
            "expected 'accessing .field' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_bracket_access_on_erroring_target_has_frame() {
        // $nonexistent[0] -- the target itself fails
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::BracketAccess {
                expr: Box::new(Spanned::new(
                    Expr::VarRef("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                key: Box::new(Spanned::new(Expr::Int(0), test_span(1, 13, 1, 14))),
            },
            access_span,
        );

        let err = eval(&access_expr, env, 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $nonexistent"));
        assert!(
            err.stack.iter().any(|f| f.label == "accessing [..]"),
            "expected 'accessing [..]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_range_access_on_erroring_target_has_frame() {
        // $nonexistent[0..2] -- the target itself fails
        let env = empty_env();
        let access_span = test_span(1, 1, 1, 20);
        let access_expr = Spanned::new(
            Expr::RangeAccess {
                expr: Box::new(Spanned::new(
                    Expr::VarRef("nonexistent".into()),
                    test_span(1, 1, 1, 12),
                )),
                start: Some(Box::new(Spanned::new(
                    Expr::Int(0),
                    test_span(1, 13, 1, 14),
                ))),
                end: Some(Box::new(Spanned::new(
                    Expr::Int(2),
                    test_span(1, 16, 1, 17),
                ))),
            },
            access_span,
        );

        let err = eval(&access_expr, env, 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $nonexistent"));
        assert!(
            err.stack.iter().any(|f| f.label == "accessing [..:..]"),
            "expected 'accessing [..:..]' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_chained_access_error_shows_chain() {
        // [a: [x: $missing]]
        // $a.x  -- force chain
        // When materialized, the error should show the materialization chain.
        let inner_env = empty_env();
        let mut inner_map = IndexMap::new();
        inner_map.insert(
            Key::String("x".into()),
            Rc::new(Thunk::new_unevaluated(
                Rc::new(Spanned::new(
                    Expr::VarRef("missing".into()),
                    test_span(1, 10, 1, 18),
                )),
                Rc::clone(&inner_env),
                test_span(1, 10, 1, 18),
            )),
        );
        let inner_dict = Value::Dict(inner_map);

        let env = empty_env();
        env.borrow_mut().insert(
            "a".into(),
            Rc::new(Thunk::new_materialized(inner_dict, test_span(1, 1, 1, 20))),
        );

        // Build $a.x access
        let access_span = test_span(2, 1, 2, 5);
        let access_expr = Spanned::new(
            Expr::DotAccess {
                expr: Box::new(Spanned::new(
                    Expr::VarRef("a".into()),
                    test_span(2, 1, 2, 2),
                )),
                field: "x".into(),
            },
            access_span,
        );

        // Eval returns the thunk for $missing
        let thunk = eval(&access_expr, Rc::clone(&env), 0).unwrap();

        // Materialize with a different span (simulating a reference from $b)
        let b_span = test_span(3, 1, 3, 5);
        let err = materialize(&thunk, Some(&b_span), 0).unwrap_err();
        assert!(err.message().contains("undefined variable: $missing"));
        assert_eq!(
            err.materialization_span,
            Some(b_span),
            "materialization span should be the forcing site"
        );
    }

    #[test]
    fn test_func_label_varref() {
        assert_eq!(func_label(&Expr::VarRef("f".into())), "call $f");
    }

    #[test]
    fn test_func_label_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::VarRef("utils".into()))),
            field: "run".into(),
        };
        assert_eq!(func_label(&expr), "call $utils.run");
    }

    #[test]
    fn test_func_label_chained_dot_access() {
        let expr = Expr::DotAccess {
            expr: Box::new(sp(Expr::DotAccess {
                expr: Box::new(sp(Expr::VarRef("a".into()))),
                field: "b".into(),
            })),
            field: "c".into(),
        };
        assert_eq!(func_label(&expr), "call $a.b.c");
    }

    #[test]
    fn test_func_label_anonymous() {
        assert_eq!(func_label(&Expr::Int(42)), "call <anonymous>");
    }

    #[test]
    fn test_materialize_chain_no_duplicate_frames() {
        // When the same mat_span propagates through nested materialize calls,
        // we should not get duplicate frames for the same span.
        let env = empty_env();

        // Create a thunk whose body is another unevaluated thunk that errors
        let inner_expr = Spanned::new(Expr::VarRef("missing".into()), test_span(1, 1, 1, 8));
        let inner_thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(inner_expr),
            Rc::clone(&env),
            test_span(1, 1, 1, 8),
        ));

        // Materialize with a specific span
        let mat_span = test_span(5, 1, 5, 10);
        let err = materialize(&inner_thunk, Some(&mat_span), 0).unwrap_err();

        // Count how many frames have the same span
        let frame_count = err.stack.iter().filter(|f| f.span == mat_span).count();
        assert!(
            frame_count <= 1,
            "expected at most 1 frame with mat_span, got {frame_count}: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_call_arity_error_has_call_frame() {
        // Calling a function with wrong arity should include the call site frame
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
            body: Rc::new(sp(Expr::VarRef("a".into()))),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "f".into(),
            Rc::new(Thunk::new_materialized(fn_val, test_span(1, 1, 1, 20))),
        );

        // Call with wrong arity: [call $f 1] (needs 2 args)
        let call_span = test_span(2, 1, 2, 15);
        let call_expr = Spanned::new(
            Expr::Call {
                func: Box::new(Spanned::new(
                    Expr::VarRef("f".into()),
                    test_span(2, 7, 2, 8),
                )),
                args: vec![Spanned::new(Expr::Int(1), test_span(2, 10, 2, 11))],
                named_args: vec![],
            },
            call_span,
        );

        let err = eval(&call_expr, env, 0).unwrap_err();
        assert!(err.message().contains("arity mismatch"));
        assert!(
            err.stack.iter().any(|f| f.label == "call $f"),
            "expected 'call $f' frame, got: {:?}",
            err.stack
        );
    }

    #[test]
    fn test_error_display_with_full_stack() {
        // Integration test: verify the Display output includes all stack frames
        let err = EvalError::new("something broke", test_span(1, 5, 1, 12))
            .with_materialization_span(test_span(10, 1, 10, 5))
            .with_frame("call $inner", test_span(5, 1, 5, 20))
            .with_frame("call $outer", test_span(8, 1, 8, 25));
        let display = format!("{err}");
        assert!(display.contains("something broke"));
        assert!(display.contains("defined at 1:5-1:12"));
        assert!(display.contains("materialized at 10:1-10:5"));
        assert!(display.contains("in call $inner at 5:1-5:20"));
        assert!(display.contains("in call $outer at 8:1-8:25"));
    }

    // ── PendingCall thunk state tests ──────────────────────────────────

    #[test]
    fn test_pending_call_llt_function() {
        // Create a PendingCall thunk that calls an LLT function
        // [fn [x y] [call $+ $x $y]] with args (3, 4)
        let env = empty_env();

        // Create a simple addition function
        let add_fn = Value::Function {
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
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::VarRef("+".into()))),
                args: vec![sp(Expr::VarRef("x".into())), sp(Expr::VarRef("y".into()))],
                named_args: vec![],
            })),
            env: Rc::clone(&env),
        };

        // Add the builtin $+ to the environment
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin {
                    name: "+",
                    func: add_builtin,
                },
                test_span(1, 1, 1, 5),
            )),
        );

        // Create PendingCall thunk
        let func_thunk = Rc::new(Thunk::new_materialized(add_fn, test_span(1, 1, 1, 20)));
        let arg1 = Rc::new(Thunk::new_materialized(
            Value::Int(3),
            test_span(1, 21, 1, 22),
        ));
        let arg2 = Rc::new(Thunk::new_materialized(
            Value::Int(4),
            test_span(1, 23, 1, 24),
        ));
        let call_span = test_span(2, 1, 2, 15);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        );

        // Materialize should call the function and return the result
        let result = materialize(&pending, None, 0).unwrap();
        assert_eq!(result, Value::Int(7));
    }

    #[test]
    fn test_pending_call_builtin_function() {
        // Create a PendingCall thunk where the function is a Builtin
        fn multiply_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x * y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }

        let func_thunk = Rc::new(Thunk::new_materialized(
            Value::Builtin {
                name: "*",
                func: multiply_builtin,
            },
            test_span(1, 1, 1, 5),
        ));
        let arg1 = Rc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 6, 1, 7),
        ));
        let arg2 = Rc::new(Thunk::new_materialized(
            Value::Int(6),
            test_span(1, 8, 1, 9),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg1, arg2],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        );

        // Materialize should call the builtin directly and return the result
        let result = materialize(&pending, None, 0).unwrap();
        assert_eq!(result, Value::Int(30));
    }

    #[test]
    fn test_pending_call_memoizes() {
        // PendingCall should memoize: second materialization returns cached value
        let env = empty_env();

        // Create a function that would fail if called twice
        // (we'll verify it's only called once by checking the state)
        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));
        let arg = Rc::new(Thunk::new_materialized(
            Value::Int(42),
            test_span(1, 11, 1, 13),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Rc::new(Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        ));

        // First materialization
        let result1 = materialize(&pending, None, 0).unwrap();
        assert_eq!(result1, Value::Int(42));

        // Check that the thunk is now in Materialized state
        match &*pending.state() {
            ThunkState::Materialized(v) => assert_eq!(*v, Value::Int(42)),
            other => panic!("expected Materialized after first call, got {other:?}"),
        }

        // Second materialization should return cached value
        let result2 = materialize(&pending, None, 0).unwrap();
        assert_eq!(result2, Value::Int(42));
    }

    #[test]
    fn test_pending_call_non_function_error() {
        // PendingCall with a non-Function/Builtin value should error
        let not_a_function = Rc::new(Thunk::new_materialized(
            Value::Int(123),
            test_span(1, 1, 1, 4),
        ));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            not_a_function,
            vec![],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        );

        let err = materialize(&pending, None, 0).unwrap_err();
        assert!(
            err.message()
                .contains("expected Function or Builtin, got Int"),
            "got: {}",
            err.message()
        );
    }

    #[test]
    fn test_pending_call_with_unevaluated_args() {
        // PendingCall should work with unevaluated argument thunks (lazy evaluation)
        let env = empty_env();

        let identity_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("x".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(identity_fn, test_span(1, 1, 1, 10)));

        // Create an unevaluated arg
        let arg_expr = Rc::new(sp(Expr::Int(99)));
        let arg = Rc::new(Thunk::new_unevaluated(
            arg_expr,
            Rc::clone(&env),
            test_span(1, 11, 1, 13),
        ));

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            vec![arg],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        );

        // Materialize should evaluate the arg thunk and return the result
        let result = materialize(&pending, None, 0).unwrap();
        assert_eq!(result, Value::Int(99));
    }

    #[test]
    fn test_pending_call_with_named_args() {
        // PendingCall should pass named args through to function invocation
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin {
                    name: "+",
                    func: add_builtin,
                },
                test_span(1, 1, 1, 5),
            )),
        );

        // Create a function that takes a mix of positional and named parameters
        let fn_with_named = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "a".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "b".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: sp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::VarRef("+".into()))),
                args: vec![sp(Expr::VarRef("a".into())), sp(Expr::VarRef("b".into()))],
                named_args: vec![],
            })),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(
            fn_with_named,
            test_span(1, 1, 1, 10),
        ));

        // Pass first arg positionally, second as named
        let positional = vec![Rc::new(Thunk::new_materialized(
            Value::Int(5),
            test_span(1, 11, 1, 12),
        ))];

        let mut named = IndexMap::new();
        named.insert(
            "b".into(),
            Rc::new(Thunk::new_materialized(
                Value::Int(3),
                test_span(1, 13, 1, 14),
            )),
        );

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            named,
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call-named"),
        );

        // Materialize should pass named args through correctly
        let result = materialize(&pending, None, 0).unwrap();
        assert_eq!(result, Value::Int(8)); // 5 + 3
    }

    #[test]
    fn test_pending_call_with_default_named_args() {
        // PendingCall with partial named args should use defaults
        let env = empty_env();

        // Install a built-in add function
        fn add_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { args, .. } = ctx;
            let a = materialize(&args[0], None, 0)?;
            let b = materialize(&args[1], None, 0)?;
            match (a, b) {
                (Value::Int(x), Value::Int(y)) => Ok(Rc::new(Thunk::new_materialized(
                    Value::Int(x + y),
                    test_span(1, 1, 1, 1),
                ))),
                _ => panic!("test expects Int args"),
            }
        }
        env.borrow_mut().insert(
            "+".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin {
                    name: "+",
                    func: add_builtin,
                },
                test_span(1, 1, 1, 5),
            )),
        );

        let fn_with_default = Value::Function {
            params: Rc::new(vec![
                Param {
                    name: "x".into(),
                    annotation: None,
                    variadic: false,
                },
                Param {
                    name: "y".into(),
                    annotation: Some(sp(Annotation::PropertyDict(vec![sp(Entry {
                        key: Some(sp(Expr::Str("default".into()))),
                        value: sp(Expr::Int(10)),
                    })]))),
                    variadic: false,
                },
            ]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::VarRef("+".into()))),
                args: vec![sp(Expr::VarRef("x".into())), sp(Expr::VarRef("y".into()))],
                named_args: vec![],
            })),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(
            fn_with_default,
            test_span(1, 1, 1, 10),
        ));

        // Provide x positionally, omit y so it uses default (10)
        let positional = vec![Rc::new(Thunk::new_materialized(
            Value::Int(7),
            test_span(1, 11, 1, 12),
        ))];

        let call_span = test_span(2, 1, 2, 10);

        let pending = Thunk::new_pending_call(
            func_thunk,
            positional,
            IndexMap::new(), // no named args - let y use default
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call-default"),
        );

        // Materialize should use default for y (10)
        let result = materialize(&pending, None, 0).unwrap();
        assert_eq!(result, Value::Int(17)); // 7 + 10
    }

    // ── Failed thunk state tests ───────────────────────────────────────

    #[test]
    fn test_failed_state_returns_cached_error() {
        // When a thunk fails, it should cache the error in Failed state
        // and return it on subsequent materialization attempts
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("x".into()))),
            value: sp(Expr::VarRef("undefined".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();

        let x_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("x".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First materialization: should fail and cache the error
        let err1 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err1.message().contains("undefined variable: $undefined"),
            "first error: got: {}",
            err1.message()
        );

        // Check that the thunk is now in Failed state
        match &*x_thunk.state() {
            ThunkState::Failed(cached_err) => {
                assert!(cached_err
                    .message()
                    .contains("undefined variable: $undefined"));
            }
            other => panic!("expected Failed state, got {other:?}"),
        }

        // Second materialization: should return the cached error
        let err2 = materialize(&x_thunk, None, 0).unwrap_err();
        assert!(
            err2.message().contains("undefined variable: $undefined"),
            "second error: got: {}",
            err2.message()
        );
    }

    #[test]
    fn test_failed_state_updates_materialization_span() {
        // Failed state should preserve the first materialization_span and add
        // subsequent access sites as stack frames (dual-span model)
        let entries = vec![sp(Entry {
            key: Some(sp(Expr::Str("broken".into()))),
            value: sp(Expr::VarRef("missing".into())),
        })];
        let dict = sp(Expr::Dict(entries));
        let env = empty_env();
        let dict_thunk = eval(&dict, Rc::clone(&env), 0).unwrap();
        let dict_val = materialize(&dict_thunk, None, 0).unwrap();

        let broken_thunk = match &dict_val {
            Value::Dict(map) => Rc::clone(map.get(&Key::String("broken".into())).unwrap()),
            other => panic!("expected Dict, got {other:?}"),
        };

        // First access with one materialization span
        let span1 = test_span(10, 1, 10, 5);
        let err1 = materialize(&broken_thunk, Some(&span1), 0).unwrap_err();
        assert_eq!(err1.materialization_span, Some(span1));
        assert_eq!(err1.stack.len(), 0);

        // Second access with a different materialization span should preserve span1
        // and add span2 as a stack frame
        let span2 = test_span(20, 1, 20, 5);
        let err2 = materialize(&broken_thunk, Some(&span2), 0).unwrap_err();
        assert_eq!(err2.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err2.stack.len(), 1);
        assert_eq!(err2.stack[0].label, "materialized");
        assert_eq!(err2.stack[0].span, span2);

        // Third access with no materialization span returns error with the
        // original materialization_span and the stack frame from the second access
        let err3 = materialize(&broken_thunk, None, 0).unwrap_err();
        assert_eq!(err3.materialization_span, Some(span1)); // PRESERVED
        assert_eq!(err3.stack.len(), 1);
        assert_eq!(err3.stack[0].span, span2);
    }

    #[test]
    fn test_failed_state_preserves_stack_frames() {
        // Failed state should preserve the original error's stack frames
        let env = empty_env();

        // Create a function that will fail
        let failing_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::VarRef("nonexistent".into()))),
            env: Rc::clone(&env),
        };

        env.borrow_mut().insert(
            "bad_fn".into(),
            Rc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 20))),
        );

        // Call the failing function
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("bad_fn".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![],
        });

        let thunk = eval(&call_expr, env, 0).unwrap();

        // First materialization: error should have stack frames
        let err1 = materialize(&thunk, None, 0).unwrap_err();
        assert!(err1.message().contains("undefined variable: $nonexistent"));
        let frame_count1 = err1.stack.len();
        assert!(frame_count1 > 0, "should have at least one stack frame");

        // Second materialization: error should have the same stack frames
        let err2 = materialize(&thunk, None, 0).unwrap_err();
        assert_eq!(
            err2.stack.len(),
            frame_count1,
            "stack frames should be preserved"
        );
    }

    #[test]
    fn test_pending_builtin_error_becomes_failed() {
        // When a PendingBuiltin fails, it should transition to Failed state
        fn failing_builtin(ctx: crate::value::BuiltinArgs) -> EvalResult<Rc<Thunk>> {
            let crate::value::BuiltinArgs { call_span, .. } = ctx;
            Err(EvalError::new("builtin intentionally failed", call_span).into())
        }

        let env = empty_env();
        env.borrow_mut().insert(
            "fail".into(),
            Rc::new(Thunk::new_materialized(
                Value::Builtin {
                    name: "fail",
                    func: failing_builtin,
                },
                test_span(1, 1, 1, 5),
            )),
        );

        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("fail".into()))),
            args: vec![],
            named_args: vec![],
        });

        let thunk = eval(&call_expr, env, 0).unwrap();

        // First materialization: should fail
        let err1 = materialize(&thunk, None, 0).unwrap_err();
        assert!(err1.message().contains("builtin intentionally failed"));

        // Check that the thunk is now in Failed state
        match &*thunk.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, 0).unwrap_err();
        assert!(err2.message().contains("builtin intentionally failed"));
    }

    #[test]
    fn test_pending_call_error_becomes_failed() {
        // When a PendingCall fails, it should transition to Failed state
        let env = empty_env();

        let failing_fn = Value::Function {
            params: Rc::new(vec![]),
            body: Rc::new(sp(Expr::VarRef("does_not_exist".into()))),
            env: Rc::clone(&env),
        };

        let func_thunk = Rc::new(Thunk::new_materialized(failing_fn, test_span(1, 1, 1, 10)));
        let call_span = test_span(2, 1, 2, 10);

        let pending = Rc::new(Thunk::new_pending_call(
            func_thunk,
            vec![],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        ));

        // First materialization: should fail
        let err1 = materialize(&pending, None, 0).unwrap_err();
        assert!(err1
            .message()
            .contains("undefined variable: $does_not_exist"));

        // Check that the thunk is now in Failed state
        match &*pending.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&pending, None, 0).unwrap_err();
        assert!(err2
            .message()
            .contains("undefined variable: $does_not_exist"));
    }

    #[test]
    fn test_pending_call_func_materialization_failure() {
        let bad_func = Rc::new(Thunk::new_unevaluated(
            Rc::new(sp(Expr::VarRef("nonexistent_func".into()))),
            empty_env(),
            test_span(1, 1, 1, 10),
        ));
        let call_span = test_span(2, 1, 2, 10);
        let pending = Rc::new(Thunk::new_pending_call(
            bad_func,
            vec![],
            IndexMap::new(),
            call_span,
            call_span,
            Cow::Borrowed("test-pending-call"),
        ));

        // First materialization should fail with undefined variable error
        let err = materialize(&pending, None, 0).unwrap_err();
        assert!(err
            .message()
            .contains("undefined variable: $nonexistent_func"));

        // The thunk should be in Failed state, NOT InProgress
        match &*pending.state() {
            ThunkState::Failed(_) => {}
            ThunkState::InProgress => panic!("BUG: thunk stuck in InProgress"),
            other => panic!("unexpected state: {other:?}"),
        }

        // Second access should return cached error, NOT "circular dependency"
        let err2 = materialize(&pending, None, 0).unwrap_err();
        assert!(err2
            .message()
            .contains("undefined variable: $nonexistent_func"));
        assert!(!err2.message().contains("circular dependency"));
    }

    #[test]
    fn test_unevaluated_error_becomes_failed() {
        // When an Unevaluated thunk fails during materialization, it should transition to Failed
        let expr = sp(Expr::VarRef("undefined_var".into()));
        let env = empty_env();
        let thunk = Rc::new(Thunk::new_unevaluated(
            Rc::new(expr),
            Rc::clone(&env),
            test_span(1, 1, 1, 15),
        ));

        // First materialization: should fail
        let err1 = materialize(&thunk, None, 0).unwrap_err();
        assert!(err1
            .message()
            .contains("undefined variable: $undefined_var"));

        // Check that the thunk is now in Failed state
        match &*thunk.state() {
            ThunkState::Failed(_) => {}
            other => panic!("expected Failed state after error, got {other:?}"),
        }

        // Second materialization: should return cached error
        let err2 = materialize(&thunk, None, 0).unwrap_err();
        assert!(err2
            .message()
            .contains("undefined variable: $undefined_var"));
    }

    #[test]
    fn test_pending_call_cycle_detection() {
        // 256 levels of LLT recursion needs more than the default 8MB Rust stack.
        let result = std::thread::Builder::new()
            .stack_size(16 * 1024 * 1024)
            .spawn(|| {
                let env = empty_env();

                let recursive_fn = Value::Function {
                    params: Rc::new(vec![Param {
                        name: "x".into(),
                        annotation: None,
                        variadic: false,
                    }]),
                    body: Rc::new(sp(Expr::Call {
                        func: Box::new(sp(Expr::VarRef("f".into()))),
                        args: vec![sp(Expr::VarRef("x".into()))],
                        named_args: vec![],
                    })),
                    env: Rc::clone(&env),
                };

                env.borrow_mut().insert(
                    "f".into(),
                    Rc::new(Thunk::new_materialized(
                        recursive_fn,
                        test_span(1, 1, 1, 20),
                    )),
                );

                let call_expr = sp(Expr::Call {
                    func: Box::new(sp(Expr::VarRef("f".into()))),
                    args: vec![sp(Expr::Int(1))],
                    named_args: vec![],
                });

                let thunk = eval(&call_expr, env, 0).unwrap();
                materialize(&thunk, None, 0).unwrap_err()
            })
            .unwrap()
            .join()
            .unwrap();
        assert!(
            result
                .message()
                .contains("maximum evaluation depth exceeded"),
            "got: {}",
            result.message()
        );
    }
}
