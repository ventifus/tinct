# What If: Pattern Matching for tinct

What would it take to add pattern matching to tinct's bracket-based lazy
functional language?

## Current State

tinct has no pattern matching construct. Type-based dispatch requires
verbose `$type-of` + string comparison chains:

```lisp
[call $if [call $= [call $type-of $x] Dict]
    [call $handle-dict $x]
    [call $if [call $= [call $type-of $x] Seq]
        [call $handle-seq $x]
        [call $error "unexpected type"]]]
```

The available dispatch mechanisms are:

- **`$type-of`** returns a string (`"Int"`, `"Float"`, `"Dict"`, `"Seq"`, etc.)
- **`$seq?`** is the only type predicate builtin (returns Bool)
- **`$if` + `$=` chains** are the only dispatch mechanism
- No destructuring bind
- No exhaustiveness checking

This matters for three reasons:

1. **Self-hosting builtins.** Dual-dispatch builtins (`$map`, `$filter`,
   `$reduce`, `$join`, `$take`, `$drop`) are in Rust primarily because they
   need to branch on `Value::Dict` vs `Value::Seq`. With pattern matching +
   more granular primitives, tinct could define the dispatch wrapper in tinct
   and keep only the type-specific implementations in Rust. The macros whatif
   (`doc/whatif/macros.md`) identifies pattern matching as "the biggest
   enabler for self-hosting more Rust builtins."

2. **User code.** Any polymorphic data processing — handling different
   shapes, optional fields, tagged unions, `$try` results — is awkward
   without matching.

3. **Error handling.** `$try` returns `[ok: value]` or `[err: message]`.
   Dispatching on the result key without destructuring is clunky:
   ```lisp
   result: [call $try $risky]
   [call $if [call $has? $result ok]
       [call $handle-ok $result.ok]
       [call $handle-err $result.err]]
   ```

## What Pattern Matching Would Provide

- **Type dispatch** without `$type-of` + string comparison chains
- **Destructuring bind** — extract dict fields, seq head/tail, tagged
  union payloads in one expression
- **Multi-branch dispatch** — replace nested `$if`/`$cond` chains with a
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
   without pattern matching syntax. tinct's `$cond` + `$try-or` offer a
   similar, if less ergonomic, composition.

## Approaches

### Approach A: `[match]` Special Form

A new keyword `match` parsed as a special form, like `call` and `fn`:

```lisp
[match $expr
    [pattern1] $body1
    [pattern2] $body2
    _          $default]
```

Each arm is a pattern followed by a body expression. Patterns are parsed
inside `[]` brackets using tinct's existing syntax. The match expression
evaluates to the body of the first matching arm.

**Pattern syntax** (using existing tinct constructs where possible):

```lisp
# Type patterns — bare words match type-of result
[match $x
    Int    [call $+ $x 1]            # matches if $type-of is "Int"
    Str    [call $str "got: " $x]    # matches if $type-of is "Str"
    _      $x]                       # wildcard — always matches

# Literal patterns — literals match by equality
[match $x
    42     the-answer
    true   yes
    hello  greeting
    _      other]

# Dict patterns — destructure by key
[match $result
    [ok: $v]    $v                   # binds $v to the value at key "ok"
    [err: $msg] [call $error $msg]   # binds $msg to the value at key "err"
    _           [call $error "unexpected"]]

# Seq patterns — head/tail destructure
[match $xs
    [seq $h $t] [call $process $h $t]  # binds head and tail
    []          empty                   # empty collection
    _           [call $error "expected seq"]]

# Nested patterns
[match $event
    [type: click  target: [id: $id]]  [call $handle-click $id]
    [type: hover  target: [id: $id]]  [call $handle-hover $id]
    _                                  ignored]

# Guards
[match $x
    $n when [call $> $n 0]  positive
    $n when [call $< $n 0]  negative
    _                       zero]
```

