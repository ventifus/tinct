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
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{builtin, ok_val, reject_named, require_string, synthetic_call_expr};
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
    let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));

    let span_id = match &err.span {
        Some(s) => make_span_dict(s, ctx, call_span),
        None => {
            // No tinct source span — report the Rust location that handled the error.
            make_span_dict(&crate::rust_span!(), ctx, call_span)
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
        let arg_thunk = ctx.get_thunk(args[0]);
        let call_result = materialize(&arg_thunk, Some(&call_span), &ctx).await;

        match call_result {
            Ok(val) => {
                // Success: {ok: value}. Caller uses [builtin-has-key? "ok" raw] to discriminate.
                // Both success and failure return Dicts so builtin-has-key? is always safe.
                let mut map = IndexMap::new();
                let val_tid = ctx.alloc_thunk(0, ok_val(val, call_span.clone())?);
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
                    ctx.alloc_thunk(0, ok_val(string_val(&e.to_string()), call_span.clone())?);
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
            caller_env_id,
            ctx,
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
        let pred_matchable_binding = crate::eval::resolve_matchable_binding_from_fn(&pred_fn_val);
        // Wrap the materialized predicate back into a thunk for use in pending calls.
        let pred_thunk = Arc::new(Thunk::value(pred_fn_val, call_span.clone()));

        loop {
            // Create a pending call to pred(val) and materialize it.
            let pred_id = ctx.alloc_thunk(0, Arc::clone(&pred_thunk));
            let val_id = ctx.alloc_thunk(0, Arc::clone(&val_thunk));
            let pred_result = Arc::new(Thunk::fn_call(
                pred_id,
                vec![val_id],
                IndexMap::new(),
                call_span.clone(),
                caller_env_id,
                val_thunk.span.clone().with_name(Arc::from("until")),
                Arc::clone(&ctx),
                synthetic_call_expr(call_span.clone()),
            ));

            let pred_val = materialize(&pred_result, Some(&call_span), &ctx).await?;

            // call_to_match_opt_resolved ignores legacy env (B-515 tracks FlatEnv arm binding lookup).
            let dummy_env_for_match =
                std::sync::Arc::new(std::sync::RwLock::new(crate::value::Environment::new()));
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
                let f_id = ctx.alloc_thunk(0, Arc::clone(&f_thunk));
                let val_id = ctx.alloc_thunk(0, Arc::clone(&val_thunk));
                let f_result = Arc::new(Thunk::fn_call(
                    f_id,
                    vec![val_id],
                    IndexMap::new(),
                    call_span.clone(),
                    caller_env_id,
                    call_span.clone().with_name(Arc::from("until")),
                    Arc::clone(&ctx),
                    synthetic_call_expr(call_span.clone()),
                ));

                // Eagerly materialize f(val) and re-wrap as a thunk for the next iteration
                // This breaks the thunk chain and prevents stack overflow
                let f_val = materialize(&f_result, Some(&call_span), &ctx).await?;
                val_thunk = Arc::new(Thunk::value(f_val, call_span.clone()));
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
                    call_span: call_span.with_name(Arc::from("apply")),
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
                    if i < positional_ids.len() && (s == Strictness::Seq || s == Strictness::Spine)
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
            caller_env_id,
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
        Ok(Arc::new(Thunk::builtin_call(
            // force_count=2: pre-materialize both args[0] (function) and args[1] (args-dict)
            // before calling builtin_apply_impl, which uses try_get_materialized().expect(...).
            builtin!("builtin-apply", builtin_apply_impl, [], 2),
            args,
            named_opt,
            call_span.with_name(Arc::from("apply")),
            caller_env_id,
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
        let macro_name = require_string(
            "macro-injects",
            macro_name_val,
            ctx.get_thunk(args[0]).span.clone(),
        )?;

        // Look up the macro in the inject map
        let inject_names: &[String] = match ctx.config.macro_injects_map.get(&macro_name) {
            Some(names) => names.as_slice(),
            None => &[],
        };

        // Build an integer-keyed Dict of inject names
        let mut dict = IndexMap::new();
        for (i, name) in inject_names.iter().enumerate() {
            let id = ctx.alloc_thunk(
                0,
                Arc::new(Thunk::value(string_val(name), call_span.clone())),
            );
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
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("pending-builtin"),
                        call_span.clone(),
                    )),
                ),
            );
            entries.insert(
                crate::value::HashableValue::Str("name".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val(def.name),
                        call_span.clone(),
                    )),
                ),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for PendingCall
        if thunk.is_pending_call() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("pending-call"),
                        call_span.clone(),
                    )),
                ),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Guarded
        if thunk.is_guarded() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("thunk"),
                        call_span.clone(),
                    )),
                ),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("guarded"),
                        call_span.clone(),
                    )),
                ),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for InProgress
        if matches!(thunk.state(), crate::value::ThunkState::InProgress { .. }) {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("thunk"),
                        call_span.clone(),
                    )),
                ),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("in-progress"),
                        call_span.clone(),
                    )),
                ),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Failed
        if let Some(err) = thunk.get_cached_error() {
            let mut entries = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("thunk"),
                        call_span.clone(),
                    )),
                ),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val("failed"),
                        call_span.clone(),
                    )),
                ),
            );
            entries.insert(
                crate::value::HashableValue::Str("error".into()),
                ctx.alloc_thunk(
                    0,
                    Arc::new(crate::value::Thunk::value(
                        string_val(&err.kind.to_string()),
                        call_span.clone(),
                    )),
                ),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict(entries),
                call_span,
            )));
        }

        // Check for Surface (runtime-v2: return Expr.* variant)
        if let Some(node) = thunk.peek_surface_node() {
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::surface_convert::surface_node_to_expr_variant(&node, &ctx),
                call_span,
            )));
        }

        // Check for AstNodeField (runtime-v2: return the containing SurfaceNode as Expr.* variant)
        if let Some((node, _field)) = thunk.peek_ast_node_field() {
            return Ok(Arc::new(crate::value::Thunk::value(
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
                    ctx.alloc_thunk(
                        0,
                        Arc::new(crate::value::Thunk::value(
                            string_val("fn"),
                            call_span.clone(),
                        )),
                    ),
                );

                // doc: string or empty string
                let doc_str = annotation
                    .as_ref()
                    .and_then(|a| a.doc.as_deref())
                    .unwrap_or("");
                dict.insert(
                    HashableValue::Str("doc".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(crate::value::Thunk::value(
                            string_val(doc_str),
                            call_span.clone(),
                        )),
                    ),
                );

                // return-ann: annotation dict or empty dict (null)
                let return_ann_tid = match annotation.as_ref().and_then(|a| a.return_ann.as_ref()) {
                    Some(ann) => crate::surface_convert::annotation_to_thunk_id(
                        ann,
                        call_span.clone(),
                        &ctx,
                    )?,
                    None => ctx.alloc_thunk(
                        0,
                        Arc::new(crate::value::Thunk::value(
                            Value::Dict(IndexMap::new()),
                            call_span.clone(),
                        )),
                    ),
                };
                dict.insert(HashableValue::Str("return-ann".into()), return_ann_tid);

                // params: integer-keyed dict of param entry dicts [{name: "x", annotation: ...}, ...]
                let param_tids: Vec<ThunkId> = params
                    .iter()
                    .map(|p| {
                        let mut param_dict = IndexMap::new();
                        param_dict.insert(
                            HashableValue::Str("name".into()),
                            ctx.alloc_thunk(
                                0,
                                Arc::new(crate::value::Thunk::value(
                                    string_val(&p.name),
                                    call_span.clone(),
                                )),
                            ),
                        );
                        if let Some(ann) = &p.annotation {
                            let ann_tid = crate::surface_convert::annotation_to_thunk_id(
                                &ann.node,
                                call_span.clone(),
                                &ctx,
                            )?;
                            param_dict.insert(HashableValue::Str("annotation".into()), ann_tid);
                        }
                        Ok(ctx.alloc_thunk(
                            0,
                            Arc::new(crate::value::Thunk::value(
                                Value::Dict(param_dict),
                                call_span.clone(),
                            )),
                        ))
                    })
                    .collect::<crate::error::EvalResult<Vec<_>>>()?;

                let params_tid = crate::surface_convert::list_to_thunk_id(
                    param_tids.into_iter(),
                    call_span.clone(),
                    &ctx,
                )?;
                dict.insert(HashableValue::Str("params".into()), params_tid);

                return Ok(Arc::new(crate::value::Thunk::value(
                    Value::Dict(dict),
                    call_span,
                )));
            }

            let synthetic_node: Arc<SurfaceNode> = match val {
                crate::value::Value::Int(n) => make_node(SurfaceExpression::Int(n)),
                crate::value::Value::U64(n) => make_node(SurfaceExpression::U64(n)),
                crate::value::Value::Float(f) => make_node(SurfaceExpression::Float(f)),
                crate::value::Value::String { source, start, end } => {
                    make_node(SurfaceExpression::StringLiteral {
                        prefix: String::new(),
                        delimiter: "\"".to_string(),
                        content: source[start..end].to_string(),
                    })
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

            return Ok(Arc::new(crate::value::Thunk::value(
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
            Value::Variant { tycon, ctor, .. } => {
                ok_val(string_val(&format!("{}.{}", tycon, ctor)), call_span)
            }
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
        let program_id = match prog_val {
            Value::Program { id, .. } => id,
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
                        for body_expr in &arm.body {
                            if let Some(r) = find_in_node(body_expr, offset) {
                                return Some(r);
                            }
                        }
                    }
                }
                _ => {}
            }
            None
        }

        let found: Option<(u32, u32)> = ctx.with_program(program_id, |program| {
            let mut found = None;
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
            found
        });

        let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
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
                            ctx.alloc_thunk(
                                0,
                                Arc::new(Thunk::value(string_val(doc_str), call_span.clone())),
                            ),
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
                            ctx.alloc_thunk(
                                0,
                                Arc::new(Thunk::value(string_val(&s), call_span.clone())),
                            ),
                        );
                    }
                    // Flatten all extra fields into the dict (includes evaluated `doc`).
                    // Extra fields overwrite doc inserted above, ensuring the evaluated
                    // version wins for triple-quoted doc strings.
                    for (key, extra_val) in &ann.extra {
                        entries.insert(
                            HashableValue::Str(key.as_str().into()),
                            ctx.alloc_thunk(
                                0,
                                Arc::new(Thunk::value(extra_val.clone(), call_span.clone())),
                            ),
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

        let inner_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by pos_strictness[0]=Seq");
        let ann_val = ctx
            .get_thunk(args[1])
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
        Value::Variant { tycon, ctor, .. } => return format!("{}.{}", tycon, ctor),
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

/// `builtin-scopes`: Return all scopes in the scope arena.
///
/// Takes 0 args. Returns Dict {scope_id_int: parent_id_int_or_empty_dict, ...}
/// For each scope, if scope.parent is Some(id) return Int(id), else return empty dict (null).
pub(crate) fn builtin_scopes(
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
        crate::builtins::reject_named("builtin-scopes", named.as_ref(), call_span.clone())?;
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
        }

        // Collect (scope_id, parent_val) while holding the borrow, then drop before allocating.
        // Allocating thunks requires scope_arena.borrow_mut() via alloc_thunk(); holding
        // the immutable borrow concurrently would panic at runtime.
        let pairs: Vec<(usize, Value)> = {
            let arena = ctx.scope_arena.borrow();
            arena
                .scopes
                .iter()
                .enumerate()
                .map(|(i, scope)| {
                    let parent_val = match scope.parent {
                        Some(parent_id) => Value::Int(parent_id.0 as i64),
                        None => Value::Dict(IndexMap::new()), // null = empty dict
                    };
                    (i, parent_val)
                })
                .collect()
        }; // borrow dropped here
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, parent_val) in pairs {
            let thunk_id =
                ctx.alloc_thunk(0, Arc::new(Thunk::value(parent_val, call_span.clone())));
            result.insert(HashableValue::Int(i as i64), thunk_id);
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `builtin-scope-new`: Create a new scope with the given parent and bindings.
///
/// Takes 2 args:
/// - arg0: Int (parent scope-id) or empty Dict (null = create root, no parent)
/// - arg1: Dict of string-keyed bindings to install
///
/// Non-string keys → type error (raise immediately).
/// Returns Int(new_scope_id).
pub(crate) fn builtin_scope_new(
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
        crate::builtins::reject_named("builtin-scope-new", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // arg0: parent scope-id (Int) or null (empty Dict)
        let parent_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let parent_id_opt: Option<crate::arena::ScopeId> = match parent_val {
            Value::Int(n) => {
                if n < 0 {
                    return Err(EvalError::type_mismatch_ctx(
                        "builtin-scope-new parent scope-id must be non-negative Int".to_string(),
                        "non-negative Int",
                        &format!("{}", n),
                        call_span,
                    )
                    .into());
                }
                let parent_u32 = n as u32;
                // Validate parent scope-id range before taking borrow_mut.
                {
                    let arena = ctx.scope_arena.borrow();
                    let scope_len = arena.scopes.len();
                    if parent_u32 as usize >= scope_len {
                        return Err(EvalError::user_error(
                            format!(
                                "scope-id {} out of range (max {})",
                                parent_u32,
                                scope_len.saturating_sub(1)
                            ),
                            call_span,
                        )
                        .into());
                    }
                } // immutable borrow dropped before borrow_mut below
                Some(crate::arena::ScopeId(parent_u32))
            }
            Value::Dict(ref d) if d.is_empty() => None, // null = no parent
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-scope-new parent".to_string(),
                    "Int or []",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // arg1: Dict of string-keyed bindings
        let bindings_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let bindings_dict = match bindings_val {
            Value::Dict(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-scope-new bindings".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Verify all keys are strings and pre-fetch all thunks BEFORE taking borrow_mut.
        // get_thunk() calls scope_arena.borrow() internally; holding borrow_mut concurrently
        // would panic. Collect (name, Arc<Thunk>) pairs first, then install while holding
        // the mutable borrow.
        let mut binding_thunks: Vec<(String, Arc<Thunk>)> = Vec::with_capacity(bindings_dict.len());
        for (key, thunk_id) in &bindings_dict {
            match key {
                HashableValue::Str(name) => {
                    let thunk = ctx.get_thunk(*thunk_id); // immutable borrow, short-lived
                    binding_thunks.push((name.to_string(), Arc::clone(&thunk)));
                }
                _ => {
                    // Non-string keys (positional/integer entries) have no string name
                    // and cannot become named scope bindings — skip silently.
                }
            }
        }

        // Create the new scope and install bindings under borrow_mut.
        let new_scope_id = {
            let mut arena = ctx.scope_arena.borrow_mut();
            let new_id = match parent_id_opt {
                Some(parent_id) => arena.alloc_child(parent_id, binding_thunks.len()),
                None => arena.alloc_root(binding_thunks.len()),
            };
            for (name, thunk) in binding_thunks {
                let _ = name; // name is on the thunk's span
                arena.push_slot(new_id, thunk);
            }
            new_id
        }; // borrow_mut dropped here

        ok_val(Value::Int(new_scope_id.0 as i64), call_span)
    })
}

/// `builtin-scope-parent`: Return the parent scope-id of a scope.
///
/// Takes 1 arg: scope-id (Int).
/// Returns Int(parent_id) if Some, empty Dict if None.
pub(crate) fn builtin_scope_parent(
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
        crate::builtins::reject_named("builtin-scope-parent", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let scope_id_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let scope_id_n = match scope_id_val {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-scope-parent".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };
        if scope_id_n < 0 {
            return Err(EvalError::type_mismatch_ctx(
                "builtin-scope-parent scope-id must be non-negative Int".to_string(),
                "non-negative Int",
                &format!("{}", scope_id_n),
                call_span,
            )
            .into());
        }
        let scope_id_u32 = scope_id_n as u32;
        let arena = ctx.scope_arena.borrow();
        let scope_len = arena.scopes.len();
        if scope_id_u32 as usize >= scope_len {
            return Err(EvalError::user_error(
                format!(
                    "scope-id {} out of range (max {})",
                    scope_id_u32,
                    scope_len.saturating_sub(1)
                ),
                call_span,
            )
            .into());
        }
        let scope = &arena.scopes[scope_id_u32 as usize];
        let parent_val = match scope.parent {
            Some(parent_id) => Value::Int(parent_id.0 as i64),
            None => Value::Dict(IndexMap::new()), // null = empty dict
        };
        ok_val(parent_val, call_span)
    })
}

/// `builtin-scope-frame`: Return the resolver frame for a single scope level.
///
/// Takes 1 arg: scope-id (Int).
/// Returns Dict {String(name): Int(slot)} for all slots in the scope that carry a name
/// on their thunk's span. Slots without a span name are omitted.
/// Used by the loaders to reconstruct resolver frames from scope chains.
pub(crate) fn builtin_scope_frame(
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
        crate::builtins::reject_named("builtin-scope-frame", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let scope_id_n = match ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness")
        {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-scope-frame".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        if scope_id_n < 0 {
            return Err(EvalError::user_error(
                format!("scope-id must be non-negative, got {}", scope_id_n),
                call_span,
            )
            .into());
        }
        let scope_id_u32 = scope_id_n as u32;
        // Build frame: {name: slot-index} for all named slots.
        // Names come from thunk span.name (set at thunk creation time for blame tracking).
        let pairs: Vec<(String, u32)> = {
            let arena = ctx.scope_arena.borrow();
            let scope_len = arena.scopes.len();
            if scope_id_u32 as usize >= scope_len {
                return Err(EvalError::user_error(
                    format!(
                        "scope-id {} out of range (max {})",
                        scope_id_u32,
                        scope_len.saturating_sub(1)
                    ),
                    call_span,
                )
                .into());
            }
            arena.scopes[scope_id_u32 as usize]
                .slots
                .iter()
                .enumerate()
                .filter_map(|(slot, thunk_opt)| {
                    let name = thunk_opt.as_ref()?.span.name.as_deref()?.to_string();
                    Some((name, slot as u32))
                })
                .collect()
        };
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (name, slot) in pairs {
            let thunk_id = ctx.alloc_thunk(
                0,
                Arc::new(Thunk::value(Value::Int(slot as i64), call_span.clone())),
            );
            result.insert(HashableValue::Str(name.into()), thunk_id);
        }
        ok_val(Value::Dict(result), call_span)
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
/// (desugar + resolve) and then `builtin-typecheck-doc` before passing the result to `builtin-eval`.
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

        // Parse — spans carry the path for error messages via the SourceFile.
        // Fatal parse errors (lexer failure, unclosed brackets) are captured in the
        // errors list rather than raised, so callers can inspect them programmatically.
        let source_file = Arc::new(crate::ast::SourceFile {
            path: Arc::from(path_str.as_str()),
            content: Arc::from(source.as_str()),
        });
        let (parsed, fatal_errors) = match crate::parser::parse(&source, Arc::clone(&source_file)) {
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
        let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
        let mut errors_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, pe) in all_parse_errors.iter().enumerate() {
            let err_id = parse_error_to_dict(pe, &ctx, &call_span);
            errors_dict.insert(HashableValue::Int(i as i64), err_id);
        }

        // Build the program value. If the parse was fatal, produce an empty program.
        let surface_program = if let Some(output) = parsed {
            output.program
        } else {
            // Fatal parse: return empty program so {program, errors} is always usable.
            crate::ast::SurfaceProgram { documents: vec![] }
        };
        let store_id = ctx.push_program(surface_program);
        let program_value = Value::Program {
            id: store_id,
            resolutions: std::sync::Arc::new(Default::default()),
            types: std::sync::Arc::new(Default::default()),
            expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
        };

        // Return {program: Value::Program, errors: integer-keyed Dict of error dicts}.
        let program_id =
            ctx.alloc_thunk(0, Arc::new(Thunk::value(program_value, call_span.clone())));
        let errors_id = alloc(Value::Dict(errors_dict));
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        result.insert(HashableValue::Str("program".into()), program_id);
        result.insert(HashableValue::Str("errors".into()), errors_id);
        Ok(Arc::new(Thunk::value(Value::Dict(result), call_span)))
    })
}

/// `builtin-resolve`: Resolve a single `Value::Document` in-place.
///
/// Takes 2 positional args: the document to resolve and a frames dict.
///
/// **Arguments:**
/// - arg0: `Value::Document` (only Document, not Program — error if Program)
/// - arg1: Dict — frames in format `{0: {name-a: 0, name-b: 1}, 1: {name-c: 5}, ...}`
///   - Outer dict: integer-keyed, each entry is one scope frame
///   - Inner dict: String key → Int value as (name, slot)
///
/// **Returns** `{doc: Document, errors: Dict<Int, ErrorDict>}`.
///
/// Writes De Bruijn coordinates into the inline `Resolution` OnceLocks on each `VarRef`/`DotAccess`
/// node of the document's AST. After this call, `builtin-eval` lowers the resolved nodes directly.
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
        crate::builtins::reject_named("builtin-resolve", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // arg0: Document — TypeAssert on the parameter enforces this at the call boundary.
        let doc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let doc_arc = if let Value::Document(d) = doc_val {
            d
        } else {
            return Err(EvalError::internal(
                format!("expected Document, got {}", doc_val.type_name()),
                call_span,
            )
            .into());
        };

        // arg1: Map[Int, Map[String, Int]] — structure enforced by TypeAssert on frames parameter.
        let frames_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let frames_dict = match frames_val {
            Value::Dict(d) => d,
            other => {
                return Err(EvalError::internal(
                    format!("frames is not a Dict: {}", other.type_name()),
                    call_span,
                )
                .into())
            }
        };

        // Sort frame entries by integer key so outermost (lowest index) comes first.
        let mut frame_entries: Vec<(i64, ThunkId)> = frames_dict
            .iter()
            .map(|(k, v)| match k {
                HashableValue::Int(i) => (*i, *v),
                other => panic!("frames outer key is not Int: {:?}", other),
            })
            .collect();
        frame_entries.sort_by_key(|(i, _)| *i);

        let mut initial_frames: Vec<indexmap::IndexMap<String, u32>> = Vec::new();
        for (_, frame_tid) in frame_entries {
            let frame_val =
                crate::eval::materialize(&ctx.get_thunk(frame_tid), Some(&call_span), &ctx).await?;
            let inner = match frame_val {
                Value::Dict(d) => d,
                other => {
                    return Err(EvalError::internal(
                        format!("frame entry is not a Dict: {}", other.type_name()),
                        call_span,
                    )
                    .into())
                }
            };
            let mut frame: indexmap::IndexMap<String, u32> = indexmap::IndexMap::new();
            for (k, v_tid) in &inner {
                let name = match k {
                    HashableValue::Str(s) => s,
                    other => panic!("frame inner key is not Str: {:?}", other),
                };
                let slot_val =
                    crate::eval::materialize(&ctx.get_thunk(*v_tid), Some(&call_span), &ctx)
                        .await?;
                let slot = match slot_val {
                    Value::Int(n) => n,
                    other => {
                        return Err(EvalError::internal(
                            format!("slot index is not Int: {}", other.type_name()),
                            call_span,
                        )
                        .into())
                    }
                };
                frame.insert(name.to_string(), slot as u32);
            }
            initial_frames.push(frame);
        }

        // Resolve the document in-place.
        // _resolve_table: the full ResolutionTable mapping spans to de Bruijn coords. Intentionally
        //   discarded — the evaluator uses inline OnceLock resolutions written directly onto each
        //   SurfaceNode::VarRef during the resolve walk (see lower.rs: resolution.get()). The
        //   ResolutionTable is not read by the evaluator.
        // _new_frames: the frames produced by this document's declarations (type aliases, etc.).
        //   Intentionally discarded — the new scope-id returned by builtin-eval captures the
        //   document's bindings in the ScopeArena, and tinct callers use scope-to-frames to
        //   rebuild frames from the scope chain for the next document.
        let (_resolve_table, resolve_errors, _new_frames) =
            crate::resolve::resolve_surface_document_inplace(&doc_arc, &initial_frames);

        // Build errors dict from resolve_errors (undefined variables).
        let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
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

        // Return {doc: Value::Document, errors: Dict} — NO scope-frames
        let doc_thunk_id = ctx.alloc_thunk(
            0,
            Arc::new(Thunk::value(
                Value::Document(std::sync::Arc::clone(&doc_arc)),
                call_span.clone(),
            )),
        );
        let errors_thunk_id = ctx.alloc_thunk(
            0,
            Arc::new(Thunk::value(Value::Dict(errors_dict), call_span.clone())),
        );
        let mut result_dict = indexmap::IndexMap::new();
        result_dict.insert(HashableValue::Str("doc".into()), doc_thunk_id);
        result_dict.insert(HashableValue::Str("errors".into()), errors_thunk_id);
        ok_val(Value::Dict(result_dict), call_span)
    })
}

// builtin_typecheck removed — use builtin_typecheck_doc for per-document type-checking

/// `builtin-typecheck-doc`: Type-check a single resolved `Value::Document`.
///
/// Takes 2 args:
/// - arg0: Value::Document (already resolved)
/// - arg1: Value::TypeContext
///
/// Returns Value::Document (same Arc — type annotations written inline).
pub(crate) fn builtin_typecheck_doc(
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
        crate::builtins::reject_named("builtin-typecheck-doc", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // arg0: Value::Document
        let doc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let doc_arc = match doc_val {
            Value::Document(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-typecheck-doc".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // arg1: Value::TypeContext
        let tc_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let tc_arc = match tc_val {
            Value::TypeContext(arc) => arc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-typecheck-doc".to_string(),
                    "TypeContext",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Extract TypeContext state
        let (mut state, mut type_map, parent_env) = {
            let guard = tc_arc.lock().unwrap();
            let mut state = crate::types::InferState::new();
            state.tycon_env = guard.tycon_env.clone();
            state.env = Arc::clone(&guard.inference_env);
            state.eval_ctx = Some(Arc::clone(&ctx));

            // Build type_stage_map from the type-stage scope chain.
            // Walk all scopes from root to leaf at type_stage_scope_id. For each slot whose
            // thunk carries a span name (set by eval_dict_core for string-keyed entries),
            // record its ThunkId. Innermost scope entries override outer ones (dict overrides).
            // Seed with Unknown → Resolved(Unknown) so @Unknown always resolves.
            let mut ts_map = std::collections::HashMap::new();
            ts_map.insert(
                "Unknown".to_string(),
                crate::type_infer::TypeStageEntry::Resolved(crate::types::Type::Unknown),
            );
            if let Some(scope_id) = guard.type_stage_scope_id {
                let arena = ctx.scope_arena.borrow();
                let chain = arena.collect_parent_chain(scope_id);
                for sid in &chain {
                    for (slot, thunk_opt) in
                        arena.scopes[sid.0 as usize].slots.iter().enumerate()
                    {
                        if let Some(thunk) = thunk_opt {
                            if let Some(name) = thunk.span.name.as_deref() {
                                let thunk_id = crate::arena::ThunkId {
                                    scope_id: sid.0,
                                    slot: slot as u32,
                                };
                                ts_map.insert(
                                    name.to_string(),
                                    crate::type_infer::TypeStageEntry::Function(thunk_id),
                                );
                            }
                        }
                    }
                }
            }
            state.type_stage_map = Some(ts_map);

            let type_map = crate::ast::TypeAnnotationTable::new();
            let parent_env = Arc::clone(&guard.inference_env);
            (state, type_map, parent_env)
        };

        // process_document processes all items in source order, extends env with schemes from
        // the last dict body, and returns (doc_env, result_type, errors).
        let (doc_env, _, errors) = crate::typecheck::process_document(
            &doc_arc,
            &parent_env,
            &mut state,
            &mut type_map,
            &mut None,
        )
        .await;

        // Write results back to TypeContext:
        // - tycon_env: new type constructor definitions from this document
        // - inference_env: the accumulated doc_env so subsequent documents see this document's
        //   type schemes (Hindley-Milner accumulation across files per TypeContextData doc comment)
        {
            let mut guard = tc_arc.lock().unwrap();
            for (name, def) in &state.tycon_env {
                guard
                    .tycon_env
                    .entry(name.clone())
                    .or_insert_with(|| Arc::clone(def));
            }
            guard.inference_env = doc_env;
        }

        // Return {doc: Document, warnings: {0: {message, span, ...}, ...}}
        // The init program decides how to handle type errors — they are never silently dropped.
        let alloc = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
        let mut warnings_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, err) in errors.iter().enumerate() {
            let span_id = make_span_dict(&err.span, &ctx, &call_span);
            let mut w: IndexMap<HashableValue, ThunkId> = IndexMap::new();
            w.insert(
                HashableValue::Str("kind".into()),
                alloc(string_val("type-error")),
            );
            w.insert(
                HashableValue::Str("message".into()),
                alloc(string_val(&err.message)),
            );
            w.insert(HashableValue::Str("span".into()), span_id);
            let mut notes_dict: IndexMap<HashableValue, ThunkId> = IndexMap::new();
            for (j, note) in err.notes.iter().enumerate() {
                notes_dict.insert(HashableValue::Int(j as i64), alloc(string_val(note)));
            }
            w.insert(
                HashableValue::Str("notes".into()),
                alloc(Value::Dict(notes_dict)),
            );
            // call-stack, macro-expand, blame: empty dicts per the diagnostic dict protocol.
            // loader.llt and test-loader.llt always access these keys; missing keys crash with
            // "key not found: call-stack".
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
            warnings_dict.insert(HashableValue::Int(i as i64), alloc(Value::Dict(w)));
        }
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        result.insert(
            HashableValue::Str("doc".into()),
            alloc(Value::Document(doc_arc)),
        );
        result.insert(
            HashableValue::Str("warnings".into()),
            alloc(Value::Dict(warnings_dict)),
        );
        ok_val(Value::Dict(result), call_span)
    })
}

/// `builtin-get-type-context`: Return a clone of the EvalContext's TypeContext.
///
/// Returns a clone of the TypeContextData initialized by the Rust bootstrap (main.rs).
/// Errors if the TypeContext was not initialized before this call.
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
                // TypeContext must be initialized by main.rs before any tinct code runs.
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
/// Creates a `TypeContextData` and installs it on the current `EvalContext`. Returns a
/// `Value::TypeContext` handle wrapping the same `Arc<Mutex<TypeContextData>>` stored on
/// `EvalContext` — so both the returned handle and the EvalContext share the same mutable state.
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
            type_stage_scope_id: None,
            inference_env: crate::imports::get_builtin_core_type_env()
                .await
                .expect("builtin_core type env unavailable"),
            tycon_env: std::collections::HashMap::new(),
            type_errors: Vec::new(),
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
        let parent_val = ctx
            .get_thunk(args[0])
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

/// `builtin-tc-with-scope`: Registers the scope-id of type-stage function thunks in the TypeContext.
///
/// Takes 2 positional args (both forced):
///   - arg 0: `Value::TypeContext` — the TypeContext to update
///   - arg 1: `Value::Int` — the scope-id produced by evaluating type-stage documents
///
/// Locks the TypeContext mutex and records the scope-id for type-stage scope-chain lookup.
/// Returns the **same** `Value::TypeContext` value (the mutation is in-place).
///
/// Used by loader.llt and test-loader.llt to wire the type-stage scope into the TypeContext
/// before type-checking.
///
/// Signature: `[builtin-tc-with-scope type-ctx ts-scope-id]`
/// where `ts-scope-id` is the Int returned by the final scope-id from type-stage evaluation.
pub(crate) fn builtin_tc_with_scope(
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
        crate::builtins::reject_named("builtin-tc-with-scope", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }
        let tc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let scope_id_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");

        let tc_arc = match tc_val {
            Value::TypeContext(arc) => arc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-with-scope".to_string(),
                    "TypeContext",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // arg 1: scope-id as Int
        let scope_id = match scope_id_val {
            Value::Int(n) => n as u32,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-with-scope".to_string(),
                    "Int (scope-id from type-stage evaluation)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Update TypeContext: record scope-id for type-stage scope-chain lookup.
        {
            let mut guard = tc_arc.lock().unwrap();
            guard.type_stage_scope_id = Some(scope_id);
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

        let val = ctx
            .get_thunk(args[0])
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
                            // T-1603: Use Arc::clone to preserve OnceLocks (resolver coordinates).
                            // Deep clone would create a new SurfaceDocument with empty OnceLocks,
                            // breaking resolution. Arc::clone just increments refcount.
                            documents.push(crate::ast::Spanned {
                                node: Arc::clone(&surface_doc),
                                span: rust_span!(),
                            });
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

        // Construct the SurfaceProgram and push it into the program store.
        let surface_program = crate::ast::SurfaceProgram { documents };
        let store_id = ctx.push_program(surface_program);

        // Return as Value::Program with empty tables (caller can run expand/resolve if needed)
        ok_val(
            Value::Program {
                id: store_id,
                resolutions: std::sync::Arc::new(Default::default()),
                types: std::sync::Arc::new(Default::default()),
                expects_resolved: std::sync::Arc::new(std::collections::HashMap::new()),
            },
            call_span,
        )
    })
}

/// `builtin-desugar`: Desugar a `Value::Program` in-place.
///
/// Thin wrapper around `desugar_surface_program`. Takes 1 arg: Value::Program
/// Returns Value::Program with desugaring applied.
pub(crate) fn builtin_desugar(
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
        crate::builtins::reject_named("builtin-desugar", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let program_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let (program_id, resolutions, types, expects_resolved) = match program_val {
            Value::Program {
                id,
                resolutions,
                types,
                expects_resolved,
            } => (id, resolutions, types, expects_resolved),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-desugar".to_string(),
                    "Program",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // ORDERING: builtin-desugar MUST be called before builtin-program-docs.
        // Calling builtin-program-docs first shares Arc<SurfaceDocument> references from the
        // program store; subsequent desugar passes call Arc::get_mut() which panics on shared Arcs.
        // Correct call order in the pipeline: builtin-desugar → builtin-program-docs → builtin-resolve.
        //
        // Mutate the program in-place in the program_store. The store holds the unique
        // SurfaceProgram; desugar runs before any Arc<SurfaceDocument> sharing (program-docs
        // is called after desugar in the pipeline), so Arc::get_mut succeeds inside the
        // desugar passes. Correct order: instance decl transformation must run first,
        // then $_ desugaring and pipe lowering.
        // Verify all document Arcs are exclusively owned before mutating.
        // If builtin-program-docs was called first, document Arcs are shared (refcount > 1)
        // and the desugar passes' Arc::get_mut() calls would panic. Return EvalError instead.
        let all_exclusive = ctx.with_program(program_id, |program| {
            program
                .documents
                .iter()
                .all(|doc| std::sync::Arc::strong_count(&doc.node) == 1)
        });
        if !all_exclusive {
            return Err(EvalError::user_error(
                "builtin-desugar: cannot desugar after builtin-program-docs was called \
                 (document Arcs are shared). Call builtin-desugar before builtin-program-docs."
                    .to_string(),
                call_span,
            )
            .into());
        }
        ctx.with_program_mut(program_id, |program| {
            crate::desugar::desugar_instance_decls_surface_program(program);
            crate::desugar::desugar_surface_program(program);
        });

        ok_val(
            Value::Program {
                id: program_id,
                resolutions,
                types,
                expects_resolved,
            },
            call_span,
        )
    })
}

/// `builtin-program-docs`: Extract documents from a `Value::Program`.
///
/// Thin wrapper to access `SurfaceProgram.documents`. Takes 1 arg: Value::Program
/// Returns auto-indexed Dict of Value::Document values (Arc::clone each doc).
pub(crate) fn builtin_program_docs(
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
        crate::builtins::reject_named("builtin-program-docs", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let program_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let program_id = match program_val {
            Value::Program { id, .. } => id,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-program-docs".to_string(),
                    "Program",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Collect Arc::clone handles while the store borrow is held, then release it
        // before allocating thunks (alloc_thunk re-borrows scope_arena, not program_store).
        let doc_arcs: Vec<(usize, Arc<crate::ast::SurfaceDocument>)> =
            ctx.with_program(program_id, |program| {
                program
                    .documents
                    .iter()
                    .enumerate()
                    .map(|(i, spanned)| (i, Arc::clone(&spanned.node)))
                    .collect()
            });
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (i, doc_arc) in doc_arcs {
            let doc_thunk = Arc::new(Thunk::value(Value::Document(doc_arc), call_span.clone()));
            result.insert(HashableValue::Int(i as i64), ctx.alloc_thunk(0, doc_thunk));
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `builtin-doc-expressions`: Return expression items from a document.
///
/// Takes 1 arg: Value::Document.
///
/// Returns an auto-indexed Dict of Expr-variant values, one per SurfaceItem::Expr
/// in the document. SurfaceItem::Decl items are skipped. The Expr-variant format
/// is the same as produced by `surface_node_to_expr_variant`, which is what
/// `json-expression` in json.llt consumes.
///
/// This replaces the `Value::Document.expressions` field-get backdoor that was
/// deleted from builtins_dict.rs (T-1605 follow-up, S-926 R4).
pub(crate) fn builtin_doc_expressions(
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
            "builtin-doc-expressions",
            named.as_ref(),
            call_span.clone(),
        )?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let doc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let doc_arc = match doc_val {
            Value::Document(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-doc-expressions".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        let mut i = 0usize;
        for item in &doc_arc.items {
            if let crate::ast::SurfaceItem::Expr(node) = item {
                let expr_val = crate::surface_convert::surface_node_to_expr_variant(node, &ctx);
                let id = ctx.alloc_thunk(0, Arc::new(Thunk::value(expr_val, call_span.clone())));
                result.insert(HashableValue::Int(i as i64), id);
                i += 1;
            }
        }
        ok_val(Value::Dict(result), call_span)
    })
}

/// `builtin-doc-meta`: Evaluate document header metadata.
///
/// Evaluates the header values of a document in the given scope. Takes 2 args:
/// - arg0: Value::Document
/// - arg1: Int (scope-id for evaluating header values)
///
/// Returns Dict {key: evaluated-value, ...}.
/// Returns empty Dict if header is empty.
pub(crate) fn builtin_doc_meta(
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
        crate::builtins::reject_named("builtin-doc-meta", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let doc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let doc_arc = match doc_val {
            Value::Document(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-doc-meta".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        let scope_id_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by force_count/pos_strictness");
        let scope_id = match scope_id_val {
            Value::Int(n) => {
                if n < 0 {
                    return Err(EvalError::type_mismatch_ctx(
                        "builtin-doc-meta scope-id must be non-negative Int".to_string(),
                        "non-negative Int",
                        &format!("{}", n),
                        call_span,
                    )
                    .into());
                }
                let u = n as u32;
                let scope_len = ctx.scope_arena.borrow().scopes.len();
                if u as usize >= scope_len {
                    return Err(EvalError::user_error(
                        format!(
                            "scope-id {} out of range (max {})",
                            u,
                            scope_len.saturating_sub(1)
                        ),
                        call_span,
                    )
                    .into());
                }
                Some(u)
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-doc-meta scope-id".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Evaluate each header SurfaceNode value in the given scope and collect results.
        let mut result: IndexMap<HashableValue, ThunkId> = IndexMap::new();
        for (key, node_arc) in &doc_arc.header {
            let nodes = vec![Arc::clone(node_arc)];
            let eval_result =
                crate::eval::eval_document_exprs_with_env(&nodes, &ctx, scope_id).await;
            match eval_result {
                Ok((thunk, _)) => {
                    result.insert(
                        HashableValue::Str(key.clone().into()),
                        ctx.alloc_thunk(0, thunk),
                    );
                }
                Err(e) => return Err(e),
            }
        }
        ok_val(Value::Dict(result), call_span)
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
                    let builtin_thunk =
                        Arc::new(Thunk::value(Value::Builtin(def), call_span.clone()));
                    let thunk_id = ctx.alloc_thunk(0, builtin_thunk);
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

/// `builtin-eval`: evaluate a `Value::Document` in a given scope.
///
/// Takes 2 positional args: the typed document and the parent scope-id.
///
/// Positional args:
/// - arg0: `Value::Document` ONLY (Expr.* Dict path DELETED)
/// - arg1: Int (scope-id — becomes parent of the document's internal scope)
///
/// Returns: `Value::Dict` with keys:
/// - `result:` — last expression's thunk (on success)
/// - `scope-id:` (`Value::Int`) — the NEW child scope-id created for this document
/// - `errors:` (`Value::Dict([])` = null on success, `Value::Dict({0: String(message)})` on failure)
///
/// On the error path, `scope-id` returns the unchanged input parent scope-id.
///
/// Callers MUST check `result.errors` before using `result.scope-id`. This design
/// ensures Rust never prints errors — tinct code receives errors as data.
/// Check for errors with `[null? ev.errors]` — empty dict (null) means success,
/// non-empty dict means failure.
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
        crate::builtins::reject_named("builtin-eval", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // arg0: Value::Document ONLY
        let doc_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let doc_arc = match doc_val {
            Value::Document(d) => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-eval".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // arg1: Int (scope-id)
        let scope_id_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let scope_id = match scope_id_val {
            Value::Int(n) => {
                let arena_len = ctx.scope_arena.borrow().scopes.len();
                if n < 0 || (n as usize) >= arena_len {
                    return Err(EvalError::user_error(
                        format!(
                            "builtin-eval scope-id {} out of range (valid: 0..{})",
                            n,
                            arena_len.saturating_sub(1)
                        ),
                        call_span,
                    )
                    .into());
                }
                n as u32
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-eval scope-id".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Extract expression nodes from the document
        let expression_nodes: Vec<std::sync::Arc<crate::ast::SurfaceNode>> = doc_arc
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

        // Evaluate the expression sequence
        let eval_result =
            crate::eval::eval_document_exprs_with_env(&expression_nodes, &ctx, Some(scope_id))
                .await;

        // Build result dict: {result, scope-id, errors}
        let mut result_map: indexmap::IndexMap<HashableValue, ThunkId> = indexmap::IndexMap::new();
        match eval_result {
            Ok((result_thunk, new_scope_id)) => {
                result_map.insert(
                    HashableValue::Str("result".into()),
                    ctx.alloc_thunk(0, result_thunk),
                );
                result_map.insert(
                    HashableValue::Str("scope-id".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(
                            Value::Int(new_scope_id as i64),
                            call_span.clone(),
                        )),
                    ),
                );
                result_map.insert(
                    HashableValue::Str("errors".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(
                            Value::Dict(indexmap::IndexMap::new()), // null = success
                            call_span.clone(),
                        )),
                    ),
                );
            }
            Err(e) => {
                let msg = format!("{}", e);
                result_map.insert(
                    HashableValue::Str("result".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(
                            Value::Dict(indexmap::IndexMap::new()), // null on error
                            call_span.clone(),
                        )),
                    ),
                );
                result_map.insert(
                    HashableValue::Str("scope-id".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(
                            Value::Int(scope_id as i64), // unchanged input parent scope-id
                            call_span.clone(),
                        )),
                    ),
                );
                // errors: integer-keyed Dict<Int, String> matching builtin-parse/builtin-resolve schema.
                // {0: String(message)} — non-null Dict so callers can check [null? ev.errors].
                let mut errors_map: indexmap::IndexMap<HashableValue, ThunkId> =
                    indexmap::IndexMap::new();
                errors_map.insert(
                    HashableValue::Int(0),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(string_val(&msg), call_span.clone())),
                    ),
                );
                result_map.insert(
                    HashableValue::Str("errors".into()),
                    ctx.alloc_thunk(
                        0,
                        Arc::new(Thunk::value(Value::Dict(errors_map), call_span.clone())),
                    ),
                );
            }
        }

        Ok(Arc::new(Thunk::value(Value::Dict(result_map), call_span)))
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
        let val = ctx
            .get_thunk(args[0])
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

/// `builtin-eval-repr`: evaluate a document in a scope and return `builtin-llt-repr` of the
/// last expression's result. Combines `builtin-eval` + `builtin-llt-repr` atomically.
///
/// Positional arg[0]: Value::Document — the typed document to evaluate.
/// Positional arg[1]: Value::Int — the scope-id to evaluate in (same as builtin-eval).
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

        crate::builtins::reject_named("eval-repr", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let input_val = ctx
            .get_thunk(args[0])
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

        let scope_id_val = ctx
            .get_thunk(args[1])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");
        let scope_id: Option<u32> = match scope_id_val {
            Value::Int(n) => {
                let arena_len = ctx.scope_arena.borrow().scopes.len();
                if n < 0 || (n as usize) >= arena_len {
                    return Err(EvalError::user_error(
                        format!(
                            "builtin-eval-repr scope-id {} out of range (valid: 0..{})",
                            n,
                            arena_len.saturating_sub(1)
                        ),
                        call_span,
                    )
                    .into());
                }
                Some(n as u32)
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "eval-repr scope-id".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        let (result_thunk, _root_env_id) =
            crate::eval::eval_document_exprs_with_env(&expression_nodes, &ctx, scope_id).await?;

        let result_val = materialize(&result_thunk, Some(&call_span), &ctx).await?;
        let repr = crate::value_to_display_string(&result_val, &ctx, call_span.clone())
            .await
            .map_err(|e| {
                EvalError::internal(format!("eval-repr: {}", e.kind), call_span.clone())
            })?;
        ok_val(string_val(&repr), call_span)
    })
}

/// `builtin-current-env`: capture and return the calling FlatEnv id.
///
/// Takes zero arguments. Returns `Value::Int(caller_env_id)` — the FlatEnv id of the
/// caller's evaluation scope. This is the env_id in scope at the `[builtin-current-env]`
/// call site.
pub(crate) fn builtin_current_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            caller_env_id,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("current-env", named.as_ref(), call_span.clone())?;
        if !args.is_empty() {
            return Err(EvalError::arity_mismatch(0, args.len(), call_span).into());
        }
        Ok(Arc::new(Thunk::value(
            Value::Int(caller_env_id as i64),
            call_span,
        )))
    })
}

/// `builtin-eval-macro-ast`: evaluate a macro-produced AST in the call-site scope.
///
/// Takes 1 positional arg: an `Expr.*` Value::Variant representing the AST to evaluate.
///
/// The call-site FlatEnv id is provided directly via `BuiltinArgs.caller_env_id`, which is
/// injected by the @Expr PendingCallDispatch handler in eval_materialize.rs.  This allows
/// macro functions to call `[eval-macro-ast ast]` without explicitly threading the call
/// environment through their parameter lists.
///
/// Evaluation pipeline:
///   1. Use `caller_env_id` directly as the call-site FlatEnv id
///   2. Convert the `Expr.*` variant to a `SurfaceNode` using `dict_to_surface_node`
///   3. Wrap in a single-expression `SurfaceProgram`
///   4. Desugar + resolve in the call-site env
///   5. Evaluate via `eval_document_exprs_with_env`
///   6. Return the result thunk (the `%` of the resulting environment)
pub(crate) fn builtin_eval_macro_ast(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named: _,
            call_span,
            ctx,
            caller_env_id,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // ── Step 1: Obtain call-site FlatEnv id ───────────────────────────────
        //
        // The @Expr PendingCallDispatch handler in eval_materialize.rs injects the
        // call-site env id directly via BuiltinArgs.caller_env_id. bind_args_thunks
        // (eval_call.rs BIND-SYSTEM) skips names containing '∷' — they are never
        // stored in the FlatEnv — so caller_env_id is the sole authoritative source.
        let call_site_env_id: u32 = caller_env_id;

        // ── Step 2: Convert Expr.* variant → SurfaceNode ─────────────────────
        let expr_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        let expr_node = match expr_val {
            Value::Variant { ref tycon, .. } if tycon == "Expr" => {
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
            header: indexmap::IndexMap::new(),
            items: vec![crate::ast::SurfaceItem::Expr(expr_node)],
        };
        let mut program = crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_span.clone(),
            )],
        };

        // ── Step 4: Desugar + resolve in the call-site environment ────────────
        crate::desugar::desugar_surface_program(&mut program);
        // Seed resolver from the full parent chain of call_site_env_id so that all names
        // visible at the macro call site (builtins, prelude, user-defined) resolve to
        // de Bruijn coordinates rather than falling back to name-based lookup via MAX/MAX.
        // Using the FlatEnv parent chain here is correct for macros: they are generated at
        // runtime and need to see the same lexical scope as the call site.
        let initial_frames: Vec<indexmap::IndexMap<String, u32>> = {
            let arena = ctx.scope_arena.borrow();
            let chain = arena.collect_parent_chain(call_site_env_id);
            chain
                .iter()
                .map(|&id| {
                    arena.scopes[id.0 as usize]
                        .slots
                        .iter()
                        .enumerate()
                        .filter_map(|(slot, thunk_opt)| {
                            let name = thunk_opt.as_ref()?.span.name.as_deref()?.to_string();
                            Some((name, slot as u32))
                        })
                        .collect::<indexmap::IndexMap<String, u32>>()
                })
                .collect()
        };
        let (_resolve_table, _new_frames) =
            crate::resolve::resolve_surface_program(&program, &initial_frames);

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
        let (result_thunk, _root_env_id) = crate::eval::eval_document_exprs_with_env(
            &expression_nodes,
            &ctx,
            Some(call_site_env_id),
        )
        .await?;

        Ok(result_thunk)
    })
}

/// `eval-types`: same as `eval` but evaluates in the type-stage environment.
///
/// This is used for evaluating type-level expressions (type aliases, class declarations).
/// Base environment: the type_stage_scope_id from the current TypeContext.
pub(crate) fn builtin_eval_types(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>>>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            caller_env_id,
            ctx,
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // Reject all named args — eval-types takes only the positional document arg.
        crate::builtins::reject_named("eval-types", named.as_ref(), call_span.clone())?;

        // Get type_stage_scope_id from the TypeContext.
        // This is the scope populated by the type-stage evaluation pass.
        let type_stage_scope_id: Option<u32> = {
            let tc_guard = ctx.type_context.lock().unwrap();
            tc_guard.as_ref().and_then(|tc| tc.type_stage_scope_id)
        };

        // Materialize the input — accepts Value::Document only.
        let input_val = ctx
            .get_thunk(args[0])
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
            let (result_thunk, _root_env_id) =
                crate::eval::eval_document_exprs_with_env(&expr_nodes, &ctx, type_stage_scope_id)
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
                Value::Variant { ref tycon, .. } if tycon == "Expr" => {
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
            let surface_thunk = Arc::new(Thunk::surface(
                node,
                Arc::new(std::collections::HashMap::new()),
                Arc::new(std::collections::HashMap::new()),
                caller_env_id,
                Arc::clone(&ctx),
                call_span.clone(),
            ));
            let surface_thunk_id = ctx.alloc_thunk(0, surface_thunk);
            result_dict.insert(HashableValue::Int(i as i64), surface_thunk_id);
        }

        ok_val(Value::Dict(result_dict), call_span)
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
                let schema_thunk_id =
                    ctx.alloc_thunk(0, Arc::new(Thunk::value(schema.clone(), call_span.clone())));
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
            Ok(Arc::new(Thunk::value(data, call_span)))
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

    let val1 = ctx
        .get_thunk(args[0])
        .try_get_materialized()
        .expect("pre-materialized by force_count/pos_strictness");
    let val2 = ctx
        .get_thunk(args[1])
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
                                matches!(&req_val, Value::Bool(true))
                                    || matches!(&req_val, Value::Int(n) if *n != 0)
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
            Value::Variant { tycon, ctor, .. } if tycon == "TypeNode" && ctor == "RecursiveRef" => {
                false
            }

            // Case 3: Union and Intersect are non-guarding — recurse into all children.
            Value::Variant {
                tycon,
                ctor,
                payload,
            } if tycon == "TypeNode" && (ctor == "Union" || ctor == "Intersect") => {
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
                Value::Variant { ref tycon, .. } if tycon == "Expr" => {
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
        let expr_val = ctx
            .get_thunk(args[0])
            .try_get_materialized()
            .expect("pre-materialized by Strictness::Seq");

        // Convert Expr.* Variant to SurfaceNode
        let expr_node = match expr_val {
            Value::Variant { ref tycon, .. } if tycon == "Expr" => {
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
            header: indexmap::IndexMap::new(),
            items: vec![crate::ast::SurfaceItem::Expr(expr_node)],
        };

        let program = crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_site_span_actual.clone(),
            )],
        };

        // Push into program store and return Value::Program { id }.
        let store_id = ctx.push_program(program);

        // Return Value::Program
        ok_val(
            Value::Program {
                id: store_id,
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
                Value::Variant { ref tycon, .. } if tycon == "Boolean"
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
/// - arg 1: Int — scope-id to search (walks parent chain via ScopeArena)
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
            Value::Int(scope_id) => {
                // Look up the name in the ScopeArena parent chain slot_names.
                let arena = ctx.scope_arena.borrow();
                let env_id = scope_id as u32;
                if (env_id as usize) < arena.scopes.len() {
                    let chain = arena.collect_parent_chain(env_id);
                    chain.iter().any(|eid| {
                        arena.scopes[eid.0 as usize].slots.iter().any(|t| {
                            t.as_ref()
                                .and_then(|t| t.span.name.as_deref())
                                .map_or(false, |n| n == name)
                        })
                    })
                } else {
                    false
                }
            }
            _ => false,
        };

        let (tycon, ctor) = if found {
            ("Boolean", "True")
        } else {
            ("Boolean", "False")
        };
        ok_val(
            Value::Variant {
                tycon: tycon.to_string(),
                ctor: ctor.to_string(),
                payload: None,
            },
            call_span,
        )
    })
}

/// `builtin-arena-new`: create a named evaluation scope (arena).
///
/// Takes 1 positional arg (String name). Returns `Value::Arena { name, start_env_id }`.
/// The arena tracks a named scope starting at `start_env_id`; the dynamic end is always
/// `scopes.len()` at drop/migrate time. Scope is created in the `ScopeArena` with
/// zero initial slot capacity.
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
            Value::String {
                ref source,
                start,
                end,
            } => source[start..end].into(),
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
        let start_env_id = ctx.scope_arena.borrow_mut().alloc_root(0);
        Ok(Arc::new(Thunk::value(
            Value::Arena {
                name,
                start_env_id: start_env_id.0,
            },
            call_span,
        )))
    })
}

