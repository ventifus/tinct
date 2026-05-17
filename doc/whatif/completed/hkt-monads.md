# What If: Higher-Kinded Types, Generic Monadic Composition, and Label-Polymorphic Field Access for tinct

**State:** Accepted — 2026-05-11

What would it take to give tinct rank-1 higher-kinded types, making `[do]` inference-driven and enabling generic functions polymorphic over any Functor or Monad — and to give `get`/`get-in` precise, label-polymorphic types via a `HasField` qualified-type constraint — without adding full System F-omega or breaking the existing explicit-dispatch model?

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

5. **`get` and `get-in` return `Unknown`.** Field access on TypeVar-typed dicts, union-typed dicts, or with non-literal keys returns `Unknown`. BAS union distribution over field access — `get "port" (A | B)` returning `A.port | B.port` — is unimplemented. Label-polymorphic functions that take a field name as a parameter have no precise type. `get-in` does not exist as a typed special form.

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

**Precise field access types.** The same kind-system extension that introduces `Kind::Operator` also introduces `Kind::Label` — type-level string identifiers for record field names. This makes `get` and `get-in` label-polymorphic: `[get "host" config]` returns `String` (not `Unknown`) when `config`'s type is known; `[get "port" (A | B)]` returns `A.port | B.port` via BAS union distribution; and functions that take a field name as a parameter gain precise polymorphic types via the `HasField` qualified-type constraint.

## Syntax Design

### Kind Annotations

Tinct avoids infix operators. The kind `* → *` (type constructor) is written as the reserved kind-level name `Operator`. Kind annotations follow the existing `@Type` annotation syntax:

| Annotation | Meaning |
|------------|---------|
| `f@Operator` | `f` is an unconstrained type constructor (kind `* → *`) |
| `m@Monad` | `m` is a type constructor with a `Monad` instance |
| `f@Functor` | `f` is a type constructor with a `Functor` instance |
| `f@Mappable` | `f` is a type constructor with a `Mappable` instance |
| `key@Label` | `key` is bound to an anonymous label TypeVar (system-generated name); the type checker generates a `HasField` constraint automatically |
| `key@[label: l]` | `key` is bound to a named label TypeVar `l`; use when the same label must appear elsewhere in the type signature |

A constrained annotation like `m@Monad` implies `Operator` — no separate kind annotation is needed when the constraint provides the kind. 

**Label annotations** create Label-kinded TypeVars:
- `key@Label` (anonymous) — the type checker generates a fresh label TypeVar internally (system-generated name like `_label_0`), registers it in `kind_env` with `Kind::Label`, and generates a `HasField` constraint automatically. Use when the label is not referenced elsewhere in the type.
- `key@[label: l]` (named) — creates a label TypeVar named `l`, binds `key` to `TypeVar(l_fresh)`, and registers `kind_env[l_fresh] = Kind::Label`. Use when the same label must appear in multiple type positions (e.g., two parameters that must access the same field).

**HasField constraints are never user-written** — they are generated by the type checker from label annotations.

### Type Constructor Application in Annotation Positions

In annotation positions, `[f a]` (square brackets without colons) denotes type constructor application. This form is valid for Operator-kinded type variables and user-defined parameterized type aliases. All parameterized types — including builtins like `Seq` and `Map` — use the same bracket application syntax:

| Syntax | When valid | Meaning |
|--------|-----------|---------|
| `@[m a]` | `m` is an Operator type variable | Apply type constructor `m` to type `a` |
| `@[m [Seq a]]` | `m` is an Operator type variable | `m` applied to `[Seq a]` (nested) |
| `@[MyAlias Int]` | `MyAlias` is a user parameterized type alias | Alias instantiation |
| `@[Seq Int]` | always | Sequence of `Int` |
| `@[Map [String: Int]]` | always | Map from String to Int |
| `@[key: T]` | always | Record type (has colon — not application) |

When an Operator-kinded variable is resolved to a builtin (e.g., `m` resolves to `Seq`), the resulting type is normalized to the builtin form (`Seq(T)`, not `App(Seq, T)`) during instance resolution. This preserves the existing `Type::Seq` variant and avoids introducing a duplicate representation alongside it.

The disambiguation rule: square brackets with at least one colon form a record type. Square brackets without colons are type application.

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
  # Primitive fold-based implementation — NOT via generic sequence/traverse,
  # which would be circular (sequence calls traverse calls t.traverse calls sequence).
  [traverse: [fn [f xs]
    [reduce
      [fn [acc x]
        [f.lift2 [fn [as a] [concat as [a]]] acc [f x]]]
      [f.pure []]
      xs]]]]]

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
# sequence: collect a Traversable of monadic values into a single monad
# Generic over any Traversable container (Seq, Result, Maybe, ...)
sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
  [traverse f [fn [x] x] xs]]

