# What If: Matchable — Open, User-Defined Patterns for tinct

**State:** Proposal

What would it take to unify tinct's `[match]` arm patterns and `[fn]` parameter
patterns into a single open, user-extensible system?

## Goals

1. **One pattern language everywhere.** Function parameters, match arms, and let
   bindings all use the same pattern vocabulary and the same semantics.
2. **Patterns are first-class values.** A pattern is any value implementing the
   `Matchable` typeclass. Users can define new pattern types without touching the
   compiler.
3. **Built-in patterns are tinct code.** Dict patterns, constructor patterns,
   literal equality, wildcard, type guards — all implemented as `Matchable`
   instances in prelude, not hard-coded in Rust.
4. **Type narrowing is user-extensible.** The `narrow` method carries the
   type-level proof: given the scrutinee's type, what types do the bindings
   have in the success arm? User-defined patterns participate in type narrowing,
   dead-arm detection, and exhaustiveness analysis on the same footing as
   built-in patterns.
5. **Minimal Rust surface.** The only Rust responsibility is the dispatch
   mechanism and scope injection. All pattern logic lives in tinct.

## Current State

Tinct has two separate pattern systems that share nothing:

**Function parameters** — flat binding lists with optional type annotations:

```tinct
[fn [let x@Int  y@String] [str x " " y]]
```

Parameters can only be simple names. There is no structural destructuring, no
literal matching, no constructor patterns. To destructure an argument, you must
name it and then access its fields in the body:

```tinct
process: [fn [let point]
  [x: point.x  y: point.y]
  [str "(" x ", " y ")"]]
```

**Match arms** — a richer but closed set of patterns:

```tinct
[match value
  Color.Red:           "#ff0000"
  42:                  "the answer"
  [case [let h p] [host: h  port: p]  [str h ":" p]]
  [case [let _]   _                    "other"]]
```

The `[case [let names] pattern body]` form separates binding declarations
(`[let names]`) from the structural pattern. Names listed in `[let]` are
introduced as new scope variables; names in the pattern that are not in `[let]`
are value references (pins) from the enclosing scope.

### What's Missing

1. No structural destructuring in function parameters.
2. No literal or constructor patterns in function parameters.
3. No user-defined patterns — the pattern set is closed and hard-coded in Rust.
4. Two separate systems with no shared abstractions or combinators.
5. No pattern combinators (or-patterns, guard-patterns, as-patterns) as
   composable library functions.

## Why Matchable Matters for tinct

**User-defined patterns.** Any type can become a valid pattern by implementing
`Matchable`. An `IpAddressPattern` can match and destructure IP addresses. A
`RangePattern` can match integers in a range. A predicate function can serve
directly as a pattern.

**Functions become pattern-dispatched.** Function parameters are Matchables.
A function `[fn [let [Option.Some v]  n@Int] ...]` only accepts calls where the
first argument is `Some` and the second is an integer — the type system and
runtime enforce this via the same mechanism.

**Combinators as library code.** `or-pattern`, `guard-pattern`, `as-pattern`,
`not-pattern` are ordinary tinct functions that return Matchable values. They
compose with any user-defined pattern.

**The prelude is the pattern library.** All patterns anyone has ever needed in
tinct — dict matching, constructor matching, type narrowing — live in tinct code
that users can read, understand, and extend, not in opaque Rust.

## Design

### The Matchable Typeclass

```tinct
Matchable: [class [let p]
  # Runtime: does the subject match? If so, what are the binding values?
  # On success: Dict mapping binding-name → extracted value.
  # On failure: Absent.Absent.
  try-match: [Fn@[or Dict Absent] [p Any]]

  # Type-level proof: given the scrutinee's TypeNode, what TypeNodes do the
  # bindings have when the match succeeds?
  # On success: Dict mapping binding-name → TypeNode.
  # On failure (Absent): this pattern structurally cannot match that type —
  #   the arm is unreachable and the type checker may warn.
  narrow: [Fn@[or Dict Absent] [p TypeNode]]]
```

`try-match` is the runtime half: given a pattern value and a subject value,
return the extracted bindings or `Absent.Absent`. `narrow` is the type-level
proof: given a pattern value and the scrutinee's `TypeNode`, return the
`TypeNode` for each binding, or `Absent.Absent` if the pattern cannot match a
value of that type.

