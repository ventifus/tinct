# Pure-Tinct Regular Expression Engine

## Overview

tinct implements a full regular expression engine entirely in pure-tinct,
with no Rust builtins and no crate dependency. The engine uses Thompson's
NFA simulation (Thompson 1968), achieving O(nm) matching with no
exponential blowup.

The NFA is a tinct value — a dict that users inspect, cache at file level
(lazy evaluation memoizes the thunk automatically), extend with custom
transition rules, or run with alternative simulators. This is not possible
with an opaque Rust builtin. Serialization helpers such as
`yaml-quote-string`, `toml-escape`, and `nginx-escape` reduce to a single
`re-match` call. The engine depends only on `char-code` (from
`doc/feature/lib-supplemental.md` §Bitwise Primitives) and string
dual-dispatch — no new Cargo dependency, no unsafe code.

## Design

The engine uses **Thompson's NFA simulation** (Thompson 1968):

1. **Parse** the pattern string into a regex AST dict.
2. **Compile** the AST into an NFA dict via Thompson's construction.
3. **Simulate** the NFA character-by-character, maintaining a set of
   active states. ε-closure is computed after each step.

This gives O(nm) matching (n = string length, m = pattern length) with
no exponential blowup. The entire pipeline — parse, compile, simulate —
operates on tinct dicts.

### Supported Syntax

| Construct | Syntax | Example |
|-----------|--------|---------|
| Literal | `a` | matches `a` |
| Any character | `.` | matches any char except newline |
| Character class | `[a-z0-9]` | matches chars in ranges |
| Negated class | `[^a-z]` | matches chars not in ranges |
| Kleene star | `a*` | zero or more `a` |
| Plus | `a+` | one or more `a` |
| Optional | `a?` | zero or one `a` |
| Repetition | `a{n,m}` | between n and m `a` |
| Concatenation | `ab` | `a` followed by `b` |
| Alternation | `a\|b` | `a` or `b` |
| Group | `(ab)` | capturing group |
| Named group | `(?P<name>ab)` | named capture |
| Anchor start | `^` | beginning of string |
| Anchor end | `$` | end of string |

### Regex AST Representation

The parser produces a recursive dict structure where every node has a
`type` key:

```tinct
[type: "concat"   left: ...  right: ...]         # ab
[type: "alt"      left: ...  right: ...]         # a|b
[type: "star"     child: ...]                    # a*
[type: "plus"     child: ...]                    # a+
[type: "opt"      child: ...]                    # a?
[type: "repeat"   min: 1  max: 3  child: ...]   # a{1,3}
[type: "char"     code: 97]                      # literal 'a' (char-code)
[type: "any"]                                    # .
[type: "class"    ranges: [[lo: 48  hi: 57]]  negate: false]  # [0-9]
[type: "anchor"   kind: "start"]                 # ^
[type: "anchor"   kind: "end"]                   # $
[type: "group"    id: 1  child: ...]             # (...)
[type: "named"    id: 1  name: "host"  child: ...] # (?P<name>...)
```

Character class ranges are stored as integer codepoints via `char-code`,
so `[a-z]` becomes `[lo: 97  hi: 122]`. Literal character matching
also uses `char-code` to convert the input character to an Int for
transition table lookup — this is the only Bitwise Primitive the engine
depends on.

### NFA State Representation

Each NFA state is a tinct dict. The full NFA is a 0-indexed dict of
states:

```tinct
# A single state
[
  transitions: [        # char-code → [state-id ...]
    97: [1]             # on 'a' (code 97), go to state 1
  ]
  epsilon: [3 4]        # ε-transitions — free moves, no character consumed
  accept: false
  group-start: []       # capture group ids that open at this state
  group-end: []         # capture group ids that close at this state
]

# Full NFA
[
  states: [
    0: [transitions: [97: [1]]  epsilon: []  accept: false  group-start: []  group-end: []]
    1: [transitions: []         epsilon: []  accept: true   group-start: []  group-end: []]
  ]
  start: 0
  groups: [             # group id → name ("" for unnamed groups)
    1: "host"
    2: "port"
  ]
]
```

