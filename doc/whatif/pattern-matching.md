# What If: Pattern Matching for tinct

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
   arrays, and or-patterns. Each release was a working system. This phased
   approach is directly applicable to tinct.

4. **Dhall's `merge` is specialized but provides compile-time
   exhaustiveness.** It only works for union elimination — not general
   pattern matching. But the exhaustiveness guarantee is valuable.

5. **jq demonstrates that compositional alternatives work.** `select()`,
   `//` (alternative), and `try-catch` compose to handle most dispatch cases
   without pattern matching syntax. tinct's `cond` + `try-or` offer a
   similar, if less ergonomic, composition.

## Design

A new keyword `match` parsed as a special form, like `call` and `fn`:

```tinct
[match expr
    [pattern1] body1
    [pattern2] body2
    _          default]
```

Each arm is a pattern followed by a body expression. Patterns are parsed
inside `[]` brackets using tinct's existing syntax. The match expression
evaluates to the body of the first matching arm.

**Pattern syntax** (using existing tinct constructs where possible):

```tinct
# Type patterns — bare words match type-of result
[match x
    Int    [+ x 1]            # matches if type-of is "Int"
    Str    [str "got: " x]    # matches if type-of is "Str"
    _      x]                 # wildcard — always matches

# Literal patterns — literals match by equality
[match x
    42     the-answer
    true   yes
    "hello" greeting
    _      other]

# Dict patterns — destructure by key
[match result
    [ok: v]    v                   # binds v to the value at key "ok"
    [err: msg] [error msg]         # binds msg to the value at key "err"
    _          [error "unexpected"]]

# Seq patterns — head/tail destructure
[match xs
    [seq h t] [process h t]  # binds head and tail
    []        "empty"              # empty collection
    _         [error "expected seq"]]

# Nested patterns
[match event
    [type: "click"  target: [id: id]]  [handle-click id]
    [type: "hover"  target: [id: id]]  [handle-hover id]
    _                                   "ignored"]

# Guards
[match x
    n when [> n 0]  "positive"
    n when [< n 0]  "negative"
    _               "zero"]
```

This design reuses tinct's `[]` brackets naturally for patterns, making
destructuring bindings obvious (`v` in a pattern binds). Guards use existing
tinct expressions. The pattern language is amenable to exhaustiveness analysis
as the type system matures. The trade-off is a new keyword in the grammar
(`match` added to the denylist), a new AST variant (`Expr::Match`), and
implementation work spanning parser, evaluator, and type checker.

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
    $expected  "matched!"   # pin: result must equal current value of `expected`
    other      "no match"]  # bind: `other` is bound to result's value

[match event
    $start-event  [handle-start]
    $end-event    [handle-end]
    other         [handle-other other]]
```

Bare `name` in a pattern = new binding. `$name` in a pattern = match against
the existing value. `$name` requires `name` to be in scope at the match site —
an undefined `$name` is a compile-time or runtime error, same as `$name` in
an expression. Ships with Phase 2 at no extra syntactic cost.

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

### Why a Special Form

A special form is chosen over alternatives for three reasons:

1. **Self-hosting requires destructuring.** The primary motivation is
   replacing Rust-side `match` on `Value::Dict` vs `Value::Seq` with
   tinct-level dispatch. A builtin function or `cond` enhancement cannot
   support structural destructuring --- extracting dict fields and seq
   structure in the tinct-level wrapper requires pattern syntax.

2. **tinct's bracket syntax accommodates `match` naturally.** Unlike
   s-expression languages where `match` is just another list form, tinct's
   keywords (`call`, `fn`, `type`) are parsed specially. `match` fits the
   same pattern. Patterns inside `[]` reuse existing syntax: bare words are
   type tags, a bare name is a binding, literals match by value.

3. **Not match-as-function.** Nickel's match-as-function interacts poorly
   with tinct's `_` desugaring --- both introduce implicit parameters.
   A `[match scrutinee ...]` form with an explicit scrutinee avoids this
   conflict and is clearer to read.

## What Would Change

### Parser / Grammar

**Current:** Keywords `call`, `fn`, `type` are recognized as special forms.
No pattern parsing mode exists.

**Proposed:** Add `match` to the keyword denylist and special form
recognition. The parser recognizes `[match ...]` and produces
`Expr::Match` nodes. Pattern parsing reuses existing expression parsing
with a "pattern mode" that interprets bare names as bindings (not lookups)
and capitalized words as type tags.

**Impact:** Moderate. One new keyword, one new parsing mode (pattern vs
expression). Pattern mode is structurally similar to existing expression
parsing but with different semantic interpretation of bare names and
capitalized words.

### AST

**Current:** `Expr` enum has no match or pattern variants.

**Proposed:** Add `Expr::Match`, `MatchArm`, `Pattern`, and
`LiteralPattern` types.

**Impact:** Major. Two new enums (`Pattern`, `LiteralPattern`), one new
struct (`MatchArm`), one new `Expr` variant. Every pass that exhaustively
matches `Expr` (typechecker, evaluator, desugar, formatter) must handle
the new variant.

```rust
pub enum Expr {
    // ... existing variants ...