# traverse: map a monadic function over any Traversable and collect results
traverse: [fn@[f [t b]] [f@Monad  t@Traversable  fn@[f b] [a]  xs@[t a]]
  [t.traverse f xs]]

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

**Implementation:** `[do]` is currently a stub in `stdlib/macros.llt`. The explicit form desugars `[x: expr] rest` to `[monad.bind expr [fn [x] [do monad rest]]]` and non-binding steps to `[monad.bind expr [fn [_] [do monad rest]]]`. The inferred form emits a typeclass constraint that the type checker resolves to a concrete monad dict, substituting the resolved dict name in place of the `%do-infer` sentinel before handing the desugared AST back to the evaluator — the runtime always sees `[monad.bind ...]` with a concrete dict.

**Superclass dictionary extraction.** When a `Monad f` dict is passed to a function expecting `Applicative f`, the instance resolver extracts the `Applicative` superclass by looking up the superclass chain registered in `ClassEnv`: `Monad → Applicative → Functor`. The resolved `MonadResult` instance dict carries the `Applicative` methods (`pure`, `lift2`) directly (they are required by `ApplicativeResult`). No separate dictionary coercion is needed at the call site — the instance dict is a tinct dict value with all inherited method fields present.

## Formal Type Rules

### Kind System

The kind grammar extends with two new productions:

```
Kind ::= *              -- concrete types (kind of Int, Str, Bool, Record, ...)
       | Row            -- record field sets (kind of row variables)
       | Operator       -- type constructors (kind * → *, written as `Operator`)
       | Label          -- type-level string labels (kind of StringLiteral when used as a field selector)
```

`Operator` is notation for `* → *`. The parser treats it as a reserved kind-level name, not a type. `Label` classifies type-level string identifiers used as record field names — they are never widened to `Str` during constraint resolution.

Types have kinds; the kinding judgment `Γ ⊢ T : κ` applies to type expressions. `App(f, a)` is a type expression of kind `*`, derivable by `KIND-OPERATOR` (see §Kind Checking). Kinds themselves are not type expressions — `Kind` is a separate syntactic category.

```rust
pub enum Kind { Star, Row, Operator, Label }
```

#### `Kind::Label` — Label Types

`Kind::Label` classifies type-level string labels: the string literals that name dict fields at the term level, treated as type-level identifiers in the `HasField` constraint (see §Field Access Typing below).

`HasField(l : Label, d : Record, a : *)` is a structural constraint asserting that record type `d` has a field at label `l` with type `a`. Its implementation machinery (`Kind::Label`, `Label` ADT, `kind_env`, promotion suppression) lands in `hkt-foundation`; the full field-access typing design is in §Field Access Typing under §Formal Type Rules.

A concrete usage example:

```tinct
# get-field: a function polymorphic over record field names
# key@Label declares an anonymous label TypeVar — kind_env[_label_0] = Kind::Label
[get-field: [fn@a [rec@d  key@Label]
  [get key rec]]]

# At call site: the anonymous label unifies with StringLiteral("host"), field type is inferred
[get-field config "host"]    # → String (if config has host: String)
```

**Label TypeVars** are regular TypeVars registered in `kind_env` with `Kind::Label`. The annotation `key@Label` (anonymous form) causes the resolver to create a fresh TypeVar with a system-generated name and insert `kind_env[fresh] = Kind::Label`. For named label TypeVars (when the same label must appear in multiple positions), use `key@[label: l]`. This requires no change to the `TypeVar(String, u32)` Rust representation.

**`TypeScheme` carries label vars.** Generalized label TypeVars are tracked in `TypeScheme.label_vars: Vec<String>` (parallel to `type_vars`). `instantiate_scheme` registers freshly-instantiated label vars in `state.kind_env` with `Kind::Label` so that promotion suppression applies at every call site, not only at the definition site.

**Promotion suppression.** `promote_literal_for_constrained_var` in `src/type_unify.rs` widens `StringLiteral(s) → Str` for any constrained TypeVar — a necessary rule for numeric/equality constraints (`[+ 1 2]` requires widening `IntLiteral(1)` to `Int` so both arguments unify). For label TypeVars this promotion is wrong. The fix: before promoting, check `state.kind_env.get(var_name)`; if `Kind::Label`, return `ty` unchanged.

**Structural enforcement via the `Label` ADT.** The `HasField` constraint uses a dedicated Rust ADT for the label position — providing compile-time enforcement that the label is always a string or label variable reference, never an arbitrary `Type`:

```rust
pub enum Label {
    Concrete(String),  // a known label: "host"
    Var(String),       // a label TypeVar name referencing kind_env
}
```

Label TypeVars remain in `Substitution.type_map` (bound to `StringLiteral` values after unification); no separate `label_map` is needed. `HasField { label: Label::Var(name), ... }` resolves by looking up `name` in `subst.type_map`.

