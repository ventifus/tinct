# Tinct Quick Reference

A concise reference for writing tinct programs. For full details see the relevant `doc/*.md` chapters.

---

## Everything is Brackets

Tinct has one syntactic form: `[...]`. Whether it's a dict, a function call, or a declaration depends on what's inside.

```tinct
[
  empty:   []                          # empty dict (also serves as null)
  keyed:   [k: "v"  k2: "v2"]        # dict with string keys
  indexed: [0: "a"  1: "b"  2: "c"]  # dict with integer keys
  auto:    ["a" "b" "c"]              # same — auto-indexed
  result:  [+ 1 2]                    # function call → 3
  fn-def:  [fn [let x] [* x 2]]      # function definition
  branch:  [if true "yes" "no"]       # conditional → "yes"
]
```

**The parser rule:** if the first element is a bare word (not followed by `:`), it's a call. If the first element is a keyword (`fn`, `match`, `type`, `let`, `class`, `instance`, `macro`), it's that construct. Otherwise it's a dict.

---

## Dict

```tinct
[
  person:  [name: "Alice"  age: 30]
  items:   ["first" "second" "third"]  # auto-indexed: 0, 1, 2
  merged:  [merge [a: 1] [a: 2  b: 3]] # → [a: 2  b: 3] (right-biased)
  empty:   []                           # null/empty dict
]
```

`[]` is null. `[null? []]` → `true`.

---

## Dot Access

```tinct
[
  person:  [name: "Alice"  address: [city: "Portland"]]
  list:    ["first" "second" "third"]

  name:    person.name          # → "Alice"
  city:    person.address.city  # chained → "Portland"
  first:   list.0               # integer key → "first"
  second:  list.0               # list.1 → "second"
]
```

For dynamic or computed keys, use `get`:

```tinct
[
  person:  [name: "Alice"  age: 30]
  field:   "name"

  static:  [get "name" person]    # → "Alice"
  dynamic: [get field person]     # → "Alice"
  indexed: [get 0 ["x" "y"]]     # → "x"
  nested:  [get-in person ["address" "city"]]
  safe:    [get-or person "missing" "default"]  # → "default"
  exists:  [has? "name" person]   # → true
]
```

---

## Functions

```tinct
[
  # Named functions are just dict entries
  double:  [fn [let x] [* x 2]]
  add:     [fn [let x y] [+ x y]]

  # With type annotations
  greet:   [fn@String [let name@String] [str "Hello, " name "!"]]

  # Full metadata form
  clamp:   [fn@[return: Number  doc: "Clamp n between lo and hi"]
             [let n@Number lo@Number hi@Number]
             [if [< n lo] lo [if [> n hi] hi n]]]

  # Optional parameter with default
  repeat:  [fn [let s@String n@[type: Int  default: 3]]
             [join "" [take n [cycle [s]]]]]

  # Variadic
  sum:     [fn [let ...nums] [reduce + 0 nums]]

  # Calling them
  r1:      [double 5]         # → 10
  r2:      [add 3 4]          # → 7
  r3:      [greet "world"]    # → "Hello, world!"
  r4:      [sum 1 2 3 4 5]   # → 15
]
```

**`_` shorthand** — desugars to a single-argument lambda:

```tinct
[
  nums:    [1 2 3 4 5]
  doubled: [map [* _ 2] nums]          # [fn [_] [* _ 2]]
  adults:  [filter [>= _.age 18] []]   # [fn [_] [>= _.age 18]]
]
```

---

## Pipelines

The `|` operator threads a value left-to-right through a sequence of functions. It desugars entirely before evaluation — no runtime overhead.

```tinct
lhs | [f args]    →  [f args lhs]    # lhs appended as last positional arg
lhs | name        →  [name lhs]      # bare word: call name with lhs
```

`|` is left-associative:

```tinct
users | each | [filter _.active] | [map _.name] | collect
# desugars to: [collect [map _.name [filter _.active [each users]]]]
```

**Generator functions** — explode dicts into lazy Seqs and back:

```tinct
[
  users: [
    0: [name: "Alice"  active: true]
    1: [name: "Bob"    active: false]
    2: [name: "Carol"  active: true]
  ]

  # Explode → transform → collect
  names:   [users | each | [map _.name] | collect]
  # → [0: "Alice"  1: "Bob"  2: "Carol"]

  active:  [users | each | [filter _.active] | [map _.name] | collect]
  # → [0: "Alice"  1: "Carol"]

  squares: [[range 0 5] | [map [* _ _]] | collect]
  # → [0: 0  1: 1  2: 4  3: 9  4: 16]

  # Dynamic field access
  config:  [name: "prod"  host: "example.com"]
  field:   "host"
  value:   [config | [get field]]  # → "example.com"
]
```

**Threading with `->` (alternative for runtime stage lists):**

```tinct
[[-> [1 2 3 4 5]
  [filter [> _ 2]]
  [map [* _ 10]]
  collect]]
# → [0: 30  1: 40  2: 50]
```

---

## Types and Annotations

`@` attaches a type to a name or expression:

```tinct
[
  # Parameter and return types
  add:    [fn@Number [let x@Number y@Number] [+ x y]]

  # Union type
  lenient: [fn [let x@[or Int String]] [str x]]

  # Parameterized type
  first:  [fn@a [let xs@[Seq a]] [head xs]]

  # Inline record type
  show:   [fn@String [let p@[name: String  age: Int]]
            [str p.name " is " p.age]]

  # TypeVar (polymorphic)
  identity: [fn@a [let x@a] x]

  # Runtime type assertion
  n:      [@Int 42]   # errors if not Int at runtime
]
```

Type declarations and construction:

```tinct
[type [Ok payload] [Error String]]  # sum type — payload is a TypeVar
[type [Red] [Green] [Blue]]         # enum (unit variants)

[
  success: [Ok 42]
  failure: [Error "not found"]
  color:   [Red]                    # unit variant — no argument

  # Matching on them
  value:   [match success
              [Ok v]:    v          # → 42
              [Error e]: [raise e]]

  hex:     [match color
              [Red]:   "#ff0000"
              [Green]: "#00ff00"
              [Blue]:  "#0000ff"]
]
```

---

## Pattern Matching

Patterns appear directly as `pattern: body` pairs inside `[match ...]`:

```tinct
[
  data: [host: "localhost"  port: 8080]

  # Structural dict match
  url: [match data
    [host: h  port: p]: [str h ":" p]
    _:                   "unknown"]
  # → "localhost:8080"

  # Multiple patterns
  label: [match 42
    0:   "zero"
    42:  "the answer"
    _:   "other"]
  # → "the answer"
]
```

**Brackets are required for constructor patterns.** A bare name is a *variable capture* — it matches anything:

```tinct
[
  color: [Red]

  # Correct — [Red] is the constructor pattern
  hex:   [match color
    [Red]:   "#ff0000"
    [Green]: "#00ff00"
    _:       "unknown"]

  # WRONG — Red: is a variable capture, always matches first arm
  # [match color  Red: "#ff0000"  Green: "#00ff00"  _: "unknown"]
]
```

---

## Error Handling

Errors propagate automatically through the thunk graph. Unused values never error.

```tinct
[
  # raise — always errors, never returns
  validated: [fn [let port]
    [if [and [>= port 1] [<= port 65535]]
      port
      [raise [str "invalid port: " port]]]]

  # try — returns [Ok value] or [Error message]
  result:  [try [fn [] [+ 1 "oops"]]]
  # → [Error "type mismatch: ..."]

  # match on the result
  value:   [match [try [fn [] [+ 1 2]]]
    [Ok v]:    v   # → 3
    [Error _]: 0]

  # try-or — shorthand for the common fallback pattern
  safe:    [try-or [fn [] [raise "boom"]] "default"]
  # → "default"
]
```

---

## Lazy Evaluation

Values are computed only when accessed. This means:

- Unused dict entries never evaluate — neither in the output nor in intermediate scope-chain dicts
- `filter`/`map` return lazy sequences; only accessed elements are computed
- Recursive structures like `[cons 1 ones]` are fine — they expand on demand
- `[materialize v]` forces a value one level deep (WHNF) — on a Seq, evaluates the head but leaves the tail lazy
- `[collect xs]` forces a lazy Seq to completion, producing a concrete integer-keyed Dict — use this to fully realize a sequence

```tinct
# 'slow' never evaluates — it's unused in the next expression
[fast: [+ 1 1]  slow: [raise "never runs"]]
[used: fast]    # → 2; slow never forced
```

