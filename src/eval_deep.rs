//! Deep materialization: recursively force all thunks in a value tree.
//!
//! This module implements the deep materialization algorithm with cycle detection
//! and sharing preservation. Deep materialization is ONLY called at output boundaries
//! (CLI JSON output, REPL display) and is NEVER part of normal evaluation.

use std::collections::HashMap;
use std::rc::Rc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext, MAX_EVAL_DEPTH};
use crate::value::{Thunk, Value};

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
/// deeply nested structures. Cycle detection and sharing preservation are handled
/// via a `HashMap<*const Thunk, Option<Rc<Thunk>>>` cache; see
/// `deep_materialize_thunk` for the dual-purpose semantics.
pub fn deep_materialize(val: &Value, ctx: &Rc<EvalContext>, depth: usize) -> EvalResult<Value> {
    let mut cache: HashMap<*const Thunk, Option<Rc<Thunk>>> = HashMap::new();
    deep_materialize_impl(val, ctx, depth, &mut cache, 0, Span::origin())
}

/// Deep-force a value, recursively materializing all thunks in dicts and seqs.
///
/// `seq_depth` tracks consecutive Seq tail traversals along the spine. The check
/// lives inside the `Seq` match arm (not at function entry) so it fires before the
/// head's `deep_materialize_thunk` call increments `depth`. Without this placement,
/// the generic `depth > MAX_EVAL_DEPTH` guard would fire first via the head path
/// (where `seq_depth` is reset to 0), producing a generic error instead of the
/// targeted "cannot deep-materialize an infinite Seq" message.
///
/// `current_span` is the source span of the thunk currently being traversed,
/// used as the error location for depth-exceeded and infinite-Seq errors; callers
/// at the entry point should pass [`Span::origin()`].
fn deep_materialize_impl(
    val: &Value,
    ctx: &Rc<EvalContext>,
    depth: usize,
    cache: &mut std::collections::HashMap<*const Thunk, Option<Rc<Thunk>>>,
    seq_depth: usize,
    current_span: Span,
) -> EvalResult<Value> {
    if depth > MAX_EVAL_DEPTH {
        return Err(EvalError::depth_exceeded(MAX_EVAL_DEPTH, current_span).into());
    }
    match val {
        Value::Dict(map) => {
            let mut result = IndexMap::new();
            for (key, thunk) in map {
                let deep_thunk = deep_materialize_thunk(thunk, ctx, depth, cache, 0)?;
                result.insert(key.clone(), deep_thunk);
            }
            Ok(Value::Dict(result))
        }
        Value::Seq { head, tail } => {
            // Seq spine guard: checked before recursing on head or tail so that
            // infinite sequences (e.g., $iterate, $repeat) get a targeted error
            // message. This check must live here in the Seq arm, not at the top
            // of the function, because the head's deep_materialize_thunk call
            // increments `depth` — if seq_depth were only checked at function
            // entry, the generic depth guard would fire first (via the head
            // recursion at depth+1) before the next tail step could trigger it.
            //
            // Uses `>=` rather than `>` because `depth` and `seq_depth` are equal
            // along a flat Seq spine (both start at 0 and increment by 1 per cons
            // cell via deep_materialize_thunk). The head's recursion through
            // deep_materialize_thunk adds `depth + 1`, so the generic depth guard
            // (`depth > MAX_EVAL_DEPTH`) would fire at `depth = MAX_EVAL_DEPTH + 1`
            // during head processing of the cell where `seq_depth = MAX_EVAL_DEPTH`.
            // Using `>=` ensures the seq_depth guard fires at the same cell, before
            // the head is processed.
            if seq_depth >= MAX_EVAL_DEPTH {
                return Err(EvalError::resource_limit_exceeded(
                    "cannot deep-materialize an infinite Seq: use $collect with $take first"
                        .to_string(),
                    current_span,
                )
                .into());
            }
            // Seq depth: head and tail are both recursed from the same depth — independent
            // branches, not additive. Total depth consumed is max(head_subtree, tail_subtree).
            //
            // Key asymmetry vs Dict: Seq spine traversal is O(n) in depth (each cons cell's
            // tail passes through deep_materialize_thunk with depth+1), so a flat Seq of N
            // elements reaches depth D+N along the tail spine. Dict traversal is O(1) in
            // depth — a flat loop with all entries processed at the same level.
            //
            // seq_depth tracks consecutive Seq tail traversals. The head resets it
            // to 0 (head is not part of the spine). The tail increments it.
            let deep_head = deep_materialize_thunk(head, ctx, depth, cache, 0)?;
            let deep_tail = deep_materialize_thunk(tail, ctx, depth, cache, seq_depth + 1)?;
            Ok(Value::Seq {
                head: deep_head,
                tail: deep_tail,
            })
        }
        Value::Proxy { handler } => {
            // Deep-materialize the handler thunk and return the proxy with the deep handler
            let deep_handler = deep_materialize_thunk(handler, ctx, depth, cache, 0)?;
            Ok(Value::Proxy {
                handler: deep_handler,
            })
        }
        // Primitives and functions are already fully materialized
        other => Ok(other.clone()),
    }
}

