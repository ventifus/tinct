# Error Handling

## User Guide

### Letting Errors Propagate

The default: do nothing. Errors in tinct propagate automatically through the thunk graph. If a value fails to materialize, anything that depends on it also fails. Unused values never error.

```tinct
[
  data:   [read-all [open %fs "config.json" Readable]]  # fails if file missing
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
  [let [raw: [try [fn [] [read-all [open %fs "config.json" Readable]]]]]
    [and-then [fn [text]
      [and-then [fn [data]
        [Ok [get "host" data]]]
        [try [fn [] [from-json text]]]]]
      raw]]
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

**Don't `try` the uncatchable.** `ResourceLimitExceeded` propagates through `try` — it indicates resource exhaustion, not a recoverable failure. There is no way to catch it in user code.

**Name error messages in context.** Include the operation and the bad value: `"invalid port: 0"` not `"invalid value"`. The kind name tells you the class; the message tells you the specific problem.

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

**Implementation note:** Thunks must record definition-site source location. When materialized, the materialization-site span is passed as a parameter to `materialize()`, not stored in the thunk. Error messages include both locations and a reconstructed call stack showing the chain of materializations. The core evaluator uses an iterative CEK machine (see [Architecture](16-architecture.md) §Iterative Evaluator) with no recursive depth limit.

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

**Uncatchable errors:** `ResourceLimitExceeded` errors are not catchable by `try` — they propagate through `try` to the caller. Resource limit exhaustion is a boundary condition that must halt evaluation, not be masked by error handling. See `is_catchable()` in the Error Semantics formal specification below.

**`try-or`** is a stdlib convenience: `[try-or [fn [] expr] default]` returns `default` if `expr` fails.

## Error Semantics — Formal Specification

This section formalizes how errors are represented, propagated, decorated, memoized, and caught. It builds on the Failed state and FORCE-FAILED rule from §Thunk Lifecycle — Formal Specification and the error classes from §Call Convention — Part 4: Error Taxonomy. Error message formats and span assignments are specified in the Display Format and Error Categories sections below.

### Part 1: Error Representation

An evaluation error `ε` is a record:

```text
ε = ⟨kind, spans, macro_expansion?, blame?, pipeline_stage?, stack⟩  where
  kind             : ErrorKind                 — structured error variant with domain-specific data
  spans            : [(Span, String)]          — labeled spans in priority order (see below)
  macro_expansion  : Option<(String, Span)>    — macro expansion provenance (macro name, call site)
  blame            : Option<BlameLabel>        — gradual typing boundary label
  pipeline_stage   : Option<PipelineBlame>     — pipeline stage provenance
  stack            : [StackFrame]              — chain of materialization contexts, outermost last
