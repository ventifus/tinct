# What If: Parse-Stage Macros for tinct

**State:** Proposal

What would it take to let macros declare context-sensitive parsing rules —
specifically, different key-identity semantics for their body — rather than
operating only on fully-formed AST dicts?

## Current State

tinct's macro system (`doc/whatif/macros.md`) operates post-parse: macros receive
`Expr` AST dicts and return AST dicts. The parser always produces a complete,
fully-formed AST — including duplicate-key detection — before any macro runs.

Duplicate-key detection fires at **parse time** and uses **bare name** as key
identity: `n@Int` and `n@String` are both field `"n"` → duplicate error.
This is correct for regular dicts. But it makes pattern matching via dict
syntax impossible for variable-binding arms.

### The Core Problem: Key Identity is Context-Dependent

Consider using dict syntax for match arms — each key is a pattern spec,
each value is a body:

```tinct
[match x
    n@Int:    i"int: $n"
    n@String: i"str: $n"   # PARSE ERROR: duplicate key "n"
    _:        "other"]
```

`n@Int` and `n@String` are different patterns — they match different types
and bind the same name `n` to different values. But the parser sees two
entries with key name `"n"` and rejects them as duplicates.

This is **not fixable post-parse**. The rejection happens at parse time,
before any macro runs. No amount of post-parse transformation can recover
the match arms from a rejected parse.

The same problem does not exist for type-dispatch arms (each type is a
distinct key) or wildcards (`_` is unique), only for **variable-binding
arms** where the variable name is the same across arms.

### What the Right Match Syntax Looks Like

```tinct
[match x
    n@Int:                            i"int: $n"
    n@String:                         i"str: $n"
    n@[type: Int  is: [> _ 0]]: i"positive int: $n"
    _:                                "other"]
```

- Key = full pattern spec (bare name + annotation, taken together)
- Value = body expression
- `is:` in the annotation is the guard predicate — a `Fn@Bool [Any]`; `true` = arm fires, `false` = fall through
- Duplicate detection uses full annotated expression equality: `n@Int ≠ n@String`
- Regular dicts remain unchanged: `[n@Int: 1  n@String: "2"]` is still a
  duplicate-`n` error

This is the ideal match syntax — pure tinct dict structure, no infix keywords,
guards via `is:` annotation, clean separation of pattern and body. Parse-
stage macros are the mechanism that makes it possible.

### What's Missing

1. **Context-sensitive key identity** — no mechanism for a macro to declare
   that its body uses full-annotated-expression equality instead of bare-name
   equality for duplicate detection
2. **Syntax classes** — no way for a macro to declare parse modes for argument
   positions (follows from key identity as a special case)
3. **New infix operators** — no way to register user-defined operators with
   precedence (secondary, addressed in Phase 3)

## Why Parse-Stage Macros Matter for tinct

**Match syntax without compromise.** Variable-binding arms with different type
constraints need to coexist in a single match form. Post-parse macros cannot
recover from a parse-time duplicate rejection. Parse-stage macros let `[match]`
declare that its body uses pattern-identity (full annotated expression) rather
than field-identity (bare name).

**Guards are annotations, not keywords.** With full annotated key identity,
`n@[is: [> _ 0]]` is a complete pattern spec — the guard lives in the
`is:` property alongside the `type:` constraint. No `when` keyword, no infix syntax, no new constructs.

**Regular dicts are unchanged.** `[n@Int: 1  n@String: "2"]` remains a
duplicate-`n` error in all non-match contexts. The override applies only
inside `[match ...]` — the one context where the annotation is a pattern
discriminator, not a parameter type.

**Future extensibility.** Once the mechanism exists for context-sensitive
key identity, it generalizes to other syntax classes and operator precedence
(Phase 2 and 3 below).

## Design

Parse-stage macros for tinct are **scoped and minimal** — not full reader
macros (character-level hooks). The central capability is syntax classes:
macros declare parse modes for their bodies, including key identity rules.

### Capability 1: Syntax Classes with Context-Sensitive Key Identity

A macro declares a **syntax class** that specifies:
- How argument positions are parsed
- What counts as a duplicate key within its body

```tinct
# Declare that [match scrutinee arms-dict] parses arms in "match-arms" mode
[declare-syntax match
  [scrutinee: expr
   arms: match-arms-dict]]

# match-arms-dict: a dict where key identity = full annotated expression
[syntax-class match-arms-dict
  key-identity: full-annotated-expr    # n@Int ≠ n@String ≠ n
  values: expr]
```

