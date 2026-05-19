# What If: Guardedness Checking for tinct

**State:** Proposal

What would it take to detect circular data dependencies statically in tinct, turning a class of runtime divergence errors into compile-time type errors?

---

## Current State

Tinct's letrec dict model gives every binding in a dict closure over the same shared environment. This enables mutual recursion between functions — a core feature — but it also allows non-productive circular value definitions that diverge at runtime:

```tinct
# Silently accepted by the type system; diverges when demanded
x: [+ x 1]

# Mutual cycle — both diverge
[a: [str b]  b: [str a]]

# Trivially self-referential
shadow: shadow
```

These are caught at runtime by OnceLock blackholing (locally) or CMH probe propagation (across nodes in a distributed cluster), both of which produce `EvalError::circular_dependency`. But the error surfaces only when the binding is demanded — potentially deep inside an otherwise-correct program. For distributed evaluation, a cross-node cycle is especially costly: it requires at least one full network round-trip of probe messages before the error is surfaced.

The type system — HM + row polymorphism + typeclasses + BAS — does not prevent these cycles. It assigns `x` the type `Int` (or whatever the arithmetic context requires), `a` and `b` the type `Str`, and `shadow` a polymorphic type `α`. No type-level inconsistency exists; the failure is entirely in the value-level evaluation order.

### What Productive Corecursion Looks Like

Not all cycles are errors. Tinct's lazy evaluation makes infinite data structures a valid programming model. The following are genuinely productive:

```tinct
# Infinite seq of ones — each demand produces a head (1) and defers the tail
ones: [cons 1 ones]

# Mutual corecursion — evens interleaved with odds
[evens: [cons 0 odds]
 odds:  [cons 1 evens]]

# Mutually recursive functions — each call may terminate depending on input
[even?: [fn [let n] [if [= n 0] true  [odd?  [- n 1]]]]
 odd?:  [fn [let n] [if [= n 0] false [even? [- n 1]]]]]
```

The type system accepts all of these too — because structurally they look the same as the bad cases. A **guardedness checker** distinguishes them: productive cycles have every recursive reference in a position where at least one layer of structure is produced before the reference is demanded. Non-productive cycles demand the reference before producing anything.

### What's Missing

1. No static rejection of non-productive circular value definitions.
2. No user-visible distinction between "infinite lazy structure" (intended) and "diverging value" (bug).
3. No early error in the distributed case — cross-node cycles require network round-trips to detect.

---

## Why Guardedness Matters for tinct

**Converts runtime divergence to compile-time type errors.** A `[x: [+ x 1]]` definition becomes a type error at the definition site, not a hung evaluation hours later. The `Never` type (from BAS) propagates to every use of `x`, making all downstream code unreachable — accurately reflecting that `x` never produces a value.

**Makes productive corecursion a first-class feature.** Today infinite streams like `[cons 1 ones]` work at runtime but are indistinguishable from bugs at the type level. With guardedness, `ones: [cons 1 ones]` is explicitly accepted and `x: [+ x 1]` is explicitly rejected. Tinct gains a principled story for infinite data.

**Eliminates distributed cycle detection overhead.** A guardedness-verified program cannot contain cross-node cycles due to value-level circular definitions. The CMH probe mechanism remains for defensive programming but should never fire for well-typed programs.

**LSP feedback.** Instead of "program hung" or "circular dependency at runtime," users get a red underline at the definition with a message explaining which recursive reference is in a strict position.

---

## Design

### The Guard Boundary

Every position in a tinct expression is either **guarded** or **unguarded** for a given variable reference `x`:

A position is **guarded** if the recursive reference is behind a lazy barrier — a thunk boundary that is not forced until explicitly demanded. Producing the binding's "outermost layer" does not require evaluating the reference:

| Guarded position | Reason |
|-----------------|--------|
| `[fn [let params] BODY]` — inside BODY | Body is a thunk; not evaluated until function is called |
| `[if cond TRUE FALSE]` — inside TRUE or FALSE | Branch is a thunk; only chosen branch is forced |
| `[match scrutinee [case P: BODY]]` — inside BODY | Arm body is a thunk; only matching arm is forced |
| `[cons head TAIL]` — inside TAIL | Tail is a thunk; deferred until next element demanded |
| `[task BODY]` — inside BODY | Task body is a thunk; deferred until task is awaited |
| `[quote EXPR]` — inside EXPR | Quoted expression is never evaluated as code |

