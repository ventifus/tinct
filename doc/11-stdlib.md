# Standard Library

## Language vs Stdlib Boundary

### Argument Order Convention

**Most stdlib functions are data-last** to enable `->` threading (pipeline composition):

```tinct
[-> users
  [filter is-active]
  [map get-name]
  [sort]]
```

Each function receives data as its **last** parameter: `[map fn data]`, `[filter pred data]`, etc. This aligns with Unix pipe semantics (`data | transform`) and allows partial application patterns in languages with currying.

**Exceptions: `get-or` and `get-in-or` are data-first**:

```tinct
[get-or config key default]      # data-first
[get-in-or config path default]  # data-first
```

**Rationale:** Data-first order mirrors bracket access syntax `$data[$key]` and follows Clojure's `get` convention, making lookups read naturally as "from collection, get key, or default." The trade-off: these functions don't compose directly with `->` threading (they would require wrapping in a lambda to reorder arguments).

### Special Forms vs Stdlib Functions

**Lazy evaluation means most "control flow" is just regular functions.** In an eager language, `if` must be a special form because both branches would be evaluated before `if` runs. In Tinct, all arguments are thunks — the unused branch is never materialized.

Only constructs that affect **binding structure** or **dict construction** need to be special forms (built into the language). The parser recognizes these by checking the first entry of every `[]`:

| Language-level (special forms) | Why |
|-------------------------------|-----|
| `call` | Triggers function application (exact arity required) |
| `fn` | Introduces parameter bindings, creates a new scope |
| `type` | Compile-time type declaration, not a runtime value |

Everything else can be a regular function in the stdlib:

| Stdlib function | How it works with lazy eval |
|----------------|----------------------------|
| `if` | Materializes `cond`, returns the matching branch thunk (other branch never materialized) |
| `cond` | Materializes conditions in order, returns first matching branch |
| `when` | Like one-armed `if`; materializes condition, returns body or `[]` |
| `unless` | Inverse of `when`; materializes condition, returns body or `[]` |
| `and` | Materializes first arg; if false, returns false without materializing second |
| `or` | Materializes first arg; if truthy, returns it without materializing second; otherwise returns second |
| `not` | Materializes its argument; returns the boolean inverse |

```tinct
# These are stdlib functions, not special forms:
[if [> x 0] positive non-positive]
[and [valid? input] [process input]]  # process never called if invalid
[or cached-value [expensive-compute]]  # compute skipped if cached
```

### Language vs Stdlib

Tracking what must be built into the language vs what can be implemented in the stdlib.

## Language Builtins (Special Forms)

These require special evaluation or parsing rules — they can't be expressed as regular functions. The parser recognizes them by checking the first entry of every `[]`:

- `call` — function application (exact arity required)
- `fn` — function definition (creates scope, binds params)
- `type` — type alias declaration

## Stdlib Functions

These leverage lazy evaluation and can be regular functions. Each function is classified by its **thunk behavior** — whether it preserves thunks, creates new ones, or materializes values:

- **Structural** — rearranges entries without inspecting values. Thunks pass through untouched.
- **Lazy-transforming** — applies a function to values but produces new thunks. No computation until the result is materialized.
- **Materializing** — must compute values to determine the result. Some operations like `$length` and `$empty?` materialize only the collection structure (to count entries), not the values themselves — values remain as thunks.
- **Selective** — materializes some arguments, leaves others as thunks (e.g., short-circuit evaluation).

**Control flow** (selective):

| Function | Materialization behavior |
|----------|------------------------|
| `if` | Materializes condition only; returns chosen branch as thunk (other branch never materialized). |
| `cond` | Materializes conditions in order; returns first matching branch as thunk |
| `when`, `unless` | Materializes condition; returns body or `[]` |
| `and` | Materializes first; if false, returns false without materializing second |
| `or` | Materializes first; if truthy, returns it without materializing second |
| `not` | Materializes its argument |

**List operations** (integer keys only, always renumber to dense 0..n):

| Function | Materialization behavior |
|----------|------------------------|
| `first`, `rest` | Structural — returns thunks in new positions |
| `cons`, `conj` | Structural — combines thunks into new structure |
| `concat` | **Dual-dispatch** — Seq path is lazy (O(1) chain); Dict path is **Materializing** (eagerly reindexes both dicts to 0..n) |
| `reverse`, `reindex` | Structural — reorders/renumbers, values untouched |
| `sort`, `sort-by` | **Materializing** — must compare values to determine order. `$sort` uses lexicographic comparison for strings, numeric comparison for numbers. `$sort` errors at runtime when called on a collection containing incompatible types for comparison; no compile-time detection. |

**Dict operations** (any key type, preserve keys):

| Function | Materialization behavior |
|----------|------------------------|
| `get`, `get-or`, `has?` | Structural — key lookup, returns thunk |
| `get-in`, `get-in-or` | **Materializing** — deep path access. Takes a dict and a list of keys, traverses nested dicts. Must evaluate each key lookup. `get-in-or` returns a default on missing keys instead of erroring. |
| `set`, `remove` | Structural — add/remove entries |
| `merge` | **Materializing** — eagerly materializes both dicts, builds new IndexMap (O(n)); values remain as thunk Rc-clones. |
| `keys` | Structural — keys are always evaluated, not thunks |
| `values`, `entries` | Structural — returns thunks in dict insertion order |
| `update` | Lazy-transforming — produces thunk `[call $f $old-value]` |

**Universal collections** (any collection, preserve keys, insertion order):

| Function | Materialization behavior |
|----------|------------------------|
| `nth`, `last`, `slice` | Structural — positional access, returns thunks |
| `take`, `drop` | Structural — positional subsequence, thunks preserved |
| `zip` | Structural — pairs entries, values stay thunks |
| `length`, `empty?` | Materializing — materializes collection structure to count entries (values remain as thunks) |
| `map`, `map-entries` | Lazy-transforming — on dicts, returns dict with PendingCall thunks; on seqs, returns lazy seq |
| `filter` | **Asymmetric:** on dicts, returns Seq (must evaluate predicates to decide inclusion; keys are not preserved — use `$collect` to convert result to dict); on seqs, returns lazy Seq |
| `reduce`, `fold` | **Selective** — Dict path builds lazy PendingCall chain; Seq path materializes tail at each step |
| `find-deep` | **Materializing** — must traverse structure looking for keys |
| `flatten` | **Materializing** — must inspect values to check if they are lists |

**Arithmetic & comparison** (materializing — must evaluate operands):
- `+`, `-`, `*` (auto-promote: Int op Int → Int, mixed → Float)
- `/` (always returns Float), `quot`, `mod` (Int only, return Int; both are prelude functions)
- `=`, `<`, `>`, `<=`, `>=` (work on Int, Float, String, Bool; cross-type Int/Float comparison allowed). `$=` returns `false` for Dict, Function, and Builtin values — there is no structural/deep equality. Structural/deep equality of dicts is intentionally not provided (forcing nested fields would violate lazy evaluation, and pointer equality would be inconsistent with value semantics).
- `to-int`, `to-float`, `floor`, `ceil`, `round` (numeric conversions)

**Strings** (materializing — must evaluate arguments):
- `str` (exact concat), `words` (split by space, filter empties), `join` (with separator)
- `split`, `replace`
- `upper`, `lower`, `trim`

