# Evaluation

## Lazy Evaluation

Everything is a thunk until materialized. Compute only what's needed, when it's needed.

```tinct
[
    # Won't run unless `result` is actually used
    result: [call $expensive-computation $data]

    # Infinite sequences -- only compute what you take
    naturals: [call $range 0]
    first-ten-evens: [call $collect
        [call $take 10
            [call $filter [fn [n] [call $= 0 [call $mod $n 2]]] $naturals]]]

    # Short-circuit: if condition is true, never evaluate the else branch
    value: [call $if $condition $cheap-option $very-expensive-option]
]
```

## Recursive Dict Scoping (`letrec`)

**All entries in a dict see each other.** Entry order doesn't matter semantically.

```tinct
[
    x: [call $+ $y 1]    # thunk — when materialized, looks up $y → 6
    y: 5
]

# Mutual recursion works
[
    even?: [fn [n] [call $if [call $= $n 0] true  [call $odd?  [call $- $n 1]]]]
    odd?:  [fn [n] [call $if [call $= $n 0] false [call $even? [call $- $n 1]]]]
]
```

**Why:** Dicts are the fundamental unit — they shouldn't be order-dependent. Lazy evaluation makes this free: all bindings are thunks referencing a shared environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

**Key evaluation scope:** Dict keys are evaluated in the *parent* scope, not the dict's own letrec scope. This means key expressions cannot reference sibling bindings within the same dict. This is intentional for letrec correctness: keys must be deterministic regardless of entry order, and allowing keys to depend on sibling values (which are still unevaluated thunks) would introduce order-dependence or require eager evaluation of referenced entries.

**Why parent scope for keys:** The two-environment pattern (`parent_env` for keys, `dict_env` for values) ensures that computed keys are pure with respect to the dict's own bindings. A key expression like `[$a]` in `[x: 1 [$a]: 2]` resolves `$a` in the *enclosing* scope, not the dict scope — users might expect `$a` to reference the sibling binding `x: 1`, but this would create ordering dependence (does `x` exist when the key is evaluated?) and break the letrec invariant that all entries are mutually visible *as thunks* before any are forced.

Implementation: keys are evaluated via `eval_key(key_expr, parent_env, ctx, depth)` (in `eval_dict` in `src/eval.rs`) before the shared `dict_env` is populated with value thunks. This sequencing is critical: all keys must be known before string-keyed entries can be inserted into `dict_env` as bindings (in the dict environment binding loop in `eval_dict`).

**Effectful key expressions:** Computed keys may contain effectful operations (currently only `$include`). These effects execute in the parent scope context, not the dict's letrec scope. For example, `[$include "keys.llt"]` in a dict key position evaluates the included file with access to the parent environment's bindings, not the dict's own entries. This is consistent with the scoping rule but means included files used as keys cannot reference the dict's own bindings.

**Circular dependencies** are detected at materialization-time and reported with a clear cycle trace.

**Nested dicts create new scopes.** Each `[]` dict introduces a new lexical scope. Inner scopes see all bindings from outer scopes, and inner bindings shadow outer bindings of the same name within that inner dict. Scoping is lexical, not dynamic — closures capture their defining environment, not the calling environment. This matches Haskell's `let`/`where` and Nix's attribute sets.

The `Environment` struct's `parent` field implements this: each nested dict gets a new `Environment` whose `parent` points to the enclosing dict's environment. Variable lookup walks the parent chain outward.

```tinct
[
    x: 10
    inner: [
        x: 20              # shadows outer x
        y: [call $+ $x 1]  # $x is 20 (inner), not 10 (outer)
    ]
    z: [call $+ $x 1]      # $x is 10 (outer)
]
```

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
evens: [call $filter [fn [n] [call $= 0 [call $mod $n 2]]] [call $range 0]]

# Data (materialized, finite, dict with integer keys)
first-ten: [call $collect [call $take 10 $evens]]
# -> [0 2 4 6 8 10 12 14 16 18]
```

`$collect` runs the computation and pours results into a dict with integer keys 0..n. Calling `$collect` on an infinite sequence without `$take` is an error (hits depth/memory limit). This is explicit by design -- no accidental infinite materialization.

**Sequence constructors:**

| Function | Finite | Infinite | Description |
|----------|--------|----------|-------------|
| `range` | `[call $range 0 10]` | `[call $range 0]` | Integers from start (inclusive); 2-arg has end (exclusive), 1-arg is infinite |
| `repeat` | `[call $take 5 [call $repeat x]]` | `[call $repeat x]` | Infinite Seq of val; use `take` for finite |
| `cycle` | `[call $take 3 [call $cycle xs]]` | `[call $cycle xs]` | Infinite Seq cycling through dict entries; use `take` for finite |
| `seq` | -- | -- | Low-level: `[call $seq $head $tail-thunk]` |
| `iterate` | -- | `[call $iterate $f $x]` | `x, f(x), f(f(x)), ...` |
| `unfold` | varies | varies | `[call $unfold $step $seed]`; step returns `[value state]` or `[]` |

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

**Sequences are coinductive** — they are defined by observations (head/tail), not by construction (Coquand 1994). A sequence is **productive** if every observation step terminates: taking the head yields a value, and forcing the tail yields another sequence (or `[]`).

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

**`$seq` is the raw constructor with user-managed obligations.** `[call $seq $head $tail]` wraps two thunks into a Seq without forcing either. This enables guarded corecursion:

```tinct
ones: [call $seq 1 $ones]
# Works: $seq does NOT force $ones. The tail thunk captures $ones
# as an unevaluated reference. Each $tail observation produces a
# new Seq(1, <thunk>) without diverging.
```

`$seq` is lazy — it does not materialize its arguments (`builtins.rs:builtin_seq` wraps `Rc::clone(&args[0])` and `Rc::clone(&args[1])` directly). This is critical: it means `$seq` acts as a guard in the coinductive sense, allowing corecursive definitions that would cycle under eager evaluation.

**User obligations for `$seq`:**

1. The head thunk must terminate when observed.
2. The tail thunk must evaluate to either a `Seq` or `[]`.
3. Corecursive definitions must have at least one `$seq` constructor between the binding and the recursive reference (guardedness).

Violating these produces a runtime error (cycle detection or depth limit) for the failure modes tinct can detect. Slowly diverging computations (e.g., superpolynomial head evaluation) will appear to hang — this is inherent to any Turing-complete language without static totality.

**Why not static productivity checking:** Idris makes totality/productivity checking opt-in because mandatory checking rejects valid programs (Brady's rationale). Agda/Coq's mandatory guardedness is known to be fragile — it rejects intuitively productive programs, especially those using higher-order functions. Abel & Pientka's (2013) copatterns with sized types provide automatic productivity checking, but require size annotations threaded through the entire type system — constraint solving beyond HM unification. For a data transformation language, the pragmatic approach (productive-by-construction combinators + runtime backstop) provides the right tradeoff between safety and expressiveness.

**Error quality matters more than static checking.** Nix's biggest user-facing pain point with non-productive definitions is not the lack of static checking but the poor diagnostics ("infinite recursion encountered" with no useful context). tinct's error reporting should include: the thunk origin (which binding diverged), the materialization chain (who forced it), and the depth at which the limit was hit.

#### Testing Requirements

Corpus tests are required for each sequence constructor (`$range`, `$repeat`, `$cycle`, `$iterate`, `$unfold`), malformed tail errors (Seq tail evaluating to non-Seq, non-`[]` value), and depth limit behavior on diverging sequences. Tests should demonstrate the three runtime protection layers: blackholing (direct cycles), depth limit (runaway recursion), and tail discipline (type checking in `$collect`/`$head`/`$tail`).

### Dual-Dispatch for `$map` and `$filter`

`$map` and `$filter` accept both dicts and sequences, with behavior determined by input type:

| Input | `$map` result | `$filter` result |
|-------|--------------|-----------------|
| Dict | Dict (lazy values via PendingCall thunks) | Seq (must evaluate predicates) |
| Seq | Seq (lazy) | Seq (lazy) |

`$map` on a dict is the key insight: it returns a dict with the **same keys** but each value wrapped in a `PendingCall` thunk. No computation happens until a specific value is accessed. This gives `[call $map $f $big-dict]` O(n) construction and O(1) per-element access.

`$filter` on a dict must return a Seq because the output keys are unknown without evaluating predicates. Use `$collect` to get a dict back.

```tinct
# $map on dict: same keys, lazy values (no computation yet)
prices-usd: [call $map [fn [p] [call $* $p 1.1]] $prices-eur]
$prices-usd.widget    # only this one price is computed

# $filter on dict: returns seq (must evaluate predicates to decide inclusion)
expensive: [call $collect [call $filter [fn [p] [call $> $p 100]] $prices-eur]]

