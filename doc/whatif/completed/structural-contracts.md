# What If: Structural Contracts for tinct

**State:** Accepted — 2026-05-05

What would it take to let tinct programs declare the shape of data
they expect — so that pipeline consumers, formatters, and library
functions can specify their input contracts?

## Current State

tinct has several mechanisms that touch on data validation, but no
unified contract system:

- **TypeAssert** (`[@Type expr]`) — runtime type assertion. Checks
  that a value matches a type at the point of use. Validates lazily
  for records (fields checked via proxy contracts when accessed, not
  upfront). Errors are type mismatches at the access site.

- **Parameter annotations** (`x@Number`) — declare expected types on
  function parameters. Checked by the type checker (advisory) and at
  runtime via TypeAssert semantics.

- **Type aliases** (`Person: [type [name: String  age: Number]]`) —
  named types for documentation and reuse. No runtime effect beyond
  TypeAssert.

- **`%` pipeline threading** — each document's output becomes `%`
  for the next. `%` has no declared type — it's whatever the previous
  stage produced.

### What's Missing

1. **No input contract declaration.** A formatter like `fmt/yaml.llt`
   accepts any value. A formatter like `fmt/nginx.llt` expects a
   specific shape (`{port: Int, hostname: String, locations: [...]}`),
   but there's no way to declare this — the user must read the code
   to discover the expected shape.

2. **No pipeline boundary type checking.** The type checker operates
   within a single document. It doesn't verify that document N's
   output matches document N+1's expected `%` type.

3. **No constraint validation beyond types.** Types express structure
   (`port: Int`) but not constraints (`port > 0 && port < 65536`).
   doc/14-patterns.md §Structural Contracts lists this as an open question.

4. **No schema inspection.** Tooling (LSP, CLI, documentation
   generators) cannot discover what a program expects without
   executing it.

## What Structural Contracts Would Provide

1. **Self-documenting formatters.** `fmt/nginx.llt` declares its
   expected input shape at the top. Users read the contract, not the
   implementation. `tinct describe fmt/nginx.llt` prints the contract.

2. **Pipeline validation.** `tinct eval data.llt fmt/nginx.llt` checks
   that `data.llt`'s output matches `fmt/nginx.llt`'s input contract
   before evaluation (or at the pipeline boundary, with a clear error).

3. **Rich constraints.** Beyond type shapes: range checks
   (`port: 1..65535`), string patterns, required vs optional fields,
   default values — the full expressiveness of a schema language.

4. **Composable contracts.** Combine contracts via intersection,
   extension, and refinement. A base contract can be extended for
   specialized formatters.

5. **Blame assignment.** When a contract violation occurs in a
   pipeline, the error identifies which stage produced invalid data
   and which contract it violated — not just "type mismatch at line N."

## Design

A hybrid approach: static types for structural shape, runtime schemas
for rich constraints. Two complementary layers, each doing what it
does best.

### Two Layers

The type system checks structure — "is this an Int? is this a String?"
— at type-check time, catching structural errors before evaluation.
Runtime schemas check constraints — "is this Int between 1 and 65535?"
— during evaluation, catching domain errors with full expressiveness.
Neither subsumes the other.

```tinct
# fmt/nginx.llt — typed interface + runtime schema
%@NginxConfig

NginxConfig: [type [port: Int  hostname: String  locations: [path: String  upstream: String]]]

nginx-schema: [
  port: [min: 1  max: 65535]
  hostname: [pattern: "^[a-z0-9.-]+$"]
]

[validate nginx-schema %]

---

[emit [to-nginx %]]
```

### Syntax

**Pipeline input types** use the existing `@` annotation syntax on
`%`:

```tinct
%@[port: Int  hostname: String]       # inline record type
%@NginxConfig                          # named type alias
```

This extends a syntax users already know from parameter annotations
(`x@Number`) and expression assertions (`[@Int expr]`). No new syntax
forms are needed for the type layer.

