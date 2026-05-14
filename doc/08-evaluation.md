# Evaluation

## Lazy Evaluation

A **thunk** is a suspended computation — an expression paired with its evaluation environment, stored without being evaluated. Thunks are the unit of lazy evaluation in tinct: a value "is a thunk" until it is *materialized* (forced), at which point the result is computed and memoized. Subsequent accesses return the cached result without re-evaluation.

Everything is a thunk until materialized. Compute only what's needed, when it's needed. For the complete per-builtin materialization behavior, see §Laziness Design below.

```tinct
[
    # Won't run unless `result` is actually used
    result: [expensive-computation data]

    # Infinite sequences -- only compute what you take
    naturals: [range 0]
    first-ten-evens: [collect
        [take 10
            [filter [fn [n] [= 0 [mod n 2]]] naturals]]]

    # Short-circuit: if condition is true, never evaluate the else branch
    value: [if condition cheap-option very-expensive-option]
]
```

## Recursive Dict Scoping (`letrec`)

**All entries in a dict see each other.** Entry order doesn't matter semantically.

```tinct
[
    x: [+ y 1]    # thunk — when materialized, looks up y → 6
    y: 5
]

# Mutual recursion works
[
    even?: [fn [n] [if [= n 0] true  [odd?  [- n 1]]]]
    odd?:  [fn [n] [if [= n 0] false [even? [- n 1]]]]
]
```

**Why:** Dicts are the fundamental unit — they shouldn't be order-dependent. Lazy evaluation makes this free: all bindings are thunks referencing a shared environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

**Key evaluation scope:** Dict keys are evaluated in the *parent* scope, not the dict's own letrec scope. This means key expressions cannot reference sibling bindings within the same dict. This is intentional for letrec correctness: keys must be deterministic regardless of entry order, and allowing keys to depend on sibling values (which are still unevaluated thunks) would introduce order-dependence or require eager evaluation of referenced entries.

**Why parent scope for keys:** The two-environment pattern (`parent_env` for keys, `dict_env` for values) ensures that computed keys are pure with respect to the dict's own bindings. A key expression like `[$a]` in `[x: 1 $a: 2]` resolves `a` in the *enclosing* scope, not the dict scope — users might expect `a` to reference the sibling binding `x: 1`, but this would create ordering dependence (does `x` exist when the key is evaluated?) and break the letrec invariant that all entries are mutually visible *as thunks* before any are materialized.

Implementation: keys are evaluated via `eval_key(key_expr, parent_env, ctx, depth)` (in `eval_dict` in `src/eval.rs`) before the shared `dict_env` is populated with value thunks. This sequencing is critical: all keys must be known before string-keyed entries can be inserted into `dict_env` as bindings (in the dict environment binding loop in `eval_dict`).

**Effectful key expressions:** Computed keys may contain effectful operations (such as `$include`). These effects execute in the parent scope context, not the dict's letrec scope. For example, `[include "keys.llt"]` in a dict key position evaluates the included file with access to the parent environment's bindings, not the dict's own entries. This is consistent with the scoping rule but means included files used as keys cannot reference the dict's own bindings.

**Circular dependencies** are detected at materialization-time and reported with a clear cycle trace.

**Nested dicts create new scopes.** Each `[]` dict introduces a new lexical scope. Inner scopes see all bindings from outer scopes, and inner bindings shadow outer bindings of the same name within that inner dict. Scoping is lexical, not dynamic — closures capture their defining environment, not the calling environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

The `Environment` struct's `parent` field implements this: each nested dict gets a new `Environment` whose `parent` points to the enclosing dict's environment. Variable lookup walks the parent chain outward.

```tinct
[
    x: 10
    inner: [
        x: 20          # shadows outer x
        y: [+ x 1]     # x is 20 (inner), not 10 (outer)
    ]
    z: [+ x 1]         # x is 10 (outer)
]
```

For the formal specification of scope chains across sequential expressions (the `---` pipeline and multi-expression documents), see [Documents & Pipelines](09-documents.md) §Scope Chain Semantics.

## Sequences and Lazy Computation

**Sequences are lazy computations, not data.** Dicts are data (finite, random-access, known keys). Sequences are suspended computations that produce elements on demand (possibly infinite, sequential access, unknown structure).

This distinction preserves the "everything is a dict" invariant for data while enabling lazy, composable pipelines for computation.

**Runtime representation:**

A sequence is a cons cell: a head value and a tail that is itself a sequence (or empty dict `[]` for end-of-sequence).

```
Value::Seq(head: Rc<Thunk>, tail: Rc<Thunk>)
```

The tail thunk evaluates to either another `Seq` or `[]` (done). Since thunks are memoized, traversing the same sequence twice reuses cached results -- unlike Python generators, which are single-pass.

**`$collect` is the boundary between computation and data:**

```tinct
# Computation (lazy, possibly infinite)
evens: [filter [fn [n] [= 0 [mod n 2]]] [range 0]]

# Data (materialized, finite, dict with integer keys)
first-ten: [collect [take 10 evens]]
# -> [0 2 4 6 8 10 12 14 16 18]
```

`$collect` runs the computation and pours results into a dict with integer keys 0..n. Calling `$collect` on an infinite sequence without `$take` is an error:

```tinct
[collect [range 0]]    # → Error: sequence exceeds MAX_COLLECT_SIZE (1,000,000 elements)
```

This is explicit by design — no accidental infinite materialization.

**Sequence constructors:**

| Function | Finite | Infinite | Description |
|----------|--------|----------|-------------|
| `range` | `[range 0 10]` | `[range 0]` | Integers from start (inclusive); 2-arg has end (exclusive), 1-arg is infinite |
| `repeat` | `[take 5 [repeat x]]` | `[repeat x]` | Infinite Seq of val; use `take` for finite |
| `cycle` | `[take 3 [cycle xs]]` | `[cycle xs]` | Infinite Seq cycling through dict entries; use `take` for finite |
| `seq` | -- | -- | Low-level: `[seq head tail-thunk]` |
| `iterate` | -- | `[iterate f x]` | `x, f(x), f(f(x)), ...` |
| `unfold` | varies | varies | `[unfold step seed]`; step returns `[value state]` or `[]` |

**Sequence operations (lazy -- return sequences):**

| Function | Description |
|----------|-------------|
| `take` | First n elements |
| `drop` | Skip first n elements |
| `filter` | Elements matching predicate |
| `map` | Transform each element (on seq input; on dict input, returns lazy dict) |
| `$concat` | Concatenate two sequences |
| `$zip` | Pair elements from two sequences |

**Sequence destructors (materializing):**

| Function | Description |
|----------|-------------|
| `collect` | Seq to dict with integer keys 0..n |
| `head` | First element (materializes head thunk) |
| `tail` | Rest of sequence (returns seq, does not materialize) |
| `reduce` | Accumulate over sequence elements |
| `seq?` | Type check: is this a Seq? |

### Productivity Obligations

**Sequences are coinductive** — they are defined by observations (head/tail), not by construction (Coquand 1994). A sequence is **productive** if every observation step terminates: taking the head yields a value, and materializing the tail yields another sequence (or `[]`).

**tinct makes no static productivity guarantee.** This is a deliberate choice, shared by every practical lazy language with general recursion (Haskell, Nix, Nickel, Jsonnet). Static productivity checking requires either totality (Turner 2004, Dhall's approach — Turing-incomplete) or sized types (Abel & Pientka 2013, Abel 2012 — require constraint solving beyond HM unification, incompatible with tinct's type inference). Guardedness alone is insufficient: Coquand's proof that guardedness implies productivity assumes all sub-computations terminate, which general recursion does not guarantee. Sequence constructors (`$seq`, `$range`, `$repeat`, etc.) infer as `Type::Seq` — see [Type System Extensions](07-type-extensions.md) §Precision.

**Three layers of runtime protection:**

| Layer | Mechanism | What it catches |
|-------|-----------|----------------|
| Blackholing | `InProgress` thunk state sentinel | Direct cycles: a thunk that references itself during evaluation |
| Depth limit | `MAX_EVAL_DEPTH=256` | Runaway recursion: deeply nested or diverging evaluation chains |
| Tail discipline | `$collect`/`$head`/`$tail` type checks | Malformed tails: sequence tail that evaluates to a non-Seq, non-`[]` value |

**Built-in constructors are productive by construction.** The standard sequence API guarantees productivity for well-behaved arguments:

| Constructor | Productivity guarantee |
|-------------|----------------------|
| `$range` | Always productive (generates integers) |
| `$repeat` | Always productive (repeats a constant) |
| `$cycle` | Productive if input dict is non-empty |
| `$iterate` | Productive if `f` terminates on every input |
| `$unfold` | Productive if step function terminates on every state |
| `$map` on Seq | Productive if source is productive and `f` terminates |
| `$filter` on Seq | Productive if source is productive, predicate terminates, **and infinitely many elements pass** (or source is finite) |

**`$seq` is the raw constructor with user-managed obligations.** `[seq head tail]` wraps two thunks into a Seq without materializing either. This enables guarded corecursion:

```tinct
ones: [seq 1 ones]
# Works: seq does NOT materialize ones. The tail thunk captures ones
# as an unevaluated reference. Each tail observation produces a
# new Seq(1, <thunk>) without diverging.
```

`$seq` is lazy — it does not materialize its arguments (`builtins.rs:builtin_seq` wraps `Rc::clone(&args[0])` and `Rc::clone(&args[1])` directly). This is critical: it means `$seq` acts as a guard in the coinductive sense, allowing corecursive definitions that would cycle under eager evaluation.

**User obligations for `$seq`:**

1. The head thunk must terminate when observed.
2. The tail thunk must evaluate to either a `Seq` or `[]`.
3. Corecursive definitions must have at least one `$seq` constructor between the binding and the recursive reference (guardedness).

Violating these produces a runtime error (cycle detection or depth limit) for the failure modes tinct can detect. Slowly diverging computations (e.g., superpolynomial head evaluation) will appear to hang — this is inherent to any Turing-complete language without static totality.