**Kind rules for label vars:**

```
KIND-LABEL (kinding judgment):
  kind_env(l) = Label
  ──────────────────────────────────────────
  kind_env ⊢ TypeVar(l) : Label

  (Implementation corollary: promote_literal_for_constrained_var skips promotion
   when kind_env(var) = Label, preserving StringLiteral(s) in type_map.)

KIND-LABEL-ERROR:
  kind_env(l) = Label    κ ≠ Label
  ─────────────────────────────────────────────────────────────────────
  Γ ⊢ C[TypeVar(l)] : κ  ⊢ kind error "label variable l cannot appear as kind κ"
```

Concretely: `Seq(TypeVar(l))` where `kind_env(l) = Label` is a kind error — `Seq` expects a `*`-kinded argument.

### Type Constructor Application

Two new `Type` variants in `src/types.rs`:

```rust
pub enum Type {
    // ... existing variants ...
    App(Box<Type>, Box<Type>),  // type constructor application: f applied to a
    Operator(String),           // type constructor variable: f, m, t
}
```

Unification extends with two new cases:

```
UNIFY-OPERATOR:
  m ∉ ftv(T)    kind_env ⊢ T : *     -- T must be a proper type, not Operator or Label
  ──────────────────────────────────
  unify(Operator(m), T) = [m ↦ T]

  unify(T, Operator(m)) = [m ↦ T]    (symmetric)

UNIFY-APP:
  unify(f₁, f₂) = θ₁    unify(θ₁(a₁), θ₁(a₂)) = θ₂
  ─────────────────────────────────────────────────────
         unify(App(f₁, a₁), App(f₂, a₂)) = θ₂ ∘ θ₁
```

UNIFY-OPERATOR binds a type constructor variable `m` to a concrete type constructor `T`. The occurs check `m ∉ ftv(T)` prevents infinite kinds. The `kind_env ⊢ T : *` premise prevents binding an Operator or Label var in a type position.

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

The rank-1 restriction: `App(Operator("f"), Operator("g"))` — applying one Operator variable *to* another Operator variable — is excluded. This prevents rank-2 kind polymorphism (`(* → *) → *`) while still allowing multiple flat Operator quantifiers in a single method type. `traverse`, for example, has two Operator variables simultaneously: `f@Applicative` (the effectful container) and `t@Traversable` (the traversable structure) — these are flat prenex quantifiers, not nested applications, and are correctly rank-1 (Jones 1993). This restriction keeps kind inference decidable.

### Field Access Typing — `HasField` and `get`/`get-in`

**Theoretical foundation.** This design adapts Gaster & Jones (1996) first-class
label polymorphism to BAS. The key divergence: tinct has no row variables — BAS
replaces open rows with width subtyping. The `Lacks r l` predicate is unnecessary
(tinct has no record extension operation). G-J's `{l :: a | r}` open-row type is
reformulated as a `HasField` qualified-type constraint checked against a closed
record via width subtyping. This is simultaneously more restricted (no row
variables, no extension) and more expressive (union distribution is a first-class
BAS rule).

**Width subtyping scope.** Width subtyping subsumes G-J's open rows for *field consumption* — a function typed against `{name: Str}` accepts any record with at least field `name: Str`, just as G-J's `{name :: a | r}` type does. It does *not* subsume open rows for field-preserving transforms: a G-J function `{l :: a | r} → {l :: b | r}` (same unknown tail through the function) cannot be expressed with closed records and width subtyping alone — the extra fields in `r` are accepted at the call site but cannot appear in the return type. Field-preserving record transforms remain out of scope; `HasField` covers field *reading* fully.

**Note on novelty.** No published work formally combines G-J first-class labels
with BAS. Castagna (2023) formally proves union distribution over field access in
semantic subtyping. Castagna & Peyrot (2025) combine row polymorphism with
set-theoretic types. The specific mechanism — Jones (1994) qualified-type
`HasField` constraint with BAS union/intersection distribution — appears to be
novel. Proof sketches are included; mechanization is deferred.

#### `HasField` Qualified-Type Constraint

`HasField l d a` is a three-argument qualified-type constraint: "dict type `d`
has label `l` with field type `a`." Functional dependency `(l, d) → a` — given
the label and dict type, the field type is uniquely determined (Jones 1994).

**`Constraint` enum.** `HasField` requires extending `Constraint` from its
current single-var struct:

```rust
pub enum Constraint {
    Class { class: String, var: String },           // existing
    HasField { label: Label, dict_var: String, field_var: String },  // new
}
```

`Label::Concrete` constraints resolve immediately; `Label::Var` constraints are
schematic (carried in `TypeScheme.constraints` for label-polymorphic functions).