**Runtime schemas** are ordinary tinct dicts with recognized keys:

```tinct
nginx-schema: [
  port: [min: 1  max: 65535]
  hostname: [pattern: "^[a-z0-9.-]+$"]
  locations: [min-length: 1]
]
```

Schema keys: `type`, `min`, `max`, `min-length`, `max-length`,
`pattern`, `required`, `default`, `items`, `fields`, `enum`.

### Semantics

**`%@Type` binding.** When the type checker encounters `%@T` as the
first expression in a document, it binds `%` to type `T` within that
document's scope. Within a single file, cross-document checking
ensures document N's output type unifies with document N+1's `%@T`.
In multi-file pipelines, the same checking applies across files.

**`validate` evaluation.** `validate` walks a schema dict and a data
value in parallel, collecting ALL violations (not fail-fast). It
returns the data value on success (pass-through for pipeline use) or
throws a structured error listing violations with field paths:

```tinct
[validate nginx-schema %]
# On success: returns % unchanged
# On failure: error with [violations: [{field: "port"  message: "must be >= 1"} ...]]
```

**Schema composition.** Schemas are dicts, so they compose via
`merge`:

```tinct
base-schema: [hostname: [pattern: "^[a-z0-9.-]+$"]]
nginx-schema: [merge base-schema [port: [min: 1  max: 65535]]]
```

### Interaction with Type Inference

`%@Type` annotations create a unification constraint: the inferred
type of `%` (from the previous pipeline stage) must unify with the
declared type. This interacts with row polymorphism — an open record
type `[port: Int ...r]` accepts dicts with additional fields, while a
closed record type `[port: Int  hostname: String]` requires an exact
match.

For cross-document checking, the type checker must propagate the
output type of each document to the next document's `%` binding.
This requires extending the current single-document inference to a
pipeline-level inference pass.

### Interaction with Lazy Evaluation

TypeAssert already validates records lazily (proxy contracts check
fields when accessed, not upfront). `%@Type` follows the same
semantics — type checking is static, but any runtime enforcement
uses proxy contracts. `validate` is eager: it forces all fields
named in the schema and reports all violations at once.