```

**Span model:** `spans` is an ordered list of labeled source locations. `spans[0]` is the **primary span** (the main error location, typically the definition site). Its label is determined by the error kind: `"defined at"` for most errors, `"value from"` for `TypeAssertFailed`. `spans[1..]` are **note spans** — materialization sites (labeled `"evaluated here"`) and secondary value-origin spans (labeled with context like `"value produced here"`). Each note span is displayed as `"  note: {label} at {span}"`.

**Dual-span model:** Errors typically carry two source locations: the **definition site** (`spans[0]`) where the error-producing expression was written, and the **materialization site** (`spans[1]`, labeled `"evaluated here"`) where a consumer materialized the thunk that failed. When these coincide, the materialization span is omitted. When a Failed thunk is re-accessed from a third location, the new access site is pushed onto `stack` as a frame.

Example: given `[x: [/ 1 0]  y: x]`, accessing `y` produces an error with definition site at `[/ 1 0]` (where the division was written) and materialization site at `x` (where the thunk was first materialized), displayed as `"evaluated here"`.

**Note spans — value origin (Nickel dual-position pattern):** For lazy evaluation errors where the **value that caused the failure** was produced far from the error site, note spans (`spans[1..]`) carry labeled pointers to value creation spans. This is the Nickel `EvaluationError` dual-position pattern: "error triggered here, but the offending value came from there." The `Thunk.span` field (set at thunk creation time) provides this origin span without requiring any additional storage.

Note spans are populated by `with_materialization_span` (label `"evaluated here"`) and by error construction sites for secondary value-origin spans. Most errors have zero or one note span; future multi-span errors may have more. When a note span would duplicate the primary span, it is suppressed. Each note span displays as `"\n  note: {label} at {span}"`.

**Stack frames:** Each frame is `⟨label, span⟩` where `label` identifies the context (e.g., the thunk's origin name, `"materialized"` for re-access) and `span` is the source location. Frames are added by `attach_materialization_context` during propagation and by the Failed state handler during re-access.

### Part 2: Error Sources

All errors are constructed via `EvalError` methods that create an error with a specific `ErrorKind` variant. The main named constructors are:

| Constructor | ErrorKind Variant | Message Pattern | `def_span` Source |
|------------|-------------------|----------------|-------------------|
| `key_not_found(key, available_keys, span)` | `KeyNotFound { key, available_keys }` | `"key not found: {key}"` | Access expression |
| `type_mismatch(expected, got, span)` | `TypeMismatch { context: None, expected, got }` | `"type mismatch: expected {expected}, got {got}"` | Expression producing wrong type |
| `type_mismatch_ctx(context, expected, got, span)` | `TypeMismatch { context: Some(context), expected, got }` | `"{context}: expected {expected}, got {got}"` | Expression producing wrong type |
| `arity_mismatch(expected, got, span)` | `ArityMismatch { expected, got, callee, params }` | `"arity mismatch: expected {expected} arguments, got {got}"` | Call expression |
| `circular_dependency(name, definition_span, cycle_path)` | `CircularDependency { name, cycle_path }` | `"circular dependency detected while evaluating {name}"` | Thunk definition |
| `user_error(message, span)` | `UserError { message }` | `"{message}"` (user-provided) | `raise` call site |
| `integer_overflow(op, span)` | `IntegerOverflow { op }` | `"{op}: integer overflow"` | Arithmetic expression |
| `division_by_zero(op, span)` | `DivisionByZero { op }` | `"{op}: division by zero"` | Division expression |
| `float_not_finite(builtin, value, span)` | `FloatNotFinite { builtin, value }` | `"{builtin}: {value} is not a finite number"` | Builtin call expression |
| `float_out_of_range(builtin, value, span)` | `FloatOutOfRange { builtin, value }` | `"{builtin}: {value} is out of range for Int"` | Builtin call expression |
| `empty_collection(op, span)` | `EmptyCollection { op }` | `"{op} on empty collection"` | Builtin call expression |
| `named_arg_rejected(builtin, span)` | `NamedArgRejected { builtin }` | `"{builtin} does not accept named arguments"` | Call expression |
| `resource_limit_exceeded(message, span)` | `ResourceLimitExceeded { message }` | `"{message}"` (caller-provided) | Resource-checking site |
| `schema_violation(violations, span)` | `SchemaViolation { violations }` | `"schema validation failed with {n} error(s):\n  {field}: {msg}\n..."` | Schema validation expression |
| `internal(message, span)` | `Internal { message }` | `"{message}"` (implementation-defined) | Context-dependent |

See `src/error.rs` for the full set of `ErrorKind` variants and their constructors. The table above shows representative error constructors; additional variants not listed include: `UndefinedVariable`, `TypeAssertFailed`, `NoInstance`, `MacroError`, `NamedArgConflict`, `UnknownNamedArg`, `MissingRequiredParam`, `DuplicateKey`, `ValueNotSerializable`, `IncludeIoError`, `IncludeCycle`, `IncludeFileTooLarge`, `IncludeHashMismatch`, `IncludeHashRequired`, `ParseConversion`, `UriParseError`, `MatchExhaustion`, `DuplicateVariable`, `Unimplemented`, `BuilderFinished`, and `KindMismatch`.

**Special error properties:**

- **`ResourceLimitExceeded` is not catchable:** `try` does not catch `ResourceLimitExceeded` errors — they propagate to the runtime. Resource limit errors should not be suppressible by user code (follows GHC's `StackOverflow` and Racket's `exn:fail:resource` semantics). The `is_catchable()` method returns `false` for this variant, `true` for all others.

- **All errors are cached in Failed thunk state:** When a thunk fails, the error is cached permanently via `OnceCell`. There are no non-cacheable errors — the old recursive evaluator's depth system (which had non-cacheable `DepthExceeded` errors) was replaced by the CEK machine.

### Part 3: Error Decoration

**[DECORATE]** — `attach_materialization_context(ε, mat_span, origin, thunk_span)`:

```text
DECORATE(ε, mat_span, origin, thunk_span):
  (1) if mat_span is Some(s) ∧ ε has no "evaluated here" note span:
        ε.spans.push(⟨s, "evaluated here"⟩)
  (2) if mat_span is Some(s) ∧ ε already has an "evaluated here" note span s'
        ∧ s ≠ s' ∧ s ∉ {f.span | f ∈ ε.stack}:
        ε.stack.push(⟨"materialized", s⟩)
  (3) if origin ≠ "" ∧ ∄f ∈ ε.stack. f.label = origin ∧ f.span = thunk_span:
        ε.stack.push(⟨origin, thunk_span⟩)
