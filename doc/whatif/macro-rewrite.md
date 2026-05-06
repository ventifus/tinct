# What If: Macro-Rewrite — Desugar and Typing-Cluster as Macros

**State:** Proposal

What would it take to replace tinct's hardcoded desugar pass with
`[defmacro]` definitions, and to land the typing-cluster's user-facing
features as macros rather than new Rust AST variants and evaluator cases?

## Current State

tinct's pre-typecheck pipeline is:

```
parse → desugar_file() → resolve → typecheck → eval
```

`src/desugar.rs` is a hardcoded Rust AST pass that handles two transformations:

1. **`$_` underscore** — wraps expressions containing `_` in lambdas:
   `[+ _ 1]` → `[fn [_] [+ _ 1]]`
2. **`|` pipe** — desugars infix pipe to calls:
   `xs | [map f]` → `[map f xs]`; `xs | collect` → `[collect xs]`

String interpolation (`i"Hello $name"`) is desugared in the **parser** itself —
`desugar_interpolated_string()` in `src/parser.rs` converts the `InterpolatedString`
token to a `[str "Hello " name]` call at parse time.

The typing-cluster plan (`doc/whatif/plans/typing-cluster.md`) currently
schedules these features as Rust changes:

| Sprint | Rust surface area added |
|--------|------------------------|
| `let-binding` (A1) | Parser change to `fn` body; new `Expr::Sequential` or similar |
| `pattern-matching-basic` (A2) | `Expr::Match` + `eval::eval_match()` + `typecheck::check_match()` + `formatter::format_match()` + arms in resolve.rs, desugar.rs |
| `pattern-matching-destructure` (A3) | `Pattern::Dict`, `Pattern::Seq`, evaluator + formatter extensions |
| `adts` (C1) | `[type ...]` multi-entry extension to type checker; `Type::Union` from B1 |

`Expr::Match` alone would add arms to every exhaustive match on `Expr` in the
codebase — approximately 20 sites spanning eval.rs, typecheck.rs, formatter.rs,
resolve.rs, desugar.rs, and ast_dict.rs.

### What's Missing

1. **No user-extensible desugar** — every new piece of syntax requires a Rust
   change to desugar.rs or the parser.
2. **Typing-cluster features balloon the AST** — `Expr::Match`, `Pattern::*`
   and `Expr::Sequential` propagate through the entire codebase.
3. **Tests live in the wrong place** — desugar transformations are tested via
   Rust unit tests, not tinct corpus tests that can be read and understood without
   Rust knowledge.

## Why Macro-Rewrite Matters

**Fewer Rust changes per feature.** A `[defmacro match]` that expands to
nested `if`/`type-of` chains requires zero new AST variants, zero evaluator
cases, zero formatter arms, zero type checker cases. The macro expander handles
all of it. Adding a new pattern type is a tinct code change, not a Rust change.

**Self-documenting desugar.** A `[defmacro let]` in `stdlib/macros.llt` is
readable tinct code with corpus tests. The current `desugar.rs` is Rust that
requires Rust knowledge to understand and modify.

**Typing-cluster lands faster.** Pattern matching and let binding as macros
have no Rust dependency beyond the macro expansion infrastructure already being
built. They can be written the day macros Phase 2 ships.

**Smaller permanent Rust surface.** `Expr::Match` would live forever once
added — every future feature that touches the AST (new eval strategy, new
optimization, LSP features, the tinct-hosted formatter) must handle it. A
macro keeps that complexity in tinct, where it belongs.

## Design

### What Stays in Rust

**`$_` underscore desugaring** — the `is_direct_underscore` predicate and
`wrap_with_lambda` transformation fire on the *container* of `_`, not on `_`
itself. This is an "ambient" rewrite rule (fires everywhere, not on explicit
invocation) that the current `[defmacro]` system cannot express — `[defmacro]`
fires only when users write `[name ...]`. Implementing `_` as a macro requires
macro system extensions (ambient rules or "syntax transformers") beyond Phase 2.
**`$_` stays in `src/desugar.rs` for now**, shrinking to just this one rule once
the other transformations migrate.

