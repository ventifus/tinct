//! Type introspection, evaluation control, and meta builtins.
#![allow(clippy::needless_borrow)]
//!
//! These builtins provide type checking, evaluation control, AST manipulation,
//! file inclusion, and schema validation.
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
//! - All type predicates (`int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?`,
//!   `bytes?`, `proxy?`, `num?`, `record?`, `map?`) are implemented in stdlib/prelude.llt
//!   via `[match x TypeTag: true _: false]` — no Rust implementations remain.
//!
//! **AST and evaluation:**
//! - `eval-ast`: Reconstruct and evaluate AST from dict representation
//! - `gensym`: Generate unique symbol names for macro hygiene
//!
//! **Number types:**
//! - `decimal`: Parse/convert to exact decimal (rust_decimal)
//! - `big-int`: Parse/convert to arbitrary-precision integer
//!
//! **I/O:**
//! - `include`: Evaluate external LLT files with cycle detection and integrity checking
//!   (Note: from-json is now implemented in pure tinct in stdlib/codecs/json.llt)
//!
//! **Schema validation:**
//! - `validate`: Runtime structural validation with constraint checking
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration is via `core_builtins()` in `src/builtins_core.rs`, dispatched by
//! `builtin_module("core")` in `src/builtins.rs`.

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::arena::ThunkId;
use crate::ast::{Span, SurfaceExpression};
use crate::builtins::{
    builtin, ok_val, reject_named, require_string, synthetic_call_expr, MAX_COLLECT_SIZE,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, materialize_sync, wrap_with_nominal_validation};
use crate::eval_call::{invoke_function, CallContext};
use crate::eval_materialize::force_dict_tree;
use crate::value::{string_val, BuiltinArgs, Key, Strictness, Thunk, Value};

/// Global counter shared by all gensym call sites. A single counter guarantees
/// globally unique IDs regardless of which scope character is used.
static GENSYM_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Produce a fresh gensym name, advancing the global counter.
///
/// The format is `{scope}ꜱʏᴍ⧼{prefix}⧽{id}`. The scope character encodes provenance:
///
/// | Scope | Codepoint | Provenance |
/// |-------|-----------|------------|
/// | `ℊ`  | U+210A    | user-facing `gensym` via tinct prelude |
/// | `𝜇`  | U+1D707   | μ-binder RecVar names (equirecursive types) |
/// | `𝒩`  | U+1D4A9   | nominal-input validation guards (`eval.rs`) |
/// | `𝒻`  | U+1D4BB   | formatter capture-avoiding renaming (`surface_fmt.rs`) |
///
/// All names are practically unguessable — collision requires deliberate IME input.
/// Use this function at every Rust call site; do not maintain separate local counters.
pub(crate) fn gensym_fresh(scope: char, prefix: &str) -> String {
    use std::sync::atomic::Ordering;
    let id = GENSYM_COUNTER.fetch_add(1, Ordering::Relaxed);
    make_gensym_name_with_scope(scope, prefix, id)
}

/// Format a gensym name with an explicit scope character and ID.
pub(crate) fn make_gensym_name_with_scope(scope: char, prefix: &str, id: u64) -> String {
    format!("{}ꜱʏᴍ⧼{}⧽{}", scope, prefix, id)
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
        let forced = crate::builtins::expect_one_arg(
            "materialize",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
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
        let val = crate::builtins::expect_one_arg(
            "raise",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let msg = require_string("raise", val, args[0].span.clone())?;
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
        crate::builtins::reject_named("builtin-macro-error", named.as_ref(), call_span.clone())?;

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
                    args[0].span.clone(),
                )
                .into());
            }
        };

        // Extract message (second argument)
        let msg_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count=2");
        let message = require_string("builtin-macro-error", msg_val, args[1].span.clone())?;

        // Helper to extract a dict field and materialize it
        let get_dict =
            |parent: &IndexMap<Key, ThunkId>, field: &str| -> EvalResult<IndexMap<Key, ThunkId>> {
                let field_id = parent.get(&Key::String(field.into())).ok_or_else(|| {
                    EvalError::user_error(
                        format!("builtin-macro-error: span dict missing '{}' field", field),
                        args[0].span.clone(),
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
                        field_thunk.span.clone(),
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
                        args[0].span.clone(),
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
                        field_thunk.span.clone(),
                    )
                    .into()),
                    other => Err(EvalError::type_mismatch_ctx(
                        format!("builtin-macro-error ({}.{})", context, field),
                        "Int",
                        other.type_name(),
                        field_thunk.span.clone(),
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

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `[Result.Ok value]`
/// on success or `[Result.Error message]` on failure.
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
        reject_named("try", named.as_ref(), call_span.clone())?;
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
                        call_span.clone(),
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
                    body.span.clone(),
                ));
                materialize(&body_thunk, Some(&call_span), &ctx).await
            }
            Value::Builtin(def) => {
                let builtin_args = BuiltinArgs {
                    args: vec![],
                    named: None,
                    call_span: call_span.clone(),
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
                    call_span.clone(),
                )
                .into())
            }
        };

        match call_result {
            Ok(val) => {
                // Success: return Value::Variant { tag: "Result.Ok", payload: Some(value) }
                let payload_thunk_id = ctx.alloc_thunk(ok_val(val, call_span.clone())?);
                ok_val(
                    Value::Variant {
                        tag: "Result.Ok".to_string(),
                        payload: Some(payload_thunk_id),
                    },
                    call_span,
                )
            }
            Err(e) => {
                // DepthExceeded and ResourceLimitExceeded are non-catchable:
                // they indicate system-level limits, not user-level errors.
                use crate::error::ErrorKind;
                if let ErrorKind::ResourceLimitExceeded { .. } = &e.kind {
                    return Err(e);
                }
                // Error: return Value::Variant { tag: "Result.Error", payload: Some(message) }
                let msg_thunk_id =
                    ctx.alloc_thunk(ok_val(string_val(&e.kind.to_string()), call_span.clone())?);
                ok_val(
                    Value::Variant {
                        tag: "Result.Error".to_string(),
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
/// Rust builtin for performance: avoids per-iteration thunk allocation and memoization
/// overhead that a TCO-optimized LLT tail-recursive version would still incur.
///
/// This implementation uses a Rust loop with eager materialization at each step.
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
        reject_named("until", named.as_ref(), call_span.clone())?;
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
                call_span.clone(),
                Arc::clone(&ctx.config.stdlib_env),
                val_thunk.span.clone(),
                Some(Arc::from("until")),
                Arc::clone(&ctx),
                synthetic_call_expr(call_span.clone()),
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
                        call_span.clone(),
                        Arc::clone(&ctx.config.stdlib_env),
                        call_span.clone(),
                        Some(Arc::from("until")),
                        Arc::clone(&ctx),
                        synthetic_call_expr(call_span.clone()),
                    ));

                    // Eagerly materialize f(val) and re-wrap as a thunk for the next iteration
                    // This breaks the thunk chain and prevents stack overflow
                    let f_val = materialize(&f_result, Some(&call_span), &ctx).await?;
                    val_thunk = Arc::new(Thunk::new_materialized(f_val, call_span.clone()));
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
        reject_named("apply", named.as_ref(), call_span.clone())?;
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

        let arg_dict = crate::builtins::require_dict(
            "apply",
            args_val,
            args[1].span.clone(),
            &ctx,
            call_span.clone(),
        )?;

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
            builtin!("builtin-apply", builtin_apply_impl, [], 2),
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
/// - Zero args: returns `"ℊꜱʏᴍ⧼gensym⧽N"` where N is a global monotonic counter.
/// - One arg (prefix string): returns `"ℊꜱʏᴍ⧼prefix⧽N"` where prefix is the argument.
///
/// The `ℊꜱʏᴍ` prefix (U+210A U+A731 U+028F U+1D0D) and `⧼`/`⧽` brackets (U+29FC/U+29FD)
/// ensure these names are valid tinct identifiers (parseable) while being practically
/// unguessable — collision requires deliberate IME input of these codepoints.
/// `builtin-gensym`: the single Rust gensym primitive.
///
/// Takes exactly two string arguments: `scope` (a single Unicode character encoding
/// provenance) and `prefix` (an arbitrary string label). Returns a globally unique
/// name of the form `{scope}ꜱʏᴍ⧼{prefix}⧽{N}`.
///
/// Not called directly in tinct — use the prelude wrappers:
///   `gensym prefix`            → scope defaults to `ℊ`
///   `gensym-with-scope scope prefix` → explicit scope
pub(crate) fn builtin_gensym(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
        } = ctx_arg;

        reject_named("builtin-gensym", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::user_error(
                format!(
                    "builtin-gensym takes exactly 2 arguments (scope, prefix), got {}",
                    args.len()
                ),
                call_span,
            )
            .into());
        }

        let get_str = |thunk: &Arc<Thunk>, name: &str, span: &Span| -> EvalResult<String> {
            match thunk.try_get_materialized().expect("pre-materialized") {
                Value::String { source, start, end } => Ok(source[start..end].to_string()),
                v => Err(EvalError::type_mismatch_ctx(
                    format!("builtin-gensym {name}"),
                    "String",
                    v.type_name(),
                    span.clone(),
                )
                .into()),
            }
        };

        let scope_str = get_str(&args[0], "scope", &call_span)?;
        let prefix = get_str(&args[1], "prefix", &call_span)?;

        let scope = scope_str.chars().next().unwrap_or('ℊ');
        let name = gensym_fresh(scope, &prefix);
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
            call_span.clone(),
        )?;
        let macro_name = require_string("macro-injects", macro_name_val, args[0].span.clone())?;

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
        let val = crate::builtins::expect_one_arg(
            "decimal",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
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
        let val = crate::builtins::expect_one_arg(
            "big-int",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
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
        let val = crate::builtins::expect_one_arg(
            "type-of",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let name = match val.type_name() {
            "Builtin" => "Function",
            other => other,
        };
        ok_val(string_val(name), call_span)
    })
}

