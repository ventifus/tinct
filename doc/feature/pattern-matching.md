# Pattern Matching

## Overview

`[match x ...]` is a first-class `Expr::Match` AST node with dedicated
type checker and evaluator support. It replaces `type-of` + string
comparison chains with flat arm lists that dispatch on type, literal
value, dict structure, seq structure, or guards. The type checker narrows
the scrutinee type per-arm, checks exhaustiveness against inferred union
types, and computes precise result types. Arms use dict syntax — the
pattern spec is the key, the body is the value.

## Design

`[match]` is implemented as a first-class `Expr::Match` AST node — a
parser special form with dedicated type checker and evaluator support.

**Context-sensitive key identity:** Inside `[match]`, the parser enters a dedicated
match-arm parsing mode where the full annotated expression is the key identity —
`n@Integer` and `n@String` are distinct keys even though both have base name `n`. This
is implemented in the parser directly (not via a syntax class in stdlib). Regular
dicts are unchanged: `[n@Integer: 1  n@String: "2"]` remains a duplicate-key error.

**Pattern syntax** (the key position of each arm):

```tinct
# Type patterns — uppercase bare word is a type tag
[match x
    Int:  [+ x 1]       # matches if x is an Int; x still in scope
    Str:  i"got: $x"    # matches if x is a Str
    _:    x]            # wildcard — always matches

# Variable binding — lowercase bare word binds the scrutinee
[match x
    n:  [+ n 1]]        # n is bound to x's value; rarely useful but complete

# Type + binding — annotated bare word: bind AND type-constrain
[match x
    n@Integer:  [+ n 1]
    n@String:  i"got: $n"
    _:      x]

# **Note:** `n@Integer:` is a compile-time type annotation that narrows the inferred type of `n`
# within the arm body. It does NOT perform a runtime type check — for runtime type checking,
# use `[is: int?]` guard instead.

# Literal patterns — literals match by equality
[match x
    42:      the-answer
    true:    yes
    "hello": greeting
    _:       other]

# Guards via `is:` annotation — predicate must return true
[match x
    n@[is: [> _ 0]]:   "positive"
    n@[is: [< _ 0]]:   "negative"
    _:                  "zero"]

# Type + guard combined
[match x
    n@[type: Int  is: [> _ 0]]:   i"positive int: $n"
    n@Integer:                         i"non-positive int: $n"
    _:                             "not an int"]

# Dict patterns — dict literal as key, destructures by key
[match result
    [ok: v]:    v
    [err: msg]: [error msg]
    _:          [error "unexpected"]]

# Nested patterns
[match event
    [type: "click"  target: [id: id]]:  [handle-click id]
    [type: "hover"  target: [id: id]]:  [handle-hover id]
    _:                                   "ignored"]

# Seq patterns — [seq h t] in key position
[match xs
    [seq h t]:  [process h t]
    _:          "empty"]

# Or-patterns — `|` pipe node as key; both sub-patterns must bind same vars
[match result
    [ok: v] | [success: v]:   v
    [err: msg]:               [error msg]]

# Pin operator — $name matches against existing variable value
[match result
    $expected:  "matched!"
    other:      "no match"]
```

**`is:` predicate semantics:** The `is:` key in an annotation property dict
is a `Fn@Boolean [Any]` predicate. The match macro calls it with the bound value.
`true` = arm fires; `false` = fall through to next arm. Use `_` as the
placeholder: `[> _ 0]` desugars to `[fn [_] [> _ 0]]`. For multiple
predicates use `and` composition: `[is: [and [> _ 5] [< _ 10]]]`. Named
contracts work directly: `n@[is: PortRange]` where `PortRange: [fn [v] ...]`.

The pattern language is amenable to exhaustiveness analysis via `infer_match()`.
The type checker narrows types per-arm, checks exhaustiveness against inferred
union types, and computes precise result types.

### Keyword Choice: `match` vs `case`

`match` is used by Rust, Nickel, Scala, F#, OCaml. `case` is used by
Haskell, Elixir, Erlang. `match` reads more naturally in tinct's bracket
syntax: `[match x ...]` vs `[case x ...]`.

### Pattern Variable Syntax

In tinct, `x` is a variable reference (lookup). In patterns, `x` means
"bind the matched value to `x`." This dual meaning follows Elixir's
precedent (variables in patterns bind, not match). The alternative — a new
sigil for pattern bindings — adds complexity without proportional benefit.

**Pin operator:** Use `$name` to match against the existing value of a
variable (pin), rather than binding a new variable named `name`. This
is consistent with tinct's existing `$` semantics — `$` already marks a
reference to something already named, in both expression and pattern context:

```tinct
[match result
    $expected:  "matched!"   # pin: result must equal current value of `expected`
    other:      "no match"]  # bind: `other` is bound to result's value

[match event
    $start-event:  [handle-start]
    $end-event:    [handle-end]
    other:         [handle-other other]]
```

Bare `name` in a pattern = new binding. `$name` in a pattern = match against
the existing value. `$name` requires `name` to be in scope at the match site —
an undefined `$name` is a compile-time or runtime error, same as `$name` in
an expression.