**`|` pipe desugar** — the lexer and parser must handle `|` regardless
(`Token::Pipe`, `Expr::Pipe`). The desugar rule is ~20 lines of Rust.
`Expr::Pipe` nodes could be handled by the macro expander (registering a
built-in macro for `Expr::Pipe` → `Call`), but the savings are minimal.
**`|` pipe desugar stays in `src/desugar.rs`** alongside `$_`.

### What Moves to Macros

#### `i"Hello $name"` → `[defmacro tmpl]`

Currently in `src/parser.rs::desugar_interpolated_string()`. The parser still
emits a call node — the macro handles the rest:

```tinct
# The lexer emits IString("Hello $name") as an opaque string token.
# The parser converts it to [tmpl "Hello $name"] (a Call node).
# The macro expands it at compile time:

[defmacro tmpl [template]
  # parse-template walks template.value char by char, splitting on $
  [apply str [parse-template template.value]]]

# parse-template is a tinct stdlib function, not Rust:
parse-template: [fn [s]
  # returns a Seq of string nodes and var nodes
  ...]
```

The `parse-template` logic moves from Rust to tinct — inspectable, testable
with corpus tests, evolvable without recompilation.

#### `[let [bindings] body]` → `[defmacro let]`

Replaces the planned `let-binding.md` parser change entirely. No `Expr::Sequential`,
no parser modification needed.

```tinct
# [let [x 1  y [+ x 1]] body]
# desugars to: [[fn [x] [[fn [y] body] [+ x 1]]] 1]

[defmacro let [bindings body]
  [fold-right
    [fn [pair acc]
      [quote [[fn [[unquote pair.name]] [unquote acc]]
               [unquote pair.value]]]]
    body
    [pair-up bindings]]]
```

Sequential scoping is preserved: `y`'s initializer `[+ x 1]` is inside the
scope where `x` is bound. No new AST variant. No evaluator change. Lazy
semantics of `[fn ...]` application handle the binding naturally.

#### `[match x arms...]` → `[defmacro match]`

Replaces `pattern-matching-basic` (A2) and `pattern-matching-destructure` (A3)
as Rust sprints. `[match]` becomes a macro that desugars to nested `if` chains.

```tinct
[defmacro match [scrutinee ...arms]
  # arms are pairs: [pattern body pattern body ...]
  # expand to: [if (matches? scrutinee pattern1)
  #                (bind-pattern scrutinee pattern1 body1)
  #                [if ...]]
  [expand-match-arms scrutinee [pair-up arms]]]

expand-match-arms: [fn [scrutinee arm-pairs]
  [if [null? arm-pairs]
    [quote [error "match: no arm matched"]]
    [let [arm [head arm-pairs]
          rest [tail arm-pairs]
          pat arm.0
          body arm.1]
      [quote [if [unquote [pattern-test scrutinee pat]]
                 [unquote [pattern-bind scrutinee pat body]]
                 [unquote [expand-match-arms scrutinee rest]]]]]]]
```

Pattern classification — `pattern-test` and `pattern-bind` dispatch on the
pattern node's shape:

| Pattern | `pattern-test` emits | `pattern-bind` emits |
|---------|---------------------|---------------------|
| `Int`, `Str`, `Bool` (uppercase var) | `[int? s]`, `[str? s]`, `[bool? s]` | body unchanged |
| `42`, `"hello"` (literal) | `[= s 42]` | body unchanged |
| `_` (wildcard) | `true` | body unchanged |
| `name` (lowercase var) | `true` | `[[fn [name] body] s]` |
| `[ok: v]` (dict pattern) | `[and [dict? s] [has? s "ok"]]` | `[[fn [v] body] s.ok]` |
| `[seq h t]` (seq pattern) | `[seq? s]` | `[[fn [h t] body] [head s] [tail s]]` |

This replaces sprints A2 and A3 entirely — no `Expr::Match`, no
`eval::eval_match()`, no `format_match()`.