/// `builtin-ast-of` (`ast-of` in prelude): returns the AST of an expression as a `Value::Expression`.
///
/// This builtin does NOT materialize its argument, making it safe to use
/// for introspection of unevaluated expressions.
///
/// - Unevaluated thunks (Surface/AstNodeField) → Value::Expression wrapping the SurfaceNode
/// - Materialized literals (Int/Float/Bool/String) → Value::Expression wrapping a synthetic node
/// - Other materialized values → error (cannot reconstruct original AST)
/// - PendingCall/PendingBuiltin/Guarded/InProgress/Failed → descriptor dict (legacy, will be updated)
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
        crate::builtins::reject_named("builtin-ast-of", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
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
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::Key::String("name".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val(def.name),
                    call_span.clone(),
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
                    call_span.clone(),
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
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("guarded"),
                    call_span.clone(),
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
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("in-progress"),
                    call_span.clone(),
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
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::Key::String("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("failed"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::Key::String("error".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val(&err.kind.to_string()),
                    call_span.clone(),
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

        // Force the thunk if it is in CoreExpr unevaluated state (the only remaining state after
        // the peek checks above).  This is necessary for Value::Function, which only exists as a
        // materialized value — there is no way to reconstruct function metadata from the CoreExpr
        // body without evaluating it.  For already-materialized thunks this is a no-op (cached).
        materialize(thunk, Some(&call_span), &ctx).await?;

        // Check for Materialized — construct synthetic SurfaceNode for simple literals
        // For complex values, ast-of should be called on unevaluated expressions, but we
        // provide a fallback for materialized literals to avoid breaking existing code.
        if let Some(val) = thunk.try_get_materialized() {
            use crate::ast::{SurfaceExpression, SurfaceNode};
            let make_node = |expr: SurfaceExpression| {
                Arc::new(SurfaceNode {
                    expr,
                    span: call_span.clone(),
                })
            };

            // Handle Value::Function — build a metadata dict: {type: "fn", doc: ..., return-ann: ..., params: [...]}
            if let crate::value::Value::Function {
                params, annotation, ..
            } = &val
            {
                let mut dict = IndexMap::new();

                // type: "fn"
                dict.insert(
                    Key::String("type".into()),
                    ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                        string_val("fn"),
                        call_span.clone(),
                    ))),
                );

                // doc: string or empty string
                let doc_str = annotation
                    .as_ref()
                    .and_then(|a| a.doc.as_deref())
                    .unwrap_or("");
                dict.insert(
                    Key::String("doc".into()),
                    ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                        string_val(doc_str),
                        call_span.clone(),
                    ))),
                );

                // return-ann: annotation dict or empty dict (null)
                let return_ann_tid = match annotation.as_ref().and_then(|a| a.return_ann.as_ref()) {
                    Some(ann) => crate::surface_convert::annotation_to_thunk_id(
                        ann,
                        call_span.clone(),
                        &ctx,
                    )?,
                    None => ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                        Value::Dict(IndexMap::new()),
                        call_span.clone(),
                    ))),
                };
                dict.insert(Key::String("return-ann".into()), return_ann_tid);

                // params: integer-keyed dict of param entry dicts [{name: "x", annotation: ...}, ...]
                let param_tids: Vec<crate::arena::ThunkId> = params
                    .iter()
                    .map(|p| {
                        let mut param_dict = IndexMap::new();
                        param_dict.insert(
                            Key::String("name".into()),
                            ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                                string_val(&p.name),
                                call_span.clone(),
                            ))),
                        );
                        if let Some(ann) = &p.annotation {
                            let ann_tid = crate::surface_convert::annotation_to_thunk_id(
                                &ann.node,
                                call_span.clone(),
                                &ctx,
                            )?;
                            param_dict.insert(Key::String("annotation".into()), ann_tid);
                        }
                        Ok(
                            ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                                Value::Dict(param_dict),
                                call_span.clone(),
                            ))),
                        )
                    })
                    .collect::<crate::error::EvalResult<Vec<_>>>()?;

                let params_tid = crate::surface_convert::list_to_thunk_id(
                    param_tids.into_iter(),
                    call_span.clone(),
                    &ctx,
                )?;
                dict.insert(Key::String("params".into()), params_tid);

                return Ok(Arc::new(crate::value::Thunk::new_materialized(
                    Value::Dict(dict),
                    call_span,
                )));
            }

            let synthetic_node = match val {
                crate::value::Value::Int(n) => make_node(SurfaceExpression::Int(n)),
                crate::value::Value::U64(n) => make_node(SurfaceExpression::U64(n)),
                crate::value::Value::Float(f) => make_node(SurfaceExpression::Float(f)),
                crate::value::Value::Bool(b) => make_node(SurfaceExpression::Bool(b)),
                crate::value::Value::String { source, start, end } => {
                    make_node(SurfaceExpression::Str(source[start..end].to_string()))
                }
                // For other materialized values (Builtin, Dict, etc.), we cannot
                // reconstruct the original AST, so error out.
                _ => {
                    return Err(EvalError::type_mismatch(
                        "Expression",
                        val.type_name(),
                        call_span.clone(),
                    )
                    .into());
                }
            };

            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                Value::Expression(synthetic_node),
                call_span,
            )));
        }

        // Placeholder or unknown state (should not be observable in user code)
        Err(EvalError::internal("ast-of: thunk in unknown state".to_string(), call_span).into())
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
        let val = crate::builtins::expect_one_arg(
            "llt-repr",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        // value_to_display_string materializes nested values on demand via visit_value
        let display_str = crate::value_to_display_string(&val, &ctx, call_span.clone())
            .map_err(|e| EvalError::internal(format!("llt-repr: {}", e.kind), call_span.clone()))?;
        ok_val(string_val(&display_str), call_span)
    })
}