# $map on seq: returns seq (lazy)
doubled: [call $map [fn [n] [call $* $n 2]] [call $range 0]]
# nothing computed until $take/$collect
```

#### Testing Requirements

Each dual-dispatch builtin (`map`, `filter`, `take`, `drop`, `reduce`, `join`) requires corpus tests for both Dict and Seq input paths. Tests should verify the dispatch logic (Dict input produces Dict/Seq output as specified) and that the results are semantically equivalent regardless of input type.

## Thunk Lifecycle — Formal Specification

Extends Launchbury (1993) natural semantics for call-by-need with four additional thunk states (PendingBuiltin, PendingCall, Guarded, Failed) for deferred computation, contract validation, and error memoization. PendingBuiltin and PendingCall are defunctionalized continuations (Reynolds 1972; Danvy & Nielsen 2003) — they represent deferred computation as data rather than closures. Guarded implements proxy contracts (Findler & Felleisen 2002) for lazy TypeAssert field validation.

**State set:** `S = { Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed }`

### Part 1: State Transition Graph

The valid state transitions form an almost-acyclic directed graph — nearly all transitions move strictly forward, with one backward edge exception: non-cacheable errors (DepthExceeded) restore `InProgress → Guarded` to allow retry at a shallower depth (see Exception below).

```
Unevaluated ──────────┐
PendingBuiltin ────────┤
PendingCall ───────────┼──→ InProgress ──┬──→ Materialized
Guarded ──────────────┘                 └──→ Failed ⟲
```

The transition graph governs state *transitions*, not construction. Thunks may be constructed directly in Unevaluated, PendingBuiltin, PendingCall, Guarded, or Materialized state (via `Thunk::new_materialized`). The transition graph applies only to subsequent state changes.

Transition rules (each maps to one `take_*` or `set_state` call in `src/value.rs`):

| Transition | Trigger | Atomicity |
|-----------|---------|-----------|
| Unevaluated → InProgress | `take_unevaluated()` | Atomic (`mem::replace`) |
| PendingBuiltin → InProgress | `take_pending_builtin()` | Atomic (`mem::replace`) |
| PendingCall → InProgress | `take_pending_call()` | Atomic (`mem::replace`) |
| Guarded → InProgress | `take_guarded()` | Atomic (`mem::replace`) |
| InProgress → Materialized | `set_state(Materialized(v))` | Direct write |
| InProgress → Failed | `cache_failure(err)` | Via `transition()` |
| InProgress → Guarded | `set_state(Guarded(...))` | Direct write — **backward edge**, non-cacheable DepthExceeded only; restores original state to allow retry at lower depth (see [FORCE-GUARD-DEPTH]) |
| InProgress → PendingBuiltin | `set_state(PendingBuiltin(...))` | Direct write — **backward edge**, non-cacheable DepthExceeded only; restores original state for retry at lower depth |
| InProgress → PendingCall | `set_state(PendingCall(...))` | Direct write — **backward edge**, non-cacheable DepthExceeded only; restores original state for retry at lower depth |
| Failed → Failed | `set_state(Failed(e'))` | Direct write (diagnostic refinement only) |

**Monotonicity proof sketch:** The graph has no cycles (the single backward edge is acyclic: InProgress cannot return to itself through Guarded). Each source state (Unevaluated, PendingBuiltin, PendingCall, Guarded) transitions only to InProgress. InProgress transitions only to Materialized or Failed — with one exception: the backward `InProgress → Guarded` edge for non-cacheable DepthExceeded errors (see Exception below); this preserves semantic monotonicity because the thunk's observable meaning is unchanged between retries. Materialized is terminal — no transitions out. Failed has a self-edge for diagnostic refinement (enriching materialization spans and stack frames), but the error's semantic identity is fixed — only diagnostic metadata may be updated. Therefore all transition sequences are finite, and the semantic content of a thunk is monotonically determined. ∎

**Exception — retryable non-cacheable errors:** The `InProgress → Guarded` backward edge occurs under two conditions documented by `[FORCE-GUARD-OUTER-DEPTH]` (depth already at limit before inner thunk is forced) and `[FORCE-GUARD-DEPTH]` (inner thunk materialization fails with a non-cacheable error). Because `DepthExceeded` is a transient resource-bound error (not a semantic error), it is non-cacheable — `cache_failure` is skipped and the thunk is restored to `Guarded` state so the computation can be retried at a shallower call depth. This `InProgress → Guarded` backward restoration means strict state-order monotonicity does not hold for the `DepthExceeded` path. However, semantic monotonicity is preserved: the thunk's observable meaning is unchanged between attempts, and the error identity is not fixed. Every other error kind is cacheable and takes the normal `InProgress → Failed` forward edge. (`src/eval.rs`, in the `ThunkState::Guarded` arm of `materialize()`)

**Atomicity invariant:** Each `take_*` method atomically swaps the thunk state to InProgress before returning the captured data. This ensures no observer can see the old state after the transition begins. The atomicity is provided by `std::mem::replace` under an exclusive `borrow_mut()` — Rust's borrow checker prevents double borrows within a single thread.

### Part 2: Forcing Rules

Forcing (materialization) dispatches on the current state to produce a value or error. Rules use two judgment forms: `force(θ, d) ⇒ v` where θ is a thunk, d is the current depth, and v is the resulting value; and `eval(e, ρ, Σ, d) ⇒ θ` where e is an expression, ρ is the lexical environment, Σ is the EvalContext (base directory, include guards, stdlib env), d is the current depth, and θ is the resulting thunk. The EvalContext Σ is captured inside each thunk at construction time (written Σ_θ when referencing a specific thunk's context) and is not a parameter of `force` — it is part of the thunk's closure.

**Notation:** The rules use an implementation-oriented notation mixing imperative state updates (`θ.state ← InProgress`) with declarative judgments (`eval(expr, env, Σ_θ, d+1) ⇒ θ'`). `Σ_θ` denotes the evaluation context (`EvalContext`) captured at thunk construction time — it carries context-dependent state (base directory, include guards) that must reflect the thunk's definition site. A standard operational semantics would thread an explicit store σ mapping thunk IDs to states: `force(θ, d, σ) ⇒ (v, σ')`. The notation here maps directly to the `materialize()` implementation for ease of cross-checking.

**Precondition:** FORCE-DEPTH is checked before state dispatch. All other rules implicitly have `d ≤ MAX_EVAL_DEPTH` as a precondition.

**[FORCE-DEPTH]**
```
d > MAX_EVAL_DEPTH
───────────────────────────
force(θ, d) ⇒ error("maximum evaluation depth exceeded")
θ.state unchanged             (depth is a stack property, not a thunk property; see Commitment 3)
```

FORCE-DEPTH does not update θ.state because the depth limit is context-dependent. The same thunk may succeed when forced at a lower depth. This is the only forcing rule that does not transition the thunk state — it is also the only rule that breaks determinism in the pure subset (the same thunk can produce different results depending on the call-site depth). The CEK machine replaces MAX_EVAL_DEPTH with configurable resource limits, making this rule moot.

**[FORCE-CACHED]**
```
θ.state = Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-FAILED]**
```
θ.state = Failed(e)
───────────────────────────
force(θ, d) ⇒ error(e')
```

The materialization span update has three cases (`eval.rs:876-896`): (1) if e has no materialization span and one is available, set it; (2) if the access span matches the existing materialization span, no-op; (3) if the access span differs and is not already in the stack, add it as a stack frame (preserving the original materialization span). This Failed → Failed diagnostic refinement is an intentional relaxation of strict idempotence at the error-representation level — the error's identity and root cause are fixed, but diagnostic annotations accumulate across access paths.

**[FORCE-CYCLE]**
```
θ.state = InProgress
───────────────────────────
force(θ, d) ⇒ error("circular dependency")
θ.state ← Failed(err)         (memoize the cycle error)
```

**Cycle detection recovery strategy:** When a thunk in `InProgress` state is re-encountered during materialization (indicating a circular dependency), the evaluator constructs a `CircularDependency` error, decorates it with the materialization span (if provided), and transitions the thunk to `Failed` state via `cache_failure()` before propagating the error (in `materialize()` InProgress case in `src/eval.rs`). The `InProgress → Failed` transition is permanent — subsequent access to the same thunk returns the cached error without re-detecting the cycle. The error caching happens *before* propagation to ensure that all references to the cyclic thunk see the same error.

**State management after cycle detection:** The thunk is left in `Failed` state, not restored to its original state (`Unevaluated`, `PendingBuiltin`, etc.). This is correct because circular dependencies are semantic errors, not transient resource exhaustion — retrying the same thunk will always produce the same cycle. The cached error may be refined with additional materialization spans as the error propagates through the call stack (via the `Failed → Failed` diagnostic self-edge), but the error identity is fixed.

**Error propagation path:** After transitioning to `Failed`, the error is returned via `Err(err_boxed)`. Callers higher in the materialization stack see the error and propagate it upward. If the same thunk is accessed from a different call site later, the `Failed` case (in `materialize()` in `src/eval.rs`) fires immediately, returning the cached error (potentially with an updated materialization span for the new access site).

**[FORCE-GUARD]**
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
force(θ_inner, d+1) ⇒ v
v ∈ τ                                          (validate)
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-GUARD-INNER-ERR]** — inner thunk materialization fails with a cacheable error:
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
force(θ_inner, d+1) ⇒ error(e)    where e.is_cacheable()
θ.state ← Failed(e)                           (memoize; propagation error, not type mismatch)
───────────────────────────
force(θ, d) ⇒ error(e)
```

**[FORCE-GUARD-DEPTH]** — inner thunk materialization fails with DepthExceeded (non-cacheable):
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
force(θ_inner, d+1) ⇒ error(e)               where ¬e.is_cacheable()
θ.state ← Guarded(θ_inner, τ, path, span)     (restore — retry possible at lower depth)
───────────────────────────
force(θ, d) ⇒ error(e)
```

