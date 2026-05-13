# Consistent Error Handling

Implemented 2026-05-09 (`result-nominal` sprint). Nominal `Ok[T] | Err[String]`
Result type; `and-then` combinator; `[do monad ...]` macro.

## Overview

Tinct has a single, prescriptive convention for error handling — one that is type-safe,
composable, and works cleanly with lazy evaluation.

Tinct's lazy evaluation model makes inconsistent error handling particularly dangerous.
A value that may crash is just a thunk until forced — the error surfaces at observation
time, not at the call site. Capturing errors at the I/O boundary (where the failure
actually occurs) and representing them as `Ok[T] | Err[String]` makes programs
predictable: callers inspect whether a field succeeded or failed before committing to
output.

For a configuration language, partial success is a common and useful outcome. A script
that fetches 28 crate versions produces 27 good cells and 1 error cell, not a crash
because one crates.io request timed out. Nominal Result enables this naturally.

## Design

### The Convention

Three rules cover all cases:

**Rule 1 — Fallible I/O returns Result.** Functions that perform network I/O, file
I/O, or parse untrusted external input return `Ok[T] | Err[String]`. Failure is an
expected outcome, not a bug. Callers use `match` or `[do]` to handle both cases.

**Rule 2 — Pure functions propagate.** Functions that operate on values already in
memory (string manipulation, arithmetic, collection transforms) use `[error "msg"]`
for misuse. These failures are programming errors, not expected conditions. Callers
do not need to handle them routinely; `try` is available for the rare case where
defensive recovery is needed.

**Rule 3 — "Not found" returns a typed default.** Lookups like `get-or` and `has?`
return the caller's supplied default or a Bool. They do not return Result. "Not found"
is a normal outcome of a lookup, not a failure.

Sentinel strings (`"ERR:..."`) are never used in stdlib.

### The `Result` Type

Result is a **nominal union type** — a discriminated union with two constructors:

```tinct
[Result: [type [Ok a] [Err String]]]
```

`Ok` and `Err` are nominal constructors. A value is either `Ok(payload)` or
`Err(message)` — the nominal tag discriminates them, not a field name. This is
required by BAS: structural records with different field names (`{ok: T}` vs
`{err: String}`) collapse to ⊤ under S-RcdTop and cannot be discriminated at the
type level. Nominal tags remain distinct under S-ClsBot: `#Ok & #Err ≤ Never`.

```tinct
# Construction
[Ok  42]           # Ok variant with payload 42
[Err "not found"]  # Err variant with message

# Pattern matching — nominal patterns (uppercase, no colon)
[match result
  [Ok  v]:   [use v]
  [Err msg]: [error msg]]
```

The `try` builtin returns nominal variants: `Ok(value)` on success, `Err(message)` on
caught error. This replaces the former structural dict `{ok: v}` / `{err: msg}` return form.

Parameterized Result annotations use the type annotation form:

```tinct
fetch@[Ok Dict]      # fetch returns Ok[Dict] | Err[String]
parse-json@[Ok Any]  # parse-json returns Ok[Any] | Err[String]
```

### Combinators

Four combinators in `prelude.llt` make Result composable:

```tinct
# Sequence: if result is Ok v, call f with v; pass Err through unchanged.
and-then: [fn [result f]
  [match result
    [Ok v]:    [f v]
    [Err msg]: [Err msg]]]

# Transform: apply f to the Ok value; pass Err through unchanged.
result-map: [fn [result f]
  [match result
    [Ok v]:    [Ok [f v]]
    [Err msg]: [Err msg]]]

# Default: unwrap Ok value; return default on Err.
result-or: [fn [result default]
  [match result
    [Ok v]:  v
    [Err _]: default]]

# Wrap: lift a plain value into Ok.
result-ok: [fn [v] [Ok v]]
```

`and-then` is the monadic bind; `result-map` is fmap; `result-or` is `fromMaybe`;
`result-ok` is `pure`/`return`.

### The `[do]` Macro

`[do]` provides sequential composition of Result-returning operations using a readable
binding syntax, eliminating nested `and-then` calls. It dispatches via a **monad
dictionary** — a dict with a `bind:` field — passed as its first argument:

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

This reads left-to-right, with each line binding the success value of the previous
step. If any step returns `Err msg`, `and-then` short-circuits and the whole `[do]`
expression evaluates to `Err msg`.

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