A position is **unguarded** if the recursive reference must be evaluated before the binding's outermost value can be produced at all:

| Unguarded position | Reason |
|-------------------|--------|
| `[+ x e]`, `[- x e]`, `[* x e]`, `[/ x e]` — either operand | Arithmetic forces both operands |
| `[= x y]`, `[< x y]`, `[<= x y]` — either operand | Comparison forces both operands |
| `[str x]`, `[str-contains? x s]`, `[str-split x s]` — string arg | String operations force their arguments |
| `[length x]`, `[keys x]`, `[values x]` | Force the collection |
| `[int? x]`, `[str? x]`, `[dict? x]`, `[null? x]`, `[seq? x]`, `[bool? x]` | Type predicates force the value |
| `[has? x k]`, `[get k x]` | Force the collection |
| `[if COND true false]` — the condition | Condition is always forced |
| `[match SCRUTINEE ...]` — the scrutinee | Scrutinee is always forced |
| `[FUNC args]` — the function position | Function must be forced to dispatch |
| `[cons HEAD tail]` — the head | Head is immediately available |
| `[emit x]`, `[deep-materialize x]` | Force the value completely |
| `[map f SEQ]` or `[filter pred SEQ]` — the seq argument when `SEQ` is itself recursive | Traversal forces elements on demand, making self-referential seqs circular |
| `[reduce f init SEQ]` — the seq argument | Forces the entire seq |

**Dict entry value positions are not themselves guarded or unguarded** — they are always thunk-wrapped at the letrec boundary. The guard classification applies to positions *within* each entry's expression.

### Dependency Graph and SCC Analysis

For a dict with letrec bindings `{x₁: e₁, ..., xₙ: eₙ}`, construct a **labeled dependency graph** G:

- **Nodes:** binding names `x₁...xₙ`.
- **Edges:** for each variable reference to `xⱼ` appearing in `eᵢ`, add an edge `xᵢ → xⱼ` labeled **G** (guarded) if the reference is in a guarded position, or **U** (unguarded) if in an unguarded position. A single binding may have both G-labeled and U-labeled edges to the same target (if the target appears in both guarded and unguarded positions within the expression).

Run Tarjan's algorithm to find **strongly connected components** (SCCs). For each SCC:

- **Singleton with no self-loop:** not recursive — no action.
- **Singleton with a self-loop:** recursive self-reference. If the self-loop is labeled G: productive corecursion — accepted. If labeled U: non-productive — assign `Never` to this binding.
- **Multi-node SCC:** mutual recursion. If **all** edges within the SCC are labeled G: productive mutual corecursion — accepted. If **any** edge is labeled U: non-productive cycle — assign `Never` to **all** bindings in the SCC.

The non-productive check uses "any U edge" rather than "all U edges" because a single unguarded dependency breaks the productivity of the entire component: if `a → b` is unguarded, demanding `a` demands `b` immediately, regardless of whether `b → a` is guarded.

### Integration with BAS and the Type Checker

The guardedness checker runs as a dedicated pass between resolution (`src/resolve.rs`) and type inference (`src/typecheck.rs`). It consults the `ResolutionTable` to trace which `VarRef` nodes resolve to which letrec bindings, then produces a `GuardednessErrors: HashSet<NodeId>` — the set of `SurfaceNode` identifiers for bindings in non-productive cycles.

The type checker, in `infer_dict`, checks whether each binding's `NodeId` is in `GuardednessErrors`. If so, it pre-seeds that binding's type as `Never` before running the standard inference on its expression. Type inference then proceeds normally: `Never` propagates via BAS unification to every use site, surfacing as a type error wherever the binding is consumed.

**Error reporting.** The `EvalError` produced (or rather, the type error raised) identifies:
1. The binding that is non-productive.
2. The specific variable reference that is unguarded (with its source span).
3. The other bindings in the SCC, if it is a multi-node cycle.

```
TypeError: non-productive circular definition
  x: [+ x 1]
  ^^^^^^^^^^^ x references itself in a strict (unguarded) position
  Hint: to define an infinite sequence, use [cons head tail] with x in the tail position
```

### Accepted and Rejected: Complete Examples

