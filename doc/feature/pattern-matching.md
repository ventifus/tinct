# Pattern Matching

## Overview

`[match x ...]` is a first-class `Expr::Match` AST node with dedicated
type checker and evaluator support. Arms use dict syntax — the
pattern is the key, the body is the value. The evaluator materializes the
scrutinee, tries arms top-to-bottom, and evaluates the first matching arm's
body. No match → runtime error.

## Pattern Syntax

Patterns are `Arc<SurfaceNode>` values — the same AST nodes as expressions.
The evaluator's `match_pattern` function dispatches on `SurfaceExpression`
variant to determine matching behavior.

### Wildcard

`...:` (Placeholder(None, None)) — always matches, no binding:

```tinct
[match x
    42:   "forty-two"
    ...:  "other"]
```

### Literal Patterns

Integer, float, or string literals match by equality:

```tinct
[match x
    0:       "zero"
    42:      "the answer"
    "hello": "greeting"
    3.14:    "pi"
    ...:     "other"]
```

### Constructor Tag Patterns (Field)

A dot-access expression in pattern position matches a `Value::Variant` by
tag — the chain `TypeName.CtorName` must equal `tycon.ctor` in the variant:

```tinct
[match color
    Color.Red:   "#ff0000"
    Color.Green: "#00ff00"
    Color.Blue:  "#0000ff"
    ...:         "unknown"]
```

The tag must be a fully qualified `TypeName.CtorName` — bare uppercase names
are not automatically qualified at runtime. The evaluator flattens the
dot-access chain via `flatten_dot_access_to_tag` to produce the tag string.

### Constructor + Payload Patterns (Call)

`[Tag payload-pattern]:` — matches a `Value::Variant` by tag AND then matches
the payload value against `payload-pattern`:

```tinct
# Wildcard payload — match tag, ignore payload content
[match sh
    [Shape.Circle ...]: "any circle"
    ...:                "other"]

# Dict sub-pattern — match tag and destructure payload
[match sh
    [Shape.Circle [r: 5]]: "unit circle-ish"
    [Shape.Circle ...]:    "some circle"
    ...:                   "other"]
```

Rules for the payload sub-pattern:
- `[Tag]:` (no arg) — tag match only, no payload check (same as bare `Tag:`)
- `[Tag ...]:` — wildcard payload: tag matched, payload ignored
- `[Tag [dict-pat]]:` — recursively matches payload against the dict pattern
- `[Tag lit]:` — compares payload value against the literal
- Multiple args are not supported; `[Tag a b]:` is a runtime error

**For payload binding**, use the `[case [let p] ...]` form (see below) — the
Call pattern form does not introduce new bindings.

### Pin Patterns (VarRef)

A lowercase bare word in pattern position is a **pin**: it looks up the
variable in the current scope and compares the scrutinee against that value.
If the name is not in scope, the resolver emits a diagnostic and the arm
does not match.

```tinct
[
  expected: 42
  result: [match x
    expected:  "got 42"     # pin — compares x against current value of expected
    ...:       "other"]
]
```

Pin semantics:
- Resolver runs before the match and records the de Bruijn (level, slot) for each
  VarRef in pattern position via a `Resolution` OnceLock.
- At runtime: `Some(Some((level, slot)))` → look up value at that slot and compare.
- `Some(None)` → name not in scope → resolver warned; arm does not match.
- `None` → resolver did not run (internal error).

**`...` is the explicit wildcard, not `_`.** A bare `_` in pattern position is
treated as a pin attempt on the name `_`. Since `_` is almost never in scope,
the resolver sets `Some(None)` and the arm silently never matches — it is NOT a
wildcard. Use `...:` for wildcard arms.

### Dict Patterns

A dict literal in pattern position matches a dict value by checking that all
specified keys are present and their values match the corresponding sub-patterns.
Extra keys in the scrutinee are ignored (open matching):

```tinct
[match result
    [ok: v]:    v
    [err: msg]: [error msg]
    ...:        [error "unexpected"]]
```

Nested dict patterns compose recursively:

```tinct
[match event
    [type: "click"  target: [id: id-val]]:  [handle-click id-val]
    [type: "hover"  target: [id: id-val]]:  [handle-hover id-val]
    ...:                                     "ignored"]
```

Note: the sub-patterns `v`, `msg`, `id-val` are themselves VarRef nodes in
pattern position — they are pins. For these to work as bindings, use the
CaseArm form.

### CaseArm Form — Payload Binding

`[case [let bindings...] pattern body]` — the canonical form for match arms
that need to bind new names. It takes three arguments:

1. `[let name1 name2 ...]` — names that this arm introduces as bindings
2. `pattern` — the structural match (evaluated as in `match_pattern`)
3. `body` — the expression to evaluate when the arm matches, with bindings in scope

```tinct
[match sh
    [case [let p] [Shape.Circle p] [* p.r p.r]]
    ...:                            0]
```

In the CaseArm, `p` in the pattern position is a **binding**, not a pin —
the arm introduces `p` into the body's scope and binds it to the payload dict.
The `[let p]` declaration is what makes this a binding rather than a pin.

More examples:

```tinct
# Payload binding — p gets the variant's payload dict
[match result
    [case [let p] [Result.Ok p]    p.value]
    [case [let p] [Result.Error p] [error p.msg]]
    ...:                           [error "no match"]]

# Dict pattern with binding — v gets the value at key "ok"
[match response
    [case [let v] [ok: v] [str "success: " v]]
    [case [let] [err: ...] "error"]
    ...:                   "unknown"]
```

## Implementation

### Parser (`src/parser.rs`)

`match` is a keyword. The parser enters match-arm parsing mode for the key
position of each arm: the full expression is parsed as a `SurfaceNode` and
stored directly as `SurfaceMatchArm.pattern: Arc<SurfaceNode>`.

`[case [let bindings] pattern body]` is parsed as a `SurfaceExpression::CaseArm`.
The resolver and evaluator handle it specially.

### AST (`src/ast.rs`)

`SurfaceMatchArm.pattern` is an `Arc<SurfaceNode>` — the same type as any
other surface expression. The `Pattern` enum was deleted in S-944 (T-1750);
all pattern dispatch now operates on `SurfaceExpression` variants directly.

The `flatten_dot_access_to_tag` function extracts a qualified tag string from
a dot-access chain (`Color.Red` → `"Color.Red"`). Used by both the Field and
Call arms of `match_pattern`.

### Resolver (`src/resolve.rs`)

`walk_surface_node` walks `arm.pattern` for each match arm. For `VarRef`
nodes in pattern position, `collect_varrefs_in_node` includes them in the
lost-binding lint analysis.

The resolver sets each `VarRef.resolution` OnceLock before evaluation:
- `Some(Some((level, slot)))` — variable found at that de Bruijn coordinate
- `Some(None)` — variable not in scope; resolver emits a diagnostic

### Evaluator (`src/eval.rs`)

`match_pattern(pattern, value, env, value_span, env_id, ctx)` dispatches on
`SurfaceExpression`:

| Pattern form | AST node | Behavior |
|---|---|---|
| `...:` | `Placeholder(None, None)` | Always matches |
| `varname:` | `VarRef { resolution }` | Pin: compare scrutinee to scope value |
| `42:` / `"s":` / `3.14:` | `Int` / `StringLiteral` / `Float` | Literal equality |
| `Tag.Ctor:` | `Field` | Tag match: check `tycon.ctor == tag` |
| `[Tag ...]:` | `Call(args=[Placeholder])` | Tag match + ignore payload |
| `[Tag sub]:` | `Call(args=[node])` | Tag match + recurse into payload |
| `[k: sub]:` | `Dict` | Field presence + recurse into values |
| Other | — | Runtime error: not a valid pattern |

`eval_case_arm_structural_pattern` handles the `CaseArm` path, which
introduces bindings into the arm environment before evaluating the body.

### Coverage (`src/coverage.rs`)

`ast_pattern_to_coverage` converts `Arc<SurfaceNode>` to coverage lattice
elements:

- `Placeholder(None, None)` → `Wildcard` (exhaustive)
- `VarRef` with `Some(Some(...))` (resolved pin) → `Constructor` (non-exhaustive, specific tag)
- `VarRef` with `Some(None)` (unresolvable) → `Wildcard` (conservative: may match anything)
- `Field` → `Constructor` for the specific tag
- `Call` → `Constructor` for the tag (payload ignored for coverage)
- `Int` / `StringLiteral` / `Float` → `Literal`
- `Dict` → structural dict coverage

### Type Checker (`src/typecheck.rs`, `src/typecheck_match.rs`)

`infer_match()` infers the scrutinee type, narrows it per-arm based on the
pattern, infers each arm body under the narrowed environment, and joins arm
result types.

**Narrowing:** Type-tag arms narrow statically. Dict patterns `[ok: v]:` narrow
to `[ok: ...]`. Literal patterns narrow to the literal type. The type checker
applies narrowing constraints directly from the `SurfaceExpression` pattern
without desugaring to `if`/predicate chains.

**Exhaustiveness:** When the scrutinee's type is a `Type::Union`, the type
checker performs Maranget-style coverage analysis on the arm patterns.
`...:` (Placeholder/Wildcard) covers all remaining variants. Arms after a
wildcard are flagged as unreachable.

## References

**Pattern matching compilation:**

- Augustsson, L. (1985). "Compiling pattern matching." In *FPCA '85*,
  LNCS 201, pp. 368–381. Springer.
- Maranget, L. (2008). "Compiling pattern matching to good decision
  trees." In *ML '08*, pp. 35–46. ACM.
- Karachalias, G., Schrijvers, T., Vytiniotis, D. & Peyton Jones, S.
  (2015). "GADTs meet their match." In *ICFP '15*, pp. 424–436. ACM.
- Scott, K. & Ramsey, N. (2000). "When do match-compilation heuristics
  matter?" Technical Report CS-2000-13, University of Virginia.
- Peyton Jones, S.L. (1987). *The Implementation of Functional
  Programming Languages.* Prentice Hall. Chapter 5.

**Pattern matching and laziness:**

- Wadler, P. (1987). "Views: a way for pattern matching to cohabit with
  data abstraction." In *POPL '87*, pp. 307–313. ACM.

**Comparable language designs:**

- Nickel v1.5/1.7 changelogs (2024). Record/enum patterns, wildcards,
  guards, or-patterns.
- Elixir: Pattern matching as core language feature. `case`, function
  heads, guards, pin operator.
- Nix manual §5.1: Function argument set patterns.
