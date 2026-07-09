# What If: Record/Map Split and Parameterized Maps for tinct

**State:** Accepted — 2026-05-09

What would it take to give tinct a principled distinction between structural records and homogeneous maps, with a typed `Map@[K: V]` constructor and a `Dict` type that is their well-formed BAS union?

## Current State

Tinct represents all dicts as a single runtime type (`Value::Dict(IndexMap<Key, Thunk>)`) and at the type level as `Record(Row)` — Rémy-style row polymorphism over string-keyed fields. This works well for structural records with known fields:

```tinct
# Structural record — type checker tracks "host" and "port"
server: [host: "localhost"  port: 8080]
[@[host: String  port: Int] server]  # ✓
```

But tinct conflates two fundamentally different dict shapes under a single `@Dict` annotation that resolves to an open record with a fresh row variable — a type that says nothing useful about value uniformity:

| Shape | Example | Currently typed as |
|-------|---------|-------------------|
| Structural record | `[host: "localhost"  port: 8080]` | `Record([host: Str  port: Int], ρ)` |
| Homogeneous map | `[0: [1 2]  1: [3]  2: [4 5]]` | `Dict` = open record with fresh ρ — useless |

### What's Missing

1. **Typed homogeneous maps.** NFA `transitions: Map@[Int: Seq@Integer]` (char code → successor state IDs) and regex `groups: Map@[Int: String]` (capture group → matched text) are integer-keyed uniform-value dicts. The type checker cannot reason about them; computation over them produces `Any`.

2. **Formal semantics for `@Dict`.** `@Dict` today means "some dict" with no information. It should denote the union of all structural records and all homogeneous maps.

3. **Sound union elimination.** Pattern matching over `Dict` values cannot narrow the type — the type checker has no way to distinguish the two dict shapes.

4. **Typed key-safe access.** `[get k map]` on a typed `Map@[Int: String]` should return `String | Null` — the type checker knows the value type without knowing which key is present.

## Why Record/Map Split Matters for tinct

**Self-documenting stdlib signatures.** NFA and regex accumulator patterns use homogeneous maps. `stat`, `tls-peer-cert`, and `list-dir` return structural records. Precise types make signatures machine-checkable.

**Formal `Dict`.** `Dict: [type [Record Map]]` — a first-class BAS union — gives `@Dict` formal meaning and enables sound union elimination in pattern matching.

**Typed reduce pipelines.** `builtin-reduce` accumulating a dict with uniform-value inserts infers `Map@[K: V]` rather than `Any`.

**Key-safe access.** `[get k map]` on `Map@[K: V]` returns `V | Null`; `get-or` eliminates the nullable branch.

## Design

### The Type Hierarchy

```text
          Dict                    ← BAS union: Record ∨ Map
         /    \
    Record    Map@[K: V]          ← structural forms
      |            |
@[x:T y:U]   @[Map [Int: [Seq Int]]]  ← concrete annotations
```

**Runtime:** Both `Record` and `Map@[K: V]` are `Value::Dict(IndexMap<Key, Thunk>)` — identical representation. The split is purely at the type level, fully erased before evaluation.

### `Map@[K: V]` — Parameterized Homogeneous Map

`Map@[K: V]` is a new parameterized type constructor, parallel to the existing `Seq@T`:

```rust
// src/types.rs — parallel to Type::Seq(Box<Type>)
Map(Box<Type>, Box<Type>),   // Map(key_type, value_type)
```

**Key type K** is **invariant**. K is constrained to `Int`, `Str`, or `Int | Str` at annotation-resolution time — a kind check enforced during type alias expansion. `Map[Bool String]` is a kind error. K appears in both covariant (key returned by `keys`) and contravariant (`get` argument) positions, making invariance the sound choice for a general rule. However, K is covariant for immutable read-only access specifically, because access returns `V | Null` — key-type widening can only increase `Null` frequency, never cause a value-type mismatch. The proposal uses K-invariant as the subtyping rule and notes covariant K as a potential relaxation if needed.

**Value type V** is **covariant** — `Map@[Int: String] <: Map@[Int: Any]`.

**Annotation syntax:** `@[Map [K: V]]` bracket application form — parameterized map type. Bare `@Map` means `Map[Any: Any]`. The explicit form `@[Map [key: K  value: V]]` is also accepted.

```tinct
transitions: @[Map [Int: [Seq Int]]]  # NFA: char code → successor state IDs
groups:      @[Map [Int: String]]     # regex: capture group → matched text
index:       @[Map [String: Any]]     # string-keyed, untyped values
cache:       @Map                     # bare: Map[Any: Any]

# Explicit named form for complex types or clarity:
x@[Map [key: Int  value: [Ok String | Err String]]]
```

### `Record` — Bare Structural Record Type