    /// Pattern match expression: [match expr arm1 arm2 ...]
    Match {
        scrutinee: Box<Spanned<Expr>>,
        arms: Vec<MatchArm>,
    },
}

pub struct MatchArm {
    pub pattern: Spanned<Pattern>,
    pub guard: Option<Box<Spanned<Expr>>>,
    pub body: Box<Spanned<Expr>>,
}

pub enum Pattern {
    /// Wildcard: _
    Wildcard,
    /// Variable binding: x (binds the matched value to x)
    Var(String),
    /// Literal: 42, true, "hello"
    Literal(LiteralPattern),
    /// Type tag: Int, Str, Dict, Seq, etc.
    Type(String),
    /// Dict destructure: [key1: v1  key2: v2]
    Dict {
        fields: Vec<(String, Spanned<Pattern>)>,
        rest: bool,  // true if ... present (open match)
    },
    /// Seq destructure: [seq head tail]
    Seq {
        head: Box<Spanned<Pattern>>,
        tail: Box<Spanned<Pattern>>,
    },
    /// Or-pattern: pat1 | pat2 (both must bind same variables)
    Or(Vec<Spanned<Pattern>>),
}

pub enum LiteralPattern {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),
}
```

### Evaluator

**Current:** No pattern matching logic. Type dispatch requires
`type-of` + `=` comparisons via builtins.

**Proposed:** Implement pattern matching as a new evaluation rule with
the following semantics.

**Impact:** Major. New evaluation rule with recursive pattern matching,
environment extension for bindings, and guard evaluation. Interacts with
thunk forcing (scrutinee materialization) and lazy dict/seq access.

Pattern matching in the evaluator:

1. **Evaluate the scrutinee** — fully materialize to determine type and
   structure. This is inherently materializing (like `type-of`).

2. **Try each arm top-to-bottom** — for each arm, attempt to match the
   pattern against the materialized value:
   - `Wildcard` / `Var` — always match. `Var` binds the value.
   - `Literal` — compare by equality.
   - `Type` — compare `type-of` result.
   - `Dict` — check that all specified keys exist, recursively match
     sub-patterns on their values. If `rest: false`, no extra keys allowed.
   - `Seq` — match head and tail.
   - `Or` — try sub-patterns left-to-right, succeed on first match.

3. **Check guard** (if present) — evaluate the guard expression in an
   environment extended with pattern bindings. If it evaluates to `true`,
   the arm matches.

4. **Evaluate body** — in the environment extended with pattern bindings.

5. **No match** — if no arm matches, raise a runtime error (MatchError).

### Lazy Evaluation

**Current:** Thunks are forced by builtins that inspect values (e.g.,
`type-of`). No lazy pattern-driven forcing exists.

**Proposed:** Pattern matching on the scrutinee is inherently materializing
(same semantics as `type-of`). Dict pattern matching forces matched keys
only --- when matching `[ok: v]` against a dict, only the key `"ok"` is
accessed; other keys remain as thunks. Seq pattern matching forces the head
and binds the tail thunk without forcing it, consistent with `head` and
`tail` builtin behavior. Only the matching arm's body is evaluated ---
other arms' bodies are never entered.

**Impact:** Minor. Follows existing materialization conventions from
doc/08-evaluation.md. No new forcing semantics, just application of established
patterns to a new construct.

### Type Checker

**Current:** No pattern-related inference. Type dispatch is invisible to
the type system (it occurs via string-valued `type-of` comparisons).

**Proposed:** Initially, `match` expressions are typed as `Any` (gradual
typing). As the type system matures: (1) scrutinee type constrains which
patterns are valid --- if the scrutinee is typed `Int`, dict patterns are
statically rejected; (2) pattern bindings get types --- in `[ok: v]`, `v`
gets the type of the `ok` field from the scrutinee's record type;
(3) result type is the join of all arm body types --- if all arms return
`Int`, the match returns `Int`, otherwise `Any` (or a union type if
union types are added, see `doc/whatif/union-types.md`); (4) exhaustiveness
checking (Phase 5) warns when patterns don't cover all cases.

**Impact:** Minor initially (typed as `Any`). Major in later phases when
pattern types are inferred and checked. GADTs or refinement types would
enable full pattern-type interaction but are not required for the initial
design.

## Phased Adoption

### Phase 1: Type Predicates (No Grammar Change)

Add type predicate builtins alongside existing `seq?`:

```
int?   float?   num?   str?   bool?   null?   dict?   fn?
```

Each returns `Bool`. Typed as `Any → Bool`. These are useful with `cond`
and `if` for simple type dispatch without pattern matching:

```tinct
[cond [
    [[dict? x]  [map-dict f x]]
    [[seq? x]   [map-seq f x]]
    [true       [error "expected Dict or Seq"]]
]]
```

This delivers immediate value: type dispatch in tinct without grammar
changes. It also establishes the runtime type-checking primitives that
`match` will use internally.

### Phase 2: Basic `[match]` — Type and Literal Patterns

Add the `match` keyword to the grammar. Support:

- **Type patterns:** `Int`, `Str`, `Dict`, `Seq`, `Bool`, `Float`, `Null`, `Fn`
- **Literal patterns:** `42`, `true`, `"hello"`, `"quoted string"`
- **Wildcard:** `_`
- **Variable binding:** `x` (binds the scrutinee to `x`)

```tinct
[match x
    Int   [+ x 1]
    Str   [str "got: " x]
    _     x]

