//! Deep materialization: recursively force all thunks in a value tree.
//!
//! This module implements the deep materialization algorithm with cycle detection
//! and sharing preservation. Deep materialization is ONLY called at output boundaries
//! (CLI JSON output, REPL display) and is NEVER part of normal evaluation.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use indexmap::IndexMap;

use crate::ast::Span;
use crate::builtins::flatten_overlay;
use crate::builtins::MAX_COLLECT_SIZE;
use crate::error::{EvalError, EvalResult};
use crate::eval::{materialize, EvalContext};
use crate::value::{Key, Thunk, ThunkId, Value};

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
/// Cycle detection and sharing preservation are handled via a
/// `HashMap<*const Thunk, Option<Arc<Thunk>>>` cache; see the
/// dual-purpose cache semantics in `force_thunk`.
///
/// `call_site_span` is the span of the call site (e.g., a builtin call) that
/// triggered deep materialization. If provided, it is attached to errors as the
/// materialization-site span. If `None`, the thunk's own span is used.
pub fn deep_materialize(
    val: &Value,
    ctx: &Arc<EvalContext>,
    call_site_span: Option<&Span>,
) -> EvalResult<Value> {
    // Fast path: primitives and functions need no traversal and no cache allocation.
    // This avoids a HashMap heap allocation for the common case where the top-level
    // value is already a scalar (Int, Float, Bool, String) or a function.
    match val {
        Value::Int(_)
        | Value::Float(_)
        | Value::String { .. }
        | Value::Bool(_)
        | Value::Bytes { .. }
        | Value::Function { .. }
        | Value::Builtin(_)
        | Value::Variant { payload: None, .. } => return Ok(val.clone()),
        _ => {}
    }
    let mut cache: HashMap<*const Thunk, Option<Arc<Thunk>>> = HashMap::new();
    let initial_span = call_site_span.copied().unwrap_or_else(Span::origin);
    deep_materialize_impl(val, ctx, &mut cache, 0, initial_span)
}

// Implementation note (iterative-eval-d): The iterative work-stack deep_materialize_impl
// eliminates O(nesting) Rust stack frames. Before: 100-deep dict → 100 recursive calls.
// After: 100-deep dict → 100 work items processed in a loop (constant Rust stack depth).
// Avoids O(n) repeated key collection by storing Rc<IndexMap> directly in BuildDict.
// (Formal benchmarks via criterion are tracked as future work in perf-foundations.)

// ---------------------------------------------------------------------------
// Iterative work-stack items
// ---------------------------------------------------------------------------

