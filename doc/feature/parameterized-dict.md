# Record/Map Split and Parameterized Maps

Implemented 2026-05-09 (`record-map-split` sprint). `Record` vs `Map@[K: V]` type split;
`Dict = Record ∨ Map` BAS union; `get?` for safe map access.

## Overview

Tinct distinguishes structural records from homogeneous maps with a typed `Map@[K: V]`
constructor and a `Dict` type that is their well-formed BAS union.

The type hierarchy:

```
          Dict                    ← BAS union: Record ∨ Map
         /    \
    Record    Map@[K: V]          ← structural forms
      |            |
@[x:T y:U]   @[Map [Int: [Seq Int]]]  ← concrete annotations
```

**Runtime:** Both `Record` and `Map@[K: V]` are `Value::Dict(IndexMap<Key, Thunk>)` —
identical representation. The split is purely at the type level, fully erased before
evaluation.

The split matters because:

- **Self-documenting stdlib signatures.** NFA and regex accumulator patterns use homogeneous maps. `stat`, `tls-peer-cert`, and `list-dir` return structural records. Precise types make signatures machine-checkable.
- **Formal `Dict`.** `Dict: [type [Record Map]]` — a first-class BAS union — gives `@Dict` formal meaning and enables sound union elimination in pattern matching.
- **Typed reduce pipelines.** `builtin-reduce` accumulating a dict with uniform-value inserts infers `Map@[K: V]` rather than `Any`.
- **Key-safe access.** `[get k map]` on `Map@[K: V]` returns `V | Null`; `get-or` eliminates the nullable branch.

## Design

### `Map@[K: V]` — Parameterized Homogeneous Map

`Map@[K: V]` is a new parameterized type constructor, parallel to the existing `Seq@T`:

```rust
// src/types.rs — parallel to Type::Seq(Box<Type>)
Map(Box<Type>, Box<Type>),   // Map(key_type, value_type)
```

**Key type K** is **invariant**. K is constrained to `Int`, `Str`, or `Int | Str` at
annotation-resolution time — a kind check enforced during type alias expansion.
`Map@[Bool: String]` is a kind error. K appears in both covariant (key returned by
`keys`) and contravariant (`get` argument) positions, making invariance the sound
choice for a general rule.

**Value type V** is **covariant** — `Map@[Int: String] <: Map@[Int: Any]`.

**Annotation syntax:**

```tinct
transitions: @[Map [Int: [Seq Int]]]   # NFA: char code → successor state IDs
groups:      @[Map [Int: String]]    # regex: capture group → matched text
index:       @[Map [Str: Any]]       # string-keyed, untyped values
cache:       @Map                    # bare: Map[Any: Any]
```

### `Record` — Bare Structural Record Type

`Record` without parameters is **not** a lattice top element. Each occurrence of
`@Record` produces a fresh universally quantified open row variable — the same
mechanism `@Dict` uses today:

```
@Record  →  Record(Row { fields: {}, tail: RowVar(fresh_ρ, level) })
```

`@Record` is row-polymorphic: a function annotated `@Record` accepts any structural
record and unifies the fresh row variable against the actual fields at the call site.
Every use of `@Record` generates an independent fresh row variable.

`@Record` is strictly more expressive than `@Dict` was: it preserves row-polymorphic
behavior for structural records, whereas `@Dict` after the split loses this behavior.

### `Dict` as a BAS Union

```tinct
Dict: [type [Record Map]]
```

Under BAS, this is a first-class Boolean-algebra union — `Record ∨ Map`. BAS's
constraint solver handles union formation and elimination properly, without the
row-variable sharing problems that arise in a naïve HM encoding.

**Migration for existing `@Dict` code:** Functions currently annotated `@Dict` that
rely on row-polymorphic unification migrate to `@Record`. `@Dict` after the split
denotes `Record ∨ Map` and does not drive unification of row variables.

| Current | Migrated | Notes |
|---------|----------|-------|
| `@Dict` (row-polymorphic) | `@Record` | Preserves row unification |
| `@Dict` (any dict) | `@Dict` | Same meaning, now formal |
| `@Dict` (homogeneous) | `@[Map [K: V]]` | More precise |

