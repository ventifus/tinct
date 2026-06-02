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
  safe:    [get-or "missing" "default" person]  # → "default"
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
             [join "" [take n [cycle [cons s []]]]]]

  # Variadic
  sum:     [fn [let ...nums] [reduce + 0 nums]]

  # Calling them
  r1:      [double 5]         # → 10
  r2:      [add 3 4]          # → 7
  r3:      [greet "world"]    # → "Hello, world!"
  r4:      [sum 1 2 3 4 5]   # → 15
]
```

**Multi-body functions** — a function body can be multiple dict expressions. All but the last are intermediate bindings (local scope); only the last is returned:

```tinct
[
  # Intermediate dict establishes local names; final expression is returned.
  normalize: [fn [let xs]
    [total: [reduce + 0 xs]]
    [map [fn [let x] [/ x total]] xs]]

  # Chained intermediate dicts — each can see the previous.
  describe: [fn [let n]
    [sign:  [if [< n 0] "negative" "non-negative"]]
    [label: [str sign " (" n ")"]]
    label]

  r1: [normalize [3 1 6]]              # → [0: 0.3  1: 0.1  2: 0.6]
  r2: [describe -42]                   # → "negative (-42)"
]
```

Intermediate dicts are letrec-scoped among themselves but do not appear in the function's return value — they are discarded after the final expression evaluates.

**Laziness note:** intermediate dict entries are thunks — they are only forced when accessed by subsequent expressions. Writing `[a: [expensive-call]]` as an intermediate body does not compute `expensive-call` unless `a` is used.

**`begin` / `>>` — forced sequencing for side effects** — when you need to evaluate an expression for its side effect and discard the result:

```tinct
[>> [send channel value]   # forced even though result is discarded
    next-value]            # returned