**Composition** (structural — builds function pipelines, no values materialized):
- `->` (threading)
- `compose`
- `apply` — call function with dict spread (Key::String → named args, Key::Int sorted → positional args)

**Sequences** (lazy computation -- produce `Seq` values):
- `range`, `repeat`, `cycle`, `iterate`, `unfold` -- constructors (finite or infinite)
- `seq` -- low-level cons: `[call $seq $head $tail-thunk]`
- `collect` -- materializes a Seq into a dict with integer keys
- `concat` -- lazy Seq concatenation (O(1) chain for Seq path; Dict path is materializing)
- `head`, `tail` -- destructors
- `seq?` -- type check

**Utility:**

| Function | Materialization behavior |
|----------|------------------------|
| `identity` | Structural — returns its argument as-is |
| `type-of` | **Materializing** — must evaluate to determine type. Returns `"Function"` for both user-defined functions and Rust-native builtins (intentionally indistinguishable to user code). |
| `assert` | **Materializing** — must evaluate condition |
| `error` | Structural — constructs error value, not materialized until propagated |
| `try`, `try-or` | **Materializing** — materializes body, catches exceptions. `$try` returns `[ok: value]` on success or `[err: message]` on failure (tagged dict, not a special type). |

**Materialization** (runtime-supported):
- `eval` — recursively forces all thunks (runtime-supported, may diverge on infinite structures)
- `from-json` — parses JSON string into Tinct dict (pure function, safe on untrusted input)

**Key implications for lazy evaluation:**

```tinct
# map on dict is lazy — returns dict with PendingCall thunks
big-result: [map [fn [x] [expensive x]] big-dict]
big-result.widget          # Only this one element gets computed

# filter on dict returns a Seq (must evaluate predicates to decide inclusion)
# Other fields on kept users remain thunks until accessed
expensive: [collect [filter [fn [x] [> x.price 100]] products]]

# sort must materialize everything — can't sort without comparing
sorted: [sort big-list]  # All values materialized immediately

# Infinite sequences — lazy all the way
naturals: [range 0]   # O(1), nothing computed
squares: [map [fn [n] [* n n]] naturals]  # still O(1)
first-ten: [collect [take 10 squares]]
# -> [0 1 4 9 16 25 36 49 64 81]
```

## Rust-Native vs Tinct-Implemented Boundary

**Principle:** Only implement in Rust what cannot be expressed in Tinct itself. For operators that must remain Rust (arithmetic, comparison, control flow, sequence manipulation), register both the primary name and a stable alias. The primary name is wrapped in a Tinct prelude function; the stable alias is the fallback that wrappers call, and that domain-specific stdlib modules (e.g. `stdlib/sql.llt`) call when they need to shadow the primary name.

**Rust-native builtins:**

| Group | Stable alias | Primary name | Rationale |
|-------|-------------|--------------|-----------|
| Arithmetic | `builtin-add`, `builtin-sub`, `builtin-mul`, `builtin-div` | `+`, `-`, `*`, `/` | Host numeric types (i64, f64). |
| Comparison | `builtin-lt`, `builtin-eq` | `<`, `=` | Cross-type Int/Float coercion at host level. `>`, `<=`, `>=` are derived from `<` and `not`. |
| Control | `builtin-if` | `if` | Selective materialization — only the chosen branch is forced. |
| Field intercept | — | `proxy` | Takes a handler `fn [field-name] value`; returns `Value::Proxy`. Any field access `.field` calls `handler(field-name)`. Enables proxy rows, mock objects, virtual namespaces. |
| Dict primitives | — | `keys`, `length`, `merge`, `append` | Operate on IndexMap directly. |
| Strings | — | `str`, `split`, `replace`, `upper`, `lower`, `trim`, `join` | Strings are opaque; all content operations require Rust. `join` uses an O(n) string builder (dual-dispatch Dict/Seq); no stable alias needed. |
| Numeric | — | `floor`, `round` | `f64::floor`, `f64::round`. `ceil` and `trunc` are derived. |
| Parsing | — | `to-int`, `to-float` | String-to-number only. |
| Evaluation control | — | `eval`, `error`, `try`, `apply` | `eval` deep-forces; `error` constructs EvalError; `try` catches materialization errors; `apply` spreads dict (Key::String → named args, Key::Int sorted → positional args). |
| Type introspection | — | `type-of`, `int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` | Inspect the Value enum variant. |
| Sequences | `builtin-filter`, `builtin-map`, `builtin-reduce`, `builtin-take`, `builtin-drop` | `filter`, `map`, `reduce`, `take`, `drop` | Dual-dispatch on Dict/Seq; require `Rc<Thunk>` manipulation. Also: `seq`, `head`, `tail`, `collect`, `seq?`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `concat` (no stable aliases needed). |
| I/O | — | `from-json`, `include` | serde_json, filesystem access. |

**Tinct-implemented stdlib (wrappers and derived functions):**

The prelude wraps every primary-name operator that has a stable alias, making it shadowable by domain-specific stdlib modules:

| Function | Derivation | Notes |
|----------|-----------|-------|
| `<` | `[fn [a b] [builtin-lt a b]]` | Shadowable; calls stable alias `builtin-lt` |
| `=` | `[fn [a b] [builtin-eq a b]]` | Shadowable; calls stable alias `builtin-eq` |
| `+` | `[fn [a b] [builtin-add a b]]` | Shadowable; calls `builtin-add` |
| `-` | `[fn [a b] [builtin-sub a b]]` | Shadowable; calls `builtin-sub` |
| `*` | `[fn [a b] [builtin-mul a b]]` | Shadowable; calls `builtin-mul` |
| `/` | `[fn [a b] [builtin-div a b]]` | Shadowable; calls `builtin-div` |
| `if` | `[fn [c t e] [builtin-if c t e]]` | Shadowable; calls `builtin-if` |
| `filter` | `[fn [pred xs] [builtin-filter pred xs]]` | Shadowable; calls `builtin-filter` |
| `map` | `[fn [f xs] [builtin-map f xs]]` | Shadowable; calls `builtin-map` |
| `reduce` | `[fn [f init xs] [builtin-reduce f init xs]]` | Shadowable; calls `builtin-reduce` |
| `take` | `[fn [n xs] [builtin-take n xs]]` | Shadowable; calls `builtin-take` |
| `drop` | `[fn [n xs] [builtin-drop n xs]]` | Shadowable; calls `builtin-drop` |
| `not` | `[fn [x] [builtin-if x false true]]` | Uses `builtin-if` directly |
| `>` | `[fn [a b] [builtin-lt b a]]` | Argument swap |
| `<=` | `[fn [a b] [not [builtin-lt b a]]]` | Negated `>` |
| `>=` | `[fn [a b] [not [builtin-lt a b]]]` | Negated `<` |
| `and` | `[fn [a b] [builtin-if a b false]]` | Short-circuit via lazy args |
| `or` | `[fn [a b] [builtin-if a a b]]` | Pass-through: returns `a` if truthy |
| `quot` | `[fn [a b] [trunc [builtin-div a b]]]` | Truncation toward zero |
| `mod` | `[fn [a b] [builtin-sub a [builtin-mul [quot a b] b]]]` | Algebraic identity |
| `ceil` | `[fn [x] [builtin-sub 0 [floor [builtin-sub 0 x]]]]` | `ceil(x) = -floor(-x)` |
| `trunc` | `[fn [x] [builtin-if [>= x 0] [floor x] [ceil x]]]` | Conditional floor/ceil |
| `words` | `[builtin-filter [fn [w] [not [builtin-eq w ""]]] [split " " s]]` | Uses stable `builtin-filter`, `builtin-eq` |