```tinct
# REJECTED — self-reference in arithmetic (unguarded)
x: [+ x 1]

# REJECTED — self-reference as string argument (unguarded)
label: [str label]

# REJECTED — trivial self-reference with no guard
shadow: shadow

# REJECTED — mutual cycle with one unguarded edge (a demands b immediately)
[a: [str b]   # b in str → unguarded edge a→b
 b: [str a]]  # a in str → unguarded edge b→a

# REJECTED — self-reference in if condition (unguarded)
x: [if x 1 0]   # x is the condition — always forced

# ACCEPTED — self-reference in if branch (guarded)
# Produces a function; calling it produces 1 or recurses — valid
f: [fn [let flag] [if flag 1 [f true]]]

# ACCEPTED — infinite seq of ones (cons tail is guarded)
ones: [cons 1 ones]

# ACCEPTED — infinite seq of naturals
nats: [cons 0 [map [fn [let n] [+ n 1]] nats]]
# 'nats' appears as the seq arg of map — this is the lazy-traversal case.
# map over a seq is productive: element 0 of nats is 0 (from cons), element 1
# is produced by map when the tail is demanded, and so on.
# See §Lazy Traversal Classification below.

# ACCEPTED — mutual corecursion (both tails are guarded)
[evens: [cons 0 odds]
 odds:  [cons 1 evens]]

# ACCEPTED — mutually recursive functions (both bodies are guarded under fn)
[even?: [fn [let n] [if [= n 0] true  [odd?  [- n 1]]]]
 odd?:  [fn [let n] [if [= n 0] false [even? [- n 1]]]]]

# ACCEPTED — function referencing another function (guarded via fn body)
[double:  [fn [let n] [+ n n]]
 quadruple: [fn [let n] [double [double n]]]]
```

### Lazy Traversal Classification

`map` and `filter` applied to a recursive seq binding occupy a special position. On one hand, they do not force the entire seq at definition time — they produce a lazy chain. On the other hand, to produce element `i` of the output, they force element `i` of the input. If the input IS the output (direct self-reference), this creates a cycle: element 0 of `xs = [map f xs]` requires element 0 of `xs`.

The rule: a recursive binding `xs` appearing as the **direct seq argument** to `map` or `filter` is **unguarded** if `xs` appears nowhere else in a guarded position in the same expression.

```tinct
# REJECTED — xs references itself as the direct map input
xs: [map [fn [let n] [+ n 1]] xs]

# ACCEPTED — nats is in the TAIL of cons; map operates on that tail
# cons produces element 0 (concrete: 0) before map is involved
nats: [cons 0 [map [fn [let n] [+ n 1]] nats]]
```

In the accepted case, `nats` appears as the seq argument of `map`, but that `map` expression is in the guarded tail position of `cons`. The outmost layer produced is `(cons 0 ...)` — available without evaluating `nats`. The classification of the `map` reference is therefore guarded, because it is nested within a `cons` tail.

**The general rule:** the guard classification of a reference is determined by the **outermost** enclosing position — the first guard-boundary or strict-boundary encountered walking up the expression tree from the reference to the binding's root expression. A reference nested inside a `cons` tail that is itself inside an arithmetic operation inherits the arithmetic operation's unguarded status.

### What Guardedness Does Not Cover

**Function non-termination.** A mutually recursive function pair like `even?`/`odd?` is accepted by guardedness — function bodies are guarded. But if called with a negative argument, it loops forever. Guardedness checks productivity (something is produced before recursing), not termination (the recursion eventually stops). Termination analysis requires sized types with explicit well-founded measures — a separate, complementary feature.

**Task-body cycles.** `[t: [task [+ t 1]]]` — `t` appears in a `task` body (guarded), so guardedness accepts it. At runtime, when `t` is awaited, the task demands `t` (itself), which is a circular dependency. This is a **weak guard**: the reference is not forced at definition time but is forced when the task is eventually demanded. The checker therefore marks `task`-body references as **weakly guarded** in diagnostics, with a warning (not an error): "recursive reference in task body: will diverge when awaited."

**Dynamic binding.** If a binding name is resolved only at runtime (e.g., via `eval` or a computed key), the checker cannot trace the dependency statically and conservatively treats it as acyclic.

---

## Corecursion as a First-Class Pattern

