# What If: Higher-Kinded Types and Generic Monadic Composition for tinct

**State:** Proposal

What would it take to give tinct rank-1 higher-kinded types, making `[do]` inference-driven and enabling generic functions polymorphic over any Functor or Monad — without adding full System F-omega or breaking the existing explicit-dispatch model?

## Current State

`doc/whatif/error-patterns.md` introduced `[do monad ...]` — sequential monadic composition via an explicit builder dict. The builder dict carries `bind:` and `pure:` fields; the `[do]` macro desugars to nested `monad.bind` calls. This is tinct's F#-style computation expression model, and it is *complete* for expressing monadic composition:

```tinct
[do result
  [r:    [fetch %nc url]]
  [data: [from-json r.body]]
  [get "max_stable_version" [get "crate" data]]]
```

The result monad dict:
```tinct
result: [bind: and-then  pure: result-ok]
```

Any value with a `bind:` field is a valid monad for `[do]`. This works today with no type system changes. The four stdlib combinators (`and-then`, `result-map`, `result-or`, `result-ok`) and the `result` monad dict are sufficient for all current use cases.

### What's Missing

1. **`[do]` still requires the monad argument.** `[do result ...]` must name the monad explicitly. When the return type of the enclosing function is annotated `@Result`, the monad is already implied — the type checker should be able to infer it.

2. **No generic functions polymorphic over functors.** There is no way to write `traverse` (apply a function that returns a monadic value to each element of a collection, then sequence the results) that works for any `Monad m`. You would have to write `traverse-result` and `traverse-seq` separately.

3. **No `Functor`, `Applicative`, `Monad` typeclass hierarchy.** Generic combinators like `fmap`, `liftA2`, `sequence`, `forM`, `mapM` cannot be expressed as typed library functions — they all require a type variable of kind `* → *`.

## Why Rank-1 HKT Matters for tinct

The explicit `[do monad]` syntax is not wrong — F# has used exactly this model for decades, and F#'s community finds it sufficient. The question is whether tinct wants generic abstraction over the monad, not just convenient syntax for one monad at a time.

**The key gap is `sequence` and `traverse`.** These functions turn a collection of monadic values into a monad of collections:

```tinct
# Fetch all URLs, collecting results or short-circuiting on first failure
[do result
  [results: [sequence result [map [fn [url] [fetch %nc url]] urls]]]
  [map [fn [r] [from-json r.body]] results]]
```

`sequence` is a function of type `Seq (m a) → m (Seq a)` where `m` ranges over monads. Without HKT, this cannot be typed. With rank-1 HKT and a `Monad` typeclass, it is expressible and the type checker enforces that the elements of the sequence have the same monad type as the result.

**The type-checker inference benefit.** With a `Monad` instance, the type checker knows that `[do ...]` inside a function annotated `@Result` uses `and-then` — so it can type-check the binding expressions against `{ok: T} | {err: String}` and report errors if a non-Result expression appears in a binding position.

## Design

### The Kind System Extension

Tinct's type system currently has two kinds:
- Kind `*` — concrete types (`Int`, `String`, `Bool`, `Result`, `{ok: T}`)
- Kind `Row` — row types (field sets for `Handle[...]` and record types)

This proposal adds one new kind:
- Kind `* → *` — type constructors: parameterized types where the parameter is a concrete type

Examples of type constructors (kind `* → *`):
- `Result` — `Result a` is `{ok: a} | {err: String}` for any `a : *`
- `Seq` — `Seq a` is a lazy sequence of values of type `a`
- `Maybe` — `Maybe a` is `a | Null` (absent or present)

The **rank-1 restriction** bounds the complexity: type constructor variables (kind `* → *`) appear only at the outermost level of typeclass constraints. They cannot appear as arguments to other type constructors (`f (g a)` where both `f` and `g` are variables is excluded). This makes kind inference decidable and type checking tractable.

### The Typeclass Hierarchy

```tinct
# Functor: map a function over the contents of a container
[Functor: [class [f@(* → *)]
  [fmap: [fn@(f b) [fn@b [a]] [f@(f a)]]]]]

# Applicative: apply a wrapped function to a wrapped value
[Applicative: [class [f@(* → *)]  extends [Functor f]
  [pure:  [fn@(f a) [a]]]
  [lift2: [fn@(f c) [fn@c [a b]] [f@(f a)] [f@(f b)]]]]]

# Monad: sequential composition
[Monad: [class [m@(* → *)]  extends [Applicative m]
  [bind: [fn@(m b) [m@(m a)] [fn@(m b) [a]]]]]]
```

Instances for the stdlib types:

```tinct
[FunctorResult: [instance [Functor Result]
  [fmap: result-map]]]

[ApplicativeResult: [instance [Applicative Result]
  [pure:  result-ok]
  [lift2: [fn [f ra rb]
    [and-then ra [fn [a] [and-then rb [fn [b] [result-ok [f a b]]]]]]]]]]]

[MonadResult: [instance [Monad Result]
  [bind: and-then]]]

[FunctorSeq: [instance [Functor Seq]
  [fmap: map]]]

[MonadSeq: [instance [Monad Seq]
  [pure:  [fn [x] [x]]]
  [bind:  flat-map]]]
```

