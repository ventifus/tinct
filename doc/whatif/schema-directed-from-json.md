# What If: Schema-Directed from-json

**State:** Proposal

What would it take to add a typed variant of `from-json` that returns a specific Record type rather than `Unknown`?

## Current State

`from-json` in tinct returns `Type::Unknown` — the parsed JSON value has no static type. Any downstream field access, arithmetic, or function call on the result is untyped:

```tinct
data: [from-json input]   # data: Unknown
port: data.port           # port: Unknown — no type checking
server: [start port]      # no warning if start expects Int
```

### What's Missing

1. A way to declare the expected schema for a JSON parse
2. Type-directed parsing that validates structure at the boundary
3. Static type for the parsed result enabling downstream type inference

## Why Schema-Directed from-json Matters for tinct

**Config file processing**: Tinct is primarily a configuration language. Typed JSON ingestion would make config-reading programs statically verified.

**Boundary validation**: The boundary guard system (commit 887f75b) already inserts runtime guards at Unknown→concrete crossings. Schema-directed `from-json` makes these guards explicit and static.

**Documentation**: The schema doubles as documentation of the expected JSON structure.

## Design

```tinct
# Schema-directed from-json: returns {host: Str, port: Int}
config: [from-json @[host: Str  port: Int] input]
# config : {host: Str, port: Int}

# Use the port — type-checked as Int
server: [start config.port]
```

The `@[...]` annotation after `from-json` specifies the expected Record schema. The type checker:
1. Uses the annotation as the return type of `from-json`  
2. Inserts a `TypeAssert` at the parse boundary
3. The runtime validates the structure and raises `[E005]` on mismatch

Alternative syntax using a type alias:
```tinct
Config: [type [record host: Str  port: Int]]
config: [from-json@Config input]
```

## What Would Change

### src/type_env.rs

**Current:** `from-json` registered as `Type::Unknown` return.
**Proposed:** Add `from-json@Schema` variant or optional annotation parameter.
**Impact:** Minor — special-case in `check_call`.

### src/typecheck.rs

**Current:** No special handling for `from-json`.
**Proposed:** When `from-json` call has an annotation, use the annotation type as the return type; insert boundary guard.
**Impact:** Minor — new case in call type-checking.

## Prerequisites

- `Type::Variant` for nominal type discrimination (companion proposal `type-variant.md`)
- `hkt-map-filter-types` for Map[K: V] precise types (companion proposal `hkt-map-filter-types.md`)

The schema-directed parse returns a Record type — no HKT required for basic `{field: Type}` schemas.

## References

- Garcia, R. et al. (2016). "Abstracting Gradual Typing." *POPL 2016.* — blame-tracked boundaries between typed and untyped regions
- Tobin-Hochstadt, S. & Felleisen, M. (2008). "The Design and Implementation of Typed Racket." *POPL 2008.* — typed/untyped boundary contracts