`Record` without parameters is **not** a lattice top element. Each occurrence of `@Record` produces a fresh universally quantified open row variable — the same mechanism `@Dict` uses today:

```text
@Record  →  Record(Row { fields: {}, tail: RowVar(fresh_ρ, level) })
```

This means `@Record` is row-polymorphic: a function annotated `@Record` accepts any structural record and unifies the fresh row variable against the actual fields at the call site. Every use of `@Record` generates an independent fresh row variable; there is no sharing between call sites.

`@Record` is strictly more expressive than `@Dict` was: it preserves row-polymorphic behavior for structural records, whereas after the split `@Dict` loses this behavior (see below).

### `Dict` as a BAS Union

```tinct
Dict: [type [Record Map]]
```

Under BAS, this is a first-class Boolean-algebra union — `Record ∨ Map`. BAS's constraint solver handles union formation and elimination properly, without the row-variable sharing problems that would arise in a naïve HM encoding. Because BAS replaces Rémy row variables with a Boolean algebra over field-label atoms, the alias-body-freshening issue (shared ρ across uses of a union alias body) does not arise.

**Migration for existing `@Dict` code:** Functions currently annotated `@Dict` that rely on row-polymorphic unification must migrate to `@Record`. `@Dict` after the split denotes `Record ∨ Map` and does not drive unification of row variables — it is an umbrella type for "either form." The migration is:

| Current | Migrated | Notes |
|---------|----------|-------|
| `@Dict` (row-polymorphic) | `@Record` | Preserves row unification |
| `@Dict` (any dict) | `@Dict` | Same meaning, now formal |
| `@Dict` (homogeneous) | `@[Map [K: V]]` | More precise |

### Access Semantics and `get` Behavior

**On structural records:** Field access is guaranteed total — the type checker verifies the field exists in the record type. `[get "x" rec]` where `rec: @[x: String]` returns `String`.

**On `Map@[K: V]`:** The type checker knows the value type but not which keys are present. Access returns `V | Null`:

```tinct
[get k map]          # Map with keys/values [Int: String] → String | Null
[has? map k]         # Map with keys/values [Int: String] → Bool
[get-or map k "—"]   # Map with keys/values [Int: String] → String   (null eliminator)
```

`Null` in tinct is the empty closed record `[]` — `Type::Record(Row{fields:{}, tail:Empty})`. `V | Null` is a BAS union expressible in the existing type system.

**Runtime behavior change for `get` on typed maps:** Currently `builtin-get` raises `KeyNotFound` on a missing key. For the `V | Null` return type to be honest, `get` on a value typed `Map@[K: V]` must return `[]` rather than error on miss. This requires either:

- A new `get?` builtin (safe get, returns `V | Null`) alongside the existing `get` (which errors on miss and is appropriate for records where the field is guaranteed to exist), or
- The TypeAssert elaboration for `@[Map [key: K  value: V]]` wraps subsequent `get` calls in a null-returning variant

The preferred design is **`get?`** — a new safe-get builtin that returns `V | Null`. This keeps `get` strict (errors on miss, appropriate for records) and adds `get?` for dynamic map access. `get-or` is built on `get?`:

```tinct
get-or: [fn@V [map@[Map [K: V]]  k@K  default@V]
  [x: [get? map k]]
  [if [= x []] default x]]
```

**TypeAssert runtime cost:** `[@[Map [K: V]] expr]` requires at runtime that all keys are of type K and all values are of type V — an O(n) traversal. This is handled via tinct's proxy contract mechanism (Findler & Felleisen 2002): keys are checked eagerly on TypeAssert; value types are checked lazily on access (wrapped in a guard thunk). This is consistent with how `@[name: String]` record assertions work.

### Cross-Form Subtyping: Record → Map

A structural record can satisfy a `Map@[K: V]` annotation if its keys are all of type K and its values are uniformly of type V. This is the Nickel rule `{f₁: T₁, ..., fₙ: Tₙ} <: {_: T}` when all Tᵢ <: T, adapted for tinct's two key types:

```text
∀i: key(eᵢ) : K  ∧  type(eᵢ) <: V
────────────────────────────────────────────  [RECORD→MAP]
Record(entries)  <:  Map[K V]
```

This rule makes a `Map@[Int: String]` annotation checkable against a dict literal `[0: "a" 1: "b"]` — the literal infers a `Record` (see §Inference), and `is_subtype` uses [RECORD→MAP] to verify the annotation.

The converse does **not** hold: `Map@[K: V] <: Record(row)` is false — a map does not guarantee the presence of any specific field.

### Inference: Record Takes Priority

Inference **always produces `Record` types** for dict literals. This is required by the principal type property (Damas & Milner 1982): `Record([x: IntLiteral(42), y: IntLiteral(99)], Empty)` is strictly more informative than `Map@[Str: Int]`, which would lose the field-name information. Inference must produce the most general *most specific* type.

