# What If: Higher-Kinded Types and Generic Monadic Composition for tinct

**State:** Accepted — 2026-05-11

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

4. **`Mappable` is hardcoded.** The existing `Mappable` constraint gives `$map` and `$filter` precise types over `Record` and `Seq`, but it is implemented as a fixed instance set in `src/typecheck.rs` with no user-extensibility. User-defined types cannot declare themselves `Mappable`. Full `Mappable` is the concrete near-term motivation for this proposal — see §Mappable Constraint.

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

**The type-checker inference benefit.** With a `Monad` instance, the type checker knows that `[do ...]` inside a function annotated `@Result` uses `and-then` — so it can type-check the binding expressions against `[ok: T  err: String]` and report errors if a non-Result expression appears in a binding position.

## Syntax Design

### Kind Annotations

Tinct avoids infix operators. The kind `* → *` (type constructor) is written as the reserved kind-level name `Operator`. Kind annotations follow the existing `@Type` annotation syntax:

| Annotation | Meaning |
|------------|---------|
| `f@Operator` | `f` is an unconstrained type constructor (kind `* → *`) |
| `m@Monad` | `m` is a type constructor with a `Monad` instance |
| `f@Functor` | `f` is a type constructor with a `Functor` instance |
| `f@Mappable` | `f` is a type constructor with a `Mappable` instance |

A constrained annotation like `m@Monad` implies `Operator` — no separate kind annotation is needed when the constraint provides the kind.

### Type Constructor Application in Annotation Positions

In annotation positions, `[f a]` (square brackets without colons) denotes type constructor application when `f` is either an Operator-kinded type variable (from a class or function annotation) or a user-defined parameterized type alias. Built-in type constructors (`Seq`, `Map`) keep their existing `@Seq@T`, `@Map@K@V` syntax:

| Syntax | When valid | Meaning |
|--------|-----------|---------|
| `@[m a]` | `m` is an Operator type variable | Apply type constructor `m` to type `a` |
| `@[m [Seq a]]` | `m` is an Operator type variable | `m` applied to `Seq a` (nested) |
| `@[MyAlias Int]` | `MyAlias` is a user parameterized type alias | Alias instantiation |
| `@Seq@Int` | always | Builtin sequence of `Int` (existing syntax) |
| `@[key: T]` | always | Record type (has colon — not application) |

When an Operator-kinded variable is resolved to a builtin (e.g., `m` resolves to `Seq`), the resulting type is normalized to the builtin form (`Seq(T)`, not `App(Seq, T)`) during instance resolution. This preserves the existing `Type::Seq` variant and avoids introducing a duplicate representation alongside it.

The disambiguation rule: square brackets with at least one colon form a record type. Square brackets without colons are type application — valid only for Operator variables and user aliases, not for bare builtin names.

### Class and Instance Declarations

Class declarations use existing `class` syntax with `@Operator` kind annotations:

```tinct
[ClassName: [class [typeParam@Operator]
  [method: method-type]
  [method: method-type]]]
```

With superclass constraint (prefix `extends`):

```tinct
[ClassName: [class [typeParam@Operator] extends [SuperClass typeParam]
  [method: method-type]]]
```

Instance declarations bind a concrete type constructor to a class:

```tinct
[InstanceName: [instance [ClassName ConcreteType]
  [method: implementation]]]
```

## The Typeclass Hierarchy

### Functor

```tinct
[Functor: [class [f@Operator]
  [fmap: [fn@[f b] [fn@b [a]  [f a]]]]]]
```

`fmap` takes a function `a → b` and an `f a`, returning `f b`.

Instances:

```tinct
[FunctorResult: [instance [Functor Result]
  [fmap: result-map]]]

[FunctorSeq: [instance [Functor Seq]
  [fmap: map]]]
```

### Applicative

```tinct
[Applicative: [class [f@Operator] extends [Functor f]
  [pure:  [fn@[f a] [a]]]
  [lift2: [fn@[f c] [fn@c [a b]  [f a]  [f b]]]]]]
```

`pure` wraps a value in the container. `lift2` applies a two-argument function inside the container.

Instances:

```tinct
[ApplicativeResult: [instance [Applicative Result]
  [pure:  result-ok]
  [lift2: [fn [f ra rb]
    [and-then ra [fn [a]
    [and-then rb [fn [b]
      [result-ok [f a b]]]]]]]]]]

[ApplicativeSeq: [instance [Applicative Seq]
  [pure:  [fn [x] [x]]]
  [lift2: [fn [f sa sb]
    [flat-map sa [fn [a]
      [map [fn [b] [f a b]] sb]]]]]]]
```