/// `builtin-arena-drop`: drop all scopes in the arena range, freeing all their thunks.
///
/// Takes 1 positional arg (`Value::Arena`). Clears all slots in every FlatEnv in the range
/// [start_env_id, envs.len()), releasing all `Arc<Thunk>` references. Returns empty dict `[]`.
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
        let start_env_id = match arena_val {
            Value::Arena { start_env_id, .. } => start_env_id,
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
        // Drop all scopes from start_env_id up to the current arena length.
        // The dynamic end is always envs.len() — this captures all FlatEnvs allocated
        // during builtin-eval that belong to this arena. This matches how
        // builtin_arena_migrate computes its src_end.
        //
        // Stack-top contract: the arena being dropped must be the most recently allocated
        // arena; no other arenas may have been created after this one. For the standard
        // usage pattern (arena-new → builtin-eval → arena-migrate → arena-drop), this
        // always holds. The debug_assert below catches violations in debug builds.
        let end_env_id = ctx.scope_arena.borrow().scopes.len() as u32;
        debug_assert!(
            end_env_id >= start_env_id,
            "arena-drop: end_env_id ({end_env_id}) < start_env_id ({start_env_id}) — double-drop or corrupt arena handle"
        );
        let mut arena_mut = ctx.scope_arena.borrow_mut();
        for eid in start_env_id..end_env_id {
            arena_mut.drop_scope(crate::arena::ScopeId(eid));
        }
        // Truncate the arena vec to free the empty FlatEnv shells.
        // LIFO invariant: start_env_id is the stack top, so everything from
        // start_env_id onward belongs to this arena and can be reclaimed.
        arena_mut.scopes.truncate(start_env_id as usize);
        Ok(Arc::new(Thunk::value(
            Value::Dict(indexmap::IndexMap::new()),
            call_span,
        )))
    })
}