### Access Semantics and `get` Behavior

**On structural records:** Field access is guaranteed total — the type checker verifies
the field exists in the record type. `[get "x" rec]` where `rec: @[x: String]` returns
`String`.

**On `Map@[K: V]`:** The type checker knows the value type but not which keys are present.
Access returns `V | Null`:

```tinct
[get k map]          # Map@[Int: String] → String | Null
[has? map k]         # Map@[Int: String] → Bool
[get-or map k "—"]   # Map@[Int: String] → String   (null eliminator)
```

`Null` in tinct is the empty closed record `[]` — `Type::Record(Row{fields:{}, tail:Empty})`.
`V | Null` is a BAS union expressible in the existing type system.

**`get?`** is a new safe-get builtin that returns `V | Null`. This keeps `get` strict
(errors on miss, appropriate for records where the field is guaranteed to exist) and
adds `get?` for dynamic map access. `get-or` is built on `get?`:

```tinct
get-or: [fn@V [map@[Map [K: V]]  k@K  default@V]
  [x: [get? map k]]
  [if [= x []] default x]]
```

**TypeAssert runtime cost:** `[@[Map [K: V]] expr]` requires at runtime that all keys are
of type K and all values are of type V — an O(n) traversal. This is handled via tinct's
proxy contract mechanism: keys are checked eagerly on TypeAssert; value types are
checked lazily on access (wrapped in a guard thunk).

### Cross-Form Subtyping: Record → Map

A structural record can satisfy a `Map@[K: V]` annotation if its keys are all of type
K and its values are uniformly of type V:

```
∀i: key(eᵢ) : K  ∧  type(eᵢ) <: V
────────────────────────────────────────────  [RECORD→MAP]
Record(entries)  <:  Map[K V]
```

This rule makes `@[Map [Int: String]]` checkable against a dict literal `[0: "a" 1: "b"]` —
the literal infers a `Record`, and `is_subtype` uses [RECORD→MAP] to verify the annotation.

The converse does **not** hold: `Map@[K: V] <: Record(row)` is false — a map does not
guarantee the presence of any specific field.

### Inference: Record Takes Priority

Inference **always produces `Record` types** for dict literals. This is required by the
principal type property: `Record([x: IntLiteral(42), y: IntLiteral(99)], Empty)` is
strictly more informative than `Map@[Str: Int]`, which loses the field-name information.

`Map@[K: V]` arises only from:
- Explicit `@[Map [K: V]]` annotations
- Builtins whose return type is declared `Map@[K: V]`
- Inference from `builtin-reduce` accumulating uniform-value `set` operations (Phase 2 refinement)

### Unification Rules

`unify(Map[K₁ V₁], Map[K₂ V₂])` proceeds element-wise:
- Unify K₁ with K₂ (K is invariant — unification produces a common binding, not a subtype)
- Unify V₁ with V₂

`unify(Record(row), Map[K V])` is a type error — different type constructors. Cross-form
compatibility is handled by `is_subtype` ([RECORD→MAP]) not unification.

### `$merge` with Mixed Operands

`merge(Record([x: Int]), Map@[Str: String])` produces a value that is neither pure Record
nor pure Map. The result type is `Dict` — the umbrella union.

If both operands are `Record`, the result is `Record` (row concatenation). If both are
`Map@[K: V]` with matching V, the result is `Map@[K: V]`. Mixed operands produce `Dict`.

### Narrowing and `dict?` / `record?` / `map?`

After the split, `dict?` narrows to `Dict` (the union type). Two new predicates:

- `record?` — narrows to `Record` (any structural record with row-polymorphic type)
- `map?` — narrows to `Map` (any homogeneous map, key/value types unknown without annotation)

BAS refinement in `match` arms narrows from annotated types:

```tinct
[match val
  @[x: Int  y: Str]:  ...    # Record branch — x and y proved present
  @[Map [Str: Int]]:  ...    # Map branch — get returns Int|Null
  @Dict:              ...    # catch-all for untyped dicts
  _:                  ...]
```

