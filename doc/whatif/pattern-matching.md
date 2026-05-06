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

`[match]` is implemented as `[defmacro match]` (see `doc/whatif/macro-rewrite.md`)
with parse-stage macro support for context-sensitive key identity (see
`doc/whatif/parse-stage-macros.md`). Arms use dict syntax — the pattern spec
is the key, the body is the value:

```tinct
[match expr
    pattern1-key: body1
    pattern2-key: body2
    _:            default]
```

**Context-sensitive key identity:** Inside `[match]`, the full annotated
expression is the key identity — `n@Int` and `n@String` are distinct keys even
though both have base name `n`. This is declared via `[syntax-class match-arms-dict
key-identity: full-annotated-expr]` in `stdlib/syntax.llt`. Regular dicts
are unchanged: `[n@Int: 1  n@String: "2"]` remains a duplicate-key error.

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
    n@Int:  [+ n 1]
    n@Str:  i"got: $n"
    _:      x]

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
    n@Int:                         i"non-positive int: $n"
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
is a `Fn@Bool [Any]` predicate. The match macro calls it with the bound value.
`true` = arm fires; `false` = fall through to next arm. Use `_` as the
placeholder: `[> _ 0]` desugars to `[fn [_] [> _ 0]]`. For multiple
predicates use `and` composition: `[is: [and [> _ 5] [< _ 10]]]`. Named
contracts work directly: `n@[is: PortRange]` where `PortRange: [fn [v] ...]`.

The pattern language is amenable to exhaustiveness analysis as the type
system matures. The key advantage over a special form: no new `Expr::Match`
AST variant — `[match]` is a macro, so the evaluator, type checker,
formatter, and resolver need zero changes.

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

### Why a Macro (Not a Special Form)

`[match]` is implemented via `[defmacro match]` rather than as an `Expr::Match`
special form. This is the right call for three reasons:

1. **Zero AST surface area.** `Expr::Match` would propagate through every
   exhaustive match on `Expr` in the codebase — evaluator, type checker,
   formatter, resolver, desugar, ast_dict.rs. As a macro, none of those
   files need changes. `[match]` expands to nested `if`/`type-of` chains
   that the existing evaluator already handles.

2. **Dict arm syntax is natural tinct.** Using dict key-value syntax for
   arms — pattern as key, body as value — means arms are plain tinct data.
   The parse-stage macro (`[declare-syntax match]`) handles the key identity
   override so `n@Int` and `n@String` coexist. No bespoke pattern-parsing
   mode needed in the main parser.

3. **Not match-as-function.** Nickel's match-as-function interacts poorly
   with tinct's `_` desugaring — both introduce implicit parameters. The
   `[match scrutinee ...]` form with an explicit scrutinee avoids this
   conflict and is clearer to read.

## What Would Change

### Parser (`src/parser.rs`)

**Current:** Keywords `call`, `fn`, `type` are recognized as special forms.

**Proposed:** Add `match` to the keyword denylist so `match:` cannot appear as a dict key in non-match contexts. No new parsing mode — the arm dict is parsed normally; the parse-stage macro handles key identity.

**Impact:** Minor — one keyword added to the denylist.

### AST (`src/ast.rs`)

**No changes.** `[match]` is a macro — it desugars to `if`/`type-of` chains using existing `Expr` variants. No `Expr::Match`, no `Pattern` enum, no `MatchArm` struct. Every existing exhaustive match on `Expr` is unaffected.

### `stdlib/syntax.llt`

**Proposed:** Declare the match arms dict syntax class with full-annotated-expression key identity:

```tinct
[declare-syntax match
  [scrutinee: expr  arms: match-arms-dict]]

[syntax-class match-arms-dict
  key-identity: full-annotated-expr
  values: expr]
```

This is the parse-stage change that allows `n@Int` and `n@String` to coexist as distinct arm keys. See `doc/whatif/parse-stage-macros.md`.

**Impact:** New stdlib file entry — no Rust changes.

### `stdlib/macros.llt`

**Proposed:** `[defmacro match]` receives the arm dict (with full-annotated keys) and expands to nested `if` chains. Pattern dispatch on the key node type:

- `VarRef("_")` → wildcard: always matches
- `VarRef("n")` (lowercase) → variable binding: always matches, bind `n`
- `VarRef("Int")` (uppercase) → type pattern: `[int? scrutinee]`
- `Int(42)` (literal) → literal match: `[= scrutinee 42]`
- `Dict([ok: VarRef("v")])` → dict pattern: `[and [dict? s] [has? s "ok"]]`, bind `v`
- `Annotated("n", Simple("Int"))` → type-constrained binding
- `Annotated("n", PropertyDict([is: pred]))` → guard: call `pred` with bound value
- `Pipe(p1, p2)` → or-pattern: try both sub-patterns

**Impact:** New macro in stdlib/macros.llt — no Rust changes.

### Lazy Evaluation

The expanded `if`/`type-of` chains follow existing materialization
conventions. Dict pattern matching forces only matched keys — implemented
by the macro expansion's use of `has?` and dot access. Seq pattern matching
uses `head` and `tail` builtins, which force the head and leave the tail
as a thunk. Only the matching arm's body evaluates. No new forcing semantics.

### Type Checker