**[FORCE-GUARD-OUTER-DEPTH]** — outer thunk depth check fires before inner thunk is forced (Path A):
```
θ.state = Guarded(θ_inner, τ, path, span)
d ≥ MAX_EVAL_DEPTH
θ.state ← InProgress                          (via take_guarded)
θ.state ← Guarded(θ_inner, τ, path, span)     (restore — retry possible at lower depth)
───────────────────────────
force(θ, d) ⇒ error(DepthExceeded)
```

[FORCE-GUARD-DEPTH] fires when the *inner* thunk exhausts depth during forcing (Path B). [FORCE-GUARD-OUTER-DEPTH] fires when depth is already at the limit before the inner thunk is forced at all (Path A). Both paths restore `Guarded` state for the same reason: DepthExceeded is a transient resource-bound error, not a semantic one. (`src/eval.rs`, after `take_guarded()`, before calling `run()`, in the `ThunkState::Guarded` arm of `materialize()`)

**[FORCE-GUARD-TYPE-ERR]** — inner thunk succeeds but value does not inhabit the expected type:
```
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
force(θ_inner, d+1) ⇒ v
v ∉ τ                                          (validation fails)
e = type_assert_failed(path, τ, typeof(v), span)
θ.state ← Failed(e)                           (memoize type assertion error)
───────────────────────────
force(θ, d) ⇒ error(e)
```

Guarded thunks implement proxy contracts (Findler & Felleisen 2002) for TypeAssert record field validation. The inner thunk is forced, the result is validated against the expected type τ, and the validated value is memoized. If validation fails, the thunk transitions to Failed with a type assertion error decorated with the field path. Guard memoization ensures each field is validated at most once. Computation errors (inner thunk fails for non-type reasons) propagate directly and are cached; they do not trigger the `default:` fallback — only type assertion failures do. DepthExceeded is unique in restoring the Guarded state instead of transitioning to Failed, since it is a transient resource-bound condition rather than a semantic error.

**[FORCE-EVAL]**
```
θ.state = Unevaluated(expr, env, Σ_θ)
θ.state ← InProgress                          (blackhole)
eval(expr, env, Σ_θ, d+1) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)                     (memoize)
───────────────────────────
force(θ, d) ⇒ v
```

**Note:** If `d ≥ MAX_EVAL_DEPTH (256)`, `force` returns `ErrorKind::MaxDepthExceeded` before entering this rule (see [FORCE-DEPTH] above). The thunk state is left unchanged so the same thunk may succeed when forced at a lower depth.

`Σ_θ` is the evaluation context captured at thunk construction time. The thunk evaluates in its captured context, not the current forcing context — this ensures that context-dependent state (base directory, include guards, depth budget) reflects the thunk's definition site.

**[FORCE-EVAL-ERR]**
```
θ.state = Unevaluated(expr, env, Σ_θ)
θ.state ← InProgress
eval(expr, env, Σ_θ, d+1) ⇒ θ'
force(θ', d+1) ⇒ error(e)
θ.state ← Failed(e)                           (memoize error)
───────────────────────────
force(θ, d) ⇒ error(e)
```