**Pros:**
- First-class language construct — can be optimized, type-checked, analyzed
- Pattern syntax reuses tinct's `[]` brackets and `$` sigils naturally
- Destructuring bindings are obvious (`$v` in a pattern binds)
- Guards use existing tinct expressions
- Amenable to exhaustiveness analysis (if type system supports it later)

**Cons:**
- New keyword in the grammar (parser change, `match` added to denylist)
- New AST variant (`Expr::Match`)
- Implementation spans parser, evaluator, and type checker

### Approach B: `$match` Builtin Function

Pattern matching as a regular function, not a special form:

```lisp
[call $match $expr
    [fn [x@Int] [call $+ $x 1]]
    [fn [x@Str] [call $str "got: " $x]]
    [fn [x]     $x]]
```

Each arm is a function with type annotations used for dispatch. The match
builtin tries each function in order, calling the first whose annotation
accepts the argument.

**Pros:**
- No grammar changes — `$match` is a regular builtin
- Uses existing `fn` + annotation syntax
- Functions are first-class — arms can be computed, passed around

**Cons:**
- No destructuring — annotations check types but don't bind sub-structure
- Nested patterns impossible (can't annotate "dict with key ok")
- Verbose — every arm needs `[fn [...] ...]` wrapping
- Type annotations aren't designed for pattern dispatch
- No literal matching (annotations are types, not values)
- Guards require wrapping body in `$if` — no syntactic support

### Approach C: `$cond` Enhancement (No Pattern Matching)

Enhance the existing `$cond` stdlib function and add type predicates
instead of adding pattern matching:

```lisp
# Enhanced cond with type predicates
[call $cond [
    [[call $int? $x]    [call $+ $x 1]]
    [[call $str? $x]    [call $str "got: " $x]]
    [true               $x]
]]
```

Add type predicates: `$int?`, `$float?`, `$str?`, `$bool?`, `$dict?`,
`$null?`, `$fn?` alongside the existing `$seq?`.

**Pros:**
- Zero grammar changes
- Zero parser/evaluator changes
- Type predicates are independently useful
- Follows Nix/Jsonnet precedent (both work without pattern matching)
- Simplest implementation

**Cons:**
- No destructuring — still need manual field access after dispatch
- Verbose — condition + body in nested brackets
- No exhaustiveness checking (even theoretical)
- Doesn't solve the self-hosting motivation (still need Rust for dispatch)
- Doesn't scale to nested structure matching

### Approach D: Nickel-Style Match-as-Function

Match expression evaluates to a function (Nickel model):

```lisp
# match evaluates to a one-argument function
handler: [match
    [ok: $v]    $v
    [err: $msg] [call $error $msg]]

# apply it
[call $handler $result]

# or inline
[call [match
    Int [call $+ $_ 1]
    Str [call $str "got: " $_]
    _   $_] $x]
```

**Pros:**
- Match is a value (function) — composable with `$map`, `$->`, etc.
- `[call $map [match ...] $collection]` works naturally
- Aligns with tinct's "functions are values" philosophy
- Nickel proves this works in a lazy language

**Cons:**
- Confusing semantics — `[match ...]` alone doesn't evaluate, must be
  applied. Nickel users found this surprising.
- Requires `$_` or implicit parameter — patterns bind against what?
- Interaction with tinct's existing `$_` desugaring is tricky (both
  introduce implicit parameters)

## What Would Change

### Parser

A new keyword `match` enters the denylist (cannot be used as a bare-word
string in positions where it would be ambiguous). The parser recognizes
`[match ...]` and produces `Expr::Match` nodes. Pattern parsing reuses
existing expression parsing with a "pattern mode" that interprets `$name`
as a binding (not a lookup) and bare words as type tags.

### AST