/// `tag-of`: Return the tag of a Variant as a String.
///
/// After T-974 (S-845), user-defined ADT constructors carry qualified tags (e.g.,
/// `"Result.Ok"` instead of `"Ok"`). `tag-of` returns the full qualified tag. Code that
/// compares `[= [tag-of x] "Ok"]` must be updated to use `[= [tag-of x] "Result.Ok"]`
/// or use pattern matching instead.
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
        let val = crate::builtins::expect_one_arg(
            "tag-of",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
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

/// `annotation-of`: return the annotation dict for a value.
///
/// - `Value::Function { annotation, .. }` — returns a `Value::Dict` built from the
///   `FnAnnotation`: `doc:` (string, if present) plus all fields from `annotation.extra`.
///   `return_ann` is an AST-level construct, not a plain Value; it is intentionally omitted
///   from the runtime dict. Callers that need the return annotation should use `ast-of`.
/// - `Value::Annotated { annotation, .. }` — returns the annotation dict directly.
/// - All other values — returns an empty dict `{}`.
///
/// This builtin does NOT force its argument beyond WHNF (pos_strictness[0] = Seq).
/// The annotation dict is available after WHNF without further forcing.
pub(crate) fn builtin_annotation_of(
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
            "annotation-of",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        match val {
            Value::Function { annotation, .. } => {
                // Build the annotation dict from FnAnnotation fields.
                // `doc` is a well-known field; `extra` holds all custom fields.
                let mut entries: IndexMap<Key, ThunkId> = IndexMap::new();

                if let Some(ann) = annotation.as_deref() {
                    // Include `doc` field if present
                    if let Some(ref doc_str) = ann.doc {
                        entries.insert(
                            Key::String("doc".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(doc_str),
                                call_span.clone(),
                            ))),
                        );
                    }
                    // Flatten all extra fields into the dict
                    for (key, extra_val) in &ann.extra {
                        entries.insert(
                            Key::String(key.as_str().into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                extra_val.clone(),
                                call_span.clone(),
                            ))),
                        );
                    }
                }

                ok_val(Value::Dict(entries), call_span)
            }
            Value::Annotated { annotation, .. } => {
                // Return the annotation value directly — no materialization needed,
                // it was stored materialized at annotation construction time.
                ok_val(*annotation, call_span)
            }
            _ => {
                // All other values have no annotation — return empty dict.
                ok_val(Value::Dict(IndexMap::new()), call_span)
            }
        }
    })
}