/// `builtin-arena-stats`: return stats dict for an arena scope.
///
/// Takes 1 positional arg (`Value::Arena`). Returns a dict with:
/// - `name`: String — the arena's name
/// - `thunks-allocated`: Int — total thunks ever allocated across all scopes in [start_env_id, envs.len())
/// - `thunks-live`: Int — thunks currently live (slot is Some) across the same range
/// - `scopes`: Int — number of FlatEnv scopes in the arena's range
/// - `heap-bytes`: Int — current process heap allocation in bytes
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
        let (name, start_env_id) = match arena_val {
            Value::Arena {
                name, start_env_id, ..
            } => (name, start_env_id),
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
        let (thunks_allocated, thunks_live, scope_count) = {
            let arena = ctx.scope_arena.borrow();
            let end = arena.scopes.len() as u32;
            let mut live = 0i64;
            let mut scopes = 0u32;
            for eid in start_env_id..end {
                let env = &arena.scopes[eid as usize];
                live += env.count_live() as i64;
                scopes += 1;
            }
            (crate::memory_budget::thunk_total(), live, scopes)
        };
        let heap_bytes = crate::memory_budget::allocated_bytes();
        let mut result_map: indexmap::IndexMap<HashableValue, ThunkId> = indexmap::IndexMap::new();
        let alloc_str = |v: Value| ctx.alloc_thunk(0, Arc::new(Thunk::value(v, call_span.clone())));
        result_map.insert(
            HashableValue::Str("name".into()),
            alloc_str(string_val(&name.to_string())),
        );
        result_map.insert(
            HashableValue::Str("thunks-allocated".into()),
            alloc_str(Value::Int(thunks_allocated as i64)),
        );
        result_map.insert(
            HashableValue::Str("thunks-live".into()),
            alloc_str(Value::Int(thunks_live)),
        );
        result_map.insert(
            HashableValue::Str("scopes".into()),
            alloc_str(Value::Int(scope_count as i64)),
        );
        result_map.insert(
            HashableValue::Str("heap-bytes".into()),
            alloc_str(Value::Int(heap_bytes)),
        );
        Ok(Arc::new(Thunk::value(Value::Dict(result_map), call_span)))
    })
}

