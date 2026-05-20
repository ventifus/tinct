//! Type introspection, evaluation control, and meta builtins.
//!
//! These builtins provide type checking, evaluation control, AST manipulation,
//! JSON conversion, file inclusion, and schema validation.
//!
//! **Evaluation control:**
//! - `deep-materialize`: Deep-materialize a value recursively
//! - `error`: Raise a user error with a custom message
//! - `try`: Catch errors from a zero-arg function
//! - `apply`: Spread dict values as function args
//! - `until`: Iterative loop until predicate holds
//!
//! **Type introspection:**
//! - `type-of`: Return the runtime type name
//! - `tag-of`: Extract tag from a Variant
//! - `variant`: Create a unit variant
//! - `int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?`: Type predicates (plus `num?`, `record?`, `map?` in LLT stdlib)
//!
//! **AST and evaluation:**
//! - `expand`: Run macro expansion on a file AST dict (dict → expand → dict)
//! - `eval`: Evaluate AST expression nodes with optional `env:` and `%:` named args
//! - `eval-types`: Evaluate AST expression nodes in the type stage environment
//! - `eval-ast`: Reconstruct and evaluate AST from dict representation
//! - `gensym`: Generate unique symbol names for macro hygiene
//!
//! **Number types:**
//! - `decimal`: Parse/convert to exact decimal (rust_decimal)
//! - `big-int`: Parse/convert to arbitrary-precision integer
//!
//! **JSON and I/O:**
//! - `from-json`: Parse JSON string to LLT value
//! - `include`: Evaluate external LLT files with cycle detection and integrity checking
//!
//! **Schema validation:**
//! - `validate`: Runtime structural validation with constraint checking
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` and `create_root_env()` remains in `builtins.rs`.

use std::cell::RefCell;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::Span;
use crate::builtins::{
    builtin, ok_val, reject_named, require_string, JSON_DEPTH_LIMIT, MAX_COLLECT_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::materialize;
use crate::eval_call::{invoke_function, CallContext};
use crate::value::{string_val, BuiltinArgs, Environment, Key, Thunk, Value};

/// `deep-materialize`: takes 1 arg, deep-forces all thunks recursively.
/// Delegates to [`crate::eval_deep::deep_materialize`].
/// Inherently materializing: deep-forces all thunks by definition.
pub(crate) fn builtin_deep_materialize(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("deep-materialize", args, named, &ctx, call_span)?;
    let deep = crate::eval_deep::deep_materialize(&val, &ctx, Some(&call_span))?;
    ok_val(deep, call_span)
}

/// `materialize`: takes 1 arg, forces it to WHNF and returns it.
///
/// Gives users explicit control over evaluation order. Equivalent to `$deep-materialize` for
/// flat values, but only forces to weak head normal form (WHNF) — dicts remain
/// dicts with unforced entries, not deep-forced. Use `$deep-materialize` for deep forcing.
pub(crate) fn builtin_force(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let forced = crate::builtins::expect_one_arg("materialize", args, named, &ctx, call_span)?;
    ok_val(forced, call_span)
}

/// `raise`: takes 1 arg (String message), always raises.
/// Inherently materializing: constructs concrete error value.
pub(crate) fn builtin_raise(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    if args.len() != 1 {
        // Temporary diagnostic: print args info before failing
        let named_count = named.map(|n| n.len()).unwrap_or(0);
        println!(
            "[DEBUG builtin_raise] got {} positional args, {} named args at {:?}",
            args.len(),
            named_count,
            call_span
        );
        for (i, arg) in args.iter().enumerate() {
            if let Some(v) = arg.try_get_materialized() {
                println!("  arg[{}] = {:?}", i, v.type_name());
            } else {
                println!("  arg[{}] = <unevaluated>", i);
            }
        }
    }
    let val = crate::builtins::expect_one_arg("raise", args, named, &ctx, call_span)?;
    let msg = require_string("raise", val, args[0].span)?;
    Err(EvalError::user_error(msg.to_string(), call_span).into())
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[Ok value]`
/// on success or `[Err message]` on failure.
/// Inherently materializing: must materialize body to catch errors.
pub(crate) fn builtin_try(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("try", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let func_val = materialize(&args[0], Some(&call_span), &ctx)?;

    let call_result = match func_val {
        Value::Function {
            params,
            body,
            env: closure_env,
            ..
        } => {
            if !params.is_empty() {
                return Err(EvalError::type_mismatch_ctx(
                    "try".to_string(),
                    "zero-argument function",
                    &format!("{}-parameter function", params.len()),
                    call_span,
                )
                .into());
            }
            // Evaluate the body in the closure's environment
            let body_thunk = Rc::new(Thunk::new_unevaluated(
                Rc::clone(&body),
                Rc::clone(&closure_env),
                Rc::clone(&ctx),
                body.span,
            ));
            materialize(&body_thunk, Some(&call_span), &ctx)
        }
        Value::Builtin(def) => {
            let builtin_args = BuiltinArgs {
                args: &[],
                named: None,
                call_span,
                ctx: Rc::clone(&ctx),
            };
            (def.func)(builtin_args).and_then(|thunk| materialize(&thunk, Some(&call_span), &ctx))
        }
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "try".to_string(),
                "Function",
                func_val.type_name(),
                call_span,
            )
            .into())
        }
    };

    match call_result {
        Ok(val) => {
            // Success: return Value::Variant { tag: "Ok", payload: Some(value) }
            let payload_thunk_id = ctx.alloc_thunk(ok_val(val, call_span)?);
            ok_val(
                Value::Variant {
                    tag: "Ok".to_string(),
                    payload: Some(payload_thunk_id),
                },
                call_span,
            )
        }
        Err(e) => {
            // DepthExceeded and ResourceLimitExceeded are non-catchable:
            // they indicate system-level limits, not user-level errors.
            use crate::error::ErrorKind;
            match &e.kind {
                ErrorKind::DepthExceeded { .. } | ErrorKind::ResourceLimitExceeded { .. } => {
                    return Err(e);
                }
                _ => {}
            }
            // Error: return Value::Variant { tag: "Error", payload: Some(message) }
            let msg_thunk_id = ctx.alloc_thunk(ok_val(string_val(&e.kind.to_string()), call_span)?);
            ok_val(
                Value::Variant {
                    tag: "Error".to_string(),
                    payload: Some(msg_thunk_id),
                },
                call_span,
            )
        }
    }
}

