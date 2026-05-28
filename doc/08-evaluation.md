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

**All non-literal key expressions force eagerly.** While literal keys (`Int`, `Float`, `Bool`, `Str`) use the `Materialized` fast-path without creating thunks, ALL non-literal key expressions (including pure computations like `[+ 1 2]` or effectful operations like `$include`) are forced to concrete `Key` values at dict construction time. This is necessary because `IndexMap<Key, ThunkId>` requires concrete keys before letrec scoping can proceed. The evaluator cannot defer key computation — it must produce a `Key::Int(n)` or `Key::String(s)` value immediately to insert the entry into the dict.

**Effectful key expressions:** Computed keys may contain effectful operations (such as `$include`). These effects execute in the parent scope context, not the dict's letrec scope. For example, `[include "keys.llt"]` in a dict key position evaluates the included file with access to the parent environment's bindings, not the dict's own entries. This is consistent with the scoping rule but means included files used as keys cannot reference the dict's own bindings.

**Circular dependencies** are detected at materialization-time and reported with a clear cycle trace.

**Self-referencing entries (`[k: k]`):** Because dict entries use letrec scoping, writing `[name: name]` does **not** capture an outer-scope binding called `name` — it creates a circular self-reference where `name` resolves to itself within the dict environment. This is a common mistake when constructing dicts from outer-scope variables:

```tinct
[fn [name doc sig]
    [name: name  doc: doc  sig: sig]]   # BUG: each entry refers to itself
```

The type checker emits a T002 warning for bare `[k: k]` entries. To reference an outer-scope variable with the same name, alias it first or use a different parameter name:

```tinct
# Option 1: use different parameter names
[fn [n d s]
    [name: n  doc: d  sig: s]]

# Option 2: alias before the dict
[fn [name doc sig]
    [let n name  d doc  s sig]
    [name: n  doc: d  sig: s]]
```

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

## Runtime-Injected Capability Bindings

The tinct runtime automatically injects **capability bindings** into the root environment before evaluation begins. These variables provide controlled access to system resources and are distinguished from user-defined variables by the `%` sigil.

**Standard capability bindings:**

| Binding | Type | Description | Suppression Flag |
|---------|------|-------------|------------------|
| `%cwd` | `DirCap` | Process working directory at invocation time | `--no-cwd` |
| `%libdir` | `DirCap` | Standard library directory for `[include %libdir "module.llt"]` | `--no-libdir` |
| `%stdin` | `Handle[Readable Text]` | Standard input stream handle | Only injected when `-i` flag is present |

**Injection mechanism:**

These bindings are added to the root environment by the CLI before the first expression in the user's program evaluates. They are not declared by the user — they appear as pre-existing bindings visible to all top-level expressions.

```tinct
# User code can reference capability bindings directly
config: [include %libdir "config.llt"]
project-files: [list %cwd]
user-input: [slurp %stdin]  # Only if -i flag was used
```

**The `%` sigil convention:**

User-defined variables use the `$` sigil (`$x`, `$config`, `$result`). Runtime-injected capabilities use `%` to signal that these bindings come from the execution environment, not the program source. This distinction makes it clear at a glance which variables are under user control and which are provided by the runtime.

**Suppression flags:**

Each capability binding can be suppressed via CLI flags:

- `--no-cwd` — suppresses `%cwd` injection. Any attempt to reference `%cwd` produces an "undefined variable" error.
- `--no-libdir` — suppresses `%libdir` injection. Standard library `include` directives will fail unless using absolute paths or user-injected capabilities.
- `-i` flag absence — `%stdin` is only injected when the `-i` (interactive input) flag is present. Without `-i`, referencing `%stdin` produces an "undefined variable" error.

**Fully sandboxed invocation:**

```bash
llt eval --no-cwd --no-libdir script.llt
# %cwd and %libdir unavailable
# %stdin also unavailable (no -i flag)
# Only builtins and user-defined bindings accessible
```

**User-injected capabilities:**

In addition to the standard capability bindings, users can inject custom directory and network capabilities via `--cap-fs` and `--cap-net` flags:

```bash
llt eval --cap-fs pkg=/var/lib/plugins --cap-net api=schema.internal script.llt
# Injects %pkg (DirCap) and %api (NetCap) into root environment
```

These user-injected capabilities also use the `%` sigil and follow the same scoping rules as standard capability bindings.

**Type checking:**

The type checker pre-seeds the type environment with capability variable types so that references to `%cwd`, `%libdir`, and `%stdin` (when `-i` is present) do not produce "undefined variable" warnings during type inference. This seeding occurs in `build_prelude_env_inner()` and `build_type_env()` in `src/imports.rs`.

## Sequences and Lazy Computation

**Sequences are lazy computations, not data.** Dicts are data (finite, random-access, known keys). Sequences are suspended computations that produce elements on demand (possibly infinite, sequential access, unknown structure).

This distinction preserves the "everything is a dict" invariant for data while enabling lazy, composable pipelines for computation.

**Runtime representation:**

A sequence is a cons cell: a head value and a tail that is itself a sequence (or empty dict `[]` for end-of-sequence).

```text
Value::Seq { head: ThunkId, tail: ThunkId }
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
| Continuation stack limit | `MAX_CONTINUATION_STACK = 2048` | Runaway recursion: deeply nested or diverging evaluation chains (iterative CEK machine) |
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

`$seq` is lazy — it does not materialize its arguments (`builtin_seq` allocates each argument into the arena via `ctx.alloc_thunk(Arc::clone(&args[N]))` and stores the resulting `ThunkId` in `Value::Seq { head, tail }` without forcing either thunk). This is critical: it means `$seq` acts as a guard in the coinductive sense, allowing corecursive definitions that would cycle under eager evaluation.

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

The valid state transitions form an almost-acyclic directed graph — nearly all transitions move strictly forward, with four backward edge exceptions: `InProgress → Guarded`, `InProgress → Unevaluated`, `InProgress → PendingBuiltin`, and `InProgress → PendingCall`, all of which restore state when a non-cacheable error (typically `DepthExceeded`) occurs and the thunk must be retried (see Exception below).

```text
Placeholder ───────────────────────────────→ {any non-InProgress state}