**[FORCE-BUILTIN]**
```
θ.state = PendingBuiltin(f, args, named, pd, cs, Σ_θ)
θ.state ← InProgress
f(args, named, Σ_θ, pd, cs) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

The builtin receives `pd` (the pending depth captured at PendingBuiltin construction time) for its own recursion budget, but the subsequent `force(θ', d+1)` uses the current depth `d`. A PendingBuiltin created at depth 10 but forced at depth 200 runs the builtin with depth-context 10 but recurses at depth 201.

**Depth semantics rationale.** The two-depth design is intentional. `pd` (pending depth) governs the builtin's *internal* materialization budget — how deep the builtin itself may recurse when examining its arguments (e.g., `$merge` materializing both operands). The current depth `d` governs the *continuation* — how deep the result may be forced after the builtin returns. Using `pd` for the builtin preserves the depth budget the caller intended when constructing the PendingBuiltin; using `d` for the continuation reflects the actual call-stack depth at forcing time. This prevents a deeply-deferred PendingBuiltin from circumventing depth limits: the builtin runs with its original budget, but the result is forced at the current (possibly deeper) stack position. The iterative evaluator replaces depth tracking with explicit fuel/stack-size limits, eliminating this two-depth distinction.

**[FORCE-CALL]**
```
θ.state = PendingCall(f_θ, args, named, cs, Σ_caller, Σ_θ)
θ.state ← InProgress
force(f_θ, d+1) ⇒ Function(params, body, env)
invoke(params, body, env, args, named, Σ_caller) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

**[FORCE-CALL-BUILTIN]**
```
θ.state = PendingCall(f_θ, args, named, cs, Σ_caller, Σ_θ)
θ.state ← InProgress
force(f_θ, d+1) ⇒ Builtin(func)
func(args, named, Σ_caller, d, cs) ⇒ θ'
force(θ', d+1) ⇒ v
θ.state ← Materialized(v)
───────────────────────────
force(θ, d) ⇒ v
```

If `force(f_θ)` produces a value that is neither Function nor Builtin, the forcing fails with a type mismatch error (in `materialize()` PendingCall case in `src/eval.rs`), which is cached in Failed state.

Error variants for FORCE-BUILTIN, FORCE-CALL, and FORCE-CALL-BUILTIN follow FORCE-EVAL-ERR: on any error, `θ.state ← Failed(e)` before propagation.

**Error decoration:** All errors are decorated via `attach_materialization_context` (in `src/eval.rs`) before caching, adding the materialization span (if not already set) and origin stack frames. The decoration happens in the `map_err(&decorate)` chain before `cache_failure` is called.

**Fast path:** In FORCE-BUILTIN, FORCE-CALL, and FORCE-CALL-BUILTIN, if θ' is already Materialized, skip the recursive `force` and extract the value directly. This is observationally equivalent to the general rule — FORCE-CACHED fires immediately on the recursive `force(θ', d+1)` — but avoids the function call overhead (in `materialize()` PendingBuiltin and PendingCall cases in `src/eval.rs`).

**Value::Proxy access dispatch.** Dot access (`$proxy.field`) and bracket access (`$proxy[key]`) on a `Value::Proxy` are not part of the thunk lifecycle — they occur *after* materialization produces a Proxy value. The evaluator dispatches to `invoke_proxy_handler`, which materializes the handler thunk (sharing-preserving via Launchbury memoization) and invokes it with the key. Each proxy access costs one depth level via `materialize(handler, ..., depth + 1)`. Proxy-handler-returns-Proxy chains are bounded by `MAX_EVAL_DEPTH`.

### Part 3: Semantic Properties

Six properties essential for call-by-need soundness (Launchbury 1993, Ariola & Felleisen 1997):

| Property | Status | Qualification |
|----------|--------|---------------|
| **Determinism** | Satisfied | Pure subset only; `$include` introduces external state dependence. FORCE-DEPTH is also context-dependent (same thunk may succeed at different depths) |
| **Sharing (evaluate-at-most-once)** | Satisfied | Materialized and Failed are semantically terminal — subsequent forces return cached result (Failed may refine diagnostic metadata) |
| **Monotonicity** | Satisfied with exception | transition graph has no backward edges except `InProgress → Guarded` for non-cacheable DepthExceeded errors (retry semantics); Failed self-edge refines diagnostics only (proven above) |
| **Adequacy** | Holds for extensions | PendingBuiltin/PendingCall are observationally equivalent to Unevaluated (defunctionalization preserves semantics). Guarded is observationally equivalent to an Unevaluated thunk that forces and validates (proxy contract). Failed extends the codomain from Value⊥ to Value + Error⊥ (absorbing, deterministic) |
| **Confluence** | Pure subset only | `$include` makes evaluation order observable; in the pure subset, forcing order does not affect final values |
| **Sharing preservation** | Satisfied | `Rc<Thunk>` ensures identity-based sharing; the CEK machine preserves thunk identity through continuation dispatch |

### Semantic Commitments

Implicit decisions in the current implementation, made explicit:

**1. Error memoization is permanent.** Once a thunk reaches Failed, it never retries. This includes I/O failures from `$include` — a file-not-found error is cached forever, even if the file appears later. This is correct for a build-time evaluator (deterministic builds) and matches Nix's `nFailed` semantics (Peyton Jones et al. 1999 "imprecise exceptions"). Retryable failures would require a new `Retryable` state or external retry logic — not planned.

**2. Confluence holds only in the pure subset.** `$include` introduces evaluation-order dependence: if file A includes file B and file B includes file A, the result depends on which is evaluated first (cycle detection fires on the second). All other tinct operations are confluent — forcing order does not affect the result. The pure subset of tinct (no `$include`) satisfies the diamond property of Ariola & Felleisen's (1997) call-by-need calculus.

**3. MAX_EVAL_DEPTH is practical, not semantic.** The depth bound (256) is an implementation artifact to prevent stack overflow in the recursive evaluator. It is not part of the formal semantics — a correct implementation with sufficient stack space should produce the same values without the bound. The CEK machine migration (with heap-allocated continuations) should remove this bound, replacing it with configurable resource limits (`--max-depth`) if needed. Consequently, FORCE-DEPTH errors are non-destructive: the thunk state is unchanged, and the same thunk may succeed at a lower depth.

**4. Finite vs productive thunk lifecycles.** Dict-entry thunks have a **finite lifecycle**: they must eventually reach Materialized or Failed. Seq tail thunks have a **productive lifecycle**: materializing a tail yields a Seq value (containing a new tail thunk) or the terminal `[]`. The state machine is identical; the liveness obligation differs. This distinction is not enforced by the type system — it is a semantic contract between the sequence constructors and the programmer (see §Productivity Obligations).

### Adequacy of PendingBuiltin and PendingCall

These states are defunctionalized continuations (Reynolds 1972). Each is observationally equivalent to an Unevaluated thunk holding an expression that would perform the same computation:

- `PendingBuiltin(f, args, named, pd, cs, Σ_θ)` ≡ `Unevaluated([call $f ...args ...named], env, Σ_θ)` where env binds the arg thunks
- `PendingCall(f_θ, args, named, cs, Σ_θ)` ≡ `Unevaluated([call <force f_θ> ...args ...named], env, Σ_θ)`
- `Guarded(θ_inner, τ, path, span)` ≡ `Unevaluated(<force θ_inner then validate ∈ τ>, env, Σ_θ)` — a proxy contract monitor (Findler & Felleisen 2002)

The equivalence for PendingCall holds because `eval` of `[call ...]` already performs dynamic dispatch on the callee — if `f_θ` materializes to a Builtin rather than a Function, both the PendingCall path (FORCE-CALL-BUILTIN) and the hypothetical Unevaluated path would dispatch to the same builtin.

The difference is operational: PendingBuiltin/PendingCall avoid constructing AST nodes for deferred computations. A formal adequacy proof would show bisimulation: every forcing sequence starting with `PendingBuiltin(f, args, ...)` produces the same value as forcing `Unevaluated([call $f ...args], env)`. This is conjectured based on the defunctionalization correspondence (Reynolds 1972; Danvy & Nielsen 2003) but not mechanically verified.

### Relationship to CEK Machine Migration

The iterative evaluator (§Iterative Evaluator) uses explicit `Cont` variants on the continuation stack to process ThunkState transitions. The CEK machine does not remove PendingBuiltin and PendingCall from ThunkState — these are permanent design elements representing persistent deferred computation:

- **PendingBuiltin** stores deferred builtin calls for lazy sequences (`$map`, `$filter`, `$fold_step`, etc.) and proxy handler dispatch. Cannot be replaced by Unevaluated because builtin function pointers (`BuiltinFn`) have no AST representation. Lazy sequences need persistent storage for deferred steps.
- **PendingCall** stores deferred function calls for lazy dispatch and tail-call optimization. Represents work already done by `eval_call` (evaluated func_expr, wrapped args) that Unevaluated would duplicate.
- The monotonicity proof and semantic properties remain unchanged — the 7-state transition graph (Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed) is the stable design.
- **Sharing preservation is the critical migration invariant**: thunk identity (`Rc<Thunk>` pointer) must be preserved through continuation dispatch. A materialized thunk must be the same allocation that was created at the definition site.
- MAX_EVAL_DEPTH is replaced by configurable resource limits (`--max-depth`, `--max-memory`) rather than hardcoded safety bounds

## Error Reporting

Error semantics are specified in [Error Handling](10-errors.md). This section summarizes the key concepts; see doc/10 for formal rules and implementation mappings.

**Dual-span model:** Every error carries a definition site (where the error-producing expression was written) and a materialization site (where a consumer forced the thunk that failed). The `attach_materialization_context` function decorates errors with these spans during propagation through the `map_err(&decorate)` chain.

**Stack frame accumulation:** When an error propagates through multiple materialization layers (e.g., `θ₁ → θ₂ → θ₃`), each layer adds a stack frame via DECORATE (doc/10 §Part 3). The first materialization site becomes `mat_span`; subsequent sites become stack frames. Deduplication guards prevent redundant frames.

**Error caching:** Cacheable errors (all except `DepthExceeded`) are memoized in `Failed` state via `cache_failure()`. Subsequent access returns the cached error with additional materialization context. Non-cacheable errors (`DepthExceeded`) restore the thunk to its original state, allowing retry at a shallower depth. See MEMO-CACHE and MEMO-SKIP rules in doc/10 §Part 5.

**Error condition specifications:** The trigger conditions for all `ErrorKind` variants (when each error is raised) are documented in [Error Handling](10-errors.md) §Part 2: Error Sources. Propagation rules (PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-CYCLE, PROP-DEPTH) are in doc/10 §Part 4.

## Selective Materialization — Formal Specification

Specifies which arguments each Rust-native builtin forces (materializes) before execution and how the result is constructed. This is a two-tier specification: a **strictness signature table** covering all 46 builtins (auditable summary), plus **delta rules** for builtins whose forcing behavior cannot be captured by a flat per-argument annotation.

The signature notation draws on Mycroft's (1981) abstract interpretation framework for strictness analysis. The delta rules follow Plotkin's (1981) structural operational semantics, using the same judgment style as §Thunk Lifecycle — Formal Specification.

### Part 1: Strictness Signature Notation

Each builtin receives a per-argument strictness annotation and a result classification:

**Input strictness (per argument position):**

| Symbol | Meaning | Implementation pattern |
|--------|---------|----------------------|
| `S` | Strict — argument is materialized before the builtin executes | `materialize(&args[i], None, depth)` |
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

All 46 Rust-native builtins. Builtins marked `†` have dual dispatch on Dict/Seq (delta rule required). Builtins marked `‡` have non-trivial forcing patterns (delta rule required).

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
| `if` ‡ | `S × Sc × Sc → Θ` | Selective | Exactly one of args[1]/args[2] is forced; the other is never touched |

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
| `eval` | `S → V` | Materializing | Deep materialization — recursively forces all thunks |
| `error` | `S → ⊥` | Materializing | Always raises; never returns |
| `try` ‡ | `S → D` | Materializing | Strict on function arg — materializes before invocation, catches errors |
| `apply` | `S × S → Θ` | Materializing | Materializes both; delegates to function invocation. Result type depends on the applied function |

**Type introspection:**

| Builtin | Signature | Category |
|---------|-----------|----------|
| `type-of` | `S → V` | Materializing |

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
| `head` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns head thunk (not forced) |
| `tail` ‡ | `S → Θ` | Structural | Materializes arg to verify Seq; returns tail thunk (not forced) |
| `collect` ‡ | `S → D` | Structural | Materializes Seq spine (all tails); head thunks pass through into Dict |
| `seq?` | `S → V` | Materializing | Materializes arg; returns Bool |

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

Delta rules specify the forcing behavior for builtins marked ‡ in the signature table, plus dual-dispatch builtins (†) whose Dict/Seq paths have materially different forcing patterns. Builtins marked † without ‡ (e.g., `$take`, `$drop`) follow the same dual-dispatch pattern as `$map`/`$filter` but with simpler per-path logic — their forcing is fully characterized by the signature.

Rules use the judgment form `δ(f, [θ₁, ..., θₙ], d, cs) ⇒ r` where f is the builtin, θᵢ are argument thunks, d is the current depth, cs is the call span, and r is the result (a thunk or error). All current delta rules use positional args only; named args are empty (`∅`) and omitted from rules for brevity.

**Depth in PendingBuiltin:** When constructing a PendingBuiltin, builtins that perform no materialization themselves (e.g., `$repeat`, `$iterate`, `$unfold`) store `depth+1` to account for the recursion step when the PendingBuiltin is eventually forced (the materialization-site depth governs recursive forcing via FORCE-BUILTIN in §Thunk Lifecycle). Builtins that materialize within the step function (e.g., `$filter` step, `$reduce` step) store the current `depth` for their internal materialization calls.

**[DELTA-IF-TRUE]**
```
force(θ_cond, d) ⇒ true
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], d, cs) ⇒ θ_then
```

**[DELTA-IF-FALSE]**
```
force(θ_cond, d) ⇒ false
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], d, cs) ⇒ θ_else
```

**Branch isolation guarantee:** The unchosen branch is never forced. `θ_then` and `θ_else` are returned via `Rc::clone` — no state transition occurs on the unchosen thunk. This is the foundational selective materialization property from which `$and`, `$or`, `$when`, `$unless`, and `$cond` derive their short-circuit behavior (see Part 5). The chosen branch thunk is returned to the caller; its subsequent forcing happens via FORCE-BUILTIN in §Thunk Lifecycle, which calls `force(θ', d+1)` on the builtin's result — the separation between "builtin execution" and "result forcing" is what makes `$if`'s laziness guarantee possible.

**[DELTA-SEQ]**
```
───────────────────────────
δ(seq, [θ_head, θ_tail], d, cs) ⇒ Materialized(Seq(Rc::clone(θ_head), Rc::clone(θ_tail)))
```

No arguments are forced. Both pass through as thunks within the Seq value. This is the coinductive guard — `$seq` enables corecursive definitions by deferring evaluation of both head and tail.

**[DELTA-HEAD]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(head, [θ_xs], d, cs) ⇒ θ_h
```

**[DELTA-TAIL]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(tail, [θ_xs], d, cs) ⇒ θ_t
```

DELTA-HEAD and DELTA-TAIL materialize the container to verify it is a Seq, but return the extracted thunk *without forcing it*. The head/tail thunk retains its original state (Unevaluated, PendingCall, etc.). Empty dict `[]` as input produces a specific error (`"head/tail on empty sequence"`).

**[DELTA-COLLECT-EMPTY]**
```
force(θ_xs, d) ⇒ Dict({})
───────────────────────────
δ(collect, [θ_xs], d, cs) ⇒ Materialized(Dict({}))
```

**[DELTA-COLLECT]**
```
force(θ_xs, d) ⇒ Seq(θ_h₁, θ_t₁)
force(θ_t₁, d) ⇒ Seq(θ_h₂, θ_t₂)
...
force(θ_tₙ, d) ⇒ Dict({})          (terminal)
───────────────────────────
δ(collect, [θ_xs], d, cs) ⇒ Materialized(Dict({0↦θ_h₁, 1↦θ_h₂, ..., n↦θ_hₙ}))
```

Collect materializes the Seq *spine* (all tail thunks) but head thunks pass through into the result Dict without forcing. This is the key distinction: `$collect` is strict in the structure but lazy in the values.

**[DELTA-ITERATE]**
```
───────────────────────────
δ(iterate, [θ_f, θ_x], d, cs) ⇒ Materialized(Seq(
    Rc::clone(θ_x),
    PendingBuiltin(iterate, [Rc::clone(θ_f), PendingCall(θ_f, [θ_x])], d+1, cs)
))
```

Fully lazy: neither f nor x is forced. The result Seq's head is x (unchanged thunk), and the tail is a PendingBuiltin that will produce `iterate(f, f(x))` when forced. The `f(x)` is itself a PendingCall — computation unfolds one step at a time. When the tail PendingBuiltin is forced, DELTA-ITERATE applies again with `f(x)` as the new seed, enabling corecursive unfolding of the infinite sequence.

**[DELTA-TRY]**
```
force(θ_func, d) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env, d+1) ⇒ θ_body
force(θ_body, d+1) ⇒ v
───────────────────────────
δ(try, [θ_func], d, cs) ⇒ Materialized(Dict({"ok"↦Materialized(v)}))

force(θ_func, d) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env, d+1) ⇒ θ_body
force(θ_body, d+1) ⇒ error(e)
───────────────────────────
δ(try, [θ_func], d, cs) ⇒ Materialized(Dict({"err"↦Materialized(e.message)}))
```

`$try` materializes the function argument and invokes it. On success, returns `[ok: value]`; on error, returns `[err: message]`. The error is caught — `$try` itself does not propagate errors (it is the catching boundary). Also handles Builtin callees (dispatches with zero args).

**[DELTA-MAP-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
∀i. θ'ᵢ = PendingCall(θ_f, [θᵢ], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], d, cs) ⇒ Materialized(Dict({k₁↦θ'₁, ..., kₙ↦θ'ₙ}))
```

`θ_f` is never forced — it is captured by reference (`Rc::clone`) in each PendingCall. No values are computed; the result Dict is O(n) to construct and O(1) per element access.

**[DELTA-MAP-SEQ]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
θ'_h = PendingCall(θ_f, [θ_h], ∅, cs)
θ'_t = PendingBuiltin(map, [Rc::clone(θ_f), θ_t], ∅, d, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], d, cs) ⇒ Materialized(Seq(θ'_h, θ'_t))
```

Recursive structure: head is a PendingCall, tail is a PendingBuiltin that will apply DELTA-MAP-DICT or DELTA-MAP-SEQ when forced.

**[DELTA-FILTER-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
θ_step = PendingBuiltin(filter_dict_step, [θ_pred, θ_xs_mat, θ_keys, θ_idx], ∅, d, cs)
    where θ_xs_mat, θ_keys, θ_idx are pre-computed materialized thunks
───────────────────────────
δ(filter, [θ_pred, θ_xs], d, cs) ⇒ θ_step
```

The predicate `θ_pred` is not forced at the top level — it is captured for deferred evaluation in the step function. The step function materializes one element at a time, applies the predicate, and either includes or skips it. Returns a Seq (not a Dict) because filtered keys are unpredictable.

**[DELTA-FILTER-SEQ]**
```
force(θ_xs, d) ⇒ Seq(_, _)
θ_step = PendingBuiltin(filter_seq_step, [θ_pred, θ_xs], d, cs)
───────────────────────────
δ(filter, [θ_pred, θ_xs], d, cs) ⇒ θ_step
```

The step function receives the *original seq thunk* (not destructured head/tail) and materializes it internally to obtain head and tail. This avoids redundant materialization since the dispatch already forced the collection. Lazy filter on sequences: the step function forces head, applies predicate, and either includes it (Seq node) or skips it (recurse on tail). Elements are tested only when the result Seq is consumed.

**[DELTA-REDUCE-DICT]**
```
force(θ_xs, d) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
acc₀ = θ_init
∀i. accᵢ = PendingCall(θ_f, [accᵢ₋₁, θᵢ], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], d, cs) ⇒ accₙ
```

Builds a chain of PendingCall thunks without forcing any values. The entire reduction is deferred — nothing computes until the result thunk is forced. At that point, the chain unwinds from the inside out.

**[DELTA-REDUCE-SEQ]**
```
force(θ_xs, d) ⇒ Seq(θ_h, θ_t)
θ_step = PendingBuiltin(reduce_seq_step, [θ_f, θ_init, θ_h, θ_t], ∅, d, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], d, cs) ⇒ θ_step
```

Seq reduction uses a step function that materializes the tail to check for termination, then recurses. Unlike Dict reduction, Seq reduction is incremental (processes one element per step function invocation).

### Part 4: Dual-Dispatch Pattern

Six builtins (`map`, `filter`, `take`, `drop`, `reduce`, `join`) dispatch on the runtime type of their collection argument:

```
force(θ_xs, d) ⇒ v
    v = Dict(...)  →  apply Dict-specific rule
    v = Seq(...)   →  apply Seq-specific rule
    otherwise      →  type error
```

This dispatch materializes the collection argument to determine its type, then applies the appropriate delta rule. The function/predicate argument (if present) is *not* forced at dispatch time — it is captured by reference for deferred application.

**Result type asymmetry:** The Dict and Seq paths of a dual-dispatch builtin may produce different result types. For example, `$filter` on a Dict returns a Seq (not a Dict), because filtered keys are unpredictable. The signature table (Part 2) captures the Seq-path result; see §Type System and Dual-Dispatch Builtins for the full Dict-vs-Seq result matrix.

**In the iterative evaluator,** dual dispatch is a `Cont::CollectionDispatch` continuation that forces the collection, inspects its type, and pushes the appropriate next continuation. The function argument must be preserved on the continuation stack without forcing.

### Part 5: Derived Selectivity

Standard library functions defined in `stdlib/prelude.llt` inherit their materialization behavior from the builtins they invoke. Key derived selectivity properties:

| Function | Definition | Inherited behavior |
|----------|------------|-------------------|
| `not` | `[fn [x] [call $if $x false true]]` | Materializing — forces x via `$if`'s condition position |
| `and` | `[fn [a b] [call $if $a $b false]]` | Selective — forces a; b forced only if a is true |
| `or` | `[fn [a b] [call $if $a $a $b]]` | Selective — forces a; b forced only if a is false; returns a if truthy |
| `when` | `[fn [pred body] [call $if $pred $body []]]` | Selective — forces pred; body forced only if pred is true |
| `unless` | `[fn [pred body] [call $if $pred [] $body]]` | Selective — forces pred; body forced only if pred is false |
| `cond` | Recursive via `cond-impl` → `cond-check` → `$if` | Selective — forces conditions left-to-right via nested `$if`; first matching branch returned as thunk |
| `assert` | `[fn [cond msg] [call $if $cond true [call $error $msg]]]` | Selective — forces cond; error raised only if cond is false |

**Inheritance proof sketch:** Each derived function's selectivity follows by inlining its definition and applying DELTA-IF-TRUE/DELTA-IF-FALSE. For `$and`:

```
and(θ_a, θ_b)
  = if(θ_a, θ_b, false)
  DELTA-IF-TRUE:  force(θ_a) ⇒ true  → θ_b    (b is forced only when the caller forces the result)
  DELTA-IF-FALSE: force(θ_a) ⇒ false → false   (b is never touched)
```

This compositional guarantee means that making `$if` lazier (see §Laziness Design) automatically improves all derived control flow functions without code changes.

### Part 6: Properties and Guarantees

**Branch isolation (fundamental guarantee):**

For any builtin with `Sc` positions, the unchosen arguments are never forced, never transition state, and never appear in error traces. Formally: if `δ(if, [θ_c, θ_t, θ_e], d, cs) ⇒ θ_t`, then θ_e's `ThunkState` is unchanged after the call.

**No unnecessary forcing (structural guarantee):**

Builtins classified as Structural or Lazy-transforming force only the minimum arguments needed to determine the result structure. Value thunks within input collections pass through to output collections without forcing. This is verifiable by inspection: every `Rc::clone(&args[i])` or `Rc::clone(value_thunk)` preserves the thunk's state.

**Sharing preservation:**

All delta rules preserve thunk identity. When a thunk appears in both the input and output of a builtin (e.g., `$head` extracting a Seq's head), the same `Rc<Thunk>` allocation is shared — not copied. Subsequent forcing of the output thunk memoizes the value for all holders of that `Rc`.

**Strictness monotonicity:**

The signature table is monotonic with respect to the implementation: a builtin marked `L` at position i will never call `materialize()` on `args[i]`. A change that adds a `materialize()` call on a position marked `L` is a breaking change to the laziness contract and must update the signature table.

**Dual-dispatch consistency:**

For dual-dispatch builtins, the Dict and Seq paths must agree on which non-collection arguments are forced. For example, `$map`'s Dict path and Seq path both leave `θ_f` unforced — if one path started materializing `θ_f`, it would break laziness for programs that pass expensive computations as the function argument.

## Laziness Design

This table documents the laziness behavior of every operation and the rationale for each decision.

| Operation | Behavior | Rationale |
|-----------|----------|-----------|
| **Control Flow** | | |
| `$if` | Returns branch thunk directly (no materialization of branch) | The chosen branch stays lazy until accessed by caller |
| `$and` | Materializes first arg; second only if first is true (short-circuit via `$if`) | Short-circuit via `[fn [a b] [call $if $a $b false]]` |
| `$or` | Materializes first arg; second only if first is false (short-circuit via `$if`) | Short-circuit via `[fn [a b] [call $if $a $a $b]]`; returns a if truthy |
| `$not` | Materializes argument | Must inspect value |
| `$when`, `$unless` | Materializes condition; body returned as thunk | Body returned lazy via `$if` |
| `$cond` | Materializes conditions left-to-right; first matching branch returned as thunk | Delegates to `$if`; no code change needed |
| **Dict Operations** | | |
| `$merge` | Eagerly materializes both dicts; values pass through as thunks (Rc::clone) | See merge-lazy-overlay sprint in TODO.md for planned lazy overlay upgrade |
| `$get`, `$get-or` | Returns value thunk (structural) | Already lazy |
| `$keys` | Keys always evaluated | Keys are never thunks |
| `$values` | Returns list of thunks | Already lazy |
| `$entries` | Returns list of entry dicts (values stay as thunks) | Already lazy |
| `$set`, `$remove` | Values stay as thunks | Already lazy on values |
| `$update` | *Planned:* PendingCall thunk. *Current:* calls `$set` → `$merge` (eager materialization) | Wrapper around $set → $merge; same eager semantics as $merge. Lazy overlay planned. See merge-lazy-overlay sprint in TODO.md. |
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
| `$head` | Materializes container to verify Seq; returns head thunk (not forced) | Structural Seq operation |
| `$tail` | Materializes container to verify Seq; returns tail thunk (not forced) | Structural Seq operation |
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
| `$eval` | Deep-forces all thunks recursively | Explicit materialization primitive |
| `$type-of` | Materializes argument to inspect type | Must know runtime type |
| `$error` | Constructs error value (structural) | Structural |
| `$try`, `$try-or` | Materializes body, catches exceptions | Must run body to catch errors |
| `$assert` | Materializes condition | Must check condition |
| `$from-json` | Materializes JSON string, parses | Must parse entire JSON |
| `$include` | Evaluates file; returns cached thunk on re-include | Include memoization |
| **Internal (eval.rs)** | | |
| `eval_key` (dict construction) | Materializes all dict keys | Keys must be known for dict insertion |
| `builtin_keys` | Materializes dict | Keys are never thunks |
| `TypeAssert` body (`[@Type expr]`) | Forced at annotation site — `eval()` calls `materialize()` on the inner expression immediately | Cannot type-check an unevaluated thunk: the type constraint must be verified before the value is bound. Annotation-time forcing (not access-time). Known laziness violation; tracked in TODO (Fix TypeAssert forces materialization in eval()). |

**Error reporting impact:** Operations that shift from eager to lazy (e.g., `$if`, `$merge`, `$map`) will report errors at access time rather than construction time. This provides more accurate source locations (pointing to where materialization failed) but changes error timing. Inherently materializing operations continue to produce errors at call time.

---

## Deep Materialization — Implementation

The `$eval` builtin and CLI `--eval` flag use `deep_materialize` to recursively force all thunks in a value tree. This is distinct from selective materialization (which forces only what's needed for computation) — deep materialization forces *everything*, producing a fully-evaluated value tree suitable for serialization or comparison.

**Cache data structure:** `deep_materialize` uses a stack-local `HashMap<*const Thunk, Option<Rc<Thunk>>>` created at the `deep_materialize` entry point and passed through the recursion. The cache has a dual-purpose design (in `deep_materialize_impl`):

| Cache entry | Meaning | Purpose |
|-------------|---------|---------|
| `None` | Blackhole sentinel — thunk is currently being deep-materialized | Cycle detection (Launchbury 1993 blackholing) |
| `Some(Rc<Thunk>)` | Cached result — thunk was already deep-materialized | Sharing preservation (reuse the same `Rc<Thunk>` allocation) |

**Cache lifecycle:** The cache is created per `deep_materialize` call and dropped on return. It is *not* shared across multiple top-level `deep_materialize` invocations — each call to `$eval` or CLI evaluation creates a fresh cache. The cache is global *within* a single call: all branches of a nested dict or sequence tree share the same cache instance.

**Cycle handling:** When a thunk pointer is encountered for the second time within the same deep materialization (cache entry is `None`), the function returns `Rc::clone(thunk)` — the original thunk, not forced (in `deep_materialize_thunk`). This prevents infinite recursion on cyclic structures (e.g., `[x: $x]` or mutual dict references). The cycle is detected at the *structure* level (same thunk pointer seen twice during traversal), not the *value* level (the thunk's own `InProgress` sentinel, which detects cycles within a single thunk's evaluation).

**Sharing preservation:** When a thunk appears multiple times in the input value tree (e.g., `let shared = [expensive: [call $f]] in [a: $shared  b: $shared]`), the cache ensures the deep-materialized result is a *single* `Rc<Thunk>` shared by all references. Without the cache, `deep_materialize` would create independent copies for each occurrence, breaking `Rc::ptr_eq` and wasting memory. The `Rc::ptr_eq` invariant holds only within one `deep_materialize` call; two separate calls on overlapping trees produce distinct output pointers.

**Cache cleanup on error:** If `materialize()` or recursive `deep_materialize_impl()` fails, the cache entry is removed before propagating the error (in `deep_materialize_impl`). This prevents cache poisoning: a failed thunk leaves no stale `None` sentinel that would cause subsequent encounters to incorrectly return an unevaluated thunk.

**Growth characteristics:** For large dicts, the `HashMap` grows monotonically as new thunks are encountered. The cache never shrinks during traversal — it accumulates all seen thunk pointers until the entire `deep_materialize` call completes. For a dict with 10,000 entries containing shared sub-dicts, the cache may hold thousands of entries. This is acceptable because (a) the cache lifetime is bounded by the single `deep_materialize` call, not the session, and (b) the alternative (no cache) would traverse shared structures multiple times, defeating sharing.

**Comparison to selective materialization:** Regular `materialize()` has no visited set — it forces a single thunk and memoizes the result in `ThunkState::Materialized`. Cyclic dependencies are caught by the `InProgress` sentinel *within* the thunk, not by a global traversal cache. `deep_materialize` adds a *second* layer of cycle detection at the structural level (pointer identity across the value tree) via a `HashMap<*const Thunk, Option<Rc<Thunk>>>` dual-purpose cache, orthogonal to the per-thunk `InProgress` cycle detection.

**Relationship to Nix:** Nix's `forceValueDeep` (eval.cc:2264) uses a similar `std::set<const Value *> seen` for pointer-identity cycle detection. The key difference: Nix's set is visit-tracking only (all entries are pointers, not `Option<ptr>`), because Nix uses a conservative GC and doesn't need explicit sharing preservation — shared `Value*` pointers are naturally deduplicated. Tinct's `Option<Rc<Thunk>>` design combines visit-tracking (`None`) with result caching (`Some(rc)`) in a single structure.

**Decision:** Two-phase strategy. Phase 1 applies backward-compatible optimizations to the current `Rc<Thunk>` + `IndexMap<String, Rc<Thunk>>` runtime. Phase 2 introduces arena allocation and flat environments bundled with the iterative evaluator.

**Current allocation profile:**

| Component | Representation | Cost |
|-----------|---------------|------|
| Thunks | `Rc<Thunk>` with `RefCell<ThunkState>` | Individual heap alloc per thunk, triple indirection |
| Environments | `Rc<RefCell<Environment>>` with `IndexMap<String, Rc<Thunk>>` + parent chain | O(depth) variable lookup |
| Dict keys | `Key::String(String)` | Cloned 2× per dict entry (env bindings + dict_map) |
| Thunk origin | `origin: Cow<'static, str>` | Zero-cost for empty/static origins (`Cow::Borrowed`); allocates only for dynamic labels |
| Type inference sets | `HashSet<String>` in `collect_type_vars`, `collect_row_vars`, `collect_all_vars`, `instantiate_scheme`, `instantiate_at_level`, `generalize` | Transient per-call allocations in `src/types.rs`; each call allocates a fresh `HashSet`, collects variable names via tree traversal, then drops the set. Hot paths during type inference — `instantiate_scheme` is called per polymorphic variable reference, `generalize` per dict entry at Pass 4. Planned elimination: Phase 2's flat environments with de Bruijn indices remove the need for name-based variable collection entirely. Phase 1 mitigation: pre-sized `HashSet::with_capacity` based on scheme quantifier count, or `SmallVec`-backed collection for schemes with few variables (the common case). |

**Phase 1:** Backward-compatible optimizations. Baseline: ~113 `Rc::new(Thunk)` calls in eval.rs, ~142 `IndexMap::new()` calls in builtins.rs. Expected impact: 75-85% of addressable allocation cost.

- **Dict literal fast-path** (Nix `maybeThunk`): In `eval_dict`, when `entry.value.node` is `Int|Float|Bool|Str`, create `Materialized` thunks directly instead of wrapping in `Unevaluated`. Eliminates ~40-60% of thunk allocations for config-heavy files. Safe because literals are side-effect-free, deterministic, and don't participate in letrec cycles.
- **String interning**: `HashSet<Rc<str>>` with `Borrow<str>` lookup (avoids key duplication of `HashMap<String, Rc<str>>`). Interns *structural identifiers only* — `Key::String`, variable names, builtin names, and thunk origins. Does NOT intern user data strings (may be large and unique). Reduces key cloning to `Rc::clone` and enables O(1) pointer-equality comparison. Scoped to evaluation session lifetime (lives in `EvalContext`, cleared per `eval_file()`). Production alternative: `lasso::Rodeo` for zero-copy Spur handles.
- **Key cloning reduction**: Eliminate the 2× `String` clone per dict entry in `eval_dict` (once into `dict_env` bindings, once into `dict_map`). Use `entry_mut()` pattern or restructure insert order. ~30% of dict allocation cost.
- **AST cloning reduction**: Change `CallExpr` args from `Spanned<Expr>` to `Rc<Spanned<Expr>>` so `eval_call` can `Rc::clone` instead of deep-cloning entire AST subtrees per argument. ~20-40% of call overhead. Internal refactor to ast.rs and parser.rs; backward-compatible at the public API level.
- **func_label allocation reduction**: `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for the common VarRef case (most calls). Only allocate for DotAccess labels. ~5-10% of call overhead.
- **Capacity hints**: `IndexMap::with_capacity(entries.len())` on all dict construction paths (`eval_dict`, `builtin_drop` Dict path, range access, `builtin_split`).
- **SmallVec**: `SmallVec<[Rc<Thunk>; 4]>` for call args (most calls have ≤4 args), `SmallVec<[StackFrame; 8]>` for error stacks.
- **Origin optimization**: `origin: String` → `Rc<str>` via string interner, with static empty sentinel for the common case.

**Phase 2:** Arena allocation + flat environments, bundled with the recursive-to-iterative evaluator conversion.

- **Arena allocator**: Replace `Rc<Thunk>` with arena-allocated thunks. Recommended approach: index-based arena (`Vec<Thunk>` + `ThunkId` newtype over `usize`) for stable references, bounds-checked indexing, and safe letrec (allocate `ThunkId` slots, fill later, no UB). Alternatives (typed-arena, bumpalo) require unsafe and don't offer clear wins for Tinct's use case.
- **Flat environments with slot indices**: Replace `IndexMap<String, Rc<Thunk>>` chain with flat `Vec` arrays indexed by compile-time (level, slot) pairs (de Bruijn levels). Variable lookup becomes O(1). Environment reuse in function calls becomes trivially safe (each call writes to its own activation frame).
- **Variable resolution pass**: Pre-eval pass assigns (level, slot) indices to every `VarRef`. This pass also enables TCO detection.

**Arena lifetime and persistent values:** The arena lifetime is **one document section** — the text between `---` boundaries (or the entire file for single-section documents). At each `---` boundary, values reachable from the section result are **selectively migrated** from the arena to `Rc`-backed persistent storage, bound as `$$` for the next section, and the section's arena is dropped.

**Selective migration** is a scoped copying pass that preserves thunk state — it translates storage, not evaluation state. Unevaluated thunks stay unevaluated (lazy), Materialized thunks keep their cached values, closures retain their environment chains. The `---` boundary is **not** a strictness point. This preserves the existing lazy pipeline semantics (§Scope Chain Semantics, DOC-PIPELINE): the `---` boundary does not force evaluation.

The migration algorithm traces from `$$` (the section result) and rewrites arena handles to `Rc`-backed storage:

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

Within a section, all thunks are arena-allocated and lazy. Letrec entries reference each other freely within the arena. At `---`, only thunks reachable from `$$` are migrated — unreachable intermediate thunks (temporaries, shadowed bindings) are reclaimed when the arena drops.

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
| CLI (multi-section) | Per section | `$$` (selectively migrated) | Arena per section, migrate at `---` |
| REPL | Per input | `$$` (selectively migrated) | Each input is implicitly a section |
| LSP | Per section | `$$` (selectively migrated) | Editing section N re-evaluates N+ with cached `$$` from N-1 |

**Cost model:** Migration is O(thunks reachable from `$$`), not O(total section thunks). For sections where `$$` is a small result derived from large intermediate computations, migration cost is much lower than deep-materialization. For sections where most thunks are reachable from `$$`, cost approaches deep-materialization minus the forcing cost (migration copies state; deep-materialization evaluates).

**Rejected alternatives:** (1) Session-scoped arena — unbounded memory growth during long REPL sessions; requires stop-the-world compaction with pointer fixup across all live references. (2) Hybrid arena+Rc — two allocation paths; every thunk creation must decide arena vs Rc; closures capturing thunks make escape analysis intractable. (3) Deep-materialization at `---` — changes language semantics (lazy→eager), breaks closures (env chains hold dangling arena handles after drop), and diverges on infinite sequences in `$$`. (4) Per-eval copy-out without section granularity — forces materialization of intermediate values within a section, losing laziness benefits.

**LSP incremental re-evaluation:** Migrated `$$` values are self-contained `Rc`-backed storage with no arena references. The LSP caches `$$` per section. Editing section N re-uses cached `$$` from section N-1 (already migrated, no re-evaluation) and re-evaluates only sections N through the end.

**`$include` interaction:** Included files are evaluated in their own arena. The include cache stores migrated results — the cache outlives any single section's arena. An `$include` call returns an already-migrated `Rc`-backed value, which is arena-independent and can be used freely across sections. This creates a controlled one-way dependency within sections: arena-allocated thunks may reference `Rc`-backed `$include` results, but never the reverse. This is structurally determined (section-local = arena, imported = Rc) and does not require per-thunk escape analysis — the "hybrid arena+Rc" alternative (rejected above) fails because it requires per-thunk decisions, not because mixing storage backends is inherently unsound.

**Rationale:** The iterative evaluator shares prerequisites with arena allocation — both require explicit frame management and compile-time analysis. Bundling avoids two separate invasive refactors. Phase 1 captures 75-85% of addressable allocation wins with near-zero risk. Profiling data from Phase 1 guides whether Phase 2's arena is necessary.

**Measurement plan:** Phase 1 must establish baseline metrics before and after optimization: total allocations per eval (count `Rc::new`, `IndexMap::new`, `Vec::new`), peak memory usage (heaptrack RSS on dict-heavy and deeply-nested workloads), and allocation hotspots (which paths account for >10% of allocations). Decision threshold for Phase 2: if Phase 1 achieves >80% allocation reduction, defer Phase 2 indefinitely; if <50%, proceed.

**Key tradeoff:** Environment lookup stays O(depth) until Phase 2, but string interning makes each lookup step cheaper (pointer comparison vs byte comparison), and the literal fast-path reduces total thunk allocations significantly.

**Precedent:** Nix uses flat `Value*[]` arrays with de Bruijn levels and Boehm GC. Jsonnet uses GC heap with flat bindings. Nickel uses `Rc<RefCell<Closure>>` (same as Tinct's current approach). Phase 1 keeps Tinct at Nickel's level; Phase 2 moves toward Nix's level.

**Constraint:** Phase 2's arena model must handle letrec self-reference safely in Rust (thunk slots allocated before fill, no dangling pointers). The safe Rust arena patterns are analyzed in `doc/whatif/arena-patterns.md` — the recommended approach is an index-based arena (`Vec<Thunk>` + `ThunkId` handles), following the cranelift entity pattern.

## Iterative Evaluator (CEK Machine)

**Decision:** Replace the recursive `eval()` / `materialize()` call stack with an iterative CEK machine (Control-Environment-Kontinuation). Continuations are defunctionalized — each closure that CPS would create becomes a variant in a `Cont` enum, stored in a `Vec<Cont>` stack.

**Problem:** `eval()` and `materialize()` are mutually recursive across 8+ call patterns. Deeply-nested lazy chains exhaust the Rust call stack before `MAX_EVAL_DEPTH` fires. Tinct works around this with a 64MB worker thread stack.

**Architecture:** Two enums, one loop.

`Action` represents what to do now (the "control" register):

```rust
enum Action {
    Eval { expr: Rc<Spanned<Expr>>, env: Rc<RefCell<Environment>>, depth: usize },
    Materialize { thunk: Rc<Thunk>, mat_span: Option<Span>, depth: usize },
    Continue(Result<Value, Box<EvalError>>),
}
```

`Cont` represents what to do with the result (the reified continuation / "kontinuation" stack):

```rust
enum Cont {
    // eval() continuations — access chains
    DotAccessForce { field: String, span: Span, depth: usize },
    BracketForceTarget { key_expr: Rc<Spanned<Expr>>, env: ..., span: Span, depth: usize },
    BracketForceKey { target: Value, span: Span },  // not yet implemented; key eval is synchronous via eval_key()
    RangeForceTarget { start_expr: ..., end_expr: ..., env: ..., span: Span, depth: usize },
    RangeForceStart { target: Value, end_expr: ..., env: ..., span: Span, depth: usize },
    RangeForceEnd { target: Value, start: Value, span: Span },

    // eval() continuations — calls and type assertions
    CallForceFunc { args: Box<Vec<Rc<Thunk>>>, named: Box<IndexMap<...>>, env: ..., span: Span, depth: usize, label: String },
    TypeAssertCheck { annotation: ..., env: ..., span: Span, depth: usize },
    TypeAssertForce { type_expr: ..., default_expr: Option<...>, env: ..., span: Span, depth: usize },

    // eval() continuations — dict construction
    DictBuildKey { value_expr: Rc<Spanned<Expr>>, remaining: ..., env: ..., span: Span, depth: usize },

    // eval() continuations — function defaults
    BindArgDefault { param: String, remaining_params: ..., env: ..., depth: usize },

    // materialize() continuations
    Memoize { thunk: Rc<Thunk>, mat_span: Option<Span>, origin: String },
    PendingBuiltinForceResult { thunk: Rc<Thunk>, mat_span: Option<Span>, ... },
    PendingCallForceFunc { thunk: Rc<Thunk>, args: Box<Vec<Rc<Thunk>>>, call_span: Span, ... },
    PendingCallForceResult { thunk: Rc<Thunk>, mat_span: Option<Span>, ... },

    // Document pipeline
    DocumentScope { remaining: Vec<Spanned<Expr>>, env: ..., depth: usize },

    // Deep materialization
    DeepEntries { map: Rc<IndexMap<Key, Rc<Thunk>>>, idx: usize, ... },
    DeepSeqTail { tail: Rc<Thunk>, ... },
}
```

Large fields in `CallForceFunc` and `PendingCallForceFunc` are boxed to keep the `Cont` enum ≤96 bytes. `DeepEntries` holds an `Rc` to the original map plus an index rather than cloning entries into a `Vec`.

The main loop is a two-register machine — `action` (what's happening now) and `stack` (what's waiting):

```rust
fn run(initial: Action) -> Result<Value, Box<EvalError>> {
    let mut stack: Vec<Cont> = Vec::with_capacity(64);
    let mut action = initial;

    loop {
        action = match action {
            Action::Eval { expr, env, depth } => {
                match &expr.node {
                    Expr::Int(n) => Action::Continue(Ok(Value::Int(*n))),
                    Expr::DotAccess { expr, field } => {
                        stack.push(Cont::DotAccessForce { field, span, depth });
                        Action::Eval { expr, env, depth }
                    }
                    // ...
                }
            }
            Action::Materialize { thunk, mat_span, depth } => {
                match /* thunk state */ {
                    Materialized(v) => Action::Continue(Ok(v.clone())),
                    Failed(e) => Action::Continue(Err(e.clone())),
                    Unevaluated { expr, env } => {
                        stack.push(Cont::Memoize { thunk, mat_span, origin });
                        Action::Eval { expr, env, depth: depth + 1 }
                    }
                    PendingCall { func, args, named, call_span, caller_env, ctx } => {
                        stack.push(Cont::PendingCallDispatch { thunk, args, named, call_span, caller_env, ctx });
                        Action::Materialize { thunk: func, mat_span, depth: depth + 1 }
                    }
                    // ... (PendingBuiltin, Guarded, InProgress)
                }
            }
            Action::Continue(result) => {
                match stack.pop() {
                    None => return result,
                    Some(cont) => /* dispatch on cont, produce next Action */
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

**deep_materialize:** Currently implemented as a separate recursive function in `eval_deep.rs`, calling `materialize()` per dict entry and seq element with cycle detection and sharing preservation via a `HashMap` cache. The target architecture expresses this as `DeepEntries` and `DeepSeqTail` continuations within the CEK loop, eliminating the separate recursive helper. Migration is planned for after `materialize()` is subsumed by `run()` (iterative-eval-b5).

**Tail-call optimization:** In tail position (e.g., last expression in a function body), set `action = Action::Eval { body, ... }` without pushing a `Cont`. The current frame is reused. TCO for recursive stdlib functions (`fold`, `map`, `filter`) follows the same pattern: detect tail calls during the variable resolution pass, mark them, and skip the continuation push. TCO applies to user-defined function calls only. Builtin calls always push a continuation — builtins rely on `PendingBuiltin` thunk deferral for lazy behavior, not tail-call elimination.

**Error stack traces:** Walk `Vec<Cont>` to reconstruct the call stack. Each `Cont::CallForceFunc` carries the call-site span and label, replacing the current `EvalError::stack` vector. This gives precise "materialized at" context for every frame in the stack.

**Cont variant count:** ~18-20 variants, one per continuation point in the current recursive evaluator. Each variant stores only its specific continuation data (Rc pointers + Span + small fields). Target frame size: ≤96 bytes per Cont (achieved by boxing large fields in the biggest variants).

**Relationship to allocation strategy:** This design is Phase 2 of the allocation strategy. Arena allocation and flat environments integrate naturally: `Cont` variants hold `ThunkId` handles into the arena, and the `Vec<Cont>` stack's lifetime defines the arena's lifetime scope.

**Precedent:** Jsonnet's VM uses 22 `FrameKind` variants with a value register (production-tested at Google). Nickel uses an iterative stack machine with `OpFirst`/`OpSecond` continuations (production Rust). Both are defunctionalized CPS machines. The theoretical foundation is Felleisen & Friedman's CEK machine.

**Recursive call sites being converted:**

| Current recursive call | Becomes |
|----------------------|---------|
| `eval()` → `eval()` (TypeAssert, desugar, defaults) | `Action::Eval` + `Cont::TypeAssertCheck` etc. |
| `eval_call()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::CallForceFunc` |
| `eval_dot_access()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DotAccessForce` |
| `eval_bracket_access()` → `eval()` + `materialize()` ×2 | `Action::Eval` + `Cont::BracketForceTarget` → `Cont::BracketForceKey` |
| `eval_range_access()` → `materialize()` ×3 | `Action::Materialize` + `Cont::RangeForceTarget` → `Cont::RangeForceStart` → `Cont::RangeForceEnd` |
| `eval_dict()` → computed key materialization | `Action::Eval` + `Cont::DictBuildKey` |
| `eval_document()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::DocumentScope` (`$$` bound as `Unevaluated` thunk, never materialized) |
| `bind_args_thunks()` → default eval | `Action::Eval` + `Cont::BindArgDefault` |
| `materialize()` → `eval()` + `materialize()` | `Action::Eval` + `Cont::Memoize` |
| `materialize()` → builtin call + `materialize()` | Builtin dispatch + `Cont::PendingBuiltinForceResult` |
| `materialize()` → `materialize()` (PendingCall) | `Action::Materialize` + `Cont::PendingCallForceFunc` → `Cont::PendingCallForceResult` |
| `deep_materialize()` → `materialize()` + recurse | `Action::Materialize` + `Cont::DeepEntries` / `Cont::DeepSeqTail` (within CEK loop, no separate helper) |