### Thompson's Construction (Key Cases)

```tinct
# stdlib/regex.llt — NFA compiler (excerpt)

# Fresh state id = current state count
nfa-new-state: [fn [nfa]
  [length nfa.states]]

# Literal character: two states, one transition
nfa-char: [fn [code nfa0]
  [let
    [s0 [nfa-new-state nfa0]
     s1 [+ s0 1]
     nfa1 [nfa-add-state s0 [code [list s1]] [] false nfa0]
     nfa2 [nfa-add-state s1 [] [] false nfa1]]
    [entry: s0  exit: s1  nfa: nfa2]]]

# Concatenation: ε from exit of left to entry of right
nfa-concat: [fn [left right nfa0]
  [let
    [nfa1 [nfa-add-epsilon left.exit right.entry nfa0]]
    [entry: left.entry  exit: right.exit  nfa: nfa1]]]

# Alternation: new entry ε-forks to both; both exits ε to new accept
nfa-alt: [fn [left right nfa0]
  [let
    [entry  [nfa-new-state nfa0]
     accept [+ entry 1]
     nfa1   [nfa-add-state entry [] [list left.entry right.entry] false nfa0]
     nfa2   [nfa-add-epsilon left.exit  accept nfa1]
     nfa3   [nfa-add-epsilon right.exit accept nfa2]
     nfa4   [nfa-add-state accept [] [] true nfa3]]
    [entry: entry  exit: accept  nfa: nfa4]]]

# Kleene star: new entry and accept; child loops back via ε
nfa-star: [fn [child nfa0]
  [let
    [entry  [nfa-new-state nfa0]
     accept [+ entry 1]
     nfa1   [nfa-add-state entry []
               [list child.entry accept] false nfa0]
     nfa2   [nfa-add-epsilon child.exit child.entry nfa1]
     nfa3   [nfa-add-epsilon child.exit accept nfa2]
     nfa4   [nfa-add-state accept [] [] true nfa3]]
    [entry: entry  exit: accept  nfa: nfa4]]]
```

### NFA Simulation — Thompson's VM

The simulator maintains a dict mapping `state-id → capture-snapshot`.
At each character, all active states advance on that character and
the ε-closure of the resulting states is computed.

With string dual-dispatch (from `doc/feature/lib-supplemental.md`
§Strings as Character Sequences), strings participate directly in `fold`
as sequences of single-character strings. Each character in the fold step
is a one-char String; `char-code` converts it to an Int for transition
table lookup.

```tinct
# captures: Dict[group-id → [start: Int  end: Int]]
# active:   Dict[state-id → captures]

nfa-step: [fn [nfa active char]
  [nfa-epsilon-closure nfa
    [flat-map
      [fn [entry]
        [let [sid entry.key  caps entry.value]
          [nfa-char-transitions nfa sid char caps]]]
      [entries active]]]]

nfa-run: [fn [nfa s]
  [fold
    [fn [active char] [nfa-step nfa active char]]
    [nfa-epsilon-closure nfa [make-entry nfa.start []]]
    s]]   # fold directly over String — dual-dispatch iterates characters

nfa-accepts: [fn [nfa s]
  [any?
    [fn [e] [get "accept" [get e.key nfa.states]]]
    [entries [nfa-run nfa s]]]]
```

### Type Definitions

**`Pattern`** — the structural dict type returned by `re-compile`:

```tinct
[
  NfaState: [type [
    transitions: @Dict
    epsilon:     @Seq@Int
    accept:      @Bool
    group-start: @Seq@Int
    group-end:   @Seq@Int
  ]]

  NfaDict: [type [
    states: @Seq@NfaState
    start:  @Int
    groups: @Dict
  ]]

  MatchResult: [type [
    match: @String   # matched substring
    start: @Int      # character offset of match start in haystack
    end:   @Int      # character offset of match end (exclusive)
    # ... additional @String fields: one per named capture group
  ]]
]
```

