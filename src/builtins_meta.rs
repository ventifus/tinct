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
//! - Type predicates: all implemented in stdlib/prelude.llt via type pattern matching.
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
use std::sync::{Arc, Mutex, RwLock};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{
    builtin, ok_val, reject_named, require_string, synthetic_call_expr, MAX_COLLECT_SIZE,
};
use crate::env::Env;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, TypeContextData};
use crate::eval_call::{invoke_function, CallContext};
use crate::eval_materialize::make_span_dict;
use crate::rust_span;
use crate::value::ThunkId;
use crate::value::{string_val, BuiltinArgs, HashableValue, Strictness, Thunk, Value};

// ── Unified error dict helpers ────────────────────────────────────────────────

/// Build a unified error dict from a `ParseError` for return from `builtin-parse`.
///
/// Schema: `{kind, message, span, notes, call-stack, macro-expand, blame}`
/// `kind` is always `"parse-error"`. `span` uses the ParseError's optional span
/// (defaults to a zero-span if absent).
fn parse_error_to_dict(
    err: &crate::parser::ParseError,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: &crate::ast::Span,
) -> ThunkId {
    use crate::ast::{Position, Span};
    let alloc = |v: Value| ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())));

    let span_id = match &err.span {
        Some(s) => make_span_dict(s, ctx, call_span),
        None => {
            // No span available — produce a zero-position span dict.
            let zero_pos = Position {
                offset: 0,
                line: 0,
                column: 0,
            };
            let zero_span = Span {
                start: zero_pos,
                end: zero_pos,
                file: std::sync::Arc::new(crate::ast::SourceFile {
                    path: std::sync::Arc::from("<macro-raise>"),
                    content: std::sync::Arc::from(""),
                }),
            };
            make_span_dict(&zero_span, ctx, call_span)
        }
    };

    let mut w: IndexMap<HashableValue, ThunkId> = IndexMap::new();
    w.insert(
        HashableValue::Str("kind".into()),
        alloc(string_val("parse-error")),
    );
    w.insert(
        HashableValue::Str("message".into()),
        alloc(string_val(&err.message)),
    );
    w.insert(HashableValue::Str("span".into()), span_id);
    w.insert(
        HashableValue::Str("notes".into()),
        alloc(Value::Dict(IndexMap::new())),
    );
    w.insert(
        HashableValue::Str("call-stack".into()),
        alloc(Value::Dict(IndexMap::new())),
    );
    w.insert(
        HashableValue::Str("macro-expand".into()),
        alloc(Value::Dict(IndexMap::new())),
    );
    w.insert(
        HashableValue::Str("blame".into()),
        alloc(Value::Dict(IndexMap::new())),
    );
    alloc(Value::Dict(w))
}

// ─────────────────────────────────────────────────────────────────────────────

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
            ..
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
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "raise",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let msg = require_string("raise", val, ctx.get_thunk(args[0]).span.clone())?;
        Err(EvalError::user_error(msg.to_string(), call_span).into())
    })
}