### Monad

```tinct
[Monad: [class [m@Operator] extends [Applicative m]
  [bind: [fn@[m b] [[m a]  fn@[m b] [a]]]]]]
```

`bind` sequences a monadic value with a function that returns a new monadic value.

Instances:

```tinct
[MonadResult: [instance [Monad Result]
  [bind: and-then]]]

[MonadSeq: [instance [Monad Seq]
  [bind: flat-map]]]
```

### Mappable

The `Mappable` class subsumes the current hardcoded constraint. It is a supertype of `Functor` — every `Functor` is `Mappable`, but `Mappable` requires only a weaker `map` contract (no naturality law enforcement):

```tinct
[Mappable: [class [f@Operator]
  [map: [fn@[f b] [fn@b [a]  [f a]]]]]]

[MappableSeq:    [instance [Mappable Seq]    [map: map]]]
[MappableRecord: [instance [Mappable Record] [map: map]]]
```

`$map` and `$filter` are given precise types via `Mappable` rather than hardcoded special cases in `src/typecheck.rs`. User-defined types can implement `Mappable` by declaring an instance.

### Foldable

`Foldable` generalizes fold/reduce over any container structure. It enables `sequence` and `traverse` to work on any foldable container, not just `Seq`:

```tinct
[Foldable: [class [t@Operator]
  [fold:   [fn@b [fn@b [b a]  b  [t a]]]]
  [to-seq: [fn@[Seq a] [[t a]]]]]]

[FoldableSeq:    [instance [Foldable Seq]
  [fold:   reduce]
  [to-seq: [fn [xs] xs]]]]

[FoldableRecord: [instance [Foldable Record]
  [fold:   reduce]
  [to-seq: values]]]
```

`Foldable` is the companion to `Functor`: `Functor` maps over structure, `Foldable` collapses structure. Together they enable generic container processing without knowing the concrete container type. The method is named `fold` (not `foldl`/`foldr`) because tinct sequences are finite and materialized — the left/right distinction applies only to lazy infinite structures. `FoldableSeq.fold = reduce` maps cleanly since both use accumulator-first, element-second argument order.

### Traversable

`Traversable` combines `Functor` and `Foldable`: it maps a structure-preserving function over a container while collecting effects into an `Applicative`. It is what makes `sequence` and `traverse` generic over any container, not just `Seq`:

```tinct
[Traversable: [class [t@Operator]
  extends [Functor t]
  extends [Foldable t]
  [traverse: [fn@[f [t b]] [f@Applicative  fn@[f b] [a]  [t a]]]]]]

[TraversableSeq: [instance [Traversable Seq]
  [traverse: [fn [f xs] [sequence f [map f xs]]]]]]

[TraversableResult: [instance [Traversable Result]
  [traverse: [fn [f r]
    [match r
      [Ok a]  [f.fmap Ok [f a]]
      [Err e] [f.pure [Err e]]]]]]]

[TraversableMaybe: [instance [Traversable Maybe]
  [traverse: [fn [f ma]
    [match ma
      [Some a] [f.fmap Some [f a]]
      [None]   [f.pure [None]]]]]]]
```

With `Traversable`, `sequence` and `traverse` are fully generic over any traversable container:

```tinct
# sequence: t (f a) → f (t a) — collect effects from any Traversable
sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
  [traverse f [fn [x] x] xs]]

# traverse: (a → f b) → t a → f (t b) — map with effects over any Traversable
traverse: [fn@[f [t b]] [f@Monad  t@Traversable  fn@[f b] [a]  xs@[t a]]
  [t.traverse f xs]]
```

This replaces the `Seq`-specific implementations in §Generic Functions.

### Appendable

`Appendable` is a kind-`*` typeclass (not `Operator`-kinded) that generalizes concatenation. It replaces the current hardcoded fixed-instance set in `src/typecheck.rs`:

```tinct
[Appendable: [class [a]
  [append: [fn@a [a a]]]
  [empty:  a]]]

[AppendableStr:    [instance [Appendable Str]    [append: str]     [empty: ""]]]
[AppendableSeq:    [instance [Appendable [Seq b]] [append: concat]  [empty: []]]]
[AppendableRecord: [instance [Appendable Record]  [append: merge]   [empty: []]]]
```

`$concat` and `$conj` are given precise types via `Appendable` rather than hardcoded special cases.

