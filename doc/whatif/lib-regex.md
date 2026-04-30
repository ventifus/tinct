# What If: Pure-Tinct Regular Expression Engine

**State:** Proposal

What would it take to implement a full regular expression engine
entirely in pure-tinct, with no Rust builtins and no crate dependency?

## Current State

tinct has no pattern matching for strings. The only string inspection
tools are `$split`, `$replace`, `$upper`, `$lower`, `$trim`, and the
pure-tinct predicates in `stdlib/strings.llt` (`str-contains?`,
`str-starts-with?`, `str-ends-with?`). These cover simple cases but
cannot express structured patterns.

The `stdlib/strings.llt` predicates are implemented by splitting on the
needle and checking part counts — correct for containment but unable to
handle patterns like character classes, quantifiers, alternation, or
anchors:

```lisp
# Current: test if string needs YAML quoting
# Must enumerate every special character individually
yaml-needs-quoting?: [fn [s]
  [call $or [call $= "" $s]
    [call $or [call $str-contains? ":" $s]
      [call $or [call $str-contains? "#" $s]
        # ... 12 more cases
        ]]]]
```

### What's Missing

1. **Pattern matching** — no way to test a string against a character
   class, quantifier, alternation, or anchor.
2. **Capture groups** — no way to extract structured substrings
   (hostname, port, path) from unstructured strings.
3. **Pattern-based replace and split** — `$replace` works on literal
   strings only.

## Why a Pure-Tinct Regex Engine Matters

**Serialization helpers become writable in one line.** `yaml-quote-string`,
`toml-escape`, `nginx-escape` all reduce to a single `re-match` call
instead of a chain of `str-contains?` checks.

**The NFA is a tinct value.** Because the compiled state machine is a
tinct dict, users can inspect it, cache it at file level (lazy
evaluation memoizes the thunk automatically), extend it with custom
transition rules, or write alternative runners. This is not possible
with an opaque Rust builtin.

**No crate dependency.** Thompson's NFA simulation achieves O(nm)
matching — the same asymptotic complexity as the `regex` crate's DFA
approach — with no new Cargo dependency and no unsafe code.

**Primitives compose.** The engine is built on the string utilities
from `stdlib/strings.llt` (Phase 1 of `doc/whatif/lib-supplemental.md`)
and the bitwise primitives from Phase 4 of the same doc (`$char-code`
for character class range comparison). Once those primitives exist,
the regex engine is pure library code users can read and modify.

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

```lisp
[type: concat   left: ...  right: ...]         # ab
[type: alt      left: ...  right: ...]         # a|b
[type: star     child: ...]                    # a*
[type: plus     child: ...]                    # a+
[type: opt      child: ...]                    # a?
[type: repeat   min: 1  max: 3  child: ...]   # a{1,3}
[type: char     code: 97]                      # literal 'a' ($char-code)
[type: any]                                    # .
[type: class    ranges: [[lo: 48  hi: 57]]  negate: false]  # [0-9]
[type: anchor   kind: start]                   # ^
[type: anchor   kind: end]                     # $
[type: group    id: 1  child: ...]             # (...)
[type: named    id: 1  name: host  child: ...] # (?P<name>...)
```

Character class ranges are stored as integer codepoints from `$char-code`,
so `[a-z]` becomes `[lo: 97  hi: 122]`.

### NFA State Representation

Each NFA state is a tinct dict. The full NFA is a 0-indexed dict of
states:

```lisp
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
    1: host
    2: port
  ]
]
```

### Thompson's Construction (Key Cases)