```tinct
# collect vs materialize on sequences
[
  lazy:  [filter [> _ 3] [range 0 10]]
  first: [materialize lazy]   # forces head only → 4
  all:   [collect lazy]       # forces full spine → [0: 4  1: 5  2: 6  ...]
]
```

---

## Common Prelude

These functions are always available — no `include` needed.

**Sequences:** `map`, `filter`, `reduce`, `each`, `take`, `drop`, `head`, `tail`, `cons`, `range`, `collect`, `flat-map`, `reverse`, `sort`, `sort-by`, `group-by`, `partition`, `first`, `last`, `rest`, `length`, `concat`, `join`

**Dicts:** `merge`, `keys`, `get`, `get-or`, `set`, `has?`, `build-dict`, `from-entries`, `entries`, `values`, `map-entries`, `deep-merge`

**Strings:** `str`, `split`, `join`, `trim`, `upper`, `lower`, `starts-with?`, `ends-with?`, `str-length`, `str-slice`, `str-contains?`, `pad-left`, `pad-right`

**Logic:** `if`, `when`, `unless`, `and`, `or`, `not`, `=`, `<`, `>`, `<=`, `>=`

**Types:** `int?`, `str?`, `dict?`, `fn?`, `seq?`, `null?`, `bool?`, `float?`, `bytes?`, `num?`

**Math:** `+`, `-`, `*`, `/`, `mod`, `floor`, `round`, `min`, `max`, `abs`

**Control:** `try`, `raise`, `try-or`, `and-then`, `identity`, `compose`, `->` (pipe)

**I/O:** `emit`, `lines`, `open`, `write`, `flush`, `close`, `stat`, `exists`, `make-dir`, `rename`, `env`, `list-dir`, `narrow`, `string-handle`, `read-chunk`

**Async:** `task`, `await`, `channel`, `send`, `recv`, `select-once`, `context`, `with-timeout`, `with-cancel`, `loop-select`, `retry`, `finally`, `exit`, `graceful-exit`, `await-all`, `recv-all`, `par-map`, `par-filter`

---

## The Two-Dict Library Convention

The canonical pattern for library files: **first dict is private, second is exported**.

```tinct
[
  # Private: helpers not visible to callers
  step: [fn [let acc x] [cons [+ x 1] acc]]
]
[
  # Public: the exported API
  increment-all: [fn [let xs] [reverse [reduce step [] xs]]]
]
```

The first dict's names are in scope when the second dict evaluates. Callers who `[include %libdir "mylib.llt"]` receive only the second dict.

**Why this matters:**
- Avoids naming collisions with caller scope
- Lets helpers be refactored freely without breaking the API
- Documents intent: first dict = internals, second = contract

For very simple libraries with no private state, a single dict is fine.

---

## Including Libraries

```tinct
# Named include — access functions via dot
[strings: [include %libdir "strings.llt"]]
[strings.pad-left "42" 6 "0"]   # → "000042"
```

```tinct
# Bare include — names flow into next expression's scope
[include %libdir "strings.llt"]
[result: [pad-left "42" 6 "0"]]
```

The `%libdir` capability points at the stdlib directory. The `include` function is self-hosted in prelude.

---

## Document Header Capabilities

Capabilities are injected by the CLI and declared with `--- caps:`:

```tinct
--- caps: [%fs: @DirCap  %net: @NetCap]
[
  config: [slurp %fs "config.json"]
  result: [fetch %net "https://api.example.com"]
]
```

Standard injected caps (no declaration needed):

- `%` — pipeline input (previous document's output)
- `%libdir` — DirCap for stdlib directory
- `%pwd` — DirCap for the working directory

Conditional caps (only when specific CLI flags are given):

- `%stdin` — readable handle for stdin (only when `-i` / `--input` is provided)

---

## Style Notes

- **Name with kebab-case:** `my-function`, `parse-timestamp`, `max-retries`
- **Predicates end in `?`:** `valid?`, `empty?`, `admin?`
- **Private names in first dict, public in second** (two-dict convention)
- **Annotate exported functions** with `fn@[return: T  doc: "..."]` — enables LSP hover and documentation
- **Prefer shallow nesting** — use intermediate dict entries rather than deeply nested calls
- **`[]` is null** — use it as a sentinel, default value, and empty collection
- **Let lazy evaluation work** — avoid `[materialize v]` or `[collect v]` unless you genuinely need immediate forcing
