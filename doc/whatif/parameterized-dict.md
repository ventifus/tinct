# What If: Record/Map Split and Parameterized Maps for tinct

**State:** Proposal

What would it take to give tinct a principled distinction between structural records and homogeneous maps, with a typed `Map[K V]` constructor and a unified `Dict` alias?

## Current State

Tinct represents all dicts as a single runtime type (`Value::Dict(IndexMap<Key, Thunk>)`) and at the type level as `Record(Row)` — Rémy-style row polymorphism over string-keyed fields. This works well for structural records with known fields:

```tinct
# Structural record — type checker knows about "host" and "port"
server: [host: "localhost"  port: 8080]
[@[host: String  port: Int] server]  # ✓
```

But tinct conflates two fundamentally different dict shapes:

| Shape | Example | Typed as |
|-------|---------|---------|
| Structural record | `[host: "localhost"  port: 8080]` | `Record([host: String  port: Int], Closed)` |
| Homogeneous map | `[0: [1 2] 1: [3] 2: [4 5]]` | `Dict` (untyped — `Any`) |

The second shape — integer-keyed or string-keyed dicts where all values share a type — has no typed representation. Annotating `@Dict` accepts any dict without constraint. This is the gap.

### What's Missing

1. **Typed homogeneous maps.** NFA `transitions: Dict[Int Seq@Int]` (char code → successor state IDs) and regex `groups: Dict[Int String]` (capture group ID → matched text) cannot be precisely typed. Both use uniform-value integer-keyed dicts; the type checker sees `Any`.

2. **Typed access on dynamic-key lookups.** `[get k map]` where `map` is a typed `Map[Int String]` should return `String | Null` — the type checker knows the value type without knowing which key is present.

3. **A formal `Dict` type.** The annotation `@Dict` currently means "some dict, I don't know anything about it." It should have formal meaning as the union of structural records and homogeneous maps.

## Why Record/Map Split Matters for tinct

**Precise types for accumulator dicts.** `builtin-reduce` accumulating into a dict with uniform value type is idiomatic tinct. Today the result is `Any`. With `Map[K V]`, the type checker tracks value types through reduce pipelines.

**Self-documenting stdlib signatures.** `stat`, `tls-peer-cert`, `list-dir` return structured dicts — these are Records with known fields. NFA transition tables and regex group captures are Maps with uniform values. The distinction makes signatures precise.

**Type-safe dynamic dispatch.** Pattern-matching on `Dict` branches narrows: a `Record` branch knows field structure; a `Map[K V]` branch knows value type. Functions can be polymorphic over either form.

**`@Dict` as a formal type.** `Dict: [type [Record Map]]` makes `@Dict` a genuine union rather than a type-erasing escape hatch.

## Design

### The Three-Level Hierarchy

```
Dict = Record | Map            ← union alias; "any dict"
         ↑           ↑
  Record(Row)    Map[K V]      ← structural forms
         ↑           ↑
@[x:T y:U]  @Map[Int Seq@Int]  ← concrete annotations
```

`Dict` is defined in the prelude as a union type alias:

```tinct
Dict: [type [Record Map]]
```

This uses tinct's existing union type machinery. `Record` is the base type for any structural record (open row, fields unspecified). `Map` is the base type for any homogeneous map (`Map[Any Any]`). The union gives `@Dict` its formal semantics: "a value that is either a structural record or a homogeneous map."

### `Map[K V]` Type Constructor

`Map[K V]` is a new parameterized type constructor, parallel to `Seq[V]` which already exists:

```rust
// src/types.rs — parallel to existing Type::Seq(Box<Type>)
Map(Box<Type>, Box<Type>),   // Map(key_type, value_type)
```