/// An item on the iterative work stack.
///
/// The traversal uses two stacks:
///
/// - `work_stack`: items to process (LIFO). Items that need results from
///   sub-items appear BELOW those sub-items on the stack.
/// - `value_stack`: completed `Arc<Thunk>` results (LIFO). Each `Force` item
///   pushes exactly one result. Each `Build*` collector pops N results and
///   pushes one assembled result.
///
/// The protocol for structural values (Dict / Seq / Proxy):
/// 1. Push `Build*` collector first (deepest on work stack → runs last).
/// 2. Push one `Force` item per child in reverse order so the first child
///    lands on top of the work stack → processed first → result deepest on
///    value stack (correct order for the collector).
enum WorkItem {
    /// Force `thunk`, materialize it one level, then:
    /// - For primitives / functions: push the wrapped result onto `value_stack`.
    /// - For Dict / Seq / Proxy: push child `Force` items and a `Build*`
    ///   collector onto `work_stack`.  Nothing is pushed to `value_stack`
    ///   immediately; the collector does that after assembling children.
    Force {
        thunk: Arc<Thunk>,
        seq_depth: usize,
        /// The span to use for materialization errors. Either the original
        /// call-site span from `deep_materialize` or the thunk's own span.
        mat_span: Span,
    },
    /// Collect entries from `value_stack`, assemble a `Value::Dict`,
    /// wrap as a `Materialized` thunk, and push onto `value_stack`.
    /// `thunk_ptr` is the original thunk pointer — used to update the sharing
    /// cache after the dict is assembled.
    /// `dict_map` provides the original IndexMap to extract keys during assembly.
    BuildDict {
        dict_map: Rc<IndexMap<Key, ThunkId>>,
        span: Span,
        /// Original thunk pointer — updated in cache after assembly.
        /// `None` if the dict is a root value (no thunk to cache).
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop two thunks from `value_stack` (tail on top, head below), assemble
    /// a `Value::Seq`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildSeq {
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop one thunk from `value_stack` (the handler), assemble a
    /// `Value::Proxy`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildProxy {
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
    /// Pop one thunk from `value_stack` (the payload), assemble a
    /// `Value::Variant`, wrap as a `Materialized` thunk, push onto `value_stack`.
    BuildVariant {
        tag: String,
        span: Span,
        thunk_ptr: Option<*const Thunk>,
    },
}

/// Deep-force a value, using an explicit work stack to avoid Rust call-stack
/// recursion for deeply nested dicts and seq spines.
fn deep_materialize_impl(
    root_val: &Value,
    ctx: &Arc<EvalContext>,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    current_span: Span,
) -> EvalResult<Value> {
    // Fast path: primitives and functions need no traversal.
    match root_val {
        Value::Int(_)
        | Value::Float(_)
        | Value::String { .. }
        | Value::Bool(_)
        | Value::Bytes { .. }
        | Value::Function { .. }
        | Value::Builtin(_)
        | Value::Variant { payload: None, .. } => return Ok(root_val.clone()),
        _ => {}
    }

    // For structural values we need the work stack.  Seed it by expanding the
    // root value's immediate children.  The root has no thunk pointer in the
    // cache (it was already materialized by the caller).
    let mut work_stack: Vec<WorkItem> = Vec::new();
    let mut value_stack: Vec<Arc<Thunk>> = Vec::new();

    push_structural(
        root_val,
        cache,
        seq_depth,
        current_span,
        None,         // root has no thunk pointer
        current_span, // propagate call-site span
        &mut work_stack,
        &mut value_stack,
        ctx,
    )?;

    // Main work loop.
    while let Some(item) = work_stack.pop() {
        match item {
            WorkItem::Force {
                thunk,
                seq_depth: item_seq_depth,
                mat_span,
            } => {
                process_force(
                    &thunk,
                    ctx,
                    cache,
                    item_seq_depth,
                    mat_span,
                    &mut work_stack,
                    &mut value_stack,
                )?;
            }
            WorkItem::BuildDict {
                dict_map,
                span,
                thunk_ptr,
            } => {
                let key_count = dict_map.len();
                let stack_len = value_stack.len();
                debug_assert!(
                    stack_len >= key_count,
                    "BuildDict: expected {key_count} values on stack, have {stack_len}"
                );
                let start = stack_len - key_count;
                let mut result: IndexMap<Key, ThunkId> = IndexMap::with_capacity(key_count);
                // Iterate keys from dict_map (which preserves insertion order)
                for (key, thunk) in dict_map.keys().cloned().zip(value_stack.drain(start..)) {
                    result.insert(key, ctx.alloc_thunk(thunk));
                }
                let assembled = Arc::new(Thunk::new_materialized(Value::Dict(result), span));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            WorkItem::BuildSeq { span, thunk_ptr } => {
                let tail = value_stack
                    .pop()
                    .expect("BuildSeq: missing tail on value_stack");
                let head = value_stack
                    .pop()
                    .expect("BuildSeq: missing head on value_stack");
                let head_id = ctx.alloc_thunk(head);
                let tail_id = ctx.alloc_thunk(tail);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Seq {
                        head: head_id,
                        tail: tail_id,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            WorkItem::BuildProxy { span, thunk_ptr } => {
                let handler = value_stack
                    .pop()
                    .expect("BuildProxy: missing handler on value_stack");
                let handler_id = ctx.alloc_thunk(handler);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Proxy {
                        handler: handler_id,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
            WorkItem::BuildVariant {
                tag,
                span,
                thunk_ptr,
            } => {
                let payload_thunk = value_stack
                    .pop()
                    .expect("BuildVariant: missing payload on value_stack");
                let payload_id = ctx.alloc_thunk(payload_thunk);
                let assembled = Arc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag,
                        payload: Some(payload_id),
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&assembled)));
                }
                value_stack.push(assembled);
            }
        }
    }

    // The work loop should leave exactly one result on the value stack.
    debug_assert_eq!(
        value_stack.len(),
        1,
        "deep_materialize_impl: expected 1 result on value_stack, got {}",
        value_stack.len()
    );
    let result_thunk = value_stack
        .pop()
        .expect("deep_materialize_impl: empty value_stack after work loop");

    // Extract the materialized value from the result thunk.
    match result_thunk.try_get_materialized() {
        Some(v) => Ok(v),
        None => {
            unreachable!("deep_materialize_impl: result thunk is not Materialized after work loop")
        }
    }
}

/// Push work items to process the children of a structural `Value`
/// (Dict / Seq / Proxy) onto the work and value stacks.
///
/// For primitives and functions, push a pre-materialized thunk directly onto
/// `value_stack` (no child work needed).
///
/// `thunk_ptr` is the cache key for the parent thunk (if any), forwarded to
/// the `Build*` collector so it can update the sharing cache after assembly.
///
/// `mat_span` is the materialization-site span to thread through nested materializations.
///
/// Returns `Err` if the Seq spine guard fires (seq_depth >= MAX_COLLECT_SIZE).
#[allow(clippy::too_many_arguments)] // Internal helper for deep_materialize work queue
fn push_structural(
    val: &Value,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    span: Span,
    thunk_ptr: Option<*const Thunk>,
    mat_span: Span,
    work_stack: &mut Vec<WorkItem>,
    value_stack: &mut Vec<Arc<Thunk>>,
    ctx: &Arc<EvalContext>,
) -> EvalResult<()> {
    match val {
        Value::Overlay(l, r) => {
            // Flatten overlay to dict, then recurse as Dict.
            let map = flatten_overlay(l, r, "deep-materialize", ctx, span)?;
            return push_structural(
                &Value::Dict(map),
                cache,
                seq_depth,
                span,
                thunk_ptr,
                mat_span,
                work_stack,
                value_stack,
                ctx,
            );
        }
        Value::Dict(map) => {
            if map.is_empty() {
                // Empty dict: assemble immediately, no children.
                let t = Arc::new(Thunk::new_materialized(Value::Dict(IndexMap::new()), span));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&t)));
                }
                value_stack.push(t);
                return Ok(());
            }
            // Collector runs last: push first. Store an Rc to the original dict_map
            // so we can iterate its keys during BuildDict without allocating a Vec.
            let dict_map_rc = Rc::new(map.clone());
            work_stack.push(WorkItem::BuildDict {
                dict_map: Rc::clone(&dict_map_rc),
                span,
                thunk_ptr,
            });
            // Push Force items in reverse: first key ends on top → processed
            // first → result deepest on value_stack → collected in order.
            for key in dict_map_rc.keys().rev() {
                let entry_thunk = ctx.get_thunk(map[key]);
                work_stack.push(WorkItem::Force {
                    thunk: entry_thunk,
                    seq_depth: 0, // dict entries reset seq_depth
                    mat_span,     // propagate call-site span through nested materializations
                });
            }
        }
        Value::Seq { head, tail } => {
            // Seq spine guard: prevents unbounded traversal of infinite sequences.
            if seq_depth >= MAX_COLLECT_SIZE {
                return Err(EvalError::resource_limit_exceeded(
                    "cannot deep-materialize an infinite Seq: use $collect with $take first"
                        .to_string(),
                    span,
                )
                .into());
            }
            // Collector runs last: push first.
            work_stack.push(WorkItem::BuildSeq { span, thunk_ptr });
            // Push tail SECOND on work_stack → processed second → lands on TOP
            // of value_stack → BuildSeq pops tail first.
            let tail_thunk = ctx.get_thunk(*tail);
            work_stack.push(WorkItem::Force {
                thunk: tail_thunk,
                seq_depth: seq_depth + 1,
                mat_span, // propagate call-site span through nested materializations
            });
            // Push head LAST on work_stack → processed first → result BELOW
            // tail on value_stack → BuildSeq pops head after tail.
            let head_thunk = ctx.get_thunk(*head);
            work_stack.push(WorkItem::Force {
                thunk: head_thunk,
                seq_depth: 0, // head resets seq_depth
                mat_span,     // propagate call-site span through nested materializations
            });
        }
        Value::Proxy { handler } => {
            work_stack.push(WorkItem::BuildProxy { span, thunk_ptr });
            let handler_thunk = ctx.get_thunk(*handler);
            work_stack.push(WorkItem::Force {
                thunk: handler_thunk,
                seq_depth: 0,
                mat_span, // propagate call-site span through nested materializations
            });
        }
        Value::Variant { tag, payload } => {
            if let Some(payload_id) = payload {
                // Variant with payload: force the payload recursively
                work_stack.push(WorkItem::BuildVariant {
                    tag: tag.clone(),
                    span,
                    thunk_ptr,
                });
                let payload_thunk = ctx.get_thunk(*payload_id);
                work_stack.push(WorkItem::Force {
                    thunk: payload_thunk,
                    seq_depth: 0, // variant payload resets seq_depth
                    mat_span,     // propagate call-site span through nested materializations
                });
            } else {
                // Variant without payload: leaf value, no children to traverse
                let t = Arc::new(Thunk::new_materialized(
                    Value::Variant {
                        tag: tag.clone(),
                        payload: None,
                    },
                    span,
                ));
                if let Some(ptr) = thunk_ptr {
                    cache.insert(ptr, Some(Arc::clone(&t)));
                }
                value_stack.push(t);
            }
        }
        // Primitives and functions: no children.
        other => {
            let t = Arc::new(Thunk::new_materialized(other.clone(), span));
            if let Some(ptr) = thunk_ptr {
                cache.insert(ptr, Some(Arc::clone(&t)));
            }
            value_stack.push(t);
        }
    }
    Ok(())
}

