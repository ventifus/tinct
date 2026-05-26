//! Type introspection, evaluation control, and meta builtins.
//!
//! These builtins provide type checking, evaluation control, AST manipulation,
//! JSON conversion, file inclusion, and schema validation.
//!
//! **Evaluation control:**
//! - `error`: Raise a user error with a custom message
//! - `builtin-macro-error`: Raise a macro error with precise span information
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

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::Span;
use crate::builtins::{
    builtin, ok_val, reject_named, require_string, JSON_DEPTH_LIMIT, MAX_COLLECT_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, materialize_sync};
use crate::eval_call::{invoke_function, CallContext};
use crate::eval_materialize::force_dict_tree;
use crate::value::{string_val, BuiltinArgs, Key, Strictness, Thunk, Value};

/// `builtin-to-json`: takes 1 arg and serializes to a JSON string.
///
/// This is the Rust-native JSON serializer used by the CLI output pipeline as a replacement
/// for the LLT-based codecs/json.llt approach. Avoids the `[include %libdir ...]` dependency
/// that was broken by the include-decomp sprint.
///
/// Returns a String containing the compact JSON representation of the value.
/// Errors on non-serializable values (Function, Builtin, Seq, NaN/Infinity floats).
pub(crate) fn builtin_to_json(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "builtin-to-json",
            &args,
            named.as_ref(),
            &ctx,
            call_span,
        )?;
        // value_to_json materializes nested values on demand via visit_value
        // LLT null compatibility: [] (empty dict) serializes as JSON null,
        // matching the behavior of the old codecs/json.llt `null?` check.
        if let crate::value::Value::Dict(ref map) = val {
            if map.is_empty() {
                return ok_val(crate::value::string_val("null"), call_span);
            }
        }
        // Serialize to JSON using the Rust-native converter.
        // value_to_json already receives call_span and threads it correctly through
        // visit_value; child thunk spans are preserved for nested errors.
        let json_val = crate::value_to_json(&val, &ctx, call_span)?;
        let json_str = json_val.to_string();
        ok_val(crate::value::string_val(&json_str), call_span)
    })
}

/// `materialize`: takes 1 arg, forces it to WHNF and returns it.
///
/// Gives users explicit control over evaluation order. Only forces to weak head normal form
/// (WHNF) — dicts remain dicts with unforced entries, not deep-forced.
pub(crate) fn builtin_force(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let forced =
            crate::builtins::expect_one_arg("materialize", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(forced, call_span)
    })
}

/// `raise`: takes 1 arg (String message), always raises.
/// Inherently materializing: constructs concrete error value.
pub(crate) fn builtin_raise(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("raise", &args, named.as_ref(), &ctx, call_span)?;
        let msg = require_string("raise", val, args[0].span)?;
        Err(EvalError::user_error(msg.to_string(), call_span).into())
    })
}