### Open vs Closed Dict Matching

Dict patterns default to **open matching** (extra keys allowed). This is
consistent with row polymorphism's open records and is more useful for
configuration data where extra fields are common. Closed matching (reject
extra keys) uses explicit syntax (e.g., trailing `|` or `!`).

### Interaction with `_` Desugaring

`[match]` bodies can use `_` normally — the `_` desugaring pass runs
before evaluation, and `match` bodies are ordinary expressions. No special
interaction. The match scrutinee can also be `_`, creating a function:
`[fn [_] [match _ ...]]` via the WRAP-CALL rule.

### Materialization Semantics

Pattern matching on the scrutinee is inherently materializing (like
`type-of`). This means `[match thunk ...]` forces `thunk`. Within
dict patterns, only matched keys are forced. This is documented in
doc/08-evaluation.md Builtin Materialization Behavior as the standard pattern for
builtins that need to inspect value structure.

### Why a Special Form (Not a Macro)

`[match]` is implemented as an `Expr::Match` parser special form rather
than via `[macro match ...]`. This was decided after evaluating both
approaches against the type system requirements:

1. **Type checker integration.** As a first-class AST node, the type
   checker's `infer_match()` narrows the scrutinee type per-arm,
   checks exhaustiveness against inferred union types, and computes a
   precise union result type `τ₁ | τ₂ | τ₃` — none of which are
   possible when match desugars to `if` chains before the type checker
   runs. See Tobin-Hochstadt & Felleisen (2010) for the theoretical
   basis of per-arm narrowing.

2. **No fragile coupling.** A macro must produce `if`/`int?` chains
   that the narrowing extractor (`extract_narrowings()`) pattern-matches
   against. Two independently maintained components must agree on AST
   shapes with no enforced contract. With `Expr::Match`, the type checker
   handles patterns directly — no reverse-engineering.

3. **Exhaustiveness with inferred types.** The type checker has access to
   inferred scrutinee types, enabling automatic coverage checking without
   requiring explicit `[@Type ...]` annotations. A macro runs before the
   type checker and can only check coverage against declared type aliases.

4. **Not match-as-function.** Nickel's match-as-function interacts poorly
   with tinct's `_` desugaring — both introduce implicit parameters. The
   `[match scrutinee ...]` form with an explicit scrutinee avoids this
   conflict and is clearer to read.

The trade-off is AST surface area: `Expr::Match` adds one arm to every
exhaustive match on `Expr` in the codebase (~20 sites). This is a
one-time cost that pays for itself through type checker clarity.

## Implementation

### Parser (`src/parser.rs`)

`match` is a keyword alongside `call`, `fn`, `type`. The parser enters a
pattern-parsing mode for match arms: bare names as bindings, capitalized
words as type tags, literals as literal patterns. Arms are parsed as
pattern-body pairs.

### AST (`src/ast.rs`)

`Expr::Match` with `MatchArm` and `Pattern` types. `Pattern` covers type
tags, literals, wildcards, variable bindings, dict/seq destructuring,
or-patterns, and guards. Every exhaustive match on `Expr` gains one arm
(~20 sites).

### Evaluator (`src/eval.rs`)

The `CoreExpr::Match` arm in `eval_core_expr` materializes the scrutinee, tries arms
top-to-bottom. Each arm's pattern is matched against the scrutinee value via
`match_pattern`. First matching arm's body is evaluated. No match → runtime error.

### Type Checker (`src/typecheck.rs`)

`infer_match()` infers the scrutinee type, narrows it per-arm based on the
pattern, infers each arm body under the narrowed environment, and joins arm
result types. If the scrutinee is a union type, calls the coverage algorithm.
Pattern types:

- `VarRef("_")` → wildcard: always matches
- `VarRef("n")` (lowercase) → variable binding: always matches, bind `n`
- `VarRef("Int")` (uppercase) → type pattern: `[int? scrutinee]`
- `Int(42)` (literal) → literal match: `[= scrutinee 42]`
- `Dict([ok: VarRef("v")])` → dict pattern: `[and [dict? s] [has? "ok" s]]`, bind `v`
- `Annotated("n", Simple("Int"))` → type-constrained binding
- `Annotated("n", PropertyDict([is: pred]))` → guard: call `pred` with bound value
- `Pipe(p1, p2)` → or-pattern: try both sub-patterns

**Narrowing:** `infer_match()` narrows the scrutinee type per-arm directly:

Type-predicate arms narrow statically. `n@Integer:` narrows `n` to `Int`
in the arm body. Similarly `n@Str:` narrows to `Str`, dict patterns
`[ok: v]:` narrow to `[ok: ...]`, etc. The type checker applies the
narrowing constraint from the pattern directly — no desugaring to
`if`/`int?` chains required.

`is:` predicate arms do NOT narrow the type. `n@[is: [> _ 0]]:` proves a runtime
condition (`n > 0`) but the type checker cannot derive a static type from an arbitrary
`Fn@Boolean [Any]` predicate. `n` retains whatever type the scrutinee had — `Int` if the
scrutinee was typed `Int`, `Any` if it was untyped. This is correct behavior: `is:`
guards are value constraints, not type constraints.

