# Null Semantics

## Overview

`Null` is the type name for the closed empty-dict type — `Type::Record(Row::Empty)`. It captures tinct's existing empty-dict null convention without adding a new runtime variant.

- **Precise I/O types**: `fn@Null` makes void-returning functions self-documenting. `[fn@Null [content@String cap@DirCap] ...]` tells the caller there is no meaningful return value.
- **Annotation round-trips**: The formatter emits `fn@Null` and the type checker accepts it back.
- **Foundation for nullable types**: `Null` as a named type enables `String | Null` (from the union-types proposal) to express optional values cleanly.
- **LSP hover accuracy**: functions like `write` and `emit` hover as `→ Null`, not `→ Any`.

## Supersession Notes

- **`@Dict` semantics**: Under [parameterized-dict.md](parameterized-dict.md) (2026-05-09), `@Dict` resolves as a closed empty Record (`Type::Record(Row { fields: HashMap::new() })`), not an open record with a row-variable tail. The `Record ∨ Map` BAS union described in parameterized-dict is the design target but the current resolution is conservative.
- **`Row::Empty`**: There is no `Row::Empty` variant. Use `Row { fields: HashMap::new() }` (or equivalently `Row::default()`).

## Decision: `null` keyword

**Status: Not planned.** The `null` keyword (desugaring `null` → `[]`) is not scheduled. `[]` already serves as the null value and is well-understood. Adding a keyword would be purely cosmetic — the type system, evaluator, and existing code all work with `[]` as null. Users write `[]` explicitly; `@Null` in annotation position already resolves to the empty closed record type. No sprint created.

## Design

`Null` is the type name for the closed empty-dict type — `Type::Record(Row::Empty)`. This is the tightest static type for `{}`: a record with exactly zero fields and a closed row tail.

```tinct
# @Null on a function return type
write-config: [fn@Null [path@String  content@String  dir@DirCap]
    [write dir path content]]

# @Null as a parameter type (unusual but consistent)
assert-done: [fn@Boolean [result@Null]
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
- `[@Null expr]` type-asserts that `expr` is an empty dict: it fails at runtime if `expr` is a non-empty dict or any non-dict value

**Implementation**: one arm in `resolve_type_name`:

```rust
"Null" => Ok(Type::Record(Row::Empty)),
```

**`null` keyword**: a `null` keyword desugaring to `[]` improves ergonomics and follows in a subsequent proposal. This feature focuses on the type-level fix.

### No new `Type::Null` variant

An alternative design adds a new `Type::Null` variant to the `Type` enum, distinct from `Type::Record(Row::Empty)`. This distinguishes "this dict must be empty" from "this is a null value" at the type level. However:

- The distinction does not exist at runtime — both are `Value::Dict(IndexMap::new())`
- Adding a new enum variant requires updating every `match` arm in the type checker, formatter, and resolver
- The extra expressiveness is not useful without union types: the interesting question is `String | Null`, and that question belongs to the union-types proposal

`Type::Record(Row::Empty)` captures the real semantics — null is an empty dict — at zero implementation cost.

### Subtyping

`Null` is not a subtype of all types. It is a `Dict` — specifically the closed empty-dict type. This means:

- `null?` and `dict?` both return true for null values (correct)
- `[@String some_nullable]` is a type error — you cannot silently pass null where a string is expected
- SQL-style null propagation (`null + 1 = null`) does not apply — tinct's `+` errors on an empty-dict argument

### Nullable Types (completes with union-types Phase 2)

The correct way to express "this value may be a string or null" is:

```tinct
# future syntax — depends on union-types Phase 2
result: x@[String | Null]
```

This completes when the union-types proposal Phase 2 lands — `String | Null` becomes `Type::Union(Type::Str, Type::Record(Row::Empty))` with no additional work.

## Implementation

### Type Checker (`src/typecheck.rs`)

`resolve_type_name` gains `"Null" => Ok(Type::Record(Row::Empty))` alongside `"Int"`, `"Float"`, etc. Previously, writing `fn@Null` caused the name to fall through to the type-variable path (lowercase check fails, then env lookup), resulting in an unexpected type variable rather than an error or a concrete type.

### Builtin Type Signatures (`src/types.rs`)

`write`, `emit`, `revoke-cap`, `mkdir`, `delete` and other void builtins previously registered `Type::Any` as their return type. Their return type is now `Type::Record(Row::Empty)` (i.e., `Null`), making LSP hover and type inference accurate. Approximately 5–6 builtin type entries updated.

### `env` Builtin (`src/types.rs`)

`env` retains `Type::Any` until union types provide `String | Null`. The type signature comment documents this.

**Gradual-typing B2 interaction note:** After `doc/whatif/gradual-typing.md` Phase 2
splits `Type::Any` into `Unknown` (consistent with everything) and `Top` (true supertype),
`env` and similar builtins are reclassified from `Any` to `Unknown`. A value from
`env` used in a `String | Null` annotation context — `x@[or String Null]` — is then
checked via `is_consistent(Unknown, Union(Str, Record(Row::Empty)))` rather than
the current `is_subtype(Any, ...)`. The consistency check succeeds (Unknown is consistent with
any type), but the behavior is semantically different from the B1 phase where `Any`-as-bottom
satisfied subtype checks. The migration of `env`-style builtins from `Any` to `Unknown` at B2
must be audited to confirm no unexpected narrowing failures arise in `String | Null`
annotation contexts.

### `null?` Predicate (`src/types.rs`)

`null?` remains registered as `Any → Bool`. The runtime check is `map.is_empty()`. `null?` must accept `Any` because it is used to discriminate unknown values (`[null? [from-json input]]`). The semantics are precise: `null?` returns true if and only if the argument is `Null` (empty dict). A hypothetical `Null → Bool` signature makes `null?` useless as a guard.

### Documentation (`doc/05-type-annotations.md`, `doc/11a-builtins.md`)

`Null` appears in the type conventions table in `doc/05-type-annotations.md`. `doc/11a-builtins.md` shows `fn@Null` signatures for void builtins.

## References

- tinct `doc/whatif/union-types.md` — `String | Null` nullable type syntax; Phase 2 is when `Null` becomes fully useful as a union member
- tinct `src/types.rs` — `Type::Record(Row::Empty)` is the existing closed empty-record type that `Null` resolves to
