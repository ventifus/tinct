# Builtin Reference

This chapter provides a complete reference for all 46 Rust-native builtins. For an overview of the stdlib boundary and higher-level LLT-implemented functions, see [Standard Library](11-stdlib.md). For strictness analysis and thunk lifecycle details, see [Evaluation](08-evaluation.md).

## Notation

**Arity:** Exact count or range (e.g., `2` = exactly two args, `1-2` = one or two args, `1+` = one or more).

**Strictness signature:** Describes which arguments are materialized before the builtin executes:
- `S` = Strict — argument is materialized
- `L` = Lazy — argument passes through as a thunk (never materialized by this builtin)
- `Sc` = Selectively strict — materialization is conditional on another argument's value
- `S*` = Variadic strict — all arguments are materialized

**Result type:**
- `→ V` = Value result (Int, Float, String, Bool)
- `→ D` = Container result (Dict or Seq; may contain thunks from inputs)
- `→ Θ` = Thunk result (Rc::clone of input or new PendingBuiltin/PendingCall)
- `→ LT` = Lazy-transforming result (Dict or Seq with new PendingBuiltin thunks)
- `→ ⊥` = Always raises an error; never returns

**Category:**
- **Structural** — rearranges entries without inspecting values; thunks pass through untouched
- **Materializing** — must compute values to determine the result
- **Lazy-transforming** — applies a function but produces new thunks; no computation until result is materialized
- **Selective** — materializes some arguments, leaves others as thunks

## Arithmetic

All arithmetic operations materialize both arguments and return computed values. Type promotion: `Int + Int → Int`, mixed types or `Float` → `Float`.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `+` | 2 | `S × S → V` | Int or Float | Add two numbers |
| `-` | 2 | `S × S → V` | Int or Float | Subtract second from first |
| `*` | 2 | `S × S → V` | Int or Float | Multiply two numbers |
| `/` | 2 | `S × S → V` | Float | Divide first by second (always returns Float) |

**Error cases:**
- All: Type mismatch if either arg is not Int or Float
- `/`: Division by zero (catchable via `try`)

## Comparison

Both comparison operators materialize both arguments and return Bool values.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `=` | 2 | `S × S → V` | Bool | Cross-type equality; dicts use reference equality (always false unless same Rc) |
| `<` | 2 | `S × S → V` | Bool | Less-than comparison; works on Int, Float, String (lexicographic) |

**Error cases:**
- `<`: Type mismatch if arguments are incomparable types (e.g., Int and String)

## Control Flow

| Builtin | Arity | Signature | Category | Description |
|---------|-------|-----------|----------|-------------|
| `if` | 3 | `S × Sc × Sc → Θ` | Selective | Materializes condition; returns chosen branch thunk without forcing it |

**Selective materialization:** Exactly one of the branch arguments is returned; the other is never materialized. This is the foundation for short-circuit evaluation in the stdlib (`and`, `or`, `when`, `unless`, `cond`).

**Error cases:** Type mismatch if condition is not Bool.

## Dict Primitives

Core operations on dicts. All materialize the dict structure (the IndexMap) to perform their work, but most preserve value thunks.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `keys` | 1 | `S → D` | Dict | Return dict with same keys, values are the keys themselves (newly constructed Int/String/Float) |
| `length` | 1 | `S → V` | Int | Count entries (works on Dict or Seq — materializes structure, not values) |
| `merge` | 2 | `S × S → D` | Dict | Right-biased merge; materializes both dicts for key set, values are Rc::clone thunks |
| `append` | 2 | `S × L → D` | Dict | Add entry to dict; materializes dict for key computation, value passes through as thunk |

**Error cases:**
- `keys`: Type mismatch if arg is not Dict or Seq
- `length`: Type mismatch if arg is not Dict or Seq
- `merge`: Type mismatch if either arg is not Dict
- `append`: Type mismatch if first arg is not Dict or second arg is not a two-entry dict (key-value pair)

## Strings

All string operations materialize their arguments and return computed String values.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `str` | 1+ | `S* → V` | String | Concatenate all args after stringifying them (variadic) |
| `split` | 2 | `S × S → D` | Dict | Split string by delimiter; returns dict with 0-indexed entries |
| `replace` | 3 | `S × S × S → V` | String | Replace all occurrences of pattern (arg 2) with replacement (arg 3) in string (arg 1) |
| `upper` | 1 | `S → V` | String | Convert string to uppercase |
| `lower` | 1 | `S → V` | String | Convert string to lowercase |
| `trim` | 1 | `S → V` | String | Remove leading and trailing whitespace |