/// `until`: Repeat f(val) until predicate(val) returns true.
///
/// This is a Rust builtin to avoid the recursion depth limit of the LLT version.
/// The LLT recursive version hits MAX_EVAL_DEPTH at ~230 iterations.
///
/// This implementation uses a Rust loop with eager materialization at each step,
/// avoiding both depth limits and stack overflow from long thunk chains.
pub(crate) fn builtin_until(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("until", named, call_span)?;
    if args.len() != 3 {
        return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
    }

    let pred_thunk = Rc::clone(&args[0]);
    let f_thunk = Rc::clone(&args[1]);
    let mut val_thunk = Rc::clone(&args[2]);

    loop {
        // Create a pending call to pred(val) and materialize it
        let pred_result = Rc::new(Thunk::new_pending_call(
            Rc::clone(&pred_thunk),
            vec![Rc::clone(&val_thunk)],
            IndexMap::new(),
            call_span,
            Rc::clone(&ctx.config.stdlib_env),
            val_thunk.span,
            Some(Rc::from("until")),
            Rc::clone(&ctx),
        ));

        let pred_val = materialize(&pred_result, Some(&call_span), &ctx)?;

        match pred_val {
            Value::Bool(true) => {
                // Predicate holds, return the current value (as thunk)
                return Ok(val_thunk);
            }
            Value::Bool(false) => {
                // Predicate doesn't hold yet, apply f and materialize to get next value
                let f_result = Rc::new(Thunk::new_pending_call(
                    Rc::clone(&f_thunk),
                    vec![val_thunk],
                    IndexMap::new(),
                    call_span,
                    Rc::clone(&ctx.config.stdlib_env),
                    call_span,
                    Some(Rc::from("until")),
                    Rc::clone(&ctx),
                ));

                // Eagerly materialize f(val) and re-wrap as a thunk for the next iteration
                // This breaks the thunk chain and prevents stack overflow
                let f_val = materialize(&f_result, Some(&call_span), &ctx)?;
                val_thunk = Rc::new(Thunk::new_materialized(f_val, call_span));
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "until".to_string(),
                    "Bool",
                    pred_val.type_name(),
                    call_span,
                )
                .into())
            }
        }
    }
}

/// Helper that performs the actual $apply logic after args are pre-materialized.
/// This is separated from builtin_apply so that builtin_apply can return a
/// PendingBuiltin thunk, enabling iterative arg materialization via BuiltinForceArg.
pub(crate) fn builtin_apply_impl(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("apply", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    // Both args[0] and args[1] have been pre-materialized by BuiltinForceArg,
    // so these materialize() calls are O(1) cache hits.
    let func_val = materialize(&args[0], None, &ctx)?;
    let args_val = materialize(&args[1], None, &ctx)?;

    let arg_dict = crate::builtins::require_dict("apply", args_val, args[1].span, &ctx, call_span)?;

    // Split dict entries: integer-keyed → positional, string-keyed → named
    let mut int_entries: Vec<(i64, Rc<Thunk>)> = Vec::with_capacity(arg_dict.len());
    let mut named_args: IndexMap<String, Rc<Thunk>> = IndexMap::with_capacity(arg_dict.len());
    for (key, thunk_id) in &arg_dict {
        let thunk = ctx.get_thunk(*thunk_id);
        match key {
            Key::Int(n) => int_entries.push((*n, thunk)),
            Key::String(s) => {
                named_args.insert(s.clone(), thunk);
            }
        }
    }
    int_entries.sort_by_key(|(k, _)| *k);
    let positional: Vec<Rc<Thunk>> = int_entries.into_iter().map(|(_, v)| v).collect();

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
            closure_env_id: None,
            positional: &positional,
            named: if named_args.is_empty() {
                None
            } else {
                Some(&named_args)
            },
            default_env: &closure_env,
            ctx: &ctx,
            call_span,
            origin: Some(Rc::from("apply")),
        }),
        Value::Builtin(def) => {
            let builtin_args = BuiltinArgs {
                args: &positional,
                named: if named_args.is_empty() {
                    None
                } else {
                    Some(&named_args)
                },
                call_span,
                ctx: Rc::clone(&ctx),
            };
            (def.func)(builtin_args)
        }
        _ => Err(EvalError::type_mismatch_ctx(
            "apply".to_string(),
            "Function",
            func_val.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `apply`: wrapper that returns a PendingBuiltin thunk for iterative arg materialization.
pub(crate) fn builtin_apply(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    // Return a PendingBuiltin thunk that wraps builtin_apply_impl.
    // When materialized, the PendingBuiltin handler will use BuiltinForceArg
    // to pre-materialize both args[0] and args[1] iteratively, avoiding
    // Rust stack growth.
    // Pass named args through: $apply may forward named args to the target function.
    // Use None when named is empty to skip the IndexMap allocation.
    let named_opt = if named.map(|n| n.is_empty()).unwrap_or(true) {
        None
    } else {
        Some(named.expect("checked by if condition above").clone())
    };
    Ok(Rc::new(Thunk::new_pending_builtin(
        builtin!("apply", builtin_apply_impl),
        args.to_vec(),
        named_opt,
        call_span,
        Some(Rc::from("apply")),
        ctx,
    )))
}

/// `eval-ast`: takes 1 arg (Dict from quote), reconstructs AST, evaluates it.
/// Inherently materializing: must materialize dict to extract AST structure,
/// then evaluate the reconstructed expression.
pub(crate) fn builtin_eval_ast(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("eval-ast", args, named, &ctx, call_span)?;

    // Convert the dict to an AST node
    let ast = crate::ast_dict::dict_to_ast(&val, &ctx)
        .map_err(|e| EvalError::user_error(format!("eval-ast: {}", e), call_span))?;

    // Evaluate the reconstructed AST in the stdlib environment
    let env = Rc::clone(&ctx.config.stdlib_env);
    crate::eval::eval(Rc::new(ast), env, &ctx)
}

/// `expand`: Run macro expansion on a file AST dict.
///
/// Takes a file AST dict (the output of `load` or `ast-of`) and returns the
/// macro-expanded file AST dict. The round-trip is:
/// 1. Dict → File (via `dict_to_file`)
/// 2. File → expanded File (via `expand_macros_in_ctx`)
/// 3. expanded File → Dict (via `ast_to_dict`)
///
/// Uses `expand_macros_in_ctx` instead of `expand_macros` to avoid a redundant
/// stdlib reload. The caller's `EvalContext` already has a fully-loaded stdlib env
/// (including all prelude functions and macro transformers), so there is no need to
/// call `create_stdlib_env_with_arena()` again from within the include pipeline.
///
/// Schema errors from `dict_to_file` surface as user errors.
pub(crate) fn builtin_expand(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("expand", args, named, &ctx, call_span)?;

    // Convert the dict to a File AST
    let file = crate::ast_dict::dict_to_file(&val, &ctx)
        .map_err(|e| EvalError::user_error(format!("expand: {}", e), call_span))?;

    // Run macro expansion using the caller's existing EvalContext.
    // This avoids a redundant create_stdlib_env_with_arena() call that would otherwise
    // re-parse the prelude, overwrite STDLIB_ARENA_CACHE, and break ThunkId sharing.
    let spanned_file = crate::ast::Spanned::new(file, call_span);
    let expand_result = crate::expand::expand_macros_in_ctx(spanned_file, &ctx)?;

    // Convert back to dict
    let opts = crate::ast_dict::AstToDictOpts::default();
    crate::ast_dict::ast_to_dict(&expand_result.file.node, &opts, &ctx)
}

/// `eval`: Evaluate AST expression nodes in the runtime stage environment.
///
/// Positional arg: `exprs` — a positional Dict of expression AST dicts (from a document's
/// `expressions` field after `expand`).
///
/// Named args:
/// - `%:` — the pipeline input, bound as `%` in the evaluation environment (default: `[]`)
/// - `env:` — extra bindings dict, merged into the environment (default: `[]`)
///
/// Returns the result of evaluating the expressions with sequential let* scoping.
pub(crate) fn builtin_eval(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Get the positional dict of expression AST dicts.
    // Do NOT use expect_one_arg here — it rejects named args, but builtin_eval
    // accepts env: and %: named args that are read below.
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let exprs_dict_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let exprs_dict = match &exprs_dict_val {
        Value::Dict(d) => d,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "eval".to_string(),
                "Dict",
                exprs_dict_val.type_name(),
                call_span,
            )
            .into());
        }
    };

    // Extract expression AST dicts from the positional dict
    let mut exprs = Vec::new();
    for i in 0.. {
        match exprs_dict.get(&Key::Int(i)) {
            Some(thunk_id) => {
                let thunk = ctx.get_thunk(*thunk_id);
                let expr_val = materialize(&thunk, Some(&call_span), &ctx)?;
                let expr_ast = crate::ast_dict::dict_to_ast(&expr_val, &ctx).map_err(|e| {
                    EvalError::user_error(format!("eval: expression {}: {}", i, e), call_span)
                })?;
                // dict_to_ast already returns Spanned<Expr>, don't wrap again
                exprs.push(expr_ast);
            }
            None => break,
        }
    }

    // Build environment chain: stdlib_env → env: entries → "$" = %:
    let base_env = Rc::clone(&ctx.config.stdlib_env);

    // Add env: entries if provided
    let env_with_extra = if let Some(named_map) = named {
        if let Some(env_thunk) = named_map.get("env") {
            let env_val = materialize(env_thunk, Some(&call_span), &ctx)?;
            let env_dict = match env_val {
                Value::Dict(d) => d,
                Value::Overlay(l, r) => {
                    crate::builtins::flatten_overlay(&l, &r, "eval env:", &ctx, call_span)?
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval env:".to_string(),
                        "Dict",
                        env_val.type_name(),
                        call_span,
                    )
                    .into());
                }
            };

            let child_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(&base_env))));
            for (key, val_thunk_id) in env_dict {
                if let Key::String(name) = key {
                    let val_thunk = ctx.get_thunk(val_thunk_id);
                    child_env.borrow_mut().insert(name, val_thunk);
                }
            }
            child_env
        } else {
            base_env
        }
    } else {
        base_env
    };

    // Add "%" = %: if provided
    let final_env = if let Some(named_map) = named {
        if let Some(percent_thunk) = named_map.get("%") {
            let child_env = Rc::new(RefCell::new(Environment::with_parent(Rc::clone(
                &env_with_extra,
            ))));
            child_env
                .borrow_mut()
                .insert("%".to_string(), Rc::clone(percent_thunk));
            child_env
        } else {
            env_with_extra
        }
    } else {
        env_with_extra
    };

    // Call eval_expressions
    crate::eval::eval_expressions(&exprs, final_env, &ctx)
}