[match code
    200  "ok"
    404  "not-found"
    500  "server-error"
    _    "unknown"]
```

No destructuring yet — patterns are flat. This is the minimal useful
`match` that replaces `type-of` + `=` chains.

**Implementation:**
- Parser: `match` keyword, `Expr::Match` AST node, flat `Pattern` enum
- Evaluator: materialize scrutinee, try arms top-to-bottom
- Type checker: initially types as `Any`; later, check patterns against
  scrutinee type

### Phase 3: Dict and Seq Destructuring

Add structural patterns:

- **Dict patterns:** `[key1: v1  key2: v2]` — match keys, bind values
- **Open dict patterns:** `[key: v ...]` — match at least these keys
- **Seq patterns:** `[seq head tail]` — destructure cons cell
- **Nested patterns:** patterns inside patterns

```tinct
# Destructure try result
[match [try risky]
    [ok: v]    v
    [err: msg] [error msg]]

# Nested destructure
[match event
    [type: "click"  target: [id: id]]  [handle-click id]
    [type: "hover"  target: [id: id]]  [handle-hover id]
    _                                   "ignored"]

# Seq head/tail
[match xs
    [seq h t]  [process h t]
    _          "empty"]
```

This is the phase that enables self-hosting: tinct-level dispatch wrappers
can destructure dicts and seqs, delegating to Rust-native type-specific
implementations.

**Dict pattern semantics:** A dict pattern `[k1: v1  k2: v2]` matches a
materialized dict if keys `k1` and `k2` exist. By default, extra keys are
allowed (open matching) — this is consistent with row polymorphism's open
records. Closed matching `[k1: v1  k2: v2 |]` (or similar syntax) rejects
extra keys.

**Lazy dict matching:** Only the keys named in the pattern are accessed.
Other keys remain as unevaluated thunks. The bound variables (`v1`, `v2`)
are thunks — they are not forced until the body references them.

### Phase 4: Guards and Or-Patterns

Add:

- **Guards:** `pat when condition` — match if pattern matches AND
  condition is true
- **Or-patterns:** `pat1 | pat2` — match if either pattern matches
  (both must bind the same set of variables)

```tinct
[match x
    n when [> n 0]   "positive"
    n when [< n 0]   "negative"
    0                "zero"]