/// `make-annotated`: Wrap a value in `Value::Annotated` with the given annotation dict.
///
/// Forms:
/// - `[make-annotated value annotation-dict]` — returns `Value::Annotated { inner: value, annotation: annotation-dict }`
///
/// Used internally by the desugar pass (`build_constructor_value`) to wrap unit constructor
/// values in `Value::Annotated` when the constructor carries a `@[...]` annotation (T-1121).
/// The annotation dict must be a `Value::Dict`; passing any other type is a type error.
///
/// Both arguments are pre-materialized by `pos_strictness = [Seq, Seq]`.
pub(crate) fn builtin_make_annotated(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("make-annotated", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let inner_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let ann_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[1]=Seq");

        // annotation must be a Dict
        if !matches!(ann_val, Value::Dict(_)) {
            return Err(Box::new(EvalError::type_mismatch(
                "Dict",
                ann_val.type_name(),
                call_span,
            )));
        }

        ok_val(
            Value::Annotated {
                inner: Box::new(inner_val),
                annotation: Box::new(ann_val),
            },
            call_span,
        )
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
        crate::builtins::reject_named("variant", named.as_ref(), call_span.clone())?;

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
                                | "CaseArm"
                        );

                        if is_ast_variant {
                            // Convert payload dict to SurfaceNode using dict_to_surface_node.
                            // Deep-materialize all nested dict values so dict_to_surface_node can access them.
                            // dict_to_surface_node uses try_get_materialized on all field thunks.
                            let payload_val =
                                materialize(payload_thunk, Some(&call_span), &ctx).await?; // H2: conditional force — only when tag is a known AST variant name
                            let deep_payload = force_dict_tree(&payload_val, &ctx).await?;
                            // Wrap as Variant so dict_to_surface_node can extract the tag
                            let payload_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                deep_payload,
                                call_span.clone(),
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
                                            call_span.clone(),
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

// All type predicates (int?, float?, str?, bool?, null?, dict?, fn?, seq?, bytes?, proxy?)
// are implemented in stdlib/prelude.llt via [match x TypeTag: true _: false].
// The Rust implementations were removed in the type-predicates-to-tinct sprint.

/// Helper for runtime type name extraction.
fn type_name(val: &Value) -> String {
    match val {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::String { .. } => "String",
        Value::Bool(_) => "Bool",
        Value::Bytes { .. } => "Bytes",
        Value::Dict(_) | Value::Overlay(..) => "Dict",
        Value::Variant { ref tag, .. } if tag == "Seq.Cons" || tag == "Seq.Nil" => "Seq",
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
        Value::ReactiveCell(_) => "ReactiveCell",
        Value::Builder(_) => "Builder",
        Value::BroadcastChannel(_) => "BroadcastChannel",
        Value::OneshotSender(_) => "OneshotSender",
        Value::OneshotReceiver(_) => "OneshotReceiver",
        Value::U64(_) => "U64",
        // Annotated is transparent — delegate to inner value's type_name.
        Value::Annotated { inner, .. } => return type_name(inner),
    }
    .to_string()
}

// DELETED: json_to_value and builtin_from_json (json-serde-removal sprint)
// JSON parsing is now handled by the pure-tinct from-json implementation
// in stdlib/codecs/json.llt, which is exported by the prelude.

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
        let val = crate::builtins::expect_one_arg(
            "blake3",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("blake3", val, args[0].span.clone())?;
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
            call_span.clone(),
        )?;

        let dir = match &val {
            Value::DirCap { dir, .. } => Rc::clone(dir),
            Value::RevocableDirCap { inner, revoked, .. } => {
                if revoked.get() {
                    return Err(EvalError::internal(
                        "cap-identity: capability has been revoked".to_string(),
                        call_span.clone(),
                    )
                    .into());
                }
                Rc::clone(inner)
            }
            _ => {
                return Err(EvalError::type_mismatch(
                    "DirCap",
                    val.type_name(),
                    args[0].span.clone(),
                )
                .into());
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
                    call_span.clone(),
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
                let name_str = require_string("load", name_val, name_thunk.span.clone())?;
                Some(name_str)
            } else {
                None
            };
            let expected_hash = if let Some(hash_thunk) = named_map.get("hash") {
                let hash_val = materialize(hash_thunk, Some(&call_span), &ctx).await?; // H2: conditional (only when hash: named arg present)
                let hash_str = require_string("load", hash_val, hash_thunk.span.clone())?;
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
        let source = require_string("load", source_val, args[0].span.clone())?;

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
            EvalError::user_error(
                format!("load: parse error in \"{}\": {}", display_name, e),
                call_span.clone(),
            )
        })?;

        // load returns the raw parsed SurfaceProgram with empty tables.
        // Formatters need the unexpanded AST. Callers that need expansion
        // should call `[expand [load ...]]` explicitly.
        let program = parsed.program;
        let program_value = Value::Program {
            program: std::sync::Arc::new(program),
            resolutions: std::sync::Arc::new(crate::ast::ResolutionTable::new()),
            types: std::sync::Arc::new(crate::ast::TypeAnnotationTable::new()),
            expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
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
        crate::builtins::reject_named("expand", named.as_ref(), call_span.clone())?;
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
                expects_resolved: _old_expects_resolved,
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
                        call_span.clone(),
                    )
                })?;
                // Desugar $_ patterns introduced by macros.
                crate::desugar::desugar_surface_program(&mut new_surface_program);
                // Inject ADT constructor bindings (must run after desugar, before resolve).
                crate::desugar::inject_adt_constructors_surface_program(&mut new_surface_program);
                // Transform instance decls to method dicts (T-1142).
                crate::desugar::desugar_instance_decls_surface_program(&mut new_surface_program);

                // Re-compute resolution table for the expanded and desugared program
                let new_resolutions = crate::resolve::resolve_surface_program(&new_surface_program);

                // Typecheck to populate TypeAnnotationTable for static type resolution in TypeAssert nodes.
                // Type errors are advisory — eval proceeds regardless. Callers that care
                // about type errors use `builtin_eval_types`.
                let (_annotation_errors, type_annotation_table, new_expects_resolved) =
                    crate::typecheck::typecheck_surface_program_annotation_table(
                        &new_surface_program,
                    );

                // Return as Value::Program with fresh resolution, type, and expects_resolved tables
                ok_val(
                    Value::Program {
                        program: std::sync::Arc::new(new_surface_program),
                        resolutions: std::sync::Arc::new(new_resolutions),
                        types: std::sync::Arc::new(type_annotation_table),
                        expects_resolved: std::sync::Arc::new(new_expects_resolved),
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

/// `builtin-program`: Construct a `Value::Program` from a sequence of Document values.
///
/// Takes a single positional argument: a Seq or Dict of `Value::Document` values.
/// Returns a `Value::Program` with the documents wrapped in a `SurfaceProgram` structure.
///
/// This is the primitive for reconstructing programs after transformation (e.g., desugar.llt).
/// The resolution, type annotation, and expects_resolved tables are initialized as empty —
/// callers should use `expand` or other builtins to populate them if needed.
///
/// Example usage in desugar.llt:
/// ```llt
/// [program [map desugar-document p.documents]]
/// ```
pub(crate) fn builtin_program(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;
        crate::builtins::reject_named("builtin-program", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Extract documents from the input collection (Seq or Dict)
        let mut documents = Vec::new();

        match val {
            Value::Dict(map) => {
                // Iterate through dict entries in insertion order
                for (_key, thunk_id) in map.into_iter() {
                    let thunk = ctx.get_thunk(thunk_id);
                    let doc_val = crate::eval::materialize(&thunk, Some(&call_span), &ctx).await?;
                    match doc_val {
                        Value::Document(surface_doc) => {
                            // Wrap the SurfaceDocument in a Spanned with origin span.
                            // The document is reconstructed/transformed, so we use origin()
                            // to indicate synthetic content. The original spans are preserved
                            // in the expression nodes inside the SurfaceDocument.
                            documents.push(crate::ast::Spanned {
                                node: (*surface_doc).clone(),
                                span: crate::ast::Span::origin(),
                            });
                            if documents.len() >= MAX_COLLECT_SIZE {
                                return Err(EvalError::resource_limit_exceeded(
                                    format!(
                                        "builtin-program: exceeded maximum document count ({})",
                                        MAX_COLLECT_SIZE
                                    ),
                                    call_span,
                                )
                                .into());
                            }
                        }
                        _ => {
                            return Err(EvalError::type_mismatch_ctx(
                                "builtin-program".to_string(),
                                "Document",
                                doc_val.type_name(),
                                call_span,
                            )
                            .into());
                        }
                    }
                }
            }
            ref v if crate::value::is_seq(v) => {
                // Collect all seq elements
                let mut current = val;
                loop {
                    match current {
                        Value::Variant {
                            ref tag,
                            payload: None,
                        } if tag == "Seq.Nil" => {
                            // Seq.Nil is the terminator
                            break;
                        }
                        Value::Variant {
                            ref tag,
                            payload: Some(payload_id),
                        } if tag == "Seq.Cons" => {
                            let payload_thunk = ctx.get_thunk(payload_id);
                            let payload_val =
                                crate::eval::materialize(&payload_thunk, Some(&call_span), &ctx)
                                    .await?;
                            let (head, tail) = if let Value::Dict(ref d) = payload_val {
                                let head = *d
                                    .get(&crate::value::Key::String("head".into()))
                                    .expect("Seq.Cons must have head");
                                let tail = *d
                                    .get(&crate::value::Key::String("tail".into()))
                                    .expect("Seq.Cons must have tail");
                                (head, tail)
                            } else {
                                return Err(EvalError::internal(
                                    "Seq.Cons payload must be a Dict".to_string(),
                                    call_span,
                                )
                                .into());
                            };
                            let head_thunk = ctx.get_thunk(head);
                            let doc_val =
                                crate::eval::materialize(&head_thunk, Some(&call_span), &ctx)
                                    .await?;
                            match doc_val {
                                Value::Document(surface_doc) => {
                                    documents.push(crate::ast::Spanned {
                                        node: (*surface_doc).clone(),
                                        span: crate::ast::Span::origin(),
                                    });
                                    if documents.len() >= MAX_COLLECT_SIZE {
                                        return Err(EvalError::resource_limit_exceeded(
                                            format!(
                                                "builtin-program: exceeded maximum document count ({})",
                                                MAX_COLLECT_SIZE
                                            ),
                                            call_span,
                                        )
                                        .into());
                                    }
                                }
                                _ => {
                                    return Err(EvalError::type_mismatch_ctx(
                                        "builtin-program".to_string(),
                                        "Document",
                                        doc_val.type_name(),
                                        call_span,
                                    )
                                    .into());
                                }
                            }
                            let tail_thunk = ctx.get_thunk(tail);
                            current = crate::eval::materialize(&tail_thunk, Some(&call_span), &ctx)
                                .await?;
                        }
                        _ => {
                            return Err(EvalError::type_mismatch_ctx(
                                "builtin-program".to_string(),
                                "Seq or Dict",
                                current.type_name(),
                                call_span,
                            )
                            .into());
                        }
                    }
                }
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-program".to_string(),
                    "Seq or Dict",
                    val.type_name(),
                    call_span,
                )
                .into());
            }
        }

        // Construct the SurfaceProgram
        let surface_program = crate::ast::SurfaceProgram { documents };

        // Return as Value::Program with empty tables (caller can run expand/resolve if needed)
        ok_val(
            Value::Program {
                program: std::sync::Arc::new(surface_program),
                resolutions: std::sync::Arc::new(crate::ast::ResolutionTable::new()),
                types: std::sync::Arc::new(crate::ast::TypeAnnotationTable::new()),
                expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
            },
            call_span,
        )
    })
}