/// `eval-types`: Evaluate AST expression nodes in the type stage environment.
///
/// Positional arg: `exprs` — a positional Dict of expression AST dicts.
///
/// No `%:` or `env:` parameters. Uses `ctx.config.type_stage_env` as the base environment,
/// which contains type-level builtins only (no IO, no caps, no runtime API).
pub(crate) fn builtin_eval_types(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    reject_named("eval-types", named, call_span)?;

    // Get the positional dict of expression AST dicts.
    // reject_named above already validated no named args; use a manual arity check
    // here so the intent is clear (expect_one_arg would re-check named redundantly).
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }
    let exprs_dict_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let exprs_dict = match &exprs_dict_val {
        Value::Dict(d) => d,
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "eval-types".to_string(),
                "Dict",
                exprs_dict_val.type_name(),
                call_span,
            )
            .into());
        }
    };

    // Extract expression AST dicts from the positional dict
    let mut exprs = Vec::new();
    for i in 0.. {
        match exprs_dict.get(&Key::Int(i)) {
            Some(thunk_id) => {
                let thunk = ctx.get_thunk(*thunk_id);
                let expr_val = materialize(&thunk, Some(&call_span), &ctx)?;
                let expr_ast = crate::ast_dict::dict_to_ast(&expr_val, &ctx).map_err(|e| {
                    EvalError::user_error(format!("eval-types: expression {}: {}", i, e), call_span)
                })?;
                // dict_to_ast already returns Spanned<Expr>, don't wrap again
                exprs.push(expr_ast);
            }
            None => break,
        }
    }

    // Use type_stage_env if available, otherwise fall back to stdlib_env
    let base_env = ctx
        .config
        .type_stage_env
        .as_ref()
        .map(Rc::clone)
        .unwrap_or_else(|| Rc::clone(&ctx.config.stdlib_env));

    // Call eval_expressions
    crate::eval::eval_expressions(&exprs, base_env, &ctx)
}

/// `gensym`: Generate a unique symbol name for macro hygiene.
///
/// - Zero args: returns `":gensym:N"` where N is a global monotonic counter.
/// - One arg (prefix string): returns `":prefix:N"` where prefix is the argument.
///
/// The `:` prefix ensures these names cannot collide with user-written identifiers
/// (`:` is not allowed in bare word identifiers).
pub(crate) fn builtin_gensym(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    use std::sync::atomic::{AtomicU64, Ordering};

    // Global counter for gensym IDs
    static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);

    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    reject_named("gensym", named, call_span)?;

    // Accept 0 or 1 positional arguments
    let prefix = if args.is_empty() {
        "gensym".to_string()
    } else if args.len() == 1 {
        // Materialize the first argument to get the prefix string
        let prefix_val = materialize(&args[0], Some(&call_span), &ctx)?;
        match prefix_val {
            Value::String { source, start, end } => source[start..end].to_string(),
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "gensym".to_string(),
                    "String",
                    prefix_val.type_name(),
                    call_span,
                )
                .into());
            }
        }
    } else {
        return Err(EvalError::user_error(
            format!("gensym takes 0 or 1 arguments, got {}", args.len()),
            call_span,
        )
        .into());
    };

    let id = GENSYM_COUNTER.fetch_add(1, Ordering::SeqCst);
    let name = format!(":{}:{}", prefix, id);
    ok_val(string_val(&name), call_span)
}

/// `macro-injects`: Returns the inject: default name for a macro.
///
/// Takes one argument (macro name as String), returns the inject: default name (String)
/// if the macro has an `inject:` declaration, or Null (empty dict) if not.
///
/// This is a reflection primitive for anaphoric macros per macros-v2.md §inject:.
/// Enables runtime introspection of macro inject defaults for documentation tools.
///
/// Example:
///   [macro-injects "aif"]   # → "it"  (if aif has inject: it)
///   [macro-injects "swap"]  # → null  (if swap uses only gensym hygiene)
///
/// Non-materializing: only inspects the macro_injects_map from EvalConfig.
pub(crate) fn builtin_macro_injects(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    let macro_name_val =
        crate::builtins::expect_one_arg("macro-injects", args, named, &ctx, call_span)?;
    let macro_name = require_string("macro-injects", macro_name_val, args[0].span)?;

    // Look up the macro in the inject map
    match ctx.config.macro_injects_map.get(&macro_name) {
        Some(inject_default) => ok_val(string_val(inject_default), call_span),
        None => ok_val(Value::Dict(IndexMap::new()), call_span), // Null = empty dict
    }
}

/// `decimal`: Parse a string as an exact base-10 decimal (rust_decimal::Decimal).
/// Returns Value::Decimal. Error on invalid format.
pub(crate) fn builtin_decimal(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("decimal", args, named, &ctx, call_span)?;
    match val {
        Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[start..end];
            use std::str::FromStr;
            match rust_decimal::Decimal::from_str(s) {
                Ok(d) => ok_val(Value::Decimal(d), call_span),
                Err(e) => Err(EvalError::internal(
                    format!("decimal: cannot parse \"{s}\": {e}"),
                    call_span,
                )
                .into()),
            }
        }
        Value::Int(n) => ok_val(Value::Decimal(rust_decimal::Decimal::from(n)), call_span),
        _ => Err(EvalError::type_mismatch("String or Int", &type_name(&val), call_span).into()),
    }
}