/// `builtin-macro-error`: takes 2 args (span dict, message string).
/// Creates a macro error with a precise source span from the span dict.
/// Inherently materializing: must materialize both arguments.
pub(crate) fn builtin_macro_error(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        // Reject named arguments
        crate::builtins::reject_named("builtin-macro-error", named.as_ref(), call_span)?;

        // Expect exactly 2 arguments
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Extract span dict (first argument)
        let span_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let span_dict = match span_val {
            Value::Dict(ref map) => map,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-macro-error".to_string(),
                    "Dict (span)",
                    other.type_name(),
                    args[0].span,
                )
                .into());
            }
        };

        // Extract message (second argument)
        let msg_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let message = require_string("builtin-macro-error", msg_val, args[1].span)?;

        // Helper to extract a dict field and materialize it
        let get_dict =
            |parent: &IndexMap<Key, ThunkId>, field: &str| -> EvalResult<IndexMap<Key, ThunkId>> {
                let field_id = parent.get(&Key::String(field.into())).ok_or_else(|| {
                    EvalError::user_error(
                        format!("builtin-macro-error: span dict missing '{}' field", field),
                        args[0].span,
                    )
                })?;
                let field_thunk = ctx.get_thunk(*field_id);
                let field_val = materialize_sync(&field_thunk, Some(&call_span), &ctx)?;
                match field_val {
                    Value::Dict(map) => Ok(map),
                    other => Err(EvalError::type_mismatch_ctx(
                        format!("builtin-macro-error (span.{})", field),
                        "Dict",
                        other.type_name(),
                        field_thunk.span,
                    )
                    .into()),
                }
            };

        // Helper to extract an integer field from a dict
        let get_int =
            |parent: &IndexMap<Key, ThunkId>, field: &str, context: &str| -> EvalResult<usize> {
                let field_id = parent.get(&Key::String(field.into())).ok_or_else(|| {
                    EvalError::user_error(
                        format!("builtin-macro-error: {} missing '{}' field", context, field),
                        args[0].span,
                    )
                })?;
                let field_thunk = ctx.get_thunk(*field_id);
                let field_val = materialize_sync(&field_thunk, Some(&call_span), &ctx)?;
                match field_val {
                    Value::Int(n) if n >= 0 => Ok(n as usize),
                    Value::Int(n) => Err(EvalError::user_error(
                        format!(
                            "builtin-macro-error: {}.{} must be non-negative, got {}",
                            context, field, n
                        ),
                        field_thunk.span,
                    )
                    .into()),
                    other => Err(EvalError::type_mismatch_ctx(
                        format!("builtin-macro-error ({}.{})", context, field),
                        "Int",
                        other.type_name(),
                        field_thunk.span,
                    )
                    .into()),
                }
            };

        // Extract start and end dicts
        let start_dict = get_dict(span_dict, "start")?;
        let end_dict = get_dict(span_dict, "end")?;

        // Extract position fields
        let start_line = get_int(&start_dict, "line", "span.start")?;
        let start_col = get_int(&start_dict, "col", "span.start")?;
        let start_offset = get_int(&start_dict, "offset", "span.start")?;

        let end_line = get_int(&end_dict, "line", "span.end")?;
        let end_col = get_int(&end_dict, "col", "span.end")?;
        let end_offset = get_int(&end_dict, "offset", "span.end")?;

        // Construct Span
        let span = crate::ast::Span::new(
            crate::ast::Position {
                offset: start_offset,
                line: start_line,
                column: start_col,
            },
            crate::ast::Position {
                offset: end_offset,
                line: end_line,
                column: end_col,
            },
        );

        // Create macro error with the extracted span
        Err(EvalError::macro_error(message, span).into())
    })
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[Ok value]`
/// on success or `[Error message]` on failure.
/// Inherently materializing: must materialize body to catch errors.
pub(crate) fn builtin_try(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("try", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let func_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

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
                // Create an empty call environment as a child of the closure env.
                //
                // This mirrors what invoke_function/bind_args_thunks does for zero-param
                // functions: even with no parameters, a new child env is created so that
                // the De Bruijn (level, slot) coordinates assigned by the resolver match
                // the runtime env chain. Without this, the resolver's fn-params scope
                // (which is always pushed, even for empty param lists) would be missing at
                // runtime, causing all references inside the fn body to resolve to wrong
                // slots (off-by-one in the parent chain).
                let call_env = std::sync::Arc::new(std::sync::RwLock::new(
                    crate::value::Environment::with_parent(Arc::clone(&closure_env)),
                ));
                let body_thunk = Arc::new(Thunk::new_unevaluated_core(
                    Arc::clone(&body),
                    call_env,
                    Arc::clone(&ctx),
                    body.span,
                ));
                materialize(&body_thunk, Some(&call_span), &ctx).await
            }
            Value::Builtin(def) => {
                let builtin_args = BuiltinArgs {
                    args: vec![],
                    named: None,
                    call_span,
                    ctx: Arc::clone(&ctx),
                };
                match (def.func)(builtin_args).await {
                    Ok(thunk) => materialize(&thunk, Some(&call_span), &ctx).await,
                    Err(e) => Err(e),
                }
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
                let msg_thunk_id =
                    ctx.alloc_thunk(ok_val(string_val(&e.kind.to_string()), call_span)?);
                ok_val(
                    Value::Variant {
                        tag: "Error".to_string(),
                        payload: Some(msg_thunk_id),
                    },
                    call_span,
                )
            }
        }
    })
}

/// `until`: Repeat f(val) until predicate(val) returns true.
///
/// This is a Rust builtin to avoid the recursion depth limit of the LLT version.
/// The LLT recursive version hits MAX_EVAL_DEPTH at ~230 iterations.
///
/// This implementation uses a Rust loop with eager materialization at each step,
/// avoiding both depth limits and stack overflow from long thunk chains.
pub(crate) fn builtin_until(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("until", named.as_ref(), call_span)?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        let pred_thunk = Arc::clone(&args[0]);
        let f_thunk = Arc::clone(&args[1]);
        let mut val_thunk = Arc::clone(&args[2]);

        loop {
            // Create a pending call to pred(val) and materialize it
            let pred_result = Arc::new(Thunk::new_pending_call(
                Arc::clone(&pred_thunk),
                vec![Arc::clone(&val_thunk)],
                IndexMap::new(),
                call_span,
                Arc::clone(&ctx.config.stdlib_env),
                val_thunk.span,
                Some(Arc::from("until")),
                Arc::clone(&ctx),
            ));

            let pred_val = materialize(&pred_result, Some(&call_span), &ctx).await?;

            match pred_val {
                Value::Bool(true) => {
                    // Predicate holds, return the current value (as thunk)
                    return Ok(val_thunk);
                }
                Value::Bool(false) => {
                    // Predicate doesn't hold yet, apply f and materialize to get next value
                    let f_result = Arc::new(Thunk::new_pending_call(
                        Arc::clone(&f_thunk),
                        vec![val_thunk],
                        IndexMap::new(),
                        call_span,
                        Arc::clone(&ctx.config.stdlib_env),
                        call_span,
                        Some(Arc::from("until")),
                        Arc::clone(&ctx),
                    ));

                    // Eagerly materialize f(val) and re-wrap as a thunk for the next iteration
                    // This breaks the thunk chain and prevents stack overflow
                    let f_val = materialize(&f_result, Some(&call_span), &ctx).await?;
                    val_thunk = Arc::new(Thunk::new_materialized(f_val, call_span));
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
    })
}

/// Helper that performs the actual $apply logic after args are pre-materialized.
/// This is separated from builtin_apply so that builtin_apply can return a
/// PendingBuiltin thunk, enabling iterative arg materialization via BuiltinForceArg.
pub(crate) fn builtin_apply_impl(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        reject_named("apply", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // Both args[0] and args[1] have been pre-materialized by force_count.
        let func_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let args_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let arg_dict =
            crate::builtins::require_dict("apply", args_val, args[1].span, &ctx, call_span)?;

        // Split dict entries: integer-keyed → positional, string-keyed → named
        let mut int_entries: Vec<(i64, Arc<Thunk>)> = Vec::with_capacity(arg_dict.len());
        let mut named_args: IndexMap<String, Arc<Thunk>> = IndexMap::with_capacity(arg_dict.len());
        for (key, thunk_id) in &arg_dict {
            let thunk = ctx.get_thunk(*thunk_id);
            match key {
                Key::Int(n) => int_entries.push((*n, thunk)),
                Key::String(s) => {
                    named_args.insert(s.to_string(), thunk);
                }
            }
        }
        int_entries.sort_by_key(|(k, _)| *k);
        let positional: Vec<Arc<Thunk>> = int_entries.into_iter().map(|(_, v)| v).collect();

        match func_val {
            Value::Function {
                params,
                body,
                env: closure_env,
                ..
            } => {
                invoke_function(&CallContext {
                    params: &params,
                    body: &body,
                    closure_env: &closure_env,
                    positional: &positional,
                    named: if named_args.is_empty() {
                        None
                    } else {
                        Some(&named_args)
                    },
                    default_env: &closure_env,
                    ctx: &ctx,
                    call_span,
                    origin: Some(Arc::from("apply")),
                })
                .await
            }
            Value::Builtin(def) => {
                // Pre-materialize strict args before calling the builtin.
                // `builtin_apply_impl` calls `def.func` directly (not through the CEK machine),
                // so `force_count` and `pos_strictness` pre-materialization do NOT happen
                // automatically. Builtins that use `try_get_materialized().expect(...)` rely
                // on force_count/pos_strictness having been applied; without this, passing a
                // force_count>0 builtin like `$keys` through `$apply` would panic.
                //
                // Ordering: force_count range first (matches force_step dispatch order), then
                // pos_strictness Seq/Spine. Both loops skip args that are already materialized.
                let force_limit = def.force_count.min(positional.len());
                for arg in &positional[..force_limit] {
                    if arg.try_get_materialized().is_none() {
                        materialize(arg, Some(&call_span), &ctx).await?;
                    }
                }
                for (i, &s) in def.pos_strictness.iter().enumerate() {
                    if i < positional.len()
                        && (s == Strictness::Seq || s == Strictness::Spine)
                        && positional[i].try_get_materialized().is_none()
                    {
                        materialize(&positional[i], Some(&call_span), &ctx).await?;
                    }
                }
                let builtin_args = BuiltinArgs {
                    args: positional,
                    named: if named_args.is_empty() {
                        None
                    } else {
                        Some(named_args)
                    },
                    call_span,
                    ctx: Arc::clone(&ctx),
                };
                (def.func)(builtin_args).await
            }
            _ => Err(EvalError::type_mismatch_ctx(
                "apply".to_string(),
                "Function",
                func_val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `apply`: wrapper that returns a PendingBuiltin thunk for iterative arg materialization.
pub(crate) fn builtin_apply(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
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
        let named_opt = if named.as_ref().map(|n| n.is_empty()).unwrap_or(true) {
            None
        } else {
            named
        };
        Ok(Arc::new(Thunk::new_pending_builtin(
            // force_count=2: pre-materialize both args[0] (function) and args[1] (args-dict)
            // before calling builtin_apply_impl, which uses try_get_materialized().expect(...).
            // Must match what builtin_apply_impl actually requires.
            builtin!("apply", builtin_apply_impl, [], 2),
            args,
            named_opt,
            call_span,
            Some(Arc::from("apply")),
            ctx,
        )))
    })
}

/// `gensym`: Generate a unique symbol name for macro hygiene.
///
/// - Zero args: returns `":gensym:N"` where N is a global monotonic counter.
/// - One arg (prefix string): returns `":prefix:N"` where prefix is the argument.
///
/// The `:` prefix ensures these names cannot collide with user-written identifiers
/// (`:` is not allowed in bare word identifiers).
pub(crate) fn builtin_gensym(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        use std::sync::atomic::{AtomicU64, Ordering};

        // Global counter for gensym IDs
        static GENSYM_COUNTER: AtomicU64 = AtomicU64::new(0);

        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        reject_named("gensym", named.as_ref(), call_span)?;

        // Accept 0 or 1 positional arguments
        let prefix = if args.is_empty() {
            "gensym".to_string()
        } else if args.len() == 1 {
            let prefix_val = args[0]
                .try_get_materialized()
                .expect("pre-materialized by pos_strictness[0]=Seq");
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

        let id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(":{}:{}", prefix, id);
        ok_val(string_val(&name), call_span)
    })
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
pub(crate) fn builtin_macro_injects(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let macro_name_val = crate::builtins::expect_one_arg(
            "macro-injects",
            &args,
            named.as_ref(),
            &ctx,
            call_span,
        )?;
        let macro_name = require_string("macro-injects", macro_name_val, args[0].span)?;

        // Look up the macro in the inject map
        match ctx.config.macro_injects_map.get(&macro_name) {
            Some(inject_default) => ok_val(string_val(inject_default), call_span),
            None => ok_val(Value::Dict(IndexMap::new()), call_span), // Null = empty dict
        }
    })
}

/// `decimal`: Parse a string as an exact base-10 decimal (rust_decimal::Decimal).
/// Returns Value::Decimal. Error on invalid format.
pub(crate) fn builtin_decimal(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("decimal", &args, named.as_ref(), &ctx, call_span)?;
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
    })
}

/// `big-int`: Convert an Int or String to a BigInt (arbitrary-precision integer).
pub(crate) fn builtin_big_int(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("big-int", &args, named.as_ref(), &ctx, call_span)?;
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
    })
}

/// `type-of`: takes 1 arg, materializes it, returns the type name.
/// Both `Function` and `Builtin` return "Function" (from the user's perspective).
/// Returns "Dict" for all dicts, with no distinction between list-like (sequential int keys)
/// and map-like dicts — the type system does not track key structure at runtime.
/// Inherently materializing: must inspect value variant to determine type.
pub(crate) fn builtin_type_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("type-of", &args, named.as_ref(), &ctx, call_span)?;
        let name = match val.type_name() {
            "Builtin" => "Function",
            other => other,
        };
        ok_val(string_val(name), call_span)
    })
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
pub(crate) fn builtin_ast_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        // Reject named args and ensure exactly 1 arg
        crate::builtins::reject_named("ast-of", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let thunk = &args[0];

        // Inspect the thunk state WITHOUT forcing it using ThunkInner API

        // Check for PendingBuiltin
        if let Some(def) = thunk.peek_builtin_def() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("pending-builtin"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val(def.name),
                    call_span,
                ))),
            );
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for PendingCall
        if thunk.is_pending_call() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("pending-call"),
                    call_span,
                ))),
            );
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Guarded
        if thunk.is_guarded() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("guarded"),
                    call_span,
                ))),
            );
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for InProgress
        if thunk.is_in_progress() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("in-progress"),
                    call_span,
                ))),
            );
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Failed
        if let Some(err) = thunk.get_cached_error() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("failed"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("error".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val(&err.kind.to_string()),
                    call_span,
                ))),
            );
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Surface (runtime-v2: return Value::Expression directly)
        if let Some(node) = thunk.peek_surface_node() {
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                Value::Expression(node),
                call_span,
            )));
        }

        // Check for AstNodeField (runtime-v2: return the containing SurfaceNode)
        if let Some((node, _field)) = thunk.peek_ast_node_field() {
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                Value::Expression(node),
                call_span,
            )));
        }

        // Check for Materialized
        let dict_entries = if let Some(val) = thunk.try_get_materialized() {
            // Value is already materialized — inspect it
            match val {
                crate::value::Value::Function {
                    params, annotation, ..
                } => {
                    let mut entries = IndexMap::new();

                    // Add type field
                    entries.insert(
                        crate::value::Key::String("type".into()),
                        ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                            string_val("function"),
                            call_span,
                        ))),
                    );

                    // Add params field as a list of param names
                    let param_names: Vec<ThunkId> = params
                        .iter()
                        .map(|p| {
                            ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                                string_val(&p.name),
                                call_span,
                            )))
                        })
                        .collect();

                    if !param_names.is_empty() {
                        let params_seq = param_names.into_iter().rev().fold(
                            ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                                crate::value::Value::Dict(IndexMap::new()),
                                call_span,
                            ))),
                            |tail, head| {
                                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
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
                                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                                    string_val(doc_str),
                                    call_span,
                                ))),
                            );
                        }
                    }

                    entries
                }
                other => {
                    let mut entries = IndexMap::new();
                    entries.insert(
                        crate::value::Key::String("type".into()),
                        ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                            string_val(other.type_name()),
                            call_span,
                        ))),
                    );
                    entries
                }
            }
        } else {
            // Placeholder or unknown state (should not be observable in user code)
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::Key::String("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span,
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("placeholder"),
                    call_span,
                ))),
            );
            entries
        };

        ok_val(crate::value::Value::Dict(dict_entries), call_span)
    })
}

/// `llt-repr`: takes 1 arg, materializes it recursively, returns its LLT display string representation.
/// This is the programmatic equivalent of the LLT display format (Int(42), Dict({...}), etc.).
/// Used by the `-o llt` output formatter.
pub(crate) fn builtin_llt_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("llt-repr", &args, named.as_ref(), &ctx, call_span)?;
        // value_to_display_string materializes nested values on demand via visit_value
        let display_str = crate::value_to_display_string(&val, &ctx, call_span)
            .map_err(|e| EvalError::internal(format!("llt-repr: {}", e.kind), call_span))?;
        ok_val(string_val(&display_str), call_span)
    })
}

/// `tag-of`: Return the tag of a Variant as a String.
pub(crate) fn builtin_tag_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("tag-of", &args, named.as_ref(), &ctx, call_span)?;
        match val {
            Value::Variant { tag, .. } => ok_val(string_val(&tag), call_span),
            // runtime-v2: Value::Expression supports tag-of via surface_expr_tag
            Value::Expression(node) => ok_val(
                string_val(crate::surface_fields::surface_expr_tag(&node.expr)),
                call_span,
            ),
            _ => Err(Box::new(EvalError::type_mismatch(
                "Variant",
                val.type_name(),
                call_span,
            ))),
        }
    })
}

/// `variant`: Create a variant with the given tag and optional payload.
///
/// Forms:
/// - `[variant "Tag"]` — unit variant (no payload)
/// - `[variant "Tag" payload]` — variant with payload (stored as ThunkId)
///
/// Special behavior for AST variant names:
/// When the tag is a known AST variant name (VarRef, Literal, Call, Dict, etc.),
/// the payload dict is converted to a Value::Expression(SurfaceNode) using
/// dict_to_surface_node. This enables the macro call convention migration from
/// Dict-encoded AST to native Value::Expression.
pub(crate) fn builtin_variant(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        crate::builtins::reject_named("variant", named.as_ref(), call_span)?;

        match args.len() {
            1 => {
                // Unit variant: [variant "Tag"]
                let tag_thunk = &args[0];
                let tag_val = tag_thunk
                    .try_get_materialized()
                    .expect("pre-materialized by pos_strictness[0]=Seq");
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
                let tag_val = tag_thunk
                    .try_get_materialized()
                    .expect("pre-materialized by pos_strictness[0]=Seq");
                match tag_val {
                    Value::String {
                        ref source,
                        start,
                        end,
                    } => {
                        let tag = &source[start..end];

                        // Check if this is a known AST variant name
                        let is_ast_variant = matches!(
                            tag,
                            "VarRef"
                                | "Literal"
                                | "Call"
                                | "Dict"
                                | "LetDecl"
                                | "Fn"
                                | "Sequential"
                                | "Annotated"
                                | "DotAccess"
                                | "TypeAssert"
                                | "Match"
                                | "Quote"
                                | "Unquote"
                                | "UnquoteSplice"
                                | "Rest"
                                | "Placeholder"
                                | "Pipe"
                                | "AstError"
                                | "PatternDecl"
                                | "TypeApp"
                                | "CaseArm"
                        );

                        if is_ast_variant {
                            // Convert payload dict to SurfaceNode using dict_to_surface_node
                            // H2: conditional force — only when tag is a known AST variant name
                            let payload_val =
                                materialize(payload_thunk, Some(&call_span), &ctx).await?;
                            // Deep-materialize all nested dict values so dict_to_surface_node can access them
                            // dict_to_surface_node uses try_get_materialized on all field thunks
                            let deep_payload = force_dict_tree(&payload_val, &ctx).await?;
                            // Wrap as Variant so dict_to_surface_node can extract the tag
                            let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                deep_payload,
                                call_span,
                            )));
                            let variant_val = Value::Variant {
                                tag: tag.to_string(),
                                payload: Some(payload_id),
                            };
                            let surface_node =
                                crate::surface_convert::dict_to_surface_node(&variant_val, &ctx)
                                    .map_err(|e| {
                                        Box::new(EvalError::user_error(
                                            format!(
                                        "variant '{}': failed to convert payload to AST node{}: {}",
                                        tag,
                                        if e.field_path.is_empty() {
                                            String::new()
                                        } else {
                                            format!(" (at field {})", e.field_path.join("."))
                                        },
                                        e.message
                                    ),
                                            call_span,
                                        ))
                                    })?;
                            ok_val(Value::Expression(surface_node), call_span)
                        } else {
                            // Non-AST variant: store the payload as a ThunkId (lazy, won't be forced until accessed)
                            let payload_id = ctx.alloc_thunk(Arc::clone(payload_thunk));
                            ok_val(
                                Value::Variant {
                                    tag: tag.to_string(),
                                    payload: Some(payload_id),
                                },
                                call_span,
                            )
                        }
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
    })
}

/// `int?`: Return true if the argument is an Int.
pub(crate) fn builtin_int_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("int?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(matches!(val, Value::Int(_))), call_span)
    })
}

/// `float?`: Return true if the argument is a Float.
pub(crate) fn builtin_float_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("float?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(matches!(val, Value::Float(_))), call_span)
    })
}

// num? is implemented in LLT as [or [int? x] [float? x]] — see stdlib/prelude.llt

/// `str?`: Return true if the argument is a String.
pub(crate) fn builtin_str_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("str?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(matches!(val, Value::String { .. })), call_span)
    })
}

/// `bool?`: Return true if the argument is a Bool.
pub(crate) fn builtin_bool_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("bool?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(matches!(val, Value::Bool(_))), call_span)
    })
}

/// `bytes?`: Return true if the argument is Bytes.
pub(crate) fn builtin_bytes_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("bytes?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(Value::Bool(matches!(val, Value::Bytes { .. })), call_span)
    })
}

/// `null?`: Return true if the argument is Null (represented as an empty Dict).
pub(crate) fn builtin_null_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("null?", &args, named.as_ref(), &ctx, call_span)?;
        let is_null = match val {
            Value::Dict(map) => map.is_empty(),
            Value::Overlay(l, r) => {
                let map = crate::builtins::flatten_overlay(&l, &r, "null?", &ctx, call_span)?;
                map.is_empty()
            }
            _ => false,
        };
        ok_val(Value::Bool(is_null), call_span)
    })
}

/// `dict?`: Return true if the argument is a Dict (including lists and null).
pub(crate) fn builtin_dict_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("dict?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(
            Value::Bool(matches!(val, Value::Dict(_) | Value::Overlay(..))),
            call_span,
        )
    })
}

// record? and map? are implemented in LLT as aliases of dict? — see stdlib/prelude.llt

/// `fn?`: Return true if the argument is callable (Function or Builtin).
pub(crate) fn builtin_fn_check(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg("fn?", &args, named.as_ref(), &ctx, call_span)?;
        ok_val(
            Value::Bool(matches!(val, Value::Function { .. } | Value::Builtin(_))),
            call_span,
        )
    })
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
        Value::Program { .. } => "Program",
        Value::Document(_) => "Document",
        Value::Expression(_) => "Expression",
        Value::Task(_) => "Task",
        Value::Channel(_) => "Channel",
        Value::Context(_) => "Context",
        Value::Builder(_) => "Builder",
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
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<Arc<Thunk>> {
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
                map.insert(Key::String(k.as_str().into()), thunk_id);
            }
            ok_val(Value::Dict(map), span)
        }
    }
}

/// `from-json`: takes 1 arg (String containing JSON), parses into LLT value.
/// Inherently materializing: must parse entire JSON string to construct value.
pub(crate) fn builtin_from_json(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("from-json", &args, named.as_ref(), &ctx, call_span)?;
        let json_str = require_string("from-json", val, args[0].span)?;
        let parsed: serde_json::Value = serde_json::from_str(&json_str)
            .map_err(|e| EvalError::json_parse(e.to_string(), call_span))?;
        json_to_value(&parsed, 0, call_span, &ctx)
    })
}

// DELETED: parse_integrity_hash (include-decomp-redelete sprint)
// Was used by builtin_include which has been deleted.
// Integrity checking is now done in tinct-level include function in stdlib/prelude.llt.

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
pub(crate) fn builtin_blake3(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val =
            crate::builtins::expect_one_arg("blake3", &args, named.as_ref(), &ctx, call_span)?;
        let s = require_string("blake3", val, args[0].span)?;
        let hex = blake3_hex(s.as_bytes());
        ok_val(string_val(hex.as_str()), call_span)
    })
}

/// `cap-identity`: return the stable identity string `"dev:ino"` of a DirCap.
///
/// Takes 1 positional arg (DirCap or RevocableDirCap) and returns a String of the form
/// `"<dev>:<ino>"` derived from `fstat` on the directory's open file descriptor.
/// On non-Unix platforms, falls back to `"0:<hash>"` using a path hash.
///
/// This stable identity can be combined with source text to form a content-addressed
/// cache key: `blake3(cap-identity + "|" + source)`.
pub(crate) fn builtin_cap_identity(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "cap-identity",
            &args,
            named.as_ref(),
            &ctx,
            call_span,
        )?;

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
                return Err(
                    EvalError::type_mismatch("DirCap", val.type_name(), args[0].span).into(),
                );
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
            // On non-Unix, use blake3 hash of the Debug representation of the Dir.
            // This provides a stable, collision-resistant identity across process restarts.
            let debug_repr = format!("{:?}", *dir);
            let hash = blake3::hash(debug_repr.as_bytes());
            format!("0:{}", hash.to_hex())
        };

        ok_val(string_val(identity.as_str()), call_span)
    })
}

/// `load`: parse a source String into a file AST dict (same format as `ast-of`).
///
/// Takes 1 positional arg (String source text) and an optional `name:` named arg (String,
/// used as the provenance hint for error messages).
///
/// Pipeline: parse → macro-expand → desugar → surface_program_to_dict.
/// Returns `Value::Program(Arc<SurfaceProgram>)` — the runtime-v2 native AST type.
///
/// This is the primitive underlying the `include` pipeline in the include-decomposition design.
/// runtime-v2 Part G: changed from Dict schema to Value::Program.
pub(crate) fn builtin_load(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Extract optional name: and hash: named args
        let (name_hint, expected_hash): (Option<String>, Option<String>) = if let Some(named_map) =
            named
        {
            // Reject unknown named args
            for key in named_map.keys() {
                if key != "name" && key != "hash" {
                    return Err(EvalError::named_arg_rejected("load".to_string(), call_span).into());
                }
            }
            let name_hint = if let Some(name_thunk) = named_map.get("name") {
                let name_val = materialize(name_thunk, Some(&call_span), &ctx).await?; // H2: conditional (only when name: named arg present)
                let name_str = require_string("load", name_val, name_thunk.span)?;
                Some(name_str)
            } else {
                None
            };
            let expected_hash = if let Some(hash_thunk) = named_map.get("hash") {
                let hash_val = materialize(hash_thunk, Some(&call_span), &ctx).await?; // H2: conditional (only when hash: named arg present)
                let hash_str = require_string("load", hash_val, hash_thunk.span)?;
                Some(hash_str)
            } else {
                None
            };
            (name_hint, expected_hash)
        } else {
            (None, None)
        };

        // Extract source string
        let source_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let source = require_string("load", source_val, args[0].span)?;

        // Use name hint for error messages
        let display_name = name_hint.as_deref().unwrap_or("<load>");

        // Integrity hash verification
        if let Some(expected) = &expected_hash {
            let actual = blake3_hex(source.as_bytes());
            if actual != *expected {
                return Err(EvalError::include_hash_mismatch(
                    display_name.to_string(),
                    expected.clone(),
                    actual,
                    call_span,
                )
                .into());
            }
        } else if ctx.config.require_integrity {
            // --require-integrity flag is set but no hash: argument provided
            return Err(
                EvalError::include_hash_required(display_name.to_string(), call_span).into(),
            );
        }

        // Parse
        let parsed = crate::parser::parse(&source).map_err(|e| {
            EvalError::include_parse_failed(display_name.to_string(), e.to_string(), call_span)
        })?;

        // PIPELINE INVARIANT: parse -> expand_surface_program -> desugar -> resolve.
        // Use expand_surface_program (not expand_macros) so SurfaceItem::Decl macros are seen.
        // Desugar AFTER macro expansion so that macros can introduce $_ patterns.
        let mut program = parsed.program;
        crate::expand::expand_surface_program(&mut program, ctx.config.no_fs, &ctx.config.base_dir)
            .await
            .map_err(|e| {
                EvalError::include_parse_failed(
                    display_name.to_string(),
                    format!("macro expansion error: {e}"),
                    call_span,
                )
            })?;
        // Desugar $_ implicit lambdas after macro expansion (macros may introduce $_ patterns).
        crate::desugar::desugar_surface_program(&mut program);
        // Variable resolution pass (Phase 1 of arena allocation strategy).
        let res_table = crate::resolve::resolve_surface_program(&program);
        // Typecheck to populate TypeAnnotationTable for static type resolution in TypeAssert nodes.
        // This enables included files to use the resolved type path instead of RuntimeTypeCheck fallback.
        // Type errors are advisory — eval proceeds regardless. Callers that care
        // about type errors use `builtin_eval_types`.
        let (_annotation_errors, type_annotation_table) =
            crate::typecheck::typecheck_surface_program_annotation_table(&program);
        let program_value = Value::Program {
            program: std::sync::Arc::new(program),
            resolutions: std::sync::Arc::new(res_table),
            types: std::sync::Arc::new(type_annotation_table),
        };
        let thunk = Arc::new(Thunk::new_materialized(program_value, call_span));
        Ok(thunk)
    })
}

/// `expand`: takes a `Value::Program`, runs macro expansion, returns `Value::Program`.
///
/// For the include-decomp self-hosted pipeline which separates parse/expand/eval
/// into distinct primitives.
///
/// Pipeline: unwrap Program → run expand_surface_program → wrap back as Program.
pub(crate) fn builtin_expand(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        crate::builtins::reject_named("expand", named.as_ref(), call_span)?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::Program {
                program: surface_program,
                resolutions: _old_resolutions,
                types: _old_types,
            } => {
                // Run macro expansion using expand_surface_program so SurfaceItem::Decl macros are seen.
                // PIPELINE INVARIANT: expand -> desugar -> resolve (macros can introduce $_ patterns).
                let mut new_surface_program = (*surface_program).clone();
                crate::expand::expand_surface_program(
                    &mut new_surface_program,
                    ctx.config.no_fs,
                    &ctx.config.base_dir,
                )
                .await
                .map_err(|e| {
                    EvalError::user_error(
                        format!("expand: macro expansion error: {}", e.kind),
                        call_span,
                    )
                })?;
                // Desugar $_ patterns introduced by macros.
                crate::desugar::desugar_surface_program(&mut new_surface_program);

                // Re-compute resolution table for the expanded and desugared program
                let new_resolutions = crate::resolve::resolve_surface_program(&new_surface_program);

                // Typecheck to populate TypeAnnotationTable for static type resolution in TypeAssert nodes.
                // Type errors are advisory — eval proceeds regardless. Callers that care
                // about type errors use `builtin_eval_types`.
                let (_annotation_errors, type_annotation_table) =
                    crate::typecheck::typecheck_surface_program_annotation_table(
                        &new_surface_program,
                    );

                // Return as Value::Program with fresh resolution and type tables
                ok_val(
                    Value::Program {
                        program: std::sync::Arc::new(new_surface_program),
                        resolutions: std::sync::Arc::new(new_resolutions),
                        types: std::sync::Arc::new(type_annotation_table),
                    },
                    call_span,
                )
            }
            _ => Err(EvalError::type_mismatch_ctx(
                "expand".to_string(),
                "Program",
                val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `eval`: evaluates a sequence of Expression values in a given environment.
///
/// For the include-decomp self-hosted pipeline. Takes a `[Seq Expression]` and
/// returns a `[Seq Any]` of lazy thunks (one per expression).
///
/// Each Expression is wrapped in a ThunkState::Surface which defers lowering and
/// evaluation until the thunk is forced.
///
/// Named args:
/// - `env:` (Dict) — bindings added to the base environment (default: empty)
/// - `%:` (Any) — the pipeline input value, bound as `$` in the environment
/// - `program:` (Program) — the source Program providing resolution/type tables for the expressions (optional)
///
/// Base environment: stdlib_env (the standard library prelude).
pub(crate) fn builtin_eval(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Extract optional env:, %:, and program: named args
        let (env_dict, pipeline_input, program_opt) = if let Some(named_map) = named {
            // Reject unknown named args
            for key in named_map.keys() {
                if key != "env" && key != "%" && key != "program" {
                    return Err(EvalError::named_arg_rejected("eval".to_string(), call_span).into());
                }
            }

            let env_dict = named_map.get("env").map(Arc::clone);
            let pipeline_input = named_map.get("%").map(Arc::clone);
            let program_opt = named_map.get("program").map(Arc::clone);
            (env_dict, pipeline_input, program_opt)
        } else {
            (None, None, None)
        };

        // Start with stdlib environment
        let base_env = Arc::clone(&ctx.config.stdlib_env);

        // Add env: dict bindings if provided
        let env_with_bindings = if let Some(env_thunk) = env_dict {
            let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
            // Flatten Overlay to Dict before processing env bindings
            let env_val = match env_val {
                Value::Overlay(l, r) => Value::Dict(crate::builtins::flatten_overlay(
                    &l, &r, "eval", &ctx, call_span,
                )?),
                other => other,
            };
            match env_val {
                Value::Dict(entries) => {
                    // Create child environment with dict entries as bindings
                    let child_env = Arc::new(std::sync::RwLock::new(
                        crate::value::Environment::with_parent(Arc::clone(&base_env)),
                    ));
                    for (key, thunk_id) in entries.iter() {
                        if let Key::String(name) = key {
                            child_env
                                .write()
                                .unwrap()
                                .insert(name.to_string(), ctx.get_thunk(*thunk_id));
                        }
                    }
                    child_env
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "Dict",
                        env_val.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            base_env
        };

        // Add %: (pipeline input) as $ binding if provided
        let final_env = if let Some(input_thunk) = pipeline_input {
            let child_env = Arc::new(std::sync::RwLock::new(
                crate::value::Environment::with_parent(Arc::clone(&env_with_bindings)),
            ));
            child_env
                .write()
                .unwrap()
                .insert("$".to_string(), input_thunk);
            child_env
        } else {
            env_with_bindings
        };

        // Materialize the sequence argument
        let seq_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Collect Expression nodes from the sequence
        let mut expression_nodes = Vec::new();
        let mut current = seq_val;
        loop {
            match current {
                Value::Seq { head, tail } => {
                    let head_val =
                        materialize(&ctx.get_thunk(head), Some(&call_span), &ctx).await?;
                    match head_val {
                        Value::Expression(node) => {
                            expression_nodes.push(node);
                        }
                        _ => {
                            return Err(EvalError::type_mismatch_ctx(
                                "eval".to_string(),
                                "Seq of Expression",
                                &format!("Seq containing {}", head_val.type_name()),
                                call_span,
                            )
                            .into())
                        }
                    }
                    current = materialize(&ctx.get_thunk(tail), Some(&call_span), &ctx).await?;
                }
                Value::Dict(ref entries) if entries.is_empty() => {
                    // Empty dict = end of sequence
                    break;
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "Seq",
                        current.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }

        // Get resolution and type tables from the program: argument if provided
        let (res_table, types_table) = if let Some(program_thunk) = program_opt {
            let program_val = materialize(&program_thunk, Some(&call_span), &ctx).await?;
            match program_val {
                Value::Program {
                    resolutions, types, ..
                } => (Arc::clone(&resolutions), Arc::clone(&types)),
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "Program (for program: argument)",
                        program_val.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            // No program provided - use empty tables (expressions won't have resolution info)
            (
                crate::ast::empty_resolution_table_arc(),
                crate::ast::empty_type_annotation_table_arc(),
            )
        };

        // Create Surface thunks and force each one in sequence, returning the last value.
        // eval-document-runtime expects the LAST expression's evaluated value, not a Seq of thunks.
        if expression_nodes.is_empty() {
            return ok_val(Value::Dict(IndexMap::new()), call_span);
        }

        let mut last_thunk = Arc::new(Thunk::new_materialized(
            Value::Dict(IndexMap::new()),
            call_span,
        ));
        for node in expression_nodes.into_iter() {
            let surface_thunk = Arc::new(Thunk::new_surface(
                node,
                Arc::clone(&res_table),
                Arc::clone(&types_table),
                Arc::clone(&final_env),
                Arc::clone(&ctx),
                call_span,
            ));
            // Force the Surface thunk to get the evaluated value
            let val = crate::eval::materialize(&surface_thunk, Some(&call_span), &ctx).await?;
            last_thunk = Arc::new(Thunk::new_materialized(val, call_span));
        }

        ok_val(
            last_thunk
                .try_get_materialized()
                .expect("just materialized"),
            call_span,
        )
    })
}

/// `eval-types`: same as `eval` but evaluates in the type-stage environment.
///
/// This is used for evaluating type-level expressions (type aliases, class declarations).
/// Base environment: ctx.config.type_stage_env (contains type-level bindings).
pub(crate) fn builtin_eval_types(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Extract optional env: and %: named args (same as eval)
        let (env_dict, pipeline_input) = if let Some(named_map) = named {
            for key in named_map.keys() {
                if key != "env" && key != "%" {
                    return Err(
                        EvalError::named_arg_rejected("eval-types".to_string(), call_span).into(),
                    );
                }
            }

            let env_dict = named_map.get("env").map(Arc::clone);
            let pipeline_input = named_map.get("%").map(Arc::clone);
            (env_dict, pipeline_input)
        } else {
            (None, None)
        };

        // Use type_stage_env as the base: type-level evaluation builtins only, no IO, no caps, no runtime API.
        let base_env = Arc::clone(&ctx.config.type_stage_env);

        // Add env: dict bindings if provided
        let env_with_bindings = if let Some(env_thunk) = env_dict {
            let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
            match env_val {
                Value::Dict(entries) => {
                    let child_env = Arc::new(std::sync::RwLock::new(
                        crate::value::Environment::with_parent(Arc::clone(&base_env)),
                    ));
                    for (key, thunk_id) in entries.iter() {
                        if let Key::String(name) = key {
                            child_env
                                .write()
                                .unwrap()
                                .insert(name.to_string(), ctx.get_thunk(*thunk_id));
                        }
                    }
                    child_env
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval-types".to_string(),
                        "Dict",
                        env_val.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            base_env
        };

        // Add %: (pipeline input) as $ binding if provided
        let final_env = if let Some(input_thunk) = pipeline_input {
            let child_env = Arc::new(std::sync::RwLock::new(
                crate::value::Environment::with_parent(Arc::clone(&env_with_bindings)),
            ));
            child_env
                .write()
                .unwrap()
                .insert("$".to_string(), input_thunk);
            child_env
        } else {
            env_with_bindings
        };

        // Materialize the sequence argument
        let seq_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Collect Expression nodes from the sequence
        let mut expression_nodes = Vec::new();
        let mut current = seq_val;
        loop {
            match current {
                Value::Seq { head, tail } => {
                    let head_val =
                        materialize(&ctx.get_thunk(head), Some(&call_span), &ctx).await?;
                    match head_val {
                        Value::Expression(node) => {
                            expression_nodes.push(node);
                        }
                        _ => {
                            return Err(EvalError::type_mismatch_ctx(
                                "eval-types".to_string(),
                                "Seq of Expression",
                                &format!("Seq containing {}", head_val.type_name()),
                                call_span,
                            )
                            .into())
                        }
                    }
                    current = materialize(&ctx.get_thunk(tail), Some(&call_span), &ctx).await?;
                }
                Value::Dict(ref entries) if entries.is_empty() => {
                    break;
                }
                _ => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval-types".to_string(),
                        "Seq",
                        current.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }

        // Create empty resolution and type tables
        let res_table = crate::ast::empty_resolution_table_arc();
        let types_table = crate::ast::empty_type_annotation_table_arc();

        // Create Surface thunks for each expression
        let mut result_seq = Value::Dict(IndexMap::new()); // Start with empty (nil)
        for node in expression_nodes.into_iter().rev() {
            let surface_thunk = Arc::new(Thunk::new_surface(
                node,
                Arc::clone(&res_table),
                Arc::clone(&types_table),
                Arc::clone(&final_env),
                Arc::clone(&ctx),
                call_span,
            ));
            let surface_thunk_id = ctx.alloc_thunk(surface_thunk);

            let tail_thunk_id = ctx.alloc_thunk(ok_val(result_seq, call_span)?);
            result_seq = Value::Seq {
                head: surface_thunk_id,
                tail: tail_thunk_id,
            };
        }

        ok_val(result_seq, call_span)
    })
}

/// `include-cache-get`: look up the string-keyed include cache by blake3 key.
///
/// Takes 1 positional arg (String key). Returns:
/// - `[Missing]`       — key not in cache
/// - `[Pending]`       — key is marked as in-progress (cycle detection)
/// - `[Cached value]`  — cached result thunk
pub(crate) fn builtin_include_cache_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "include-cache-get",
            &args,
            named.as_ref(),
            &ctx,
            call_span,
        )?;
        let key = require_string("include-cache-get", val, args[0].span)?;

        let entry = ctx
            .state
            .lock()
            .unwrap()
            .string_include_cache
            .get(&key)
            .cloned();

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
            Some(crate::eval::IncludeCacheEntry::Cached(thunk, _res, _types)) => {
                let payload_id = ctx.alloc_thunk(Arc::clone(&thunk));
                ok_val(
                    Value::Variant {
                        tag: "Cached".to_string(),
                        payload: Some(payload_id),
                    },
                    call_span,
                )
            }
        }
    })
}

/// `include-cache-put`: insert or update the string-keyed include cache.
///
/// Takes 2 positional args: String key and a value.
/// The value must be a `[Missing]`, `[Pending]`, or `[Cached x]` Variant.
/// Returns the stored value (pass-through).
pub(crate) fn builtin_include_cache_put(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        reject_named("include-cache-put", named.as_ref(), call_span)?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = require_string("include-cache-put", key_val, args[0].span)?;

        // The second arg is the entry variant: [Missing], [Pending], or [Cached value]
        let entry_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

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
                    crate::eval::IncludeCacheEntry::Cached(
                        payload_thunk,
                        crate::ast::empty_resolution_table_arc(),
                        crate::ast::empty_type_annotation_table_arc(),
                    )
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
            .lock()
            .unwrap()
            .string_include_cache
            .insert(key, entry);

        // Return an empty dict [] so that include-cache-put can be used as an intermediate
        // sequential expression in the scope chain (sequential expressions must return Dict).
        // The stored value is available via include-cache-get; callers don't need the return.
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

// TOMBSTONE: builtin_include deleted in include-decomp-redelete sprint (2026-05-20).
// The `include` function is now implemented in stdlib/prelude.llt as a self-hosted
// pipeline using the decomposed primitives: load, expand, eval, blake3, cap-identity,
// include-cache-get, include-cache-put. See doc/whatif/include-decomposition.md.

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
pub(crate) fn builtin_validate(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        // Expect exactly 2 args: schema, data
        let (schema, data) = expect_two_args("validate", &args, named.as_ref(), &ctx, call_span)?;

        // Schema must be a Dict
        let schema_dict = match schema {
            Value::Dict(ref d) => d.clone(),
            Value::Overlay(..) => {
                // Materialize Overlay to Dict before validation
                let schema_thunk_id =
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(schema.clone(), call_span)));
                let schema_thunk = ctx.get_thunk(schema_thunk_id);
                let materialized = materialize(&schema_thunk, Some(&call_span), &ctx).await?;
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
                return Err(EvalError::type_mismatch(
                    "Dict (schema)",
                    &type_name(&schema),
                    call_span,
                )
                .into());
            }
        };

        // Collect violations
        let mut violations = Vec::new();
        validate_value(&schema_dict, &data, "", &mut violations, &ctx, call_span)?;

        if violations.is_empty() {
            // Success: return data unchanged
            Ok(Arc::new(Thunk::new_materialized(data, call_span)))
        } else {
            // Failure: throw SchemaViolation with all violations
            Err(EvalError::schema_violation(violations, call_span).into())
        }
    })
}

/// Helper: extract exactly 2 pre-materialized positional arguments, no named args.
fn expect_two_args(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    _ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<(Value, Value)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    if named.is_some() && !named.unwrap().is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }

    let val1 = args[0]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let val2 = args[1]
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

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
    ctx: &Arc<crate::eval::EvalContext>,
    span: Span,
) -> EvalResult<()> {
    use crate::value::StrKey;

    // Check `type` constraint
    if let Some(&type_thunk_id) = schema.get(&StrKey("type")) {
        let type_thunk = ctx.get_thunk(type_thunk_id);
        let type_val = materialize_sync(&type_thunk, Some(&span), ctx)?;
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
    if let Some(&min_thunk_id) = schema.get(&StrKey("min")) {
        let min_thunk = ctx.get_thunk(min_thunk_id);
        let min_val = materialize_sync(&min_thunk, Some(&span), ctx)?;
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

    if let Some(&max_thunk_id) = schema.get(&StrKey("max")) {
        let max_thunk = ctx.get_thunk(max_thunk_id);
        let max_val = materialize_sync(&max_thunk, Some(&span), ctx)?;
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
    if let Some(&min_len_thunk_id) = schema.get(&StrKey("min-length")) {
        let min_len_thunk = ctx.get_thunk(min_len_thunk_id);
        let min_len_val = materialize_sync(&min_len_thunk, Some(&span), ctx)?;
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

    if let Some(&max_len_thunk_id) = schema.get(&StrKey("max-length")) {
        let max_len_thunk = ctx.get_thunk(max_len_thunk_id);
        let max_len_val = materialize_sync(&max_len_thunk, Some(&span), ctx)?;
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
    if let Some(&pattern_thunk_id) = schema.get(&StrKey("pattern")) {
        let pattern_thunk = ctx.get_thunk(pattern_thunk_id);
        let pattern_val = materialize_sync(&pattern_thunk, Some(&span), ctx)?;
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
    if let Some(&enum_thunk_id) = schema.get(&StrKey("enum")) {
        let enum_thunk = ctx.get_thunk(enum_thunk_id);
        let enum_val = materialize_sync(&enum_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref enum_dict) = enum_val {
            // Pre-materialize all enum values once, then check membership.
            // This avoids re-materializing on early-exit scenarios.
            let allowed_values: Vec<Value> = enum_dict
                .iter()
                .map(|(_key, &val_thunk_id)| {
                    let val_thunk = ctx.get_thunk(val_thunk_id);
                    materialize_sync(&val_thunk, Some(&span), ctx)
                })
                .collect::<EvalResult<Vec<Value>>>()?;

            let found = allowed_values.iter().any(|val| values_equal(val, data));
            if !found {
                violations.push((path.to_string(), "value not in allowed enum".to_string()));
            }
        }
    }

    // Check fields constraint (for dicts)
    if let Some(&fields_thunk_id) = schema.get(&StrKey("fields")) {
        let fields_thunk = ctx.get_thunk(fields_thunk_id);
        let fields_val = materialize_sync(&fields_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref fields_schema) = fields_val {
            if let Value::Dict(ref data_dict) = data {
                // Validate each field in the schema
                for (field_key, &field_schema_thunk_id) in fields_schema {
                    let field_schema_thunk = ctx.get_thunk(field_schema_thunk_id);
                    let field_schema_val = materialize_sync(&field_schema_thunk, Some(&span), ctx)?;
                    if let Value::Dict(ref field_schema) = field_schema_val {
                        let field_name = match field_key {
                            Key::String(s) => s.to_string(),
                            Key::Int(i) => i.to_string(),
                        };

                        let field_path = if path.is_empty() {
                            field_name.clone()
                        } else {
                            format!("{}.{}", path, field_name)
                        };

                        // Check if field is required
                        let is_required =
                            if let Some(&req_thunk_id) = field_schema.get(&StrKey("required")) {
                                let req_thunk = ctx.get_thunk(req_thunk_id);
                                let req_val = materialize_sync(&req_thunk, Some(&span), ctx)?;
                                matches!(req_val, Value::Bool(true))
                            } else {
                                false
                            };

                        if let Some(&field_value_thunk_id) = data_dict.get(field_key) {
                            let field_value_thunk = ctx.get_thunk(field_value_thunk_id);
                            let field_value =
                                materialize_sync(&field_value_thunk, Some(&span), ctx)?;
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
    if let Some(&items_thunk_id) = schema.get(&StrKey("items")) {
        let items_thunk = ctx.get_thunk(items_thunk_id);
        let items_val = materialize_sync(&items_thunk, Some(&span), ctx)?;
        if let Value::Dict(ref items_schema) = items_val {
            match data {
                Value::Dict(ref data_dict) => {
                    for (idx, (_key, &val_thunk_id)) in data_dict.iter().enumerate() {
                        let val_thunk = ctx.get_thunk(val_thunk_id);
                        let val = materialize_sync(&val_thunk, Some(&span), ctx)?;
                        let item_path = if path.is_empty() {
                            format!("[{}]", idx)
                        } else {
                            format!("{}[{}]", path, idx)
                        };
                        validate_value(items_schema, &val, &item_path, violations, ctx, span)?;
                    }
                }
                Value::Seq { .. } => {
                    // Validate each element of the Seq against the items schema
                    // Helper closure to recursively walk the Seq spine
                    fn validate_seq_items(
                        seq_val: &Value,
                        items_schema: &IndexMap<Key, ThunkId>,
                        path: &str,
                        idx: usize,
                        violations: &mut Vec<(String, String)>,
                        ctx: &Arc<crate::eval::EvalContext>,
                        span: Span,
                    ) -> EvalResult<()> {
                        match seq_val {
                            Value::Seq { head, tail } => {
                                let head_thunk = ctx.get_thunk(*head);
                                let head_val = materialize_sync(&head_thunk, Some(&span), ctx)?;
                                let item_path = if path.is_empty() {
                                    format!("[{}]", idx)
                                } else {
                                    format!("{}[{}]", path, idx)
                                };
                                validate_value(
                                    items_schema,
                                    &head_val,
                                    &item_path,
                                    violations,
                                    ctx,
                                    span,
                                )?;

                                let tail_thunk = ctx.get_thunk(*tail);
                                let tail_val = materialize_sync(&tail_thunk, Some(&span), ctx)?;
                                validate_seq_items(
                                    &tail_val,
                                    items_schema,
                                    path,
                                    idx + 1,
                                    violations,
                                    ctx,
                                    span,
                                )
                            }
                            Value::Dict(d) if d.is_empty() => {
                                // Empty dict is the Seq terminator
                                Ok(())
                            }
                            _ => {
                                // Malformed Seq (non-empty dict or other value as tail)
                                Ok(())
                            }
                        }
                    }
                    validate_seq_items(data, items_schema, path, 0, violations, ctx, span)?;
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