Together they provide both sides of the Curry-Howard correspondence for
patterns: `try-match` is the runtime witness, `narrow` is the compile-time
proof. A user-defined pattern that supplies `narrow` participates fully in type
narrowing, dead-arm detection, and exhaustiveness analysis. A pattern without
`narrow` (or one that always returns `Absent.Absent` from `narrow`) is opaque
to the type checker — it still works at runtime but contributes no static
guarantees.

### The [let] Binding Contract

`[let names...]` in both `[fn]` and `[case]` is the explicit binding introducer.
It declares which names from `try-match`'s returned dict enter the local scope.
Nothing enters scope implicitly.

Inside `[let]`, names are interpreted as:

- **Bare name** (`h`, `port`) — a new binding to be populated from the matched
  value. The name becomes a key expected in `try-match`'s result dict.
- **`$name`** — a pin: compare the matched position against the current value of
  `name` from the enclosing scope. Does not introduce a new binding.
- Literals (`42`, `"hello"`) — match exactly. No binding introduced.
- **`_`** — wildcard. Always succeeds. No binding introduced.

### Built-in Matchable Instances (prelude)

All of these are tinct code. Each instance implements both `try-match` (runtime)
and `narrow` (type-level proof):

```tinct
# Wildcard — always succeeds, no bindings, never narrows
WildcardPattern: [type Wildcard]
_: WildcardPattern.Wildcard

[instance Matchable WildcardPattern
  [try-match: [fn [let _ val] []]]
  [narrow:    [fn [let _ typ] []]]]   # succeeds for any type, no bindings

# Name binder — bind the whole subject to a name
Bind: [type [name: String]]

[instance Matchable Bind
  [try-match: [fn [let b val]
    [make-entry b.name val]]]
  [narrow: [fn [let b typ]
    [make-entry b.name typ]]]]        # binding gets the scrutinee's type

# Literal equality via Equatable — narrows to the singleton literal type
[instance Matchable Int
  [try-match: [fn [let lit val]
    [if [= lit val] [] Absent.Absent]]]
  [narrow: [fn [let lit typ]
    [if [not [consistent-subtype? typ TypeNode.Int]] Absent.Absent
      []]]]]                          # no bindings; narrows the scrutinee

[instance Matchable String
  [try-match: [fn [let lit val]
    [if [= lit val] [] Absent.Absent]]]
  [narrow: [fn [let lit typ]
    [if [not [consistent-subtype? typ TypeNode.String]] Absent.Absent
      []]]]]

# Dict pattern — fields maps key-string → Matchable for each field
DictPattern: [type [fields: Dict  rest: Boolean]]

[instance Matchable DictPattern
  [try-match: [fn [let dp val]
    [if [not [dict? val]]
      Absent.Absent
      [match-fields dp.fields val dp.rest]]]]
  [narrow: [fn [let dp typ]
    # For each field, ask its Matchable to narrow; collect binding types
    [if [not [consistent-subtype? typ TypeNode.Dict]] Absent.Absent
      [narrow-fields dp.fields typ]]]]]

# Type guard — check type, then delegate to inner pattern
TypeGuard: [type [typ: Type  inner: Any]]

[instance Matchable TypeGuard
  [try-match: [fn [let tg val]
    [if [not [value-matches-type? val tg.typ]]
      Absent.Absent
      [try-match tg.inner val]]]]
  [narrow: [fn [let tg scrutinee-typ]
    # If scrutinee type is inconsistent with tg.typ, arm is dead
    [if [not [consistent-subtype? scrutinee-typ tg.typ]] Absent.Absent
      # Ask inner to narrow within the now-confirmed type
      [narrow tg.inner tg.typ]]]]]

# Constructor pattern
ConstructorPattern: [type [tag: String  payload: Any]]

[instance Matchable ConstructorPattern
  [try-match: [fn [let cp val]
    [if [not [= [tag-of val] cp.tag]]
      Absent.Absent
      [try-match cp.payload [payload-of val]]]]]
  [narrow: [fn [let cp scrutinee-typ]
    # Narrow scrutinee to the specific constructor's payload type
    [payload-typ: [constructor-payload-type cp.tag scrutinee-typ]]
    [if [absent? payload-typ] Absent.Absent
      [narrow cp.payload payload-typ]]]]]

# Predicate — any boolean function becomes a pattern; opaque to type checker
[instance Matchable [Fn@Bool [Any]]
  [try-match: [fn [let pred val]
    [if [pred val] [] Absent.Absent]]]
  [narrow: [fn [let _ _] []]]]       # no bindings, no narrowing info
```