/// `big-int`: Convert an Int or String to a BigInt (arbitrary-precision integer).
pub(crate) fn builtin_big_int(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("big-int", args, named, &ctx, call_span)?;
    match val {
        Value::Int(n) => ok_val(Value::BigInt(num_bigint::BigInt::from(n)), call_span),
        Value::String {
            ref source,
            start,
            end,
        } => {
            let s = &source[start..end];
            match s.parse::<num_bigint::BigInt>() {
                Ok(n) => ok_val(Value::BigInt(n), call_span),
                Err(e) => Err(EvalError::internal(
                    format!("big-int: cannot parse \"{s}\": {e}"),
                    call_span,
                )
                .into()),
            }
        }
        _ => Err(EvalError::type_mismatch("Int or String", &type_name(&val), call_span).into()),
    }
}

/// `type-of`: takes 1 arg, materializes it, returns the type name.
/// Both `Function` and `Builtin` return "Function" (from the user's perspective).
/// Returns "Dict" for all dicts, with no distinction between list-like (sequential int keys)
/// and map-like dicts — the type system does not track key structure at runtime.
/// Inherently materializing: must inspect value variant to determine type.
pub(crate) fn builtin_type_of(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("type-of", args, named, &ctx, call_span)?;
    let name = match val.type_name() {
        "Builtin" => "Function",
        other => other,
    };
    ok_val(string_val(name), call_span)
}

/// `ast-of`: returns metadata about a value's AST or thunk state.
///
/// This builtin does NOT materialize its argument, making it safe to use
/// for introspection of unevaluated expressions.
///
/// - Materialized values → metadata dict based on value type
/// - Unevaluated thunks → AST dict from stored expression
/// - PendingCall/PendingBuiltin → descriptor dict
/// - Other thunk states → descriptor dict with state name
pub(crate) fn builtin_ast_of(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Reject named args and ensure exactly 1 arg
    crate::builtins::reject_named("ast-of", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    let thunk = &args[0];

    // Inspect the thunk state WITHOUT forcing it
    let state = thunk.state();

    let dict_entries = match &*state {
        crate::value::ThunkState::Materialized(val) => {
            // Value is already materialized — return Variant(type_name, payload_dict),
            // mirroring the Unevaluated branch which returns Variant(ast_tag, Dict(fields)).
            match val {
                crate::value::Value::Function {
                    params, annotation, ..
                } => {
                    let mut entries = IndexMap::new();

                    // Add params field as a list of param names
                    let param_names: Vec<ThunkId> = params
                        .iter()
                        .map(|p| {
                            ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                                string_val(&p.name),
                                call_span,
                            )))
                        })
                        .collect();

                    if !param_names.is_empty() {
                        let params_seq = param_names.into_iter().rev().fold(
                            ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                                crate::value::Value::Dict(IndexMap::new()),
                                call_span,
                            ))),
                            |tail, head| {
                                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                                    crate::value::Value::Seq { head, tail },
                                    call_span,
                                )))
                            },
                        );
                        entries.insert(crate::value::Key::String("params".into()), params_seq);
                    }

                    // Add doc field if present
                    if let Some(ann) = annotation {
                        if let Some(ref doc_str) = ann.doc {
                            entries.insert(
                                crate::value::Key::String("doc".into()),
                                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                                    string_val(doc_str),
                                    call_span,
                                ))),
                            );
                        }
                    }

                    let payload_thunk =
                        ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                            crate::value::Value::Dict(entries),
                            call_span,
                        )));
                    return ok_val(
                        crate::value::Value::Variant {
                            tag: "Function".to_string(),
                            payload: Some(payload_thunk),
                        },
                        call_span,
                    );
                }
                other => {
                    // For all other materialized types, return Variant(type_name, empty_dict)
                    let payload_thunk =
                        ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                            crate::value::Value::Dict(IndexMap::new()),
                            call_span,
                        )));
                    return ok_val(
                        crate::value::Value::Variant {
                            tag: other.type_name().to_string(),
                            payload: Some(payload_thunk),
                        },
                        call_span,
                    );
                }
            }
        }
        crate::value::ThunkState::Unevaluated { expr, .. } => {
            // Thunk contains an unevaluated expression — convert AST to variant WITHOUT evaluating
            let opts = crate::ast_dict::AstToDictOpts::default();
            let ast_thunk = crate::ast_dict::ast_to_dict_expr(expr, &opts, &ctx)?;
            // The ast_to_dict_expr now returns a Variant(tag, Dict), so just return the whole thing
            match ast_thunk.try_get_materialized() {
                Some(Value::Variant { .. }) => {
                    // Return the variant directly (ast-of now returns Variant, not Dict)
                    return Ok(ast_thunk);
                }
                Some(Value::Dict(_)) => {
                    // Legacy path: if somehow we get a dict, return it
                    return Ok(ast_thunk);
                }
                _ => {
                    // This shouldn't happen — ast_to_dict_expr always returns Materialized(Variant)
                    return Err(EvalError::internal(
                        "ast_to_dict_expr did not return a materialized variant or dict"
                            .to_string(),
                        call_span,
                    )
                    .into());
                }
            }
        }
        crate::value::ThunkState::PendingBuiltin { def, .. } => {
            // Pending builtin call — return descriptor
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("pending-builtin"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("name".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val(def.name),
                    call_span,
                ))),
            );
            entries
        }
        crate::value::ThunkState::PendingCall { .. } => {
            // Pending function call — return descriptor
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("pending-call"),
                    call_span,
                ))),
            );
            entries
        }
        crate::value::ThunkState::Guarded { .. } => {
            // Type-guarded thunk — return descriptor
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("guarded"),
                    call_span,
                ))),
            );
            entries
        }
        crate::value::ThunkState::InProgress => {
            // Circular dependency detected — return descriptor
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("in-progress"),
                    call_span,
                ))),
            );
            entries
        }
        crate::value::ThunkState::Failed(err) => {
            // Failed evaluation — return error descriptor
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("failed"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("error".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val(&err.kind.to_string()),
                    call_span,
                ))),
            );
            entries
        }
        crate::value::ThunkState::Placeholder => {
            // Placeholder state (should not be observable in user code)
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Rc::new(crate::value::Thunk::new_materialized(
                    string_val("placeholder"),
                    call_span,
                ))),
            );
            entries
        }
    };

    ok_val(crate::value::Value::Dict(dict_entries), call_span)
}

/// `llt-repr`: takes 1 arg, deep-materializes it, returns its LLT display string representation.
/// This is the programmatic equivalent of the LLT display format (Int(42), Dict({...}), etc.).
/// Used by the `-o llt` output formatter.
pub(crate) fn builtin_llt_repr(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("llt-repr", args, named, &ctx, call_span)?;
    // Deep-materialize the value first (value_to_display_string requires it)
    let deep_val = crate::eval_deep::deep_materialize(&val, &ctx, Some(&call_span))?;
    // Convert to display string
    let display_str = crate::value_to_display_string(&deep_val, &ctx)
        .map_err(|e| EvalError::internal(format!("llt-repr: {}", e.kind), call_span))?;
    ok_val(string_val(&display_str), call_span)
}