**Why not static productivity checking:** Idris makes totality/productivity checking opt-in because mandatory checking rejects valid programs (Brady's rationale). Agda/Coq's mandatory guardedness is known to be fragile — it rejects intuitively productive programs, especially those using higher-order functions. Abel & Pientka's (2013) copatterns with sized types provide automatic productivity checking, but require size annotations threaded through the entire type system — constraint solving beyond HM unification. For a data transformation language, the pragmatic approach (productive-by-construction combinators + runtime backstop) provides the right tradeoff between safety and expressiveness.

**Error quality matters more than static checking.** Nix's biggest user-facing pain point with non-productive definitions is not the lack of static checking but the poor diagnostics ("infinite recursion encountered" with no useful context). tinct's error reporting should include: the thunk origin (which binding diverged), the materialization chain (who materialized it), and the cycle path when circular dependencies are detected.

#### Testing Requirements

Corpus tests are required for each sequence constructor (`$range`, `$repeat`, `$cycle`, `$iterate`, `$unfold`), malformed tail errors (Seq tail evaluating to non-Seq, non-`[]` value), and blackholing on diverging sequences. Tests should demonstrate the two runtime protection layers: blackholing (direct cycles via `InProgress` detection), and tail discipline (type checking in `$collect`/`$head`/`$tail`).

### Dual-Dispatch for `$map` and `$filter`

`$map` and `$filter` accept both dicts and sequences, with behavior determined by input type:

| Input | `$map` result | `$filter` result |
|-------|--------------|-----------------|
| Dict | Dict (lazy values via PendingCall thunks) | Seq (must evaluate predicates) |
| Seq | Seq (lazy) | Seq (lazy) |

`$map` on a dict is the key insight: it returns a dict with the **same keys** but each value wrapped in a `PendingCall` thunk. No computation happens until a specific value is accessed. This gives `[map f big-dict]` O(n) construction and O(1) per-element access.

`$filter` on a dict must return a Seq because the output keys are unknown without evaluating predicates. Use `$collect` to get a dict back.

```tinct
# map on dict: same keys, lazy values (no computation yet)
prices-usd: [map [fn [p] [* p 1.1]] prices-eur]
prices-usd.widget    # only this one price is computed

# filter on dict: returns seq (must evaluate predicates to decide inclusion)
expensive: [collect [filter [fn [p] [> p 100]] prices-eur]]

# map on seq: returns seq (lazy)
doubled: [map [fn [n] [* n 2]] [range 0]]
# nothing computed until take/collect
```

#### Testing Requirements

Each dual-dispatch builtin (`map`, `filter`, `take`, `drop`, `reduce`, `join`) requires corpus tests for both Dict and Seq input paths. Tests should verify the dispatch logic (Dict input produces Dict/Seq output as specified) and that the results are semantically equivalent regardless of input type.

## Thunk Lifecycle — Formal Specification

Extends Launchbury (1993) natural semantics for call-by-need with five additional thunk states (Placeholder, PendingBuiltin, PendingCall, Guarded, Failed) for pre-allocation sentinels, deferred computation, contract validation, and error memoization. PendingBuiltin and PendingCall are defunctionalized continuations (Reynolds 1972; Danvy & Nielsen 2003) — they represent deferred computation as data rather than closures. Guarded implements proxy contracts (Findler & Felleisen 2002) for lazy TypeAssert field validation.

**User-visible states:** As a user, you observe three effective states:
- A thunk is **unevaluated** when first created (you defined a binding; nothing has used it yet).
- A thunk is **materialized** when first accessed (you used a value; it was computed and cached).
- A thunk is **failed** when a computation error occurred (the error is cached; re-accessing returns the same error).

The runtime uses five additional internal states for deferred builtins (`PendingBuiltin`), deferred function calls (`PendingCall`), type assertion contracts (`Guarded`), cycle detection (`InProgress`), and pre-allocation sentinels (`Placeholder`).

**State set:** `S = { Placeholder, Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed }`

Placeholder is a pre-construction sentinel excluded from all materialization rules. The materialization lifecycle has 7 participating states.

### Part 1: State Transition Graph

The valid state transitions form an almost-acyclic directed graph — nearly all transitions move strictly forward, with one backward edge exception: non-cacheable errors from builtins restore `InProgress → Guarded` to allow retry (see Exception below).

```
Placeholder ───────────────────────────────→ {any non-InProgress state}

Unevaluated ──────────┐
PendingBuiltin ────────┤
PendingCall ───────────┼──→ InProgress ──┬──→ Materialized
Guarded ──────────────┘                 └──→ Failed ⟲
```

The transition graph governs state *transitions*, not construction. Thunks may be constructed directly in Placeholder state (via `Thunk::new_placeholder()`), Unevaluated, PendingBuiltin, PendingCall, Guarded, or Materialized state (via `Thunk::new_materialized`). The transition graph applies only to subsequent state changes.

Transition rules (each maps to one `take_*` or `set_state` call in `src/value.rs`):

| Transition | Trigger | Atomicity |
|-----------|---------|-----------|
| Placeholder → {any non-InProgress state} | `set_state(...)` at arena allocation time | Direct write — pre-construction sentinel only; materializing a `Placeholder` thunk panics. Legal targets: Unevaluated, PendingBuiltin, PendingCall, Guarded, Materialized. InProgress is excluded because it would trigger cycle detection on the next materialization attempt. |
| Unevaluated → InProgress | `take_unevaluated()` | Atomic (`mem::replace`) |
| PendingBuiltin → InProgress | `take_pending_builtin()` | Atomic (`mem::replace`) |
| PendingCall → InProgress | `take_pending_call()` | Atomic (`mem::replace`) |
| Guarded → InProgress | `take_guarded()` | Atomic (`mem::replace`) |
| InProgress → Materialized | `set_state(Materialized(v))` | Direct write |
| InProgress → Failed | `cache_failure(err)` | Via `transition()` |
| InProgress → Guarded | `set_state(Guarded(...))` | Direct write — **backward edge**, non-cacheable errors from builtins only; restores original state to allow retry |
| InProgress → PendingBuiltin | `set_state(PendingBuiltin(...))` | Direct write — **backward edge**, non-cacheable errors only; restores original state for retry |
| InProgress → PendingCall | `set_state(PendingCall(...))` | Direct write — **backward edge**, non-cacheable errors only; restores original state for retry |
| Failed → Failed | `set_state(Failed(e'))` | Direct write (diagnostic refinement only) |

**Monotonicity proof sketch:** `Placeholder` is a pre-construction sentinel — it sits below all other states in the construction-time ordering. It is not part of the materialization path: materializing a `Placeholder` thunk panics rather than transitioning through InProgress. `Placeholder` transitions directly to any non-InProgress state at allocation time, establishing the thunk's initial materialization state before any evaluation begins. This is a pure construction-time concept and does not interact with the Launchbury monotonicity argument below.

The materialization graph (excluding `Placeholder`) has no cycles (the single backward edge is acyclic: InProgress cannot return to itself through Guarded). Each source state (Unevaluated, PendingBuiltin, PendingCall, Guarded) transitions only to InProgress. InProgress transitions only to Materialized or Failed — with one exception: the backward `InProgress → Guarded` edge for non-cacheable errors from builtins (see Exception below); this preserves semantic monotonicity because the thunk's observable meaning is unchanged between retries. Materialized is terminal — no transitions out. Failed has a self-edge for diagnostic refinement (enriching materialization spans and stack frames), but the error's semantic identity is fixed — only diagnostic metadata may be updated. Therefore all transition sequences are finite, and the semantic content of a thunk is monotonically determined. ∎

**Exception — retryable non-cacheable errors:** Three backward edges exist for non-cacheable errors from deferred-computation states:
- `InProgress → Guarded`: fires when a Guarded thunk's inner materialization fails with a non-cacheable error (see `[MATERIALIZE-GUARD-NONCACHEABLE]`).
- `InProgress → PendingBuiltin`: fires when a PendingBuiltin's execution raises a non-cacheable error; restores the original PendingBuiltin state for retry.
- `InProgress → PendingCall`: fires when a PendingCall's invocation raises a non-cacheable error; restores the original PendingCall state for retry.

All three fire under the same condition: any non-cacheable error (e.g., `DepthExceeded` from builtins) from a deferred state restores that state for retry. With the iterative CEK machine, `DepthExceeded` no longer arises from the core materialize/eval loop — it can only be raised by individual builtins (e.g., `MAX_COLLECT_SIZE` in `deep_materialize`). Because such errors are transient resource-bound conditions (not semantic errors), they are non-cacheable — `cache_failure` is skipped and the thunk is restored to its pre-InProgress state so the computation can be retried. These backward restorations mean strict state-order monotonicity does not hold for the non-cacheable path. However, semantic monotonicity is preserved: the thunk's observable meaning is unchanged between attempts, and the error identity is not fixed. Every other error kind is cacheable and takes the normal `InProgress → Failed` forward edge. (`src/eval_materialize.rs`, in the `force_step()` match arms for `Guarded`, `PendingBuiltin`, and `PendingCall`)

**Atomicity invariant:** Each `take_*` method atomically swaps the thunk state to InProgress before returning the captured data. This ensures no observer can see the old state after the transition begins. The atomicity is provided by `std::mem::replace` under an exclusive `borrow_mut()` — Rust's borrow checker prevents double borrows within a single thread.

### Part 2: Materialization Rules

Materialization dispatches on the current state to produce a value or error. Rules use two judgment forms: `materialize(θ) ⇒ v` where θ is a thunk and v is the resulting value; and `eval(e, ρ, Σ) ⇒ θ` where e is an expression, ρ is the lexical environment, Σ is the EvalContext (base directory, include guards, stdlib env), and θ is the resulting thunk. The EvalContext Σ is captured inside each thunk at construction time (written Σ_θ when referencing a specific thunk's context) and is not a parameter of `materialize` — it is part of the thunk's closure.

**Notation:** The rules use an implementation-oriented notation mixing imperative state updates (`θ.state ← InProgress`) with declarative judgments (`eval(expr, env, Σ_θ) ⇒ θ'`). `Σ_θ` denotes the evaluation context (`EvalContext`) captured at thunk construction time — it carries context-dependent state (base directory, include guards) that must reflect the thunk's definition site. A standard operational semantics would thread an explicit store σ mapping thunk IDs to states: `materialize(θ, σ) ⇒ (v, σ')`. The notation here maps directly to the `materialize()` implementation for ease of cross-checking.

**Depth tracking:** The iterative CEK machine (see §Iterative Evaluator below) has eliminated recursive depth tracking. There is no `MAX_EVAL_DEPTH` check in the core materialization loop — the heap-allocated continuation stack (`Vec<Cont>`) replaces the Rust call stack, bounded only by available memory. Sequence-spine guards in `deep_materialize` use `MAX_COLLECT_SIZE` (1,000,000), a separate constant for preventing unbounded sequence collection.

**[MATERIALIZE-CACHED]**
```
θ.state = Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

**[MATERIALIZE-FAILED]**
```
θ.state = Failed(e)
───────────────────────────
materialize(θ) ⇒ error(e')
```

The materialization span update has three cases (`eval.rs:876-896`): (1) if e has no materialization span and one is available, set it; (2) if the access span matches the existing materialization span, no-op; (3) if the access span differs and is not already in the stack, add it as a stack frame (preserving the original materialization span). This Failed → Failed diagnostic refinement is an intentional relaxation of strict idempotence at the error-representation level — the error's identity and root cause are fixed, but diagnostic annotations accumulate across access paths.

**[MATERIALIZE-CYCLE]**
```
θ.state = InProgress
───────────────────────────
materialize(θ) ⇒ error("circular dependency")
θ.state ← Failed(err)         (memoize the cycle error)
```

**Example — cycle detection in practice:**
```tinct
[x: x]             # circular dependency: x depends on itself
[a: b  b: a]       # circular dependency: a and b depend on each other
```
The cycle error is discovered lazily — only when something tries to access `x` or `a`, not when the dict is defined. The error message includes a cycle trace: `circular dependency detected: a → b → a`.

**Cycle detection recovery strategy:** When a thunk in `InProgress` state is re-encountered during materialization (indicating a circular dependency), the evaluator constructs a `CircularDependency` error, decorates it with the materialization span (if provided), and transitions the thunk to `Failed` state via `cache_failure()` before propagating the error (in `materialize()` InProgress case in `src/eval.rs`). The `InProgress → Failed` transition is permanent — subsequent access to the same thunk returns the cached error without re-detecting the cycle. The error caching happens *before* propagation to ensure that all references to the cyclic thunk see the same error.

**State management after cycle detection:** The thunk is left in `Failed` state, not restored to its original state (`Unevaluated`, `PendingBuiltin`, etc.). This is correct because circular dependencies are semantic errors, not transient resource exhaustion — retrying the same thunk will always produce the same cycle. The cached error may be refined with additional materialization spans as the error propagates through the call stack (via the `Failed → Failed` diagnostic self-edge), but the error identity is fixed.

**Error propagation path:** After transitioning to `Failed`, the error is returned via `Err(err_boxed)`. Callers higher in the materialization stack see the error and propagate it upward. If the same thunk is accessed from a different call site later, the `Failed` case (in `materialize()` in `src/eval.rs`) fires immediately, returning the cached error (potentially with an updated materialization span for the new access site).

**[MATERIALIZE-GUARD]**
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ v
v ∈ τ                                          (validate)
θ.state ← Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

**[MATERIALIZE-GUARD-INNER-ERR]** — inner thunk materialization fails with a cacheable error:
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ error(e)    where e.is_cacheable()
θ.state ← Failed(e)                           (memoize; propagation error, not type mismatch)
───────────────────────────
materialize(θ) ⇒ error(e)
```

**[MATERIALIZE-GUARD-NONCACHEABLE]** — inner thunk materialization fails with a non-cacheable error (e.g., DepthExceeded from a builtin):
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ error(e)               where ¬e.is_cacheable()
θ.state ← Guarded(θ_inner, τ, path, span)     (restore — retry possible)
───────────────────────────
materialize(θ) ⇒ error(e)
```

Note: With the iterative CEK machine, `DepthExceeded` no longer arises from the core materialize/eval loop. It can only be raised by individual builtins (e.g., `MAX_COLLECT_SIZE` in `deep_materialize`). The backward `InProgress → Guarded` edge remains in the code for non-cacheable errors from builtins.

[MATERIALIZE-GUARD-NONCACHEABLE] fires when the inner thunk's materialization fails with a non-cacheable error (e.g., DepthExceeded from a builtin). The Guarded state is restored because non-cacheable errors are transient resource-bound conditions, not semantic errors. (`src/eval_materialize.rs`, in the `ThunkState::Guarded` arm of `force_step()`)

**[MATERIALIZE-GUARD-TYPE-ERR]** — inner thunk succeeds but value does not inhabit the expected type:
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ v
v ∉ τ                                          (validation fails)
e = type_assert_failed(path, τ, typeof(v), span)
θ.state ← Failed(e)                           (memoize type assertion error)
───────────────────────────
materialize(θ) ⇒ error(e)
```

Guarded thunks implement proxy contracts (Findler & Felleisen 2002) for TypeAssert record field validation. The inner thunk is materialized, the result is validated against the expected type τ, and the validated value is memoized. If validation fails, the thunk transitions to Failed with a type assertion error decorated with the field path. Guard memoization ensures each field is validated at most once. Computation errors (inner thunk fails for non-type reasons) propagate directly and are cached; they do not trigger the `default:` fallback — only type assertion failures do. Non-cacheable errors (from builtins) restore the Guarded state instead of transitioning to Failed, since they represent transient resource-bound conditions rather than semantic errors.

**[MATERIALIZE-UNEVALUATED]**
```
θ.state = Unevaluated(expr, env, Σ_θ)
θ.state ← InProgress                          (blackhole)
eval(expr, env, Σ_θ) ⇒ θ'
materialize(θ') ⇒ v
θ.state ← Materialized(v)                     (memoize)
───────────────────────────
materialize(θ) ⇒ v
```

`Σ_θ` is the evaluation context captured at thunk construction time. The thunk evaluates in its captured context, not the current materialization context — this ensures that context-dependent state (base directory, include guards) reflects the thunk's definition site.

**[MATERIALIZE-UNEVALUATED-ERR]**
```
θ.state = Unevaluated(expr, env, Σ_θ)
θ.state ← InProgress
eval(expr, env, Σ_θ) ⇒ θ'
materialize(θ') ⇒ error(e)
θ.state ← Failed(e)                           (memoize error)
───────────────────────────
materialize(θ) ⇒ error(e)
```

**[MATERIALIZE-BUILTIN]**
```
θ.state = PendingBuiltin(f, args, named, cs, Σ_θ)
θ.state ← InProgress
f(args, named, Σ_θ, cs) ⇒ θ'
materialize(θ') ⇒ v
θ.state ← Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

The builtin receives `BuiltinArgs { args, named, call_span, ctx }` — no depth parameter. The iterative CEK machine eliminated depth tracking; builtins that need recursion limits use their own constants (e.g., `MAX_COLLECT_SIZE`).

**[MATERIALIZE-CALL]**
```
θ.state = PendingCall(f_θ, args, named, cs, caller_env, Σ_θ)
θ.state ← InProgress
materialize(f_θ) ⇒ Function(params, body, env)
invoke(params, body, env, args, named, caller_env) ⇒ θ'
materialize(θ') ⇒ v
θ.state ← Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

**[MATERIALIZE-CALL-BUILTIN]**
```
θ.state = PendingCall(f_θ, args, named, cs, caller_env, Σ_θ)
θ.state ← InProgress
materialize(f_θ) ⇒ Builtin(func)
func(args, named, Σ_θ, cs) ⇒ θ'
materialize(θ') ⇒ v
θ.state ← Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

If `materialize(f_θ)` produces a value that is neither Function nor Builtin, the materialization fails with a type mismatch error (in `force_step()` PendingCall case in `src/eval_materialize.rs`), which is cached in Failed state.

Error variants for MATERIALIZE-BUILTIN, MATERIALIZE-CALL, and MATERIALIZE-CALL-BUILTIN follow MATERIALIZE-UNEVALUATED-ERR: on any error, `θ.state ← Failed(e)` before propagation.

**Error decoration:** All errors are decorated via `attach_materialization_context` (in `src/eval.rs`) before caching, adding the materialization span (if not already set) and origin stack frames. The decoration happens in the `map_err(&decorate)` chain before `cache_failure` is called.

**Fast path:** In MATERIALIZE-BUILTIN, MATERIALIZE-CALL, and MATERIALIZE-CALL-BUILTIN, if θ' is already Materialized, skip the recursive `materialize` and extract the value directly. This is observationally equivalent to the general rule — MATERIALIZE-CACHED fires immediately on the recursive `materialize(θ')` — but avoids the function call overhead (in `force_step()` PendingBuiltin and PendingCall cases in `src/eval_materialize.rs`).

**Value::Proxy access dispatch.** Dot access (`$proxy.field`) on a `Value::Proxy` is not part of the thunk lifecycle — it occurs *after* materialization produces a Proxy value. The evaluator dispatches to `invoke_proxy_handler`, which materializes the handler thunk (sharing-preserving via Launchbury memoization) and invokes it with the key. Under the iterative CEK machine, proxy chains do not consume recursion depth — they are processed iteratively via continuation dispatch.

### Part 3: Semantic Properties

Six properties essential for call-by-need soundness (Launchbury 1993, Ariola & Felleisen 1997):

| Property | Status | Qualification |
|----------|--------|---------------|
| **Determinism** | Satisfied | Pure subset only; `$include` introduces external state dependence |
| **Sharing (evaluate-at-most-once)** | Satisfied | Materialized and Failed are semantically terminal — subsequent materializations return cached result (Failed may refine diagnostic metadata) |
| **Monotonicity** | Satisfied with exception | transition graph has no backward edges except `InProgress → Guarded` for non-cacheable errors from builtins (retry semantics); Failed self-edge refines diagnostics only (proven above) |
| **Adequacy** | Holds for extensions | PendingBuiltin/PendingCall are observationally equivalent to Unevaluated (defunctionalization preserves semantics). Guarded is observationally equivalent to an Unevaluated thunk that materializes and validates (proxy contract). Failed extends the codomain from Value⊥ to Value + Error⊥ (absorbing, deterministic) |
| **Confluence** | Pure subset only | `$include` makes evaluation order observable; in the pure subset, materialization order does not affect final values |
| **Sharing preservation** | Satisfied | `Rc<Thunk>` ensures identity-based sharing; the CEK machine preserves thunk identity through continuation dispatch |

### Semantic Commitments

Implicit decisions in the current implementation, made explicit:

**1. Error memoization is permanent.** Once a thunk reaches Failed, it never retries. This includes I/O failures from `$include` — a file-not-found error is cached forever, even if the file appears later. This is correct for a build-time evaluator (deterministic builds) and matches Nix's `nFailed` semantics (Peyton Jones et al. 1999 "imprecise exceptions"). Retryable failures would require a new `Retryable` state or external retry logic.

**2. Confluence holds only in the pure subset.** `$include` introduces evaluation-order dependence: if file A includes file B and file B includes file A, the result depends on which is evaluated first (cycle detection fires on the second). All other tinct operations are confluent — materialization order does not affect the result. The pure subset of tinct (no `$include`) satisfies the diamond property of Ariola & Felleisen's (1997) call-by-need calculus.

**3. No recursive depth limit in core evaluator.** The iterative CEK machine uses a heap-allocated continuation stack (`Vec<Cont>`), eliminating the `MAX_EVAL_DEPTH` bound that existed in the recursive evaluator. There is no depth parameter in `materialize()` or `eval()`. Individual builtins may impose their own limits (e.g., `MAX_COLLECT_SIZE` for sequence collection in `deep_materialize`), but these are domain-specific bounds, not a global recursion limit. (Note: `DepthExceeded` as an `ErrorKind` still exists — see [Errors](10-errors.md) §Error Categories — because individual builtins may raise it for domain-specific resource limits. The eliminated limit is `MAX_EVAL_DEPTH` from the recursive evaluator call stack.)

**4. Finite vs productive thunk lifecycles.** Dict-entry thunks have a **finite lifecycle**: they must eventually reach Materialized or Failed. Seq tail thunks have a **productive lifecycle**: materializing a tail yields a Seq value (containing a new tail thunk) or the terminal `[]`. The state machine is identical; the liveness obligation differs. This distinction is not enforced by the type system — it is a semantic contract between the sequence constructors and the programmer (see §Productivity Obligations).

### Adequacy of PendingBuiltin and PendingCall

These states are defunctionalized continuations (Reynolds 1972). Each is observationally equivalent to an Unevaluated thunk holding an expression that would perform the same computation:

- `PendingBuiltin(f, args, named, cs, Σ_θ)` ≡ `Unevaluated([f ...args ...named], env, Σ_θ)` where env binds the arg thunks
- `PendingCall(f_θ, args, named, cs, caller_env, Σ_θ)` ≡ `Unevaluated([call <materialize f_θ> ...args ...named], env, Σ_θ)`
- `Guarded(θ_inner, τ, path, span)` ≡ `Unevaluated(<materialize θ_inner then validate ∈ τ>, env, Σ_θ)` — a proxy contract monitor (Findler & Felleisen 2002)

The equivalence for PendingCall holds because `eval` of `[call ...]` already performs dynamic dispatch on the callee — if `f_θ` materializes to a Builtin rather than a Function, both the PendingCall path (MATERIALIZE-CALL-BUILTIN) and the hypothetical Unevaluated path would dispatch to the same builtin.

The difference is operational: PendingBuiltin/PendingCall avoid constructing AST nodes for deferred computations. A formal adequacy proof would show bisimulation: every materialization sequence starting with `PendingBuiltin(f, args, ...)` produces the same value as materializing `Unevaluated([f ...args], env)`. This is conjectured based on the defunctionalization correspondence (Reynolds 1972; Danvy & Nielsen 2003) but not mechanically verified.

### Relationship to CEK Machine Migration

The iterative evaluator (§Iterative Evaluator) uses explicit `Cont` variants on the continuation stack to process ThunkState transitions. The CEK machine does not remove PendingBuiltin and PendingCall from ThunkState — these are permanent design elements representing persistent deferred computation:

- **PendingBuiltin** stores deferred builtin calls for lazy sequences (`$map`, `$filter`, `$fold_step`, etc.) and proxy handler dispatch. Cannot be replaced by Unevaluated because builtin function pointers (`BuiltinFn`) have no AST representation. Lazy sequences need persistent storage for deferred steps.
- **PendingCall** stores deferred function calls for lazy dispatch and tail-call optimization. Represents work already done by `eval_call` (evaluated func_expr, wrapped args) that Unevaluated would duplicate.
- The monotonicity proof and semantic properties remain unchanged — the 7-state transition graph (Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed) is the stable design.
- **Sharing preservation is the critical migration invariant**: thunk identity (`Rc<Thunk>` pointer) must be preserved through continuation dispatch. A materialized thunk must be the same allocation that was created at the definition site.
- The iterative CEK machine uses heap-allocated continuations with no hardcoded depth bound

## Error Reporting

Error semantics are specified in [Error Handling](10-errors.md). This section summarizes the key concepts; see doc/10 for formal rules and implementation mappings.

**Dual-span model:** Every error carries a definition site (where the error-producing expression was written) and a materialization site (where a consumer materialized the thunk that failed). The `attach_materialization_context` function decorates errors with these spans during propagation through the `map_err(&decorate)` chain.

**Stack frame accumulation:** When an error propagates through multiple materialization layers (e.g., `θ₁ → θ₂ → θ₃`), each layer adds a stack frame via DECORATE (doc/10 §Part 3). The first materialization site becomes `mat_span`; subsequent sites become stack frames. Deduplication guards prevent redundant frames.

**Error caching:** Cacheable errors (all except `DepthExceeded`) are memoized in `Failed` state via `cache_failure()`. Subsequent access returns the cached error with additional materialization context. Non-cacheable errors (`DepthExceeded` from builtins) restore the thunk to its original state, allowing retry. See MEMO-CACHE and MEMO-SKIP rules in doc/10 §Part 5.

**Error condition specifications:** The trigger conditions for all `ErrorKind` variants (when each error is raised) are documented in [Error Handling](10-errors.md) §Part 2: Error Sources. Propagation rules (PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-CYCLE, PROP-DEPTH) are in doc/10 §Part 4.

## Selective Materialization — Formal Specification

Specifies which arguments each Rust-native builtin materializes before execution and how the result is constructed. This is a two-tier specification: a **strictness signature table** covering the core evaluation and collection builtins (auditable summary), plus **delta rules** for builtins whose materialization behavior cannot be captured by a flat per-argument annotation. I/O, capability, datetime, crypto, and network builtins are omitted as inherently materializing. See `doc/11a-builtins.md` for the full catalog of all 189 registered builtins.

The signature notation draws on Mycroft's (1981) abstract interpretation framework for strictness analysis. The delta rules follow Plotkin's (1981) structural operational semantics, using the same judgment style as §Thunk Lifecycle — Formal Specification.

### Part 1: Strictness Signature Notation

Each builtin receives a per-argument strictness annotation and a result classification:

**Input strictness (per argument position):**

| Symbol | Meaning | Implementation pattern |
|--------|---------|----------------------|
| `S` | Strict — argument is materialized before the builtin executes | `materialize(&args[i], None, ctx)` |
| `L` | Lazy — argument passes through as a thunk; never materialized by this builtin | `Rc::clone(&args[i])` |
| `Sc` | Selectively strict — materialization is conditional on another argument's value; delta rule required | Pattern-match on a previously materialized value to decide |

**Result classification:**

| Symbol | Meaning | Description |
|--------|---------|-------------|
| `→ V` | Value result | Result is a fully computed atomic value (Int, Float, String, Bool) |
| `→ D` | Container result | Result is a Dict or Seq; values within may be thunks from inputs (structural preservation) |
| `→ Θ` | Thunk result | Result is a thunk (Rc::clone of an input, or a new PendingBuiltin/PendingCall) |
| `→ LT` | Lazy-transforming result | Result is a Dict or Seq containing *new* PendingCall/PendingBuiltin thunks wrapping inputs |
| `→ ⊥` | Divergent | Always raises an error; never returns a value |

For dual-dispatch builtins, the result classification refers to the more interesting path (typically Seq). Notes indicate when the Dict path differs.

**Derived property:** A builtin's category (§Stdlib Functions) can be approximated from its signature. These are sufficient conditions, not necessary-and-sufficient — builtins that materialize structure while preserving value thunks (e.g., `$merge`, `$collect`) span categories:
- **Structural** — all args are `L` and result preserves input thunks without new computation
- **Materializing** — all args are `S` and result contains no deferred computation from inputs
- **Lazy-transforming** — result is `→ LT` (contains new PendingCall/PendingBuiltin thunks)
- **Selective** — any arg is `Sc`

**eval_call strictness:** The function expression in `[call ...]` is materialized at the call-site to determine dispatch (Function vs Builtin). Arguments are wrapped as Unevaluated thunks (call-by-need per Launchbury 1993). This is eager function dispatch with lazy arguments.

### Part 2: Strictness Signature Table

All 59 Rust-native builtins. Builtins marked `†` have dual dispatch on Dict/Seq (delta rule required). Builtins marked `‡` have non-trivial materialization patterns (delta rule required).

**Arithmetic** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `+` | `S × S → V` | Materializing |
| `-` | `S × S → V` | Materializing |
| `*` | `S × S → V` | Materializing |
| `/` | `S × S → V` | Materializing |

**Comparison** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `=` | `S × S → V` | Materializing |
| `<` | `S × S → V` | Materializing |

**Control flow:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `if` ‡ | `S × Sc × Sc → Θ` | Selective | Exactly one of args[1]/args[2] is materialized; the other is never touched |

**Dict primitives:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `keys` | `S → D` | Materializing | Materializes arg; returns Dict of key values (all newly constructed) |
| `length` | `S → V` | Materializing | Materializes arg; returns Int count |
| `merge` | `S × S → D` | Materializing | Materializes both dicts for key set; values are Rc::clone (thunks preserved) |
| `append` | `S × L → D` | Materializing | Materializes dict for key computation; value arg passes through as thunk |

**Strings** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `str` | `S* → V` | Materializing (variadic) |
| `split` | `S × S → D` | Materializing |
| `replace` | `S × S × S → V` | Materializing |
| `upper` | `S → V` | Materializing |
| `lower` | `S → V` | Materializing |
| `trim` | `S → V` | Materializing |

**Numeric** (all materializing):

| Builtin | Signature | Category |
|---------|-----------|----------|
| `floor` | `S → V` | Materializing |
| `round` | `S → V` | Materializing |
| `to-int` | `S → V` | Materializing |
| `to-float` | `S → V` | Materializing |

**Evaluation control:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `eval` | `S → V` | Materializing | Deep materialization — recursively materializes all thunks |
| `error` | `S → ⊥` | Materializing | Always raises; never returns |
| `try` ‡ | `S → D` | Materializing | Strict on function arg — materializes before invocation, catches errors |
| `apply` | `S × S → Θ` | Materializing | Materializes both; delegates to function invocation. Result type depends on the applied function |

**Type introspection:**

| Builtin | Signature | Category |
|---------|-----------|----------|
| `type-of` | `S → V` | Materializing |
| `int?` | `S → V` | Materializing |
| `float?` | `S → V` | Materializing |
| `num?` | `S → V` | Materializing |
| `str?` | `S → V` | Materializing |
| `bool?` | `S → V` | Materializing |
| `null?` | `S → V` | Materializing |
| `dict?` | `S → V` | Materializing |
| `fn?` | `S → V` | Materializing |
| `seq?` | `S → V` | Materializing |

**I/O:**

| Builtin | Signature | Category |
|---------|-----------|----------|
| `from-json` | `S → D` | Materializing |
| `include` | `S → D` | Materializing (I/O) |

**Sequence constructors:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `seq` ‡ | `L × L → D` | Structural | Both args pass through as thunks inside the Seq value |
| `range` | `S (× S)? → LT` | Lazy-transforming | Materializes bounds; constructs Seq with PendingBuiltin tail |
| `repeat` | `L → LT` | Lazy-transforming | Arg passes through; PendingBuiltin tail for infinite repetition |
| `cycle` | `S → LT` | Lazy-transforming | Materializes dict; PendingBuiltin step for cycling |
| `iterate` ‡ | `L × L → LT` | Lazy-transforming | Both args pass through; PendingCall + PendingBuiltin for co-recursion |
| `unfold` | `L × L → Θ` | Lazy-transforming | Both args pass through; returns PendingBuiltin thunk |

**Sequence destructors:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `head` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns head thunk (not materialized) |
| `tail` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns tail thunk (not materialized) |
| `collect` ‡ | `S → D` | Structural | Materializes Seq spine (all tails); head thunks pass through into Dict |

**Higher-order collection operations:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `map` † ‡ | `L × S → LT` | Lazy-transforming | Function arg lazy; collection strict for dispatch |
| `filter` † ‡ | `L × S → LT` | Lazy-transforming | Predicate lazy at top level; collection strict for dispatch |
| `take` † | `S × S → LT` | Lazy-transforming | Both strict; Seq result has PendingBuiltin tail |
| `drop` † | `S × S → LT` | Lazy-transforming | Both strict; Seq result via PendingBuiltin step |
| `reduce` † ‡ | `L × L × S → LT` | Lazy-transforming | Function and init lazy; collection strict for dispatch |
| `join` † | `S × S → V` | Materializing | Both strict; materializes all elements for concatenation |
| `concat` † | `S × L → LT` | Lazy-transforming | First arg strict for dispatch; second lazy; Seq path lazy chain, Dict path eager merge |

**Proxy:**

| Builtin | Signature | Category | Notes |
|---------|-----------|----------|-------|
| `proxy` | `L → D` | Structural | Lazy in handler arg; returns Proxy container |

### Part 3: Delta Rules

Delta rules specify the materialization behavior for builtins marked ‡ in the signature table, plus dual-dispatch builtins (†) whose Dict/Seq paths have materially different materialization patterns. Builtins marked † without ‡ (e.g., `$take`, `$drop`) follow the same dual-dispatch pattern as `$map`/`$filter` but with simpler per-path logic — their materialization behavior is fully characterized by the signature.

Rules use the judgment form `δ(f, [θ₁, ..., θₙ], cs) ⇒ r` where f is the builtin, θᵢ are argument thunks, cs is the call span, and r is the result (a thunk or error). All current delta rules use positional args only; named args are empty (`∅`) and omitted from rules for brevity.

**PendingBuiltin construction:** Builtins that defer computation (e.g., `$iterate`, `$map` on Seq, `$filter` step) construct PendingBuiltin thunks that capture the builtin function pointer and argument thunks. These are materialized later by the CEK machine's MATERIALIZE-BUILTIN rule (see §Thunk Lifecycle).

**[DELTA-IF-TRUE]**
```
materialize(θ_cond) ⇒ true
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], cs) ⇒ θ_then
```

**[DELTA-IF-FALSE]**
```
materialize(θ_cond) ⇒ false
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], cs) ⇒ θ_else
```

**Branch isolation guarantee:** The unchosen branch is never materialized. `θ_then` and `θ_else` are returned via `Rc::clone` — no state transition occurs on the unchosen thunk. This is the foundational selective materialization property from which `$and`, `$or`, `$when`, `$unless`, and `$cond` derive their short-circuit behavior (see Part 5). The chosen branch thunk is returned to the caller; its subsequent materialization happens via MATERIALIZE-BUILTIN in §Thunk Lifecycle, which calls `materialize(θ')` on the builtin's result — the separation between "builtin execution" and "result materialization" is what makes `$if`'s laziness guarantee possible.

**[DELTA-SEQ]**
```
───────────────────────────
δ(seq, [θ_head, θ_tail], cs) ⇒ Materialized(Seq(Rc::clone(θ_head), Rc::clone(θ_tail)))
```

No arguments are materialized. Both pass through as thunks within the Seq value. This is the coinductive guard — `$seq` enables corecursive definitions by deferring evaluation of both head and tail.

**[DELTA-HEAD]**
```
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(head, [θ_xs], cs) ⇒ θ_h
```

**[DELTA-TAIL]**
```
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(tail, [θ_xs], cs) ⇒ θ_t
```

DELTA-HEAD and DELTA-TAIL materialize the container to verify it is a Seq, but return the extracted thunk *without materializing it*. The head/tail thunk retains its original state (Unevaluated, PendingCall, etc.). Empty dict `[]` as input produces a specific error (`"head/tail on empty sequence"`).

**[DELTA-COLLECT-EMPTY]**
```
materialize(θ_xs) ⇒ Dict({})
───────────────────────────
δ(collect, [θ_xs], cs) ⇒ Materialized(Dict({}))
```

**[DELTA-COLLECT]**
```
materialize(θ_xs) ⇒ Seq(θ_h₁, θ_t₁)
materialize(θ_t₁) ⇒ Seq(θ_h₂, θ_t₂)
...
materialize(θ_tₙ) ⇒ Dict({})          (terminal)
───────────────────────────
δ(collect, [θ_xs], cs) ⇒ Materialized(Dict({0↦θ_h₁, 1↦θ_h₂, ..., n↦θ_hₙ}))
```

Collect materializes the Seq *spine* (all tail thunks) but head thunks pass through into the result Dict without materializing. This is the key distinction: `$collect` is strict in the structure but lazy in the values.

**[DELTA-ITERATE]**
```
───────────────────────────
δ(iterate, [θ_f, θ_x], cs) ⇒ Materialized(Seq(
    Rc::clone(θ_x),
    PendingBuiltin(iterate, [Rc::clone(θ_f), PendingCall(θ_f, [θ_x])], cs)
))
```

Fully lazy: neither f nor x is materialized. The result Seq's head is x (unchanged thunk), and the tail is a PendingBuiltin that will produce `iterate(f, f(x))` when materialized. The `f(x)` is itself a PendingCall — computation unfolds one step at a time. When the tail PendingBuiltin is materialized, DELTA-ITERATE applies again with `f(x)` as the new seed, enabling corecursive unfolding of the infinite sequence.

**[DELTA-TRY]**
```
materialize(θ_func) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env) ⇒ θ_body
materialize(θ_body) ⇒ v
───────────────────────────
δ(try, [θ_func], cs) ⇒ Materialized(Dict({"ok"↦Materialized(v)}))

materialize(θ_func) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env) ⇒ θ_body
materialize(θ_body) ⇒ error(e)
───────────────────────────
δ(try, [θ_func], cs) ⇒ Materialized(Dict({"err"↦Materialized(e.message)}))
```

`$try` materializes the function argument and invokes it. On success, returns `[ok: value]`; on error, returns `[err: message]`. The error is caught — `$try` itself does not propagate errors (it is the catching boundary). Also handles Builtin callees (dispatches with zero args).

**[DELTA-MAP-DICT]**
```
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
∀i. θ'ᵢ = PendingCall(θ_f, [θᵢ], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], cs) ⇒ Materialized(Dict({k₁↦θ'₁, ..., kₙ↦θ'ₙ}))
```

`θ_f` is never materialized — it is captured by reference (`Rc::clone`) in each PendingCall. No values are computed; the result Dict is O(n) to construct and O(1) per element access.

**[DELTA-MAP-SEQ]**
```
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
θ'_h = PendingCall(θ_f, [θ_h], ∅, cs)
θ'_t = PendingBuiltin(map, [Rc::clone(θ_f), θ_t], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], cs) ⇒ Materialized(Seq(θ'_h, θ'_t))
```

Recursive structure: head is a PendingCall, tail is a PendingBuiltin that will apply DELTA-MAP-DICT or DELTA-MAP-SEQ when materialized.

**[DELTA-FILTER-DICT]**
```
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
θ_step = PendingBuiltin(filter_dict_step, [θ_pred, θ_xs_mat, θ_keys, θ_idx], ∅, cs)
    where θ_xs_mat, θ_keys, θ_idx are pre-computed materialized thunks
───────────────────────────
δ(filter, [θ_pred, θ_xs], cs) ⇒ θ_step
```

The predicate `θ_pred` is not materialized at the top level — it is captured for deferred evaluation in the step function. The step function materializes one element at a time, applies the predicate, and either includes or skips it. Returns a Seq (not a Dict) because filtered keys are unpredictable.

**[DELTA-FILTER-SEQ]**
```
materialize(θ_xs) ⇒ Seq(_, _)
θ_step = PendingBuiltin(filter_seq_step, [θ_pred, θ_xs], cs)
───────────────────────────
δ(filter, [θ_pred, θ_xs], cs) ⇒ θ_step
```

The step function receives the *original seq thunk* (not destructured head/tail) and materializes it internally to obtain head and tail. This avoids redundant materialization since the dispatch already materialized the collection. Lazy filter on sequences: the step function materializes head, applies predicate, and either includes it (Seq node) or skips it (recurse on tail). Elements are tested only when the result Seq is consumed.

**[DELTA-REDUCE-DICT]**
```
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
acc₀ = θ_init
∀i. accᵢ = PendingCall(θ_f, [accᵢ₋₁, θᵢ], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], cs) ⇒ accₙ
```

Builds a chain of PendingCall thunks without materializing any values. The entire reduction is deferred — nothing computes until the result thunk is materialized. At that point, the chain unwinds from the inside out.

**[DELTA-REDUCE-SEQ]**
```
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
θ_step = PendingBuiltin(reduce_seq_step, [θ_f, θ_init, θ_h, θ_t], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], cs) ⇒ θ_step
```

Seq reduction uses a step function that materializes the tail to check for termination, then recurses. Unlike Dict reduction, Seq reduction is incremental (processes one element per step function invocation).

### Part 4: Dual-Dispatch Pattern

Six builtins (`map`, `filter`, `take`, `drop`, `reduce`, `join`) dispatch on the runtime type of their collection argument:

```
materialize(θ_xs) ⇒ v
    v = Dict(...)  →  apply Dict-specific rule
    v = Seq(...)   →  apply Seq-specific rule
    otherwise      →  type error
```

This dispatch materializes the collection argument to determine its type, then applies the appropriate delta rule. The function/predicate argument (if present) is *not* materialized at dispatch time — it is captured by reference for deferred application.

**Result type asymmetry:** The Dict and Seq paths of a dual-dispatch builtin may produce different result types. For example, `$filter` on a Dict returns a Seq (not a Dict), because filtered keys are unpredictable — the output keys are unknown without evaluating predicates. The signature table (Part 2) captures the Seq-path result. Dict-path results: `map` returns Dict, `filter` returns Seq, `take`/`drop` return Dict, `reduce`/`fold` return the accumulator type, `join` returns String.

**In the iterative evaluator,** dual dispatch is a `Cont::CollectionDispatch` continuation that materializes the collection, inspects its type, and pushes the appropriate next continuation. The function argument must be preserved on the continuation stack without materializing.

### Part 5: Derived Selectivity

Standard library functions defined in `stdlib/prelude.llt` inherit their materialization behavior from the builtins they invoke. Key derived selectivity properties:

| Function | Definition | Inherited behavior |
|----------|------------|-------------------|
| `not` | `[fn [x] [if x false true]]` | Materializing — materializes x via `if`'s condition position |
| `and` | `[fn [a b] [if a b false]]` | Selective — materializes a; b materialized only if a is true |
| `or` | `[fn [a b] [if a a b]]` | Selective — materializes a; b materialized only if a is false; returns a if truthy |
| `when` | `[fn [pred body] [if pred body []]]` | Selective — materializes pred; body materialized only if pred is true |
| `unless` | `[fn [pred body] [if pred [] body]]` | Selective — materializes pred; body materialized only if pred is false |
| `cond` | Recursive via `cond-impl` → `cond-check` → `if` | Selective — materializes conditions left-to-right via nested `if`; first matching branch returned as thunk |
| `assert` | `[fn [cond msg] [if cond true [error msg]]]` | Selective — materializes cond; error raised only if cond is false |

**Inheritance proof sketch:** Each derived function's selectivity follows by inlining its definition and applying DELTA-IF-TRUE/DELTA-IF-FALSE. For `$and`:

```
and(θ_a, θ_b)
  = if(θ_a, θ_b, false)
  DELTA-IF-TRUE:  materialize(θ_a) ⇒ true  → θ_b    (b is materialized only when the caller materializes the result)
  DELTA-IF-FALSE: materialize(θ_a) ⇒ false → false   (b is never touched)
```

This compositional guarantee means that making `$if` lazier (see §Laziness Design) automatically improves all derived control flow functions without code changes.

### Part 6: Properties and Guarantees

**Branch isolation (fundamental guarantee):**

For any builtin with `Sc` positions, the unchosen arguments are never materialized, never transition state, and never appear in error traces. Formally: if `δ(if, [θ_c, θ_t, θ_e], cs) ⇒ θ_t`, then θ_e's `ThunkState` is unchanged after the call.

**No unnecessary materialization (structural guarantee):**

Builtins classified as Structural or Lazy-transforming materialize only the minimum arguments needed to determine the result structure. Value thunks within input collections pass through to output collections without materializing. This is verifiable by inspection: every `Rc::clone(&args[i])` or `Rc::clone(value_thunk)` preserves the thunk's state.

**Sharing preservation:**

All delta rules preserve thunk identity. When a thunk appears in both the input and output of a builtin (e.g., `$head` extracting a Seq's head), the same `Rc<Thunk>` allocation is shared — not copied. Subsequent materialization of the output thunk memoizes the value for all holders of that `Rc`.

**Strictness monotonicity:**

The signature table is monotonic with respect to the implementation: a builtin marked `L` at position i will never call `materialize()` on `args[i]`. A change that adds a `materialize()` call on a position marked `L` is a breaking change to the laziness contract and must update the signature table.

**Dual-dispatch consistency:**

For dual-dispatch builtins, the Dict and Seq paths must agree on which non-collection arguments are materialized. For example, `$map`'s Dict path and Seq path both leave `θ_f` unmaterialized — if one path started materializing `θ_f`, it would break laziness for programs that pass expensive computations as the function argument.

## Laziness Design

### Sequential Expressions in Function Bodies

When a function body contains multiple expressions, the parser wraps them in `Expr::Sequential`. This construct enables intermediate bindings within a function while maintaining lazy evaluation semantics:

```tinct
[fn [x]
    [y: [* x 2]]        # intermediate binding
    [z: [+ y 1]]        # another intermediate binding
    [result: [str y " and " z]]  # final result
]
```

**Semantics:**

- **Environment extension:** Each intermediate expression (if it's a dict) adds its bindings to the environment for subsequent expressions
- **Lazy intermediate bindings:** Intermediate dict values remain as unevaluated thunks — they are only materialized when accessed
- **Result is final expression:** The value of the last expression in the sequence is the function's return value
- **CEK machine routing:** The CEK machine routes `Sequential` to `eval_recursive` via the `eval_materialize.rs` fallback (continuations like `SeqRest` manage the sequence)

This is identical to how document-level expression sequences work (see [Documents](09-documents.md) §Scope Chain Semantics), but scoped within a single function body rather than across documents.

**Grammar:** The `fn_form` rule in `doc/02-syntax.md` §Complete Grammar uses `value+` to permit multiple body expressions. The parser automatically wraps `value+` in `Expr::Sequential` when more than one expression is present.

### Strictness Exceptions

Tinct's evaluation model is lazy by default — values remain unevaluated until accessed. Four intentional exceptions deviate from this default by triggering materialization at construction time rather than access time:

1. **TypeAssert validation via continuation:** `[@Type expr]` evaluates `expr` and schedules validation via `Cont::TypeAssertCheck` continuation. For structural types (record shapes), validation is deferred via `Guarded` thunks that check field types lazily at first access. For primitive types, validation is immediate. This ensures type errors are caught at annotation sites (for primitives) or field access sites (for records), providing clear error reporting. See [Type System Extensions](07-type-extensions.md) §TypeAssert Runtime Validation.

2. **reduce eager iteration (Seq path only):** `$reduce` (and `$fold`) on Seq inputs materialize each accumulator step to prevent O(N) Rust stack depth from nested PendingCall thunks. The accumulator chain is still lazy (each step is a PendingCall thunk), but the Seq iteration itself materializes tails at each step to detect sequence end without building deep call chains. (Dict path: fully lazy PendingCall chain — see §Laziness Design table below.)

3. **Guarded default fallback:** When a guard fails and a `default:` value is provided, the default is evaluated and materialized immediately. This prevents deferred errors from propagating when the guard explicitly signals a fallback path should be taken.

4. **Sequential expression scope chain (SEQ-SCOPE):** Named bindings from intermediate expressions in a multi-expression document have their keys extracted eagerly (for scope chain construction), but values remain lazy thunks. Only the dict structure (keys) must be known to create the scope chain — values are forced on demand when accessed. See [Documents & Pipelines](09-documents.md) §Scope Chain Semantics for the formal specification.

This table documents the laziness behavior of every operation and the rationale for each decision.

| Operation | Behavior | Rationale |
|-----------|----------|-----------|
| **Control Flow** | | |
| `$if` | Returns branch thunk directly (no materialization of branch) | The chosen branch stays lazy until accessed by caller |
| `$and` | Materializes first arg; second only if first is true (short-circuit via `$if`) | Short-circuit via `[fn [a b] [if a b false]]` |
| `$or` | Materializes first arg; second only if first is false (short-circuit via `$if`) | Short-circuit via `[fn [a b] [if a a b]]`; returns a if truthy |
| `$not` | Materializes argument | Must inspect value |
| `$when`, `$unless` | Materializes condition; body returned as thunk | Body returned lazy via `$if` |
| `$cond` | Materializes conditions left-to-right; first matching branch returned as thunk | Delegates to `$if`; no code change needed |
| **Dict Operations** | | |
| `$merge` | Eagerly materializes both dicts; values pass through as thunks (Rc::clone) | See §Merge — Lazy Overlay Compatibility in doc/11-stdlib.md for the lazy overlay design |
| `$get`, `$get-or` | Returns value thunk (structural) | Already lazy |
| `$keys` | Keys always evaluated | Keys are never thunks |
| `$values` | Returns list of thunks | Already lazy |
| `$entries` | Returns list of entry dicts (values stay as thunks) | Already lazy |
| `$set`, `$remove` | Values stay as thunks | Already lazy on values |
| `$update` | Calls `$set` → `$merge` (eager materialization) | Wrapper around $set → $merge; same semantics as $merge |
| `$has?` | Wraps `$try` around access (structural) | Already optimal |
| `$get-in`, `$get-in-or` | Materializes each step of path | Must traverse nested dicts |
| `$length` | Materializes dict to count entries | Must count entries |
| `$empty?` | Calls `$length` then compares to 0 | Depends on `$length` |
| **Universal Collection Ops** | | |
| `$map` on dict | Returns dict with PendingCall thunks, O(n) construct / O(1) per access | Enables lazy dict transforms |
| `$map` on seq | Returns seq applying function to each element (lazy) | Enables infinite sequence transforms |
| `$filter` on dict | Returns Seq (must evaluate predicates) | Predicates must run to know which keys to keep |
| `$filter` on seq | Returns seq filtering elements (lazy) | Lazy sequence filtering |
| `$reduce`, `$fold` on dict | Builds lazy PendingCall chain (acc₀=init, acc₁=PendingCall(f,[acc₀,v₀]), ...) | Fully lazy — no materialization during chain construction |
| `$reduce`, `$fold` on seq | Builds lazy PendingCall accumulator chain; materializes tail at each step for Seq path only | Must check tail to detect sequence end, but accumulator builds same lazy chain as Dict path |
| `$map-entries` | Returns dict with PendingCall thunks on transformed entries | Same as `$map` on dicts |
| `$from-entries` | Eagerly reduces entry pairs into dict | Must construct concrete dict |
| `$any?`, `$all?` | Short-circuit: materializes elements until condition met/failed | Predicates must run |
| `$until` | Iterates until predicate holds | Must evaluate predicate each step |
| `$find-deep` | Materializes while searching | Must traverse structure |
| `$flatten` | Traverses and rebuilds | Must inspect values to check if list |
| `$zip` | Lazy Seq for sequences; eager for dicts | Seq zip is lazy, dict zip is materializing |
| **List Operations** | | |
| `$first` | Returns first value thunk (structural) | Already lazy |
| `$nth`, `$last` | Returns value thunk by position (structural) | Already lazy |
| `$rest` | Returns Seq tail for sequences; O(1) | Seq `rest` is O(1) vs O(n) dict clone |
| `$cons` | Returns Seq cons for sequences; O(1) | Seq `cons` is O(1) vs O(n) dict clone |
| `$conj` | Materializes + clones dict O(n), inserts new entry O(1) | O(n) clone + O(1) insert |
| `$concat` | Returns Seq concat for sequences | Seq concat is lazy, dict concat is eager |
| `$reverse` | Builds reversed dict | Must know all entries to reverse |
| `$reindex` | Rebuilds with dense 0..n keys | Must traverse all entries |
| `$sort`, `$sort-by` | Materializes all values to compare | Must compare all values to sort |
| `$take` | Dual-dispatch: Dict preserves keys, Seq returns finite Seq (O(1)) | Seq `take` is O(1), dict `take` is structural |
| `$drop` | Returns lazy Seq for sequences | Seq `drop` is O(1), dict `drop` is structural |
| `$slice` | Positional slice; preserves thunks | Already lazy on values |
| **Sequences** | | |
| `$range` | Lazy Seq, O(1) construction; 1-arg infinite, 2-arg finite | Enables infinite ranges |
| `$repeat` | Lazy infinite Seq, O(1) construction | Enables infinite repetition |
| `$cycle` | Lazy infinite Seq, O(1) construction | Enables infinite cycling |
| `$iterate` | Lazy infinite Seq: `x, f(x), f(f(x)), ...` | New lazy sequence constructor |
| `$unfold` | Lazy Seq from step function | New lazy sequence constructor |
| `$seq` | Low-level Seq constructor (cons cell); both args pass through as thunks | Rust builtin for Seq construction |
| `$collect` | Materializes Seq spine; head thunks pass through into dict | Seq → Dict boundary |
| `$head` | Materializes container to verify Seq; returns head thunk (not materialized) | Structural Seq operation |
| `$tail` | Materializes container to verify Seq; returns tail thunk (not materialized) | Structural Seq operation |
| **Type Predicates** | | |
| `$int?` | Materializes argument; returns Bool | Type introspection |
| `$float?` | Materializes argument; returns Bool | Type introspection |
| `$num?` | Materializes argument; returns Bool | Type introspection |
| `$str?` | Materializes argument; returns Bool | Type introspection |
| `$bool?` | Materializes argument; returns Bool | Type introspection |
| `$null?` | Materializes argument; returns Bool | Type introspection |
| `$dict?` | Materializes argument; returns Bool | Type introspection |
| `$fn?` | Materializes argument; returns Bool | Type introspection |
| `$seq?` | Materializes argument; returns Bool | Type introspection |
| **Arithmetic & Comparison** | | |
| `$+`, `$-`, `$*`, `$/` | Materializes both operands | Must inspect numeric values |
| `$quot`, `$mod` | Materializes both operands | Depends on arithmetic |
| `$=`, `$<`, `$>`, `$<=`, `$>=` | Materializes both operands | Must compare values |
| `$to-int`, `$to-float` | Materializes argument | Must parse/convert value |
| `$floor`, `$ceil`, `$round`, `$trunc` | Materializes argument | Must inspect numeric value |
| **Strings** | | |
| `$str` | Materializes all arguments | Must concatenate string content |
| `$split`, `$replace`, `$upper`, `$lower`, `$trim` | Materializes argument | Must inspect string content |
| `$join` | Materializes separator + all list elements | Must concatenate all strings |
| `$words` | Materializes string, filters empty | Depends on `$split` |
| **Composition** | | |
| `$apply` | Materializes function + arg dict; splits by key type; invokes. Result laziness depends on applied function | Materializing |
| `$identity` | Returns argument thunk (structural) | Already lazy |
| `$compose` | Returns function thunk | Functions are always thunks |
| `$->` (threading) | Threads thunk through functions (structural) | Already lazy |
| **Runtime & Introspection** | | |
| `$eval` | Deep-materializes all thunks recursively | Explicit materialization primitive |
| `$type-of` | Materializes argument to inspect type | Must know runtime type |
| `$error` | Constructs error value (structural) | Structural |
| `$try`, `$try-or` | Materializes body, catches exceptions | Must run body to catch errors |
| `$assert` | Materializes condition | Must check condition |
| `$from-json` | Materializes JSON string, parses | Must parse entire JSON |
| `$include` | Evaluates file; returns cached thunk on re-include | Include memoization |
| **Document Pipeline** | | |
| `%` (document pipeline) | Bound as `Unevaluated` thunk across `---` boundary | `---` is not a materialization point — laziness is preserved across documents |
| Document scope chain (`eval_document`) | Named binding keys extracted eagerly; values remain lazy thunks | Scope chain construction requires knowing dict keys, but values are inserted as lazy thunks and forced only on access. Dead bindings remain unevaluated. (`eval.rs:682-692`) |
| **Internal (eval.rs)** | | |
| `eval_key` (dict construction) | Materializes all dict keys | Keys must be known for dict insertion |
| `builtin_keys` | Materializes dict | Keys are never thunks |
| `TypeAssert` body (`[@Type expr]`) | Shape checked immediately (required keys present, cardinality for closed records); field type validation via Guarded thunks — each field's type constraint is checked lazily at first access | Known partial strictness: shape check cannot be deferred, but individual field types are validated lazily via `Cont::TypeAssertCheck` continuation. See [Type System Extensions](07-type-extensions.md) §TypeAssert Runtime Validation. |

**Force-side-effect idiom.** Tinct has no `!` or `seq` operator. To force a side-effectful binding before returning a result, use the equality-check pattern:

```tinct
[w: [side-effect]]
[if [= w w] result result]
```

This forces `w` via the equality check (which materializes both operands), then returns `result`. The `_` identifier cannot be used for this purpose because `_` triggers implicit lambda desugaring — `[fn [_] result]` — rather than being a discard.

**Error reporting impact:** Operations that shift from eager to lazy (e.g., `$if`, `$merge`, `$map`) will report errors at access time rather than construction time. This provides more accurate source locations (pointing to where materialization failed) but changes error timing. Inherently materializing operations continue to produce errors at call time.

---

## Deep Materialization — Implementation

The `$eval` builtin and CLI `--eval` flag use `deep_materialize` to recursively materialize all thunks in a value tree. This is distinct from selective materialization (which materializes only what's needed for computation) — deep materialization materializes *everything*, producing a fully-evaluated value tree suitable for serialization or comparison.

**Cache data structure:** `deep_materialize` uses a stack-local `HashMap<*const Thunk, Option<Rc<Thunk>>>` created at the `deep_materialize` entry point and passed through the recursion. The cache has a dual-purpose design (in `deep_materialize_impl`):

| Cache entry | Meaning | Purpose |
|-------------|---------|---------|
| `None` | Blackhole sentinel — thunk is currently being deep-materialized | Cycle detection (Launchbury 1993 blackholing) |
| `Some(Rc<Thunk>)` | Cached result — thunk was already deep-materialized | Sharing preservation (reuse the same `Rc<Thunk>` allocation) |

**Cache lifecycle:** The cache is created per `deep_materialize` call and dropped on return. It is *not* shared across multiple top-level `deep_materialize` invocations — each call to `$eval` or CLI evaluation creates a fresh cache. The cache is global *within* a single call: all branches of a nested dict or sequence tree share the same cache instance.

**Cycle handling:** When a thunk pointer is encountered for the second time within the same deep materialization (cache entry is `None`), the function returns `Rc::clone(thunk)` — the original thunk, not materialized (in `deep_materialize_thunk`). This prevents infinite recursion on cyclic structures (e.g., `[x: x]` or mutual dict references). The cycle is detected at the *structure* level (same thunk pointer seen twice during traversal), not the *value* level (the thunk's own `InProgress` sentinel, which detects cycles within a single thunk's evaluation).

**Sharing preservation:** When a thunk appears multiple times in the input value tree (e.g., `let shared = [expensive: [f]] in [a: shared  b: shared]`), the cache ensures the deep-materialized result is a *single* `Rc<Thunk>` shared by all references. Without the cache, `deep_materialize` would create independent copies for each occurrence, breaking `Rc::ptr_eq` and wasting memory. The `Rc::ptr_eq` invariant holds only within one `deep_materialize` call; two separate calls on overlapping trees produce distinct output pointers.

**Cache cleanup on error:** If `materialize()` or recursive `deep_materialize_impl()` fails, the cache entry is removed before propagating the error (in `deep_materialize_impl`). This prevents cache poisoning: a failed thunk leaves no stale `None` sentinel that would cause subsequent encounters to incorrectly return an unevaluated thunk.

**Growth characteristics:** For large dicts, the `HashMap` grows monotonically as new thunks are encountered. The cache never shrinks during traversal — it accumulates all seen thunk pointers until the entire `deep_materialize` call completes. For a dict with 10,000 entries containing shared sub-dicts, the cache may hold thousands of entries. This is acceptable because (a) the cache lifetime is bounded by the single `deep_materialize` call, not the session, and (b) the alternative (no cache) would traverse shared structures multiple times, defeating sharing.

**Comparison to selective materialization:** Regular `materialize()` has no visited set — it materializes a single thunk and memoizes the result in `ThunkState::Materialized`. Cyclic dependencies are caught by the `InProgress` sentinel *within* the thunk, not by a global traversal cache. `deep_materialize` adds a *second* layer of cycle detection at the structural level (pointer identity across the value tree) via a `HashMap<*const Thunk, Option<Rc<Thunk>>>` dual-purpose cache, orthogonal to the per-thunk `InProgress` cycle detection.

**Relationship to Nix:** Nix's `forceValueDeep` (eval.cc:2264) uses a similar `std::set<const Value *> seen` for pointer-identity cycle detection. The key difference: Nix's set is visit-tracking only (all entries are pointers, not `Option<ptr>`), because Nix uses a conservative GC and doesn't need explicit sharing preservation — shared `Value*` pointers are naturally deduplicated. Tinct's `Option<Rc<Thunk>>` design combines visit-tracking (`None`) with result caching (`Some(rc)`) in a single structure.

**Allocation strategy:** The runtime uses two complementary strategies: backward-compatible optimizations to the current `Rc<Thunk>` + `IndexMap<String, Rc<Thunk>>` runtime, and arena-based allocation with flat environments for deeper efficiency gains.

**Current allocation profile:**

| Component | Representation | Cost |
|-----------|---------------|------|
| Thunks | `Rc<Thunk>` with `RefCell<ThunkState>` | Individual heap alloc per thunk, triple indirection |
| Environments | `Rc<RefCell<Environment>>` with `IndexMap<String, Rc<Thunk>>` + parent chain | O(depth) variable lookup |
| Dict keys | `Key::String(String)` | Cloned 2× per dict entry (env bindings + dict_map) |
| Thunk origin | `origin: Cow<'static, str>` | Zero-cost for empty/static origins (`Cow::Borrowed`); allocates only for dynamic labels |
| Type inference sets | `HashSet<String>` in `collect_type_vars`, `collect_row_vars`, `collect_all_vars`, `instantiate_scheme`, `instantiate_at_level`, `generalize` | Transient per-call allocations in `src/types.rs`; each call allocates a fresh `HashSet`, collects variable names via tree traversal, then drops the set. Hot paths during type inference — `instantiate_scheme` is called per polymorphic variable reference, `generalize` per dict entry at Pass 4. Elimination: flat environments with de Bruijn indices remove the need for name-based variable collection entirely. Mitigation: pre-sized `HashSet::with_capacity` based on scheme quantifier count, or `SmallVec`-backed collection for schemes with few variables (the common case). |

**Backward-compatible optimizations.** Baseline: ~113 `Rc::new(Thunk)` calls in eval.rs, ~142 `IndexMap::new()` calls in builtins.rs. Expected impact: 75-85% of addressable allocation cost.

- **Dict literal fast-path** (Nix `maybeThunk`): In `eval_dict`, when `entry.value.node` is `Int|Float|Bool|Str`, create `Materialized` thunks directly instead of wrapping in `Unevaluated`. Eliminates ~40-60% of thunk allocations for config-heavy files. Safe because literals are side-effect-free, deterministic, and don't participate in letrec cycles.
- **String interning**: `HashSet<Rc<str>>` with `Borrow<str>` lookup (avoids key duplication of `HashMap<String, Rc<str>>`). Interns *structural identifiers only* — `Key::String`, variable names, builtin names, and thunk origins. Does NOT intern user data strings (may be large and unique). Reduces key cloning to `Rc::clone` and enables O(1) pointer-equality comparison. Scoped to evaluation session lifetime (lives in `EvalContext`, cleared per `eval_file()`). Production alternative: `lasso::Rodeo` for zero-copy Spur handles.
- **Key cloning reduction**: Eliminate the 2× `String` clone per dict entry in `eval_dict` (once into `dict_env` bindings, once into `dict_map`). Use `entry_mut()` pattern or restructure insert order. ~30% of dict allocation cost.
- **AST cloning reduction**: Change `CallExpr` args from `Spanned<Expr>` to `Rc<Spanned<Expr>>` so `eval_call` can `Rc::clone` instead of deep-cloning entire AST subtrees per argument. ~20-40% of call overhead. Internal refactor to ast.rs and parser.rs; backward-compatible at the public API level.
- **func_label allocation reduction**: `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for the common VarRef case (most calls). Only allocate for DotAccess labels. ~5-10% of call overhead.
- **Capacity hints**: `IndexMap::with_capacity(entries.len())` on all dict construction paths (`eval_dict`, `builtin_drop` Dict path, `builtin_split`).
- **SmallVec**: `SmallVec<[Rc<Thunk>; 4]>` for call args (most calls have ≤4 args), `SmallVec<[StackFrame; 8]>` for error stacks.
- **Origin optimization**: `origin: String` → `Rc<str>` via string interner, with static empty sentinel for the common case.

**Arena allocation (current implementation).** The runtime uses `ThunkArena` with `ThunkId` handles for all thunk storage. This is the "arena-backed registry" approach:
- `ThunkArena` exists in `EvalContext` with `RefCell` interior mutability
- `Value` variants use `ThunkId` handles: `Dict(IndexMap<Key, ThunkId>)`, `Seq { head: ThunkId, tail: ThunkId }`, `Overlay(ThunkId, ThunkId)`
- Allocation goes through `ctx.alloc_thunk(Thunk)` which wraps in `Rc<Thunk>` and stores in arena `Vec<Rc<Thunk>>`
- Arena persists across `---` boundaries (append-only, no per-section deallocation)
- **No migration needed**: ThunkIds are stable indices that never invalidate; `$include` cache stores standalone `Rc<Thunk>` (arena-independent)

**Full arena-based allocation for per-section lifetimes.** Further optimization enables per-section lifetimes:
- **Arena allocator**: Replace `Rc::new(Thunk)` call sites with `arena.alloc(Thunk)`. Arena stores `Vec<Thunk>` (direct ownership, not Rc-wrapped). Recommended approach: index-based arena (`Vec<Thunk>` + `ThunkId` newtype over `usize`) for stable references, bounds-checked indexing, and safe letrec (allocate `ThunkId` slots, fill later, no UB).
- **Flat environments with slot indices**: Replace `IndexMap<String, Rc<Thunk>>` chain with flat `Vec` arrays indexed by compile-time (level, slot) pairs (de Bruijn levels). Variable lookup becomes O(1). Environment reuse in function calls becomes trivially safe (each call writes to its own activation frame).
- **Variable resolution pass**: Pre-eval pass assigns (level, slot) indices to every `VarRef`. This pass also enables TCO detection.

**Arena lifetime and persistent values:** The arena lifetime is **one document section** — the text between `---` boundaries (or the entire file for single-section documents). At each `---` boundary, values reachable from the section result are **selectively migrated** from the arena to `Rc`-backed persistent storage, bound as `%` for the next section, and the section's arena is dropped.

**Selective migration** is a scoped copying pass that preserves thunk state — it translates storage, not evaluation state. Unevaluated thunks stay unevaluated (lazy), Materialized thunks keep their cached values, closures retain their environment chains. The `---` boundary is **not** a strictness point. This preserves the existing lazy pipeline semantics (§Scope Chain Semantics, DOC-PIPELINE): the `---` boundary does not trigger materialization.

The migration algorithm traces from `%` (the section result) and rewrites arena handles to `Rc`-backed storage:

```
migrate(value, arena, thunk_table, env_table) → Rc<Thunk>:
  for each ThunkId in value:
    if thunk_table[id] exists:     return thunk_table[id]  (preserves sharing)
    thunk = arena[id]
    rc = Rc::new(Thunk::placeholder())       (allocate before recursing)
    thunk_table[id] = rc                     (insert before recursing — breaks cycles)
    rc.fill(match thunk.state:
      Materialized(v)            → Materialized(migrate_value(v, arena, thunk_table, env_table))
      Unevaluated(expr, env)     → Unevaluated(expr, migrate_env(env, arena, thunk_table, env_table))
      PendingBuiltin(f, args, …) → PendingBuiltin(f, migrate_args(args, …), …)
      PendingCall(f_θ, args, …)  → PendingCall(migrate(f_θ, …), migrate_args(…), …)
      Failed(e)                  → Failed(e)
      InProgress                 → unreachable at --- boundary
    )
  return rc
```

Two-phase allocation: `Rc::new(placeholder())` is inserted into the table *before* recursing into the thunk's state. This is the standard graph-copying pattern for structures with cycles — letrec environments contain mutual references, so the table entry must exist before `migrate_env` encounters the same ThunkId transitively. The placeholder is filled via `RefCell` after the recursive migration completes. This matches how `deep_materialize` inserts into its visited set before recursing.

**Two translation tables** preserve identity across the migration boundary:

- `thunk_table: HashMap<ThunkId, Rc<Thunk>>` — ensures two references to the same arena thunk map to the same `Rc<Thunk>`.
- `env_table: HashMap<EnvId, Rc<RefCell<Environment>>>` — ensures two closures capturing the same arena environment share the same migrated environment. Without this, letrec groups that share an environment would become independent copies, breaking the sharing invariant.

AST nodes (`Rc<Spanned<Expr>>`) are reference-counted and arena-independent — they are shared, not copied. The builtins environment (root of every parent chain) is always `Rc`-backed and never arena-allocated — it is the base case that terminates `migrate_env` recursion.

Within a section, all thunks are arena-allocated and lazy. Letrec entries reference each other freely within the arena. At `---`, only thunks reachable from `%` are migrated — unreachable intermediate thunks (temporaries, shadowed bindings) are reclaimed when the arena drops.

**What migrates correctly:**

| Value type | Migration behavior |
|------------|-------------------|
| Primitives (Int, Str, Bool, …) | Copied directly (no arena handles) |
| Dict entries | Each thunk migrated; sharing preserved via table |
| Functions/closures | Captured environment chain migrated recursively |
| Infinite Seq | Only the cons cell is migrated; lazy tail stays lazy |
| `$include` results | Already Rc-backed (include cache outlives sections) |

Per execution context:

| Context | Arena lifetime | Cross-boundary value | Notes |
|---------|---------------|---------------------|-------|
| CLI (single section) | Entire eval | None | One arena, dropped at end. No migration. |
| CLI (multi-section) | Per section | `%` (selectively migrated) | Arena per section, migrate at `---` |
| REPL | Per input | `%` (selectively migrated) | Each input is implicitly a section |
| LSP | Per section | `%` (selectively migrated) | Editing section N re-evaluates N+ with cached `%` from N-1 |

**Cost model:** Migration is O(thunks reachable from `%`), not O(total section thunks). For sections where `%` is a small result derived from large intermediate computations, migration cost is much lower than deep-materialization. For sections where most thunks are reachable from `%`, cost approaches deep-materialization minus the materialization cost (migration copies state; deep-materialization evaluates).

**Rejected alternatives:** (1) Session-scoped arena — unbounded memory growth during long REPL sessions; requires stop-the-world compaction with pointer fixup across all live references. (2) Hybrid arena+Rc — two allocation paths; every thunk creation must decide arena vs Rc; closures capturing thunks make escape analysis intractable. (3) Deep-materialization at `---` — changes language semantics (lazy→eager), breaks closures (env chains hold dangling arena handles after drop), and diverges on infinite sequences in `%`. (4) Per-eval copy-out without section granularity — triggers materialization of intermediate values within a section, losing laziness benefits.

**LSP incremental re-evaluation:** Migrated `%` values are self-contained `Rc`-backed storage with no arena references. The LSP caches `%` per section. Editing section N re-uses cached `%` from section N-1 (already migrated, no re-evaluation) and re-evaluates only sections N through the end.

**`$include` interaction:** Included files are evaluated in their own arena. The include cache stores migrated results — the cache outlives any single section's arena. An `$include` call returns an already-migrated `Rc`-backed value, which is arena-independent and can be used freely across sections. This creates a controlled one-way dependency within sections: arena-allocated thunks may reference `Rc`-backed `$include` results, but never the reverse. This is structurally determined (section-local = arena, imported = Rc) and does not require per-thunk escape analysis — the "hybrid arena+Rc" alternative (rejected above) fails because it requires per-thunk decisions, not because mixing storage backends is inherently unsound.

**Rationale:** The iterative evaluator shares prerequisites with arena allocation — both require explicit frame management and compile-time analysis. Bundling avoids two separate invasive refactors. Backward-compatible optimizations capture 75-85% of addressable allocation wins with near-zero risk. Profiling data from those optimizations guides whether the full arena is necessary.

**Measurement plan:** Establish baseline metrics before and after optimization: total allocations per eval (count `Rc::new`, `IndexMap::new`, `Vec::new`), peak memory usage (heaptrack RSS on dict-heavy and deeply-nested workloads), and allocation hotspots (which paths account for >10% of allocations). Decision threshold for full arena: if backward-compatible optimizations achieve >80% allocation reduction, the arena migration can be deferred indefinitely; if <50%, proceed.

**Key tradeoff:** Environment lookup stays O(depth) until arena evaluation with flat environments, but string interning makes each lookup step cheaper (pointer comparison vs byte comparison), and the literal fast-path reduces total thunk allocations significantly.

**Precedent:** Nix uses flat `Value*[]` arrays with de Bruijn levels and Boehm GC. Jsonnet uses GC heap with flat bindings. Nickel uses `Rc<RefCell<Closure>>` (same as Tinct's current approach). Backward-compatible optimizations keep Tinct at Nickel's level; arena evaluation moves toward Nix's level.

**Constraint:** The arena model must handle letrec self-reference safely in Rust (thunk slots allocated before fill, no dangling pointers). The safe Rust arena patterns are analyzed in `doc/whatif/arena-patterns.md` — the recommended approach is an index-based arena (`Vec<Thunk>` + `ThunkId` handles), following the cranelift entity pattern.

## Iterative Evaluator (CEK Machine)

**Decision:** Replace the recursive `eval()` / `materialize()` call stack with an iterative CEK machine (Control-Environment-Kontinuation). Continuations are defunctionalized — each closure that CPS would create becomes a variant in a `Cont` enum, stored in a `Vec<Cont>` stack.

**Problem:** `eval()` and `materialize()` are mutually recursive across 8+ call patterns. Deeply-nested lazy chains exhaust the Rust call stack before `MAX_EVAL_DEPTH` fires. Tinct works around this with a 64MB worker thread stack.

**Architecture:** Two enums, one loop.

`Action` represents what to do now (the "control" register):

```rust
enum Action {
    Continue(EvalResult<Value>),
    Materialize { thunk: Rc<Thunk>, mat_span: Option<Span> },
    Eval { expr: Rc<Spanned<Expr>>, env: Rc<RefCell<Environment>>, ctx: Rc<EvalContext> },
}
```

`Cont` represents what to do with the result (the reified continuation / "kontinuation" stack):

```rust
enum Cont {
    // materialize() continuations
    Memoize(Box<MemoizeData>),                       // cache result/error in thunk
    PendingCallDispatch(Box<PendingCallDispatchData>),// force callee, then invoke
    GuardedValidate(Box<GuardedValidateData>),        // validate against type annotation
    BuiltinForceArg(Box<BuiltinForceArgData>),        // force arg[0] for builtins

    // eval() continuations
    DotAccessForce(Box<DotAccessForceData>),           // access field from materialized dict
    TypeAssertCheck(Box<TypeAssertCheckData>),          // validate against TypeAssert annotation
}
```

All `Cont` variant payloads are boxed to keep the `Cont` enum ≤96 bytes (one cache line). The compile-time assertion at `src/eval_materialize.rs:252` enforces this.

The main loop is a two-register machine — `action` (what's happening now) and `stack` (what's waiting):

```rust
pub(crate) fn run(initial: Action, ctx: &Rc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::with_capacity(64);
    let mut action = initial;

    loop {
        action = match action {
            Action::Eval { expr, env, ctx } => {
                // eval_step() handles the expression and may push continuations
                eval_step(&expr, &env, &mut stack, &ctx)
            }
            Action::Materialize { thunk, mat_span } => {
                // force_step() dispatches on thunk state, pushes Memoize continuation
                force_step(&thunk, mat_span, &mut stack, ctx)
            }
            Action::Continue(result) => {
                match stack.pop() {
                    None => return result,
                    Some(cont) => apply_cont(cont, result, &mut stack, ctx)
                }
            }
        };
    }
}
```

**How this works:** Instead of recursive calls, each continuation point becomes a `Cont` variant pushed onto the stack. When a sub-computation completes (`Action::Continue`), the top continuation is popped and dispatched. The `Cont` variant stores exactly the state that a closure would have captured — no more, no less.

**Memoize error handling:** On `Err`, `Cont::Memoize` must call `cache_failure()` (set `ThunkState::Failed`) before propagating the error up the continuation stack. This ensures failed thunks cache their error and don't retry on every access.

**Builtin return dispatch:** Builtins return `Rc<Thunk>`, not `Value`. After a builtin call, the CEK machine inspects the result: if the thunk is already `Materialized`, extract the value and produce `Action::Continue(Ok(value))`. If it is `Unevaluated` or `PendingBuiltin`, the dispatch depends on the **continuation context**, not a dynamic inference:

- If the top of the continuation stack is `Cont::Memoize` (the builtin was called during materialization of a parent thunk), the result must be materialized — produce `Action::Materialize { thunk: result_thunk, ... }`.
- If the top is `Cont::DictBuildValue`, `Cont::BindArgDefault`, or similar construction contexts, the result stays lazy — produce `Action::Continue(Ok(Value::from_thunk(result_thunk)))`.

This is **structurally determined** by the `Cont` variant on the stack, not inferred at runtime. Each `Cont` variant statically knows whether it needs a materialized value or accepts a thunk. The strictness signature table (§Selective Materialization — Formal Specification) declares per-argument strictness for builtin *inputs*; the continuation context determines strictness for builtin *outputs*. Builtins like `$if` and `$get` return lazy thunks that must not be auto-materialized when used as dict values or function arguments.

**deep_materialize:** Implemented as a separate recursive function in `eval_deep.rs`, calling `materialize()` per dict entry and seq element with cycle detection and sharing preservation via a `HashMap` cache. The target architecture expresses this as `DeepEntries` and `DeepSeqTail` continuations within the CEK loop, eliminating the separate recursive helper.

**Tail-call optimization:** In tail position (e.g., last expression in a function body), set `action = Action::Eval { body, ... }` without pushing a `Cont`. The current frame is reused. TCO for recursive stdlib functions (`fold`, `map`, `filter`) follows the same pattern: detect tail calls during the variable resolution pass, mark them, and skip the continuation push. TCO applies to user-defined function calls only. Builtin calls always push a continuation — builtins rely on `PendingBuiltin` thunk deferral for lazy behavior, not tail-call elimination.

**Error stack traces:** Walk `Vec<Cont>` to reconstruct the call stack. Each `Cont::CallForceFunc` carries the call-site span and label, replacing the current `EvalError::stack` vector. This gives precise "materialized at" context for every frame in the stack.

**Cont variant count:** ~18-20 variants, one per continuation point in the current recursive evaluator. Each variant stores only its specific continuation data (Rc pointers + Span + small fields). Target frame size: ≤96 bytes per Cont (achieved by boxing large fields in the biggest variants).

**Relationship to allocation strategy:** Arena allocation and flat environments integrate naturally with the CEK machine: `Cont` variants hold `ThunkId` handles into the arena, and the `Vec<Cont>` stack's lifetime defines the arena's lifetime scope.

## Runtime Reflection

### `FnAnnotation` — Function Metadata at Runtime

`Value::Function` carries a `FnAnnotation` alongside its `params`, `body`, and `env`:

```rust
pub struct FnAnnotation {
    pub doc: Option<String>,              // extracted from fn@[doc: "..."] at eval_fn time
    pub return_ann: Option<Annotation>,   // the fn-level Annotation (return type, constraints)
    pub constraints: Vec<Constraint>,     // parsed from return_ann at creation time
    pub source_file: Option<PathBuf>,     // file path — from EvalConfig.current_file
    pub source_span: Span,               // always available at eval_fn time
}
```

Wrapped as `Option<Box<FnAnnotation>>` — `None` for unannotated functions (zero overhead). `doc` is extracted from `return_ann` at function creation if `return_ann` is a `PropertyDict` with a `"doc"` entry. `source_file` is threaded from `EvalConfig.current_file`, set via `with_base_dir_and_path` at `src/builtins_meta.rs:1152`.

### `ast-of` Builtin

`ast-of` is a Rust primitive in the `%rust "meta"` module. It returns the AST dict for any value using the existing `ast_to_dict` schema (`doc/15-ast.md §AST Dict Schema`):

- **`Value::Function`**: returns `[type: "fn" return-ann: <ann-dict> params: [...] body: <ast-dict>]`. The body is eagerly serialized via `ast_to_dict_expr`. `return-ann` and each param's annotation use the existing `annotation_to_thunk_id` schema, with compound annotation entry values recursively serialized (not placeholder strings).
- **`Value::Builtin`**: returns `[type: "builtin" name: <name> module: <module>]` using a shared `builtin_type_for(name)` static table that de-duplicates the parallel registration in `standard_builtins()` and `TypeEnv::with_builtins()`.
- **Other values**: `[type: type-of(val)]` — a minimal structural description.

`ast-of` returns `Unknown` from the type checker's perspective. Field accesses on the result are on an `Unknown`-typed value and are not statically verified. The reflection layer is inherently dynamically typed — consistent with Python `inspect`, Common Lisp `describe`, and Elixir `Module.docs/2`. Tinct's gradual typing allows this: `@Unknown` opts out of static checking for the reflection helpers.

### Reflection Helpers in Prelude

`describe`, `sig-from-ast`, `annotation-to-str`, `annotation-of`, and `source-of` are pure tinct functions in `stdlib/prelude.llt` using only existing primitives. They use `find-first-or` (not `find-first`) for null-safe annotation entry lookup. `describe` on a function returns a dict with `doc:`, `return-ann:`, `params:`, and `sig:` fields; on a non-function value it returns `[type: type-of(val)]`.

The round-trip paths are: in-memory (`[eval-ast [ast-of f]]`, works for pure/stdlib-only functions); file persistence (format via formatter, write to `DirCap`, re-include).

**References:** Sheard, T. & Peyton Jones, S. (2002). "Template Haskell." *Haskell Workshop.* [runtime staging analogue]

## Quote Semantics

`[quote expr]` converts the syntactic form of `expr` into an AST dict (per `doc/15-ast.md` §AST Dict Schema) without evaluating it. The conversion happens when the `Expr::Quote` node is materialized by the normal evaluator — this is runtime evaluation, not a compile-time operation. The result is an ordinary `Value::Dict`.

`[unquote expr]` inside a `[quote ...]` evaluates `expr` in the current runtime environment when the surrounding `[quote]` is materialized, then splices the result into the dict structure. `[unquote-splice expr]` evaluates to a `Value::Seq` and splices each element into the enclosing list position. Nesting depth follows Bawden (1999): nested `[quote [quote [unquote x]]]` preserves the inner `unquote` as AST (not evaluated, since depth > 1).

Quoted expressions have type `Dict`. No special type rules — `quote` is transparent to the type system.

## Macro Expansion Pipeline

After parsing, the pipeline inserts an expansion phase:

```
parse → expand_macros → desugar → resolve → typecheck → eval
```

`expand_macros` walks the AST top-down. When a `Call` node's function name matches a registered macro in `MacroEnv`, the expander:
1. Quotes all arguments (calls `ast_to_dict_expr` per arg — arguments are never evaluated)
2. Calls the macro function with the quoted argument dicts
3. Calls `dict_to_ast` on the result to produce a replacement AST node
4. Replaces the original call with the expansion and re-expands

`[defmacro name [params] body]` is processed by the expander: the body is evaluated in a **fresh `EvalContext`** (not shared with the runtime pass — prevents `IncludeContext` cache pollution and depth budget erosion) that inherits `EvalConfig` (capability flags, `no_fs`). The resulting callable is registered in `MacroEnv`. The `Expr::DefMacro` node is removed from the AST after registration — the typechecker and evaluator never see it.

**Termination:** Depth limit 100 per call-site (configurable via `TINCT_MACRO_DEPTH`), plus a total node-count cap of 100k nodes post-expansion to prevent exponential AST blowup. A `HashSet<(file_id, byte_offset)>` tracks in-progress call sites; macro-generated nodes with no source position receive a `SyntheticId(u64)` as an alternate key.

**Namespace protection:** Macros cannot shadow registered Rust builtins — enforced at registration time.

**`gensym`:** Produces names of the form `:gensym:N` (colon prefix is forbidden in bare-word identifiers, making user collision structurally impossible). Names are unique but not stable across evaluation orders.

**Include ordering:** The `$include` builtin runs the full pipeline (parse → expand_macros → desugar → resolve → eval) on included files. Macros defined in an included file are expanded within that file's scope, but are **not** propagated to the includer — macro definitions are expansion-time constructs that don't cross the runtime `$include` boundary. Cross-file macro availability would require static include resolution during the expansion phase, which conflicts with tinct's runtime-based include model. This is a consequence of Flatt (2002) phase separation: compile-time imports must be resolved before expansion begins, but tinct's `$include` is a runtime operation.

## Macro Hygiene

Scope sets (Flatt 2016) prevent accidental variable capture. Each macro invocation gets a fresh `ScopeId(u32)`. Bindings introduced by the macro body carry the definition-site scope; call-site variables carry the caller's scope. Two bindings with the same string name but different `ScopeId`s are distinct.

This is a simplification of Flatt's full biggest-subset binding resolution rule, sufficient for non-recursive macros. If recursive macro patterns or nested macro definitions arise, upgrade to the full biggest-subset model.

**Dual-span error provenance** uses a side map (`HashMap<NodeKey, Span>`) maintained by the expander. The side map records `(macro_name, call_site_span, expansion_rule_index)` per generated node — honest tags per Pombrio & Krishnamurthi (2015) Theorem 2 (Abstraction). Error messages show "in expansion of `<name>` at line N" with provenance chains for nested expansions.

**No intentional hygiene escape hatch.** `var!` or any mechanism that lets a macro inject bindings into the caller's scope is not provided. Macro bindings are always hygienic.

**Precedent:** Jsonnet's VM uses 22 `FrameKind` variants with a value register (production-tested at Google). Nickel uses an iterative stack machine with `OpFirst`/`OpSecond` continuations (production Rust). Both are defunctionalized CPS machines. The theoretical foundation is Felleisen & Friedman's CEK machine.

**Recursive call sites being converted:**

| Current recursive call | Becomes |
|----------------------|---------|
| `eval()` → `eval()` (TypeAssert, desugar, defaults) | `Action::Eval` + `Cont::TypeAssertCheck` etc. |
| `eval_call()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::CallForceFunc` |
| `eval_dot_access()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DotAccessForce` |
| `eval_dict()` → computed key materialization | `Action::Eval` + `Cont::DictBuildKey` |
| `eval_document()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DocumentScope` (`%` bound as `Unevaluated` thunk, never materialized) |
| `bind_args_thunks()` → default eval | `Action::Eval` + `Cont::BindArgDefault` |
| `materialize()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::Memoize` |
| `materialize()` → builtin call + `materialize()` | Builtin dispatch + `Cont::PendingBuiltinForceResult` |
| `materialize()` → `materialize()` (PendingCall) | `Action::Materialize` + `Cont::PendingCallForceFunc` → `Cont::PendingCallForceResult` |
| `deep_materialize()` → `materialize()` + recurse | `Action::Materialize` + `Cont::DeepEntries` / `Cont::DeepSeqTail` (within CEK loop, no separate helper) |