/// `builtin-macro-error`: takes 1-2 args (message string, optional AST node).
/// Creates a macro error with a precise source span.
/// If the optional node is provided, uses its span; otherwise uses call_span.
/// Arg 1 (message) is Strictness::Seq (materialized).
/// Arg 2 (node) is Strictness::Id (not materialized - stays as thunk).
pub(crate) fn builtin_macro_error(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        // Reject named arguments
        crate::builtins::reject_named("builtin-macro-error", named.as_ref(), call_span.clone())?;

        // Expect 1 or 2 arguments
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(
                if args.len() == 0 { 1 } else { 2 },
                args.len(),
                call_span,
            )
            .into());
        }

        // Extract message (first argument) - materialized by Strictness::Seq
        let arg0_thunk = ctx.get_thunk(args[0]);
        let msg_val = arg0_thunk
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let message = require_string("builtin-macro-error", msg_val, arg0_thunk.span.clone())?;

        // Determine the span to use for the error
        let error_span = if args.len() == 2 {
            // Second argument provided - check if it's an Expression and use its span
            // Note: arg 2 has Strictness::Id, so it's not materialized - fall back to call_span.
            // Expr.* variants do not carry source spans in their payload, so we use call_span.
            call_span.clone()
        } else {
            // No second argument - use call_span
            call_span.clone()
        };

        // Create macro error with the determined span
        Err(EvalError::macro_error(message, error_span).into())
    })
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `{ok: value}` on success
/// or `{error: message}` on failure. Both branches are Dicts so callers can always
/// use `[builtin-has-key? "ok" raw]` to discriminate without a type check.
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
            ..
        } = ctx_arg;
        reject_named("try", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let func_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let call_result = match func_val {
            Value::Function {
                params,
                body,
                closure_env_id,
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
                // T-1558: Use closure_env_id as the call env id.
                // T-1559 will allocate a proper child FlatEnv scope with param slots.
                let body_thunk = Arc::new(Thunk::new_unevaluated_core(
                    Arc::clone(&body),
                    closure_env_id,
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
                    caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
                    caller_env_id: 0,
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
                // Success: {ok: value}. Caller uses [builtin-has-key? "ok" raw] to discriminate.
                // Both success and failure return Dicts so builtin-has-key? is always safe.
                let mut map = IndexMap::new();
                let val_tid = ctx.alloc_thunk(ok_val(val, call_span.clone())?);
                map.insert(HashableValue::Str("ok".into()), val_tid);
                ok_val(Value::Dict(map), call_span)
            }
            Err(e) => {
                // ResourceLimitExceeded is non-catchable: it indicates a system-level limit.
                use crate::error::ErrorKind;
                if let ErrorKind::ResourceLimitExceeded { .. } = &e.kind {
                    return Err(e);
                }
                // Failure: {error: message}
                let mut map = IndexMap::new();
                let err_tid =
                    ctx.alloc_thunk(ok_val(string_val(&e.to_string()), call_span.clone())?);
                map.insert(HashableValue::Str("error".into()), err_tid);
                ok_val(Value::Dict(map), call_span)
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
            caller_env,
            caller_env_id: _,
        } = ctx_arg;
        reject_named("until", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        let pred_thunk = ctx.get_thunk(args[0]);
        let f_thunk = ctx.get_thunk(args[1]);
        let mut val_thunk = ctx.get_thunk(args[2]);

        // Pre-materialize the predicate function to extract its return type annotation.
        // This lets us pre-resolve the Matchable binding name once before the loop,
        // avoiding repeated runtime type derivation on every iteration.
        let pred_fn_val = materialize(&pred_thunk, Some(&call_span), &ctx).await?;
        let pred_matchable_binding =
            crate::eval::resolve_matchable_binding_from_fn(&pred_fn_val);
        // Wrap the materialized predicate back into a thunk for use in pending calls.
        let pred_thunk = Arc::new(Thunk::new_materialized(pred_fn_val, call_span.clone()));

        loop {
            // Create a pending call to pred(val) and materialize it.
            // T-1558: alloc ThunkIds for func and args; use ctx.current_env_id as caller env.
            let pred_id = ctx.alloc_thunk(Arc::clone(&pred_thunk));
            let val_id = ctx.alloc_thunk(Arc::clone(&val_thunk));
            let pred_result = Arc::new(Thunk::new_pending_call(
                pred_id,
                vec![val_id],
                IndexMap::new(),
                call_span.clone(),
                ctx.current_env_id,
                val_thunk.span.clone(),
                Some(Arc::from("until")),
                Arc::clone(&ctx),
                synthetic_call_expr(call_span.clone()),
            ));

            let pred_val = materialize(&pred_result, Some(&call_span), &ctx).await?;

            // T-1558: call_to_match_opt_resolved ignores env (T-1559 will wire FlatEnv lookup).
            let dummy_env_for_match = std::sync::Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
            let _ = &caller_env; // caller_env no longer used for match dispatch
            if crate::eval::call_to_match_opt_resolved(
                &pred_val,
                pred_matchable_binding.as_deref(),
                &dummy_env_for_match,
                &ctx,
                &call_span,
            )
            .await
            {
                // Predicate holds, return the current value (as thunk)
                return Ok(val_thunk);
            } else {
                // Predicate doesn't hold yet, apply f and materialize to get next value.
                // T-1558: alloc ThunkIds for func and arg.
                let f_id = ctx.alloc_thunk(Arc::clone(&f_thunk));
                let val_id = ctx.alloc_thunk(Arc::clone(&val_thunk));
                let f_result = Arc::new(Thunk::new_pending_call(
                    f_id,
                    vec![val_id],
                    IndexMap::new(),
                    call_span.clone(),
                    ctx.current_env_id,
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
            ..
        } = ctx_arg;
        reject_named("apply", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        // Both args[0] and args[1] have been pre-materialized by force_count.
        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let func_val = arg0_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let args_val = arg1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let arg_dict = crate::builtins::require_dict(
            "apply",
            args_val,
            arg1_thunk.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        // Split dict entries: integer-keyed → positional, string-keyed → named
        let mut int_entries: Vec<(i64, ThunkId)> = Vec::with_capacity(arg_dict.len());
        let mut named_args: IndexMap<String, ThunkId> = IndexMap::with_capacity(arg_dict.len());
        for (key, &thunk_id) in &arg_dict {
            match key {
                HashableValue::Int(n) => int_entries.push((*n, thunk_id)),
                HashableValue::Str(s) => {
                    named_args.insert(s.to_string(), thunk_id);
                }
            }
        }
        int_entries.sort_by_key(|(k, _)| *k);
        let positional_ids: Vec<ThunkId> = int_entries.into_iter().map(|(_, id)| id).collect();

        match func_val {
            Value::Function {
                params,
                body,
                closure_env_id,
                ..
            } => {
                let named_ids: IndexMap<String, ThunkId> = named_args;
                invoke_function(&CallContext {
                    params: &params,
                    body: &body,
                    closure_env_id,
                    positional: &positional_ids,
                    named: if named_ids.is_empty() {
                        None
                    } else {
                        Some(&named_ids)
                    },
                    default_env_id: closure_env_id,
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
                let force_limit = def.force_count.min(positional_ids.len());
                for &arg_id in &positional_ids[..force_limit] {
                    let arg = ctx.get_thunk(arg_id);
                    if arg.try_get_materialized().is_none() {
                        materialize(&arg, Some(&call_span), &ctx).await?;
                    }
                }
                for (i, &s) in def.pos_strictness.iter().enumerate() {
                    if i < positional_ids.len()
                        && (s == Strictness::Seq || s == Strictness::Spine)
                    {
                        let arg = ctx.get_thunk(positional_ids[i]);
                        if arg.try_get_materialized().is_none() {
                            materialize(&arg, Some(&call_span), &ctx).await?;
                        }
                    }
                }
                let builtin_args = BuiltinArgs {
                    args: positional_ids,
                    named: if named_args.is_empty() {
                        None
                    } else {
                        Some(named_args)
                    },
                    call_span,
                    caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
                    caller_env_id: 0,
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
            ..
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
            builtin!("builtin-apply", builtin_apply_impl, [], 2),
            args,
            named_opt,
            call_span,
            Some(Arc::from("apply")),
            ctx.current_env_id, // T-1558: caller_env_id
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
            ctx,
            ..
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

        let get_str = |thunk: Arc<Thunk>, name: &str, span: &Span| -> EvalResult<String> {
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

        let scope_str = get_str(ctx.get_thunk(args[0]), "scope", &call_span)?;
        let prefix = get_str(ctx.get_thunk(args[1]), "prefix", &call_span)?;

        let scope = scope_str.chars().next().unwrap_or('ℊ');
        let name = gensym_fresh(scope, &prefix);
        ok_val(string_val(&name), call_span)
    })
}

/// `macro-injects`: Returns the names deliberately injected into the caller's scope by a macro.
///
/// Takes one argument (macro name as String). Returns a `Seq` of the injected binding names.
/// Returns an empty Dict if the macro has no `inject:` declaration.
///
/// This is a reflection primitive for anaphoric macros per doc/feature/macros.md §Hygiene.
/// Enables runtime introspection of macro inject names for documentation tools and callers
/// that need to be aware of introduced bindings.
///
/// Example:
///   [macro-injects "aif"]   # → Seq("it")  (if aif has `inject: it`)
///   [macro-injects "swap"]  # → {}          (if swap uses only gensym hygiene)
///   [macro-injects "foo"]   # → Seq("x" "y")  (if foo has `inject: [x y]`)
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
            ..
        } = ctx_arg;

        let macro_name_val = crate::builtins::expect_one_arg(
            "macro-injects",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let macro_name = require_string("macro-injects", macro_name_val, ctx.get_thunk(args[0]).span.clone())?;

        // Look up the macro in the inject map
        let inject_names: &[String] = match ctx.config.macro_injects_map.get(&macro_name) {
            Some(names) => names.as_slice(),
            None => &[],
        };

        // Build an integer-keyed Dict of inject names
        let mut dict = IndexMap::new();
        for (i, name) in inject_names.iter().enumerate() {
            let id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(name),
                call_span.clone(),
            )));
            dict.insert(HashableValue::Int(i as i64), id);
        }
        ok_val(Value::Dict(dict), call_span)
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
            ..
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
            ..
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
            ..
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

/// `builtin-ast-of` (`ast-of` in prelude): returns the AST of an expression as an `Expr.*` variant.
///
/// This builtin does NOT materialize its argument, making it safe to use
/// for introspection of unevaluated expressions.
///
/// - Unevaluated thunks (Surface/AstNodeField) → `Value::Variant { tag: "Expr.<Tag>", .. }`
/// - Materialized literals (Int/Float/Bool/String) → `Expr.Literal` variant
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
            ..
        } = ctx_arg;

        // Reject named args and ensure exactly 1 arg
        crate::builtins::reject_named("builtin-ast-of", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span.clone()).into());
        }

        let thunk = ctx.get_thunk(args[0]);

        // Inspect the thunk state WITHOUT forcing it using ThunkInner API

        // Check for PendingBuiltin
        if let Some(def) = thunk.peek_builtin_def() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("pending-builtin"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::HashableValue::Str("name".into()),
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
                crate::value::HashableValue::Str("type".into()),
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
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
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
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
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
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("thunk"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                ctx.alloc_thunk(Arc::new(crate::value::Thunk::new_materialized(
                    string_val("failed"),
                    call_span.clone(),
                ))),
            );
            entries.insert(
                crate::value::HashableValue::Str("error".into()),
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

        // Check for Surface (runtime-v2: return Expr.* variant)
        if let Some(node) = thunk.peek_surface_node() {
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::surface_convert::surface_node_to_expr_variant(&node, &ctx),
                call_span,
            )));
        }

        // Check for AstNodeField (runtime-v2: return the containing SurfaceNode as Expr.* variant)
        if let Some((node, _field)) = thunk.peek_ast_node_field() {
            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::surface_convert::surface_node_to_expr_variant(&node, &ctx),
                call_span,
            )));
        }

        // Force the thunk if it is in CoreExpr unevaluated state (the only remaining state after
        // the peek checks above).  This is necessary for Value::Function, which only exists as a
        // materialized value — there is no way to reconstruct function metadata from the CoreExpr
        // body without evaluating it.  For already-materialized thunks this is a no-op (cached).
        materialize(&thunk, Some(&call_span), &ctx).await?;

        // Check for Materialized — construct synthetic SurfaceNode for simple literals
        // For complex values, ast-of should be called on unevaluated expressions, but we
        // provide a fallback for materialized literals to avoid breaking existing code.
        if let Some(val) = thunk.try_get_materialized() {
            use crate::ast::{SurfaceExpression, SurfaceNode};
            let make_node =
                |expr: SurfaceExpression| Arc::new(SurfaceNode::new(expr, call_span.clone()));

            // Handle Value::Function — build a metadata dict: {type: "fn", doc: ..., return-ann: ..., params: [...]}
            if let crate::value::Value::Function {
                params, annotation, ..
            } = &val
            {
                let mut dict = IndexMap::new();

                // type: "fn"
                dict.insert(
                    HashableValue::Str("type".into()),
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
                    HashableValue::Str("doc".into()),
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
                dict.insert(HashableValue::Str("return-ann".into()), return_ann_tid);

                // params: integer-keyed dict of param entry dicts [{name: "x", annotation: ...}, ...]
                let param_tids: Vec<ThunkId> = params
                    .iter()
                    .map(|p| {
                        let mut param_dict = IndexMap::new();
                        param_dict.insert(
                            HashableValue::Str("name".into()),
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
                            param_dict.insert(HashableValue::Str("annotation".into()), ann_tid);
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
                dict.insert(HashableValue::Str("params".into()), params_tid);

                return Ok(Arc::new(crate::value::Thunk::new_materialized(
                    Value::Dict(dict),
                    call_span,
                )));
            }

            let synthetic_node: Arc<SurfaceNode> = match val {
                crate::value::Value::Int(n) => make_node(SurfaceExpression::Int(n)),
                crate::value::Value::U64(n) => make_node(SurfaceExpression::U64(n)),
                crate::value::Value::Float(f) => make_node(SurfaceExpression::Float(f)),
                crate::value::Value::String { source, start, end } => {
                    make_node(SurfaceExpression::Str(source[start..end].to_string()))
                }
                // For other materialized values (Builtin, Dict, etc.), we cannot
                // reconstruct the original AST, so error out.
                _ => {
                    return Err(EvalError::type_mismatch(
                        "Expr.*",
                        val.type_name(),
                        call_span.clone(),
                    )
                    .into());
                }
            };

            return Ok(Arc::new(crate::value::Thunk::new_materialized(
                crate::surface_convert::surface_node_to_expr_variant(&synthetic_node, &ctx),
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
            ..
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
            .await
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
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "tag-of",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        // Peel Value::Annotated wrappers before matching.
        // Unit constructors declared with @[...] annotations evaluate to
        // Value::Annotated { inner: Variant(...), annotation: {...} }.
        // Annotations are metadata-only — tag-of sees only the inner value.
        // Consistent with Pattern::Constructor (eval.rs:3411-3421),
        // primitive_eq (eval.rs), and visit_value (lib.rs:588-591).
        let peeled: &Value = {
            let mut v = &val;
            while let Value::Annotated { inner, .. } = v {
                v = inner.as_ref();
            }
            v
        };
        match peeled {
            // Expr.* variants return their full qualified tag (e.g. "Expr.Call", "Expr.VarRef").
            Value::Variant { tag, .. } => ok_val(string_val(tag), call_span),
            _ => Err(Box::new(EvalError::type_mismatch(
                "Variant",
                peeled.type_name(),
                call_span,
            ))),
        }
    })
}

/// `span-of`: extract source span from an AST node value as a dict.
///
/// Returns a dict with the structure:
/// ```
/// {
///   file: String,       // file path or "" if unavailable
///   start-line: Int,    // starting line number (1-based)
///   start-col: Int,     // starting column number (1-based)
///   end-line: Int,      // ending line number (1-based)
///   end-col: Int        // ending column number (1-based)
/// }
/// ```
///
/// `Expr.*` variants do not carry source spans in their payload — spans are not preserved
/// when converting `SurfaceNode` to `Expr.*` variants. Returns empty dict `[]` for all inputs.
///
/// Used for precise error reporting in macros.
pub(crate) fn builtin_span_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "span-of",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;

        // Expr.* variants do not carry source spans in their payload.
        // Return empty dict for all inputs.
        let _ = val;
        ok_val(Value::Dict(IndexMap::new()), call_span)
    })
}

/// `builtin-var-resolution`: given a byte offset and a resolved Program, return the
/// de Bruijn coordinates `{level: N, slot: M}` for the VarRef whose span contains
/// that offset, or `[]` if no VarRef is found there.
///
/// Used to inspect resolver output from tinct code.
pub(crate) fn builtin_var_resolution(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-var-resolution", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let offset_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let offset = match offset_val {
            Value::Int(n) => n as usize,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-var-resolution".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let prog_val = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;
        let program = match prog_val {
            Value::Program { program, .. } => program,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-var-resolution".to_string(),
                    "Program",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Walk all VarRef nodes in the program looking for one whose span contains `offset`.
        fn find_in_node(
            node: &std::sync::Arc<crate::ast::SurfaceNode>,
            offset: usize,
        ) -> Option<Option<(u32, u32)>> {
            if node.span.end.offset < offset || node.span.start.offset > offset {
                return None;
            }
            use crate::ast::SurfaceExpression;
            match &node.expr {
                SurfaceExpression::VarRef { resolution, .. } => {
                    return Some(resolution.get().flatten());
                }
                SurfaceExpression::Call {
                    func,
                    args,
                    named_args,
                    ..
                } => {
                    if let Some(r) = find_in_node(func, offset) {
                        return Some(r);
                    }
                    for a in args {
                        if let Some(r) = find_in_node(a, offset) {
                            return Some(r);
                        }
                    }
                    for na in named_args {
                        if let Some(r) = find_in_node(&na.node.value, offset) {
                            return Some(r);
                        }
                    }
                }
                SurfaceExpression::Dict(entries) => {
                    for e in entries {
                        if let Some(k) = &e.node.key {
                            if let Some(r) = find_in_node(k, offset) {
                                return Some(r);
                            }
                        }
                        if let Some(r) = find_in_node(&e.node.value, offset) {
                            return Some(r);
                        }
                    }
                }
                SurfaceExpression::Fn { body, .. } => {
                    if let Some(r) = find_in_node(body, offset) {
                        return Some(r);
                    }
                }
                SurfaceExpression::Field { expr: Some(e), .. } => {
                    if let Some(r) = find_in_node(e, offset) {
                        return Some(r);
                    }
                }
                SurfaceExpression::Match { scrutinee, arms } => {
                    if let Some(r) = find_in_node(scrutinee, offset) {
                        return Some(r);
                    }
                    for arm in arms {
                        if let Some(r) = find_in_node(&arm.body, offset) {
                            return Some(r);
                        }
                    }
                }
                _ => {}
            }
            None
        }

        let mut found: Option<(u32, u32)> = None;
        'outer: for doc_spanned in &program.documents {
            for item in &doc_spanned.node.items {
                if let crate::ast::SurfaceItem::Expr(node) = item {
                    if let Some(coords) = find_in_node(node, offset) {
                        found = coords;
                        break 'outer;
                    }
                }
            }
        }

        let alloc =
            |v: Value| ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())));
        match found {
            None => ok_val(Value::Dict(IndexMap::new()), call_span),
            Some((level, slot)) => {
                let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                result.insert(
                    HashableValue::Str("level".into()),
                    alloc(Value::Int(level as i64)),
                );
                result.insert(
                    HashableValue::Str("slot".into()),
                    alloc(Value::Int(slot as i64)),
                );
                ok_val(Value::Dict(result), call_span)
            }
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
            ..
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
                // `extra` holds all evaluable annotation fields including `doc`.
                // `return_ann` is exposed as a string representation of the return type.
                let mut entries: IndexMap<HashableValue, ThunkId> = IndexMap::new();

                if let Some(ann) = annotation.as_deref() {
                    // Include `doc` field from FnAnnotation.doc (derived from extra["doc"] at
                    // function definition time). This is a fallback for any code path where
                    // doc is not yet in extra; extra["doc"] below will overwrite if present.
                    if let Some(ref doc_str) = ann.doc {
                        entries.insert(
                            HashableValue::Str("doc".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(doc_str),
                                call_span.clone(),
                            ))),
                        );
                    }
                    // Expose the return annotation as a string representation of the return type.
                    // `return` is excluded from `extra` (type expressions cannot be safely
                    // evaluated at definition time), so it is derived from `return_ann` here.
                    let return_str: Option<String> = match &ann.return_ann {
                        Some(crate::ast::Annotation::Simple(name)) => Some(name.clone()),
                        Some(ann_node) => ann_node.get_property("return").map(|n| n.to_string()),
                        None => None,
                    };
                    if let Some(s) = return_str {
                        entries.insert(
                            HashableValue::Str("return".into()),
                            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                                string_val(&s),
                                call_span.clone(),
                            ))),
                        );
                    }
                    // Flatten all extra fields into the dict (includes evaluated `doc`).
                    // Extra fields overwrite doc inserted above, ensuring the evaluated
                    // version wins for triple-quoted doc strings.
                    for (key, extra_val) in &ann.extra {
                        entries.insert(
                            HashableValue::Str(key.as_str().into()),
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
/// Used by the lower.rs constructor dict (T-1193) to wrap unit constructor values in
/// `Value::Annotated` when the constructor carries a `@[...]` annotation (T-1121).
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
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("make-annotated", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let inner_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let ann_val = ctx.get_thunk(args[1])
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

// Type predicates are implemented in stdlib/prelude.llt via type pattern matching.

/// Helper for runtime type name extraction.
fn type_name(val: &Value) -> String {
    match val {
        Value::Int(_) => "Int",
        Value::Float(_) => "Float",
        Value::String { .. } => "String",
        Value::Bytes { .. } => "Bytes",
        Value::Dict(_) | Value::Overlay(..) => "Dict",
        Value::Function { .. } => "Function",
        Value::Builtin(_) => "Builtin",
        Value::Proxy { .. } => "Proxy",
        Value::DirCap { .. } | Value::RevocableDirCap { .. } => "DirCap",
        Value::NetCap(_) => "NetCap",
        Value::File(_) => "File",
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
        Value::TypeContext(_) => "TypeContext",
        Value::Environment(_) => "Environment",
        Value::Bool(_) => "Bool",
        Value::Handle { .. } => "Handle",
        Value::WriteHandle { .. } => "WriteHandle",
        Value::Seq { .. } => "Seq",
        Value::Expression(_) => "Expression",
        Value::Arena { .. } => "Arena",
    }
    .to_string()
}

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
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "blake3",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let s = require_string("blake3", val, ctx.get_thunk(args[0]).span.clone())?;
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
            ..
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
                    ctx.get_thunk(args[0]).span.clone(),
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
            ..
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
            let name_hint = if let Some(&name_thunk_id) = named_map.get("name") {
                let name_thunk = ctx.get_thunk(name_thunk_id);
                let name_val = materialize(&name_thunk, Some(&call_span), &ctx).await?; // H2: conditional (only when name: named arg present)
                let name_str = require_string("load", name_val, name_thunk.span.clone())?;
                Some(name_str)
            } else {
                None
            };
            let expected_hash = if let Some(&hash_thunk_id) = named_map.get("hash") {
                let hash_thunk = ctx.get_thunk(hash_thunk_id);
                let hash_val = materialize(&hash_thunk, Some(&call_span), &ctx).await?; // H2: conditional (only when hash: named arg present)
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
        let arg0_thunk = ctx.get_thunk(args[0]);
        let source_val = arg0_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let source = require_string("load", source_val, arg0_thunk.span.clone())?;

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
        // Formatters need the raw AST. Callers that need resolution
        // should call `[builtin-resolve [load ...]]` explicitly.
        let program = parsed.program;
        let program_value = Value::Program {
            program: std::sync::Arc::new(program),
            resolutions: std::sync::Arc::new(Default::default()),
            types: std::sync::Arc::new(Default::default()),
            expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
        };
        let thunk = Arc::new(Thunk::new_materialized(program_value, call_span));
        Ok(thunk)
    })
}

/// `builtin-parse`: Parse Bytes + path String → `{program, errors}`.
///
/// Signature: `[builtin-parse bytes path]`
/// - `bytes` (Bytes): the source file contents as raw bytes
/// - `path` (String): file path hint used in parse error messages
///
/// Returns `{program: Value::Program, errors: Dict<Int, ErrorDict>}`.
///   - `program`: raw `Value::Program` with empty resolution/type/expects_resolved tables.
///   - `errors`: integer-keyed dict of unified error dicts `{kind, message, span, notes, ...}`.
///     `kind` is always `"parse-error"` for parse errors.
///     Empty dict (`[]`) when parse succeeded with no errors.
///
/// Callers should check `errors` before proceeding. The `program` is always present
/// (may be an empty program on fatal parse failure). Callers must call `builtin-resolve`
/// (desugar + resolve) and then `builtin-typecheck` before passing the result to `builtin-eval`.
///
/// This is Stage 1 of the 4-stage pipeline: parse → resolve → typecheck → eval.
pub(crate) fn builtin_parse(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-parse", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // First arg: Bytes (the source file contents)
        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let bytes_val = arg0_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let source_bytes = match bytes_val {
            Value::Bytes { source, start, end } => source[start..end].to_vec(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-parse".to_string(),
                    "Bytes",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Second arg: String (the path hint for error messages)
        let path_val = arg1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let path_str = require_string("builtin-parse", path_val, arg1_thunk.span.clone())?;

        // Decode bytes as UTF-8 source text
        let source = std::str::from_utf8(&source_bytes)
            .map_err(|e| {
                EvalError::user_error(
                    format!(
                        "builtin-parse: UTF-8 decode error in \"{}\": {}",
                        path_str, e
                    ),
                    call_span.clone(),
                )
            })?
            .to_string();

        // Parse — use parse_with_file so spans carry the path for error messages.
        // Fatal parse errors (lexer failure, unclosed brackets) are captured in the
        // errors list rather than raised, so callers can inspect them programmatically.
        let source_file = Arc::new(crate::ast::SourceFile {
            path: Arc::from(path_str.as_str()),
            content: Arc::from(source.as_str()),
        });
        let (parsed, fatal_errors) =
            match crate::parser::parse_with_file(&source, Arc::clone(&source_file)) {
                Ok(output) => (Some(output), vec![]),
                Err(fatal) => (None, vec![fatal]),
            };

        // Build errors list: fatal error (if any) + recovered errors from ParseOutput.
        let all_parse_errors: Vec<crate::parser::ParseError> = if let Some(ref output) = parsed {
            output.errors.clone()
        } else {
            fatal_errors
        };

        // Build the integer-keyed errors Dict.
        let alloc =
            |v: Value| ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())));
        let mut errors_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, pe) in all_parse_errors.iter().enumerate() {
            let err_id = parse_error_to_dict(pe, &ctx, &call_span);
            errors_dict.insert(HashableValue::Int(i as i64), err_id);
        }

        // Build the program value. If the parse was fatal, produce an empty program.
        let program_value = if let Some(output) = parsed {
            Value::Program {
                program: std::sync::Arc::new(output.program),
                resolutions: std::sync::Arc::new(Default::default()),
                types: std::sync::Arc::new(Default::default()),
                expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
            }
        } else {
            // Fatal parse: return empty program so {program, errors} is always usable.
            Value::Program {
                program: std::sync::Arc::new(crate::ast::SurfaceProgram { documents: vec![] }),
                resolutions: std::sync::Arc::new(Default::default()),
                types: std::sync::Arc::new(Default::default()),
                expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
            }
        };

        // Return {program: Value::Program, errors: integer-keyed Dict of error dicts}.
        let program_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
            program_value,
            call_span.clone(),
        )));
        let errors_id = alloc(Value::Dict(errors_dict));
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        result.insert(HashableValue::Str("program".into()), program_id);
        result.insert(HashableValue::Str("errors".into()), errors_id);
        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(result),
            call_span,
        )))
    })
}