```lisp
# stdlib/regex.llt — NFA compiler (excerpt)

# Fresh state id = current state count
nfa-new-state: [fn [nfa]
  [call $length $nfa.states]]

# Literal character: two states, one transition
nfa-char: [fn [code nfa0]
  [call $let
    [s0 [call $nfa-new-state $nfa0]
     s1 [call $+ $s0 1]
     nfa1 [call $nfa-add-state $s0 [$code [list $s1]] [] false nfa0]
     nfa2 [call $nfa-add-state $s1 [] [] false nfa1]]
    [entry: $s0  exit: $s1  nfa: $nfa2]]]

# Concatenation: ε from exit of left to entry of right
nfa-concat: [fn [left right nfa0]
  [call $let
    [nfa1 [call $nfa-add-epsilon $left.exit $right.entry $nfa0]]
    [entry: $left.entry  exit: $right.exit  nfa: $nfa1]]]

# Alternation: new entry ε-forks to both; both exits ε to new accept
nfa-alt: [fn [left right nfa0]
  [call $let
    [entry  [call $nfa-new-state $nfa0]
     accept [call $+ $entry 1]
     nfa1   [call $nfa-add-state $entry [] [list $left.entry $right.entry] false $nfa0]
     nfa2   [call $nfa-add-epsilon $left.exit  $accept $nfa1]
     nfa3   [call $nfa-add-epsilon $right.exit $accept $nfa2]
     nfa4   [call $nfa-add-state $accept [] [] true $nfa3]]
    [entry: $entry  exit: $accept  nfa: $nfa4]]]

# Kleene star: new entry and accept; child loops back via ε
nfa-star: [fn [child nfa0]
  [call $let
    [entry  [call $nfa-new-state $nfa0]
     accept [call $+ $entry 1]
     nfa1   [call $nfa-add-state $entry []
               [list $child.entry $accept] false $nfa0]
     nfa2   [call $nfa-add-epsilon $child.exit $child.entry $nfa1]
     nfa3   [call $nfa-add-epsilon $child.exit $accept $nfa2]
     nfa4   [call $nfa-add-state $accept [] [] true $nfa3]]
    [entry: $entry  exit: $accept  nfa: $nfa4]]]
```

### NFA Simulation — Thompson's VM

The simulator maintains a dict mapping `state-id → capture-snapshot`.
At each character, all active states advance on that character and
the ε-closure of the resulting states is computed:

```lisp
# captures: Dict[group-id → [start: Int  end: Int]]
# active:   Dict[state-id → captures]

nfa-step: [fn [nfa active char]
  [call $nfa-epsilon-closure $nfa
    [call $flat-map
      [fn [entry]
        [call $let [sid $entry.key  caps $entry.value]
          [call $nfa-char-transitions $nfa $sid $char $caps]]]
      [call $entries $active]]]]

nfa-run: [fn [nfa s]
  [call $fold
    [fn [active char] [call $nfa-step $nfa $active $char]]
    [call $nfa-epsilon-closure $nfa [call $make-entry $nfa.start []]]
    [call $str-chars $s]]]

nfa-accepts: [fn [nfa s]
  [call $any?
    [fn [e] [call $get accept [call $get $e.key $nfa.states]]]
    [call $entries [call $nfa-run $nfa $s]]]]
```

### Public API

**`re-compile pattern`** → `NFA dict` — parses and compiles the pattern.
Returns the NFA as an inspectable tinct dict. Lazy evaluation
memoizes the result automatically when bound at file level:

```lisp
# Compiled once, reused on every call — lazy eval handles caching
ip-pattern: [call $re-compile "([0-9]{1,3}\\.){3}[0-9]{1,3}"]
ip?: [fn [s] [call $re-match-compiled $ip-pattern $s]]
```

**`re-match pattern s`** → `Bool` — tests whether `s` contains a match.

**`re-find pattern s`** → `[match: String  start: Int  end: Int  ...]`
or `[]` — first match with named capture groups as additional keys:

```lisp
[call $re-find "(?P<host>[a-z0-9.-]+):(?P<port>[0-9]+)" "db.prod:5432"]
# → [match: "db.prod:5432"  host: "db.prod"  port: "5432"  start: 0  end: 12]
```

**`re-findall pattern s`** → `Dict` — all non-overlapping matches,
same shape as `re-find`.

**`re-replace pattern replacement s`** → `String` — replaces all
matches. Capture group references via `\1`, `\2`, or `\k<name>`.