This generality preserves the path to HKT-based Monad typeclass dispatch: when HKT
and typeclass inference are available, the explicit monad dict argument becomes optional
(inferred from the return type of the first expression), and the desugaring dispatches
through the `Monad` typeclass instead of a runtime field access. The user-facing `[do]`
syntax stays unchanged.

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
    [Err [str "HTTP " r.status]]   # short-circuit with nominal Err
    [Ok r]]]
```

Since the macro desugars to `and-then`, any expression that evaluates to `Err msg`
short-circuits the chain.

### Stdlib Retrofit

All stdlib I/O functions that previously propagated are updated to return
`Ok[T] | Err[String]` (nominal Result):

- `stdlib/net.llt`: `fetch`, `http-get` — wrap connection/read errors as `Err msg`
- `stdlib/io.llt`: `read-file`, `read-lines` — wrap file-not-found, permission errors as `Err msg`
- `stdlib/toml-lite.llt`: `parse-toml-lite` — wrap parse errors as `Err msg`
- `stdlib/regex.llt`: `re-match`, `re-find` — these are pure (pattern mismatch is normal, not failure); keep as Bool/Dict returns

The `try` builtin returns nominal variants: `Ok(value)` on success, `Err(message)` on
caught error. Existing prelude helpers (`has?-impl`, `try-or-impl`, `find-deep-try-check`)
migrate from structural patterns `[ok: _]` / `[err: _]` to nominal patterns
`[Ok _]` / `[Err _]`.

## Implementation

### `stdlib/prelude.llt`

Adds `and-then`, `result-map`, `result-or`, `result-ok`, and the `result` monad dict
to the exported public dict. Existing `try-or` is kept (it is the eager version of
`result-or`).

### `stdlib/macros.llt`

Implements `[do monad ...]` as a macro alongside `[defmacro tmpl]`. The expand pass
handles `[fn]`, `[match]`, `[quote]`; `[do]` fits the same pattern. The macro needs
access to the binding structure `[name: expr]` within the steps, which is visible to
the AST transformer.

### `stdlib/net.llt`, `stdlib/io.llt`, `stdlib/toml-lite.llt`

All fallible operations return `Ok[T] | Err[String]`. This is a breaking change for
callers that assume these functions return plain values. The fix at every call site is
to add `[do result ...]` or an explicit `match`.

### Type System

Nominal variants (`Ok[T]`, `Err[String]`) are supported via the typing cluster's C2/C3
nominal variant sprints. `[Result: [type [Ok a] [Err String]]]` declares a valid nominal
union type using existing machinery. With BAS, the union `Ok[T] | Err[String]` becomes
checkable via S-ClsBot (`#Ok & #Err ≤ Never` — nominal tags are disjoint), enabling
`match` exhaustiveness checking and precise arm types.

Note: structural `{ok: T} | {err: String}` is intentionally **not** used. BAS's
S-RcdTop rule identifies unions of records with different field names as ⊤ — making
structural Result useless for type-level discrimination.

## References

- Chau, C.Y. & Parreaux, L. (2026). "Boolean-Algebraic Subtyping: Intersections, Unions, Negations, and Principal Type Inference." *Proc. ACM Program. Lang.*, 10(POPL). — [S-RcdTop rule (§2.2.2) explains why structural `{ok:}|{err:}` collapses to ⊤; S-ClsBot (§2.2.2) explains why nominal `Ok|Err` is a proper discriminated union; §2.3.2 explicitly recommends class-tagged unions for this use case]
- Wadler, P. (1985). "How to replace failure by a list of successes." *FPCA 1985*. — [The connection between `Maybe`, list monad, and failure handling; foundation for `and-then` composition]
- Wadler, P. (1992). "The Essence of Functional Programming." *POPL 1992*. — [Monads as a unified model for IO, state, and exceptions; the theoretical basis for `result` monad dict]
- Wlaschin, S. (2014). "Railway-Oriented Programming." *NDC Oslo 2014*. — [Practical two-track composition of Result-returning functions; directly describes `and-then` pattern without monadic machinery]
- Leijen, D. & Meijer, E. (2001). "Parsec: Direct-Style Monadic Parser Combinators for the Real World." *Haskell Workshop 2001*. — [Shows how error-propagating parsers compose cleanly with Result types; relevant to `toml-lite.llt` and `parse-http-response` retrofit]
- Czaplicki, E. (2012). "Elm: Concurrent FRP for Functional GUIs." Harvard thesis. — [Elm's design choice of `Result.andThen` over Monad typeclass; explicit dict-passing as HKT-free alternative; the closest language design precedent for tinct's approach]