**What `key-identity: full-annotated-expr` means:**

| Key form | Bare-name identity | Full-annotated identity |
|---|---|---|
| `n` | `"n"` | `VarRef("n")` |
| `n@Int` | `"n"` (same — duplicate!) | `Annotated("n", Simple("Int"))` |
| `n@String` | `"n"` (same — duplicate!) | `Annotated("n", Simple("String"))` |
| `n@[type: Int  is: [> _ 0]]` | `"n"` | `Annotated("n", PropertyDict([type: Int, is: ...]))` |
| `_` | `"_"` | `VarRef("_")` |

Under full-annotated identity, `n@Int` and `n@String` are structurally
different nodes — no collision. Two `n@Int` entries would still be flagged as
duplicates (same annotated form). `_: body1 _: body2` would also be a
duplicate (both bare `_`).

**What the parser produces** for the match arm dict is unchanged — it still
produces `[type: "dict" entries: [...]]` with each entry as `[type: "entry" key: ...
value: ...]`. The difference is that duplicate detection runs against the
full key node, not just the extracted name. The macro receives the same AST
dict shape it always would; the parse-stage change is only in what counts
as a collision.

**The match macro** then processes the arm dict:
- Key `VarRef("_")` → wildcard arm
- Key `VarRef("n")` (bare, no annotation) → variable binding arm
- Key `Annotated("n", Simple("Int"))` → type-constrained binding arm
- Key `Annotated("n", PropertyDict(...))` → type + `is:`-guarded binding arm
- Key `VarRef("Int")` (uppercase bare word) → type pattern arm (no binding)
- Key `Int(42)` → literal pattern arm
- Key `Dict([ok: VarRef("v")])` → dict pattern arm

### Capability 2: Syntax Classes for Argument Positions

Beyond key identity, syntax classes can declare parse modes for specific
argument positions — useful when a sub-expression should be parsed differently
from a regular expression.

Example: the second positional argument to `[match]` is always the arms dict
(syntax class `match-arms-dict`). Future: a `[regex pattern]` macro could
declare that `pattern` is parsed in regex mode (different tokenization rules
than tinct expressions).

For tinct's near-term needs this is secondary to key identity — the match
design above only requires Capability 1. Capability 2 generalizes it.

### Capability 3: Operator Registration (Secondary)

A macro can register a new infix operator with declared precedence:

```tinct
[declare-operator >>>
  associativity: left
  precedence:    between [. |]]
```

Adds `>>>` to the infix precedence table. Expanded by `[defmacro >>>]`.
This is independent of Capabilities 1 and 2 and can ship separately.

### What Is NOT Included

**Full reader macros (character-level hooks)** — the lexer handles character-
level tokenization and must remain in Rust for security and error recovery.
User code does not touch the character stream.

**Arbitrary tokenization modes** — regex `/pattern/`, SQL keywords, etc.
require embedded lexers. Out of scope.

**Modal lexers** — switching tokenization rules mid-file. Not needed for
tinct's use cases.

## What Would Change

### `src/parser.rs` — Syntax Class Registry

**Current:** Fixed duplicate-key detection using bare name extraction.

**Proposed:** Before parsing `[name ...]` args, check the syntax class registry
for `name`. If a class with `key-identity: full-annotated-expr` is found, use
structural equality of the full key node for duplicate detection. Otherwise
use the current bare-name extraction.

The registry is populated during a **registration pass** that runs after
parsing `[declare-syntax ...]` forms at the top of a file, before the
main parse of expressions.

**Impact:** Moderate. Registry lookup is O(1) per `[name ...]` form. Key
identity dispatch adds a branch to the existing duplicate-check logic.

### `stdlib/syntax.llt` (new file)

```tinct
[declare-syntax match
  [scrutinee: expr  arms: match-arms-dict]]

[syntax-class match-arms-dict
  key-identity: full-annotated-expr
  values: expr]
```

These declarations are themselves parsed normally (no bootstrap problem —
they are `[...]` forms parsed before the registration pass). Loaded alongside
`stdlib/macros.llt`.

### `stdlib/macros.llt` — `[defmacro match]` Update

The `[defmacro match]` from `doc/whatif/macro-rewrite.md` is updated to
receive structured arm dicts with full-expression key identity. Pattern
dispatch reads the key node's `type:` field:

- `"var"` + name `"_"` → wildcard
- `"var"` + lowercase name → variable binding
- `"var"` + uppercase name → type pattern (`Int`, `Str`, `Bool`)
- `"annotated"` → extract name for binding + annotation for constraints
- `"literal"` → literal match (`42`, `"hello"`, `true`)
- `"dict"` → dict pattern (`[ok: v]`)

`is:` in annotation property dicts → guard predicate (`Fn@Bool [Any]`) called with the bound value; `true` fires the arm, `false` falls through.

### `doc/whatif/pattern-matching.md`

**Proposed:** Update the pattern matching syntax from:
```tinct
[match x  Int [+ x 1]  _ x]   # positional pairs
```
to:
```tinct
[match x  n@Int: [+ n 1]  _: x]   # dict with full-annotated key identity
```
The value proposition: natural tinct dict syntax, type constraints and guards
in the key, no infix keywords, no guard separator conventions.

## Phased Adoption

### Phase 1: Syntax Class Registry + Match Arm Key Identity

Implement the registration pass and the `key-identity: full-annotated-expr`
syntax class option. Scope: unblock `[match]` with variable-binding arms.

- `src/parser.rs`: registration pass; `key-identity` dispatch in duplicate detection
- `stdlib/syntax.llt`: `[declare-syntax match ...]` + `[syntax-class match-arms-dict ...]`
- `stdlib/macros.llt`: `[defmacro match]` updated to receive structured arm dicts
- `doc/whatif/pattern-matching.md`: update syntax examples
- Tests: `n@Int` and `n@String` coexist in match; `n@Int` twice is still duplicate;
  regular dicts unaffected; `is:`-guarded arms; wildcard; literal patterns

### Phase 2: General Syntax Classes

Generalize: any macro can declare a syntax class. Argument-position parse modes
beyond key identity.

- `stdlib/syntax.llt`: full `[declare-syntax]` / `[syntax-class]` forms
- Tests: custom user macros with syntax classes

### Phase 3: Operator Registration

Add Pratt precedence layer to the infix loop. `[declare-operator]` for new
infix operators.

- `src/parser.rs`: Pratt precedence climb; operator registry
- `stdlib/syntax.llt`: `[declare-operator]` form
- Tests: precedence relative to `.` and `|`; associativity

### Prerequisites

- **Macros Phase 2** (`[defmacro]`) — parse-stage macros extend, not replace
- **`doc/whatif/macro-rewrite.md` Phase 2** — `[defmacro match]` updated here
- **`doc/whatif/structural-contracts.md`** — `validate` builtin is the throwing boundary-check; `is:` is the Bool-returning predicate convention for annotations

### Trigger

**Phase 1:** When `[defmacro match]` with variable-binding arms needs to
distinguish `n@Int` from `n@String` as different patterns. Concrete: as soon
as pattern matching Phase 2 (`[match]` basic) lands — the duplicate-key
problem appears immediately with the natural dict arm syntax.

**Phase 2:** When a second macro needs syntax classes.

**Phase 3:** When library authors need interoperating infix operators.

## References

- tinct `doc/whatif/macros.md` — base macro system; parse-stage macros extend it
- tinct `doc/whatif/macro-rewrite.md` — `[defmacro match]` updated in Phase 1; depends on syntax classes
- tinct `doc/whatif/structural-contracts.md` — `validate` builtin (throwing) vs `is:` annotation predicate (Bool-returning) distinction
- tinct `doc/whatif/pattern-matching.md` — pattern matching syntax updated to use dict arm form
- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — Racket's `#lang` mechanism: language-level parse customization; tinct's syntax classes are a minimal, safe subset scoped to individual macro bodies rather than whole files
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — hygiene for macro-introduced bindings; syntax class dispatch must maintain hygiene across parse-mode boundaries
- Pratt, V.R. (1973). "Top down operator precedence." *POPL '73*, pp. 41–51. ACM. — Pratt parsing for operator precedence; the algorithm for Capability 3's infix operator extension
- Ford, B. (2002). "Packrat Parsing: Simple, Powerful, Lazy, Linear Time." *ICFP '02*, pp. 36–47. ACM. — PEG parsing as an alternative for extensible grammars; context for why tinct's approach is a narrower, targeted extension rather than a full PEG rewrite
