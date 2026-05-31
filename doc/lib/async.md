# async

### `await-all`

[unindent "\nWait for all tasks and return a Seq of results in submission order.\n\nExample: [await-all [[task [+ 1 2]] [task [* 3 4]]]] => [3 12]\n\nNote: Each element must be a Task value. Results are in input order.\nThe map drives evaluation; for parallel work, spawn tasks before calling await-all.\n"]

```tinct
fn@Any [let tasks]
```

### `recv-all`

[unindent "\nReceive exactly n items from a channel, returning them as a Seq.\n\nExample: [recv-all ch 3]\n\nNote: Blocks until all n items have been received. Uses reduce over [range 0 n]\nto collect results; the accumulator grows by one item per step.\n"]

```tinct
fn@Any [let ch@Channel n@Int]
```

### `exit`

[unindent "\nGraceful shutdown: cancel all tasks, drain, then exit with code.\n\nExample: [exit 0]\n\nNote: Waits indefinitely for tasks to drain. For bounded wait, use graceful-exit.\n"]

```tinct
fn@Any [let code@Int]
```

### `graceful-exit`

[unindent "\nBounded graceful shutdown: cancel tasks, drain with timeout, then exit.\n\nExample: [graceful-exit 0 5000]  # 5-second drain timeout\n\nArgs:\n- code: exit code\n- timeout-ms: maximum milliseconds to wait for drain\n\nIf drain times out, exits immediately without waiting for remaining tasks.\n"]

```tinct
fn@Any [let code@Int timeout-ms@Int]
```

### `loop-select`

[unindent "\nRepeatedly select from channels until the context is cancelled.\n\nExample:\n  [loop-select [context]\n    [[sig-ch  [fn [_] [exit 0]]]\n     [req-ch  [fn [req] [task [handle req]]]]]\n    identity]\n\nNote: Tail-recursive; hits CEK continuation stack limit for very deep recursion.\nFor long-running servers, use an explicit [task [loop ...]] pattern until\ntco-proper-fix is implemented.\n"]

```tinct
fn@Any [let ctx sources@Seq handler@Fn]
```

### `retry`

[unindent "\nRetry a zero-arg function up to n times on error.\n\nExample: [retry 3 [fn [] [flaky-operation]]]\n\nNote: Makes up to n+1 total attempts (1 initial + n retries).\nRaises \"retry limit exceeded\" if all attempts fail.\nInherently materializing: must force each attempt to catch errors.\n"]

```tinct
fn@Any [let n@Int thunk@Fn]
```

### `finally`

[unindent "\nRun cleanup regardless of whether body succeeds or errors, then return/re-raise.\n\nExample:\n  [finally\n    [fn [] [close-conn conn]]\n    [fn [] [do-work-with conn]]]\n\nNote: cleanup runs in a non-cancellable context — it will complete even if\nthe parent context is cancelled. Cleanup return value is discarded.\nRust-level EvalErrors bypass finally.\n"]

```tinct
fn@Any [let cleanup@Fn body@Fn]
```