The distinction matters for arm body type safety: after `n@Integer:` the type checker
knows `n` is an `Int` and can reject `n.field` as a type error; after `n@[is: [> _ 0]]:`,
it cannot. Users who need both should compose: `n@[type: Int  is: [> _ 0]]:` gives
type narrowing AND the value guard.

### Exhaustiveness

Exhaustiveness is checked in `infer_match()` when the scrutinee's type is a
`Type::Union`. The type checker extracts the variant set from the scrutinee's
union type and performs Maranget-style coverage analysis on the arm patterns:

- Type-tag arms (`n@Integer:`) cover the `Int` variant
- Dict pattern arms (`[ok: v]:`) cover the `[ok: a]` structural variant
- Wildcard `_:` covers all remaining variants
- Or-pattern arms (`p1 | p2:`) cover both sub-patterns
- `is:` predicate arms are **opaque** — they do not contribute to coverage
  (the guard is a runtime condition, not a type constraint)

Without a union-typed scrutinee, no coverage analysis is performed —
the match is dynamically correct but statically unverified. A runtime
`MatchError` fires if no arm matches.

- **Unreachable arms:** an arm after a wildcard `_:` is flagged as a type warning.
- **`is:` arms and coverage:** **all guarded arms are fully opaque** to exhaustiveness
  analysis. Arms with `is:` predicates — including combined `type:` + `is:` arms like
  `n@[type: Int  is: [> _ 0]]:` — are excluded from coverage entirely, regardless of
  any type constraint they carry. The `Int` type annotation in this example does **not**
  contribute to coverage; the arm is treated as if it were `n@[is: [> _ 0]]:` with no
  type constraint at all. This matches Karachalias et al. (2015) lazy bottom semantics:
  guards are opaque runtime predicates whose truthfulness cannot be statically determined,
  so the exhaustiveness checker conservatively assumes the arm might not match even when
  its type constraint is satisfied. An unguarded `n@Integer:` arm or wildcard `_:` arm is
  still required to satisfy exhaustiveness for the `Int` variant.

### Lazy Evaluation

The evaluator materializes the scrutinee then tries arms top-to-bottom.
Dict pattern matching forces only matched keys — implemented via `has?`
and dot access internally. Seq pattern matching uses `head` and `tail`,
which force the head and leave the tail as a thunk. Only the matching
arm's body evaluates. No new forcing semantics.

## References

**Pattern matching compilation:**

- Augustsson, L. (1985). "Compiling pattern matching." In *FPCA '85*,
  LNCS 201, pp. 368–381. Springer. — Decision tree compilation for
  pattern matching in lazy functional languages.
- Maranget, L. (2008). "Compiling pattern matching to good decision
  trees." In *ML '08*, pp. 35–46. ACM. — Optimal decision trees for
  pattern compilation. Directly applicable to Phase 2+ compilation.
- Karachalias, G., Schrijvers, T., Vytiniotis, D. & Peyton Jones, S.
  (2015). "GADTs meet their match: pattern-matching warnings that
  account for GADTs, guards, and laziness." In *ICFP '15*, pp. 424–436.
  ACM. — Extends exhaustiveness and redundancy checking to handle guards
  (treated as opaque/irrefutable for coverage), laziness (divergent
  scrutinees), and GADTs (type refinement in arms). The guard opacity
  result directly applies to tinct's `is:` predicate arms: guards do not
  contribute to coverage analysis.
- Scott, K. & Ramsey, N. (2000). "When do match-compilation heuristics
  matter?" Technical Report CS-2000-13, University of Virginia. —
  Empirical comparison of match compilation strategies; shows simple
  heuristics suffice in practice.
- Peyton Jones, S.L. (1987). *The Implementation of Functional
  Programming Languages.* Prentice Hall. Chapter 5: pattern matching
  compilation strategies for lazy languages.

**Pattern matching and laziness:**

- Wadler, P. (1987). "Views: a way for pattern matching to cohabit with
  data abstraction." In *POPL '87*, pp. 307–313. ACM. — Pattern matching
  over abstract types via views. Relevant to tinct's dict/seq dispatch
  where the underlying representation may differ from the pattern surface.

**Nickel pattern matching:**

- Nickel v1.5 changelog (2024). Introduction of match expressions with
  record and enum patterns.
- Nickel v1.7 changelog (2024). Extension with wildcards, constants,
  guards, array patterns, and or-patterns.

**Union elimination (Dhall model):**

- Christiansen, D.R. (2013). "Bidirectional typing rules: a tutorial."
  — Checking mode for eliminators ensures exhaustiveness.

**Comparable language designs:**

- Nix manual §5.1: Function argument set patterns (`{ x, y, ... }: body`).
  No general pattern matching after 30+ years.
- Jsonnet spec: No pattern matching. Type dispatch via `std.type()`.
- jq manual: Compositional filters (`select()`, `//`, `try-catch`) as
  pattern matching alternative.
- Elixir: Pattern matching as core language feature. `case`, function
  heads, guards, pin operator.
