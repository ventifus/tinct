# What If: Pattern Matching for tinct

**State:** Accepted — 2026-05-05

What would it take to add pattern matching to tinct's bracket-based lazy
functional language?

## Current State

tinct has no pattern matching construct. Type-based dispatch requires
verbose `type-of` + string comparison chains:

```tinct
[if [= [type-of x] "Dict"]
    [handle-dict x]
    [if [= [type-of x] "Seq"]
        [handle-seq x]
        [error "unexpected type"]]]
```

The available dispatch mechanisms are:

- **`type-of`** returns a string (`"Int"`, `"Float"`, `"Dict"`, `"Seq"`, etc.)
- **`seq?`** is the only type predicate builtin (returns Bool)
- **`if` + `=` chains** are the only dispatch mechanism

### What's Missing

1. **Destructuring bind** --- no way to extract dict fields or seq
   head/tail in a binding position
2. **Multi-branch dispatch** --- type dispatch requires nested
   `if`/`cond` chains, not flat arm lists
3. **Exhaustiveness checking** --- no static or runtime guarantee that
   all cases are covered
4. **Type predicates** --- only `seq?` exists; no `dict?`, `int?`,
   `str?`, etc.

This matters for three reasons:

1. **Self-hosting builtins.** Dual-dispatch builtins (`map`, `filter`,
   `reduce`, `join`, `take`, `drop`) are in Rust primarily because they
   need to branch on `Value::Dict` vs `Value::Seq`. With pattern matching +
   more granular primitives, tinct could define the dispatch wrapper in tinct
   and keep only the type-specific implementations in Rust. The macros whatif
   (`doc/whatif/macros.md`) identifies pattern matching as "the biggest
   enabler for self-hosting more Rust builtins."

2. **User code.** Any polymorphic data processing — handling different
   shapes, optional fields, tagged unions, `try` results — is awkward
   without matching.

3. **Error handling.** `try` returns `[ok: value]` or `[err: message]`.
   Dispatching on the result key without destructuring is clunky:

   ```tinct
   result: [try risky]
   [if [has? result "ok"]
       [handle-ok result.ok]
       [handle-err result.err]]
   ```

## What Pattern Matching Would Provide

- **Type dispatch** without `type-of` + string comparison chains
- **Destructuring bind** — extract dict fields, seq head/tail, tagged
  union payloads in one expression
- **Multi-branch dispatch** — replace nested `if`/`cond` chains with a
  flat list of pattern → expression arms
- **Self-hosting path** — tinct-level wrappers around Rust primitives,
  reducing the Rust builtin surface
- **Expressiveness** — tinct gains a core construct that most functional
  languages consider essential

## Survey: Pattern Matching in Comparable Languages

| Language | Has PM? | Syntax | Patterns | Exhaustiveness | Lazy? |
|----------|---------|--------|----------|----------------|-------|
| **Nickel** | Full | `match { pat => expr }` | Literals, wildcards, records, arrays, enums, or-patterns, guards | Runtime (MatchError) | Lazy |
| **Nix** | Arg only | `{ x, y, ... }: body` | Set destructure with defaults, `@`-binding | None | Lazy |
| **Dhall** | `merge` | `merge { H1 = ..., H2 = ... } union` | Union elimination only | Compile-time | Strict/Total |
| **Jsonnet** | None | N/A | N/A | N/A | Lazy |
| **jq** | None | Compositional filters | `select()`, `//`, `try-catch` | N/A | Streaming |
| **Elixir** | Full | `case expr do pat -> body end` | Literals, vars, tuples, lists, maps, pin, guards | Runtime (CaseClauseError) | Strict |

### Key Findings

1. **Lazy config languages mostly avoid pattern matching.** Nickel is the
   only lazy config language with full pattern matching. Nix has operated
   for 30+ years with only function argument destructuring. Jsonnet uses
   `std.type()` + if-else despite being lazy.

2. **Nickel's design is the closest precedent.** Nickel is lazy, Rust-based,
   and bracket-adjacent. Its `match` expression evaluates to a function that
   must be applied: `match { ... } value`. Patterns include records, enums,
   literals, wildcards, or-patterns, and guards.

3. **Nickel evolved pattern matching across 3 releases:** v1.5 introduced
   record + enum patterns (11 PRs). v1.7 added wildcards, constants, guards,
   arrays, and or-patterns.