### The Empty Dict and `Map@[K: V]`

The empty dict `[]` has type `Null = Record(Row{fields:{}, tail:Empty})`. The empty dict
is a valid homogeneous map of any type: `Null <: Map@[K: V]` for all K, V.

### Structural Equality

Dict equality (`=`) is **order-insensitive structural equality** for both `Record` and
`Map@[K: V]`. Both forms reduce to the same algorithm at runtime: sort keys canonically,
then compare key-by-key and value-by-value recursively. `[= [a: 1 b: 2] [b: 2 a: 1]]`
is `true`.

**Implementation** (`src/builtins_math.rs`):
1. Extract and sort keys from both dicts (string keys lexicographically, integer keys numerically, string keys sort after integer keys)
2. If key sequences differ, return `false`
3. For each key pair, force both value thunks and recurse
4. Cycle detection via a visited set of `(ThunkId, ThunkId)` pairs — if the pair is already in the set, treat as equal (structural coinduction)

## Implementation

### Type System (`src/types.rs`)

- Add `Type::Map(Box<Type>, Box<Type>)`
- Register `Record`, `Map`, `Dict` in `TypeEnv::with_builtins()`; `Record` and `Map` produce fresh-variable open types at each use; `Dict` is a BAS union
- Add `is_subtype` rules: `Map(K, V₁) <: Map(K, V₂)` when `V₁ <: V₂` (K invariant, V covariant); `Record(ρ) <: Dict`; `Map(K,V) <: Dict`; `Null <: Map(K,V)`; [RECORD→MAP] cross-form rule
- Add `Map` handling to `value_matches_type` for TypeAssert runtime checking

### Type Checker (`src/typecheck.rs`)

- `check_get`: target type `Map@[K: V]` → return `V | Null`; target `Record(ρ)` → return field type (total)
- `check_get?`: new builtin, returns `V | Null`
- `check_get_or`: target `Map@[K: V]` → return `V`
- `check_match`: BAS refinement for `Dict` union elimination

### Builtins (`src/builtins.rs`)

- Add `get?`: returns the value or `[]` (Null) on missing key — no error
- Add `record?`, `map?` type-narrowing predicates (returning Bool)
- Update `dict?` to narrow to `Dict` (union)
- Implement structural dict equality in `builtin_eq` (`src/builtins_math.rs`): sorted-key recursive walk, cycle detection via `(ThunkId, ThunkId)` visited set

### Standard Library

- `get-or` implemented in terms of `get?`: returns the value or the default
- Type signature: `Map@[K: V] → K → V → V`
- `has?` type signature: `Map@[K: V] → K → Bool`

## References

- Chau, C.Y. & Parreaux, L. (2026). "Boolean-Algebraic Subtyping: Intersections, Unions, Negations, and Principal Type Inference." *Proc. ACM Program. Lang.*, 10(POPL). — BAS provides the union type algebra that makes `Dict = Record ∨ Map` well-formed and union elimination in pattern matching sound; prerequisite for this design.
- Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." *POPL '82*, pp. 207–212. — Principal type property: inference must produce the most specific type; motivates why dict literals always infer `Record`, not `Map@[K: V]`.
- Findler, R.B. & Felleisen, M. (2002). "Contracts for higher-order functions." *ICFP '02*, pp. 48–59. — Proxy contract mechanism for lazy structural TypeAssert; used for O(n) `@[Map [K: V]]` runtime validation.
- Nickel language documentation (Tweag). nickel-lang.org. — Nickel's `{_: Type}` dictionary type as precedent for the Record/Map split; the cross-form subtyping rule `{f₁: T₁, ..., fₙ: Tₙ} <: {_: T}` when all Tᵢ <: T.
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. — §11 (Record types and subtyping); §23 (Universal polymorphism and System F); row-polymorphic records and parametric type constructors address orthogonal typing concerns.
- Rémy, D. (1994). "Type Inference for Records in a Natural Extension of ML." *Theoretical Aspects of Object-Oriented Programming* (Gunter & Mitchell, eds.), MIT Press. — Row polymorphism for structural records; basis for tinct's current `Record(Row)` type.