```rust
pub enum Expr {
    // ... existing variants ...

    /// Pattern match expression: [match $expr arm1 arm2 ...]
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
    /// Variable binding: $x (binds the matched value to x)
    Var(String),
    /// Literal: 42, true, hello
    Literal(LiteralPattern),
    /// Type tag: Int, Str, Dict, Seq, etc.
    Type(String),
    /// Dict destructure: [key1: $v1  key2: $v2]
    Dict {
        fields: Vec<(String, Spanned<Pattern>)>,
        rest: bool,  // true if ... present (open match)
    },
    /// Seq destructure: [seq $head $tail]
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

Pattern matching in the evaluator:

1. **Evaluate the scrutinee** — fully materialize to determine type and
   structure. This is inherently materializing (like `$type-of`).

2. **Try each arm top-to-bottom** — for each arm, attempt to match the
   pattern against the materialized value:
   - `Wildcard` / `Var` — always match. `Var` binds the value.
   - `Literal` — compare by equality.
   - `Type` — compare `$type-of` result.
   - `Dict` — check that all specified keys exist, recursively match
     sub-patterns on their values. If `rest: false`, no extra keys allowed.
   - `Seq` — match head and tail.
   - `Or` — try sub-patterns left-to-right, succeed on first match.

3. **Check guard** (if present) — evaluate the guard expression in an
   environment extended with pattern bindings. If it evaluates to `true`,
   the arm matches.

4. **Evaluate body** — in the environment extended with pattern bindings.

5. **No match** — if no arm matches, raise a runtime error (MatchError).

### Lazy Evaluation Interaction

**The scrutinee is forced.** Pattern matching requires knowing the value's
type and structure, so the scrutinee must be materialized. This is the same
semantics as `$type-of` — inherently materializing.

**Dict pattern matching forces matched keys only.** When matching
`[ok: $v]` against a dict, only the key `"ok"` is accessed. Other keys
remain as thunks. This preserves laziness for unmatched structure.

**Seq pattern matching forces the head.** Matching `[seq $h $t]` forces
the head thunk and binds the tail thunk without forcing it. This is
consistent with `$head` and `$tail` builtin behavior.

**Bodies are lazy.** Only the matching arm's body is evaluated — other
arms' bodies are never entered.

### Type System

Initially, `match` expressions are typed as `Any` (gradual typing). As
the type system matures:

1. **Scrutinee type constrains which patterns are valid.** If the scrutinee
   is typed `Int`, dict patterns are statically rejected.

2. **Pattern bindings get types.** In `[ok: $v]`, `$v` gets the type of the
   `ok` field from the scrutinee's record type.

3. **Result type is the join of all arm body types.** If all arms return
   `Int`, the match returns `Int`. If arms return different types, the
   result is their least upper bound (or `Any` without union types, or
   `Int | Str` with union types — see `doc/whatif/union-types.md`).

4. **Exhaustiveness checking** (Phase 3, see below) — with type information,
   the type checker can warn when patterns don't cover all cases.

## Recommendation

**Approach A: `[match]` special form, with phased adoption.**

### Rationale

1. **Self-hosting requires destructuring.** The primary motivation is
   replacing Rust-side `match` on `Value::Dict` vs `Value::Seq` with
   tinct-level dispatch. Approaches B (builtin) and C (cond enhancement)
   don't support destructuring, which is needed to extract dict fields and
   seq structure in the tinct-level wrapper.

2. **tinct's bracket syntax accommodates `match` naturally.** Unlike
   s-expression languages where `match` is just another list form, tinct's
   keywords (`call`, `fn`, `type`) are parsed specially. `match` fits the
   same pattern. Patterns inside `[]` reuse existing syntax: bare words are
   type tags, `$name` is a binding, literals match by value.

3. **Not match-as-function (Approach D).** Nickel's match-as-function
   interacts poorly with tinct's `$_` desugaring — both introduce implicit
   parameters. A `[match $scrutinee ...]` form with an explicit scrutinee
   avoids this conflict and is clearer to read.

4. **Not a builtin (Approach B).** Type annotations aren't pattern
   specifications. `fn` parameters with `@Int` annotations don't express
   "dict with key ok" or "seq with at least one element." A dedicated
   pattern syntax is needed for structural matching.

5. **Not cond-only (Approach C).** Type predicates are independently useful
   and should be added regardless. But predicates alone don't provide
   destructuring, and nested `$if`/`$cond` chains don't scale to the
   multi-branch structural dispatch that self-hosting requires.

### Phased Adoption

#### Phase 1: Type Predicates (No Grammar Change)

Add type predicate builtins alongside existing `$seq?`:

```
$int?   $float?   $num?   $str?   $bool?   $null?   $dict?   $fn?
```

Each returns `Bool`. Typed as `Any → Bool`. These are useful with `$cond`
and `$if` for simple type dispatch without pattern matching:

```lisp
[call $cond [
    [[call $dict? $x]  [call $map-dict $f $x]]
    [[call $seq? $x]   [call $map-seq $f $x]]
    [true              [call $error "expected Dict or Seq"]]
]]
```

This delivers immediate value: type dispatch in tinct without grammar
changes. It also establishes the runtime type-checking primitives that
`match` will use internally.

#### Phase 2: Basic `[match]` — Type and Literal Patterns

Add the `match` keyword to the grammar. Support:

- **Type patterns:** `Int`, `Str`, `Dict`, `Seq`, `Bool`, `Float`, `Null`, `Fn`
- **Literal patterns:** `42`, `true`, `hello`, `"quoted string"`
- **Wildcard:** `_`
- **Variable binding:** `$x` (binds the scrutinee to `x`)

```lisp
[match $x
    Int   [call $+ $x 1]
    Str   [call $str "got: " $x]
    _     $x]