**Why shadowable wrappers matter:**

Any `include`d stdlib module can shadow the primary-name operators in lexical scope. `stdlib/sql.llt` uses this to provide SQL-aware versions of `filter`, `map`, `<`, `=`, `and`, `if`, etc. that propagate SQL expression trees when applied to proxy rows. Each shadow calls the stable `builtin-X` alias for non-SQL fallback. User code written after `[include "stdlib/sql.llt"]` gets transparent SQL dispatch without any API changes. See `doc/whatif/lib-sql.md`.

**Loading mechanism:**

The Tinct stdlib lives in `stdlib/prelude.llt`, bundled at compile time via `include_str!`. At startup:

1. Create root environment with Rust-native builtins (primary names + stable aliases)
2. Parse and evaluate `prelude.llt` with root environment as parent — adds wrappers and derived functions
3. User code's environment inherits from the stdlib environment

```
Rust primitives (builtin-lt, builtin-eq, builtin-add, builtin-if, builtin-filter, proxy, ...)
  └── Tinct prelude (<, =, +, -, *, /, if, filter, map, not, >, and, or, ...)
        └── User code / domain stdlib ([include "stdlib/sql.llt"] shadows filter, map, <, =, ...)
              └── User predicates and programs
```

## Stdlib Function Reference

**Architecture:** 59 Rust-native builtins (see `standard_builtins()` in `src/builtins.rs`) + LLT-implemented functions in `stdlib/prelude.llt` (including 12 shadowable wrappers). Of the 59 Rust builtins, 12 are wrapped by LLT functions (`<`, `=`, `+`, `-`, `*`, `/`, `if`, `filter`, `map`, `reduce`, `take`, `drop`) to enable shadowing via `$include`. The wrapped builtins remain accessible via stable `builtin-*` aliases (e.g., `builtin-lt`, `builtin-eq`).

Functions available to all user code. Collection operators (`map`, `filter`, `reduce`, `take`, `drop`) and arithmetic/comparison operators (`+`, `-`, `*`, `/`, `<`, `=`, `if`) are Tinct prelude wrappers over stable Rust aliases — shadowable by `$include`d modules. Sequence constructors (`range`, `repeat`, `cycle`, `iterate`, `unfold`) and `join` are Rust-native builtins with no wrapper. Private implementation details (functions suffixed with `-impl`, `-step`, `-check`) are omitted from this reference.

**New stdlib categories (added in recent cycles):**
- **Aggregates** (`sum`, `product`, `min`, `max`, `count`, `contains?`, `uniq`) — reduce-based collection summaries for common data analysis patterns
- **Higher-order utilities** (`with-entries`, `partition`, `flat-map`, `find-first`, `group-by`, `deep-merge`, `walk`, `transpose`) — advanced collection transformations following Jsonnet/jq/Nix stdlib patterns
- **Type predicates** (`int?`, `float?`, `num?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` as Rust builtins; `list?` as LLT stdlib) — runtime type inspection for dynamic dispatch and validation

These additions bring Tinct's stdlib coverage closer to mature configuration languages while maintaining the LLT-first principle; predicate builtins are Rust-native, `list?` is LLT-implemented on top of them.

> **Note:** Overrides apply to the initial dispatch only; Seq corecursion steps always call the underlying Rust implementation directly.

**Utility Functions:**

Functions primarily used internally by other stdlib functions, but also available to user code.

| Function | Signature | Description |
|----------|-----------|-------------|
| `make-entry` | `[fn [k v] ...]` | **Internal helper** — construct a single-entry dict from a computed key and value; not part of the public API |

**Identity:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `identity` | `[fn [x] x]` | Returns its argument unchanged |
| `const` | `[fn [x] [fn [y] x]]` | Returns first argument, ignores second. Classic K combinator |

**Logic:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `not` | `[fn [x] ...]` | Boolean negation |
| `and` | `[fn [a b] ...]` | Short-circuit AND: returns `b` if `a` is true, else `false` |
| `or` | `[fn [a b] ...]` | Short-circuit OR: returns first arg unchanged when truthy, otherwise evaluates and returns second arg |
| `any?` | `[fn [pred xs] ...]` | True if predicate holds for any element in collection |
| `all?` | `[fn [pred xs] ...]` | True if predicate holds for all elements in collection |

