# Patterns & Comparisons

## Common Patterns

### Shared Base Config

```tinct
[
    base: [timeout: 30  retries: 3]
    dev:  [merge base [env: "dev"]]
    prod: [merge base [env: "prod"  timeout: 60]]
]
```

### List Transformation

```tinct
[
    users: [...]

    # Filtering and projection
    admin-users: [filter [fn [u] u.is-admin] users]
    user-names:  [map [fn [u] u.name] users]

    # Complex predicates
    senior-admins: [filter [fn [u] [and u.is-admin [> u.age 40]]] users]
    user-summaries: [map [fn [u] [n: u.name  a: u.age]] users]

    # List operations (renumber to clean list)
    reversed: [reverse user-names]
    sorted: [sort user-names]
    without-first: [rest users]

    # Filter + reindex for clean dense list
    active: [-> users
        [filter _.active _]
        reindex]
]
```

### Conditional Logic

```tinct
[
    mode: "production"
    config: [if [= mode "production"]
        [timeout: 60  logging: "error"]
        [timeout: 10  logging: "debug"]]

    # Conditional — returns body or [] (empty dict)
    debug-info: [when [= mode "dev"] [trace: true  verbose: true]]
]
```

### Template Function

```tinct
[
    make-service: [fn@[name: String  port: Number  health: String] [name@String  port@Number]
        [name: name  port: port  health: "/health"]]

    web: [make-service "web" 8080]
    api: [make-service "api" 3000]
]
```

### Pipeline (Using Stdlib Threading)

```tinct
[-> raw-data
    [filter active? _]
    [map extract-name _]
    [sort-by last-name _]]
```

### Library Module (Private Helpers, Public API)

Within a single document, each dict expression's bindings become the parent scope for the next expression, but **only the last expression is returned**. Earlier dicts are private: visible inside the document but not exported to callers.

```tinct
# Private helpers — in scope below, not exported
[
    clamp-impl: [fn@Number [lo@Number hi@Number x@Number]
        [if [< x lo] lo [if [> x hi] hi x]]]

    lerp-impl: [fn@Number [a@Number b@Number t@Number]
        [+ a [* [- b a] t]]]
]

# Public API — only this dict is returned by include / create_stdlib_env
[
    clamp: [fn@Number [lo@Number hi@Number x@Number]
        [clamp-impl lo hi x]]

    lerp: [fn@Number [a@Number b@Number t@Number]
        [clamp-impl 0.0 1.0 [lerp-impl a b t]]]
]
```

Callers see `clamp` and `lerp`; `clamp-impl` and `lerp-impl` are unreachable. This is the pattern used throughout the standard library to keep `-impl`, `-step`, and `-check` helpers out of the public namespace.

The same principle applies when merging an included file into scope via sequential expressions:

```tinct
[include "lib/math.llt"]   # math helpers become parent scope

[
    result: [lerp 0 100 0.5]   # lerp visible via parent — math internals are not
]
```

---

## Pattern Matching

The `[match]` special form provides pattern matching with exhaustiveness checking and automatic binding of matched values.

### Syntax

```tinct
[match scrutinee
    pattern₁: expr₁
    pattern₂: expr₂
    patternₙ: exprₙ]
```

Each arm is a `pattern: body` keyed entry — the pattern is the key, the body is the value. This uses tinct's standard key-value syntax and makes arm boundaries unambiguous.

### Patterns

| Pattern | Syntax | Behavior |
|---------|--------|----------|
| Wildcard | `_` | Matches anything; does not bind |
| Variable | `x` | Matches anything and binds the value to `x` |
| Literal | `42`, `"text"`, `true` | Matches exact value |
| Constructor | `[Ok value]` | Matches nominal variant and binds payload to `value` |
| Constructor (no binding) | `[Tag]` | Matches any nominal variant with that tag, regardless of payload — equivalent to `[Tag _]`. **Note:** this equivalence is runtime-only; the exhaustiveness checker may emit a false non-exhaustive warning for payload-bearing variants until coverage.rs is updated (see B-252). |

### Exhaustiveness Checking

Exhaustiveness checking runs **only** for `Type::Union` scrutinees. When the type checker infers that the scrutinee has a union type (e.g., `Result = Ok(T) | Err(E)`), it verifies that all constructors in the union are covered by the match arms.

For non-union scrutinees (e.g., `Int`, `String`, `Record`), exhaustiveness is not checked. If no pattern matches at runtime, a `MatchError` is raised.

### Example: Result Unwrapping

```tinct
[match [try-operation input]
    [Ok value] [error [str "Operation failed: " msg]]]
```

### Example: Option Handling

```tinct
[match [find-user id]
    [Some user]: user.name
    [None]:      "Unknown user"]
```

### Dynamic Errors

If the scrutinee matches none of the provided patterns, a `MatchError` is raised at runtime:

```text
error: no match arm satisfied
  at match expression line 42
```

### Implementation Notes