/// `builtin-module`: Returns a dict of all builtins in the named module.
///
/// Takes a module name (String) and returns a Dict mapping builtin names to their
/// implementations. Used by the bootstrap loader to inject modules into the stdlib
/// environment.
///
/// Available modules: "core", "datetime", "net"
pub(crate) fn builtin_builtin_module(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        let name_val = crate::builtins::expect_one_arg(
            "builtin-module",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        let name = require_string("builtin-module", name_val, call_span.clone())?;

        match crate::builtins::builtin_module(&name) {
            Some(defs) => {
                // Convert Vec<BuiltinDef> to a Dict
                let mut dict_map = IndexMap::new();
                for def in defs {
                    let builtin_thunk = Arc::new(Thunk::new_materialized(
                        Value::Builtin(def),
                        call_span.clone(),
                    ));
                    let thunk_id = ctx.alloc_thunk(builtin_thunk);
                    dict_map.insert(Key::String(def.name.into()), thunk_id);
                }
                ok_val(Value::Dict(dict_map), call_span)
            }
            None => Err(EvalError::user_error(
                format!("unknown native module: {:?}", name),
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
/// - `scope:` (Dict) — bindings injected after env (for module loading)
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

        // Extract optional env:, scope:, %:, program:, and expects: named args
        let (env_dict, scope_dict, pipeline_input, program_opt, expects_opt) =
            if let Some(named_map) = named {
                // Reject unknown named args
                for key in named_map.keys() {
                    if key != "env"
                        && key != "scope"
                        && key != "%"
                        && key != "program"
                        && key != "expects"
                    {
                        return Err(
                            EvalError::named_arg_rejected("eval".to_string(), call_span).into()
                        );
                    }
                }

                let env_dict = named_map.get("env").map(Arc::clone);
                let scope_dict = named_map.get("scope").map(Arc::clone);
                let pipeline_input = named_map.get("%").map(Arc::clone);
                let program_opt = named_map.get("program").map(Arc::clone);
                let expects_opt = named_map.get("expects").map(Arc::clone);
                (
                    env_dict,
                    scope_dict,
                    pipeline_input,
                    program_opt,
                    expects_opt,
                )
            } else {
                (None, None, None, None, None)
            };

        // Get resolution, type, and expects_resolved tables from the program: argument if provided.
        // - res_table and types_table are used by eval_document_exprs when lowering expressions.
        // - program_expects_resolved is used by expects: validation to look up pre-resolved types
        //   for pipeline input contracts, keyed by the annotation's span.
        let (res_table, types_table, program_expects_resolved) =
            if let Some(ref program_thunk) = program_opt {
                let program_val = materialize(program_thunk, Some(&call_span), &ctx).await?; // H2: conditional force — only when program: named arg is present
                match program_val {
                    Value::Program {
                        resolutions,
                        types,
                        expects_resolved,
                        ..
                    } => (
                        Arc::clone(&resolutions),
                        Arc::clone(&types),
                        Arc::clone(&expects_resolved),
                    ),
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
                    Arc::new(std::collections::HashMap::new()),
                )
            };

        // Validate pipeline input against expects: annotation if provided.
        // expects: is either [] (null — skip) or Value::Expression(TypeAssert node from doc.expects).
        // Look up the annotation's resolved type in program_expects_resolved (keyed by annotation
        // span), then wrap the pipeline input as a lazy GuardedThunk via wrap_with_nominal_validation.
        // This mirrors eval_surface_document_pipeline's `--- expects: @Type` handling exactly.
        let validated_pipeline_input = if let Some(expects_thunk) = expects_opt {
            let expects_val = materialize(&expects_thunk, Some(&call_span), &ctx).await?;
            match expects_val {
                // Empty dict == null (no expects annotation)
                Value::Dict(ref m) if m.is_empty() => pipeline_input.clone(),
                // Expression node from doc.expects — a synthetic TypeAssert wrapping VarRef("%").
                // Extract the annotation and look up its resolved type in expects_resolved.
                Value::Expression(ref node) => {
                    match &node.expr {
                        SurfaceExpression::TypeAssert { annotation, .. } => {
                            // Look up the pre-resolved type from the typecheck pass.
                            // If not found (e.g. dynamic code not type-checked), resolved_type
                            // falls back to None and wrap_with_nominal_validation uses Unknown
                            // (gradual typing: Unknown ~<: T for all T — annotation is a no-op).
                            let resolved_type =
                                program_expects_resolved.get(&annotation.span).cloned();
                            // Get or create the pipeline input thunk to wrap.
                            let input_thunk = match pipeline_input {
                                Some(ref t) => Arc::clone(t),
                                None => Arc::new(crate::value::Thunk::new_materialized(
                                    Value::Dict(indexmap::IndexMap::new()),
                                    call_span.clone(),
                                )),
                            };
                            Some(wrap_with_nominal_validation(
                                input_thunk,
                                annotation,
                                resolved_type,
                                call_span.clone(),
                                &ctx,
                                None, // no pipeline blame for $eval (not a --- boundary)
                            ))
                        }
                        // Non-TypeAssert expression node in expects: position — not expected,
                        // but treat as no-op (pass pipeline_input through unchanged).
                        _ => pipeline_input.clone(),
                    }
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "[] or Expression (for expects: argument)",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            pipeline_input.clone()
        };

        // Start with stdlib environment
        let base_env = Arc::clone(&ctx.config.stdlib_env);

        // Add env: dict bindings if provided
        let env_with_bindings = if let Some(env_thunk) = env_dict {
            let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
            // Flatten Overlay to Dict before processing env bindings
            let env_val = match env_val {
                Value::Overlay(l, r) => Value::Dict(crate::builtins::flatten_overlay(
                    &l,
                    &r,
                    "eval",
                    &ctx,
                    call_span.clone(),
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

        // Add scope: dict bindings if provided (injected after env:)
        let env_with_scope = if let Some(scope_thunk) = scope_dict {
            let scope_val = materialize(&scope_thunk, Some(&call_span), &ctx).await?;
            // Flatten Overlay to Dict before processing scope bindings
            let scope_val = match scope_val {
                Value::Overlay(l, r) => Value::Dict(crate::builtins::flatten_overlay(
                    &l,
                    &r,
                    "eval",
                    &ctx,
                    call_span.clone(),
                )?),
                other => other,
            };
            match scope_val {
                Value::Dict(entries) => {
                    // Create child environment with dict entries as bindings
                    let child_env = Arc::new(std::sync::RwLock::new(
                        crate::value::Environment::with_parent(Arc::clone(&env_with_bindings)),
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
                        scope_val.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            env_with_bindings
        };

        // Add %: (pipeline input) as % binding if provided (using validated input)
        let final_env = if let Some(input_thunk) = validated_pipeline_input {
            let child_env = Arc::new(std::sync::RwLock::new(
                crate::value::Environment::with_parent(Arc::clone(&env_with_scope)),
            ));
            child_env
                .write()
                .unwrap()
                .insert("%".to_string(), input_thunk);
            child_env
        } else {
            env_with_scope
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
                Value::Variant {
                    ref tag,
                    payload: None,
                } if tag == "Seq.Nil" => {
                    // End of sequence
                    break;
                }
                Value::Variant {
                    ref tag,
                    payload: Some(payload_id),
                } if tag == "Seq.Cons" => {
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_val = materialize(&payload_thunk, Some(&call_span), &ctx).await?;
                    let (head, tail) = if let Value::Dict(ref d) = payload_val {
                        let head = *d
                            .get(&crate::value::Key::String("head".into()))
                            .expect("Seq.Cons must have head");
                        let tail = *d
                            .get(&crate::value::Key::String("tail".into()))
                            .expect("Seq.Cons must have tail");
                        (head, tail)
                    } else {
                        return Err(EvalError::internal(
                            "Seq.Cons payload must be a Dict".to_string(),
                            call_span,
                        )
                        .into());
                    };
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

        // Delegate the scope-chaining loop to the shared eval_document_exprs function.
        // It handles empty slices, intermediate Dict/Overlay binding promotion, and lazy
        // last-expression return — eliminating the duplication that existed here before.
        crate::eval::eval_document_exprs(
            &expression_nodes,
            final_env,
            &ctx,
            &res_table,
            &types_table,
        )
        .await
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

        // Add %: (pipeline input) as % binding if provided
        let final_env = if let Some(input_thunk) = pipeline_input {
            let child_env = Arc::new(std::sync::RwLock::new(
                crate::value::Environment::with_parent(Arc::clone(&env_with_bindings)),
            ));
            child_env
                .write()
                .unwrap()
                .insert("%".to_string(), input_thunk);
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
                Value::Variant {
                    ref tag,
                    payload: None,
                } if tag == "Seq.Nil" => {
                    // End of sequence
                    break;
                }
                Value::Variant {
                    ref tag,
                    payload: Some(payload_id),
                } if tag == "Seq.Cons" => {
                    let payload_thunk = ctx.get_thunk(payload_id);
                    let payload_val = materialize(&payload_thunk, Some(&call_span), &ctx).await?;
                    let (head, tail) = if let Value::Dict(ref d) = payload_val {
                        let head = *d
                            .get(&crate::value::Key::String("head".into()))
                            .expect("Seq.Cons must have head");
                        let tail = *d
                            .get(&crate::value::Key::String("tail".into()))
                            .expect("Seq.Cons must have tail");
                        (head, tail)
                    } else {
                        return Err(EvalError::internal(
                            "Seq.Cons payload must be a Dict".to_string(),
                            call_span,
                        )
                        .into());
                    };
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
        let mut result_seq = crate::value::make_seq_nil();
        for node in expression_nodes.into_iter().rev() {
            let surface_thunk = Arc::new(Thunk::new_surface(
                node,
                Arc::clone(&res_table),
                Arc::clone(&types_table),
                Arc::clone(&final_env),
                Arc::clone(&ctx),
                call_span.clone(),
            ));
            let surface_thunk_id = ctx.alloc_thunk(surface_thunk);

            let tail_thunk_id = ctx.alloc_thunk(ok_val(result_seq, call_span.clone())?);
            result_seq = crate::value::make_seq_cons(surface_thunk_id, tail_thunk_id, &ctx);
        }

        ok_val(result_seq, call_span)
    })
}

/// `include-cache-get`: look up the string-keyed include cache by blake3 key.
///
/// Takes 1 positional arg (String key). Returns:
/// - `IncludeCacheEntry.Missing`       — key not in cache
/// - `IncludeCacheEntry.Pending`       — key is marked as in-progress (cycle detection)
/// - `IncludeCacheEntry.Cached value`  — cached result thunk
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
            call_span.clone(),
        )?;
        let key = require_string("include-cache-get", val, args[0].span.clone())?;

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
                    tag: "IncludeCacheEntry.Missing".to_string(),
                    payload: None,
                },
                call_span,
            ),
            Some(crate::eval::IncludeCacheEntry::Missing) => ok_val(
                Value::Variant {
                    tag: "IncludeCacheEntry.Missing".to_string(),
                    payload: None,
                },
                call_span,
            ),
            Some(crate::eval::IncludeCacheEntry::Pending) => ok_val(
                Value::Variant {
                    tag: "IncludeCacheEntry.Pending".to_string(),
                    payload: None,
                },
                call_span,
            ),
            Some(crate::eval::IncludeCacheEntry::Cached(thunk, _res, _types)) => {
                let payload_id = ctx.alloc_thunk(Arc::clone(&thunk));
                ok_val(
                    Value::Variant {
                        tag: "IncludeCacheEntry.Cached".to_string(),
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
/// The value must be an `IncludeCacheEntry.Missing`, `IncludeCacheEntry.Pending`, or `IncludeCacheEntry.Cached x` Variant.
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

        reject_named("include-cache-put", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let key_val = args[0]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = require_string("include-cache-put", key_val, args[0].span.clone())?;

        // The second arg is the entry variant: [Missing], [Pending], or [Cached value]
        let entry_val = args[1]
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let entry = match &entry_val {
            Value::Variant { tag, payload } => {
                // Strip qualifier prefix ("IncludeCacheEntry.Pending" → "Pending") for
                // compatibility with T-974 qualified variant tags from inject_adt_constructors_expr.
                let original_tag = tag.as_str();
                let tag_name = tag
                    .strip_prefix("IncludeCacheEntry.")
                    .unwrap_or(tag.as_str());
                match tag_name {
                    "Missing" => crate::eval::IncludeCacheEntry::Missing,
                    "Pending" => crate::eval::IncludeCacheEntry::Pending,
                    "Cached" => {
                        let payload_thunk = match payload {
                            Some(id) => ctx.get_thunk(*id),
                            None => {
                                return Err(EvalError::type_mismatch_ctx(
                                    "include-cache-put".to_string(),
                                    "IncludeCacheEntry.Cached value",
                                    "IncludeCacheEntry.Cached (no payload)",
                                    args[1].span.clone(),
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
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "include-cache-put".to_string(),
                            "IncludeCacheEntry.Missing | IncludeCacheEntry.Pending | IncludeCacheEntry.Cached value",
                            original_tag,
                            args[1].span.clone(),
                        )
                        .into())
                    }
                } // close match tag_name
            }
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "include-cache-put".to_string(),
                    "IncludeCacheEntry.Missing | IncludeCacheEntry.Pending | IncludeCacheEntry.Cached value",
                    entry_val.type_name(),
                    args[1].span.clone(),
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
        let (schema, data) =
            expect_two_args("validate", &args, named.as_ref(), &ctx, call_span.clone())?;

        // Schema must be a Dict
        let schema_dict = match schema {
            Value::Dict(ref d) => d.clone(),
            Value::Overlay(..) => {
                // Materialize Overlay to Dict before validation
                let schema_thunk_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    schema.clone(),
                    call_span.clone(),
                )));
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

        // Collect violations. validate_value is async-recursive via Box::pin.
        let violations = validate_value(
            schema_dict,
            data.clone(),
            String::new(),
            Arc::clone(&ctx),
            call_span.clone(),
        )
        .await?;

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

/// Return type alias for `validate_value` and `validate_seq_items`.
///
/// Both functions are async-recursive (they call each other), so their return types must be
/// `Pin<Box<dyn Future<...>>>`. This alias avoids the "very complex type" Clippy warning.
type ValidationFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Vec<(String, String)>>>>>;

/// Recursive validation helper.
///
/// `path` is the dot-separated field path (e.g., "user.address.zip").
/// Returns a list of violations found; an empty list means the value is valid.
///
/// Uses `Box::pin(async move { ... })` to support mutual recursion with async
/// `validate_seq_items`. Takes owned parameters so all data can be moved into the future.
fn validate_value(
    schema: IndexMap<Key, ThunkId>,
    data: Value,
    path: String,
    ctx: Arc<crate::eval::EvalContext>,
    span: Span,
) -> ValidationFuture {
    Box::pin(async move {
        use crate::value::StrKey;
        let mut violations: Vec<(String, String)> = Vec::new();

        // Check `type` constraint
        if let Some(&type_thunk_id) = schema.get(&StrKey("type")) {
            let type_thunk = ctx.get_thunk(type_thunk_id);
            let type_val = materialize_sync(&type_thunk, Some(&span), &ctx)?;
            if let Value::String {
                ref source,
                start,
                end,
            } = type_val
            {
                let expected_type = &source[start..end];
                let actual_type = type_name(&data);
                if expected_type != actual_type {
                    violations.push((
                        path.clone(),
                        format!("expected type {}, got {}", expected_type, actual_type),
                    ));
                }
            }
        }

        // Check numeric range constraints (min, max)
        if let Some(&min_thunk_id) = schema.get(&StrKey("min")) {
            let min_thunk = ctx.get_thunk(min_thunk_id);
            let min_val = materialize_sync(&min_thunk, Some(&span), &ctx)?;
            match (&data, &min_val) {
                (Value::Int(n), Value::Int(min)) if n < min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Float(n), Value::Float(min)) if n < min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Int(n), Value::Float(min)) if (*n as f64) < *min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Float(n), Value::Int(min)) if *n < (*min as f64) => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                _ => {}
            }
        }

        if let Some(&max_thunk_id) = schema.get(&StrKey("max")) {
            let max_thunk = ctx.get_thunk(max_thunk_id);
            let max_val = materialize_sync(&max_thunk, Some(&span), &ctx)?;
            match (&data, &max_val) {
                (Value::Int(n), Value::Int(max)) if n > max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Float(n), Value::Float(max)) if n > max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Int(n), Value::Float(max)) if (*n as f64) > *max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Float(n), Value::Int(max)) if *n > (*max as f64) => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                _ => {}
            }
        }

        // Check string/sequence length constraints
        if let Some(&min_len_thunk_id) = schema.get(&StrKey("min-length")) {
            let min_len_thunk = ctx.get_thunk(min_len_thunk_id);
            let min_len_val = materialize_sync(&min_len_thunk, Some(&span), &ctx)?;
            if let Value::Int(min_len) = min_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                    } => Some((end - start) as i64),
                    Value::Dict(d) => Some(d.len() as i64),
                    ref v if crate::value::is_seq(v) => {
                        // For Seq, walking the spine to count is expensive.
                        // Skip for now; document limitation.
                        None
                    }
                    _ => None,
                };
                if let Some(len) = actual_len {
                    if len < min_len {
                        violations.push((path.clone(), format!("length must be >= {}", min_len)));
                    }
                }
            }
        }

        if let Some(&max_len_thunk_id) = schema.get(&StrKey("max-length")) {
            let max_len_thunk = ctx.get_thunk(max_len_thunk_id);
            let max_len_val = materialize_sync(&max_len_thunk, Some(&span), &ctx)?;
            if let Value::Int(max_len) = max_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                    } => Some((end - start) as i64),
                    Value::Dict(d) => Some(d.len() as i64),
                    ref v if crate::value::is_seq(v) => None,
                    _ => None,
                };
                if let Some(len) = actual_len {
                    if len > max_len {
                        violations.push((path.clone(), format!("length must be <= {}", max_len)));
                    }
                }
            }
        }

        // Check pattern constraint (for strings)
        if let Some(&pattern_thunk_id) = schema.get(&StrKey("pattern")) {
            let pattern_thunk = ctx.get_thunk(pattern_thunk_id);
            let pattern_val = materialize_sync(&pattern_thunk, Some(&span), &ctx)?;
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
                                    path.clone(),
                                    format!("must match pattern: {}", pattern_str),
                                ));
                            }
                        }
                        Err(_) => {
                            violations.push((
                                path.clone(),
                                format!("invalid regex pattern: {}", pattern_str),
                            ));
                        }
                    }
                }
            }
        }

        // Check enum constraint.
        // Uses the canonical async values_equal so that all value types (including
        // Variant with payload and Dict) are compared correctly.
        if let Some(&enum_thunk_id) = schema.get(&StrKey("enum")) {
            let enum_thunk = ctx.get_thunk(enum_thunk_id);
            let enum_val = materialize_sync(&enum_thunk, Some(&span), &ctx)?;
            if let Value::Dict(ref enum_dict) = enum_val {
                // Pre-materialize all enum values, then check membership via canonical equality.
                let allowed_values: Vec<Value> = enum_dict
                    .iter()
                    .map(|(_key, &val_thunk_id)| {
                        let val_thunk = ctx.get_thunk(val_thunk_id);
                        materialize_sync(&val_thunk, Some(&span), &ctx)
                    })
                    .collect::<EvalResult<Vec<Value>>>()?;

                let mut found = false;
                for allowed in &allowed_values {
                    if crate::eval::values_equal(
                        allowed.clone(),
                        data.clone(),
                        span.clone(),
                        Arc::clone(&ctx),
                    )
                    .await?
                    {
                        found = true;
                        break;
                    }
                }
                if !found {
                    violations.push((path.clone(), "value not in allowed enum".to_string()));
                }
            }
        }

        // Check fields constraint (for dicts)
        if let Some(&fields_thunk_id) = schema.get(&StrKey("fields")) {
            let fields_thunk = ctx.get_thunk(fields_thunk_id);
            let fields_val = materialize_sync(&fields_thunk, Some(&span), &ctx)?;
            if let Value::Dict(ref fields_schema) = fields_val {
                if let Value::Dict(ref data_dict) = data {
                    // Validate each field in the schema
                    for (field_key, &field_schema_thunk_id) in fields_schema {
                        let field_schema_thunk = ctx.get_thunk(field_schema_thunk_id);
                        let field_schema_val =
                            materialize_sync(&field_schema_thunk, Some(&span), &ctx)?;
                        if let Value::Dict(field_schema) = field_schema_val {
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
                            let is_required = if let Some(&req_thunk_id) =
                                field_schema.get(&StrKey("required"))
                            {
                                let req_thunk = ctx.get_thunk(req_thunk_id);
                                let req_val = materialize_sync(&req_thunk, Some(&span), &ctx)?;
                                matches!(req_val, Value::Bool(true))
                            } else {
                                false
                            };

                            if let Some(&field_value_thunk_id) = data_dict.get(field_key) {
                                let field_value_thunk = ctx.get_thunk(field_value_thunk_id);
                                let field_value =
                                    materialize_sync(&field_value_thunk, Some(&span), &ctx)?;
                                let sub_violations = validate_value(
                                    field_schema,
                                    field_value,
                                    field_path,
                                    Arc::clone(&ctx),
                                    span.clone(),
                                )
                                .await?;
                                violations.extend(sub_violations);
                            } else if is_required {
                                violations
                                    .push((field_path, "required field is missing".to_string()));
                            }
                        }
                    }
                }
            }
        }

        // Check items constraint (for sequences/dicts with uniform element schema)
        if let Some(&items_thunk_id) = schema.get(&StrKey("items")) {
            let items_thunk = ctx.get_thunk(items_thunk_id);
            let items_val = materialize_sync(&items_thunk, Some(&span), &ctx)?;
            if let Value::Dict(items_schema) = items_val {
                match &data {
                    Value::Dict(ref data_dict) => {
                        for (idx, (_key, &val_thunk_id)) in data_dict.iter().enumerate() {
                            let val_thunk = ctx.get_thunk(val_thunk_id);
                            let val = materialize_sync(&val_thunk, Some(&span), &ctx)?;
                            let item_path = if path.is_empty() {
                                format!("[{}]", idx)
                            } else {
                                format!("{}[{}]", path, idx)
                            };
                            let sub_violations = validate_value(
                                items_schema.clone(),
                                val,
                                item_path,
                                Arc::clone(&ctx),
                                span.clone(),
                            )
                            .await?;
                            violations.extend(sub_violations);
                        }
                    }
                    ref v if crate::value::is_seq(v) => {
                        // Validate each element of the Seq against the items schema
                        let sub_violations = validate_seq_items(
                            data.clone(),
                            items_schema,
                            path.clone(),
                            0,
                            Arc::clone(&ctx),
                            span.clone(),
                        )
                        .await?;
                        violations.extend(sub_violations);
                    }
                    _ => {}
                }
            }
        }

        Ok(violations)
    })
}