/// Process a single `WorkItem::Force`: check the sharing/cycle cache, call
/// [`materialize`], then expand the materialized value's structure.
///
/// On success, exactly one new result is eventually pushed onto `value_stack`
/// (either immediately for cached/leaf values, or after the children are
/// processed by a `Build*` collector).
///
/// On error, propagates the error.  The cache sentinel (`None`) is removed
/// before propagating to prevent cache poisoning (same as the old
/// `deep_materialize_thunk`).
fn process_force(
    thunk: &Arc<Thunk>,
    ctx: &Arc<EvalContext>,
    cache: &mut HashMap<*const Thunk, Option<Arc<Thunk>>>,
    seq_depth: usize,
    mat_span: Span,
    work_stack: &mut Vec<WorkItem>,
    value_stack: &mut Vec<Arc<Thunk>>,
) -> EvalResult<()> {
    let thunk_ptr = Arc::as_ptr(thunk);

    // Cache lookup: sharing hit or cycle sentinel.
    match cache.get(&thunk_ptr) {
        Some(Some(cached)) => {
            value_stack.push(Arc::clone(cached));
            return Ok(());
        }
        Some(None) => {
            // Cycle sentinel: return the original thunk unchanged.
            // Returns Arc::clone(thunk) safely because materialize() has already transitioned
            // the thunk to Materialized; sub-structure of the returned thunk is not deep-forced
            // (documented behavior for cycles).
            value_stack.push(Arc::clone(thunk));
            return Ok(());
        }
        None => {}
    }

    let thunk_span = thunk.span;

    // Insert the in-progress (cycle) sentinel.
    cache.insert(thunk_ptr, None);

    // Materialize the thunk one level.
    // Use the mat_span from the WorkItem::Force, which is either the original
    // call-site span from deep_materialize or the thunk's own span.
    let v = match crate::async_rt::block_on_anywhere(materialize(thunk, Some(&mat_span), ctx)) {
        Ok(v) => v,
        Err(e) => {
            // Clean up sentinel on error (same as old deep_materialize_thunk).
            cache.remove(&thunk_ptr);
            return Err(e);
        }
    };

    // Expand the materialized value.  For leaf values, push directly to
    // value_stack and update cache.  For structural values, push_structural
    // queues child work items and a Build* collector; the collector updates
    // the cache with `thunk_ptr` when it assembles the final result.
    push_structural(
        &v,
        cache,
        seq_depth,
        thunk_span,
        Some(thunk_ptr),
        mat_span, // propagate call-site span through nested materializations
        work_stack,
        value_stack,
        ctx,
    )
    .map_err(|mut e| {
        // Depth / infinite-Seq error from a child: attach the source thunk's
        // span as a frame so depth-exceeded errors show where in the structure
        // the recursion limit was hit.
        if thunk_span != Span::origin() {
            e.push_frame("deep-materializing".to_string(), thunk_span);
        }
        // Remove the sentinel for this thunk since we failed.
        // (push_structural already cleaned up any sentinels it inserted.)
        cache.remove(&thunk_ptr);
        // Clear work_stack to avoid leaking WorkItem::Force Arc<Thunk> references.
        // When push_structural fails mid-traversal, it may have pushed Build* and
        // Force items that will never be processed. Clearing prevents Rc leak.
        work_stack.clear();
        e
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Expr, Spanned};
    use crate::test_util::test_span;
    use crate::value::{Environment, Key};
    use std::sync::RwLock;

    fn test_ctx() -> Arc<EvalContext> {
        let base_dir = cap_std::fs::Dir::open_ambient_dir(".", cap_std::ambient_authority())
            .expect("failed to open test base_dir");
        let stdlib_env = crate::builtins::create_stdlib_env().expect("stdlib failed");
        let type_stage_env =
            crate::imports::build_type_stage_env().unwrap_or_else(|| Arc::clone(&stdlib_env));
        EvalContext::new(base_dir, stdlib_env, type_stage_env, false)
    }

    #[test]
    fn test_deep_materialize_cycle_sentinel() {
        // Test the cycle detection path.
        // When a thunk pointer is already in the cache with None value
        // (the cycle sentinel), process_force should return the original thunk unchanged.
        //
        // Uses `Thunk::new_materialized` to isolate cache-lookup logic from evaluation;
        // real cycles are encountered after `materialize()` has already transitioned the thunk.
        let span = test_span(1, 1, 1, 5);
        let thunk = Arc::new(Thunk::new_materialized(Value::Int(42), span));

        // Create a cache and pre-populate it with a None entry for this thunk
        let mut cache = std::collections::HashMap::new();
        let thunk_ptr = Arc::as_ptr(&thunk);
        cache.insert(thunk_ptr, None);

        // Call process_force with the pre-populated cache
        let mut work_stack = Vec::new();
        let mut value_stack = Vec::new();
        let ctx = test_ctx();
        process_force(
            &thunk,
            &ctx,
            &mut cache,
            0,
            span, // mat_span
            &mut work_stack,
            &mut value_stack,
        )
        .unwrap();

        // The original thunk should have been pushed onto value_stack
        assert_eq!(value_stack.len(), 1);
        assert!(
            Arc::ptr_eq(&thunk, &value_stack[0]),
            "process_force must push the original thunk when cycle sentinel (None) is found in cache"
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
        let env = Arc::new(RwLock::new(Environment::new()));
        let ctx = test_ctx();

        // Create an unevaluated thunk and allocate it once — same ThunkId for both positions
        let shared_thunk_rc = Arc::new(Thunk::new_unevaluated(expr, env, Arc::clone(&ctx), span));
        let shared_id = ctx.alloc_thunk(shared_thunk_rc);

        // Place the same ThunkId in two positions of a dict
        let mut map: IndexMap<crate::value::Key, crate::arena::ThunkId> = IndexMap::new();
        map.insert(Key::String("a".into()), shared_id);
        map.insert(Key::String("b".into()), shared_id);
        let val = Value::Dict(map);

        // Deep materialize the container
        let result = deep_materialize(&val, &ctx, None).unwrap();

        match result {
            Value::Dict(map) => {
                let a = &map[&Key::String("a".into())];
                let b = &map[&Key::String("b".into())];

                // Verify both entries resolve to the same value (ThunkId equality not guaranteed).
                let va = crate::async_rt::block_on_anywhere(crate::eval::materialize(
                    &ctx.get_thunk(*a),
                    None,
                    &ctx,
                ))
                .unwrap();
                let vb = crate::async_rt::block_on_anywhere(crate::eval::materialize(
                    &ctx.get_thunk(*b),
                    None,
                    &ctx,
                ))
                .unwrap();
                assert_eq!(va, Value::Int(42), "entry a should be Int(42)");
                assert_eq!(vb, Value::Int(42), "entry b should be Int(42)");
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    #[test]
    fn test_deep_materialize_cache_cleanup_on_materialize_error() {
        // Test that cacheable errors (undefined variable) leave the thunk in ThunkState::Failed
        // and are memoized for retry — a second deep_materialize call returns the same cached error
        // rather than re-evaluating. This complements test_deep_materialize_cache_cleanup_on_error
        // which tests DepthExceeded (non-cacheable, sentinel removed on error).
        let ctx = test_ctx();
        let span = test_span(1, 1, 1, 5);

        // Create a thunk that will fail with a cacheable error (undefined variable)
        let env = Arc::new(RwLock::new(Environment::new()));
        let error_expr = Rc::new(Spanned::new(Expr::var_ref("undefined".into()), span));
        let error_thunk = Arc::new(Thunk::new_unevaluated(
            error_expr,
            env,
            Arc::clone(&ctx),
            span,
        ));

        // Place the error thunk in a dict
        let error_id = ctx.alloc_thunk(Arc::clone(&error_thunk));
        let mut map: IndexMap<Key, crate::arena::ThunkId> = IndexMap::new();
        map.insert(Key::String("x".into()), error_id);
        let dict_val = Value::Dict(map);

        // Attempt to deep materialize — should fail
        let err = deep_materialize(&dict_val, &ctx, None).unwrap_err();
        assert!(
            err.kind.to_string().contains("undefined"),
            "Expected undefined variable error, got: {}",
            err.kind.to_string()
        );

        // Verify the error_thunk is in Failed state (cacheable error was cached)
        {
            // Check that the thunk has a failed result
            assert!(
                error_thunk.try_get_materialized().is_none(),
                "Expected thunk to not be materialized (should be Failed)"
            );
            // The error is cached - we can't easily inspect it without .state(),
            // but we can verify a second materialization attempt fails with the same error
        }

        // A second deep_materialize should also fail (error is cached in thunk)
        let err2 = deep_materialize(&dict_val, &ctx, None).unwrap_err();
        assert!(
            err2.kind.to_string().contains("undefined"),
            "Expected cached error on retry, got: {}",
            err2.kind.to_string()
        );
    }
}