4. **Dhall's `merge` is specialized but provides compile-time
   exhaustiveness.** It only works for union elimination — not general
   pattern matching. But the exhaustiveness guarantee is valuable.

5. **jq demonstrates that compositional alternatives work.** `select()`,
   `//` (alternative), and `try-catch` compose to handle most dispatch cases
   without pattern matching syntax. tinct's `cond` + `try-or` offer a
   similar, if less ergonomic, composition.

## Design

`[match]` is implemented as a first-class `Expr::Match` AST node — a
parser special form with dedicated type checker and evaluator support.
Arms use dict syntax — the pattern spec is the key, the body is the
value:

```tinct
[match expr
    pattern1-key: body1
    pattern2-key: body2
    ...:            default]
```

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
    ...:    x]            # wildcard — always matches

# Variable binding — lowercase bare word binds the scrutinee
[match x
    n:  [+ n 1]]        # n is bound to x's value; rarely useful but complete

# Type + binding — annotated bare word: bind AND type-constrain
[match x
    n@Integer:  [+ n 1]
    n@String:  i"got: $n"
    ...:      x]

# Literal patterns — literals match by equality
[match x
    42:      the-answer
    true:    yes
    "hello": greeting
    ...:       other]

# Guards via `is:` annotation — predicate must return true
[match x
    n@[is: [> _ 0]]:   "positive"
    n@[is: [< _ 0]]:   "negative"
    ...:                  "zero"]

# Type + guard combined
[match x
    n@[type: Int  is: [> _ 0]]:   i"positive int: $n"
    n@Integer:                         i"non-positive int: $n"
    ...:                             "not an int"]

# Dict patterns — dict literal as key, destructures by key
[match result
    [ok: v]:    v
    [err: msg]: [error msg]
    ...:          [error "unexpected"]]

# Nested patterns
[match event
    [type: "click"  target: [id: id]]:  [handle-click id]
    [type: "hover"  target: [id: id]]:  [handle-hover id]
    ...:                                   "ignored"]

# Seq patterns — [seq h t] in key position
[match xs
    [seq h t]:  [process h t]
    ...:          "empty"]

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

The pattern language is amenable to exhaustiveness analysis as the type
system matures. As a first-class `Expr::Match` AST node, the type checker
can narrow types per-arm, check exhaustiveness against inferred union
types, and compute precise result types.

### Keyword Choice: `match` vs `case`

Both are common. `match` is used by Rust, Nickel, Scala, F#, OCaml.
`case` is used by Haskell, Elixir, Erlang. `match` reads more naturally
in tinct's bracket syntax: `[match x ...]` vs `[case x ...]`.

### Pattern Variable Syntax

In tinct, `x` is a variable reference (lookup). In patterns, `x` means
"bind the matched value to `x`." This dual meaning follows Elixir's
precedent (variables in patterns bind, not match). The alternative --- a new
sigil for pattern bindings --- adds complexity without proportional benefit.

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

`[match]` bodies can use `_` normally --- the `_` desugaring pass runs
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
than via `[defmacro match]`. This was decided after evaluating both
approaches against the type system requirements:

1. **Type checker integration.** As a first-class AST node, the type
   checker's `infer_match()` can narrow the scrutinee type per-arm,
   check exhaustiveness against inferred union types, and compute a
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

## What Would Change

### Parser (`src/parser.rs`)

**Current:** Keywords `call`, `fn`, `type` are recognized as special forms.

**Proposed:** Add `match` to the keyword list alongside `call`, `fn`,
`type`. The parser enters a pattern-parsing mode for match arms:
bare names as bindings, capitalized words as type tags, literals as
literal patterns. Arms are parsed as pattern-body pairs.

**Impact:** Moderate — new keyword, new parsing mode for arm patterns.

### AST (`src/ast.rs`)

**New variants.** `Expr::Match` with `MatchArm` and `Pattern` types.
`Pattern` covers type tags, literals, wildcards, variable bindings,
dict/seq destructuring, or-patterns, and guards. Every exhaustive match
on `Expr` gains one arm (~20 sites).

### Evaluator (`src/eval.rs`)

**Proposed:** `eval_match()` materializes the scrutinee, tries arms
top-to-bottom. Each arm's pattern is matched against the scrutinee value.
First matching arm's body is evaluated. No match → runtime error.