/// `tag-of`: Return the tag of a Variant as a String.
pub(crate) fn builtin_tag_of(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("tag-of", args, named, &ctx, call_span)?;
    match val {
        Value::Variant { tag, .. } => ok_val(string_val(&tag), call_span),
        _ => Err(Box::new(EvalError::type_mismatch(
            "Variant",
            val.type_name(),
            call_span,
        ))),
    }
}

/// `variant`: Create a variant with the given tag and optional payload.
///
/// Forms:
/// - `[variant "Tag"]` — unit variant (no payload)
/// - `[variant "Tag" payload]` — variant with payload (stored as ThunkId)
pub(crate) fn builtin_variant(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    crate::builtins::reject_named("variant", named, call_span)?;

    match args.len() {
        1 => {
            // Unit variant: [variant "Tag"]
            let tag_thunk = &args[0];
            let tag_val = crate::eval::materialize(tag_thunk, Some(&call_span), &ctx)?;
            match tag_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => {
                    let tag = &source[start..end];
                    ok_val(
                        Value::Variant {
                            tag: tag.to_string(),
                            payload: None,
                        },
                        call_span,
                    )
                }
                _ => Err(Box::new(EvalError::type_mismatch(
                    "String",
                    tag_val.type_name(),
                    call_span,
                ))),
            }
        }
        2 => {
            // Variant with payload: [variant "Tag" payload]
            let tag_thunk = &args[0];
            let payload_thunk = &args[1];
            let tag_val = crate::eval::materialize(tag_thunk, Some(&call_span), &ctx)?;
            match tag_val {
                Value::String {
                    ref source,
                    start,
                    end,
                } => {
                    let tag = &source[start..end];
                    // Store the payload as a ThunkId (lazy, won't be forced until accessed)
                    let payload_id = ctx.alloc_thunk(Rc::clone(payload_thunk));
                    ok_val(
                        Value::Variant {
                            tag: tag.to_string(),
                            payload: Some(payload_id),
                        },
                        call_span,
                    )
                }
                _ => Err(Box::new(EvalError::type_mismatch(
                    "String",
                    tag_val.type_name(),
                    call_span,
                ))),
            }
        }
        n => Err(Box::new(EvalError::arity_mismatch(2, n, call_span))),
    }
}

/// `int?`: Return true if the argument is an Int.
pub(crate) fn builtin_int_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("int?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Int(_))), call_span)
}

/// `float?`: Return true if the argument is a Float.
pub(crate) fn builtin_float_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("float?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Float(_))), call_span)
}

// num? is implemented in LLT as [or [int? x] [float? x]] — see stdlib/prelude.llt

/// `str?`: Return true if the argument is a String.
pub(crate) fn builtin_str_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("str?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::String { .. })), call_span)
}

/// `bool?`: Return true if the argument is a Bool.
pub(crate) fn builtin_bool_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("bool?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Bool(_))), call_span)
}

/// `bytes?`: Return true if the argument is Bytes.
pub(crate) fn builtin_bytes_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("bytes?", args, named, &ctx, call_span)?;
    ok_val(Value::Bool(matches!(val, Value::Bytes { .. })), call_span)
}

/// `null?`: Return true if the argument is Null (represented as an empty Dict).
pub(crate) fn builtin_null_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("null?", args, named, &ctx, call_span)?;
    let is_null = match val {
        Value::Dict(map) => map.is_empty(),
        Value::Overlay(l, r) => {
            let map = crate::builtins::flatten_overlay(&l, &r, "null?", &ctx, call_span)?;
            map.is_empty()
        }
        _ => false,
    };
    ok_val(Value::Bool(is_null), call_span)
}

/// `dict?`: Return true if the argument is a Dict (including lists and null).
pub(crate) fn builtin_dict_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("dict?", args, named, &ctx, call_span)?;
    ok_val(
        Value::Bool(matches!(val, Value::Dict(_) | Value::Overlay(..))),
        call_span,
    )
}

// record? and map? are implemented in LLT as aliases of dict? — see stdlib/prelude.llt

/// `fn?`: Return true if the argument is callable (Function or Builtin).
pub(crate) fn builtin_fn_check(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("fn?", args, named, &ctx, call_span)?;
    ok_val(
        Value::Bool(matches!(val, Value::Function { .. } | Value::Builtin(_))),
        call_span,
    )
}

/// Helper for runtime type name extraction.
fn type_name(val: &Value) -> String {
    match val {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::String { .. } => "String",
        Value::Bool(_) => "Bool",
        Value::Bytes { .. } => "Bytes",
        Value::Dict(_) | Value::Overlay(..) => "Dict",
        Value::Seq { .. } => "Seq",
        Value::Function { .. } => "Function",
        Value::Builtin(_) => "Builtin",
        Value::Proxy { .. } => "Proxy",
        Value::DirCap { .. } | Value::RevocableDirCap { .. } => "DirCap",
        Value::NetCap(_) => "NetCap",
        Value::Handle { .. } => "Handle",
        Value::WriteHandle { .. } => "WriteHandle",
        Value::Variant { tag, .. } => tag.as_str(),
        Value::Decimal(_) => "Decimal",
        Value::BigInt(_) => "BigInt",
        Value::Uri { .. } => "Uri",
        Value::Timestamp(_) => "Timestamp",
        Value::Duration(_) => "Duration",
        Value::ClockCap(_) => "ClockCap",
        Value::Timezone(_) => "Timezone",
        Value::QuicSession(_) => "QuicSession",
        Value::Http2Session { .. } => "Http2Session",
        Value::Http3Session(_) => "Http3Session",
        Value::QuicDatagramHandle(_) => "QuicDatagramHandle",
        Value::DatagramHandle { .. } => "DatagramHandle",
    }
    .to_string()
}

/// Convert a `serde_json::Value` into an LLT `Value`.
///
/// JSON null maps to an empty dict, arrays map to integer-keyed dicts,
/// and objects map to string-keyed dicts. Numbers are converted to `Int`
/// when they fit in i64, otherwise `Float`.
pub fn json_to_value(
    json: &serde_json::Value,
    depth: usize,
    span: Span,
    ctx: &Rc<crate::eval::EvalContext>,
) -> EvalResult<Rc<Thunk>> {
    if depth > JSON_DEPTH_LIMIT {
        return Err(EvalError::json_depth_exceeded(JSON_DEPTH_LIMIT, span).into());
    }
    match json {
        serde_json::Value::Null => ok_val(Value::Dict(IndexMap::new()), span),
        serde_json::Value::Bool(b) => ok_val(Value::Bool(*b), span),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ok_val(Value::Int(i), span)
            } else if let Some(f) = n.as_f64() {
                if !f.is_finite() {
                    // JSON does not support NaN or Infinity, but some parsers
                    // (or manual serde_json::Number construction) can produce
                    // non-finite values. Reject them explicitly.
                    Err(EvalError::float_not_finite("from-json".to_string(), f, span).into())
                } else {
                    ok_val(Value::Float(f), span)
                }
            } else {
                // Unreachable with default serde_json: as_f64() covers all
                // non-i64 numbers. Return error instead of panicking.
                Err(EvalError::json_range(span).into())
            }
        }
        serde_json::Value::String(s) => ok_val(string_val(s), span),
        serde_json::Value::Array(arr) => {
            if arr.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: array exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let thunk = json_to_value(item, depth + 1, span, ctx)?;
                let thunk_id = ctx.alloc_thunk(thunk);
                map.insert(
                    Key::Int(i64::try_from(i).map_err(|_| {
                        EvalError::internal("collection index overflow".to_string(), span)
                    })?),
                    thunk_id,
                );
            }
            ok_val(Value::Dict(map), span)
        }
        serde_json::Value::Object(obj) => {
            if obj.len() > MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    format!(
                        "from-json: object exceeds maximum collection size ({})",
                        MAX_COLLECT_SIZE
                    ),
                    span,
                )
                .into());
            }
            let mut map = IndexMap::with_capacity(obj.len());
            for (k, v) in obj {
                let thunk = json_to_value(v, depth + 1, span, ctx)?;
                let thunk_id = ctx.alloc_thunk(thunk);
                map.insert(Key::String(k.clone()), thunk_id);
            }
            ok_val(Value::Dict(map), span)
        }
    }
}