```

Rule (1) adds the materialization span as a note span labeled `"evaluated here"` on first decoration. Rule (2) adds subsequent materialization sites as stack frames without overwriting the original note span. Rule (3) adds the thunk's origin label (e.g., variable name) as a frame — the `origin` parameter corresponds to the thunk's origin name as described in §Scope Chain Semantics — Formal Specification. The deduplication guards (`s ∉ stack`, `∄f matching (label, span)`) prevent redundant frames when the same span propagates through nested `materialize` calls.

**Invariant:** The first `"evaluated here"` note span, once added, is never replaced. The materialization span records the *first* site that materialized the thunk; subsequent sites become stack frames.

### Part 4: Error Propagation

Errors propagate upward through materialization chains via Rust's `?` operator (early return of `Result::Err`). Every `materialize` call site is a potential decoration point. Rules use the same judgment notation as [Evaluation](08-evaluation.md) §Forcing Rules: `Σ_θ` denotes the `EvalContext` captured at thunk construction time.

**[PROP-EVAL]** — Unevaluated thunk evaluation:

```text
eval(expr, env, Σ_θ) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
thunk.state ← Failed(ε')
──────────────────────────
materialize(thunk, mat_span) ⇒ Err(ε')
```

Note: `eval()` may internally call `materialize()` recursively (e.g., for PendingCall resolution). PROP-EVAL covers the outer eval → error path; nested materialization within eval follows PROP-RESULT or PROP-BUILTIN depending on the thunk state encountered.

**[PROP-BUILTIN]** — PendingBuiltin execution:

```text
func(args, named, Σ_θ, pd, cs) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
thunk.state ← Failed(ε')
──────────────────────────
materialize(thunk, mat_span) ⇒ Err(ε')
```

**[PROP-RESULT]** — Recursive materialization of result thunk:

```text
func(...) ⇒ Ok(θ_result)
materialize(θ_result, mat_span) ⇒ Err(ε)
ε' = DECORATE(ε, mat_span, origin, thunk_span)
thunk.state ← Failed(ε')
──────────────────────────
materialize(thunk, mat_span) ⇒ Err(ε')
```

All errors are cached in Failed thunk state. There are no non-cacheable errors — once a thunk fails, the error is permanently stored.

**PendingCall coverage:** PendingCall thunks have four error paths (function materialization, invoke_function, result materialization, type mismatch). All follow the same DECORATE + cache pattern: function materialization failures and type mismatches are decorated inline; result materialization follows PROP-RESULT; invoke_function failures are decorated and cached. PendingCall restoration requires cloning `func`, `args`, and `named` before evaluation (all `Rc::clone` — no materialization) since `take_pending_call()` consumes ownership.

**Nested materialization span:** When a PendingCall handler materializes `func_thunk` and that materialization fails, the error's `"evaluated here"` note span is set to `call_span` (the site where the function call was written), not the span of the inner expression that actually failed. This follows from passing `Some(&call_span)` as the `mat_span` parameter to `materialize(&func_thunk, ...)`. The same behavior applies to PendingBuiltin when materializing arguments — the builtin's `call_span` becomes the materialization site for nested errors. This ensures that error reports consistently attribute materialization to the call site, even when the actual failure occurs in a deeply nested thunk chain.

**[PROP-CYCLE]** — Circular dependency:

```text
thunk.state = InProgress
ε = circular_dependency(name, thunk.span)
thunk.state ← Failed(ε)
──────────────────────────
materialize(thunk, mat_span) ⇒ Err(ε)
```

Note: PROP-CYCLE constructs the error inline at the detection site — it does *not* pass through DECORATE. This is the only propagation rule that bypasses decoration, because the error originates at the materialization site itself rather than propagating from a deeper call.

**Propagation path:** In a chain `θ₁ → θ₂ → θ₃` where θ₃ fails, the error propagates θ₃ → θ₂ → θ₁, each level applying DECORATE. The result is an error with the primary span from θ₃ (where the problem was defined), a note span from the first materialization site (labeled `"evaluated here"`), and stack frames from intermediate materialization points.

### Part 5: Failed State Memoization

**[MEMO-CACHE]** — On first error, cache in Failed state:

```text
materialize(thunk, ...) ⇒ Err(ε)
──────────────────────────
thunk.state ← Failed(ε)
```

All errors are cached in Failed thunk state via `OnceCell`. There are no non-cacheable errors.

**[MEMO-REACCESS]** — On subsequent access of a Failed thunk:

```text
thunk.state = Failed(ε_cached)
ε' = clone(ε_cached)
(1) if mat_span is Some(s) ∧ ε' has no "evaluated here" note span:
      ε'.spans.push(⟨s, "evaluated here"⟩)
      update cache: thunk.state ← Failed(ε')
(2) if mat_span is Some(s) ∧ ε' already has "evaluated here" note span s'
      ∧ s ≠ s' ∧ s ∉ {f.span | f ∈ ε'.stack}:
      ε'.stack.push(⟨"materialized", s⟩)
      update cache: thunk.state ← Failed(ε')
(3) if mat_span is None:
      (no decoration, no cache update — return ε' unchanged)
──────────────────────────
materialize(thunk, mat_span) ⇒ Err(ε')
```

MEMO-REACCESS mirrors DECORATE but operates on the cached error. Cache updates are progressive: each new access site enriches the cached error's stack. This is the Failed self-edge in the thunk lifecycle DAG — it refines diagnostic metadata without changing the error's semantic content (message, primary span).

**Permanence:** Once a thunk reaches Failed, it never returns to any other state (§Thunk Lifecycle, Semantic Commitment #1). No retry, no recovery. This includes I/O failures from `include`.

### Part 6: `try` Catching Boundary

These rules describe **`builtin-try`** — the Rust-native builtin registered as `$try`. The prelude `try` wrapper calls `builtin-try` and converts its plain-dict result into nominal `Variant("Ok", ...)` / `Variant("Error", ...)` values. See below for the two-layer distinction.

**[TRY]** — `builtin-try`: tinct function callee succeeds:

```text
materialize(θ_func, _) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _) ⇒ Ok(v)
──────────────────────────
try(θ_func, s) ⇒ ok_val(Variant("Ok", θ(v)))
```

**[TRY-ERR]** — `builtin-try`: tinct function callee raises a catchable error:

```text
materialize(θ_func, _) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _) ⇒ Err(ε)
ε.kind.is_catchable()
──────────────────────────
try(θ_func, s) ⇒ ok_val(Variant("Error", θ(ε.kind.to_string())))
```

**[TRY-UNCATCHABLE]** — `builtin-try`: uncatchable error re-raised:

```text
materialize(θ_func, _) ⇒ Function([], body, env)
θ_body = Thunk::new_unevaluated(body, env, body.span)
materialize(θ_body, _) ⇒ Err(ε)
¬ε.kind.is_catchable()
──────────────────────────
try(θ_func, s) ⇒ Err(ε)
```

**[TRY-BUILTIN]** — `builtin-try`: builtin zero-arg function callee. Note: returns a **plain dict** (`{ok ↦ v}`), not a nominal variant. The prelude `try` wrapper is responsible for converting this to `Variant("Ok", ...)` / `Variant("Error", ...)`:

```text
materialize(θ_func, _) ⇒ Builtin(f)
f([], {}, s) ⇒ Ok(θ_result)
materialize(θ_result, _) ⇒ Ok(v)
──────────────────────────
try(θ_func, s) ⇒ ok_val(Dict({ok ↦ θ(v)}))
```

(Catchable error variant: same structure, `Err(ε), ε.kind.is_catchable() ⇒ Dict({err ↦ θ(ε.kind.to_string())})`; uncatchable errors re-raised per TRY-UNCATCHABLE)

**Two-layer design:** `builtin-try` (the Rust native) handles error catching and produces a plain dict `{ok: v}` or `{error: msg}` when the callee is a builtin. The prelude `try` function wraps `builtin-try` and normalizes the result to `Variant("Ok", ...)` or `Variant("Error", ...)` regardless of whether the callee was a tinct function or a builtin. User code should always call prelude `try`, never `builtin-try` directly.

**Catching boundary:** `try` catches errors at the zero-argument function body boundary. The function is materialized *outside* the catch — if the function thunk itself fails to materialize, that error propagates to `try`'s caller (not caught). Only errors from *calling* the function (evaluating its body) are caught.

**Error-to-value conversion:** `try` extracts only the message string (`ε.kind.to_string()`). The spans and stack frames are discarded — `try` is for program-level error handling, not diagnostic reporting. The result is a nominal variant `[Ok ...]` or `[Error ...]`, not an ordinary dict.

**Arity constraint:** The function must take zero parameters. If `params.len() > 0`, `try` raises an error (not caught): `"try: expected a zero-argument function, got {n} parameters"`.

**Interaction with Failed state:** When `try` materializes a Failed thunk *inside* the body, the cached error is returned via MEMO-REACCESS and caught by `try`. The Failed thunk's cache is updated (stack frame added) but the error is converted to `[Error message]` — it does not propagate past `try`.

**`try` interaction with structured errors:** `try` extracts `ε.kind.to_string()` — the Display output of ErrorKind. This preserves the behavior that the caught value is a human-readable error message string, not a structured error object.

**Rationale:** `try` is for program-level error recovery ("did it fail?"), not error introspection. Programs that need to distinguish error kinds should use type checking and validation, not `try`-and-parse. Exposing structured error data through `try` would create a coupling between error representation (an implementation detail) and user programs.

**Display stability:** Display message *wording* is not part of the stability contract — message text may be refined for clarity across releases. Programs that match on `try` error strings are inherently fragile and should not rely on exact wording.

### Part 7: Properties

**E1 — Error determinism:** For a given program state (environment, thunk graph), the same error is produced regardless of evaluation order. This follows from the pure subset's confluence (§Thunk Lifecycle, Semantic Properties). `$include` breaks this — file system state introduces nondeterminism.

**E2 — Memoization permanence:** `Failed(ε)` is absorbing — no transition out of Failed exists. Formally: if `thunk.state = Failed(ε)` at time t, then `thunk.state = Failed(ε')` for all t' > t, where `ε'.kind = ε.kind` and the primary span (`spans[0]`) is unchanged. Note spans may grow (new `"evaluated here"` entries added). Stack frames may grow monotonically.

**E3 — Propagation preserves primary span:** DECORATE never modifies `ε.spans[0]` (the primary span). The definition site is set at error construction and propagated unchanged through any number of materialization layers.

**E4 — Materialization site is first-materialization:** The first `"evaluated here"` note span records the first site that triggered materialization. Subsequent access sites become stack frames. This is enforced by DECORATE rule (1) (add only if no note span exists) and MEMO-REACCESS rule (1).

**E5 — `try` isolation:** Errors caught by `try` do not propagate to `try`'s caller. `try` converts errors to values — the error is consumed, not rethrown. There is no `$rethrow` mechanism.

**E6 — Stack frame monotonicity:** The `stack` field of a cached error grows monotonically — frames are appended, never removed or reordered. Each re-access of a Failed thunk from a new location adds at most one frame.

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
| MEMO-CACHE | see `Thunk::cache_failure` in `src/value.rs` |
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

1. **Programmatic error identity** — tests and tooling can branch on error kind via `kind_name()` (returns a kebab-case string like `"key-not-found"`, `"type-mismatch"`).
2. **Structured data extraction** — domain-specific fields (e.g., `key` in `KeyNotFound`, `available_keys` for suggestions) are directly accessible.
3. **Multi-format rendering** — error data is separated from presentation, supporting JSON output, LSP diagnostics, and format-independent rendering.

The structured error model is fully implemented. The builtins registered via `core_builtins()` / `builtin_module("core")` and the corpus error tests comprehensively exercise the error variants.

### Design: `ErrorKind` Enum

Replace the `message: String` field in `EvalError` with `kind: ErrorKind`. Each variant carries structured domain data. Human-readable messages are derived via `Display` on `ErrorKind`, not stored.

```rust
pub struct EvalError {
    pub kind: ErrorKind,
    /// All labeled spans in priority order.
    /// `spans[0]` = primary (header) location; `spans[1..]` = note spans.
    pub spans: Vec<(Span, String)>,
    pub stack: SmallVec<[StackFrame; 8]>,
    pub macro_expansion: Option<(String, Span)>,
    pub blame: Option<BlameLabel>,
    pub pipeline_stage: Option<PipelineBlame>,
}
```

### Part 1: Variant Catalog

```rust
#[derive(Debug, Clone)]
pub enum ErrorKind {
    // --- Access errors ---
    KeyNotFound { key: String, available_keys: Vec<String> },
    /// `name` stores the identifier (bare name, no sigil prefix).
    /// For `%` pipeline refs, the name includes the `%`: `"undefined variable: %foo"`.
    UndefinedVariable { name: String },

    // --- Type errors ---
    /// Runtime type mismatch from evaluator or builtin dispatch.
    /// `context` carries the operation name (e.g., `"merge"`, `"document pipeline"`)
    /// when the mismatch originates from a named operation; `None` for generic mismatches.
    /// `expected` is human-readable, not machine-parseable — may contain
    /// compound descriptions like `"Dict or Seq"`.
    TypeMismatch { context: Option<String>, expected: String, got: String },
    /// User-written type assertion (`[@Type value]`) failed at runtime.
    /// Semantically distinct from `TypeMismatch` — this is a user-authored
    /// type guard, not an internal evaluator check.
    TypeAssertFailed { expected: String, got: String },
    /// No typeclass instance found for the given types.
    NoInstance { class_name: String, type_tags: Vec<String> },
    /// Macro expansion error — validation failures, splice position errors, etc.
    MacroError { message: String },

    // --- Call errors ---
    ArityMismatch { expected: ArityBound, got: usize, callee: Option<Arc<str>>, params: Vec<Arc<str>> },
    MissingRequiredParam { param: String, callee: Option<Arc<str>> },
    NamedArgConflict { param: String, callee: Option<Arc<str>> },
    UnknownNamedArg { name: String, valid_params: Vec<String>, callee: Option<Arc<str>> },
    NamedArgRejected { builtin: String },

    // --- Value errors ---
    DuplicateKey { key: String },
    /// `op` carries the operator symbol (e.g., `"/"`) for Display prefix.
    DivisionByZero { op: String },
    IntegerOverflow { op: String },
    /// Covers NaN, Infinity, and -Infinity — values that are not finite
    /// and cannot be converted to Int or used in contexts requiring finite floats.
    FloatNotFinite { builtin: String, value: f64 },
    EmptyCollection { op: String },
    /// Runtime value type cannot be serialized to JSON (Function, Builtin, Seq, Proxy).
    ValueNotSerializable { value_type: String },
    /// Float-to-int conversion failed: value is finite but outside i64 range.
    FloatOutOfRange { builtin: String, value: f64 },

    // --- Limit errors ---
    /// Resource limit exceeded (collection size, string size, etc.).
    /// This is non-catchable — resource limits are
    /// safety boundaries, not application-level errors.
    ResourceLimitExceeded { message: String },

    // --- Include errors ---
    /// Covers both "cannot open" (canonicalize failure) and "cannot read"
    /// (metadata/read failure). The `detail` field carries the OS error.
    IncludeIoError { path: String, detail: String },
    IncludeCycle { path: String },
    IncludeFileTooLarge { path: String, size: u64, limit: u64 },
    IncludeHashMismatch { path: String, expected: String, actual: String },
    IncludeHashRequired { path: String },

    // --- Conversion errors ---
    ParseConversion { builtin: String, input: String, target: String },
    UriParseError { detail: String },

    // --- Evaluation structure ---
    CircularDependency { name: String, cycle_path: Vec<(Arc<str>, Span)> },
    /// Non-exhaustive match: no pattern arm matched the scrutinee.
    MatchExhaustion { scrutinee_type: String },
    /// A variable name appears more than once in a single pattern.
    DuplicateVariable { name: String },

    // --- User-generated ---
    UserError { message: String },
    /// Placeholder expression (`...`) reached during evaluation.
    Unimplemented { message: String },
    /// Operation attempted on a builder that has already been finished (frozen).
    BuilderFinished { op: String },

    // --- Schema validation ---
    SchemaViolation { violations: Vec<(String, String)> },
    /// Type kind mismatch — a type constructor's kind does not match the expected kind.
    KindMismatch { expected: String, got: String },

    // --- Escape hatch ---
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
- **`TypeMismatch` vs `TypeAssertFailed`:** `TypeMismatch` is an evaluator/builtin dispatch error ("merge got the wrong type"). `TypeAssertFailed` is a user-written runtime type guard failure (`[@Integer "hello"]`). Different error class, different suggestions, different kind name. `ThunkState::Guarded` validation (§TypeAssert Runtime Validation) produces `TypeAssertFailed` for guard failures.
- **`context: Option<String>` in `TypeMismatch`** carries the operation name when the mismatch originates from a named operation (e.g., `"merge"` in `"merge: expected Dict, got Int"`). `None` for generic type mismatches from the evaluator. The `expected` field is human-readable, not machine-parseable — it may contain compound descriptions like `"Dict or Seq"`. Programmatic matching on expected types is not supported; use the kind name and `context` field instead.
- **`DivisionByZero` carries `op`** to preserve the operator prefix in Display output (e.g., `"/: division by zero"`). This maintains `try` message compatibility and future-proofs for additional division operators.
- **`FloatNotFinite`** covers NaN, Infinity, and -Infinity — all non-finite `f64` values. Named `NotFinite` rather than `OutOfRange` because NaN is not a range concept.
- **`FloatOutOfRange`** covers finite values outside the `i64` range (e.g., `1e19`). Named `OutOfRange` rather than `NotFinite` because the value is mathematically finite — it simply exceeds the integer domain of `$floor`/`$round`. Distinct from `FloatNotFinite` because the conditions and user-facing diagnostics differ: `FloatNotFinite` is "not a finite number", `FloatOutOfRange` is "out of range for Int".
- **`IncludeIoError`** covers both "cannot open" (canonicalize failure) and "cannot read" (metadata/read failure) — both are filesystem IO failures distinguished by the `detail` field.
- **`Internal` is an escape hatch**, not a permanent category. It accepts a freeform message string for incremental migration. New error sites should use a typed variant; `Internal` should trend toward zero usage over time.
- **Terminology:** "Type error" in this section always means a *runtime* type mismatch (`ErrorKind::TypeMismatch`). Static type checking failures are `TypeError` in `src/types.rs` — a separate type, separate system, separate error reporting path.

### Part 2: Error Kind Names

Each variant maps to a stable kebab-case kind name via the `kind_name()` method. Kind names are the programmatic identifier for error classes — they appear in diagnostic output and can be used for filtering.

| Kind name | Variant | Category |
|-----------|---------|----------|
| `key-not-found` | `KeyNotFound` | Access |
| `undefined-variable` | `UndefinedVariable` | Access |
| `type-mismatch` | `TypeMismatch` | Type |
| `type-assert-failed` | `TypeAssertFailed` | Type |
| `no-instance` | `NoInstance` | Type |
| `macro-error` | `MacroError` | Type |
| `arity-mismatch` | `ArityMismatch` | Call |
| `named-arg-conflict` | `NamedArgConflict` | Call |
| `unknown-named-arg` | `UnknownNamedArg` | Call |
| `named-arg-rejected` | `NamedArgRejected` | Call |
| `missing-required-param` | `MissingRequiredParam` | Call |
| `duplicate-key` | `DuplicateKey` | Value |
| `division-by-zero` | `DivisionByZero` | Value |
| `integer-overflow` | `IntegerOverflow` | Value |
| `float-not-finite` | `FloatNotFinite` | Value |
| `empty-collection` | `EmptyCollection` | Value |
| `value-not-serializable` | `ValueNotSerializable` | Value |
| `float-out-of-range` | `FloatOutOfRange` | Value |
| `resource-limit` | `ResourceLimitExceeded` | Limit |
| `include-io-error` | `IncludeIoError` | Include |
| `include-cycle` | `IncludeCycle` | Include |
| `include-file-too-large` | `IncludeFileTooLarge` | Include |
| `include-hash-mismatch` | `IncludeHashMismatch` | Include |
| `include-hash-required` | `IncludeHashRequired` | Include |
| `parse-conversion` | `ParseConversion` | Conversion |
| `uri-parse-error` | `UriParseError` | Conversion |
| `circular-dependency` | `CircularDependency` | Evaluation |
| `match-exhaustion` | `MatchExhaustion` | Evaluation |
| `duplicate-variable` | `DuplicateVariable` | Evaluation |
| `user-error` | `UserError` | User |
| `unimplemented` | `Unimplemented` | User |
| `builder-finished` | `BuilderFinished` | User |
| `schema-violation` | `SchemaViolation` | Schema validation |
| `kind-mismatch` | `KindMismatch` | Schema validation |
| `internal-error` | `Internal` | Internal |

Kind names are derived from the variant via a method:

```rust
impl ErrorKind {
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::KeyNotFound { .. } => "key-not-found",
            Self::UndefinedVariable { .. } => "undefined-variable",
            Self::TypeMismatch { .. } => "type-mismatch",
            Self::TypeAssertFailed { .. } => "type-assert-failed",
            Self::NoInstance { .. } => "no-instance",
            Self::MacroError { .. } => "macro-error",
            Self::ArityMismatch { .. } => "arity-mismatch",
            // ... etc for each variant
            Self::Internal { .. } => "internal-error",
        }
    }

    /// Returns `false` for errors that `try` must not catch.
    /// Resource limit errors (`ResourceLimitExceeded`) should
    /// propagate to the runtime, not be suppressible by user code.
    /// Follows GHC's StackOverflow and Racket's exn:fail:resource semantics.
    pub fn is_catchable(&self) -> bool {
        !matches!(self, Self::ResourceLimitExceeded { .. })
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
                    write!(f, "unexpected named argument: \"{name}\" (function has no parameters)")
                } else {
                    write!(f, "unexpected named argument: \"{name}\" (valid parameter names: {})",
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
            Self::ResourceLimitExceeded { message } =>
                write!(f, "{message}"),
            Self::IncludeIoError { path, detail } =>
                write!(f, "include: cannot access \"{path}\": {detail}"),
            Self::IncludeCycle { path } =>
                write!(f, "circular include detected: \"{path}\""),
            Self::IncludeFileTooLarge { path, size, limit } =>
                write!(f, "include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"),
            Self::ParseConversion { builtin, input, target } =>
                write!(f, "{builtin}: cannot parse {input:?} as {target}"),
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

`EvalError::Display` renders the error message with labeled spans:

```rust
impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Primary span: spans[0] — its label is determined by the error kind
        let primary_span = self.spans.first().map(|(s, _)| s);
        if let Some(ref def_span) = primary_span {
            let def_label = self.kind.definition_span_label();
            write!(f, "{} ({def_label} {})", self.kind, def_span)?;
        } else {
            write!(f, "{}", self.kind)?;
        }
        // Note spans: spans[1..] — each displayed as "  note: {label} at {span}"
        for (ref span, ref label) in self.spans.iter().skip(1) {
            write!(f, "\n  note: {label} at {}", span)?;
        }
        // Stack frames — all frames are shown
        for frame in &self.stack {
            write!(f, "\n  in {} at {}", frame.label, frame.definition_span)?;
        }
        Ok(())
    }
}
```

Example output: `key not found: name (defined at 3:5-3:10)\n  note: evaluated here at 7:1-7:8`

The display format is:

```text
{message} ({def_label} {line}:{col}-{line}:{col})
  note: {label} at {line}:{col}-{line}:{col}
  in {frame_label} at {line}:{col}-{line}:{col}
```

The primary span label (`def_label`) is determined by the error kind: `"defined at"` for most errors, `"value from"` for `TypeAssertFailed`. Note spans use their own labels — `"evaluated here"` for materialization sites, and context-specific labels for value-origin spans.

Stack frames are printed in the order they were added during propagation — innermost (closest to the error source) first, outermost (closest to the output root) last.

### Part 5: Named Constructors

Named constructors create `EvalError` values with the appropriate `ErrorKind` variant and primary span:

```rust
impl EvalError {
    pub fn key_not_found(key: &str, available_keys: Vec<String>, span: Span) -> Self {
        Self {
            kind: ErrorKind::KeyNotFound { key: key.to_string(), available_keys },
            spans: vec![(span, String::new())],
            stack: SmallVec::new(),
            macro_expansion: None, blame: None, pipeline_stage: None,
        }
    }
    // ... etc for each variant with a convenience constructor
}
```

`EvalError::internal(message, span)` constructs `ErrorKind::Internal` as an escape hatch for cases where no typed `ErrorKind` variant applies. Prefer typed constructors; `internal()` usage is greppable and intentionally verbose.

### Part 6: `try` Interaction

See the `try` Catching Boundary section above (Error Semantics Part 6) for the formal specification of `try` semantics.

### Part 7: Rendering Separation

The `ErrorKind` enum separates error **data** from error **presentation**. The `Display` impl is the default text renderer. Renderers pattern-match on `ErrorKind` directly:

- **JSON output** (`--error-format json`): serialize `ErrorKind` variant name, structured fields, kind name, spans
- **LSP diagnostics**: map `ErrorKind` to `DiagnosticSeverity`, use structured fields for `relatedInformation`
- **Rich terminal** (`codespan-reporting`): use spans for source snippets with carets

These renderers are not specified here. The error construction sites are designed to support them without modification.

### Part 8: Stack Frame Display Policy

All stack frames are shown in user-facing error output. The `should_display_frame()` function unconditionally returns `true` — no filtering is applied. Every frame has a real source location (either a user source span or a Rust source span from `rust_span!()`), so there is no legitimate reason to suppress a frame.

```rust
fn should_display_frame(_frame: &StackFrame) -> bool {
    true
}
```

All frames in `self.stack` are printed in `EvalError::Display`.

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

### Part 10: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| `ErrorKind` enum | `src/error.rs` |
| `ArityBound` enum | `src/error.rs` |
| Kind names | `ErrorKind::kind_name()` method (`src/error.rs`) |
| Catchability | `ErrorKind::is_catchable()` method (`src/error.rs`), integrated in `builtin_try` |
| Message generation | `ErrorKind::Display` impl (`src/error.rs`) |
| `EvalError` struct | `struct EvalError` in `src/error.rs` |
| Named constructors | `src/error.rs` (convenience constructors per variant) |
| `try` extraction | `builtins.rs` builtin_try (`e.kind.to_string()`) |
| Freeform escape hatch | `EvalError::internal()` → `ErrorKind::Internal` |

### Part 10: Source Snippet Rendering

**Status:** Implemented. See `render_span_snippet()` in `src/error.rs`.

Source snippets (source lines with caret annotations, like rustc) are rendered by the **caller** at the display boundary, not by `EvalError::Display`. Source text is not stored in `EvalError` — each display site provides the source string it already holds.

**`render_span_snippet(source: &str, span: Span) -> Option<String>`** — public helper in `src/error.rs`:

- Returns `None` for `Span::origin()` (synthetic spans have no source line)
- Returns `None` if `span.start_line` exceeds the source line count (defensive)
- Single-line spans: extracts the line, places `^` characters from `start_col` to `end_col` (clamped to line length)
- Multi-line spans: first line gets `^` from `start_col` to end of line; middle lines get full `^`; last line gets `^` from column 0 to `end_col`

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

All `ErrorKind` variants map to kebab-case kind names (via `kind_name()`) and human-readable messages.

| ErrorKind Variant | Kind Name | Message Pattern | Definition Site |
|-------------------|-----------|----------------|-----------------|
| **KeyNotFound** | `key-not-found` | `"key not found: {key}"` (base); appends `" (did you mean: '{suggestion}')"` when a close match exists (Jaro-Winkler > 0.8), or `" (available keys: {k1}, {k2}, ...)"` (up to 5) when no close match exists and `available_keys` is non-empty | Access expression (`dict.key`, `dict[key]`) |
| **UndefinedVariable** | `undefined-variable` | `"undefined variable: {name}"` | Variable reference expression |
| **TypeMismatch** | `type-mismatch` | `"type mismatch: expected {expected}, got {got}"` or `"{context}: expected {expected}, got {got}"` | Expression that produced the wrong type |
| **TypeAssertFailed** | `type-assert-failed` | `"type assertion failed: expected {expected}, got {got}"` | Type assertion expression (`[@Type expr]`) |
| **NoInstance** | `no-instance` | `"no instance of {class_name} for types ({type_tags})"` | Typeclass dispatch site |
| **MacroError** | `macro-error` | `"{message}"` | Macro expansion site |
| **ArityMismatch** | `arity-mismatch` | `"arity mismatch: expected {bound}, got {n}"` | Call expression |
| **NamedArgConflict** | `named-arg-conflict` | `"parameter '{param}' received both positional and named argument"` | Call expression |
| **UnknownNamedArg** | `unknown-named-arg` | `"unexpected named argument: \"{name}\" (valid parameter names: {params})"` or `"unexpected named argument: \"{name}\" (function has no parameters)"` | Call expression |
| **NamedArgRejected** | `named-arg-rejected` | `"{builtin} does not accept named arguments"` | Call expression |
| **MissingRequiredParam** | `missing-required-param` | `"missing argument for required parameter '{param}'"` | Call expression |
| **DuplicateKey** | `duplicate-key` | `"duplicate key: {key}"` | Second occurrence of the key |
| **DivisionByZero** | `division-by-zero` | `"{op}: division by zero"` | Division call expression |
| **IntegerOverflow** | `integer-overflow` | `"{op}: integer overflow"` | Arithmetic call expression |
| **FloatNotFinite** | `float-not-finite` | `"{builtin}: {value} is not a finite number"` | Builtin call expression |
| **EmptyCollection** | `empty-collection` | `"{op} on empty collection"` | Builtin call expression |
| **ValueNotSerializable** | `value-not-serializable` | `"cannot serialize {value_type} to JSON"` | Value being serialized |
| **FloatOutOfRange** | `float-out-of-range` | `"{builtin}: {value} is out of range for Int"` | Builtin call expression |
| **ResourceLimitExceeded** | `resource-limit` | `"{message}"` (implementation-defined) | Context-dependent |
| **IncludeIoError** | `include-io-error` | `"include: cannot access \"{path}\": {detail}"` | `include` call expression |
| **IncludeCycle** | `include-cycle` | `"circular include detected: \"{path}\""` | `include` call expression |
| **IncludeFileTooLarge** | `include-file-too-large` | `"include: file \"{path}\" is {size} bytes, exceeds {limit} byte limit"` | `include` call expression |
| **IncludeHashMismatch** | `include-hash-mismatch` | `"include: integrity check failed for \"{path}\": expected {expected}, got {actual}"` | `include` call expression |
| **IncludeHashRequired** | `include-hash-required` | `"include: integrity hash required for \"{path}\" (--require-integrity)"` | `include` call expression |
| **ParseConversion** | `parse-conversion` | `"{builtin}: cannot parse {input:?} as {target}"` | Builtin call expression |
| **UriParseError** | `uri-parse-error` | `"URI parse error: {detail}"` | `uri`/`url`/`urn` call expression |
| **CircularDependency** | `circular-dependency` | `"circular dependency detected while evaluating {name}"` (appends cycle path visualization when `cycle_path` is non-empty) | Thunk definition (dict entry) |
| **MatchExhaustion** | `match-exhaustion` | `"non-exhaustive match: no pattern matched {scrutinee_type} value"` | `match` expression |
| **DuplicateVariable** | `duplicate-variable` | `"duplicate variable in pattern: {name}"` | Pattern binding |
| **UserError** | `user-error` | `"{message}"` (user-provided) | `raise` call expression |
| **Unimplemented** | `unimplemented` | `"{message}"` | Placeholder expression (`...`) |
| **BuilderFinished** | `builder-finished` | `"builder already finished: cannot {op}"` | Builder operation |
| **SchemaViolation** | `schema-violation` | `"schema validation failed with N error(s):\n  {field}: {msg}\n  ..."` (one violation per line, N = violation count) | Schema validation expression |
| **KindMismatch** | `kind-mismatch` | `"kind mismatch: expected {expected}, got {got}"` | Kind check site |
| **Internal** | `internal-error` | `"{message}"` (implementation-defined) | Context-dependent |

The variants above are exhaustive — every runtime error maps to one of these `ErrorKind` variants. The call convention errors (`arity-mismatch` through `missing-required-param`) correspond to constraint violations C-COVERAGE, C-NO-OVERLAP, and C-NAMED-VALID from doc/04-functions.md §Call Convention. `MissingRequiredParam` is the per-parameter coverage check from the Kotlin model — it fires when a required parameter is not covered by either a positional or named argument. Kind names are stable across releases; message wording may vary.

---

## Discriminated Error Unions Per Subsystem

Operations fail in predictable, distinguishable ways. `try` is for exceptional and unexpected failures — crashes, bugs, and truly unrecoverable states. Expected failure modes return **typed discriminated unions** that callers pattern-match directly. This is the Rust/Haskell model: one typed error union per subsystem, no string matching.

```tinct
[match [lookup-ips cap DnsQtype.A host]
  [Result.Ok addrs]:           [happy-connect cap port addrs]
  [DnsError.NXDomain name]:    [error [str "no such host: " name]]
  [DnsError.Timeout]:          [retry-with-tcp cap host]
  [DnsError.Refused]:          [error "nameserver refused query"]
  [DnsError.ServerFailure]:    [try-next-nameserver cap host]]
```

### Design Rules

1. **One discriminated union per subsystem.** Each domain (`Dns`, `Tls`, `Http`, `Net`, etc.) defines a `FooError` type whose variants enumerate every distinguishable failure mode.

2. **Variants are named after the failure, not the mechanism.** `DnsError.NXDomain` not `DnsError.ResponseCode3`. The variant name is what the caller cares about.

3. **`try` is for unexpected failures only.** If a failure is predictable (host not found, connection refused, authentication failed), it belongs in a typed return value, not in `try`.

4. **Payloads carry context.** `[DnsError.NXDomain name]` carries the queried name. `[TlsError.CertificateExpired cert]` carries the cert. Callers get what they need to produce a meaningful error message or decide on a recovery strategy — without string parsing.

### Pattern

```tinct
# Define the error union for a subsystem
FooError: [type
  [NotFound   key@String]
  [Forbidden  reason@String]
  [Timeout    after@Integer]
  [BadFormat  offset@Integer]]

# Return it alongside success values
do-thing: [fn@[or FooResult FooError] [let input]
  ...]

# Callers pattern-match on named failure modes
[match [do-thing my-input]
  [FooResult payload]:         [use payload.value]
  [FooError.NotFound key]:     [error [str "not found: " key]]
  [FooError.Timeout after]:    [retry-after after]
  [FooError.Forbidden reason]: [raise reason]
  [FooError.BadFormat offset]: [log-and-skip offset]]
```

The net-specific error unions (`DnsError`, `TlsError`, `NetError`, `HttpError`, `WsError`, `QuicError`) are defined in their respective stdlib files.
