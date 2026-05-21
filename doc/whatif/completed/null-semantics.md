# What If: Null Semantics for tinct

**State:** Proposal

What would it take to give tinct a formal `Null` type that captures the existing empty-dict null convention without adding a new runtime variant?

## Current State

tinct represents null as the empty dict `{}` at runtime. This convention is load-bearing:

- `from-json "null"` → `Value::Dict(IndexMap::new())`
- Sequences terminate with `{}` — the empty dict is the sequence sentinel
- `emit`, `write`, `revoke-cap` and other void-returning builtins return `Value::Dict(IndexMap::new())`
- `null?` checks `map.is_empty()` at runtime — identical to `empty?`

```tinct
# null is written as an empty dict literal
result: []

# null? tests for it
[null? result]      # true
[dict? result]      # also true — null IS a dict
[empty? result]     # also true
```

### What's Missing

1. **No `@Null` annotation** — `resolve_type_name` only handles `Int`, `Float`, `String`, `Bool`, `Number`, `Any`. Writing `fn@Null` is currently a parse error / falls through to type variable treatment.
2. **Void-returning builtins typed imprecisely** — `write`, `emit`, `revoke-cap` use `Type::Any` in their registered type signatures, with comments like `// returns Null (empty dict)`. The type system cannot express their actual return type.
3. **No `null` keyword** — users write `[]` to mean null, which is unidiomatic in config files.
4. **Nullable types require workarounds** — `env` can return a string or null, but its type is `Any`. There's no way to express `String | Null` without union types.

## Why Null Semantics Matter for tinct

- **Precise I/O types**: `fn@Null` makes void-returning functions self-documenting. `[fn@Null [content@String cap@DirCap] ...]` tells the caller there is no meaningful return value.
- **Annotation round-trips**: If the formatter emits `fn@Null`, the type checker must accept it back — currently it doesn't.
- **Foundation for nullable types**: once `Null` is a named type, `String | Null` (from the union-types proposal) expresses optional values cleanly.
- **LSP hover accuracy**: functions like `write` and `emit` currently hover as `→ Any`. They should hover as `→ Null`.

## Design

`Null` is the type name for the closed empty-dict type — `Type::Record(Row::Empty)`. This is the tightest static type for `{}`: a record with exactly zero fields and a closed row tail.

```tinct
# @Null on a function return type
write-config: [fn@Null [path@String  content@String  dir@DirCap]
    [write dir path content]]

# @Null as a parameter type (unusual but consistent)
assert-done: [fn@Bool [result@Null]
    true]

# Type assertion: check that a value is null
[@Null [env "UNSET_VAR"]]   # type error if env returns a string
```

**`Null` = `Type::Record(Row::Empty)`** — the closed empty-record type. It is not a new `Type` enum variant; it resolves to an existing type expression. This means:

- `[]` has static type `Null` — every empty-dict literal is `Type::Record(Row::Empty)`
- Builtins typed as `fn@Null` return `Null` statically: `emit`, `write`, `revoke-cap`, etc.
- Dynamically-empty dicts from operations like `from-json`, `merge`, or `filter` retain the static type of their operation (`Any`, `Record(...)`, etc.) — tinct has HM inference, not refinement types, so the type checker cannot prove that a general dict operation returns an empty result
- `null?` returns true for `Null`-typed values at runtime — the runtime check is the same as `empty?` — but its type signature is `Any → Bool`, not `Null → Bool`, because `null?` is a discriminating predicate meant to test unknown values
- `dict?` returns true for `Null`-typed values — null IS a dict in tinct
- `[@Null expr]` type-asserts that `expr` is an empty dict: it will fail at runtime if `expr` is a non-empty dict or any non-dict value

**Implementation**: one arm in `resolve_type_name`:

```rust
"Null" => Ok(Type::Record(Row::Empty)),
```

**`null` keyword**: a `null` keyword desugaring to `[]` improves ergonomics and should follow in a subsequent proposal. This proposal focuses on the type-level fix.

### No new `Type::Null` variant

An alternative design would add a new `Type::Null` variant to the `Type` enum, distinct from `Type::Record(Row::Empty)`. This would allow the type system to distinguish "this dict must be empty" from "this is a null value" at the type level. However:

- The distinction does not exist at runtime — both are `Value::Dict(IndexMap::new())`
- Adding a new enum variant requires updating every `match` arm in the type checker, formatter, and resolver
- The extra expressiveness is not useful without union types: the interesting question is `String | Null`, and that question belongs to the union-types proposal

`Type::Record(Row::Empty)` captures the real semantics — null is an empty dict — at zero implementation cost.

### Subtyping

`Null` is not a subtype of all types. It is a `Dict` — specifically the closed empty-dict type. This means:

- `null?` and `dict?` both return true for null values (correct)
- `[@String some_nullable]` is a type error — you cannot silently pass null where a string is expected
- SQL-style null propagation (`null + 1 = null`) does not apply — tinct's `+` will error on an empty-dict argument