With guardedness accepted in the type checker, infinite data structures become a documented, encouraged pattern rather than a runtime coincidence.

### Infinite Sequences

```tinct
# Fibonacci sequence
[fibs: [cons 0 [cons 1 [map [fn [let p] [+ p.0 p.1]]
                             [zip fibs [seq-tail fibs]]]]]]

# Iterate — apply f repeatedly starting from seed
iterate: [fn [let f seed]
  [cons seed [iterate f [f seed]]]]

# cycle — repeat a finite seq infinitely
cycle: [fn [let xs]
  [seq-concat xs [cycle xs]]]
```

### Lazy Pipelines

Lazy corecursive sequences compose with `map`, `filter`, and `take` to build lazy pipelines that only compute what is consumed:

```tinct
# First 10 primes — sieve operates lazily over an infinite candidate stream
[candidates: [iterate [fn [let n] [+ n 1]] 2]
 primes:     [sieve candidates]]

# sieve: given a lazy stream, keep head, remove its multiples from tail
sieve: [fn [let stream]
  [let [h: [seq-head stream]
        t: [seq-tail stream]]
  [cons h [sieve [filter [fn [let n] [not [= [mod n h] 0]]] t]]]]]
```

### Type Signatures for Infinite Structures

The type checker infers correct types for corecursive definitions without annotation:

```tinct
ones@[Seq Int]:  [cons 1 ones]
fibs@[Seq Int]:  [cons 0 [cons 1 ...]]
```

`Seq@T` is the type of both finite and infinite sequences — the guardedness checker determines whether the definition is productive; the type system makes no distinction in the type itself. `[take n xs]` always terminates regardless of whether `xs` is finite or infinite, because `take` only demands as many elements as requested.

---

## What Would Change

### New pass: `src/guardedness.rs`

**Current:** No guardedness analysis. Circular definitions are accepted at parse/typecheck time.

**Proposed:** A new compilation pass inserted between `src/resolve.rs` and `src/typecheck.rs`. The pass:

1. Walks each `SurfaceDoc`'s top-level dict and any nested dict expressions, constructing the letrec dependency graph for each scope.
2. For each `VarRef` node in the `ResolutionTable` that resolves to a binding in the same scope, classifies the reference as guarded or unguarded based on its enclosing syntactic context (see §Guard Boundary).
3. Runs Tarjan's SCC algorithm on the dependency graph.
4. For each SCC with any unguarded edge, adds all binding `NodeId`s in the SCC to `GuardednessErrors`.
5. Returns `GuardednessErrors: HashSet<NodeId>` to the caller (the pipeline entry point).

The pass is purely syntactic — it does not depend on types, does not evaluate expressions, and does not modify the AST. It requires only the resolved AST and the `ResolutionTable`.

**Impact:** New file, ~350 lines. One new pipeline stage.

### `src/typecheck.rs` — Pre-seeding `Never` for unguarded bindings

**Current:** `infer_dict` infers each binding's type from its expression.

**Proposed:** At the start of `infer_dict`, for each binding whose `NodeId` is in `GuardednessErrors`, insert a pre-constraint `ty = Never` into the inference state before running `infer_expr` on that binding's expression. The expression is still inferred (for secondary error messages), but the binding's externally-visible type is forced to `Never` via `unify(inferred_ty, Never)`. BAS's lattice operations then propagate `Never` through every use.

**Impact:** ~15 lines in `typecheck_dict.rs`. Minor change.

### `src/error.rs` — New error kind

**Current:** `EvalError::circular_dependency` is a runtime error.

**Proposed:** Add `TypeError::non_productive_cycle { binding_span: Span, unguarded_ref_span: Span, cycle: Vec<String> }`. Surfaced by the type checker (not the evaluator) for statically-detected cycles. The runtime `circular_dependency` error remains for dynamically-detected cycles (dynamic keys, `eval`-produced bindings, task-body cycles).

**Impact:** ~20 lines in `error.rs`. Minor.

### `src/resolve.rs` — ResolutionTable exposure

**Current:** `ResolutionTable` is returned as part of the resolution output.

**Proposed:** No change to the table structure. The guardedness pass reads it directly. One additional field added to the pipeline return value: `GuardednessErrors`.

**Impact:** Negligible.

### `src/lsp/analysis.rs` — Inline diagnostics