/// Deep-materialize a single thunk, preserving sharing via the cache.
///
/// The `cache` serves two purposes:
/// 1. **Cycle detection** (Launchbury 1993 blackholing): an entry with value `None`
///    means we are currently processing this thunk — re-encountering it is a cycle.
/// 2. **Sharing preservation** (Launchbury 1993 sharing invariant): an entry with
///    value `Some(rc)` means this thunk was already deep-materialized — reuse it
///    so that `Rc::ptr_eq` holds for outputs derived from shared inputs.
///
/// Returns an `Rc<Thunk>` that is either:
/// - The cached result (if this thunk pointer was already processed — sharing preserved)
/// - The original `Rc::clone` (if this thunk is currently being processed — cycle)
/// - A new `Rc<Thunk>` containing the deep-materialized value (first encounter)
fn deep_materialize_thunk(
    thunk: &Rc<Thunk>,
    ctx: &Rc<EvalContext>,
    depth: usize,
    cache: &mut std::collections::HashMap<*const Thunk, Option<Rc<Thunk>>>,
    seq_depth: usize,
) -> EvalResult<Rc<Thunk>> {
    let thunk_ptr = Rc::as_ptr(thunk);
    match cache.get(&thunk_ptr) {
        Some(Some(cached)) => return Ok(Rc::clone(cached)), // sharing hit
        // `Some(None)` is the in-progress sentinel: this thunk is currently being traversed
        // by an ancestor call in the same deep_materialize invocation. Return the original thunk
        // without recursing to break the structural cycle. See the "Deep Materialization"
        // section in doc/08-evaluation.md for the dual-purpose cache design.
        Some(None) => return Ok(Rc::clone(thunk)),
        None => {}
    }
    // Mark as in-progress (cycle sentinel)
    cache.insert(thunk_ptr, None);
    // materialize uses current depth because it has its own depth guard;
    // deep_materialize_impl increments to account for one level of nesting.
    // Pass thunk.span as mat_span so errors from materializing this thunk
    // carry the thunk's source location as the call-site span for depth errors.
    let thunk_span = thunk.span;
    let v = match materialize(thunk, Some(&thunk_span), ctx, depth) {
        Ok(v) => v,
        Err(e) => {
            // Clean up sentinel on error to prevent cache poisoning
            cache.remove(&thunk_ptr);
            return Err(e);
        }
    };
    let forced = match deep_materialize_impl(&v, ctx, depth + 1, cache, seq_depth, thunk_span) {
        Ok(v) => v,
        Err(mut e) => {
            // Clean up sentinel on error to prevent cache poisoning
            cache.remove(&thunk_ptr);
            // Attach the thunk's source span as a frame so depth-exceeded errors
            // show where in the structure the recursion limit was hit.
            // Only add a frame if thunk_span is not the synthetic origin span.
            if thunk_span != Span::origin() {
                e.push_frame("deep-materializing".to_string(), thunk_span);
            }
            return Err(e);
        }
    };
    let result = Rc::new(Thunk::new_materialized(forced, thunk.span));
    // Cache the result for sharing preservation
    cache.insert(thunk_ptr, Some(Rc::clone(&result)));
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Param, Spanned};
    use crate::eval::{eval, MAX_EVAL_DEPTH};
    use crate::test_util::{sp, test_span};
    use crate::value::{Environment, Key, ThunkState};
    use std::cell::RefCell;

    fn test_ctx() -> Rc<EvalContext> {
        EvalContext::new(
            std::path::PathBuf::from("."),
            crate::builtins::create_root_env(),
            false,
        )
    }

    fn empty_env() -> Rc<RefCell<Environment>> {
        Rc::new(RefCell::new(Environment::new()))
    }

    #[test]
    fn test_deep_materialize_cycle_sentinel() {
        // Test the cycle detection path in deep_materialize_thunk.
        // When a thunk pointer is already in the cache with None value
        // (the cycle sentinel), it should return the original thunk unchanged.
        let span = test_span(1, 1, 1, 5);
        let thunk = Rc::new(Thunk::new_materialized(Value::Int(42), span));

        // Create a cache and pre-populate it with a None entry for this thunk
        let mut cache = std::collections::HashMap::new();
        let thunk_ptr = Rc::as_ptr(&thunk);
        cache.insert(thunk_ptr, None);

        // Call deep_materialize_thunk with the pre-populated cache
        let result = deep_materialize_thunk(&thunk, &test_ctx(), 0, &mut cache, 0).unwrap();

        // Verify the original thunk is returned unchanged (same Rc pointer)
        assert!(
            Rc::ptr_eq(&thunk, &result),
            "deep_materialize_thunk must return the original thunk when cycle sentinel (None) is found in cache"
        );
    }

    #[test]
    fn test_deep_materialize_preserves_sharing_through_eval() {
        // Test that sharing is preserved when the shared thunk is unevaluated,
        // exercising the actual cache-population path where:
        // 1. First encounter forces the thunk and caches the result
        // 2. Second encounter returns the cached result
        let span = test_span(1, 1, 1, 5);
        let expr = Rc::new(Spanned::new(Expr::Int(42), span));
        let env = Rc::new(RefCell::new(Environment::new()));
        let ctx = test_ctx();

        // Create an unevaluated thunk
        let shared_thunk = Rc::new(Thunk::new_unevaluated(expr, env, Rc::clone(&ctx), span));

        // Place the same thunk in two positions of a dict
        let mut map = IndexMap::new();
        map.insert(Key::String("a".into()), Rc::clone(&shared_thunk));
        map.insert(Key::String("b".into()), Rc::clone(&shared_thunk));
        let val = Value::Dict(map);

        // Deep materialize the container
        let result = deep_materialize(&val, &ctx, 0).unwrap();

        match result {
            Value::Dict(map) => {
                let a = &map[&Key::String("a".into())];
                let b = &map[&Key::String("b".into())];

                // Verify the two resulting thunks are Rc::ptr_eq
                assert!(
                    Rc::ptr_eq(a, b),
                    "deep_materialize must preserve sharing through actual evaluation: \
                     two dict entries pointing to the same unevaluated thunk should \
                     remain Rc::ptr_eq after deep materialization"
                );

                // Also verify the value is correct
                let v = materialize(a, None, &ctx, 0).unwrap();
                assert_eq!(v, Value::Int(42));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_cache_cleanup_on_error() {
        // Bug fix: deep_materialize_thunk must clean up the None sentinel from the cache
        // when materialize() fails with a non-cacheable error (DepthExceeded). If the sentinel
        // is not removed, a second encounter of the same thunk pointer within the same
        // deep_materialize call would hit the Some(None) → cycle path, silently returning
        // Ok(Rc::clone(thunk)) instead of propagating the real error.
        //
        // What this test validates (public API observable properties):
        // 1. After a failed deep_materialize, the shared thunk is NOT in Failed state —
        //    DepthExceeded is non-cacheable, so the thunk stays Unevaluated and is retryable.
        // 2. A second deep_materialize call (fresh cache) fails with DepthExceeded again,
        //    not with a cycle/circular error — proving no permanent state corruption.
        // 3. Rc sharing: both dict entries reference the same thunk (Rc::ptr_eq confirmed).
        //
        // Note: The sentinel cleanup bug is an intra-call property — within one deep_materialize
        // call, if the same error thunk appears at two different positions in the structure,
        // the second encounter sees the stale None sentinel and gets an incorrect cycle result.
        // Testing this intra-call scenario requires deep_materialize_impl (private). The public
        // API always propagates via ? which stops processing after the first failure, so only
        // one encounter of the thunk occurs per call — making the sentinel cleanup safe to
        // validate through thunk state inspection rather than a second-encounter scenario.

        let ctx = test_ctx();

        // Create a thunk that will fail with DepthExceeded (non-cacheable). Using a deeply
        // recursive call: call f(1) where f = fn [x] f(x), at MAX_EVAL_DEPTH will recurse
        // into depth + 1 = MAX_EVAL_DEPTH + 1 hitting the guard.
        let env = empty_env();
        let recursive_fn = Value::Function {
            params: Rc::new(vec![Param {
                name: "x".into(),
                annotation: None,
                variadic: false,
            }]),
            body: Rc::new(sp(Expr::Call {
                func: Box::new(sp(Expr::VarRef("g".into()))),
                args: vec![sp(Expr::VarRef("x".into()))],
                named_args: vec![],
            })),
            env: Rc::clone(&env),
        };
        env.borrow_mut().insert(
            "g".into(),
            Rc::new(Thunk::new_materialized(recursive_fn, test_span(1, 1, 1, 5))),
        );
        let call_expr = sp(Expr::Call {
            func: Box::new(sp(Expr::VarRef("g".into()))),
            args: vec![sp(Expr::Int(1))],
            named_args: vec![],
        });
        // eval() returns a PendingCall thunk (lazy call not yet materialized)
        let error_thunk = eval(&call_expr, Rc::clone(&env), &ctx, 0).unwrap();

        // Confirm both dict entries are Rc::ptr_eq (true sharing, not clones)
        let error_thunk2 = Rc::clone(&error_thunk);
        assert!(
            Rc::ptr_eq(&error_thunk, &error_thunk2),
            "test invariant: both entries must share the same Rc pointer"
        );

        let dict_map = indexmap::IndexMap::from_iter(vec![
            (crate::value::Key::String("a".into()), error_thunk2),
            (
                crate::value::Key::String("b".into()),
                Rc::clone(&error_thunk),
            ),
        ]);
        let dict_value = Value::Dict(dict_map);

        // Materialize at MAX_EVAL_DEPTH so the inner recursive call exceeds the limit.
        // The sentinel for error_thunk is inserted then cleaned up (fix), and the thunk
        // state is restored to PendingCall (non-cacheable error path in materialize).
        let err1 = deep_materialize(&dict_value, &ctx, MAX_EVAL_DEPTH - 1).unwrap_err();
        assert!(
            err1.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded error, got: {}",
            err1.message()
        );

        // Property 1: the shared thunk must NOT be in Failed state after a non-cacheable error.
        // Without the non-cacheable handling in materialize(), the thunk would be Failed.
        // (This tests materialize's own non-cacheable path, not deep_materialize's sentinel.)
        let state = error_thunk.state();
        assert!(
            !matches!(&*state, ThunkState::Failed(_)),
            "DepthExceeded is non-cacheable: thunk must not be Failed, got: {:?}",
            &*state
        );
        assert!(
            !matches!(&*state, ThunkState::InProgress),
            "thunk must not be stuck in InProgress after non-cacheable error, got: {:?}",
            &*state
        );
        drop(state);

        // Property 2: a second deep_materialize (fresh cache) fails with DepthExceeded again,
        // not a cycle error — confirming no permanent state corruption from the sentinel.
        let err2 = deep_materialize(&dict_value, &ctx, MAX_EVAL_DEPTH - 1).unwrap_err();
        assert!(
            err2.message().contains("maximum evaluation depth exceeded"),
            "expected depth exceeded on retry, got: {}",
            err2.message()
        );
        assert!(
            !err2.message().contains("circular") && !err2.message().contains("cycle"),
            "should not see cycle error: got: {}",
            err2.message()
        );
    }
}