**`re-split pattern s`** → `Dict` — parts of `s` between matches,
same shape as `$split` output.

### Interaction with Lazy Evaluation

The NFA dict is a tinct value — lazy by default. `re-compile` returns
a thunk that forces on first use and is memoized thereafter. Patterns
bound at document level compile once per document evaluation. There is
no per-call compilation overhead if the pattern is a literal bound to
a name.

### Interaction with the Type Checker

`re-match` has type `String → String → Bool`. `re-find` returns an
open record type — the fixed keys (`match`, `start`, `end`) plus
additional keys for named capture groups whose names are not statically
known. The return type is `[match: String  start: Int  end: Int  ...]`
(open record) or `[]` (empty dict). Type inference infers `Any` for
named capture group access until a `TypeAssert` annotation constrains
it.

## What Would Change

### Standard Library (`stdlib/regex.llt`)

**Current:** No regex support.

**Proposed:** New file `stdlib/regex.llt` implementing the full engine:
parser, NFA compiler, NFA simulator, and the public API functions
(`re-compile`, `re-match`, `re-find`, `re-findall`, `re-replace`,
`re-split`, `re-match-compiled`). Loaded automatically alongside
`prelude.llt`.

**Impact:** Major — large new stdlib file (~400–600 lines), but zero
change to `src/builtins.rs` or `Cargo.toml`.

### Dependencies on Other Modules

**`stdlib/strings.llt`** (Phase 1 of `doc/whatif/lib-supplemental.md`):
`str-chars` for iterating pattern and input characters; `str-find` for
locating match positions.

**Phase 4 bitwise primitives** (`doc/whatif/lib-supplemental.md`):
`$char-code` for converting characters to integer codes for transition
table lookup and character class range comparisons. Without `$char-code`,
character classes (`[a-z]`, `\d`, `\w`) cannot be implemented.

**Impact:** Phase 3 (this proposal) must be delivered after both Phase 1
and Phase 4 of `lib-supplemental.md`.

## Phased Adoption

### Phase 1: Core NFA Engine

Implement the parser, compiler, and simulator for the core constructs:
literals, `.`, `*`, `+`, `?`, concatenation, alternation, `^`/`$`
anchors, and unnamed groups. Ships as `stdlib/regex.llt`.

**What it enables:** `re-match` and basic `re-find` with positional
capture groups. Sufficient for `yaml-quote-string`, CIDR validation,
hostname checks.

### Phase 2: Full Syntax

Add character classes (`[a-z]`, `[^...]`, `\d`, `\w`, `\s`), named
capture groups (`(?P<name>...)`), counted repetition (`{n,m}`),
and `re-replace` with capture group back-references.

**What it enables:** Full regex expressiveness. Named group extraction
for structured parsing.

### Phase 3: Performance Optimization

Replace the pure-tinct simulation loop with a Rust builtin
`$nfa-run nfa-dict s` that interprets the same dict representation
at native speed. The public API is unchanged — the tinct wrappers
call the Rust runner instead of the pure-tinct one. This phase is
purely an optimization and can be deferred indefinitely.

### Prerequisites

- **Phase 1 of `lib-supplemental.md`** complete — `str-chars`,
  `str-find` required.
- **Phase 4 of `lib-supplemental.md`** complete — `$char-code`
  required for character classes (needed even in Phase 1 of this doc,
  since literal matching uses `$char-code` for transition lookup).
  Deliver this doc after Phase 4 of `lib-supplemental.md`.

### Trigger

- When `str-contains?` chains in serialization helpers grow beyond
  ~5 checks and a single `re-match` would be cleaner.
- When the first use case requiring capture group extraction arises
  (structured string parsing, config validation with error attribution).

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
  construction (NFA from regex AST). This doc implements §3.7 only;
  subset construction (§3.7.1) is not needed for NFA simulation.
- doc/whatif/lib-supplemental.md — prerequisite stdlib modules:
  Phase 1 (string utilities) and Phase 4 (bitwise primitives including
  `$char-code`).
