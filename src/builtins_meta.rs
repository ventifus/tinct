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
use crate::builtins::{ok_val, reject_named, require_string, require_type_context};
use crate::error::Diagnostic;
use crate::eval::{materialize, TypeContextData};
use crate::eval_materialize::make_span_dict;
use crate::rust_span;
use crate::value::{string_val, BuiltinArgs, HashableValue, Thunk, Value, TYPE_ID_DICT};

const KIND: &str = "eval";

// ── Unified error dict helpers ────────────────────────────────────────────────

/// Serialize a `crate::error::Diagnostic` into the unified diagnostic dict format
/// (level, kind, message, span, secondary-spans, notes, help, call-stack, macro-expand, blame).
///
/// `call_span` is the span used as the creation-time span for all emitted `Thunk`s.
///
/// The `macro-expand` field is serialized recursively: if `diag.macro_expand` is `Some`,
/// the inner `Diagnostic` is serialized by a recursive call and wrapped in a `Value::Dict`;
/// otherwise `{}` (empty dict) is emitted.
fn serialize_diagnostic(
    diag: &crate::error::Diagnostic,
    call_span: &crate::ast::Span,
) -> IndexMap<HashableValue, Arc<Thunk>> {
    let mk =
        |v: crate::value::Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
    let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> {
        crate::eval_materialize::make_span_dict(span, call_span)
    };

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

    // secondary-spans: spans[1..] as [{span, label}] dicts
    let mut secondary: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (j, sl) in diag.labels.iter().skip(1).enumerate() {
        let mut ss: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        ss.insert(HashableValue::Str("span".into()), mk_span(&sl.span));
        ss.insert(
            HashableValue::Str("label".into()),
            mk(string_val(&sl.message)),
        );
        secondary.insert(
            HashableValue::Int(j as i64),
            mk(crate::value::Value::Dict {
                entries: ss,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
    }
    w.insert(
        HashableValue::Str("secondary-spans".into()),
        mk(crate::value::Value::Dict {
            entries: secondary,
            type_decl_id: TYPE_ID_DICT,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_DICT,
            )),
        }),
    );

    // notes: integer-keyed dict of strings
    let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (j, note) in diag.notes.iter().enumerate() {
        notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
    }
    w.insert(
        HashableValue::Str("notes".into()),
        mk(crate::value::Value::Dict {
            entries: notes_dict,
            type_decl_id: TYPE_ID_DICT,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_DICT,
            )),
        }),
    );

    // children: serialized child diagnostics
    let mut children_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
    for (j, child) in diag.children.iter().enumerate() {
        let child_entries = serialize_diagnostic(child, call_span);
        children_dict.insert(
            HashableValue::Int(j as i64),
            mk(crate::value::Value::Dict {
                entries: child_entries,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
    }
    w.insert(
        HashableValue::Str("children".into()),
        mk(crate::value::Value::Dict {
            entries: children_dict,
            type_decl_id: TYPE_ID_DICT,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_DICT,
            )),
        }),
    );

    w
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let (result, diags) = crate::builtins::expect_one_arg(
            "materialize",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        ).into_parts();
        _dr_diags.extend(diags);
        let forced = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        ok_val(forced, call_span).with_diagnostics(_dr_diags)
    })
}

/// `raise`: takes 1 arg (Diagnostic dict), always raises.
/// Inherently materializing: constructs concrete error value.
/// The argument must be a Diagnostic dict with at minimum a `message:` String field.
/// Optional `notes:` field is an integer-keyed dict of Strings appended as notes.
pub(crate) fn builtin_raise(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let (result, diags) = crate::builtins::expect_one_arg(
            "raise",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        ).into_parts();
        _dr_diags.extend(diags);
        let val = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let arg_span = Arc::clone(&args[0]).span.clone();
        let (result, diags) = crate::builtins::require_dict("raise", val, arg_span.clone(), &ctx, call_span.clone())
                .await.into_parts();
        _dr_diags.extend(diags);
        let map = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

        // Extract required `message:` field
        let message_thunk = match map
            .get(&HashableValue::Str("message".into()))
            .ok_or_else(|| {
                Diagnostic::error(
                    KIND,
                    "raise: Diagnostic dict missing required `message:` field".to_string(),
                    call_span.clone(),
                )
            }) {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let (result, diags) = materialize(message_thunk, Some(&call_span), &ctx).await.into_parts();
        _dr_diags.extend(diags);
        let message_val = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let (result, diags) = require_string("raise", message_val, message_thunk.span.clone(), &ctx).into_parts();
        _dr_diags.extend(diags);
        let message = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

        let mut err = Diagnostic::error(KIND, message, call_span.clone());

        // Extract optional `notes:` field — integer-keyed dict of Strings
        if let Some(notes_thunk) = map.get(&HashableValue::Str("notes".into())) {
            let (result, diags) = materialize(notes_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let notes_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            let (result, diags) = crate::builtins::require_dict(
                "raise",
                notes_val,
                notes_thunk.span.clone(),
                &ctx,
                call_span.clone(),
            )
            .await.into_parts();
            _dr_diags.extend(diags);
            let notes_map = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            // Iterate in insertion order (integer keys 0, 1, 2, ...)
            for (_, note_thunk) in &notes_map {
                let (result, diags) = materialize(note_thunk, Some(&call_span), &ctx).await.into_parts();
                _dr_diags.extend(diags);
                let note_val = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };
                let (result, diags) = require_string("raise", note_val, note_thunk.span.clone(), &ctx).into_parts();
                _dr_diags.extend(diags);
                let note = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };
                err = err.with_note(note);
            }
        }

        crate::error::DiagnosticResult::err(err).with_diagnostics(_dr_diags)
    })
}

/// `builtin-macro-error`: takes 1-2 args (message string, optional AST node).
/// Creates a macro error with a precise source span.
/// If the optional node is provided, uses its span; otherwise uses call_span.
/// Arg 1 (message) is Strictness::Seq (materialized).
/// Arg 2 (node) is Strictness::Id (not materialized - stays as thunk).
pub(crate) fn builtin_macro_error(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        // Reject named arguments
        {
            let (result, diags) = crate::builtins::reject_named("builtin-macro-error", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        // Expect 1 or 2 arguments
        if args.len() != 1 && args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(if args.is_empty() { 1 } else { 2 }),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Extract message (first argument) - materialized by Strictness::Seq
        let arg0_thunk = Arc::clone(&args[0]);
        let msg_val = {
            let (result, diags) = arg0_thunk.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (result, diags) = require_string(
            "builtin-macro-error",
            msg_val,
            arg0_thunk.span.clone(),
            &ctx,
        ).into_parts();
        _dr_diags.extend(diags);
        let message = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

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
        crate::error::DiagnosticResult::err(Diagnostic::error(KIND, message, error_span)).with_diagnostics(_dr_diags)
    })
}

/// `try`: takes 1 arg (a zero-arg Function). Calls it. Returns `{ok: value}` on success
/// or a unified diagnostic dict (fields: level, kind, message, span, secondary-spans,
/// notes, call-stack, macro-expand, blame) on failure. Both branches are Dicts so callers
/// can always use `[builtin-has-key? "ok" raw]` to discriminate without a type check.
/// Inherently materializing: must materialize body to catch errors.
pub(crate) fn builtin_try(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("try", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        let arg_thunk = Arc::clone(&args[0]);
        let (call_result, diags) = materialize(&arg_thunk, Some(&call_span), &ctx).await.into_parts();
        _dr_diags.extend(diags);

        match call_result {
            Ok(val) => {
                // Success: {ok: value}. Caller uses [builtin-has-key? "ok" raw] to discriminate.
                // Both success and failure return Dicts so builtin-has-key? is always safe.
                let mut map = IndexMap::new();
                map.insert(
                    HashableValue::Str("ok".into()),
                    {
                        let (result, diags) = ok_val(val, call_span.clone()).into_parts();
                        _dr_diags.extend(diags);
                        match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        }
                    },
                );
                ok_val(
                    Value::Dict {
                        entries: map,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                )
                .with_diagnostics(_dr_diags)
            }
            Err(e) => {
                // ResourceLimitExceeded is non-catchable: it indicates a system-level limit.
                if e.message.contains("exceeded") && e.message.contains("limit") {
                    return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags);
                }
                // Failure: full diagnostic dict matching the unified protocol.
                // Discriminated by absence of "ok" key (success has "ok", failure does not).
                let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
                let mk_span =
                    |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };
                let primary_span = e.labels.first().map(|sl| &sl.span).unwrap_or(&call_span);
                let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                // Unified diagnostic protocol fields:
                let error_msg = e.message.clone();
                w.insert(HashableValue::Str("level".into()), mk(string_val("error")));
                w.insert(HashableValue::Str("kind".into()), mk(string_val(e.kind)));
                w.insert(
                    HashableValue::Str("message".into()),
                    mk(string_val(&error_msg)),
                );
                w.insert(HashableValue::Str("span".into()), mk_span(primary_span));
                // secondary-spans: spans[1..] as [{span, label}] dicts
                let mut secondary: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for (j, sl) in e.labels.iter().skip(1).enumerate() {
                    let mut ss: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                    ss.insert(HashableValue::Str("span".into()), mk_span(&sl.span));
                    ss.insert(
                        HashableValue::Str("label".into()),
                        mk(string_val(&sl.message)),
                    );
                    secondary.insert(
                        HashableValue::Int(j as i64),
                        mk(Value::Dict {
                            entries: ss,
                            type_decl_id: TYPE_ID_DICT,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_DICT,
                            )),
                        }),
                    );
                }
                w.insert(
                    HashableValue::Str("secondary-spans".into()),
                    mk(Value::Dict {
                        entries: secondary,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    }),
                );
                let mut notes_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for (j, note) in e.notes.iter().enumerate() {
                    notes_dict.insert(HashableValue::Int(j as i64), mk(string_val(note)));
                }
                w.insert(
                    HashableValue::Str("notes".into()),
                    mk(Value::Dict {
                        entries: notes_dict,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    }),
                );
                w.insert(
                    HashableValue::Str("help".into()),
                    mk(Value::Dict {
                        entries: IndexMap::new(),
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    }),
                );
                // call-stack: now empty — stack frames are merged into labels (secondary-spans above)
                let stack: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                w.insert(
                    HashableValue::Str("call-stack".into()),
                    mk(Value::Dict {
                        entries: stack,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    }),
                );
                let macro_val =
                    if let Some(child) = e.children.iter().find(|c| c.kind == "macro-expansion") {
                        let mut me: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                        me.insert(
                            HashableValue::Str("name".into()),
                            mk(string_val(&child.message)),
                        );
                        me.insert(
                            HashableValue::Str("span".into()),
                            mk_span(child.primary_span()),
                        );
                        Value::Dict {
                            entries: me,
                            type_decl_id: TYPE_ID_DICT,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_DICT,
                            )),
                        }
                    } else {
                        Value::Dict {
                            entries: IndexMap::new(),
                            type_decl_id: TYPE_ID_DICT,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_DICT,
                            )),
                        }
                    };
                w.insert(HashableValue::Str("macro-expand".into()), mk(macro_val));
                w.insert(
                    HashableValue::Str("blame".into()),
                    mk(Value::Dict {
                        entries: IndexMap::new(),
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    }),
                );
                ok_val(
                    Value::Dict {
                        entries: w,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                )
                .with_diagnostics(_dr_diags)
            }
        }
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        {
            let (result, diags) = reject_named("builtin-gensym", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-gensym takes exactly 2 arguments (scope, prefix), got {}",
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let get_str = |thunk: Arc<Thunk>,
                       name: &str,
                       span: &Span|
         -> crate::error::DiagnosticResult<String> {
            let mut inner_diags: Vec<crate::error::Diagnostic> = Vec::new();
            let val = {
                let (result, diags) = thunk.require_value().into_parts();
                inner_diags.extend(diags);
                match result {
                    Ok(v) => v.clone(),
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(inner_diags),
                }
            };
            match val {
                Value::String {
                    source, start, end, ..
                } => crate::error::DiagnosticResult::ok(source[start..end].to_string()).with_diagnostics(inner_diags),
                v => crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        format!("builtin-gensym {name}"),
                        "String",
                        ctx.type_display_name(v.type_decl_id_of())
                    ),
                    span.clone(),
                )).with_diagnostics(inner_diags),
            }
        };

        let (result, diags) = get_str(Arc::clone(&args[0]), "scope", &call_span).into_parts();
        _dr_diags.extend(diags);
        let scope_str = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let (result, diags) = get_str(Arc::clone(&args[1]), "prefix", &call_span).into_parts();
        _dr_diags.extend(diags);
        let prefix = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

        let scope = scope_str.chars().next().unwrap_or('ℊ');
        let name = gensym_fresh(scope, &prefix);
        ok_val(string_val(&name), call_span).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        let (result, diags) = crate::builtins::expect_one_arg(
            "macro-injects",
            &args,
            named.as_ref(),
            &ctx,
            call_span.clone(),
        ).into_parts();
        _dr_diags.extend(diags);
        let macro_name_val = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let (result, diags) = require_string(
            "macro-injects",
            macro_name_val,
            Arc::clone(&args[0]).span.clone(),
            &ctx,
        ).into_parts();
        _dr_diags.extend(diags);
        let macro_name = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

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
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `decimal`: Parse a string as an exact base-10 decimal (rust_decimal::Decimal).
/// Returns Value::Decimal. Error on invalid format.
pub(crate) fn builtin_decimal(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "decimal",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
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
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_DECIMAL,
                            )),
                        },
                        call_span,
                    ).with_diagnostics(_dr_diags),
                    Err(e) => crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("decimal: cannot parse \"{s}\": {e}"),
                        call_span,
                    )).with_diagnostics(_dr_diags),
                }
            }
            Value::Int { n, .. } => ok_val(
                Value::Decimal {
                    n: rust_decimal::Decimal::from(n),
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DECIMAL,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags),
            _ => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "type mismatch: expected {}, got {}",
                    "String or Int",
                    ctx.type_display_name(val.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `big-int`: Convert an Int or String to a BigInt (arbitrary-precision integer).
pub(crate) fn builtin_big_int(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "big-int",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match val {
            Value::Int { n, .. } => ok_val(
                Value::BigInt {
                    n: num_bigint::BigInt::from(n),
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_BIGINT,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags),
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
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_BIGINT,
                            )),
                        },
                        call_span,
                    ).with_diagnostics(_dr_diags),
                    Err(e) => crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("big-int: cannot parse \"{s}\": {e}"),
                        call_span,
                    )).with_diagnostics(_dr_diags),
                }
            }
            _ => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "type mismatch: expected {}, got {}",
                    "Int or String",
                    ctx.type_display_name(val.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `type-of`: takes 1 arg, materializes it, returns the TypeValue.Nominal(id) for the value.
///
/// Constructs TypeValue.Nominal from the value's type_decl_id (on Dict/Variant) or
/// the reserved TYPE_ID_* constant for primitive types.
pub(crate) fn builtin_type_of(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "type-of",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let type_id = val.type_decl_id_of();
        let tv = { crate::type_infer::make_typevalue_nominal(type_id) };
        crate::error::DiagnosticResult::ok(Arc::new(Thunk::value(tv.as_ref().clone(), call_span))).with_diagnostics(_dr_diags)
    })
}