### Type Checker (`src/typecheck.rs`)

**Proposed:** `infer_match()` infers the scrutinee type, narrows it
per-arm based on the pattern, infers each arm body under the narrowed
environment, and joins arm result types. If the scrutinee is a union
type, calls the coverage algorithm. Pattern types:

- `VarRef("_")` → wildcard: always matches
- `VarRef("n")` (lowercase) → variable binding: always matches, bind `n`
- `VarRef("Int")` (uppercase) → type pattern: `[int? scrutinee]`
- `Int(42)` (literal) → literal match: `[= scrutinee 42]`
- `Dict([ok: VarRef("v")])` → dict pattern: `[and [dict? s] [has? s "ok"]]`, bind `v`
- `Annotated("n", Simple("Int"))` → type-constrained binding
- `Annotated("n", PropertyDict([is: pred]))` → guard: call `pred` with bound value
- `Pipe(p1, p2)` → or-pattern: try both sub-patterns

**Impact:** New `infer_match()` function in `src/typecheck.rs`.

### Lazy Evaluation

The evaluator materializes the scrutinee then tries arms top-to-bottom.
Dict pattern matching forces only matched keys — implemented via `has?`
and dot access internally. Seq pattern matching uses `head` and `tail`,
which force the head and leave the tail as a thunk. Only the matching
arm's body evaluates. No new forcing semantics.

### Type Checker — Narrowing

`infer_match()` narrows the scrutinee type per-arm directly:

**Type-predicate arms narrow statically.** `n@Integer:` narrows `n` to `Int`
in the arm body. Similarly `n@Str:` narrows to `Str`, dict patterns
`[ok: v]:` narrow to `[ok: ...]`, etc. The type checker applies the
narrowing constraint from the pattern directly — no desugaring to
`if`/`int?` chains required.

**`is:` predicate arms do NOT narrow the type.** `n@[is: [> _ 0]]:` proves a runtime
condition (`n > 0`) but the type checker cannot derive a static type from an arbitrary
`Fn@Boolean [Any]` predicate. `n` retains whatever type the scrutinee had — `Int` if the
scrutinee was typed `Int`, `Any` if it was untyped. This is correct behavior: `is:`
guards are value constraints, not type constraints.

The distinction matters for arm body type safety: after `n@Integer:` the type checker
knows `n` is an `Int` and can reject `n.field` as a type error; after `n@[is: [> _ 0]]:`,
it cannot. Users who need both should compose: `n@[type: Int  is: [> _ 0]]:` gives
type narrowing AND the value guard.

### Interaction with `access-pipeline` (`|` operator)

`access-pipeline` (see `doc/whatif/access-pipeline.md`) lands **before** pattern matching.
It adds `|` as an infix reverse-apply operator and generator-aware flatMap. This
shapes four aspects of pattern matching design:

#### 1. `| [match _]` — idiomatic per-element dispatch

With `|` and generators in place, matching over a collection becomes a first-class
pipeline idiom. The `_` scrutinee triggers WRAP-CALL desugaring → the `[match _]`
form becomes a single-argument function that `|` flatMaps over a Seq:

```tinct
$events | [each] | [match _
    [type: "click"  target: t]:  [handle-click t]
    [type: "hover"  target: t]:  [handle-hover t]
    ...:                           "ignored"]
| [collect]
```

This falls out naturally from both designs — no new mechanism needed. The `_` scrutinee
rule (documented in §Interaction with `_` Desugaring above) already covers it.

#### 2. Guards compose with `|` pipelines

`is:` predicates can themselves use `|` pipelines for readable predicate expressions:

```tinct
[match user
    [role: r]@[is: [r | [= _ "admin"]]]:  [admin-panel user]
    [role: r]:                              [user-panel user]]
```

No design change needed; this is a documentation and example concern.

#### 3. Integer-key dict patterns

`access-pipeline` extends dot access to integer keys (`$a.0`). For consistency,
dict patterns should also support integer-key fields. The current `Dict` pattern
uses `String` for field names; after `access-pipeline` lands, this becomes `Key`:

```rust
// Before: only string keys in patterns
Dict { fields: Vec<(String, Spanned<Pattern>)>, rest: bool }

// After: string or integer keys
Dict { fields: Vec<(Key, Spanned<Pattern>)>, rest: bool }
```