**Instance resolution rules:**

```
l ∈ dom(fields)    fields(l) = τ
─────────────────────────────────────────   [HAS-FIELD-REC]
HasField (Concrete l) Record(fields) τ


HasField l τ₁ a₁    HasField l τ₂ a₂
─────────────────────────────────────────   [HAS-FIELD-UNION]
HasField l (τ₁ | τ₂) (a₁ | a₂)


HasField l τ₁ a₁    HasField l τ₂ a₂
─────────────────────────────────────────   [HAS-FIELD-INTER]
HasField l (τ₁ & τ₂) (a₁ & a₂)


─────────────────────────────────────────   [HAS-FIELD-TOP]
HasField l ⊤ Unknown
    (for BAS-normalized unions with disjoint field names — S-RcdTop collapses
     {name: A} | {port: B} to ⊤ before [HAS-FIELD-UNION] can fire)


─────────────────────────────────────────   [HAS-FIELD-UNKNOWN]
HasField l Unknown Unknown


─────────────────────────────────────────   [HAS-FIELD-NEVER]
HasField l Never Never
    (vacuous — Never has no inhabitants)
```

No instance for `Negation` — `HasField l (¬τ)` is underdetermined; falls back
to `Unknown`.

**Gradual typing interaction.** `[HAS-FIELD-UNKNOWN]` gives `a = Unknown` regardless of downstream knowledge about the field. If a dict's type is widened to `Unknown` (e.g., by `from-json` which returns `Unknown`), `get "host" result` returns `Unknown` even if the caller expects `String`. Annotate the dict to recover precision: `[@[host: String port: Int] result]`.

**BAS normalization ordering.** `[HAS-FIELD-UNION]` fires on the union before
BAS normalization collapses it. If `{name: A} | {port: B}` (disjoint field names)
is normalized to `⊤` via `[S-RcdTop]` first, `[HAS-FIELD-TOP]` applies instead.
The implementation must resolve `HasField` constraints eagerly when the dict type
is a union, before BAS RDNF normalization runs.

**`[HAS-FIELD-INTER]` and conflicting field types.** If `x : {name: Int} & {name: Str}`,
then `x.name : Int & Str`. Under BAS, `Int & Str` stays as `Intersection([Int, Str])`
rather than reducing to `Never` (S-ClsBot only reduces records with *different*
field names, not primitive intersections). The result is vacuously sound — the
intersection type `{name: Int} & {name: Str}` is itself uninhabitable — but
implementations should warn: "field access on an uninhabitable intersection type."

#### Type Rules for `get` and `get-in`

**[GET]** — central inference rule:

```
Γ ⊢ key ⇒ StringLiteral(l)    Γ ⊢ dict ⇒ d
fresh β    C' = C ∪ {HasField l d β}
──────────────────────────────────────────────────   [GET]
Γ, C ⊢ [get key dict] ⇒ β, C'
```

Resolution of `HasField l d β` is eager when possible:

- **`d` is `Record(fields)`:** look up `l` in `fields`; if found, bind `β ↦ fields(l)` and drop the constraint; if not, type error — "record has no field `l`"
- **`d` is `τ₁ | τ₂`:** apply [HAS-FIELD-UNION]; bind `β ↦ β₁ | β₂`
- **`d` is `τ₁ & τ₂`:** apply [HAS-FIELD-INTER]; bind `β ↦ β₁ & β₂`
- **`d` is `⊤`:** apply [HAS-FIELD-TOP]; return `Unknown`
- **`d` is a TypeVar `α`:** defer — bind `α ↦ {l: β}` via unification (minimum record requirement); multiple deferred constraints on the same `α` accumulate: `HasField "age" α γ` merges into `α ↦ {name: β, age: γ}` (field-set union, not structural record unification — the latter would fail on different-field closed records)
- **`d` is `Unknown`:** return `Unknown`

**[GET-IN]** — chained literal-key access, expanded inline:

```
──────────────────────────────────────────────────   [GET-IN-NIL]
Γ, C ⊢ [get-in [] dict] ⇒ type(dict), C


Γ, C ⊢ [get h dict] ⇒ τ, C'
Γ, C' ⊢ [get-in path τ] ⇒ a, C''
──────────────────────────────────────────────────   [GET-IN-CONS]
Γ, C ⊢ [get-in (h :: path) dict] ⇒ a, C''
```

`check_get_in` requires the path to be a syntactic `Seq` literal whose elements
are all `StringLiteral` at the call site. Variable-length or non-literal paths
fall back to `Unknown`. `Seq` is homogeneous (`Seq(Box<Type>)`) — element-level
literal types are only preserved in syntactic position, not in the inferred type.

#### Label Polymorphism

Label TypeVars are generalized at the same binding boundaries as type TypeVars.
A function that receives a label as a parameter has a label-polymorphic scheme:

```tinct
# Inferred: ∀ (l : Label) d a. HasField l d a => StringLiteral(l) → d → a
[fn [key@[label: l]  dict] [get key dict]]
```

The `get` builtin's full scheme:
```
get : ∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a
```

Call-site resolution of `[get "name" user]`:
1. Instantiate scheme: fresh label var `l'`, type vars `d'`, `a'`, constraint `HasField l' d' a'`
2. `unify(StringLiteral(l'), StringLiteral("name"))` → `l' ↦ StringLiteral("name")`
3. `unify(d', type(user))` → `d' ↦ {name: String, age: Int}`
4. Resolve `HasField Concrete("name") {name: String, age: Int} a'` via [HAS-FIELD-REC] → `a' ↦ String`
5. Return type: `String` ✓

#### Proof Obligations

**P1 — Soundness of [HAS-FIELD-UNION].** If `x : τ₁ | τ₂` and `HasField l τ₁ a₁`
and `HasField l τ₂ a₂`, then `x.l : a₁ | a₂`. *Proof sketch:* by BAS
[UNION-ELIM], `x` is either a `τ₁`-value (then `x.l : a₁ <: a₁ | a₂`) or a
`τ₂`-value (then `x.l : a₂ <: a₁ | a₂`). □ This corresponds to the field-selection
rule in Castagna (2023) proven for semantic subtyping.

**P2 — Functional dependency uniqueness under union.** `HasField l (τ₁ | τ₂) a`
is unique: `a = a₁ | a₂`, determined by [HAS-FIELD-REC] on each member. BAS
normalization keeps `a₁ | a₂` canonical (e.g., `Int | Int = Int`,
`Int | Number = Number`). □

**P3 — Principal types.** Jones (1994) proves principal types for qualified-type
systems with confluent functional dependencies. [HAS-FIELD-REC] is deterministic
(HashMap lookup); [HAS-FIELD-UNION] and [HAS-FIELD-INTER] are structurally
recursive. The ambiguity risk: `HasField l d a` with both `l` and `d` unbound
cannot be resolved — emit "ambiguous field access: annotate the key or dict type."
In practice, `l` is always bound at the call site by the key argument.

**P4 — Constraint merge soundness.** Accumulating `HasField "name" α β` and
`HasField "age" α γ` merges to `α ↦ {name: β, age: γ}`. Soundness: any concrete
dict satisfying both must have both fields (by [HAS-FIELD-REC]). Width subtyping
allows extra fields. The merge is commutative (field-set union). □

**P5 — `[HAS-FIELD-INTER]` soundness and uninhabitable-intersection warning.**
`[HAS-FIELD-INTER]` yields `a₁ & a₂` for `HasField l (τ₁ & τ₂)`. The rule
is sound for inhabited intersections: if `x : τ₁ & τ₂`, then `x.l` must
satisfy both `a₁` and `a₂`, giving type `a₁ & a₂`. The uninhabitable-intersection
warning fires when `a₁ & a₂ ≤ Never` in BAS. *Caveat:* the current BAS
implementation's `S-ClsBot` applies to closed single-field records with different
field names, not to primitive type pairs like `Int & Str`. Until `S-ClsBot`
covers primitive disjointness, the warning must use a direct type-disjointness
check in `resolve_has_field` (e.g., `is_subtype(normalize_intersection([a₁, a₂]), Never)`).
The warning is always a sound over-approximation — flagging a possibly-uninhabitable
intersection is conservative, not incorrect.

## Interaction with Row Polymorphism

Tinct already has `Handle@[...]` — tinct's streaming session type where the parameter is a row of protocol operations (see `doc/09-documents.md`) — this is `Handle : Row → *`, a type constructor taking a row argument. The new `Operator` kind is the value-type-parameter analog. All three new kinds are mutually orthogonal:

- `Handle@[Tls Stream]` uses `Row` kind
- `@[Result T]` uses `Operator` kind
- `@Map@[K: V]` uses `Operator → Operator → *` (rank-2, supported for concrete applications only)
- `key@Label` or `key@[label: l]` uses `Label` kind — a label TypeVar names a specific field; it does not participate in row construction or type constructor application. `Seq(TypeVar(l, Label))` is a `KIND-LABEL-ERROR` — `Seq` expects a `*`-kinded argument.

Row variables remain unchanged. `Operator` and `Label` variables are new and separate from each other and from row variables. The existing `Row`-kinded `Handle` is not affected.

## Interaction with BAS

BAS operates on types of kind `*`. With rank-1 HKT, the BAS constraint solver extends to handle type constructor applications:

- **Application atoms:** `App(f, a)` is an atom in the BAS lattice for each concrete `(f, a)` pair. `App(Result, Int)` and `App(Result, Str)` are distinct atoms.
- **Covariant functorial subtyping:** For covariant type constructors (all `Functor` instances are covariant in their argument), `a <: b` implies `App(f, a) <: App(f, b)`. This is the functorial subtyping rule.
- **Join of applications (one-directional):** `App(m, a) | App(m, b) <: App(m, a | b)` for covariant `m`. This follows directly from covariance: since `a <: a | b` and `b <: a | b`, functorial subtyping gives `App(m, a) <: App(m, a|b)` and `App(m, b) <: App(m, a|b)`, so their join is also a subtype. The reverse direction — `App(m, a|b) <: App(m, a) | App(m, b)` — is not asserted and is unsound for diagonal functors (those that use their type parameter in more than one structural position).

The BAS lattice remains boolean. The constraint solver handles `App` by treating concrete applications as atoms during constraint generation and applying functorial subtyping during the subtype check.

**Covariance assumption.** The functorial subtyping rule `a <: b ⟹ App(f, a) <: App(f, b)` assumes `f` is covariant in its argument. For tinct's stdlib instances (`Maybe`, `Result` ok-field, `Seq`), covariance holds by structural inspection. For user-defined instances, no syntactic positivity check is enforced. The rule is restricted to instances declared in `stdlib/prelude.llt`; user-defined `Functor` instances do not automatically receive the BAS lift unless a positivity check is added in a future sprint.

**Instance resolution scope.** Instance dicts are named bindings (`[MonadResult: [instance [Monad Result] ...]]`). They are resolved from `InferState.class_env` which is populated globally at startup. For files composed via `$include`, all instances are visible — there is no local override mechanism. This matches Haskell's global coherence model. A per-file instance scope (Scala 3 `given` model) is not implemented.

`Kind::Label` variables do not introduce new BAS atoms — label TypeVars are phantom indices used in `HasField` constraints to select record fields. They do not inhabit the BAS lattice. `HasField` constraints on union dict types are resolved eagerly within `check_get` before the union is passed to `simplify_type` — this ensures `[HAS-FIELD-UNION]` fires before `S-RcdTop` can collapse a disjoint-field union to `⊤` (see §Field Access Typing — `HasField` and `get`/`get-in` §BAS Normalization Ordering for the complete rule and the `[HAS-FIELD-TOP]` fallback). `HasField` constraints on TypeVar dict types are deferred until the TypeVar resolves; they do not participate in BAS RDNF normalization.

Contravariant positions (e.g., the input type of a function inside a container) require explicit annotation or remain `Unknown`. This is acceptable at rank-1.

## Why Not Effect Systems

Algebraic effects (Koka, Frank, Unison) are the major alternative to monads for structuring side effects. They are not appropriate for tinct for one reason: **lazy evaluation and algebraic effects do not compose cleanly**.

In a strict language, an effectful expression executes when evaluated — effects are ordered by control flow. In a lazy language, a thunk is evaluated on demand, potentially reordering or deduplicating effects. Haskell's IO monad exists precisely to give effects a total order in an otherwise lazy language. Algebraic effects assume strict evaluation.

Tinct's lazy evaluation model makes the IO monad (and Result monad for failure) the right abstraction — they explicitly sequence effects through bind. Effect systems would require tinct to become strict or to add explicit thunk materialization in the effects semantics, neither of which is acceptable.

## Backward Note: Mappable Migration

`Mappable` is the concrete near-term motivation for the `hkt-mappable-appendable` sprint: the existing hardcoded fixed-instance set in `src/typecheck.rs` is replaced by the `Mappable` class declaration (see §The Typeclass Hierarchy §Mappable) and normal instance resolution. The migration is covered in §What Would Change §Type Checker.

## Backward Compatibility

Every existing `[do monad ...]` call is valid in the new model:
- The explicit monad argument is still accepted and takes priority over inference.
- The bind-field dispatch (looking up `monad.bind`) still works for dicts without a registered `Monad` instance.
- User-defined monad dicts that predate the `Monad` typeclass are unaffected.

The upgrade path: existing code compiles unchanged. New code can drop the explicit monad argument when the return type is annotated. No migration is required.

## What Would Change

### Parser (`src/parser.rs`, `src/lexer.rs`) *(hkt-foundation)*

- Recognize `Operator` as a reserved kind-level name in annotation positions.
- In annotation positions, parse `[f a]` (no colons) as `Expr::TypeApp(f, a)` when `f` is an Operator-kinded type variable or a user-defined parameterized type alias. Built-in names (`Seq`, `Map`, etc.) continue to use the existing `@Seq@T` path and produce `Type::Seq(T)` directly — no `App(Seq, T)` variant for builtins. When an Operator variable is resolved to a builtin at instance resolution time, normalize to the builtin type form.
- Extend `class` declaration parsing to accept `extends [SuperClass param]` clause.