`Pattern` uses `Value::Variant { tag: "Pattern", payload: nfa-dict }`
— a nominal variant wrapper. This ensures provenance: only `re-compile`
produces a `Pattern`; an arbitrary dict cannot accidentally match the
type even if it has the right shape. The NFA dict is accessible via
`payload`:

```tinct
[compiled: [re-compile "a+"]]
[nfa: [payload compiled]]   # → NfaDict; inspect states, start, groups
nfa.states                  # → Seq@NfaState

# Write a custom runner against the same NFA:
[my-run: [fn [s] [my-nfa-sim nfa s]]]
```

`re-match pattern@[String Pattern] ...` — the type checker accepts
either a `String` (compiles on the fly) or a `Pattern` variant
(uses the pre-compiled NFA directly). Nominal wrapping prevents
structural spoofing.

### Public API

All functions that accept a pattern accept **`String | Pattern`**. When
given a `String`, the pattern is compiled on the fly. When given a
`Pattern`, the pre-compiled NFA is used directly.

```tinct
# Internal dispatch (pure-tinct):
re-ensure-compiled: [fn [pattern]
  [if [str? pattern]
    [re-compile pattern]
    pattern]]   # already a Pattern
```

**Caching pre-compiled patterns:** bind at file level — lazy evaluation
memoizes the `re-compile` call automatically:

```tinct
# Compiled once on first use; zero overhead on subsequent calls
ip-pattern: [re-compile "([0-9]{1,3}\\.){3}[0-9]{1,3}"]

# Pass the Pattern directly — no recompilation:
ip?: [fn [s@String] [re-match ip-pattern s]]
```

---

**`re-compile pattern@String`** → `Pattern`

Parses and compiles the pattern string into a `Pattern` value. The
inner NFA dict is accessible via `[payload p]` for inspection or
custom runners.

**`re-match pattern@[String|Pattern] s@String`** → `Bool`

Tests whether `s` contains a match anywhere. Compiles `pattern` if
it is a `String`; uses the pre-compiled NFA if it is a `Pattern`.

**`re-find pattern@[String|Pattern] s@String`** → `Dict`

Returns the first match as a dict, or `[]` if no match:

```tinct
[re-find "(?P<host>[a-z0-9.-]+):(?P<port>[0-9]+)" "db.prod:5432"]
# → [match: "db.prod:5432"  host: "db.prod"  port: "5432"  start: 0  end: 12]

[re-find ip-pattern "no match here"]
# → []
```

Fixed keys: `match` (the matched substring), `start`, `end` (byte
offsets). Named capture groups add their names as additional keys.

**`re-findall pattern@[String|Pattern] s@String`** → `Seq`

All non-overlapping matches in order, each a dict of the same shape
as `re-find`.

**`re-replace pattern@[String|Pattern] replacement@String s@String`** → `String`

Replaces all non-overlapping matches. Capture group references in
`replacement` via `\1`, `\2`, or `\k<name>`, `\0` (whole match).

**Security note:** if `replacement` comes from untrusted input, an
attacker can inject `\k<name>` to expand arbitrary named capture groups.
Use `re-escape-replacement` to make a replacement string safe:

```tinct
# re-escape-replacement — escape a string so it is treated as a literal
# replacement (no back-reference expansion)
re-escape-replacement: [fn@String [s@String]
  [replace "\\" "\\\\" s]]   # escape backslashes; \1 becomes \\1
```

Use this whenever the replacement string comes from user input or config values.

**`re-split pattern@[String|Pattern] s@String`** → `Seq@String`

Parts of `s` between matches. **Zero-length match policy:** if the
pattern can match the empty string, zero-length matches at the boundary
of a previous match are skipped (same behaviour as Python 3.7+ and
PCRE2). A zero-length match at the very start of the string produces
a leading empty string. This avoids infinite empty-string production
while preserving n-matches → n+1-parts semantics for non-ambiguous
patterns.