/// `builtin-resolve`: Desugar and resolve a raw `Value::Program`.
///
/// Takes a parsed (but unresolved) Program and applies:
/// 1. `desugar_surface_program` — `$_` desugaring
/// 2. `resolve_surface_program` (with optional `env:` argument for env-seeded resolution)
///    — name resolution (De Bruijn levels)
///
/// **Optional `env:` argument**: When provided (a `Value::Env`), the resolver is
/// pre-seeded from the env chain so that prelude/stdlib names resolve to proper de Bruijn
/// coordinates instead of producing resolution errors. This is the primary path for user code.
/// When absent, bootstrap mode is used (empty scope stack) — suitable for loader.llt and tests.
///
/// Does NOT run the type checker. Call `builtin-typecheck` afterwards.
///
/// This is Stage 2 of the 4-stage pipeline (Stage 1 is parse from loader.llt).
pub(crate) fn builtin_resolve(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        // Extract optional env: named argument (Value::Env).
        // Reject any other named arguments.
        let opt_env = if let Some(ref named_map) = named {
            let unknown: Vec<&str> = named_map
                .keys()
                .filter(|k| k.as_str() != "env")
                .map(|k| k.as_str())
                .collect();
            if !unknown.is_empty() {
                return Err(EvalError::user_error(
                    format!(
                        "builtin-resolve: unknown named arguments: {}",
                        unknown.join(", ")
                    ),
                    call_span,
                )
                .into());
            }
            named_map.get("env").copied()
        } else {
            None
        };
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        // Materialize the env: argument if present
        let env_arc = if let Some(env_thunk_id) = opt_env {
            let env_thunk = ctx.get_thunk(env_thunk_id);
            let env_val = crate::eval::materialize(&env_thunk, Some(&call_span), &ctx).await?;
            match env_val {
                Value::Environment(arc) => Some(arc),
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "builtin-resolve env:".to_string(),
                        "Environment",
                        other.type_name(),
                        call_span,
                    )
                    .into());
                }
            }
        } else {
            None
        };

        let val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            // Program path removed — builtin-resolve only accepts Value::Document.
            // Tinct code should resolve per-document (the narrow builtin principle):
            // the loader orchestrates, the builtin does one thing.
            // Callers that previously passed Value::Program should loop over
            // program.documents and call [builtin-resolve doc env: env] for each.
            Value::Program { .. } => {
                return Err(EvalError::user_error(
                    "builtin-resolve: Value::Program is no longer accepted — \
                     resolve per-document instead: \
                     [reduce [fn [let acc doc] [builtin-resolve doc env: e]] [] docs]"
                        .to_string(),
                    call_span,
                )
                .into());
            }
            // Build errors dict from resolve errors (undefined variables).
            // Per-document resolution path: `[builtin-resolve doc env: E]`
            //
            // Resolves a single document with proper scope accumulation for intermediate
            // dict expressions. Returns a dict:
            //   {doc: Value::Document, errors: Dict<Int, ErrorDict>}
            //
            // Resolution is written inline to the AST nodes in `doc_arc`. After this call,
            // lowering reads inline resolution directly from each VarRef/DotAccess node.
            // No table: is needed in builtin-eval — the inline resolution is already on the nodes.
            // The `errors:` dict contains resolve errors (undefined variables).
            Value::Document(doc_arc) => {
                let env = match env_arc {
                    Some(ref e) => e,
                    None => return Err(EvalError::user_error(
                        "builtin-resolve: env: argument is required when first arg is a Document"
                            .to_string(),
                        call_span,
                    )
                    .into()),
                };

                let alloc = |v: Value| {
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(v, call_span.clone())))
                };

                // Resolve the document in-place: writes de Bruijn coords directly to the
                // inline Resolution OnceLocks on the original Arc<SurfaceDocument>'s nodes.
                // We do NOT clone — cloning would write to a copy's OnceLocks, not the
                // original, so builtin-eval would see empty OnceLocks and lower everything
                // to CoreExpr::Placeholder (with a LowerDiagnostic for each unresolved name).
                //
                // The env chain is walked to populate resolver scopes from outermost to innermost.
                // Names not found in the env chain have their OnceLock left unset (None) and are
                // returned in resolve_errors. The resolver MUST be seeded from the same env the
                // document will be evaluated with — a mismatch causes wrong de Bruijn levels.
                let (_resolve_table, resolve_errors) =
                    crate::resolve::resolve_surface_document_inplace(&doc_arc, env);

                // Build errors dict from resolve_errors (undefined variables).
                let mut errors_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                for (i, (name, span)) in resolve_errors.iter().enumerate() {
                    let span_id = make_span_dict(span, &ctx, &call_span);
                    let mut w: IndexMap<HashableValue, ThunkId> = IndexMap::new();
                    w.insert(
                        HashableValue::Str("kind".into()),
                        alloc(string_val("resolve-error")),
                    );
                    w.insert(
                        HashableValue::Str("message".into()),
                        alloc(string_val(&format!("undefined variable: {}", name))),
                    );
                    w.insert(HashableValue::Str("span".into()), span_id);
                    w.insert(
                        HashableValue::Str("notes".into()),
                        alloc(Value::Dict(IndexMap::new())),
                    );
                    w.insert(
                        HashableValue::Str("call-stack".into()),
                        alloc(Value::Dict(IndexMap::new())),
                    );
                    w.insert(
                        HashableValue::Str("macro-expand".into()),
                        alloc(Value::Dict(IndexMap::new())),
                    );
                    w.insert(
                        HashableValue::Str("blame".into()),
                        alloc(Value::Dict(IndexMap::new())),
                    );
                    errors_dict.insert(HashableValue::Int(i as i64), alloc(Value::Dict(w)));
                }

                // Return {doc: Value::Document, errors: Dict}
                // Resolution is now inline on the document's AST nodes.
                let doc_thunk_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Document(std::sync::Arc::clone(&doc_arc)),
                    call_span.clone(),
                )));
                let errors_thunk_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Dict(errors_dict),
                    call_span.clone(),
                )));
                let mut result_dict = indexmap::IndexMap::new();
                result_dict.insert(HashableValue::Str("doc".into()), doc_thunk_id);
                result_dict.insert(HashableValue::Str("errors".into()), errors_thunk_id);
                ok_val(Value::Dict(result_dict), call_span)
            }

            _ => Err(EvalError::type_mismatch_ctx(
                "builtin-resolve".to_string(),
                "Program or Document",
                val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-typecheck`: Type-check a resolved `Value::Program`, returning a typed Program.
///
/// Takes a resolved Program (output of `builtin-resolve` or `expand`) and runs the type
/// checker against it. Type errors are advisory — eval proceeds regardless. The returned
/// Program has populated inline type annotations and `expects_resolved` map for use by `builtin-eval`.
///
/// Seeds the type checker from `inference_env` on the current EvalContext's TypeContext.
/// After type-checking, writes the resulting `final_env` back to `inference_env` so that
/// subsequent calls (e.g. user code after prelude) see all previously declared types.
///
/// The second argument is the TypeContext handle (`Value::TypeContext`). The TypeContext is
/// accepted and validated.
///
/// TypeContext arg (arg 2) must be a Value::TypeContext handle from builtin-get-type-context.
///
/// Signature: `[builtin-typecheck resolved-program]` or
///             `[builtin-typecheck resolved-program type-ctx]`
pub(crate) fn builtin_typecheck(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        // Extract optional env: named argument. When provided, used as the resolver seed env
        // so that instance binding names (ɪ-prefixed) visible in the runtime env are also
        // visible to the type checker's name resolution pass. This fixes false-positive
        // "undefined variable" warnings for class method VarRefs (cast, +, -, etc.) when
        // type-checking the prelude.
        // Reject any unknown named arguments.
        let opt_env_thunk = if let Some(ref named_map) = named {
            let unknown: Vec<&str> = named_map
                .keys()
                .filter(|k| k.as_str() != "env")
                .map(|k| k.as_str())
                .collect();
            if !unknown.is_empty() {
                return Err(EvalError::user_error(
                    format!(
                        "builtin-typecheck: unknown named arguments: {}",
                        unknown.join(", ")
                    ),
                    call_span.clone(),
                )
                .into());
            }
            named_map.get("env").copied()
        } else {
            None
        };
        // Materialize and validate the env: argument if provided.
        // Extract the Arc<RwLock<Env>> for use as resolver seed in typecheck_surface_program_with_env.
        let resolver_seed_env: Option<std::sync::Arc<std::sync::RwLock<crate::env::Env>>> =
            if let Some(env_thunk_id) = opt_env_thunk {
                let env_thunk = ctx.get_thunk(env_thunk_id);
                let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
                match env_val {
                    Value::Environment(arc) => Some(arc),
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "builtin-typecheck env:".to_string(),
                            "Environment",
                            other.type_name(),
                            call_span,
                        )
                        .into());
                    }
                }
            } else {
                None
            };
        // Accept 1 or 2 args: program, [type-ctx]
        if args.is_empty() || args.len() > 2 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        // Extract and validate the TypeContext argument (arg 2).
        // The passed TypeContext is the one we read from AND write to — tinct code
        // is in charge of propagating it between calls (e.g. by passing the same tc
        // to each builtin-typecheck in a uses-scope reduce loop). This allows TyConDefs
        // (DirCap, File, etc.) from builtin_core.llt to accumulate in the TypeContext
        // and be visible when builtin_io.llt is subsequently type-checked.
        // A TypeContext must always be passed explicitly — callers are responsible for
        // threading the TypeContext so TyConDefs accumulate across module type-checks.
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let tc_arc: Arc<Mutex<crate::eval::TypeContextData>> = {
            let tc_thunk = ctx.get_thunk(args[1]);
            let tc_val = materialize(&tc_thunk, Some(&call_span), &ctx).await?;
            match tc_val {
                Value::TypeContext(arc) => arc,
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "builtin-typecheck".to_string(),
                        "TypeContext",
                        other.type_name(),
                        call_span,
                    )
                    .into());
                }
            }
        };
        let val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match val {
            Value::Program {
                program: surface_program,
                ..
            } => {
                // Seed from the passed TypeContext (tc_arc). The tinct caller is responsible
                // for passing the same TypeContext across calls so TyConDefs (DirCap, File, etc.)
                // declared in builtin_core.llt accumulate and are visible to builtin_io.llt etc.
                let (parent_env, type_stage_env, seed_tycon_env): (
                    Arc<std::sync::RwLock<crate::env::Env>>,
                    Option<Arc<std::sync::RwLock<crate::env::Env>>>,
                    std::collections::HashMap<String, std::sync::Arc<crate::type_def::TyConDef>>,
                ) = {
                    let guard = tc_arc.lock().unwrap();
                    (
                        Arc::clone(&guard.inference_env),
                        Some(Arc::clone(&guard.type_stage_env)),
                        guard.tycon_env.clone(),
                    )
                };

                // Run the full typecheck pass seeded from the accumulated env.
                // Pass resolver_seed_env (from the env: named argument, if any) so that
                // instance binding names (ɪ-prefixed) visible in the runtime eval env are
                // also visible to the type checker's name resolution pass. Without this,
                // method VarRefs (cast, +, -, etc.) produce false-positive "undefined variable"
                // warnings because the type-only parent_env lacks ɪ-prefixed instance bindings.
                // Pass type_stage_env from the TypeContext so that eval_type_stage_expr can
                // evaluate user-defined TypeNode functions and type-stage combinators.
                // Pass seed_tycon_env so that TyConDefs (DirCap, File, ClockCap, etc.) from
                // builtin_core.llt are available when type-checking builtin_io.llt etc.
                let (
                    errors,
                    _type_map,
                    _doc_map,
                    _scheme_map,
                    _diagnostics,
                    state,
                    final_env,
                    _annotation_table,
                ) = crate::typecheck::typecheck_surface_program_with_env(
                    &surface_program,
                    parent_env,
                    false, // enable_scheme_map
                    resolver_seed_env,
                    type_stage_env,
                    seed_tycon_env,
                    Some(Arc::clone(&ctx)), // pass EvalContext — capabilities come from tinct
                )
                .await;

                // Write the final_env and new TyConDefs back into tc_arc (the passed TypeContext).
                // Since tinct code passes the same tc_arc to each builtin-typecheck call,
                // this accumulation is visible to subsequent calls — tinct is in control.
                // TyConDefs use or_insert: core types declared first are never overwritten.
                {
                    let mut guard = tc_arc.lock().unwrap();
                    guard.inference_env = Arc::clone(&final_env);
                    for (name, def) in &state.tycon_env {
                        guard.tycon_env.entry(name.clone()).or_insert_with(|| Arc::clone(def));
                    }
                }
                let _ = errors; // type errors are no longer stored in Value::Program

                ok_val(
                    Value::Program {
                        program: surface_program,
                        resolutions: std::sync::Arc::new(Default::default()),
                        types: std::sync::Arc::new(Default::default()),
                        expects_resolved: std::sync::Arc::new(state.expects_resolved),
                    },
                    call_span,
                )
            }
            _ => Err(EvalError::type_mismatch_ctx(
                "builtin-typecheck".to_string(),
                "Program",
                val.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-get-type-context`: Return a clone of the EvalContext's TypeContext.
///
/// Returns a clone of the TypeContextData initialized by the Rust bootstrap (main.rs /
/// lsp/document.rs). Errors if the TypeContext was not initialized before this call.
///
/// Note: this builtin is vestigial. Tinct code should use the TypeContext created by
/// `[builtin-make-type-ctx]` and threaded explicitly through `uses-scope` / `fundamental-tc`.
///
/// Signature: `[builtin-get-type-context]`
pub(crate) fn builtin_get_type_context(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named(
            "builtin-get-type-context",
            named.as_ref(),
            call_span.clone(),
        )?;
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
        }
        // Return the real TypeContext handle from EvalContext.
        // Errors if builtin-make-type-ctx has not yet been called to initialize it.
        let tc_guard = ctx.type_context.lock().unwrap();
        match tc_guard.as_ref() {
            Some(tc) => {
                // Clone the TypeContextData and wrap it in a new Arc<Mutex<>> for the Value.
                // The Value owns a separate handle — to share state the caller must use the
                // same TypeContext they obtained from make-type-ctx.
                let tc_clone = tc.clone();
                drop(tc_guard);
                ok_val(
                    Value::TypeContext(Arc::new(Mutex::new(tc_clone))),
                    call_span,
                )
            }
            None => {
                drop(tc_guard);
                // TypeContext must be initialized by the caller (main.rs / lsp/document.rs)
                // before any tinct code runs. If it's missing here, something is wrong.
                Err(EvalError::internal(
                    "builtin-get-type-context: TypeContext not initialized — \
                     this is a Rust bootstrap error, not a tinct error"
                        .to_string(),
                    call_span,
                )
                .into())
            }
        }
    })
}

