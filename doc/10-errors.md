# Error Handling

## User Guide

### Letting Errors Propagate

The default: do nothing. Errors in tinct propagate automatically through the thunk graph. If a value fails to materialize, anything that depends on it also fails. Unused values never error.

```tinct
[
  data:   [slurp %fs "config.json"]     # fails if file missing
  parsed: [from-json data]              # fails if data failed
  host:   [get "host" parsed]           # fails if parsed failed
  port:   [get "port" parsed]           # independent — doesn't depend on host
]
```

If `data` fails, accessing `host` or `parsed` fails with the original error (decorated with where you accessed it). Accessing `port` fails too. But a field that never gets accessed never errors. This is the right default for most code.

### Raising Errors

Use `[raise "message"]` to raise an error from your own code:

```tinct
validated-port: [fn [p]
  [if [and [>= p 1] [<= p 65535]]
    p
    [raise [str "invalid port: " p " (must be 1-65535)"]]]]

# Usage
port: [validated-port 8080]   # → 8080
port: [validated-port 0]      # error: "invalid port: 0 (must be 1-65535)"
```

`raise` always raises — it never returns. Type: `Str → Never`.

### Catching Errors with `try`

`try` takes a zero-argument function and returns `[Ok value]` or `[Error message]`:

```tinct
result: [try [fn [] [from-json "[invalid"]]]
# → [Error "from-json: invalid JSON: ..."]

result: [try [fn [] [from-json "[1, 2, 3]"]]]
# → [Ok [0 1 1 2 2 3]]
```

Dispatch on the result with `match`:

```tinct
[match [try [fn [] [parse-config]]]
  [Ok config]:  [start-server config]
  [Error msg]:    [raise [str "startup failed: " msg]]]
```

`try` only catches the body — not failures in setting up arguments. Wrap exactly what you want to recover from.

### Returning Errors from Functions

For functions that may legitimately fail, return `[Ok value]` or `[Error message]` explicitly rather than raising:

```tinct
parse-port: [fn [s]
  [try [fn []
    [let [n: [int s]]
      [if [and [>= n 1] [<= n 65535]]
        [Ok n]
        [Error [str "port out of range: " s]]]]]]]

# Caller uses match to handle both cases
[match [parse-port "8080"]
  [Ok port]:  [start port]
  [Error msg]:  [log-warn msg]]
```

The `Ok`/`Error` nominal variants (from [Consistent Error Handling](feature/error-patterns.md)) give callers a typed choice between success and failure without exceptions.

### Combining Results

The `and-then` combinator chains operations that each return `Ok`/`Error`:

```tinct
result:
  [let [raw: [try [fn [] [slurp %fs "config.json"]]]]
    [and-then raw [fn [text]
      [and-then [try [fn [] [from-json text]]] [fn [data]
        [Ok [get "host" data]]]]]]]
```

`Ok` is the constructor and also serves as `pure`/`return` for the Result monad — use it directly to lift a plain value into a Result chain:

```tinct
[Ok 42]              # → [Ok 42]
[do result
  [x: [read-file cap "f"]]
  [Ok [str-length x]]]  # lift plain Int into the chain
```

### Best Practices

**Let errors propagate by default.** Don't wrap everything in `try`. Propagation is zero-cost and gives the best error messages (full span and stack context).

**Use `try` at boundaries** — CLI argument parsing, file loading, external API calls, user input validation. Catch at the layer that can meaningfully recover, not inside deep library code.

**Use `raise` for invariant violations** — conditions that represent programmer errors or corrupted state. Don't use `error` as a control-flow mechanism.

**Use `Ok`/`Error` for expected failure modes** — functions where "not found" or "invalid" is a normal outcome that callers should handle. Return `Ok`/`Error` instead of raising so callers can choose their response.

**Don't `try` the uncatchable.** `DepthExceeded` and `ResourceLimitExceeded` propagate through `try` — they indicate resource exhaustion, not recoverable failures. There is no way to catch them in user code.

**Name error messages in context.** Include the operation and the bad value: `"invalid port: 0"` not `"invalid value"`. The error code tells you the class; the message tells you the specific problem.

---

## Exceptions by Default

**Errors are exceptions that propagate when a thunk is materialized.** No `Result` wrapping in normal code. Thunks record source location at creation for error reporting.

```tinct
[
    x: [/ 1 0]              # Thunk created — no error yet
    y: [+ x 1]              # Materializing x raises: "division by zero"
    z: 42                   # Fine — x never materialized through z
]
```

**Why:** Simple default path — most code lets errors propagate. Lazy eval means unmaterialized errors never happen ("pay for what you use"). `try` available when explicit handling is needed.

**Implementation note:** Thunks must record definition-site source location. When materialized, the materialization-site span is passed as a parameter to `materialize()`, not stored in the thunk. Error messages include both locations and a reconstructed call stack showing the chain of materializations. The core evaluator uses an iterative CEK machine (see [Architecture](16-architecture.md) §Iterative Evaluator) with no recursive depth limit; individual builtins may impose domain-specific depth limits (e.g., `MAX_EVAL_DEPTH` for builtin-internal recursion).

### Lazy Error Behavior

Errors in tinct are lazy — they don't occur until a value is materialized:

```tinct
[
    x: [/ 1 0]        # No error yet — x is a thunk
    y: [+ x 1]        # Materializing y materializes x → error
    z: 42             # Fine — x never materialized through z
]
```

Accessing `z` succeeds. Accessing `y` fails with the division-by-zero error from `x`, with `x`'s definition as the definition site and `y`'s access as the materialization site.

Once a thunk fails, the error is cached permanently. Subsequent accesses return the same error with additional stack frame context showing where the re-access occurred.

## `try` — Catching Errors in Stdlib

`try` takes a zero-argument function and returns a nominal variant:

```tinct
# Explicit catching via stdlib
safe: [try [fn [] [/ 10 2]]]       # → [Ok 5]
safe: [try [fn [] [/ 1 0]]]        # → [Error "/: division by zero"]
safe: [try-or [fn [] [/ 1 0]] 0]   # → 0
```

**`try` return shape:** `try` returns a nominal variant — `[Ok value]` on success or `[Error message]` on failure. Use `match` to distinguish outcomes. The `message` is the error's message string — spans and stack traces are not included in the caught value.

**What `try` catches:** Errors from evaluating the function's body. Errors from materializing the function itself (e.g., if the function argument is a broken thunk) are *not* caught — they propagate to `try`'s caller.

**Uncatchable errors:** `DepthExceeded` and `ResourceLimitExceeded` errors are not catchable by `try` — they propagate through `try` to the caller. Resource limit exhaustion is a boundary condition that must halt evaluation, not be masked by error handling. See `is_catchable()` in the Error Semantics formal specification below.

**`try-or`** is a stdlib convenience: `[try-or [fn [] expr] default]` returns `default` if `expr` fails.

## Error Semantics — Formal Specification

This section formalizes how errors are represented, propagated, decorated, memoized, and caught. It builds on the Failed state and FORCE-FAILED rule from §Thunk Lifecycle — Formal Specification and the error classes from §Call Convention — Part 4: Error Taxonomy. Error message formats and span assignments are specified in the Display Format and Error Categories sections below.

### Part 1: Error Representation

An evaluation error `ε` is a record with five fields:

```text
ε = ⟨kind, def_span, mat_span?, secondary_span?, macro_expansion?, blame?, pipeline_stage?, stack⟩  where
  kind             : ErrorKind                 — structured error variant with domain-specific data
  def_span         : Span                      — where the problematic value was defined
  mat_span         : Option<Span>              — where the value was first materialized (if different)
  secondary_span   : Option<(Span, String)>    — secondary "value origin" span with label (see below)
  macro_expansion  : Option<(String, Span)>    — macro expansion provenance (macro name, call site)
  blame            : Option<BlameLabel>        — gradual typing boundary label
  pipeline_stage   : Option<PipelineBlame>     — pipeline stage provenance
  stack            : [StackFrame]              — chain of materialization contexts, outermost last
```

