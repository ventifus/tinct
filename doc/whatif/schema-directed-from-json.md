# What If: Schema-Directed from-json

**State:** Proposal

What would it take to add a typed variant of `from-json` that returns a specific Record type rather than `Unknown`, with deep structural validation at the parse boundary?

## Current State

`from-json` returns `Type::Unknown` — the parsed JSON value has no static type. Any downstream field access, arithmetic, or function call on the result is untyped:

```tinct
data: [from-json input]   # data: Unknown
port: data.port           # port: Unknown — no type checking
server: [start port]      # no warning if start expects Int
```

### What's Missing

1. A way to declare the expected schema for a JSON parse
2. Deep structural validation at the boundary
3. Static type for the parsed result enabling downstream type inference
4. Precise, path-based error messages when validation fails

## Why Schema-Directed from-json Matters for tinct

**Config file processing**: Tinct is primarily a configuration language. Typed JSON ingestion makes config-reading programs statically verified.

**Boundary validation**: The boundary guard system already inserts runtime guards at Unknown→concrete crossings. Schema-directed `from-json` makes these guards explicit, structural, and user-visible.

**Documentation**: The schema doubles as documentation of the expected JSON structure.

## Design

### Syntax

The `@[...]` annotation after `from-json` specifies the expected Record schema. The annotation type drives both the static return type and the runtime validation:

```tinct
# Closed validation — @[host: Str  port: Int] is a closed Record type in tinct.
# Extra JSON fields are a validation error.
config: [from-json @[host: Str  port: Int] input]
# config : {host: Str, port: Int}

# Open validation — @[host: Str  port: Int  ...rest] has an open row variable.
# Extra JSON fields are kept as Unknown.
config: [from-json @[host: Str  port: Int  ...rest] input]
# config : {host: Str, port: Int, ...Unknown}
```

This is natural: tinct's record type system already distinguishes closed records from open-row records via the `...rest` syntax in annotations (per type-annotations-v2). Schema-directed `from-json` inherits this semantics directly — the annotation type is the schema.

Type aliases work identically:

```tinct
Config: [type [record host: Str  port: Int]]   # closed
config: [from-json @Config input]
```

### Validation semantics

**Deep and eager**: validation occurs at parse time, recursively. Every nested field is validated before `from-json` returns. This concentrates errors at the boundary, which is the point. Lazy/deferred validation is not supported — for a config language, a misspelled field three levels deep must surface at the `from-json` call, not at consumption.

**Nullable fields**: a field that may be JSON `null` must be declared `@[or T Null]` in the schema. An undeclared-nullable field receiving `null` is a validation error. JSON `null` maps to `Value::Null` (see the `json-null-fix` TODO item for the prerequisite bug fix).

**Nested containers**: `@[users: [Seq [name: Str  age: Int]]]` validates each element of the `users` array against `{name: Str, age: Int}`.

### Error model

A schema mismatch is a new error variant, `EvalError::schema_mismatch`, distinct from `type_assert_failed`. It carries structured path information (JSON Pointer, RFC 6901) and accumulates **all** mismatches in a single pass — not just the first:

```
schema mismatch in from-json:
  at /users/2/name: expected Str, got 42
  at /port: expected Int, got "eight"
```

The path format uses `/field` for dict keys and `/N` for sequence indices.

### Return type

`from-json @Schema input` returns `Schema` directly. A schema mismatch is a hard runtime error. For recovery, use `try`:

```tinct
result: [try [from-json @Config input]]
[match result
  [case [let c: Ok]  [start c.port]]
  [case [let e: Err] [log-error "bad config" error: e]]]
```

This matches `include`'s precedent — boundary ingestion functions fail hard; `try` is the recovery mechanism.

## What Would Change

### src/eval.rs or src/builtins_io.rs

**Current:** `from-json` returns `Value` with no schema checking.
**Proposed:** When called with a schema annotation (detected in `check_call` / builtin dispatch), run recursive structural validation before returning. New `validate_against_schema(value, schema_type) -> Vec<(JsonPath, Expected, Got)>` function. If the vec is non-empty, raise `EvalError::schema_mismatch`.
**Impact:** Moderate — new validation pass; new error variant.

### src/typecheck.rs

**Current:** No special handling for `from-json`.
**Proposed:** When `from-json` call has an annotation argument, use the annotation type as the static return type. No new inference rules — the annotation is already the type.
**Impact:** Minor — new case in call type-checking.

### src/error.rs

**Current:** No `schema_mismatch` variant.
**Proposed:** `ErrorKind::SchemaMismatch { mismatches: Vec<(String, String, String)> }` — (json_pointer_path, expected_type, got_type). Displayed as multiline.
**Impact:** Minor — new error variant and display impl.

## Prerequisites

- `json-null-fix` TODO item: `json_to_value` currently maps JSON `null` to `Value::Dict(IndexMap::new())` (empty dict — a bug). Schema validation requires `null` to map to `Value::Null`. This fix must land before `from-json @Schema` handles nullable fields correctly.
- `TypeAssert` mechanics and `Record` type (both already present).

## References

- Garcia, R. et al. (2016). "Abstracting Gradual Typing." *POPL 2016.* — blame-tracked boundaries between typed and untyped regions; schema validation is an instance of a positive-direction boundary check
- Tobin-Hochstadt, S. & Felleisen, M. (2008). "The Design and Implementation of Typed Racket." *POPL 2008.* — typed/untyped boundary contracts; fail-hard with `try` for recovery
- Findler, R.B. & Felleisen, M. (2002). "Contracts for Higher-Order Functions." *ICFP 2002.* — path-accumulating contract monitoring (first-order values use eager-check; higher-order deferred — JSON is first-order)
- RFC 6901. "JavaScript Object Notation (JSON) Pointer." — path format for error messages