Unevaluated ──────────┐
PendingBuiltin ────────┤
PendingCall ───────────┼──→ InProgress ──┬──→ Materialized
Guarded ──────────────┘                 └──→ Failed ⟲
```

The transition graph governs state *transitions*, not construction. Thunks may be constructed directly in Placeholder state (via `Thunk::new_placeholder()`), Unevaluated, PendingBuiltin, PendingCall, Guarded, or Materialized state (via `Thunk::new_materialized`). The transition graph applies only to subsequent state changes.

Transition rules (each maps to one `take_*`, `set_materialized`, `restore_unevaluated`, or `cache_failure` call in `src/value.rs`):

| Transition | Trigger | Atomicity |
|-----------|---------|-----------|
| Placeholder → {any non-InProgress state} | construction-time direct write | Direct write — pre-construction sentinel only; materializing a `Placeholder` thunk returns a `CircularDependency` error (the runtime treats Placeholder identically to InProgress — see `is_in_progress()` in `value.rs`). Legal targets: Unevaluated, PendingBuiltin, PendingCall, Guarded, Materialized. InProgress is excluded because it would trigger cycle detection on the next materialization attempt. |
| Unevaluated → InProgress | `take_unevaluated()` | Atomic (`mem::replace`) |
| PendingBuiltin → InProgress | `take_pending_builtin()` | Atomic (`mem::replace`) |
| PendingCall → InProgress | `take_pending_call()` | Atomic (`mem::replace`) |
| Guarded → InProgress | `take_guarded()` | Atomic (`mem::replace`) |
| InProgress → Materialized | `set_materialized(v)` | Direct write (clears `unevaluated`, writes `Ok(v)` to result OnceCell) |
| InProgress → Failed | `cache_failure(err)` | Direct write (clears `unevaluated`, sets `result` via OnceCell) |
| InProgress → Unevaluated | `restore_unevaluated(UnevaluatedState::Unevaluated { ... })` | Direct write — **backward edge**, non-cacheable errors only; restores original state for retry |
| InProgress → Guarded | `restore_unevaluated(UnevaluatedState::Guarded { ... })` | Direct write — **backward edge**, non-cacheable errors from builtins only; restores original state to allow retry |
| InProgress → PendingBuiltin | `restore_unevaluated(UnevaluatedState::PendingBuiltin { ... })` | Direct write — **backward edge**, non-cacheable errors only; restores original state for retry |
| InProgress → PendingCall | `restore_unevaluated(UnevaluatedState::PendingCall { ... })` | Direct write — **backward edge**, non-cacheable errors only; restores original state for retry |
| Failed → Failed | `cache_failure(e')` | Direct write (diagnostic refinement only — enriches materialization spans and stack frames) |

**Monotonicity proof sketch:** `Placeholder` is a pre-construction sentinel — it sits below all other states in the construction-time ordering. It is not part of the materialization path: materializing a `Placeholder` thunk panics rather than transitioning through InProgress. `Placeholder` transitions directly to any non-InProgress state at allocation time, establishing the thunk's initial materialization state before any evaluation begins. This is a pure construction-time concept and does not interact with the Launchbury monotonicity argument below.

The materialization graph (excluding `Placeholder`) has no cycles. Four backward edges exist (InProgress → Unevaluated, InProgress → Guarded, InProgress → PendingBuiltin, InProgress → PendingCall), all acyclic: InProgress cannot cycle back through any of the four deferred states because none of those states transition to InProgress without first transitioning forward to Materialized or Failed on their own path. Each source state (Unevaluated, PendingBuiltin, PendingCall, Guarded) transitions only to InProgress. InProgress transitions only to Materialized or Failed — with one exception: the four backward edges for non-cacheable errors from deferred states (see Exception below); these preserve semantic monotonicity because the thunk's observable meaning is unchanged between retries. Materialized is terminal — no transitions out. Failed has a self-edge for diagnostic refinement (enriching materialization spans and stack frames), but the error's semantic identity is fixed — only diagnostic metadata may be updated. Therefore all transition sequences are finite, and the semantic content of a thunk is monotonically determined. ∎

**Exception — retryable non-cacheable errors:** Four backward edges exist for non-cacheable errors from deferred-computation states:

- `InProgress → Unevaluated`: fires when an Unevaluated thunk's expr evaluation fails with a non-cacheable error; restores the original Unevaluated state for retry.
- `InProgress → Guarded`: fires when a Guarded thunk's inner materialization fails with a non-cacheable error (see `[MATERIALIZE-GUARD-NONCACHEABLE]`).
- `InProgress → PendingBuiltin`: fires when a PendingBuiltin's execution raises a non-cacheable error; restores the original PendingBuiltin state for retry.
- `InProgress → PendingCall`: fires when a PendingCall's invocation raises a non-cacheable error; restores the original PendingCall state for retry.

All four fire under the same condition: any non-cacheable error (e.g., `DepthExceeded`) from a deferred state restores that state for retry. `DepthExceeded` can be raised by the continuation stack depth guard (`MAX_CONTINUATION_STACK = 2048` frames, `src/eval_materialize.rs:309`) inside the core CEK loop, and by individual builtins (e.g., `MAX_COLLECT_SIZE` in `$collect`). Because such errors are transient resource-bound conditions (not semantic errors), they are non-cacheable — `cache_failure` is skipped and the thunk is restored to its pre-InProgress state so the computation can be retried. These backward restorations mean strict state-order monotonicity does not hold for the non-cacheable path. However, semantic monotonicity is preserved: the thunk's observable meaning is unchanged between attempts, and the error identity is not fixed. Every other error kind is cacheable and takes the normal `InProgress → Failed` forward edge. (`src/eval_materialize.rs`, in the `force_step()` match arms for `Unevaluated`, `Guarded`, `PendingBuiltin`, and `PendingCall`)

**Atomicity invariant:** Each `take_*` method atomically swaps the thunk state to InProgress before returning the captured data. This ensures no observer can see the old state after the transition begins. The atomicity is provided by `std::mem::replace` under an exclusive `borrow_mut()` — Rust's borrow checker prevents double borrows within a single thread.

### Part 2: Materialization Rules

Materialization dispatches on the current state to produce a value or error. Rules use two judgment forms: `materialize(θ) ⇒ v` where θ is a thunk and v is the resulting value; and `eval(e, ρ, Σ) ⇒ θ` where e is an expression, ρ is the lexical environment, Σ is the EvalContext (base directory, include guards, stdlib env), and θ is the resulting thunk. The EvalContext Σ is captured inside each thunk at construction time (written Σ_θ when referencing a specific thunk's context) and is not a parameter of `materialize` — it is part of the thunk's closure.

**Notation:** The rules use an implementation-oriented notation mixing imperative state updates (`θ.state ← InProgress`) with declarative judgments (`eval(expr, env, Σ_θ) ⇒ θ'`). `Σ_θ` denotes the evaluation context (`EvalContext`) captured at thunk construction time — it carries context-dependent state (base directory, include guards) that must reflect the thunk's definition site. A standard operational semantics would thread an explicit store σ mapping thunk IDs to states: `materialize(θ, σ) ⇒ (v, σ')`. The notation here maps directly to the `materialize()` implementation for ease of cross-checking.

**Depth tracking:** The iterative CEK machine (see §Iterative Evaluator below) has replaced recursive depth tracking with a bounded continuation stack. There is no `MAX_EVAL_DEPTH` check in the core materialization loop — the heap-allocated continuation stack (`Vec<Cont>`) replaces the Rust call stack and is bounded by `MAX_CONTINUATION_STACK = 2048` frames (`src/eval_materialize.rs:33`, ~192 KB). Sequence iteration guards in builtins use `MAX_COLLECT_SIZE` (1,000,000), a separate constant for preventing unbounded sequence collection.

**[MATERIALIZE-CACHED]**

```text
θ.state = Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

**[MATERIALIZE-FAILED]**

```text
θ.state = Failed(e)
───────────────────────────
materialize(θ) ⇒ error(e')
```

The materialization span update has three cases (`eval.rs:876-896`): (1) if e has no materialization span and one is available, set it; (2) if the access span matches the existing materialization span, no-op; (3) if the access span differs and is not already in the stack, add it as a stack frame (preserving the original materialization span). This Failed → Failed diagnostic refinement is an intentional relaxation of strict idempotence at the error-representation level — the error's identity and root cause are fixed, but diagnostic annotations accumulate across access paths.

**[MATERIALIZE-CYCLE]**

```text
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

```text
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ v
v ∈ τ                                          (validate)
θ.state ← Materialized(v)
───────────────────────────
materialize(θ) ⇒ v
```

**[MATERIALIZE-GUARD-INNER-ERR]** — inner thunk materialization fails with a cacheable error:

```text
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ error(e)    where e.is_cacheable()
θ.state ← Failed(e)                           (memoize; propagation error, not type mismatch)
───────────────────────────
materialize(θ) ⇒ error(e)
```

**[MATERIALIZE-GUARD-NONCACHEABLE]** — inner thunk materialization fails with a non-cacheable error (e.g., DepthExceeded from a builtin):

```text
θ.state = Guarded(θ_inner, τ, path, span)
θ.state ← InProgress
materialize(θ_inner) ⇒ error(e)               where ¬e.is_cacheable()
θ.state ← Guarded(θ_inner, τ, path, span)     (restore — retry possible)
───────────────────────────
materialize(θ) ⇒ error(e)
```

Note: `DepthExceeded` can arise from the continuation stack depth guard (`check_stack_depth()` enforcing `MAX_CONTINUATION_STACK = 2048` frames) inside the CEK loop, and from individual builtins (e.g., `MAX_COLLECT_SIZE` in `$collect`). The backward `InProgress → Guarded` edge handles non-cacheable errors from both sources.

[MATERIALIZE-GUARD-NONCACHEABLE] fires when the inner thunk's materialization fails with a non-cacheable error (e.g., DepthExceeded from a builtin). The Guarded state is restored because non-cacheable errors are transient resource-bound conditions, not semantic errors. (`src/eval_materialize.rs`, in the `Guarded` arm of `force_step()`)

**[MATERIALIZE-GUARD-TYPE-ERR]** — inner thunk succeeds but value does not inhabit the expected type:

```text
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

```text
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

```text
θ.state = Unevaluated(expr, env, Σ_θ)
θ.state ← InProgress
eval(expr, env, Σ_θ) ⇒ θ'
materialize(θ') ⇒ error(e)
θ.state ← Failed(e)                           (memoize error)
───────────────────────────
materialize(θ) ⇒ error(e)
```

**[MATERIALIZE-BUILTIN]**

```text
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

```text
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

```text
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
| **Monotonicity** | Satisfied with exceptions | transition graph has four backward edges (`InProgress → Guarded/Unevaluated/PendingBuiltin/PendingCall`) for non-cacheable errors from builtins (retry semantics); Failed self-edge refines diagnostics only (proven above) |
| **Adequacy** | Holds for extensions | PendingBuiltin/PendingCall are observationally equivalent to Unevaluated (defunctionalization preserves semantics). Guarded is observationally equivalent to an Unevaluated thunk that materializes and validates (proxy contract). Failed extends the codomain from Value⊥ to Value + Error⊥ (absorbing, deterministic) |
| **Confluence** | Pure subset only | `$include` makes evaluation order observable; in the pure subset, materialization order does not affect final values |
| **Sharing preservation** | Satisfied | `Arc<Thunk>` ensures identity-based sharing; the CEK machine preserves thunk identity through continuation dispatch |

### Semantic Commitments

Implicit decisions in the current implementation, made explicit:

**1. Error memoization is permanent.** Once a thunk reaches Failed, it never retries. This includes I/O failures from `$include` — a file-not-found error is cached forever, even if the file appears later. This is correct for a build-time evaluator (deterministic builds) and matches Nix's `nFailed` semantics (Peyton Jones et al. 1999 "imprecise exceptions"). Retryable failures would require a new `Retryable` state or external retry logic.

**2. Confluence holds only in the pure subset.** `$include` introduces evaluation-order dependence: if file A includes file B and file B includes file A, the result depends on which is evaluated first (cycle detection fires on the second). All other tinct operations are confluent — materialization order does not affect the result. The pure subset of tinct (no `$include`) satisfies the diamond property of Ariola & Felleisen's (1997) call-by-need calculus.

**3. No recursive depth limit in core evaluator.** The iterative CEK machine uses a heap-allocated continuation stack (`Vec<Cont>`), eliminating the `MAX_EVAL_DEPTH` bound that existed in the recursive evaluator. There is no depth parameter in `materialize()` or `eval()`. Individual builtins may impose their own limits (e.g., `MAX_COLLECT_SIZE` for sequence collection in `$collect`), but these are domain-specific bounds, not a global recursion limit. (Note: `CoreSurfaceExpression::Sequential` and `CoreExpr::Match` currently use async recursion rather than CEK continuations; see `cek-match-sequential-rust-stack` in TODO.md. Also: `DepthExceeded` as an `ErrorKind` still exists — see [Errors](10-errors.md) §Error Categories — because individual builtins may raise it for domain-specific resource limits. The eliminated limit is `MAX_EVAL_DEPTH` from the recursive evaluator call stack.)

**4. Finite vs productive thunk lifecycles.** Dict-entry thunks have a **finite lifecycle**: they must eventually reach Materialized or Failed. Seq tail thunks have a **productive lifecycle**: materializing a tail yields a Seq value (containing a new tail thunk) or the terminal `[]`. The state machine is identical; the liveness obligation differs. This distinction is not enforced by the type system — it is a semantic contract between the sequence constructors and the programmer (see §Productivity Obligations).

### Adequacy of PendingBuiltin and PendingCall

These states are defunctionalized continuations (Reynolds 1972). Each is observationally equivalent to an Unevaluated thunk holding an expression that would perform the same computation:

- `PendingBuiltin(f, args, named, cs, Σ_θ)` ≡ `Unevaluated([f ...args ...named], env, Σ_θ)` where env binds the arg thunks
- `PendingCall(f_θ, args, named, cs, caller_env, Σ_θ)` ≡ `Unevaluated([call <materialize f_θ> ...args ...named], env, Σ_θ)`
- `Guarded(θ_inner, τ, path, span)` ≡ `Unevaluated(<materialize θ_inner then validate ∈ τ>, env, Σ_θ)` — a proxy contract monitor (Findler & Felleisen 2002)

The equivalence for PendingCall holds because `eval` of `[call ...]` already performs dynamic dispatch on the callee — if `f_θ` materializes to a Builtin rather than a Function, both the PendingCall path (MATERIALIZE-CALL-BUILTIN) and the hypothetical Unevaluated path would dispatch to the same builtin.

The difference is operational: PendingBuiltin/PendingCall avoid constructing AST nodes for deferred computations. A formal adequacy proof would show bisimulation: every materialization sequence starting with `PendingBuiltin(f, args, ...)` produces the same value as materializing `Unevaluated([f ...args], env)`. This is conjectured based on the defunctionalization correspondence (Reynolds 1972; Danvy & Nielsen 2003) but not mechanically verified.

### Relationship to CEK Machine Migration

The iterative evaluator (§Iterative Evaluator) uses explicit `Cont` variants on the continuation stack to process thunk state transitions. The CEK machine does not remove PendingBuiltin and PendingCall — these are permanent design elements representing persistent deferred computation:

- **PendingBuiltin** stores deferred builtin calls for lazy sequences (`$map`, `$filter`, `$fold_step`, etc.) and proxy handler dispatch. Cannot be replaced by Unevaluated because builtin function pointers (`BuiltinFn`) have no AST representation. Lazy sequences need persistent storage for deferred steps.
- **PendingCall** stores deferred function calls for lazy dispatch and tail-call optimization. Represents work already done by `eval_call` (evaluated func_expr, wrapped args) that Unevaluated would duplicate.
- The monotonicity proof and semantic properties remain unchanged — the 7-state transition graph (Unevaluated, PendingBuiltin, PendingCall, Guarded, InProgress, Materialized, Failed) is the stable design.
- **Sharing preservation is the critical migration invariant**: thunk identity (`Arc<Thunk>` pointer) must be preserved through continuation dispatch. A materialized thunk must be the same allocation that was created at the definition site.
- The iterative CEK machine uses heap-allocated continuations with no hardcoded depth bound

## Error Reporting

Error semantics are specified in [Error Handling](10-errors.md). This section summarizes the key concepts; see doc/10 for formal rules and implementation mappings.

**Dual-span model:** Every error carries a definition site (where the error-producing expression was written) and a materialization site (where a consumer materialized the thunk that failed). The `attach_materialization_context` function decorates errors with these spans during propagation through the `map_err(&decorate)` chain.

**Stack frame accumulation:** When an error propagates through multiple materialization layers (e.g., `θ₁ → θ₂ → θ₃`), each layer adds a stack frame via DECORATE (doc/10 §Part 3). The first materialization site becomes `mat_span`; subsequent sites become stack frames. Deduplication guards prevent redundant frames.

**Error caching:** Cacheable errors (all except `DepthExceeded`) are memoized in `Failed` state via `cache_failure()`. Subsequent access returns the cached error with additional materialization context. Non-cacheable errors (`DepthExceeded` from builtins) restore the thunk to its original state, allowing retry. See MEMO-CACHE and MEMO-SKIP rules in doc/10 §Part 5.

**Error condition specifications:** The trigger conditions for all `ErrorKind` variants (when each error is raised) are documented in [Error Handling](10-errors.md) §Part 2: Error Sources. Propagation rules (PROP-EVAL, PROP-BUILTIN, PROP-RESULT, PROP-CYCLE, PROP-DEPTH) are in doc/10 §Part 4.

## Selective Materialization — Formal Specification

Specifies which arguments each Rust-native builtin materializes before execution and how the result is constructed. This is a two-tier specification: a **strictness signature table** covering the core evaluation and collection builtins (auditable summary), plus **delta rules** for builtins whose materialization behavior cannot be captured by a flat per-argument annotation. I/O, capability, datetime, crypto, and network builtins are omitted as inherently materializing. See `doc/11a-builtins.md` for the full catalog of registered builtins (count verified by `standard_builtins_count` test in `src/builtins.rs`).

The signature notation draws on Mycroft's (1981) abstract interpretation framework for strictness analysis. The delta rules follow Plotkin's (1981) structural operational semantics, using the same judgment style as §Thunk Lifecycle — Formal Specification.

### Part 1: Strictness Signature Notation

Each builtin receives a per-argument strictness annotation and a result classification:

**Input strictness (per argument position):**

| Symbol | Meaning | Implementation pattern |
|--------|---------|----------------------|
| `I` | Identity — argument passes as-is; builtin inspects thunk state without forcing | CEK machine does NOT auto-materialize; builtin receives the raw thunk |
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

**Identity strictness (`I`):** Used by `ast-of` (see §Runtime Reflection) to inspect thunk state without forcing. The CEK machine does not auto-materialize `I`-annotated arguments — the builtin receives the raw thunk and branches on its state (Materialized, Unevaluated, or Pending). This enables runtime reflection without triggering side effects.

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
| `include` | `DirCap → String → Any` | Materializing (I/O); content-addressed cache; tinct-defined in prelude |
| `load` | `String → Dict` | Non-materializing (pure parse) |
| `expand` | `Dict → Dict` | Non-materializing (pure transform) |
| `eval` | `Dict × Any × Dict → Any` | Materializing (evaluation) |

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

```text
materialize(θ_cond) ⇒ true
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], cs) ⇒ θ_then
```

**[DELTA-IF-FALSE]**

```text
materialize(θ_cond) ⇒ false
───────────────────────────
δ(if, [θ_cond, θ_then, θ_else], cs) ⇒ θ_else
```

**Branch isolation guarantee:** The unchosen branch is never materialized. `θ_then` and `θ_else` are returned via `Rc::clone` — no state transition occurs on the unchosen thunk. This is the foundational selective materialization property from which `$and`, `$or`, `$when`, `$unless`, and `$cond` derive their short-circuit behavior (see Part 5). The chosen branch thunk is returned to the caller; its subsequent materialization happens via MATERIALIZE-BUILTIN in §Thunk Lifecycle, which calls `materialize(θ')` on the builtin's result — the separation between "builtin execution" and "result materialization" is what makes `$if`'s laziness guarantee possible.

**[DELTA-SEQ]**

```text
───────────────────────────
δ(seq, [θ_head, θ_tail], cs) ⇒ Materialized(Seq(Rc::clone(θ_head), Rc::clone(θ_tail)))
```

No arguments are materialized. Both pass through as thunks within the Seq value. This is the coinductive guard — `$seq` enables corecursive definitions by deferring evaluation of both head and tail.

**[DELTA-HEAD]**

```text
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(head, [θ_xs], cs) ⇒ θ_h
```

**[DELTA-TAIL]**

```text
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
───────────────────────────
δ(tail, [θ_xs], cs) ⇒ θ_t
```

DELTA-HEAD and DELTA-TAIL materialize the container to verify it is a Seq, but return the extracted thunk *without materializing it*. The head/tail thunk retains its original state (Unevaluated, PendingCall, etc.). Empty dict `[]` as input produces a specific error (`"head/tail on empty sequence"`).

**[DELTA-COLLECT-EMPTY]**

```text
materialize(θ_xs) ⇒ Dict({})
───────────────────────────
δ(collect, [θ_xs], cs) ⇒ Materialized(Dict({}))
```

**[DELTA-COLLECT]**

```text
materialize(θ_xs) ⇒ Seq(θ_h₁, θ_t₁)
materialize(θ_t₁) ⇒ Seq(θ_h₂, θ_t₂)
...
materialize(θ_tₙ) ⇒ Dict({})          (terminal)
───────────────────────────
δ(collect, [θ_xs], cs) ⇒ Materialized(Dict({0↦θ_h₁, 1↦θ_h₂, ..., n↦θ_hₙ}))
```

Collect materializes the Seq *spine* (all tail thunks) but head thunks pass through into the result Dict without materializing. This is the key distinction: `$collect` is strict in the structure but lazy in the values.

**[DELTA-ITERATE]**

```text
───────────────────────────
δ(iterate, [θ_f, θ_x], cs) ⇒ Materialized(Seq(
    Rc::clone(θ_x),
    PendingBuiltin(iterate, [Rc::clone(θ_f), PendingCall(θ_f, [θ_x])], cs)
))
```

Fully lazy: neither f nor x is materialized. The result Seq's head is x (unchanged thunk), and the tail is a PendingBuiltin that will produce `iterate(f, f(x))` when materialized. The `f(x)` is itself a PendingCall — computation unfolds one step at a time. When the tail PendingBuiltin is materialized, DELTA-ITERATE applies again with `f(x)` as the new seed, enabling corecursive unfolding of the infinite sequence.

**[DELTA-TRY]**

```text
materialize(θ_func) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env) ⇒ θ_body
materialize(θ_body) ⇒ v
───────────────────────────
δ(try, [θ_func], cs) ⇒ Materialized(Variant("Ok", Materialized(v)))

materialize(θ_func) ⇒ Function(params, body, env)    where |params| = 0
eval(body, env) ⇒ θ_body
materialize(θ_body) ⇒ error(e)
───────────────────────────
δ(try, [θ_func], cs) ⇒ Materialized(Variant("Error", Materialized(e.kind.to_string())))
```

`$try` materializes the function argument and invokes it. On success, returns `[Ok value]`; on error, returns `[Error message]`. The error is caught — `$try` catches all user errors but re-propagates system-level limits (`DepthExceeded`, `ResourceLimitExceeded`) to the caller. Also handles Builtin callees (dispatches with zero args).

**[DELTA-MAP-DICT]**

```text
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
∀i. θ'ᵢ = PendingCall(θ_f, [θᵢ], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], cs) ⇒ Materialized(Dict({k₁↦θ'₁, ..., kₙ↦θ'ₙ}))
```

`θ_f` is never materialized — it is captured by reference (`Rc::clone`) in each PendingCall. No values are computed; the result Dict is O(n) to construct and O(1) per element access.

**[DELTA-MAP-SEQ]**

```text
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
θ'_h = PendingCall(θ_f, [θ_h], ∅, cs)
θ'_t = PendingBuiltin(map, [Rc::clone(θ_f), θ_t], ∅, cs)
───────────────────────────
δ(map, [θ_f, θ_xs], cs) ⇒ Materialized(Seq(θ'_h, θ'_t))
```

Recursive structure: head is a PendingCall, tail is a PendingBuiltin that will apply DELTA-MAP-DICT or DELTA-MAP-SEQ when materialized.

**[DELTA-FILTER-DICT]**

```text
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
θ_step = PendingBuiltin(filter_dict_step, [θ_pred, θ_xs_mat, θ_keys, θ_idx], ∅, cs)
    where θ_xs_mat, θ_keys, θ_idx are pre-computed materialized thunks
───────────────────────────
δ(filter, [θ_pred, θ_xs], cs) ⇒ θ_step
```

The predicate `θ_pred` is not materialized at the top level — it is captured for deferred evaluation in the step function. The step function materializes one element at a time, applies the predicate, and either includes or skips it. Returns a Seq (not a Dict) because filtered keys are unpredictable.

**[DELTA-FILTER-SEQ]**

```text
materialize(θ_xs) ⇒ Seq(_, _)
θ_step = PendingBuiltin(filter_seq_step, [θ_pred, θ_xs], cs)
───────────────────────────
δ(filter, [θ_pred, θ_xs], cs) ⇒ θ_step
```

The step function receives the *original seq thunk* (not destructured head/tail) and materializes it internally to obtain head and tail. This avoids redundant materialization since the dispatch already materialized the collection. Lazy filter on sequences: the step function materializes head, applies predicate, and either includes it (Seq node) or skips it (recurse on tail). Elements are tested only when the result Seq is consumed.

**[DELTA-REDUCE-DICT]**

```text
materialize(θ_xs) ⇒ Dict({k₁↦θ₁, ..., kₙ↦θₙ})
acc₀ = θ_init
∀i. accᵢ = PendingCall(θ_f, [accᵢ₋₁, θᵢ], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], cs) ⇒ accₙ
```

Builds a chain of PendingCall thunks without materializing any values. The entire reduction is deferred — nothing computes until the result thunk is materialized. At that point, the chain unwinds from the inside out.

**[DELTA-REDUCE-SEQ]**

```text
materialize(θ_xs) ⇒ Seq(θ_h, θ_t)
θ_step = PendingBuiltin(reduce_seq_step, [θ_f, θ_init, θ_h, θ_t], ∅, cs)
───────────────────────────
δ(reduce, [θ_f, θ_init, θ_xs], cs) ⇒ θ_step
```

Seq reduction uses a step function that materializes the tail to check for termination, then recurses. Unlike Dict reduction, Seq reduction is incremental (processes one element per step function invocation).

### Part 4: Dual-Dispatch Pattern

Six builtins (`map`, `filter`, `take`, `drop`, `reduce`, `join`) dispatch on the runtime type of their collection argument:

```text
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

```text
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

All delta rules preserve thunk identity. When a thunk appears in both the input and output of a builtin (e.g., `$head` extracting a Seq's head), the same `Arc<Thunk>` allocation is shared — not copied. Subsequent materialization of the output thunk memoizes the value for all holders of that `Arc`.

**Strictness monotonicity:**

The signature table is monotonic with respect to the implementation: a builtin marked `L` at position i will never call `materialize()` on `args[i]`. A change that adds a `materialize()` call on a position marked `L` is a breaking change to the laziness contract and must update the signature table.

**Dual-dispatch consistency:**

For dual-dispatch builtins, the Dict and Seq paths must agree on which non-collection arguments are materialized. For example, `$map`'s Dict path and Seq path both leave `θ_f` unmaterialized — if one path started materializing `θ_f`, it would break laziness for programs that pass expensive computations as the function argument.

## Laziness Design

### Sequential Expressions in Function Bodies

When a function body contains multiple expressions, the parser wraps them in `SurfaceExpression::Sequential`. This construct enables intermediate bindings within a function while maintaining lazy evaluation semantics:

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
- **CEK machine routing:** `CoreSurfaceExpression::Sequential` is handled directly inside `eval_core_expr` in `eval.rs` via a recursive async call — it iterates the expression list, materializing each intermediate dict to extend the scope chain, then tail-calls into the final expression. This path uses the Rust async call stack rather than the CEK continuation stack (see `cek-match-sequential-rust-stack` in TODO.md)

This is identical to how document-level expression sequences work (see [Documents](09-documents.md) §Scope Chain Semantics), but scoped within a single function body rather than across documents.

**Grammar:** The `fn_form` rule in `doc/02-syntax.md` §Complete Grammar uses `value+` to permit multiple body expressions. The parser automatically wraps `value+` in `SurfaceExpression::Sequential` when more than one expression is present.

### Strictness Exceptions

Tinct's evaluation model is lazy by default — values remain unevaluated until accessed. Four intentional exceptions deviate from this default by triggering materialization at construction time rather than access time:

1. **TypeAssert validation via continuation:** `[@Type expr]` evaluates `expr` and schedules validation via `Cont::TypeAssertCheck` continuation. For structural types (record shapes), validation is deferred via `Guarded` thunks that check field types lazily at first access. For primitive types, validation is immediate. This ensures type errors are caught at annotation sites (for primitives) or field access sites (for records), providing clear error reporting. See [Type System Extensions](07-type-extensions.md) §TypeAssert Runtime Validation.

2. **reduce eager iteration (Seq path only):** `$reduce` (and `$fold`) on Seq inputs materialize each accumulator step to prevent O(N) Rust stack depth from nested PendingCall thunks. The accumulator chain is still lazy (each step is a PendingCall thunk), but the Seq iteration itself materializes tails at each step to detect sequence end without building deep call chains. (Dict path: fully lazy PendingCall chain — see §Laziness Design table below.)

3. **Guarded default fallback:** When a guard fails and a `default:` value is provided, the default is evaluated and materialized immediately. This prevents deferred errors from propagating when the guard explicitly signals a fallback path should be taken.

4. **Sequential expression scope chain (SEQ-SCOPE):** Named bindings from intermediate expressions in a multi-expression document have their keys extracted eagerly (for scope chain construction), but values remain lazy thunks. Only the dict structure (keys) must be known to create the scope chain — values are forced on demand when accessed. See [Documents & Pipelines](09-documents.md) §Scope Chain Semantics for the formal specification.

### Overlay Eagerness

`$merge` constructs a `Value::Overlay(L, R)` in O(1) without materializing either argument — the merge is purely structural at call time. However, `flatten_overlay()` is called eagerly the moment any builtin receives an `Overlay` value (e.g., key lookup, `$keys`, `$map`, `$filter`, pattern matching). At that point the entire Overlay chain is walked synchronously: every L and R thunk is materialized, and all entries are merged into a concrete `IndexMap`. There is no incremental or demand-driven flattening.

**Why flattening is eager.** `IndexMap` requires concrete `Key` values for insertion. There is no lazy map abstraction that can defer key materialization. When a builtin needs to look up a key, iterate entries, or compute the length of a dict, the full key set must be known. Flattening the entire tree at once (rather than level by level) also avoids re-traversing the chain on repeated access.

**Space leak for accumulator patterns.** A pattern that repeatedly merges into an accumulating dict:

```tinct
[result: [$reduce items [fn [acc item]
    [$merge acc [item-key item]: [item-val item]]
] {}]]
```

builds an O(N)-deep `Overlay(Overlay(... Overlay({}, d₁) ..., dₙ₋₁), dₙ)` chain. Every intermediate dict is kept alive by the chain. Flattening at the end materializes all N intermediate dicts simultaneously, producing a temporary spike in memory proportional to the total size of all intermediate steps, not just the final dict.

**Recommended pattern.** For accumulation, prefer `$collect` applied to a lazy sequence that produces `[key: value]` entries:

```tinct
[result: [$collect [$map items [fn [item]
    [item-key item]: [item-val item]
]]]]
```

`$collect` materializes the Seq spine and inserts entries into a single dict without building an Overlay chain. Each intermediate dict is allocated once, and no O(N) chain accumulates in memory.

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
| Document scope chain (`eval_surface_document`) | Named binding keys extracted eagerly; values remain lazy thunks | Scope chain construction requires knowing dict keys, but values are inserted as lazy thunks and forced only on access. Dead bindings remain unevaluated. (`eval_pipeline.rs`) |
| **Internal (eval.rs)** | | |
| `eval_key` (dict construction) | Materializes all dict keys | Keys must be known for dict insertion |
| `builtin_keys` | Materializes dict | Keys are never thunks |
| `TypeAssert` body (`[@Type expr]`) | Shape checked immediately (required keys present, cardinality for closed records); field type validation via Guarded thunks — each field's type constraint is checked lazily at first access | Known partial strictness: shape check cannot be deferred, but individual field types are validated lazily via `Cont::TypeAssertCheck` continuation. See [Type System Extensions](07-type-extensions.md) §TypeAssert Runtime Validation and §Consistent Subtyping. |

**Force-side-effect idiom.** Tinct has no `!` or `seq` operator. To force a side-effectful binding before returning a result, use the equality-check pattern:

```tinct
[w: [side-effect]]
[if [= w w] result result]
```

This forces `w` via the equality check (which materializes both operands), then returns `result`. The `_` identifier cannot be used for this purpose because `_` triggers implicit lambda desugaring — `[fn [_] result]` — rather than being a discard.

**Error reporting impact:** Operations that shift from eager to lazy (e.g., `$if`, `$merge`, `$map`) will report errors at access time rather than construction time. This provides more accurate source locations (pointing to where materialization failed) but changes error timing. Inherently materializing operations continue to produce errors at call time.

---

## Cancellation and Evaluation Contexts

### EvalContext and the `cancel` Field

Every evaluation runs inside an `EvalContext` (`src/eval.rs`). `EvalContext` carries a `cancel: CancellationToken` field (from the `tokio-util` crate). This token represents the *cancellation scope* for all evaluations within that context.

```rust
pub struct EvalContext {
    // ...
    pub cancel: tokio_util::sync::CancellationToken,
    // ...
}
```

**Inheritance:** Child contexts created via `with_base_dir()` (used by `$include` for the included file's context) inherit the parent's cancellation token. This means cancelling the root context also cancels any in-progress `$include` evaluations.

**Root context:** When `EvalContext::new()` creates a fresh context (e.g., at program startup), it creates a fresh `CancellationToken::new()` — a root token that is not yet cancelled.

### Cancellation Primitives

Eight builtins expose cancellation context management to LLT programs:

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `context` | `→ Context` | Creates a fresh root cancellation context (independent of the runtime's own context) |
| `with-cancel` | `Context → {child-ctx: Context, cancel: Context}` | Creates a child context; the `cancel` field is the same child token — call `[cancel-task cancel]` to fire it |
| `with-timeout` | `Context → Int → Context` | Creates a child context that auto-cancels after `Int` milliseconds |
| `with-deadline` | `Context → Int → Context` | Creates a child context that auto-cancels at an absolute Unix timestamp in milliseconds |
| `cancelled?` | `Context → Bool` | Synchronous (non-blocking) check: `true` if the context has been cancelled |
| `cancel-task` | `Context → Null` | Explicitly cancels the context and all its child contexts |
| `non-cancellable` | `→ Context` | Creates a fresh root cancellation token that nothing will ever cancel — for use in cleanup code that must run even when the parent context is cancelled |
| `with-context` | `Context → Fn@[]@T → T` | Evaluates a zero-arg function under the given cancellation context — the thunk's blocking operations respond to the given context's cancellation state, not the caller's |

**Implementation note (`with-cancel`):** The spec describes `cancel` as a zero-arg callable `Fn@[]@Null`. The current implementation returns a `Context` value instead, requiring `[cancel-task pair.cancel]` rather than `[pair.cancel]`. This deviation is documented in the memory palace and avoids the need to close over a Rust `CancellationToken` in a `Value::Function`. The functional semantics are equivalent.

### Cancellation in Blocking Builtins

**Contract:** Every builtin that blocks (suspends the current task while waiting) must race against the evaluation context's cancellation token. When cancelled, the builtin must return an error immediately rather than blocking indefinitely.

**Implementation pattern:**

```rust
tokio::select! {
    result = blocking_operation => result,
    _ = ctx.cancel.cancelled() => {
        Err(EvalError::user_error("operation: cancelled".to_string(), call_span).into())
    }
}
```

**Affected builtins:**

| Builtin | Blocking operation | Cancellation behavior |
|---------|-------------------|----------------------|
| `send` | `channel.sender.send(value).await` (suspends if buffer full) | Returns error "send: cancelled" |
| `recv` | `rx.recv().await` (suspends until value available) | Returns error "recv: cancelled" |
| `await` | `handle.await` (suspends until task finishes) | Returns error "await: cancelled" |
| `select-once` | `yield_now()` poll loop (busy-waits for first channel) | Checks `ctx.cancel.is_cancelled()` each iteration; returns error "select-once: cancelled" |
| `par-map` / `par-filter` | `handle.await` (serial collection of `JoinHandle`s) | Not cancellable — awaits all spawned tasks to completion regardless of context state (known limitation; tracked as `runtime-v2-par-map-cancellation` in TODO.md) |

**Design:** When a context is cancelled while `recv` is waiting, the blocking `.await` inside `tokio::select!` is dropped and the cancellation branch fires. The channel itself is not closed — other non-cancelled tasks can still send and receive from the same channel.

### Scope Inheritance and `$include`

`$include` evaluates included files in a child `EvalContext` created via `with_base_dir()`:

```rust
pub fn with_base_dir(&self, ...) -> Arc<EvalContext> {
    Arc::new(EvalContext {
        cancel: self.cancel.clone(), // inherits parent's token
        // ...
    })
}
```

Cancelling the parent context propagates into included files: any blocking operation in an included file's evaluation will be interrupted by the parent's cancellation.

### Event Sources and Long-Running Contexts

Event-source builtins (`signal-channel`, `timer-channel`, `watch-channel`) spawn background `spawn_local` tasks that run indefinitely. These background tasks are tracked in `EvalContext`'s `task_registry: Arc<Mutex<Vec<JoinHandle<()>>>>` field. The following spawn sites register their handles: `signal-channel`, `timer-channel`, `watch-channel` (×2 tasks), `with-timeout`, `with-deadline`, and `EvalContext::with_timeout_ms`. Calling `[drain]` awaits all registered handles, enabling a clean shutdown sequence: `[cancel-root] [drain] [exit-now code]`. Cancel-on-drop is not implemented — background tasks are not automatically stopped when the channel value is dropped; `[drain]` must be called explicitly.

**Graceful shutdown sequence:** Each background loop uses `tokio::select!` to check `ctx.cancel.cancelled()` on every iteration, with the cancellation branch listed first (highest priority). When `[cancel-root]` fires, all background loops see the cancelled signal and `break` cleanly on their next iteration. `[drain]` then awaits the `JoinHandle`s, which complete promptly because the loops have already exited. This is the recommended shutdown sequence: `[cancel-root]` signals loops to exit cleanly, `[drain]` awaits their completion, `[exit-now code]` terminates the process. `[drain]` always calls `handle.abort()` on every registered handle before awaiting it. For background loops that already exited cleanly (via the cancellation branch after `[cancel-root]`), `abort()` is a no-op on the completed task. For one-shot sleep tasks (`with-timeout`, `with-deadline`) that are still running, `abort()` terminates them immediately rather than waiting for the full sleep duration. The `abort()` may leave resources (open channels, file handles) in an inconsistent state for tasks that did not exit cleanly — which is why calling `[cancel-root]` first is recommended.

---

## Output Serialization

Value serialization (e.g., JSON output, display formatting) uses the `visit_value` visitor pattern in `src/lib.rs`. This is distinct from selective materialization (which materializes only what's needed for computation) — output serialization materializes the value tree as needed during traversal, producing output suitable for external consumption.

The CLI `--eval` flag performs only shallow (WHNF) materialization. When combined with `-o <formatter>`, the formatter handles recursive traversal and materialization internally (e.g., `to-json` from `codecs/json.llt`); without `-o`, only top-level forcing is performed.

**Visitor pattern:** The `ValueVisitor` trait (defined in `src/lib.rs`) provides callback methods for each value variant:

```rust
pub trait ValueVisitor {
    type Output;
    
    fn visit_int(&self, v: i64) -> Self::Output;
    fn visit_float(&self, v: f64, span: Span) -> Result<Self::Output, Box<EvalError>>;
    fn visit_bool(&self, v: bool) -> Self::Output;
    fn visit_str(&self, v: &str) -> Self::Output;
    fn visit_bytes(&self, v: &[u8]) -> Self::Output;
    fn visit_null(&self) -> Self::Output;
    fn visit_dict(&self, entries: Vec<(Key, Self::Output)>) -> Self::Output;
    fn visit_seq_head(&self, head: Self::Output, span: Span) -> Result<Self::Output, Box<EvalError>>;
    fn visit_function(&self, params: &[Param], span: Span) -> Result<Self::Output, Box<EvalError>>;
    fn visit_builtin(&self, name: &str, module: &str, span: Span) -> Result<Self::Output, Box<EvalError>>;
    
    fn depth_limit_output(&self, depth: usize, span: Span) -> Option<Result<Self::Output, Box<EvalError>>>;
}
```

**Traversal:** The `visit_value` function recursively walks the value tree, materializing thunks as needed (via the CEK machine's `materialize()`) and dispatching to the visitor callbacks. For dict values, it materializes each entry thunk and recursively visits the result. For sequence values, it materializes the head thunk and visits it (tail is shown as `...` without forcing for display purposes, or expanded iteratively for serialization).

**Cycle handling:** Cyclic structures are caught by the per-thunk `InProgress` sentinel during materialization, not by a global visited set in the visitor. The `InProgress` state detects cycles within a single thunk's evaluation. The visitor does not need separate cycle detection because it operates on already-materialized values (or calls `materialize()` which has its own cycle detection).

**Depth limiting:** Each visitor can implement `depth_limit_output` to short-circuit traversal at a maximum depth. This prevents stack overflow on deeply-nested structures and is used by display formatters to show `...` for values beyond the depth limit.

**Comparison to selective materialization:** Regular `materialize()` forces a single thunk to WHNF and memoizes the result. The visitor pattern extends this to full tree traversal, calling `materialize()` on each encountered thunk and recursing into the result. This is lazy-by-default: only thunks actually visited are materialized.

---

## ValueVisitor — Output Serialization

The `ValueVisitor` trait (in `src/lib.rs`) provides a visitor pattern for structural traversal of materialized `Value` trees. It's used to produce JSON and display-string output from evaluated values.

**Design:** The `visit_value` function walks a materialized `Value`, calling visitor methods for each primitive type (`visit_int`, `visit_float`, `visit_str`, `visit_bool`, etc.) and recursively traversing structured types (`visit_dict`, `visit_seq_head`). The visitor returns a type-safe `Output` associated type — `String (compact JSON)` for `JsonVisitor`, `String` for `DisplayVisitor`.

**Span threading:** Every `visit_*` method receives the source `Span` of the value being visited. For structured types (Dict, Seq), the span is propagated from the thunk that contained the value. This ensures that serialization errors (e.g., "cannot serialize Function to JSON") include accurate source locations pointing to where the problematic value was defined.

**Depth limiting:** The `depth_limit_output(depth, span)` method allows visitors to implement recursion depth limits. `JsonVisitor` enforces a 256-level depth limit to prevent stack overflow on deeply nested structures, returning an error with the span of the depth-exceeded value. `DisplayVisitor` enforces a 5-level depth limit (used for error messages) and truncates with `"..."`.

**Implementations:**

- **JsonVisitor** (`src/lib.rs`) — produces compact JSON `String`, used by `visit_value` for JSON output. Rejects values that cannot be represented in JSON (NaN, Infinity, Function, Builtin, Seq). Detects array-like dicts (sequential integer keys 0..n) and serializes them as JSON arrays. Used by `run_literate_eval`/`run_literate_weave`; the CLI `-o json` formatter calls the tinct `to-json` function from `stdlib/codecs/json.llt` instead.
- **DisplayVisitor** (`src/lib.rs:1000-1100`) — produces LLT display strings, used for error messages and debug output. Accepts all value types, rendering functions as `Function([params])` and sequences as `Seq`.

**Integration with materialization:** The `visit_value` function calls `materialize()` on each dict entry and sequence head thunk before recursing, ensuring thunks are forced to WHNF before the visitor callbacks are invoked. This integrates naturally with the CEK machine's selective materialization — visitors only force what they traverse.

---

**Allocation strategy:** The runtime uses two complementary strategies: backward-compatible optimizations to the current `Arc<Thunk>` + `IndexMap<String, Arc<Thunk>>` runtime, and arena-based allocation with flat environments for deeper efficiency gains.

**Current allocation profile:**

| Component | Representation | Cost |
|-----------|---------------|------|
| Thunks | `Arc<Thunk>` with `ThunkInner { Mutex<Option<UnevaluatedState>>, OnceCell<Result<Value, Arc<EvalError>>> }` | Individual heap alloc per thunk; unevaluated slot uses Mutex for atomic take; result uses lock-free OnceCell |
| Environments | `Arc<RwLock<Environment>>` with `IndexMap<String, Arc<Thunk>>` + parent chain | O(depth) variable lookup |
| Dict keys | `Key::String(String)` | Cloned 2× per dict entry (env bindings + dict_map) |
| Thunk origin | `origin: Cow<'static, str>` | Zero-cost for empty/static origins (`Cow::Borrowed`); allocates only for dynamic labels |
| Type inference sets | `HashSet<String>` in `collect_type_vars`, `collect_row_vars`, `collect_all_vars`, `instantiate_scheme`, `instantiate_at_level`, `generalize` | Transient per-call allocations in `src/types.rs`; each call allocates a fresh `HashSet`, collects variable names via tree traversal, then drops the set. Hot paths during type inference — `instantiate_scheme` is called per polymorphic variable reference, `generalize` per dict entry at Pass 4. Elimination: flat environments with de Bruijn indices remove the need for name-based variable collection entirely. Mitigation: pre-sized `HashSet::with_capacity` based on scheme quantifier count, or `SmallVec`-backed collection for schemes with few variables (the common case). |

**Backward-compatible optimizations.** The runtime already uses `Arc<Thunk>` and `Arc<RwLock<Environment>>` throughout. Remaining allocation hot-spots:

- **Dict literal fast-path** (Nix `maybeThunk`): In `eval_dict`, when `entry.value.node` is `Int|Float|Bool|Str`, create `Materialized` thunks directly instead of wrapping in `Unevaluated`. Eliminates ~40-60% of thunk allocations for config-heavy files. Safe because literals are side-effect-free, deterministic, and don't participate in letrec cycles.
- **String interning**: `HashSet<Arc<str>>` with `Borrow<str>` lookup. Interns *structural identifiers only* — `Key::String`, variable names, builtin names, and thunk origins. Does NOT intern user data strings (may be large and unique). Reduces key cloning to `Arc::clone` and enables O(1) pointer-equality comparison. Scoped to evaluation session lifetime (lives in `EvalContext`, cleared per `eval_file()`). Production alternative: `lasso::Rodeo` for zero-copy Spur handles.
- **Key cloning reduction**: Eliminate the 2× `String` clone per dict entry in `eval_dict` (once into `dict_env` bindings, once into `dict_map`). Use `entry_mut()` pattern or restructure insert order. ~30% of dict allocation cost.
- **func_label allocation reduction**: `format!("${name}")` on every PendingCall creation → `Cow<'static, str>` for the common VarRef case (most calls). Only allocate for DotAccess labels. ~5-10% of call overhead.
- **Capacity hints**: `IndexMap::with_capacity(entries.len())` on all dict construction paths (`eval_dict`, `builtin_drop` Dict path, `builtin_split`).
- **SmallVec**: `SmallVec<[Arc<Thunk>; 4]>` for call args (most calls have ≤4 args), `SmallVec<[StackFrame; 8]>` for error stacks.
- **Origin optimization**: `origin: String` → `Arc<str>` via string interner, with static empty sentinel for the common case.

**Arena allocation (current implementation).** The runtime uses `ThunkArena` with `ThunkId` handles for all thunk storage. This is the "arena-backed registry" approach:

- `ThunkArena` exists in `EvalContext` with `RefCell` interior mutability
- `Value` variants use `ThunkId` handles: `Dict(IndexMap<Key, ThunkId>)`, `Seq { head: ThunkId, tail: ThunkId }`, `Overlay(ThunkId, ThunkId)`
- Allocation goes through `ctx.alloc_thunk(Thunk)` which wraps in `Arc<Thunk>` and stores in arena `Vec<Arc<Thunk>>`
- Arena persists across `---` boundaries (append-only, no per-section deallocation)
- **No migration needed**: ThunkIds are stable indices that never invalidate; `$include` cache stores standalone `Arc<Thunk>` (arena-independent)

**Full arena-based allocation for per-section lifetimes (deferred).** A further optimization — not yet implemented — would give the arena a per-section lifetime, reclaiming unreachable thunks at `---` boundaries:

- **Per-section arena**: Give each `---`-delimited section its own arena. Thunks not reachable from `%` are reclaimed when the section ends. Requires selective migration of reachable thunks across the boundary.
- **Flat environments with slot indices**: Replace `IndexMap<String, Arc<Thunk>>` chain with flat `Vec` arrays indexed by compile-time (level, slot) pairs (de Bruijn levels). Variable lookup becomes O(1). Environment reuse in function calls becomes trivially safe.
- **Variable resolution pass**: Pre-eval pass assigns (level, slot) indices to every `VarRef`. This pass also enables TCO detection. (Partially implemented: `src/resolve.rs` produces `ResolutionTable`; full flat-env usage is deferred.)

**Arena lifetime and persistent values (deferred design).** If per-section arenas are adopted, the arena lifetime would be **one document section**. At each `---` boundary, values reachable from `%` would be **selectively migrated** from the section arena to `Arc`-backed persistent storage. The `---` boundary is **not** a strictness point — unevaluated thunks stay unevaluated through migration.

The migration algorithm would trace from `%` and rewrite arena handles:

```text
migrate(value, arena, thunk_table, env_table) → Arc<Thunk>:
  for each ThunkId in value:
    if thunk_table[id] exists:     return thunk_table[id]  (preserves sharing)
    thunk = arena[id]
    arc = Arc::new(Thunk::placeholder())     (allocate before recursing)
    thunk_table[id] = arc                    (insert before recursing — breaks cycles)
    arc.fill(match thunk.state:
      Materialized(v)            → Materialized(migrate_value(v, arena, thunk_table, env_table))
      Unevaluated(node, env)     → Unevaluated(node, migrate_env(env, arena, thunk_table, env_table))
      PendingBuiltin(f, args, …) → PendingBuiltin(f, migrate_args(args, …), …)
      PendingCall(f_θ, args, …)  → PendingCall(migrate(f_θ, …), migrate_args(…), …)
      Failed(e)                  → Failed(e)
      InProgress                 → unreachable at --- boundary
    )
  return arc
```

Two-phase allocation: `Arc::new(placeholder())` is inserted into the table *before* recursing into the thunk's state. This is the standard graph-copying pattern for structures with cycles — letrec environments contain mutual references, so the table entry must exist before `migrate_env` encounters the same ThunkId transitively.

**Two translation tables** preserve identity across the migration boundary:

- `thunk_table: HashMap<ThunkId, Arc<Thunk>>` — ensures two references to the same arena thunk map to the same `Arc<Thunk>`.
- `env_table: HashMap<EnvId, Arc<RwLock<Environment>>>` — ensures two closures capturing the same arena environment share the same migrated environment. Without this, letrec groups that share an environment would become independent copies, breaking the sharing invariant.

AST nodes (`Arc<SurfaceNode>`) are reference-counted and arena-independent — they are shared, not copied. The builtins environment (root of every parent chain) is always `Arc`-backed and never arena-allocated — it is the base case that terminates `migrate_env` recursion.

Within a section, all thunks are arena-allocated and lazy. At `---`, only thunks reachable from `%` are migrated — unreachable intermediate thunks are reclaimed when the arena drops.

**What migrates correctly:**

| Value type | Migration behavior |
|------------|-------------------|
| Primitives (Int, Str, Bool, …) | Copied directly (no arena handles) |
| Dict entries | Each thunk migrated; sharing preserved via table |
| Functions/closures | Captured environment chain migrated recursively |
| Infinite Seq | Only the cons cell is migrated; lazy tail stays lazy |
| `include` results | Already Arc-backed (include cache outlives sections) |

Per execution context (deferred per-section model):

| Context | Arena lifetime | Cross-boundary value | Notes |
|---------|---------------|---------------------|-------|
| CLI (single section) | Entire eval | None | One arena, dropped at end. No migration. |
| CLI (multi-section) | Per section | `%` (selectively migrated) | Arena per section, migrate at `---` |
| REPL | Per input | `%` (selectively migrated) | Each input is implicitly a section |
| LSP | Per section | `%` (selectively migrated) | Editing section N re-evaluates N+ with cached `%` from N-1 |

**Cost model:** Migration is O(thunks reachable from `%`), not O(total section thunks). For sections where `%` is a small result derived from large intermediate computations, migration cost is much lower than deep-materialization.

**Rejected alternatives:** (1) Session-scoped arena — unbounded memory growth during long REPL sessions; requires stop-the-world compaction with pointer fixup across all live references. (2) Hybrid arena+Arc — two allocation paths; every thunk creation must decide arena vs Arc; closures capturing thunks make escape analysis intractable. (3) Deep-materialization at `---` — changes language semantics (lazy→eager), breaks closures (env chains hold dangling arena handles after drop), and diverges on infinite sequences in `%`. (4) Per-eval copy-out without section granularity — triggers materialization of intermediate values within a section, losing laziness benefits.

**LSP incremental re-evaluation:** Migrated `%` values are self-contained `Arc`-backed storage with no arena references. The LSP caches `%` per section. Editing section N re-uses cached `%` from section N-1 (already migrated, no re-evaluation) and re-evaluates only sections N through the end.

**Key tradeoff:** Environment lookup stays O(depth) until flat-env optimization, but the existing `Arc<RwLock<Environment>>` chain already enables safe concurrent reads and the ThunkArena keeps allocation hot.

**Precedent:** Nix uses flat `Value*[]` arrays with de Bruijn levels and Boehm GC. Jsonnet uses GC heap with flat bindings. Nickel uses `Rc<RefCell<Closure>>`. Tinct uses `Arc<Thunk>` + `ThunkArena` (arena-backed registry); per-section arenas would move closer to Nix's model.

**Constraint:** The arena model must handle letrec self-reference safely in Rust (thunk slots allocated before fill, no dangling pointers). The safe Rust arena patterns are analyzed in `doc/whatif/arena-patterns.md` — the recommended approach is an index-based arena (`Vec<Thunk>` + `ThunkId` handles), following the cranelift entity pattern.

## Iterative Evaluator (CEK Machine)

**Implementation:** The evaluator uses an iterative CEK machine (Control-Environment-Kontinuation) with an explicit bounded continuation stack (`MAX_CONTINUATION_STACK = 2048` in `src/eval_materialize.rs`). This replaced the old recursive evaluator which used `MAX_EVAL_DEPTH = 256` and relied on Rust's call stack.

**Evaluation depth is bounded by:**

- Parser depth limit: `MAX_PARSE_DEPTH = 256` (nested syntax depth)
- Continuation stack limit: `MAX_CONTINUATION_STACK = 2048` (evaluation nesting depth)
- Cycle detection: `InProgress` thunk state sentinel (catches circular references)
- Rust stack bounds: the CEK machine runs iteratively on the heap, avoiding deep Rust recursion

**Decision:** Replace the recursive `eval()` / `materialize()` call stack with an iterative CEK machine. Continuations are defunctionalized — each closure that CPS would create becomes a variant in a `Cont` enum, stored in a `Vec<Cont>` stack.

**Problem:** `eval()` and `materialize()` are mutually recursive across 8+ call patterns. Deeply-nested lazy chains exhausted the Rust call stack before `MAX_EVAL_DEPTH` fired. The old implementation required a 64MB worker thread stack.

**Architecture:** Two enums, one loop.

`Action` represents what to do now (the "control" register):

```rust
enum Action {
    Continue(EvalResult<Value>),
    Materialize { thunk: Arc<Thunk>, mat_span: Option<Span> },
    EvalCore { expr: Arc<Spanned<CoreExpr>>, env: Arc<RwLock<Environment>>, ctx: Arc<EvalContext> },
}
```

`Cont` represents what to do with the result (the reified continuation / "kontinuation" stack):

```rust
enum Cont {
    // materialize() continuations
    Memoize(Box<MemoizeData>),                       // cache result/error in thunk
    PendingCallDispatch(Box<PendingCallDispatchData>),// force callee, then invoke
    GuardedValidate(Box<GuardedValidateData>),        // validate against type annotation
    BuiltinForceArg(Box<BuiltinForceArgData>),        // force args[0..force_count] then W1 Seq/Spine positions

    // eval() continuations
    DotAccessForce(Box<DotAccessForceData>),           // access field from materialized dict
    TypeAssertCheck(Box<TypeAssertCheckData>),          // validate against TypeAssert annotation
    SequentialStep(Box<SequentialStepData>),            // process next step in Sequential expression
    ForceAndBind(Box<ForceAndBindData>),                // force dict entry and bind to environment
    MatchDispatch(Box<MatchDispatchData>),              // dispatch to next arm after scrutinee materialized
    MatchGuardCheck(Box<MatchGuardCheckData>),          // check guard result for matched arm
    PredicateCheck(Box<PredicateCheckData>),            // check is: predicate result for TypeAssert
}
```

All `Cont` variant payloads are boxed to keep the `Cont` enum ≤96 bytes (one cache line). The compile-time assertion at `src/eval_materialize.rs:467` enforces this.

The main loop is a two-register machine — `action` (what's happening now) and `stack` (what's waiting):

```rust
pub(crate) async fn run(initial: Action, ctx: &Arc<EvalContext>) -> EvalResult<Value> {
    let mut stack: Vec<Cont> = Vec::new();
    let mut action = initial;

    loop {
        match action {
            Action::EvalCore { expr, env, ctx: action_ctx } => {
                // eval_core_expr_pub() evaluates the CoreExpr to a thunk; if already
                // materialized, take the fast path; otherwise dispatch to Materialize
                action = match eval_core_expr_pub(&expr, &env, &action_ctx).await {
                    Ok(thunk) => match thunk.try_get_materialized() {
                        Some(value) => Action::Continue(Ok(value)),
                        None => Action::Materialize { thunk, mat_span: Some(expr.span) },
                    },
                    Err(e) => Action::Continue(Err(e)),
                };
            }
            Action::Materialize { thunk, mat_span } => {
                // force_step() dispatches on thunk state, pushes Memoize continuation
                action = force_step(&thunk, mat_span, &mut stack, ctx).await;
            }
            Action::Continue(result) => match stack.pop() {
                None => return result,
                // ctx is threaded via MemoizeData stored in Cont variants, not a param
                Some(cont) => {
                    action = apply_cont(cont, result, &mut stack).await;
                }
            },
        }
    }
}
```

**How this works:** Instead of recursive calls, each continuation point becomes a `Cont` variant pushed onto the stack. When a sub-computation completes (`Action::Continue`), the top continuation is popped and dispatched. The `Cont` variant stores exactly the state that a closure would have captured — no more, no less.

**Memoize error handling:** On `Err`, `Cont::Memoize` must call `cache_failure()` (transition thunk to Failed) before propagating the error up the continuation stack. This ensures failed thunks cache their error and don't retry on every access.

**Builtin return dispatch:** Builtins return `Arc<Thunk>`, not `Value`. After a builtin call, the CEK machine inspects the result: if the thunk is already `Materialized`, extract the value and produce `Action::Continue(Ok(value))`. If it is `Unevaluated` or `PendingBuiltin`, the dispatch depends on the **continuation context**, not a dynamic inference:

- If the top of the continuation stack is `Cont::Memoize` (the builtin was called during materialization of a parent thunk), the result must be materialized — produce `Action::Materialize { thunk: result_thunk, ... }`.
- Otherwise (any other continuation context), the result stays lazy — produce `Action::Continue(Ok(Value::from_thunk(result_thunk)))`.

This is **structurally determined** by the `Cont` variant on the stack, not inferred at runtime. Each `Cont` variant statically knows whether it needs a materialized value or accepts a thunk. The strictness signature table (§Selective Materialization — Formal Specification) declares per-argument strictness for builtin *inputs*; the continuation context determines strictness for builtin *outputs*. Builtins like `$if` and `$get` return lazy thunks that must not be auto-materialized when used as dict values or function arguments.

**Output serialization:** Value serialization (e.g., JSON output) uses the `visit_value` visitor pattern in `src/lib.rs`. The `ValueVisitor` trait defines callbacks for each value variant (`visit_int`, `visit_dict`, `visit_seq_head`, etc.), and `visit_value` recursively traverses the value tree, materializing thunks as needed and dispatching to the visitor callbacks. Each output format implements the `ValueVisitor` trait. This replaced the old `deep_materialize` approach which used a separate recursive function to pre-materialize the entire value tree before serialization.

**Tail-call optimization:** TCO via `Memoize`-reuse was investigated but reverted due to `EvalStackGuard` invariant violations — reusing the memoize frame caused guard bookkeeping to go out of sync, producing double-cache-writes and incorrect failure propagation. Proper TCO is tracked as the `tco-proper-fix` sprint in TODO.md. Until that sprint lands, recursive calls always push a fresh `Cont::Memoize` frame. Builtin calls likewise always push a continuation — builtins rely on `PendingBuiltin` thunk deferral for lazy behavior, not tail-call elimination.

**Error stack traces:** Walk `Vec<Cont>` to reconstruct the call stack, using each variant's stored span and label to produce precise "materialized at" context for every frame. This replaces the current `EvalError::stack` vector with a continuation-derived trace.

**Cont variant count:** 11 variants — `Memoize`, `PendingCallDispatch`, `GuardedValidate`, `BuiltinForceArg`, `DotAccessForce`, `TypeAssertCheck`, `SequentialStep`, `ForceAndBind`, `MatchDispatch`, `MatchGuardCheck`, `PredicateCheck`. Each variant stores only its specific continuation data (Arc pointers + Span + small fields). Frame size: ≤96 bytes per Cont (enforced by the compile-time assertion at `src/eval_materialize.rs:467`).

**Relationship to allocation strategy:** Arena allocation and flat environments integrate naturally with the CEK machine: `Cont` variants hold `ThunkId` handles into the arena, and the `Vec<Cont>` stack's lifetime defines the arena's lifetime scope.

## Runtime Reflection

### `FnAnnotation` — Function Metadata at Runtime

`Value::Function` carries a `FnAnnotation` alongside its `params`, `body`, and `env`:

```rust
pub struct FnAnnotation {
    pub doc: Option<String>,              // extracted from fn@[doc: "..."] at eval_fn time
    pub source_file: Option<String>,      // file path — from EvalConfig.current_file
}
```

Wrapped as `Option<Box<FnAnnotation>>` — `None` for unannotated functions (zero overhead). `doc` is extracted from the function's annotation metadata dict at function creation if present. `source_file` is threaded from `EvalConfig.current_file`.

### `ast-of` Builtin

`ast-of` is a Rust primitive in the `%rust "meta"` module. It is **non-materializing** — it inspects its argument's thunk state without forcing evaluation, branching on three cases:

- **Materialized thunk** — inspects the concrete `Value`:
  - `Value::Function` → `[type: "fn" return-ann: <ann-dict> params: [...] body: <ast-dict>]`. `return-ann` and each param's annotation use the `annotation_to_thunk_id` schema. For Materialized values, it returns the value's AST dict. The body AST is serialized via `ast_to_dict_expr`.
  - `Value::Builtin` → `[type: "builtin" name: <name> module: <module>]` using a shared `builtin_type_for(name)` static table.
  - Other values → `[type: type-of(val)]` — a minimal structural description.
- **Unevaluated thunk** — returns the expression AST via `ast_to_dict_expr` without forcing. Doc annotations visible in the expression tree (e.g., `@[doc: "..."]` on a `fn` expression) appear in the result. This allows introspecting module bindings without triggering side effects.
- **Pending thunk** — returns a state descriptor `[type: "pending"]` without forcing. This allows detecting that a thunk wraps a deferred call (e.g., a pipeline stage file result) without materializing it.

`ast-of` returns `Unknown` from the type checker's perspective. Field accesses on the result are on an `Unknown`-typed value and are not statically verified. The reflection layer is inherently dynamically typed — consistent with Python `inspect`, Common Lisp `describe`, and Elixir `Module.docs/2`. Tinct's gradual typing allows this: `@Unknown` opts out of static checking for the reflection helpers.

### Reflection Helpers in Prelude

`describe`, `sig-from-ast`, `annotation-to-str`, `annotation-of`, and `source-of` are pure tinct functions in `stdlib/prelude.llt` using only existing primitives. They use `find-first-or` (not `find-first`) for null-safe annotation entry lookup. `describe` on a function returns a dict with `doc:`, `return-ann:`, `params:`, and `sig:` fields; on a non-function value it returns `[type: type-of(val)]`.

The round-trip paths are: in-memory (`[eval [seq [ast-of f]]]`, works for pure/stdlib-only functions); file persistence (format via formatter, write to `DirCap`, re-include).

**References:** Sheard, T. & Peyton Jones, S. (2002). "Template Haskell." *Haskell Workshop.* [runtime staging analogue]

## Quote Semantics

`[quote expr]` converts the syntactic form of `expr` into a `Value::Expression` — a native AST node wrapped in the Expression value type — without evaluating `expr`. The conversion happens when the `SurfaceExpression::Quote` node is materialized by the normal evaluator — this is runtime evaluation, not a compile-time operation. The result can be inspected via `tag-of` (which returns the SurfaceExpression variant name, e.g., "Call", "Var") and field access (e.g., `.fn`, `.args` on a Call node).

`[unquote expr]` inside a `[quote ...]` evaluates `expr` in the current runtime environment when the surrounding `[quote]` is materialized, then splices the result into the AST structure. `[unquote-splice expr]` evaluates to a `Value::Seq` and splices each element into the enclosing list position. Nesting depth follows Bawden (1999): nested `[quote [quote [unquote x]]]` preserves the inner `unquote` as AST (not evaluated, since depth > 1).

Quoted expressions have type `Expression`. No special type rules — `quote` is transparent to the type system.

## Macro Expansion Pipeline

After parsing, the pipeline inserts an expansion phase:

```text
parse → expand_surface_program → desugar → resolve → typecheck → eval
```

`expand_surface_program` walks the AST top-down. A **pre-scan pass** first walks the entire AST to register all `[macro ...]`, `[syntax-class ...]`, and `[defmacro ...]` declarations before the transformation walk begins — giving the expander a complete registry before it processes any form. Then, when a `Call` node's function name matches a registered macro, the expander:

1. Quotes all arguments — converts each argument AST node into the corresponding typed `Expr` variant value (e.g., `Variant("Call", {fn: ..., args: ...})`). Arguments are never evaluated.
2. Binds the quoted forms to the macro's `[let ...]` parameter pattern. If any parameter is annotated (e.g., `name@VarRef`), the expander validates the argument's Expr variant before binding — an annotation mismatch raises `MacroError` at the call site before the macro body runs.
3. Calls the macro function with the bound arguments
4. Converts the result back to AST and re-expands

`[macro name [let params] body]` is processed by the expander: the body is evaluated in a **fresh `EvalContext`** (not shared with the runtime pass — prevents cache pollution and depth budget erosion) that inherits `EvalConfig` (capability flags, `no_fs`). The resulting callable is registered in `MacroEnv`. The `SurfaceExpression::MacroDecl` node is removed from the AST after registration — the typechecker and evaluator never see it.

**Termination:** A **shared** depth counter of 100 total across all expansion in a file (not per call-site — a single recursive macro cannot consume the entire budget), plus a total node-count cap of 100k nodes post-expansion to prevent exponential AST blowup.

**Namespace protection:** Macros cannot shadow registered Rust builtins — enforced at registration time.

**`gensym`:** Produces names of the form `:prefix:N` (colon prefix is forbidden in bare-word identifiers, making user collision structurally impossible). Names are unique but not stable across evaluation orders. Wrap the returned string with `do-var-node` or `ident` to obtain a `VarRef` AST node.

**`macro-error`:** `[macro-error span-dict message]` terminates expansion with `ErrorKind::MacroError` (E012) at the given span. `[span-of expr]` extracts the source span from any AST node as a dict. Together they enable macros to report precise, call-site-attributed errors.

**`splice`:** A macro returns `SurfaceExpression::UnquoteSplice` to inject multiple forms into the surrounding dict context. `UnquoteSplice` in expression position is an expansion-time error.

**Include ordering:** The `$include` builtin runs the full pipeline (parse → expand_surface_program → desugar → resolve → eval) on included files. Macros defined in an included file are expanded within that file's scope, but are **not** propagated to the includer — macro definitions are expansion-time constructs that don't cross the runtime `$include` boundary. This is a consequence of Flatt (2002) phase separation.

## Macro Hygiene

Tinct's macro hygiene is complete without scope sets. Every name in a macro body is one of two kinds:

- **Pattern-bound from user input** — names bound in the `[let ...]` argument pattern are the user's own names, inherently in user scope. No renaming needed; they hold pieces of the caller's input AST.
- **Gensym'd** — `[gensym "prefix"]` returns `:prefix:N`, a colon-prefixed name that is syntactically unforgeable (users cannot write colon-prefixed identifiers in source). Collision is structurally impossible.

No third category exists where a macro could accidentally introduce a name that captures user scope. Scope sets (Flatt 2016) address that third category; tinct eliminates it instead by design.

**`inject:`** provides a controlled anaphoric escape hatch. A macro declares `inject: it: default-expr` to deliberately introduce `it` into the caller's scope by convention. The `macro-injects` builtin lets callers reflect on which bindings a macro will inject and what their defaults are.

**Dual-span error provenance** uses a side map (`HashMap<NodeKey, Span>`) maintained by the expander. The side map records `(macro_name, call_site_span)` per generated node. Error messages show "in expansion of `<name>` at line N" with provenance chains for nested expansions — honest tags per Pombrio & Krishnamurthi (2015) Theorem 2 (Abstraction).

## Allocation Strategy

**Hybrid Arena + Arc\<Thunk\> Model**

Thunks are allocated in a `ThunkArena` (global bulk deallocation boundary) but still wrapped in `Arc<Thunk>` for shared ownership. The arena acts as a registry: `alloc_thunk(Arc<Thunk>)` appends to `Vec<Arc<Thunk>>` and returns a `ThunkId(u32)` index.

**Arena sharing:** Child contexts (created via `new_sharing_arena()` for macro expansion) share the parent's arena. This is critical for stdlib ThunkId validity: prelude dicts store `ThunkId` handles allocated during stdlib loading. When user code accesses `result.bind`, the ThunkId resolves via the shared arena.

**Snapshot pattern (`clone_for_child()`):** When creating a child context that needs stdlib ThunkIds but will grow independently, the arena is cloned via `clone_for_child()`, which creates a new `Vec<Arc<Thunk>>` pre-populated with `Arc::clone()` of every existing thunk. The child arena starts with the same indices 0..N (stdlib thunks) and appends user thunks starting at N+1. This preserves ThunkId validity while allowing independent growth.

**Cache consistency:** `create_stdlib_env_with_arena()` writes the stdlib arena to `STDLIB_ARENA_CACHE` (thread-local storage) so that subsequent `EvalContext::new()` calls on the same thread inherit stdlib ThunkIds without explicit arena threading. This is a convenience layer — explicit arena threading via `new_sharing_arena()` is preferred for non-test code.

**Precedent:** Jsonnet's VM uses 22 `FrameKind` variants with a value register (production-tested at Google). Nickel uses an iterative stack machine with `OpFirst`/`OpSecond` continuations (production Rust). Both are defunctionalized CPS machines. The theoretical foundation is Felleisen & Friedman's CEK machine.

**Iterative evaluation call patterns:**

| Operation | CEK encoding |
|----------------------|---------|
| TypeAssert validation | `Action::EvalCore` (inner expr) + `Cont::TypeAssertCheck` |
| Dot access (field extraction) | `Action::Materialize` (target dict) + `Cont::DotAccessForce` |
| Function/builtin call dispatch | `Action::Materialize` (callee) + `Cont::PendingCallDispatch` |
| Sequential expression chain | `Action::EvalCore` (current expr) + `Cont::SequentialStep` (for next) |
| Match expression dispatch | `Action::Materialize` (scrutinee) + `Cont::MatchDispatch` |
| Match guard check | `Action::EvalCore` (guard expr) + `Cont::MatchGuardCheck` |
| Guarded thunk validation | `Action::Materialize` (inner thunk) + `Cont::GuardedValidate` |
| Builtin strict argument forcing | `Action::Materialize` (arg thunk) + `Cont::BuiltinForceArg` |
| Sequential binding force | `Action::Materialize` (entry value) + `Cont::ForceAndBind` |
| TypeAssert predicate check | `Action::EvalCore` (predicate) + `Cont::PredicateCheck` |
| Thunk memoization | `Action::Materialize` (result) + `Cont::Memoize` |