**Comparison (derived from `<` and `=`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `>` | `[fn [a b] ...]` | Greater than |
| `<=` | `[fn [a b] ...]` | Less than or equal |
| `>=` | `[fn [a b] ...]` | Greater than or equal |

**Arithmetic (derived from `+`, `-`, `*`, `/`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `quot` | `[fn [a b] ...]` | Integer quotient, truncates toward zero (Clojure semantics) |
| `mod` | `[fn [a b] ...]` | Remainder: `a - (a quot b) * b` |

**Numeric Conversion (derived from `floor`):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `ceil` | `[fn [x] ...]` | Ceiling: smallest integer >= x. Derived as `-floor(-x)` |
| `trunc` | `[fn [x] ...]` | Truncate toward zero: `floor` for positive, `ceil` for negative |
| `abs` | `[fn [x] ...]` | Absolute value: returns non-negative magnitude of a number |
| `sign` | `[fn [x] ...]` | Sign of a number: -1 for negative, 0 for zero, 1 for positive |
| `clamp` | `[fn [lo hi x] ...]` | Clamp a value between lo and hi bounds (inclusive) |

**String:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `join` | Rust native builtin — no LLT wrapper | Join values as strings with separator (O(n) string builder; dual-dispatch Dict/Seq) |
| `words` | `[fn [s] ...]` | Split a string by spaces, filtering empty strings (returns Seq). Derived from `str`, `split`, and `filter`. |

**Control Flow:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `when` | `[fn [pred body] ...]` | Returns `body` if `pred` is true, else `[]` |
| `unless` | `[fn [pred body] ...]` | Returns `body` if `pred` is false, else `[]` |
| `cond` | `[fn [pairs] ...]` | Multi-branch conditional: takes a list of `[condition result]` pairs |
| `until` | `[fn [pred f x] ...]` | Iterate function until predicate holds. Applies `f` repeatedly to `x` until `pred(x)` is true. Recursive; hits MAX_EVAL_DEPTH (~256) on large inputs |

**Field Interception:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `proxy` | `[fn [handler] ...]` | Takes a handler `fn [field-name] value`; returns `Value::Proxy`. Any field access `.field` calls `handler(field-name)` |

**Dict Utilities:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `get` | `[fn [xs k] ...]` | Get value by key (bracket access wrapper) |
| `has?` | `[fn [xs k] ...]` | Check if a key exists (uses `try` around access) |
| `get-or` | `[fn [xs k default] ...]` | Get value by key with fallback default |
| `get-in` | `[fn [xs path] ...]` | Traverse nested dicts by a list of keys; errors on missing key |
| `get-in-or` | `[fn [xs path default] ...]` | Traverse nested dicts with fallback default |
| `empty?` | `[fn [xs] ...]` | Check if a collection has zero entries |
| `set` | `[fn [xs k v] ...]` | Return new dict with key added/updated |
| `remove` | `[fn [xs k] ...]` | Return new dict with key removed |
| `update` | `[fn [xs k f] ...]` | Apply function `f` to the value at key `k` |
| `values` | `[fn [xs] ...]` | Get all values as an integer-indexed list; preserves dict insertion order |
| `entries` | `[fn [xs] ...]` | Get all entries as a list of `[key: k value: v]` dicts; preserves dict insertion order |
| `from-entries` | `[fn [pairs] ...]` | Reconstruct a dict from a list or Seq of `[key: k value: v]` pairs |

**List Operations (integer keys, dense 0..n output):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `first` | `[fn [xs] ...]` | Get the first element (key 0) |
| `nth` | `[fn [xs n] ...]` | Get element by insertion-order position (supports negative indices) |
| `last` | `[fn [xs] ...]` | Get the last element by insertion-order position |
| `rest` | `[fn [xs] ...]` | All elements except the first, reindexed from 0 |
| `cons` | `[fn [x xs] ...]` | Prepend an element, reindexing from 0 |
| `conj` | `[fn [xs x] ...]` | Append an element (delegates to `$append`) |
| `concat` | Rust native builtin — no LLT wrapper | Concatenate two collections; Seq concat is lazy (O(1) chain), Dict concat reindexes to 0..n |
| `reverse` | `[fn [xs] ...]` | Reverse a list |
| `reindex` | `[fn [xs] ...]` | Rebuild with dense 0..n integer keys |

**Sorting:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `sort` | `[fn [xs] ...]` | Sort using natural ordering (mergesort) |
| `sort-by` | `[fn [cmp xs] ...]` | Sort using a custom comparator function |

**Universal Collection Operations (preserve keys):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `map` | `[fn [f xs] ...]` | Apply function to every value, preserving keys. On dicts, returns a dict with PendingCall thunks (lazy); on seqs, returns a lazy seq. Note: unlike `filter`, `map` preserves key types — dict input returns dict output. |
| `map-entries` | `[fn [f xs] ...]` | Apply function to every entry `[key: k value: v]`; f receives the entry dict and returns the **new value** (keys are preserved unchanged) |
| `filter` | `[fn [pred xs] ...]` | Keep values where predicate returns true. **Asymmetry:** returns Seq when input is Dict (keys are not preserved — must evaluate predicates to determine which values survive, breaking the key-value relationship); returns lazy Seq when input is Seq. Use `collect` to convert the result back to a dict. See also `map`, which preserves dict keys. |
| `reduce` | `[fn [f init xs] ...]` | Left fold (Rust builtin; dual-dispatch Dict/Seq) |
| `fold` | `[fn [f init xs] ...]` | Alias for `reduce` — left fold, identical semantics; use whichever name fits context |
| `foldr` | `[fn [f acc xs] ...]` | Right fold: fold from the right, equivalent to `fold(f, acc, reverse(xs))` |
| `slice` | `[fn [xs start end] ...]` | Positional slice (start inclusive, end exclusive) |
| `take` | `[fn [n xs] ...]` | Take the first n entries, preserving keys |
| `take-while` | `[fn [pred xs] ...]` | Take elements from beginning while predicate holds; stop at first failure |
| `drop` | `[fn [n xs] ...]` | Skip first n entries (Rust builtin; dual-dispatch Dict/Seq) |
| `drop-while` | `[fn [pred xs] ...]` | Drop elements from beginning while predicate holds; return remaining suffix |
| `zip` | `[fn [xs ys] ...]` | Pair entries from two collections by position |
| `unzip` | `[fn [pairs] ...]` | Unzip a list of pairs into a pair of lists |
| `flatten` | `[fn [xs] ...]` | Flatten nested lists one level deep |
| `find-deep` | `[fn [xs target] ...]` | Recursively search for a key in nested dicts |

**Higher-Order Dict/List Utilities:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `with-entries` | `[fn [xs f] ...]` | Transform a dict via entries: `entries → map(f) → from-entries` |
| `partition` | `[fn [pred xs] ...]` | Split into two groups: elements satisfying pred (`pass`) and those that don't (`fail`) |
| `flat-map` | `[fn [f xs] ...]` | Map function over collection and flatten (concatenate) the results |
| `find-first` | `[fn [pred xs] ...]` | Return the first element satisfying pred, or error if none found |
| `find-first-or` | `[fn [pred default xs] ...]` | Return the first element satisfying pred, or default if none found |
| `group-by` | `[fn [f xs] ...]` | Group elements into a dict of lists, keyed by the result of applying f |
| `deep-merge` | `[fn [a b] ...]` | Merge two dicts recursively; sub-dicts are merged depth-first, other values from b override a |
| `walk` | `[fn [f xs] ...]` | Apply function bottom-up to every node in nested dict structure |
| `transpose` | `[fn [rows] ...]` | Transpose a 2D dict structure (flip rows and columns) |

**Aggregates:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `sum` | `[fn [xs] ...]` | Sum all elements of a numeric collection |
| `product` | `[fn [xs] ...]` | Product of all elements in a numeric collection |
| `min` | `[fn [xs] ...]` | Return the minimum element (errors on empty collection) |
| `max` | `[fn [xs] ...]` | Return the maximum element (errors on empty collection) |
| `count` | `[fn [pred xs] ...]` | Count elements satisfying predicate |
| `contains?` | `[fn [xs val] ...]` | Check if a collection contains val (structural equality) |
| `uniq` | `[fn [xs] ...]` | Remove duplicate elements, keeping the first occurrence of each |

**Composition:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `compose` | `[fn [f g] ...]` | Compose two functions: `(compose f g)(x) = f(g(x))` |
| `->` | `[fn [x ...stages] ...]` | Thread a value through a series of functions |

**Type Predicates:**

| Function | Implementation | Description |
|----------|---------------|-------------|
| `int?` | Rust builtin | Return true if value is an Int |
| `float?` | Rust builtin | Return true if value is a Float |
| `num?` | Rust builtin | Return true if value is an Int or Float |
| `str?` | Rust builtin | Return true if value is a String |
| `bool?` | Rust builtin | Return true if value is a Bool |
| `null?` | Rust builtin | Return true if value is Null (empty dict `[]`) |
| `dict?` | Rust builtin | Return true if value is a Dict (includes lists, which are dicts with integer keys) |
| `fn?` | Rust builtin | Return true if value is callable (Function or Builtin) |
| `seq?` | Rust builtin | Return true if value is a Seq |
| `list?` | LLT stdlib | Return true if value is a Dict whose keys are all integers (i.e., a list-shaped dict) |

**Error Handling:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `try-or` | `[fn [f default] ...]` | Call a function; return default if it errors |

**Sequences:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `range` | `[fn [start] ...]` or `[fn [start end] ...]` | Seq of integers from start (inclusive); infinite if 1-arg, finite (end exclusive) if 2-arg |
| `repeat` | `[fn [val] ...]` | Infinite Seq of copies of val; for finite, use `[take n [repeat val]]` |
| `cycle` | `[fn [xs] ...]` | Infinite Seq cycling through dict entries; for finite, use `[take n [cycle xs]]` |
| `iterate` | `[fn [f x] ...]` | Infinite seq: x, f(x), f(f(x)), ... |
| `unfold` | `[fn [step seed] ...]` | Seq from step function; step returns `[value state]` or `[]` to stop |
| `take` | `[fn [n xs] ...]` | Dual-dispatch: on Dict, take first n entries preserving keys; on Seq, return finite Seq of first n elements |
| `seq` | `[fn [head tail] ...]` | Low-level seq constructor (cons cell) |
| `collect` | `[fn [s] ...]` | Materialize seq into dict with integer keys 0..n |
| `head` | `[fn [s] ...]` | First element of seq |
| `tail` | `[fn [s] ...]` | Rest of seq (seq, not materialized) |
| `seq?` | Rust native builtin — no LLT wrapper | True if x is a Seq |

**Assertions:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `assert` | `[fn [cond msg] ...]` | Assert condition; error with message if false |

## Known Limitations

**Stdlib error message spans:** Error messages from stdlib functions that call `$error` internally (such as `$flatten`, `$take-while`, `$drop-while`) point to the stdlib implementation source location, not the user's call site. This is inherent to stdlib-authored error messages — the `$error` builtin correctly reports the span of the `[call $error ...]` expression, which happens to be inside `stdlib/prelude.llt`. User call sites will appear in the error's stack trace, but not as the primary error location. This will be addressed when file-path-based stack frame filtering is implemented to suppress stdlib internal frames and promote user frames.

## Two Map Variants

- `map` — transforms values, preserves keys
- `map-entries` — receives each entry as `[key: k value: v]`, must return the **new value**; keys are preserved (to remap keys, use `with-entries`)

## Threading `->` in Stdlib

Not language syntax. Implemented in stdlib:

```tinct
->: [fn [x ...stages]
    [builtin-reduce [fn [acc f] [f acc]] x stages]]
```

## Equality and Comparison — Formal Specification

This section formalizes the two primitive comparison builtins (`$=` and `$<`) and the three derived comparison operators (`$>`, `$<=`, `$>=`). The specification defines type-dispatch semantics, totality and partiality properties, cross-type promotion, and the algebraic properties these relations satisfy or intentionally violate.

### Part 1: Primitive Relations

Two builtins form the comparison basis. All others are derived compositions.

**EQ — Total equality (`=`):**

```
EQ(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_eq(v₁, v₂))
```

**LT — Partial ordering (`<`):**

```
LT(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_lt(v₁, v₂))    if defined
  ⟨v₁, v₂⟩ ↦ Error(type_mismatch, s)       otherwise
```

The `_` in `materialize(θ, _, d)` is the materialization span (`Option<&Span>`), passed as `None` by both builtins — it is a diagnostic concern, not a semantic parameter. The span `s` is the call-site span: unused in EQ (total function, never errors) but required for LT error reporting.

Both builtins require exactly 2 positional arguments and reject named arguments (`reject_named`). Both are **inherently materializing**: they must inspect the concrete values of both operands to produce a result. This is a §Selective Materialization boundary — comparison always forces. If materialization of either operand raises an error (cycle detection, division by zero, depth limit), that error propagates immediately — comparison dispatch is never reached.

### Part 2: Type-Dispatch Tables

**`dispatch_eq(v₁, v₂) → bool`:**

| v₁ | v₂ | Result | Rule |
|----|----|--------|------|
| Int(a) | Int(b) | a == b | EQ-INT |
| Float(a) | Float(b) | a == b (IEEE 754) | EQ-FLOAT |
| String(a) | String(b) | a == b (byte equality) | EQ-STR |
| Bool(a) | Bool(b) | a == b | EQ-BOOL |
| Int(a) | Float(b) | (a as f64) == b | EQ-PROMOTE-IF |
| Float(a) | Int(b) | a == (b as f64) | EQ-PROMOTE-FI |
| _ | _ | false | EQ-INCOMP |

**`dispatch_lt(v₁, v₂) → bool | ⊥`:**

| v₁ | v₂ | Result | Rule |
|----|----|--------|------|
| Int(a) | Int(b) | a < b | LT-INT |
| Float(a) | Float(b) | a < b (IEEE 754) | LT-FLOAT |
| String(a) | String(b) | a < b (lexicographic) | LT-STR |
| Bool(a) | Bool(b) | ¬a ∧ b (false < true) | LT-BOOL |
| Int(a) | Float(b) | (a as f64) < b | LT-PROMOTE-IF |
| Float(a) | Int(b) | a < (b as f64) | LT-PROMOTE-FI |
| _ | _ | ⊥ (type error) | LT-ERROR |

The critical difference: EQ-INCOMP returns `false` (totality), while LT-ERROR raises a type error (partiality). This reflects the design that "are these equal?" always has a reasonable answer (no, different types are never equal), while "is this less than that?" has no meaningful answer across incompatible types.

### Part 3: Cross-Type Promotion Semantics

Int/Float promotion uses Rust's `as f64` cast, which is the IEEE 754 `convertToFloat64` operation. This is exact for integers in the range [−2⁵³, 2⁵³] but loses precision outside it:

```
Promotion: Int(n) → Float(n as f64)

Exact range:  |n| ≤ 2⁵³ (9,007,199,254,740,992)
Loss example: Int(2⁵³ + 1) promotes to Float(2⁵³)
              → EQ-PROMOTE: [call $= 9007199254740993 9007199254740992.0] = true  (!)
```

**Design rationale:** The alternative — rejecting cross-type comparison entirely — would force users to manually cast in every mixed expression. The promotion follows JavaScript, Python, Ruby, and Lua conventions. The precision-loss edge case affects only integers outside the safe range, which is rare in configuration contexts.

Promotion is **symmetric**: `EQ-PROMOTE-IF` and `EQ-PROMOTE-FI` always produce the same result because IEEE 754 `==` is symmetric and `as f64` is deterministic.

### Part 4: Derived Relations

Three comparison operators are derived from `<` and `not` in `stdlib/prelude.llt`:

```
GT(a, b)  ≡  LT(b, a)               # >:  [fn [a b] [< b a]]
LEQ(a, b) ≡  ¬LT(b, a)              # <=: [fn [a b] [not [< b a]]]
GEQ(a, b) ≡  ¬LT(a, b)              # >=: [fn [a b] [not [< a b]]]
```

Note: `<=` is defined as `¬GT` (not as `LT ∨ EQ`), and `>=` as `¬LT` (not as `GT ∨ EQ`). These are equivalent for total orders but diverge in the presence of NaN (see Part 5). The stdlib definitions are correct because `<` is a strict weak order on each comparable type (NaN is incomparable to everything, and `¬(NaN < x)` correctly yields `true` for `>=`... but see the NaN anomaly below).

### Part 5: IEEE 754 NaN Behavior

Float comparison follows IEEE 754 semantics inherited from Rust's `f64` operations:

```
EQ-FLOAT with NaN:   NaN == NaN → false     (IEEE 754 §5.11)
LT-FLOAT with NaN:   NaN < x   → false      (for any x, including NaN)
                      x < NaN   → false      (for any x, including NaN)
```

**Consequence for derived relations:**

```
[= NaN NaN]  → false    (NaN ≠ NaN — correct per IEEE 754)
[< NaN 1.0]  → false    (NaN is unordered)
[> NaN 1.0]  → false    (= [< 1.0 NaN] → false)
[<= NaN 1.0] → true     (= [not [< 1.0 NaN]] = [not false] = true — ANOMALY)
[>= NaN 1.0] → true     (= [not [< NaN 1.0]] = [not false] = true — ANOMALY)
```

The `<=` and `>=` anomalies arise because the stdlib derives them via negation of the *swapped* `<`, rather than via `LT ∨ EQ`. Under IEEE 754, `¬(b < a)` is *not* equivalent to `a ≤ b` when either operand is NaN. This is a known deviation: IEEE 754 §5.11 defines `totalOrder` separately from the partial comparison predicates.

**NaN-vs-NaN anomaly:**

```
[<= NaN NaN] → true     (= [not [< NaN NaN]] = [not false] = true)
[>= NaN NaN] → true     (= [not [< NaN NaN]] = [not false] = true)
```

Both `[<= NaN NaN]` and `[>= NaN NaN]` return `true`, even though `[= NaN NaN]` returns `false`. Tinct reports NaN as both "less-than-or-equal-to itself" and "greater-than-or-equal-to itself" while simultaneously reporting it as "not equal to itself."

**NaN/Infinity rejection (decided):** Tinct enforces the invariant that **all floats are finite**. Non-finite values are rejected at two layers: (1) `from-json` rejects `f64::INFINITY` and `f64::NAN` from `serde_json::Number::as_f64()` at parse time, closing the entry path, and (2) arithmetic builtins (`+`, `-`, `*`, `/`) reject non-finite results via a shared `check_float_result` helper, catching overflow (`1e308 + 1e308`) at point of origin. This matches the consensus approach for config languages targeting JSON output (Jsonnet, Nickel, CUE all reject non-finite floats). With this invariant, the `<=`/`>=` NaN anomaly documented above becomes unreachable — it is retained as documentation of IEEE 754 behavior but cannot occur in practice.

**Pragmatic justification for the anomaly documentation:** The `<=`/`>=` NaN anomaly is documented but not fixed (no `is-nan` check in derived comparisons) because the finite-float invariant makes it unreachable. If the invariant were ever relaxed, the negation-based derivation would need revisiting.

### Part 6: Key Ordering (`Key::PartialOrd`)

Separate from value comparison, the `Key` type has its own partial ordering used by range access (`$data[start..end]`):

```
Key::partial_cmp:
  (Int(a),    Int(b))    → Some(a.cmp(b))     # total within Int keys
  (String(a), String(b)) → Some(a.cmp(b))     # total within String keys (lexicographic)
  (Int(_),    String(_)) → None                # mixed key types: incomparable
  (String(_), Int(_))    → None                # mixed key types: incomparable
```

Mixed-type key comparison in range access raises an error (via `key_in_range`, §Access Chain Evaluation). `Key::PartialOrd` is semantically equivalent to the Int/String subset of `dispatch_lt` but exists as a separate relation because it operates at the `Key` level (before value materialization), while `$<` operates at the `Value` level (after materialization). Range access needs to filter dict keys without forcing any values — it compares keys directly from the `IndexMap<Key, Rc<Thunk>>`, never touching the thunks. This is an optimization that preserves laziness: `$data[2..5]` filters keys without materializing any values.

### Part 7: `Value::PartialEq` vs `$=` Divergence

The Rust-level `Value::PartialEq` implementation differs from the `$=` builtin:

| Aspect | `Value::PartialEq` | `$=` builtin |
|--------|-------------------|-------------|
| Int/Float cross-type | `false` (different variants) | promotes Int → Float |
| Dict, Function, Builtin, Seq | `false` (catch-all) | `false` (catch-all) |
| NaN == NaN | `false` (IEEE 754 via `f64::eq`) | `false` (IEEE 754 via `f64::eq`) |
| Used by | Internal Rust code, tests | User-facing tinct programs |

The divergence is intentional: `Value::PartialEq` uses Rust's native dispatch (no cross-variant matching), while `$=` adds the Int/Float promotion rules (EQ-PROMOTE-IF, EQ-PROMOTE-FI) that users expect from a dynamically-typed numeric tower. Internal Rust code must use `Value::PartialEq` for exact variant matching (tests, pattern matching). User-facing tinct programs must use `$=` via the builtin. Never compare `Value` instances directly in user-facing contexts — the missing cross-type promotion would silently give wrong answers for mixed Int/Float comparisons.

### Part 8: Properties

**P1 — EQ reflexivity (conditional):** `∀v. dispatch_eq(v, v) = true` **iff** `v ∉ {NaN, Dict, Function, Builtin, Seq}`. NaN violates reflexivity per IEEE 754. Dict/Function/Builtin/Seq return false even for identity (same Rc pointer) because no structural comparison is performed — structural dict equality would violate lazy evaluation by forcing all field thunks (e.g., comparing `[x: [/ 1 0]]` with itself would force the division-by-zero error in an unreferenced field). **Future breaking change:** if typeclasses add user-defined equality, `[= [x: 1] [x: 1]]` would change from `false` to `true`. Current code relying on dicts always being unequal may break.

**P2 — EQ symmetry:** `∀v₁, v₂. dispatch_eq(v₁, v₂) = dispatch_eq(v₂, v₁)`. Holds unconditionally — the dispatch table is symmetric (EQ-PROMOTE-IF and EQ-PROMOTE-FI produce identical results; EQ-INCOMP is symmetric; IEEE 754 `==` is symmetric).

**P3 — EQ transitivity (conditional):** `dispatch_eq(a, b) ∧ dispatch_eq(b, c) → dispatch_eq(a, c)` holds within each type. **WARNING: Cross-type promotion violates transitivity at the 2⁵³ boundary.** Concrete example: `dispatch_eq(Int(2⁵³+1), Float(2⁵³)) = true` (EQ-PROMOTE-IF, both promote to same float) and `dispatch_eq(Float(2⁵³), Int(2⁵³)) = true` (EQ-PROMOTE-FI), but `dispatch_eq(Int(2⁵³+1), Int(2⁵³)) = false` (EQ-INT, distinct integers). Programs relying on equivalence substitution for integers outside [−2⁵³, 2⁵³] will observe non-transitive equality.

**P4 — LT irreflexivity:** `∀v. dispatch_lt(v, v) = false` wherever defined. Holds for Int, Float (excluding NaN, which returns false for `<` anyway), String, Bool. NaN: `NaN < NaN → false` — technically satisfies irreflexivity even though NaN is unordered.

**P5 — LT asymmetry:** `dispatch_lt(a, b) = true → dispatch_lt(b, a) = false`. Holds for all comparable types. (Consequence: `dispatch_lt(a, b) ∧ dispatch_lt(b, a)` is impossible.)

**P6 — LT transitivity:** `dispatch_lt(a, b) ∧ dispatch_lt(b, c) → dispatch_lt(a, c)` within each type. Cross-type Int/Float promotion inherits the same precision-boundary caveat as EQ transitivity (P3).

**P7 — LT/EQ trichotomy (conditional):** Trichotomy holds within each type (excluding NaN): exactly one of `dispatch_lt(a, b)`, `dispatch_eq(a, b)`, `dispatch_lt(b, a)` is true. Two violations: (1) NaN — all three are false; (2) cross-type Int/Float at the precision boundary — promotion may cause both `dispatch_lt` and `dispatch_eq` to disagree with same-type comparisons (same caveat as P3).

**P8 — Totality of EQ:** `=` never errors. For any two values (including incompatible types), it returns a Bool. This is the defining characteristic that distinguishes it from `<`.

**P9 — Partiality of LT:** `<` errors on type pairs not in the dispatch table (LT-ERROR). The comparable domain is: {Int, Float} × {Int, Float} ∪ String × String ∪ Bool × Bool.

**P10 — Materialization obligation:** Both `=` and `<` call `materialize(θ, _, d)` on both arguments before dispatch. This is a forcing operation (§Thunk Lifecycle: FORCE-EVAL, FORCE-BUILTIN, or FORCE-CALL depending on the thunk's state) — the thunk moves from Unevaluated/PendingCall/PendingBuiltin to Evaluated, and the resulting value is cached for subsequent access. If materialization detects a cycle (thunk in InProgress state), it raises a circular dependency error via FORCE-CYCLE — comparison dispatch is never reached. Note: for Dict/Seq values, `=` materializes the outer structure (forces the thunk to produce a `Value::Dict` or `Value::Seq`) but does NOT recursively force field values — it matches on the Value variant and returns `false` (EQ-INCOMP) immediately.

## Merge — Formal Specification

This section formalizes `merge`, the only builtin that allows key collision. The specification defines operational semantics (right-biased merge with insertion-order preservation), algebraic properties, interaction with record typing (closed records and row variables), and the lazy overlay compatibility invariant.

`merge` is the composition primitive: it underlies shared base config (`[merge base overrides]`), `set` (single-key overlay), `from-entries` (construction from pairs), and `map` on dicts (per-entry rebuild). Its semantics propagate through these dependents.

### Part 1: Notation

| Symbol | Meaning |
|--------|---------|
| `D = {k₁↦θ₁, ..., kₙ↦θₙ}` | A dict: ordered map from keys to thunks |
| `K(D)` | Key set of D: `{k₁, ..., kₙ}` |
| `D(k)` | Thunk bound to key k in D |
| `\|D\|` | Number of entries in D |
| `pos(D, k)` | Insertion-order position of key k in D (0-indexed) |
| `∅` | Empty dict `{}` |
| `θ` | A thunk (§Thunk Lifecycle) — values remain unevaluated |

Keys are materialized values (`Key` type: Int, String). Values are thunks — `$merge` never materializes values, only dict structure.

### Part 2: Operational Rule

**[MERGE]**

```
materialize(θ_L, _, d) ⇒ Dict(L)
materialize(θ_R, _, d) ⇒ Dict(R)
Result = L ⊕ R
───────────────────────────
merge(θ_L, θ_R, d, s) ⇒ ok_val(Dict(Result))
```

where `L ⊕ R` (right-biased merge) is defined as:

```
L ⊕ R = D  where
  dom(D) = K(L) ∪ K(R)
  D(k) = R(k)           if k ∈ K(R)         [RIGHT-BIAS]
  D(k) = L(k)           if k ∈ K(L) \ K(R)  [LEFT-KEEP]
```

**Iteration order of D:**

```
order(D) = order_L(L, R) ++ new(R, L)  where
  order_L(L, R) = [k for k in L in insertion order]
                  (values replaced by R(k) where k ∈ K(R), position unchanged)
  new(R, L)     = [k for k in R in insertion order where k ∉ K(L)]
```

Left keys retain their positions. Right keys that collide replace the value at the left key's position. Right keys that are new are appended in their original order.

**Strictness:** `S × S → D` (§Selective Materialization). Both operands are materialized eagerly to produce the result dict. Values are `Rc::clone` (thunk pointers copied, not forced). See Part 5 for a planned lazy overlay optimization.

When both operands are list-dicts (integer keys `0..n`), `merge` performs positional override, not concatenation: `merge([a b c], [x y])` produces `{0:x, 1:y, 2:c}`. Use `concat` for list concatenation.

**Error cases:**

| Condition | Error |
|-----------|-------|
| `args.len() ≠ 2` | Arity mismatch |
| `materialize(θ_L) ⇒ v` where v is not Dict | Type error: "merge: expected Dict, got {type}" |
| `materialize(θ_R) ⇒ v` where v is not Dict | Type error: "merge: expected Dict, got {type}" |
| `materialize(θ_L)` or `materialize(θ_R)` raises error | Error propagates (cycle, depth limit) |

Named arguments are rejected (`reject_named`).

### Part 3: Typing Rules

**Typing:** `merge` is typed via `TypeEnv::with_builtins()`, which registers precise builtin signatures. When an operand has type `TypeVar(α)`, the type checker falls back to T-MERGE-ANY (treating unresolved type variables as `Any`). With row-variable unification, option (a) — unifying `α` with a fresh open record type — becomes available.

**[T-MERGE] Closed records:**

T-MERGE applies only when both operands have closed record types (`RowTail::Empty`). Open records (`RowTail::RowVar`) fall through to T-MERGE-ANY.

```
Γ ⊢ L : Record(F_L, Closed),  Γ ⊢ R : Record(F_R, Closed)
───────────────────────────────────────────────────────────
Γ ⊢ merge(L, R) : Record(F_L ⊕ F_R, Closed)
```

where `F_L ⊕ F_R` is the field-level right-biased merge:

```
dom(F_L ⊕ F_R) = dom(F_L) ∪ dom(F_R)
(F_L ⊕ F_R)(k) = F_R(k)     if k ∈ dom(F_R)          [T-RIGHT-BIAS]
(F_L ⊕ F_R)(k) = F_L(k)     if k ∈ dom(F_L) \ dom(F_R) [T-LEFT-KEEP]
```

For shared keys, the right operand's type wins. This mirrors the runtime semantics: the right value is what gets returned on access.

**[T-MERGE-ANY] Gradual fallback:**

```
Γ ⊢ L : Any   or   Γ ⊢ R : Any
────────────────────────────────
Γ ⊢ merge(L, R) : Any
```

If either operand has type `Any` (unannotated, forward reference, or gradual escape), the result is `Any`. The type checker cannot compute field-level merge without knowing the field sets. This also applies when an operand is a `TypeVar` or has an open record type.

**Design choice:** When only one operand is `Any`, partial information could be preserved (e.g., `merge(Any, Record(F, Closed)) : Record(F, Open)`). This is rejected: it complicates the gradual typing story (see §Expressiveness in [Type System Extensions](07-type-extensions.md)) and gains little in practice.

**Row variable generalization:** With row-variable unification (§Row-Variable Unification — Kinded Rémy Model), the typing rule generalizes to:

```
Γ ⊢ L : Record(F_L, ρ₁),  Γ ⊢ R : Record(F_R, ρ₂)
─────────────────────────────────────────────────────
Γ ⊢ merge(L, R) : Record(F_L ⊕ F_R, ρ₃)
```

where `ρ₃` captures fields from `ρ₁` and `ρ₂` not in the known field sets. The precise definition of `ρ₃` depends on the row-unification design — Harper & Pierce (1991) require disjointness (`K(ρ₁) ∩ K(ρ₂) = ∅`) for symmetric concatenation, but tinct's right-biased semantics relax this. Rémy (1994) handles non-disjoint row extensions via presence/absence flags; tinct's right-bias is a simpler alternative that achieves similar expressiveness without the full flag system.

Row-variable unification defines how `⊕` interacts with row tails, subject to three constraints:

1. **Closed-record preservation:** When `ρ₁ = ρ₂ = ∅`, T-MERGE (closed records) is recovered as a special case.
2. **Common-tail preservation:** When `ρ₁ = ρ₂ = ρ`, then `ρ₃ = ρ` — merge preserves the common tail because it neither adds nor removes fields from the unknown extension.
3. **Principality:** The choice of `ρ₃` must preserve principal types. When `ρ₁ ≠ ρ₂`, options include: (a) fresh `ρ₃` constrained by `ρ₁` and `ρ₂`, (b) unify `ρ₁` and `ρ₂`, or (c) error on incompatible open records. See §Row-Variable Unification Case 4 (fresh row variable for shared unknown tail) for the pattern.

### Part 4: Algebraic Properties

**P1 — Right-bias identity:** `(L ⊕ R)(k) = R(k)` for all `k ∈ K(R)`. The right operand's value is always chosen for shared keys, regardless of the left operand's value. This is the defining property.

**P2 — Left identity:** `∅ ⊕ R = R`. Merging an empty dict on the left produces the right dict unchanged. Both key set and iteration order are preserved.

**P3 — Right identity:** `L ⊕ ∅ = L`. Merging an empty dict on the right produces the left dict unchanged.

**P4 — Associativity (content and iteration order):** `(A ⊕ B) ⊕ C = A ⊕ (B ⊕ C)` on both key-value content and iteration order. The rightmost dict wins for any key: in both groupings, `C(k)` wins if `k ∈ K(C)`, else `B(k)` if `k ∈ K(B)`, else `A(k)`.

Iteration-order proof: In `L ⊕ R`, the result order is `[keys from L in L's order] ++ [keys from R not in L, in R's order]`. For the three-operand case, both groupings produce `[A keys] ++ [B keys \ A] ++ [C keys \ (A ∪ B)]`, each segment preserving its source's insertion order. This follows from IndexMap's insert-at-existing-position semantics: the leftmost operand containing a key determines its position.

**P5 — Non-commutativity:** `L ⊕ R ≠ R ⊕ L` in general. Counterexample: `{x↦1} ⊕ {x↦2} = {x↦2}`, but `{x↦2} ⊕ {x↦1} = {x↦1}`. Right-bias makes merge inherently directional.

**P6 — Idempotence:** `D ⊕ D = D`. Merging a dict with itself produces the same dict (same keys, same thunks — `Rc::clone` of the same allocation).

**P7 — Monoid structure:** `(Dict, ⊕, ∅)` forms a monoid over ordered maps: ⊕ is associative on both content and iteration order (P4) with identity element ∅ (P2, P3). It is not a commutative monoid (P5). This justifies n-ary merge as a left fold: `merge*(D₁, ..., Dₙ) = (...((D₁ ⊕ D₂) ⊕ D₃)... ⊕ Dₙ)`, where later operands take priority. By P4, any grouping produces the same result.

**P8 — Value preservation:** `merge` never materializes, transforms, or copies values. It copies thunk pointers (`Rc::clone`). After `D = L ⊕ R`, for any key k, `D(k)` is the exact same `Rc<Thunk>` as `R(k)` or `L(k)` — not a new thunk wrapping the old one. This preserves sharing (§Thunk Lifecycle: evaluate-at-most-once).

### Part 5: Lazy Overlay Compatibility (Planned Future Design)

**Current implementation:** `merge` eagerly materializes both operands (lines 437-438 in `builtins.rs`). The specification below describes a planned lazy overlay optimization that would defer materialization.

The lazy overlay representation would defer the merge operation itself:

```
Overlay(L, R) — O(1) construction
  access(k): if k ∈ K(R) then R(k) else L(k) — O(1) per key
  iterate:   flatten to concrete IndexMap — O(|L| + |R|)
```

The lazy overlay must satisfy **behavioral equivalence**: for any program P, replacing the eager `L ⊕ R` with `Overlay(L, R)` produces the same observable results (modulo documented error timing differences). Specifically:

1. **Same values:** `Overlay(L, R)(k) = (L ⊕ R)(k)` for all `k ∈ dom(L) ∪ dom(R)`
2. **Same iteration order:** When flattened, `iterate(Overlay(L, R))` produces keys in the same order as `L ⊕ R`
3. **Same sharing:** Overlay access must preserve the `Rc::clone` contract from P8 — `Overlay(L, R)(k)` returns `Rc::clone` of `R(k)` or `L(k)`, the same `Rc<Thunk>` that eager merge would produce. This is pointer-level identity (`Rc::ptr_eq`), not just logical equivalence.

The overlay introduces two observable differences, both intentional:

**Error timing:** With the lazy overlay, materialization of both L and R is deferred until access or iteration. A dict that would fail materialization (e.g., contains a cycle) fails at access time rather than at merge time. This is an intentional behavior of the overlay design — see §Laziness Design.

**Error ordering:** When both operands contain errors, eager merge reports L's error first (L is materialized before R at `builtins.rs:446-447`). Overlay reports whichever operand's error is triggered first by access patterns. Programs should not depend on which operand's error is reported when both are broken.

**Chained overlays:** `Overlay(Overlay(A, B), C)` has O(k) access per key for k chained merges. Flattening on iteration prevents unbounded chain depth during traversal. Overlay chain traversal is structural (key lookup, not thunk forcing) and does not consume depth budget from `MAX_EVAL_DEPTH` — it is analogous to `$get` on a nested scope chain, not to recursive materialization.

### Part 6: Implementation Correspondence

| Spec element | Implementation |
|-------------|----------------|
| MERGE rule | `builtin_merge` (`builtins.rs:425-454`) |
| `materialize(θ_L, _, d)` | `materialize(&args[0], None, depth)` (line 437) |
| `materialize(θ_R, _, d)` | `materialize(&args[1], None, depth)` (line 438) |
| `require_dict` | `require_dict("merge", left_val, call_span)` (lines 439-440) |
| LEFT-KEEP | First loop: `result.insert(key.clone(), Rc::clone(thunk))` (lines 446-448) |
| RIGHT-BIAS | Second loop: `result.insert(key.clone(), Rc::clone(thunk))` (lines 450-452) |
| Iteration order | IndexMap preserves insertion order; `insert` on existing key replaces value at existing position |
| Value preservation (P8) | `Rc::clone(thunk)` — pointer copy, no materialization |
| `reject_named` | `reject_named("merge", named, call_span)` (line 433) |
| Arity check | `args.len() != 2` (line 434) |

### Part 7: Worked Example

```tinct
base:  [timeout: 30  retries: 3  env: "staging"]
prod:  [merge base [env: "prod"  timeout: 60]]
```

Applying MERGE:

```
L = {timeout↦θ(30), retries↦θ(3), env↦θ("staging")}
R = {env↦θ("prod"), timeout↦θ(60)}

K(L) = {timeout, retries, env}
K(R) = {env, timeout}
K(L) ∩ K(R) = {timeout, env}    (shared keys — R wins)
K(R) \ K(L) = ∅                  (no new keys from R)

L ⊕ R:
  timeout → θ(60)     [RIGHT-BIAS: R has timeout]     pos 0 (from L)
  retries → θ(3)      [LEFT-KEEP: only in L]          pos 1 (from L)
  env     → θ("prod") [RIGHT-BIAS: R has env]         pos 2 (from L)

Result: {timeout↦θ(60), retries↦θ(3), env↦θ("prod")}
```

Note: `timeout` stays at position 0 (its position in L), not position 1 (its position in R). `retries` stays at position 1. No new keys from R, so nothing appended. Values `θ(60)`, `θ(3)`, `θ("prod")` are thunk pointers — the integers and string are not materialized by `$merge`.