**Type narrowing** — the type checker sees expanded `if`/`type-of`/`int?` chains.
Pattern typing starts as `Any` (same as the planned Rust implementation for Phase A).
When `narrowing.md` lands, the type checker can narrow `x` in each branch of the
expanded `if` — no changes needed to `[defmacro match]` itself.

#### Multi-entry `[type [ok: a] [err: Str]]` — no macro needed

ADT declarations use the existing `[type ...]` form extended to accept multiple
positional type entries. `[type [ok: a] [err: Str]]` expands in the type checker
to `Type::Union(vec![Record({ok: TypeVar("a")}), Record({err: Str})])`. No macro
required, but the implementation is more than "two lines":

1. **Multi-entry body → union:** detect multiple positional entries in `[type ...]`
   and call `resolve_type_expr` on each, wrapping in `Type::Union`.
2. **`Expr::Str` → `Type::StringLiteral`:** add a conditional branch in
   `resolve_type_expr` before the existing `Expr::Str` arm (which routes to
   `resolve_type_name`) so quoted strings produce `Type::StringLiteral(s)`.
3. **TypeScheme storage (critical):** aliases with type variables (`a` in the
   example) must be stored as `TypeScheme { type_vars: ["a"], body: Type::Union(...) }`,
   NOT as a bare `Type::Union` with a free variable. Without scheme wrapping, two
   call sites share `TypeVar("a")` and unify against each other — a soundness bug.

`Type::Union` from sprint B1 is still the hard prerequisite.

### Impact on the Typing-Cluster Plan

Updated sprint status after macro-rewrite:

| Sprint | Before | After |
|--------|--------|-------|
| `let-binding` (A1) | Parser change + desugar | `[defmacro let]` in `stdlib/macros.llt` — no Rust |
| `pattern-matching-basic` (A2) | `Expr::Match` + evaluator | `[defmacro match]` Phase 1 — no Rust |
| `pattern-matching-destructure` (A3) | Pattern variants + evaluator | Extend `[defmacro match]` — no Rust |
| `adts` (C1) | Parser + type alias registration | Multi-entry `[type ...]` — type checker extension (union body + StringLiteral + TypeScheme storage), no macro |
| `tmpl` string interpolation | In `src/parser.rs` | `[defmacro tmpl]` in `stdlib/macros.llt` |

Rust that is **eliminated**:
- `Expr::Match` variant (and all ~20 exhaustive match sites in the codebase)
- `eval::eval_match()`
- `typecheck::check_match()`
- `formatter::format_match()`
- `Pattern::Dict`, `Pattern::Seq` variants and their evaluator cases
- `desugar_interpolated_string()` in `src/parser.rs`

`src/desugar.rs` shrinks to two rules: `$_` and `|` pipe.

### Parse-Stage Macros

When pattern matching Phase 4+ lands (guards `when`, or-patterns `|`), the
post-parse re-interpretation of arm syntax may become ambiguous (e.g.,
`[f x]` in an arm position could be a constructor pattern or a call). Parse-stage
macros — where the macro controls how its body is tokenized and parsed — resolve
this cleanly. This is tracked as a separate research item
(`doc/whatif/parse-stage-macros.md`) and scheduled between regular macros and
the typing-cluster implementation.

## What Would Change

### `src/desugar.rs`

**Current:** Handles `$_` and `|` pipe.

**Proposed:** `$_` and `|` remain. `desugar_file` no longer needs to handle
`Pipe` traversal once pipe moves to the macro expander (optional — pipe desugar
can stay here). Net: file shrinks from ~400 lines to ~150 lines.

### `src/parser.rs`

**Current:** `desugar_interpolated_string()` converts `InterpolatedString` token
to `[str ...]` call at parse time.

**Proposed:** Parser converts `InterpolatedString` to `[tmpl "raw-string"]` call
node. The `[defmacro tmpl]` in `stdlib/macros.llt` handles the expansion.
Parser change: ~20 lines, net simpler.

### `stdlib/macros.llt` (new file)

