# What If: Parse-Stage Macros for tinct

**State:** Proposal

What would it take to let macros declare context-sensitive parsing rules —
specifically, different key-identity semantics for their body — rather than
operating only on fully-formed AST dicts?

**Note:** The original motivating use case — `[defmacro match]` needing
context-sensitive key identity for match arms — is no longer applicable.
Match is implemented as `Expr::Match` (a Rust special form) with dedicated
parser support for arm syntax. See `doc/whatif/pattern-matching.md` §Why a
Special Form. This proposal provides the mechanism for user-defined macros
that need context-sensitive parsing.

## Current State

tinct's macro system (`doc/whatif/macros.md`) operates post-parse: macros receive
`Expr` AST dicts and return AST dicts. The parser always produces a complete,
fully-formed AST — including duplicate-key detection — before any macro runs.

Duplicate-key detection fires at **parse time** and uses **bare name** as key
identity: `n@Int` and `n@String` are both field `"n"` → duplicate error.
This is correct for regular dicts. But it makes dict-syntax forms with
annotated keys impossible when the same name appears with different annotations.

### The Core Problem: Key Identity is Context-Dependent

Consider a hypothetical macro using dict syntax where each key is annotated
and each value is a body:

```tinct
[dispatch x
    n@Int:    i"int: $n"
    n@String: i"str: $n"   # PARSE ERROR: duplicate key "n"
    _:        "other"]
```

`n@Int` and `n@String` carry different annotations — they should coexist as
distinct entries. But the parser sees two entries with key name `"n"` and
rejects them as duplicates.

This is **not fixable post-parse**. The rejection happens at parse time,
before any macro runs. No amount of post-parse transformation can recover
the entries from a rejected parse.

**Note:** For `[match]` specifically, this problem is solved by the parser's
dedicated match-arm parsing mode (part of the `Expr::Match` special form).
The problem persists for user-defined macros that want similar syntax.

### Example: A Hypothetical Dict-Syntax Macro

```tinct
[dispatch x
    n@Int:                            i"int: $n"
    n@String:                         i"str: $n"
    n@[type: Int  is: [> _ 0]]: i"positive int: $n"
    _:                                "other"]
```

- Key = full annotated expression (bare name + annotation, taken together)
- Value = body expression
- Duplicate detection uses full annotated expression equality: `n@Int ≠ n@String`
- Regular dicts remain unchanged: `[n@Int: 1  n@String: "2"]` is still a
  duplicate-`n` error

Parse-stage macros are the mechanism that makes this possible for user-defined
forms. (`[match]` achieves this via dedicated parser support instead.)

### What's Missing

1. **Context-sensitive key identity** — no mechanism for a macro to declare
   that its body uses full-annotated-expression equality instead of bare-name
   equality for duplicate detection
2. **Syntax classes** — no way for a macro to declare parse modes for argument
   positions (follows from key identity as a special case)
3. **New infix operators** — no way to register user-defined operators with
   precedence

## Why Parse-Stage Macros Matter for tinct

**Dict-syntax macros without compromise.** User-defined macros that use dict
syntax with annotated keys need context-sensitive key identity. Post-parse
macros cannot recover from a parse-time duplicate rejection. Parse-stage macros
let a macro declare that its body uses full-annotated-expression identity rather
than bare-name identity for duplicate detection.

**Regular dicts are unchanged.** `[n@Int: 1  n@String: "2"]` remains a
duplicate-`n` error in all contexts without a syntax class override.

**Extensibility.** The mechanism for context-sensitive key identity generalizes
to other syntax classes and operator precedence registration.

**Note:** `[match]` was the original motivating use case but is now handled
by `Expr::Match` with dedicated parser support. Parse-stage macros provide
the general mechanism for user-defined forms that need similar syntax
flexibility.

## Design

Parse-stage macros for tinct are **scoped and minimal** — not full reader
macros (character-level hooks). The central capability is syntax classes:
macros declare parse modes for their bodies, including key identity rules.

### Capability 1: Syntax Classes with Context-Sensitive Key Identity

A macro declares a **syntax class** that specifies:
- How argument positions are parsed
- What counts as a duplicate key within its body