**Current:** Type errors appear in hover and diagnostics.

**Proposed:** `non_productive_cycle` errors surface as LSP diagnostics on the binding name span, with a secondary diagnostic on the unguarded reference span. The LSP message identifies: (1) which binding is non-productive, (2) which reference is unguarded and why (with the builtin name), (3) how to fix it (e.g., "wrap in `[fn [let] ...]` to defer evaluation" or "place in `cons` tail position").

**Impact:** ~30 lines in `analysis.rs`. Minor.

### `src/eval.rs` — Runtime cycle detection retained

**Current:** OnceLock blackholing detects cycles at runtime.

**Proposed:** No change. Runtime detection remains for:
- Dynamically-computed binding keys (cannot be statically analyzed)
- `eval`-produced bindings (runtime-synthesized, not in the AST)
- Task-body cycles (weakly guarded — runtime error on demand)
- Cross-node cycles in distributed evaluation (CMH probes)

**Impact:** None. The runtime check is defence-in-depth.

### `stdlib/seq.llt` — Corecursive combinators

**Current:** `iterate`, `cycle`, `zip` may exist but are not documented as corecursive.

**Proposed:** `iterate`, `cycle`, `interleave`, `unfold` documented explicitly as productive corecursive combinators. Their implementations are accepted by the guardedness checker (each uses `cons` with guarded recursive tails). Add to `stdlib/seq.llt`:

```tinct
iterate: [fn [let f seed]
  [cons seed [iterate f [f seed]]]]

cycle: [fn [let xs]
  [seq-concat xs [cycle xs]]]

unfold: [fn [let f state]
  [let [step: [f state]]
  [if [null? step]
    [seq]
    [cons step.value [unfold f step.next]]]]]
```

**Impact:** ~30 lines added to `stdlib/seq.llt`.

### `doc/08-evaluation.md` — Guardedness section

Add a new section §Guardedness describing the guard boundary, the checker's behavior, productive corecursion patterns, and the distinction from function termination analysis.

---

## Prerequisites

- **BAS (`boolean-algebraic-subtyping`, DONE)** — `Type::Never` is required for the type-level propagation of non-productive cycle errors.
- **`runtime-v2.md`** — `SurfaceNode` and `ResolutionTable` replace `VarRef.resolved: RefCell<...>`; the guardedness pass walks `SurfaceExpr` and consults the `ResolutionTable`. The pass is impossible to write cleanly against the current `Expr` enum with RefCell fields.
- **`include-decomp-*`** — prerequisite of `runtime-v2`.

---

## References

- Coquand, T. (1994). "Infinite objects in type theory." *TYPES '93*, LNCS 806, pp. 62–78. — Original formulation of guardedness for corecursive definitions; the syntactic guard criterion.
- Abel, A., Pientka, B., Thibodeau, D. & Setzer, A. (2013). "Copatterns: programming infinite structures by observations." *POPL '13*, pp. 27–38. — Copattern matching as a systematic language mechanism for productive corecursion; informs the `unfold` combinator design.
- Abel, A. (2012). "MiniAgda: integrating sized and dependent types." *PLPV '10*, EPTCS 71, pp. 14–28. — Sized types as the complement to guardedness for termination; confirms the two approaches are orthogonal, not competing.
- Danielsson, N.A. & Altenkirch, T. (2010). "Mixing induction and coinduction." *Draft*. — The interaction between finite and infinite data in a lazy language; confirms that `Seq@T` naturally covers both.
- Thibodeau, D., Cave, A. & Pientka, B. (2016). "Indexed codata types." *ICFP '16*. — Extends guardedness to indexed types; not required for tinct's first implementation but relevant for future `Seq@n T` sized sequences.
- GHC documentation. "Guardedness" in `GHC.Base`. — Haskell's production implementation of the guardedness criterion; the rule "recursive call must appear under a constructor" is the operational precedent for tinct's "guarded position" classification.
- Agda documentation. "Coinduction." — Agda's `♭` (force) and `♯` (delay) operators; the explicit corecursion markers that tinct makes implicit via lazy evaluation.
- Hughes, J., Pareto, L. & Sabry, A. (1996). "Proving the correctness of reactive systems using sized types." *POPL '96*, pp. 410–423. — Sized types for termination; the orthogonal complement to guardedness for function recursion.