### Parser Desugaring

The parser translates existing pattern syntax into Matchable construction calls:

| Surface syntax | Desugars to |
|---|---|
| `_` | `WildcardPattern.Wildcard` |
| `h` (in `[let h]`) | `Bind "h"` |
| `42` | `42` (Int literal, Matchable via instance) |
| `"hello"` | `"hello"` (String literal) |
| `x@Int` | `TypeGuard Int (Bind "x")` |
| `[host: h  port: p]` | `DictPattern {fields: {host: Bind "h", port: Bind "p"}, rest: true}` |
| `[Option.Some p]` | `ConstructorPattern "Option.Some" (Bind "p")` |
| `Color.Red` | `ConstructorPattern "Color.Red" WildcardPattern` |

### Combinators as Library Functions

```tinct
# Or-pattern — try a, fall back to b
or-pattern: [fn [let a@Matchable b@Matchable]
  ...]

# Guard — pattern must match AND predicate must hold  
guard-pattern: [fn [let inner@Matchable pred]
  ...]

# As-pattern — bind whole value AND match inner pattern
as-pattern: [fn [let name@String inner@Matchable]
  ...]

# Not — succeed exactly when inner fails
not-pattern: [fn [let inner@Matchable]
  ...]
```

### Type Narrowing and Exhaustiveness

`narrow` is what makes the type checker understand user-defined patterns. When
the type checker processes a match arm, it calls `narrow pattern scrutinee-type`
at the type stage:

- **Success arm typing**: the dict returned by `narrow` gives the types of all
  bindings in the arm body. The scrutinee itself is narrowed to the intersection
  of its original type with what this pattern accepts.

- **Failure typing**: in subsequent arms (or after the full match), the
  scrutinee is known NOT to have matched, providing further narrowing. For
  simple cases (like `TypeGuard Int`), the failure type is `scrutinee-type ∖ Int`.

- **Dead arm detection**: if `narrow` returns `Absent.Absent` for the
  scrutinee's type, the arm can never fire. The type checker emits a warning.

- **Exhaustiveness**: a match over type `T` is exhaustive when the union of what
  all arms' `narrow` functions accept covers `T`. For algebraic types this is
  structural; for user-defined patterns it is whatever `narrow` declares.

Example — after matching `x@Int`, the body knows `x: Int`:

```tinct
[match val
  [case [let x] x@Int   [+ x 1]]   # type checker knows x: Int here
  [case [let s] s@String [str s]]]   # type checker knows s: String here
```

The type checker calls `narrow (TypeGuard Int (Bind "x")) (typeof val)` for the
first arm, which returns `{x: TypeNode.Int}`. It then types the body with `x:
Int` in scope.

Example — user-defined `Range` pattern with narrowing:

```tinct
Range: [type [from: Int  to: Int]]

[instance Matchable Range
  [try-match: [fn [let r val]
    [if [and [>= val r.from] [< val r.to]] [value: val] Absent.Absent]]]
  [narrow: [fn [let r typ]
    # Range only applies to Int; binding "value" has type Int
    [if [not [consistent-subtype? typ TypeNode.Int]] Absent.Absent
      [value: TypeNode.Int]]]]]
```

A pattern that provides no useful `narrow` (returning `[]` for any type)
still works at runtime — it just contributes no static type information, and
the type checker must assume the worst (bindings have type `Any`).

### match Arms

A match arm's key position is a Matchable expression. The `[case [let names]
pattern body]` form:

```tinct
[match data
  [case [let h p] [host: h  port: p]      [str h ":" [str p]]]
  [case [let h p] [host: h  port: $default-port]  [str h " (default)"]]
  [case [let _]   _                         "other"]]