/// `builtin-make-type-ctx`: Create a fresh TypeContext seeded with core type definitions.
///
/// Creates a `TypeContextData` with an empty `type_stage_env` and installs it on
/// the current `EvalContext`. Returns a `Value::TypeContext` handle wrapping the same
/// `Arc<Mutex<TypeContextData>>` stored on `EvalContext` — so both the returned handle
/// and the EvalContext share the same mutable state.
///
/// Signature: `[builtin-make-type-ctx]`
pub(crate) fn builtin_make_type_ctx(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-make-type-ctx", named.as_ref(), call_span.clone())?;
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
        }
        // Fresh TypeContext seeded with the builtin_core type env.
        // Use this for isolated type-checking that shouldn't inherit accumulated state.
        // For loader's fundamental-tc, use [builtin-get-type-context] instead —
        // it returns the TypeContext that main.rs pre-populated from builtin_core.llt.
        let tc = TypeContextData {
            type_stage_env: Arc::new(RwLock::new(Env::new())),
            inference_env: crate::imports::get_builtin_core_type_env()
                .await
                .expect("builtin_core type env unavailable"),
            tycon_env: std::collections::HashMap::new(),
        };
        // Install it on EvalContext (no-op if already initialized).
        ctx.init_type_context(tc.clone());
        // Wrap in Arc<Mutex<>> and return as a Value::TypeContext handle.
        ok_val(Value::TypeContext(Arc::new(Mutex::new(tc))), call_span)
    })
}