/// `from-json`: takes 1 arg (String containing JSON), parses into LLT value.
/// Inherently materializing: must parse entire JSON string to construct value.
pub(crate) fn builtin_from_json(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("from-json", args, named, &ctx, call_span)?;
    let json_str = require_string("from-json", val, args[0].span)?;
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| EvalError::json_parse(e.to_string(), call_span))?;
    json_to_value(&parsed, 0, call_span, &ctx)
}

// parse_integrity_hash deleted along with builtin_include.

/// Compute the blake3 hash of `bytes` and return a lowercase hex string.
pub(crate) fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// `blake3`: compute the blake3 hash of a String argument.
///
/// Takes 1 positional arg (String) and returns the lowercase hex digest as a String.
/// The string is encoded as UTF-8 bytes before hashing.
///
/// Example: `[blake3 "hello"]` → `"ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"`
pub(crate) fn builtin_blake3(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("blake3", args, named, &ctx, call_span)?;
    let s = require_string("blake3", val, args[0].span)?;
    let hex = blake3_hex(s.as_bytes());
    ok_val(string_val(hex.as_str()), call_span)
}

/// `cap-identity`: return the stable identity string `"dev:ino"` of a DirCap.
///
/// Takes 1 positional arg (DirCap or RevocableDirCap) and returns a String of the form
/// `"<dev>:<ino>"` derived from `fstat` on the directory's open file descriptor.
/// On non-Unix platforms, falls back to `"0:<hash>"` using a path hash.
///
/// This stable identity can be combined with source text to form a content-addressed
/// cache key: `blake3(cap-identity + "|" + source)`.
pub(crate) fn builtin_cap_identity(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("cap-identity", args, named, &ctx, call_span)?;

    let dir = match &val {
        Value::DirCap { dir, .. } => Rc::clone(dir),
        Value::RevocableDirCap { inner, revoked, .. } => {
            if revoked.get() {
                return Err(EvalError::internal(
                    "cap-identity: capability has been revoked".to_string(),
                    call_span,
                )
                .into());
            }
            Rc::clone(inner)
        }
        _ => {
            return Err(EvalError::type_mismatch("DirCap", val.type_name(), args[0].span).into());
        }
    };

    // Get the identity string from the directory's metadata.
    // On Unix: "dev:ino" from fstat on the O_DIRECTORY fd.
    // On non-Unix: "0:<hash>" as a best-effort fallback.
    #[cfg(unix)]
    let identity = {
        use cap_std::fs::MetadataExt;
        let meta = dir.open(".").and_then(|f| f.metadata()).map_err(|e| {
            EvalError::internal(
                format!("cap-identity: failed to stat directory: {e}"),
                call_span,
            )
        })?;
        format!("{}:{}", meta.dev(), meta.ino())
    };

    #[cfg(not(unix))]
    let identity = {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        // On non-Unix, use the Debug representation of the Dir as a stable-ish id.
        let mut hasher = DefaultHasher::new();
        format!("{:?}", *dir).hash(&mut hasher);
        format!("0:{}", hasher.finish())
    };

    ok_val(string_val(identity.as_str()), call_span)
}

/// `load`: parse a source String into a file AST dict (same format as `ast-of`/`ast_to_dict`).
///
/// Takes 1 positional arg (String source text) and an optional `name:` named arg (String,
/// used as the provenance hint for error messages).
///
/// Pipeline: parse → macro-expand → desugar → resolve → ast_to_dict.
/// Returns a Dict in the canonical AST schema (type: "file", schema-version: 1, documents: [...]).
///
/// This is the primitive underlying the `include` pipeline in the include-decomposition design.
pub(crate) fn builtin_load(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    // Extract optional name: named arg
    let name_hint: Option<String> = if let Some(named_map) = named {
        // Reject unknown named args
        for key in named_map.keys() {
            if key != "name" {
                return Err(EvalError::named_arg_rejected("load".to_string(), call_span).into());
            }
        }
        if let Some(name_thunk) = named_map.get("name") {
            let name_val = materialize(name_thunk, Some(&call_span), &ctx)?;
            let name_str = require_string("load", name_val, name_thunk.span)?;
            Some(name_str)
        } else {
            None
        }
    } else {
        None
    };

    // Extract source string
    let source_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let source = require_string("load", source_val, args[0].span)?;

    // Use name hint for error messages
    let display_name = name_hint.as_deref().unwrap_or("<load>");

    // Parse
    let parsed = crate::parser::parse(&source).map_err(|e| {
        EvalError::include_parse_failed(display_name.to_string(), e.to_string(), call_span)
    })?;

    // Macro expansion — use ctx.config.base_dir as the directory context for macro includes.
    let expand_result =
        crate::expand::expand_macros(parsed.file, ctx.config.no_fs, &ctx.config.base_dir).map_err(
            |e| {
                EvalError::include_parse_failed(
                    display_name.to_string(),
                    format!("macro expansion error: {e}"),
                    call_span,
                )
            },
        )?;
    let mut file = expand_result.file;

    // Desugar $_ implicit lambdas
    crate::desugar::desugar_file(&mut file.node);

    // Variable resolution
    crate::resolve::resolve_file(&file.node);

    // Convert to AST dict
    let opts = crate::ast_dict::AstToDictOpts {
        source: Some(&source),
        comments: None,
    };
    crate::ast_dict::ast_to_dict(&file.node, &opts, &ctx)
}

/// `include-cache-get`: look up the string-keyed include cache by blake3 key.
///
/// Takes 1 positional arg (String key). Returns:
/// - `[Missing]`       — key not in cache
/// - `[Pending]`       — key is marked as in-progress (cycle detection)
/// - `[Cached value]`  — cached result thunk
pub(crate) fn builtin_include_cache_get(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;
    let val = crate::builtins::expect_one_arg("include-cache-get", args, named, &ctx, call_span)?;
    let key = require_string("include-cache-get", val, args[0].span)?;

    let entry = ctx.state.borrow().string_include_cache.get(&key).cloned();

    match entry {
        None => ok_val(
            Value::Variant {
                tag: "Missing".to_string(),
                payload: None,
            },
            call_span,
        ),
        Some(crate::eval::IncludeCacheEntry::Missing) => ok_val(
            Value::Variant {
                tag: "Missing".to_string(),
                payload: None,
            },
            call_span,
        ),
        Some(crate::eval::IncludeCacheEntry::Pending) => ok_val(
            Value::Variant {
                tag: "Pending".to_string(),
                payload: None,
            },
            call_span,
        ),
        Some(crate::eval::IncludeCacheEntry::Cached(thunk)) => {
            let payload_id = ctx.alloc_thunk(Rc::clone(&thunk));
            ok_val(
                Value::Variant {
                    tag: "Cached".to_string(),
                    payload: Some(payload_id),
                },
                call_span,
            )
        }
    }
}