```

The `[let h p]` section declares that `h` and `p` will be introduced as new
scope variables. `$default-port` in the pattern pins against the existing scope
value of `default-port`. The evaluator calls `try-match pattern value`, extracts
the bindings dict, projects the names declared in `[let]` into scope, then
evaluates `body`.

Simple arms with no bindings need no `[case]` wrapper:

```tinct
[match color
  Color.Red:   "#ff0000"
  Color.Green: "#00ff00"
  42:          "forty-two"
  ...:         "other"]
```

### Function Parameters

Each entry in a `[fn [let ...] body]` parameter list is a Matchable. At call
time the evaluator calls `try-match` for each argument against its parameter
pattern:

```tinct
# Current — still valid, params are simple Bind patterns
[fn [let x@Int  y@String] [str x " " y]]

# New — destructuring first param
[fn [let [host: h  port: p]] [str h ":" [str p]]]

# New — constructor param
[fn [let [Option.Some v]  extra@Int] [+ v extra]]

# New — any predicate as param guard
[fn [let x@[>= 0]] [sqrt x]]
```

### Multi-Clause Functions

Multi-clause functions are expressed with a dedicated `[fn-clauses ...]` form
(since multiple `[fn ...]` entries in a dict parse as separate positional
entries, not clauses of one function):

```tinct
fib: [fn-clauses
  [[let 0]    0]
  [[let 1]    1]
  [[let n@Int] [+ [fib [- n 1]] [fib [- n 2]]]]]
```

Each clause is `[[let patterns...] body]`. The evaluator tries clauses in order,
using the first whose param Matchables all succeed. If no clause matches, the
call is a runtime error (or a type error when the type checker can prove
exhaustiveness).

### User-Defined Patterns

Any value implementing `Matchable` is a valid pattern:

```tinct
# Range pattern — with narrow for type-checker integration
Range: [type [from: Int  to: Int]]

[instance Matchable Range
  [try-match: [fn [let r val]
    [if [and [>= val r.from] [< val r.to]]
      [value: val]
      Absent.Absent]]]
  [narrow: [fn [let r typ]
    [if [not [consistent-subtype? typ TypeNode.Int]] Absent.Absent
      [value: TypeNode.Int]]]]]

# Usage
[match score
  [case [let v] [Range 90 100] [str "A: " v]]
  [case [let v] [Range 80 90]  [str "B: " v]]
  ...:                          "below B"]

# Regex pattern (hypothetical, requires regex library)
[instance Matchable RegexPattern
  [try-match: [fn [let rx val]
    [if [not [str? val]] Absent.Absent
      [m: [regex-match rx val]]
      [if [absent? m] Absent.Absent
        [groups: m]]]]]]

# Any predicate function is already a Matchable via the Fn@Bool instance
[match name
  [starts-with? "Dr."]: "doctor"
  [ends-with? "PhD"]:   "academic"
  ...:                  "other"]