### `[do]` Inference

With `Monad` instances registered, `[do]` without an explicit monad argument is valid when the enclosing return type provides enough context:

```tinct
# Explicit (current — always works)
[do result
  [r: [fetch %nc url]]
  [r.body]]

# Inferred (future — requires @Result annotation in scope)
[fetch-and-parse: [fn@Result [url@String]
  [do
    [r:    [fetch %nc url]]      # type checker infers m = Result from @Result
    [data: [from-json r.body]]
    [get "items" data]]]]
```

**Inference rules:**
1. If the enclosing function or binding has a return type annotation `@Result`, `m = Result` and `MonadResult` is resolved.
2. If the `[do]` block's first expression has inferred type `{ok: T} | {err: E}`, `m = Result`.
3. If neither provides context, `[do]` requires an explicit monad argument (backward compat).

The explicit `[do monad ...]` form always works and takes priority over inference. Old code is unaffected.

### Generic Functions

With rank-1 HKT and the typeclass hierarchy, the following generic functions become expressible and type-checked:

```tinct
# sequence: Seq (m a) → m (Seq a) — collect monadic values into one monad
sequence: [fn@(m (Seq a)) [m@Monad xs@(Seq (m a))]
  [reduce
    [fn [acc x]
      [m.bind acc [fn [as]
      [m.bind x   [fn [a]
        [m.pure [append as a]]]]]]]
    [m.pure []]
    xs]]

# traverse: (a → m b) → Seq a → m (Seq b)
traverse: [fn@(m (Seq b)) [m@Monad f@Fn xs@(Seq a)]
  [sequence m [map f xs]]]

# when: m () conditioned on a Bool
when: [fn@(m Null) [m@Monad cond@Bool action@(m Null)]
  [if cond action [m.pure []]]]

# liftM2: generalisation of lift2 through bind
liftM2: [fn@(m c) [m@Monad f@Fn ma@(m a) mb@(m b)]
  [m.bind ma [fn [a] [m.bind mb [fn [b] [m.pure [f a b]]]]]]]
```

Usage:

```tinct
# Fetch all URLs, short-circuit on first failure
[sequence result [map [fn [url] [fetch %nc url]] urls]]
# → {ok: [r1 r2 r3]} | {err: "..."}

# Same, with explicit URL processing
[traverse result [fn [url] [do result [r: [fetch %nc url]] [from-json r.body]]] urls]
# → {ok: [data1 data2 data3]} | {err: "..."}

# List comprehension with Seq monad
[do seq-monad
  [x: [1 2 3]]
  [y: [10 100]]
  [seq-monad.pure [* x y]]]
# → [10 100 20 200 30 300]
```

### Interaction with Row Polymorphism

Tinct already has `Handle[R]` where `R` is a row type — this is effectively `Handle : Row → *`, a type constructor taking a row. The new `* → *` kind is the value-type-parameter analog. They are orthogonal:

- `Handle[Tls Stream]` uses kind `Row`
- `Result a` uses kind `* → *`
- `Map[K V]` uses kind `* → * → *` (which rank-1 HKT supports for concrete applications)

Row variables (kind `Row`) remain unchanged. Type constructor variables (kind `* → *`) are new and separate.

### Interaction with BAS

BAS operates on types (kind `*`). With rank-1 HKT, the BAS lattice needs to handle:
- `m a | m b` — union of applications of the same type constructor to different types
- Subtype rules for `m a <: m b` when `f : Functor` and `a <: b` (functorial subtyping)