### Type System (`src/types.rs`, `src/type_unify.rs`) *(hkt-foundation)*

New `Kind` variants and `Type` variants:
```rust
pub enum Kind { Star, Row, Operator, Label }  // Label: kind of type-level string labels

pub enum Type {
    // ... existing variants ...
    App(Box<Type>, Box<Type>),  // type constructor application
    Operator(String),           // type constructor variable
}
```

The `Label` ADT (defined in §Kind System) lives in `src/types.rs` or `src/type_unify.rs`. No new definition here — see §Kind System for the Rust code.

New unification cases (`src/type_unify.rs`): `UNIFY-OPERATOR` and `UNIFY-APP` as above.

**`promote_literal_for_constrained_var` extension:** before widening `StringLiteral(s) → Str`, check `state.kind_env.get(var_name)`; if `Kind::Label`, return `ty` unchanged. This is the sole change that preserves label identity for `HasField` constraints.

### Type Checker (`src/typecheck.rs`)

- **Kind inference pass** *(hkt-foundation)*: Determine kinds of type expressions before HM inference. Check type constructor variables are used at kind `Operator` in class method signatures. Register label TypeVars with `Kind::Label` when encountered in `key@Label` or `key@[label: l]` annotation positions. Enforce `KIND-LABEL-ERROR`: reject any `Kind::Label` TypeVar in a position expecting `Kind::Type`.
- **Typeclass resolution for HKT** *(hkt-kind-inference)*: Extend `ClassEnv` lookup to handle `* → *` classes. When `[do]` is used without an explicit monad, consult the return type for `m` and resolve the `Monad m` instance.
- **`App` type inference** *(hkt-kind-inference)*: Unify `App(Operator("m"), a)` against concrete applications to resolve `m`.
- **`Mappable` + `Appendable` rewrite** *(hkt-mappable-appendable)*: Remove both hardcoded fixed-instance sets; replace with normal class + instance resolution.
- **`HasField` constraint resolution** *(hkt-mappable-appendable)*: When `Constraint::HasField { label, dict_var, field_var }` is encountered and `dict_var` is bound to a concrete `Record(fields)`:
  - If `label = Concrete("name")` and `"name" ∈ dom(fields)`: unify `field_var` with `fields["name"]` — constraint satisfied
  - If `label = Concrete("name")` and `"name" ∉ dom(fields)`: type error — "record type has no field 'name'"
  - If `label = Var(l)` and `l` is bound in `subst.type_map` to `StringLiteral("name")`: use concrete label `"name"` and proceed as above
  - If `dict_var` is an unbound TypeVar: defer — accumulate field constraint; when resolved, retry

### `src/type_unify.rs` — HasField resolution *(hkt-mappable-appendable)*

Add `resolve_has_field(label: &Label, dict_type: &Type, field_var: &str, state: &mut InferState) → Option<Type>` implementing [HAS-FIELD-REC], [HAS-FIELD-UNION], [HAS-FIELD-INTER], [HAS-FIELD-TOP], [HAS-FIELD-UNKNOWN], [HAS-FIELD-NEVER], deferred TypeVar constraint accumulation with field-set merge, and a warning for uninhabitable intersection field types.

**TypeVar accumulation mechanism.** When the dict type is a TypeVar `α`, `check_get` emits a `Constraint::HasField { label, dict_var: "α", field_var: "β" }` into `state.constraints`. A subsequent `check_get` call that also has `α` as the dict TypeVar scans `state.constraints` for existing `HasField { dict_var: "α", ... }` entries and merges the new field into the accumulated `Record` that `α` will be bound to — the merge accumulates `{name: β₁, age: β₂, ...}` as a growing set of field requirements. When `α` is later bound (during unification at the call site), all deferred `HasField` constraints on `α` are resolved via [HAS-FIELD-REC] against the concrete record. The merge uses field-set union (not structural record unification, which would fail on different-key closed records).

### `src/typecheck.rs` — `check_get`, new `check_get_in` *(hkt-mappable-appendable)*

Extend `check_get` with: TypeVar arm (defer `HasField`, accumulate into growing record constraint), Union arm ([HAS-FIELD-UNION] distribution), Top/Unknown arms. Add `check_get_in` implementing [GET-IN-NIL]/[GET-IN-CONS] inline unfolding for syntactic literal-path sequences, falling back to `Unknown` for variable-length or non-literal paths.

### `src/type_env.rs` — `get` and `get-in` signatures *(hkt-mappable-appendable)*

Register `get`'s label-polymorphic scheme: `∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a`. Register `get-in` as a special form dispatched to `check_get_in`.

### `src/typecheck_annot.rs` — label TypeVar annotations *(label-annotation-syntax)*

**Two annotation forms for Label-kinded TypeVars:**