/// `builtin-variant?`: test whether a value is a nominal variant (Value::Variant).
///
/// Returns Int(1) if the value (after peeling Annotated wrappers) is a Value::Variant,
/// Int(0) otherwise. Irreducible — tinct code cannot determine this without exhaustively
/// checking all known primitive type names, which is not future-proof.
pub(crate) fn builtin_is_variant(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "builtin-variant?",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let mut v = &val;
        while let Value::Annotated { inner, .. } = v {
            v = inner.as_ref();
        }
        let is_variant = matches!(v, Value::Variant { .. });
        ok_val(
            Value::Int {
                n: if is_variant { 1 } else { 0 },
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;

        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        // Reject named args and ensure exactly 1 arg
        {
            let (result, diags) = crate::builtins::reject_named("builtin-ast-of", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags);
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
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
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
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
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
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
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
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
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
                    string_val(err.kind),
                    call_span.clone(),
                )),
            );
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::value::Value::Dict {
                    entries,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
        }

        // Check for AstNodeField (runtime-v2: return the containing SurfaceNode as Expr.* variant)
        if let Some((node, _field)) = thunk.peek_ast_node_field() {
            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                crate::surface_convert::surface_node_to_expr_variant(&node, &ctx),
                call_span,
            ))).with_diagnostics(_dr_diags);
        }

        // Force the thunk if it is in CoreExpr unevaluated state (the only remaining state after
        // the peek checks above).  This is necessary for Value::Function, which only exists as a
        // materialized value — there is no way to reconstruct function metadata from the CoreExpr
        // body without evaluating it.  For already-materialized thunks this is a no-op (cached).
        {
            let (result, diags) = materialize(&thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Check for Materialized Value::Function — build a metadata dict:
        // {type: "fn", doc: ..., return-ann: ..., params: [...]}
        // materialize() succeeded above, so require_value() cannot fail here.
        // Propagate anyway — an impossible error must not be silently discarded.
        let val = {
            let (result, diags) = thunk.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        if let crate::value::Value::Function {
            clauses,
            annotation,
            ..
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
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span.clone(),
                )),
            };
            dict.insert(HashableValue::Str("return-ann".into()), return_ann_thunk);

            let first_clause_params = clauses
                .first()
                .map(|c| c.params.as_slice())
                .unwrap_or_default();
            let param_arcs: Vec<Arc<Thunk>> = {
                let collected: Result<Vec<_>, crate::error::Diagnostic> = first_clause_params
                    .iter()
                    .map(|p| {
                        let mut param_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                        param_dict.insert(
                            HashableValue::Str("name".into()),
                            Arc::new(crate::value::Thunk::value(
                                string_val(&p.node.name),
                                call_span.clone(),
                            )),
                        );
                        if let Some(ann) = &p.node.annotation {
                            let ann_thunk = crate::surface_convert::alloc_annotation(ann, &ctx);
                            param_dict.insert(HashableValue::Str("annotation".into()), ann_thunk);
                        }
                        Ok(Arc::new(crate::value::Thunk::value(
                            Value::Dict {
                                entries: param_dict,
                                type_decl_id: TYPE_ID_DICT,
                                type_val: Some(crate::type_infer::make_typevalue_nominal(
                                    crate::value::TYPE_ID_DICT,
                                )),
                            },
                            call_span.clone(),
                        )))
                    })
                    .collect();
                match collected {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            };

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
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span.clone(),
                )),
            );

            return crate::error::DiagnosticResult::ok(Arc::new(crate::value::Thunk::value(
                Value::Dict {
                    entries: dict,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ))).with_diagnostics(_dr_diags);
        }

        // ast-of is defined only for unevaluated expressions (AstNodeField) and
        // Value::Function. If the thunk holds any other materialized value, the caller
        // should have passed the unevaluated form instead.
        crate::error::DiagnosticResult::err(Diagnostic::error(
            KIND,
            format!(
                "type mismatch: expected {}, got {}",
                "Expr.*",
                ctx.type_display_name(val.type_decl_id_of())
            ),
            call_span,
        )).with_diagnostics(_dr_diags)
    })
}

/// `llt-repr`: takes 1 arg, materializes it recursively, returns its LLT display string representation.
/// This is the programmatic equivalent of the LLT display format (Int(42), Dict({...}), etc.).
/// Used by the `-o llt` output formatter.
pub(crate) fn builtin_llt_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "llt-repr",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        // value_to_tinct_repr_string produces tinct bracket format: ["key": value  ...]
        let display_str = {
            let (result, diags) = crate::value_to_tinct_repr_string(&val, &ctx, call_span.clone())
                .await
                .into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!("llt-repr: {}", e.kind),
                    call_span.clone(),
                )).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(&display_str), call_span).with_diagnostics(_dr_diags)
    })
}

