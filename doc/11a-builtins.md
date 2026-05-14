# Builtin Reference

This chapter provides a complete reference for all 189 Rust-native builtins. For an overview of the stdlib boundary and higher-level LLT-implemented functions, see [Standard Library](11-stdlib.md). For strictness analysis and thunk lifecycle details, see [Evaluation](08-evaluation.md).

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

Arithmetic operators dispatch via the `Add`/`Sub`/`Mul`/`Div` MPTC classes. The result type is determined by the operand types: `Int + Int → Int`, `Float + Float → Float`, `Int + Float → Float`. User-defined numeric types participate by declaring `Add` instances. See `doc/feature/advanced-typeclasses.md §Precise Mixed-Mode Arithmetic`.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `+` | 2 | `S × S → V` | Add a b c | Add two values; result type determined by Add instance |
| `-` | 2 | `S × S → V` | Sub a b c | Subtract second from first |
| `*` | 2 | `S × S → V` | Mul a b c | Multiply two values |
| `/` | 2 | `S × S → V` | Float | Divide first by second (always returns Float) |

**Error cases:**
- All: No matching `Add`/`Sub`/`Mul`/`Div` instance for the operand types
- `/`: Division by zero (catchable via `try`)

## Comparison

Comparison operators dispatch via `Equatable` and `Comparable` typeclass instances. Primitive types (Int, Float, Str, Bool, Number) are handled by built-in Rust dispatch. User-defined types participate by declaring `Equatable`/`Comparable` instances. See `doc/feature/advanced-typeclasses.md §User-Defined Types in Primitive Operators`.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `=` | 2 | `S × S → V` | Bool | Equality — primitive types use built-in dispatch; user-defined types route through registered `Equatable` instance |
| `<` | 2 | `S × S → V` | Bool | Less-than — primitive types (Int, Float, Str) use built-in dispatch; user-defined types route through registered `Comparable` instance |

**Error cases:**
- `=`: No registered `Equatable` instance for a non-primitive type
- `<`: No registered `Comparable` instance, or mismatched types

## Control Flow

| Builtin | Arity | Signature | Category | Description |
|---------|-------|-----------|----------|-------------|
| `if` | 3 | `S × Sc × Sc → Θ` | Selective | Materializes condition; returns chosen branch thunk without materializing it |

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

## Dict Access (Seq-Producing)

Convert a Dict to a lazy Seq of its contents. All three builtins use an internal offset parameter to avoid O(n²) IndexMap rebuilds — each recursive step increments the offset rather than rebuilding the remaining dict.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `builtin-get` | 2 | `S × S → Θ` | Any | Look up key (Int or String) in dict; returns value thunk or errors if key absent |
| `each` | 1 | `S → LT` | Seq | Convert dict to lazy Seq of its values in insertion order; keys are discarded |
| `each-key` | 1 | `S → LT` | Seq | Convert dict to lazy Seq of its keys in insertion order; values are discarded |
| `each-kv` | 1 | `S → LT` | Seq | Convert dict to lazy Seq of `[key: K  value: V]` dicts in insertion order |

**`builtin-get` note:** This is a primitive for runtime key lookup by computed key value. Use `data.key` for static string-key dot access; `builtin-get` is for cases where the key itself is a runtime value (e.g., the result of `each-key`).

**Error cases:**
- `builtin-get`: Type mismatch if first arg is not Int or String; key-not-found error if key is absent from dict
- `each`, `each-key`, `each-kv`: Type mismatch if arg is not Dict

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
| `eval` | 1 | `S → V` | Any | Deep materialization — recursively materializes all thunks in the value tree |
| `error` | 1 | `S → ⊥` | Never returns | Materializes arg as error message, raises catchable error |
| `try` | 1 | `S → D` | Variant | Materializes function arg, invokes it with no args, catches errors; returns `[Ok result]` or `[Err message]` (ADT variants, destructured with `match`) |
| `apply` | 2 | `S × S → Θ` | Any | Materialize function and dict, call function with dict as named args |
| `force` | 1 | `S → V` | Any | Force WHNF (weak head normal form) evaluation: materializes the thunk but does not recursively materialize nested thunks |
| `eval-ast` | 1 | `S → V` | Any | Evaluate an AST dict (as produced by `[quote ...]`); converts via `dict_to_ast` and evaluates in the current environment |

