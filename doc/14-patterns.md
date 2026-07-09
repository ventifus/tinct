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
  _:           "other"]    # wildcard — no binding
```

**`[case ...]` arms** — the canonical 3-argument form when names must be bound:

```tinct
[case [let bindings]  pattern  body]
```

- **`[let bindings]`** — declares which names in `pattern` are binding targets. Listed names are introduced into `body`'s scope. Empty `[let]` means no new bindings.
- **`pattern`** — the structural match. Dispatch rules:
  - Uppercase or dot-access head (`[Result.Ok v]`, `[Constructor field: v]`) → structural constructor match; names listed in `[let]` bind payload fields
  - Lowercase or operator head (`[> n 0]`, `[= n x]`) → guard expression, evaluated with all `[let]` names bound to the scrutinee
  - `_` → wildcard, always matches
- **`body`** — evaluated when the arm matches, with `[let]` names in scope.

**Binding-name rule:** A name in `pattern` that IS listed in `[let bindings]` is a fresh binding target. A name in `pattern` that is NOT listed in `[let]` is a pin — looked up from the enclosing scope and compared to the scrutinee.

```tinct
[match result
  [case [let v]  [Result.Ok v]   v]           # v listed in [let] → fresh binding
  [case [let e]  [Result.Err e]  [log e]]     # e listed in [let] → fresh binding
  [case [let _]  _               0]]          # wildcard

[match n
  [case [let n]  [> n 0]  "positive"]        # guard: [> n 0] evaluated with n bound to scrutinee
  [case [let n]  [< n 0]  "negative"]
  [case [let _]  _        "zero"]]
```

Both arm forms can appear in the same `[match]` expression.

### Patterns

| Pattern | Syntax | Behavior |
|---------|--------|----------|
| Wildcard | `_` | Matches anything; does not bind |
| Variable | `x` | Matches anything and binds the value to `x` |
| Literal | `42`, `"text"`, `true` | Matches exact value |
| Constructor | `[Result.Ok value]` | Matches nominal variant by qualified tag and binds payload |
| Constructor (no binding) | `Color.Red:` | Matches unit constructor variant, no binding |
| Constructor (no binding) | `[Tag]` | Matches any nominal variant with that tag, regardless of payload — equivalent to `[Tag _]` |
| TypeAssert | `[@Integer x]` | Matches if value has type `Int`, binds to `x` |
| TypeAssert (bare) | `[@Integer _]:` | Bare type assertion with wildcard binding — preferred form for type predicates |

### Constructor Pattern Qualification

The parser produces `Pattern::Constructor { tag, binding }`. Tags are assembled as qualified strings by the parser:

- **Dot-access heads** (`[Result.Ok v]`, `Color.Red:`): the parser calls `flatten_dot_access_to_tag` (defined in `src/ast.rs` as `pub(crate)`) to walk the `DotAccess` chain and assemble `"Result.Ok"`, `"Net.Transport.Tcp"`, etc. directly from the AST structure — no type environment access.
- **Bare uppercase words** (`None:`, `Tcp:`): the current parser produces `Pattern::TypeTag` for ALL bare uppercase `VarRef` in pattern position. Builtin-type names (`Int:`, `Str:`, `Bool:`, etc.) match via `value.type_name()` at runtime. For user variant names (`None:`, `Tcp:`), the prelude uses the bracket form `[None]:` which produces `Pattern::Constructor`; the bare word form produces `Pattern::TypeTag("None")` which matches nothing at runtime since no value has `type_name() == "None"`. *(S-845: the elaboration pass will rewrite bare uppercase words to `Pattern::TypeAssert` for builtin-type TyCons and `Pattern::Constructor` for user variants.)*
- **Rebound aliases** (`Ok` in scope as `Result.Ok`): the type checker's elaboration pass follows the binding to get the qualified tag.

The elaboration pass in `typecheck_match.rs` processes every pattern before type checking the arm bodies:

- `Pattern::TypeAssertPending { annotation, inner }` → call `resolve_annotation` → `Pattern::TypeAssert { resolved_type, inner }`
- `Pattern::Constructor { tag }` where tag resolves to a nominal type constructor → keep as `Pattern::Constructor` with qualified tag
- `Pattern::Constructor { tag }` where tag resolves to a `builtin-type` TyCon → rewrite to `Pattern::TypeAssert { resolved_type: Type::TyCon(tag), inner: binding }`
- `Pattern::Constructor { tag }` not found in TyConEnv → pattern left UNCHANGED (graceful fallback while T-1003/S-852 is pending); future: type error once tycon_env is populated

The elaborated pattern is used locally within `infer_match` for `collect_pattern_bindings`; the stored match arm pattern remains `TypeAssertPending` for the evaluator, which uses its own runtime resolution path.

`Pattern::TypeAssertPending` is a surface-only form produced by the parser for explicit `[@Type x]` annotations. The typecheck elaboration pass rewrites it to `Pattern::TypeAssert` for `collect_pattern_bindings`, but the evaluator also performs its own minimal runtime resolution from `TypeAssertPending` for known primitive type names. The `Pattern::TypeAssert` arm in the evaluator handles fully-resolved types once the elaboration bridge is complete (S-850+).

### Pattern AST Nodes

```rust
// Surface form — parser produces this; rewritten during elaboration
Pattern::TypeAssertPending { annotation: Spanned<Annotation>, inner: Option<Box<Spanned<Pattern>>> }

// Core form — elaboration produces this; evaluator uses this
Pattern::TypeAssert { resolved_type: Type, inner: Option<Box<Spanned<Pattern>>> }

// Constructor pattern (nominal variant)
Pattern::Constructor { tag: String, binding: Option<Box<Spanned<Pattern>>> }
```

`inner: None` = bare type pattern (`Int:`, `Color.Red:`). `inner: Some(pat)` = type-guarded binding (`[@Integer x]`).

The evaluator's `match_pattern` arm for `TypeAssert`:

> (S-850+: currently unreachable from normal eval pipeline — TypeAssertPending runtime handler at
> eval.rs is the operative path. The TypeAssert arm exists for the future state when elaboration
> results are persisted through to the evaluator.)

```rust
Pattern::TypeAssert { resolved_type, inner } => {
    if !value_matches_type(value, resolved_type) {
        return None;
    }
    match inner {
        None => Some(env),
        Some(pat) => match_pattern(pat, value, env, ctx),
    }
}
```

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
- **Elaboration:** `typecheck_match.rs` resolves `Pattern::TypeAssertPending → Pattern::TypeAssert` and qualifies constructor tags before type checking arm bodies. The elaborated pattern is used locally within `infer_match` for `collect_pattern_bindings`; the stored match arm pattern remains `TypeAssertPending` for the evaluator, which uses its own runtime resolution path.
- **`flatten_dot_access_to_tag`:** defined in `src/ast.rs` as `pub(crate)`; called from `src/parser.rs` (two sites) and `src/typecheck_special.rs` (monad detection)

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