**Error cases:**
- `str`: None (all types can be stringified)
- `split`: Type mismatch if either arg is not String
- `replace`: Type mismatch if any arg is not String
- `upper`, `lower`, `trim`: Type mismatch if arg is not String

## Numeric Conversion

Numeric functions materialize their arguments and return computed values.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `floor` | 1 | `S → V` | Int | Round down to nearest integer |
| `round` | 1 | `S → V` | Int | Round to nearest integer (half-up) |
| `to-int` | 1 | `S → V` | Int | Parse string to Int |
| `to-float` | 1 | `S → V` | Float | Parse string to Float |

**Error cases:**
- `floor`, `round`: Type mismatch if arg is not Float or Int
- `to-int`: Type mismatch if arg is not String; parse error if string is not a valid integer
- `to-float`: Type mismatch if arg is not String; parse error if string is not a valid float

## Evaluation Control

Control over evaluation order and error handling.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `eval` | 1 | `S → V` | Any | Deep materialization — recursively forces all thunks in the value tree |
| `error` | 1 | `S → ⊥` | Never returns | Materializes arg as error message, raises catchable error |
| `try` | 1 | `S → D` | Dict | Materializes function arg, invokes it with no args, catches errors; returns `[ok: result]` or `[error: msg]` |
| `apply` | 2 | `S × S → Θ` | Any | Materialize function and dict, call function with dict as named args |

**Error cases:**
- `eval`: Propagates any error from deep forcing
- `error`: Always raises (by design)
- `try`: Type mismatch if arg is not a function (zero-arity)
- `apply`: Type mismatch if first arg is not a function or second is not a dict

## Type Introspection

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `type-of` | 1 | `S → V` | String | Return type name: "Int", "Float", "String", "Bool", "Dict", "Seq", "Function", "Proxy" |
| `seq?` | 1 | `S → V` | Bool | Return true if arg is a Seq, false otherwise |

**Error cases:** None.

## I/O

File loading and JSON parsing.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `from-json` | 1 | `S → D` | Dict | Parse JSON string to dict; numbers become Int or Float, arrays become dicts with 0-indexed keys |
| `include` | 1 | `S → D` | Dict | Load and evaluate an LLT file; returns the file's final value |

**Error cases:**
- `from-json`: Type mismatch if arg is not String; parse error if JSON is invalid
- `include`: Type mismatch if arg is not String; file not found; parse/eval errors from included file

## Sequences

Sequence constructors create lazy Seq values; destructors materialize the Seq spine to varying degrees; higher-order operations apply functions lazily.

### Constructors

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `seq` | 2 | `L × L → D` | Seq | Construct Seq from head and tail thunks (both pass through; coinductive guard) |
| `range` | 1-2 | `S (× S)? → LT` | Seq | Finite integer range: `[call $range 5]` → 0..5, `[call $range 2 5]` → 2..5 |
| `repeat` | 1 | `L → LT` | Seq | Infinite repetition of a value (arg passes through as thunk) |
| `cycle` | 1 | `S → LT` | Seq | Infinite repetition of a dict's values (materializes dict, constructs PendingBuiltin step) |
| `iterate` | 2 | `L × L → LT` | Seq | Infinite sequence: `x, f(x), f(f(x)), ...` (both args pass through; co-recursive PendingCall + PendingBuiltin) |
| `unfold` | 2 | `L × L → Θ` | Seq | General unfold: `f(state) → [value: v  next: state']`; returns PendingBuiltin thunk |

**Error cases:**
- `seq`: None (any values can be head/tail)
- `range`: Type mismatch if args are not Int; arity error if more than 2 args
- `repeat`: None
- `cycle`: Type mismatch if arg is not Dict
- `iterate`: None (function applied lazily; errors deferred to materialization)
- `unfold`: None (function applied lazily; errors deferred to materialization)

### Destructors

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `head` | 1 | `S → Θ` | Any | Materialize arg to verify Seq, return head thunk (not forced) |
| `tail` | 1 | `S → Θ` | Seq or Dict | Materialize arg to verify Seq, return tail thunk (not forced) |
| `collect` | 1 | `S → D` | Dict | Materialize entire Seq spine (all tails until terminal `[]`); head thunks pass through into Dict |

**Error cases:**
- `head`, `tail`: Type mismatch if arg is not Seq
- `collect`: Type mismatch if arg is not Seq; resource limit if Seq exceeds MAX_COLLECT_SIZE (10M elements)