**Error cases:**
- `eval`: Propagates any error from deep materialization
- `error`: Always raises (by design)
- `try`: Type mismatch if arg is not a function (zero-arity)
- `apply`: Type mismatch if first arg is not a function or second is not a dict
- `force`: Propagates any error from materialization
- `eval-ast`: Type mismatch if arg is not a Dict conforming to the AST schema; conversion error if the dict structure is invalid

## Type Introspection

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `type-of` | 1 | `S → V` | String | Return type name: "Int", "Float", "String", "Bool", "Dict", "Seq", "Function", "Proxy" |
| `int?` | 1 | `S → V` | Bool | Return true if arg is an Int |
| `float?` | 1 | `S → V` | Bool | Return true if arg is a Float |
| `num?` | 1 | `S → V` | Bool | Return true if arg is an Int or Float |
| `str?` | 1 | `S → V` | Bool | Return true if arg is a String |
| `bool?` | 1 | `S → V` | Bool | Return true if arg is a Bool |
| `null?` | 1 | `S → V` | Bool | Return true if arg is Null (empty dict `[]` — tinct's null representation) |
| `dict?` | 1 | `S → V` | Bool | Return true if arg is a Dict (includes lists, which are dicts with integer keys) |
| `fn?` | 1 | `S → V` | Bool | Return true if arg is callable (Function or Builtin) |
| `seq?` | 1 | `S → V` | Bool | Return true if arg is a Seq |
| `record?` | 1 | `S → V` | Bool | Return true if arg is a Dict/Overlay (runtime has no key-type tracking; type-level distinction only) |
| `map?` | 1 | `S → V` | Bool | Return true if arg is a Dict/Overlay (runtime has no key-type tracking; type-level distinction only) |
| `bytes?` | 1 | `S → V` | Bool | Return true if arg is a Bytes value |

Each predicate materializes its argument and checks the `Value` variant. `num?` checks both `Int` and `Float`, mirroring the `Number` supertype. `fn?` checks both `Function` and `Builtin`, since both are callable. `record?` and `map?` both return true for any `Dict` or `Overlay` value — the key-type distinction (string keys vs mixed keys) exists only at the type level. The runtime does not track key types, so both predicates behave identically and accept all dicts. No `list?` **builtin** exists because lists are dicts (Principle 1: Dicts Are Fundamental) — "list-ness" is a convention, not a type distinction — `list?` is available as a standard library function (see [Standard Library](11-stdlib.md) §Type Predicates).

**Error cases:** None.

## Meta & Code Generation

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `gensym` | 0-1 | `() or S → V` | String | Generate a unique symbol string; optional prefix arg for debugging (e.g., `[gensym "tmp"]` → `"tmp_42"`) |
| `llt-repr` | 1 | `S → V` | String | Convert value to LLT source code representation (inverse of parsing; useful for code generation) |
| `ast-of` | 1 | `S → V` | Dict (Unknown) | Return the AST dict for any value — `[type: "fn" return-ann: ... params: [...] body: ...]` for functions, `[type: "builtin" name: ...]` for builtins, `[type: type-of(val)]` for others. See `doc/feature/runtime-reflection.md`. |
| `eval-ast` | 1 | `S → V` | Any | Evaluate an AST dict in the stdlib environment and return the resulting value. |
| `str` | variadic | `S... → V` | String | Stringify and concatenate all arguments. Routes through registered `Showable` instance for user-defined types; built-in Rust dispatch for primitives. |

**Error cases:**
- `gensym`: None
- `llt-repr`: None (all values have a repr)
- `ast-of`: None (all values return a dict)
- `eval-ast`: Dict does not conform to AST schema; free variables not in stdlib

## Variant Construction

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `variant` | 2 | `S × S → V` | Variant | Construct a variant value: `[variant "Ok" value]` → `Value::Variant { tag: "Ok", payload: Some(value) }` |
| `tag-of` | 1 | `S → V` | String | Extract tag from variant: `[tag-of [Ok 42]]` → `"Ok"` |

**Error cases:**
- `variant`: Type mismatch if tag is not String
- `tag-of`: Type mismatch if arg is not a Variant

## Numeric Type Conversion

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `float` | 1 | `S → V` | Float | Convert Int to Float; pass-through for Float; type error otherwise |
| `decimal` | 1 | `S → V` | Decimal | Convert String or Int to Decimal (extended numeric type for exact decimal arithmetic) |
| `big-int` | 1 | `S → V` | BigInt | Convert String or Int to BigInt (arbitrary-precision integer) |

**Error cases:**
- `float`: Type mismatch if arg is not Int or Float
- `decimal`: Type mismatch if arg is not String or Int; parse error for malformed string
- `big-int`: Type mismatch if arg is not String or Int; parse error for malformed string

## Dict Access

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `get?` | 2 | `S × S → V` | Bool or Value | Optional key lookup: returns `[Ok value]` if key exists, `[Err "key not found"]` otherwise (Result variant) |

**Error cases:**
- Type mismatch if first arg is not Dict or second arg is not a valid key type (Int or String)

## Datagram I/O

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `send-datagram` | 2 | `S × S → V` | Int | Send bytes to a datagram Handle; returns number of bytes sent |
| `recv-datagram` | 1-2 | `S (× S)? → V` | String | Receive bytes from datagram Handle; optional max-size arg (default 65536) |

**Error cases:**
- `send-datagram`: Type mismatch if first arg is not Handle or second arg is not Bytes/String; capability error if Handle does not carry `Datagram` capability; I/O error on send failure
- `recv-datagram`: Type mismatch if arg is not Handle; capability error if Handle does not carry `Datagram` capability; I/O error on receive failure

## Schema Validation

Runtime structural validation with constraint checking. See [Structural Contracts](../whatif/structural-contracts.md) for the full design.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `validate` | 2 | `S × S → V` | Any | Validate data against schema; returns data unchanged on success, throws SchemaViolation on failure |

**Schema keys:**

- `type`: Expected type name (String: `"Int"`, `"String"`, `"Bool"`, `"Dict"`, `"Seq"`, etc.)
- `min`, `max`: Numeric range constraints (Int or Float)
- `min-length`, `max-length`: String or collection length constraints (Int)
- `pattern`: Regex pattern for strings (String)
- `required`: Whether field is required (Bool; default: false)
- `default`: Default value if field is missing (Any)
- `items`: Schema for sequence/dict elements (Dict)
- `fields`: Schema for dict fields (Dict mapping field names to field schemas)
- `enum`: List of allowed values (Seq)

**Behavior:**

`validate` walks the schema dict and data value in parallel, collecting ALL constraint violations (not fail-fast). On success, it returns the data value unchanged (pass-through for pipeline use). On failure, it throws a `SchemaViolation` error with all violations listed as `(field_path, error_message)` pairs.

Field paths use dot notation (e.g., `"user.address.zip"`). **Limitation:** field paths are ambiguous for keys containing `.` — this is a documented trade-off for simplicity.

**Example:**

```tinct
nginx-schema: [
  fields: [
    port: [
      type: "Int"
      min: 1
      max: 65535
    ]
    hostname: [
      type: "String"
      pattern: "^[a-z0-9.-]+$"
    ]
  ]
]

config: [
  port: 8080
  hostname: "example.com"
]

[validate $nginx-schema $config]
# Returns config unchanged on success
# Throws SchemaViolation with all violations on failure
```

**Error cases:**

- Type mismatch if schema is not Dict
- SchemaViolation if data violates one or more constraints (error lists all violations with field paths)
- Invalid regex pattern in `pattern` constraint (reported as a violation)

## I/O

File loading, JSON parsing, and text output.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `from-json` | 1 | `S → D` | Dict | Parse JSON string to dict; numbers become Int or Float, arrays become dicts with 0-indexed keys |
| `include` | 1-3 | `S (× S)? → D` or `S × S (× S)? → D` | Dict | Load and evaluate an LLT file; returns the file's final value |
| `emit` | 1 | `S → Null` | Null | Write string to stdout; suppresses default JSON output; returns empty dict (Null) |
| `write` | 3 | `DirCap × S × S → Null` | Null | Write content to file; takes DirCap, path (String), content (String); returns empty dict (Null) |
| `write-atomic` | 3 | `DirCap × S × S → Null` | Null | Atomically write content to file via temp+rename; takes DirCap, path, content; returns empty dict (Null) |
| `revoke-cap` | 1 | `RevocableDirCap → Null` | Null | Revoke a RevocableDirCap; subsequent uses will error; returns empty dict (Null) |

**`include` call patterns:**

1. **`[include "path"]`** — Backward compatible: load file from current working directory (via `ctx.config.base_dir`). Path is relative to the directory containing the evaluating file.

2. **`[include "path" "hash"]`** — Backward compatible with integrity check: same as (1) but with a required integrity hash in `"algo:hexdigest"` format (e.g., `"blake3:abc123..."`).

3. **`[include $cap "path"]`** — Cap-qualified: load file from the given `DirCap`. Path is relative to the cap's root directory. The cap can be a user-provided capability (e.g., `pwd`, `libdir`) or an attenuated cap created via `narrow`.

4. **`[include $cap "path" "hash"]`** — Cap-qualified with integrity check: same as (3) but with a required integrity hash.

**Caching:** Files are cached by `(st_dev, st_ino)` inode identity on Unix systems (by path hash on non-Unix). The same physical file accessed via different caps or paths is evaluated only once.

**`emit` behavior:**

`emit` writes UTF-8 text directly to stdout, bypassing the default JSON serialization. When `emit` is called during evaluation, the CLI suppresses the automatic JSON output at the end. Multiple `emit` calls append sequentially. This enables text-based formatters and templating workflows (see [Documents & Pipelines](09-documents.md) §Multi-File Pipeline).

**Error cases:**
- `from-json`: Type mismatch if arg is not String; parse error if JSON is invalid
- `include`: Type mismatch if first arg is not DirCap or String; arity mismatch if DirCap is provided but path is missing; file not found; parse/eval errors from included file; revoked capability error if using a revoked `RevocableDirCap`
- `emit`: Type mismatch if arg is not String; I/O error if stdout write fails
- `write`: Type mismatch if first arg is not DirCap, or path/content are not String; I/O error on file creation or write failure; revoked capability error if using a revoked `RevocableDirCap`
- `write-atomic`: Type mismatch if first arg is not DirCap, or path/content are not String; I/O error on temp file creation, write, sync, or rename failure; revoked capability error if using a revoked `RevocableDirCap`
- `revoke-cap`: Type mismatch if arg is not RevocableDirCap

## Sequences

Sequence constructors create lazy Seq values; destructors materialize the Seq spine to varying degrees; higher-order operations apply functions lazily.

### Constructors

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `seq` | 2 | `L × L → D` | Seq | Construct Seq from head and tail thunks (both pass through; coinductive guard) |
| `range` | 1-2 | `S (× S)? → LT` | Seq | Integer range: 1-arg form creates infinite Seq starting from arg (`[call $range 5]` → infinite Seq: `5, 6, 7, ...`); 2-arg form creates finite range (`[call $range 2 5]` → `2, 3, 4`, end exclusive) |
| `repeat` | 1 | `L → LT` | Seq | Infinite repetition of a value (arg passes through as thunk) |
| `cycle` | 1 | `S → LT` | Seq | Infinite repetition of a dict's values (materializes dict, constructs PendingBuiltin step) |
| `iterate` | 2 | `L × L → LT` | Seq | Infinite sequence: `x, f(x), f(f(x)), ...` (both args pass through; co-recursive PendingCall + PendingBuiltin) |
| `unfold` | 2 | `L × L → Θ` | Seq | General unfold: `f(state) → dict`; step dict must have value as **first** entry and next state as **second** entry (insertion order matters; key names are ignored); returns PendingBuiltin thunk |

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
| `head` | 1 | `S → Θ` | Any | Materialize arg to verify Seq, return head thunk (not materialized) |
| `tail` | 1 | `S → Θ` | Seq or Dict | Materialize arg to verify Seq, return tail thunk (not materialized) |
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
| `proxy` | 1 | `Fn → Proxy` | Value::Proxy | Takes a handler function; returns `Value::Proxy`. Any field access `.field` calls `handler(field-name)`. Enables virtual namespaces, mock objects, and computed fields. |

**Error cases:** Type mismatch if arg is not a function.

**Proxy behavior:** A Proxy wraps a handler function. When a field is accessed via dot syntax (e.g., `p.name`), the handler is called with the field name as its argument and the result is returned. This is a field-intercept pattern — the proxy does not contain actual dict entries.

## Network

Network builtins create and operate on `Value::Handle`, `Value::HttpConn`, and URI value types. For the Handle capability row model, see [Data Model](03-data-model.md) §Handles.

All network operations materialize their non-Handle arguments. Handle arguments are passed by reference — they carry the connection state and are not materialized as thunks.

**Connector security policy:** User-defined Connectors are pure-tinct functions that cannot call I/O builtins directly. All network I/O flows through `connect` which enforces the NetCap allowlist. Custom Connectors (WireGuard clients, test fakes, protocol layers) receive allowlist-validated connections from `connect` and may transform them, but cannot bypass the allowlist to create new OS-level network connections.

### Transport — connect

Opens a transport-layer connection via a Connector and returns a `Handle`.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `connect` | 3–4 | `S × S × S (× S)? → Handle` | Handle | Transport-generic dispatch: `connector transport host [port]` |

**Transport variants:** `Tcp | Udp | UnixStream | UnixDatagram | NamedPipe | Icmp`

Each transport variant determines the connection semantics and argument requirements:

```tinct
# TCP/UDP: require host (String) and port (Int)
[connect net Tcp "api.example.com" 443]   # → Handle{ Binary Readable Writable Stream }
[connect net Udp "8.8.8.8" 53]            # → Handle{ Binary Readable Writable Datagram }

# Unix sockets: require DirCap (not NetCap), path (String), no port
[connect dir UnixStream "app.sock"]       # → Handle{ Binary Readable Writable Stream }

# ICMP: require host (String), no port
[connect net Icmp "8.8.8.8"]              # → Handle{ Binary Readable Datagram }

# Tcp is the default when Transport is omitted:
[connect net "api.example.com" 443]       # same as Tcp form
```

The first argument is any Connector — a capability value that authorizes connections. `NetCap` (injected via `--cap-net`) gates TCP/UDP/ICMP connections to allowlist hosts. `DirCap` (injected via `--cap-dir`) gates Unix socket connections relative to the directory. User-defined Connectors implement custom routing or tunneling.

**Capability requirements by transport:**

| Transport | Connector Type | Arguments | Handle Capabilities |
|-----------|----------------|-----------|---------------------|
| `Tcp` | `NetCap` or custom | `host port` | `Binary Readable Writable Stream` |
| `Udp` | `NetCap` or custom | `host port` | `Binary Readable Writable Datagram` |
| `UnixStream` | `DirCap` | `path` | `Binary Readable Writable Stream` |
| `UnixDatagram` | `DirCap` | `path` | `Binary Readable Writable Datagram` |
| `NamedPipe` | `DirCap` (Windows) | `path` | `Binary Readable Writable Stream` |
| `Icmp` | `NetCap` or custom | `host` | `Binary Readable Datagram` |

**Platform support:** `Tcp` is supported on all platforms. `UnixStream` is supported on Linux (uses `/proc/self/fd` for path resolution). `Udp`, `UnixDatagram`, `NamedPipe`, and `Icmp` require datagram infrastructure and platform-specific socket support; they raise a runtime error if invoked without the requisite transport infrastructure.

**Error cases:** Type mismatch if arguments don't match the transport's requirements; connection refused or timeout at the OS level; Connector rejects the connection (allowlist violation for `NetCap`, path escape for `DirCap`); unsupported transport variant.

### TLS — tls-layer

Establishes a TLS 1.3 session and returns a `Handle` with the `Tls` capability.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `tls-layer` | 3 | `S × S × S → Handle` | Handle | Layer TLS: `handle sni opts` → Handle with Tls capability |

**Usage:**

```tinct
[tcp: [connect net Tcp "10.0.0.5" 443]]
[tls: [tls-layer tcp "api.example.com" []]]
# → Handle{ Binary Readable Writable Stream Tls }
```

`tls-layer` is the general Layer pattern for composing Handle transformations. It consumes the underlying Handle's raw TCP stream (via `raw_tcp: Option<TcpStream>`) and returns a new Handle wrapping the TLS session. After `tls-layer` extracts the TCP stream, the original Handle is invalidated — subsequent operations on it produce a runtime error.

The `sni` argument is the Server Name Indication hostname for the TLS handshake. It may differ from the IP connected to (e.g., when connecting to a specific IP but validating a certificate for a domain).

**Default trust:** System CA roots via `rustls-native-certs` (Linux: `/etc/ssl/certs`; macOS: Keychain; Windows: Certificate Store). Override via the `opts` dict.

**Options dict (`opts`):**

| Key | Type | Default | Description |
|-----|------|---------|-------------|
| `ca-bundle` | `Handle[Text Readable …]` | — | PEM file via `[open cap path Readable]`; added to system roots |
| `no-system-roots` | `Bool` | `false` | Drop system roots; trust only `ca-bundle` (fully private PKI) |
| `mozilla-roots` | `Bool` | `false` | Also load compiled-in Mozilla roots (`webpki-roots`) |
| `client-cert` | `Handle[Text Readable …]` | — | PEM client certificate for mutual TLS |
| `client-key` | `Handle[Text Readable …]` | — | PEM private key for the client certificate |
| `pins` | `Seq[SpkiPin]` | — | SPKI fingerprints; leaf cert must match one (see §SPKI Pinning) |
| `alpn` | `Seq[String]` | `["http/1.1"]` | ALPN protocol list for negotiation |

All three trust sources (`ca-bundle`, system roots, Mozilla roots) union when combined. Set `no-system-roots: true` to trust only `ca-bundle` (required for fully private PKI where public CAs must be excluded).

**Mutual TLS example:**

```tinct
[cert: [open fs "certs/client.pem" Readable]]
[key:  [open fs "certs/client-key.pem" Readable]]
[h: [tls-layer tcp "api.internal" [client-cert: cert  client-key: key]]]
```

**Error cases:** Type mismatch if handle is not a Handle or sni is not String; capability error if the Handle does not carry the `Stream` capability or the underlying TCP stream has already been consumed; TLS handshake failure (certificate verification, expired cert, hostname mismatch); SPKI pin mismatch if `pins` is specified and the leaf cert matches none.

### SPKI Pinning

SPKI (Subject Public Key Info) hash pinning locks a `tls-layer` call to a specific public key, defending against CA compromise. Pinning survives certificate rotation as long as the key is reused.

A `SpkiPin` value carries the hash algorithm and raw fingerprint bytes:

```tinct
[spki-pin Sha3-256 [hex-decode "aabbcc..."]]   # SHA3-256 (preferred)
[spki-pin Sha256   [base64-decode "AAAA...="]] # SHA-256 (compatibility)
```

`SpkiPin` is constructed via the `spki-pin` stdlib function (two positional args: `HashAlgorithm` variant and `Bytes`). SHA-3 (Keccak construction) is preferred for new deployments; SHA-256 is accepted for compatibility with existing tooling.

Maintain both current and next-rotation pins to allow key rotation without a service outage — `tls-layer` succeeds if the leaf SPKI matches any pin in the list using that pin's algorithm.

### TLS Introspection — tls-peer-cert

Reads the peer certificate from a TLS Handle.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `tls-peer-cert` | 1 | `S → D` | Dict | Return peer certificate fields; requires Handle with `Tls` capability |

The argument must be a `Handle` carrying the `Tls` capability (i.e., produced by `tls-connect`). The `Tls` capability in the Handle's cap row stores this information at handshake time; `tls-peer-cert` extracts it without making any additional network calls.

The returned dict has these fields:

| Field | Type | Description |
|-------|------|-------------|
| `subject` | `String` | Distinguished name, e.g. `"CN=api.internal,O=Internal Corp"` |
| `issuer` | `String` | Distinguished name of the signing CA |
| `sans` | `Dict` (list of `String`) | Subject Alternative Names |
| `not-before` | `Timestamp` | Certificate validity start (lib-datetime Timestamp) |
| `not-after` | `Timestamp` | Certificate validity end; compare with `[now clock]` for expiry checks |
| `spki-sha256` | `String` | `sha256//base64=` format SPKI fingerprint |

```tinct
[h:    [tls-connect net "api.internal" 443]]
[cert: [tls-peer-cert h]]
[days-left: [days-between [parse-timestamp cert.not-after] [now clock]]]
[if [< days-left 30]
  [emit [str "WARNING: cert expires in " days-left " days"]]
  null]
```

**Error cases:** Type mismatch if arg is not a Handle; capability error if the Handle does not carry the `Tls` capability (calling `tls-peer-cert` on a plain TCP Handle is a static type error and a runtime capability error).

### Handle Capability Access — cap-data, has-cap?

Read capability data from the Handle's capability row.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `cap-data` | 2 | `S × S → V` | Any | Return the `Value` stored for capability `name` in Handle `h` |
| `has-cap?` | 2 | `S × S → V` | Bool | Return true if Handle `h` carries capability `name` |

```tinct
[has-cap? h "Tls"]        # → true if h was created by tls-connect
[cap-data h "Tls"]        # → dict with cert fields (same as tls-peer-cert)
[has-cap? h "Readable"]   # → true for all read-capable Handles
```

`cap-data` errors if the named capability is absent. Use `has-cap?` to test first. Boolean capabilities (Readable, Writable, Stream, Datagram, Seekable, Binary, Text) store `Value::Null` as their data; `cap-data` on these returns `null`.

**Error cases:** Type mismatch if first arg is not Handle or second arg is not String; key-not-found error from `cap-data` if capability is absent.

### HTTP Requests — http-get, fetch

Single-shot HTTP requests. `http-get` is implemented in pure-tinct (`stdlib/net.llt`) over a `Handle[Binary Readable Writable]`; it handles both `http://` and `https://` by dispatching on `url.scheme`. `https-get` does not exist as a separate function.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `http-get` | 2–4 | `S × S (× S)? (× S)? → D` | Dict | HTTP GET request; dispatches on url.scheme for http/https |
| `fetch` | 2 | `S × S → D` | Dict | Convenience wrapper: `http-get connector url [] null` |

**Signatures:**

```
http-get : [fn@Dict [connector@Connector  url@Url  headers@Dict  tls-opts@[TlsOpts Null]]]
fetch    : [fn@Dict [connector@Connector  url@Url]]
```

`http-get` accepts a Connector and opens a fresh connection per call.

The returned dict:

| Field | Type | Description |
|-------|------|-------------|
| `status` | `Int` | HTTP status code, e.g. `200`, `404` |
| `headers` | `Dict` | Response headers, lowercase keys |
| `body` | `String` | Response body as UTF-8 string |

```tinct
[resp: [fetch net [url "https://api.example.com/config"]]]
resp.status   # → 200
resp.body     # → "{...}"
```

**Error cases:** Type mismatch if url is not Url; unsupported scheme (only `"http"` and `"https"` are handled); connection or TLS errors; non-UTF-8 response body.

### URI Builtins — uri, url, urn

Parse URI strings into structured values with dot-accessible fields.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `uri` | 1 | `S → Uri` | Uri | Parse any RFC 3986 URI string → `Value::Uri` |
| `url` | 1 | `S → Url` | Url | Parse hierarchical URL → `Value::Url`; errors if no authority |
| `urn` | 1 | `S → Urn` | Urn | Parse URN → `Value::Urn`; errors if not `urn:` scheme |

For field descriptions, see [Data Model](03-data-model.md) §URI Values.

**Error cases:**
- `uri`: Parse error if string is not a valid RFC 3986 URI
- `url`: Parse error if not a valid URI; type error if no authority (host) component is present
- `urn`: Parse error if not a valid URI; type error if scheme is not `"urn"`

### ICMP Echo — icmp-ping

Send an ICMP echo request (ping) to a host and return the round-trip time.

| Builtin | Arity | Signature | Result | Description |
|---------|-------|-----------|--------|-------------|
| `icmp-ping` | 3 | `S × S × S → D` | Dict | `cap host timeout-ms` — sends ICMP echo request, returns `{ok: {latency-ms: Int}}` or `{err: String}` |

**Signature:**

```
icmp-ping : [fn [cap@NetCap  host@String  timeout-ms@Int]]
            → {ok: {latency-ms: Int}} | {err: String}
```

**Usage:**

```tinct
[r: [icmp-ping net "8.8.8.8" 1000]]
[if [= [$r.ok?] true]
  [str "RTT: " r.ok.latency-ms "ms"]
  [str "failed: " r.err]]
```

`timeout-ms` is the maximum wait time in milliseconds. A value of `0` disables the timeout (not recommended). The host may be an IPv4 address string or a hostname — DNS resolution is performed before sending.

**Returned dict:** Always returns a dict (never throws on network failure):
- Success: `{ok: {latency-ms: Int}}` — latency in whole milliseconds
- Failure: `{err: String}` — human-readable error message

Failure cases that produce `{err: ...}`:
- DNS resolution failure
- Timeout
- ICMP socket creation failure (see privilege requirements below)
- `sendto` or `recv` syscall failure
- Unexpected ICMP reply type (not Echo Reply type 0)

**Privilege requirements (Linux):** Uses `SOCK_DGRAM + IPPROTO_ICMP` (unprivileged ICMP ping sockets, available since Linux 3.11+). Root is **not** required, but the kernel must allow your group to create ICMP sockets. Check and configure via:

```sh
# Check current setting (default: 1 65534, includes all non-root users)
cat /proc/sys/net/ipv4/ping_group_range

# Allow all users (if restricted):
sysctl -w net.ipv4.ping_group_range="0 65534"
```

If socket creation fails, the error dict includes a message explaining the `ping_group_range` requirement.

**Platform support:** Linux only. On non-Unix platforms (Windows, etc.), `icmp-ping` always returns `{err: "icmp-ping: ICMP ping is not supported on this platform"}`.

**NetCap allowlist:** The host is checked against the `NetCap` allowlist before any socket operations. Port-based allowlist entries (`hostname:port`) do not match (ICMP has no ports); hostname and CIDR entries apply normally.

**Error cases (hard errors, not `{err: ...}` dict):**
- Type mismatch if `cap` is not a `NetCap` (E010)
- Type mismatch if `host` is not a String (E010)
- Type mismatch if `timeout-ms` is not an Int (E010)
- Negative `timeout-ms` (E080)
- `cap` allowlist violation before socket creation (E080)

### Implementation Note: Tokio Runtime Strategy for Future Async Builtins

When `quic-session` or other async network builtins (`http2-session`, `http3-session`) are implemented, the Tokio runtime strategy must be carefully managed to avoid the "cannot start a runtime from within a runtime" panic.

**Rule: one runtime per builtin call, never nested.**

Each async builtin (e.g., `quic-session`, a future `http3-connect`) must create and block on its own scoped runtime:

```rust
// In builtin_quic_session():
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()
    .build()
    .map_err(|e| EvalError::user_error(format!("quic-session: runtime error: {}", e), span))?;
let result = rt.block_on(async { /* quinn QUIC handshake */ });
```

**Why `current_thread` and not `Runtime::new()`:** LLT builtins are called from synchronous Rust. A `current_thread` runtime blocks the calling thread for the duration of one async operation, then is dropped. It does not spawn background threads, making it safe to create and destroy per-call.

**Shared runtime problem:** If `reqwest` (HTTP/1.1 and HTTP/2) and `quinn` (QUIC / HTTP/3) are both used in the same process, each may attempt to create a Tokio runtime. The two runtimes must not nest. The solution is to use the same `current_thread` pattern for both and ensure they are never called recursively (they cannot be, since LLT is single-threaded and builtins return before the next builtin is called).

**Not a concern for `tls-connect` / `connect`:** These use blocking `std::net::TcpStream` and `rustls::StreamOwned` synchronously — no Tokio required.

**Extension:** If a session builtin needs to hold a live async connection across multiple builtin calls (e.g., streaming gRPC), the runtime must outlive the builtin call. In that case, store the runtime alongside the handle in the `Value::Handle` `caps` dict as an opaque Rust-managed resource.

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

**Total:** 189 Rust-native builtins + 27 stable aliases = 216 registered names.

Builtins are organized by functionality but counted individually. See `standard_builtins()` in `src/builtins.rs` for the authoritative list. Key categories include arithmetic, comparison, control flow, dict primitives, sequences, strings, I/O, networking, type introspection, and meta/code generation. The 27 stable aliases (prefixed `builtin-*`) provide access to raw Rust implementations bypassing LLT prelude wrappers.

**Design principle:** These builtins are the minimal set of primitives that **cannot be expressed in LLT itself**. Everything else (sorting, logic operators, dict utilities, composition functions) is implemented in the [Standard Library](11-stdlib.md) using only these primitives plus LLT's syntax and lazy evaluation.
