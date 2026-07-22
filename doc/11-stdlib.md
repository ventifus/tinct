# Standard Library

## Language vs Stdlib Boundary

### Argument Order Convention

**Most stdlib functions are data-last** to enable `->` threading (pipeline composition):

```tinct
[-> users
  [filter is-active]
  [map get-name]
  [sort]]
=== error
type errors:
  undefined variable: users at 1:5-1:10
  arity mismatch: expected 2 argument(s), got 1 (1 positional, 0 named) at 2:3-2:21
  arity mismatch: expected 2 argument(s), got 1 (1 positional, 0 named) at 3:3-3:17
  arity mismatch: expected 1 argument(s), got 0 (0 positional, 0 named) at 4:3-4:9

```

Each function receives data as its **last** parameter: `[map fn data]`, `[filter pred data]`, etc. This aligns with Unix pipe semantics (`data | transform`) and allows partial application patterns in languages with currying.

**`get-or` and `get-in-or` are subject-last** (dict last), consistent with all other stdlib functions:

```tinct
[get-or "timeout" 30 config]      # key, default, dict
[get-in-or ["db" "host"] "localhost" config]  # path, default, dict
=== error
type errors:
  undefined variable: config at 1:22-1:28
  undefined variable: config at 2:37-2:43

```

**Rationale:** Subject-last order aligns with Unix pipe semantics — `config | [get-or "timeout" 30]` works naturally. The dict (subject) is the last argument so it can be piped in.

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
=== error
type errors:
  undefined variable: x at 2:8-2:9
  undefined variable: cached-value at 4:5-4:17
  undefined variable: expensive-compute at 4:19-4:36

```

### Language vs Stdlib

Tracking what must be built into the language vs what can be implemented in the stdlib.

## Language Builtins (Special Forms)

These require special evaluation or parsing rules — they can't be expressed as regular functions. The parser recognizes them by checking the first entry of every `[]`:

- `call` — function application (exact arity required)
- `fn` — function definition (creates scope, binds params)
- `type` — type alias declaration
- `match` — pattern matching with exhaustiveness checking and arm bindings
- `quote` — captures AST as data without evaluating (code-as-data)
- `unquote` — splices evaluated values into quoted templates (inside `quote` only)
- `unquote-splice` — splices sequence elements into quoted list positions (inside `quote` only)

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
| `merge` | **Materializing** — builds new `IndexMap` from both operands (O(n)); individual values remain as lazy thunks. |
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
- `=`, `<`, `>`, `<=`, `>=` (work on Int, Float, String, Bool; cross-type Int/Float comparison allowed). `$=` returns `false` for Dict, Function, and Builtin values — there is no structural/deep equality. Structural/deep equality of dicts is intentionally not provided (materializing nested fields would violate lazy evaluation, and pointer equality would be inconsistent with value semantics).
- `to-int`, `to-float`, `floor`, `ceil`, `round` (numeric conversions)

**Strings** (materializing — must evaluate arguments):

- `str` (exact concat), `words` (split by space, filter empties), `join` (with separator)
- `split`, `replace`
- `upper`, `lower`, `trim`, `unindent` (`upper`/`lower` are stdlib functions in `strings.llt` built on `str-map-chars` + `str-to-upper-char`/`str-to-lower-char`)
- `str-to-upper-char`, `str-to-lower-char` — single-character case-conversion primitives (Rust builtins)
- `str-map-chars` — map a function over each Unicode character, concatenate results (Rust builtin)
- `regex-match?` — test if a regex matches anywhere in a string (Rust builtin, uses `regex` crate)

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
| `try`, `try-or` | **Materializing** — materializes body, catches exceptions. `$try` returns `[Ok value]` on success or `[Error message]` on failure (ADT variants, destructured with `match`). |

**Materialization** (runtime-supported):

- `eval` — recursively materializes all thunks (runtime-supported, may diverge on infinite structures)
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
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 4:2:11
  |
  2 | big-result: [map [fn [x] [expensive x]] big-dict]
    |           ^
```

## Rust-Native vs Tinct-Implemented Boundary

**Principle:** Only implement in Rust what cannot be expressed in Tinct itself. For operators that must remain Rust (arithmetic, comparison, control flow, sequence manipulation), register both the primary name and a stable alias. The primary name is wrapped in a Tinct prelude function; the stable alias (`builtin-*`) is visible only to `prelude.llt` during its evaluation — it is not exposed to user code or non-prelude stdlib modules.

**Rust-native builtins:**

| Group | Stable alias | Primary name | Rationale |
|-------|-------------|--------------|-----------|
| Arithmetic | `builtin-add`, `builtin-int-sub`, `builtin-mul`, `builtin-div` | `+`, `-`, `*`, `/` | Host numeric types (i64, f64). `builtin-int-sub` is the stable alias for integer subtraction. |
| Comparison | `builtin-lt`, `builtin-eq`, `builtin-gt`, `builtin-lte`, `builtin-gte` | `<`, `=`, `>`, `<=`, `>=` | Cross-type Int/Float coercion at host level. All five comparison operators have stable `builtin-*` aliases. |
| Control | `builtin-if` | `if` | Selective materialization — only the chosen branch is materialized. |
| Field intercept | — | `proxy` | Takes a handler `fn [field-name] value`; returns `Value::Proxy`. Any field access `.field` calls `handler(field-name)`. Enables proxy rows, mock objects, virtual namespaces. |
| Dict primitives | `builtin-get`, `builtin-length`, `builtin-append` | `get`, `keys`, `length`, `merge`, `append` | Operate on IndexMap directly. `get`, `length`, and `append` have stable aliases for shadowability. |
| Strings | `builtin-str`, `builtin-split`, `builtin-str-length`, `builtin-str-slice`, `builtin-replace`, `builtin-str-chars`, `builtin-char-code`, `builtin-chr`, `builtin-str-bytes`, `builtin-bytes-str`, `builtin-str-index-of`, `builtin-trim-start`, `builtin-trim-end`, `builtin-str-to-upper-char`, `builtin-str-to-lower-char`, `builtin-str-map-chars`, `builtin-regex-match?` | `str`, `split`, `replace`, `trim`, `trim-start`, `trim-end`, `str-chars`, `char-code`, `chr`, `str-bytes`, `bytes-str`, `str-index-of`, `str-to-upper-char`, `str-to-lower-char`, `str-map-chars`, `regex-match?`, `join` | Strings are opaque; all content operations require Rust. `upper`/`lower` are stdlib functions in `strings.llt`. `join` uses an O(n) string builder (dual-dispatch Dict/Seq). All string primitives have stable `builtin-*` aliases. |
| Numeric | — | `floor`, `round` | `f64::floor`, `f64::round`. `ceil` and `trunc` are derived. |
| Math | `builtin-pow`, `builtin-sqrt`, `builtin-log`, `builtin-log2`, `builtin-log10`, `builtin-exp`, `builtin-sin`, `builtin-cos`, `builtin-tan`, `builtin-asin`, `builtin-acos`, `builtin-atan`, `builtin-atan2`, `builtin-nan?`, `builtin-inf?`, `builtin-finite?` | `pow`, `sqrt`, `log`, `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `nan?`, `inf?`, `finite?` (via `stdlib/math.llt`) | Math primitives available after `[include libdir "math.llt"]`. Not in prelude — import from math.llt. `pi`, `e`, `phi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` are pure-tinct helpers in math.llt. |
| Bitwise | `builtin-band`, `builtin-bor`, `builtin-bxor`, `builtin-shl`, `builtin-shr` | `band`, `bor`, `bxor`, `shl`, `shr` | Integer bitwise operations. Prelude exports bare names. |
| Type conversion | `builtin-float` | `float` | Explicit Int→Float cast without precision guard. Prelude exports `float` wrapper. |
| Parsing | — | `to-int`, `to-float` | String-to-number only. |
| Evaluation control | `builtin-raise` | `materialize`, `raise`, `try`, `apply` | `materialize` forces to WHNF; `raise` constructs EvalError; `try` catches materialization errors; `apply` spreads dict (Key::String → named args, Key::Int sorted → positional args). The prelude exports `error` as an alias for `raise`. `builtin-raise` is the stable alias for `raise`. |
| Type introspection | — | `type-of`, `int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` | Inspect the Value enum variant. (`num?`, `record?`, `map?` are LLT stdlib aliases derived from these primitives.) |
| Sequences | `builtin-seq`, `builtin-head`, `builtin-tail`, `builtin-collect`, `builtin-range`, `builtin-repeat`, `builtin-cycle`, `builtin-iterate`, `builtin-unfold`, `builtin-filter`, `builtin-map`, `builtin-reduce`, `builtin-take`, `builtin-drop`, `builtin-join`, `builtin-concat`, `builtin-first`, `builtin-last`, `builtin-cons`, `builtin-reverse`, `builtin-sort` | `seq`, `head`, `tail`, `collect`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `filter`, `map`, `reduce`, `take`, `drop`, `join`, `concat`, `first`, `last`, `rest`, `cons`, `reverse`, `sort`, `seq?` | Dual-dispatch on Dict/Seq; require `Rc<Thunk>` manipulation. All sequence/list functions have stable aliases for shadowability except `seq?`. `rest` is implemented in tinct prelude via `tail`. |
| Include primitives | — | `load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put` | Thin Rust primitives for the self-hosted include pipeline. `load` parses source text; `expand` runs macro expansion; `eval` evaluates document expressions in the runtime env; `eval-types` evaluates in the type-stage env; `blake3` hashes content; `cap-identity` extracts DirCap identity; `include-cache-get`/`include-cache-put` manage the content-addressed include cache. Prelude implements `include`, `eval-file`, and the document pipeline using these primitives. |

**Tinct-implemented stdlib (wrappers and derived functions):**

The prelude wraps every primary-name operator that has a stable alias, making it shadowable by domain-specific stdlib modules. The `builtin-*` names used in the derivation column are accessible to all code via the environment chain (injected from the "core" builtin module during bootstrap), but are typically accessed only by the prelude and domain stdlib modules that need to call through a shadow:

| Function | Derivation | Notes |
|----------|-----------|-------|
| `<` | `[fn [a b] [builtin-lt a b]]` | Shadowable; calls stable alias `builtin-lt` |
| `=` | `[fn [a b] [builtin-eq a b]]` | Shadowable; calls stable alias `builtin-eq` |
| `+` | `[fn [a b] [builtin-add a b]]` | Shadowable; calls `builtin-add` |
| `-` | `Subtractable` typeclass dispatch | Resolved via `Subtractable` class instances to `builtin-int-sub` / `builtin-float-sub` |
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
| `and` | `[fn@[a Bool] [p b@a] [builtin-if p b false]]` | Short-circuit via lazy args; returns `b` or `false` |
| `or` | `[fn [a b] [builtin-if a a b]]` | Pass-through: returns `a` if truthy |
| `quot` | `[fn [a b] [trunc [builtin-div a b]]]` | Truncation toward zero |
| `mod` | `[fn [a b] [- a [* [quot a b] b]]]` | Algebraic identity |
| `ceil` | `[fn [x] [- 0 [floor [- 0 x]]]]` | `ceil(x) = -floor(-x)` |
| `trunc` | `[fn [x] [builtin-if [>= x 0] [floor x] [ceil x]]]` | Conditional floor/ceil |
| `words` | `[builtin-filter [fn [w] [not [builtin-eq w ""]]] [split " " s]]` | Uses stable `builtin-filter`, `builtin-eq` |

**Functions implemented in tinct (not Rust):**

The following functions are tinct implementations in `stdlib/strings.llt` or `stdlib/io.llt` / `stdlib/net.llt`, built on the primitives that require Rust:

| Function | Now lives in | Built on |
|----------|-------------|----------|
| `str-contains?` | `stdlib/prelude.llt` | `str-index-of` (Rust, O(n) substr search) |
| `starts-with?` | `stdlib/prelude.llt` | `str-index-of` |
| `ends-with?` | `stdlib/prelude.llt` | `str-slice`, `str-length` (char-based; correct for Unicode) |
| `upper` | `stdlib/strings.llt` | `str-map-chars` + `str-to-upper-char` |
| `lower` | `stdlib/strings.llt` | `str-map-chars` + `str-to-lower-char` |
| `copy` | `stdlib/io.llt` | `open`, `read-all`, `write` |
| `spki-pin` | `stdlib/net.llt` | pure dict construction, no I/O |
| `has-cap?` | `stdlib/io.llt` | `has?` (checks method key existence on protocol dict handle) |

Type predicates `num?`, `record?`, and `map?` are LLT stdlib functions defined in `prelude.llt`: `num?` is `[or [int? x] [float? x]]`; `record?` and `map?` are both aliases for `dict?` (the runtime makes no key-type distinction).

**Why shadowable wrappers matter:**

Any `include`d stdlib module can shadow the primary-name operators in lexical scope. `stdlib/sql.llt` uses this to provide SQL-aware versions of `filter`, `map`, `<`, `=`, `and`, `if`, etc. that propagate SQL expression trees when applied to proxy rows. Each shadow calls the stable `builtin-X` alias for non-SQL fallback. User code written after `[include "stdlib/sql.llt"]` gets transparent SQL dispatch without any API changes. See `doc/whatif/lib-sql.md`.

**Loading mechanism:**

Both `stdlib/loader.llt` and `stdlib/prelude.llt` are loaded automatically at startup (bundled at compile time via `include_str!`). All other stdlib modules must be loaded explicitly with `[include ...]`. Startup follows a four-phase bootstrap inside `create_stdlib_env_inner()`:

1. **Phase 1 — core env:** A fresh environment is populated with all builtins from `builtin_module("core")` — `builtin-lt`, `builtin-add`, `eval`, `raise`, `load`, `blake3`, etc.
2. **Phase 2 — loader.llt:** `stdlib/loader.llt` is evaluated in the core env, producing `eval-program` and `eval-programs` (the pipeline entry points). `make-entry` is a private helper in a preceding dict.
3. **Phase 3 — prelude.llt:** `stdlib/prelude.llt` is evaluated using `eval-programs`, with the loader dict injected into scope. Prelude defines its private helpers in a first dict and its full public API in a second dict.
4. **Phase 4 — stdlib env:** The prelude's public dict becomes the stdlib env. User code and domain stdlib modules inherit this env.

```text
Core env: builtin_module("core") (builtin-lt, builtin-add, eval, raise, load, blake3, ...)
  └── Loader dict: loader.llt (eval-program, eval-programs, make-entry)
        └── Prelude dict: prelude.llt exports (<, =, +, -, *, /, if, filter, map, not, >, and, or, ...)
              └── User code / domain stdlib ([include "stdlib/sql.llt"] shadows filter, map, <, =, ...)
                    └── User predicates and programs