[match result
    [ok: v] | [success: v]   v       # accept either key
    [err: msg]               [error msg]]
```

### Phase 5: Exhaustiveness Checking

With type system support (requires union types or constrained type
variables — see `doc/whatif/union-types.md`, `doc/whatif/typeclasses.md`):

- Warn when a match on a known type set is missing cases
- Warn when a wildcard makes earlier arms unreachable
- Require exhaustive matching in checked contexts (opt-in via annotation)

This phase depends on the type system being expressive enough to represent
"all possible types" for the scrutinee. With the current `Any`-based
gradual typing, exhaustiveness checking is limited to cases where the
scrutinee has a known concrete type.

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
    [type: "click"  target: t]  [handle-click t]
    [type: "hover"  target: t]  [handle-hover t]
    _                            "ignored"]
| [collect]
```

This falls out naturally from both designs — no new mechanism needed. The `_` scrutinee
rule (documented in §Interaction with `_` Desugaring above) already covers it.

#### 2. Guards compose better with `|`

Guards are full expressions, so once `|` is available they become more readable:

```tinct
[match user
    [role: r] when [r | "admin"]  [admin-panel user]
    [role: r]                      [user-panel user]]
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
    [0: a  1: b]  [use a b]
    _              [error "expected pair"]]
```

**Implementation note:** wait for `access-pipeline` to land first (it adds
`Key::Int` dot lookup to the evaluator), then extend the pattern parser to
accept `Token::Int` in the `key:` position of a dict pattern.

#### 4. Path-key patterns — DRY deep structure matching

Dict patterns for deeply nested data quickly become repetitive. Path-key
syntax lets the key position in a dict pattern use a dotted path, desugaring
to an equivalent nested dict:

```
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
    [cluster: [primary: [tls: [cert: cert  key: key]]]]
    [connect-tls cert key]
    _ [error "no tls"]]

# Path-key to the subtree node — DRY when the subtree has multiple fields
[match config
    [cluster.primary.tls: [cert: cert  key: key]]
    [connect-tls cert key]
    _ [error "no tls"]]

# Mixed: path-to-node + path-to-leaf in the same pattern arm
[match config
    [cluster.primary.tls: [cert: cert  key: key]
     cluster.primary.host: h]
    [connect-tls cert key h]
    _ [error "no tls"]]
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

### Prerequisites

- **Phase 1:** None — type predicates are standalone builtins
- **Phase 2:** Lexer/parser infrastructure for keywords (already exists
  for `call`, `fn`, `type`). `match` added to keyword denylist.
- **Phase 3:** Phase 2 complete. Seq type stable (`Value::Seq` in
  evaluator). Integer-key dict patterns and path-key integer segments:
  wait for `access-pipeline` (adds integer dot access to the evaluator).
  Path-key pattern desugaring (string paths): can land with Phase 3 —
  it is a pure parser-level transformation with no evaluator dependency.
- **Phase 4:** Phase 3 complete.
- **Phase 5:** Type system maturity — union types or type classes for
  exhaustiveness analysis.

### Trigger

Phase 1 (type predicates): adopt immediately — these are independently
useful, trivial to implement, and have no grammar impact.

Phase 2 (basic match): adopt when:
- A second dual-dispatch builtin needs a tinct-level wrapper
- User code patterns show repeated `type-of` + `=` chains
- The `_` desugaring is complete and stable (confirming the
  special-form pattern)

Phase 3 (destructuring): adopt when:
- Self-hosting of dual-dispatch builtins begins
- `try` result handling becomes a common pattern in user code
- Record/dict destructuring is the #1 user ergonomics request

## References

**Pattern matching compilation:**
- Augustsson, L. (1985). "Compiling pattern matching." In *FPCA '85*,
  LNCS 201, pp. 368–381. Springer. — Decision tree compilation for
  pattern matching in lazy functional languages.
- Maranget, L. (2008). "Compiling pattern matching to good decision
  trees." In *ML '08*, pp. 35–46. ACM. — Optimal decision trees for
  pattern compilation. Directly applicable to Phase 2+ compilation.
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
