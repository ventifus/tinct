# What If: Macro-Rewrite — Desugar and Typing-Cluster as Macros

**State:** Superseded — let-binding implemented as `Expr::Sequential`; match as `Expr::Match`. `i"..."` migrated to `[defmacro tmpl]` (`tmpl-macro` sprint complete).
**Superseded by:** [`macros-v2.md`](macros-v2.md)

What would it take to replace tinct's hardcoded desugar pass with
`[defmacro]` definitions, and to land the typing-cluster's user-facing
features as macros rather than new Rust AST variants and evaluator cases?

## Current State

tinct's pre-typecheck pipeline is:

```text
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

The typing-cluster plan (`doc/whatif/plans/typing-cluster.md`) scheduled
these features as Rust changes; all are now complete (2026-05-07):

| Sprint | Implementation | Status |
|--------|----------------|--------|
| `let-binding` (A1) | Parser change to `fn` body; sequential scoping | ✓ Done |
| `pattern-matching-basic` (A2) | `Expr::Match` + `eval::eval_match()` + `typecheck::infer_match()` | ✓ Done |
| `pattern-matching-destructure` (A3) | `Pattern::Dict`, `Pattern::Seq`, evaluator extensions | ✓ Done |
| `adts` (C1) | `[type ...]` multi-entry; `Type::Union` | ✓ Done |

**Decision:** `Expr::Match` (A2, A3) is implemented as a Rust special form,
not as a macro. The type checker benefits of first-class `Expr::Match` —
per-arm narrowing, exhaustiveness with inferred types, precise union result
types — outweigh the AST surface area cost. Match is excluded from this
proposal's scope. See `doc/whatif/pattern-matching.md` §Why a Special Form.

### What's Missing

1. **No user-extensible desugar** — every new piece of syntax requires a Rust
   change to desugar.rs or the parser.
2. **`i"..."` string interpolation lives in the parser** — `desugar_interpolated_string()`
   is in `src/parser.rs`; moving it to `[defmacro tmpl]` makes it corpus-testable
   and modifiable without recompilation.
3. **Tests live in the wrong place** — desugar transformations are tested via
   Rust unit tests, not tinct corpus tests that can be read and understood without
   Rust knowledge.

## Why Macro-Rewrite Matters

**Fewer Rust changes per feature.** A `[defmacro let]` that expands to
nested `fn` applications requires zero new AST variants, zero evaluator
cases, zero formatter arms, zero type checker cases. The macro expander handles
all of it.

**Self-documenting desugar.** A `[defmacro let]` in `stdlib/macros.llt` is
readable tinct code with corpus tests. The current `desugar.rs` is Rust that
requires Rust knowledge to understand and modify.

**Typing-cluster A1 lands faster.** Let binding as a macro has no Rust
dependency beyond the macro expansion infrastructure already being built.
It can be written the day macros Phase 2 ships.

**Note:** Match (`Expr::Match`) is excluded from macro-rewrite — see
decision note in §Current State. The type checker benefits of first-class
match (per-arm narrowing, exhaustiveness, precise union result types) justify
the AST surface area cost.

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

#### ~~`[match x arms...]` → `[defmacro match]`~~ — Excluded

**Decision:** Match is implemented as `Expr::Match` (a Rust special form), not
as a macro. See the decision note in §Current State above. The type checker
benefits of first-class `Expr::Match` — per-arm narrowing, exhaustiveness
checking with inferred types, precise union result types — outweigh the AST
surface area cost. See `doc/whatif/pattern-matching.md` §Why a Special Form
for the full rationale.

Pattern matching sprints A2 and A3 are implemented via `Expr::Match`,
`eval::eval_match()`, and `typecheck::infer_match()` in Rust, as specified
in `doc/whatif/plans/typing-cluster.md`.

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

All typing-cluster sprints are complete (2026-05-07). They were implemented
as Rust special forms as originally designed — macro-rewrite did not change
those decisions. The remaining work is specific to desugar.rs consolidation
and `i"..."` migration:

| Sprint | Implementation | Status |
|--------|----------------|--------|
| `let-binding` (A1) | Parser change to multi-expr fn bodies | ✓ Done (Rust special form) |
| `pattern-matching-basic` (A2) | `Expr::Match` special form | ✓ Done (Rust special form) |
| `pattern-matching-destructure` (A3) | `Pattern::Dict`, `Pattern::Seq` | ✓ Done (Rust special form) |
| `adts` (C1) | Multi-entry `[type ...]`, `Type::Union` | ✓ Done (type checker extension) |
| `tmpl` string interpolation | In `src/parser.rs` | **Open** — migrate to `[defmacro tmpl]` |

Rust that is **eliminated** by macro-rewrite:

- `desugar_interpolated_string()` in `src/parser.rs`

Rust that **stays** (match as special form, not macro):

- `Expr::Match`, `Pattern::*` variants — implemented in typing-cluster A2/A3
- `eval::eval_match()`, `typecheck::infer_match()`, `formatter::format_match()`

`src/desugar.rs` shrinks to two rules: `$_` and `|` pipe.

### Parse-Stage Macros

Parse-stage macros (`doc/whatif/parse-stage-macros.md`) remain relevant for
future syntax extensions — e.g., custom infix operators, argument-position
parse modes — but are no longer gated on `[match]`. Match arm parsing is
handled by the parser's dedicated pattern-parsing mode as part of the
`Expr::Match` special form (typing-cluster A2).

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
- ADT declarations via multi-entry `[type ...]` — no macro, type checker extension only

Match is excluded — implemented as `Expr::Match` special form (see decision note
in §Current State).

Corpus-tested. Readable. Modifiable without recompiling tinct.

### Typing-Cluster Plan (`doc/whatif/plans/typing-cluster.md`)

Sprints A2 and A3 (pattern matching) are **not** affected by macro-rewrite —
match is implemented as `Expr::Match` (Rust special form). Sprint A1
(let-binding) is a parser change (multi-expression fn bodies), not a macro.
Sprint C1 (ADTs) uses type checker extension, not macros.

## Prerequisites

All prerequisites are met (2026-05-07):

- ~~Macros Phase 2~~ ✓ Complete — `defmacro`, hygiene, expansion loop (`macro-integration` sprint)
- ~~Quasiquoting Phase 2~~ ✓ Complete — `[quote]`/`[unquote]` implemented
- ~~`ast_to_dict_expr`~~ ✓ Complete — AST dict schema (`ast-dict-core` sprint)
- ~~`Type::Union`~~ ✓ Complete — `union-types` sprint

## References

- tinct `doc/whatif/macros.md` — base macro system; `[defmacro]`, hygiene model, expansion pipeline
- tinct `doc/whatif/quasiquoting.md` — `[quote]`/`[unquote]` for ergonomic macro bodies
- tinct `doc/whatif/ast-schema.md` — AST dict schema used by macro bodies
- tinct `doc/whatif/plans/typing-cluster.md` — sprints affected by this proposal (C1)
- tinct `doc/whatif/parse-stage-macros.md` — future extension for parse-time control over arm syntax
- Flatt, M. (2002). "Composable and compilable macros: you want it when?" *ICFP '02*, pp. 72–83. ACM. — phase separation; macro bodies execute at compile time
- Ballantyne, M., King, A. & Felleisen, M. (2020). "Macros for domain-specific languages." *OOPSLA '20*. — surface-to-core macro architecture; `[match]` as a DSL macro is the canonical example
