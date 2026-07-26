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

Match arms come in two forms:

**Keyed shorthand arms** — pattern is the key, body is the value. Concise for non-binding or simple arms:

```tinct
[match color
  Color.Red:   "#ff0000"   # unit constructor — no binding
  Color.Green: "#00ff00"
  42:          "forty-two" # literal equality — no binding
  ...:         "other"]    # wildcard — no binding
```

**`[case ...]` arms** — the canonical 3-argument form when names must be bound:

```tinct
[case [let bindings]  pattern  body]
```

- **`[let bindings]`** — declares which names in `pattern` are binding targets. Listed names are introduced into `body`'s scope. Empty `[let]` means no new bindings.
- **`pattern`** — the structural match. Dispatch rules:
  - Uppercase or dot-access head (`[Result.Ok v]`, `[Constructor field: v]`) → structural constructor match; names listed in `[let]` bind payload fields
  - Lowercase or operator head (`[> n 0]`, `[= n x]`) → guard expression, evaluated with all `[let]` names bound to the scrutinee
  - `...` → wildcard, always matches
- **`body`** — evaluated when the arm matches, with `[let]` names in scope.

**Binding-name rule:** A name in `pattern` that IS listed in `[let bindings]` is a fresh binding target. A name in `pattern` that is NOT listed in `[let]` is a pin — looked up from the enclosing scope and compared to the scrutinee.

```tinct
[match result
  [case [let v]  [Result.Ok v]   v]           # v listed in [let] → fresh binding
  [case [let e]  [Result.Err e]  [log e]]     # e listed in [let] → fresh binding
  ...:                           0]           # wildcard — no binding

[match n
  [case [let n]  [> n 0]  "positive"]        # guard: [> n 0] evaluated with n bound to scrutinee
  [case [let n]  [< n 0]  "negative"]
  ...:                    "zero"]            # wildcard fallback
```

Both arm forms can appear in the same `[match]` expression.

### Patterns

| Pattern | Syntax | Behavior |
|---------|--------|----------|
| Wildcard | `...` | Matches anything; does not bind |
| Pin | `varname` | Looks up `varname` in the enclosing scope; matches if scrutinee equals that value |
| Literal | `42`, `"text"`, `true` | Matches exact value by equality |
| Constructor (unit) | `Color.Red:` | Matches unit constructor variant by qualified tag; no payload |
| Constructor (wildcard payload) | `[Tag ...]:` | Matches any nominal variant with that tag; ignores payload |
| Constructor (sub-pattern) | `[case [let p] [Tag p] body]` | Matches tag; binds payload dict to `p` in `body` |
| Dict field | `[k: sub-pat]` | Matches if scrutinee has key `k` and its value matches `sub-pat` |

**Binding rule:** Names listed in `[let bindings]` of a `[case ...]` arm are fresh binding targets — they receive values from the matched structure. Names in the pattern that are NOT listed in `[let]` are pins — they are looked up from the enclosing scope and compared for equality against the matched position.

**Constructor tag qualification:** Tags are assembled as qualified strings by the parser. For dot-access heads (`[Result.Ok v]`, `Color.Red:`), the parser calls `flatten_dot_access_to_tag` (defined in `src/ast.rs` as `pub(crate)`) to walk the `DotAccess` chain and assemble `"Result.Ok"`, `"Net.Transport.Tcp"`, etc. directly from the AST structure — no type environment access.

### `primitive_eq` — Primitive Equality

Tinct has one `primitive_eq` function in `src/eval.rs` (synchronous). Pattern matching and type-specific equality builtins go through it:

- `builtin_eq_int` (`builtin-eq-int`) — calls `eval::primitive_eq`
- `builtin_eq_float` (`builtin-eq-float`) — calls `eval::primitive_eq`
- `builtin_eq_string` (`builtin-eq-string`) — calls `eval::primitive_eq`
- Pin patterns — calls `eval::primitive_eq`
- `builtins_meta.rs` enum constraint handler — calls `eval::primitive_eq`

The implementation handles: `(Int, Int)`, `(Float, Float)`, `(String, String)`, `(Variant{payload:None}, Variant{payload:None})` unit-tag equality. All other combinations (including cross-type Int/Float, Dict, payload Variants) return `false`.