Valid key types: `Int`, `Str`, or `Int | Str` (tinct's two runtime key types). Value type `V` is any type.

**Annotation syntax** (using tinct's existing parameterized type alias machinery):

```tinct
# Annotating NFA transitions
transitions: @Map[Int Seq@Int]

# Annotating regex capture groups
groups: @Map[Int String]

# Open map — any value type
index: @Map[Str Any]

# The base type (most general map)
some-map: @Map
```

### Access Semantics: `V | Null`

A key lookup on `Map[K V]` returns `V | Null` — the key may not exist at runtime:

```tinct
[get k map]          # Map[Int String] → String | Null
[has? map k]         # Map[Int String] → Bool
[get-or map k "—"]   # Map[Int String] → String  (default eliminator)
```

`get-or` already exists in the prelude; its type signature becomes precise:
`get-or: [fn@V [map@Map[K V]  k@K  default@V] ...]`

Structural record access is unchanged — `@[x: String]` guarantees `x` is present, so `rec.x` or `[get "x" rec]` returns `String` (not `String | Null`). The `| Null` is exclusive to `Map[K V]` access.

### `Record` as a Base Type

`Record` without parameters is the base type for any structural record — equivalent to an open row with no known fields. It subsumes all concrete record types:

```tinct
# Record — accepts any structural record
[fn [r@Record] r.name]   # Advisory: "name" access on unknown Record

# Concrete record — specific fields known
[fn [r@[name: String  age: Int]] r.name]  # ✓ precise
```

`Record` participates in row polymorphism: `@Record` is a supertype of all `@[field: T ...]` annotations.

### `Dict` as `Record | Map`

After the split:

```tinct
Dict: [type [Record Map]]
```

All existing `@Dict` annotations remain valid — they now have formal meaning as the union. Functions that accept either form use `@Dict`; functions that need precision use `@Record` or `@Map[K V]`.

### Type Narrowing

In pattern match arms, the type checker narrows `Dict`:

```tinct
[match val
  [@[x: Int  y: Str]  ...]   # Record branch — x and y are known
  [@Map[Str Int]  ...]        # Map branch — values are Int, get returns Int|Null
  [_ ...]]                    # catches other dicts
```

## What Would Change

### Type System (`src/types.rs`)

**Current:** `Type::Record(Row)` is the only dict-related variant. `@Dict` resolves to a bare `Dict` tag.

**Proposed:**
- Add `Type::Map(Box<Type>, Box<Type>)` — parallel to `Type::Seq(Box<Type>)`
- Register `Record` and `Map` as base type names in `TypeEnv::with_builtins()` 
- Define `Dict: [type [Record Map]]` in prelude type environment
- Add `is_subtype` rules: `Map(K, V) <: Map(K', V')` when `K <: K'` and `V <: V'`; `Map(K, V) <: Dict`; `Record(ρ) <: Dict`

**Impact:** Moderate — new variant, new subtyping rules, base type registration.

### Type Checker (`src/typecheck.rs`)

**Current:** `@Dict` annotations produce `Type::Any` or bare dict tag. `get` returns `Any` for dict access.

**Proposed:**
- `infer_dict`: when entries are all integer-keyed with uniform value type, infer `Map[Int V]`; otherwise infer `Record(Row)` as now
- `check_get`: if target type is `Map[K V]`, return type is `V | Null`; if `Record(ρ)`, return type is the field type (no Null)
- `check_get_or`: if target is `Map[K V]`, return type is `V` (default eliminates Null)

**Impact:** Moderate — new inference cases, new access typing rules.

### Stdlib (`stdlib/prelude.llt`)

**Current:** `Dict: [type ...]` does not exist. `get-or` has type `Dict → Key → Any → Any`.

**Proposed:**
- Add `Dict: [type [Record Map]]` to prelude type environment (via `src/imports.rs` registration)
- Update `get-or` type signature to `Map[K V] → K → V → V`
- Add `Record` and `Map` as named base types

**Impact:** Minor — type registration, signature updates. No runtime changes.

### Runtime (`src/eval.rs`, `src/value.rs`)

**No runtime changes.** Both `Record` and `Map[K V]` are `Value::Dict(IndexMap<Key, Thunk>)` at runtime — identical representation. The split is purely at the type level. Types are erased before evaluation.

**Impact:** None.

### Documentation (`doc/03-data-model.md`, `doc/05-type-annotations.md`)

**Current:** `@Dict` is documented as accepting any dict. Structural records and homogeneous maps are not distinguished.

**Proposed:** Add §Record vs Map to the data model chapter documenting the two shapes and when to use each. Update type annotation reference with `@Map[K V]`, `@Record`, and `@Dict`.

**Impact:** Minor.

## Phased Adoption

### Phase 1: `Map[K V]` Annotation and Base Types

Add `Type::Map(Box<Type>, Box<Type>)` to the type system. Register `Record`, `Map`, and `Dict` as named types. Enable `@Map[K V]` annotation syntax. Update `get` and `get-or` signatures for `Map[K V]` targets.

What this enables:
- Annotate NFA transitions as `@Map[Int Seq@Int]`
- Type-safe `get` returns `V | Null` on typed maps
- `@Dict` has formal meaning as `Record | Map`
- Existing code continues to work unchanged

No inference changes required — annotations are checked but not inferred.

### Phase 2: Map Inference from Usage

Infer `Map[K V]` from dict literals and `builtin-reduce` accumulation patterns:

```tinct
# Infer Map[Int String] from uniform int-keyed entries
groups: [0: "alice"  1: "bob"  2: "carol"]

# Infer Map[Str Int] from reduce accumulating same-typed values
counts: [builtin-reduce [fn [acc k] [set acc k [+ 1 [get-or acc k 0]]]] [] input]
```

What this enables: typed accumulator dicts without explicit annotations. `builtin-reduce` returning a dict infers the map type from the value type of `set` operations.

### Phase 3: BAS Integration (Optional)

If BAS (`doc/whatif/boolean-algebraic-subtyping.md`) is adopted, `Map[K V]` participates in the Boolean algebra:

```tinct
# Union of two map types
heterogeneous: @Map[Int String] | @Map[Int Int]

# Intersection — a map that is both (value must be subtype of both)
constrained: @Map[Int String] & @Map[Int NamedString]
```

This phase is not required for Phases 1 and 2. BAS is only needed for union/intersection *over* map types, not for the map type itself.

### Prerequisites

- **Phase 1:** Parameterized type aliases (complete — `[type [params] body]` is implemented). Union types (complete).
- **Phase 2:** Phase 1 complete.
- **Phase 3:** BAS adoption (`doc/whatif/boolean-algebraic-subtyping.md`).

### Trigger

- **Phase 1:** When `@Dict` appears in stdlib code that should have a more precise type (NFA transitions, regex groups, stat results, tls-peer-cert). The annotation gap is already present.
- **Phase 2:** When inferred `@Dict` in function return types causes type errors downstream that would be caught with `Map[K V]` inference.
- **Phase 3:** If map types need to participate in union/intersection reasoning (likely only needed for advanced type-level programming patterns).

## References

- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction, and Polymorphism." *ACM Computing Surveys*, 17(4), 471–523. — Universal polymorphism (Map[K V]) vs ad-hoc polymorphism (structural records).
- Garnock-Jones, N. et al. (2022). "Nickel: A configuration language with gradual typing." *POPL '22 (extended abstract)*. — Nickel's `{_: Type}` dictionary type as precedent for the Record/Map split.
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press, §11 (Records), §25 (System F with records). — Formal distinction between record types and parametric map types; they address orthogonal concerns.
- Rémy, D. (1994). "Type Inference for Records in a Natural Extension of ML." *Theoretical Aspects of Object-Oriented Programming* (Gunter & Mitchell, eds.), MIT Press. — Row polymorphism for structural records; orthogonal to parameterized map types.