/// `builtin-fork-type-ctx`: Create a child TypeContext inheriting from a parent.
///
/// Accepts a `Value::TypeContext` parent handle. Clones the parent's `TypeContextData`
/// into a new independent `Arc<Mutex<TypeContextData>>` — mutations to the child
/// (future type declarations) do not propagate upward to the parent.
///
/// Signature: `[builtin-fork-type-ctx parent-ctx]`
pub(crate) fn builtin_fork_type_ctx(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-fork-type-ctx", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let parent_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        match parent_val {
            Value::TypeContext(parent_arc) => {
                // Clone the parent's TypeContextData into an independent child.
                // The child gets a fresh Arc<Mutex<>> — parent and child are independent
                // after this point. Mutations to child do not propagate to parent.
                let child_data = parent_arc.lock().unwrap().clone();
                ok_val(
                    Value::TypeContext(Arc::new(Mutex::new(child_data))),
                    call_span,
                )
            }
            other => Err(EvalError::type_mismatch_ctx(
                "builtin-fork-type-ctx".to_string(),
                "TypeContext",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-tc-with-type-stage-env`: Inject a runtime `Value::Environment` into a `TypeContext`
/// as its type-stage environment.
///
/// Takes 2 positional args (both forced):
///   - arg 0: `Value::TypeContext` — the TypeContext to update
///   - arg 1: `Value::Environment` — the env produced by evaluating type-stage documents
///
/// Locks the TypeContext mutex, replaces `tc.type_stage_env` with the provided env (wrapped
/// in a fresh `Arc<RwLock<_>>`), and returns the **same** `Value::TypeContext` value.
/// The mutation is in-place — the caller's handle already points to the same underlying data.
///
/// This is a thin wrapper with no logic beyond the field update. Used by loader.llt and
/// test-loader.llt to wire the type-stage env into the TypeContext before type-checking.
///
/// Signature: `[builtin-tc-with-type-stage-env type-ctx ts-env]`
pub(crate) fn builtin_tc_with_type_stage_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named(
            "builtin-tc-with-type-stage-env",
            named.as_ref(),
            call_span.clone(),
        )?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let tc_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let env_val = ctx.get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let tc_arc = match tc_val {
            Value::TypeContext(arc) => arc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-with-type-stage-env".to_string(),
                    "TypeContext",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let env_arc = match env_val {
            Value::Environment(arc) => arc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-with-type-stage-env".to_string(),
                    "Environment",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        {
            let mut guard = tc_arc.lock().unwrap();
            guard.type_stage_env = Arc::clone(&env_arc);
        }

        ok_val(Value::TypeContext(tc_arc), call_span)
    })
}

/// `builtin-program`: Construct a `Value::Program` from a sequence of Document values.
///
/// Takes a single positional argument: a Seq or Dict of `Value::Document` values.
/// Returns a `Value::Program` with the documents wrapped in a `SurfaceProgram` structure.
///
/// This is the primitive for reconstructing programs after transformation (e.g., desugar.llt).
/// The resolution, type annotation, and expects_resolved tables are initialized as empty —
/// callers should use `builtin-resolve` or other builtins to populate them if needed.
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
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-program", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let val = ctx.get_thunk(args[0])
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
                                node: std::sync::Arc::new((*surface_doc).clone()),
                                span: rust_span!(),
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
            _ => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-program".to_string(),
                    "Dict of Document values",
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
                resolutions: std::sync::Arc::new(Default::default()),
                types: std::sync::Arc::new(Default::default()),
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
            ..
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
                    dict_map.insert(HashableValue::Str(def.name.into()), thunk_id);
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

/// `builtin-eval`: evaluate a `Value::Document` or `Expr.*` nodes in a given environment.
///
/// Positional arg[0]: `Value::Document` (preferred) or integer-keyed `Dict` of `Expr.*` variants.
///
/// Named args:
/// - `env:`   (`Value::Env`) — the starting environment (required).
/// - `table:` (`Value::Dict`) — span-keyed resolution table from `builtin-resolve doc env: E`
///             (optional; if absent, empty table is used — unresolved VarRefs produce Error at eval time).
///
/// The `program:` argument has been removed. Resolution is now done per-document via
/// `[builtin-resolve doc env: E]` which returns `{doc, table}`. Pass `table:` here for
/// correct de Bruijn coordinate lookup during lowering.
///
/// Returns: `Value::Dict` with two keys:
/// - `env:` (`Value::Env`) — child env with `%` bound to the last expression's thunk
///   on success, or the original starting_env on failure.
/// - `error:` (`Value::Dict([])` = null on success, `Value::Str(message)` on failure)
///
/// Callers MUST check `result.error` before using `result.env`. This design ensures
/// Rust never prints errors — tinct code receives errors as data and decides what to do.
///
/// The `env:` arg is mandatory. Callers must pass a `Value::Env` (e.g. one constructed
/// via `builtin-extend-env`). There is no default stdlib_env fallback.
pub(crate) fn builtin_eval(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Named args:
        //   env:       (Value::Env) — required
        //   flat-env:  (Int, optional) — parent EnvId for flat-env evaluation scope
        //
        // Resolution is now inline on the AST nodes (written by builtin-resolve).
        // The `table:` argument has been removed; pass env: only.
        let (env_thunk_id, flat_env_parent_id) = if let Some(ref named_map) = named {
            for key in named_map.keys() {
                if key != "env" && key != "flat-env" {
                    return Err(EvalError::named_arg_rejected("eval".to_string(), call_span).into());
                }
            }
            let env_id = match named_map.get("env").copied() {
                Some(t) => t,
                None => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "Env (for env: argument — required)",
                        "absent",
                        call_span,
                    )
                    .into())
                }
            };
            // Extract optional flat-env: argument as a parent EnvId (Int → u32).
            // The value must be an Int (the flat-env-id returned by a prior builtin-eval call).
            // A wrong type is a correctness error — silently ignoring it would produce an
            // incomplete display chain and wrong VarRef resolution for stdlib names at level > 0.
            let flat_env_id = if let Some(&fe_thunk_id) = named_map.get("flat-env") {
                let fe_thunk = ctx.get_thunk(fe_thunk_id);
                let fe_val = materialize(&fe_thunk, Some(&call_span), &ctx).await?;
                match fe_val {
                    Value::Int(n) => Some(n as u32),
                    other => return Err(EvalError::type_mismatch_ctx(
                        "builtin-eval flat-env:".to_string(),
                        "Int (EnvId returned from a prior builtin-eval call)",
                        other.type_name(),
                        call_span,
                    )
                    .into()),
                }
            } else {
                None
            };
            (env_id, flat_env_id)
        } else {
            // No named args at all — env: is missing.
            return Err(EvalError::type_mismatch_ctx(
                "eval".to_string(),
                "Env (for env: argument — required)",
                "absent",
                call_span,
            )
            .into());
        };

        // Force env: — must be Value::Env.
        let env_thunk = ctx.get_thunk(env_thunk_id);
        let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
        let starting_env = match env_val {
            Value::Environment(e) => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval".to_string(),
                    "Env (for env: argument)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // arg[0]: Value::Document or Dict of Expr.* nodes.
        //
        // Value::Document path (preferred for normal document evaluation):
        //   Extracts SurfaceNodes directly from the document's item list using the
        //   original Arc pointers. Resolution is already inline on the nodes from builtin-resolve.
        //
        // Dict of Expr.* path (for metaprogramming, quote/unquote):
        //   Deserialises Expr.* variants into new SurfaceNodes. Resolution must have been
        //   written inline to the original nodes; deserialized nodes start unresolved.
        let input_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        let (expression_nodes, doc_name): (Vec<std::sync::Arc<crate::ast::SurfaceNode>>, String) =
            match input_val {
                // ── Document path (no round-trip) ──────────────────────────────
                Value::Document(doc) => {
                    let name = doc.name.clone().unwrap_or_default();
                    let nodes = doc
                        .items
                        .iter()
                        .filter_map(|item| {
                            if let crate::ast::SurfaceItem::Expr(node) = item {
                                Some(std::sync::Arc::clone(node))
                            } else {
                                None
                            }
                        })
                        .collect();
                    (nodes, name)
                }

                // ── Dict of Expr.* path (metaprogramming) ──────────────────────
                Value::Dict(input_map) => {
                    let mut keyed_entries: Vec<(i64, _)> = Vec::new();
                    for (key, val_id) in &input_map {
                        if let HashableValue::Int(idx) = key {
                            keyed_entries.push((*idx, *val_id));
                        }
                    }
                    keyed_entries.sort_by_key(|(k, _)| *k);

                    let mut nodes = Vec::new();
                    for (_idx, val_id) in keyed_entries {
                        let val =
                            materialize(&ctx.get_thunk(val_id), Some(&call_span), &ctx).await?;
                        match val {
                            Value::Variant { ref tag, .. } if tag.starts_with("Expr.") => {
                                let node = crate::surface_convert::dict_to_surface_node(
                                    &val, &call_span, &ctx,
                                )
                                .map_err(|e| {
                                    EvalError::internal(
                                        format!("eval: Expr.* conversion failed: {}", e),
                                        call_span.clone(),
                                    )
                                })?;
                                nodes.push(node);
                            }
                            other => {
                                return Err(EvalError::type_mismatch_ctx(
                                    "eval".to_string(),
                                    "Expr.*",
                                    other.type_name(),
                                    call_span,
                                )
                                .into())
                            }
                        }
                    }
                    (nodes, String::new())
                }

                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval".to_string(),
                        "Document or Dict of Expr.*",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            };

        // Evaluate the expression sequence. Use the _with_env variant so we get the
        // leaf env (current_env after all intermediate dict bindings are applied).
        // This is critical: intermediate dicts add bindings to current_env's chain,
        // so the result env must be a child of current_env (not of starting_env) to
        // expose those bindings (e.g. prelude's `=`, `map`) via ancestor lookup.
        //
        // Errors are returned as data in the result dict rather than propagated via
        // Rust's ? operator. This ensures Rust never prints errors — the caller (tinct
        // code) receives {env: ..., error: null_or_string} and decides what to do.
        //
        // flat-env: arg is threaded to eval_document_exprs_with_env for scoped arena allocation.
        let eval_result = crate::eval::eval_document_exprs_with_env(
            &expression_nodes,
            Arc::clone(&starting_env),
            &ctx,
            flat_env_parent_id,
        )
        .await;

        // The flat-env-id is the root EnvId of the FlatEnv scope allocated for this document.
        // eval_document_exprs_with_env returns the root env_id directly so we do not need to
        // inspect ctx.current_env_id (which would always be the pre-call value, not the new scope).
        let flat_env_id_val: i64 = match &eval_result {
            Ok((_, _, root_env_id)) => *root_env_id as i64,
            Err(_) => ctx.current_env_id as i64, // error path: return current (pre-call) env_id
        };

        let mut result_map: indexmap::IndexMap<HashableValue, ThunkId> = indexmap::IndexMap::new();

        match eval_result {
            Ok((result_thunk, leaf_env, _root_env_id)) => {
                // Return {env: leaf_env, result: last_thunk, doc-name: name, error: null,
                //         flat-env-id: Int}. The flat-env-id was already extracted above.
                // No % injection — tinct code decides what name to bind the result under.
                // No dict entry promotion — tinct pipeline loop uses make-entry to bind
                // result names into the env chain.
                result_map.insert(
                    HashableValue::Str("env".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Environment(leaf_env),
                        call_span.clone(),
                    ))),
                );
                result_map.insert(
                    HashableValue::Str("result".into()),
                    ctx.alloc_thunk(result_thunk),
                );
                result_map.insert(
                    HashableValue::Str("doc-name".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(&doc_name),
                        call_span.clone(),
                    ))),
                );
                result_map.insert(
                    HashableValue::Str("error".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Dict(indexmap::IndexMap::new()),
                        call_span.clone(),
                    ))),
                );
                result_map.insert(
                    HashableValue::Str("flat-env-id".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Int(flat_env_id_val),
                        call_span.clone(),
                    ))),
                );
            }
            Err(e) => {
                let msg = format!("{}", e);
                result_map.insert(
                    HashableValue::Str("env".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Environment(starting_env),
                        call_span.clone(),
                    ))),
                );
                result_map.insert(
                    HashableValue::Str("error".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        string_val(&msg),
                        call_span.clone(),
                    ))),
                );
                result_map.insert(
                    HashableValue::Str("flat-env-id".into()),
                    ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                        Value::Int(flat_env_id_val),
                        call_span.clone(),
                    ))),
                );
            }
        }

        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(result_map),
            call_span,
        )))
    })
}

/// `builtin-variant-payload`: extract the payload from a Variant, returning it directly.
/// Takes 1 arg (a Variant). Returns the payload value (forces the payload thunk).
/// Used to extract values from Result.Ok/Error without going through field-get (which
/// fails when the payload is a non-dict value like String).
pub(crate) fn builtin_variant_payload(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("variant-payload", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        match val {
            Value::Variant {
                payload: Some(payload_id),
                ..
            } => {
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = materialize(&payload_thunk, Some(&call_span), &ctx).await?;
                ok_val(payload_val, call_span)
            }
            Value::Variant { payload: None, .. } => {
                ok_val(Value::Dict(indexmap::IndexMap::new()), call_span)
            }
            other => Err(EvalError::type_mismatch_ctx(
                "variant-payload".to_string(),
                "Variant",
                other.type_name(),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-eval-repr`: evaluate a document in an env and return `builtin-llt-repr` of the
/// last expression's result. Combines `builtin-eval` + `builtin-llt-repr` atomically.
///
/// Named arg `env:` (Value::Env) — required, same as builtin-eval.
/// Positional arg[0]: Value::Document or Dict of Expr.* nodes.
/// Returns: String (the llt-repr of the last expression's value).
pub(crate) fn builtin_eval_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let env_thunk_id = if let Some(ref named_map) = named {
            for key in named_map.keys() {
                if key != "env" {
                    return Err(
                        EvalError::named_arg_rejected("eval-repr".to_string(), call_span).into(),
                    );
                }
            }
            match named_map.get("env").copied() {
                Some(t) => t,
                None => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval-repr".to_string(),
                        "Env (for env: argument — required)",
                        "absent",
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            return Err(EvalError::type_mismatch_ctx(
                "eval-repr".to_string(),
                "Env (for env: argument — required)",
                "absent",
                call_span,
            )
            .into());
        };

        let env_thunk = ctx.get_thunk(env_thunk_id);
        let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
        let starting_env = match env_val {
            Value::Environment(e) => e,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval-repr".to_string(),
                    "Env (for env: argument)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let input_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        let expression_nodes: Vec<std::sync::Arc<crate::ast::SurfaceNode>> = match input_val {
            Value::Document(doc) => doc
                .items
                .iter()
                .filter_map(|item| {
                    if let crate::ast::SurfaceItem::Expr(node) = item {
                        Some(std::sync::Arc::clone(node))
                    } else {
                        None
                    }
                })
                .collect(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval-repr".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let (result_thunk, _leaf_env, _root_env_id) = crate::eval::eval_document_exprs_with_env(
            &expression_nodes,
            Arc::clone(&starting_env),
            &ctx,
            None,
        )
        .await?;

        let result_val = materialize(&result_thunk, Some(&call_span), &ctx).await?;
        let repr = crate::value_to_display_string(&result_val, &ctx, call_span.clone())
            .await
            .map_err(|e| {
                EvalError::internal(format!("eval-repr: {}", e.kind), call_span.clone())
            })?;
        ok_val(string_val(&repr), call_span)
    })
}

/// `builtin-extend-env`: create a child (or fresh) environment with additional bindings.
///
/// arg[0]: `Value::Env` — the parent environment, OR `Value::Dict({})` (empty dict)
///   to create a fresh environment with no parent chain.
/// arg[1]: `Value::Dict` — string-keyed bindings to add as a child layer, OR
///   `Value::Env` — whose own top-level bindings are copied into the child.
///   Integer keys in a Dict are silently skipped. The values remain as thunks — no
///   materialization of dict or env values.
///
/// Full matrix:
/// - `(Env, Dict)` → child of parent env with dict's string-keyed entries
/// - `(Dict({}), Dict)`    → fresh env (no parent) with dict's string-keyed entries
/// - `(Env, Env)` → child of parent env with source env's own bindings
/// - `(Dict({}), Env)`    → fresh env (no parent) with source env's own bindings
///
/// Returns: `Value::Dict` with `env` (Value::Environment) and `flat-env-id` (Value::Int) fields.
///
/// Useful for injecting additional bindings into an environment before passing it to
/// `builtin-eval`, without mutating the original env.
pub(crate) fn builtin_extend_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        // Extract optional flat-env: named arg (parent's flat-env-id for display chain)
        let parent_flat_env_id: Option<u32> = if let Some(ref named_map) = named {
            if let Some(&fei_tid) = named_map.get("flat-env") {
                let fei_thunk = ctx.get_thunk(fei_tid);
                let fei_val = materialize(&fei_thunk, Some(&call_span), &ctx).await?;
                match fei_val {
                    Value::Int(n) => Some(n as u32),
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "extend-env flat-env:".to_string(),
                            "Int",
                            other.type_name(),
                            call_span,
                        )
                        .into())
                    }
                }
            } else {
                None
            }
        } else {
            None
        };

        // Reject any other named args
        if let Some(ref named_map) = named {
            for key in named_map.keys() {
                if key != "flat-env" {
                    return Err(EvalError::named_arg_rejected(
                        "extend-env".to_string(),
                        call_span,
                    )
                    .into());
                }
            }
        }

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Force arg[0] — must be Value::Env or Value::Dict({}) (empty dict → fresh env).
        let arg0_thunk = ctx.get_thunk(args[0]);
        let parent_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let base_env: Option<Arc<std::sync::RwLock<Env>>> = match parent_val {
            Value::Environment(e) => Some(e),
            Value::Dict(ref m) if m.is_empty() => None, // empty dict → fresh env with no parent
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "extend-env".to_string(),
                    "Env or empty Dict",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Create the child environment (or a fresh env if base_env is None).
        let child_env = Arc::new(std::sync::RwLock::new(match base_env {
            Some(ref parent) => Env::with_parent(Arc::clone(parent)),
            None => Env::new(),
        }));

        // Force arg[1] — must be Value::Dict or Value::Env.
        // Normalize: peel Annotated wrappers and flatten Overlay into a plain Dict.
        // builtin-merge returns Value::Overlay (lazy); @Any annotations wrap in Annotated.
        // Both have type_name()="Dict" but don't match Value::Dict directly.
        let arg1_thunk = ctx.get_thunk(args[1]);
        let bindings_raw = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;
        let bindings_val = {
            let mut v = bindings_raw;
            while let Value::Annotated { inner, .. } = v {
                v = *inner;
            }
            if let Value::Overlay(ref left, ref right) = v {
                v = Value::Dict(
                    crate::builtins::flatten_overlay(
                        left,
                        right,
                        "extend-env",
                        &ctx,
                        call_span.clone(),
                    )
                    .await?,
                );
            }
            v
        };
        // T-1571: Allocate FlatEnv and populate slots for Dict bindings.
        let child_env_id = match bindings_val {
            Value::Dict(ref bindings) => {
                let mut env_write = child_env.write().unwrap();

                // Count string-keyed bindings to size the FlatEnv
                let binding_count = bindings.iter()
                    .filter(|(k, _)| matches!(k, HashableValue::Str(_)))
                    .count();

                // Allocate child FlatEnv. If parent flat-env-id is provided via flat-env:,
                // use alloc_child to chain the display vector. Otherwise alloc_root.
                let child_env_id = if let Some(parent_id) = parent_flat_env_id {
                    ctx.env_arena.borrow_mut()
                        .alloc_child(crate::arena::EnvId(parent_id), binding_count)
                } else {
                    ctx.env_arena.borrow_mut()
                        .alloc_root(binding_count)
                };

                // Register slot names in Environment (for resolver metadata)
                // and fill FlatEnv slots with thunk values (for runtime lookup).
                let mut slot_idx = 0u32;
                for (key, thunk_id) in bindings.iter() {
                    if let HashableValue::Str(name) = key {
                        env_write.insert_slot_name_only(name.to_string());

                        // Fill the FlatEnv slot with the binding's thunk
                        ctx.env_arena.borrow_mut()
                            .fill_letrec_slot(child_env_id, slot_idx, *thunk_id);
                        slot_idx += 1;
                    }
                }

                child_env_id
            }
            Value::Environment(ref src_env) => {
                // Env case: register slot names but don't populate FlatEnv slots.
                // We can't extract ThunkIds from Value::Environment (Env no longer stores thunks).
                // Callers that need FlatEnv population must use the Dict form.
                let mut env_write = child_env.write().unwrap();
                let src_read = src_env.read().unwrap();
                for (name, _slot) in src_read.iter_slots() {
                    env_write.insert_slot_name_only(name.to_string());
                }
                drop(env_write);
                drop(src_read);

                // Allocate an empty FlatEnv (no slots populated)
                ctx.env_arena.borrow_mut().alloc_root(0)
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "extend-env".to_string(),
                    "Dict or Env",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // T-1571: Return a dict with both env and flat-env-id
        let mut result_map: indexmap::IndexMap<crate::value::HashableValue, crate::value::ThunkId> = indexmap::IndexMap::new();
        result_map.insert(
            crate::value::HashableValue::Str("env".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Environment(child_env),
                call_span.clone(),
            ))),
        );
        result_map.insert(
            crate::value::HashableValue::Str("flat-env-id".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(child_env_id.0 as i64),
                call_span.clone(),
            ))),
        );
        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(result_map),
            call_span,
        )))
    })
}

/// `builtin-current-env`: capture and return the calling environment.
///
/// Takes zero arguments. Returns the `Value::Env` that was the caller's
/// lexical environment at the point of the call. This is the environment in scope
/// at the `[builtin-current-env]` call site — not the environment of `builtin-current-env`
/// itself (which has no body).
///
/// This builtin works because the evaluator threads `caller_env` through `BuiltinArgs`,
/// capturing it when a `PendingCall` resolves to a `Value::Builtin`. Internal
/// builtin-to-builtin calls (not via user-code PendingCall dispatch) will produce an
/// empty environment — `builtin-current-env` is only meaningful when called from user code.
pub(crate) fn builtin_current_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("current-env", named.as_ref(), call_span.clone())?;
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
        }
        // T-1558: caller_env is Arc<RwLock<value::Environment>> (legacy binding chain),
        // but Value::Environment wraps Arc<RwLock<env::Env>> (type-metadata env).
        // Until T-1559 wires the FlatEnv→Env frame connection, return stdlib_env as the
        // captured environment. This preserves the return type contract while T-1559
        // completes the proper call-site env capture.
        Ok(Arc::new(Thunk::new_materialized(
            Value::Environment(Arc::clone(&ctx.config.stdlib_env)),
            call_span,
        )))
    })
}