```

The prelude wraps builtins with stable `builtin-*` aliases (`builtin-add`, `builtin-int-sub`, `builtin-mul`, `builtin-div`, `builtin-eq`, `builtin-lt`, `builtin-if`, `builtin-seq`, `builtin-head`, `builtin-tail`, `builtin-collect`, `builtin-range`, `builtin-repeat`, `builtin-cycle`, `builtin-iterate`, `builtin-unfold`, `builtin-map`, `builtin-filter`, `builtin-take`, `builtin-drop`, `builtin-reduce`, `builtin-join`, `builtin-concat`, `builtin-first`, `builtin-last`, `builtin-cons`, `builtin-reverse`, `builtin-sort`, `builtin-get`, `builtin-length`, `builtin-append`, `builtin-str`, `builtin-split`, `builtin-str-length`, `builtin-str-slice`, `builtin-raise`) so domain modules can shadow the primary names while still calling the original implementation. All other builtins (e.g., `eval`, `load`) are used directly by name.

**Optional stdlib modules** — load with `[include libdir "<module>.llt"]`. The `libdir` variable is a `DirCap` injected at startup pointing to the installed stdlib directory (resolves to `stdlib/` in dev builds, `<prefix>/share/tinct/stdlib/` in installed builds):

| Module | Functions provided | When to include |
|--------|-------------------|-----------------|
| `numeric.llt` | `UInt8`, `UInt16`, `UInt32`, `UInt64`, `Int8`, `Int16`, `Int32`, `Int64`, `to-bytes` | Fixed-width integer type aliases with range constraints |
| `strings.llt` | `pad-left`, `pad-right`, `str-reverse`, `upper`, `lower` | String formatting, reversal, and case conversion (`str-find`, `str-repeat` are in prelude) |
| `math.llt` | `pi`, `e`, `phi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` | Math constants, derived trig/log functions |
| `encoding.llt` | `base64-encode`, `base64-decode`, `hex-encode`, `hex-decode`, `mask-apply`, `bytes-reverse`, `bytes-repeat` | Binary encoding/decoding |
| `datetime.llt` | `parse-timestamp`, `format-timestamp`, `timestamp-add`, etc. | Date/time formatting |
| `regex.llt` | `regex-match`, `regex-find-all`, `regex-replace` | Pattern matching |
| `path.llt` | `basename`, `dirname`, `path-join`, `path-ext` | File path manipulation |
| `io.llt` | `write-line`, `write-file`, `write-file-atomic`, `read-file` | File I/O helpers |
| `net.llt` | `http-get`, `fetch`, `uri-params`, `uri-origin`, `uri->string` | HTTP client utilities |
| `toml-lite.llt` | `parse-toml-lite` | Subset TOML parser |
| `codecs/json.llt` | `from-json`, `to-json` | Pure-tinct JSON parser and serializer; `from-json` parses a JSON string, `to-json` serializes a tinct value to JSON |
| `cli/in/json.llt` | JSON pipeline input (stdin → parsed dict) | Pipeline input stage |
| `cli/out/json.llt` | `json` | JSON output formatting |
| `cli/out/yaml.llt` | `yaml` | YAML output formatting |
| `cli/out/csv.llt` | `csv` | CSV output formatting |
| `cli/out/toml.llt` | `toml` | TOML output formatting |

Note: Prelude builtins (`+`, `-`, `*`, `/`, `<`, `=`, `if`, string ops, bitwise ops, `floor`, `round`, etc.) are always available without any include. Domain-specific modules require explicit include: `pow`, `sqrt`, and other math functions require `[include libdir "math.llt"]`; `hex-encode`, `base64-encode`, etc. require `[include libdir "encoding.llt"]`. Prelude functions (e.g., `starts-with?`, `ends-with?`, `str-contains?`, `str-find`, `str-repeat`) are also always available without any include.

## Organization

The stdlib follows four organizing principles:

1. **`prelude.llt`** — Universal core, auto-included. General-purpose utilities with no domain-specific functions. Two-dict pattern: private first dict (`-impl`/`-step`/`-check` helpers), public second dict (exported API). Non-prelude modules must never copy prelude exports.

2. **Domain modules** (`strings.llt`, `math.llt`, `encoding.llt`, `path.llt`, `io.llt`, `net.llt`, `regex.llt`, `toml-lite.llt`, `datetime.llt`) — Single-topic, explicit `[include %libdir "..."]` required. Depend on prelude (always in scope); two-dict pattern; no prelude duplication.

3. **Pipeline adapters** (`cli/in/`, `cli/out/`, `cli/fmt/`) — Thin wrappers for document pipeline stages; not general-purpose libraries.

4. **Protocol libraries** (`protocols/`) — Low-level RFC wire format helpers; self-contained; no prelude duplication.

| Layer | Location | Auto-loaded | Requires include |
|-------|----------|-------------|-----------------|
| Core prelude | `stdlib/prelude.llt` | Yes | No |
| Domain modules | `stdlib/*.llt` | No | Yes |
| Pipeline adapters | `stdlib/cli/in/`, `stdlib/cli/out/`, `stdlib/cli/fmt/` | No | Yes |
| Protocol libraries | `stdlib/protocols/` | No | Yes |

## Stdlib Function Reference

**Architecture:** ~244 Rust-native builtins (with stable `builtin-*` aliases for shadowability) (see `core_builtins()` in `src/builtins_core.rs` (199), `datetime_builtins()` in `src/builtins_datetime.rs` (30), `net_builtins()` in `src/builtins_net.rs` (15), dispatched via `builtin_module()`) + ~117 LLT-implemented functions in `stdlib/prelude.llt` (including shadowable wrappers). The shadowable wrappers are: operators (`<`, `=`, `+`, `-`, `*`, `/`, `if`), core collection ops (`filter`, `map`, `reduce`, `take`, `drop`), and sequence/list ops (`seq`, `head`, `tail`, `collect`, `range`, `repeat`, `cycle`, `iterate`, `unfold`, `join`, `concat`, `first`, `last`, `rest`, `cons`, `reverse`, `sort`), dict ops (`get`, `length`, `append`), and string ops (`str`, `split`, `str-length`, `str-slice`), plus `raise`. All wrapped builtins remain accessible via stable `builtin-*` aliases (e.g., `builtin-lt`, `builtin-eq`, `builtin-add`, `builtin-get`, `builtin-str`, `builtin-raise`). `collect-kv`, `str-repeat`, and `str-find` are pure LLT implementations in `prelude.llt` — shadowable via `$include` like other prelude functions, but with no `builtin-*` aliases.

**Total stdlib API:** ~244 Rust builtins + ~140 prelude LLT functions = ~384 functions available after prelude load.

Functions available to all user code. Collection operators (`map`, `filter`, `reduce`, `take`, `drop`) and arithmetic/comparison operators (`+`, `-`, `*`, `/`, `<`, `=`, `if`) are Tinct prelude wrappers over stable Rust aliases — shadowable by `$include`d modules. Private implementation details (functions suffixed with `-impl`, `-step`, `-check`) are omitted from this reference.

**Stdlib categories:**

- **Aggregates** (`sum`, `product`, `min`, `max`, `count`, `contains?`, `uniq`) — reduce-based collection summaries for common data analysis patterns
- **Higher-order utilities** (`with-entries`, `partition`, `flat-map`, `find-first`, `group-by`, `deep-merge`, `walk`, `transpose`) — advanced collection transformations following Jsonnet/jq/Nix stdlib patterns
- **Type predicates** (`int?`, `float?`, `str?`, `bool?`, `null?`, `dict?`, `fn?`, `seq?` as Rust builtins; `num?`, `bytes?`, `record?`, `map?`, `list?`, `variant?`, `proxy?` as LLT stdlib) — runtime type inspection for dynamic dispatch and validation
- **Extended strings** (`char-code`, `chr`, `str-bytes`, `bytes-str`, `str-length`, `str-slice`, `str-chars` as Rust builtins; `str-contains?`, `starts-with?`, `ends-with?`, `str-find`, `str-repeat` in prelude; `pad-left`, `pad-right`, `str-reverse` in `stdlib/strings.llt`) — string prefix/suffix matching, padding, character/byte operations; `str-contains?`, `starts-with?`, `ends-with?`, `str-find`, and `str-repeat` are prelude functions (always available); `pad-left`, `pad-right`, `str-reverse` require `[include libdir "strings.llt"]`
- **Extended math** (`pow`, `sqrt`, `log`, `log2`, `log10`, `exp`, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `nan?`, `inf?`, `finite?` in `stdlib/math.llt` as wrappers over `builtin-*` primitives; `pi`, `e`, `phi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` are pure-tinct helpers in `math.llt`) — trigonometric, exponential, and logarithmic functions; NaN/infinity checks; all require `[include libdir "math.llt"]`
- **Bitwise & Encoding** (`band`, `bor`, `bxor`, `shl`, `shr` as prelude wrappers over `builtin-*` primitives; `hex-encode`, `hex-decode`, `base64-encode`, `base64-decode` in `stdlib/encoding.llt`) — bitwise primitives available without include via prelude; pure-tinct encoding functions require `[include libdir "encoding.llt"]`
- **Bytes** (`bytes`, `bytes-find`, `bytes-of`, `bytes-equal?`, `ct-equal?` as Rust builtins) — byte buffer operations with constant-time equality for cryptographic use
- **Datetime** (`parse-timestamp`, `format-timestamp`, `timestamp->unix`, `unix->timestamp`, `now`, `fixed-clock`, `timestamp-add`, `timestamp-diff`, `timestamp<?`, `timestamp>?`, `timestamp=?`, `timestamp-year`, `timestamp-month`, `timestamp-day`, `timestamp-hour`, `timestamp-minute`, `timestamp-second`, `timestamp-parts`, `duration-nanos`, `duration-seconds`, `duration-minutes`, `duration-hours`, `duration-days`, `duration->seconds`, `duration->nanos`, `load-tz`, `timestamp-in-tz`, `local->timestamp`, `local-tz-name` as prelude wrappers over datetime module builtins) — RFC 3339 timestamp parsing/formatting, Unix epoch conversion, arithmetic, timezone handling; all available without `include`
- **URI & HTTP** (`uri`, `url`, `urn` as Rust builtins; `uri-params`, `uri-origin`, `uri->string`, `http-get`, `fetch` in `stdlib/net.llt`) — RFC 3986/8141 URI parsing; HTTP client operations via reqwest
- **Network handles** (`connect`, `tls-connect`, `tls-peer-cert`, `spki-pin`, `http-connect`, `socks5-connect`, `proxy-connect` as Rust builtins) — TCP/UDP/TLS connections with capability security; SPKI pinning; HTTP/2+3 connection pools; SOCKS5 and HTTP proxy tunneling
- **I/O handles** (`open`, `read-all`, `lines`, `write`, `write-atomic`, `flush`, `close` as Rust builtins; `write-handle` as prelude LLT wrapper; `has-cap?` and `write-line` in `stdlib/io.llt`) — file/stream I/O with capability rows (Readable/Writable/Binary/Text/Seekable/Stream/Datagram/Tls); protocol dict handles for streaming output

Predicate builtins are Rust-native; `list?` is LLT-implemented on top of them.

> **Note:** Overrides apply to the initial dispatch only; Seq corecursion steps always call the underlying Rust implementation directly.

### Placeholder Lambda Syntax (`_`)

The `_` placeholder creates anonymous single-argument lambda functions, enabling concise composition with `map`, `filter`, `reduce`, and other higher-order functions.

**Syntax:** Any expression containing `_` in argument position is automatically wrapped in `[fn [_] ...]`.

```tinct
# These are equivalent:
[map [+ _ 1] list]
[map [fn [_] [+ _ 1]] list]

# Multiple uses of _ in the same expression all refer to the same argument:
[map [* _ _] [1 2 3]]  # => [1 4 9] — squares each element

# Works with field access:
[map _.name users]
[map [fn [_] _.name] users]  # equivalent

# Compose with other functions:
[filter [> _ 0] numbers]
[reduce [+ _ _] 0 numbers]  # sum (both _ refer to the same arg in binary position)
=== error
type errors:
  undefined variable: list at 2:14-2:18
  undefined variable: list at 3:23-3:27
  undefined variable: users at 9:13-9:18
  undefined variable: users at 10:22-10:27
  undefined variable: numbers at 13:17-13:24
  undefined variable: numbers at 14:19-14:26
  arity mismatch: expected 2 arguments, got 1 at 14:1-14:27

```

**Limitations:**

- Only works for **single-argument** functions — `_` cannot create multi-argument lambdas
- All `_` occurrences in the same expression refer to the **same** argument (no `_1`, `_2` pattern)
- `_` is desugared at parse time — it's not a special value, just syntax sugar

**When to use:**

- ✅ Short inline transformations: `[map [+ _ 1] xs]`
- ✅ Field extraction: `[map _.id records]`
- ✅ Simple predicates: `[filter [> _ threshold] values]`
- ❌ Multi-argument functions: use explicit `[fn [a b] ...]`
- ❌ Complex logic with local bindings: use explicit `fn`

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
| `str-repeat` | `fn@String [n@Integer s@String]` | Repeat string `s` exactly `n` times. Pure LLT implementation using `reduce` over `range` |
| `str-find` | `fn@Integer [needle@String haystack@String]` | Find first occurrence of `needle` in `haystack`; returns byte index or -1 if not found. Pure LLT implementation |
| `unindent` | `fn@String [s@String]` | Strip common leading indentation from multi-line string. Algorithm: last line (whitespace-only) determines indent depth; strips that many characters from each content line |

**Control Flow:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `when` | `fn@[a Null] [pred body@a]` | Returns `body` if `pred` is true, else `[]` |
| `unless` | `fn@[a Null] [pred body@a]` | Returns `body` if `pred` is false, else `[]` |
| `cond` | `fn@[a Null] [pairs@Dict]` | Multi-branch conditional: takes a list of `[condition result]` pairs |
| `until` | Rust native builtin — no LLT wrapper | Iterate function until predicate holds. Applies `f` repeatedly to `x` until `pred(x)` is true. Implemented in Rust for performance (avoids per-iteration thunk allocation overhead; unlimited iterations) |

**Field Interception:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `proxy` | `[fn [handler] ...]` | Takes a handler `fn [field-name] value`; returns `Value::Proxy`. Any field access `.field` calls `handler(field-name)` |

**Dict Utilities:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `get` | `fn@Unknown [k xs@Dict]` | Key accessor, curried for pipeline composition: `[get "name" dict]`, `dict \| [get $key]` |
| `has?` | `[fn [k xs] ...]` | Check if a key exists (uses `try` around access) |
| `get-or` | `fn@a [k default@a xs@Dict]` | Get value by key with fallback default |
| `get-in` | `fn@Unknown [path@Dict xs]` | Traverse nested dicts by a list of keys; errors on missing key |
| `get-in-or` | `fn@Unknown [path@Dict default xs]` | Traverse nested dicts with fallback default |
| `empty?` | `[fn [xs] ...]` | Check if a collection has zero entries |
| `set` | `[fn [xs ...kvs] ...]` | Return new dict merged with variadic named key-value pairs |
| `remove` | `[fn [k xs] ...]` | Return new dict with key removed |
| `update` | `[fn [k f xs] ...]` | Apply function `f` to the value at key `k` |
| `values` | `[fn [xs] ...]` | Get all values as an integer-indexed list; preserves dict insertion order |
| `entries` | `[fn [xs] ...]` | Get all entries as a list of `[key: k value: v]` dicts; preserves dict insertion order |
| `from-entries` | `[fn [pairs] ...]` | Reconstruct a dict from a list or Seq of `[key: k value: v]` pairs |
| `collect-kv` | `fn@Dict [xs]` | Reconstruct dict from key-value pairs (as produced by `each-kv`). Takes a Seq of `[key: K value: V]` dicts. Pure LLT implementation using `reduce` and `merge` |

**List Operations (integer keys, dense 0..n output):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `first` | `[fn [xs] ...]` | Get the first element (key 0) |
| `nth` | `[fn [n xs] ...]` | Get element by insertion-order position (supports negative indices) |
| `last` | `[fn [xs] ...]` | Get the last element by insertion-order position |
| `rest` | `[fn [xs] ...]` | All elements except the first, reindexed from 0 |
| `cons` | `[fn [x xs] ...]` | Prepend an element, reindexing from 0 |
| `conj` | `[fn [x xs] ...]` | Append an element (delegates to `$append`) |
| `concat` | Rust native builtin — no LLT wrapper | Concatenate two collections; Seq concat is lazy (O(1) chain), Dict concat reindexes to 0..n |
| `reverse` | `[fn [xs] ...]` | Reverse a list |
| `reindex` | `[fn [xs] ...]` | Rebuild with dense 0..n integer keys |

**Sorting:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `sort` | `[fn [xs] ...]` | Sort using natural ordering (mergesort) |
| `sort-by` | `[fn [cmp xs] ...]` | Sort using a custom comparator function |
| `sorted` | `[fn [xs] ...]` | Like `sort` but accepts Seq or Dict input; collects a Seq first before sorting |
| `sorted-by` | `[fn [cmp xs] ...]` | Like `sort-by` but accepts Seq or Dict input |

**Universal Collection Operations (preserve keys):**

| Function | Signature | Description |
|----------|-----------|-------------|
| `map` | `[fn [f xs] ...]` | Apply function to every value, preserving keys. On dicts, returns a dict with PendingCall thunks (lazy); on seqs, returns a lazy seq. Note: unlike `filter`, `map` preserves key types — dict input returns dict output. |
| `map-entries` | `[fn [f xs] ...]` | Apply function to every entry `[key: k value: v]`; f receives the entry dict and returns the **new value** (keys are preserved unchanged) |
| `filter` | `[fn [pred xs] ...]` | Keep values where predicate returns true. **Asymmetry:** returns Seq when input is Dict (keys are not preserved — must evaluate predicates to determine which values survive, breaking the key-value relationship); returns lazy Seq when input is Seq. Use `collect` to convert the result back to a dict. See also `map`, which preserves dict keys. |
| `reduce` | `[fn [f init xs] ...]` | Left fold (Rust builtin; dual-dispatch Dict/Seq) |
| `fold` | `[fn [f init xs] ...]` | Alias for `reduce` — left fold, identical semantics; use whichever name fits context |
| `foldr` | `[fn [f acc xs] ...]` | Right fold: fold from the right, equivalent to `fold(f, acc, reverse(xs))` |
| `slice` | `[fn [start end xs] ...]` | Positional slice (start inclusive, end exclusive) |
| `take` | `[fn [n xs] ...]` | Take the first n entries, preserving keys |
| `take-while` | `[fn [pred xs] ...]` | Take elements from beginning while predicate holds; stop at first failure |
| `drop` | `[fn [n xs] ...]` | Skip first n entries (Rust builtin; dual-dispatch Dict/Seq) |
| `drop-while` | `[fn [pred xs] ...]` | Drop elements from beginning while predicate holds; return remaining suffix |
| `zip` | `[fn [xs ys] ...]` | Pair entries from two collections by position |
| `unzip` | `[fn [pairs] ...]` | Unzip a list of pairs into a pair of lists |
| `flatten` | `[fn [xs] ...]` | Flatten nested lists one level deep |
| `find-deep` | `[fn [target xs@Dict] ...]` | Recursively search for a key in nested dicts |

**Higher-Order Dict/List Utilities:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `with-entries` | `[fn [f xs] ...]` | Transform a dict via entries: `entries → map(f) → from-entries` |
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
| `contains?` | `[fn [val xs] ...]` | Check if a collection contains val (structural equality) |
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
| `num?` | LLT stdlib | Return true if value is an Int or Float (`[or [int? x] [float? x]]`) |
| `str?` | Rust builtin | Return true if value is a String |
| `bool?` | Rust builtin | Return true if value is a Bool |
| `null?` | Rust builtin | Return true if value is Null (empty dict `[]`) |
| `dict?` | Rust builtin | Return true if value is a Dict (includes lists, which are dicts with integer keys) |
| `fn?` | Rust builtin | Return true if value is callable (Function or Builtin) |
| `seq?` | Rust builtin | Return true if value is a Seq |
| `record?` | LLT stdlib | Return true if value is a Dict/Overlay; alias for `dict?` (runtime has no key-type tracking) |
| `map?` | LLT stdlib | Return true if value is a Dict/Overlay; alias for `dict?` (runtime has no key-type tracking) |
| `list?` | LLT stdlib | Return true if value is a Dict whose keys are all integers (i.e., a list-shaped dict) |
| `variant?` | LLT stdlib | Return true if value is a Variant |
| `payload-of` | LLT stdlib | Extract the payload dict from a Variant value; returns `[]` for unit variants |

**Numeric Predicates:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `between` | `fn@Fn [lo hi]` | Predicate factory `lo hi → (v → Bool)` for inclusive range check |
| `non-negative` | `fn@Boolean [v]` | Predicate for `v >= 0` |
| `positive` | `fn@Boolean [v]` | Predicate for `v > 0` |

## Typeclass Hierarchy

The stdlib declares the following typeclass hierarchy in `stdlib/prelude.llt`. All classes use `[class ...]` and `[instance ...]` forms — no Rust special-casing. User-defined types can implement any of these classes by declaring instances.

### Functor

```tinct
[Functor: [class [f@Operator]
  [fmap: [fn@[f b] [fn@b [a]  [f a]]]]]]
```

Instances: `Result`, `Seq`, `Maybe`.

`fmap` lifts a function `a → b` over the container `f`, producing `f b`. All `Functor` instances are covariant — the BAS rule `a <: b` implies `App(f, a) <: App(f, b)` for all stdlib Functor instances.

### Applicative

```tinct
[Applicative: [class [f@Operator] extends [Functor f]
  [pure:  [fn@[f a] [a]]]
  [lift2: [fn@[f c] [fn@c [a b]  [f a]  [f b]]]]]]
```

Instances: `Result`, `Seq`, `Maybe`.

`pure` wraps a plain value in the applicative. `lift2` lifts a two-argument function over two containers simultaneously, combining effects.

### Monad

```tinct
[Monad: [class [m@Operator] extends [Applicative m]
  [bind: [fn@[m b] [[m a]  fn@[m b] [a]]]]]]
```

Instances: `Result`, `Seq`, `Maybe`.

`bind` (the `>>=` operation) sequences monadic computations. Used by `[do]` desugaring. The monad dict provides `bind` and `pure` for the `[do]` macro:

```tinct
[do result
  [r: [fetch %nc url]]
  [data: [from-json r.body]]
  [get "items" data]]
```

### Foldable

```tinct
[Foldable: [class [t@Operator]
  [fold:   [fn@b [fn@b [b a]  b  [t a]]]]
  [to-seq: [fn@[Seq a] [[t a]]]]]]
```

Instances: `Seq`, `Record`.

`fold` is a left fold (accumulator-first, element-second argument order). Named `fold` rather than `foldl`/`foldr` because tinct sequences are finite — the left/right distinction applies only to lazy infinite structures. `FoldableSeq.fold = reduce` and `FoldableRecord.fold = reduce`.

### Traversable

```tinct
[Traversable: [class [t@Operator]
  extends [Functor t]
  extends [Foldable t]
  [traverse: [fn@[f [t b]] [f@Applicative  fn@[f b] [a]  [t a]]]]]]
```

Instances: `Seq`, `Result`.

`traverse` maps a function `a → f b` over a traversable container, collecting the applicative effects. With `Traversable`, the generic functions `sequence` and `traverse` (defined in `prelude.llt`) work over any traversable container:

```tinct
sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
  [traverse f [fn [x] x] xs]]

traverse: [fn@[f [t b]] [f@Monad  t@Traversable  fn@[f b] [a]  xs@[t a]]
  [t.traverse f xs]]
```

### Mappable

```tinct
[Mappable: [class [f@Operator]
  [map: [fn@[f b] [fn@b [a]  [f a]]]]]]
```

Instances: `Seq`, `Record`.

`Mappable` is a weaker contract than `Functor` — it requires only a `map` operation with no naturality law enforcement. Every `Functor` is `Mappable`. The stdlib `map` builtin dispatches through the `Mappable` class rather than using hardcoded special cases, so user-defined types can participate by declaring a `Mappable` instance.

### Appendable

```tinct
[Appendable: [class [a]
  [append: [fn@a [a a]]]
  [empty:  a]]]
```

Instances: `Str`, `[Seq b]`, `Record`.

`append` concatenates two values of the same appendable type. `empty` is the identity element. The `AppendableSeq` instance head is `[Seq b]` — the instance resolver pattern-matches any `Seq(T)` for fresh `b = T`.

### Instance Summary

| Class | Kind | Instances |
|-------|------|-----------|
| `Functor` | `Operator` | `Result`, `Seq`, `Maybe` |
| `Applicative` | `Operator` | `Result`, `Seq`, `Maybe` |
| `Monad` | `Operator` | `Result`, `Seq`, `Maybe` |
| `Foldable` | `Operator` | `Seq`, `Record` |
| `Traversable` | `Operator` | `Seq`, `Result` |
| `Mappable` | `Operator` | `Seq`, `Record` |
| `Appendable` | `*` | `Str`, `[Seq b]`, `Record` |

**Result Type Combinators:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `and-then` | `fn@Unknown [f result]` | Monadic bind for Result: if `res` is `[Ok v]`, apply `f(v)` (which must return a Result); if `[Error e]`, propagate the error |
| `result-map` | `fn@Unknown [f result]` | Map over Result: if `res` is `[Ok v]`, return `[Ok [f v]]`; if `[Error e]`, propagate the error |
| `result-or` | `fn@Unknown [default result]` | Extract value from Result with fallback: if `res` is `[Ok v]`, return `v`; if `[Error e]`, return `default` |
| `result` | Dict (monad dict) | Result monad dict with fields: `[bind: and-then  pure: Ok]`. Use `[Ok v]` directly to lift a plain value into Result. |

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
| `unfold` | `[fn [step seed] ...]` | Seq from step function; step returns a dict where the **first** entry (by insertion order) is the value and the **second** entry is the next state (key names are ignored; only position matters); return `[]` to stop |
| `take` | `[fn [n xs] ...]` | Dual-dispatch: on Dict, take first n entries preserving keys; on Seq, return finite Seq of first n elements |
| `seq` | `[fn [head tail] ...]` | Low-level seq constructor (cons cell) |
| `collect` | `[fn [s] ...]` | Materialize seq into dict with integer keys 0..n |
| `head` | `[fn [s] ...]` | First element of seq |
| `tail` | `[fn [s] ...]` | Rest of seq (seq, not materialized) |
| `seq?` | Rust native builtin — no LLT wrapper | True if x is a Seq |

**Assertions:**

| Function | Signature | Description |
|----------|-----------|-------------|
| `assert` | `fn@Unknown [cond msg@String]` | Assert condition; error with message if false |

## Standard Formatters

Tinct ships a set of ready-made formatters in `stdlib/cli/out/`. They are not bundled with the prelude — each must be loaded explicitly with `include`. Formatters fall into two structural categories:

- **Function-dict formatters** (`csv`, `env`, `yaml`, `toml`) — export a named function (same name as the formatter, e.g. `yaml`) in a dict. After `include`, call as `fmt.yaml value`.
- **Bare-expression formatters** (`json`, `json-pretty`, `llt`, `raw`, `none`) — evaluate immediately to a result when included; they cannot be called as a named function from the dict returned by `include`.

### Loading pattern

Formatters are invoked via the `-o` CLI flag:

```bash
tinct run -o yaml mydata.llt       # YAML output via stdlib/cli/out/yaml.llt
tinct run -o json mydata.llt       # compact JSON output
tinct run -o json-pretty mydata.llt  # indented JSON output
```

To call a formatter function directly from tinct code, load it with `include` and call its function:

```tinct
[
  fmt: [include %libdir "cli/out/yaml.llt"]
  result: [fmt.yaml my-value]
]
```

`%libdir` is the built-in reference to the directory that contains `stdlib/`. Function-dict formatters (`csv`, `env`, `yaml`, `toml`) export a named function (same name as the formatter) that accepts a value and returns a String. Bare-expression formatters (`json`, `json-pretty`, `llt`, `raw`, `none`) evaluate immediately when included and are not callable as named functions from the include result.

### Available formatters

| File | Function | Input | Output |
|------|----------|-------|--------|
| `cli/out/json.llt` | `json` | any value | compact JSON string |
| `cli/out/json-pretty.llt` | `json-pretty` | any value | indented JSON string (2-space indent) |
| `cli/out/yaml.llt` | `yaml` | any value | YAML 1.2 string |
| `cli/out/toml.llt` | `toml` | flat or nested dict | TOML string |
| `cli/out/env.llt` | `env` | flat string-keyed dict | `KEY=VALUE` lines (`.env` format) |
| `cli/out/csv.llt` | `csv` | list of dicts (same keys) | CSV string with header row |
| `cli/out/llt.llt` | `llt` | any value | LLT debug representation |
| `cli/out/raw.llt` | `raw` | String or Seq | raw string (Seq joined with newlines) |
| `cli/out/none.llt` | `none` | any value | `""` (empty — no output) |

### `cli/out/yaml.llt` — YAML serializer

Converts any tinct value to a YAML 1.2 string. Dicts with integer keys are emitted as YAML sequences; string-keyed dicts become YAML mappings. Nested dicts recurse. Scalar values render as their YAML equivalents (`null`, `true`/`false`, bare numbers, quoted strings). Strings that collide with YAML keywords (`true`, `false`, `null`, `yes`, `~`, empty string) are double-quoted automatically.

```bash
tinct run -o yaml -e '[name: "Alice" age: 30 tags: ["admin" "editor"]]'
# =>
# name: Alice
# age: 30
# tags:
# - admin
# - editor
```

### `cli/out/json.llt` — compact JSON

Converts any tinct value to compact (single-line) JSON. Dicts with sequential integer keys `0..n-1` become JSON arrays; all other dicts become JSON objects with string-coerced keys. Empty dicts serialize as `null`.

```bash
tinct run -o json -e '[name: "Alice" age: 30]'
# => {"name":"Alice","age":30}
```

### `cli/out/json-pretty.llt` — indented JSON

Produces indented JSON output with 2-space indentation. Dicts with sequential integer keys `0..n-1` become JSON arrays; all other dicts become JSON objects with string-coerced keys. Empty dicts render as `{}`, empty arrays as `[]` (compact, no newlines).

```bash
tinct run -o json-pretty -e '[x: 1 y: [2 3]]'
# => {
#   "x": 1,
#   "y": [
#     2,
#     3
#   ]
# }
```

### `cli/out/toml.llt` — TOML

Converts a tinct dict to TOML format. Flat scalar keys are emitted as top-level `key = value` pairs. Nested dict values become `[section]` tables. Integer-keyed dicts (lists) are emitted as inline TOML arrays. Nested dict values containing further nesting are emitted as inline `{}` (TOML does not support arbitrarily deep `[[array of tables]]` from this formatter).

```bash
tinct run -o toml -e '[host: "localhost" port: 5432]'
# =>
# host = "localhost"
# port = 5432
```

### `cli/out/env.llt` — KEY=VALUE

Converts a flat string-keyed dict to `.env`-style `KEY=VALUE` lines. Each entry becomes one line. Values are coerced to strings with `str`. Nested dicts are not supported — pass a flat dict.

```bash
tinct run -o env -e '[DATABASE_URL: "postgres://localhost/mydb" PORT: 3000]'
# =>
# DATABASE_URL=postgres://localhost/mydb
# PORT=3000
```

### `cli/out/csv.llt` — CSV from list of dicts

Converts a list of dicts (all sharing the same keys) to CSV format. The header row is derived from the keys of the first row. Each dict in the list becomes a data row; missing keys default to the empty string. All fields are double-quoted; literal `"` characters are escaped as `""` (RFC 4180).

```bash
tinct run -o csv -e '[[name: "Alice" score: 95] [name: "Bob" score: 87]]'
# =>
# "name","score"
# "Alice","95"
# "Bob","87"
```

**Note:** Formatters fall into three categories by implementation:

- **LLT-recursive formatters** (`csv`, `env`, `yaml`, `toml`) — implemented entirely in LLT using recursive accumulator patterns. Subject to `MAX_EVAL_DEPTH` (~256) on very deeply nested inputs. For production use with large datasets, prefer streaming or chunked approaches.
- **Tinct-implemented formatters** (`json`, `json-pretty`) — call the prelude's `to-json` / `to-json-pretty` function directly (not `codecs/json.llt`). Subject to the LLT recursion depth limit; handles sequences natively (collects and serializes as arrays). `llt` delegates to the Rust `$llt-repr` builtin.
- **Trivial formatters** (`raw`, `none`) — simple conditional or literal expressions; no recursion and no Rust serialization. `raw` passes through String or joins Seq elements with newlines; `none` always returns `""`.

**Known limitation:** The `format-instance` helper in `compact.llt` and `pretty.llt` emits a placeholder `<N arm(s)>` for instance values instead of rendering their structure. This is a temporary stub pending full instance serialization support.

## Protocol Modules

Tinct ships a set of network-protocol helpers in `stdlib/protocols/`. All four modules are **pure-tinct** — they operate on byte strings using `builtin-str-slice`, `builtin-char-code`, `builtin-chr`, and arithmetic primitives and require no Handle I/O. Each module relies on the parent scope (normally the prelude) for `length`, `str`, `set`, `get`, and `keys`.

Load with `[include libdir "protocols/<name>.llt"]`. The second document in each file contains the public API; private helpers in the first document are not exported.

| File | Functions / constants | Reference |
|------|-----------------------|-----------|
| `protocols/websocket.llt` | `build-ws-frame`, `parse-ws-frame-header`, `build-ws-handshake`; constants `Continuation`, `Text`, `Binary`, `Close`, `Ping`, `Pong` | RFC 6455 |
| `protocols/socks5.llt` | `build-socks5-greeting`, `build-socks5-connect`, `parse-socks5-response`; constants `SOCKS5-VERSION`, `NO-AUTH`, `AUTH-USERNAME-PASSWORD`, `CMD-CONNECT`, `ADDR-IPV4`, `ADDR-DOMAIN`, `ADDR-IPV6` | RFC 1928 |
| `protocols/grpc.llt` | `build-grpc-frame`, `parse-grpc-frame-header`; constant `GRPC-HEADER-LEN` | gRPC over HTTP/2 §5 |
| `protocols/dns.llt` | `encode-dns-name`, `build-dns-query`; constants `A`, `NS`, `CNAME`, `PTR`, `MX`, `TXT`, `AAAA`, `SRV`, `DNS-FLAGS-QUERY`, `DNS-CLASS-IN` | RFC 1035 |

### `protocols/websocket.llt` — WebSocket frame encoding (RFC 6455)

Build and parse WebSocket frames. All frame construction is for client-to-server direction (always masked, per RFC 6455 §5.3). Extended 64-bit payloads (>65535 bytes) are not supported.

```tinct
[include libdir "protocols/websocket.llt"]
---
[build-ws-frame Text "hello" "\x01\x02\x03\x04"]
# => [header: <bytes> payload: <masked-bytes> frame: <full-frame>]
=== error
error: invalid escape sequence: \x
 --> block 13:3:30
  |
  3 | [build-ws-frame Text "hello" "\x01\x02\x03\x04"]
    |                              ^^
```

| Function | Type | Description |
|----------|------|-------------|
| `build-ws-frame` | `(Int -> String -> String -> Dict)` | Build a masked frame. `opcode` is one of the opcode constants; `mask-key` must be exactly 4 bytes. Returns `[header: payload: frame:]`. |
| `parse-ws-frame-header` | `(String -> Dict)` | Parse raw frame bytes. Returns `[fin: rsv: opcode: masked: payload-len: header-len:]`; `header-len` is the byte offset where the payload starts. |
| `build-ws-handshake` | `(String -> String -> String -> String)` | Build an HTTP/1.1 WebSocket upgrade request. Args: `host`, `path`, `key` (base64 nonce). |

Opcode constants: `Continuation` (0), `Text` (1), `Binary` (2), `Close` (8), `Ping` (9), `Pong` (10).

### `protocols/socks5.llt` — SOCKS5 request helpers (RFC 1928)

Build client greeting and CONNECT request messages; parse server responses. Uses ATYP=3 (DOMAINNAME) for the CONNECT request — the proxy server resolves the hostname.

```tinct
[include libdir "protocols/socks5.llt"]
---
[build-socks5-greeting [0: NO-AUTH]]
# => "\x05\x01\x00"
=== error
type errors:
  undefined variable: build-socks5-greeting at 3:2-3:23

```

| Function | Type | Description |
|----------|------|-------------|
| `build-socks5-greeting` | `(Dict -> String)` | Build the initial client greeting. Arg is a dict of method-code integers, e.g. `[0: NO-AUTH]` or `[0: NO-AUTH 1: AUTH-USERNAME-PASSWORD]`. |
| `build-socks5-connect` | `(String -> Int -> String)` | Build a CONNECT request for `host:port`. Uses ATYP=3 (domain). Hostname max 255 bytes. |
| `parse-socks5-response` | `(String -> Dict)` | Parse the server response. Returns `[version: rep: status: atyp: addr: port: success?:]`. |

Protocol constants: `SOCKS5-VERSION` (5), `NO-AUTH` (0), `AUTH-USERNAME-PASSWORD` (2), `CMD-CONNECT` (1), `ADDR-IPV4` (1), `ADDR-DOMAIN` (3), `ADDR-IPV6` (4).

### `protocols/grpc.llt` — gRPC Length-Prefixed Message frames

Encode and decode the 5-byte gRPC LPM header that wraps serialized protobuf payloads over HTTP/2. Does not handle HTTP/2 framing or protobuf serialization — those layers are handled by the caller.

```tinct
[include libdir "protocols/grpc.llt"]
---
[build-grpc-frame my-proto-bytes false]
# => "\x00" + 4-byte big-endian length + proto-bytes
=== error
type errors:
  undefined variable: build-grpc-frame at 3:2-3:18

```

| Function | Type | Description |
|----------|------|-------------|
| `build-grpc-frame` | `(String -> Bool -> String)` | Prepend the 5-byte LPM header. `compressed` sets Compressed-Flag byte to 1. |
| `parse-grpc-frame-header` | `(String -> Dict)` | Parse the 5-byte header. Returns `[compressed: length: header-len:]` on success or `[err: String]` on malformed input. `header-len` is always 5. |

Constant: `GRPC-HEADER-LEN` (5).

### `protocols/dns.llt` — DNS query helpers (RFC 1035)

Build DNS wire-format query messages ready to send over UDP. Only query construction is provided — response parsing is not included.

```tinct
[include libdir "protocols/dns.llt"]
---
[build-dns-query 42 "example.com" A]
# => 12-byte header + question section (wire format, ready for UDP send)
=== error
type errors:
  undefined variable: build-dns-query at 3:2-3:17

```

| Function | Type | Description |
|----------|------|-------------|
| `encode-dns-name` | `(String -> String)` | Encode a dot-separated domain name in DNS label wire format (RFC 1035 §3.1). Empty string encodes as the DNS root (`\x00`). |
| `build-dns-query` | `(Int -> String -> Int -> String)` | Build a full DNS query message. Args: 16-bit `id`, `domain`, `qtype`. Sets RD=1 (recursion desired), QCLASS=IN. |

QTYPE constants: `A` (1), `NS` (2), `CNAME` (5), `PTR` (12), `MX` (15), `TXT` (16), `AAAA` (28), `SRV` (33). Other constants: `DNS-FLAGS-QUERY` (256, standard recursive query flags), `DNS-CLASS-IN` (1).

## Two Map Variants

- `map` — transforms values, preserves keys
- `map-entries` — receives each entry as `[key: k value: v]`, must return the **new value**; keys are preserved (to remap keys, use `with-entries`)

## Threading `->` in Stdlib

Not language syntax. Implemented in stdlib:

```tinct
->: [fn [x ...stages]
    [builtin-reduce [fn [acc f] [f acc]] x stages]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 17:1:3
  |
  1 | ->: [fn [x ...stages]
    |   ^
```

## Equality and Comparison — Formal Specification

This section formalizes the two primitive comparison builtins (`$=` and `$<`) and the three derived comparison operators (`$>`, `$<=`, `$>=`). The specification defines type-dispatch semantics, totality and partiality properties, cross-type promotion, and the algebraic properties these relations satisfy or intentionally violate.

### Part 1: Primitive Relations

Two builtins form the comparison basis. All others are derived compositions.

**EQ — Total equality (`=`):**

```text
EQ(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_eq(v₁, v₂))
```

**LT — Partial ordering (`<`):**

```text
LT(θ₁, θ₂, d, s) :
  v₁ = materialize(θ₁, _, d)
  v₂ = materialize(θ₂, _, d)
  ─────────────────────────────
  ⟨v₁, v₂⟩ ↦ Bool(dispatch_lt(v₁, v₂))    if defined
  ⟨v₁, v₂⟩ ↦ Error(type_mismatch, s)       otherwise
```

The `_` in `materialize(θ, _, d)` is the materialization span (`Option<&Span>`), passed as `None` by both builtins — it is a diagnostic concern, not a semantic parameter. The span `s` is the call-site span: unused in EQ (total function, never errors) but required for LT error reporting.

Both builtins require exactly 2 positional arguments and reject named arguments (`reject_named`). Both are **inherently materializing**: they must inspect the concrete values of both operands to produce a result. This is a §Selective Materialization boundary — comparison always materializes. If materialization of either operand raises an error (cycle detection, division by zero, depth limit), that error propagates immediately — comparison dispatch is never reached.

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
| Dict(a) | Dict(b) | structural equality (order-insensitive, recursive with cycle detection) | EQ-DICT |
| Variant(t₁,p₁) | Variant(t₂,p₂) | t₁ == t₂ ∧ recursive structural equality on payloads | EQ-VARIANT |
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

```text
Promotion: Int(n) → Float(n as f64)

Exact range:  |n| ≤ 2⁵³ (9,007,199,254,740,992)
Loss example: Int(2⁵³ + 1) promotes to Float(2⁵³)
              → EQ-PROMOTE: [call $= 9007199254740993 9007199254740992.0] = true  (!)
```

**Design rationale:** The alternative — rejecting cross-type comparison entirely — would require users to manually cast in every mixed expression. The promotion follows JavaScript, Python, Ruby, and Lua conventions. The precision-loss edge case affects only integers outside the safe range, which is rare in configuration contexts.

Promotion is **symmetric**: `EQ-PROMOTE-IF` and `EQ-PROMOTE-FI` always produce the same result because IEEE 754 `==` is symmetric and `as f64` is deterministic.

### Part 4: Derived Relations

Three comparison operators are derived from `<` and `not` in `stdlib/prelude.llt`:

```text
GT(a, b)  ≡  LT(b, a)               # >:  [fn [a b] [< b a]]
LEQ(a, b) ≡  ¬LT(b, a)              # <=: [fn [a b] [not [< b a]]]
GEQ(a, b) ≡  ¬LT(a, b)              # >=: [fn [a b] [not [< a b]]]
```

Note: `<=` is defined as `¬GT` (not as `LT ∨ EQ`), and `>=` as `¬LT` (not as `GT ∨ EQ`). These are equivalent for total orders but diverge in the presence of NaN (see Part 5). The stdlib definitions are correct because `<` is a strict weak order on each comparable type (NaN is incomparable to everything, and `¬(NaN < x)` correctly yields `true` for `>=`... but see the NaN anomaly below).

### Part 5: IEEE 754 NaN Behavior

Float comparison follows IEEE 754 semantics inherited from Rust's `f64` operations:

```text
EQ-FLOAT with NaN:   NaN == NaN → false     (IEEE 754 §5.11)
LT-FLOAT with NaN:   NaN < x   → false      (for any x, including NaN)
                      x < NaN   → false      (for any x, including NaN)
```

**Consequence for derived relations:**

```text
[= NaN NaN]  → false    (NaN ≠ NaN — correct per IEEE 754)
[< NaN 1.0]  → false    (NaN is unordered)
[> NaN 1.0]  → false    (= [< 1.0 NaN] → false)
[<= NaN 1.0] → true     (= [not [< 1.0 NaN]] = [not false] = true — ANOMALY)
[>= NaN 1.0] → true     (= [not [< NaN 1.0]] = [not false] = true — ANOMALY)
```

The `<=` and `>=` anomalies arise because the stdlib derives them via negation of the *swapped* `<`, rather than via `LT ∨ EQ`. Under IEEE 754, `¬(b < a)` is *not* equivalent to `a ≤ b` when either operand is NaN. This is a known deviation: IEEE 754 §5.11 defines `totalOrder` separately from the partial comparison predicates.

**NaN-vs-NaN anomaly:**

```text
[<= NaN NaN] → true     (= [not [< NaN NaN]] = [not false] = true)
[>= NaN NaN] → true     (= [not [< NaN NaN]] = [not false] = true)
```

Both `[<= NaN NaN]` and `[>= NaN NaN]` return `true`, even though `[= NaN NaN]` returns `false`. Tinct reports NaN as both "less-than-or-equal-to itself" and "greater-than-or-equal-to itself" while simultaneously reporting it as "not equal to itself."

**NaN/Infinity rejection (decided):** Tinct enforces the invariant that **all floats are finite**. Non-finite values are rejected at two layers: (1) `from-json` rejects `f64::INFINITY` and `f64::NAN` from `serde_json::Number::as_f64()` at parse time, closing the entry path, and (2) arithmetic builtins (`+`, `-`, `*`, `/`) reject non-finite results via a shared `check_float_result` helper, catching overflow (`1e308 + 1e308`) at point of origin. This matches the consensus approach for config languages targeting JSON output (Jsonnet, Nickel, CUE all reject non-finite floats). With this invariant, the `<=`/`>=` NaN anomaly documented above becomes unreachable — it is retained as documentation of IEEE 754 behavior but cannot occur in practice.

**Pragmatic justification for the anomaly documentation:** The `<=`/`>=` NaN anomaly is documented but not fixed (no `is-nan` check in derived comparisons) because the finite-float invariant makes it unreachable. If the invariant were ever relaxed, the negation-based derivation would need revisiting.

### Part 6: Key Ordering (`Key::PartialOrd`)

Separate from value comparison, the `Key` type has its own partial ordering used by `sort-by` ordering:

```text
Key::partial_cmp:
  (Int(a),    Int(b))    → Some(a.cmp(b))     # total within Int keys
  (String(a), String(b)) → Some(a.cmp(b))     # total within String keys (lexicographic)
  (Int(_),    String(_)) → None                # mixed key types: incomparable
  (String(_), Int(_))    → None                # mixed key types: incomparable
```

`Key::PartialOrd` is semantically equivalent to the Int/String subset of `dispatch_lt` but exists as a separate relation because it operates at the `Key` level (before value materialization), while `$<` operates at the `Value` level (after materialization).

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

**P1 — EQ reflexivity (conditional):** `∀v. dispatch_eq(v, v) = true` **iff** `v ∉ {NaN, Dict, Function, Builtin, Seq}`. NaN violates reflexivity per IEEE 754. Dict/Function/Builtin/Seq return false even for identity (same Rc pointer) because no structural comparison is performed — structural dict equality would violate lazy evaluation by materializing all field thunks (e.g., comparing `[x: [/ 1 0]]` with itself would materialize the division-by-zero error in an unreferenced field). **Future breaking change:** if typeclasses add user-defined equality, `[= [x: 1] [x: 1]]` would change from `false` to `true`. Current code relying on dicts always being unequal may break.

**P2 — EQ symmetry:** `∀v₁, v₂. dispatch_eq(v₁, v₂) = dispatch_eq(v₂, v₁)`. Holds unconditionally — the dispatch table is symmetric (EQ-PROMOTE-IF and EQ-PROMOTE-FI produce identical results; EQ-INCOMP is symmetric; IEEE 754 `==` is symmetric).

**P3 — EQ transitivity (conditional):** `dispatch_eq(a, b) ∧ dispatch_eq(b, c) → dispatch_eq(a, c)` holds within each type. **WARNING: Cross-type promotion violates transitivity at the 2⁵³ boundary.** Concrete example: `dispatch_eq(Int(2⁵³+1), Float(2⁵³)) = true` (EQ-PROMOTE-IF, both promote to same float) and `dispatch_eq(Float(2⁵³), Int(2⁵³)) = true` (EQ-PROMOTE-FI), but `dispatch_eq(Int(2⁵³+1), Int(2⁵³)) = false` (EQ-INT, distinct integers). Programs relying on equivalence substitution for integers outside [−2⁵³, 2⁵³] will observe non-transitive equality.

**P4 — LT irreflexivity:** `∀v. dispatch_lt(v, v) = false` wherever defined. Holds for Int, Float (excluding NaN, which returns false for `<` anyway), String, Bool. NaN: `NaN < NaN → false` — technically satisfies irreflexivity even though NaN is unordered.

**P5 — LT asymmetry:** `dispatch_lt(a, b) = true → dispatch_lt(b, a) = false`. Holds for all comparable types. (Consequence: `dispatch_lt(a, b) ∧ dispatch_lt(b, a)` is impossible.)

**P6 — LT transitivity:** `dispatch_lt(a, b) ∧ dispatch_lt(b, c) → dispatch_lt(a, c)` within each type. Cross-type Int/Float promotion inherits the same precision-boundary caveat as EQ transitivity (P3).

**P7 — LT/EQ trichotomy (conditional):** Trichotomy holds within each type (excluding NaN): exactly one of `dispatch_lt(a, b)`, `dispatch_eq(a, b)`, `dispatch_lt(b, a)` is true. Two violations: (1) NaN — all three are false; (2) cross-type Int/Float at the precision boundary — promotion may cause both `dispatch_lt` and `dispatch_eq` to disagree with same-type comparisons (same caveat as P3).

**P8 — Totality of EQ:** `=` never errors. For any two values (including incompatible types), it returns a Bool. This is the defining characteristic that distinguishes it from `<`.

**P9 — Partiality of LT:** `<` errors on type pairs not in the dispatch table (LT-ERROR). The comparable domain is: {Int, Float} × {Int, Float} ∪ String × String ∪ Bool × Bool.

**P10 — Materialization obligation:** Both `=` and `<` call `materialize(θ, _, d)` on both arguments before dispatch. This is a materialization operation (§Thunk Lifecycle: MATERIALIZE-UNEVALUATED, MATERIALIZE-BUILTIN, or MATERIALIZE-CALL depending on the thunk's state) — the thunk moves from Unevaluated/PendingCall/PendingBuiltin to Evaluated, and the resulting value is cached for subsequent access. If materialization detects a cycle (thunk in InProgress state), it raises a circular dependency error via MATERIALIZE-CYCLE — comparison dispatch is never reached. Note: for Dict/Seq values, `=` materializes the outer structure (materializes the thunk to produce a `Value::Dict` or `Value::Seq`) but does NOT recursively materialize field values — it matches on the Value variant and returns `false` (EQ-INCOMP) immediately.

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

```text
materialize(θ_L, _, d) ⇒ Dict(L)
materialize(θ_R, _, d) ⇒ Dict(R)
Result = L ⊕ R
───────────────────────────
merge(θ_L, θ_R, d, s) ⇒ ok_val(Dict(Result))
```

where `L ⊕ R` (right-biased merge) is defined as:

```text
L ⊕ R = D  where
  dom(D) = K(L) ∪ K(R)
  D(k) = R(k)           if k ∈ K(R)         [RIGHT-BIAS]
  D(k) = L(k)           if k ∈ K(L) \ K(R)  [LEFT-KEEP]
```

**Iteration order of D:**

```text
order(D) = order_L(L, R) ++ new(R, L)  where
  order_L(L, R) = [k for k in L in insertion order]
                  (values replaced by R(k) where k ∈ K(R), position unchanged)
  new(R, L)     = [k for k in R in insertion order where k ∉ K(L)]
```

Left keys retain their positions. Right keys that collide replace the value at the left key's position. Right keys that are new are appended in their original order.

**Strictness:** `S × S → D` (§Selective Materialization). Both operands are materialized eagerly to produce the result dict. Values are `Rc::clone` (thunk pointers copied, not materialized). See Part 5 for the lazy overlay design.

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

**Typing:** `merge` is typed via `build_builtins_type_env()` (in `builtins.rs`), which registers precise builtin signatures. When an operand has type `TypeVar(α)`, the type checker falls back to T-MERGE-ANY (treating unresolved type variables as `Any`). With row-variable unification, option (a) — unifying `α` with a fresh open record type — becomes available.

**[T-MERGE] Closed records:**

T-MERGE applies only when both operands have closed record types (`RowTail::Empty`). Open records (`RowTail::RowVar`) fall through to T-MERGE-ANY.

```text
Γ ⊢ L : Record(F_L, Closed),  Γ ⊢ R : Record(F_R, Closed)
───────────────────────────────────────────────────────────
Γ ⊢ merge(L, R) : Record(F_L ⊕ F_R, Closed)
```

where `F_L ⊕ F_R` is the field-level right-biased merge:

```text
dom(F_L ⊕ F_R) = dom(F_L) ∪ dom(F_R)
(F_L ⊕ F_R)(k) = F_R(k)     if k ∈ dom(F_R)          [T-RIGHT-BIAS]
(F_L ⊕ F_R)(k) = F_L(k)     if k ∈ dom(F_L) \ dom(F_R) [T-LEFT-KEEP]
```

For shared keys, the right operand's type wins. This mirrors the runtime semantics: the right value is what gets returned on access.

**[T-MERGE-ANY] Gradual fallback:**

```text
Γ ⊢ L : Any   or   Γ ⊢ R : Any
────────────────────────────────
Γ ⊢ merge(L, R) : Any
```

If either operand has type `Any` (unannotated, forward reference, or gradual escape), the result is `Any`. The type checker cannot compute field-level merge without knowing the field sets. This also applies when an operand is a `TypeVar` or has an open record type.

**Design choice:** When only one operand is `Any`, partial information could be preserved (e.g., `merge(Any, Record(F, Closed)) : Record(F, Open)`). This is rejected: it complicates the gradual typing story (see §Expressiveness in [Type System Extensions](07-type-extensions.md)) and gains little in practice.

**Row variable generalization:** With row-variable unification (§Row-Variable Unification — Kinded Rémy Model), the typing rule generalizes to:

```text
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

### Part 5: Lazy Overlay Compatibility

The `merge` implementation eagerly materializes both operands. The lazy overlay design defers the merge operation itself:

```text
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

**Chained overlays:** `Overlay(Overlay(A, B), C)` has O(k) access per key for k chained merges. Flattening on iteration prevents unbounded chain depth during traversal. Overlay chain traversal is structural (key lookup, not thunk materialization) and does not consume depth budget from `MAX_EVAL_DEPTH` — it is analogous to `$get` on a nested scope chain, not to recursive materialization.

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
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 18:1:5
  |
  1 | base:  [timeout: 30  retries: 3  env: "staging"]
    |     ^
```

Applying MERGE:

```text
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

## Prelude Type Signatures

Type signatures for LLT-implemented prelude functions, as declared by `@`-annotations on `fn` forms in `stdlib/prelude.llt`. Return types come from the `fn@T` annotation; parameter types come from `param@T` annotations. Functions without annotations are intentionally polymorphic (noted below).

Notation: `(A -> B -> C)` means a curried function taking `A` then `B` and returning `C`. `a` and `b` are type variables (polymorphic). `Comparable` means any type accepted by `<`: `Number`, `String`, or `Bool`.

### Type Class Hierarchy

Tinct provides a hierarchy of typeclasses in `stdlib/prelude.llt` enabling generic functions polymorphic over any type constructor (`Seq`, `Result`, `Maybe`, or user-defined).

**Kind-`* → *` classes (Operator-kinded):**

| Class | Extends | Key methods | Purpose |
|-------|---------|-------------|---------|
| `Functor` | — | `fmap: (a → b) → f a → f b` | Map a function over a wrapped value |
| `Applicative` | `Functor` | `pure: a → f a`; `lift2: (a → b → c) → f a → f b → f c` | Lift values and apply wrapped functions |
| `Monad` | `Applicative` | `bind: m a → (a → m b) → m b` | Sequential monadic composition |
| `Foldable` | — | `fold: (b → a → b) → b → t a → b`; `to-seq: t a → Seq a` | Collapse a structure to a value |
| `Traversable` | `Functor`, `Foldable` | `traverse: (a → f b) → t a → f (t b)` | Map with effects, preserving structure |
| `Mappable` | — | `map: (a → b) → f a → f b` | Weaker variant of `Functor`; no naturality law |

Instances: `FunctorResult`, `FunctorSeq`, `FunctorMaybe`, `ApplicativeResult`, `ApplicativeSeq`, `ApplicativeMaybe`, `MonadResult`, `MonadSeq`, `MonadMaybe`, `FoldableSeq`, `FoldableRecord`, `FoldableResult`, `TraversableSeq`, `TraversableResult`, `TraversableMaybe`, `MappableSeq`, `MappableDict`.

**Kind-`*` classes:**

| Class | Extends | Key methods | Purpose |
|-------|---------|-------------|---------|
| `Appendable` | — | `append: a → a → a`; `empty: a` | Monoid — concatenation with identity |
| `Equatable` | — | `eq?: a → a → Bool` | Structural equality |
| `Comparable` | `Equatable` | `lt?: a → a → Bool` | Ordering |
| `Castable` | — | `cast: source → target` | General cast protocol (two params: target, source) |

Instances: `AppendableStr`, `AppendableSeq`, `AppendableDict`, and instances of `Equatable`, `Comparable`, `Castable String` for `Int`, `Float`, `Str`, `Bool`, `Bytes`, `Dict`, `Map`.

**Generic functions over any `Monad` + `Traversable`:**

| Function | Notes |
|----------|-------|
| `sequence` | `t (m a) → m (t a)` — collect effects from a traversable |
| `traverse` | `(a → m b) → t a → m (t b)` — map with effects |
| `forM` | `t a → (a → m b) → m (t b)` — `traverse` with arguments flipped |
| `when` | `Bool → m [] → m []` — conditionally execute a monadic action |
| `liftM2` | `(a → b → c) → m a → m b → m c` — lift a binary function into the monad |

The `Maybe` type is declared in the prelude: `Maybe: [type [a] [Some a] | [None]]`.

---

### Identity

| Function | Type signature | Notes |
|----------|---------------|-------|
| `identity` | `(a -> a)` | Polymorphic — no annotation; passes through any value unchanged |
| `const` | `(a -> b -> a)` | Polymorphic — no annotation; classic K combinator |

### Logic

| Function | Type signature | Notes |
|----------|---------------|-------|
| `not` | `(Any -> Bool)` | `fn@Boolean [x]` — materializes x |
| `and` | `(Any -> a -> Union(a, Bool))` | `fn@[a Bool] [p b@a]` — short-circuit; returns `b` or `false` |
| `or` | `(Any -> Any -> Any)` | `fn@Unknown [a b]` — short-circuit; returns `a` or `b` (args need not be same type) |
| `any?` | `((a -> Bool) -> Dict a -> Bool)` | `fn@Boolean [pred@Fn xs@Dict]` |
| `all?` | `((a -> Bool) -> Dict a -> Bool)` | `fn@Boolean [pred@Fn xs@Dict]` |

### Comparison

| Function | Type signature | Notes |
|----------|---------------|-------|
| `<` | `(Comparable -> Comparable -> Bool)` | `fn@Boolean [a b]` — shadowable wrapper over `builtin-lt` |
| `=` | `(Comparable -> Comparable -> Bool)` | `fn@Boolean [a b]` — shadowable wrapper over `builtin-eq` |
| `>` | `(Comparable -> Comparable -> Bool)` | `fn@Boolean [a b]` |
| `<=` | `(Comparable -> Comparable -> Bool)` | `fn@Boolean [a b]` |
| `>=` | `(Comparable -> Comparable -> Bool)` | `fn@Boolean [a b]` |

### Arithmetic

| Function | Type signature | Notes |
|----------|---------------|-------|
| `+` | `(Number -> Number -> Number)` | `fn@Number [a@Number b@Number]` — shadowable |
| `-` | `(Number -> Number -> Number)` | `fn@Number [a@Number b@Number]` — shadowable |
| `*` | `(Number -> Number -> Number)` | `fn@Number [a@Number b@Number]` — shadowable |
| `/` | `(Number -> Number -> Number)` | `fn@Number [a@Number b@Number]` — shadowable; always returns Float at runtime |
| `quot` | `(Number -> Number -> Int)` | `fn@Integer [a@Number b@Number]` |
| `mod` | `(Number -> Number -> Number)` | `fn@Number [a@Number b@Number]` |
| `abs` | `(Number -> Number)` | `fn@Number [x@Number]` |
| `sign` | `(Number -> Int)` | `fn@Integer [x@Number]` |
| `ceil` | `(Number -> Int)` | `fn@Integer [x@Number]` |
| `trunc` | `(Number -> Int)` | `fn@Integer [x@Number]` |
| `clamp` | `(Number -> Number -> Number -> Number)` | `fn@Number [lo@Number hi@Number x@Number]` |

### String

| Function | Type signature | Notes |
|----------|---------------|-------|
| `words` | `(String -> Seq)` | `fn@Seq [s@String]` — annotation enforces `Seq(Any)`; element type is `String` at runtime but not constrained by the annotation |

### Control Flow

| Function | Type signature | Notes |
|----------|---------------|-------|
| `if` | `(Bool -> ⊤ -> ⊤ -> ⊤)` | `fn@Top [condition@Boolean then_@Top else_@Top]` — shadowable; returns chosen branch |
| `when` | `(Any -> a -> Union(a, Null))` | `fn@[a Null] [pred body@a]` — returns body or `[]` |
| `unless` | `(Any -> a -> Union(a, Null))` | `fn@[a Null] [pred body@a]` — returns body or `[]` |
| `cond` | `(Dict [Dict, Any] -> Union(a, Null))` | `fn@[a Null] [pairs@Dict]` — polymorphic result |

### Dict Operations

| Function | Type signature | Notes |
|----------|---------------|-------|
| `get` | `(a -> Dict b -> b)` | `fn@Unknown [k xs@Dict]` — polymorphic value type |
| `has?` | `(b -> Dict a -> Bool)` | `fn@Boolean [k xs@Dict]` |
| `get-or` | `(b -> a -> Dict a -> a)` | `fn@a [k default@a xs@Dict]` — polymorphic; return type unified with default type |
| `get-in` | `(Dict -> Dict a -> a)` | `fn@Unknown [path@Dict xs]` — polymorphic; errors on missing key |
| `get-in-or` | `(Dict -> a -> Dict a -> a)` | `fn@Unknown [path@Dict default xs]` — polymorphic; returns default on missing key |
| `empty?` | `(Any -> Bool)` | `fn@Boolean [xs]` — false for Seq (never empty by definition) |
| `set` | `(Dict a -> ...Dict -> Dict a)` | `fn@Dict [xs@Dict ...kvs@Dict]` — variadic named key-value pairs merged into xs |
| `remove` | `(b -> Dict a -> Dict a)` | `fn@Dict [k xs@Dict]` |
| `update` | `(b -> (a -> a) -> Dict a -> Dict a)` | `fn@Dict [k f@Fn xs@Dict]` |
| `values` | `(Dict a -> Dict a)` | `fn@Dict [xs@Dict]` — integer-indexed list of values |
| `entries` | `(Dict a -> Dict [key: b  value: a])` | `fn@Dict [xs@Dict]` |
| `from-entries` | `(Dict [key: a  value: b] -> Dict b)` | `fn@Dict [pairs]` — return annotation `@Dict`; parameter unannotated (accepts any collection with `.key`/`.value` entries) |
| `build-dict` | `(Seq [key: a  value: b] -> Dict b)` | Rust builtin — efficiently constructs dict from key-value pairs; O(n) replacement for O(n²) merge-accumulation |

#### Transient Builders

**One-shot/sequential-use invariant:** Builders are mutable containers for efficient dict construction. Once `builder-finish` is called, the builder becomes **frozen** — all subsequent mutations return errors. Builders are not safe for concurrent modification (the `Mutex` protects internal state consistency, not semantic correctness). Use builders for local accumulation patterns, then immediately finish to get the final dict.

**Pattern:** Replace O(n²) merge-accumulation with O(n) builder operations:

```tinct
# OLD: O(n²) merge-accumulation
[builtin-reduce
    [fn [acc x]
        [merge acc [make-entry [f x] x]]]
    []
    xs]

# NEW: O(n) with builder
[builder-finish
    [builtin-reduce
        [fn [b x]
            [builder-set [f x] x b]]
        [make-builder]
        xs]]
```

**Builtins:**

| Function | Type | Notes |
|----------|------|-------|
| `make-builder` | `(-> Builder)` | Create empty mutable builder |
| `builder-set` | `(Key -> a -> Builder -> Builder)` | Set key-value pair; returns builder for chaining; errors if frozen |
| `builder-delete` | `(Key -> Builder -> Builder)` | Remove key; returns builder for chaining; errors if frozen |
| `builder-finish` | `(Builder -> Dict a)` | Take inner dict, freeze builder permanently; errors if already frozen |
| `builder-snapshot` | `(Builder -> Dict a)` | Clone inner dict without freezing; errors if frozen |
| `builder-has?` | `(Key -> Builder -> Bool)` | Check if key exists; errors if frozen |
| `builder-get` | `(Key -> Builder -> a)` | Get value by key; errors if key not found or frozen |

**Example — `group-by` (O(n) with builder):**

```tinct
group-by: [fn [let f xs]
    [let raw
        [builder-finish
            [builtin-reduce
                [fn [let b x]
                    [let k [f x]]
                    [builder-set k [cons x [builder-get-or k [] b]] b]]
                [make-builder]
                xs]]]
    [map-entries [fn [let e] [reverse e.value]] raw]]
```

Each bucket accumulates elements via `cons` (O(1) prepend onto the bucket Dict). After `builder-finish`, each bucket is reversed in one O(n) pass. Total: O(n).

**Frozen invariant:** Once `builder-finish` is called, the builder's inner map is `take()`n (set to `None`). All subsequent mutations (`builder-set`, `builder-delete`) and reads (`builder-snapshot`, `builder-get`) return errors identifying the frozen state. This prevents accidental reuse after finishing.

### List Operations

| Function | Type signature | Notes |
|----------|---------------|-------|
| `first` | `(Dict a -> a)` | `fn [xs@Dict]` — no return annotation; polymorphic element type |
| `nth` | `(Int -> Dict a -> a)` | `fn [n@Integer xs@Dict]` — no return annotation |
| `last` | `(Dict a -> a)` | `fn [xs@Dict]` — no return annotation |
| `conj` | `(a -> Dict a -> Dict a)` | `fn@Dict [x xs@Dict]` |
| `reindex` | `(Dict a -> Dict a)` | `fn@Dict [xs@Dict]` |

### Sequence Constructors

| Function | Type signature | Notes |
|----------|---------------|-------|
| `range` | `(Int -> Seq Int)` or `(Int -> Int -> Seq Int)` | `fn [start]` (infinite) or `fn [start end]` (finite, end exclusive) |
| `repeat` | `(a -> Seq a)` | `fn [val]` — infinite Seq of copies of val; use `[take n [repeat val]]` for finite |
| `cycle` | `(Dict a -> Seq a)` | `fn [xs]` — infinite Seq cycling through dict entries; use `[take n [cycle xs]]` for finite |
| `iterate` | `((a -> a) -> a -> Seq a)` | `fn [f x]` — infinite seq: x, f(x), f(f(x)), ... |
| `unfold` | `((b -> [value: a  state: b]) -> b -> Seq a)` | `fn [step seed]` — Seq from step function; step returns `[value state]` or `[]` to stop |

### Collection Operations

| Function | Type signature | Notes |
|----------|---------------|-------|
| `take-while` | `((a -> Bool) -> Dict a -> Dict a)` | `fn@Dict [pred@Fn xs@Dict]` |
| `drop-while` | `((a -> Bool) -> Dict a -> Dict a)` | `fn@Dict [pred@Fn xs@Dict]` |
| `map-entries` | `(([key: k  value: a] -> b) -> Dict a -> Dict b)` | `fn@Dict [f@Fn xs@Dict]` |
| `fold` | `((b -> a -> b) -> b -> c -> b)` | `fn@Unknown [f@Fn init xs]` — delegates to `builtin-reduce` |
| `foldr` | `((b -> a -> b) -> b -> c -> b)` | `fn [f@Fn acc xs]` — no return annotation |
| `slice` | `(Int -> Int -> Dict a -> Dict a)` | `fn@Dict [start@Integer end@Integer xs@Dict]` |
| `zip` | `(a -> b -> Dict [Dict, Dict])` | `fn@Unknown [xs ys]` — lazy for Seq+Seq, eager for Dict |
| `flatten` | `(Dict a -> Dict b)` | `fn@Dict [xs@Dict]` — one level deep |
| `find-deep` | `(b -> Dict a -> a)` | `fn [target xs@Dict]` — no return annotation; searches recursively for key; errors with E000 if key not found |
| `sort-by` | `((a -> a -> Bool) -> Dict a -> Dict a)` | `fn@Dict [cmp@Fn xs@Dict]` |
| `filter` | `((a -> Bool) -> b -> Seq a)` | `fn [pred@Fn xs]` — no return annotation; shadowable; returns Seq |
| `map` | `((a -> b) -> c -> d)` | `fn [f@Fn xs]` — no return annotation; shadowable |
| `reduce` | `((b -> a -> b) -> b -> c -> b)` | `fn [f@Fn init xs]` — no return annotation; shadowable |
| `take` | `(Int -> a -> a)` | `fn [n@Integer xs]` — no return annotation; shadowable |
| `drop` | `(Int -> a -> a)` | `fn [n@Integer xs]` — no return annotation; shadowable |
| `collect-kv` | `(Seq [key: a  value: b] -> Dict b)` | `fn@Dict [xs]` — reconstructs Dict from `each-kv` output |

### Higher-Order

| Function | Type signature | Notes |
|----------|---------------|-------|
| `with-entries` | `(([key: k  value: a] -> b) -> Dict a -> Dict b)` | `fn@Dict [f@Fn xs@Dict]` |
| `partition` | `((a -> Bool) -> b -> [pass: Dict a  fail: Dict a])` | `fn@Dict [pred@Fn xs]` |
| `flat-map` | `((a -> Dict b) -> c -> Dict b)` | `fn@Dict [f@Fn xs]` |
| `find-first` | `((a -> Bool) -> b -> a)` | `fn@a [pred@Fn xs]` — returns first matching element; errors if none found |
| `find-first-or` | `((a -> Bool) -> b -> a -> a)` | `fn@a [pred@Fn default@a xs]` — returns matching element or `default` |
| `group-by` | `((a -> k) -> b -> Dict (Dict a))` | `fn@Dict [f@Fn xs]` |
| `deep-merge` | `(Dict a -> Dict a -> Dict a)` | `fn@Dict [a@Dict b@Dict]` |
| `walk` | `((a -> b) -> c -> b)` | `fn [f@Fn xs]` — no return annotation; bottom-up tree transform |
| `unzip` | `(Dict [a, b] -> [a: Dict a  b: Dict b])` | `fn@Dict [pairs]` |
| `transpose` | `(Dict (Dict a) -> Dict (Dict a))` | `fn@Dict [rows@Dict]` |
| `compose` | `((b -> c) -> (a -> b) -> (a -> c))` | `fn@Fn [f@Fn g@Fn]` |
| `->` | `(a -> Fn... -> b)` | `fn [x ...stages]` — no return annotation; variadic |

### Aggregates

| Function | Type signature | Notes |
|----------|---------------|-------|
| `sum` | `(c -> Number)` | `fn@Number [xs]` — no collection type constraint in annotation |
| `product` | `(c -> Number)` | `fn@Number [xs]` |
| `min` | `(c -> a)` | `fn@Unknown [xs]` — polymorphic; errors on empty |
| `max` | `(c -> a)` | `fn@Unknown [xs]` — polymorphic; errors on empty |
| `count` | `((a -> Bool) -> c -> Int)` | `fn@Integer [pred@Fn xs]` |
| `contains?` | `(a -> c -> Bool)` | `fn@Boolean [val xs]` |
| `uniq` | `(Dict a -> Dict a)` | `fn@Dict [xs@Dict]` |

### Type Predicates

| Function | Type signature | Notes |
|----------|---------------|-------|
| `int?` | `(Any -> Bool)` | Rust builtin |
| `float?` | `(Any -> Bool)` | Rust builtin |
| `num?` | `(Any -> Bool)` | LLT stdlib; `[or [int? x] [float? x]]` |
| `str?` | `(Any -> Bool)` | Rust builtin |
| `bool?` | `(Any -> Bool)` | Rust builtin |
| `null?` | `(Any -> Bool)` | Rust builtin |
| `dict?` | `(Any -> Bool)` | Rust builtin |
| `fn?` | `(Any -> Bool)` | Rust builtin |
| `seq?` | `(Any -> Bool)` | Rust builtin |
| `record?` | `(Any -> Bool)` | LLT stdlib alias for `dict?`; runtime has no key-type tracking |
| `map?` | `(Any -> Bool)` | LLT stdlib alias for `dict?`; runtime has no key-type tracking |
| `list?` | `(Any -> Bool)` | `fn@Boolean [xs]` — LLT stdlib; checks all keys are integers |

### Error Handling and Assertions

| Function | Type signature | Notes |
|----------|---------------|-------|
| `try-or` | `(Fn -> a -> a)` | `fn [f@Fn default]` — no return annotation; returns default on error |
| `assert` | `(Any -> String -> Bool)` | `fn@Unknown [cond msg@String]` — true path returns true; false path diverges via `[error]` |

## Supplemental Module Reference

Individual stdlib modules have detailed documentation in `doc/lib/`. These docs are generated from `@[doc: "..."]` annotations in `stdlib/` source files:

**Core modules:**

- [prelude](lib/prelude.md) — Always-available functions in `stdlib/prelude.llt`
- [numeric](lib/numeric.md) — Numeric utilities and constants
- [path](lib/path.md) — Filesystem path manipulation

**Optional modules** (load with `[include libdir "<module>.llt"]`):

- [cli-in-json](lib/cli-in-json.md), [cli-in-toml-lite](lib/cli-in-toml-lite.md) — CLI input parsers
- [cli-out-csv](lib/cli-out-csv.md), [cli-out-env](lib/cli-out-env.md), [cli-out-json](lib/cli-out-json.md), [cli-out-toml](lib/cli-out-toml.md), [cli-out-yaml](lib/cli-out-yaml.md) — CLI output formatters
- [datetime](lib/datetime.md) — Timestamp and duration utilities
- [encoding](lib/encoding.md) — Base64, hex encoding/decoding
- [formatter-compact](lib/formatter-compact.md), [formatter-pretty](lib/formatter-pretty.md) — JSON formatters
- [io](lib/io.md) — File I/O helpers
- [macros](lib/macros.md) — Code generation and metaprogramming
- [math](lib/math.md) — Mathematical functions and constants
- [out-csv](lib/out-csv.md), [out-env](lib/out-env.md), [out-json](lib/out-json.md), [out-toml](lib/out-toml.md), [out-yaml](lib/out-yaml.md) — Output formatters
- [protocols-dns](lib/protocols-dns.md), [protocols-grpc](lib/protocols-grpc.md), [protocols-socks5](lib/protocols-socks5.md), [protocols-websocket](lib/protocols-websocket.md) — Network protocol helpers
- [regex](lib/regex.md) — Regular expression utilities
- [strings](lib/strings.md) — String manipulation functions
- [toml-lite](lib/toml-lite.md) — TOML parsing