/// `include-cache-put`: insert or update the string-keyed include cache.
///
/// Takes 2 positional args: String key and a value.
/// The value must be a `[Missing]`, `[Pending]`, or `[Cached x]` Variant.
/// Returns the stored value (pass-through).
pub(crate) fn builtin_include_cache_put(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    reject_named("include-cache-put", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let key_val = materialize(&args[0], Some(&call_span), &ctx)?;
    let key = require_string("include-cache-put", key_val, args[0].span)?;

    // The second arg is the entry variant: [Missing], [Pending], or [Cached value]
    let entry_val = materialize(&args[1], Some(&call_span), &ctx)?;

    let entry = match &entry_val {
        Value::Variant { tag, payload } => match tag.as_str() {
            "Missing" => crate::eval::IncludeCacheEntry::Missing,
            "Pending" => crate::eval::IncludeCacheEntry::Pending,
            "Cached" => {
                let payload_thunk = match payload {
                    Some(id) => ctx.get_thunk(*id),
                    None => {
                        return Err(EvalError::type_mismatch_ctx(
                            "include-cache-put".to_string(),
                            "[Cached value]",
                            "[Cached]",
                            args[1].span,
                        )
                        .into())
                    }
                };
                crate::eval::IncludeCacheEntry::Cached(payload_thunk)
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "include-cache-put".to_string(),
                    "[Missing] | [Pending] | [Cached value]",
                    &format!("[{other}]"),
                    args[1].span,
                )
                .into())
            }
        },
        _ => {
            return Err(EvalError::type_mismatch_ctx(
                "include-cache-put".to_string(),
                "[Missing] | [Pending] | [Cached value]",
                entry_val.type_name(),
                args[1].span,
            )
            .into())
        }
    };

    ctx.state
        .borrow_mut()
        .string_include_cache
        .insert(key, entry);

    // Return [] (empty dict). The entry was stored in the cache; the return value
    // is used in sequential `let*` expressions that require Dict.
    ok_val(Value::Dict(IndexMap::new()), call_span)
}

// builtin_include deleted — file inclusion is now a LLT-level operation
// using the `load` primitive. See TODO.md include-decomp-prelude for the
// LLT-level include function that replaces this builtin.

/// `validate`: Validate a value against a schema dict.
///
/// Schema → Data → Data (pass-through on success) or SchemaViolation error.
///
/// Schema keys:
/// - `type`: expected type name (String: "Int", "String", "Bool", "Dict", "Seq", etc.)
/// - `min`, `max`: numeric range constraints (Int or Float)
/// - `min-length`, `max-length`: string or sequence length constraints (Int)
/// - `pattern`: regex pattern for strings (String)
/// - `required`: whether field is required (Bool)
/// - `default`: default value if field is missing (Any)
/// - `items`: schema for sequence elements (Dict)
/// - `fields`: schema for dict fields (Dict mapping field names to field schemas)
/// - `enum`: list of allowed values (Seq)
///
/// Returns the data value unchanged on success, throws SchemaViolation with all violations on failure.
pub(crate) fn builtin_validate(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        call_span,
        ctx,
    } = ctx_arg;

    // Expect exactly 2 args: schema, data
    let (schema, data) = expect_two_args("validate", args, named, &ctx, call_span)?;

    // Schema must be a Dict
    let schema_dict = match schema {
        Value::Dict(ref d) => d.clone(),
        Value::Overlay(..) => {
            // Materialize Overlay to Dict before validation
            let schema_thunk_id =
                ctx.alloc_thunk(Rc::new(Thunk::new_materialized(schema.clone(), call_span)));
            let schema_thunk = ctx.get_thunk(schema_thunk_id);
            let materialized = materialize(&schema_thunk, Some(&call_span), &ctx)?;
            match materialized {
                Value::Dict(d) => d,
                _ => {
                    return Err(EvalError::type_mismatch(
                        "Dict (schema)",
                        &type_name(&materialized),
                        call_span,
                    )
                    .into());
                }
            }
        }
        _ => {
            return Err(
                EvalError::type_mismatch("Dict (schema)", &type_name(&schema), call_span).into(),
            );
        }
    };

    // Collect violations
    let mut violations = Vec::new();
    validate_value(&schema_dict, &data, "", &mut violations, &ctx, call_span)?;

    if violations.is_empty() {
        // Success: return data unchanged
        Ok(Rc::new(Thunk::new_materialized(data, call_span)))
    } else {
        // Failure: throw SchemaViolation with all violations
        Err(EvalError::schema_violation(violations, call_span).into())
    }
}

/// Helper: materialize and extract exactly 2 positional arguments, no named args.
fn expect_two_args(
    name: &str,
    args: &[Rc<Thunk>],
    named: Option<&IndexMap<String, Rc<Thunk>>>,
    ctx: &Rc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<(Value, Value)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    if named.is_some() && !named.unwrap().is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }

    let val1 = materialize(&args[0], Some(&call_span), ctx)?;
    let val2 = materialize(&args[1], Some(&call_span), ctx)?;

    Ok((val1, val2))
}