[match $code
    200  ok
    404  not-found
    500  server-error
    _    unknown]
```

No destructuring yet — patterns are flat. This is the minimal useful
`match` that replaces `$type-of` + `$=` chains.

**Implementation:**
- Parser: `match` keyword, `Expr::Match` AST node, flat `Pattern` enum
- Evaluator: materialize scrutinee, try arms top-to-bottom
- Type checker: initially types as `Any`; later, check patterns against
  scrutinee type

#### Phase 3: Dict and Seq Destructuring

Add structural patterns:

- **Dict patterns:** `[key1: $v1  key2: $v2]` — match keys, bind values
- **Open dict patterns:** `[key: $v ...]` — match at least these keys
- **Seq patterns:** `[seq $head $tail]` — destructure cons cell
- **Nested patterns:** patterns inside patterns

```lisp
# Destructure try result
[match [call $try $risky]
    [ok: $v]    $v
    [err: $msg] [call $error $msg]]

# Nested destructure
[match $event
    [type: click   target: [id: $id]]  [call $handle-click $id]
    [type: hover   target: [id: $id]]  [call $handle-hover $id]
    _                                   ignored]

# Seq head/tail
[match $xs
    [seq $h $t]  [call $process $h $t]
    _            empty]
```

This is the phase that enables self-hosting: tinct-level dispatch wrappers
can destructure dicts and seqs, delegating to Rust-native type-specific
implementations.

**Dict pattern semantics:** A dict pattern `[k1: $v1  k2: $v2]` matches a
materialized dict if keys `k1` and `k2` exist. By default, extra keys are
allowed (open matching) — this is consistent with row polymorphism's open
records. Closed matching `[k1: $v1  k2: $v2 |]` (or similar syntax) rejects
extra keys.

**Lazy dict matching:** Only the keys named in the pattern are accessed.
Other keys remain as unevaluated thunks. The bound variables (`$v1`, `$v2`)
are thunks — they are not forced until the body references them.

#### Phase 4: Guards and Or-Patterns

Add:

- **Guards:** `$pat when $condition` — match if pattern matches AND
  condition is true
- **Or-patterns:** `$pat1 | $pat2` — match if either pattern matches
  (both must bind the same set of variables)

```lisp
[match $x
    $n when [call $> $n 0]   positive
    $n when [call $< $n 0]   negative
    0                         zero]

[match $result
    [ok: $v] | [success: $v]   $v       # accept either key
    [err: $msg]                [call $error $msg]]