[begin [cleanup] [log "done"] result]   # multiple steps; last is returned
```

`begin` (and its alias `>>`) evaluates each expression eagerly in order and returns the last. Unlike intermediate dict bodies (which are lazy), `begin` is an explicit escape from laziness — use it only when side effects must occur regardless of whether the result is consumed. `>>` mirrors Haskell's monadic sequence operator: "evaluate for effect, discard, continue." `begin` is named after the Scheme `begin` special form.

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

The `|` operator threads a value left-to-right through a sequence of functions. It desugars entirely before evaluation — no runtime overhead. The LHS is appended as the **last** positional argument:

```tinct
[
  nums: [1 2 3 4 5]

  # [f args] form — LHS appended as last arg
  doubled: [nums | [map [* _ 2]]]   # same as [map [* _ 2] nums]
  halved:  [nums | [map [/ _ 2]]]   # same as [map [/ _ 2] nums]

  # bare name form — LHS is the only arg
  first:   [nums | head]            # same as [head nums]
]
```

`|` is left-associative. In a multi-file pipeline, `%` is the output of the previous stage:

```tinct
# Stage 1 — produce the data
[
  users: [
    0: [name: "Alice"  active: true]
    1: [name: "Bob"    active: false]
    2: [name: "Carol"  active: true]
  ]
]
```

```tinct
# Stage 2 — % is {users: [...]} from stage 1
# Chains reduce left-to-right (left-associative):
#   %.users | each | [filter [fn [let u] u.active]] | [map [fn [let u] u.name]] | collect
# desugars to: [collect [map <fn> [filter <fn> [each %.users]]]]
[
  all-names:    [collect [map [fn [let u] u.name] [each %.users]]]
  # → [0: "Alice"  1: "Bob"  2: "Carol"]

  active-names: [collect [map [fn [let u] u.name] [filter [fn [let u] u.active] [each %.users]]]]
  # → [0: "Alice"  1: "Carol"]

  squares: [[range 0 5] | [map [fn [let x] [* x x]]] | collect]
  # → [0: 0  1: 1  2: 4  3: 9  4: 16]
]
```

**Pipeline LHS: bare name vs `$name`** — a bare name in bracket head position is always an implied call (`[f]` = call `f`). To use a binding's *value* as the pipeline LHS, prefix it with `$` for an explicit variable reference:

```tinct
[
  handle: [open %cwd "data.txt" Readable]

  # WRONG — [handle | lines] calls handle as a zero-arg function
  # CORRECT — [$handle | lines] uses handle's value as the LHS
  text: [$handle | lines | collect | [join "\n"]]
]
```

**Threading with `->` (alternative for runtime stage lists):**

```tinct
[result: [collect [map [* _ 10] [filter [> _ 2] [each [1 2 3 4 5]]]]]]
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
[
  [type Color [Red] [Green] [Blue]]   # enum — unit variants

  color: Red      # unit variant as a value

  hex: [match color
    [Red _]:   "#ff0000"
    [Green _]: "#00ff00"
    [Blue _]:  "#0000ff"]
  # → "#ff0000"
]
```

Sum types with payloads use `try` in the standard library:

```tinct
[
  # try returns [Ok value] on success, [Error msg] on failure
  result: [try [fn [] [+ 1 2]]]

  # Pattern match on the result
  value: [match result
    [Ok v]:    v     # → 3
    [Error _]: 0]
]
```

---

## Nominal Variants

`[type ...]` declarations auto-generate constructor functions. After the declaration, the constructor name is callable:

```tinct
[
  [type Option [Some value: Int] [None]]   # auto-generates constructors in this scope

  # Unit variant: just use the name
  nothing: None

  # Payload variant: call the constructor with named field
  something: [Some value: 42]

  # Pattern match — [Tag v] binds the payload to v
  n: [match something
        [Some v]: v     # v is the payload → 42
        [None _]: 0]
]
```

**Constructors are ordinary values** — unit variants can be used directly as values:

```tinct
[
  [type Color [Red] [Green] [Blue]]
  c: Red
  is-red: [= c Red]   # → true
]
```

**Named-field variants** — the payload is a dict; fields are accessible directly:

```tinct
[
  [type Geometry [Point x: Float y: Float] [Origin]]

  p: [Point x: 1.0  y: 2.0]

  # Match and access the payload
  desc: [match p
           [Point v]:  [str v.x "," v.y]
           [Origin _]: "origin"]   # v is the payload dict
  # → "1.0,2.0"
]
```

**Runtime-created variants are indistinguishable from user-constructed ones.** A builtin that returns a variant and user code that constructs the same variant produce identical values — there is no privileged runtime type. This means prelude behavior is fully replicable in user code.

---

## Pattern Matching

Patterns appear directly as `pattern: body` pairs inside `[match ...]`:

```tinct
[
  data: [host: "localhost"  port: 8080]

  # Structural dict match — destructures named fields into bindings
  url: [match data
    [host: h  port: p]: [str h ":" p]
    _:                   "unknown"]
  # → "localhost:8080"

  # Literal match
  label: [match 42
    0:   "zero"
    42:  "the answer"
    _:   "other"]
  # → "the answer"

  # Nested dict destructuring
  city: [match [user: [name: "Alice"  city: "Portland"]]
    [user: [city: c]]: c
    _:                  "unknown"]
  # → "Portland"
]
```

**Constructor patterns** match nominal variants. `[Tag binding]` binds the payload to a single name; the binding itself can be a full destructuring pattern:

```tinct
[
  [type Colors [Red] [Green] [Blue]]
  color: Red

  # Unit variant — [Tag _] discards the empty payload
  hex: [match color
    [Red _]:   "#ff0000"
    [Green _]: "#00ff00"
    [Blue _]:  "#0000ff"]
  # → "#ff0000"

  # Payload variant — single binding then field access
  [type Shape [Circle r: Float] [Rect w: Float  h: Float]]
  s: [Circle r: 2.0]

  area: [match s
    [Circle c]:  [* 3.14159 [* c.r c.r]]
    [Rect dims]: [* dims.w dims.h]]
  # → 12.56636

  # Nested destructuring inside constructor — bind payload fields directly
  desc: [match s
    [Circle c]:    [str "circle r=" c.r]
    [Rect dims]:   [str "rect " dims.w "×" dims.h]]
  # → "circle r=2.0"

  # WRONG — bare name is a variable capture, not a constructor match
  # [match color  Red: "#ff0000"  ...]  ← Red: captures anything
]
```

Patterns compose: a constructor pattern's binding can itself be a dict pattern, a literal, another constructor, or a wildcard `_`.

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

  # match on try result for fallback pattern
  safe:    [match [try [fn [] [/ 1 0]]]
              [Ok v]:    v
              [Error _]: 0]
  # → 0 (division error caught)
]
```

---

## Lazy Evaluation

Values are computed only when accessed. This means:

- Unused dict entries never evaluate — neither in the output nor in intermediate scope-chain dicts
- `filter`/`map` return lazy sequences; only accessed elements are computed
- Recursive structures like `[cons 1 ones]` are fine — they expand on demand
- `[head xs]` on a lazy Seq forces only the first element — the tail stays lazy
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
  first: [head lazy]          # forces head only → 4
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

**Control:** `try`, `raise`, `try-or`, `and-then`, `identity`, `compose`, `->` (pipe), `begin` / `>>` (forced sequential)

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

```text
# Named include — access functions via dot
[math: [include %libdir "math.llt"]]
[result: [math.hypot 3 4]]   # → 5.0
```

```text
# Bare include — names flow into scope via sequential expressions
[include %libdir "math.llt"]
[result: [hypot 3 4]]        # → 5.0
```

The `%libdir` capability points at the stdlib directory. The `include` function is self-hosted in prelude.

---

## Document Header Capabilities

Capabilities are injected by the CLI and declared with `--- caps:`:

```text
--- caps: [%fs: @DirCap]
[
  handle: [open %fs "config.json" "r"]
  # %net: @NetCap — network capability; grant with --cap-net flag
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
- **Let lazy evaluation work** — avoid `[collect v]` unless you genuinely need to realize the full sequence
