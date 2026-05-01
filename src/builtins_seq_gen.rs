//! Sequence generator builtins: `range`, `repeat`, `cycle`, `iterate`, `unfold`.
//!
//! These builtins produce potentially-infinite lazy sequences via `PendingBuiltin`
//! corecursion. Each tail is a deferred thunk, not an eagerly-evaluated list.
//!
//! Extracted from `builtins.rs` to keep that file manageable.
//!
//! Registration in `standard_builtins()` remains in `builtins.rs`.

use std::borrow::Cow;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::builtins::{ok_val, reject_named};
use crate::error::{ArityBound, EvalError, EvalResult};
use crate::eval::materialize;
use crate::value::{BuiltinArgs, Thunk, Value};

/// `range`: Sequence of integers from start to end (exclusive), or infinite.
///
/// - `[call $range start]` → infinite Seq: start, start+1, start+2, ...
/// - `[call $range start end]` → finite Seq: start, start+1, ..., end-1
///   (empty if start >= end)
///
/// Both args must be Int. Uses checked_add for overflow detection.
pub(crate) fn builtin_range(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("range", named, call_span)?;
    if args.len() != 1 && args.len() != 2 {
        return Err(EvalError::arity_mismatch_bound(
            ArityBound::Range(1, 2),
            args.len(),
            call_span,
        )
        .into());
    }

    let start = materialize(&args[0], None, &ctx, depth)?;
    let start_int = match start {
        Value::Int(n) => n,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "range".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if args.len() == 1 {
        // Infinite range: [start, start+1, start+2, ...]
        let next_start = start_int
            .checked_add(1)
            .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
        let head = ok_val(Value::Int(start_int), call_span)?;
        let tail_args = vec![ok_val(Value::Int(next_start), call_span)?];
        let tail = Rc::new(Thunk::new_pending_builtin(
            "range",
            builtin_range,
            tail_args,
            IndexMap::new(),
            depth + 1,
            call_span,
            Cow::Borrowed("call $range"),
            Rc::clone(&ctx),
        ));
        ok_val(Value::Seq { head, tail }, call_span)
    } else {
        // Finite range: [start, start+1, ..., end-1]
        let end = materialize(&args[1], None, &ctx, depth)?;
        let end_int = match end {
            Value::Int(n) => n,
            other => {
                return Err(EvalError::type_mismatch_ctx(
                    "range".to_string(),
                    "Int",
                    other.type_name(),
                    call_span,
                )
                .into())
            }
        };

        if start_int >= end_int {
            // Empty range
            ok_val(Value::Dict(IndexMap::new()), call_span)
        } else {
            let next_start = start_int
                .checked_add(1)
                .ok_or_else(|| EvalError::integer_overflow("range".to_string(), call_span))?;
            let head = ok_val(Value::Int(start_int), call_span)?;
            let tail_args = vec![
                ok_val(Value::Int(next_start), call_span)?,
                ok_val(Value::Int(end_int), call_span)?,
            ];
            let tail = Rc::new(Thunk::new_pending_builtin(
                "range",
                builtin_range,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $range"),
                Rc::clone(&ctx),
            ));
            ok_val(Value::Seq { head, tail }, call_span)
        }
    }
}

/// `repeat`: Infinite sequence of a repeated value.
///
/// `[call $repeat val]` → infinite Seq: val, val, val, ...
///
/// The value is kept as a thunk (fully lazy — never materialized).
pub(crate) fn builtin_repeat(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
        ..
    } = ctx_arg;
    reject_named("repeat", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    let head = Rc::clone(&args[0]);
    let tail_args = vec![Rc::clone(&args[0])];
    let tail = Rc::new(Thunk::new_pending_builtin(
        "repeat",
        builtin_repeat,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $repeat"),
        Rc::clone(&ctx),
    ));
    ok_val(Value::Seq { head, tail }, call_span)
}

/// Internal helper for `cycle`: produces the next element in the cycle.
///
/// Takes (dict_thunk, index_thunk) where dict is the original collection to cycle
/// through and index is the current position (wrapped modulo length).
pub(crate) fn builtin_cycle_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cycle_step", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let dict = materialize(&args[0], None, &ctx, depth)?;
    let map = match &dict {
        Value::Dict(m) => m,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "cycle".to_string(),
                "Dict",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    let idx = materialize(&args[1], None, &ctx, depth)?;
    let idx_int = match idx {
        Value::Int(i) => i,
        other => {
            return Err(EvalError::type_mismatch_ctx(
                "cycle".to_string(),
                "Int",
                other.type_name(),
                call_span,
            )
            .into())
        }
    };

    if map.is_empty() {
        return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
    }

    let len = map.len() as i64;
    let current_idx = idx_int % len;
    let next_idx = (idx_int + 1) % len;

    // Get the value at current_idx
    let head = map
        .get_index(current_idx as usize)
        .map(|(_, v)| Rc::clone(v))
        .ok_or_else(|| EvalError::internal("cycle: index out of bounds".to_string(), call_span))?;

    // Create tail as PendingBuiltin for next step
    let tail_args = vec![
        Rc::clone(&args[0]),
        ok_val(Value::Int(next_idx), call_span)?,
    ];
    let tail = Rc::new(Thunk::new_pending_builtin(
        "cycle",
        builtin_cycle_step,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $cycle"),
        Rc::clone(&ctx),
    ));

    ok_val(Value::Seq { head, tail }, call_span)
}

/// `cycle`: Infinite sequence cycling through entries of a dict.
///
/// `[call $cycle xs]` → infinite Seq cycling through entries of xs by position.
///
/// Materializes xs to verify it's a non-empty Dict, then delegates to
/// `cycle_step` helper for lazy iteration.
pub(crate) fn builtin_cycle(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("cycle", named, call_span)?;
    if args.len() != 1 {
        return Err(EvalError::arity_mismatch(1, args.len(), call_span).into());
    }

    let val = materialize(&args[0], None, &ctx, depth)?;
    match val {
        Value::Dict(ref map) => {
            if map.is_empty() {
                return Err(EvalError::empty_collection("cycle".to_string(), call_span).into());
            }
            // Start cycling from index 0
            builtin_cycle_step(BuiltinArgs {
                args: &[Rc::clone(&args[0]), ok_val(Value::Int(0), call_span)?],
                named: &IndexMap::new(),
                depth,
                call_span,
                ctx: Rc::clone(&ctx),
            })
        }
        other => Err(EvalError::type_mismatch_ctx(
            "cycle".to_string(),
            "Dict",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `iterate`: Infinite sequence of iterated function applications.
///
/// `[call $iterate $f $x]` → infinite Seq: x, f(x), f(f(x)), ...
///
/// Both f and x are kept as thunks (fully lazy). The tail contains a PendingCall
/// for f(x), wrapped in a PendingBuiltin for the next iterate step.
pub(crate) fn builtin_iterate(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("iterate", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let f = Rc::clone(&args[0]);
    let x = Rc::clone(&args[1]);

    // head = x (lazy)
    let head = Rc::clone(&x);

    // Create f(x) as PendingCall
    // Use stdlib env as caller_env since there's no lexical call site for builtin-internal calls
    let f_of_x = Rc::new(Thunk::new_pending_call(
        Rc::clone(&f),
        vec![Rc::clone(&x)],
        IndexMap::new(),
        call_span,
        Rc::clone(&ctx.config.stdlib_env),
        call_span,
        Cow::Borrowed("iterate"),
        Rc::clone(&ctx),
    ));

    // tail = iterate(f, f(x))
    let tail_args = vec![Rc::clone(&f), f_of_x];
    let tail = Rc::new(Thunk::new_pending_builtin(
        "iterate",
        builtin_iterate,
        tail_args,
        IndexMap::new(),
        depth + 1,
        call_span,
        Cow::Borrowed("call $iterate"),
        Rc::clone(&ctx),
    ));

    ok_val(Value::Seq { head, tail }, call_span)
}

/// Internal helper for `unfold`: performs one unfold step.
///
/// Takes (step_function, seed) and calls step(seed), which should return either:
/// - A 2-element dict [value next_seed] to continue
/// - An empty dict [] to terminate
pub(crate) fn builtin_unfold_step(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("unfold_step", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    let step = Rc::clone(&args[0]);
    let seed = Rc::clone(&args[1]);

    // Call step(seed) as PendingCall, then materialize it
    let step_result_thunk = Rc::new(Thunk::new_pending_call(
        step.clone(),
        vec![seed],
        IndexMap::new(),
        call_span,
        Rc::clone(&ctx.config.stdlib_env),
        call_span,
        Cow::Borrowed("unfold"),
        Rc::clone(&ctx),
    ));
    let step_result = materialize(&step_result_thunk, None, &ctx, depth)?;

    match step_result {
        Value::Dict(ref map) if map.is_empty() => {
            // Termination: return empty dict
            ok_val(Value::Dict(IndexMap::new()), call_span)
        }
        Value::Dict(ref map) if map.len() >= 2 => {
            // Extract first two values (ignore keys)
            let mut iter = map.values();
            let value = Rc::clone(iter.next().unwrap());
            let next_seed = Rc::clone(iter.next().unwrap());

            // head = value (lazy)
            let head = value;

            // tail = unfold_step(step, next_seed)
            let tail_args = vec![step, next_seed];
            let tail = Rc::new(Thunk::new_pending_builtin(
                "unfold",
                builtin_unfold_step,
                tail_args,
                IndexMap::new(),
                depth + 1,
                call_span,
                Cow::Borrowed("call $unfold"),
                Rc::clone(&ctx),
            ));

            ok_val(Value::Seq { head, tail }, call_span)
        }
        Value::Dict(ref map) => Err(EvalError::type_mismatch_ctx(
            "unfold".to_string(),
            "Dict with at least 2 entries",
            &format!(
                "Dict with {} {}",
                map.len(),
                if map.len() == 1 { "entry" } else { "entries" }
            ),
            call_span,
        )
        .into()),
        other => Err(EvalError::type_mismatch_ctx(
            "unfold".to_string(),
            "Dict",
            other.type_name(),
            call_span,
        )
        .into()),
    }
}

/// `unfold`: Generate a sequence from a step function and seed.
///
/// `[call $unfold $step $seed]` → Seq where step(seed) returns [value next_seed]
/// or [] to stop.
///
/// Fully lazy — the step function is not called until the result is materialized.
pub(crate) fn builtin_unfold(ctx_arg: BuiltinArgs) -> EvalResult<Rc<Thunk>> {
    let BuiltinArgs {
        args,
        named,
        depth,
        call_span,
        ctx,
    } = ctx_arg;
    reject_named("unfold", named, call_span)?;
    if args.len() != 2 {
        return Err(EvalError::arity_mismatch(2, args.len(), call_span).into());
    }

    // Return PendingBuiltin wrapping unfold_step — fully lazy
    let tail_args = vec![Rc::clone(&args[0]), Rc::clone(&args[1])];
    let result = Rc::new(Thunk::new_pending_builtin(
        "unfold",
        builtin_unfold_step,
        tail_args,
        IndexMap::new(),
        depth,
        call_span,
        Cow::Borrowed("call $unfold"),
        Rc::clone(&ctx),
    ));
    Ok(result)
}
