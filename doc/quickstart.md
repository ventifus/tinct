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

## Strings

Single-line strings use `"..."`. Multi-line strings use triple quotes — the closing `"""` on its own line sets the indentation anchor, and that much leading whitespace is stripped from every content line:

```tinct
[
  greeting: "Hello, world!"

  query: """
    SELECT *
    FROM users
    WHERE active = true
    """
  # → "SELECT *\nFROM users\nWHERE active = true\n"

  # Trailing newline included; wrap with trim to suppress it
  label: [trim """
    Click here
    """]
  # → "Click here"
]
```

**String interpolation** — the `i"..."` prefix embeds variable references with `$name`:

```tinct
[
  name:  "Alice"
  count: 42

  msg:   i"Hello $name, you have $count messages"
  # → "Hello Alice, you have 42 messages"

  # $$ escapes to a literal $
  price: i"Total: $$$count"
  # → "Total: $42"

  # Triple-quoted interpolation works too
  body:  i"""
    Dear $name,
    Your order is ready.
    """
]
```

Variable names stop at whitespace, brackets, and common punctuation (`.`, `,`, `!`, `?`). Only `$ident` is supported — there is no `${expr}` form.

---

## Dot Access

```tinct
[
  person:  [name: "Alice"  address: [city: "Portland"]]
  list:    ["first" "second" "third"]

  name:    person.name          # → "Alice"
  city:    person.address.city  # chained → "Portland"
  first:   list.0               # integer key → "first"
  second:  list.1               # list.1 → "second"
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
  maybe:   [get? "missing" person]              # → Absent.Absent (not [])
]
```

