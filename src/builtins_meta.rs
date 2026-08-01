//! Type introspection, evaluation control, and meta builtins.
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
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::{
    builtin, ok_val, reject_named, require_string, require_type_context, synthetic_call_expr,
};
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, TypeContextData};
use crate::eval_call::{invoke_function, CallContext};
use crate::eval_materialize::make_span_dict;
use crate::rust_span;
use crate::value::{string_val, BuiltinArgs, HashableValue, Strictness, Thunk, Value};

// ── Unified error dict helpers ────────────────────────────────────────────────

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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let msg = require_string("raise", val, Arc::clone(&args[0]).span.clone())?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;

        // Reject named arguments
        crate::builtins::reject_named("builtin-macro-error", named.as_ref(), call_span.clone())?;

        // Expect 1 or 2 arguments
        if args.len() != 1 && args.len() != 2 {
            return Err(EvalError::arity_mismatch(
                if args.is_empty() { 1 } else { 2 },
                args.len(),
                call_span,
            )
            .into());
        }

        // Extract message (first argument) - materialized by Strictness::Seq
        let arg0_thunk = Arc::clone(&args[0]);
        let msg_val = arg0_thunk.require_value()?.clone();
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
/// or a unified diagnostic dict (fields: level, kind, message, span, secondary-spans,
/// notes, call-stack, macro-expand, blame) on failure. Both branches are Dicts so callers
/// can always use `[builtin-has-key? "ok" raw]` to discriminate without a type check.
/// Inherently materializing: must materialize body to catch errors.
pub(crate) fn builtin_try(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let arg_thunk = Arc::clone(&args[0]);
        let call_result = materialize(&arg_thunk, Some(&call_span), &ctx).await;

        match call_result {
            Ok(val) => {
                // Success: {ok: value}. Caller uses [builtin-has-key? "ok" raw] to discriminate.
                // Both success and failure return Dicts so builtin-has-key? is always safe.
                let mut map = IndexMap::new();
                map.insert(
                    HashableValue::Str("ok".into()),
                    ok_val(val, call_span.clone())?,
                );
                ok_val(
                    Value::Dict {
                        entries: map,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
            }
            Err(e) => {
                // ResourceLimitExceeded is non-catchable: it indicates a system-level limit.
                use crate::error::ErrorKind;
                if let ErrorKind::ResourceLimitExceeded { .. } = &e.kind {
                    return Err(e);
                }
                // Failure: full diagnostic dict matching the unified protocol.
                // Discriminated by absence of "ok" key (success has "ok", failure does not).
                let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
                let mk_span =
                    |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
                let primary_span = e.spans.first().map(|(s, _)| s).unwrap_or(&call_span);
                let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                // Unified diagnostic protocol fields:
                let error_msg = e.kind.to_string();
                w.insert(HashableValue::Str("level".into()), mk(string_val("error")));
                w.insert(
                    HashableValue::Str("kind".into()),
                    mk(string_val(e.kind.kind_name())),
                );
                w.insert(
                    HashableValue::Str("message".into()),
                    mk(string_val(&error_msg)),
                );
                w.insert(HashableValue::Str("span".into()), mk_span(primary_span));
                // secondary-spans: spans[1..] as [{span, label}] dicts
                let mut secondary: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for (j, (span, label)) in e.spans.iter().skip(1).enumerate() {
                    let mut ss: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    ss.insert(HashableValue::Str("span".into()), mk_span(span));
                    ss.insert(HashableValue::Str("label".into()), mk(string_val(label)));
                    secondary.insert(
                        HashableValue::Int(j as i64),
                        mk(Value::Dict {
                            entries: ss,
                            type_val: crate::value::unknown_type_val(),
                        }),
                    );
                }
                w.insert(
                    HashableValue::Str("secondary-spans".into()),
                    mk(Value::Dict {
                        entries: secondary,
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                w.insert(
                    HashableValue::Str("notes".into()),
                    mk(Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                w.insert(
                    HashableValue::Str("help".into()),
                    mk(Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                // call-stack: [{label, span}] from EvalError.stack
                let mut stack: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for (j, frame) in e.stack.iter().enumerate() {
                    let mut fd: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    fd.insert(
                        HashableValue::Str("label".into()),
                        mk(string_val(&frame.label)),
                    );
                    fd.insert(
                        HashableValue::Str("span".into()),
                        mk_span(&frame.definition_span),
                    );
                    stack.insert(
                        HashableValue::Int(j as i64),
                        mk(Value::Dict {
                            entries: fd,
                            type_val: crate::value::unknown_type_val(),
                        }),
                    );
                }
                w.insert(
                    HashableValue::Str("call-stack".into()),
                    mk(Value::Dict {
                        entries: stack,
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                // macro-expand: {name, span} or {}
                let macro_val = if let Some((name, span)) = &e.macro_expansion {
                    let mut me: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    me.insert(HashableValue::Str("name".into()), mk(string_val(name)));
                    me.insert(HashableValue::Str("span".into()), mk_span(span));
                    Value::Dict {
                        entries: me,
                        type_val: crate::value::unknown_type_val(),
                    }
                } else {
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    }
                };
                w.insert(HashableValue::Str("macro-expand".into()), mk(macro_val));
                w.insert(
                    HashableValue::Str("blame".into()),
                    mk(Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                ok_val(
                    Value::Dict {
                        entries: w,
                        type_val: crate::value::unknown_type_val(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            caller_env_id,
            ctx,
        } = ctx_arg;
        // caller_env_id is Some because builtin-until is registered with needs_caller_env: true.
        let caller_env_id = caller_env_id.expect(
            "builtin-until: caller_env_id is None — BuiltinDef.needs_caller_env must be true",
        );
        reject_named("until", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }

        let pred_thunk = Arc::clone(&args[0]);
        let f_thunk = Arc::clone(&args[1]);
        let mut val_thunk = Arc::clone(&args[2]);

        // Pre-materialize the predicate function to extract its return type annotation.
        // This lets us pre-resolve the Matchable binding name once before the loop,
        // avoiding repeated runtime type derivation on every iteration.
        let pred_fn_val = materialize(&pred_thunk, Some(&call_span), &ctx).await?;
        let pred_matchable_binding =
            crate::eval::resolve_matchable_binding_from_fn(&pred_fn_val, &ctx);
        // Wrap the materialized predicate back into a thunk for use in pending calls.
        let pred_thunk = Arc::new(Thunk::value(pred_fn_val, call_span.clone()));

        loop {
            // Create a pending call to pred(val) and materialize it.
            let pred_id = Arc::clone(&pred_thunk);
            let val_id = Arc::clone(&val_thunk);
            let pred_result = Arc::new(Thunk::fn_call(
                pred_id,
                vec![val_id],
                IndexMap::new(),
                val_thunk.span.clone().with_name(Arc::from("until")),
                crate::value::FnCallSpec {
                    call_span: call_span.clone(),
                    caller_env_id,
                    ctx: Arc::clone(&ctx),
                    original_call: synthetic_call_expr(call_span.clone()),
                },
            ));

            let pred_val = materialize(&pred_result, Some(&call_span), &ctx).await?;

            if crate::eval::call_to_match_opt_resolved(
                &pred_val,
                pred_matchable_binding.as_deref(),
                &ctx,
                &call_span,
            )
            .await?
            {
                // Predicate holds, return the current value (as thunk)
                return Ok(val_thunk);
            } else {
                // Predicate doesn't hold yet, apply f and materialize to get next value.
                // Alloc ThunkIds for func and arg.
                let f_id = Arc::clone(&f_thunk);
                let val_id = Arc::clone(&val_thunk);
                let f_result = Arc::new(Thunk::fn_call(
                    f_id,
                    vec![val_id],
                    IndexMap::new(),
                    call_span.clone().with_name(Arc::from("until")),
                    crate::value::FnCallSpec {
                        call_span: call_span.clone(),
                        caller_env_id,
                        ctx: Arc::clone(&ctx),
                        original_call: synthetic_call_expr(call_span.clone()),
                    },
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let func_val = arg0_thunk.require_value()?.clone();
        let args_val = arg1_thunk.require_value()?.clone();

        let arg_dict = crate::builtins::require_dict(
            "apply",
            args_val,
            arg1_thunk.span.clone(),
            &ctx,
            call_span.clone(),
        )
        .await?;

        // Split dict entries: integer-keyed → positional, string-keyed → named
        let mut int_entries: Vec<(i64, Arc<Thunk>)> = Vec::with_capacity(arg_dict.len());
        let mut named_args_arcs: IndexMap<String, Arc<Thunk>> =
            IndexMap::with_capacity(arg_dict.len());
        for (key, thunk) in &arg_dict {
            match key {
                HashableValue::Int(n) => int_entries.push((*n, Arc::clone(thunk))),
                HashableValue::Str(s) => {
                    named_args_arcs.insert(s.to_string(), Arc::clone(thunk));
                }
                _ => {
                    // Non-Int/Str keys in call argument dicts are ignored (not valid as named args)
                }
            }
        }
        int_entries.sort_by_key(|(k, _)| *k);
        // Collect positional Arc<Thunk> for invoke_function.
        let positional_ids: Vec<Arc<Thunk>> = int_entries.into_iter().map(|(_, t)| t).collect();
        let named_args: IndexMap<String, Arc<Thunk>> = named_args_arcs;

        match func_val {
            Value::Function {
                params,
                body,
                closure_env,
                ..
            } => {
                let named_ids: IndexMap<String, Arc<Thunk>> = named_args; // already Arc<Thunk>
                invoke_function(&CallContext {
                    params: &params,
                    body: &body,
                    closure_env,
                    positional: &positional_ids,
                    named: if named_ids.is_empty() {
                        None
                    } else {
                        Some(&named_ids)
                    },
                    ctx: &ctx,
                    call_span: call_span.with_name(Arc::from("apply")),
                })
                .await
            }
            Value::Builtin { def, .. } => {
                // Pre-materialize strict args before calling the builtin.
                // `builtin_apply_impl` calls `def.func` directly (not through the CEK machine),
                // so `force_count` and `pos_strictness` pre-materialization do NOT happen
                // automatically. Builtins that use `require_value()` rely
                // on force_count/pos_strictness having been applied; without this, passing a
                // force_count>0 builtin like `$keys` through `$apply` would panic.
                //
                // Ordering: force_count range first (matches force_step dispatch order), then
                // pos_strictness Seq/Spine. Both loops skip args that are already materialized.
                let force_limit = def.force_count.min(positional_ids.len());
                for arg in &positional_ids[..force_limit] {
                    if !arg.is_materialized() {
                        materialize(arg, Some(&call_span), &ctx).await?;
                    }
                }
                for (i, &s) in def.pos_strictness.iter().enumerate() {
                    if i < positional_ids.len() && (s == Strictness::Seq || s == Strictness::Spine)
                    {
                        let arg = Arc::clone(&positional_ids[i]);
                        if !arg.is_materialized() {
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
                    // builtin_apply_impl calls the target builtin directly, without going
                    // through the CEK machine. caller_env_id is not available at this point
                    // (builtin_apply_impl does not receive it in its own BuiltinArgs — it uses
                    // `..` to ignore its own caller_env_id). Builtins with needs_caller_env: true
                    // will panic if called through $apply; this is a known semantic limitation.
                    caller_env_id: None,
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            caller_env_id,
            ctx,
        } = ctx_arg;
        // caller_env_id is Some because builtin-apply is registered with needs_caller_env: true.
        let caller_env_id = caller_env_id.expect(
            "builtin-apply: caller_env_id is None — BuiltinDef.needs_caller_env must be true",
        );
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
            // before calling builtin_apply_impl, which uses require_value().
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
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
            match thunk.require_value()?.clone() {
                Value::String {
                    source, start, end, ..
                } => Ok(source[start..end].to_string()),
                v => Err(EvalError::type_mismatch_ctx(
                    format!("builtin-gensym {name}"),
                    "String",
                    v.type_name(),
                    span.clone(),
                )
                .into()),
            }
        };

        let scope_str = get_str(Arc::clone(&args[0]), "scope", &call_span)?;
        let prefix = get_str(Arc::clone(&args[1]), "prefix", &call_span)?;

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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
            Arc::clone(&args[0]).span.clone(),
        )?;

        // Look up the macro in the inject map
        let inject_names: &[String] = match ctx.config.macro_injects_map.get(&macro_name) {
            Some(names) => names.as_slice(),
            None => &[],
        };

        // Build an integer-keyed Dict of inject names
        let mut dict = IndexMap::new();
        for (i, name) in inject_names.iter().enumerate() {
            dict.insert(
                HashableValue::Int(i as i64),
                Arc::new(Thunk::value(string_val(name), call_span.clone())),
            );
        }
        ok_val(
            Value::Dict {
                entries: dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `decimal`: Parse a string as an exact base-10 decimal (rust_decimal::Decimal).
/// Returns Value::Decimal. Error on invalid format.
pub(crate) fn builtin_decimal(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
                ..
            } => {
                let s = &source[start..end];
                use std::str::FromStr;
                match rust_decimal::Decimal::from_str(s) {
                    Ok(d) => ok_val(
                        Value::Decimal {
                            n: d,
                            type_val: crate::value::unknown_type_val(),
                        },
                        call_span,
                    ),
                    Err(e) => Err(EvalError::internal(
                        format!("decimal: cannot parse \"{s}\": {e}"),
                        call_span,
                    )
                    .into()),
                }
            }
            Value::Int { n, .. } => ok_val(
                Value::Decimal {
                    n: rust_decimal::Decimal::from(n),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            ),
            _ => Err(EvalError::type_mismatch("String or Int", &type_name(&val), call_span).into()),
        }
    })
}

/// `big-int`: Convert an Int or String to a BigInt (arbitrary-precision integer).
pub(crate) fn builtin_big_int(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
            Value::Int { n, .. } => ok_val(
                Value::BigInt {
                    n: num_bigint::BigInt::from(n),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            ),
            Value::String {
                ref source,
                start,
                end,
                ..
            } => {
                let s = &source[start..end];
                match s.parse::<num_bigint::BigInt>() {
                    Ok(n) => ok_val(
                        Value::BigInt {
                            n,
                            type_val: crate::value::unknown_type_val(),
                        },
                        call_span,
                    ),
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

/// `type-of`: takes 1 arg, materializes it, returns the ground TypeValue for the value.
///
/// Uses `ground_typevalue_of` to compute the canonical TypeValue from the value's Rust
/// discriminant. This is the stable, repr-based identity that tinct type predicates
/// (`int?`, `str?`, `proxy?`, etc.) rely on. Using the ground typevalue ensures that
/// values constructed with the bootstrap `unknown_type_val()` sentinel (all literals and
/// builtins) still return the correct TypeValue after the prelude is loaded.
///
/// Returns:
/// - `TypeValue.Repr("Value::Int")` for Int / U64
/// - `TypeValue.Repr("Value::Float")` for Float
/// - `TypeValue.Repr("Value::String")` for String
/// - `TypeValue.Repr("Value::Dict")` for Dict
/// - `TypeValue.Repr("Value::Proxy")` for Proxy
/// - `TypeValue.Repr("Value::Bytes")` for Bytes
/// - `TypeValue.Fn(...)` or `TypeValue.Repr("Value::Function")` for Function/Builtin
/// - `TypeValue.Op(tycon)` for Variant (nominal dispatch)
/// - `TypeValue.Unknown` (empty dict) for all other runtime-only types
pub(crate) fn builtin_type_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let type_value = crate::eval::ground_typevalue_of(&val);
        Ok(Arc::new(Thunk::value((*type_value).clone(), call_span)))
    })
}

/// `builtin-variant?`: test whether a value is a nominal variant (Value::Variant).
///
/// Returns Int(1) if the value (after peeling Annotated wrappers) is a Value::Variant,
/// Int(0) otherwise. Irreducible — tinct code cannot determine this without exhaustively
/// checking all known primitive type names, which is not future-proof.
pub(crate) fn builtin_is_variant(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "builtin-variant?",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        let mut v = &val;
        while let Value::Annotated { inner, .. } = v {
            v = inner.as_ref();
        }
        let is_variant = matches!(v, Value::Variant { .. });
        ok_val(
            Value::Int {
                n: if is_variant { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
/// - PendingCall → `{type: "pending-call"}`
/// - PendingBuiltin → `{type: "pending-builtin", name: "<builtin-name>"}`
/// - Guarded → `{type: "thunk", state: "guarded"}`
/// - InProgress → `{type: "thunk", state: "in-progress"}`
/// - Failed → `{type: "thunk", state: <error-message>}`
pub(crate) fn builtin_ast_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let thunk = Arc::clone(&args[0]);

        // Inspect the thunk state WITHOUT forcing it using ThunkInner API

        // Check for PendingBuiltin
        if let Some(def) = thunk.peek_builtin_def() {
            let mut entries: IndexMap<crate::value::HashableValue, Arc<Thunk>> = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("pending-builtin"),
                    call_span.clone(),
                )),
            );
            entries.insert(
                crate::value::HashableValue::Str("name".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val(def.name),
                    call_span.clone(),
                )),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )));
        }

        // Check for PendingCall
        if thunk.is_pending_call() {
            let mut entries: IndexMap<crate::value::HashableValue, Arc<Thunk>> = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("pending-call"),
                    call_span.clone(),
                )),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )));
        }

        // Check for Guarded
        if thunk.is_guarded() {
            let mut entries: IndexMap<crate::value::HashableValue, Arc<Thunk>> = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("thunk"),
                    call_span.clone(),
                )),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("guarded"),
                    call_span.clone(),
                )),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )));
        }

        // Check for InProgress: result not set AND unevaluated state has been claimed
        if !thunk.is_settled() && thunk.inner.unevaluated.lock().unwrap().0.is_none() {
            let mut entries: IndexMap<crate::value::HashableValue, Arc<Thunk>> = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("thunk"),
                    call_span.clone(),
                )),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("in-progress"),
                    call_span.clone(),
                )),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )));
        }

        // Check for Failed
        if let Some(err) = thunk.try_get_error() {
            let mut entries: IndexMap<crate::value::HashableValue, Arc<Thunk>> = IndexMap::new();
            entries.insert(
                crate::value::HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("thunk"),
                    call_span.clone(),
                )),
            );
            entries.insert(
                crate::value::HashableValue::Str("state".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("failed"),
                    call_span.clone(),
                )),
            );
            entries.insert(
                crate::value::HashableValue::Str("error".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val(&err.kind.to_string()),
                    call_span.clone(),
                )),
            );
            return Ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_val: crate::value::unknown_type_val(),
                },
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

        // Check for Materialized Value::Function — build a metadata dict:
        // {type: "fn", doc: ..., return-ann: ..., params: [...]}
        // materialize() succeeded above (propagated via ?), so require_value() cannot fail here.
        // Propagate with ? anyway — an impossible error must not be silently discarded.
        let val = thunk.require_value()?.clone();
        if let crate::value::Value::Function {
            params, annotation, ..
        } = &val
        {
            let mut dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

            // type: "fn"
            dict.insert(
                HashableValue::Str("type".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val("fn"),
                    call_span.clone(),
                )),
            );

            // doc: string or empty string
            let doc_str = annotation
                .as_ref()
                .and_then(|a| a.doc.as_deref())
                .unwrap_or("");
            dict.insert(
                HashableValue::Str("doc".into()),
                Arc::new(crate::value::Thunk::value(
                    string_val(doc_str),
                    call_span.clone(),
                )),
            );

            // return-ann: annotation dict or empty dict (null)
            let return_ann_thunk = match annotation.as_ref().and_then(|a| a.return_ann.as_ref()) {
                Some(ann) => {
                    let spanned = crate::ast::Spanned::new(ann.clone(), call_span.clone());
                    crate::surface_convert::alloc_annotation(&spanned, &ctx)
                }
                None => Arc::new(crate::value::Thunk::value(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span.clone(),
                )),
            };
            dict.insert(HashableValue::Str("return-ann".into()), return_ann_thunk);

            // params: integer-keyed dict of param entry dicts [{name: "x", annotation: ...}, ...]
            let param_arcs: Vec<Arc<Thunk>> = params
                .iter()
                .map(|p| {
                    let mut param_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    param_dict.insert(
                        HashableValue::Str("name".into()),
                        Arc::new(crate::value::Thunk::value(
                            string_val(&p.name),
                            call_span.clone(),
                        )),
                    );
                    if let Some(ann) = &p.annotation {
                        // ann: &Spanned<Annotation>
                        let ann_thunk = crate::surface_convert::alloc_annotation(ann, &ctx);
                        param_dict.insert(HashableValue::Str("annotation".into()), ann_thunk);
                    }
                    Ok(Arc::new(crate::value::Thunk::value(
                        Value::Dict {
                            entries: param_dict,
                            type_val: crate::value::unknown_type_val(),
                        },
                        call_span.clone(),
                    )))
                })
                .collect::<crate::error::EvalResult<Vec<_>>>()?;

            let params_dict: IndexMap<HashableValue, Arc<Thunk>> = param_arcs
                .into_iter()
                .enumerate()
                .map(|(i, t)| (HashableValue::Int(i as i64), t))
                .collect();
            dict.insert(
                HashableValue::Str("params".into()),
                Arc::new(Thunk::value(
                    Value::Dict {
                        entries: params_dict,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span.clone(),
                )),
            );

            return Ok(Arc::new(crate::value::Thunk::value(
                Value::Dict {
                    entries: dict,
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            )));
        }

        // ast-of is defined only for unevaluated expressions (AstNodeField) and
        // Value::Function. If the thunk holds any other materialized value, the caller
        // should have passed the unevaluated form instead.
        Err(EvalError::type_mismatch("Expr.*", val.type_name(), call_span).into())
    })
}