/// Recursive validation helper.
///
/// `path` is the dot-separated field path (e.g., "user.address.zip").
/// `violations` accumulates all violations found.
fn validate_value(
    schema: &IndexMap<Key, ThunkId>,
    data: &Value,
    path: &str,
    violations: &mut Vec<(String, String)>,
    ctx: &Rc<crate::eval::EvalContext>,
    span: Span,
) -> EvalResult<()> {
    // Check `type` constraint
    if let Some(&type_thunk_id) = schema.get(&Key::String("type".to_string())) {
        let type_thunk = ctx.get_thunk(type_thunk_id);
        let type_val = materialize(&type_thunk, Some(&span), ctx)?;
        if let Value::String {
            ref source,
            start,
            end,
        } = type_val
        {
            let expected_type = &source[start..end];
            let actual_type = type_name(data);
            if expected_type != actual_type {
                violations.push((
                    path.to_string(),
                    format!("expected type {}, got {}", expected_type, actual_type),
                ));
            }
        }
    }

    // Check numeric range constraints (min, max)
    if let Some(&min_thunk_id) = schema.get(&Key::String("min".to_string())) {
        let min_thunk = ctx.get_thunk(min_thunk_id);
        let min_val = materialize(&min_thunk, Some(&span), ctx)?;
        match (data, &min_val) {
            (Value::Int(n), Value::Int(min)) if n < min => {
                violations.push((path.to_string(), format!("must be >= {}", min)));
            }
            (Value::Float(n), Value::Float(min)) if n < min => {
                violations.push((path.to_string(), format!("must be >= {}", min)));
            }
            (Value::Int(n), Value::Float(min)) if (*n as f64) < *min => {
                violations.push((path.to_string(), format!("must be >= {}", min)));
            }
            (Value::Float(n), Value::Int(min)) if *n < (*min as f64) => {
                violations.push((path.to_string(), format!("must be >= {}", min)));
            }
            _ => {}
        }
    }

    if let Some(&max_thunk_id) = schema.get(&Key::String("max".to_string())) {
        let max_thunk = ctx.get_thunk(max_thunk_id);
        let max_val = materialize(&max_thunk, Some(&span), ctx)?;
        match (data, &max_val) {
            (Value::Int(n), Value::Int(max)) if n > max => {
                violations.push((path.to_string(), format!("must be <= {}", max)));
            }
            (Value::Float(n), Value::Float(max)) if n > max => {
                violations.push((path.to_string(), format!("must be <= {}", max)));
            }
            (Value::Int(n), Value::Float(max)) if (*n as f64) > *max => {
                violations.push((path.to_string(), format!("must be <= {}", max)));
            }
            (Value::Float(n), Value::Int(max)) if *n > (*max as f64) => {
                violations.push((path.to_string(), format!("must be <= {}", max)));
            }
            _ => {}
        }
    }

    // Check string/sequence length constraints
    if let Some(&min_len_thunk_id) = schema.get(&Key::String("min-length".to_string())) {
        let min_len_thunk = ctx.get_thunk(min_len_thunk_id);
        let min_len_val = materialize(&min_len_thunk, Some(&span), ctx)?;
        if let Value::Int(min_len) = min_len_val {
            let actual_len = match data {
                Value::String {
                    source: _,
                    start,
                    end,
                } => Some((end - start) as i64),
                Value::Dict(d) => Some(d.len() as i64),
                Value::Seq { .. } => {
                    // For Seq, we'd need to walk the spine to count, which is expensive.
                    // Skip for now; document limitation.
                    None
                }
                _ => None,
            };
            if let Some(len) = actual_len {
                if len < min_len {
                    violations.push((path.to_string(), format!("length must be >= {}", min_len)));
                }
            }
        }
    }

    if let Some(&max_len_thunk_id) = schema.get(&Key::String("max-length".to_string())) {
        let max_len_thunk = ctx.get_thunk(max_len_thunk_id);
        let max_len_val = materialize(&max_len_thunk, Some(&span), ctx)?;
        if let Value::Int(max_len) = max_len_val {
            let actual_len = match data {
                Value::String {
                    source: _,
                    start,
                    end,
                } => Some((end - start) as i64),
                Value::Dict(d) => Some(d.len() as i64),
                Value::Seq { .. } => None,
                _ => None,
            };
            if let Some(len) = actual_len {
                if len > max_len {
                    violations.push((path.to_string(), format!("length must be <= {}", max_len)));
                }
            }
        }
    }

    // Check pattern constraint (for strings)
    if let Some(&pattern_thunk_id) = schema.get(&Key::String("pattern".to_string())) {
        let pattern_thunk = ctx.get_thunk(pattern_thunk_id);
        let pattern_val = materialize(&pattern_thunk, Some(&span), ctx)?;
        if let Value::String {
            ref source,
            start,
            end,
        } = pattern_val
        {
            let pattern_str = &source[start..end];
            if let Some(data_str) = data.as_str() {
                match regex::Regex::new(pattern_str) {
                    Ok(re) => {
                        if !re.is_match(data_str) {
                            violations.push((
                                path.to_string(),
                                format!("must match pattern: {}", pattern_str),
                            ));
                        }
                    }
                    Err(_) => {
                        violations.push((
                            path.to_string(),
                            format!("invalid regex pattern: {}", pattern_str),
                        ));
                    }
                }
            }
        }
    }

    // Check enum constraint
    if let Some(&enum_thunk_id) = schema.get(&Key::String("enum".to_string())) {
        let enum_thunk = ctx.get_thunk(enum_thunk_id);
        let enum_val = materialize(&enum_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref enum_dict) = enum_val {
            let mut found = false;
            for (_key, &val_thunk_id) in enum_dict {
                let val_thunk = ctx.get_thunk(val_thunk_id);
                let val = materialize(&val_thunk, Some(&span), ctx)?;
                if values_equal(&val, data) {
                    found = true;
                    break;
                }
            }
            if !found {
                violations.push((path.to_string(), "value not in allowed enum".to_string()));
            }
        }
    }

    // Check fields constraint (for dicts)
    if let Some(&fields_thunk_id) = schema.get(&Key::String("fields".to_string())) {
        let fields_thunk = ctx.get_thunk(fields_thunk_id);
        let fields_val = materialize(&fields_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref fields_schema) = fields_val {
            if let Value::Dict(ref data_dict) = data {
                // Validate each field in the schema
                for (field_key, &field_schema_thunk_id) in fields_schema {
                    let field_schema_thunk = ctx.get_thunk(field_schema_thunk_id);
                    let field_schema_val = materialize(&field_schema_thunk, Some(&span), ctx)?;
                    if let Value::Dict(ref field_schema) = field_schema_val {
                        let field_name = match field_key {
                            Key::String(s) => s.clone(),
                            Key::Int(i) => i.to_string(),
                        };

                        let field_path = if path.is_empty() {
                            field_name.clone()
                        } else {
                            format!("{}.{}", path, field_name)
                        };

                        // Check if field is required
                        let is_required = if let Some(&req_thunk_id) =
                            field_schema.get(&Key::String("required".to_string()))
                        {
                            let req_thunk = ctx.get_thunk(req_thunk_id);
                            let req_val = materialize(&req_thunk, Some(&span), ctx)?;
                            matches!(req_val, Value::Bool(true))
                        } else {
                            false
                        };

                        if let Some(&field_value_thunk_id) = data_dict.get(field_key) {
                            let field_value_thunk = ctx.get_thunk(field_value_thunk_id);
                            let field_value = materialize(&field_value_thunk, Some(&span), ctx)?;
                            validate_value(
                                field_schema,
                                &field_value,
                                &field_path,
                                violations,
                                ctx,
                                span,
                            )?;
                        } else if is_required {
                            violations.push((field_path, "required field is missing".to_string()));
                        }
                    }
                }
            }
        }
    }

    // Check items constraint (for sequences/dicts with uniform element schema)
    if let Some(&items_thunk_id) = schema.get(&Key::String("items".to_string())) {
        let items_thunk = ctx.get_thunk(items_thunk_id);
        let items_val = materialize(&items_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref items_schema) = items_val {
            match data {
                Value::Dict(ref data_dict) => {
                    for (idx, (_key, &val_thunk_id)) in data_dict.iter().enumerate() {
                        let val_thunk = ctx.get_thunk(val_thunk_id);
                        let val = materialize(&val_thunk, Some(&span), ctx)?;
                        let item_path = if path.is_empty() {
                            format!("[{}]", idx)
                        } else {
                            format!("{}[{}]", path, idx)
                        };
                        validate_value(items_schema, &val, &item_path, violations, ctx, span)?;
                    }
                }
                Value::Seq { .. } => {
                    // Would need to walk the seq spine; skip for now
                }
                _ => {}
            }
        }
    }

    Ok(())
}

/// Helper to compare two values for equality (for enum checking).
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => x == y,
        (Value::Float(x), Value::Float(y)) => (x - y).abs() < f64::EPSILON,
        (
            Value::String {
                source: s1,
                start: start1,
                end: end1,
            },
            Value::String {
                source: s2,
                start: start2,
                end: end2,
            },
        ) => s1[*start1..*end1] == s2[*start2..*end2],
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Dict(x), Value::Dict(y)) => x.is_empty() && y.is_empty(), // Null check
        _ => false,
    }
}