```tinct
# Declare that [dispatch scrutinee arms-dict] parses arms in "annotated-arms" mode
[declare-syntax dispatch
  [scrutinee: expr
   arms: annotated-arms-dict]]

# annotated-arms-dict: a dict where key identity = full annotated expression
[syntax-class annotated-arms-dict
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

**A user-defined macro** processes the arm dict, dispatching on key shape:
- Key `VarRef("_")` → wildcard entry
- Key `VarRef("n")` (bare, no annotation) → variable binding entry
- Key `Annotated("n", Simple("Int"))` → type-constrained entry
- Key `Annotated("n", PropertyDict(...))` → type + predicate-guarded entry
- Key `VarRef("Int")` (uppercase bare word) → type dispatch entry
- Key `Int(42)` → literal entry
- Key `Dict([ok: VarRef("v")])` → dict pattern entry

### Capability 2: Syntax Classes for Argument Positions

Beyond key identity, syntax classes can declare parse modes for specific
argument positions — useful when a sub-expression should be parsed differently
from a regular expression.

Example: a `[dispatch]` macro could declare its second positional argument
as an annotated-arms dict (syntax class `annotated-arms-dict`). A `[regex pattern]` macro could
declare that `pattern` is parsed in regex mode (different tokenization rules
than tinct expressions).

Capability 2 generalizes key identity to all argument positions.

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
[declare-syntax dispatch
  [scrutinee: expr  arms: annotated-arms-dict]]

[syntax-class annotated-arms-dict
  key-identity: full-annotated-expr
  values: expr]
```

These declarations are themselves parsed normally (no bootstrap problem —
they are `[...]` forms parsed before the registration pass). Loaded alongside
`stdlib/macros.llt`.

### `stdlib/macros.llt` — User Macros with Syntax Classes

User-defined macros that opt into a syntax class receive structured dicts with
full-expression key identity. The macro dispatches on the key node's `type:` field:

- `"var"` → bare variable reference
- `"annotated"` → name + annotation
- `"literal"` → literal value
- `"dict"` → nested dict pattern

**Note:** `[match]` is no longer implemented via macros — it is `Expr::Match`
with dedicated parser and type checker support. This section describes the
general mechanism available to user-defined macros.

### `doc/whatif/pattern-matching.md`

Match arm syntax is now handled by the parser's dedicated match-arm parsing
mode as part of `Expr::Match`. No parse-stage macro interaction needed.
See `doc/whatif/pattern-matching.md` for current match syntax.

## Prerequisites

- **`[defmacro]`** (`doc/whatif/macros.md`) — parse-stage macros extend, not replace. Fully implemented (`macro-integration` sprint, 2026-05-05).
- **`doc/whatif/structural-contracts.md`** — `validate` builtin is the throwing boundary-check; `is:` is the Bool-returning predicate convention for annotations.

## References

- tinct `doc/whatif/macros.md` — base macro system; parse-stage macros extend it
- tinct `doc/whatif/macro-rewrite.md` — macro-based desugaring; match excluded (now `Expr::Match`)
- tinct `doc/whatif/structural-contracts.md` — `validate` builtin (throwing) vs `is:` annotation predicate (Bool-returning) distinction
- tinct `doc/whatif/pattern-matching.md` — pattern matching as `Expr::Match` special form
- Tobin-Hochstadt, S. et al. (2011). "Languages as Libraries." *PLDI '11*, pp. 132–141. ACM. — Racket's `#lang` mechanism: language-level parse customization; tinct's syntax classes are a minimal, safe subset scoped to individual macro bodies rather than whole files
- Flatt, M. (2016). "Binding as sets of scopes." *POPL '16*, pp. 705–717. ACM. — hygiene for macro-introduced bindings; syntax class dispatch must maintain hygiene across parse-mode boundaries
- Pratt, V.R. (1973). "Top down operator precedence." *POPL '73*, pp. 41–51. ACM. — Pratt parsing for operator precedence; the algorithm for Capability 3's infix operator extension
- Ford, B. (2002). "Packrat Parsing: Simple, Powerful, Lazy, Linear Time." *ICFP '02*, pp. 36–47. ACM. — PEG parsing as an alternative for extensible grammars; context for why tinct's approach is a narrower, targeted extension rather than a full PEG rewrite