The `=` operator dispatches through Equatable type class instances. Type-specific builtins (`builtin-eq-int`, `builtin-eq-float`, `builtin-eq-string`) implement the instance methods. Types without an explicit Equatable instance fall through to the catch-all which returns `Boolean.False`. Use pattern matching for variant comparison.

### Exhaustiveness Checking

Exhaustiveness checking for nominal ADTs uses `TyConDef.constructors: Vec<(String, usize)>`. For any `TyCon(name)` or `App(TyCon(name), _)` scrutinee, look up `name` in `TyConEnv` and enumerate its constructors. Only the qualified tag string and arity are needed — field types are irrelevant to the Maranget (2007) matrix decomposition algorithm.

Builtin-type TyCons (`Int`, `Str`, etc.) have empty constructor sets — a match without a wildcard arm produces an incomplete-match warning, same behavior as today.

Exhaustiveness checking runs **only** for `Type::Union` scrutinees and nominal ADT types. When the type checker infers that the scrutinee has a union type (e.g., `Result = Ok(T) | Error(E)`), it verifies that all constructors are covered by the match arms.

For non-union, non-ADT scrutinees (e.g., `Int`, `String`, `Record`), exhaustiveness is not checked. If no pattern matches at runtime, a `MatchError` is raised.

### Example: Result Unwrapping

```tinct
[match [try-operation input]
    [case [let payload] [Result.Ok payload]    payload.value]
    [case [let payload] [Result.Error payload] [error payload.msg]]
    ...:                                       [error "no match"]]
```

### Example: Option Handling

```tinct
[match [find-user id]
    [case [let user] [Option.Some user] user.name]
    Option.None:                        "Unknown user"]
```

### Dynamic Errors

If the scrutinee matches none of the provided patterns, a `MatchError` is raised at runtime:

```text
error: no match arm satisfied
  at match expression line 42
```

### Implementation Notes

- **AST node:** `SurfaceExpression::Match { scrutinee, arms }`
- **Arm pattern type:** `SurfaceMatchArm.pattern: Arc<SurfaceNode>` — the pattern is stored as a raw Surface AST node (no separate Pattern enum)
- **Type inference:** `infer_match` in `src/typecheck.rs` infers the return type as the union of all arm expression types, narrowed by the scrutinee type
- **Evaluation:** `eval_materialize.rs` materializes the scrutinee, then evaluates arms in order until a pattern matches. `match_pattern` in `src/eval.rs` dispatches directly on `SurfaceExpression` variants: `Placeholder` = wildcard, `VarRef` = pin (resolved) or undefined (silently no-match), literals = equality, `Field` = constructor tag, `Call` = constructor+sub-pattern, `Dict` = dict field presence
- **`flatten_dot_access_to_tag`:** defined in `src/ast.rs` as `pub(crate)`; called from `src/eval.rs` (Field and Call pattern arms), `src/coverage.rs` (Field and Call coverage arms), and `src/typecheck_cek.rs` (arm narrowing and tag extraction)

See `doc/feature/nominal-variants.md` for the nominal variant design and `src/typecheck.rs` for the complete exhaustiveness algorithm.

### Variable Names in Keyed Patterns

Keyed match arm patterns introduce **no new bindings**. A name in pattern position is either a pin comparison (if the name resolves in the enclosing scope) or a wildcard (if the name is not in scope). The body of the arm cannot reference a name introduced by the pattern itself — only names already in scope when the match expression was entered.

```tinct
[
  x: 42
  # x is in scope — it acts as a pin: the pattern matches only when scrutinee equals 42
  result: [match 42
    x:   "matched"
    ...: "no match"]
  # → "matched"
]
```

If the same name appears twice in a keyed dict pattern (e.g., `[a: x  b: x  ...]`), each occurrence independently checks whether the matched field equals the in-scope value of `x` (if `x` is in scope) or silently passes (if `x` is not in scope). There is no binding of `x` from either occurrence.

**Equality matching:** To match when two dict fields have the same value, use `[case [let ...]]` with a guard:

```tinct
[match [a: 1  b: 1]
    [case [let va vb] [a: va  b: vb  ...] [= va vb]]: "equal"
    ...:                                               "not equal"]
# → "equal"
```

The `[let va vb]` form declares `va` and `vb` as names bound by the match; `[= va vb]` is the guard that checks equality. This is the correct way to impose equality constraints across matched fields.

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