/// `debug-repr`: Format a value as a Rust Debug string (DisplayVisitor format).
///
/// This produces the same output as the corpus test runner's DisplayVisitor format:
/// `Int(42)`, `Dict({...})`, `String("text")`, `Variant(Tag.Ctor, Null)`, etc.
/// Used by test-loader.llt for corpus test output formatting.
pub(crate) fn builtin_debug_repr(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "debug-repr",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        // value_to_display_string produces DisplayVisitor format: Int(42), Dict({...}), etc.
        let display_str = {
            let (result, diags) = crate::value_to_display_string(&val, &ctx, call_span.clone())
                .await
                .into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!("debug-repr: {}", e.kind),
                    call_span.clone(),
                )).with_diagnostics(_dr_diags),
            }
        };
        ok_val(string_val(&display_str), call_span).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "tag-of",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
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
            Value::Variant {
                type_decl_id,
                ctor_index,
                ..
            } => {
                let empty = std::collections::HashMap::new();
                let tycon_env = ctx.tycon_env().unwrap_or(&empty);
                ok_val(
                    string_val(&crate::type_tags::ctor_display_name(
                        *type_decl_id,
                        *ctor_index,
                        tycon_env,
                    )),
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            _ => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "type mismatch: expected {}, got {}",
                    "Variant",
                    ctx.type_display_name(peeled.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags),
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "span-of",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Extract span from Expr.* variant payload.
        if let Value::Variant {
            ref type_decl_id,
            payload: Some(payload_id),
            ..
        } = val
        {
            if *type_decl_id == crate::value::TYPE_ID_EXPR {
                let payload_thunk = Arc::clone(&payload_id);
                let payload_val = match payload_thunk.peek_result() {
                    Some(Ok(v)) => Some(v.clone()),
                    Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()).with_diagnostics(_dr_diags),
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
                            Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()).with_diagnostics(_dr_diags),
                            None => None,
                        };
                        if let Some(Value::Dict {
                            entries: span_dict, ..
                        }) = span_val
                        {
                            // Extract {start: {line, col}, end: {line, col}}
                            // Returns Ok(Some(...)) on success, Ok(None) for missing/wrong-type,
                            // Err(...) when a span thunk settled with an evaluation error.
                            let get_pos = |key: &str| -> crate::error::DiagnosticResult<
                                Option<(i64, i64)>,
                            > {
                                let pos_thunk = match span_dict.get(&HashableValue::Str(key.into()))
                                {
                                    Some(t) => t,
                                    None => return crate::error::DiagnosticResult::ok(None),
                                };
                                let pos_val = match pos_thunk.peek_result() {
                                    Some(Ok(v)) => v.clone(),
                                    Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()),
                                    None => return crate::error::DiagnosticResult::ok(None),
                                };
                                let Value::Dict {
                                    entries: pos_dict, ..
                                } = pos_val
                                else {
                                    return crate::error::DiagnosticResult::ok(None);
                                };
                                let line_thunk =
                                    match pos_dict.get(&HashableValue::Str("line".into())) {
                                        Some(t) => t,
                                        None => return crate::error::DiagnosticResult::ok(None),
                                    };
                                let line = match line_thunk.peek_result() {
                                    Some(Ok(v)) => v.clone(),
                                    Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()),
                                    None => return crate::error::DiagnosticResult::ok(None),
                                };
                                let col_thunk =
                                    match pos_dict.get(&HashableValue::Str("col".into())) {
                                        Some(t) => t,
                                        None => return crate::error::DiagnosticResult::ok(None),
                                    };
                                let col = match col_thunk.peek_result() {
                                    Some(Ok(v)) => v.clone(),
                                    Some(Err(e)) => return crate::error::DiagnosticResult::err((**e).clone()),
                                    None => return crate::error::DiagnosticResult::ok(None),
                                };
                                if let (Value::Int { n: l, .. }, Value::Int { n: c, .. }) =
                                    (line, col)
                                {
                                    crate::error::DiagnosticResult::ok(Some((l, c)))
                                } else {
                                    crate::error::DiagnosticResult::ok(None)
                                }
                            };

                            let (result, diags) = get_pos("start").into_parts();
                            _dr_diags.extend(diags);
                            let start_pos = match result {
                                Ok(v) => v,
                                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                            };
                            let (result, diags) = get_pos("end").into_parts();
                            _dr_diags.extend(diags);
                            let end_pos = match result {
                                Ok(v) => v,
                                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                            };
                            if let (Some((sl, sc)), Some((el, ec))) =
                                (start_pos, end_pos)
                            {
                                // Get the file from the thunk wrapping the Expr value.
                                let expr_thunk = Arc::clone(&args[0]);
                                let file_str = &expr_thunk.span.file;
                                let file_val = if !file_str.starts_with('<') {
                                    string_val(file_str.as_ref())
                                } else {
                                    Value::Dict {
                                        entries: IndexMap::new(),
                                        type_decl_id: TYPE_ID_DICT,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_DICT,
                                        )),
                                    }
                                };

                                let mk = |v: Value| Arc::new(Thunk::value(v, call_span.clone()));
                                let mut w: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                                w.insert(HashableValue::Str("file".into()), mk(file_val));
                                w.insert(
                                    HashableValue::Str("start-line".into()),
                                    mk(Value::Int {
                                        n: sl,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_INT,
                                        )),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("start-col".into()),
                                    mk(Value::Int {
                                        n: sc,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_INT,
                                        )),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("end-line".into()),
                                    mk(Value::Int {
                                        n: el,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_INT,
                                        )),
                                    }),
                                );
                                w.insert(
                                    HashableValue::Str("end-col".into()),
                                    mk(Value::Int {
                                        n: ec,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_INT,
                                        )),
                                    }),
                                );
                                return ok_val(
                                    Value::Dict {
                                        entries: w,
                                        type_decl_id: TYPE_ID_DICT,
                                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                                            crate::value::TYPE_ID_DICT,
                                        )),
                                    },
                                    call_span,
                                ).with_diagnostics(_dr_diags);
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
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-var-resolution", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 3 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(3),
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }
        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let arg2_thunk = Arc::clone(&args[2]);
        let line_val = {
            let (result, diags) = materialize(&arg0_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let cursor_line = match line_val {
            Value::Int { n, .. } => n as u32,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-var-resolution",
                        "Int",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };
        let col_val = {
            let (result, diags) = materialize(&arg1_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let cursor_col = match col_val {
            Value::Int { n, .. } => n as u32,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-var-resolution",
                        "Int",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };
        let prog_val = {
            let (result, diags) = materialize(&arg2_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let program_arc = match prog_val {
            Value::Program { program, .. } => program,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-var-resolution",
                        "Program",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
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
                SurfaceExpression::Dict { entries, scope: _ } => {
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
                if let Some(coords) = find_in_node(&doc_spanned.node.body, cursor_line, cursor_col) {
                    found = coords;
                    break 'outer;
                }
            }
            found
        };

        let mk = |v: Value| Arc::new(Thunk::value(v, call_span.clone()));
        match found {
            None => ok_val(
                Value::Dict {
                    entries: IndexMap::new(),
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags),
            Some(addr) => {
                use crate::ast::VarAddr;
                let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                let (addr_type, index_val): (&str, i64) = match addr {
                    VarAddr::Dispatch(_, slot) => ("letrec", slot as i64),
                    VarAddr::EffectPerform { .. } => ("effect-perform", 0),
                };
                result.insert(
                    HashableValue::Str("addr-type".into()),
                    mk(string_val(addr_type)),
                );
                result.insert(
                    HashableValue::Str("index".into()),
                    mk(Value::Int {
                        n: index_val,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_INT,
                        )),
                    }),
                );
                ok_val(
                    Value::Dict {
                        entries: result,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "annotation-of",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

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
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            Value::Annotated { annotation, .. } => {
                // Return the annotation value directly — no materialization needed,
                // it was stored materialized at annotation construction time.
                ok_val(*annotation, call_span).with_diagnostics(_dr_diags)
            }
            _ => {
                // All other values have no annotation — return empty dict.
                ok_val(
                    Value::Dict {
                        entries: IndexMap::new(),
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("make-annotated", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let inner_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let ann_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // annotation must be a Dict
        if !matches!(ann_val, Value::Dict { .. }) {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "type mismatch: expected {}, got {}",
                    "Dict",
                    ctx.type_display_name(ann_val.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        ok_val(
            Value::Annotated {
                inner: Box::new(inner_val),
                annotation: Box::new(ann_val),
                type_val: None,
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

// Type predicates are implemented in stdlib/prelude.llt via type pattern matching.

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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "blake3",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let s = {
            let (result, diags) = require_string("blake3", val, Arc::clone(&args[0]).span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let hex = blake3_hex(s.as_bytes());
        ok_val(string_val(hex.as_str()), call_span).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "cap-identity",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let dir = match &val {
            Value::DirCap { dir, .. } => dir.try_clone().expect("dir try_clone"),
            Value::RevocableDirCap { inner, revoked, .. } => {
                if revoked.load(std::sync::atomic::Ordering::Acquire) {
                    return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        "cap-identity: capability has been revoked".to_string(),
                        call_span.clone(),
                    ))
                    .with_diagnostics(_dr_diags);
                }
                inner.try_clone().expect("dir try_clone")
            }
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "DirCap",
                        ctx.type_display_name(val.type_decl_id_of())
                    ),
                    Arc::clone(&args[0]).span.clone(),
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Get the identity string from the directory's metadata.
        // On Unix: "dev:ino" from fstat on the O_DIRECTORY fd.
        // On non-Unix: "0:<hash>" as a best-effort fallback.
        #[cfg(unix)]
        let identity = {
            use cap_std::fs::MetadataExt;
            let meta = match dir.open(".").and_then(|f| f.metadata()) {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!("cap-identity: failed to stat directory: {e}"),
                    call_span.clone(),
                )).with_diagnostics(_dr_diags),
            };
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

        ok_val(string_val(identity.as_str()), call_span).with_diagnostics(_dr_diags)
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
///     `kind` is always `"parse"` for parse errors.
///     Empty dict (`[]`) when parse succeeded with no errors.
///
/// Callers should check `errors` before proceeding. The `program` is always present
/// (may be an empty program on fatal parse failure). Callers must call `builtin-resolve`
/// (desugar + resolve) and then `builtin-typecheck-doc` before passing the result to `builtin-eval`.
///
/// This is Stage 1 of the 4-stage pipeline: parse → resolve → typecheck → eval.
pub(crate) fn builtin_parse(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-parse", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        // First arg: Bytes (the source file contents)
        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let bytes_val = {
            let (result, diags) = arg0_thunk.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let source_bytes = match bytes_val {
            Value::Bytes {
                source, start, end, ..
            } => source[start..end].to_vec(),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-parse",
                        "Bytes",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        // Second arg: String (the path hint for error messages)
        let path_val = {
            let (result, diags) = arg1_thunk.require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let path_str = {
            let (result, diags) = require_string("builtin-parse", path_val, arg1_thunk.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Decode bytes as UTF-8 source text
        let source = match std::str::from_utf8(&source_bytes) {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-parse: UTF-8 decode error in \"{}\": {}",
                    path_str, e
                ),
                call_span.clone(),
            )).with_diagnostics(_dr_diags),
        }
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
        let all_parse_diagnostics: Vec<crate::error::Diagnostic> = if let Some(ref output) = parsed
        {
            output.diagnostics.clone()
        } else {
            fatal_diagnostics
        };

        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mut errors_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, diag) in all_parse_diagnostics.iter().enumerate() {
            let w = serialize_diagnostic(diag, &call_span);
            errors_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
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
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_PROGRAM,
            )),
        };

        // Return {program: Value::Program, diagnostics: integer-keyed Dict of diagnostic dicts}.
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result.insert(HashableValue::Str("program".into()), mk(program_value));
        result.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: errors_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
        crate::error::DiagnosticResult::ok(Arc::new(Thunk::value(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ))).with_diagnostics(_dr_diags)
    })
}

/// Compute the best available tinct source span for a document.
///
/// Returns the span of the first Expr item, falling back to the first header entry,
/// then to `call_span` (the builtin call site). Never returns a rust_span.
fn compute_doc_span(
    doc: &crate::ast::SurfaceDocument,
    _call_span: &crate::ast::Span,
) -> crate::ast::Span {
    doc.body.span.clone()
}

/// `builtin-resolve`: Resolve a single `Value::Document` in-place.
///
/// Takes 2 positional args: the document to resolve and a scope.
///
/// **Arguments:**
/// - arg0: `Value::Document` (only Document, not Program — error if Program)
/// - arg1: `Value::SymbolTable` — scope with name-to-slot mapping.
///
/// **Returns** `{doc: Document, diagnostics: Dict<Int, DiagnosticDict>}`.
///
/// Writes resolved `VarAddr` values into the inline `Resolution` OnceLocks on each
/// `VarRef`/`DotAccess` node of the document's AST. After this call, `builtin-lower` and
/// `builtin-eval` process the resolved nodes.
///
/// This is Stage 2 of the 4-stage pipeline (Stage 1 is parse from loader.llt).
pub(crate) fn builtin_resolve(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-resolve", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        // arg0: Document — TypeAssert on the parameter enforces this at the call boundary.
        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let doc_arc = if let Value::Document { doc: d, .. } = doc_val {
            d
        } else {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "expected Document, got {}",
                    ctx.type_display_name(doc_val.type_decl_id_of())
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        };

        // arg1: Value::SymbolTable — scope with name-to-slot mapping.
        let scope_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let table = match scope_val {
            Value::SymbolTable { table, .. } => table,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-resolve env",
                        "SymbolTable",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags);
            }
        };

        // Resolve the document using the SymbolTable scope.
        let resolve_result = crate::resolve::resolve_document(&doc_arc, &vec![table], &Vec::new());
        let resolve_diagnostics = resolve_result.diagnostics;
        let unreferenced_names = resolve_result.unreferenced;

        // Build unified diagnostics dict from Diagnostics (errors + warnings).
        // Callers distinguish severity by reading d.level on each entry.
        // There is no separate "warnings" key — diagnostics is the single bag for all severities.
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        for (i, diag) in resolve_diagnostics.iter().enumerate() {
            let w = serialize_diagnostic(diag, &call_span);
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
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
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_INT,
                    )),
                }),
            );
        }

        let doc_span = compute_doc_span(&doc_arc, &call_span);
        let mk_span = |span: &crate::ast::Span| -> Arc<Thunk> { make_span_dict(span, &call_span) };

        // Return {doc: Value::Document, diagnostics: Dict, unreferenced: Dict, doc-span: SpanDict}.
        // diagnostics is a unified bag of all severities; callers read d.level to filter.
        let mut result_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result_dict.insert(
            HashableValue::Str("doc".into()),
            mk(Value::Document {
                doc: std::sync::Arc::clone(&doc_arc),
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DOCUMENT,
                )),
            }),
        );
        result_dict.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: diagnostics_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
        result_dict.insert(
            HashableValue::Str("unreferenced".into()),
            mk(Value::Dict {
                entries: unreferenced_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
        result_dict.insert(HashableValue::Str("doc-span".into()), mk_span(&doc_span));
        ok_val(
            Value::Dict {
                entries: result_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        // Extract required named arg: pipeline-input (String)
        let pipeline_input_name = match named.as_ref().and_then(|n| n.get("pipeline-input")) {
            Some(thunk) => {
                let (result, diags) = crate::eval::materialize(thunk, Some(&call_span), &ctx).await.into_parts();
                _dr_diags.extend(diags);
                let val = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };
                let (result, diags) = crate::builtins::require_string(
                    "builtin-lint-pipeline-docs (pipeline-input:)",
                    val,
                    call_span.clone(),
                    &ctx,
                ).into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            }
            None => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    "builtin-lint-pipeline-docs: required named arg 'pipeline-input' missing"
                        .to_string(),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        let docs_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let docs_dict = match docs_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-lint-pipeline-docs: expected Dict, got {}",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags)
            }
        };

        // Sort document entries by integer key to get them in pipeline order.
        let collected = docs_dict
            .iter()
            .map(|(k, v)| match k {
                HashableValue::Int(i) => Ok((*i, Arc::clone(v))),
                other => Err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-lint-pipeline-docs: document dict key must be Int, got: {:?}",
                        other
                    ),
                    call_span.clone(),
                )),
            })
            .collect::<Result<Vec<_>, crate::error::Diagnostic>>();
        let mut doc_entries: Vec<(i64, Arc<Thunk>)> = match collected {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        doc_entries.sort_by_key(|(i, _)| *i);

        // Collect SurfaceDocument arcs from the thunks.
        let mut doc_arcs: Vec<std::sync::Arc<crate::ast::SurfaceDocument>> = Vec::new();
        for (_, thunk) in &doc_entries {
            let (result, diags) = crate::eval::materialize(thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            if let Value::Document { doc: d, .. } = val {
                doc_arcs.push(d);
            } else {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-lint-pipeline-docs: entry is not a Document: {}",
                        ctx.type_display_name(val.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags);
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
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, diag) in warnings.iter().enumerate() {
            let w = serialize_diagnostic(diag, &call_span);
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                }),
            );
        }

        ok_val(
            Value::Dict {
                entries: diagnostics_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-typecheck-doc", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 3 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "builtin-typecheck-doc: expected 3 arguments, got {}",
                    args.len()
                ),
                call_span,
            )).with_diagnostics(_dr_diags);
        }

        // arg0: Value::Document — extract doc_arc.
        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-typecheck-doc",
                        "Document",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    args[0].span.clone(),
                )).with_diagnostics(_dr_diags);
            }
        };

        // arg1: Value::TypeContext
        let tc_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let tc_arc = {
            let (result, diags) = require_type_context("builtin-typecheck-doc", tc_val, args[1].span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // arg2: Value::Dict — the doc-env (validated for type but value not retained here;
        // the doc_arc already holds the parsed document form).
        {
            let env_val = {
                let (result, diags) = args[2].require_value().into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(v) => v.clone(),
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            };
            if !matches!(env_val, Value::Dict { .. }) {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "builtin-typecheck-doc arg2 (doc-env) is not a Dict: {}",
                        ctx.type_display_name(env_val.type_decl_id_of())
                    ),
                    call_span,
                )).with_diagnostics(_dr_diags);
            }
        }

        // Extract TypeContext state
        let (mut state,) = {
            let guard = tc_arc.lock().unwrap();
            let mut state = crate::types::InferState::new();
            state.eval_ctx = Some(Arc::clone(&ctx));

            // Derive type_name_to_id from the type-stage scope if not already populated.
            // This covers the first typecheck after a fresh TypeContext is created,
            // where guard.type_name_to_id is empty but the scope carries nominal IDs.
            let mut type_name_to_id = guard.type_name_to_id.clone();
            for scope_map in &guard.type_stage_scope {
                for (name, tv) in scope_map {
                    if let Some(id) = crate::type_infer::typevalue_nominal_id(tv) {
                        if id > 0 {
                            type_name_to_id.entry(name.clone()).or_insert(id);
                        }
                    }
                }
            }

            // Ensure an innermost scope frame exists for declarations discovered during this typecheck.
            let mut type_stage_scope = guard.type_stage_scope.clone();
            if type_stage_scope.is_empty() {
                type_stage_scope.push(std::collections::HashMap::new());
            }

            // Configure InferState fields directly (build_infer_state was deleted).
            state.type_stage_data.scope = type_stage_scope;
            state.type_stage_data.fns = guard.type_stage_fns.clone();
            state.type_stage_data.type_vars = guard.type_stage_type_vars.clone();
            state.tycon_env = guard.tycon_env.clone();
            state.type_name_to_id = type_name_to_id;
            state.type_display_names = std::collections::HashMap::new();

            // Seed classes/instances from TypeContextData.
            state.classes = guard.classes.clone();
            state.instances = guard.instances.clone();

            (state,)
        };

        // Build outermost SymbolTable scope with type-stage entries if not already populated.
        if state.current_scope.is_empty() {
            let mut outermost = crate::symbol_table::SymbolTable::new();
            for (slot, (name, tv)) in state
                .type_stage_data
                .scope
                .iter()
                .flat_map(|m| m.iter())
                .enumerate()
            {
                outermost.insert(name.clone(), slot as u32, crate::rust_span!());
                outermost.set_type_scheme(slot as u32, Arc::clone(tv));
            }
            state.current_scope.push(std::sync::Arc::new(outermost));
        }

        let scope = state.current_scope.clone();
        let mut type_map_ref: Option<&mut crate::typecheck::TypeMap> = None;

        let (_, errors) =
            crate::typecheck::process_document(&doc_arc, &mut state, &scope, &mut type_map_ref)
                .await;

        // Collect Diagnostics from state.diagnostics — now includes inline CEK emissions
        let mut type_diagnostics: Vec<crate::error::Diagnostic> =
            std::mem::take(&mut state.diagnostics);

        // Write results back to TypeContext.
        {
            let mut guard = tc_arc.lock().unwrap();
            for (name, def) in &state.tycon_env {
                guard
                    .tycon_env
                    .entry(name.clone())
                    .or_insert_with(|| Arc::clone(def));
            }

            // Write back type_stage_scope: merge new declarations (ADTs, classes) from
            // frame[0] back into TypeContextData so subsequent typecheck calls see them.
            if let Some(new_entries) = state.type_stage_data.scope.first() {
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
            // Write back type_name_to_id and type_display_names: merge new mappings
            // into TypeContextData so subsequent typecheck calls and error messages see them.
            for (name, id) in &state.type_name_to_id {
                guard.type_name_to_id.entry(name.clone()).or_insert(*id);
                guard
                    .type_display_names
                    .entry(*id)
                    .or_insert_with(|| name.clone());
            }
        }

        // Write back classes and instances from InferState to TypeContextData.
        {
            let mut guard = tc_arc.lock().unwrap();
            for class in &state.classes {
                if !guard
                    .classes
                    .iter()
                    .any(|c| c.class_decl_id == class.class_decl_id)
                {
                    guard.classes.push(class.clone());
                }
            }
            for (mangled, inst) in &state.instances {
                guard
                    .instances
                    .entry(mangled.clone())
                    .or_insert_with(|| inst.clone());
            }
        }

        // Build unified diagnostics dict from all type diagnostics (errors + warnings + info)
        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };
        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();

        // Merge errors and type_diagnostics into one vec
        let mut all_diagnostics = errors;
        all_diagnostics.append(&mut type_diagnostics);

        // Build dict entries from all type diagnostics
        for (i, diag) in all_diagnostics.iter().enumerate() {
            let w = serialize_diagnostic(diag, &call_span);
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                }),
            );
        }

        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result.insert(
            HashableValue::Str("doc".into()),
            mk(Value::Document {
                doc: doc_arc,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DOCUMENT,
                )),
            }),
        );
        result.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: diagnostics_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );
        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named(
                "builtin-get-type-context",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if !args.is_empty() {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(0),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
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
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_TYPE_CONTEXT,
                        )),
                    },
                    call_span,
                )
                .with_diagnostics(_dr_diags)
            }
            None => {
                drop(tc_guard);
                // TypeContext must be initialized by main.rs before any tinct code runs.
                crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    "builtin-get-type-context: TypeContext not initialized — \
                     this is a Rust bootstrap error, not a tinct error"
                        .to_string(),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        }
    })
}