### Nullable Types (completes with union-types Phase 2)

The correct way to express "this value may be a string or null" is:

```tinct
# future syntax — depends on union-types Phase 2
result: x@[String | Null]
```

This completes when the union-types proposal Phase 2 lands — `String | Null` becomes `Type::Union(Type::Str, Type::Record(Row::Empty))` with no additional work.

## What Would Change

### Type Checker (`src/typecheck.rs`)

**Current:** `resolve_type_name` has no arm for `"Null"`. Writing `fn@Null` causes the name to fall through to the type-variable path (lowercase check fails, then env lookup), resulting in an unexpected type variable rather than an error or a concrete type.

**Proposed:** Add `"Null" => Ok(Type::Record(Row::Empty))` to `resolve_type_name`, alongside `"Int"`, `"Float"`, etc.

**Impact:** Minor — one line.

### Builtin Type Signatures (`src/types.rs`)

**Current:** `write`, `emit`, `revoke-cap`, `mkdir`, `delete` and other void builtins register `Type::Any` as their return type, with inline comments noting the actual runtime value.

**Proposed:** Change return type to `Type::Record(Row::Empty)` (i.e., `Null`). This makes LSP hover and type inference accurate.

**Impact:** Minor — swap `Type::Any` for `Type::Record(Row::Empty)` in 5–6 builtin type entries.

### `env` Builtin (`src/types.rs`)

**Current:** `env` registers `Type::Any` because it may return either a `String` or null (when the variable is unset or not permitted).

**Proposed:** Retain `Type::Any` until union types provide `String | Null`. Document this in the type signature comment.

**Impact:** None for Phase 1. Retype to `String | Null` when union-types Phase 2 lands.

**Gradual-typing B2 interaction note:** After `doc/whatif/gradual-typing.md` Phase 2
splits `Type::Any` into `Unknown` (consistent with everything) and `Top` (true supertype),
`env` and similar builtins will be reclassified from `Any` to `Unknown`. A value from
`env` used in a `String | Null` annotation context — `x@[String Null]` — will then
be checked via `is_consistent(Unknown, Union(Str, Record(Row::Empty)))` rather than the
current `is_subtype(Any, ...)`. The consistency check succeeds (Unknown is consistent with
any type), but the behavior is semantically different from the B1 phase where `Any`-as-bottom
satisfied subtype checks. Users should not observe a difference in practice, but the migration
of `env`-style builtins from `Any` to `Unknown` at B2 must be audited to confirm no unexpected
narrowing failures arise in `String | Null` annotation contexts.

### `null?` Predicate (`src/types.rs`)

**Current:** `null?` is registered as `Any → Bool`. The runtime check is `map.is_empty()`.

**Proposed:** No change to the signature — `null?` must accept `Any` because it is used to discriminate unknown values (`[null? [from-json input]]`). The *semantics* are now precise: `null?` returns true if and only if the argument is `Null` (empty dict). A hypothetical `Null → Bool` signature would make `null?` useless as a guard.

**Impact:** None — documentation clarification only.

### Documentation (`doc/05-type-annotations.md`, `doc/11a-builtins.md`)

**Current:** `@Null` is not mentioned. Void-returning builtins are documented without type information about their return value.

**Proposed:** Add `Null` to the type conventions table in `doc/05-type-annotations.md`. Update `doc/11a-builtins.md` to show `fn@Null` signatures for void builtins.

**Impact:** Minor — documentation only.

## Phased Adoption

### Phase 1: `@Null` Annotation

One arm in `resolve_type_name`. Update builtin type signatures. Add corpus tests. Update doc/05-type-annotations.md.

- `src/typecheck.rs`: `"Null" => Ok(Type::Record(Row::Empty))`
- `src/types.rs`: Update return types for `write`, `emit`, `revoke-cap`, `mkdir`, `delete`
- `tests/corpus/`: Add `fn@Null` annotation test, `[@Null []]` assertion test
- `doc/05-type-annotations.md`: Add `Null` to type conventions table
- `doc/11a-builtins.md`: Update void-returning builtin signatures

### Phase 2: Nullable Types via Union Types

When the union-types proposal Phase 2 is implemented, `String | Null` becomes the idiomatic way to express optional values. No changes to null semantics — `Null` as `Record(Row::Empty)` slots directly into `Type::Union`.

**Depends on:** `union-types.md` Phase 2

### Prerequisites

None. Phase 1 has no dependencies.

### Trigger

Phase 1 is already triggered:

- The `type-checker-fixes` sprint requests `"Null"` in `resolve_type_name`
- I/O builtins (`write`, `emit`, `revoke-cap`) return void and need precise type signatures
- The formatter should be able to round-trip `fn@Null` annotations

## References

- tinct `doc/whatif/union-types.md` — `String | Null` nullable type syntax; Phase 2 is when `Null` becomes fully useful as a union member
- tinct `src/types.rs` — `Type::Record(Row::Empty)` is the existing closed empty-record type that `Null` resolves to