```

#### Phase 5: Exhaustiveness Checking

With type system support (requires union types or constrained type
variables — see `doc/whatif/union-types.md`, `doc/whatif/typeclasses.md`):

- Warn when a match on a known type set is missing cases
- Warn when a wildcard makes earlier arms unreachable
- Require exhaustive matching in checked contexts (opt-in via annotation)

This phase depends on the type system being expressive enough to represent
"all possible types" for the scrutinee. With the current `Any`-based
gradual typing, exhaustiveness checking is limited to cases where the
scrutinee has a known concrete type.

### Prerequisites

- **Phase 1:** None — type predicates are standalone builtins
- **Phase 2:** Lexer/parser infrastructure for keywords (already exists
  for `call`, `fn`, `type`). `match` added to keyword denylist.
- **Phase 3:** Phase 2 complete. Seq type stable (`Value::Seq` in
  evaluator).
- **Phase 4:** Phase 3 complete.
- **Phase 5:** Type system maturity — union types or type classes for
  exhaustiveness analysis.

### Trigger

Phase 1 (type predicates): adopt immediately — these are independently
useful, trivial to implement, and have no grammar impact.

Phase 2 (basic match): adopt when:
- A second dual-dispatch builtin needs a tinct-level wrapper
- User code patterns show repeated `$type-of` + `$=` chains
- The `$_` desugaring is complete and stable (confirming the
  special-form pattern)

Phase 3 (destructuring): adopt when:
- Self-hosting of dual-dispatch builtins begins
- `$try` result handling becomes a common pattern in user code
- Record/dict destructuring is the #1 user ergonomics request

## Design Considerations

### `match` vs `case` Keyword

Both are common. `match` is used by Rust, Nickel, Scala, F#, OCaml.
`case` is used by Haskell, Elixir, Erlang. `match` reads more naturally
in tinct's bracket syntax: `[match $x ...]` vs `[case $x ...]`.
Recommendation: **`match`**.

### Pattern Variable Syntax

In tinct, `$x` is a variable reference (lookup). In patterns, `$x` means
"bind the matched value to `x`." This dual meaning is potentially
confusing but follows Elixir's precedent (variables in patterns bind, not
match). The alternative — a new sigil like `?x` for pattern bindings —
adds complexity without proportional benefit.

**Pin operator:** Elixir's `^` pin operator (match against existing
variable's value instead of rebinding) would be useful:
`[match $x  ^$expected result  _ other]`. Defer to Phase 4+.

### Open vs Closed Dict Matching

Dict patterns should default to **open matching** (extra keys allowed).
This matches row polymorphism's open records and is more useful for
configuration data where extra fields are common. Closed matching (reject
extra keys) would use explicit syntax (e.g., trailing `|` or `!`).

### Interaction with `$_` Desugaring

`[match]` bodies can use `$_` normally — the `$_` desugaring pass runs
before evaluation, and `match` bodies are ordinary expressions. No special
interaction. The match scrutinee can also be `$_`, creating a function:
`[fn [_] [match $_ ...]]` via the WRAP-CALL rule.

### Materialization Semantics

Pattern matching on the scrutinee is inherently materializing (like
`$type-of`). This means `[match $thunk ...]` forces `$thunk`. Within
dict patterns, only matched keys are forced. This is documented in
DESIGN.md §Builtin Materialization Behavior as the standard pattern for
builtins that need to inspect value structure.

## References

**Pattern matching in lazy languages:**
- Augustsson, L. (1985). "Compiling pattern matching." In *FPCA '85*,
  LNCS 201, pp. 368–381. Springer. — Decision tree compilation for
  pattern matching in lazy functional languages.
- Maranget, L. (2008). "Compiling pattern matching to good decision
  trees." In *ML '08*, pp. 35–46. ACM. — Optimal decision trees for
  pattern compilation.
- Peyton Jones, S.L. (1987). *The Implementation of Functional
  Programming Languages.* Prentice Hall. Chapter 5: pattern matching
  compilation strategies for lazy languages.

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