- **AST node:** `SurfaceExpression::Match { scrutinee, arms }`
- **Type inference:** `infer_match` in `src/typecheck.rs` infers the return type as the union of all arm expression types, narrowed by the scrutinee type
- **Evaluation:** `eval_materialize.rs` materializes the scrutinee, then evaluates arms in order until a pattern matches
- **Pattern compilation:** Constructor patterns are compiled to `Pattern::Constructor` AST nodes; the evaluator uses `match_pattern` to test each arm

See `doc/feature/nominal-variants.md` for the nominal variant design and `src/typecheck.rs` for the complete exhaustiveness algorithm.

### Non-Linear Patterns (Last-Binding-Wins)

Unlike Standard ML, Haskell, and OCaml — which reject duplicate variable names within a single pattern — tinct allows them. When the same variable appears more than once in a pattern, the **last binding wins**: earlier bindings are silently shadowed.

```tinct
[match [a: 1  b: 2]
    [a: x  b: x  ...]: x]
# x is bound to 2 (the second occurrence shadows the first)
```

This is equivalent to writing `[a: _  b: x  ...]` — the first `x` has no observable effect.

**Rationale:** Tinct's pattern matching is gradual (soft-skip on mismatch), and the language is dynamically typed at its core. Rejecting non-linear patterns adds implementation complexity (a linearity check pass) without clear user benefit. In statically-typed languages, non-linear patterns are rejected because they could mask bugs where the user intended an equality constraint (as in Erlang, where duplicate variables mean "must be equal"). Tinct does not interpret duplicate variables as equality constraints — it uses last-binding-wins, consistent with how dict keys work (later entries shadow earlier ones with the same key).

**Equality matching:** To match when two fields have the same value, use a guard or a pin pattern:

```tinct
[match [a: 1  b: 1]
    [a: x  b: $x  ...]: "equal"
    _: "not equal"]
```

Here `$x` is a pin pattern that matches against the already-bound value of `x`, providing the equality-constraint semantics that non-linear patterns provide in Erlang.

---

## Comparison with Other Languages

See the comparison table below for how Tinct relates to JSONnet, Dhall, Nix, CUE, and jq.

| Need | Use |
|------|-----|
| Universal compatibility | JSON |
| DevOps convention | YAML |
| Large-scale Kubernetes | JSONnet |
| Type-safe configs | Dhall |
| Package management | Nix |
| Schema validation | CUE |
| Shell JSON transforms | jq |
| **Unified data + transformation** | **Tinct** |

### Data Selection: jq / JSONPath / JMESPath / Tinct

| Operation | jq | JSONPath | JMESPath | Tinct |
|-----------|-----|---------|----------|-----|
| Field access | `.name` | `$.name` | `name` | `data.name` |
| Nested access | `.a.b.c` | `$.a.b.c` | `a.b.c` | `data.a.b.c` |
| Deep path (dynamic) | `getpath(p)` | N/A | N/A | `[get-in path data]` |
| Computed key | `.["k"]` | `$['k']` | N/A | `[get key data]` |
| Key index | `.["k"]` | `$[0]`, `$[1]` | `[0]`, `[-1]` | `[get 0 data]`, `[get -1 data]` (key-based) |
| Positional index | `.[0]`, `.[-1]` | N/A | N/A | `[nth 0 data]`, `[nth -1 data]` |
| Key-range slice | N/A | N/A | N/A | `[slice 2 5 data]` |
| Positional slice | `.[2:5]` | `$[2:5]` | `[2:5]` | `[slice 2 5 data]` |
| First/last | `.[0]`, `.[-1]` | `$[0]` | `[0]`, `[-1]` | `[get 0 data]` (key 0), `[last data]` |
| Flatten | `flatten` | N/A | `[]` | `[flatten list]` |
| All values | `.[]` | `$.*` | `*` | `[values data]` |
| Filter (simple) | `select(.age > 30)` | `[?(@.age>30)]` | `` [?age>`30`] `` | `[filter [fn [u] [> u.age 30]] data]` |
| Filter (complex) | `select(.a and .b)` | `$[?@.a && @.b]` | `[?a && b]` | `[filter [fn [u] [and u.a u.b]] data]` |
| Projection | `.items[].name` | `$.items[*].name` | `items[*].name` | `[map [fn [x] x.name] items]` |
| Reshape | `{n: .name}` | N/A | `{n: name}` | `[map [fn [x] [n: x.name  a: x.age]]]` |
| Multi-select | `[.name, .age]` | N/A | `[name, age]` | `[data.name  data.age]` |
| Pipe/chain | `\|` | implicit | `\|` | `[-> ...]` |
| Optional access | `.foo?` | N/A | N/A | `[get-or "foo" default data]` |
| Existence check | `has("key")` | N/A | N/A | `[has? "key" data]` |
| Recursive descent | `..` | `$..name` | N/A | `[find-deep "name" data]` |

---

## Open Questions

### Structural Contracts

- [ ] **Shape/contract system** — Predicate-based validation separate from the type system. Allows runtime constraints beyond what types express (e.g., "port must be 1-65535").
- [ ] **OpenAPI integration** — Load external schemas as contracts for validation.
- [ ] **Lazy vs eager validation** — Validate on materialization vs explicit `[validate! schema data]`?