/// `llt-repr`: takes 1 arg, materializes it recursively, returns its LLT display string representation.
/// This is the programmatic equivalent of the LLT display format (Int(42), Dict({...}), etc.).
/// Used by the `-o llt` output formatter.
pub(crate) fn builtin_llt_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        // value_to_tinct_repr_string produces tinct bracket format: ["key": value  ...]
        let display_str = crate::value_to_tinct_repr_string(&val, &ctx, call_span.clone())
            .await
            .map_err(|e| EvalError::internal(format!("llt-repr: {}", e.kind), call_span.clone()))?;
        ok_val(string_val(&display_str), call_span)
    })
}

/// `debug-repr`: Format a value as a Rust Debug string (DisplayVisitor format).
///
/// This produces the same output as the corpus test runner's DisplayVisitor format:
/// `Int(42)`, `Dict({...})`, `String("text")`, `Variant(Tag.Ctor, Null)`, etc.
/// Used by test-loader.llt for corpus test output formatting.
pub(crate) fn builtin_debug_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let val = crate::builtins::expect_one_arg(
            "debug-repr",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        )?;
        // value_to_display_string produces DisplayVisitor format: Int(42), Dict({...}), etc.
        let display_str = crate::value_to_display_string(&val, &ctx, call_span.clone())
            .await
            .map_err(|e| {
                EvalError::internal(format!("debug-repr: {}", e.kind), call_span.clone())
            })?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        // Consistent with constructor pattern matching (eval.rs),
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
            Value::Variant { ctor, .. } => ok_val(string_val(ctor.as_ref()), call_span),
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
/// `Expr.*` variants carry a `span` field in their payload dict (injected by
/// `inject_span_into_expr_variant` in `surface_convert.rs`). The span dict has
/// the shape `{start: {line: Int, col: Int}, end: {line: Int, col: Int}}`.
///
/// This builtin extracts that span and returns it in the flat format used by
/// `make_span_dict`: `{file, start-line, start-col, end-line, end-col}`.
/// The file is taken from the thunk's own `Span.file` field.
///
/// Returns empty dict `[]` if the value is not an Expr variant or has no span.
///
/// Used for precise error reporting in macros.
pub(crate) fn builtin_span_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        // Extract span from Expr.* variant payload.
        if let Value::Variant {
            ref ctor,
            payload: Some(payload_id),
            ..
        } = val
        {
            if crate::value::tycon_name_from_ctor(ctor.as_ref()) == "Expr" {
                let payload_thunk = Arc::clone(&payload_id);
                let payload_val = match payload_thunk.peek_result() {
                    Some(Ok(v)) => Some(v.clone()),
                    Some(Err(e)) => return Err(Box::new((**e).clone())),
                    None => None,
                };
                if let Some(Value::Dict {
                    entries: payload_dict,
                    ..
                }) = payload_val
                {
                    if let Some(span_thunk) = payload_dict.get(&HashableValue::Str("span".into())) {
                        let span_val = match span_thunk.peek_result() {
                            Some(Ok(v)) => Some(v.clone()),
                            Some(Err(e)) => return Err(Box::new((**e).clone())),
                            None => None,
                        };
                        if let Some(Value::Dict {
                            entries: span_dict, ..
                        }) = span_val
                        {
                            // Extract {start: {line, col}, end: {line, col}}
                            // Returns Ok(Some(...)) on success, Ok(None) for missing/wrong-type,
                            // Err(...) when a span thunk settled with an evaluation error.
                            let get_pos =
                                |key: &str| -> crate::error::EvalResult<Option<(i64, i64)>> {
                                    let pos_thunk =
                                        match span_dict.get(&HashableValue::Str(key.into())) {
                                            Some(t) => t,
                                            None => return Ok(None),
                                        };
                                    let pos_val = match pos_thunk.peek_result() {
                                        Some(Ok(v)) => v.clone(),
                                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                                        None => return Ok(None),
                                    };
                                    let Value::Dict {
                                        entries: pos_dict, ..
                                    } = pos_val
                                    else {
                                        return Ok(None);
                                    };
                                    let line_thunk =
                                        match pos_dict.get(&HashableValue::Str("line".into())) {
                                            Some(t) => t,
                                            None => return Ok(None),
                                        };
                                    let line = match line_thunk.peek_result() {
                                        Some(Ok(v)) => v.clone(),
                                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                                        None => return Ok(None),
                                    };
                                    let col_thunk =
                                        match pos_dict.get(&HashableValue::Str("col".into())) {
                                            Some(t) => t,
                                            None => return Ok(None),
                                        };
                                    let col = match col_thunk.peek_result() {
                                        Some(Ok(v)) => v.clone(),
                                        Some(Err(e)) => return Err(Box::new((**e).clone())),
                                        None => return Ok(None),
                                    };
                                    if let (Value::Int { n: l, .. }, Value::Int { n: c, .. }) =
                                        (line, col)
                                    {
                                        Ok(Some((l, c)))
                                    } else {
                                        Ok(None)
                                    }
                                };

                            if let (Some((sl, sc)), Some((el, ec))) =
                                (get_pos("start")?, get_pos("end")?)
                            {
                                // Get the file from the thunk wrapping the Expr value.
                                let expr_thunk = Arc::clone(&args[0]);
                                let file_str = &expr_thunk.span.file;
                                let file_val = if !file_str.starts_with('<') {
                                    string_val(file_str.as_ref())
                                } else {
                                    Value::Dict {
                                        entries: IndexMap::new(),
                                        type_val: crate::value::unknown_type_val(),
                                    }
                                };

                                let mk = |v: Value| Arc::new(Thunk::value(v, call_span.clone()));
                                let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                                w.insert(HashableValue::Str("file".into()), mk(file_val));
                                w.insert(
                                    HashableValue::Str("start-line".into()),
                                    mk(Value::Int {
                                        n: sl,
                                        type_val: crate::value::unknown_type_val(),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("start-col".into()),
                                    mk(Value::Int {
                                        n: sc,
                                        type_val: crate::value::unknown_type_val(),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("end-line".into()),
                                    mk(Value::Int {
                                        n: el,
                                        type_val: crate::value::unknown_type_val(),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("end-col".into()),
                                    mk(Value::Int {
                                        n: ec,
                                        type_val: crate::value::unknown_type_val(),
                                    }),
                                );
                                return ok_val(
                                    Value::Dict {
                                        entries: w,
                                        type_val: crate::value::unknown_type_val(),
                                    },
                                    call_span,
                                );
                            }
                        }
                    }
                }
            }
        }

        // Not an Expr variant or no span found — return empty dict.
        ok_val(
            Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-var-resolution`: given a (line, col) position and a resolved Program, return the
/// de Bruijn coordinates `{level: N, slot: M}` for the VarRef whose span contains
/// that position, or `[]` if no VarRef is found there.
///
/// Arguments: (line: Int, col: Int, program: Program)
///
/// Used to inspect resolver output from tinct code.
pub(crate) fn builtin_var_resolution(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-var-resolution", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(EvalError::arity_mismatch(3, args.len(), call_span).into());
        }
        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let arg2_thunk = Arc::clone(&args[2]);
        let line_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let cursor_line = match line_val {
            Value::Int { n, .. } => n as u32,
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
        let col_val = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;
        let cursor_col = match col_val {
            Value::Int { n, .. } => n as u32,
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
        let prog_val = materialize(&arg2_thunk, Some(&call_span), &ctx).await?;
        let program_arc = match prog_val {
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

        // Walk all VarRef nodes in the program looking for one whose span contains (line, col).
        fn span_contains(span: &crate::ast::Span, line: u32, col: u32) -> bool {
            let after_start =
                span.start_line < line || (span.start_line == line && span.start_col <= col);
            let before_end = span.end_line > line || (span.end_line == line && span.end_col >= col);
            after_start && before_end
        }
        fn find_in_node(
            node: &std::sync::Arc<crate::ast::SurfaceNode>,
            line: u32,
            col: u32,
        ) -> Option<Option<crate::ast::VarAddr>> {
            if !span_contains(&node.span, line, col) {
                return None;
            }
            use crate::ast::SurfaceExpression;
            match &node.expr {
                SurfaceExpression::VarRef { resolution, .. } => {
                    return resolution.get().flatten().cloned().map(Some).or(Some(None));
                }
                SurfaceExpression::Call {
                    func,
                    args,
                    named_args,
                    ..
                } => {
                    if let Some(r) = find_in_node(func, line, col) {
                        return Some(r);
                    }
                    for a in args {
                        if let Some(r) = find_in_node(a, line, col) {
                            return Some(r);
                        }
                    }
                    for na in named_args {
                        if let Some(r) = find_in_node(&na.node.value, line, col) {
                            return Some(r);
                        }
                    }
                }
                SurfaceExpression::Dict(entries) => {
                    for e in entries {
                        if let Some(k) = &e.node.key {
                            if let Some(r) = find_in_node(k, line, col) {
                                return Some(r);
                            }
                        }
                        if let Some(r) = find_in_node(&e.node.value, line, col) {
                            return Some(r);
                        }
                    }
                }
                SurfaceExpression::Fn { body, .. } => {
                    if let Some(r) = find_in_node(body, line, col) {
                        return Some(r);
                    }
                }
                SurfaceExpression::Field { expr: Some(e), .. } => {
                    if let Some(r) = find_in_node(e, line, col) {
                        return Some(r);
                    }
                }
                SurfaceExpression::Match { scrutinee, arms } => {
                    if let Some(r) = find_in_node(scrutinee, line, col) {
                        return Some(r);
                    }
                    for arm in arms {
                        for body_expr in &arm.body {
                            if let Some(r) = find_in_node(body_expr, line, col) {
                                return Some(r);
                            }
                        }
                    }
                }
                _ => {}
            }
            None
        }

        let found: Option<crate::ast::VarAddr> = {
            let mut found = None;
            'outer: for doc_spanned in &program_arc.documents {
                for item in &doc_spanned.node.items {
                    if let crate::ast::SurfaceItem::Expr(node) = item {
                        if let Some(coords) = find_in_node(node, cursor_line, cursor_col) {
                            found = coords;
                            break 'outer;
                        }
                    }
                }
            }
            found
        };

        let mk = |v: Value| Arc::new(Thunk::value(v, call_span.clone()));
        match found {
            None => ok_val(
                Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            ),
            Some(addr) => {
                // Return the VarAddr index as a flat dict {addr-type, index}.
                use crate::ast::VarAddr;
                let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                let (addr_type, index) = match addr {
                    VarAddr::LetrecGroupMember { slot, .. } => ("letrec", slot),
                    VarAddr::ClosureCapture(i) => ("closure", i),
                    VarAddr::Parameter(i) => ("param", i),
                };
                result.insert(
                    HashableValue::Str("addr-type".into()),
                    mk(string_val(addr_type)),
                );
                result.insert(
                    HashableValue::Str("index".into()),
                    mk(Value::Int {
                        n: index as i64,
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
                ok_val(
                    Value::Dict {
                        entries: result,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
                let mk = |v: Value| Arc::new(Thunk::value(v, call_span.clone()));
                let mut entries: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

                if let Some(ann) = annotation.as_deref() {
                    // Include `doc` field from FnAnnotation.doc (derived from extra["doc"] at
                    // function definition time). This is a fallback for any code path where
                    // doc is not yet in extra; extra["doc"] below will overwrite if present.
                    if let Some(ref doc_str) = ann.doc {
                        entries.insert(HashableValue::Str("doc".into()), mk(string_val(doc_str)));
                    }
                    // Expose the return annotation as a string representation of the return type.
                    // `return` is excluded from `extra` (type expressions cannot be safely
                    // evaluated at definition time), so it is derived from `return_ann` here.
                    let return_str: Option<String> = match &ann.return_ann {
                        Some(crate::ast::Annotation::Simple(name)) => Some(name.clone()),
                        // Returns "Expr" because that is the user-visible source-level text
                        // for the @Expr annotation — not a semantic coupling to the prelude
                        // type name. The string "Expr" is what the user wrote in source.
                        Some(crate::ast::Annotation::Quote) => Some("Expr".to_string()),
                        Some(ann_node) => ann_node.get_property("return").map(|n| n.to_string()),
                        None => None,
                    };
                    if let Some(s) = return_str {
                        entries.insert(HashableValue::Str("return".into()), mk(string_val(&s)));
                    }
                    // Flatten all extra fields into the dict (includes evaluated `doc`).
                    // Extra fields overwrite doc inserted above, ensuring the evaluated
                    // version wins for triple-quoted doc strings.
                    for (key, extra_val) in &ann.extra {
                        entries.insert(
                            HashableValue::Str(key.as_str().into()),
                            mk(extra_val.clone()),
                        );
                    }
                }

                ok_val(
                    Value::Dict {
                        entries,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
            }
            Value::Annotated { annotation, .. } => {
                // Return the annotation value directly — no materialization needed,
                // it was stored materialized at annotation construction time.
                ok_val(*annotation, call_span)
            }
            _ => {
                // All other values have no annotation — return empty dict.
                ok_val(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("make-annotated", named.as_ref(), call_span.clone())?;

        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        let inner_val = args[0].require_value()?.clone();
        let ann_val = args[1].require_value()?.clone();

        // annotation must be a Dict
        if !matches!(ann_val, Value::Dict { .. }) {
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
        Value::Int { .. } => "Int",
        Value::Float { .. } => "Float",
        Value::String { .. } => "String",
        Value::Bytes { .. } => "Bytes",
        Value::Dict { .. } => "Dict",
        Value::Function { .. } => "Function",
        Value::Builtin { .. } => "Builtin",
        Value::Proxy { .. } => "Proxy",
        Value::DirCap { .. } | Value::RevocableDirCap { .. } => "DirCap",
        Value::NetCap { .. } => "NetCap",
        Value::File { .. } => "File",
        Value::Variant { ctor, .. } => return ctor.as_ref().to_string(),
        Value::Decimal { .. } => "Decimal",
        Value::BigInt { .. } => "BigInt",
        Value::Uri { .. } => "Uri",
        Value::Timestamp { .. } => "Timestamp",
        Value::Duration { .. } => "Duration",
        Value::ClockCap { .. } => "ClockCap",
        Value::Timezone { .. } => "Timezone",
        Value::QuicSession { .. } => "QuicSession",
        Value::Http2Session { .. } => "Http2Session",
        Value::Http3Session { .. } => "Http3Session",
        Value::QuicDatagramHandle { .. } => "QuicDatagramHandle",
        Value::Program { .. } => "Program",
        Value::Document { .. } => "Document",
        Value::Task { .. } => "Task",
        Value::Channel { .. } => "Channel",
        Value::Context { .. } => "Context",
        Value::ReactiveCell { .. } => "ReactiveCell",
        Value::Builder(_) => "Builder",
        Value::BroadcastChannel { .. } => "BroadcastChannel",
        Value::OneshotSender { .. } => "OneshotSender",
        Value::OneshotReceiver { .. } => "OneshotReceiver",
        Value::U64 { .. } => "U64",
        // Annotated is transparent — delegate to inner value's type_name.
        Value::Annotated { inner, .. } => return type_name(inner),
        Value::TypeContext { .. } => "TypeContext",
        Value::Expression { .. } => "Expression",
        Value::Arena { .. } => "Arena",
        Value::CoreDocument { .. } => "CoreDocument",
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let s = require_string("blake3", val, Arc::clone(&args[0]).span.clone())?;
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
            Value::DirCap { dir, .. } => dir.try_clone().expect("dir try_clone"),
            Value::RevocableDirCap { inner, revoked, .. } => {
                if revoked.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(EvalError::internal(
                        "cap-identity: capability has been revoked".to_string(),
                        call_span.clone(),
                    )
                    .into());
                }
                inner.try_clone().expect("dir try_clone")
            }
            _ => {
                return Err(EvalError::type_mismatch(
                    "DirCap",
                    val.type_name(),
                    Arc::clone(&args[0]).span.clone(),
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

/// `builtin-parse`: Parse Bytes + path String → `{program, errors}`.
///
/// Signature: `[builtin-parse bytes path]`
/// - `bytes` (Bytes): the source file contents as raw bytes
/// - `path` (String): file path hint used in parse error messages
///
/// Returns `{program: Value::Program, errors: Dict<Int, ErrorDict>}`.
///   - `program`: raw `Value::Program` with empty resolution table.
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-parse", named.as_ref(), call_span.clone())?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // First arg: Bytes (the source file contents)
        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let bytes_val = arg0_thunk.require_value()?.clone();
        let source_bytes = match bytes_val {
            Value::Bytes {
                source, start, end, ..
            } => source[start..end].to_vec(),
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
        let path_val = arg1_thunk.require_value()?.clone();
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

        // Parse — spans carry the path for error messages.
        // Fatal parse errors (lexer failure, unclosed brackets) are captured in the
        // diagnostics list rather than raised, so callers can inspect them programmatically.
        let source_file: Arc<str> = Arc::from(path_str.as_str());
        let (parsed, fatal_diagnostics) =
            match crate::parser::parse(&source, Arc::clone(&source_file)) {
                Ok(output) => (Some(output), vec![]),
                Err(fatal) => (None, vec![fatal]),
            };

        // Build diagnostics list: fatal error (if any) + recovered errors from ParseOutput.
        let all_parse_diagnostics: Vec<crate::error::TypeDiagnostic> =
            if let Some(ref output) = parsed {
                output.diagnostics.clone()
            } else {
                fatal_diagnostics
            };

        // Build the integer-keyed diagnostics dict from TypeDiagnostic.
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
        let mut errors_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, diag) in all_parse_diagnostics.iter().enumerate() {
            let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            w.insert(
                HashableValue::Str("level".into()),
                mk(string_val(&diag.level.to_string())),
            );
            w.insert(HashableValue::Str("kind".into()), mk(string_val(diag.kind)));
            w.insert(
                HashableValue::Str("message".into()),
                mk(string_val(&diag.message)),
            );
            w.insert(
                HashableValue::Str("span".into()),
                mk_span(diag.primary_span()),
            );
            let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, note) in diag.notes.iter().enumerate() {
                notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
            }
            w.insert(
                HashableValue::Str("notes".into()),
                mk(Value::Dict {
                    entries: notes_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            let mut help_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, help) in diag.help.iter().enumerate() {
                help_dict.insert(HashableValue::Int(j as i64), mk(string_val(help)));
            }
            w.insert(
                HashableValue::Str("help".into()),
                mk(Value::Dict {
                    entries: help_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("call-stack".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("macro-expand".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("blame".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("secondary-spans".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            errors_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
        }

        // Build the program value. If the parse was fatal, produce an empty program.
        let surface_program = if let Some(output) = parsed {
            output.program
        } else {
            // Fatal parse: return empty program so {program, errors} is always usable.
            crate::ast::SurfaceProgram { documents: vec![] }
        };
        let program_value = Value::Program {
            program: std::sync::Arc::new(surface_program),
            resolutions: std::sync::Arc::new(Default::default()),
            type_val: crate::value::unknown_type_val(),
        };

        // Return {program: Value::Program, diagnostics: integer-keyed Dict of diagnostic dicts}.
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result.insert(HashableValue::Str("program".into()), mk(program_value));
        result.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: errors_dict,
                type_val: crate::value::unknown_type_val(),
            }),
        );
        Ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: result,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )))
    })
}

/// Compute the best available tinct source span for a document.
///
/// Returns the span of the first Expr item, falling back to the first header entry,
/// then to `call_span` (the builtin call site). Never returns a rust_span.
fn compute_doc_span(
    doc: &crate::ast::SurfaceDocument,
    call_span: &crate::ast::Span,
) -> crate::ast::Span {
    doc.items
        .iter()
        .find_map(|item| match item {
            crate::ast::SurfaceItem::Expr(n) => Some(n.span.clone()),
            crate::ast::SurfaceItem::Decl(_) => None,
        })
        .or_else(|| doc.header.values().next().map(|n| n.span.clone()))
        .unwrap_or_else(|| call_span.clone())
}

/// `builtin-resolve`: Resolve a single `Value::Document` in-place.
///
/// Takes 2 positional args: the document to resolve and a name-set dict.
///
/// **Arguments:**
/// - arg0: `Value::Document` (only Document, not Program — error if Program)
/// - arg1: Dict — name-set: `{name-a: 1, name-b: 1, ...}` (keys are in-scope names; values ignored)
///   Names are extracted in insertion order and seeded as `LetrecGroupMember(i)` (i = 0..n-1)
///   so that at runtime, `CoreExpr::Var { addr: LetrecGroupMember(i) }` resolves to
///   `frame.group[i]` = the i-th env-dict thunk in `eval_core_document_exprs`.
///
/// **Returns** `{doc: Document, diagnostics: Dict<Int, DiagnosticDict>}`.
///
/// Writes closure-converted `VarAddr` values into the inline `Resolution` OnceLocks on each
/// `VarRef`/`DotAccess` node of the document's AST. After this call, `builtin-lower` and
/// `builtin-eval` process the resolved nodes.
///
/// This is Stage 2 of the 4-stage pipeline (Stage 1 is parse from loader.llt).
pub(crate) fn builtin_resolve(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let doc_val = args[0].require_value()?.clone();
        let doc_arc = if let Value::Document { doc: d, .. } = doc_val {
            d
        } else {
            return Err(EvalError::internal(
                format!("expected Document, got {}", doc_val.type_name()),
                call_span,
            )
            .into());
        };

        // arg1: Dict<String, Any> — name-set: keys are the in-scope names; values are ignored.
        // Extract names in insertion order — the i-th name gets LetrecGroupMember(i) from the
        // resolver, matching the position of that env-dict thunk in the initial_group passed to
        // eval_core_document_exprs.
        let name_set_val = args[1].require_value()?.clone();
        let name_set_dict = match name_set_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return Err(EvalError::internal(
                    format!("name-set is not a Dict: {}", other.type_name()),
                    call_span,
                )
                .into())
            }
        };

        // Build the ordered name list from the name-set's string keys (insertion order).
        // The i-th env-dict name gets LetrecGroupMember(root_group_len + i) in the resolver.
        let mut env_names: Vec<String> = Vec::with_capacity(name_set_dict.len());
        for (k, _v_thunk) in &name_set_dict {
            let name = match k {
                HashableValue::Str(s) => s,
                other => {
                    return Err(EvalError::internal(
                        format!("name-set key is not a String: {}", other),
                        call_span,
                    )
                    .into())
                }
            };
            env_names.push(name.to_string());
        }

        // Seed the resolver with root_group names at their actual runtime slots.
        // root_group_resolver_map() returns (name, slot) where slot is the position
        // in EvalContext.root_group. These names (builtins like builtin-dict-get)
        // need correct resolution for Field.resolution and other scope lookups.
        // Env-dict names follow at offset root_group.len() in the accumulated_group.
        let root_map = ctx.root_group_resolver_map();
        let root_group_len = ctx.root_group.len() as u32;

        let (_resolve_table, resolve_diagnostics, unreferenced_names, unified_frames) =
            crate::resolve::resolve_surface_document_with_seed_frames(
                &doc_arc,
                &[root_map],
                &env_names,
                root_group_len,
            );

        // unified_frames already contains all scope frames from the resolver in natural
        // order: env_names frame first (outermost), then Dict letrec and BlockBody frames
        // in nesting order. resolve_name_in_frames searches from last (innermost) to first,
        // so dict frames are searched before the env_names frame.
        let dispatcher_frames = unified_frames;

        // Build unified diagnostics dict from TypeDiagnostics (errors + warnings).
        // Callers distinguish severity by reading d.level on each entry.
        // There is no separate "warnings" key — diagnostics is the single bag for all severities.
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        for (i, diag) in resolve_diagnostics.iter().enumerate() {
            let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            w.insert(
                HashableValue::Str("level".into()),
                mk(string_val(&diag.level.to_string())),
            );
            w.insert(HashableValue::Str("kind".into()), mk(string_val(diag.kind)));
            w.insert(
                HashableValue::Str("message".into()),
                mk(string_val(&diag.message)),
            );
            w.insert(
                HashableValue::Str("span".into()),
                mk_span(diag.primary_span()),
            );
            let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, note) in diag.notes.iter().enumerate() {
                notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
            }
            w.insert(
                HashableValue::Str("notes".into()),
                mk(Value::Dict {
                    entries: notes_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            let mut help_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, help) in diag.help.iter().enumerate() {
                help_dict.insert(HashableValue::Int(j as i64), mk(string_val(help)));
            }
            w.insert(
                HashableValue::Str("help".into()),
                mk(Value::Dict {
                    entries: help_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("call-stack".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("macro-expand".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("blame".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("secondary-spans".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
        }

        // Build unreferenced dict: {name: 1} for each env-dict name never referenced.
        // Exclude root_group names — they're not user-visible env-dict entries.
        let mut unreferenced_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for name in &unreferenced_names {
            unreferenced_dict.insert(
                HashableValue::Str(name.as_str().into()),
                mk(Value::Int {
                    n: 1,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
        }

        let doc_span = compute_doc_span(&doc_arc, &call_span);

        // Return {doc: Value::Document, diagnostics: Dict, unreferenced: Dict, doc-span: SpanDict}.
        // diagnostics is a unified bag of all severities; callers read d.level to filter.
        let mut result_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result_dict.insert(
            HashableValue::Str("doc".into()),
            mk(Value::Document {
                doc: std::sync::Arc::clone(&doc_arc),
                resolver_frames: Arc::new(dispatcher_frames),
                type_val: crate::value::unknown_type_val(),
            }),
        );
        result_dict.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: diagnostics_dict,
                type_val: crate::value::unknown_type_val(),
            }),
        );
        result_dict.insert(
            HashableValue::Str("unreferenced".into()),
            mk(Value::Dict {
                entries: unreferenced_dict,
                type_val: crate::value::unknown_type_val(),
            }),
        );
        result_dict.insert(HashableValue::Str("doc-span".into()), mk_span(&doc_span));
        ok_val(
            Value::Dict {
                entries: result_dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

// ── T-1742: Pipeline stage lint builtins ──────────────────────────────────────

/// `builtin-lint-pipeline-docs`: Run the cross-document pipeline stage lint.
///
/// Takes 1 positional arg: a Dict of `Value::Document` objects (integer-keyed, in order).
/// Takes 1 required named arg: `pipeline-input` (String) — the variable name to track
/// for field accesses between pipeline stages (e.g. `"%"`).
///
/// For each consecutive pair of documents (doc[i], doc[i+1]):
/// - Extracts the static produced keys from doc[i]'s final expression
/// - Extracts field accesses on the `pipeline-input` variable from doc[i+1]
/// - Checks whether all produced keys are consumed; warns on abandoned keys
///
/// Returns `Dict[Int, DiagnosticDict]` using the same diagnostic format as `builtin-resolve`.
/// The returned dict is a flat list of diagnostics (empty when no warnings).
pub(crate) fn builtin_lint_pipeline_docs(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        // Extract required named arg: pipeline-input (String)
        let pipeline_input_name = match named.as_ref().and_then(|n| n.get("pipeline-input")) {
            Some(thunk) => {
                let val = crate::eval::materialize(thunk, Some(&call_span), &ctx).await?;
                crate::builtins::require_string(
                    "builtin-lint-pipeline-docs (pipeline-input:)",
                    val,
                    call_span.clone(),
                )?
            }
            None => {
                return Err(EvalError::user_error(
                    "builtin-lint-pipeline-docs: required named arg 'pipeline-input' missing"
                        .to_string(),
                    call_span,
                )
                .into())
            }
        };

        let docs_val = args[0].require_value()?.clone();
        let docs_dict = match docs_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return Err(EvalError::internal(
                    format!(
                        "builtin-lint-pipeline-docs: expected Dict, got {}",
                        other.type_name()
                    ),
                    call_span,
                )
                .into())
            }
        };

        // Sort document entries by integer key to get them in pipeline order.
        let mut doc_entries: Vec<(i64, Arc<Thunk>)> = docs_dict
            .iter()
            .map(|(k, v)| match k {
                HashableValue::Int(i) => Ok((*i, Arc::clone(v))),
                other => Err(EvalError::user_error(
                    format!(
                        "builtin-lint-pipeline-docs: document dict key must be Int, got: {:?}",
                        other
                    ),
                    call_span.clone(),
                )
                .into()),
            })
            .collect::<crate::error::EvalResult<Vec<_>>>()?;
        doc_entries.sort_by_key(|(i, _)| *i);

        // Collect SurfaceDocument arcs from the thunks.
        let mut doc_arcs: Vec<std::sync::Arc<crate::ast::SurfaceDocument>> = Vec::new();
        for (_, thunk) in &doc_entries {
            let val = crate::eval::materialize(thunk, Some(&call_span), &ctx).await?;
            if let Value::Document { doc: d, .. } = val {
                doc_arcs.push(d);
            } else {
                return Err(EvalError::internal(
                    format!(
                        "builtin-lint-pipeline-docs: entry is not a Document: {}",
                        val.type_name()
                    ),
                    call_span,
                )
                .into());
            }
        }

        // Build lint_stages: for each consecutive pair (doc[i], doc[i+1]),
        // collect produced keys of doc[i] and accesses on pipeline-input from doc[i+1].
        let mut lint_stages: Vec<(Vec<String>, Vec<String>, bool, crate::ast::Span)> = Vec::new();
        for i in 0..doc_arcs.len().saturating_sub(1) {
            let doc_span_i = compute_doc_span(&doc_arcs[i], &call_span);
            let (produced_keys, span) =
                crate::resolve::collect_document_produced_keys(&doc_arcs[i], &doc_span_i);
            let (field_accesses, dynamic_use) =
                crate::resolve::collect_var_accesses(&doc_arcs[i + 1], &pipeline_input_name);
            lint_stages.push((produced_keys, field_accesses, dynamic_use, span));
        }

        // Run the lint.
        let warnings = crate::resolve::lint_pipeline_stages(&lint_stages);

        // Build diagnostics dict using the same format as builtin-resolve.
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, diag) in warnings.iter().enumerate() {
            let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            w.insert(
                HashableValue::Str("level".into()),
                mk(string_val(&diag.level.to_string())),
            );
            w.insert(HashableValue::Str("kind".into()), mk(string_val(diag.kind)));
            w.insert(
                HashableValue::Str("message".into()),
                mk(string_val(&diag.message)),
            );
            w.insert(
                HashableValue::Str("span".into()),
                mk_span(diag.primary_span()),
            );
            let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, note) in diag.notes.iter().enumerate() {
                notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
            }
            w.insert(
                HashableValue::Str("notes".into()),
                mk(Value::Dict {
                    entries: notes_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            let mut help_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, help) in diag.help.iter().enumerate() {
                help_dict.insert(HashableValue::Int(j as i64), mk(string_val(help)));
            }
            w.insert(
                HashableValue::Str("help".into()),
                mk(Value::Dict {
                    entries: help_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("call-stack".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("macro-expand".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("blame".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("secondary-spans".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
        }

        ok_val(
            Value::Dict {
                entries: diagnostics_dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

// builtin_typecheck removed — use builtin_typecheck_doc for per-document type-checking

/// `builtin-typecheck-doc`: Type-check a single resolved `Value::Document`.
///
/// Takes exactly 3 args:
/// - arg0: Value::Document (already resolved)
/// - arg1: Value::TypeContext
/// - arg2: Value::Dict — the doc-env (used for type-stage evaluation context).
///
/// Returns Value::Document (same Arc — type annotations written inline).
pub(crate) fn builtin_typecheck_doc(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-typecheck-doc", named.as_ref(), call_span.clone())?;
        if args.len() != 3 {
            return Err(Box::new(EvalError::user_error(
                format!(
                    "builtin-typecheck-doc: expected 3 arguments, got {}",
                    args.len()
                ),
                call_span,
            )));
        }

        // arg0: Value::Document — extract doc_arc and resolver_frames.
        let doc_val = args[0].require_value()?.clone();
        let (doc_arc, resolver_frames) = match doc_val {
            Value::Document {
                doc: d,
                resolver_frames: rf,
                ..
            } => (d, rf),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-typecheck-doc".to_string(),
                    "Document",
                    other.type_name(),
                    args[0].span.clone(),
                )
                .into());
            }
        };

        // arg1: Value::TypeContext
        let tc_val = args[1].require_value()?.clone();
        let tc_arc = require_type_context("builtin-typecheck-doc", tc_val, args[1].span.clone())?;

        // arg2: Value::Dict — the doc-env (validated for type but value not retained here;
        // the doc_arc already holds the parsed document form).
        {
            let env_val = args[2].require_value()?.clone();
            if !matches!(env_val, Value::Dict { .. }) {
                return Err(EvalError::internal(
                    format!(
                        "builtin-typecheck-doc arg2 (doc-env) is not a Dict: {}",
                        env_val.type_name()
                    ),
                    call_span,
                )
                .into());
            }
        }

        // Extract TypeContext state
        let (mut state, parent_env) = {
            let guard = tc_arc.lock().unwrap();
            let mut state = crate::types::InferState::new();
            state.tycon_env = guard.tycon_env.clone();
            state.env = Arc::clone(&guard.inference_env);
            state.eval_ctx = Some(Arc::clone(&ctx));

            // Install the TypeContextData's accumulated type-stage data.
            // Ensure an innermost scope frame exists for declarations discovered during this typecheck.
            state.type_stage_scope = guard.type_stage_scope.clone();
            state.type_stage_fns = guard.type_stage_fns.clone();
            state.type_stage_type_vars = guard.type_stage_type_vars.clone();
            if state.type_stage_scope.is_empty() {
                state
                    .type_stage_scope
                    .push(std::collections::HashMap::new());
            }

            let parent_env = Arc::clone(&guard.inference_env);
            (state, parent_env)
        };

        // Seed child_env with all root-group entries so that get_scheme_at(N, slot) correctly
        // finds them at depth N via normal parent-chain traversal. Same priority model as
        // typecheck_program_bootstrap: type-stage entries first, then parent_env chain, then Unknown.
        let child_env = Arc::new(std::sync::RwLock::new(crate::env::Env::with_parent(
            Arc::clone(&parent_env),
        )));
        {
            use crate::type_infer::make_typevalue_op;
            let mut child_inner = child_env.write().unwrap();

            // Type-stage entries get their actual TypeValues (highest priority).
            for (name, tv) in state.type_stage_scope.iter().flat_map(|m| m.iter()) {
                child_inner.insert_scheme_named_only(name.clone(), Arc::clone(tv));
            }
            for (name, _thunk) in &state.type_stage_fns {
                let tv = make_typevalue_op(name);
                child_inner.insert_scheme_named_only(name.clone(), tv);
            }
            for (name, _kind) in &state.type_stage_type_vars {
                let tv = make_typevalue_op(name);
                child_inner.insert_scheme_named_only(name.clone(), tv);
            }

            // Walk parent_env chain from outermost to innermost (innermost wins on conflict).
            let mut chain: Vec<Arc<std::sync::RwLock<crate::env::Env>>> = Vec::new();
            {
                let mut cursor = Some(Arc::clone(&parent_env));
                while let Some(arc) = cursor {
                    chain.push(Arc::clone(&arc));
                    cursor = arc.read().unwrap().parent.as_ref().map(Arc::clone);
                }
            }
            for frame in chain.iter().rev() {
                let frame_read = frame.read().unwrap();
                for (name, env_slot) in &frame_read.extras {
                    if let Some(ref scheme) = env_slot.scheme {
                        child_inner.insert_scheme_named_only(name.clone(), Arc::clone(scheme));
                    }
                }
            }
        }
        // Update state.env to point at child_env so class/instance lookups work.
        state.env = Arc::clone(&child_env);

        let mut type_map_ref: Option<&mut crate::typecheck::TypeMap> = None;

        // process_document processes all items in source order, extends env with schemes from
        // the last dict body, and returns (doc_env, result_type, errors).
        let (doc_env, _, errors) =
            crate::typecheck::process_document(&doc_arc, &child_env, &mut state, &mut type_map_ref)
                .await;

        // Collect TypeDiagnostics from state.diagnostics — now includes inline CEK emissions
        let mut type_diagnostics: Vec<crate::error::TypeDiagnostic> =
            std::mem::take(&mut state.diagnostics);

        // Write results back to TypeContext:
        // - tycon_env: new type constructor definitions from this document
        // - inference_env: merge doc_env chain into the single flat root so subsequent documents
        //   see this document's type schemes. We merge rather than replace so that inference_env
        //   stays as the single flat root (no parent chain growth). Inner frames win over outer
        //   for same-named entries (merge_env_chain_into uses or_insert semantics).
        {
            let mut guard = tc_arc.lock().unwrap();
            for (name, def) in &state.tycon_env {
                guard
                    .tycon_env
                    .entry(name.clone())
                    .or_insert_with(|| Arc::clone(def));
            }
            crate::env::merge_env_chain_into(&doc_env, &guard.inference_env);
            // guard.inference_env stays unchanged — it is the single flat root

            // Write back type_stage_scope: merge new declarations (ADTs, classes) from
            // frame[0] back into TypeContextData so subsequent typecheck calls see them.
            if let Some(new_entries) = state.type_stage_scope.first() {
                if guard.type_stage_scope.is_empty() {
                    guard
                        .type_stage_scope
                        .push(std::collections::HashMap::new());
                }
                for (name, entry) in new_entries {
                    guard.type_stage_scope[0]
                        .entry(name.clone())
                        .or_insert_with(|| entry.clone());
                }
            }
        }

        // Build unified diagnostics dict from all type diagnostics (errors + warnings + info)
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        // Merge errors and type_diagnostics into one vec
        let mut all_diagnostics = errors;
        all_diagnostics.append(&mut type_diagnostics);

        // Build dict entries from all type diagnostics
        for (i, diag) in all_diagnostics.iter().enumerate() {
            let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            w.insert(
                HashableValue::Str("level".into()),
                mk(string_val(&diag.level.to_string())),
            );
            w.insert(HashableValue::Str("kind".into()), mk(string_val(diag.kind)));
            w.insert(
                HashableValue::Str("message".into()),
                mk(string_val(&diag.message)),
            );
            w.insert(
                HashableValue::Str("span".into()),
                mk_span(diag.primary_span()),
            );
            let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, note) in diag.notes.iter().enumerate() {
                notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
            }
            w.insert(
                HashableValue::Str("notes".into()),
                mk(Value::Dict {
                    entries: notes_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            let mut help_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, help) in diag.help.iter().enumerate() {
                help_dict.insert(HashableValue::Int(j as i64), mk(string_val(help)));
            }
            w.insert(
                HashableValue::Str("help".into()),
                mk(Value::Dict {
                    entries: help_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("call-stack".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("macro-expand".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            w.insert(
                HashableValue::Str("blame".into()),
                mk(Value::Dict {
                    entries: IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            // Secondary spans (spans[1..]) — e.g., "type declared here" from TypeAssert.
            // Each entry is {span: ..., label: ...} in insertion order.
            let mut ss_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
            for (j, (span, label)) in diag.spans.iter().skip(1).enumerate() {
                let mut ss_entry: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                ss_entry.insert(HashableValue::Str("span".into()), mk_span(span));
                ss_entry.insert(HashableValue::Str("label".into()), mk(string_val(label)));
                ss_dict.insert(
                    HashableValue::Int(j as i64),
                    mk(Value::Dict {
                        entries: ss_entry,
                        type_val: crate::value::unknown_type_val(),
                    }),
                );
            }
            w.insert(
                HashableValue::Str("secondary-spans".into()),
                mk(Value::Dict {
                    entries: ss_dict,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_val: crate::value::unknown_type_val(),
                }),
            );
        }

        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result.insert(
            HashableValue::Str("doc".into()),
            mk(Value::Document {
                doc: doc_arc,
                resolver_frames,
                type_val: crate::value::unknown_type_val(),
            }),
        );
        result.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: diagnostics_dict,
                type_val: crate::value::unknown_type_val(),
            }),
        );
        ok_val(
            Value::Dict {
                entries: result,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
                    Value::TypeContext {
                        ctx: Arc::new(Mutex::new(tc_clone)),
                        type_val: crate::value::unknown_type_val(),
                    },
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
/// Creates a `TypeContextData` seeded with:
/// - `inference_env`: the builtin_core type env (type schemes for core builtins)
/// - `type_stage_scope`: the builtin_core type-stage scope (Integer, String, Dict, DirCap, etc.)
///
/// The type-stage scope MUST be seeded with `builtin_core_ts_scope` (not `Vec::new()`)
/// so that annotations like `@Dict`, `@Integer`, `@String` resolve to the correct TypeValues
/// (e.g., `Dict` → open-record TypeValue.Record with Uniform tail). Without this seeding,
/// `uses-scope` wires TyConDefs via `merge_env_schemes_into_env`, inserting `Dict → Op("Dict")`
/// which is the nominal type (not the open-record structural type needed for annotation checks).
///
/// Returns a `Value::TypeContext` handle. Also installs on the EvalContext via `init_type_context`
/// (no-op if already initialized).
///
/// Signature: `[builtin-make-type-ctx]`
pub(crate) fn builtin_make_type_ctx(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        // Fresh TypeContext seeded with the builtin_core type env AND type-stage scope.
        //
        // type_stage_scope must include builtin_core.llt's type-stage entries
        // (Integer, String, Dict, DirCap, etc.) so that @Dict and similar annotations
        // resolve to the correct TypeValues (e.g., Dict → open-record TypeValue.Record)
        // rather than undefined or to an Op type from TyConDef wiring.
        //
        // Without this seeding, when uses-scope wires TyConDefs via merge_env_schemes_into_env,
        // it inserts Dict → Op("Dict") (a nominal type, not the open-record type). Subsequent
        // @Dict annotations then incorrectly resolve to Op("Dict") or worse, to whatever type
        // was produced by an unrelated type_stage_scope[0] entry that happened to win the
        // or_insert ordering contest.
        let type_stage_data = crate::imports::get_builtin_core_type_stage_scope().await;
        let tc = TypeContextData {
            inference_env: crate::imports::get_builtin_core_type_env().await,
            tycon_env: std::collections::HashMap::new(),
            type_stage_scope: type_stage_data.scope,
            type_stage_fns: type_stage_data.fns,
            type_stage_type_vars: type_stage_data.type_vars,
            type_diagnostics: Vec::new(),
        };
        // Install it on EvalContext (no-op if already initialized).
        ctx.init_type_context(tc.clone());
        // Wrap in Arc<Mutex<>> and return as a Value::TypeContext handle.
        ok_val(
            Value::TypeContext {
                ctx: Arc::new(Mutex::new(tc)),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-fork-type-ctx", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }
        let parent_val = args[0].require_value()?.clone();
        match parent_val {
            Value::TypeContext {
                ctx: parent_arc, ..
            } => {
                // Clone the parent's TypeContextData into an independent child.
                // The child gets a fresh Arc<Mutex<>> — parent and child are independent
                // after this point. Mutations to child do not propagate to parent.
                let child_data = parent_arc.lock().unwrap().clone();
                ok_val(
                    Value::TypeContext {
                        ctx: Arc::new(Mutex::new(child_data)),
                        type_val: crate::value::unknown_type_val(),
                    },
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

/// `builtin-tc-update-type-stage-env`: populate the TypeContext's type-stage scope chain.
///
/// Takes two positional arguments:
///   - arg 0: `Value::TypeContext` — the TypeContext to update
///   - arg 1: `Value::Dict` — env-dict from type-stage document evaluation
///
/// For each string-keyed entry in the env-dict:
///   - Materializes the thunk value
///   - Delegates to `classify_type_stage_entry` (same logic as the bootstrap path in imports.rs):
///     - TypeNode leaf (Int, String, etc.) → TypeValue in type_stage_scope
///     - TypeNode.TypeVar sentinel → kind string in type_stage_type_vars
///     - Complex TypeNode variant (Union, TypeApplication, Arrow, etc.) → TypeValue in type_stage_scope
///       via `typenode_value_to_type` using the existing scope as context
///     - Function → thunk in type_stage_fns (parameterized type constructor)
///     - Otherwise: skips (not a type-stage value)
///
/// The new frame is prepended to `TypeContextData.type_stage_scope` (Vec[0] = innermost,
/// highest priority). Each module's type-stage is a distinct frame.
///
/// Returns the same TypeContext (mutations are visible through the shared Arc<Mutex>).
///
/// Populates the TypeContext type-stage scope chain from the accumulated env dict.
pub(crate) fn builtin_tc_update_type_stage_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        crate::builtins::reject_named(
            "builtin-tc-update-type-stage-env",
            named.as_ref(),
            call_span.clone(),
        )?;
        if args.len() != 2 {
            return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
        }

        // Extract TypeContext — same pattern as builtin_typecheck_doc
        let tc_arc = match args[0].require_value()?.clone() {
            Value::TypeContext { ctx: arc, .. } => arc,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-update-type-stage-env".to_string(),
                    "TypeContext",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Extract Dict
        let dict_entries = match args[1].require_value()?.clone() {
            Value::Dict { entries: d, .. } => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-tc-update-type-stage-env".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Collect names + thunks without holding the TypeContext guard
        let to_process: Vec<(String, Arc<Thunk>)> = dict_entries
            .iter()
            .filter_map(|(key, thunk)| {
                if let HashableValue::Str(name) = key {
                    Some((name.to_string(), Arc::clone(thunk)))
                } else {
                    None
                }
            })
            .collect();

        // Snapshot the existing resolved type_stage_scope so complex TypeNode variants
        // (TypeApplication, Union, Arrow, etc.) can be resolved by typenode_value_to_type,
        // which requires the scope chain for recursive conversion of child TypeNode values.
        let existing_scope: Vec<std::collections::HashMap<String, crate::type_infer::TypeValue>> = {
            let guard = tc_arc.lock().unwrap();
            guard.type_stage_scope.clone()
        };

        // Materialize each thunk and classify into the three maps
        let mut new_frame: std::collections::HashMap<String, crate::type_infer::TypeValue> =
            std::collections::HashMap::new();
        let mut new_fns: std::collections::HashMap<String, Arc<Thunk>> =
            std::collections::HashMap::new();
        let mut new_type_vars: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (name, thunk) in to_process {
            let val = materialize(&thunk, None, &ctx).await?;
            let classified = match crate::imports::classify_type_stage_entry(
                &name,
                &thunk,
                &val,
                &ctx,
                &existing_scope,
            )
            .await?
            {
                Some(c) => c,
                None => continue, // not a type-stage value — skip
            };
            let (tv, opt_thunk, opt_kind) = classified;
            new_frame.insert(name.clone(), Arc::clone(&tv));
            if let Some(thunk_arc) = opt_thunk {
                new_fns.insert(name.clone(), thunk_arc);
            }
            if let Some(kind_str) = opt_kind {
                new_type_vars.insert(name.clone(), kind_str);
            }
        }

        // Prepend new frame — it becomes innermost (Vec[0]), highest priority
        {
            let mut guard = tc_arc.lock().unwrap();
            guard.type_stage_scope.insert(0, new_frame);
            // Merge fns and type_vars (new entries take precedence via or_insert)
            for (name, thunk_arc) in new_fns {
                guard.type_stage_fns.entry(name).or_insert(thunk_arc);
            }
            for (name, kind_str) in new_type_vars {
                guard.type_stage_type_vars.entry(name).or_insert(kind_str);
            }
        }

        // Return same TypeContext Arc (mutations visible through it)
        ok_val(
            Value::TypeContext {
                ctx: tc_arc,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-program`: Construct a `Value::Program` from a sequence of Document values.
///
/// Takes a single positional argument: a Seq or Dict of `Value::Document` values.
/// Returns a `Value::Program` with the documents wrapped in a `SurfaceProgram` structure.
///
/// This is the primitive for reconstructing programs after transformation (e.g., desugar.llt).
/// The resolution table is initialized as empty — callers should use `builtin-resolve`
/// or other builtins to populate it if needed.
///
/// Example usage in desugar.llt:
/// ```llt
/// [program [map desugar-document p.documents]]
/// ```
pub(crate) fn builtin_program(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let val = args[0].require_value()?.clone();

        // Extract documents from the input collection (Seq or Dict)
        let mut documents = Vec::new();

        match val {
            Value::Dict { entries: map, .. } => {
                // Iterate through dict entries in insertion order
                for (_key, thunk) in map.into_iter() {
                    let doc_val = crate::eval::materialize(&thunk, Some(&call_span), &ctx).await?;
                    match doc_val {
                        Value::Document {
                            doc: surface_doc, ..
                        } => {
                            // Use Arc::clone to preserve OnceLocks (resolver coordinates).
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

        // Construct the SurfaceProgram and return it as Value::Program.
        let surface_program = crate::ast::SurfaceProgram { documents };

        // Return as Value::Program with empty tables (caller can run expand/resolve if needed)
        ok_val(
            Value::Program {
                program: std::sync::Arc::new(surface_program),
                resolutions: std::sync::Arc::new(Default::default()),
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-desugar`: Desugar a `Value::Program`.
///
/// Pure functional wrapper around `desugar_program_full`. Takes 1 arg: Value::Program.
/// Returns a new Value::Program with desugaring applied.
pub(crate) fn builtin_desugar(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-desugar", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let program_val = args[0].require_value()?.clone();
        let (program, resolutions) = match program_val {
            Value::Program {
                program,
                resolutions,
                ..
            } => (program, resolutions),
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

        let desugared = crate::desugar::desugar_program_full(&program);

        ok_val(
            Value::Program {
                program: std::sync::Arc::new(desugared),
                resolutions,
                type_val: crate::value::unknown_type_val(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-program-docs", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        let program_val = args[0].require_value()?.clone();
        let program_arc = match program_val {
            Value::Program { program, .. } => program,
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

        // Collect Arc::clone handles for each document in the program.
        let doc_arcs: Vec<(usize, Arc<crate::ast::SurfaceDocument>)> = program_arc
            .documents
            .iter()
            .enumerate()
            .map(|(i, spanned)| (i, Arc::clone(&spanned.node)))
            .collect();
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, doc_arc) in doc_arcs {
            let doc_thunk = Arc::new(Thunk::value(
                Value::Document {
                    doc: doc_arc,
                    resolver_frames: Arc::new(Vec::new()),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span.clone(),
            ));
            result.insert(HashableValue::Int(i as i64), doc_thunk);
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
/// This replaces the `Value::Document.expressions` internal access backdoor that was
/// deleted from builtins_dict.rs (T-1605 follow-up, S-926 R4).
pub(crate) fn builtin_doc_expressions(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let doc_val = args[0].require_value()?.clone();
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
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

        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let mut i = 0usize;
        for item in &doc_arc.items {
            if let crate::ast::SurfaceItem::Expr(node) = item {
                let expr_val = crate::surface_convert::surface_node_to_expr_variant(node, &ctx);
                let thunk = Arc::new(Thunk::value(expr_val, call_span.clone()));
                result.insert(HashableValue::Int(i as i64), thunk);
                i += 1;
            }
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-doc-meta`: Evaluate document header metadata.
///
/// Evaluates the header values of a document. Takes 2 args:
/// - arg0: Value::Document
/// - arg1: Dict — the accumulated env dict (env-dict protocol, T-1775)
///
/// Header values are almost always literals (stage: "type", pragma: ["no-prelude"]),
/// so the env is not needed for evaluation.  The argument is accepted and type-checked
/// but the env dict is not injected into the evaluation context — header evaluation
/// always runs with an empty initial_group so that non-literal header expressions
/// still evaluate in an isolated scope without accidentally resolving runtime names
/// from the caller's env.
///
/// Returns Dict {key: evaluated-value, ...}.
/// Returns empty Dict if header is empty.
pub(crate) fn builtin_doc_meta(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let doc_val = args[0].require_value()?.clone();
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
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

        // arg1: env-dict (T-1775 protocol) — must be Dict.
        // The value is accepted but not used for header evaluation — doc headers contain
        // literal values (strings, lists of strings) that do not require name resolution.
        let env_val = args[1].require_value()?.clone();
        match &env_val {
            Value::Dict { .. } => {}
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-doc-meta".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        }

        // Evaluate each header SurfaceNode value with an empty accumulated group.
        // Header values are literals — no env injection needed.
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (key, node_arc) in &doc_arc.header {
            let nodes = vec![Arc::clone(node_arc)];
            let eval_result = crate::eval::eval_document_exprs_with_env(&nodes, &ctx, None).await;
            match eval_result {
                Ok(thunk) => {
                    result.insert(HashableValue::Str(key.clone().into()), thunk);
                }
                Err(e) => return Err(e),
            }
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_val: crate::value::unknown_type_val(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
                let mut dict_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for def in defs {
                    let name_arc: Arc<str> = Arc::from(def.name);
                    let builtin_thunk = Arc::new(Thunk::value(
                        Value::Builtin {
                            def,
                            type_val: crate::value::unknown_type_val(),
                        },
                        call_span.clone().with_name(Arc::clone(&name_arc)),
                    ));
                    dict_map.insert(HashableValue::Str(name_arc), builtin_thunk);
                }
                ok_val(
                    Value::Dict {
                        entries: dict_map,
                        type_val: crate::value::unknown_type_val(),
                    },
                    call_span,
                )
            }
            None => Err(EvalError::user_error(
                format!("unknown native module: {:?}", name),
                call_span,
            )
            .into()),
        }
    })
}

/// `builtin-eval`: Evaluate a `Value::CoreDocument` against an env-dict, returning the exports dict.
///
/// Takes 2 positional args:
/// - arg0: `Value::CoreDocument` — fully lowered document (output of `builtin-lower`).
///   Passing `Value::Document` is a type error: call `builtin-lower` first.
/// - arg1: `Value::Dict` — env-dict: accumulated environment mapping names to thunks (in
///   insertion order). Values become the initial accumulated group for
///   `eval_core_document_exprs`, enabling `LetrecGroupMember(i)` references into the env.
///   The insertion order of the env-dict must match the name-set order passed to `builtin-resolve`
///   (i.e., the slot assignments from the resolver frame).
///
/// Returns `Value::Dict` — the exports dict produced by the document's last expression.
/// Errors propagate via `EvalError` (raises on failure — no diagnostics wrapper).
///
/// Accepts `Value::CoreDocument` (output of `builtin-lower`), not `Value::Document`.
/// The env-dict protocol passes a `Dict<String, Arc<Thunk>>` as the second argument,
/// replacing the old scope-id Int parameter.
pub(crate) fn builtin_eval(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        // arg0: Value::CoreDocument — reject Value::Document with a descriptive type error.
        let doc_val = args[0].require_value()?.clone();
        let (core_entries, doc_span) = match doc_val {
            Value::CoreDocument { entries, span, .. } => (entries, span),
            Value::Document { .. } => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-eval: expected CoreDocument (output of builtin-lower), got Document; \
                     call builtin-lower before builtin-eval"
                        .to_string(),
                    "CoreDocument",
                    "Document",
                    call_span,
                )
                .into());
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-eval".to_string(),
                    "CoreDocument",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // arg1: Value::Dict — env-dict: name → thunk.
        // Extract thunks in insertion order; these follow root_group thunks in initial_group.
        // The env-dict keys must be in the same insertion order as the name-set passed to
        // builtin-resolve, which determines the LGM slot assignments (with root_group_len offset).
        let env_val = args[1].require_value()?.clone();
        let env_map = match env_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-eval env-dict".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // Build initial_group: prepend root_group thunks, then append env-dict thunks.
        // LGM slots 0..R-1 are root_group entries (builtins + capabilities).
        // Env-dict entries follow at R..R+N-1, matching the resolver's root_group_len offset
        // assigned by builtin_resolve.
        let mut initial_group: Vec<Arc<Thunk>> = ctx.root_group.iter().map(Arc::clone).collect();
        initial_group.extend(env_map.iter().filter_map(|(k, v)| {
            if matches!(k, HashableValue::Str(_)) {
                Some(Arc::clone(v))
            } else {
                None
            }
        }));

        // Evaluate the pre-lowered CoreExpr entries with the env-dict as the initial group.
        // Uses eval_core_document_exprs (not eval_document_exprs_with_env) to avoid calling
        // lower() on already-lowered expressions. Errors propagate directly — no diagnostics
        // wrapping.
        let result_thunk =
            crate::eval::eval_core_document_exprs(&core_entries[..], &ctx, initial_group).await?;

        // Materialize the result to a Dict (the exports).
        // The last expression in a document is the exports dict — force it so callers receive
        // a concrete Value::Dict they can iterate to build the next env-dict.
        let result_val = materialize(&result_thunk, Some(&doc_span), &ctx).await?;
        Ok(Arc::new(Thunk::value(result_val, call_span)))
    })
}

/// `builtin-variant-payload`: extract the payload from a Variant, returning it directly.
/// Takes 1 arg (a Variant). Returns the payload value (forces the payload thunk).
/// Used to extract values from Result.Ok/Error without going through key-based lookup (which
/// fails when the payload is a non-dict value like String).
pub(crate) fn builtin_variant_payload(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let val = args[0].require_value()?.clone();
        match val {
            Value::Variant {
                payload: Some(payload_id),
                ..
            } => {
                let payload_thunk = Arc::clone(&payload_id);
                let payload_val = materialize(&payload_thunk, Some(&call_span), &ctx).await?;
                ok_val(payload_val, call_span)
            }
            Value::Variant { payload: None, .. } => ok_val(
                Value::Dict {
                    entries: indexmap::IndexMap::new(),
                    type_val: crate::value::unknown_type_val(),
                },
                call_span,
            ),
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

/// `builtin-current-env`: capture and return the calling FlatEnv id.
///
/// Takes zero arguments. Returns `Value::Int(caller_env_id)` — the FlatEnv id of the
/// caller's evaluation scope. This is the env_id in scope at the `[builtin-current-env]`
/// call site.
pub(crate) fn builtin_current_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        // caller_env_id is Some only when needs_caller_env = true in the BuiltinDef.
        // Panicking here indicates a registration bug: builtin-current-env was registered
        // without the @needs_caller_env flag.
        let env_id = caller_env_id.expect(
            "builtin-current-env: caller_env_id is None — BuiltinDef.needs_caller_env must be true",
        );
        Ok(Arc::new(Thunk::value(
            Value::Int {
                n: env_id as i64,
                type_val: crate::value::unknown_type_val(),
            },
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named: _,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // ── Step 2: Convert Expr.* variant → SurfaceNode ─────────────────────
        let expr_val = args[0].require_value()?.clone();

        let expr_node = match expr_val {
            Value::Variant { ref ctor, .. }
                if crate::value::tycon_name_from_ctor(ctor.as_ref()) == "Expr" =>
            {
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
        let program = crate::desugar::desugar_program_full(&crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_span.clone(),
            )],
        });

        // ── Step 4: Desugar + resolve ─────────────────────────────────────────
        // Resolve with empty frames — ScopeArena was deleted; de Bruijn coordinates
        // resolve via MAX/MAX name-based lookup at eval time.
        let initial_frames: Vec<indexmap::IndexMap<String, u32>> = vec![];
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
        let result_thunk =
            crate::eval::eval_document_exprs_with_env(&expression_nodes, &ctx, None).await?;

        Ok(result_thunk)
    })
}

/// `eval-types`: same as `eval` but evaluates in the type-stage environment.
///
/// This is used for evaluating type-level expressions (type aliases, class declarations).
pub(crate) fn builtin_eval_types(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        // Reject all named args — eval-types takes only the positional document arg.
        crate::builtins::reject_named("eval-types", named.as_ref(), call_span.clone())?;

        // Materialize the input — accepts Value::Document only.
        let input_val = args[0].require_value()?.clone();

        // Document path: extract SurfaceNodes directly (same as builtin-eval Document path).
        if let Value::Document { doc, .. } = &input_val {
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
            let result_thunk =
                crate::eval::eval_document_exprs_with_env(&expr_nodes, &ctx, None).await?;
            return Ok(result_thunk);
        }

        let input_map = match input_val {
            Value::Dict { entries: m, .. } => m,
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
        for (_key, val_thunk) in &input_map {
            let val = materialize(val_thunk, Some(&call_span), &ctx).await?;
            match val {
                Value::Variant { ref ctor, .. }
                    if crate::value::tycon_name_from_ctor(ctor.as_ref()) == "Expr" =>
                {
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

        // Build integer-keyed Dict of CoreExpr thunks (one per expression, pre-lowered).
        let scope_frames = ctx.scope_frames.as_ref().map(|v| v.as_slice());
        let mut result_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, node) in expression_nodes.into_iter().enumerate() {
            let (lowered, lower_diags) = crate::lower::lower(&node, scope_frames);
            if let Some(err) = crate::eval_materialize::lower_errors_to_eval_error(lower_diags) {
                return Err(err.into());
            }
            let core_thunk = Arc::new(Thunk::core_expr(
                Arc::new(lowered),
                crate::value::EvalFrame::empty(),
                Arc::clone(&ctx),
                call_span.clone(),
            ));
            result_dict.insert(HashableValue::Int(i as i64), core_thunk);
        }

        ok_val(
            Value::Dict {
                entries: result_dict,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        // Expect exactly 2 args: schema, data
        let (schema, data) = expect_two_args("validate", &args, named.as_ref(), call_span.clone())?;

        // Schema must be a Dict
        let schema_dict = match schema {
            Value::Dict { entries: d, .. } => d,
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
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
) -> EvalResult<(Value, Value)> {
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }
    if named.is_some() && !named.unwrap().is_empty() {
        return Err(EvalError::named_arg_rejected(name.to_string(), call_span).into());
    }

    let val1 = args[0].require_value()?.clone();
    let val2 = args[1].require_value()?.clone();

    Ok((val1, val2))
}

/// Return type alias for async-recursive `validate_value`.
type ValidationFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<Vec<(String, String)>>> + Send>>;

/// Recursive validation helper.
///
/// `path` is the dot-separated field path (e.g., "user.address.zip").
/// Returns a list of violations found; an empty list means the value is valid.
fn validate_value(
    schema: IndexMap<HashableValue, Arc<Thunk>>,
    data: Value,
    path: String,
    ctx: Arc<crate::eval::EvalContext>,
    span: Span,
) -> ValidationFuture {
    Box::pin(async move {
        use crate::value::HashableValue;
        let mut violations: Vec<(String, String)> = Vec::new();

        // Check `type` constraint
        if let Some(type_thunk) = schema.get(&HashableValue::Str("type".into())) {
            let type_val = materialize(type_thunk, Some(&span), &ctx).await?;
            if let Value::String {
                source, start, end, ..
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
        if let Some(min_thunk) = schema.get(&HashableValue::Str("min".into())) {
            let min_val = materialize(min_thunk, Some(&span), &ctx).await?;
            match (&data, &min_val) {
                (Value::Int { n, .. }, Value::Int { n: min, .. }) if n < min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Float { n, .. }, Value::Float { n: min, .. }) if n < min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Int { n, .. }, Value::Float { n: min, .. }) if (*n as f64) < *min => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                (Value::Float { n, .. }, Value::Int { n: min, .. }) if *n < (*min as f64) => {
                    violations.push((path.clone(), format!("must be >= {}", min)));
                }
                _ => {}
            }
        }

        if let Some(max_thunk) = schema.get(&HashableValue::Str("max".into())) {
            let max_val = materialize(max_thunk, Some(&span), &ctx).await?;
            match (&data, &max_val) {
                (Value::Int { n, .. }, Value::Int { n: max, .. }) if n > max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Float { n, .. }, Value::Float { n: max, .. }) if n > max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Int { n, .. }, Value::Float { n: max, .. }) if (*n as f64) > *max => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                (Value::Float { n, .. }, Value::Int { n: max, .. }) if *n > (*max as f64) => {
                    violations.push((path.clone(), format!("must be <= {}", max)));
                }
                _ => {}
            }
        }

        // Check string/sequence length constraints
        if let Some(min_len_thunk) = schema.get(&HashableValue::Str("min-length".into())) {
            let min_len_val = materialize(min_len_thunk, Some(&span), &ctx).await?;
            if let Value::Int { n: min_len, .. } = min_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                        ..
                    } => Some((end - start) as i64),
                    Value::Dict { entries: d, .. } => Some(d.len() as i64),
                    _ => None,
                };
                if let Some(len) = actual_len {
                    if len < min_len {
                        violations.push((path.clone(), format!("length must be >= {}", min_len)));
                    }
                }
            }
        }

        if let Some(max_len_thunk) = schema.get(&HashableValue::Str("max-length".into())) {
            let max_len_val = materialize(max_len_thunk, Some(&span), &ctx).await?;
            if let Value::Int { n: max_len, .. } = max_len_val {
                let actual_len = match &data {
                    Value::String {
                        source: _,
                        start,
                        end,
                        ..
                    } => Some((end - start) as i64),
                    Value::Dict { entries: d, .. } => Some(d.len() as i64),
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
        if let Some(pattern_thunk) = schema.get(&HashableValue::Str("pattern".into())) {
            let pattern_val = materialize(pattern_thunk, Some(&span), &ctx).await?;
            if let Value::String {
                ref source,
                start,
                end,
                ..
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
                        Err(e) => {
                            violations.push((
                                path.clone(),
                                format!("invalid regex pattern {:?}: {}", pattern_str, e),
                            ));
                        }
                    }
                }
            }
        }

        // Check enum constraint.
        // Uses primitive_eq — only primitive types (Int, Float, String, unit Variant)
        // are compared. Dict/payload-Variant are not structurally compared.
        if let Some(enum_thunk) = schema.get(&HashableValue::Str("enum".into())) {
            let enum_val = materialize(enum_thunk, Some(&span), &ctx).await?;
            if let Value::Dict {
                entries: enum_dict, ..
            } = enum_val
            {
                // Pre-materialize all enum values, then check membership via primitive equality.
                let mut allowed_values = Vec::with_capacity(enum_dict.len());
                for (_key, val_thunk) in enum_dict.iter() {
                    allowed_values.push(materialize(val_thunk, Some(&span), &ctx).await?);
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
        if let Some(fields_thunk) = schema.get(&HashableValue::Str("fields".into())) {
            let fields_val = materialize(fields_thunk, Some(&span), &ctx).await?;
            if let Value::Dict {
                entries: fields_schema,
                ..
            } = fields_val
            {
                if let Value::Dict {
                    entries: ref data_dict,
                    ..
                } = data
                {
                    // Validate each field in the schema
                    for (field_key, field_schema_thunk) in &fields_schema {
                        let field_schema_val =
                            materialize(field_schema_thunk, Some(&span), &ctx).await?;
                        if let Value::Dict {
                            entries: field_schema,
                            ..
                        } = field_schema_val
                        {
                            let field_name = field_key.to_string();

                            let field_path = if path.is_empty() {
                                field_name.clone()
                            } else {
                                format!("{}.{}", path, field_name)
                            };

                            // Check if field is required
                            let is_required = if let Some(req_thunk) =
                                field_schema.get(&HashableValue::Str("required".into()))
                            {
                                let req_val = materialize(req_thunk, Some(&span), &ctx).await?;
                                matches!(&req_val, Value::Int { n, .. } if *n != 0)
                            } else {
                                false
                            };

                            if let Some(field_value_thunk) = data_dict.get(field_key) {
                                let field_value =
                                    materialize(field_value_thunk, Some(&span), &ctx).await?;
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
        if let Some(items_thunk) = schema.get(&HashableValue::Str("items".into())) {
            let items_val = materialize(items_thunk, Some(&span), &ctx).await?;
            if let Value::Dict {
                entries: items_schema,
                ..
            } = items_val
            {
                if let Value::Dict {
                    entries: data_dict, ..
                } = data
                {
                    for (idx, (_key, val_thunk)) in data_dict.iter().enumerate() {
                        let val = materialize(val_thunk, Some(&span), &ctx).await?;
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
            }
        }

        Ok(violations)
    })
}

/// `builtin-is-contractive`: check whether a TypeNode value is contractive.
///
/// A type node is contractive iff every path from the root to a `RecursiveRef`
/// constructor passes through at least one *guarding* constructor.
/// `Union` and `Intersect` are **not** guarding — they are logical combinators.
/// All other constructors are guarding.
///
/// Three-case algorithm (from `doc/whatif/equirecursive-types.md §Contractiveness`):
///
/// 1. If the ctor is `RecursiveRef` → **not contractive** (bare self-reference).
/// 2. If the ctor is guarding (not Union/Intersect) → **contractive**.
/// 3. If the ctor is `Union` or `Intersect` → recurse into the `types` child;
///    all children must be contractive.
///
/// Prelude-agnostic: matches on constructor names only, not the tycon name.
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let result = is_contractive_value(&body_val, &ctx).await?;
        ok_val(
            Value::Int {
                n: if result { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// Recursively check contractiveness of a type node Variant value.
///
/// Returns `true` iff the node is contractive — i.e., every path to a
/// `RecursiveRef` ctor passes through at least one guarding constructor.
///
/// Prelude-agnostic: matches on constructor names only, not the tycon name.
/// The ctor names "RecursiveRef", "Union", and "Intersect" are the Rust-level protocol
/// that the prelude's TypeNode type must implement. These names are effectively reserved:
/// any TypeNode variant whose constructor is named "RecursiveRef", "Union", or "Intersect"
/// will be treated as non-contractive, regardless of the surrounding tycon name.
fn is_contractive_value<'a>(
    val: &'a Value,
    ctx: &'a Arc<crate::eval::EvalContext>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = EvalResult<bool>> + Send + 'a>> {
    Box::pin(async move {
        // Unwrap Value::Annotated transparently — annotations do not affect contractiveness.
        let val = match val {
            Value::Annotated { inner, .. } => inner.as_ref(),
            other => other,
        };

        // Helper to extract the bare constructor name from a potentially qualified ctor.
        // e.g. "TypeNode.RecursiveRef" → "RecursiveRef", "RecursiveRef" → "RecursiveRef"
        let bare = |ctor: &Arc<str>| {
            ctor.as_ref()
                .split_once('.')
                .map(|(_, c)| c)
                .unwrap_or(ctor.as_ref())
                .to_string()
        };

        match val {
            // Case 1: bare RecursiveRef — non-contractive.
            Value::Variant { ctor, .. } if bare(ctor) == "RecursiveRef" => Ok(false),

            // Case 3: Union and Intersect are non-guarding — recurse into all children.
            Value::Variant { ctor, payload, .. }
                if bare(ctor) == "Union" || bare(ctor) == "Intersect" =>
            {
                let payload_thunk = match payload {
                    Some(id) => id.clone(),
                    None => return Ok(true),
                };
                let payload_val = materialize(&payload_thunk, None, ctx).await?;
                let types_thunk = match &payload_val {
                    Value::Dict { entries: d, .. } => {
                        match d.get(&crate::value::HashableValue::Str("types".into())) {
                            Some(t) => Arc::clone(t),
                            None => return Ok(true),
                        }
                    }
                    _ => return Ok(true),
                };
                is_contractive_seq(types_thunk, ctx).await
            }

            // Case 2: all other constructors are guarding.
            _ => Ok(true),
        }
    })
}

/// Check that every element in the `types` Dict is contractive.
///
/// Union/Intersect.types is an integer-keyed Dict of type node variants.
/// Returns `Ok(true)` iff all values are contractive. Empty or malformed input returns `Ok(true)`
/// (conservative: no self-references = trivially contractive).
/// Returns `Err(e)` if materialization of the types dict or any element fails.
async fn is_contractive_seq(
    types_thunk: Arc<crate::value::Thunk>,
    ctx: &Arc<crate::eval::EvalContext>,
) -> EvalResult<bool> {
    let val = materialize(&types_thunk, None, ctx).await?;
    match &val {
        Value::Dict { entries: d, .. } => {
            for (_k, v_thunk) in d {
                let v_val = materialize(v_thunk, None, ctx).await?;
                if !is_contractive_value(&v_val, ctx).await? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true), // Not a Dict — no children to check.
    }
}

/// Walk a Seq spine and validate each element against `items_schema`.
///
/// Separated from `validate_value` so both can be async-recursive without
/// requiring a mutually-recursive `Box::pin` type cycle. Takes owned parameters
/// to enable `async move`.
///
/// `builtin-sequential`: construct a Sequential AST node from an expressions dict.
///
/// Used by boot-level macros (`>>` in loader.llt and test-loader.llt) that need
/// to produce Sequential before the prelude's `Expr` type is in scope.
///
/// Arg 0: integer-keyed dict of `Expr.*` variants (the expressions to sequence).
/// Returns: `Value::Variant { tag: "Expr.Sequential", payload: Some({exprs: dict}) }`.
pub(crate) fn builtin_sequential(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
        let arg0_thunk = Arc::clone(&args[0]);
        let exprs_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        // Extract each entry in insertion order as Expr.* variant, converting to SurfaceNode
        let exprs_dict = match &exprs_val {
            Value::Dict { entries: d, .. } => d,
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
        for (_key, thunk) in exprs_dict.iter() {
            let val = materialize(thunk, Some(&call_span), &ctx).await?;
            match val {
                Value::Variant { ref ctor, .. }
                    if crate::value::tycon_name_from_ctor(ctor.as_ref()) == "Expr" =>
                {
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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
            match named_map.get("call-site-span").cloned() {
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
        let call_site_span_thunk = call_site_span_thunk_id;
        let call_site_span_val = materialize(&call_site_span_thunk, Some(&call_span), &ctx).await?;
        let call_site_span_actual = match &call_site_span_val {
            Value::Dict { entries: dict, .. } => {
                // Extract span from the dict using extract_span
                crate::surface_convert::extract_span(dict, &ctx).ok_or_else(|| {
                    EvalError::type_mismatch_ctx(
                        "ast-to-program".to_string(),
                        "valid span dict (with start/end line/col fields)",
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
        let expr_val = args[0].require_value()?.clone();

        // Convert Expr.* Variant to SurfaceNode
        let expr_node = match expr_val {
            Value::Variant { ref ctor, .. }
                if crate::value::tycon_name_from_ctor(ctor.as_ref()) == "Expr" =>
            {
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

        // Return Value::Program with the SurfaceProgram stored directly.
        ok_val(
            Value::Program {
                program: std::sync::Arc::new(program),
                resolutions: Arc::new(Default::default()),
                type_val: crate::value::unknown_type_val(),
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
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let type_name_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let type_name = require_string("check-type", type_name_val, arg0_thunk.span.clone())?;

        let value = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;

        let passes = match type_name.as_str() {
            "String" => matches!(value, Value::String { .. }),
            "Int" => matches!(value, Value::Int { .. }),
            "Float" => matches!(value, Value::Float { .. }),
            "Dict" => matches!(value, Value::Dict { .. }),
            "Bytes" => matches!(value, Value::Bytes { .. }),
            // Unknown annotations (type variables, parameterized types, user-defined names)
            // pass conservatively — runtime cannot distinguish them without full evaluation.
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
/// - arg 1: Dict — the accumulated env dict to check (env-dict protocol, T-1775)
/// - Returns: `Int(1)` if the name is a key in the env dict,
///   `Int(0)` otherwise. Callers match on `Int(0)` for absent, non-zero for present
///   (no prelude conversion wrapper — loader.llt and test-loader.llt match directly).
///
/// Used by tinct-side caps enforcement (T-1507) as the primitive that tinct code calls
/// to validate declared caps against the accumulated env dict.
pub(crate) fn builtin_cap_env_has(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
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

        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let name_val = materialize(&arg0_thunk, Some(&call_span), &ctx).await?;
        let name = require_string("cap-env-has?", name_val, arg0_thunk.span.clone())?;

        let env_val = materialize(&arg1_thunk, Some(&call_span), &ctx).await?;
        // Check whether name is a key in the env-dict (env-dict protocol, T-1775).
        let found = match &env_val {
            Value::Dict { entries: d, .. } => {
                d.contains_key(&HashableValue::Str(Arc::from(name.as_str())))
            }
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-cap-env-has?".to_string(),
                    "Dict",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        // Return Int: 1 = found, 0 = not found.
        // The prelude wrapper converts Int → Boolean (prelude-agnostic protocol).
        ok_val(
            Value::Int {
                n: if found { 1 } else { 0 },
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// `builtin-lower`: Eagerly lower a `Value::Document` to a `Value::CoreDocument`.
///
/// Takes 1 positional arg: a `Value::Document` (the resolved+typechecked document from
/// the `doc:` field of the `builtin-typecheck-doc` result dict).
///
/// Returns `Value::CoreDocument` with all expression items lowered from `SurfaceNode` to
/// `Arc<Spanned<CoreExpr>>`. This is the discrete lowering step that separates the
/// surface-to-core transformation from evaluation. After this call, the document is ready
/// for `builtin-eval` without any lazy Surface thunk creation.
///
/// Lowering errors (unresolvable variables, malformed AST) are returned as an EvalError.
///
/// This replaces the lazy `UnevaluatedState::Surface` path: previously, lowering happened
/// on-demand when a Surface thunk was first forced. With `builtin-lower`, lowering is a
/// named, explicit pipeline step: parse → resolve → typecheck-doc → lower → eval.
///
/// The document's `resolver_frames` (stored on `Value::Document` by `builtin-resolve`)
/// contain all scope frames (env_frame first, then Dict letrec and BlockBody frames in
/// natural nesting order) and are passed as `scope_frames` to `make_method_dispatcher_fn`.
/// This enables:
///
/// 1. `resolve_name_in_frames` (for builtin-raise and mangled instance names): searches all
///    frames innermost-first to find each name's absolute LGM slot.
///
/// 2. `resolve_name_in_parent_frames` (for cross-dict method chaining): searches ancestor
///    frames for the method name, skipping the current dict's own frame.
///    `slot` is the absolute LGM slot — the only value used at runtime (depth is ignored
///    by the evaluator and type checker resolves ClosureCapture by name).
pub(crate) fn builtin_lower(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = EvalResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        crate::builtins::reject_named("builtin-lower", named.as_ref(), call_span.clone())?;
        if args.len() != 1 {
            return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
        }

        // arg0: Value::Document
        let doc_val = args[0].require_value()?.clone();
        let (doc_arc, doc_resolver_frames) = match doc_val {
            Value::Document {
                doc: d,
                resolver_frames: f,
                ..
            } => (d, f),
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "builtin-lower".to_string(),
                    "Document",
                    other.type_name(),
                    call_span,
                )
                .into());
            }
        };

        // resolver_frames already contains all scope frames (env_frame first, then Dict
        // letrec and BlockBody frames in natural nesting order). Pass directly as scope_frames.
        let scope_frames: Option<&[indexmap::IndexMap<String, u32>]> =
            if !doc_resolver_frames.is_empty() {
                Some(doc_resolver_frames.as_slice())
            } else {
                None
            };
        let mut entries: Vec<(
            String,
            std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        )> = Vec::new();
        let mut expr_idx: usize = 0;
        for item in doc_arc.items.iter() {
            let node = match item {
                crate::ast::SurfaceItem::Expr(node) => node,
                crate::ast::SurfaceItem::Decl(_) => continue,
            };
            let (core_spanned, lower_diags) = crate::lower::lower(node, scope_frames);
            if let Some(err) = crate::eval_materialize::lower_errors_to_eval_error(lower_diags) {
                return Err(err.into());
            }
            let key = format!("{expr_idx}");
            entries.push((key, std::sync::Arc::new(core_spanned)));
            expr_idx += 1;
        }

        let span = doc_arc
            .items
            .first()
            .and_then(|item| {
                if let crate::ast::SurfaceItem::Expr(node) = item {
                    Some(node.span.clone())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| call_span.clone());

        ok_val(
            Value::CoreDocument {
                entries: std::sync::Arc::new(entries),
                span,
                type_val: crate::value::unknown_type_val(),
            },
            call_span,
        )
    })
}

/// Returns all "meta" module Rust builtins.
///
/// These are the AST, evaluation, reflection, and macro builtins that are NOT in the
/// core_builtins() set. The core_builtins() items (builtin-parse, builtin-resolve,
/// builtin-typecheck-doc, builtin-eval, builtin-module, builtin-get-type-context,
/// builtin-make-type-ctx, builtin-tc-update-type-stage-env, builtin-variant-payload,
/// builtin-tag-of, builtin-llt-repr, builtin-type-of, builtin-cap-env-has?,
/// builtin-check-type, builtin-desugar, builtin-program-docs, builtin-doc-meta)
/// stay in core_builtins() for loader.llt.
///
/// Consumed exclusively by `builtin_module("meta")` in `src/builtins.rs`.
pub fn meta_builtins() -> Vec<crate::value::BuiltinDef> {
    use crate::builtins::builtin;
    use crate::value::Strictness;
    vec![
        // ── Pipeline lowering step ────────────────────────────────────────────────────
        builtin!(
            "builtin-lower",
            builtin_lower,
            [Strictness::Seq],
            1,
            ["doc"]
        ),
        // ── AST construction and evaluation ───────────────────────────────────────────
        builtin!("builtin-ast-of", builtin_ast_of, [Strictness::Id], 0, ["x"]),
        builtin!(
            @needs_caller_env,
            "builtin-eval-macro-ast",
            builtin_eval_macro_ast,
            [Strictness::Seq],
            1,
            ["ast"]
        ),
        builtin!(
            @needs_caller_env,
            "builtin-eval-types",
            builtin_eval_types,
            [Strictness::Seq],
            1,
            ["program"]
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
        builtin!(@needs_caller_env, "builtin-until", builtin_until),
        builtin!(
            @needs_caller_env,
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
        // needs_caller_env: true — only builtin that requires BuiltinArgs.caller_env_id to be Some.
        builtin!(@needs_caller_env, "builtin-current-env", builtin_current_env),
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

    /// Wrap a Value as an Arc<Thunk> for use in BuiltinArgs.args.
    fn thunk_id(val: Value) -> Arc<Thunk> {
        thunk(val)
    }

    fn call_span() -> crate::ast::Span {
        test_span(1, 1, 1, 5)
    }

    fn test_ctx() -> Arc<crate::eval::EvalContext> {
        crate::eval::EvalContext::new_empty()
    }

    fn no_named() -> Option<IndexMap<String, Arc<Thunk>>> {
        None
    }

    async fn run(
        f: impl std::future::Future<Output = EvalResult<Arc<Thunk>>>,
    ) -> EvalResult<Arc<Thunk>> {
        f.await
    }

    async fn materialize_sync(
        t: &Arc<Thunk>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> EvalResult<Value> {
        crate::eval::materialize(t, None, ctx).await
    }

    /// `builtin_tag_of` returns the tag of a bare `Value::Variant`.
    #[tokio::test]
    async fn tag_of_bare_variant() -> EvalResult<()> {
        let variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from("Color.Red"),
            payload: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(variant)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env_id: None,
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("Color.Red"));
        Ok(())
    }

    /// `builtin_tag_of` peels a single `Value::Annotated` wrapper and returns the
    /// inner variant's tag. This is the primary regression case for B-441.
    #[tokio::test]
    async fn tag_of_annotated_variant_single_wrap() -> EvalResult<()> {
        let variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from("SimpleType.Leaf"),
            payload: None,
        };
        let annotation = Value::Dict {
            entries: IndexMap::new(),
            type_val: crate::value::unknown_type_val(),
        };
        let annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(annotation),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env_id: None,
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("SimpleType.Leaf"));
        Ok(())
    }

    /// `builtin_tag_of` peels multiple nested `Value::Annotated` wrappers (the `while let`
    /// loop handles more than one layer of annotation).
    #[tokio::test]
    async fn tag_of_annotated_variant_double_wrap() -> EvalResult<()> {
        let variant = Value::Variant {
            type_val: crate::value::unknown_type_val(),
            ctor: Arc::from("Shape.Circle"),
            payload: None,
        };
        let inner_annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }),
        };
        let outer_annotated = Value::Annotated {
            inner: Box::new(inner_annotated),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(outer_annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env_id: None,
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("Shape.Circle"));
        Ok(())
    }

    /// `builtin_current_env` returns the caller's FlatEnv id as `Value::Int(caller_env_id)`.
    #[tokio::test]
    async fn current_env_captures_caller_env() -> EvalResult<()> {
        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),
            caller_env_id: Some(42),
        }))
        .await?;

        // builtin_current_env now returns Value::Int(caller_env_id).
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(
            val,
            Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val()
            },
            "expected caller_env_id as Int(42)"
        );
        Ok(())
    }

    /// `builtin_current_env` rejects positional arguments — it takes zero args.
    #[tokio::test]
    async fn current_env_rejects_positional_args() {
        let ctx = test_ctx();
        let result = run(builtin_current_env(BuiltinArgs {
            args: vec![thunk_id(Value::Int {
                n: 1,
                type_val: crate::value::unknown_type_val(),
            })],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: Some(0),
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
            inner: Box::new(Value::Int {
                n: 42,
                type_val: crate::value::unknown_type_val(),
            }),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_val: crate::value::unknown_type_val(),
            }),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx,
            caller_env_id: None,
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