/// `builtin-is-contractive`: check whether a TypeNode value is contractive.
///
/// A TypeNode is contractive iff every path from the root to a `TypeNode.RecursiveRef`
/// node passes through at least one *guarding* constructor. Guarding constructors are
/// those with `guarding: true` in their `@[...]` constructor annotation. In practice:
/// `Record`, `Arrow`, `TypeApplication`, `TypeConstructor`, and the leaf primitives
/// (`Int`, `Float`, `String`, `Bool`, `Absent`, `Unknown`, `Never`) are all guarding.
/// `Union` and `Intersect` are **not** guarding — they are logical combinators that do
/// not structurally interpose between a binder and its reference.
/// `Recursive` itself IS guarding — an inner `μb.T` shields the outer var from any
/// `RecursiveRef` nodes inside `T`.
///
/// Three-case algorithm (from `doc/whatif/equirecursive-types.md §Contractiveness`):
///
/// 1. If the node is `TypeNode.RecursiveRef` → **not contractive** (bare self-reference).
/// 2. If the node's tag is guarding (see list above) → **contractive** (any RecursiveRef
///    underneath is safely separated by a structural layer).
/// 3. Otherwise (Union, Intersect) → recurse into the `types` child Seq; all children
///    must be contractive.
///
/// This builtin takes a single positional arg (the TypeNode value) and returns a `Bool`.
/// Named args are rejected. Pre-materialized by `pos_strictness = [Strictness::Seq]`.
///
/// Used exclusively by `mu` in `stdlib/prelude.llt` (type-stage section) to gate
/// construction of `TypeNode.Recursive` values.
///
/// # Registered as
///
/// `builtin-is-contractive` in `core_builtins()` / `builtins_core.rs`.
/// S-861 will call this from `expand_named` at the annotation resolver level.
// S-860: equirecursive-types-core
pub(crate) fn builtin_is_contractive(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
        } = ctx_arg;

        reject_named("builtin-is-contractive", named.as_ref(), call_span.clone())?;

        let body_val = crate::builtins::expect_one_arg(
            "builtin-is-contractive",
            &args,
            // named already checked above; pass None so expect_one_arg doesn't re-check
            None,
            &ctx,
            call_span.clone(),
        )?;

        let result = is_contractive_value(&body_val, &ctx);
        ok_val(Value::Bool(result), call_span)
    })
}