```

## What Would Change

### Parser

**Current:** Two separate parsing paths — `[let x y z]` produces flat
`SurfaceParam` lists; match arm keys are parsed via `surface_node_to_pattern`.

**Proposed:** `[let ...]` contents are parsed as pattern expressions that
produce Matchable values. `[fn-clauses ...]` is a new parser production.
Existing arm pattern syntax is retained and desugared to Matchable construction
calls.

**Impact:** Moderate. The `LetDecl` parse frame is extended to produce patterns
instead of flat name lists.

### AST

**Current:** `SurfaceParam { name, annotation, variadic }` is separate from the
`Pattern` enum.

**Proposed:** `SurfaceParam` is replaced by `SurfaceNode` in pattern position —
function params are Matchable expressions like any other. `Pattern` enum is
retired once all pattern forms are Matchable constructions.

**Impact:** Major. Touches every pass that handles params.

### Evaluator

**Current:** Function call dispatch does simple name→value binding. Match arms
use hard-coded Rust pattern matching.

**Proposed:** Call dispatch calls `builtin-try-match` for each arg against its
param Matchable. Match arm evaluation calls `builtin-try-match` for each arm's
pattern. Rust no longer understands pattern structure — it only knows how to
call `try-match` and inject the resulting bindings dict.

**Impact:** Fundamental, but the Rust surface shrinks significantly.

### Type Checker

**Current:** Param types are extracted from `@Type` annotations. Match arm
type narrowing and exhaustiveness are hard-coded in Rust for the closed set of
pattern forms.

**Proposed:** The type checker calls `narrow pattern scrutinee-type` at the
type stage for each arm. The returned binding-type dict populates the typing
context for that arm's body. Dead arms are detected when `narrow` returns
`Absent.Absent`. Exhaustiveness is checked by verifying that the union of all
arms' `narrow` accepts covers the scrutinee type. For user-defined patterns,
the type checker is only as precise as what `narrow` declares — patterns that
return opaque `narrow` results fall back to `Any` for their bindings.

**Impact:** Major. The type checker's pattern analysis becomes a type-stage
evaluation of `narrow` rather than direct AST inspection. This is consistent
with how the type-stage is already used for type constructor resolution.

### Prelude

**Current:** No pattern types exist. Match and function call dispatch are
fully Rust-internal.

**Proposed:** Prelude gains `WildcardPattern`, `Bind`, `DictPattern`,
`TypeGuard`, `ConstructorPattern`, and combinator functions. The `Matchable`
typeclass is declared in prelude alongside `Equatable`, `Comparable`, etc.

**Impact:** Additive. Prelude grows; no existing prelude code is broken.

### Resolver

**Current:** Resolves param names from flat `SurfaceParam` list. Resolves match
arm binding names from the `[let ...]` section.

**Proposed:** Resolver walks the Matchable expression in `[let]` to collect the
names that `try-match` will produce, then assigns de Bruijn coordinates to them.
The `[let]` section remains the resolver's source of binding declarations.

**Impact:** Moderate.

## Bootstrap Strategy

Only three things must exist in Rust:

1. **`builtin-try-match`** — invokes the `try-match` method on a Matchable value
   via typeclass dispatch. Returns `Dict | Absent`.

2. **`[match]` loop** — iterates arms in order, calls `builtin-try-match` for
   each arm's pattern value, evaluates the first matching arm's body with
   bindings injected into scope. Rust controls scope injection and lazy body
   evaluation but knows nothing about pattern structure.

3. **`[fn]` call dispatcher** — calls `builtin-try-match` for each arg against
   each param Matchable, collects bindings, evaluates body if all succeed.

There is no circularity in bootstrapping. Prelude defines `Matchable` instances
using primitives (`if`, `=`, `dict?`, `tag-of`, `absent?`) that do not
themselves use Matchable. Once prelude loads, all pattern forms are available.

## Prior Work

This proposal builds on three completed whatifs:

- **[`completed/pattern-matching.md`](completed/pattern-matching.md)** —
  Established `[match]` as a first-class form and the `[case [let ...] pattern
  body]` structure. Matchable generalizes the closed pattern set defined there
  into an open, user-extensible typeclass.

- **[`completed/unified-bindings.md`](completed/unified-bindings.md)** —
  Established `[let ...]` as the self-announcing binding introducer across all
  contexts (fn params, class params, match arms). This invariant — `[let ...]`
  always means binding declaration — is the foundation that makes the merged
  pattern form unambiguous.

- **[`completed/narrowing.md`](completed/narrowing.md)** — Established
  path-sensitive type narrowing for built-in conditions. The `narrow` method in
  `Matchable` is the user-extensible generalization of this: each pattern carries
  a type-level proof that the type checker uses to narrow types in the success arm,
  detect dead arms, and check exhaustiveness.

## Prerequisites

- The typeclass system is implemented (required for `Matchable` declaration).
- `builtin-try-match` requires TypeContext to be initialized before first use
  (handled by the existing bootstrap sequence in `get_builtin_core_type_env`).
- `fn-clauses` syntax requires a parser change.
- Exhaustiveness analysis for Matchable-based patterns is separate future work.

## References

- Wadler, P. (1987). "Views: A way for pattern matching to cohabit with data
  abstraction." *POPL '87*, pp. 307–313. — Original motivation for open/user-defined patterns.
- Tullsen, M. (2000). "First class patterns." *PADL 2000*, pp. 1–15. — Patterns as
  first-class values in a functional language.
- Scala 3 extractor objects (unapply/unapplySeq). — Closest practical
  implementation of user-defined patterns in a mainstream language.
- Clojure core.match library. — Extensible pattern matching via protocol dispatch.
- Rust `std::ops` trait design. — Reference for making built-in operations into
  implementable traits without special-casing in the compiler.