/// `builtin-make-type-ctx`: Create a fresh TypeContext seeded with core type definitions.
///
/// Creates a `TypeContextData` seeded with:
/// - `tycon_env`: type constructor definitions from builtin_core
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-make-type-ctx", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if !args.is_empty() {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(0),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
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
        let (result, diags) = crate::imports::get_builtin_core_type_stage_scope().await.into_parts();
        _dr_diags.extend(diags);
        let type_stage_data = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        let tc = TypeContextData {
            tycon_env: std::collections::HashMap::new(),
            type_stage_scope: type_stage_data.scope,
            type_stage_fns: type_stage_data.fns,
            type_stage_type_vars: type_stage_data.type_vars,
            classes: vec![],
            instances: std::collections::HashMap::new(),
            type_diagnostics: Vec::new(),
            type_name_to_id: std::collections::HashMap::new(),
            type_display_names: std::collections::HashMap::new(),
        };
        // Install it on EvalContext (no-op if already initialized).
        ctx.init_type_context(tc.clone());
        // Wrap in Arc<Mutex<>> and return as a Value::TypeContext handle.
        ok_val(
            Value::TypeContext {
                ctx: Arc::new(Mutex::new(tc)),
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_TYPE_CONTEXT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-fork-type-ctx", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        let parent_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
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
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_TYPE_CONTEXT,
                        )),
                    },
                    call_span,
                )
                .with_diagnostics(_dr_diags)
            }
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "builtin-fork-type-ctx",
                    "TypeContext",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
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
/// The new frame is prepended to `TypeContextData.type_stage_data.scope` (Vec[0] = innermost,
/// highest priority). Each module's type-stage is a distinct frame.
///
/// Returns the same TypeContext (mutations are visible through the shared Arc<Mutex>).
///
/// Populates the TypeContext type-stage scope chain from the accumulated env dict.
pub(crate) fn builtin_tc_update_type_stage_env(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named(
                "builtin-tc-update-type-stage-env",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Extract TypeContext — same pattern as builtin_typecheck_doc
        let tc_arc = match {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        } {
            Value::TypeContext { ctx: arc, .. } => arc,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-tc-update-type-stage-env",
                        "TypeContext",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        // Extract Dict
        let dict_entries = match {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        } {
            Value::Dict { entries: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-tc-update-type-stage-env",
                        "Dict",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
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
        let (existing_scope, existing_name_to_id): (
            Vec<std::collections::HashMap<String, crate::type_infer::TypeValue>>,
            std::collections::HashMap<String, u64>,
        ) = {
            let guard = tc_arc.lock().unwrap();
            (
                guard.type_stage_scope.clone(),
                guard.type_name_to_id.clone(),
            )
        };

        // Materialize each thunk and classify into the three maps
        let mut new_frame: std::collections::HashMap<String, crate::type_infer::TypeValue> =
            std::collections::HashMap::new();
        let mut new_fns: std::collections::HashMap<String, Arc<Thunk>> =
            std::collections::HashMap::new();
        let mut new_type_vars: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for (name, thunk) in to_process {
            let (result, diags) = materialize(&thunk, None, &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            let (result, diags) = crate::imports::classify_type_stage_entry(
                &name,
                &thunk,
                &val,
                &ctx,
                &existing_scope,
                &existing_name_to_id,
            )
            .await
            .into_parts();
            _dr_diags.extend(diags);
            let classified_opt = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            let classified = match classified_opt {
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
            guard.type_stage_scope.push(new_frame);
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
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_TYPE_CONTEXT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-program", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Extract documents from the input collection (Seq or Dict)
        let mut documents = Vec::new();

        match val {
            Value::Dict { entries: map, .. } => {
                // Iterate through dict entries in insertion order
                for (_key, thunk) in map.into_iter() {
                    let (result, diags) = crate::eval::materialize(&thunk, Some(&call_span), &ctx).await.into_parts();
                    _dr_diags.extend(diags);
                    let doc_val = match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    };
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
                            return crate::error::DiagnosticResult::err(Diagnostic::error(
                                KIND,
                                format!(
                                    "{}: expected {}, got {}",
                                    "builtin-program",
                                    "Document",
                                    ctx.type_display_name(doc_val.type_decl_id_of())
                                ),
                                call_span,
                            ))
                            .with_diagnostics(_dr_diags);
                        }
                    }
                }
            }
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-program",
                        "Dict of Document values",
                        ctx.type_display_name(val.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        }

        // Construct the SurfaceProgram and return it as Value::Program.
        let surface_program = crate::ast::SurfaceProgram { documents };

        // Return as Value::Program with empty tables (caller can run expand/resolve if needed)
        ok_val(
            Value::Program {
                program: std::sync::Arc::new(surface_program),
                resolutions: std::sync::Arc::new(Default::default()),
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_PROGRAM,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-desugar`: Desugar a `Value::Program`.
///
/// Pure functional wrapper around `desugar_program_full`. Takes 1 arg: Value::Program.
/// Returns a new Value::Program with desugaring applied.
pub(crate) fn builtin_desugar(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-desugar", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let program_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (program, resolutions) = match program_val {
            Value::Program {
                program,
                resolutions,
                ..
            } => (program, resolutions),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-desugar",
                        "Program",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let desugared = crate::desugar::desugar_program_full(&program);

        ok_val(
            Value::Program {
                program: std::sync::Arc::new(desugared),
                resolutions,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_PROGRAM,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-expand`: Expand a `Value::Program`.
///
/// Pure functional wrapper around `expand_program`. Takes 1 arg: Value::Program.
/// Returns a new Value::Program with declaration expansion applied.
pub(crate) fn builtin_expand(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-expand", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let program_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (program, resolutions) = match program_val {
            Value::Program {
                program,
                resolutions,
                ..
            } => (program, resolutions),
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-expand",
                        "Program",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let expanded = crate::expand::expand_program(&program);

        ok_val(
            Value::Program {
                program: std::sync::Arc::new(expanded),
                resolutions,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_PROGRAM,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-program-docs`: Extract documents from a `Value::Program`.
///
/// Thin wrapper to access `SurfaceProgram.documents`. Takes 1 arg: Value::Program
/// Returns auto-indexed Dict of Value::Document values (Arc::clone each doc).
pub(crate) fn builtin_program_docs(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-program-docs", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let program_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let program_arc = match program_val {
            Value::Program { program, .. } => program,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-program-docs",
                        "Program",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
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
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DOCUMENT,
                    )),
                },
                call_span.clone(),
            ));
            result.insert(HashableValue::Int(i as i64), doc_thunk);
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-doc-expressions`: Return expression items from a document.
///
/// Takes 1 arg: Value::Document.
///
/// Returns an auto-indexed Dict of Expr-variant values, one per expression
/// in the document body's Sequential. The Expr-variant format
/// is the same as produced by `surface_node_to_expr_variant`, which is what
/// `json-expression` in json.llt consumes.
///
/// This replaces the `Value::Document.expressions` internal access backdoor that was
/// deleted from builtins_dict.rs (T-1605 follow-up, S-926 R4).
pub(crate) fn builtin_doc_expressions(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named(
                "builtin-doc-expressions",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-doc-expressions",
                        "Document",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let exprs = match &doc_arc.body.expr {
            crate::ast::SurfaceExpression::Sequential(exprs) => exprs.as_slice(),
            _ => std::slice::from_ref(&doc_arc.body),
        };
        for (i, node) in exprs.iter().enumerate() {
            let expr_val = crate::surface_convert::surface_node_to_expr_variant(node, &ctx);
            let thunk = Arc::new(Thunk::value(expr_val, call_span.clone()));
            result.insert(HashableValue::Int(i as i64), thunk);
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
    })
}

/// `builtin-doc-meta`: Evaluate document header metadata.
///
/// Evaluates the header values of a document. Takes 1 arg:
/// - arg0: Value::Document
///
/// Header values are literals (stage: "type", pragma: ["no-prelude"]),
/// so no env injection is needed. Header evaluation runs with an empty
/// initial_group in an isolated scope.
///
/// Returns Dict {key: evaluated-value, ...}.
/// Returns empty Dict if header is empty.
pub(crate) fn builtin_doc_meta(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-doc-meta", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-doc-meta",
                        "Document",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Evaluate each header SurfaceNode value with an empty SymbolTable.
        // Header values are literals — no env injection needed.
        let mut result: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        let display_names = ctx.type_display_names_snapshot();
        for (key, node_arc) in &doc_arc.header {
            let core_spanned = crate::lower::lower(node_arc, &display_names)
                .take(&ctx.runtime_diagnostics)
                .expect("lowering always succeeds");
            let core_entry = Arc::new(core_spanned);
            let (eval_result, eval_diags) =
                crate::eval_core::eval_core_expr(&core_entry, &crate::symbol_table::SymbolTable::empty_chain(), &ctx)
                    .await
                    .into_parts();
            _dr_diags.extend(eval_diags);
            match eval_result {
                Ok(thunk) => {
                    result.insert(HashableValue::Str(key.clone().into()), thunk);
                }
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        ok_val(
            Value::Dict {
                entries: result,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        )
        .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        let name_val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "builtin-module",
                &args,
                named.as_ref(),
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let name = {
            let (result, diags) = require_string("builtin-module", name_val, call_span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        match crate::builtins::builtin_module(&name) {
            Some(defs) => {
                // Convert Vec<BuiltinDef> to a Dict
                let mut dict_map: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
                for def in defs {
                    let name_arc: Arc<str> = Arc::from(def.name);
                    let builtin_thunk = Arc::new(Thunk::value(
                        Value::Builtin {
                            def,
                            type_val: Some(crate::type_infer::make_typevalue_nominal(
                                crate::value::TYPE_ID_BUILTIN,
                            )),
                        },
                        call_span.clone().with_name(Arc::clone(&name_arc)),
                    ));
                    dict_map.insert(HashableValue::Str(name_arc), builtin_thunk);
                }
                ok_val(
                    Value::Dict {
                        entries: dict_map,
                        type_decl_id: TYPE_ID_DICT,
                        type_val: Some(crate::type_infer::make_typevalue_nominal(
                            crate::value::TYPE_ID_DICT,
                        )),
                    },
                    call_span,
                ).with_diagnostics(_dr_diags)
            }
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!("unknown native module: {:?}", name),
                call_span,
            )).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-empty-scope`: Return an empty `Value::SymbolTable` with no entries.
pub(crate) fn builtin_empty_symbol_table(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx: _,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-empty-scope", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if !args.is_empty() {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(0),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        crate::error::DiagnosticResult::ok(Arc::new(Thunk::value(
            Value::SymbolTable {
                table: crate::symbol_table::SymbolTable::empty(),
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_SCOPE,
                )),
            },
            call_span,
        ))).with_diagnostics(_dr_diags)
    })
}

/// `builtin-scope-extend`: Extend a `Value::SymbolTable` with additional bindings from a `Value::Dict`.
///
/// Takes 2 positional args:
/// - arg0: `Value::SymbolTable` — the base symbol table to extend.
/// - arg1: `Value::Dict` — additions: string-keyed entries to add or override.
///
/// Returns a new `Value::SymbolTable` with the merged entries.
pub(crate) fn builtin_symbol_table_extend(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-scope-extend", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // arg0: Value::SymbolTable
        let scope_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let table = match scope_val {
            Value::SymbolTable { table, .. } => table,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-scope-extend",
                        "SymbolTable",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // arg1: Value::Dict — additions
        let additions_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let additions_map = match additions_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-scope-extend additions",
                        "Dict",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Build string-keyed map for extend
        let mut str_additions: IndexMap<String, Arc<Thunk>> = IndexMap::new();
        for (k, thunk) in &additions_map {
            if let HashableValue::Str(s) = k {
                str_additions.insert(s.to_string(), Arc::clone(thunk));
            }
        }

        // Build a merged SymbolTable: copy existing entries, then overlay additions.
        let mut merged = crate::symbol_table::SymbolTable::new();
        for (slot, (name, entry)) in table.entries().iter().enumerate() {
            merged.insert(name.clone(), slot as u32, entry.definition_span.clone());
            if let Some(ts) = entry.type_scheme.get() {
                let _ = merged.set_type_scheme(slot as u32, Arc::clone(ts));
            }
            if let Some(v) = entry.value.get() {
                let _ = merged.set_value(slot as u32, Arc::clone(v));
            }
        }
        let base_slot = table.len() as u32;
        for (i, (name, thunk)) in str_additions.into_iter().enumerate() {
            let slot = base_slot + i as u32;
            merged.insert(name, slot, crate::rust_span!());
            let _ = merged.set_value(slot, thunk);
        }
        let new_table = Arc::new(merged);

        crate::error::DiagnosticResult::ok(Arc::new(Thunk::value(
            Value::SymbolTable {
                table: new_table,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_SCOPE,
                )),
            },
            call_span,
        ))).with_diagnostics(_dr_diags)
    })
}

/// `builtin-symbol-table-get`: Look up a name in a `Value::SymbolTable`.
///
/// Takes 2 positional args:
/// - arg0: String — the name to look up.
/// - arg1: Value::SymbolTable — the symbol table to search.
///
/// Returns the thunk's value if found, or the empty dict `[]` (null) if not present.
pub(crate) fn builtin_symbol_table_get(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named(
                "builtin-symbol-table-get",
                named.as_ref(),
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // arg0: String — the name to look up
        let name_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let name = {
            let (result, diags) = crate::builtins::require_string(
                "builtin-symbol-table-get",
                name_val,
                args[0].span.clone(),
                &ctx,
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // arg1: Value::SymbolTable
        let scope_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let table = match scope_val {
            Value::SymbolTable { table, .. } => table,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-symbol-table-get",
                        "SymbolTable",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let get_result = {
            let (result, diags) = crate::symbol_table::SymbolTable::get_in_chain(&[table], &name).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match get_result {
            Some(thunk) => crate::error::DiagnosticResult::ok(thunk).with_diagnostics(_dr_diags),
            None => ok_val(
                Value::Dict {
                    entries: IndexMap::new(),
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-eval`: Evaluate a `Value::CoreDocument` against a `Value::SymbolTable`.
///
/// Takes 2 positional args:
/// - arg0: `Value::CoreDocument` — fully lowered document (output of `builtin-lower`).
///   Passing `Value::Document` is a type error: call `builtin-lower` first.
/// - arg1: `Value::SymbolTable` — accumulated environment scope.
///
/// Returns `{table: Value::SymbolTable, diagnostics: Dict}`.
pub(crate) fn builtin_eval(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-eval", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // arg0: Value::CoreDocument — reject Value::Document with a descriptive type error.
        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let (core_entries, _doc_span) = match doc_val {
            Value::CoreDocument { entries, span, .. } => (entries, span),
            Value::Document { .. } => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(KIND,
                    format!("{}: expected {}, got {}", "builtin-eval: expected CoreDocument (output of builtin-lower), got Document; \
                     call builtin-lower before builtin-eval", "CoreDocument", "Document"),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-eval",
                        "CoreDocument",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // arg1: Value::SymbolTable — env scope.
        let env_val = {
            let (result, diags) = args[1].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let table = match env_val {
            Value::SymbolTable { table, .. } => table,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-eval env",
                        "SymbolTable",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Evaluate the document with the SymbolTable scope.
        // core_entries is Vec<(String, Arc<Spanned<CoreExpr>>)>.
        let scope_chain = vec![table.clone()];
        let mut eval_diagnostics = Vec::new();
        for (_, expr) in core_entries.iter() {
            let (eval_result, eval_diags) =
                crate::eval_core::eval_core_expr(expr, &scope_chain, &ctx)
                    .await
                    .into_parts();
            eval_diagnostics.extend(eval_diags);
            match eval_result {
                Ok(thunk) => {
                    let (mat_result, mat_diags) =
                        crate::eval::materialize(&thunk, None, &ctx).await.into_parts();
                    eval_diagnostics.extend(mat_diags);
                    if let Err(e) = mat_result {
                        eval_diagnostics.push(e);
                    }
                }
                Err(e) => eval_diagnostics.push(e),
            }
        }

        let mk = |v: Value| -> Arc<Thunk> { Arc::new(Thunk::value(v, call_span.clone())) };

        let mut diagnostics_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        for (i, diag) in eval_diagnostics.iter().enumerate() {
            let w = serialize_diagnostic(diag, &call_span);
            diagnostics_dict.insert(
                HashableValue::Int(i as i64),
                mk(Value::Dict {
                    entries: w,
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                }),
            );
        }

        let mut result_dict: IndexMap<HashableValue, Arc<Thunk>> = IndexMap::new();
        result_dict.insert(
            HashableValue::Str("table".into()),
            mk(Value::SymbolTable {
                table: table,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_SCOPE,
                )),
            }),
        );
        result_dict.insert(
            HashableValue::Str("diagnostics".into()),
            mk(Value::Dict {
                entries: diagnostics_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
        );

        ok_val(
            Value::Dict {
                entries: result_dict,
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-variant-payload`: extract the payload from a Variant, returning it directly.
/// Takes 1 arg (a Variant). Returns the payload value (forces the payload thunk).
/// Used to extract values from Result.Ok/Error without going through key-based lookup (which
/// fails when the payload is a non-dict value like String).
pub(crate) fn builtin_variant_payload(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("variant-payload", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        let val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        match val {
            Value::Variant {
                payload: Some(payload_id),
                ..
            } => {
                let payload_thunk = Arc::clone(&payload_id);
                let payload_val = {
                    let (result, diags) = materialize(&payload_thunk, Some(&call_span), &ctx).await.into_parts();
                    _dr_diags.extend(diags);
                    match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    }
                };
                ok_val(payload_val, call_span).with_diagnostics(_dr_diags)
            }
            Value::Variant { payload: None, .. } => ok_val(
                Value::Dict {
                    entries: indexmap::IndexMap::new(),
                    type_decl_id: TYPE_ID_DICT,
                    type_val: Some(crate::type_infer::make_typevalue_nominal(
                        crate::value::TYPE_ID_DICT,
                    )),
                },
                call_span,
            ).with_diagnostics(_dr_diags),
            other => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "variant-payload",
                    "Variant",
                    ctx.type_display_name(other.type_decl_id_of())
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
        }
    })
}

/// `builtin-eval-macro-ast`: evaluate a macro-produced AST.
///
/// Takes 1 positional arg: an `Expr.*` Value::Variant representing the AST to evaluate.
///
/// Evaluation pipeline:
///   1. Convert the `Expr.*` variant to a `SurfaceNode` using `dict_to_surface_node`
///   2. Wrap in a single-expression `SurfaceProgram`
///   3. Desugar + expand + resolve with empty SymbolTable
///   4. Lower to CoreExpr and evaluate via `eval_core_expr` + `materialize`
///   5. Return the result thunk
pub(crate) fn builtin_eval_macro_ast(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named: _,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Convert Expr.* variant to SurfaceNode
        let expr_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let expr_node = match expr_val {
            Value::Variant {
                ref type_decl_id, ..
            } if *type_decl_id == crate::value::TYPE_ID_EXPR => {
                match crate::surface_convert::dict_to_surface_node(&expr_val, &call_span, &ctx) {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("eval-macro-ast: Expr.* conversion failed: {}", e),
                        call_span.clone(),
                    ))
                    .with_diagnostics(_dr_diags),
                }
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "eval-macro-ast",
                        "Expr.*",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        // Wrap in a single-expression SurfaceProgram
        let document = crate::ast::SurfaceDocument {
            header: indexmap::IndexMap::new(),
            body: Arc::new(crate::ast::SurfaceNode::new(
                crate::ast::SurfaceExpression::Sequential(vec![expr_node]),
                call_span.clone(),
            )),
        };
        let program = crate::desugar::desugar_program_full(&crate::ast::SurfaceProgram {
            documents: vec![crate::ast::Spanned::new(
                std::sync::Arc::new(document),
                call_span.clone(),
            )],
        });

        // Desugar + expand + resolve with empty SymbolTable
        let program = crate::expand::expand_program(&program);
        let empty_chain = crate::symbol_table::SymbolTable::empty_chain();
        crate::resolve::resolve_program(&program, &empty_chain, &Vec::new());

        // Lower each document's expressions to CoreExpr and evaluate
        let display_names = ctx.type_display_names_snapshot();
        let mut core_entries: Vec<Arc<crate::ast::Spanned<crate::ast::CoreExpr>>> = Vec::new();
        for doc_spanned in &program.documents {
            let core_spanned = crate::lower::lower(&doc_spanned.node.body, &display_names)
                .take(&ctx.runtime_diagnostics)
                .expect("lowering always succeeds");
            core_entries.push(Arc::new(core_spanned));
        }

        let scope_chain = crate::symbol_table::SymbolTable::empty_chain();
        let mut last_thunk: Option<Arc<crate::value::Thunk>> = None;
        for core_entry in &core_entries {
            let (eval_result, eval_diags) =
                crate::eval_core::eval_core_expr(core_entry, &scope_chain, &ctx)
                    .await
                    .into_parts();
            _dr_diags.extend(eval_diags);
            match eval_result {
                Ok(thunk) => {
                    let (mat_result, mat_diags) =
                        crate::eval::materialize(&thunk, None, &ctx).await.into_parts();
                    _dr_diags.extend(mat_diags);
                    if let Err(e) = mat_result {
                        return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags);
                    }
                    last_thunk = Some(thunk);
                }
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        match last_thunk {
            Some(thunk) => crate::error::DiagnosticResult::ok(thunk).with_diagnostics(_dr_diags),
            None => crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                "eval-macro-ast: no result in evaluated document".to_string(),
                call_span,
            ))
            .with_diagnostics(_dr_diags),
        }
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        // Expect exactly 2 args: schema, data
        let (result, diags) = expect_two_args("validate", &args, named.as_ref(), call_span.clone()).into_parts();
        _dr_diags.extend(diags);
        let (schema, data) = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

        // Schema must be a Dict
        let schema_dict = match schema {
            Value::Dict { entries: d, .. } => d,
            _ => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "type mismatch: expected {}, got {}",
                        "Dict (schema)",
                        ctx.type_display_name(schema.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        // Collect violations. validate_value is async-recursive via Box::pin.
        let (result, diags) = validate_value(
            schema_dict,
            data.clone(),
            String::new(),
            Arc::clone(&ctx),
            call_span.clone(),
        )
        .await.into_parts();
        _dr_diags.extend(diags);
        let violations = match result {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };

        if violations.is_empty() {
            // Success: return data unchanged
            crate::error::DiagnosticResult::ok(Arc::new(Thunk::value(data, call_span))).with_diagnostics(_dr_diags)
        } else {
            // Failure: throw SchemaViolation with all violations
            crate::error::DiagnosticResult::err({
                let mut diag = Diagnostic::error(
                    KIND,
                    format!(
                        "schema validation failed with {} violation(s)",
                        violations.len()
                    ),
                    call_span,
                );
                for (field, msg) in violations {
                    diag.add_note(format!("{field}: {msg}"));
                }
                diag
            })
            .with_diagnostics(_dr_diags)
        }
    })
}

/// Helper: extract exactly 2 pre-materialized positional arguments, no named args.
fn expect_two_args(
    name: &str,
    args: &[Arc<Thunk>],
    named: Option<&IndexMap<String, Arc<Thunk>>>,
    call_span: Span,
) -> crate::error::DiagnosticResult<(Value, Value)> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

    if args.len() != 2 {
        return crate::error::DiagnosticResult::err(Diagnostic::error(
            KIND,
            format!(
                "arity mismatch: expected {}, got {}",
                crate::error::ArityBound::Exact(2),
                args.len()
            ),
            call_span,
        ))
        .with_diagnostics(_dr_diags);
    }
    if named.is_some() && !named.unwrap().is_empty() {
        return crate::error::DiagnosticResult::err(Diagnostic::error(
            KIND,
            format!("{} does not accept named arguments", name),
            call_span,
        ))
        .with_diagnostics(_dr_diags);
    }

    let (result, diags) = args[0].require_value().into_parts();
    _dr_diags.extend(diags);
    let val1 = match result {
        Ok(v) => v.clone(),
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    let (result, diags) = args[1].require_value().into_parts();
    _dr_diags.extend(diags);
    let val2 = match result {
        Ok(v) => v.clone(),
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    crate::error::DiagnosticResult::ok((val1, val2)).with_diagnostics(_dr_diags)
}

/// Return type alias for async-recursive `validate_value`.
type ValidationFuture = std::pin::Pin<
    Box<
        dyn std::future::Future<
                Output = crate::error::DiagnosticResult<Vec<(String, String)>>,
            > + Send,
    >,
>;

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
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        let mut violations: Vec<(String, String)> = Vec::new();

        // Check `type` constraint
        if let Some(type_thunk) = schema.get(&HashableValue::Str("type".into())) {
            let (result, diags) = materialize(type_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let type_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            if let Value::String {
                source, start, end, ..
            } = type_val
            {
                let expected_type = &source[start..end];
                let actual_type = ctx.type_display_name(data.type_decl_id_of());
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
            let (result, diags) = materialize(min_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let min_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
            let (result, diags) = materialize(max_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let max_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
            let (result, diags) = materialize(min_len_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let min_len_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
            let (result, diags) = materialize(max_len_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let max_len_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
            let (result, diags) = materialize(pattern_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let pattern_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
            let (result, diags) = materialize(enum_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let enum_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
            if let Value::Dict {
                entries: enum_dict, ..
            } = enum_val
            {
                // Pre-materialize all enum values, then check membership via primitive equality.
                let mut allowed_values = Vec::with_capacity(enum_dict.len());
                for (_key, val_thunk) in enum_dict.iter() {
                    let (result, diags) = materialize(val_thunk, Some(&span), &ctx).await.into_parts();
                    _dr_diags.extend(diags);
                    let val = match result {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                    };
                    allowed_values.push(val);
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
            let (result, diags) = materialize(fields_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let fields_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
                        let (result, diags) = materialize(field_schema_thunk, Some(&span), &ctx).await.into_parts();
                        _dr_diags.extend(diags);
                        let field_schema_val = match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
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
                                let (result, diags) = materialize(req_thunk, Some(&span), &ctx).await.into_parts();
                                _dr_diags.extend(diags);
                                let req_val = match result {
                                    Ok(v) => v,
                                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                };
                                matches!(&req_val, Value::Int { n, .. } if *n != 0)
                            } else {
                                false
                            };

                            if let Some(field_value_thunk) = data_dict.get(field_key) {
                                let (result, diags) = materialize(field_value_thunk, Some(&span), &ctx).await.into_parts();
                                _dr_diags.extend(diags);
                                let field_value = match result {
                                    Ok(v) => v,
                                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                };

                                let (result, diags) = validate_value(
                                    field_schema,
                                    field_value,
                                    field_path,
                                    Arc::clone(&ctx),
                                    span.clone(),
                                )
                                .await.into_parts();
                                _dr_diags.extend(diags);
                                let sub_violations = match result {
                                    Ok(v) => v,
                                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                                };
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
            let (result, diags) = materialize(items_thunk, Some(&span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            let items_val = match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            };
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
                        let (result, diags) = materialize(val_thunk, Some(&span), &ctx).await.into_parts();
                        _dr_diags.extend(diags);
                        let val = match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };

                        let item_path = if path.is_empty() {
                            format!("[{}]", idx)
                        } else {
                            format!("{}[{}]", path, idx)
                        };

                        let (result, diags) = validate_value(
                            items_schema.clone(),
                            val,
                            item_path,
                            Arc::clone(&ctx),
                            span.clone(),
                        )
                        .await.into_parts();
                        _dr_diags.extend(diags);
                        let sub_violations = match result {
                            Ok(v) => v,
                            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                        };
                        violations.extend(sub_violations);
                    }
                }
            }
        }

        crate::error::DiagnosticResult::ok(violations).with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        {
            let (result, diags) = reject_named("builtin-is-contractive", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }

        let body_val = {
            let (result, diags) = crate::builtins::expect_one_arg(
                "builtin-is-contractive",
                &args,
                // named already checked above; pass None so expect_one_arg doesn't re-check
                None,
                &ctx,
                call_span.clone(),
            ).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let (res, diags) = is_contractive_value(&body_val, &ctx).await.into_parts();
        _dr_diags.extend(diags);
        let result = match res {
            Ok(v) => v,
            Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
        };
        ok_val(
            Value::Int {
                n: if result { 1 } else { 0 },
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = crate::error::DiagnosticResult<bool>> + Send + 'a>,
> {
    Box::pin(async move {
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        // Unwrap Value::Annotated transparently — annotations do not affect contractiveness.
        let val = match val {
            Value::Annotated { inner, .. } => inner.as_ref(),
            other => other,
        };

        match val {
            // Case 1: bare RecursiveRef — non-contractive.
            Value::Variant { ctor_index, .. }
                if *ctor_index == crate::type_tags::TV_RECURSIVE_REF =>
            {
                crate::error::DiagnosticResult::ok(false).with_diagnostics(_dr_diags)
            }

            // Case 3: Union and Intersect are non-guarding — recurse into all children.
            Value::Variant {
                ctor_index,
                payload,
                ..
            } if *ctor_index == crate::type_tags::TV_UNION
                || *ctor_index == crate::type_tags::TV_INTER =>
            {
                let payload_thunk = match payload {
                    Some(id) => id.clone(),
                    None => return crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags),
                };
                let (result, diags) = materialize(&payload_thunk, None, ctx).await.into_parts();
                _dr_diags.extend(diags);
                let payload_val = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };
                let types_thunk = match &payload_val {
                    Value::Dict { entries: d, .. } => {
                        match d.get(&crate::value::HashableValue::Str("types".into())) {
                            Some(t) => Arc::clone(t),
                            None => return crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags),
                        }
                    }
                    _ => return crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags),
                };
                let (result, diags) = is_contractive_seq(types_thunk, ctx).await.into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(v) => crate::error::DiagnosticResult::ok(v).with_diagnostics(_dr_diags),
                    Err(e) => crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            }

            // Case 2: all other constructors are guarding.
            _ => crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags),
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
) -> crate::error::DiagnosticResult<bool> {
    let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

    let (result, diags) = materialize(&types_thunk, None, ctx).await.into_parts();
    _dr_diags.extend(diags);
    let val = match result {
        Ok(v) => v,
        Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
    };

    match &val {
        Value::Dict { entries: d, .. } => {
            for (_k, v_thunk) in d {
                let (result, diags) = materialize(v_thunk, None, ctx).await.into_parts();
                _dr_diags.extend(diags);
                let v_val = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };

                let (result, diags) = is_contractive_value(&v_val, ctx).await.into_parts();
                _dr_diags.extend(diags);
                let is_contractive = match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                };

                if !is_contractive {
                    return crate::error::DiagnosticResult::ok(false).with_diagnostics(_dr_diags);
                }
            }
            crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags)
        }
        _ => crate::error::DiagnosticResult::ok(true).with_diagnostics(_dr_diags), // Not a Dict — no children to check.
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("sequential", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }
        // Materialize the expressions dict
        let arg0_thunk = Arc::clone(&args[0]);
        let exprs_val = {
            let (result, diags) = materialize(&arg0_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        // Extract each entry in insertion order as Expr.* variant, converting to SurfaceNode
        let exprs_dict = match &exprs_val {
            Value::Dict { entries: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-sequential",
                        "Dict",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };
        let mut exprs: Vec<Arc<crate::ast::SurfaceNode>> = Vec::new();
        // Iterate in insertion order (integer-keyed dict entries are in index order)
        for (_key, thunk) in exprs_dict.iter() {
            let val = {
                let (result, diags) = materialize(thunk, Some(&call_span), &ctx).await.into_parts();
                _dr_diags.extend(diags);
                match result {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
                }
            };
            match val {
                Value::Variant {
                    ref type_decl_id, ..
                } if *type_decl_id == crate::value::TYPE_ID_EXPR => {
                    let node = match crate::surface_convert::dict_to_surface_node(&val, &call_span, &ctx) {
                        Ok(v) => v,
                        Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                            KIND,
                            format!("builtin-sequential: Expr.* conversion failed: {}", e),
                            call_span.clone(),
                        ))
                        .with_diagnostics(_dr_diags),
                    };
                    exprs.push(node);
                }
                other => {
                    return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!(
                            "{}: expected {}, got {}",
                            "builtin-sequential",
                            "Expr.*",
                            ctx.type_display_name(other.type_decl_id_of())
                        ),
                        call_span,
                    ))
                    .with_diagnostics(_dr_diags)
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
        ).with_diagnostics(_dr_diags)
    })
}

/// builtin-ast-to-program — convert an Expr.* Value::Variant to a Value::Program
///
/// Takes 1 positional arg (Expr.* variant) and 1 named arg (call-site-span: span dict).
/// Converts the Expr.* AST node back to a SurfaceNode, wraps it in a single-expression
/// SurfaceProgram with one SurfaceDocument containing one expression.
/// Returns Value::Program.
pub(crate) fn builtin_ast_to_program(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();

        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // Extract mandatory call-site-span: named arg
        let call_site_span_thunk_id = if let Some(ref named_map) = named {
            for key in named_map.keys() {
                if key != "call-site-span" {
                    return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("{} does not accept named arguments", "ast-to-program"),
                        call_span,
                    ))
                    .with_diagnostics(_dr_diags);
                }
            }
            match named_map.get("call-site-span").cloned() {
                Some(t) => t,
                None => {
                    return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!(
                            "{}: expected {}, got {}",
                            "ast-to-program",
                            "span dict (for call-site-span: argument — required)",
                            "absent"
                        ),
                        call_span,
                    ))
                    .with_diagnostics(_dr_diags)
                }
            }
        } else {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "ast-to-program",
                    "span dict (for call-site-span: argument — required)",
                    "absent"
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        };

        // Materialize call-site-span: — must be a span dict
        let call_site_span_thunk = call_site_span_thunk_id;
        let call_site_span_val = {
            let (result, diags) = materialize(&call_site_span_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let call_site_span_actual = match &call_site_span_val {
            Value::Dict { entries: dict, .. } => {
                // Extract span from the dict using extract_span
                match crate::surface_convert::extract_span(dict, &ctx) {
                    Some(v) => v,
                    None => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!(
                            "{}: expected {}, got {}",
                            "ast-to-program",
                            "valid span dict (with start/end line/col fields)",
                            "invalid span dict"
                        ),
                        call_span.clone(),
                    ))
                    .with_diagnostics(_dr_diags),
                }
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "ast-to-program",
                        "span dict (for call-site-span: argument)",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        // arg[0]: Expr.* Value::Variant
        let expr_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        // Convert Expr.* Variant to SurfaceNode
        let expr_node = match expr_val {
            Value::Variant {
                ref type_decl_id, ..
            } if *type_decl_id == crate::value::TYPE_ID_EXPR => {
                match crate::surface_convert::dict_to_surface_node(
                    &expr_val,
                    &call_site_span_actual,
                    &ctx,
                ) {
                    Ok(v) => v,
                    Err(e) => return crate::error::DiagnosticResult::err(Diagnostic::error(
                        KIND,
                        format!("ast-to-program: Expr.* conversion failed: {}", e),
                        call_span.clone(),
                    ))
                    .with_diagnostics(_dr_diags),
                }
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "ast-to-program",
                        "Expr.*",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        // Wrap in a SurfaceProgram: one document, one item (Expr)
        let document = crate::ast::SurfaceDocument {
            header: indexmap::IndexMap::new(),
            body: Arc::new(crate::ast::SurfaceNode::new(
                crate::ast::SurfaceExpression::Sequential(vec![expr_node]),
                call_span.clone(),
            )),
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
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_PROGRAM,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
    })
}

/// `builtin-check-type` (`check-type` in tinct): validate a value against a named type.
///
/// - arg 0: String — the type name to check against (e.g. `"String"`, `"Int"`, `"Dict"`)
/// - arg 1: Any — the value to validate
/// - Returns: arg 1 unchanged on success, raises `Diagnostic` on type mismatch
///
/// Unknown type names (type variables, complex parameterized types) pass conservatively
/// without validation. Used by tinct-side expects validation (T-1506).
pub(crate) fn builtin_check_type(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("check-type", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let type_name_val = {
            let (result, diags) = materialize(&arg0_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let type_name = {
            let (result, diags) = require_string("check-type", type_name_val, arg0_thunk.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let value = {
            let (result, diags) = materialize(&arg1_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

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
            ok_val(value, call_span).with_diagnostics(_dr_diags)
        } else {
            crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "{}: expected {}, got {}",
                    "expects validation",
                    type_name,
                    ctx.type_display_name(value.type_decl_id_of())
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags)
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
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = reject_named("cap-env-has?", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 2 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(2),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        let arg0_thunk = Arc::clone(&args[0]);
        let arg1_thunk = Arc::clone(&args[1]);
        let name_val = {
            let (result, diags) = materialize(&arg0_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let name = {
            let (result, diags) = require_string("cap-env-has?", name_val, arg0_thunk.span.clone(), &ctx).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };

        let env_val = {
            let (result, diags) = materialize(&arg1_thunk, Some(&call_span), &ctx).await.into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v,
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        // Check whether name is a key in the env-dict (env-dict protocol, T-1775).
        let found = match &env_val {
            Value::Dict { entries: d, .. } => {
                d.contains_key(&HashableValue::Str(Arc::from(name.as_str())))
            }
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-cap-env-has?",
                        "Dict",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags)
            }
        };

        // Return Int: 1 = found, 0 = not found.
        // The prelude wrapper converts Int → Boolean (prelude-agnostic protocol).
        ok_val(
            Value::Int {
                n: if found { 1 } else { 0 },
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
/// Lowering errors (unresolvable variables, malformed AST) are returned as an Diagnostic.
///
/// This replaces the lazy `UnevaluatedState::Surface` path: previously, lowering happened
/// on-demand when a Surface thunk was first forced. With `builtin-lower`, lowering is a
/// named, explicit pipeline step: parse → resolve → typecheck-doc → lower → eval.
pub(crate) fn builtin_lower(
    ctx_arg: BuiltinArgs,
) -> Pin<Box<dyn Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>> + Send>> {
    Box::pin(async move {
        let BuiltinArgs {
            args,
            named,
            call_span,
            ctx,
            ..
        } = ctx_arg;
        let mut _dr_diags: Vec<crate::error::Diagnostic> = Vec::new();
        {
            let (result, diags) = crate::builtins::reject_named("builtin-lower", named.as_ref(), call_span.clone()).into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(()) => {}
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        }
        if args.len() != 1 {
            return crate::error::DiagnosticResult::err(Diagnostic::error(
                KIND,
                format!(
                    "arity mismatch: expected {}, got {}",
                    crate::error::ArityBound::Exact(1),
                    args.len()
                ),
                call_span,
            ))
            .with_diagnostics(_dr_diags);
        }

        // arg0: Value::Document
        let doc_val = {
            let (result, diags) = args[0].require_value().into_parts();
            _dr_diags.extend(diags);
            match result {
                Ok(v) => v.clone(),
                Err(e) => return crate::error::DiagnosticResult::err(e).with_diagnostics(_dr_diags),
            }
        };
        let doc_arc = match doc_val {
            Value::Document { doc: d, .. } => d,
            other => {
                return crate::error::DiagnosticResult::err(Diagnostic::error(
                    KIND,
                    format!(
                        "{}: expected {}, got {}",
                        "builtin-lower",
                        "Document",
                        ctx.type_display_name(other.type_decl_id_of())
                    ),
                    call_span,
                ))
                .with_diagnostics(_dr_diags);
            }
        };

        let mut entries: Vec<(
            String,
            std::sync::Arc<crate::ast::Spanned<crate::ast::CoreExpr>>,
        )> = Vec::new();
        let display_names = ctx.type_display_names_snapshot();
        let core_spanned = crate::lower::lower(&doc_arc.body, &display_names)
            .take(&ctx.runtime_diagnostics)
            .expect("lowering always succeeds");
        entries.push(("0".to_string(), std::sync::Arc::new(core_spanned)));

        let span = doc_arc.body.span.clone();

        ok_val(
            Value::CoreDocument {
                entries: std::sync::Arc::new(entries),
                span,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_CORE_DOCUMENT,
                )),
            },
            call_span,
        ).with_diagnostics(_dr_diags)
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
            "builtin-eval-macro-ast",
            builtin_eval_macro_ast,
            [Strictness::Seq],
            1,
            ["ast"]
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
    ]
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::value::TYPE_ID_DICT;
    use indexmap::IndexMap;

    use super::builtin_tag_of;
    use crate::test_util::test_span;
    use crate::value::{string_val, BuiltinArgs, Thunk, Value};

    const TEST_ID_COLOR: u64 = 400;
    const TEST_ID_SIMPLE_TYPE: u64 = 401;
    const TEST_ID_SHAPE: u64 = 402;

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
        let ctx = crate::eval::EvalContext::new_empty();
        let mut tycon_env = std::collections::HashMap::new();
        tycon_env.insert(
            TEST_ID_COLOR,
            std::sync::Arc::new(crate::type_def::TyConDef {
                type_decl_id: TEST_ID_COLOR,
                params: vec![],
                body: crate::type_infer::make_typevalue_unknown(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![
                    ("Red".to_string(), 0),
                    ("Green".to_string(), 0),
                    ("Blue".to_string(), 0),
                ],
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        tycon_env.insert(
            TEST_ID_SIMPLE_TYPE,
            std::sync::Arc::new(crate::type_def::TyConDef {
                type_decl_id: TEST_ID_SIMPLE_TYPE,
                params: vec![],
                body: crate::type_infer::make_typevalue_unknown(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![("Leaf".to_string(), 0)],
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        tycon_env.insert(
            TEST_ID_SHAPE,
            std::sync::Arc::new(crate::type_def::TyConDef {
                type_decl_id: TEST_ID_SHAPE,
                params: vec![],
                body: crate::type_infer::make_typevalue_unknown(),
                constraints: vec![],
                variance: vec![],
                constructors: vec![("Circle".to_string(), 1)],
                annotation: None,
                field_annotations: indexmap::IndexMap::new(),
                constructor_constants: indexmap::IndexMap::new(),
                definition_span: None,
            }),
        );
        let _ = ctx.tycon_env.set(std::sync::Arc::new(tycon_env));
        ctx
    }

    fn no_named() -> Option<IndexMap<String, Arc<Thunk>>> {
        None
    }

    async fn run(
        f: impl std::future::Future<Output = crate::error::DiagnosticResult<Arc<Thunk>>>,
    ) -> Result<Arc<Thunk>, crate::error::Diagnostic> {
        let (result, _diags) = f.await.into_parts();
        result
    }

    async fn materialize_sync(
        t: &Arc<Thunk>,
        ctx: &Arc<crate::eval::EvalContext>,
    ) -> Result<Value, crate::error::Diagnostic> {
        let (result, _diags) = crate::eval::materialize(t, None, ctx).await.into_parts();
        result
    }

    /// `builtin_tag_of` returns the tag of a bare `Value::Variant`.
    #[tokio::test]
    async fn tag_of_bare_variant() -> Result<(), crate::error::Diagnostic> {
        let variant = Value::Variant {
            type_decl_id: TEST_ID_COLOR,
            ctor_index: 0,
            payload: None,
            type_val: Some(crate::type_infer::make_typevalue_nominal(TEST_ID_COLOR)),
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(variant)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),

            scope: crate::symbol_table::SymbolTable::empty_chain(),
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("Red"));
        Ok(())
    }

    /// `builtin_tag_of` peels a single `Value::Annotated` wrapper and returns the
    /// inner variant's tag. This is the primary regression case for B-441.
    #[tokio::test]
    async fn tag_of_annotated_variant_single_wrap() -> Result<(), crate::error::Diagnostic> {
        let variant = Value::Variant {
            type_decl_id: TEST_ID_SIMPLE_TYPE,
            ctor_index: 0,
            payload: None,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                TEST_ID_SIMPLE_TYPE,
            )),
        };
        let annotation = Value::Dict {
            entries: IndexMap::new(),
            type_decl_id: TYPE_ID_DICT,
            type_val: Some(crate::type_infer::make_typevalue_nominal(
                crate::value::TYPE_ID_DICT,
            )),
        };
        let annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(annotation),
            type_val: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),

            scope: crate::symbol_table::SymbolTable::empty_chain(),
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("Leaf"));
        Ok(())
    }

    /// `builtin_tag_of` peels multiple nested `Value::Annotated` wrappers (the `while let`
    /// loop handles more than one layer of annotation).
    #[tokio::test]
    async fn tag_of_annotated_variant_double_wrap() -> Result<(), crate::error::Diagnostic> {
        let variant = Value::Variant {
            type_decl_id: TEST_ID_SHAPE,
            ctor_index: 0,
            payload: None,
            type_val: Some(crate::type_infer::make_typevalue_nominal(TEST_ID_SHAPE)),
        };
        let inner_annotated = Value::Annotated {
            inner: Box::new(variant),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
            type_val: None,
        };
        let outer_annotated = Value::Annotated {
            inner: Box::new(inner_annotated),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
            type_val: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(outer_annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx: std::sync::Arc::clone(&ctx),

            scope: crate::symbol_table::SymbolTable::empty_chain(),
        }))
        .await?;
        let val = materialize_sync(&result, &ctx).await?;
        assert_eq!(val, string_val("Circle"));
        Ok(())
    }

    /// `builtin_tag_of` returns a type-mismatch error when the peeled value is not a
    /// `Value::Variant` — even if it was wrapped in `Value::Annotated`.
    #[tokio::test]
    async fn tag_of_annotated_non_variant_errors() {
        let annotated = Value::Annotated {
            inner: Box::new(Value::Int {
                n: 42,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_INT,
                )),
            }),
            annotation: Box::new(Value::Dict {
                entries: IndexMap::new(),
                type_decl_id: TYPE_ID_DICT,
                type_val: Some(crate::type_infer::make_typevalue_nominal(
                    crate::value::TYPE_ID_DICT,
                )),
            }),
            type_val: None,
        };
        let ctx = test_ctx();
        let result = run(builtin_tag_of(BuiltinArgs {
            args: vec![thunk_id(annotated)],
            named: no_named(),
            call_span: call_span(),
            ctx,

            scope: crate::symbol_table::SymbolTable::empty_chain(),
        }))
        .await;
        assert!(
            result.is_err(),
            "expected type-mismatch error for annotated non-variant, got ok"
        );
        let err = result.unwrap_err();
        assert!(
            err.kind == "eval",
            "expected eval error for annotated non-variant, got: {:?}",
            err.kind
        );
    }
}