Initially, `[match]` is typed as `Any` (same as any `if` chain where arm
types differ). As the type system matures, two distinct narrowing mechanisms apply:

**Type-predicate arms narrow statically.** `n@Int:` expands to `[if [int? scrutinee] ...]`.
When occurrence typing (`doc/whatif/narrowing.md`) lands, the type checker recognises
`int?` as a type guard and narrows `n` to `Int` inside the true branch. Similarly
`n@Str:` narrows to `Str`, dict patterns `[ok: v]:` narrow to `[ok: ...]`, etc.

**`is:` predicate arms do NOT narrow the type.** `n@[is: [> _ 0]]:` proves a runtime
condition (`n > 0`) but the type checker cannot derive a static type from an arbitrary
`Fn@Bool [Any]` predicate. `n` retains whatever type the scrutinee had — `Int` if the
scrutinee was typed `Int`, `Any` if it was untyped. This is correct behavior: `is:`
guards are value constraints, not type constraints.

The distinction matters for arm body type safety: after `n@Int:` the type checker
knows `n` is an `Int` and can reject `n.field` as a type error; after `n@[is: [> _ 0]]:`,
it cannot. Users who need both should compose: `n@[type: Int  is: [> _ 0]]:` gives
type narrowing AND the value guard.

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
    Int:  [+ x 1]
    Str:  i"got: $x"
    _:    x]

[match code
    200:  "ok"
    404:  "not-found"
    500:  "server-error"
    _:    "unknown"]
```

No destructuring yet — patterns are flat. This is the minimal useful
`match` that replaces `type-of` + `=` chains.

**Implementation:**
- Parser: `match` keyword added to denylist; arm dict parsed normally
- `stdlib/syntax.llt`: `[declare-syntax match ...]` with full-annotated key identity
- `stdlib/macros.llt`: `[defmacro match]` Phase 1 — type + literal + wildcard + variable binding arms
- Type checker: expanded `if` chain types as `Any` initially

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

- **Guards:** `n@[is: pred]:` — `is:` annotation key with a `Fn@Bool [Any]` predicate; arm fires when predicate returns `true`
- **Or-patterns:** `pat1 | pat2:` — `Pipe` node as key; match if either sub-pattern matches (both must bind the same set of variables)

```tinct
[match x
    n@[is: [> _ 0]]:   "positive"
    n@[is: [< _ 0]]:   "negative"
    _:                  "zero"]

[match result
    [ok: v] | [success: v]:   v       # or-pattern: accept either key
    [err: msg]:               [error msg]]
```

### Phase 5: Exhaustiveness Checking

Exhaustiveness is checked **at macro expansion time** when the scrutinee is
wrapped in a TypeAssert — the Dhall model for union elimination. No new
`Expr::Match` AST variant is needed.

```tinct
# Without TypeAssert — no exhaustiveness check (runtime MatchError if no arm matches)
[match res
    [ok: v]:    v
    [err: msg]: [error msg]]

# With TypeAssert — exhaustiveness verified at expansion time
[match [@Result res]
    [ok: v]:    v
    [err: msg]: [error msg]]    # ✓ all variants of Result covered

[match [@Result res]
    [ok: v]:    v]              # ✗ expansion-time error: [err: Str] not covered
```

When `[defmacro match]` sees `[@Type scrutinee]`, it looks up `Type` in the
type alias registry, extracts the `Type::Union` variant set, and performs
Maranget-style coverage analysis on the arm keys:

- Type-tag arms (`n@Int:`) cover the `Int` variant
- Dict pattern arms (`[ok: v]:`) cover the `[ok: a]` structural variant
- Wildcard `_:` covers all remaining variants
- Or-pattern arms (`p1 | p2:`) cover both sub-patterns
- `is:` predicate arms are **opaque** — they do not contribute to coverage
  (the guard is a runtime condition, not a type constraint)

Without a TypeAssert scrutinee, exhaustiveness is opt-out: the match
compiles and runs correctly, but a runtime `MatchError` fires if no arm
matches. This mirrors Nickel's behavior (runtime MatchError) and is honest:
coverage can only be statically verified when the union type is declared.

- **Unreachable arms:** an arm after a wildcard `_:` is flagged at expansion time.
- **`is:` arms and coverage:** `n@[type: Int  is: [> _ 0]]:` covers the `Int`
  variant for coverage purposes (the type constraint `@Int` is structural; the
  `is:` predicate is an additional runtime filter that does not affect which
  variants are covered). This treatment of guards as opaque for coverage
  purposes follows Karachalias et al. (2015): guards are irrefutable with
  respect to type-level exhaustiveness — they may cause runtime match failure
  but cannot be statically proven to cover or exclude a type variant.

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
- **Phase 2:** Macros Phase 2 (`[defmacro]`) + Parse-stage macros Phase 1 (syntax classes with full-annotated key identity, `doc/whatif/parse-stage-macros.md`). `match` added to keyword denylist.
- **Phase 3:** Phase 2 complete. Seq type stable (`Value::Seq` in evaluator). Integer-key dict patterns and path-key integer segments: wait for `access-pipeline` (adds integer dot access to the evaluator). Path-key pattern desugaring (string paths): can land with Phase 3 — pure parser-level transformation.
- **Phase 4:** Phase 3 complete.
- **Phase 5:** Type system maturity — union types or type classes for exhaustiveness analysis.

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