This enables matching on lists (integer-keyed dicts) by position:

```tinct
[match pair
    [0: a  1: b]:  [use a b]
    ...:             [error "expected pair"]]
```

**Implementation note:** wait for `access-pipeline` to land first (it adds
`Key::Int` dot lookup to the evaluator), then extend the pattern parser to
accept `Token::Int` in the `key:` position of a dict pattern.

#### 4. Path-key patterns — DRY deep structure matching

Dict patterns for deeply nested data quickly become repetitive. Path-key
syntax lets the key position in a dict pattern use a dotted path, desugaring
to an equivalent nested dict:

```text
[a.b.c: v]  →  [a: [b: [c: v]]]
```

The value `v` on the right-hand side is any valid pattern — a binding, a
literal, a wildcard, or another nested dict pattern. This gives three
levels of granularity that are all equivalent:

| Style | When to use |
|-------|-------------|
| `cluster.primary.tls.cert: cert` | single deep leaf binding |
| `cluster.primary.tls: [cert: cert  key: key]` | multiple leaves at the same node — DRY |
| `cluster: [primary: [tls: [cert: cert  key: key]]]` | when you also want to bind or match at intermediate levels |

```tinct
# Fully spelled out — all three levels of nesting explicit
[match config
    [cluster: [primary: [tls: [cert: cert  key: key]]]]:  [connect-tls cert key]
    ...:                                                     [error "no tls"]]

# Path-key to the subtree node — DRY when the subtree has multiple fields
[match config
    [cluster.primary.tls: [cert: cert  key: key]]:  [connect-tls cert key]
    ...:                                               [error "no tls"]]

# Mixed: path-to-node + path-to-leaf in the same pattern arm
[match config
    [cluster.primary.tls: [cert: cert  key: key]
     cluster.primary.host: h]:                      [connect-tls cert key h]
    ...:                                               [error "no tls"]]
```

**Shared-prefix merging:** When two path-keys share a prefix, they merge
into a single intermediate dict. The mixed example above desugars to:

```tinct
[cluster: [primary: [tls: [cert: cert  key: key]  host: h]]]
```

**Desugar rules:**

- `[a.b.c: v]` → `[a: [b: [c: v]]]` — path on the left, any pattern on the right
- `[a.b.c: v  a.b.d: w]` → `[a: [b: [c: v  d: w]]]` — shared prefix merged
- `[a.0.name: n]` → `[a: [0: [name: n]]]` — integer segments → `Key::Int` (after `access-pipeline` lands)

**Intermediate nodes are always open.** `[cluster.primary.tls: [cert: c]]`
matches any config that *has* `cluster.primary.tls.cert` — extra keys at
any intermediate level are allowed. This is consistent with row
polymorphism. Closed matching at an intermediate level requires spelling
that level out explicitly and applying the closed-match syntax there.

**Relation to `access-pipeline`'s `[path]`:** `[path data "cluster" "primary" "tls" "cert"]`
navigates to a single deep value at runtime. Path-key patterns give the
compile-time structural complement — branching on shape and binding multiple
values simultaneously. They are complementary:

- Use `[path]` when you want **one value** from a known path
- Use a path-key pattern when you want to **branch on shape** and bind **multiple values** at different depths

**Implementation:** Path-key patterns are a pure parser-level desugaring.
The evaluator sees only the expanded nested `Pattern::Dict` form. Wait for
`access-pipeline` to land before supporting integer segments (needs
`Key::Int` in the evaluator).

#### 5. Bracket removal has no impact on pattern syntax

Removing `Token::BracketAccess` and `Token::Range` affects only access
expressions, not pattern expressions. `[key: v]` in a pattern is parsed in
pattern mode — the lexer never confuses it with `$a[key]`. No changes to
pattern parsing are needed when bracket access is removed.

## References

**Pattern matching compilation:**

- Augustsson, L. (1985). "Compiling pattern matching." In *FPCA '85*,
  LNCS 201, pp. 368–381. Springer. — Decision tree compilation for
  pattern matching in lazy functional languages.
- Maranget, L. (2008). "Compiling pattern matching to good decision
  trees." In *ML '08*, pp. 35–46. ACM. — Optimal decision trees for
  pattern compilation.
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