Standard library macros loaded alongside `stdlib/prelude.llt`:

- `[defmacro tmpl [template] ...]` — string interpolation
- `[defmacro let [bindings body] ...]` — sequential binding
- `[defmacro match [scrutinee ...arms] ...]` — pattern matching
- ADT declarations via multi-entry `[type ...]` — no macro, type checker extension only

Corpus-tested. Readable. Modifiable without recompiling tinct.

### Typing-Cluster Plan (`doc/whatif/plans/typing-cluster.md`)

Sprint tasks for A1, A2, A3, C1 updated to reflect macro-based implementation:
no `Expr::Match` or `Pattern::*` variants, no evaluator cases, no formatter arms.
The spec chapters for these features (`doc/02-syntax.md`, `doc/08-evaluation.md`)
still document the user-facing syntax — the fact that it's macro-implemented is
an implementation detail.

## Phased Adoption

### Phase 1: `[defmacro tmpl]` — String Interpolation

Lowest risk: replace `desugar_interpolated_string()` with a tinct macro. Parser
emits `[tmpl "raw"]`; macro expands to `[str ...]`. Validates the macro
infrastructure on a real existing feature.

- `src/parser.rs`: convert `InterpolatedString` to `[tmpl "..."]` call
- `stdlib/macros.llt`: `[defmacro tmpl ...]` with `parse-template` helper
- Tests: all existing string interpolation corpus tests pass through the macro path

### Phase 2: `[defmacro let]` and `[defmacro match]` Phase 1

- `stdlib/macros.llt`: `[defmacro let]` + `[defmacro match]` (type + literal + wildcard + variable patterns)
- Replaces `let-binding` and `pattern-matching-basic` Rust sprints
- Tests: 10+ corpus tests for `let` sequential scoping; 10+ for `match` basic patterns

### Phase 3: `[defmacro match]` Phase 2 — Structural Patterns

Extend `[defmacro match]` with dict patterns `[ok: v]` and seq patterns `[seq h t]`.

- `stdlib/macros.llt`: extend `pattern-test` and `pattern-bind` dispatch
- Replaces `pattern-matching-destructure` Rust sprint
- Tests: 10+ corpus tests for structural destructuring

### Phase 4: Multi-Entry `[type ...]` for ADTs

- `src/typecheck.rs`: extend `resolve_type_dict`/`[type ...]` handler to union multiple positional entries; add `Expr::Str` → `Type::StringLiteral` to `resolve_type_expr`
- Depends on `Type::Union` (B1) for the union type representation
- Tests: 6+ corpus tests for multi-entry type declarations and string literal type variants

### Prerequisites

- **Macros Phase 2** (`[defmacro]` and expansion loop) — required for all phases
- **Quasiquoting Phase 2** (`[quote]`/`[unquote]`) — strongly recommended for ergonomic macro bodies
- **`ast_to_dict_expr`** (`doc/whatif/ast-schema.md` Phase 1) — required for macro expansion

### Trigger

When macros Phase 2 ships — implement all phases of this proposal before
attempting typing-cluster sprints A1, A2, A3, C1 in Rust.

## References

- tinct `doc/whatif/macros.md` — base macro system; `[defmacro]`, hygiene model, expansion pipeline
- tinct `doc/whatif/quasiquoting.md` — `[quote]`/`[unquote]` for ergonomic macro bodies
- tinct `doc/whatif/ast-schema.md` — AST dict schema used by macro bodies
- tinct `doc/whatif/plans/typing-cluster.md` — sprints affected by this proposal (A1, A2, A3, C1)
- tinct `doc/whatif/parse-stage-macros.md` — future extension for parse-time control over arm syntax
- Flatt, M. (2002). "Composable and compilable macros: you want it when?" *ICFP '02*, pp. 72–83. ACM. — phase separation; macro bodies execute at compile time
- Ballantyne, M., King, A. & Felleisen, M. (2020). "Macros for domain-specific languages." *OOPSLA '20*. — surface-to-core macro architecture; `[match]` as a DSL macro is the canonical example