**Dual-span model:** Every error carries two source locations: the **definition site** (where the error-producing expression was written) and the **materialization site** (where a consumer materialized the thunk that failed). When these coincide, `mat_span` is `None`. When a Failed thunk is re-accessed from a third location, the new access site is pushed onto `stack` as a frame — `def_span` and `mat_span` are never overwritten after initial assignment.

The **definition site** and **materialization site** form a dual-span model: "the error was *defined* here but *triggered* there." When both sites are the same (e.g., an immediate expression like `[call $/ 1 0]`), the materialization site is omitted.

Example: given `[x: [/ 1 0]  y: x]`, accessing `y` produces an error with definition site at `[/ 1 0]` (where the division was written) and materialization site at `x` (where the thunk was first materialized).

**Secondary span — value origin (Nickel dual-position pattern):** For lazy evaluation errors where the **value that caused the failure** was produced far from the error site, `secondary_span` carries a labeled pointer to the value's creation span. This is the Nickel `EvaluationError` dual-position pattern: "error triggered here, but the offending value came from there." The `Thunk.span` field (set at thunk creation time) provides this origin span without requiring any additional storage.

`secondary_span` is populated at two specific eval sites:

| Site | `def_span` (error site) | `secondary_span` (value origin) | Label |
|------|------------------------|--------------------------|-------|
| `ThunkState::Guarded` validation failure | TypeAssert annotation span (`guard_span`) | `inner.span` (the annotated expression's creation span) | `"value produced here"` |
| `if` condition type mismatch (non-Bool condition) | Condition expression span | condition thunk's `span` | `"condition evaluated to {type} here"` |

`secondary_span` is always optional and never overwrites `def_span`/`mat_span`. When the secondary span equals `def_span`, it is suppressed (no duplicate location notes). Display format: `"\n  note: {label} at {span}"`.

**Stack frames:** Each frame is `⟨label, span⟩` where `label` identifies the context (e.g., the thunk's origin name, `"materialized"` for re-access) and `span` is the source location. Frames are added by `attach_materialization_context` during propagation and by the Failed state handler during re-access.

### Part 2: Error Sources

All errors are constructed via `EvalError` methods that create an error with a specific `ErrorKind` variant. The main named constructors are:

| Constructor | ErrorKind Variant | Message Pattern | `def_span` Source |
|------------|-------------------|----------------|-------------------|
| `key_not_found(key, available_keys, span)` | `KeyNotFound { key, available_keys }` | `"key not found: {key}"` | Access expression |
| `type_mismatch(expected, got, span)` | `TypeMismatch { context: None, expected, got }` | `"type mismatch: expected {expected}, got {got}"` | Expression producing wrong type |
| `type_mismatch_ctx(context, expected, got, span)` | `TypeMismatch { context: Some(context), expected, got }` | `"{context}: expected {expected}, got {got}"` | Expression producing wrong type |
| `arity_mismatch(expected, got, span)` | `ArityMismatch { expected, got }` | `"arity mismatch: expected {expected} arguments, got {got}"` | Call expression |
| `circular_dependency(name, definition_span, cycle_path)` | `CircularDependency { name, cycle_path }` | `"circular dependency detected while evaluating {name}"` | Thunk definition |
| `depth_exceeded(limit, span)` | `DepthExceeded { limit }` | `"maximum evaluation depth exceeded ({limit})"` | Thunk being materialized when limit hit |
| `user_error(message, span)` | `UserError { message }` | `"{message}"` (user-provided) | `error` call site |
| `integer_overflow(op, span)` | `IntegerOverflow { op }` | `"{op}: integer overflow"` | Arithmetic expression |
| `division_by_zero(op, span)` | `DivisionByZero { op }` | `"{op}: division by zero"` | Division expression |
| `float_not_finite(builtin, value, span)` | `FloatNotFinite { builtin, value }` | `"{builtin}: {value} is not a finite number"` | Builtin call expression |
| `float_out_of_range(builtin, value, span)` | `FloatOutOfRange { builtin, value }` | `"{builtin}: {value} is out of range for Int"` | Builtin call expression |
| `empty_collection(op, span)` | `EmptyCollection { op }` | `"{op} on empty collection"` | Builtin call expression |
| `named_arg_rejected(builtin, span)` | `NamedArgRejected { builtin }` | `"{builtin} does not accept named arguments"` | Call expression |
| `resource_limit_exceeded(message, span)` | `ResourceLimitExceeded { message }` | `"{message}"` (caller-provided) | Resource-checking site |
| `schema_violation(violations, span)` | `SchemaViolation { violations }` | `"schema validation failed with {n} error(s):\n  {field}: {msg}\n..."` | Schema validation expression |
| `internal(message, span)` | `Internal { message }` | `"{message}"` (implementation-defined) | Context-dependent |

See `src/error.rs` for the full set of `ErrorKind` variants and their constructors. The table above shows representative error constructors; additional variants not listed include: `UndefinedVariable`, `TypeAssertFailed`, `NamedArgConflict`, `UnknownNamedArg`, `DuplicateKey`, `ValueNotSerializable`, `JsonDepthExceeded`, `IncludeForbidden`, `IncludeNotAvailable`, `IncludeIoError`, `IncludeCycle`, `IncludeParseFailed`, `IncludeFileTooLarge`, `IncludeHashMismatch`, `IncludeHashRequired`, `IncludePathNotAllowed`, `ParseConversion`, `JsonParse`, `JsonRange`, and `UriParseError`.

**Note:** The `depth_exceeded(limit, span)` constructor is marked `#[cfg(test)]` and only available in test builds.

**Special error properties:**

- **`DepthExceeded` and `ResourceLimitExceeded` are not catchable:** `try` does not catch `DepthExceeded` or `ResourceLimitExceeded` errors — they propagate to the runtime. Resource limit errors like stack overflow should not be suppressible by user code (follows GHC's `StackOverflow` and Racket's `exn:fail:resource` semantics). The `is_catchable()` method returns `false` for these variants, `true` for all others.

- **`DepthExceeded` is not cacheable:** Failed thunk state does not cache `DepthExceeded` errors — a thunk that fails at one depth may succeed at a shallower depth. The `is_cacheable()` method returns `false` for `DepthExceeded`, `true` for all other variants (including `ResourceLimitExceeded`, which is cacheable because resource limits are absolute, not context-dependent). This implements the PROP-DEPTH non-memoization rule from Part 5.

- **`IncludeForbidden` is catchable (intentional):** Unlike `DepthExceeded` and `ResourceLimitExceeded`, the `IncludeForbidden` error (E042, raised when `include` is called in `--no-fs` mode) is catchable via `try`. This follows the Nix `tryEval` model — programs can gracefully degrade when filesystem access is unavailable (e.g., falling back to embedded defaults when external config files cannot be loaded). The tradeoff is that an attacker can detect `--no-fs` mode by wrapping `include` in `try` and observing whether it returns `[err: "filesystem access is disabled (--no-fs)"]`. This is accepted because the alternative (making `IncludeForbidden` uncatchable) would prevent legitimate graceful degradation patterns. Programs that need to behave identically regardless of sandbox mode should avoid filesystem access entirely, rather than relying on `IncludeForbidden` being undetectable.

### Part 3: Error Decoration

**[DECORATE]** — `attach_materialization_context(ε, mat_span, origin, thunk_span)`:

```text
DECORATE(ε, mat_span, origin, thunk_span):
  (1) if mat_span is Some(s) ∧ ε.mat_span is None:
        ε.mat_span ← Some(s)
  (2) if mat_span is Some(s) ∧ ε.mat_span is Some(s') ∧ s ≠ s'
        ∧ s ∉ {f.span | f ∈ ε.stack}:
        ε.stack.push(⟨"materialized", s⟩)
  (3) if origin ≠ "" ∧ ∄f ∈ ε.stack. f.label = origin ∧ f.span = thunk_span:
        ε.stack.push(⟨origin, thunk_span⟩)
```

Rule (1) sets the materialization span on first decoration. Rule (2) adds subsequent materialization sites as stack frames without overwriting the original. Rule (3) adds the thunk's origin label (e.g., variable name) as a frame — the `origin` parameter corresponds to the thunk's origin name as described in §Scope Chain Semantics — Formal Specification. The deduplication guards (`s ∉ stack`, `∄f matching (label, span)`) prevent redundant frames when the same span propagates through nested `materialize` calls.

**Invariant:** `ε.mat_span`, once set to `Some(s)`, is never changed to `Some(s')` where `s ≠ s'`. The materialization span records the *first* site that materialized the thunk; subsequent sites become stack frames.

### Part 4: Error Propagation

Errors propagate upward through materialization chains via Rust's `?` operator (early return of `Result::Err`). Every `materialize` call site is a potential decoration point. Rules use the same judgment notation as [Evaluation](08-evaluation.md) §Forcing Rules: `Σ_θ` denotes the `EvalContext` captured at thunk construction time.

**[PROP-EVAL]** — Unevaluated thunk evaluation:

```text
eval(expr, env, Σ_θ, d+1) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← Unevaluated(expr, env, Σ_θ)   // restore original state
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

Note: `eval()` may internally call `materialize()` recursively (e.g., for PendingCall resolution). PROP-EVAL covers the outer eval → error path; nested materialization within eval follows PROP-RESULT or PROP-BUILTIN depending on the thunk state encountered.

**[PROP-BUILTIN]** — PendingBuiltin execution:

```text
func(args, named, Σ_θ, pd, cs) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← PendingBuiltin(func, args, named, pd, cs, Σ_θ)   // restore
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

**[PROP-RESULT]** — Recursive materialization of result thunk:

```text
func(...) ⇒ Ok(θ_result)
materialize(θ_result, mat_span, d+1) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
if ε'.kind.is_cacheable():
  thunk.state ← Failed(ε')
else:
  thunk.state ← restore(original_state)   // restore pre-InProgress state
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

**State restoration for non-cacheable errors:** `is_cacheable()` returns `false` only for `DepthExceeded`. When a non-cacheable error occurs, the thunk's original state (Unevaluated, PendingBuiltin, or PendingCall) is restored instead of transitioning to Failed. This preserves the PROP-DEPTH invariant: a thunk that fails at depth N may succeed at depth N-1, so its semantic state must remain "not yet computed." The restoration is a backward transition in the state DAG (InProgress → original state), which is sound because `DepthExceeded` is an administrative interruption, not a semantic failure (see §Thunk Lifecycle, Semantic Commitment #3).

**PendingCall coverage:** PendingCall thunks have four error paths (function materialization, invoke_function, result materialization, type mismatch). All follow the same DECORATE + conditional-cache pattern: function materialization failures and type mismatches are decorated inline; result materialization follows PROP-RESULT; invoke_function failures are decorated and conditionally cached. PendingCall restoration requires cloning `func`, `args`, and `named` before evaluation (all `Rc::clone` — no materialization) since `take_pending_call()` consumes ownership.

**Nested materialization span:** When a PendingCall handler materializes `func_thunk` and that materialization fails, the error's `materialization_span` is set to `call_span` (the site where the function call was written), not the span of the inner expression that actually failed. This follows from passing `Some(&call_span)` as the `mat_span` parameter to `materialize(&func_thunk, ...)`. The same behavior applies to PendingBuiltin when materializing arguments — the builtin's `call_span` becomes the materialization site for nested errors. This ensures that error reports consistently attribute materialization to the call site, even when the actual failure occurs in a deeply nested thunk chain.

**[PROP-CYCLE]** — Circular dependency:

```text
thunk.state = InProgress
ε = circular_dependency(name, thunk.span)
ε.mat_span ← mat_span
thunk.state ← Failed(ε)
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε)
```

Note: PROP-CYCLE constructs the error inline at the detection site — it does *not* pass through DECORATE. This is the only propagation rule that bypasses decoration, because the error originates at the materialization site itself rather than propagating from a deeper call.

**[PROP-DEPTH]** — Depth limit exceeded:

```text
d > MAX_EVAL_DEPTH
ε = depth_exceeded(MAX_EVAL_DEPTH, thunk.span)
ε.mat_span ← mat_span
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε)
```

Note: PROP-DEPTH does *not* transition to Failed — the thunk state is unchanged (§Thunk Lifecycle, Semantic Commitment #3). The same thunk may succeed at a lower depth.

**Propagation path:** In a chain `θ₁ → θ₂ → θ₃` where θ₃ fails, the error propagates θ₃ → θ₂ → θ₁, each level applying DECORATE. The result is an error with `def_span` from θ₃ (where the problem was defined), `mat_span` from the first materialization site, and stack frames from intermediate materialization points.

### Part 5: Failed State Memoization

**[MEMO-CACHE]** — On first error, cache in Failed state:

```text
materialize(thunk, ...) ⇒ Err(ε)
ε.kind.is_cacheable()
──────────────────────────
thunk.state ← Failed(ε)
```

**[MEMO-SKIP]** — Non-cacheable error, restore thunk state:

```text
materialize(thunk, ...) ⇒ Err(ε)
¬ε.kind.is_cacheable()
──────────────────────────
thunk.state ← restore(original_state)   // pre-InProgress state
```

Cacheable error paths (PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-CYCLE) cache via `cache_failure`. Non-cacheable errors (DepthExceeded) restore the thunk to its pre-InProgress state via MEMO-SKIP, allowing the same thunk to succeed at a shallower call depth. The cached error includes decoration from DECORATE — `mat_span` and stack frames from the first materialization chain are preserved.

**[MEMO-REACCESS]** — On subsequent access of a Failed thunk:

```text
thunk.state = Failed(ε_cached)
ε' = clone(ε_cached)
(1) if mat_span is Some(s) ∧ ε'.mat_span is None:
      ε'.mat_span ← Some(s)
      update cache: thunk.state ← Failed(ε')
(2) if mat_span is Some(s) ∧ ε'.mat_span is Some(s') ∧ s ≠ s'
      ∧ s ∉ {f.span | f ∈ ε'.stack}:
      ε'.stack.push(⟨"materialized", s⟩)
      update cache: thunk.state ← Failed(ε')
(3) if mat_span is None:
      (no decoration, no cache update — return ε' unchanged)
──────────────────────────
materialize(thunk, mat_span, d) ⇒ Err(ε')
```

MEMO-REACCESS mirrors DECORATE but operates on the cached error. Cache updates are progressive: each new access site enriches the cached error's stack. This is the Failed self-edge in the thunk lifecycle DAG — it refines diagnostic metadata without changing the error's semantic content (message, def_span).

**Permanence:** Once a thunk reaches Failed, it never returns to any other state (§Thunk Lifecycle, Semantic Commitment #1). No retry, no recovery. This includes I/O failures from `$include`. The only exception: non-cacheable errors (DepthExceeded) trigger MEMO-SKIP instead of MEMO-CACHE — the thunk state is restored rather than transitioning to Failed, because depth errors are context-dependent, not intrinsic to the thunk.

### Part 6: `try` Catching Boundary

**[TRY]** — Error catching:

```text
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Ok(v)
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Variant("Ok", θ(v)))
```

**[TRY-ERR]** — Error caught:

```text
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Err(ε)
ε.kind.is_catchable()
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Variant("Error", θ(ε.kind.to_string())))
```

**[TRY-UNCATCHABLE]** — Uncatchable error re-raised:

```text
materialize(θ_func, _, d) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _, d) ⇒ Err(ε)
¬ε.kind.is_catchable()
──────────────────────────
try(θ_func, d, s) ⇒ Err(ε)
```

**[TRY-BUILTIN]** — Builtin zero-arg function:

```text
materialize(θ_func, _, d) ⇒ Builtin(f)
f([], {}, d, s) ⇒ Ok(θ_result)
materialize(θ_result, _, d) ⇒ Ok(v)
──────────────────────────
try(θ_func, d, s) ⇒ ok_val(Dict({ok ↦ θ(v)}))
```

(Catchable error variant: same structure, `Err(ε), ε.kind.is_catchable() ⇒ Dict({err ↦ θ(ε.kind.to_string())})`; uncatchable errors re-raised per TRY-UNCATCHABLE)

**Catching boundary:** `try` catches errors at the zero-argument function body boundary. The function is materialized *outside* the catch — if the function thunk itself fails to materialize, that error propagates to `try`'s caller (not caught). Only errors from *calling* the function (evaluating its body) are caught.

**Error-to-value conversion:** `try` extracts only the message string (`ε.kind.to_string()`). The spans and stack frames are discarded — `try` is for program-level error handling, not diagnostic reporting. The result is a nominal variant `[Ok ...]` or `[Error ...]`, not an ordinary dict.

**Arity constraint:** The function must take zero parameters. If `params.len() > 0`, `try` raises an error (not caught): `"try: expected a zero-argument function, got {n} parameters"`.

**Interaction with Failed state:** When `try` materializes a Failed thunk *inside* the body, the cached error is returned via MEMO-REACCESS and caught by `try`. The Failed thunk's cache is updated (stack frame added) but the error is converted to `[Error message]` — it does not propagate past `try`.

**`try` interaction with structured errors:** `try` extracts `ε.kind.to_string()` — the Display output of ErrorKind. This preserves the behavior that the caught value is a human-readable error message string, not a structured error object. Error codes are not exposed through `try`.

**Rationale:** `try` is for program-level error recovery ("did it fail?"), not error introspection. Programs that need to distinguish error kinds should use type checking and validation, not `try`-and-parse. Exposing structured error data through `try` would create a coupling between error representation (an implementation detail) and user programs.

**Display stability:** Error codes are stable across releases (see Error Codes below). Display message *wording* is not part of the stability contract — message text may be refined for clarity across releases. Programs that match on `try` error strings are inherently fragile and should not rely on exact wording.

### Part 7: Properties

**E1 — Error determinism:** For a given program state (environment, thunk graph), the same error is produced regardless of evaluation order. This follows from the pure subset's confluence (§Thunk Lifecycle, Semantic Properties). `$include` breaks this — file system state introduces nondeterminism.

**E2 — Memoization permanence:** `Failed(ε)` is absorbing — no transition out of Failed exists. Formally: if `thunk.state = Failed(ε)` at time t, then `thunk.state = Failed(ε')` for all t' > t, where `ε'.kind = ε.kind ∧ ε'.def_span = ε.def_span ∧ (ε.mat_span = Some(s) → ε'.mat_span = Some(s))` — mat_span may transition from None to Some but never from Some(s) to Some(s') where s ≠ s'. Stack frames may grow monotonically.

**E3 — Propagation preserves definition site:** DECORATE never modifies `ε.def_span`. The definition site is set at error construction and propagated unchanged through any number of materialization layers.

**E4 — Materialization site is first-materialization:** `ε.mat_span` records the first site that triggered materialization. Subsequent access sites become stack frames. This is enforced by DECORATE rule (1) (set only if None) and MEMO-REACCESS rule (1).

**E5 — `try` isolation:** Errors caught by `try` do not propagate to `try`'s caller. `try` converts errors to values — the error is consumed, not rethrown. There is no `$rethrow` mechanism.

**E6 — Depth errors are non-caching:** DepthExceeded errors have `is_cacheable() = false`, triggering MEMO-SKIP instead of MEMO-CACHE. The thunk state is restored to its pre-InProgress state, allowing the same thunk to succeed at a shallower call depth. This is the only error source that does not cache.

**E7 — Stack frame monotonicity:** The `stack` field of a cached error grows monotonically — frames are appended, never removed or reordered. Each re-access of a Failed thunk from a new location adds at most one frame.

**E8 — DECORATE idempotence:** Applying DECORATE twice with the same arguments produces the same result as applying it once: `DECORATE(DECORATE(ε, s, o, t), s, o, t) = DECORATE(ε, s, o, t)`. This follows from the deduplication guards in rules (1)–(3).

**Typing:** `try` has type `Any → Any` — more precisely it expects `Fn(→ τ)` and returns `Ok(τ) | Error(Str)` (nominal variants), but neither the constraint on the argument nor the union result type can be expressed without union types — see [Type System Extensions](07-type-extensions.md) §Expressiveness. `raise` has type `Str → Never` — the argument is materialized and coerced to String; `raise` never returns a value (it always raises an error). `Never` is the bottom type.

**Runtime vs. static errors:** Runtime errors (`EvalError`, cached in `Failed` thunks) are distinct from the type inference engine's `Type::Error` marker. `Type::Error` represents the type of expressions that are statically known to produce errors (e.g., undefined variables caught during type checking); `EvalError` is the runtime value produced during evaluation.

### Part 8: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| EvalError struct | see `struct EvalError` in `src/error.rs` |
| DECORATE | see `attach_materialization_context` in `src/eval_materialize.rs` |
| PROP-EVAL | see Unevaluated arm of `materialize()` in `src/eval_materialize.rs` |
| PROP-BUILTIN | see PendingBuiltin arm of `materialize()` in `src/eval_materialize.rs` |
| PROP-RESULT | see recursive `materialize()` calls on result thunks in `src/eval_materialize.rs` |
| PROP-CYCLE | see InProgress arm of `materialize()` in `src/eval_materialize.rs` |
| PROP-DEPTH | see depth check at top of `materialize()` in `src/eval_materialize.rs` |
| MEMO-CACHE | see `Thunk::cache_failure` in `src/value.rs` |
| MEMO-SKIP | see non-cacheable state restore branches in `materialize()` in `src/eval_materialize.rs` |
| MEMO-REACCESS | see Failed arm of `materialize()` in `src/eval_materialize.rs` |
| TRY | see `builtin_try` in `src/builtins.rs` |
| TRY-UNCATCHABLE | see `!e.kind.is_catchable()` re-raise branch in `builtin_try` in `src/builtins.rs` |
| TRY catching boundary | see body materialize call inside `builtin_try` in `src/builtins.rs` |
| Error-to-value | see `e.kind.to_string()` extraction in `builtin_try` in `src/builtins.rs` |
| $raise | see `builtin_raise` in `src/builtins_meta.rs` |

## Structured Error Model

This section specifies the structured representation that replaces the freeform `message: String` field in `EvalError`. The error semantics (propagation, decoration, memoization, catching) remain unchanged — this section restructures error **identity and data** only.

### Motivation

The `EvalError` struct uses a structured `ErrorKind` enum with domain-specific variants (see Part 1: Variant Catalog above) instead of a freeform `message: String` field. This structured approach provides:

1. **Programmatic error identity** — tests and tooling can branch on error kind via pattern matching.
2. **Structured data extraction** — domain-specific fields (e.g., `key` in `KeyNotFound`, `available_keys` for suggestions) are directly accessible.
3. **Error codes** — stable identifiers (E001–E099) enable `tinct explain` and documentation linking.
4. **Multi-format rendering** — error data is separated from presentation, supporting JSON output, LSP diagnostics, and format-independent rendering.

The structured error model is fully implemented. The builtins registered via `core_builtins()` / `builtin_module("core")` and the corpus error tests comprehensively exercise the error variants.

### Design: `ErrorKind` Enum

Replace the `message: String` field in `EvalError` with `kind: ErrorKind`. Each variant carries structured domain data. Human-readable messages are derived via `Display` on `ErrorKind`, not stored.

```rust
pub struct EvalError {
    pub kind: ErrorKind,
    pub definition_span: Span,
    pub materialization_span: Option<Span>,
    pub stack: Vec<StackFrame>,
}
```

### Part 1: Variant Catalog

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorKind {
    // --- Access errors (E000-E009) ---
    KeyNotFound { key: String, available_keys: Vec<String> },
    /// `name` stores the identifier (e.g., `"x"`).
    /// Display format: `"undefined variable: x"`.
    UndefinedVariable { name: String },

    // --- Type errors (E010-E019) ---
    /// Runtime type mismatch from evaluator or builtin dispatch.
    /// `context` carries the builtin name (e.g., `"merge"`) when the mismatch
    /// originates from a builtin; `None` for generic evaluator mismatches.
    /// `expected` is human-readable, not machine-parseable — may contain
    /// compound descriptions like `"Dict or Seq"`.
    TypeMismatch { context: Option<String>, expected: String, got: String },
    /// User-written type assertion (`[@Type value]`) failed at runtime.
    /// Semantically distinct from `TypeMismatch` — this is a user-authored
    /// type guard, not an internal evaluator check.
    TypeAssertFailed { expected: String, got: String },

    // --- Call errors (E020-E029) ---
    ArityMismatch { expected: ArityBound, got: usize },
    MissingRequiredParam { param: String },
    NamedArgConflict { param: String },
    UnknownNamedArg { name: String, valid_params: Vec<String> },
    NamedArgRejected { builtin: String },

    // --- Value errors (E030-E039) ---
    DuplicateKey { key: String },
    /// `op` carries the operator symbol (e.g., `"/"`) for Display prefix.
    DivisionByZero { op: String },
    IntegerOverflow { op: String },
    /// Covers NaN, Infinity, and -Infinity — values that are not finite
    /// and cannot be converted to Int or used in contexts requiring finite floats.
    FloatNotFinite { builtin: String, value: f64 },
    EmptyCollection { op: String },
    /// Runtime value type cannot be serialized to JSON (Function, Builtin, Seq, Proxy).
    /// `value_type` is the user-facing type name (e.g., "Function", "Proxy").
    ValueNotSerializable { value_type: String },
    /// Float-to-int conversion failed: value is finite but outside i64 range.
    /// Distinct from `FloatNotFinite` (which rejects NaN/Infinity).
    FloatOutOfRange { builtin: String, value: f64 },

    // --- Limit errors (E040-E049) ---
    /// Evaluation depth limit (recursive thunk materialization).
    DepthExceeded { limit: usize },
    /// JSON nesting depth limit (distinct from eval depth — applies during
    /// `$from-json` parsing of deeply nested JSON structures).
    JsonDepthExceeded { limit: usize },
    /// Filesystem access forbidden in `--no-fs` mode.
    IncludeForbidden,
    /// Resource limit exceeded (collection size, string size, etc.).
    /// Like `DepthExceeded`, this is non-catchable — resource limits are
    /// safety boundaries, not application-level errors.
    ResourceLimitExceeded { message: String },

    // --- Include errors (E050-E059) ---
    IncludeNotAvailable,
    /// Covers both "cannot open" (canonicalize failure) and "cannot read"
    /// (metadata/read failure). The `detail` field carries the OS error.
    IncludeIoError { path: String, detail: String },
    IncludeCycle { path: String },
    IncludeParseFailed { path: String, detail: String },
    IncludeFileTooLarge { path: String, size: u64, limit: u64 },

    // --- Conversion errors (E060-E069) ---
    ParseConversion { builtin: String, input: String, target: String },
    JsonParse { detail: String },
    JsonRange,
    UriParseError { detail: String },

    // --- Evaluation structure (E070-E079) ---
    CircularDependency { name: String, cycle_path: Vec<(String, Span)> },

    // --- User-generated (E080-E089) ---
    UserError { message: String },

    // --- Schema validation (E090-E094) ---
    SchemaViolation { violations: Vec<(String, String)> },
    /// Type kind mismatch — a type constructor's kind does not match the expected kind.
    KindMismatch { expected: String, got: String },

    // --- Escape hatch (E095-E099) ---
    Internal { message: String },
}
```

The `ArityBound` type expresses flexible arity constraints:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum ArityBound {
    Exact(usize),
    Range(usize, usize),
}
```

**Variant design principles:**

- **One variant per user-distinguishable error class.** If two errors should produce different suggestions, they get different variants. If they differ only in wording, they share a variant with a field.
- **`TypeMismatch` vs `TypeAssertFailed`:** `TypeMismatch` is an evaluator/builtin dispatch error ("merge got the wrong type"). `TypeAssertFailed` is a user-written runtime type guard failure (`[@Int "hello"]`). Different error class, different suggestions, different error code. `ThunkState::Guarded` validation (§TypeAssert Runtime Validation) produces `TypeAssertFailed` for guard failures.
- **`context: Option<String>` in `TypeMismatch`** carries the builtin name when the mismatch originates from a builtin (e.g., `"merge"` in `"merge: expected Dict, got Int"`). `None` for generic type mismatches from the evaluator. The `expected` field is human-readable, not machine-parseable — it may contain compound descriptions like `"Dict or Seq"`. Programmatic matching on expected types is not supported; use the error code and `context` field instead.
- **`DivisionByZero` carries `op`** to preserve the operator prefix in Display output (e.g., `"/: division by zero"`). This maintains `try` message compatibility and future-proofs for additional division operators.
- **`FloatNotFinite`** covers NaN, Infinity, and -Infinity — all non-finite `f64` values. Named `NotFinite` rather than `OutOfRange` because NaN is not a range concept.
- **`FloatOutOfRange`** (E036) covers finite values outside the `i64` range (e.g., `1e19`). Named `OutOfRange` rather than `NotFinite` because the value is mathematically finite — it simply exceeds the integer domain of `$floor`/`$round`. Distinct from `FloatNotFinite` because the conditions and user-facing diagnostics differ: `FloatNotFinite` is "not a finite number", `FloatOutOfRange` is "out of range for Int".
- **`DepthExceeded` vs `JsonDepthExceeded`:** Eval depth (recursive thunk materialization) and JSON nesting depth (`$from-json` parsing) are semantically different limits with different error codes. A JSON depth error at E041 does not indicate runaway evaluation.
- **`IncludeIoError`** covers both "cannot open" (canonicalize failure) and "cannot read" (metadata/read failure) — both are filesystem IO failures distinguished by the `detail` field.
- **`Internal` is an escape hatch**, not a permanent category. It accepts a freeform message string for incremental migration. New error sites should use a typed variant; `Internal` should trend toward zero usage over time.
- **Terminology:** "Type error" in this section always means a *runtime* type mismatch (`ErrorKind::TypeMismatch`). Static type checking failures are `TypeError` in `src/types.rs` — a separate type, separate system, separate error reporting path.

### Part 2: Error Codes

Each variant maps to a stable error code. Codes are `E` followed by a three-digit number, grouped by domain.

**Stability principle:** Error codes are part of tinct's public interface. A code, once assigned, always means the same error class across all releases. Codes are never reassigned to different error classes. This enables `tinct explain E001`, programmatic error filtering, and documentation linking.

| Code | Variant | Category |
|------|---------|----------|
| E001 | `KeyNotFound` | Access |
| E002 | `UndefinedVariable` | Access |
| E010 | `TypeMismatch` | Type |
| E011 | `TypeAssertFailed` | Type |
| E012 | `MacroError` | Type |
| E013 | `NoInstance` | Type |
| E020 | `ArityMismatch` | Call |
| E021 | `NamedArgConflict` | Call |
| E022 | `UnknownNamedArg` | Call |
| E023 | `NamedArgRejected` | Call |
| E024 | `MissingRequiredParam` | Call |
| E030 | `DuplicateKey` | Value |
| E031 | `DivisionByZero` | Value |
| E032 | `IntegerOverflow` | Value |
| E033 | `FloatNotFinite` | Value |
| E034 | `EmptyCollection` | Value |
| E035 | `ValueNotSerializable` | Value |
| E036 | `FloatOutOfRange` | Value |
| E040 | `DepthExceeded` | Limit |
| E041 | `JsonDepthExceeded` | Limit |
| E042 | `IncludeForbidden` | Limit |
| E043 | `ResourceLimitExceeded` | Limit |
| E044 | `CapabilityRequired` | Limit |
| E050 | `IncludeNotAvailable` | Include |
| E051 | `IncludeIoError` | Include |
| E052 | `IncludeCycle` | Include |
| E053 | `IncludeParseFailed` | Include |
| E054 | `IncludeFileTooLarge` | Include |
| E055 | `IncludeHashMismatch` | Include |
| E056 | `IncludeHashRequired` | Include |
| E057 | `IncludePathNotAllowed` | Include |
| E060 | `ParseConversion` | Conversion |
| E061 | `JsonParse` | Conversion |
| E062 | `JsonRange` | Conversion |
| E063 | `UriParseError` | Conversion |
| E070 | `CircularDependency` | Evaluation |
| E071 | `MatchExhaustion` | Evaluation |
| E072 | `DuplicateVariable` | Evaluation |
| E080 | `UserError` | User |
| E081 | `Unimplemented` | User |
| E082 | `BuilderFinished` | User |
| E090 | `SchemaViolation` | Schema validation |
| E091 | `KindMismatch` | Schema validation |
| E099 | `Internal` | Internal |

**Numbering scheme:** Codes are grouped in decades by domain with explicit ranges:

| Range | Domain |
|-------|--------|
| E000–E009 | Access |
| E010–E019 | Type |
| E020–E029 | Call |
| E030–E039 | Value |
| E040–E049 | Limit |
| E050–E059 | Include |
| E060–E069 | Conversion |
| E070–E079 | Evaluation |
| E080–E089 | User |
| E090–E094 | Schema validation |
| E095–E099 | Internal |

Gaps between codes within each range allow inserting new variants without renumbering existing codes.

Error codes are derived from the variant via a method:

```rust
impl ErrorKind {
    pub fn code(&self) -> &'static str {
        match self {
            Self::KeyNotFound { .. } => "E001",
            Self::UndefinedVariable { .. } => "E002",
            Self::TypeMismatch { .. } => "E010",
            Self::TypeAssertFailed { .. } => "E011",
            Self::ArityMismatch { .. } => "E020",
            Self::MissingRequiredParam { .. } => "E024",
            Self::NamedArgConflict { .. } => "E021",
            Self::UnknownNamedArg { .. } => "E022",
            Self::NamedArgRejected { .. } => "E023",
            Self::DuplicateKey { .. } => "E030",
            Self::DivisionByZero { .. } => "E031",
            Self::IntegerOverflow { .. } => "E032",
            Self::FloatNotFinite { .. } => "E033",
            Self::EmptyCollection { .. } => "E034",
            Self::ValueNotSerializable { .. } => "E035",
            Self::FloatOutOfRange { .. } => "E036",
            Self::DepthExceeded { .. } => "E040",
            Self::JsonDepthExceeded { .. } => "E041",
            Self::IncludeForbidden => "E042",
            Self::ResourceLimitExceeded { .. } => "E043",
            Self::IncludeNotAvailable => "E050",
            Self::IncludeIoError { .. } => "E051",
            Self::IncludeCycle { .. } => "E052",
            Self::IncludeParseFailed { .. } => "E053",
            Self::IncludeFileTooLarge { .. } => "E054",
            Self::ParseConversion { .. } => "E060",
            Self::JsonParse { .. } => "E061",
            Self::JsonRange => "E062",
            Self::UriParseError { .. } => "E063",
            Self::CircularDependency { .. } => "E070",
            Self::UserError { .. } => "E080",
            Self::SchemaViolation { .. } => "E090",
            Self::KindMismatch { .. } => "E091",
            Self::Internal { .. } => "E099",
        }
    }

    /// Returns `false` for errors that must not be cached in Failed thunk state.
    /// Only `DepthExceeded` — a thunk that fails at one depth may
    /// succeed at a shallower depth (PROP-DEPTH in §Error Semantics).
    pub fn is_cacheable(&self) -> bool {
        !matches!(self, Self::DepthExceeded { .. })
    }

    /// Returns `false` for errors that `try` must not catch.
    /// Resource limit errors (`DepthExceeded`, `ResourceLimitExceeded`) should
    /// propagate to the runtime, not be suppressible by user code.
    /// Follows GHC's StackOverflow and Racket's exn:fail:resource semantics.
    pub fn is_catchable(&self) -> bool {
        !matches!(
            self,
            Self::DepthExceeded { .. } | Self::ResourceLimitExceeded { .. }
        )
    }
}
```

### Part 3: Message Generation

`Display` on `ErrorKind` generates human-readable messages. Messages follow rustc style guidelines:

1. **No trailing punctuation.** `"key not found: foo"` not `"key not found: foo."`
2. **Lowercase start.** `"expected Dict, got Int"` not `"Expected Dict, got Int"`
3. **No questions.** `"type mismatch: expected Int, got String"` not `"did you expect Int?"`
4. **May contain names.** `"undefined variable: $x"` — include the identifier
5. **No internal jargon.** Never reference "thunk", "materialization", "PendingCall", or "Unevaluated" in user-facing messages

```rust
impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KeyNotFound { key, available_keys } => {
                write!(f, "key not found: {key}")?;
                // If available_keys is non-empty, show a suggestion or the key list.
                // Suggestion uses Jaro-Winkler similarity > 0.8 (same threshold as cargo/rustc).
                // Falls back to listing up to 5 available keys when no close match is found.
                if !available_keys.is_empty() { /* see implementation */ }
                Ok(())
            }
            Self::UndefinedVariable { name } =>
                write!(f, "undefined variable: {name}"),
            Self::TypeMismatch { context: Some(ctx), expected, got } =>
                write!(f, "{ctx}: expected {expected}, got {got}"),
            Self::TypeMismatch { context: None, expected, got } =>
                write!(f, "type mismatch: expected {expected}, got {got}"),
            Self::TypeAssertFailed { expected, got } =>
                write!(f, "type assertion failed: expected {expected}, got {got}"),
            Self::ArityMismatch { expected, got } =>
                write!(f, "arity mismatch: expected {expected}, got {got}"),
            Self::MissingRequiredParam { param } =>
                write!(f, "missing argument for required parameter '{param}'"),
            Self::NamedArgConflict { param } =>
                write!(f, "parameter '{param}' received both positional and named argument"),
            Self::UnknownNamedArg { name, valid_params } =>
                if valid_params.is_empty() {
                    write!(f, "unexpected named argument: {name} (function has no parameters)")
                } else {
                    write!(f, "unexpected named argument: {name} (valid parameter names: {})",
                           valid_params.join(", "))
                },
            Self::NamedArgRejected { builtin } =>
                write!(f, "{builtin} does not accept named arguments"),
            Self::DuplicateKey { key } =>
                write!(f, "duplicate key: {key}"),
            Self::DivisionByZero { op } =>
                write!(f, "{op}: division by zero"),
            Self::IntegerOverflow { op } =>
                write!(f, "{op}: integer overflow"),
            Self::FloatNotFinite { builtin, value } =>
                write!(f, "{builtin}: {value} is not a finite number"),
            Self::EmptyCollection { op } =>
                write!(f, "{op} on empty collection"),
            Self::ValueNotSerializable { value_type } =>
                write!(f, "cannot serialize {value_type} to JSON"),
            Self::FloatOutOfRange { builtin, value } =>
                write!(f, "{builtin}: {value} is out of range for Int"),
            Self::DepthExceeded { limit } =>
                write!(f, "maximum evaluation depth exceeded ({limit})"),
            Self::JsonDepthExceeded { limit } =>
                write!(f, "maximum JSON nesting depth exceeded ({limit})"),
            Self::IncludeNotAvailable =>
                write!(f, "include: not available in this context"),
            Self::IncludeIoError { path, detail } =>
                write!(f, "include: cannot access \"{path}\": {detail}"),
            Self::IncludeCycle { path } =>
                write!(f, "circular include detected: \"{path}\""),
            Self::IncludeParseFailed { path, detail } =>
                write!(f, "include: parse error in \"{path}\": {detail}"),
            Self::IncludeFileTooLarge { path, size, limit } =>
                write!(f, "include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"),
            Self::ParseConversion { builtin, input, target } =>
                write!(f, "{builtin}: cannot parse {input:?} as {target}"),
            Self::JsonParse { detail } =>
                write!(f, "from-json: invalid JSON: {detail}"),
            Self::JsonRange =>
                write!(f, "JSON number outside representable range"),
            Self::UriParseError { detail } =>
                write!(f, "URI parse error: {detail}"),
            Self::CircularDependency { name, cycle_path } => {
                write!(f, "circular dependency detected while evaluating {name}")?;
                if !cycle_path.is_empty() {
                    write!(f, "\n  cycle:")?;
                    for (label, span) in cycle_path {
                        write!(f, " {} ({})", label, span)?;
                        write!(f, " →")?;
                    }
                    write!(f, " [back to {}]", name)?;
                }
                Ok(())
            }
            Self::UserError { message } =>
                write!(f, "{message}"),
            Self::SchemaViolation { violations } => {
                writeln!(f, "schema validation failed with {} error(s):", violations.len())?;
                for (field, msg) in violations {
                    writeln!(f, "  {}: {}", field, msg)?;
                }
                Ok(())
            }
            Self::Internal { message } =>
                write!(f, "{message}"),
        }
    }
}
```

`ArityBound` displays as natural language:

```rust
impl fmt::Display for ArityBound {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(1) => write!(f, "1 argument"),
            Self::Exact(n) => write!(f, "{n} arguments"),
            Self::Range(lo, hi) => {
                if *lo == *hi {
                    // Range(n, n) is effectively Exact(n), so display as such
                    if *lo == 1 {
                        write!(f, "1 argument")
                    } else {
                        write!(f, "{lo} arguments")
                    }
                } else {
                    write!(f, "{lo} to {hi} arguments")
                }
            }
        }
    }
}
```

### Part 4: EvalError Display

`EvalError::Display` includes the error code:

```rust
impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {} (defined at {})", self.kind.code(), self.kind, self.definition_span)?;
        // Omit materialization span when it equals definition_span (reduces noise for immediate expressions)
        if let Some(ref mat_span) = self.materialization_span {
            if mat_span != &self.definition_span {
                write!(f, " (materialized at {mat_span})")?;
            }
        }
        for frame in &self.stack {
            write!(f, "\n  in {} at {}", frame.label, frame.span)?;
        }
        Ok(())
    }
}
```

Example output: `[E001] key not found: name (defined at 3:5-3:10) (materialized at 7:1-7:8)`

The display format is:

```text
[E0XX] {message} (defined at {line}:{col}-{line}:{col}) ({verb} {line}:{col}-{line}:{col})
  in {label} at {line}:{col}-{line}:{col}
  in {label} at {line}:{col}-{line}:{col}
```

The materialization clause uses a context-sensitive verb determined by `infer_materialization_verb()`:

- `"called at"` when the first visible stack frame starts with `'['` (function call)
- `"accessed at"` when the stack contains frames with "access", `.`, or "bracket" (field access)
- `"materialized at"` as the fallback

The materialization clause is omitted when it equals the definition site or is absent. Stack frames are printed in the order they were added during propagation — innermost (closest to the error source) first, outermost (closest to the output root) last.

### Part 5: Constructor Migration

Named constructors become typed:

```rust
impl EvalError {
    pub fn key_not_found(key: &str, available_keys: Vec<String>, span: Span) -> Self {
        Self { kind: ErrorKind::KeyNotFound { key: key.to_string(), available_keys }, definition_span: span, materialization_span: None, stack: Vec::new() }
    }
    pub fn type_mismatch(expected: &str, got: &str, span: Span) -> Self {
        Self { kind: ErrorKind::TypeMismatch { context: None, expected: expected.to_string(), got: got.to_string() }, definition_span: span, materialization_span: None, stack: Vec::new() }
    }
    // ... etc for each variant with a convenience constructor
}
```

`EvalError::internal(message, span)` constructs `ErrorKind::Internal` as an escape hatch for cases where no typed `ErrorKind` variant applies. Prefer typed constructors; `internal()` usage is greppable and intentionally verbose.

### Part 6: `try` Interaction

See the `try` Catching Boundary section above (Error Semantics Part 6) for the formal specification of `try` semantics.

### Part 7: Rendering Separation

The `ErrorKind` enum separates error **data** from error **presentation**. The `Display` impl is the default text renderer. Renderers pattern-match on `ErrorKind` directly:

- **JSON output** (`--error-format json`): serialize `ErrorKind` variant name, structured fields, code, spans
- **LSP diagnostics**: map `ErrorKind` to `DiagnosticSeverity`, use structured fields for `relatedInformation`
- **Rich terminal** (`codespan-reporting`): use spans for source snippets with carets
- **`tinct explain E001`**: look up extended help text by error code

These renderers are not specified here. The error construction sites are designed to support them without modification.

### Part 8: Stack Frame Display Policy

The `EvalError::Display` implementation filters certain stack frames from user-facing output to reduce noise from stdlib internals. Only frames whose function name does not end with `-impl`, `-step`, `-check`, or `-merge` are shown.

Note: frame labels in Phase 2 format are `"[name arg ...]"`. The implementation extracts `name` via `frame_name()` before checking suffixes, so `"[sort-merge ...]"` is correctly matched by the `-merge` suffix even though the raw label does not end with `-merge`.

```rust
// In EvalError::Display, before printing stack frames:
for frame in self.stack.iter().filter(|f| {
    let name = frame_name(&f.label); // extracts "name" from "[name ...]"
    !name.ends_with("-impl") &&
    !name.ends_with("-step") &&
    !name.ends_with("-check") &&
    !name.ends_with("-merge")
}) {
    write!(f, "\n  in {} at {}", frame.label, frame.span)?;
}
```

**Convention:** Stdlib prelude functions that are implementation helpers (not user-callable entry points) should be named with one of these suffixes so they are automatically hidden from error output. For example, `$map-impl`, `$remove-step`, `$cond-check`, `$sort-merge` would all be filtered. This mirrors Nickel's `group_by_calls()` pattern which filters stdlib internal frames.

**Important:** This filter operates only on `Display` output. The `self.stack` field is not modified — all frames are preserved for programmatic access (LSP diagnostics, future tooling).

**Naming guidance for prelude authors:** If you write a helper function that should not appear in user-facing stack traces, suffix its name with:

- `-impl` (implementation detail)
- `-step` (iteration step in a recursive helper)
- `-check` (internal validation)
- `-merge` (merge helper)

Entry-point functions (those the user calls directly) should not use these suffixes.

### Part 9: Style Guidelines

Error messages (generated by `ErrorKind::Display`) follow these rules, adapted from rustc's diagnostic guidelines:

| Rule | Example | Counterexample |
|------|---------|----------------|
| Lowercase start | `"expected Dict, got Int"` | `"Expected Dict, got Int"` |
| No trailing punctuation | `"key not found: x"` | `"key not found: x."` |
| No questions | `"/: division by zero"` | `"did you divide by zero?"` |
| Include the value/name | `"undefined variable: x"` | `"undefined variable"` |
| No internal jargon | `"circular dependency"` | `"thunk in InProgress state"` |
| Builtin prefix when relevant | `"merge: expected Dict"` | `"type mismatch in merge"` |
| Use "expected X, got Y" | `"expected Int, got String"` | `"Int required but String given"` |
| Error codes are stable | E001 always means `KeyNotFound` | Reassigning E001 to a different error class |

### Part 10: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| `ErrorKind` enum | `error.rs` (new) |
| `ArityBound` enum | `error.rs` (new) |
| Error codes | `ErrorKind::code()` method (`error.rs`) |
| Cacheability | `ErrorKind::is_cacheable()` method (`error.rs`), integrated in `materialize()` in `src/eval_materialize.rs` |
| Catchability | `ErrorKind::is_catchable()` method (`error.rs`), integrated in `builtin_try` |
| Message generation | `ErrorKind::Display` impl (`error.rs`) |
| `EvalError` struct | `struct EvalError` in `src/error.rs` (modified: `message` → `kind`) |
| Constructor migration | `error.rs` (existing constructors updated) |
| `try` extraction | `builtins.rs` builtin_try (`e.kind.to_string()`) |
| Freeform escape hatch | `EvalError::internal()` → `ErrorKind::Internal` |
| PROP-DEPTH non-caching | see depth check at top of `materialize()` in `src/eval_materialize.rs`; `ErrorKind::DepthExceeded` constructed there, `is_cacheable()` returns `false` triggering MEMO-SKIP |

### Part 10: Source Snippet Rendering

**Status:** Implemented. See `render_span_snippet()` in `src/error.rs`.

Source snippets (source lines with caret annotations, like rustc) are rendered by the **caller** at the display boundary, not by `EvalError::Display`. Source text is not stored in `EvalError` — each display site provides the source string it already holds.

**`render_span_snippet(source: &str, span: Span) -> Option<String>`** — public helper in `src/error.rs`:

- Returns `None` for `Span::origin()` (synthetic spans have no source line)
- Returns `None` if `span.start.line` exceeds the source line count (defensive)
- Single-line spans: extracts the line, places `^` characters from `start.column` to `end.column` (clamped to line length)
- Multi-line spans: first line gets `^` from `start.column` to end of line; middle lines get full `^`; last line gets `^` from column 0 to `end.column`

**REPL integration (`src/repl.rs`):** `eval_input` receives `input: &str` and calls `.map_err(|e| ...)` on each eval step. Change each `map_err(|e| format!("{e}"))` to also call `render_span_snippet(input, e.definition_span)` and append the snippet to the error string. `StepResult = Result<String, String>` is unchanged.

**CLI integration (`src/main.rs`):** Error display after `eval_file` receives the source string from the file read. Append snippet at display site.

**LSP:** Uses `DiagnosticRelatedInformation` — not a text snippet. Separate phase.

## Builtin-Specific Errors

Builtin error messages are prefixed with the builtin name when the error originates from the builtin itself (not from argument materialization). The table below shows representative examples; not all builtins are listed.

| Builtin | Message pattern | Definition site |
|---------|----------------|-----------------|
| `merge` | `"merge: expected Dict, got {type}"` | Call expression |
| `map`, `filter`, etc. | `"{name}: expected Dict or Seq, got {type}"` | Call expression |
| `try` | `"try: expected a zero-argument function, got {n} parameters"` | Call expression |
| `+`, `-`, `*` | `"integer overflow"` | Call expression |
| `<` | `"type mismatch: expected comparable types"` | Call expression |

## Error Categories — Complete Reference

All 36 `ErrorKind` variants map to stable error codes and human-readable messages. Each error code requires at least one corpus test in `tests/corpus/eval/errors/` demonstrating the error is raised correctly with the expected error code prefix.

| ErrorKind Variant | Error Code | Message Pattern | Definition Site |
|-------------------|------------|----------------|-----------------|
| **KeyNotFound** | E001 | `"key not found: {key}"` (base); appends `" (did you mean: '{suggestion}')"` when a close match exists (Jaro-Winkler > 0.8), or `" (available keys: {k1}, {k2}, ...)"` (up to 5) when no close match exists and `available_keys` is non-empty | Access expression (`dict.key`, `dict[key]`) |
| **UndefinedVariable** | E002 | `"undefined variable: {name}"` | Variable reference expression |
| **TypeMismatch** | E010 | `"type mismatch: expected {expected}, got {got}"` or `"{context}: expected {expected}, got {got}"` | Expression that produced the wrong type |
| **TypeAssertFailed** | E011 | `"type assertion failed: expected {expected}, got {got}"` | Type assertion expression (`[@Type expr]`) |
| **ArityMismatch** | E020 | `"arity mismatch: expected {bound}, got {n}"` | Call expression |
| **NamedArgConflict** | E021 | `"parameter '{param}' received both positional and named argument"` | Call expression |
| **UnknownNamedArg** | E022 | `"unexpected named argument: {name} (valid parameter names: {params})"` or `"unexpected named argument: {name} (function has no parameters)"` | Call expression |
| **NamedArgRejected** | E023 | `"{builtin} does not accept named arguments"` | Call expression |
| **MissingRequiredParam** | E024 | `"missing argument for required parameter '{param}'"` | Call expression |
| **DuplicateKey** | E030 | `"duplicate key: {key}"` | Second occurrence of the key |
| **DivisionByZero** | E031 | `"{op}: division by zero"` | Division call expression |
| **IntegerOverflow** | E032 | `"{op}: integer overflow"` | Arithmetic call expression |
| **FloatNotFinite** | E033 | `"{builtin}: {value} is not a finite number"` | Builtin call expression |
| **EmptyCollection** | E034 | `"{op} on empty collection"` | Builtin call expression |
| **ValueNotSerializable** | E035 | `"cannot serialize {value_type} to JSON"` | Value being serialized |
| **FloatOutOfRange** | E036 | `"{builtin}: {value} is out of range for Int"` | Builtin call expression |
| **DepthExceeded** | E040 | `"maximum evaluation depth exceeded ({limit})"` | Thunk being materialized when limit hit |
| **JsonDepthExceeded** | E041 | `"maximum JSON nesting depth exceeded ({limit})"` | `from-json` call expression |
| **IncludeForbidden** | E042 | `"filesystem access is disabled (--no-fs)"` | `include` call expression |
| **ResourceLimitExceeded** | E043 | `"{message}"` (implementation-defined) | Context-dependent |
| **IncludeNotAvailable** | E050 | `"include: not available in this context"` | `include` call expression |
| **IncludeIoError** | E051 | `"include: cannot access \"{path}\": {detail}"` | `include` call expression |
| **IncludeCycle** | E052 | `"circular include detected: \"{path}\""` | `include` call expression |
| **IncludeParseFailed** | E053 | `"include: parse error in \"{path}\": {detail}"` | `include` call expression |
| **IncludeFileTooLarge** | E054 | `"include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"` | `include` call expression |
| **IncludeHashMismatch** | E055 | `"include: integrity check failed for \"{path}\": expected {expected}, got {actual}"` | `include` call expression |
| **IncludeHashRequired** | E056 | `"include: integrity hash required for \"{path}\" (--require-integrity)"` | `include` call expression |
| **IncludePathNotAllowed** | E057 | (deprecated, no longer emitted after --allow-path removal) | `include` call expression |
| **ParseConversion** | E060 | `"{builtin}: cannot parse {input:?} as {target}"` | Builtin call expression |
| **JsonParse** | E061 | `"from-json: invalid JSON: {detail}"` | `from-json` call expression |
| **JsonRange** | E062 | `"JSON number outside representable range"` | `from-json` call expression |
| **UriParseError** | E063 | `"URI parse error: {detail}"` | `uri`/`url`/`urn` call expression |
| **CircularDependency** | E070 | `"circular dependency detected while evaluating {name}"` (appends cycle path visualization when `cycle_path` is non-empty) | Thunk definition (dict entry) |
| **UserError** | E080 | `"{message}"` (user-provided) | `raise` call expression |
| **SchemaViolation** | E090 | `"schema validation failed with N error(s):\n  {field}: {msg}\n  ..."` (one violation per line, N = violation count) | Schema validation expression |
| **Internal** | E099 | `"{message}"` (implementation-defined) | Context-dependent |

The variants above are exhaustive — every runtime error maps to one of these `ErrorKind` variants. The call convention errors (E020-E024) correspond to constraint violations C-COVERAGE, C-NO-OVERLAP, and C-NAMED-VALID from doc/04-functions.md §Call Convention. E024 (MissingRequiredParam) is the per-parameter coverage check from the Kotlin model — it fires when a required parameter is not covered by either a positional or named argument. Error codes are stable across releases; message wording may vary.