### Interaction with Lazy Evaluation

The NFA dict is a tinct value — lazy by default. `re-compile` returns
a thunk that forces on first use and is memoized thereafter. Patterns
bound at document level compile once per document evaluation. There is
no per-call compilation overhead if the pattern is a literal bound to
a name.

### Interaction with the Type Checker

```tinct
re-compile : [fn@Pattern              [pattern@String]]

re-match   : [fn@Bool                 [pattern@[String Pattern]  s@String]]

re-find    : [fn@[MatchResult Dict]   # Dict = [] (empty dict) on no match
               [pattern@[String Pattern]  s@String]]

re-findall : [fn@Seq@MatchResult      [pattern@[String Pattern]  s@String]]

re-replace : [fn@String               [pattern@[String Pattern]
                                       replacement@String
                                       s@String]]

re-split   : [fn@Seq@String           [pattern@[String Pattern]  s@String]]
```

`Pattern` is the nominal variant type produced by `re-compile` —
distinct from `String` and `Dict`. The first argument of every
function is a union `[String | Pattern]`.

`re-find` returns an open record `[match: String  start: Int  end: Int
...String]` — named capture groups add keys whose names are not
statically known, so their values infer as `String` with `Any` access
at the call site until narrowed by `TypeAssert`. The empty-dict case
`[]` is the "no match" sentinel; callers check `[empty? result]`.

## Implementation

### Standard Library (`stdlib/regex.llt`)

New file implementing the full engine: parser, NFA compiler, NFA
simulator, and public API (`re-compile`, `re-match`, `re-find`,
`re-findall`, `re-replace`, `re-split`). Loaded automatically alongside
`prelude.llt`. Approximately 400–600 lines; zero change to
`src/builtins.rs` or `Cargo.toml`.

### Evaluator Builtins (`src/builtins.rs`)

No new Rust builtins for the engine itself. A future performance
sprint can add `nfa-run nfa-dict s` as a Rust builtin that interprets
the same dict representation at native speed without changing the
public API.

## Dependencies

- `char-code` from `doc/feature/lib-supplemental.md` §Bitwise Primitives
  — required for character class range comparison and transition table
  lookup. This is the only Bitwise Primitive the engine uses.
- String dual-dispatch (`fold`/`map`/`filter` on `String`) from
  `doc/feature/lib-supplemental.md` §Strings as Character Sequences —
  enables `nfa-run` to fold directly over input strings without an
  explicit `str-chars` call.
- `str-find` from `doc/feature/lib-supplemental.md` §Extended String
  Utilities — used for locating match start positions.
- `contains?` (substring search for String, from prelude generalization)
  — used internally for pattern validation.

## References

- Thompson, K. (1968). "Regular expression search algorithm."
  *Communications of the ACM*, 11(6), 419–422. — Original NFA
  simulation algorithm; the basis for this engine. O(nm) matching
  with no exponential blowup.
- Cox, R. (2007). "Regular Expression Matching Can Be Simple And Fast."
  swtch.com/~rsc/regexp/regexp1.html. — Explains why Thompson NFA
  simulation avoids the exponential backtracking that plagues Perl-style
  engines; confirms O(nm) bound. The argument for implementing this
  approach in pure-tinct rather than wrapping a crate.
- Aho, A.V., Lam, M.S., Sethi, R. & Ullman, J.D. (2006). *Compilers:
  Principles, Techniques, and Tools* (2nd ed.), §3.6–3.8. — Thompson's
  construction (NFA from regex AST). This implementation covers §3.7
  only; subset construction (§3.7.1) is not needed for NFA simulation.
- doc/feature/lib-supplemental.md — prerequisite stdlib modules:
  `char-code` (Bitwise Primitives), string dual-dispatch (Strings as
  Character Sequences), `str-find` (Extended String Utilities).