`Map[K: V]` arises only from:

- Explicit `@[Map [K: V]]` annotations
- Builtins whose return type is declared `Map[K: V]` (e.g., future regex group-capture returns)
- Inference from `builtin-reduce` accumulating uniform-value `set` operations (Phase 2 refinement, not Phase 1)

### Unification Rules

`unify(Map[K₁ V₁], Map[K₂ V₂])` proceeds element-wise:

- Unify K₁ with K₂ (K is invariant — unification produces a common binding, not a subtype)
- Unify V₁ with V₂

`unify(Record(row), Map[K V])` is a type error — different type constructors. Cross-form compatibility is handled by `is_subtype` ([RECORD→MAP]) not unification.

`unify(Dict, Map[K V])` and `unify(Dict, Record(row))` proceed via BAS constraint solving — the union type constrains the other member.

### `$merge` with Mixed Operands

`merge(Record([x: Int]), Map[Str String])` produces a value that is neither pure Record (the Map fields are untracked) nor pure Map (the Record field `x` has a known type). The result type is `Dict` — the umbrella union. This matches the existing `$merge` semantics (right-biased, no structural guarantee about the result shape).

If both operands are `Record`, the result is `Record` (row concatenation). If both are `Map@[K: V]` with matching V, the result is `Map@[K: V]`. Mixed operands produce `Dict`.

### Narrowing and `dict?` / `record?` / `map?`

After the split, `dict?` narrows to `Dict` (the union type). Two new predicates are added:

- `record?` — narrows to `Record` (any structural record with row-polymorphic type)
- `map?` — narrows to `Map` (any homogeneous map, key/value types unknown without annotation)

BAS refinement in `match` arms narrows from annotated types:

```tinct
[match val
  [@[x: Int  y: Str]  ...]    # Record branch — x and y proved present
  [@[Map [Str: Int]]  ...]  # Map branch — get returns Int|Null
  [@Dict              ...]    # catch-all for untyped dicts
  [_ ...]]
```

### The Empty Dict and `Map@[K: V]`

The empty dict `[]` has type `Null = Record(Row{fields:{}, tail:Empty})` — the closed empty record. In set-theoretic terms, the empty map is a valid map of any key/value type. The proposal adopts: **`Null <: Map@[K: V]` for all K, V** — the empty dict is a valid homogeneous map of any type. This is consistent with the rule that the empty collection is trivially homogeneous.

### Structural Equality

Dict equality (`=`) is **order-insensitive structural equality** for both `Record` and `Map@[K: V]`. The two forms derive this from independent first principles that happen to agree.

**Record equality** follows from the labeled-product definition (Pierce 2002, §11): two records are equal iff they have the same field names and equal values at each name. Field names are the identity of a record entry — insertion order is not part of the labeled-product semantics. `[= [a: 1 b: 2] [b: 2 a: 1]]` is `true` because the labeled product `{a → 1, b → 2}` is identical regardless of which field is written first. This is consistent with Rémy row types, where the row `{a: T, b: U | ρ}` is defined as a set of field bindings, not a sequence.

**Map equality** follows from the finite-function definition: `Map@[K: V]` is a partial function `K → V`, and two partial functions are equal iff they have the same domain and agree at every point (extensional equality). Insertion order is a representation detail of the underlying `IndexMap`, not part of the function's identity. `[= map1 map2]` checks `dom(map1) = dom(map2)` and `map1(k) = map2(k)` for all `k`.

Both forms reduce to the same algorithm at runtime: sort keys canonically, then compare key-by-key and value-by-value recursively.

**Implementation** (`src/builtins_math.rs`):

1. Extract and sort keys from both dicts (string keys lexicographically, integer keys numerically, string keys sort after integer keys)
2. If key sequences differ, return `false`
3. For each key pair, force both value thunks and recurse
4. Cycle detection via a visited set of `(ThunkId, ThunkId)` pairs — if the pair is already in the set, treat as equal (structural coinduction)

**Cross-form comparison** (`[= record map]` where one operand is typed `Record` and the other `Map@[K: V]`): a type error at the `=` call site post-split, because the type checker sees different constructors. At runtime, both are `Value::Dict` and the structural walk applies; the type system prevents this case from arising in well-typed code.

**Functions and builtins** are not equal to anything (no meaningful closure equality). `[= f g]` returns `false` for all function values.

## What Would Change

### Type System (`src/types.rs`)

**Current:** `Type::Record(Row)` is the only dict variant. `@Dict` expands to an open record with a fresh row variable.

**Proposed:**