`Appendable` is kind-`*` — its parameter `a` is a concrete type, not a type constructor. `AppendableStr` (`a = Str`), `AppendableRecord` (`a = Record`), and `AppendableSeq` (`a = [Seq b]` for any `b`) are all well-kinded.

The `AppendableSeq` instance head `[Seq b]` has a free type variable `b` — a **parameterized instance head**. The instance resolver must pattern-match `Seq(T)` for any `T` and extract `b = T` to thread through `concat` and `empty`. This extends the current `satisfies_constraint` approach (which already does `matches!(ty, Type::Seq(_))`) to a full instance resolution path that recovers the element type. This is included here rather than deferred to avoid accumulating follow-on phases.

### Maybe

`Maybe` is a stdlib ADT representing optional values. It is the simplest non-trivial `Monad` instance and serves as a test case for the full typeclass hierarchy:

```tinct
Maybe: [type [a] [Some a] | [None]]

[FunctorMaybe: [instance [Functor Maybe]
  [fmap: [fn [f ma]
    [match ma
      [Some a] [Some [f a]]
      [None]   [None]]]]]]

[ApplicativeMaybe: [instance [Applicative Maybe]
  [pure: Some]
  [lift2: [fn [f ma mb]
    [match ma
      [Some a] [match mb
        [Some b] [Some [f a b]]
        [None]   [None]]
      [None] [None]]]]]]

[MonadMaybe: [instance [Monad Maybe]
  [bind: [fn [ma f]
    [match ma
      [Some a] [f a]
      [None]   [None]]]]]]
```

Usage:

```tinct
# Safe dict lookup chaining — short-circuits on first missing key
[do MonadMaybe
  [user:  [get? "user" config]]
  [email: [get? "email" user]]
  [domain: [get? "domain" [parse-email email]]]
  domain]
# → [Some "example.com"] | [None]
```

## Generic Functions

With rank-1 HKT and the typeclass hierarchy, the following generic functions become expressible and type-checked. The monad is passed as an explicit first argument — this preserves the existing `[do monad ...]` dispatch model and avoids implicit typeclass dictionary threading at the expression level.

```tinct
# sequence: collect a Seq of monadic values into a single monad
sequence: [fn@[m [Seq a]] [m@Monad  xs@[Seq [m a]]]
  [reduce
    [fn [acc x]
      [m.bind acc [fn [as]
      [m.bind x   [fn [a]
        [m.pure [append as a]]]]]]]
    [m.pure []]
    xs]]

# traverse: map a monadic function over a Seq and collect results
traverse: [fn@[m [Seq b]] [m@Monad  f@fn@[m b] [a]  xs@[Seq a]]
  [sequence m [map f xs]]]

# forM: traverse with arguments flipped (collection before function)
forM: [fn@[m [Seq b]] [m@Monad  xs@[Seq a]  f@fn@[m b] [a]]
  [traverse m f xs]]

# when: conditionally execute a monadic action
when: [fn@[m []] [m@Monad  cond@Bool  action@[m []]]
  [if cond action [m.pure []]]]

# liftM2: lift a two-argument function into the monad
liftM2: [fn@[m c] [m@Monad  f@fn@c [a b]  ma@[m a]  mb@[m b]]
  [m.bind ma [fn [a]
  [m.bind mb [fn [b]
    [m.pure [f a b]]]]]]]
```

Usage:

```tinct
# Fetch all URLs, short-circuit on first failure
[sequence result [map [fn [url] [fetch %nc url]] urls]]
# → [ok: [r1 r2 r3]] | [err: "..."]

# Fetch and parse all URLs
[traverse result [fn [url]
  [do result
    [r:    [fetch %nc url]]
    [from-json r.body]]]
  urls]
# → [ok: [data1 data2 data3]] | [err: "..."]

# List comprehension with Seq monad
[do MonadSeq
  [x: [1 2 3]]
  [y: [10 100]]
  [MonadSeq.pure [* x y]]]
# → [10 100 20 200 30 300]
```

## `[do]` Inference

With `Monad` instances registered, `[do]` can infer the monad from the enclosing return type annotation:

```tinct
# Explicit form (current — always works)
[do result
  [r: [fetch %nc url]]
  [r.body]]

# Inferred form (requires @Result annotation on enclosing function)
[fetch-and-parse: [fn@[ok: Str  err: Str] [url@Str]
  [do
    [r:    [fetch %nc url]]
    [data: [from-json r.body]]
    [get "items" data]]]]
```

**Inference rules (in priority order):**