/// Recursively check contractiveness of a TypeNode value.
///
/// Returns `true` iff the node is contractive — i.e., every path to a
/// `TypeNode.RecursiveRef` passes through at least one guarding constructor.
///
/// The three-case rule from `equirecursive-types.md §Contractiveness`:
///
/// - `TypeNode.RecursiveRef` → false (bare self-reference at this position)
/// - Guarding tag → true (any RecursiveRef below is safely guarded)
/// - `TypeNode.Union` / `TypeNode.Intersect` → recurse into `types` child Seq
///
/// New TypeNode constructors declare `guarding: true` or `guarding: false` in their
/// `@[...]` constructor annotation. This Rust implementation hard-codes the canonical
/// split: Union and Intersect are non-guarding; all others (including Recursive itself)
/// are guarding. S-861 should replace the hard-coded list with annotation-of lookup
/// once the annotation resolver is wired.
fn is_contractive_value(val: &Value, ctx: &Arc<crate::eval::EvalContext>) -> bool {
    // Unwrap Value::Annotated transparently — annotations do not affect contractiveness.
    let val = match val {
        Value::Annotated { inner, .. } => inner.as_ref(),
        other => other,
    };

    match val {
        // Case 1: bare RecursiveRef — non-contractive.
        Value::Variant { tag, .. } if tag == "TypeNode.RecursiveRef" => false,

        // Case 3: Union and Intersect are non-guarding — recurse into all children.
        Value::Variant { tag, payload }
            if tag == "TypeNode.Union" || tag == "TypeNode.Intersect" =>
        {
            // Extract the `types` field from the payload dict.
            // A non-guarding node with a malformed payload is treated as contractive
            // (conservative: don't reject types we can't inspect).
            let payload_id = match payload {
                Some(id) => *id,
                None => return true,
            };
            let payload_thunk = ctx.get_thunk(payload_id);
            let payload_val = match materialize_sync(&payload_thunk, None, ctx) {
                Ok(v) => v,
                Err(_) => return true,
            };
            let types_thunk_id = match &payload_val {
                Value::Dict(d) => match d.get(&crate::value::Key::String("types".into())) {
                    Some(id) => *id,
                    None => return true,
                },
                _ => return true,
            };
            // Walk the `types` Seq spine and check each member.
            is_contractive_seq(types_thunk_id, ctx)
        }

        // Case 2: all other TypeNode constructors are guarding (including Recursive,
        // Record, Arrow, TypeApplication, TypeConstructor, TypeVar, and the leaf
        // primitives Int/Float/String/Bool/Absent/Unknown/Never).
        // A RecursiveRef underneath is safely separated by this structural layer.
        _ => true,
    }
}