/// `builtin-arena-migrate`: recursively migrate a thunk tree from source to destination arena.
///
/// Takes 3 positional args: value (any), source arena (`Value::Arena`), destination arena (`Value::Arena`).
/// Recursively walks the value's structure, migrating all ThunkIds in the source arena's range
/// [start_env_id, envs.len()) to the destination arena. ThunkIds outside this range are permanent
/// and left unchanged. Materialized values are deeply copied; unevaluated thunks are shallow-copied
/// (the Arc is cloned, preserving laziness). After migration, the caller can safely drop the source
/// arena — all reachable thunks have been copied to the destination.
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
        // Force the source arena (args[1]) to get its env_id range.
        let src_arena_thunk = ctx.get_thunk(args[1]);
        let src_arena_val = materialize(&src_arena_thunk, Some(&call_span), &ctx).await?;
        let src_start = match src_arena_val {
            Value::Arena { start_env_id, .. } => start_env_id,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "arena-migrate".to_string(),
                    "Arena (src)",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };
        // The source range is [start, current arena length) — captures all scopes allocated
        // during evaluation in the source arena.
        let src_end = ctx.scope_arena.borrow().scopes.len() as u32;
        let src_range = src_start..src_end;

        // Force the destination arena (args[2]) to get its env_id.
        let dst_thunk = ctx.get_thunk(args[2]);
        let dst_val = materialize(&dst_thunk, Some(&call_span), &ctx).await?;
        let dst_env_id = match dst_val {
            Value::Arena { start_env_id, .. } => start_env_id,
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

        // Enforce the parent-first convention: dst must have been created before src.
        // src_range = [src_start, envs.len()) is only correct when dst_env_id < src_start
        // (dst is a "parent" arena that predates the src "child" arena).
        debug_assert!(
            dst_env_id < src_start,
            "arena-migrate: dst arena (id={dst_env_id}) must be created before src arena (id={src_start}). \
             Create the destination arena first, then create the source arena for temporary evaluation, \
             then migrate, then drop src."
        );

        // Recursively migrate the value from args[0].
        let mut thunk_map = std::collections::HashMap::new();
        let mut env_map = std::collections::HashMap::new();
        let migrated_id = {
            let mut arena_mut = ctx.scope_arena.borrow_mut();
            crate::arena::migrate_thunk_id(
                args[0],
                &src_range,
                crate::arena::ScopeId(dst_env_id),
                &mut thunk_map,
                &mut env_map,
                &mut arena_mut,
            )
        };
        Ok(ctx.get_thunk(migrated_id))
    })
}