1. If the enclosing function has an explicit return type annotation `@T` where `T` unifies with `App(m, _)` for a registered `Monad m` instance, use that instance. (`@Result` unifies with `App(Result, a)` → `MonadResult`)
2. If the first `[do]` binding's right-hand side has inferred type `App(m, a)` where `m` has a registered `Monad` instance, use that instance. Nominal ADT constructors produce `App` types: `[Ok 42]` infers as `App(Result, Int)`, so `m = Result` is immediately available.
3. If neither provides context, `[do]` requires an explicit monad argument. Backward compatible — existing `[do monad ...]` calls are unaffected.

The explicit `[do monad ...]` form always takes priority over inference. The `[do]` desugaring is identical in both forms — monad lookup is the only difference. Existing code is unaffected.

**Implementation:** `[do]` is currently a stub in `stdlib/macros.llt`. The explicit form desugars `[x: expr] rest` to `[monad.bind expr [fn [x] [do monad rest]]]` and non-binding steps to `[monad.bind expr [fn [_] [do monad rest]]]`. The inferred form emits a typeclass constraint that the type checker resolves to a concrete monad dict before handing back to the evaluator.

## Formal Type Rules

### Kind System

The kind grammar extends with one new production:

```
Kind ::= *              -- concrete types
       | Row            -- record field sets
       | Operator        -- type constructors (kind * → *)
```

`Operator` is notation for `* → *`. The parser treats it as a reserved kind-level name, not a type.

### Type Constructor Application

Two new `Type` variants in `src/types.rs`:

```rust
pub enum Kind { Star, Row, Operator }

pub enum Type {
    // ... existing variants ...
    App(Box<Type>, Box<Type>),  // type constructor application: f applied to a
    Operator(String),              // type constructor variable: f, m, t
}
```

Unification extends with two new cases:

```
UNIFY-OPERATOR:
  m ∉ ftv(T)
  ──────────────────────────
  unify(Operator(m), T) = [m ↦ T]

  unify(T, Operator(m)) = [m ↦ T]   (symmetric)

UNIFY-APP:
  unify(f₁, f₂) = θ₁    unify(θ₁(a₁), θ₁(a₂)) = θ₂
  ─────────────────────────────────────────────────────
         unify(App(f₁, a₁), App(f₂, a₂)) = θ₂ ∘ θ₁
```

UNIFY-OPERATOR binds a type constructor variable `m` to a concrete type constructor `T` (e.g., `Operator("m")` against `Result`). The occurs check `m ∉ ftv(T)` prevents infinite kinds.

### Typeclass Resolution for HKT

A typeclass constraint `C m` where `m : Operator` resolves by:

1. Look up `m` in the `ClassEnv` instance table.
2. Find an `instance [C M]` entry where `M` unifies with `m`.
3. Substitute the instance's method implementations.

For type inference, `App(Operator("m"), a)` is unified against known concrete applications (`App(Result, T)`, `App(Seq, T)`) to resolve `m`.

### Kind Checking

Kind checking is a pre-pass before type inference:

```
KIND-OPERATOR:
  Γ ⊢ f : Operator    Γ ⊢ a : *
  ─────────────────────────────
       Γ ⊢ [f a] : *

KIND-CLASS-PARAM:
  annotation is @Operator or @C where C is an Operator class
  ─────────────────────────────────────────────────────────
         parameter has kind Operator in method signatures
```

The rank-1 restriction: `Operator` variables appear only at the outermost position of class constraints. `App(Operator("f"), Operator("g"))` (where `g` is also a variable) is excluded. This keeps kind inference decidable.

## Interaction with Row Polymorphism

Tinct already has `Handle[R]` where `R` is a row type — this is `Handle : Row → *`, a type constructor taking a row argument. The new `Operator` kind is the value-type-parameter analog. They are orthogonal:

- `Handle[Tls Stream]` uses `Row` kind
- `@[Result T]` uses `Operator` kind
- `@[Map K V]` uses `Operator → Operator → *` (rank-2, supported for concrete applications only)

Row variables remain unchanged. `Operator` variables are new and separate. The existing `Row`-kinded `Handle` is not affected.

## Interaction with BAS

BAS operates on types of kind `*`. With rank-1 HKT, the BAS constraint solver extends to handle type constructor applications:

- **Application atoms:** `App(f, a)` is an atom in the BAS lattice for each concrete `(f, a)` pair. `App(Result, Int)` and `App(Result, Str)` are distinct atoms.
- **Covariant functorial subtyping:** For covariant type constructors (all `Functor` instances are covariant in their argument), `a <: b` implies `App(f, a) <: App(f, b)`. This is the functorial subtyping rule.
- **Join of applications (one-directional):** `App(m, a) | App(m, b) <: App(m, a | b)` for covariant `m`. This follows directly from covariance: since `a <: a | b` and `b <: a | b`, functorial subtyping gives `App(m, a) <: App(m, a|b)` and `App(m, b) <: App(m, a|b)`, so their join is also a subtype. The reverse direction — `App(m, a|b) <: App(m, a) | App(m, b)` — is not asserted and is unsound for diagonal functors (those that use their type parameter in more than one structural position).

The BAS lattice remains boolean. The constraint solver handles `App` by treating concrete applications as atoms during constraint generation and applying functorial subtyping during the subtype check.

Contravariant positions (e.g., the input type of a function inside a container) require explicit annotation or remain `Unknown`. This is acceptable at rank-1.

## Why Not Effect Systems

Algebraic effects (Koka, Frank, Unison) are the major alternative to monads for structuring side effects. They are not appropriate for tinct for one reason: **lazy evaluation and algebraic effects do not compose cleanly**.

In a strict language, an effectful expression executes when evaluated — effects are ordered by control flow. In a lazy language, a thunk is evaluated on demand, potentially reordering or deduplicating effects. Haskell's IO monad exists precisely to give effects a total order in an otherwise lazy language. Algebraic effects assume strict evaluation.

Tinct's lazy evaluation model makes the IO monad (and Result monad for failure) the right abstraction — they explicitly sequence effects through bind. Effect systems would require tinct to become strict or to add explicit thunk materialization in the effects semantics, neither of which is acceptable.

## Mappable Constraint

The existing `Mappable` constraint gives `$map` and `$filter` precise types over both `Record` and `Seq`. It is currently implemented as a hardcoded fixed-instance set in `src/typecheck.rs` — the constraint is checked but no user-defined type can declare itself `Mappable`, and the implementation does not go through the normal typeclass resolution path.

Full `Mappable` requires `Operator` kind support: `Mappable` is a class parameterized by a type constructor `f`, and `$map` is given the type `fn@[f b] [fn@b [a]  [f a]]` via the `Mappable f` constraint. Once rank-1 HKT is available:

1. The hardcoded `Mappable` special case in `src/typecheck.rs` is replaced by a normal class declaration.
2. `MappableSeq` and `MappableRecord` become normal instance declarations.
3. User types can implement `Mappable` by declaring an instance.

`Mappable` is a weaker contract than `Functor` — it requires only a `map` operation with no naturality law enforcement. `Functor` implies `Mappable` but not vice versa.

## Backward Compatibility

Every existing `[do monad ...]` call is valid in the new model:
- The explicit monad argument is still accepted and takes priority over inference.
- The bind-field dispatch (looking up `monad.bind`) still works for dicts without a registered `Monad` instance.
- User-defined monad dicts that predate the `Monad` typeclass are unaffected.

The upgrade path: existing code compiles unchanged. New code can drop the explicit monad argument when the return type is annotated. No migration is required.

## What Would Change

### Parser (`src/parser.rs`, `src/lexer.rs`)

- Recognize `Operator` as a reserved kind-level name in annotation positions.
- In annotation positions, parse `[f a]` (no colons) as `Expr::TypeApp(f, a)` when `f` is an Operator-kinded type variable or a user-defined parameterized type alias. Built-in names (`Seq`, `Map`, etc.) continue to use the existing `@Seq@T` path and produce `Type::Seq(T)` directly — no `App(Seq, T)` variant for builtins. When an Operator variable is resolved to a builtin at instance resolution time, normalize to the builtin type form.
- Extend `class` declaration parsing to accept `extends [SuperClass param]` clause.

### Type System (`src/types.rs`)

New variants:
```rust
pub enum Kind { Star, Row, Operator }

pub enum Type {
    // ... existing variants ...
    App(Box<Type>, Box<Type>),  // type constructor application
    Operator(String),              // type constructor variable
}
```

New unification cases (`src/type_unify.rs`): `UNIFY-OPERATOR` and `UNIFY-APP` as above.

### Type Checker (`src/typecheck.rs`)

- **Kind inference pass:** Determine kinds of type expressions before HM inference. Check type constructor variables are used at kind `Operator` in class method signatures.
- **Typeclass resolution for HKT:** Extend `ClassEnv` lookup to handle `* → *` classes. When `[do]` is used without an explicit monad, consult the return type for `m` and resolve the `Monad m` instance.
- **`App` type inference:** Unify `App(Operator("m"), a)` against concrete applications to resolve `m`.
- **`Mappable` + `Appendable` rewrite:** Remove both hardcoded fixed-instance sets; replace with normal class + instance resolution.