- Add `Type::Map(Box<Type>, Box<Type>)`
- Register `Record`, `Map`, `Dict` in `TypeEnv::with_builtins()`; `Record` and `Map` produce fresh-variable open types at each use; `Dict` is a BAS union
- Add `is_subtype` rules: `Map(K, V₁) <: Map(K, V₂)` when `V₁ <: V₂` (K invariant, V covariant); `Record(ρ) <: Dict`; `Map(K,V) <: Dict`; `Null <: Map(K,V)`; [RECORD→MAP] cross-form rule
- Add `Map` handling to `value_matches_type` for TypeAssert runtime checking

**Impact:** Moderate.

### Type Checker (`src/typecheck.rs`)

**Current:** `@Dict` produces a fresh open record. `get` returns `Any` for dict access.

**Proposed:**

- `check_get`: target type `Map@[K: V]` → return `V | Null`; target `Record(ρ)` → return field type (total)
- `check_get?`: new builtin, returns `V | Null`
- `check_get_or`: target `Map@[K: V]` → return `V`
- `check_match`: BAS refinement for `Dict` union elimination
- No inference changes required for annotation checking; [RECORD→MAP] enables annotation-only typing

**Impact:** Moderate.

### Builtins (`src/builtins.rs`)

**Current:** No `get?` builtin. `get` errors on missing key. `=` on dicts silently returns `false`.

**Proposed:**

- Add `get?`: returns the value or `[]` (Null) on missing key — no error
- Add `record?`, `map?` type-narrowing predicates (returning Bool)
- Update `dict?` to narrow to `Dict` (union)
- Implement structural dict equality in `builtin_eq` (`src/builtins_math.rs`): sorted-key recursive walk, cycle detection via `(ThunkId, ThunkId)` visited set

**Impact:** Minor — new builtins; equality fix closes a silent correctness gap.

### Standard Library

**Current:** `get-or` type is `Dict → Key → Any → Any`.

**Proposed:**

- `get-or` implemented in terms of `get?`: returns the value or the default
- Type signature: `Map@[K: V] → K → V → V`
- `has?` type signature: `Map@[K: V] → K → Bool`

**Impact:** Minor — signature updates, implementation change for `get-or`.

### Runtime

No changes beyond the `get?` builtin and `record?`/`map?` predicates. Both `Record` and `Map[K: V]` are `Value::Dict(IndexMap<Key, Thunk>)` at runtime. TypeAssert for `@[Map [K: V]]` uses the proxy contract mechanism: eager key-type check on assertion, lazy value-type check on access.

**Impact:** Minor.

### Documentation

Update `doc/03-data-model.md` and `doc/05-type-annotations.md` with the `Record` / `Map@[K: V]` / `Dict` hierarchy.

**Impact:** Minor.

## Prerequisites

- **Boolean Algebraic Subtyping** (`doc/whatif/boolean-algebraic-subtyping.md`) — required for `Dict: [type [Record Map]]` as a sound BAS union; union elimination in pattern match arms; well-behaved `@Dict` without row-variable sharing issues.
- **Parameterized type aliases** — complete.
- **Union types** — complete.
- **Proxy contracts / TypeAssert structural validation** — required for O(n) TypeAssert on `@[Map [key: K  value: V]]` at runtime; see `doc/07-type-extensions.md` §TypeAssert Runtime Validation.

### Trigger

When BAS is adopted (`doc/whatif/boolean-algebraic-subtyping.md` accepted and implemented). Concretely: when `@Dict` on a function parameter first causes a type error that the `Record` / `Map@[K: V]` split would prevent.

## References

- Chau, C.Y. & Parreaux, L. (2026). "Boolean-Algebraic Subtyping: Intersections, Unions, Negations, and Principal Type Inference." *Proc. ACM Program. Lang.*, 10(POPL). — BAS provides the union type algebra that makes `Dict = Record ∨ Map` well-formed and union elimination in pattern matching sound; prerequisite for this design.
- Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." *POPL '82*, pp. 207–212. — Principal type property: inference must produce the most specific type; motivates why dict literals always infer `Record`, not `Map@[K: V]`.
- Findler, R.B. & Felleisen, M. (2002). "Contracts for higher-order functions." *ICFP '02*, pp. 48–59. — Proxy contract mechanism for lazy structural TypeAssert; used for O(n) `@[Map [K: V]]` runtime validation.
- Nickel language documentation (Tweag). nickel-lang.org. — Nickel's `{_: Type}` dictionary type as precedent for the Record/Map split; the cross-form subtyping rule `{f₁: T₁, ..., fₙ: Tₙ} <: {_: T}` when all Tᵢ <: T.
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. — §11 (Record types and subtyping); §23 (Universal polymorphism and System F); row-polymorphic records and parametric type constructors address orthogonal typing concerns.
- Rémy, D. (1994). "Type Inference for Records in a Natural Extension of ML." *Theoretical Aspects of Object-Oriented Programming* (Gunter & Mitchell, eds.), MIT Press. — Row polymorphism for structural records; basis for tinct's current `Record(Row)` type.