`get?`, `head`, and `env` return `Absent.Absent` (not `[]`) when a value is not present. Use `[absent? x]` to test for it — distinct from `null?` which tests for the empty dict `[]`.

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
  describe: [fn [let n@Number]
    [sign:  [if [< n 0] "negative" "non-negative"]]
    [str sign " (" [str n] ")"]]

  r1: [normalize [3 1 6]]              # → [0: 0.3  1: 0.1  2: 0.6]
  r2: [describe -42]                   # → "negative (-42)"
]
```

Intermediate dicts are letrec-scoped among themselves but do not appear in the function's return value — they are discarded after the final expression evaluates.

**Laziness note:** intermediate dict entries are thunks — they are only forced when accessed by subsequent expressions. Writing `[a: [expensive-call]]` as an intermediate body does not compute `expensive-call` unless `a` is used.

**`begin` / `>>` — forced sequencing for side effects** — when you need to evaluate an expression for its side effect and discard the result:

```tinct
[
  r1: [>> [+ 1 1]     # evaluated for effect (result discarded)
          "continue"] # returned

  r2: [begin [+ 1 1] [+ 2 2] "done"]   # multiple steps; last is returned
]
```

`begin` (and its alias `>>`) evaluates each expression eagerly in order and returns the last. Unlike intermediate dict bodies (which are lazy), `begin` is an explicit escape from laziness — use it only when side effects must occur regardless of whether the result is consumed. `>>` mirrors Haskell's monadic sequence operator: "evaluate for effect, discard, continue." `begin` is named after the Scheme `begin` special form.

**`_` shorthand** — desugars to a single-argument lambda:

```tinct
[
  nums:    [1 2 3 4 5]
  doubled: [map [* _ 2] nums]          # [fn [_] [* _ 2]]
  evens:   [filter [fn [let x] [= 0 [mod x 2]]] nums]  # explicit lambda for even numbers
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

  # Union return type — fn@[A B] means "returns A or B"
  zero-or-null: [fn@[Int Null] [let n@Int] n]

  # Parameterized type
  first:  [fn@a [let xs@[Seq a]] [head xs]]

  # Inline record type
  show:   [fn@String [let p@[name: String  age: Int]]
            [str p.name " is " [str p.age]]]

  # TypeVar (polymorphic)
  identity: [fn@a [let x@a] x]

  # Runtime type assertion
  n:      [@Int 42]   # errors if not Int at runtime
]
```

Type declarations and construction:

```tinct
[
  Color: [type Red Green Blue]   # dict-entry form; unit constructors are bare uppercase words

  color: Red   # injected short name; variant tag is "Color.Red" internally

  hex: [match color
    Color.Red:   "#ff0000"   # unit constructor patterns — qualified name, no brackets
    Color.Green: "#00ff00"
    Color.Blue:  "#0000ff"
    _:           "unknown"]
  # → "#ff0000"
]
```

Sum types with payloads use `try` in the standard library:

```tinct
[
  # try returns Variant(Result.Ok, value) on success, Variant(Result.Error, msg) on failure
  result: [try [fn [] [+ 1 2]]]

  # Pattern match on the result
  value: [match result
    [Result.Ok v]:    v     # → 3
    [Result.Error _]: 0]
]
```

---

## Nominal Variants

`Name: [type ...]` declares a type and injects its constructors as bindings into the enclosing dict. Unit constructors are bare uppercase words; payload constructors are bracketed (`[CtorName field: Type ...]`). The injected binding names are short (e.g. `None`, `Red`); the internal variant tags are qualified (e.g. `Option.None`, `Color.Red`).

```tinct
[
  Option: [type [let a]  [Some value: a]  None]   # parameterized; None is a bare unit constructor

  # Constructors are injected into scope as short names
  nothing: None

  # Payload variant: call constructor (injected into scope from the type declaration)
  something: [Some value: 42]

  # Pattern match — [Tag binding] binds the payload DICT; access named fields with dot
  n: [match something
        [Some p]: p.value   # p is the payload dict → p.value = 42
        _:        0]        # wildcard fallback
]
```

**Constructors are injected into the enclosing dict scope** as short names (`Red`, not `Color.Red`). The internal variant tag is qualified (`Color.Red`), but the binding you reference in value position is the short name:

```tinct
[
  Color: [type Red Green Blue]
  c: Red
  is-red: [= c Red]   # → true
]
```

**Named-field variants** — the pattern binds the payload dict; access fields with dot:

```tinct
[
  Measure: [type [Length r: Float] Zero]

  m: [Length r: 2.5]

  desc: [match m
    [Length p]: [str "length=" [str p.r]]   # p is the payload dict; p.r is the Float → 2.5
    Zero:       "zero"                      # unit constructor: bare name, no brackets
    _:          "unknown"]
  # → "length=2.5"
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
    [host: h  port: p]: [str h ":" [str p]]
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

**Constructor patterns** — unit constructors match with `Tag:` (bare) or `TypeName.Tag:`; payload constructors use `[Tag binding]` or `[TypeName.Tag binding]` to bind the payload dict:

```tinct
[
  Color: [type Red Green Blue]
  color: Red   # injected short name; variant tag is "Color.Red" internally

  # Unit constructors — bare name or qualified dot notation, no brackets
  hex: [match color
    Color.Red:   "#ff0000"
    Color.Green: "#00ff00"
    Color.Blue:  "#0000ff"
    _:           "unknown"]
  # → "#ff0000"

  # Payload constructors — [Tag binding] binds the payload dict; access fields with dot
  Shape: [type [Circle r: Int] [Square s: Int]]
  sh: [Circle r: 5]

  area: [match sh
    [Shape.Circle p]: [* 3 [* p.r p.r]]   # p is the payload dict; p.r is the Int → 3*5*5 = 75
    [Shape.Square p]: [* p.s p.s]
    _:                0]
  # → 75
]
```

Patterns compose: a constructor pattern's binding can itself be a dict pattern, a literal, another constructor, or a wildcard `_`.

**Tag patterns are unambiguous:** uppercase names in pattern position are always constructor patterns. Lowercase names are variable captures. `_` is the wildcard.

**Type and binding patterns** — annotate a binding with a type to both narrow and bind:

```tinct
[
  # n@Int: bind scrutinee to n, arm fires only when type is Int
  describe: [fn [let x]
    [match x
      n@Int:  [str "int: " [str n]]
      n@Str:  [str "str: " n]
      _:      "other"]]

  r1: [describe 42]       # → "int: 42"
  r2: [describe "hello"]  # → "str: hello"
]
```

**`[case ...]` arms** — the canonical form for match arms that bind variables. Each arm takes exactly 3 arguments: `[case [let bindings] pattern body]`.

- **`[let bindings]`** — names that will be bound by this arm (empty `[let]` for no new bindings)
- **`pattern`** — the structural match: uppercase/dot-access head = constructor check; lowercase/operator head = guard expression; `_` = wildcard
- **`body`** — the expression to evaluate when the arm matches, with bound names in scope

```tinct
[
  handle: [fn [let result]
    [match result
      [case [let v]  [Result.Ok v]   [str "ok: " [str v]]]
      [case [let _]  [Result.Err _]  "error"]]]

  r1: [handle [Result.Ok 42]]    # → "ok: 42"
  r2: [handle [Result.Error ""]] # → "error"
]
```

Non-binding arms (no `[let ...]` needed) remain keyed:

```tinct
[match color
  Color.Red:   "#ff0000"   # unit constructor — no binding
  Color.Green: "#00ff00"
  42:          "forty-two" # literal equality — no binding
  _:           "other"]    # wildcard — no binding
```

---

## Type Classes

`[class ...]` declares an interface; `[instance ...]` provides an implementation for a specific type. The class header always uses the `[let ClassName params...]` form. Both appear as entries inside a dict — classes as named values, instances as named single-arm entries or positional multi-arm entries:

```tinct
[
  Printable:    [class [let Printable a]]

  PrintableStr: [instance Printable [let a@String]: [print: [fn [let x] [str "str:" x]]]]
  PrintableInt: [instance Printable [let a@Int]:    [print: [fn [let x] [str "int:" [str x]]]]]

  # Named instances are regular dict values — pass them to generic functions
  format: [fn [let inst x] [inst.print x]]
  s: [format PrintableStr "hello"]   # → "str:hello"
  n: [format PrintableInt 42]        # → "int:42"
]
```

Named single-arm instances evaluate to their method dict at runtime, so `PrintableStr.print` is the function. Generic functions receive an explicit instance dict as an argument — this is dictionary-passing style.

**Multiple methods** — the instance body is a dict:

```tinct
[
  AppendableStr: [instance Appendable
    [let a@String]: [
      append-one: [fn [let a b] [str a b]]
      empty:       ""]]
]
```

**Multi-parameter classes with functional dependencies** — `determines` declares which parameters are determined by others; `resolver` names the instance dict injected at call sites. Multi-arm instances appear as positional entries (no key):

```tinct
[
  Addable: [class [let Addable a b c] [determines: [[[a b] c]] resolver: AddResult]]

  # Extend + to work on strings via concatenation
  [instance Addable
    [let a@String b@String c]: [+: [fn@String [let x@String y@String] [str x y]]]]

  result: [+ "hello" " world"]   # → "hello world"
]
```

The built-in `+`, `-`, `*`, `/`, `=`, `<`, etc. are all resolved through type class instances defined in the prelude.

---

## Error Handling

Errors propagate automatically through the thunk graph. Unused values never error.

```tinct
[
  # raise — always errors, never returns
  validated: [fn [let port@Int]
    [if [and [>= port 1] [<= port 9999]]
      port
      [raise [str "invalid port: " [str port]]]]]

  # try catches runtime errors; returns Result.Ok or Result.Error
  value:   [match [try [fn [] [+ 1 2]]]
    [Result.Ok v]:    v   # → 3
    [Result.Error _]: 0]

  # match on try result for fallback pattern
  safe:    [match [try [fn [] [/ 1 0]]]
              [Result.Ok v]:    v
              [Result.Error _]: 0]
  # → 0 (division error caught)
]
```

`try` catches runtime errors. When the body evaluates successfully, it returns `[Result.Ok value]`. When it raises, it returns `[Result.Error message]`. Note: `try` catches evaluation errors only — to demonstrate a type error caught by `try`, run it at eval time, not typecheck time.

---

## Lazy Evaluation

Values are computed only when accessed. This means:

- Unused dict entries never evaluate — neither in the output nor in intermediate scope-chain dicts
- `filter`/`map` return lazy sequences; only accessed elements are computed
- Recursive structures like `[cons 1 ones]` are fine — they expand on demand
- `[head xs]` on a lazy Seq forces only the first element — the tail stays lazy
- `[collect xs]` forces a lazy Seq to completion, producing a concrete integer-keyed Dict — use this to fully realize a sequence

```tinct
# 'slow' never evaluates — it is unused, so the raise never fires
[
  fast: [+ 1 1]
  slow: [raise "never runs"]
  used: fast    # → 2; slow never forced
]
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

**Types:** `int?`, `str?`, `dict?`, `fn?`, `seq?`, `null?`, `absent?`, `bool?`, `float?`, `bytes?`, `num?`, `annotation-of`

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
- `%cwd` — DirCap for the working directory

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