### Standard Library (`stdlib/prelude.llt`)

New generic functions: `sequence`, `traverse`, `forM`, `when`, `liftM2`.

New class declarations: `Functor`, `Applicative`, `Monad`, `Foldable`, `Traversable`, `Mappable`, `Appendable`, `Equatable`, `Comparable`, `Showable`.

New type: `Maybe` ADT (`[Some a] | [None]`).

New instances: `FunctorResult`, `ApplicativeResult`, `MonadResult`, `FoldableResult`, `TraversableResult`, `FunctorSeq`, `ApplicativeSeq`, `MonadSeq`, `FoldableSeq`, `TraversableSeq`, `FoldableRecord`, `MappableSeq`, `MappableRecord`, `AppendableStr`, `AppendableSeq`, `AppendableRecord`, `FunctorMaybe`, `ApplicativeMaybe`, `MonadMaybe`, `TraversableMaybe`.

`Numeric` remains a hardcoded fixed-instance set — its mixed-type arithmetic semantics (`Int + Float → Float`) require multi-parameter type classes to express correctly, which are out of scope. The other four previously-hardcoded constraints (`Equatable`, `Comparable`, `Showable`, `Mappable`, `Appendable`) are all replaced by proper class/instance declarations.

### Documentation (`doc/06-type-inference.md`)

Add §Type Classes formal rules section (deferred from `type-classes-full`): constraint generation rules, entailment checking algorithm, dictionary elaboration, instance resolution, superclass extraction. This section documents the existing `ClassEnv`/`InstanceEnv` machinery plus the new HKT extensions.

### `[do]` Macro (`stdlib/macros.llt`)

Extended to support inferred form: when no explicit monad argument is present, the macro emits a typeclass constraint for resolution by the type checker. The desugaring is identical; only the monad lookup changes.

## Prerequisites

- **Boolean Algebraic Subtyping** (completed) — required for `App(m, a) | App(m, b)` as a well-formed union type; BAS's constraint solver extends to handle type constructor application atoms.
- **Existing typeclasses** (completed) — `Eq`, `Ord`, `Numeric`, `Mappable` fixed-instance infrastructure; `Monad` extends this to `Operator` kind.
- **`[do]` macro** (completed) — already implemented via `error-patterns` proposal; the explicit-dict form is the implementation base.

## References

- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction, and Polymorphism." *ACM Computing Surveys 17(4)*. — Parameterized types and type constructors; the formal basis for kind `* → *`.
- Jones, M.P. (1993). "A System of Constructor Classes: Overloading and Implicit Higher-Order Polymorphism." *FPCA '93*. — The original constructor class paper introducing `Functor`, `Monad` as typeclasses in a kind-polymorphic system; the direct ancestor of Haskell's HKT.
- Jones, M.P. (1995). "Functional Programming with Overloading and Higher-Order Polymorphism." *Advanced Functional Programming*, Lecture Notes in Computer Science 925. — Rank-1 kind polymorphism as a tractable subset of full HKT; shows decidability and practical implementation.
- Kiselyov, O. (2012). "Typed Tagless Final Interpreters." *Generic and Indexed Programming*, Lecture Notes in Computer Science 7470. — Defunctionalization as the HKT-free alternative; the theoretical basis for tinct's current `[do monad]` explicit-dispatch model.
- Leijen, D. (2014). "Koka: Programming with Row Polymorphic Effect Types." *MSFP '14*. — Effect systems as an alternative to monads for structuring I/O; argued against for tinct because of interaction with lazy evaluation.
- Marlow, S. & Peyton Jones, S. (2010). "Haskell 2010 Language Report." Ch. 6 (Typeclasses). — The `Functor`/`Applicative`/`Monad` hierarchy as implemented; the reference for what rank-1 HKT achieves in practice.
- Syme, D., Granicz, A. & Cisternino, A. (2010). *Expert F#*. Ch. 12 (Computation Expressions). — F# computation expressions as explicit builder-dict approach; validates that explicit monad dicts (`[do monad]`) are sufficient for production use without HKT.
- Wadler, P. (1992). "The Essence of Functional Programming." *POPL '92*. — Monads as a unified model for IO, state, exceptions; the theoretical foundation for `Monad m` as a typeclass.
