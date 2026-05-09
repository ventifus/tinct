# What If: Consistent Error Handling for tinct

**State:** Proposal

What would it take to give tinct a single, prescriptive convention for error handling — one that is type-safe, composable, and works cleanly with lazy evaluation?

## Current State

Tinct has the primitives for structured error handling (`try`, `error`, structural ADTs, `match`) but no documented convention for when to use which pattern. Four incompatible patterns appear in the wild:

**Propagation** — most builtins and stdlib I/O functions (`net.llt`'s `fetch`, `http-get`). Errors propagate as uncaught runtime errors until the value is demanded. The caller must remember to wrap in `try` for recovery. In a lazy language, this is particularly dangerous: the error surfaces at observation time (JSON output, another expression), potentially far from the source.

**`{ok: T}` / `{err: String}` Result** — `prelude.llt`'s `has?`, `find-deep-try`, `try-or`. The right pattern for expected failure, used inconsistently. No named type alias; callers use raw structural matching.

**Sentinel strings** — `samples/versions.llt`'s original `https-get` returned `"ERR:..."` strings. Appropriate in user scripts where a table must render even when one cell fails; never appropriate in stdlib.

**Null / empty dict** — `get-or` returns a caller-supplied default on missing key. Correct for "not found" (a normal outcome), but confused at some call sites with "failure" (an exceptional outcome).

### What's Missing

1. No canonical `Result` type that the type system can check — `{ok: T} | {err: String}` is unenforceable without union types (BAS).
2. No composition primitive — chaining N fallible operations requires N nested `match` expressions.
3. No `[do]` form for sequential composition analogous to Haskell's do-notation.
4. No documented rule for which functions should return Result vs propagate vs return defaults.
5. `net.llt`'s `fetch`, `read-file`, and other I/O functions propagate instead of returning Result, breaking the implicit contract.

## Why Consistent Error Handling Matters for tinct

Tinct's lazy evaluation model makes the status quo worse than it would be in an eager language. A value that may crash is just a thunk until forced — the error surfaces at observation time, not at the call site. Capturing errors at the I/O boundary (where the failure actually occurs) and representing them as `{ok: T} | {err: String}` makes programs predictable: callers can inspect whether a field succeeded or failed before committing to output.

For a configuration language specifically, partial success is a common and useful outcome. A script that fetches 28 crate versions should produce 27 good cells and 1 error cell, not crash entirely because one crates.io request timed out. Structural Result enables this naturally.

## Design

### The Convention

Three rules cover all cases:

**Rule 1 — Fallible I/O returns Result.** Functions that perform network I/O, file I/O, or parse untrusted external input return `{ok: T} | {err: String}`. Failure is an expected outcome, not a bug. Callers use `match` or `[do]` to handle both cases.

**Rule 2 — Pure functions propagate.** Functions that operate on values already in memory (string manipulation, arithmetic, collection transforms) use `[error "msg"]` for misuse. These failures are programming errors, not expected conditions. Callers do not need to handle them routinely; `try` is available for the rare case where defensive recovery is needed.

**Rule 3 — "Not found" returns a typed default.** Lookups like `get-or` and `has?` return the caller's supplied default or a Bool. They do not return Result. "Not found" is a normal outcome of a lookup, not a failure.

Sentinel strings (`"ERR:..."`) are never used in stdlib. They are a user-script workaround for rendering partial results in tabular output — not a pattern to institutionalize.

### The `Result` Type

The structural Result type is:

```tinct
# The type of a successful result
[ok: T]

# The type of a failed result
[err: String]

# The union (requires BAS for full type-system enforcement)
# {ok: T} | {err: String}
```

No type alias declaration is required for the runtime behavior — the pattern is structural. When BAS is adopted, `{ok: T} | {err: String}` becomes a proper union type with exhaustiveness checking in `match`. Until then, `match` arms work at runtime; type annotations are informational.

Parameterized Result annotations use the type annotation form:

```tinct
fetch@[ok: Dict]      # fetch returns {ok: Dict} | {err: String}
parse-json@[ok: Any]  # parse-json returns {ok: Any} | {err: String}
```

Custom error payloads are expressed by varying the `err:` field type:

```tinct
[ok: Dict  err: [code: Int msg: String]]   # structured error
```

### Combinators

Four combinators in `prelude.llt` (or a dedicated `stdlib/result.llt`) make Result composable:

```tinct
# Sequence: if result is {ok: v}, call f with v; pass {err: e} through unchanged.
and-then: [fn [result f]
  [match result
    [ok: v] [f v]
    [err: e] [err: e]]]

# Transform: apply f to the ok value; pass {err: e} through unchanged.
result-map: [fn [result f]
  [match result
    [ok: v] [ok: [f v]]
    [err: e] [err: e]]]

# Default: unwrap ok value; return default on err.
result-or: [fn [result default]
  [match result
    [ok: v] v
    [err: _] default]]

# Wrap: lift a plain value into Result.
result-ok: [fn [v] [ok: v]]
```

These four functions are all that is needed to compose Result chains without nesting. `and-then` is the monadic bind; `result-map` is fmap; `result-or` is `fromMaybe`; `result-ok` is `pure`/`return`.

### The `[do]` Macro

`[do]` provides sequential composition of Result-returning operations using a readable binding syntax, eliminating nested `and-then` calls. It dispatches via a **monad dictionary** — a dict with a `bind:` field — passed as its first argument:

```tinct
[do monad
  [binding: expr]
  ...
  final-expr]
```

The macro desugars this to nested calls of `monad.bind`:

```tinct
[monad.bind expr1 [fn [binding1]
  [monad.bind expr2 [fn [binding2]
    ...
    final-expr]]]]
```

The `result` monad dictionary is defined in prelude:

```tinct
result: [
  bind: and-then
  pure: result-ok]
```

Usage:

```tinct
[do result
  [r:    [fetch %nc "https://crates.io/api/v1/crates/blake3"]]
  [data: [from-json r.body]]
  [get "max_stable_version" [get "crate" data]]]
```

This reads left-to-right, with each line binding the success value of the previous step. If any step returns `{err: e}`, `and-then` short-circuits and the whole `[do]` expression evaluates to `{err: e}`.

The `[do]` macro is not limited to Result. Any dict with a `bind:` field works:

```tinct
# A hypothetical sequence monad for list comprehension
seq-monad: [bind: flat-map  pure: [fn [x] [x]]]

[do seq-monad
  [x: [1 2 3]]
  [y: [10 20]]
  [* x y]]
# → [10 20 20 40 30 60]
```

This generality preserves the path to HKT-based Monad typeclass dispatch in the future: when HKT and typeclass inference are available, the explicit monad dict argument becomes optional (inferred from the return type of the first expression), and the desugaring dispatches through the `Monad` typeclass instead of a runtime field access. The user-facing `[do]` syntax stays unchanged.

### Macro Desugaring Rules

`[do monad bindings... final]` desugars as follows:

- If the first step after the monad is `[name: expr]` (a binding), desugar to `[monad.bind expr [fn [name] <rest>]]`
- If a step has no binding name (a bare expression), desugar to `[monad.bind expr [fn [_ignored] <rest>]]`
- The final expression (no `:` colon form) is the return value of the chain
- A lone final expression with no bindings is just `final` (no wrapping)

Error handling within `[do]`:

```tinct
# Exit early with a custom error
[do result
  [r: [fetch %nc url]]
  [if [not [= r.status 200]]
    [err: [str "HTTP " r.status]]   # short-circuit
    [ok: r]]]
```

Since the macro desugars to `and-then`, any expression that evaluates to `{err: e}` short-circuits the chain. Callers can inject errors anywhere by returning `[err: msg]`.

### Stdlib Retrofit

All stdlib I/O functions that currently propagate are updated to return `{ok: T} | {err: String}`:

- `stdlib/net.llt`: `fetch`, `http-get` — wrap connection/read errors in `{err: msg}`
- `stdlib/io.llt`: `read-file`, `read-lines` — wrap file-not-found, permission errors
- `stdlib/toml-lite.llt`: `parse-toml-lite` — wrap parse errors (currently propagates on malformed input)
- `stdlib/regex.llt`: `re-match`, `re-find` — these are pure (pattern mismatch is normal, not failure); keep as Bool/Dict returns

Pure stdlib functions that already use `[error "msg"]` for misuse are unchanged: `sort-by`, `flatten`, `any?`, `all?`, etc.

## What Would Change

### `stdlib/prelude.llt`

**Current:** `try-or` is the only composition helper. `has?` and `find-deep-try` use ad-hoc Result internally but don't expose it in their signatures.

**Proposed:** Add `and-then`, `result-map`, `result-or`, `result-ok`, and the `result` monad dict to the exported public dict.

**Impact:** Minor — additive. Existing `try-or` is kept (it's the eager version of `result-or`).

### Macro System

**Current:** `[defmacro]` supports procedural AST transformation. No `[do]` macro exists.

**Proposed:** Implement `[do monad ...]` as a macro in `stdlib/macros.llt` (or as a built-in desugar handled at the expand phase alongside `[fn]`, `[match]`, etc.). The macro needs access to the binding structure `[name: expr]` within the steps, which is already visible to the AST transformer.

**Impact:** Moderate — new macro. The expand pass already handles `[fn]`, `[match]`, `[quote]`; `[do]` fits the same pattern.

### `stdlib/net.llt`, `stdlib/io.llt`, `stdlib/toml-lite.llt`

**Current:** Propagate errors.

**Proposed:** Return `{ok: T} | {err: String}` from all fallible operations.

**Impact:** Breaking change for callers that assume these functions return plain values. The fix at every call site is to add `[do result ...]` or an explicit `match`.

### Type System

**Current:** `{ok: T} | {err: String}` in a type annotation is treated as `Unknown` (no union unification).

**Proposed:** No change to the type checker now. With BAS, `{ok: T} | {err: String}` becomes a checkable union type and `match` exhaustiveness is enforced. The convention works at runtime before BAS; it becomes type-safe after.

**Impact:** None now. Major positive impact when BAS lands.

## Prerequisites

**`defmacro` system** — already implemented (`stdlib/macros.llt`, `tmpl-transformer` sprint complete). The `[do]` macro is a new entry in the same system.

**BAS (boolean-algebraic subtyping)** — not required for the convention or runtime behavior. Required for the type checker to enforce `{ok: T} | {err: String}` as a proper union type and report exhaustiveness errors on non-exhaustive `match`. See `doc/whatif/boolean-algebraic-subtyping.md`.

**HKT (higher-kinded types)** — not required. The explicit monad-dict approach works without HKT. If HKT is adopted in the future, `[do]` gains implicit dispatch and the monad argument becomes optional.

## References

- Wadler, P. (1985). "How to replace failure by a list of successes." *FPCA 1985*. — [The connection between `Maybe`, list monad, and failure handling; foundation for `and-then` composition]
- Wadler, P. (1992). "The Essence of Functional Programming." *POPL 1992*. — [Monads as a unified model for IO, state, and exceptions; the theoretical basis for `result` monad dict]
- Wlaschin, S. (2014). "Railway-Oriented Programming." *NDC Oslo 2014*. — [Practical two-track composition of Result-returning functions; directly describes `and-then` pattern without monadic machinery]
- Leijen, D. & Meijer, E. (2001). "Parsec: Direct-Style Monadic Parser Combinators for the Real World." *Haskell Workshop 2001*. — [Shows how error-propagating parsers compose cleanly with Result types; relevant to `toml-lite.llt` and `parse-http-response` retrofit]
- Czaplicki, E. (2012). "Elm: Concurrent FRP for Functional GUIs." Harvard thesis. — [Elm's design choice of `Result.andThen` over Monad typeclass; explicit dict-passing as HKT-free alternative; the closest language design precedent for tinct's approach]