This creates a design tension: lazy proxy contracts are efficient
(only check what's used) but can miss errors silently; eager
`validate` catches everything but forces evaluation. The two-layer
design lets users choose: `%@Type` for lightweight structural
checking, `validate` for exhaustive domain validation.

### Blame Assignment

When a contract violation occurs in a pipeline, the error must
identify which stage produced invalid data and which contract it
violated. The pipeline runner tags each `%` value with its source
stage. Contract violations include source-stage attribution:

```text
Error: contract violation at pipeline boundary (data.llt -> fmt/nginx.llt)
  fmt/nginx.llt expects: port to be Int
  Got: "8080" (String)
  Produced by: data.llt, line 3

  Hint: use [@Int %.port] to convert, or fix the producing stage
```

This follows Findler and Felleisen's (2002) positive/negative party
model: the producing stage is the positive party (obligated to
produce conforming data), and the consuming stage is the negative
party (obligated to use data according to its declared type). In
tinct's pipeline model, each `---` boundary or file boundary is a
contract boundary.

### Rationale

1. **`%@Type` extends existing syntax.** tinct already has `@`
   annotations on parameters and expressions. Extending to `%` is a
   natural generalization, not a new concept.

2. **Schema-as-dict is tinct-native.** Schemas are dicts — they can
   be composed via `merge`, passed as arguments, stored in variables,
   loaded via `include`. No new data model needed.

3. **Pipeline blame comes naturally.** The pipeline runner already
   knows which stage produced each `%` value. Enriching contract
   violation errors with blame context is a reporting improvement,
   not an architectural change.

4. **Tooling synergy.** `%@Type` enables LSP auto-complete for
   pipeline inputs. `validate` schemas enable `tinct describe` for
   human documentation.

5. **Precedent.** This mirrors the approach of languages that have
   both types and runtime validation:
   - Dhall has types (static, structural) + assertions (runtime,
     semantic)
   - CUE has types and constraints in the same lattice, but
     distinguishes structural from value constraints
   - TypeScript has types (static) + type guards (runtime)
   - Clojure has no types but spec (runtime, predicate-based)

## What Would Change

### Parser

**Current:** `%` is parsed as a variable reference. `@` annotations
are parsed on parameters (`x@Type`) and expressions (`[@Type expr]`).

**Proposed:** Parse `%@Type` as a document-level annotation. The
parser recognizes this as the first expression in a document and
treats it as a type binding for `%`.

**Impact:** Minor. The `@` annotation syntax already exists; extending
it to `%` requires a new parse rule but no new syntax primitives.

### Type Checker

**Current:** Type inference operates within a single document. `%` is
bound to `Any` (or the output type of the previous document within a
single file's `---` pipeline).

**Proposed:** (1) Resolve `%@Type` annotations and bind `%` to that
type within the document. (2) Cross-document type checking: document
N's output type must unify with document N+1's `%@Type`. (3)
Multi-file pipeline checking across files passed to `tinct eval`.

**Impact:** Moderate. Single-document type annotation is
straightforward. Cross-document and multi-file checking require
propagating type information across document boundaries — a new
capability for the type checker.

### Evaluator

**Current:** No schema validation builtins. TypeAssert handles
type-level checks via proxy contracts.

**Proposed:** Add `validate` builtin that walks schema dicts and data
in parallel, collecting violations. Add `describe` builtin that
introspects `%@Type` annotations and schema dicts.

**Impact:** Minor. `validate` is a pure function over dicts — it
uses existing dict traversal and type-checking primitives. `describe`
requires reading type annotations from parsed AST, which is a new
access pattern.

### CLI

**Current:** `tinct eval` processes a single file. Output is always
JSON.

**Proposed:** (1) `tinct describe file.llt` prints the input contract
(type annotation and/or schema). (2) `tinct check data.llt fmt.llt`
type-checks a pipeline without evaluating it.

**Impact:** Minor. New subcommands using existing infrastructure.

### Error Reporting

**Current:** Type errors report the mismatch location within a single
document. No cross-document attribution.

**Proposed:** Pipeline blame — contract violations identify the
producing stage, the consuming stage's contract, and suggest fixes
based on the mismatch type.

**Impact:** Moderate. Requires tagging `%` values with source-stage
metadata and threading that metadata through error construction.

## Phased Adoption

### Phase 1: `%@Type` — Pipeline Input Types

Declare the expected type of `%` at the document level:

```tinct
# fmt/nginx.llt
%@[port: Int  hostname: String]

[emit [to-nginx %]]
```

Implementation:

- Parse `%@Type` as first expression in a document
- Type checker resolves the annotation and binds `%` to that type
  within the document
- Within a single file, cross-document checking: doc N's output type
  must unify with doc N+1's `%@Type`
- Multi-file pipeline: same checking across files passed to
  `tinct eval`
- LSP: auto-complete `%` fields based on declared type

This phase enables the core use case: formatters declare their
expected input, and the type checker validates it.

### Phase 2: `validate` — Runtime Schema Validation

Add schema-as-dict validation for constraints beyond types:

```tinct
nginx-schema: [
  port: [min: 1  max: 65535]
  hostname: [pattern: "^[a-z0-9.-]+$"]
  locations: [min-length: 1]
]

[validate nginx-schema %]
```

Implementation:

- `validate` builtin: walks schema dict and data in parallel
- Collects ALL violations (not fail-fast)
- Returns data on success (pass-through for pipeline use)
- Throws structured error listing violations with field paths
- Schema keys: `type`, `min`, `max`, `min-length`, `max-length`,
  `pattern`, `required`, `default`, `items`, `fields`, `enum`

### Phase 3: `tinct describe` — Contract Inspection

Add CLI command to inspect a program's contract without running it:

```bash
$ tinct describe fmt/nginx.llt
Input (%):
  port: Int (1..65535)
  hostname: String (pattern: ^[a-z0-9.-]+$)
  locations: [{path: String, upstream: String}] (min-length: 1)
```

Implementation:

- Parse `%@Type` annotation and schema dicts
- Merge type information with schema constraints
- Output as human-readable description or as JSON/YAML for tooling

### Phase 4: Pipeline Blame

Enrich contract violation errors with blame context:

```text
Error: contract violation at pipeline boundary (data.llt -> fmt/nginx.llt)
  fmt/nginx.llt expects: port to be Int
  Got: "8080" (String)
  Produced by: data.llt, line 3

  Hint: use [@Int %.port] to convert, or fix the producing stage
```

Implementation:

- Pipeline runner tags each `%` value with its source stage
- Contract violations include source-stage attribution
- Hints suggest fixes based on the mismatch type

### Prerequisites

- **Phase 1:** Row polymorphism in the type system (for open vs closed
  record matching). Cross-document type checking infrastructure.
- **Phase 2:** Type predicates (`int?`, `str?`, etc.) for the
  `validate` builtin's type-checking dispatch.
- **Phase 3:** Phase 1 + Phase 2 (needs both to describe full
  contracts).
- **Phase 4:** Multi-file pipeline (from `doc/whatif/templating.md`
  Phase 1).

### Trigger

Phase 1 (`%@Type`): adopt after multi-file pipeline ships.
`stdlib/out/` formatters already exist and lack declared input shapes.

Phase 2 (`validate`): adopt after Phase 1. External data from
`from-json`/`from-yaml` is already untyped — schema validation fills
the gap.

Phase 3 (`tinct describe`): adopt after Phase 2. Contract inspection
is the tooling complement to contract declaration.

Phase 4 (pipeline blame): adopt after Phase 3. Multi-stage pipeline
debugging is the primary motivator for the entire contract system.

## References

**Contract systems:**

- Findler, R.B. & Felleisen, M. (2002). "Contracts for higher-order
  functions." *ICFP*, pp. 48-59. — Blame assignment theory.
  Positive/negative party model for identifying which module violated
  a contract. Directly applicable to tinct's pipeline boundaries.
- Dimoulas, C. et al. (2011). "Correct blame for contracts." *POPL*,
  pp. 215-226. — Formal semantics of blame assignment in the presence
  of higher-order contracts and module boundaries.
- Wadler, P. & Findler, R.B. (2009). "Well-typed programs can't be
  blamed." *ESOP*, LNCS 5502, pp. 1-16. — Blame calculus connecting
  contracts to gradual typing. Relevant if tinct's `Any` type
  interacts with contract boundaries.

**Schema validation:**

- Wright, A. et al. (2022). JSON Schema: A Media Type for Describing
  JSON Documents. Draft 2020-12. — Schema-as-document validation
  with `$ref`, `allOf`, `anyOf`, composition.
- Hickey, R. (2016). "clojure.spec — Rationale and Overview." —
  Predicate-based validation as composable specs, separate from the
  type system. `s/keys`, `s/and`, `s/conform` pattern.

**Type-level contracts:**

- Unison. "Ability types and structural contracts." — Types that
  encode capabilities and constraints in a content-addressed
  codebase.
- CUE. "Lattice-based configuration." — Values and constraints
  unified via lattice operations. Types are values; validation is
  evaluation.

**Language-specific:**

- Dhall. "Safety guarantees." — Total type system where well-typed
  programs cannot fail at runtime. Types as complete contracts.
- NixOS module system. `mkOption`, `types.*` — Typed option
  declarations with defaults, descriptions, and merge semantics.