### Higher-Order Operations

All have **dual dispatch** on Dict/Seq. Dict paths preserve keys; Seq paths return lazy Seqs.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `map` | 2 | `L × S → LT` | Dict or Seq | Apply function to each value; Dict → Dict with PendingCall thunks, Seq → lazy Seq |
| `filter` | 2 | `L × S → LT` | Seq | Apply predicate to each value; Dict → Seq of passing entries, Seq → lazy filtered Seq |
| `take` | 2 | `S × S → LT` | Dict or Seq | Take first n entries; Dict → Dict, Seq → lazy Seq with PendingBuiltin tail |
| `drop` | 2 | `S × S → LT` | Dict or Seq | Drop first n entries; Dict → Dict, Seq → lazy Seq via PendingBuiltin step |
| `reduce` | 3 | `L × L × S → LT` | Any | Left fold: `f(f(init, x₀), x₁), ...`; Dict → lazy PendingCall chain, Seq → materializes tail at each step |
| `join` | 2 | `S × S → V` | String | Stringify all values, join with separator; materializes all elements |
| `concat` | 2 | `S × L → LT` | Dict or Seq | Concatenate two collections; Seq → lazy chain (O(1)), Dict → eager merge with reindexing |

**Error cases:**
- `map`: Type mismatch if collection is not Dict or Seq, or function is not callable
- `filter`: Type mismatch if collection is not Dict or Seq, or predicate is not callable; predicate must return Bool
- `take`, `drop`: Type mismatch if first arg is not Int or second is not Dict/Seq; negative count errors
- `reduce`: Type mismatch if collection is not Dict or Seq, or function is not callable with 2 args
- `join`: Type mismatch if collection is not Dict or Seq or separator is not String; resource limit if output exceeds MAX_STRING_SIZE (100MB)
- `concat`: Type mismatch if first arg is not Dict or Seq; second arg must match first's type

## Proxy

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `proxy` | 1 | `S → D` | Dict (Proxy) | Wrap dict in error-capturing proxy; defers errors until access (experimental) |

**Error cases:** Type mismatch if arg is not Dict.

**Proxy behavior:** When a dict key access or builtin operation fails inside a proxy, the error is captured and stored. Subsequent operations propagate the error. This enables error-tolerant pipelines.

## Stable Aliases

The following `builtin-*` aliases provide access to the raw Rust implementations, bypassing any LLT-implemented wrappers in the prelude:

| Alias | Target | Purpose |
|-------|--------|---------|
| `builtin-add` | `+` | Escape hatch for raw addition |
| `builtin-sub` | `-` | Escape hatch for raw subtraction |
| `builtin-mul` | `*` | Escape hatch for raw multiplication |
| `builtin-div` | `/` | Escape hatch for raw division |
| `builtin-eq` | `=` | Escape hatch for raw equality |
| `builtin-lt` | `<` | Escape hatch for raw less-than |
| `builtin-if` | `if` | Escape hatch for raw conditional |
| `builtin-filter` | `filter` | Escape hatch for raw filter |
| `builtin-map` | `map` | Escape hatch for raw map |
| `builtin-reduce` | `reduce` | Escape hatch for raw reduce |
| `builtin-take` | `take` | Escape hatch for raw take |
| `builtin-drop` | `drop` | Escape hatch for raw drop |

These exist to ensure that prelude-level wrappers (e.g., `>` implemented via `$<` and `$not`) cannot shadow the underlying primitives. If a wrapper has a bug or performance issue, callers can always reach the Rust implementation.

## Summary

**Total:** 51 Rust-native builtins + 12 stable aliases = 63 registered names.

**By category:**
- Arithmetic: 4
- Comparison: 2
- Control: 1
- Dict primitives: 4
- Strings: 6
- Numeric: 4 (floor, round, to-int, to-float)
- Evaluation: 5 (eval, error, try, apply, until)
- Type introspection: 1 (type-of)
- I/O: 2
- Sequences: 17 (6 constructors, 3 destructors, 7 higher-order ops, 1 predicate)
- List operations: 4 (rest, cons, reverse, sort)
- Proxy: 1

**Design principle:** These builtins are the minimal set of primitives that **cannot be expressed in LLT itself**. Everything else (sorting, logic operators, dict utilities, composition functions) is implemented in the [Standard Library](11-stdlib.md) using only these primitives plus LLT's syntax and lazy evaluation.