1. `key@Label` (anonymous) — In `resolve_type_name`, when annotation is `Simple("Label")`, create a fresh TypeVar with system-generated name (e.g., `_label_0`), register `kind_env[fresh] = Kind::Label`. The HasField constraint is generated automatically by the type checker.

2. `key@[label: l]` (named) — In `resolve_annotation`, when `PropertyDict` has exactly one entry with key `"label"` and a bare-name value, create a named Label-kinded TypeVar, register it in `kind_env` and `ann_mapping`. Use when the same label must appear in multiple type positions.

### `stdlib/prelude.llt` — `get`/`get-or`/`get-in` annotations *(label-annotation-syntax)*

```tinct
get:    [fn@a [key@Label  dict@d] ...]
get-or: [fn@a [key@Label  dict@d  default@a] ...]
get-in: [fn@[doc: "Chained field access — return type inferred from literal path"] [path  dict] ...]
```

HasField constraints are **never user-written** — they are generated by the type checker from Label annotations.

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
- Castagna, G. (2023). "Typing Records, Maps, and Structs." *Proc. ACM Program. Lang. 7*(ICFP), pp. 215–258. doi:10.1145/3607838. — Formally proves union distribution over field access (`[HAS-FIELD-UNION]`) under semantic/algebraic subtyping; the closest published prior art to this proposal's BAS treatment.
- Castagna, G. & Peyrot, L. (2025). "Polymorphic Records for Dynamic Languages." *Proc. ACM Program. Lang. 9*(OOPSLA1), Article 132, pp. 1464–1491. doi:10.1145/3720497. — Combines row polymorphism with set-theoretic (BAS-like) type algebra; the direct formal combination of row variables and algebraic subtyping.
- Dolan, S. (2017). "Algebraic Subtyping." PhD thesis, University of Cambridge. — BAS foundation; closed records with width subtyping; union and intersection type algebra that enables `[HAS-FIELD-UNION]` and `[HAS-FIELD-INTER]`.
- Gaster, B.R. & Jones, M.P. (1996). "A Polymorphic Type System for Extensible Records and Variants." *YALEU/DCS/RR-1104*. — Introduces first-class label polymorphism with `HasField`-style constraints as the basis for polymorphic field access; the theoretical foundation for `Kind::Label` and `Label::Var` in this proposal.
- Gundry, A. (2015). GHC Proposal: `HasField` type class (GHC 8.2). — Production implementation of `HasField` with functional dependency in HM; confirms principal types under functional dependencies.
- Jones, M.P. (1993). "A System of Constructor Classes: Overloading and Implicit Higher-Order Polymorphism." *FPCA '93*. — The original constructor class paper introducing `Functor`, `Monad` as typeclasses in a kind-polymorphic system; the direct ancestor of Haskell's HKT.
- Jones, M.P. (1994). "A System of Constructor Classes: Overloading and Implicit Higher-Order Polymorphism." *Journal of Functional Programming 5*(1), 1–35. — Qualified types with functional dependencies; basis for `HasField` constraint and principal types argument.
- Jones, M.P. (1995). "Functional Programming with Overloading and Higher-Order Polymorphism." *Advanced Functional Programming*, Lecture Notes in Computer Science 925. — Rank-1 kind polymorphism as a tractable subset of full HKT; shows decidability and practical implementation; constraint simplification and entailment.
- Microsoft TypeScript Team. "Indexed Access Types." *TypeScript Handbook*. — Production evidence for union distribution `(A|B)["k"] = A["k"] | B["k"]`; no formal proof but extensive empirical validation.
- Kiselyov, O. (2012). "Typed Tagless Final Interpreters." *Generic and Indexed Programming*, Lecture Notes in Computer Science 7470. — Defunctionalization as the HKT-free alternative; the theoretical basis for tinct's current `[do monad]` explicit-dispatch model.
- Leijen, D. (2014). "Koka: Programming with Row Polymorphic Effect Types." *MSFP '14*. — Effect systems as an alternative to monads for structuring I/O; argued against for tinct because of interaction with lazy evaluation.
- Marlow, S. & Peyton Jones, S. (2010). "Haskell 2010 Language Report." Ch. 6 (Typeclasses). — The `Functor`/`Applicative`/`Monad` hierarchy as implemented; the reference for what rank-1 HKT achieves in practice.
- Syme, D., Granicz, A. & Cisternino, A. (2010). *Expert F#*. Ch. 12 (Computation Expressions). — F# computation expressions as explicit builder-dict approach; validates that explicit monad dicts (`[do monad]`) are sufficient for production use without HKT.
- Wadler, P. (1992). "The Essence of Functional Programming." *POPL '92*. — Monads as a unified model for IO, state, exceptions; the theoretical foundation for `Monad m` as a typeclass.