/// `builtin-eval-macro-ast`: evaluate a macro-produced AST in the call-site scope.
///
/// Takes 1 positional arg: an `Expr.*` Value::Variant representing the AST to evaluate.
///
/// The call-site environment and span are read from the `BuiltinArgs.caller_env` environment
/// chain, where they were bound by `bind_args_thunks` under the system-injected names
/// `ᴍᴀᴄʀᴏ∷env` and `ᴍᴀᴄʀᴏ∷span` (injected by the @Expr PendingCallDispatch handler in
/// eval_materialize.rs).  This allows macro functions to call `[eval-macro-ast ast]` without
/// explicitly threading `__call-env__` / `__call-span__` through their parameter lists.
///
/// Evaluation pipeline:
///   1. Read `ᴍᴀᴄʀᴏ∷env` from caller_env → call-site `Value::Env`
///   2. Convert the `Expr.*` variant to a `SurfaceNode` using `dict_to_surface_node`
///   3. Wrap in a single-expression `SurfaceProgram`
///   4. Desugar + resolve in the call-site env
///   5. Evaluate via `eval_document_exprs_with_env`
///   6. Return the result thunk (the `%` of the resulting environment)
///
/// Falls back to `caller_env` as the evaluation environment when `ᴍᴀᴄʀᴏ∷env` is absent
/// (i.e. when called outside a macro context — useful for testing and direct use).
pub(crate) fn builtin_eval_macro_ast(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named: _,
            call_span,
            ctx,
            caller_env,
            caller_env_id: _,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // ── Step 1: Extract ᴍᴀᴄʀᴏ∷env from the caller_env chain ─────────────
        //
        // bind_args_thunks (eval_call.rs BIND-SYSTEM) unconditionally binds any named arg
        // whose name contains '∷' into call_env.  The @Expr PendingCallDispatch handler
        // injects "ᴍᴀᴄʀᴏ∷env" and "ᴍᴀᴄʀᴏ∷span" so they propagate through every
        // tinct function in the macro call chain down to here.
        //
        // T-1558: ᴍᴀᴄʀᴏ∷env is now injected as Value::Int(caller_env_id) — a placeholder
        // until T-1559 wires the full FlatEnv→Env connection. For now, fall back to
        // ctx.config.stdlib_env as the macro evaluation scope so the resolver can assign
        // De Bruijn coordinates based on the stdlib binding set.
        const MACRO_ENV_NAME: &str = "ᴍᴀᴄʀᴏ∷env";

        let call_site_env: Arc<RwLock<Env>> = {
            let env_thunk_opt = caller_env.read().unwrap().get(MACRO_ENV_NAME);
            if let Some(env_thunk) = env_thunk_opt {
                let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
                match env_val {
                    // T-1558: ᴍᴀᴄʀᴏ∷env is now Value::Int(env_id). Use stdlib_env as the
                    // evaluation scope — T-1559 will wire the proper Env frame here.
                    Value::Int(_env_id) => Arc::clone(&ctx.config.stdlib_env),
                    Value::Environment(e) => e,
                    other => {
                        return Err(EvalError::type_mismatch_ctx(
                            "eval-macro-ast".to_string(),
                            "Env (ᴍᴀᴄʀᴏ∷env)",
                            other.type_name(),
                            call_span,
                        )
                        .into())
                    }
                }
            } else {
                // Not in a macro context — use stdlib_env so the resolver has the full
                // set of builtin names available. (T-1559 will provide the actual call-site env.)
                Arc::clone(&ctx.config.stdlib_env)
            }
        };

        // ── Step 2: Convert Expr.* variant → SurfaceNode ─────────────────────
        let expr_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        let expr_node = match expr_val {
            Value::Variant { ref tag, .. } if tag.starts_with("Expr.") => {
                crate::surface_convert::dict_to_surface_node(&expr_val, &call_span, &ctx).map_err(
                    |e| {
                        EvalError::internal(
                            format!("eval-macro-ast: Expr.* conversion failed: {}", e),
                            call_span.clone(),
                        )
                    },
                )?
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval-macro-ast".to_string(),
                    "Expr.*",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // ── Step 3: Wrap in a single-expression SurfaceProgram ────────────────
        let document = crate::ast::SurfaceDocument {
            stage: None,
            name: None,
            items: vec![crate::ast::SurfaceItem::Expr(expr_node)],
            output_type: None,
            expects: None,
            caps: None,
            uses: None,
        };
        let mut program = crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_span.clone(),
            )],
        };

        // ── Step 4: Desugar + resolve in the call-site environment ────────────
        crate::desugar::desugar_surface_program(&mut program);
        // Seed resolver from the call-site env so that builtin names in the macro
        // expansion resolve to de Bruijn coordinates rather than falling back to
        // name-based lookup via the MAX/MAX sentinel.
        let _resolve_table =
            crate::resolve::resolve_surface_program(&program, Some(&call_site_env));

        // ── Step 5: Extract the single expression node ────────────────────────
        let expression_nodes: Vec<Arc<crate::ast::SurfaceNode>> = program
            .documents
            .into_iter()
            .flat_map(|d| d.node.items.clone())
            .filter_map(|item| {
                if let crate::ast::SurfaceItem::Expr(node) = item {
                    Some(node)
                } else {
                    None
                }
            })
            .collect();

        // ── Step 6: Evaluate and return the result thunk ──────────────────────
        let (result_thunk, _leaf_env, _root_env_id) =
            crate::eval::eval_document_exprs_with_env(&expression_nodes, call_site_env, &ctx, None)
                .await?;

        Ok(result_thunk)
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
            ..
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

            let env_dict = named_map.get("env").copied();
            let pipeline_input = named_map.get("%").copied();
            (env_dict, pipeline_input)
        } else {
            (None, None)
        };

        // Use type_stage_env as the base: type-level evaluation builtins only, no IO, no caps, no runtime API.
        let base_env = Arc::clone(&ctx.config.type_stage_env);

        // Add env: dict bindings if provided
        let env_with_bindings = if let Some(env_thunk_id) = env_dict {
            let env_thunk = ctx.get_thunk(env_thunk_id);
            let env_val = materialize(&env_thunk, Some(&call_span), &ctx).await?;
            match env_val {
                Value::Dict(entries) => {
                    let child_env = Arc::new(std::sync::RwLock::new(Env::with_parent(Arc::clone(
                        &base_env,
                    ))));
                    // T-1557: Env is type-metadata only; register slot names for the resolver.
                    // Runtime thunks go into the FlatEnv arena (T-1558/T-1559 will complete this).
                    for (key, _thunk_id) in entries.iter() {
                        if let HashableValue::Str(name) = key {
                            child_env
                                .write()
                                .unwrap()
                                .insert_slot_name_only(name.to_string());
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
        let final_env = if let Some(input_thunk_id) = pipeline_input {
            let _input_thunk = ctx.get_thunk(input_thunk_id);
            let child_env = Arc::new(std::sync::RwLock::new(Env::with_parent(Arc::clone(
                &env_with_bindings,
            ))));
            // T-1557: Env is type-metadata only; register "%" as a slot name for the resolver.
            // Runtime thunk goes into the FlatEnv arena (T-1558/T-1559 will complete this).
            child_env
                .write()
                .unwrap()
                .insert_slot_name_only("%".to_string());
            child_env
        } else {
            env_with_bindings
        };

        // Materialize the input — accepts Value::Document or integer-keyed Dict of Expression values
        let input_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        // Document path: extract SurfaceNodes directly (same as builtin-eval Document path).
        if let Value::Document(doc) = &input_val {
            let expr_nodes: Vec<std::sync::Arc<crate::ast::SurfaceNode>> = doc
                .items
                .iter()
                .filter_map(|item| {
                    if let crate::ast::SurfaceItem::Expr(node) = item {
                        Some(std::sync::Arc::clone(node))
                    } else {
                        None
                    }
                })
                .collect();
            let (result_thunk, _, _root_env_id) = crate::eval::eval_document_exprs_with_env(
                &expr_nodes,
                Arc::clone(&final_env),
                &ctx,
                None,
            )
            .await?;
            return Ok(result_thunk);
        }

        let input_map = match input_val {
            Value::Dict(m) => m,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval-types".to_string(),
                    "Document or Dict of Expression",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Collect Expr.* variant nodes from the Dict (in insertion order), converting to SurfaceNode.
        let mut expression_nodes = Vec::new();
        for (_key, val_id) in &input_map {
            let val = materialize(&ctx.get_thunk(*val_id), Some(&call_span), &ctx).await?;
            match val {
                Value::Variant { ref tag, .. } if tag.starts_with("Expr.") => {
                    let node = crate::surface_convert::dict_to_surface_node(&val, &call_span, &ctx)
                        .map_err(|e| {
                            EvalError::internal(
                                format!("eval-types: Expr.* conversion failed: {}", e),
                                call_span.clone(),
                            )
                        })?;
                    expression_nodes.push(node);
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "eval-types".to_string(),
                        "Expr.*",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }

        // Build integer-keyed Dict of Surface thunks (one per expression)
        let mut result_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, node) in expression_nodes.into_iter().enumerate() {
            let surface_thunk = Arc::new(Thunk::new_surface(
                node,
                Arc::new(std::collections::HashMap::new()),
                Arc::new(std::collections::HashMap::new()),
                ctx.current_env_id,
                Arc::clone(&ctx),
                call_span.clone(),
            ));
            let surface_thunk_id = ctx.alloc_thunk(surface_thunk);
            result_dict.insert(HashableValue::Int(i as i64), surface_thunk_id);
        }

        ok_val(Value::Dict(result_dict), call_span)
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
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "include-cache-get",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let key = require_string("include-cache-get", val, ctx.get_thunk(args[0]).span.clone())?;

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
            Some(crate::eval::IncludeCacheEntry::Cached(thunk)) => {
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
            ..
        } = ctx_arg;

        reject_named("include-cache-put", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let key_val = arg0_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let key = require_string("include-cache-put", key_val, arg0_thunk.span.clone())?;

        // The second arg is the entry variant: [Missing], [Pending], or [Cached value]
        let entry_val = arg1_thunk
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let entry = match &entry_val {
            Value::Variant { tag, payload } => {
                // Strip qualifier prefix ("IncludeCacheEntry.Pending" → "Pending") for
                // compatibility with T-974 qualified variant tags (lower.rs constructor dict).
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
                                    arg1_thunk.span.clone(),
                                )
                                .into())
                            }
                        };
                        crate::eval::IncludeCacheEntry::Cached(payload_thunk)
                    }
                    _ => {
                        return Err(EvalError::type_mismatch_ctx(
                            "include-cache-put".to_string(),
                            "IncludeCacheEntry.Missing | IncludeCacheEntry.Pending | IncludeCacheEntry.Cached value",
                            original_tag,
                            arg1_thunk.span.clone(),
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
                    arg1_thunk.span.clone(),
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
            ..
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
    args: &[ThunkId],
    named: Option<&IndexMap<String, ThunkId>>,
    ctx: &Arc<crate::eval::EvalContext>,
    call_span: Span,
) -> EvalResult<(Value, Value)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    if named.is_some() && !named.unwrap().is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }

    let val1 = ctx.get_thunk(args[0])
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let val2 = ctx.get_thunk(args[1])
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");

    Ok((val1, val2))
}

/// Return type alias for async-recursive `validate_value`.
type ValidationFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Vec<(String, String)>>>>>;

/// Recursive validation helper.
///
/// `path` is the dot-separated field path (e.g., "user.address.zip").
/// Returns a list of violations found; an empty list means the value is valid.
fn validate_value(
    schema: IndexMap<HashableValue, ThunkId>,
    data: Value,
    path: String,
    ctx: Arc<crate::eval::EvalContext>,
    span: Span,
) -> ValidationFuture {
    Box::pin(async move {
        use crate::value::HashableValue;
        let mut violations: Vec<(String, String)> = Vec::new();

        // Check `type` constraint
        if let Some(&type_thunk_id) = schema.get(&HashableValue::Str("type".into())) {
            let type_thunk = ctx.get_thunk(type_thunk_id);
            let type_val = materialize(&type_thunk, Some(&span), &ctx).await?;
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
        if let Some(&min_thunk_id) = schema.get(&HashableValue::Str("min".into())) {
            let min_thunk = ctx.get_thunk(min_thunk_id);
            let min_val = materialize(&min_thunk, Some(&span), &ctx).await?;
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

        if let Some(&max_thunk_id) = schema.get(&HashableValue::Str("max".into())) {
            let max_thunk = ctx.get_thunk(max_thunk_id);
            let max_val = materialize(&max_thunk, Some(&span), &ctx).await?;
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
        if let Some(&min_len_thunk_id) = schema.get(&HashableValue::Str("min-length".into())) {
            let min_len_thunk = ctx.get_thunk(min_len_thunk_id);
            let min_len_val = materialize(&min_len_thunk, Some(&span), &ctx).await?;
            if let Value::Int(min_len) = min_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                    } => Some((end - start) as i64),
                    Value::Dict(d) => Some(d.len() as i64),
                    _ => None,
                };
                if let Some(len) = actual_len {
                    if len < min_len {
                        violations.push((path.clone(), format!("length must be >= {}", min_len)));
                    }
                }
            }
        }

        if let Some(&max_len_thunk_id) = schema.get(&HashableValue::Str("max-length".into())) {
            let max_len_thunk = ctx.get_thunk(max_len_thunk_id);
            let max_len_val = materialize(&max_len_thunk, Some(&span), &ctx).await?;
            if let Value::Int(max_len) = max_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                    } => Some((end - start) as i64),
                    Value::Dict(d) => Some(d.len() as i64),
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
        if let Some(&pattern_thunk_id) = schema.get(&HashableValue::Str("pattern".into())) {
            let pattern_thunk = ctx.get_thunk(pattern_thunk_id);
            let pattern_val = materialize(&pattern_thunk, Some(&span), &ctx).await?;
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
        // Uses primitive_eq — only primitive types (Int, Float, String, unit Variant)
        // are compared. Dict/payload-Variant are not structurally compared.
        if let Some(&enum_thunk_id) = schema.get(&HashableValue::Str("enum".into())) {
            let enum_thunk = ctx.get_thunk(enum_thunk_id);
            let enum_val = materialize(&enum_thunk, Some(&span), &ctx).await?;
            if let Value::Dict(ref enum_dict) = enum_val {
                // Pre-materialize all enum values, then check membership via primitive equality.
                let mut allowed_values = Vec::with_capacity(enum_dict.len());
                for (_key, &val_thunk_id) in enum_dict.iter() {
                    let val_thunk = ctx.get_thunk(val_thunk_id);
                    allowed_values.push(materialize(&val_thunk, Some(&span), &ctx).await?);
                }

                let mut found = false;
                for allowed in &allowed_values {
                    if crate::eval::primitive_eq(allowed.clone(), data.clone()) {
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
        if let Some(&fields_thunk_id) = schema.get(&HashableValue::Str("fields".into())) {
            let fields_thunk = ctx.get_thunk(fields_thunk_id);
            let fields_val = materialize(&fields_thunk, Some(&span), &ctx).await?;
            if let Value::Dict(ref fields_schema) = fields_val {
                if let Value::Dict(ref data_dict) = data {
                    // Validate each field in the schema
                    for (field_key, &field_schema_thunk_id) in fields_schema {
                        let field_schema_thunk = ctx.get_thunk(field_schema_thunk_id);
                        let field_schema_val =
                            materialize(&field_schema_thunk, Some(&span), &ctx).await?;
                        if let Value::Dict(field_schema) = field_schema_val {
                            let field_name = match field_key {
                                HashableValue::Str(s) => s.to_string(),
                                HashableValue::Int(i) => i.to_string(),
                            };

                            let field_path = if path.is_empty() {
                                field_name.clone()
                            } else {
                                format!("{}.{}", path, field_name)
                            };

                            // Check if field is required
                            let is_required = if let Some(&req_thunk_id) =
                                field_schema.get(&HashableValue::Str("required".into()))
                            {
                                let req_thunk = ctx.get_thunk(req_thunk_id);
                                let req_val = materialize(&req_thunk, Some(&span), &ctx).await?;
                                matches!(&req_val, Value::Bool(true)) || matches!(&req_val, Value::Int(n) if *n != 0)
                            } else {
                                false
                            };

                            if let Some(&field_value_thunk_id) = data_dict.get(field_key) {
                                let field_value_thunk = ctx.get_thunk(field_value_thunk_id);
                                let field_value =
                                    materialize(&field_value_thunk, Some(&span), &ctx).await?;
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
        if let Some(&items_thunk_id) = schema.get(&HashableValue::Str("items".into())) {
            let items_thunk = ctx.get_thunk(items_thunk_id);
            let items_val = materialize(&items_thunk, Some(&span), &ctx).await?;
            if let Value::Dict(items_schema) = items_val {
                match &data {
                    Value::Dict(ref data_dict) => {
                        for (idx, (_key, &val_thunk_id)) in data_dict.iter().enumerate() {
                            let val_thunk = ctx.get_thunk(val_thunk_id);
                            let val = materialize(&val_thunk, Some(&span), &ctx).await?;
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
            ..
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

        let result = is_contractive_value(&body_val, &ctx).await;
        ok_val(Value::Int(if result { 1 } else { 0 }), call_span)
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
fn is_contractive_value<'a>(
    val: &'a Value,
    ctx: &'a Arc<crate::eval::EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + 'a>> {
    Box::pin(async move {
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
                let payload_id = match payload {
                    Some(id) => *id,
                    None => return true,
                };
                let payload_thunk = ctx.get_thunk(payload_id);
                let payload_val = match materialize(&payload_thunk, None, ctx).await {
                    Ok(v) => v,
                    Err(_) => return true,
                };
                let types_thunk_id = match &payload_val {
                    Value::Dict(d) => {
                        match d.get(&crate::value::HashableValue::Str("types".into())) {
                            Some(id) => *id,
                            None => return true,
                        }
                    }
                    _ => return true,
                };
                is_contractive_seq(types_thunk_id, ctx).await
            }

            // Case 2: all other TypeNode constructors are guarding.
            _ => true,
        }
    })
}

/// Check that every element in the `types` Dict is contractive.
///
/// TypeNode.Union/Intersect.types is now `[Map Int TypeNode]` — integer-keyed Dict.
/// Returns `true` iff all values are contractive. Empty or malformed input returns `true`
/// (conservative: no self-references = trivially contractive).
async fn is_contractive_seq(types_thunk_id: ThunkId, ctx: &Arc<crate::eval::EvalContext>) -> bool {
    let thunk = ctx.get_thunk(types_thunk_id);
    let val = match materialize(&thunk, None, ctx).await {
        Ok(v) => v,
        Err(_) => return true,
    };
    match &val {
        Value::Dict(d) => {
            for (_k, &v_id) in d {
                let v_thunk = ctx.get_thunk(v_id);
                let v_val = match materialize(&v_thunk, None, ctx).await {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if !is_contractive_value(&v_val, ctx).await {
                    return false;
                }
            }
            true
        }
        _ => true, // Not a Dict — no children to check.
    }
}

/// Walk a Seq spine and validate each element against `items_schema`.
///
/// Separated from `validate_value` so both can be async-recursive without
/// requiring a mutually-recursive `Box::pin` type cycle. Takes owned parameters
/// to enable `async move`.

/// `builtin-sequential`: construct a Sequential AST node from an expressions dict.
///
/// Used by boot-level macros (`>>` in loader.llt and test-loader.llt) that need
/// to produce Sequential before the prelude's `Expr` type is in scope.
///
/// Arg 0: integer-keyed dict of `Expr.*` variants (the expressions to sequence).
/// Returns: `Value::Variant { tag: "Expr.Sequential", payload: Some({exprs: dict}) }`.
pub(crate) fn builtin_sequential(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("sequential", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        // Materialize the expressions dict
        let arg0_thunk = ctx.get_thunk(args[0]);
        let exprs_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        // Extract each entry in insertion order as Expr.* variant, converting to SurfaceNode
        let exprs_dict = match &exprs_val {
            Value::Dict(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-sequential".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let mut exprs: Vec<Arc<crate::ast::SurfaceNode>> = Vec::new();
        // Iterate in insertion order (integer-keyed dict entries are in index order)
        for (_key, &thunk_id) in exprs_dict.iter() {
            let thunk = ctx.get_thunk(thunk_id);
            let val = materialize(&thunk, Some(&call_span), &ctx).await?;
            match val {
                Value::Variant { ref tag, .. } if tag.starts_with("Expr.") => {
                    let node = crate::surface_convert::dict_to_surface_node(&val, &call_span, &ctx)
                        .map_err(|e| {
                            EvalError::internal(
                                format!("builtin-sequential: Expr.* conversion failed: {}", e),
                                call_span.clone(),
                            )
                        })?;
                    exprs.push(node);
                }
                other => {
                    return Err(EvalError::type_mismatch_ctx(
                        "builtin-sequential".to_string(),
                        "Expr.*",
                        other.type_name(),
                        call_span,
                    )
                    .into())
                }
            }
        }
        let node = Arc::new(crate::ast::SurfaceNode::new(
            crate::ast::SurfaceExpression::Sequential(exprs),
            call_span.clone(),
        ));
        ok_val(
            crate::surface_convert::surface_node_to_expr_variant(&node, &ctx),
            call_span,
        )
    })
}

/// builtin-ast-to-program — convert an Expr.* Value::Variant to a Value::Program
///
/// Takes 1 positional arg (Expr.* variant) and 1 named arg (call-site-span: span dict).
/// Converts the Expr.* AST node back to a SurfaceNode, wraps it in a single-expression
/// SurfaceProgram with one SurfaceDocument containing one SurfaceItem::Expr.
/// Returns Value::Program.
pub(crate) fn builtin_ast_to_program(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Extract mandatory call-site-span: named arg
        let call_site_span_thunk_id = if let Some(ref named_map) = named {
            for key in named_map.keys() {
                if key != "call-site-span" {
                    return Err(EvalError::named_arg_rejected(
                        "ast-to-program".to_string(),
                        call_span,
                    )
                    .into());
                }
            }
            match named_map.get("call-site-span").copied() {
                Some(t) => t,
                None => {
                    return Err(EvalError::type_mismatch_ctx(
                        "ast-to-program".to_string(),
                        "span dict (for call-site-span: argument — required)",
                        "absent",
                        call_span,
                    )
                    .into())
                }
            }
        } else {
            return Err(EvalError::type_mismatch_ctx(
                "ast-to-program".to_string(),
                "span dict (for call-site-span: argument — required)",
                "absent",
                call_span,
            )
            .into());
        };

        // Materialize call-site-span: — must be a span dict
        let call_site_span_thunk = ctx.get_thunk(call_site_span_thunk_id);
        let call_site_span_val = materialize(&call_site_span_thunk, Some(&call_span), &ctx).await?;
        let call_site_span_actual = match &call_site_span_val {
            Value::Dict(dict) => {
                // Extract span from the dict using extract_span
                crate::surface_convert::extract_span(dict, &ctx).ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "ast-to-program".to_string(),
                        "valid span dict (with start/end Position fields)",
                        "invalid span dict",
                        call_span.clone(),
                    )
                })?
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "ast-to-program".to_string(),
                    "span dict (for call-site-span: argument)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // arg[0]: Expr.* Value::Variant
        let expr_val = ctx.get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        // Convert Expr.* Variant to SurfaceNode
        let expr_node = match expr_val {
            Value::Variant { ref tag, .. } if tag.starts_with("Expr.") => {
                crate::surface_convert::dict_to_surface_node(
                    &expr_val,
                    &call_site_span_actual,
                    &ctx,
                )
                .map_err(|e| {
                    EvalError::internal(
                        format!("ast-to-program: Expr.* conversion failed: {}", e),
                        call_span.clone(),
                    )
                })?
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "ast-to-program".to_string(),
                    "Expr.*",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Wrap in a SurfaceProgram: one document, one item (Expr)
        let document = crate::ast::SurfaceDocument {
            stage: None,
            name: None,
            items: vec![crate::ast::SurfaceItem::Expr(expr_node)],
            output_type: None,
            expects: None,
            caps: None,
            uses: None,
        };

        let program = crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_site_span_actual.clone(),
            )],
        };

        // Return Value::Program
        ok_val(
            Value::Program {
                program: Arc::new(program),
                resolutions: Arc::new(Default::default()),
                types: Arc::new(Default::default()),
                expects_resolved: Arc::new(std::collections::HashMap::new()),
            },
            call_span,
        )
    })
}

/// `builtin-check-type` (`check-type` in tinct): validate a value against a named type.
///
/// - arg 0: String — the type name to check against (e.g. `"String"`, `"Int"`, `"Dict"`)
/// - arg 1: Any — the value to validate
/// - Returns: arg 1 unchanged on success, raises `EvalError` on type mismatch
///
/// Unknown type names (type variables, complex parameterized types) pass conservatively
/// without validation. Used by tinct-side expects validation (T-1506).
pub(crate) fn builtin_check_type(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("check-type", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let type_name_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let type_name = require_string("check-type", type_name_val, arg0_thunk.span.clone())?;

        let value = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;

        let passes = match type_name.as_str() {
            "String" | "Str" => matches!(value, Value::String { .. }),
            "Int" => matches!(value, Value::Int(_)),
            "Float" => matches!(value, Value::Float(_)),
            "Bool" | "Boolean" => matches!(
                value,
                Value::Variant { ref tag, .. } if tag.starts_with("Boolean.")
            ),
            "Dict" => matches!(value, Value::Dict(_) | Value::Overlay(_, _)),
            "Null" => matches!(value, Value::Dict(ref d) if d.is_empty()),
            // Seq is not a distinct Value variant — sequences are Dict-like at runtime.
            // Checking "Seq" passes conservatively since a lazy sequence can't be
            // distinguished from a Dict without full materialization.
            "Seq" => true,
            "Bytes" => matches!(value, Value::Bytes { .. }),
            // Unknown annotations (type variables, parameterized types) pass conservatively.
            _ => true,
        };

        if passes {
            ok_val(value, call_span)
        } else {
            Err(EvalError::type_mismatch_ctx(
                "expects validation".to_string(),
                &type_name,
                value.type_name(),
                call_span,
            )
            .into())
        }
    })
}

/// `builtin-cap-env-has?` (`cap-env-has?` in tinct): check capability presence in a
/// tinct runtime environment.
///
/// - arg 0: String — the name to look up (e.g. `"%myfs"`)
/// - arg 1: Env (`Value::Env`) — the environment to search
/// - Returns: `Boolean.True` if the name is bound in the environment chain,
///   `Boolean.False` otherwise
///
/// Walks the full parent chain of the environment. Used by tinct-side caps enforcement
/// (T-1507) as the primitive that tinct code calls to validate declared caps.
pub(crate) fn builtin_cap_env_has(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        reject_named("cap-env-has?", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let arg0_thunk = ctx.get_thunk(args[0]);
        let arg1_thunk = ctx.get_thunk(args[1]);
        let name_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let name = require_string("cap-env-has?", name_val, arg0_thunk.span.clone())?;

        let env_val = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;
        let found = match env_val {
            Value::Environment(ref env_arc) => {
                let env = env_arc.read().unwrap();
                env.has_name(&name)
            }
            _ => false,
        };

        let tag = if found {
            "Boolean.True"
        } else {
            "Boolean.False"
        };
        ok_val(
            Value::Variant {
                tag: tag.to_string(),
                payload: None,
            },
            call_span,
        )
    })
}

/// `builtin-arena-new`: create a named evaluation scope (arena).
///
/// Takes 1 positional arg (String name). Returns `Value::Arena { name, env_id }`.
/// The arena's env_id can be used with `builtin-arena-drop` and `builtin-arena-stats`.
/// Scope is created in the arena's `EnvArena` with zero initial slot capacity.
pub(crate) fn builtin_arena_new(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("arena-new", named.as_ref(), call_span.clone())?;
        let name_thunk = ctx.get_thunk(args[0]);
        let name_val = materialize(&name_thunk, Some(&call_span), &ctx).await?;
        let name: Arc<str> = match name_val {
            Value::String { ref source, start, end } => source[start..end].into(),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-new".to_string(),
                    "String",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let env_id = ctx.env_arena.borrow_mut().alloc_root(0);
        Ok(Arc::new(Thunk::new_materialized(
            Value::Arena { name, env_id: env_id.0 },
            call_span,
        )))
    })
}

/// `builtin-arena-drop`: drop a scope, freeing all its thunks.
///
/// Takes 1 positional arg (`Value::Arena`). Clears all slots in the arena's FlatEnv,
/// releasing all `Arc<Thunk>` references. Returns empty dict `[]`.
pub(crate) fn builtin_arena_drop(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("arena-drop", named.as_ref(), call_span.clone())?;
        let arena_thunk = ctx.get_thunk(args[0]);
        let arena_val = materialize(&arena_thunk, Some(&call_span), &ctx).await?;
        let env_id = match arena_val {
            Value::Arena { env_id, .. } => env_id,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-drop".to_string(),
                    "Arena",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        ctx.env_arena
            .borrow_mut()
            .drop_scope(crate::arena::EnvId(env_id));
        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(indexmap::IndexMap::new()),
            call_span,
        )))
    })
}

/// `builtin-arena-stats`: return stats dict for an arena scope.
///
/// Takes 1 positional arg (`Value::Arena`). Returns `{name: String, thunks: Int}`.
/// `thunks` is the number of thunks ever allocated in the scope (monotonically increasing).
pub(crate) fn builtin_arena_stats(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        reject_named("arena-stats", named.as_ref(), call_span.clone())?;
        let arena_thunk = ctx.get_thunk(args[0]);
        let arena_val = materialize(&arena_thunk, Some(&call_span), &ctx).await?;
        let (name, env_id) = match arena_val {
            Value::Arena { name, env_id } => (name, env_id),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-stats".to_string(),
                    "Arena",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        let alloc_count = ctx
            .env_arena
            .borrow()
            .scope_alloc_count(crate::arena::EnvId(env_id));
        let mut result_map: indexmap::IndexMap<HashableValue, ThunkId> = indexmap::IndexMap::new();
        let name_str = name.to_string();
        result_map.insert(
            HashableValue::Str("name".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                string_val(&name_str),
                call_span.clone(),
            ))),
        );
        result_map.insert(
            HashableValue::Str("thunks".into()),
            ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Int(alloc_count as i64),
                call_span.clone(),
            ))),
        );
        Ok(Arc::new(Thunk::new_materialized(
            Value::Dict(result_map),
            call_span,
        )))
    })
}

/// `builtin-arena-migrate`: register a thunk in a destination arena without forcing it.
///
/// Takes 3 positional args: value (any), source arena (`Value::Arena`), destination arena (`Value::Arena`).
/// Forces only the destination arena to obtain its `env_id`. The value thunk is copied
/// as-is via `Arc::clone` — if it is unevaluated it remains unevaluated; if it is already
/// materialized the cached value is shared. Laziness is fully preserved.
///
/// Note: ThunkIds embedded inside `Value::Dict` entries within the migrated thunk still
/// point to the source arena. Full recursive migration (walking nested ThunkIds) is
/// deferred — the outer thunk carries its value and nested ThunkIds remain addressable
/// by their original IDs as long as the source arena is not dropped.
pub(crate) fn builtin_arena_migrate(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        reject_named("arena-migrate", named.as_ref(), call_span.clone())?;
        // args[0] is the ThunkId of the value to migrate — get the Arc<Thunk> without forcing.
        let src_thunk = ctx.get_thunk(args[0]);
        // args[1] is the source arena — validate that it is a Value::Arena.
        let src_arena_thunk = ctx.get_thunk(args[1]);
        let src_arena_val = materialize(&src_arena_thunk, Some(&call_span), &ctx).await?;
        match src_arena_val {
            Value::Arena { .. } => {} // valid source arena
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-migrate".to_string(),
                    "Arena (src)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        }
        // Force only the destination arena (args[2]) to obtain its env_id.
        let dst_thunk = ctx.get_thunk(args[2]);
        let dst_val = materialize(&dst_thunk, Some(&call_span), &ctx).await?;
        let dst_env_id = match dst_val {
            Value::Arena { env_id, .. } => env_id,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-migrate".to_string(),
                    "Arena (dst)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        // Copy the Arc<Thunk> handle into the destination arena without forcing it.
        // If the thunk is unevaluated it stays unevaluated; if materialized the cached
        // value is shared. This is the only correct path — no materialization here.
        let migrated_id = ctx
            .env_arena
            .borrow_mut()
            .alloc_slot_thunk(crate::arena::EnvId(dst_env_id), Arc::clone(&src_thunk));
        Ok(ctx.get_thunk(migrated_id))
    })
}

/// Returns all "meta" module Rust builtins.
///
/// These are the AST, evaluation, reflection, and macro builtins that are NOT in the
/// Core-46 set. The Core-46 items (builtin-parse, builtin-resolve, builtin-typecheck,
/// builtin-eval, builtin-module, builtin-extend-env, builtin-get-type-context,
/// builtin-tc-with-type-stage-env, builtin-tag-of, builtin-variant-payload,
/// builtin-type-of, builtin-llt-repr, builtin-cap-env-has?, builtin-check-type)
/// stay in core_builtins() for loader.llt.
///
/// Consumed exclusively by `builtin_module("meta")` in `src/builtins.rs`.
pub fn meta_builtins() -> Vec<crate::value::BuiltinDef> {
    use crate::builtins::builtin;
    use crate::value::Strictness;
    vec![
        // ── AST construction and evaluation ───────────────────────────────────────────
        builtin!("builtin-ast-of", builtin_ast_of, [Strictness::Id], 0, ["x"]),
        builtin!(
            "builtin-eval-macro-ast",
            builtin_eval_macro_ast,
            [Strictness::Seq],
            1,
            ["ast"]
        ),
        builtin!(
            "builtin-eval-types",
            builtin_eval_types,
            [Strictness::Seq],
            1,
            ["program"]
        ),
        builtin!(
            "builtin-eval-repr",
            builtin_eval_repr,
            [Strictness::Seq],
            1,
            ["x"]
        ),
        builtin!("builtin-load", builtin_load, [Strictness::Seq], 1, ["path"]),
        builtin!(
            "builtin-ast-to-program",
            builtin_ast_to_program,
            [Strictness::Seq],
            1,
            ["ast"]
        ),
        builtin!(
            "builtin-program",
            builtin_program,
            [Strictness::Spine],
            1,
            ["docs"]
        ),
        // ── Type-checking context ─────────────────────────────────────────────────────
        // builtin-make-type-ctx is in core_builtins() — loader needs it at bootstrap.
        builtin!(
            "builtin-fork-type-ctx",
            builtin_fork_type_ctx,
            [Strictness::Seq],
            1,
            ["type-ctx"]
        ),
        // ── Reflection and introspection ───────────────────────────────────────────────
        builtin!(
            "builtin-annotation-of",
            builtin_annotation_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-make-annotated",
            builtin_make_annotated,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["value", "annotation"]
        ),
        builtin!(
            "builtin-span-of",
            builtin_span_of,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-var-resolution",
            builtin_var_resolution,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["offset", "env"]
        ),
        builtin!(
            "builtin-to-tinct",
            crate::stream::builtin_to_tinct,
            [Strictness::Seq],
            1,
            ["x"]
        ),
        // ── Evaluation control (non-Core-46) ──────────────────────────────────────────
        builtin!(
            "builtin-materialize",
            builtin_force,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!("builtin-until", builtin_until),
        builtin!(
            "builtin-apply",
            builtin_apply,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["f", "args"]
        ),
        builtin!(
            "builtin-validate",
            builtin_validate,
            [Strictness::Seq, Strictness::Seq],
            2,
            ["schema", "x"]
        ),
        // ── Schema validation ─────────────────────────────────────────────────────────
        builtin!(
            "builtin-is-contractive",
            builtin_is_contractive,
            [Strictness::Seq],
            0,
            ["type-node"]
        ),
        // ── Numeric value types ───────────────────────────────────────────────────────
        builtin!(
            "builtin-decimal",
            builtin_decimal,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        builtin!(
            "builtin-big-int",
            builtin_big_int,
            [Strictness::Seq],
            0,
            ["x"]
        ),
        // ── Proxy ────────────────────────────────────────────────────────────────────
        builtin!("builtin-proxy", crate::builtins::builtin_proxy),
        // ── Macro support ─────────────────────────────────────────────────────────────
        builtin!(
            "builtin-macro-error",
            builtin_macro_error,
            [Strictness::Seq, Strictness::Id]
        ),
        builtin!(
            "builtin-macro-injects",
            builtin_macro_injects,
            [Strictness::Seq],
            0,
            ["macro-env"]
        ),
        // ── Boot-level AST construction ───────────────────────────────────────────────
        builtin!(
            "builtin-sequential",
            builtin_sequential,
            [Strictness::Seq],
            0,
            ["exprs"]
        ),
        // ── Unique name generation ────────────────────────────────────────────────────
        builtin!(
            "builtin-gensym",
            builtin_gensym,
            [Strictness::Seq, Strictness::Seq],
            0,
            ["prefix", "n"]
        ),
        // ── Hashing and identity ──────────────────────────────────────────────────────
        builtin!(
            "builtin-blake3",
            builtin_blake3,
            [Strictness::Seq],
            0,
            ["bytes"]
        ),
        builtin!(
            "builtin-cap-identity",
            builtin_cap_identity,
            [Strictness::Seq],
            0,
            ["cap"]
        ),
        // ── Include cache ─────────────────────────────────────────────────────────────
        builtin!(
            "builtin-include-cache-get",
            builtin_include_cache_get,
            [Strictness::Seq],
            1,
            ["key"]
        ),
        builtin!(
            "builtin-include-cache-put",
            builtin_include_cache_put,
            [],
            2,
            ["key", "value"]
        ),
        // ── Environment access ────────────────────────────────────────────────────────
        // builtin-current-env: zero-arg; returns the calling lexical environment.
        builtin!("builtin-current-env", builtin_current_env, [], 0),
    ]
}


#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use indexmap::IndexMap;

    use super::{builtin_current_env, builtin_tag_of};
    use crate::env::Env;
    use crate::error::EvalResult;
    use crate::test_util::test_span;
    use crate::value::{string_val, BuiltinArgs, Thunk, Value};

    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::new_materialized(val, test_span(1, 1, 1, 5)))
    }

    /// Allocate a ThunkId for a Value in the test context. T-1558: BuiltinArgs.args uses ThunkId.
    fn thunk_id(val: Value, ctx: &std::sync::Arc<crate::eval::EvalContext>) -> crate::arena::ThunkId {
        ctx.alloc_thunk(thunk(val))
    }

    fn call_span() -> crate::ast::Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        let env = Arc::new(RwLock::new(Env::new()));
        crate::eval::EvalContext::new_empty(base_dir, env, false)
    }

    fn no_named() -> Option<IndexMap<String, crate::arena::ThunkId>> {
        None
    }

    async fn run(
        f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>,
    ) -> EvalResult<Arc<Thunk>> {
        f.await
    }

    async fn materialize_sync(t: &Arc<Thunk>, ctx: &Arc<crate::eval::EvalContext>) -> Value {
        crate::eval::materialize(t, None, ctx)
            .await
            .unwrap_or_else(|e| panic!("materialize failed: {e}"))
    }

    /// `builtin_tag_of` returns the tag of a bare `Value::Variant`.
    #[tokio::test]
    async fn tag_of_bare_variant() {
        let variant = Value::Variant {
            tag: "Color.Red".to_string(),
            payload: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(variant, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }))
        .await
        .unwrap();
        let val = materialize_sync(&result, &ctx).await;
        assert_eq!(val, string_val("Color.Red"));
    }

    /// `builtin_tag_of` peels a single `Value::Annotated` wrapper and returns the
    /// inner variant's tag. This is the primary regression case for B-441.
    #[tokio::test]
    async fn tag_of_annotated_variant_single_wrap() {
        let variant = Value::Variant {
            tag: "SimpleType.Leaf".to_string(),
            payload: None,
        };
        let annotation = Value::Dict(IndexMap::new());
        let annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(annotation),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }))
        .await
        .unwrap();
        let val = materialize_sync(&result, &ctx).await;
        assert_eq!(val, string_val("SimpleType.Leaf"));
    }

    /// `builtin_tag_of` peels multiple nested `Value::Annotated` wrappers (the `while let`
    /// loop handles more than one layer of annotation).
    #[tokio::test]
    async fn tag_of_annotated_variant_double_wrap() {
        let variant = Value::Variant {
            tag: "Shape.Circle".to_string(),
            payload: None,
        };
        let inner_annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(Value::Dict(IndexMap::new())),
        };
        let outer_annotated = Value::Annotated {
            inner: Box::new(inner_annotated),
            annotation: Box::new(Value::Dict(IndexMap::new())),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(outer_annotated, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }))
        .await
        .unwrap();
        let val = materialize_sync(&result, &ctx).await;
        assert_eq!(val, string_val("Shape.Circle"));
    }

    /// `builtin_current_env` captures the `caller_env` from `BuiltinArgs` and returns it
    /// as a `Value::Env`. The returned environment must be the exact same
    /// `Arc<RwLock<Env>>` that was passed in, so bindings inserted before the
    /// call are accessible via `get_by_name` on the captured env.
    #[tokio::test]
    async fn current_env_captures_caller_env() {
        // Build a caller environment with a known binding: "x" → Int(42).
        // T-1558: BuiltinArgs.caller_env is crate::value::Environment (runtime binding chain).
        let caller_env = Arc::new(RwLock::new(crate::value::Environment::new()));
        {
            let mut env = caller_env.write().unwrap();
            let x_thunk = Arc::new(Thunk::new_materialized(Value::Int(42), call_span()));
            env.bindings.insert("x".to_string(), x_thunk);
        }

        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env: Arc::clone(&caller_env),
            caller_env_id: 0,
        }))
        .await
        .unwrap();

        // The thunk must materialize to Value::Environment.
        let val = materialize_sync(&result, &ctx).await;
        let captured_env = match val {
            Value::Environment(env) => env,
            other => panic!("expected Value::Environment, got {other:?}"),
        };

        // The captured environment must contain the binding 'x' was registered.
        // After T-1557, Env stores only type metadata (slot names, no values).
        // Verify the name is present in the slot registry.
        let _x_present = captured_env
            .read()
            .unwrap()
            .slots
            .contains_key("x");
        assert!(_x_present, "binding 'x' must be present in captured env slots");
        // After T-1557, runtime values live in FlatEnv, not Env.
        // The value assertion is deferred to an integration test (T-1567).
    }

    /// `builtin_current_env` rejects positional arguments — it takes zero args.
    #[tokio::test]
    async fn current_env_rejects_positional_args() {
        let caller_env = Arc::new(RwLock::new(crate::value::Environment::new()));
        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![thunk_id(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env,
            caller_env_id: 0,
        }))
        .await;
        assert!(
            result.is_err(),
            "expected arity error when passing args to current-env, got ok"
        );
        let msg = format!("{}", result.unwrap_err());
        assert!(
            msg.contains("E020") || msg.contains("arity"),
            "error should be an arity mismatch, got: {msg}"
        );
    }

    /// `builtin_tag_of` returns a type-mismatch error when the peeled value is not a
    /// `Value::Variant` — even if it was wrapped in `Value::Annotated`.
    #[tokio::test]
    async fn tag_of_annotated_non_variant_errors() {
        let annotated = Value::Annotated {
            inner: Box::new(Value::Int(42)),
            annotation: Box::new(Value::Dict(IndexMap::new())),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env: Arc::new(RwLock::new(crate::value::Environment::new())),
            caller_env_id: 0,
        }))
        .await;
        assert!(
            result.is_err(),
            "expected type-mismatch error for annotated non-variant, got ok"
        );
        let err = result.unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("Variant"),
            "error should mention expected type 'Variant', got: {msg}"
        );
        assert!(
            msg.contains("Int"),
            "error should mention actual type 'Int', got: {msg}"
        );
    }
}