/// Returns all "meta" module Rust builtins.
///
/// These are the AST, evaluation, reflection, and macro builtins that are NOT in the
/// core_builtins() set. The core_builtins() items (builtin-parse, builtin-resolve,
/// builtin-typecheck-doc, builtin-eval, builtin-module, builtin-get-type-context,
/// builtin-make-type-ctx, builtin-tc-with-scope, builtin-variant-payload,
/// builtin-tag-of, builtin-llt-repr, builtin-type-of, builtin-cap-env-has?,
/// builtin-check-type, builtin-scopes, builtin-scope-new, builtin-scope-names,
/// builtin-scope-parent, builtin-desugar, builtin-program-docs, builtin-doc-meta)
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
            [Strictness::Seq, Strictness::Seq],
            2,
            ["doc", "scope-id"]
        ),
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
        // ── Environment access ────────────────────────────────────────────────────────
        // builtin-current-env: zero-arg; returns the calling lexical environment.
        builtin!("builtin-current-env", builtin_current_env, [], 0),
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use indexmap::IndexMap;

    use super::{builtin_current_env, builtin_tag_of};
    use crate::error::EvalResult;
    use crate::test_util::test_span;
    use crate::value::{string_val, BuiltinArgs, Thunk, Value};

    fn thunk(val: Value) -> Arc<Thunk> {
        Arc::new(Thunk::value(val, test_span(1, 1, 1, 5)))
    }

    /// Allocate a ThunkId for a Value in the test context. T-1558: BuiltinArgs.args uses ThunkId.
    fn thunk_id(
        val: Value,
        ctx: &std::sync::Arc<crate::eval::EvalContext>,
    ) -> crate::arena::ThunkId {
        ctx.alloc_thunk(0, thunk(val))
    }

    fn call_span() -> crate::ast::Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        let base_dir = crate::test_util::test_caps().root.try_clone().unwrap();
        crate::eval::EvalContext::new_empty(base_dir, false)
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
            tycon: "Color".to_string(),
            ctor: "Red".to_string(),
            payload: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(variant, &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
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
            tycon: "SimpleType".to_string(),
            ctor: "Leaf".to_string(),
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
            tycon: "Shape".to_string(),
            ctor: "Circle".to_string(),
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
            caller_env_id: 0,
        }))
        .await
        .unwrap();
        let val = materialize_sync(&result, &ctx).await;
        assert_eq!(val, string_val("Shape.Circle"));
    }

    /// `builtin_current_env` returns the caller's FlatEnv id as `Value::Int(caller_env_id)`.
    #[tokio::test]
    async fn current_env_captures_caller_env() {
        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env_id: 42,
        }))
        .await
        .unwrap();

        // builtin_current_env now returns Value::Int(caller_env_id).
        let val = materialize_sync(&result, &ctx).await;
        assert_eq!(val, Value::Int(42), "expected caller_env_id as Int(42)");
    }

    /// `builtin_current_env` rejects positional arguments — it takes zero args.
    #[tokio::test]
    async fn current_env_rejects_positional_args() {
        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![thunk_id(Value::Int(1), &ctx)],
            named: no_named(),
            call_span: call_span(),
            ctx,
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