These are extensions of BAS to the parameterized case. For covariant type constructors (Seq, Result's ok branch), `a <: b` implies `m a <: m b`. For invariant or contravariant positions, the rule differs. BAS's lattice structure accommodates this by extending the boolean algebra to parameterized types — `m a` is an atom in the lattice for each concrete `(m, a)` pair, and `m a | m b` is their join.

This is a non-trivial extension but a principled one: the BAS lattice remains boolean and the constraint solver handles parameterized types by treating applications as lattice atoms during constraint generation.

### Why Not Effects Systems

Algebraic effects (Koka, Frank, Unison) are the major alternative to monads for structuring side effects. They are not appropriate for tinct for one reason: **lazy evaluation and algebraic effects do not compose cleanly**.

In a strict (eager) language, an effectful expression executes when evaluated — effects are ordered by control flow. In a lazy language, a thunk is evaluated on demand, potentially reordering or deduplicating effects. Haskell's IO monad exists precisely to give effects a total order in an otherwise lazy language. Algebraic effects assume strict evaluation.

Tinct's lazy evaluation model makes the IO monad (and Result monad for failure) the right abstraction — they explicitly sequence effects through bind. Effects systems would require tinct to become strict or to add explicit thunk forcing in the effects semantics, neither of which is acceptable.

### Backward Compatibility

Every existing `[do monad ...]` call is valid in the new model:
- The explicit monad argument is still accepted — it takes priority over inference
- The bind-field dispatch (looking up `monad.bind`) still works for dicts without a registered `Monad` instance
- User-defined monad dicts that predate the `Monad` typeclass are unaffected

The upgrade path: existing code compiles unchanged. New code can drop the explicit monad argument when the return type is annotated. No migration is required.

## What Would Change

### Type System (`src/types.rs`)

**New kind:**
```rust
pub enum Kind {
    Star,          // *  — concrete types (existing)
    Row,           // Row — record field types (existing)
    Arrow(Box<Kind>, Box<Kind>),  // * → * — NEW: type constructor kinds
}
```

**New type form:**
```rust
pub enum Type {
    // ... existing variants ...
    App(Box<Type>, Box<Type>),   // NEW: type constructor application (Result a, Seq Int, etc.)
    TyCon(String),               // NEW: type constructor variable (f, m, t — kind * → *)
}
```

**Impact:** Moderate — new variants require new inference rules and kind checking.

### Type Checker (`src/typecheck.rs`)

**Kind inference:** Determine kinds of type expressions; check that `m` in `Monad m` is used at kind `* → *`.

**Typeclass resolution:** Extend the existing typeclass instance resolution to handle `* → *` typeclasses. When `[do]` is used without an explicit monad, consult the return type annotation for `m` and resolve the `Monad m` instance.

**Application type inference:** `m a` where `m : * → *` and `a : *` produces an `App(TyCon(m), a)` type, unified against concrete types (`Result`, `Seq`) at call sites.

**Impact:** Moderate to major — the most significant type checker change since typeclasses were added.

### Standard Library (`stdlib/prelude.llt`)

New generic functions (once HKT is available):
- `sequence` — `Seq (m a) → m (Seq a)`
- `traverse` — `(a → m b) → Seq a → m (Seq b)`
- `liftM2` — `(a → b → c) → m a → m b → m c`
- `when` — `Bool → m () → m ()`
- `forM` — flipped `traverse` (xs before f, for readability)

New typeclass instances:
- `MonadResult`, `FunctorResult`, `ApplicativeResult`
- `MonadSeq`, `FunctorSeq`, `ApplicativeSeq`
- `FunctorMaybe`, `MonadMaybe` (if `Maybe` is added as a stdlib type)

**Impact:** Additive — new functions alongside existing ones.

### `[do]` Macro (`stdlib/macros.llt`)

Extended to support both forms:
- `[do monad steps...]` — existing explicit form (unchanged)
- `[do steps...]` — new inferred form; macro emits a typeclass constraint for resolution

**Impact:** Minor — the desugaring is identical; only the monad lookup changes.

## Prerequisites

- **Boolean Algebraic Subtyping** (`doc/whatif/boolean-algebraic-subtyping.md`) — required for `m a | m b` as a well-formed union type in the type checker; BAS's constraint solver handles parameterized type application in the lattice.
- **Existing typeclasses** — the `Eq`, `Ord` typeclass infrastructure from the typing cluster (complete); `Monad` extends this to kind `* → *`.
- **`[do]` macro** — already implemented via `error-patterns` proposal; the explicit-dict form is the implementation base.

## References

- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction, and Polymorphism." *ACM Computing Surveys 17(4)*. — Parameterized types and type constructors; the formal basis for kind `* → *`.
- Jones, M.P. (1993). "A System of Constructor Classes: Overloading and Implicit Higher-Order Polymorphism." *FPCA '93*. — The original constructor class paper introducing `Functor`, `Monad` as typeclasses in a kind-polymorphic system; the direct ancestor of Haskell's HKT.
- Jones, M.P. (1995). "Functional Programming with Overloading and Higher-Order Polymorphism." *Advanced Functional Programming*, Lecture Notes in Computer Science 925. — Rank-1 kind polymorphism as a tractable subset of full HKT; shows decidability and practical implementation.
- Kiselyov, O. (2012). "Typed Tagless Final Interpreters." *Generic and Indexed Programming*, Lecture Notes in Computer Science 7470. — Defunctionalization as the HKT-free alternative; the theoretical basis for tinct's current `[do monad]` explicit-dispatch model.
- Leijen, D. (2014). "Koka: Programming with Row Polymorphic Effect Types." *MSFP '14*. — Effect systems as an alternative to monads for structuring I/O; argued against for tinct because of interaction with lazy evaluation.
- Marlow, S. & Peyton Jones, S. (2010). "Haskell 2010 Language Report." Ch. 6 (Typeclasses). — The `Functor`/`Applicative`/`Monad` hierarchy as implemented; the reference for what rank-1 HKT achieves in practice.
- Syme, D., Granicz, A. & Cisternino, A. (2010). *Expert F#*. Ch. 12 (Computation Expressions). — F# computation expressions as explicit builder-dict approach; validates that explicit monad dicts (`[do monad]`) are sufficient for production use without HKT.
- Wadler, P. (1992). "The Essence of Functional Programming." *POPL '92*. — Monads as a unified model for IO, state, exceptions; the theoretical foundation for `Monad m` as a typeclass.