/// Walk a `types` Seq spine and check that every element is contractive.
///
/// Returns `true` iff all elements are contractive. An empty Seq or malformed
/// spine is considered contractive (conservative: no self-references = trivially contractive).
fn is_contractive_seq(
    types_thunk_id: crate::arena::ThunkId,
    ctx: &Arc<crate::eval::EvalContext>,
) -> bool {
    let mut current_id = types_thunk_id;
    // Depth limit to guard against malformed cycles.
    let mut depth = 0usize;
    const MAX_DEPTH: usize = 256;

    loop {
        if depth >= MAX_DEPTH {
            // Treat an overlong Seq as contractive — should never occur with valid TypeNode values.
            return true;
        }
        depth += 1;

        let thunk = ctx.get_thunk(current_id);
        let val = match materialize_sync(&thunk, None, ctx) {
            Ok(v) => v,
            Err(_) => return true,
        };

        match &val {
            // Seq.Nil — all elements checked successfully; Seq is contractive.
            Value::Variant { tag, payload: None } if tag == "Seq.Nil" => return true,

            // Seq.Cons { head, tail } — check the head element, then continue on tail.
            Value::Variant {
                tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                let payload_thunk = ctx.get_thunk(*payload_id);
                let payload_val = match materialize_sync(&payload_thunk, None, ctx) {
                    Ok(v) => v,
                    Err(_) => return true,
                };
                let (head_id, tail_id) = match &payload_val {
                    Value::Dict(d) => {
                        let head = d.get(&crate::value::Key::String("head".into())).copied();
                        let tail = d.get(&crate::value::Key::String("tail".into())).copied();
                        match (head, tail) {
                            (Some(h), Some(t)) => (h, t),
                            _ => return true,
                        }
                    }
                    _ => return true,
                };
                let head_thunk = ctx.get_thunk(head_id);
                let head_val = match materialize_sync(&head_thunk, None, ctx) {
                    Ok(v) => v,
                    Err(_) => return true,
                };
                if !is_contractive_value(&head_val, ctx) {
                    return false;
                }
                // Tail-recursion: advance current_id to the tail ThunkId.
                current_id = tail_id;
            }

            // Collected integer-keyed Dict (from [builtin-collect types]) — iterate over values.
            Value::Dict(d) => {
                for (_k, &v_id) in d {
                    let v_thunk = ctx.get_thunk(v_id);
                    let v_val = match materialize_sync(&v_thunk, None, ctx) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if !is_contractive_value(&v_val, ctx) {
                        return false;
                    }
                }
                return true;
            }

            // Unknown shape — treat as contractive.
            _ => return true,
        }
    }
}

/// Walk a Seq spine and validate each element against `items_schema`.
///
/// Separated from `validate_value` so both can be async-recursive without
/// requiring a mutually-recursive `Box::pin` type cycle. Takes owned parameters
/// to enable `async move`.
fn validate_seq_items(
    seq_val: Value,
    items_schema: IndexMap<Key, ThunkId>,
    path: String,
    idx: usize,
    ctx: Arc<crate::eval::EvalContext>,
    span: Span,
) -> ValidationFuture {
    Box::pin(async move {
        match seq_val {
            Value::Variant {
                ref tag,
                payload: None,
            } if tag == "Seq.Nil" => {
                // End of sequence
                Ok(Vec::new())
            }
            Value::Variant {
                ref tag,
                payload: Some(payload_id),
            } if tag == "Seq.Cons" => {
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = materialize_sync(&payload_thunk, Some(&span), &ctx)?;
                let (head, tail) = if let Value::Dict(ref d) = payload_val {
                    let head = *d
                        .get(&Key::String("head".into()))
                        .expect("Seq.Cons must have head");
                    let tail = *d
                        .get(&Key::String("tail".into()))
                        .expect("Seq.Cons must have tail");
                    (head, tail)
                } else {
                    return Err(EvalError::internal(
                        "Seq.Cons payload must be a Dict".to_string(),
                        span,
                    )
                    .into());
                };
                let head_thunk = ctx.get_thunk(head);
                let head_val = materialize_sync(&head_thunk, Some(&span), &ctx)?;
                let item_path = if path.is_empty() {
                    format!("[{}]", idx)
                } else {
                    format!("{}[{}]", path, idx)
                };
                let mut violations = validate_value(
                    items_schema.clone(),
                    head_val,
                    item_path,
                    Arc::clone(&ctx),
                    span.clone(),
                )
                .await?;

                let tail_thunk = ctx.get_thunk(tail);
                let tail_val = materialize_sync(&tail_thunk, Some(&span), &ctx)?;
                let tail_violations =
                    validate_seq_items(tail_val, items_schema, path, idx + 1, ctx, span).await?;
                violations.extend(tail_violations);
                Ok(violations)
            }
            _ => {
                // Malformed Seq or other value — stop walking
                Ok(Vec::new())
            }
        }
    })
}
