# What If: Text Redesign — Unicode Grapheme Cluster Model

**State:** Proposal

What would it take to replace tinct's opaque UTF-8 `String` type with first-class Unicode types that are honest about their structure?

## Goals

1. **Retire `String`.** Replace it with `Graphemes = [List Grapheme]` — a collected list of Unicode grapheme clusters providing O(1) indexed access at whole-grapheme boundaries. No `Value::String` in the runtime, no `Type::Str` in the type checker, no `Key::String` in the dict key enum.

4. **Generic dict key constraint.** Any type satisfying `Hashable` can be a dict key — not just hardcoded `String` or `Int`. `[Map k v]` uniformly requires `k: Hashable`, enforced through the typeclass system.

Goals 2, 3, and 5 (typeclass hierarchy / multiple dispatch; Dict/Map/List type hierarchy; remove prelude special cases) have moved to `doc/whatif/type-foundations.md`.

Non-string-specific architectural changes (collection hierarchy, typeclass dispatch, bootstrap de-special-casing) are in `doc/whatif/type-foundations.md`.

## Current State

The runtime stores `String` as `Value::String(Arc<str>)` — an opaque, immutable UTF-8 byte slice. Builtins that accept strings operate on raw byte offsets internally. The type hierarchy presents `String` as a primitive opaque type.

```tinct
# Current: string is a black box — builtins hide the byte model
[str-length "café"]      # → 4? 5? depends whether you count bytes or code points
[str-slice "café" 0 3]   # → "caf"? "ca"? byte boundary is not a grapheme boundary
[get "café" 2]           # type error or byte offset? — not what users expect
```

### What's Missing

1. **No grapheme-level access.** There is no way to iterate, count, or index by user-perceived character. "User character" is a grapheme cluster (UAX#29), not a byte, not a Unicode code point, not a Rust `char`.

2. **`str-length` lies.** It returns byte length, not grapheme count. `"café"` (5 bytes with precomposed é) returns 5; with decomposed `é` (e + combining accent) it returns 6. Neither matches what the user perceives as "4 characters."

3. **`[get s n]` is unsound.** A string is not O(1) random-accessable — UTF-8 requires scanning from the start. The current implementation hides this. The type system should make the cost visible.

4. **No I/O boundary type.** Raw bytes from the network or filesystem are strings internally. The distinction between "text" and "bytes" is absent.

5. **No explicit decode step.** External data enters as string silently, with no codec tracking which encoding was assumed.

## Why This Redesign Matters

**Correctness first.** Text IS a sequence of grapheme clusters. Defining it that way makes the type honest: operations on text are operations on sequences, with the same costs they have for any sequence. Hidden `O(n)` random access behind a misleading `O(1)` interface is a correctness problem, not a performance problem.

**`Graphemes = [List Grapheme]` is honest.** Text is a collected sequence of grapheme clusters. `[get s 4]` is O(1). `[head s]` is O(1). `[length s]` is O(1). The type system says what the operations cost. Text builtins that worked on raw bytes or Rust `char`s are replaced by generic `[Map Int a]` operations — length, get, slice, map, filter — which apply uniformly.

**Bytes is the I/O boundary.** Network reads and file reads return `Bytes`. Text always requires an explicit codec. Making this explicit prevents the silent-UTF-8-assumption bug that plagues every language with an implicit text type.

**`Graphemes` is `Indexed`.** `[get "hello" 2]` is O(1) without any collect step — the text literal already IS a collected `[Map Int Grapheme]`. The `Indexed` contract (O(1)) is satisfied directly.

## Design

### The Type Equations

```tinct
Grapheme:       [type [Cluster code-points: [List Int]]]   # nominal variant; grapheme? validator in text.llt
Graphemes:      [type [List Grapheme]]      # = [Map Int Grapheme]
GraphemeStream: [type [Seq Grapheme]]
Bytes:          [type [List UInt8]]         # = [Map Int UInt8]
ByteStream:     [type [Seq UInt8]]
```

These five declarations are the entire model. Everything else follows from sequence semantics.

`Grapheme` is declared in `stdlib/prelude.llt`. `Graphemes` and `GraphemeStream` are declared in prelude's public dict. `String` is removed entirely — the name no longer exists in the type namespace; `@String` annotations are a type error after this change. The four types (excluding `Grapheme` itself) come in two symmetric pairs:

| Type | Structure | Random access | Runtime |
|------|-----------|--------------|---------|
| `Graphemes = [Map Int Grapheme]` | collected | O(1) via Indexed | `Value::Dict` (integer keys) |
| `GraphemeStream = [Seq Grapheme]` | lazy stream | type error | `Value::Variant` (Seq.Cons/Seq.End) |
| `Bytes = [Map Int UInt8]` | collected | O(1) via Indexed | `Value::Dict` (integer keys) |
| `ByteStream = [Seq UInt8]` | lazy stream | type error | `Value::Variant` (Seq.Cons/Seq.End) |

`Graphemes` and `Bytes` are not primitive runtime types — they are type-level aliases for `[Map Int a]`. At the runtime level, both are `Value::Dict` with integer keys and typed values.

Text literals `"..."` produce `Graphemes` — eagerly decoded at eval time to `Value::Dict`. `[get "hello" 2]` is O(1) (dict lookup). `[decode Utf8Graphemes bytes]` produces a `GraphemeStream` (lazy `Value::Variant` chain of Seq.Cons/Seq.End); call `collect` to get `Graphemes`.

### Grapheme

A `Grapheme` is a nominal variant holding a non-empty collected list of Unicode scalar values forming exactly one UAX#29 grapheme cluster. It is declared in **prelude.llt**:

```tinct
# From stdlib/prelude.llt
Grapheme: [type [Cluster code-points: [List Int]]]
```

`Grapheme.Cluster` is the qualified constructor — no scope injection. Codecs construct graphemes as `[Grapheme.Cluster code-points: cps]`. Access code points via `[match g [Grapheme.Cluster p]: p.code-points]` or the convenience function `grapheme-codepoints`.

`grapheme?` lives in **stdlib/text.llt** — NOT attached to the `Grapheme` type in prelude. It validates a raw `[List Int]` before construction:

```tinct
# grapheme? lives in stdlib/text.llt — standalone validator on raw code points.
# Codec-produced Graphemes are valid by construction (UAX#29 segmentation guarantees it).
# Import text.llt to use it.
grapheme?: [fn@Boolean [let cps@[List Int]]
  [n: [length cps]]
  [if [= n 0] False
    [block
      [step: [fn [let i prev-prop state]
        [if [>= i n]
          True
          [block
            [cp:   [nth i cps]]
            [prop: [gcb-property cp]]
            [if [gcb-boundary? prev-prop prop state]
              False
              [step [+ i 1] prop [gcb-next-state prop state]]]]]]]
      [step 1 [gcb-property [nth 0 cps]] [gcb-next-state [gcb-property [nth 0 cps]] gcb-initial-state]]]]]
```

`grapheme?` calls `gcb-property` and `gcb-boundary?`, which are builtins backed by the UAX#29 grapheme break table. The validation is encoding-agnostic — the same rules apply regardless of whether the text was originally UTF-8, UTF-16, or decoded from bytes by any codec.

`Grapheme` values are produced by any process that correctly segments text into clusters using `[Grapheme.Cluster code-points: cps]`:

- **Text literals** — source UTF-8 decoded to code points and segmented at eval time
- **Unicode codecs** — `Utf8Graphemes`, future `Utf16Graphemes`, `Utf32Graphemes`; segmentation is handled inside the codec using UAX#29
- **Single-byte encodings** (Latin-1, Windows-1252) — each byte maps to one Unicode code point; all are non-combining → single-codepoint cluster → `[Grapheme.Cluster code-points: [cp]]`
- **DBCS/TBCS encodings** (Shift-JIS, GB 2312, Big5) — the encoding's byte structure defines character boundaries (1 or 2 bytes per character depending on lead byte); the codec produces complete Graphemes directly using its own segmentation rules, without requiring UAX#29
- **User code** constructing `[Grapheme.Cluster code-points: [cp1 cp2 ...]]` directly

The constructor is unchecked — the producer is responsible for correctness. Single-codepoint clusters (`[Grapheme.Cluster code-points: [cp]]`) trivially satisfy UAX#29 regardless of the source encoding.

Access to the code points:

```tinct
grapheme-codepoints: [fn@[List Int] [let g@Grapheme]
  [match g [Grapheme.Cluster p]: p.code-points]]
```

`grapheme?` (in text.llt) is a standalone validator for raw `[List Int]` code-point lists, used before construction.

No named `Character` or `Char` type. `Grapheme` is a nominal variant over a `[List Int]` payload; `Bytes = [List UInt8]` is a byte buffer. Both hold sequences of integers, distinguished by their nominal type and the invariant each enforces.

### Graphemes

`Graphemes: [type [List Grapheme]]` — a collected, O(1)-indexed sequence of grapheme clusters. Text literals `"hello"` produce `Graphemes` directly. All `[List ...]` operations apply, including direct indexed access:

```tinct
[
  s: "café"              # Graphemes = [List Grapheme] — collected, O(1)

  n:      [length s]     # → 4 (element count), O(1)
  first:  [nth 0 s]      # → "c" (1st by insertion order — use length+nth for positional)
  third:  [nth 2 s]      # → "f" (3rd by insertion order)
  by-key: [get s 2]      # → "f" (element at key 2 — use keys+get for key-based access)
  sub:    [slice s 1 3]  # → Graphemes "af", O(1)
  upper:  [map unicode-upper s]  # → Graphemes (new [List Grapheme])

  # [tail s] returns the sub-map with original keys: {1: "a", 2: "f", 3: "é"}
  # For sequential iteration, convert to GraphemeStream: [each s]
]
```

Two access patterns — keep them consistent, don't mix:

- **Positional** (`length` + `nth`): iterate by insertion-order position; `[nth n s]` gives the n-th element regardless of key values
- **Key-based** (`keys` + `get`): iterate the actual key values; `[each [keys s]]` yields the existing keys, `[get s key]` accesses by that key

For text literals: `[= [keys "abc"] [0 1 2]]` — keys are always 0..n-1, so both patterns are equivalent. For sparse lists (e.g. after `[set 5: g s]`), `[nth 1 s]` is the 2nd inserted element; `[get s 1]` is the element at key 1 (which may differ, or be Absent if key 1 was never inserted).

For lazy streaming of large text (e.g. decoding a network response), use `GraphemeStream = [Seq Grapheme]`. Collect to get `Graphemes` when you need random access:

```tinct
gs:  [decode Utf8Graphemes bytes]   # GraphemeStream — lazy, decode on demand
s:   [collect gs]                    # Graphemes — collected, O(1) access
```

Access patterns on `GraphemeStream` (not Indexed):

### Key Constraints — Equatable, Hashable, Sortable

See `doc/whatif/type-foundations.md` for the general typeclass hierarchy design (`Equatable`, `Hashable`, `Sortable`), the `HashableValue` Rust enum, and dispatch semantics.

The `Grapheme`- and `Graphemes`-specific typeclass instances are defined in `stdlib/prelude.llt`:

```tinct
[instance Equatable [let k@Grapheme]:
  [=: [fn [let a@Grapheme b@Grapheme] [builtin-grapheme-eq a b]]]]

[instance Equatable [let k@Graphemes]:
  [=: [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-eq a b]]]]

[instance Hashable [let k@Graphemes]:
  [=:    [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-eq a b]]
   hash: [fn [let a@Graphemes]             [builtin-graphemes-hash a]]]]

[instance Hashable [let k@Grapheme]:
  [=:    [fn [let a@Grapheme b@Grapheme] [builtin-grapheme-eq a b]]
   hash: [fn [let a@Grapheme]            [builtin-grapheme-hash a]]]]

[instance Sortable [let k@Graphemes]:
  [<: [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-lt a b]]]]  # lexicographic
```

`UInt8` similarly for `Hashable` and `Sortable`. **`Float` is `Equatable` and `Sortable` but NOT `Hashable`** — IEEE 754 `NaN != NaN` violates the reflexivity law, and `+0.0 == -0.0` would require equal hashes for different bit patterns. `Float` therefore has no `Hashable` instance and cannot be used as a dict key.

### Dict, Map, and List — The Type Hierarchy

See `doc/whatif/type-foundations.md` for the full type hierarchy design. The `Indexed` contract types relevant to text and bytes:

| Type | Key | Key constraint | Value | Notes |
|------|-----|----------------|-------|-------|
| `Graphemes` = `[List Grapheme]` | `Int` | `Hashable` | `Grapheme` | text literals |
| `List a` = `[Map Int a]` | `Int` | `Hashable` | `a` | produced by `collect` |
| `Bytes` = `[List UInt8]` | `Int` | `Hashable` | `UInt8` | byte buffers |
| `Dict` | `Graphemes` | `Hashable` | `Any` | named dict (heterogeneous) |

`[Seq T]` — including `GraphemeStream` and `ByteStream` — has no `Indexed` instance. `[get gs 4]` on a `GraphemeStream` is a type error. Two patterns for positional access on streams:

### Bytes and ByteStream

```tinct
Bytes:      [type [List UInt8]]   # collected buffer — O(1) indexed
ByteStream: [type [Seq UInt8]]    # lazy stream — not Indexed
```

`[Seq UInt8]` and `[List UInt8]` are distinct — the same distinction as `[Seq Grapheme]` vs `[List Grapheme]`:

- **`Bytes = [List UInt8]`** — a collected, finite, O(1)-indexed byte buffer. `[get bytes 4]` is O(1), `[slice bytes 0 10]` is O(1). This is what `read-chunk` returns. Runtime: `Value::Dict` with integer keys and `Value::Int` (0–255) values.

- **`ByteStream = [Seq UInt8]`** — a lazy, potentially unbounded stream of bytes, not Indexed. This is the type of byte-level lazy decoders, not I/O handles. (Note: the `ByteStream` typeclass in lib-net-v3 is a different concept — it is about handles whose `read` method returns `Bytes` chunks.)

`Bytes` is the I/O boundary for materialized data. UTF-8 decoding produces `[Seq Grapheme]`. The codec makes the conversion explicit:

```tinct
[
  # I/O returns Bytes
  raw: [read-chunk handle 4096]   # → Bytes

  # Decode to grapheme clusters explicitly
  text: [decode Utf8Graphemes raw]  # → GraphemeStream = [Seq Grapheme]

  # Encode back to bytes for writing — always explicit via Codec
  out: [encode Utf8Graphemes [collect text]]   # GraphemeStream → Graphemes → Bytes
]
```

Text literals are the one source of `Graphemes` that does not require an explicit decode step — source code is always valid UTF-8. All other text comes from decoding `Bytes` via a Codec.

### Codec Instances for UTF-8

Two codec instances handle UTF-8 text, defined in `stdlib/text.llt`:

```tinct
# Byte view — Bytes ↔ UInt8 (passthrough, encoding-agnostic)
Utf8Bytes:     [type Utf8Bytes]

# Grapheme cluster view — Bytes ↔ Grapheme (UAX#29 segmentation)
Utf8Graphemes: [type Utf8Graphemes]
```

`[decode Utf8Graphemes bytes]` → lazy `[Seq Grapheme]`. `[encode Utf8Graphemes graphemes]` → `Bytes`.

## The Indexed Contract

`Indexed` means O(1) by-key access. `[Seq T]` does **not** implement `Indexed` — it is a stream. Only collected structures do:

```tinct
[class [let Indexed s k v]
  get:    [Fn@[or v Absent] [s k]]
  slice:  [Fn@s [s Int Int]]
  length: [Fn@Integer [s]]]

# Bytes — O(1) byte access
[instance Indexed
  [let a@Bytes b@Integer c@UInt8]: [get:    builtin-bytes-get
                                 slice:  builtin-bytes-slice
                                 length: builtin-bytes-length]]

# Dict — O(1) Graphemes-keyed access
[instance Indexed
  [let a@Dict b@Graphemes c@Any]: [get:    builtin-dict-get
                                 slice:  [fn [let _ _ _] [raise "slice not defined for Dict"]]
                                 length: builtin-dict-length]]

# List a = Map Int a — O(1) integer-keyed access
[instance Indexed
  [let a@[Map Int T] b@Integer c@T]: [get:    builtin-dict-get
                                   slice:  builtin-dict-slice
                                   length: builtin-dict-length]]

# [Seq T] has NO Indexed instance — [get seq n] is a type error.
# Use [head [drop n seq]] for single access, or collect for repeated access.
```

```tinct
[
  # Graphemes — IS Indexed, O(1) direct access
  s:    "hello"         # Graphemes = [Map Int Grapheme]
  a:    [get s 0]       # O(1) — first grapheme
  b:    [get s 3]       # O(1) — fourth grapheme
  sub:  [slice s 1 4]   # O(1) — subrange "ell"

  # GraphemeStream — NOT Indexed; positional access needs scanning
  gs:   [each s]        # Graphemes → GraphemeStream (lazy)
  nth:  [head [drop 3 gs]]          # 4th element — O(3) scan, no materialisation
  all:  [collect gs]                # → Graphemes (re-collected), O(n) then O(1)
]
```

## `collect`

The general `collect` semantics (materializing `[Seq a]` → `[List a]`) are in `doc/whatif/type-foundations.md`.

For `GraphemeStream` specifically: `collect` on `[each "hello"]` produces `[0: Grapheme.Cluster{code-points:[104]} ...]` — each value is a `Grapheme` variant (single code point per ASCII character).

## Equality Semantics

Grapheme equality is structural: code-point-by-code-point comparison. This is a deliberate design decision.

```tinct
# U+00E9 (precomposed é) vs [U+0065, U+0301] (e + combining accent)
# These are the same visible character but different code-point sequences.
[= "\u{e9}" "\u{65}\u{301}"]   # → False — structural equality, not visual equality
```

Silent normalization is a lossy assumption. A user encoding combining marks intentionally (e.g. in linguistics, in protocol parsing) does not want silent rewriting of their data. The same visual character in two normalization forms is two different values.

Use `unicode-nfc` when canonical (visual) equality is needed:

```tinct
[= [unicode-nfc s1] [unicode-nfc s2]]   # canonical equality
```

Dict keys follow the same rule. A `Graphemes` key is looked up structurally. If external data arrives in mixed normalization forms, normalize at the ingestion boundary:

```tinct
[set dict [unicode-nfc raw-key] value]    # insert
[get [unicode-nfc lookup-key] dict]        # lookup
```

A canonical-comparison dict variant would violate Leibniz's law — `[= k1 k2]` would be `False` but dict lookup would treat them as equal. No such variant is provided.

This matches the behavior of Python, Rust, Go, and Java. Only Swift performs canonical equivalence comparison by default.

## Implementation Spec

The implementation is correct-first. No `Value::String` in the runtime.

**`Graphemes` IS `Value::Dict` (integer keys, Grapheme values).**
**`Grapheme` IS `Value::Variant { tag: "Grapheme.Cluster", payload: Value::Dict(integer keys, Value::Int code points) }`.**
**`Bytes` IS `Value::Dict` (integer keys, UInt8 values).**
**`GraphemeStream` and `ByteStream` are `Value::Variant`** (Seq.Cons/Seq.End — since `Seq` is a user-defined algebraic type, not a Rust primitive).

**Runtime type annotations for nominal structural types.** The general annotation machinery (`Value::Annotated`, `annotation-of`, `[@Type value]` wrapping, and serializer dispatch) is described in `doc/whatif/type-foundations.md`. Text-specific annotation behavior:

**Text literals carry `[type: Graphemes]` implicitly.** `"hello"` always evaluates to `Value::Annotated(Value::Dict(...), [type: Graphemes])` — no explicit annotation by the programmer required. Text IS Graphemes, so the annotation is unconditional and automatic at `eval_text_lit` time.

**Serializer dispatch for text:** a `Value::Annotated` with `[type: Graphemes]` serializes as a text string in the target format, encoding the grapheme clusters using the format's required encoding (UTF-8 for JSON/YAML/SCN). A `Value::Annotated` with `[type: Bytes]` uses format-appropriate byte representation (hex, base64, raw, etc.).

**Construction points that attach runtime annotations:**

- Text literals: `eval_text_lit` produces a `Value::Dict` of `Grapheme` variants, wrapped in `[@Graphemes ...]`
- Each `Grapheme` produced by the codec: `Value::Variant { tag: "Grapheme.Cluster" }` — introspectable via constructor pattern, no annotation needed
- `read-chunk`, `read`: `[@Bytes ...]`
- `env`, `%args`: `[@Graphemes ...]`
- Explicit `[@Type value]`: evaluator wraps at that point

```text
Text literal "café":
  - decoded from source UTF-8 as-is (no normalization)
  - produces Value::Dict: [0: Value::Variant("Grapheme.Cluster", {code-points: [233]})  ...]
  - each Grapheme is Value::Variant with tag "Grapheme.Cluster" and code-point payload
  - outer dict annotated [type: Graphemes]

[= s1 s2]    →  Dict equality (integer keys, Grapheme values)   O(n)
[length s]   →  Dict length → grapheme count   O(1)
[get s 2]    →  Dict lookup → Grapheme   O(1)
[head s]     →  Dict entry at key 0 → Grapheme   O(1)
[tail s]     →  Dict without key 0, original keys preserved   O(1)
[encode Utf8Graphemes s]  →  encode each Grapheme's code points to UTF-8 → Bytes  O(n·m)
```

**Text literals in the lexer/parser:** The parser produces `Ast::TextLit(Arc<str>)`. At eval time, `eval_text_lit` decodes the UTF-8 via the UAX#29 grapheme segmentation algorithm, constructs a `Value::Dict` with integer keys, and attaches the `[type: Graphemes]` annotation. No `Value::String` variant is created.

**`str` is replaced by `print`.** The variadic convert-and-concatenate function is renamed `print`:

```tinct
[class [let Showable t]
  show: [Fn@Graphemes [t]]]

print: [fn@Graphemes [let ...args@Showable]
  [join "" [map show args]]]
```

`@C` (constraint annotation) is a general mechanism that applies wherever `@` can appear — function parameters, variadic parameters, dict field values, variable bindings, return types. At each site where a value's concrete type is known, the type checker verifies that a `C t` instance exists for type `t`. `...args@Showable` is simply `@Showable` on a variadic parameter: the same check applied once per arg at the call site.

`[print 42 " hello " True]` resolves `Showable Int`, `Showable Graphemes`, and `Showable Boolean` independently at the call site — exactly as `[fn [let x@Showable] [show x]]` resolves `Showable t` for a single arg. `[map show args]` in the body dispatches correctly because each element's instance is pre-resolved.

**Implementation dependency:** The general `@C` constraint annotation mechanism is not yet fully implemented in the type checker — it works for explicit `constraint:` annotations on function parameters but not universally at all value positions. This whatif depends on that general mechanism being completed. The variadic case (`...args@Showable`) and the dict field case (`[handle@Readable: %infile]`) are not special — they are instances of the same general mechanism.

`show` converts one value to `Graphemes`; `print` applies `show` to each arg and concatenates. `Showable` instances are defined for `Int`, `Float`, `Boolean`, `Graphemes` (identity), `Grapheme`, `Bytes`, and user types.

`Seq` and `Boolean` de-primitisation is in `doc/whatif/type-foundations.md` (§What Would Change).

**Builtins that currently accept `Value::String`:** Each builtin is updated to accept `Value::Dict` (for `Graphemes`) or `Value::Seq` (for `GraphemeStream`). Pattern matching on `Value::String` is removed from the Rust evaluator entirely.

### Value-Keyed Dict — Eliminating the `Key` Enum

See `doc/whatif/type-foundations.md` for the full design of `HashableValue`, the `Hash`/`Eq` semantics, and computed key expressions in dict literals.

The text-specific migration consequence: every use of `Key::String(s)` becomes a `HashableValue::Dict(...)` representing the `Graphemes` for that string — the same conversion as text literal evaluation.

**C5 — Bare-word → Graphemes:** Bare-word field names follow the text literal path: UTF-8 bytes → `[List Grapheme]` via UAX#29 grapheme segmentation. This conversion is performed entirely in Rust — the `unicode-segmentation` crate (or equivalent) provides the algorithm directly to the evaluator. `text.llt` wraps this Rust algorithm as tinct-level functions (`grapheme?`, `gcb-property`, `gcb-boundary?`, etc.) for user code; it does not define the algorithm itself. There is no bootstrap circularity: bare-word field name segmentation is available from the first instruction, regardless of which stdlib files have loaded. Non-ASCII field names (`[日本語: "value"]`) work correctly at all times — three graphemes, each a single-codepoint `[List Int]`, produced by the same Rust call used for text literals.

**C4 — Graphemes → display string (serializer):** The SCN serializer reads `annotation-of v` to determine output format. When the annotation dict contains `[type: Graphemes]`, the serializer traverses the `Value::Dict` directly: iterate grapheme clusters (inner dicts annotated `[type: Grapheme]`) → for each cluster iterate code points (inner ints) → encode each code point as UTF-8 → output as a quoted string. This is a pure Rust operation — the serializer reads the annotation dict and iterates `Value::Dict` integer keys and `Value::Int` code-point values without calling any tinct stdlib function. No `encode Utf8Graphemes` call. Values without a `[type: Graphemes]` annotation are serialized structurally. No bootstrap dependency.

**Performance:** Field name hashing is O(k) where k = code points in the field name. For typical names ("name", "id", "value"), k ≤ 10.

**Files requiring changes:**

- `src/value.rs` — replace `Key` enum with `HashableValue`; change `Value::Dict` field type
- `src/eval_dict.rs` — key construction and lookup uses `HashableValue`
- `src/eval.rs` — all `Key::Int` and `Key::String` pattern matches → `HashableValue`
- `src/builtins_core.rs`, `src/builtins_dict.rs` — dict builtins updated
- `src/serializer.rs`, `src/formatter.rs` — display/serialization of dict keys
- All corpus tests that produce dict output with string keys

## Bootstrap and Module Structure

**Bootstrap order — no stage requires text.llt:**

1. **`stdlib/loader.llt`** — the first tinct code evaluated at process startup; uses `--- uses: ["core"]` only; prelude is not yet loaded when it runs; defines `eval-programs` which loads prelude. No text.llt dependency.
2. **`stdlib/prelude.llt`** — loaded by eval-programs; no `--- uses:` declaration; no dependency on text.llt or the GCB builtins. Contains only pure type declarations and core builtins. Text literal segmentation (`eval_text_lit`) is performed at the Rust level via `unicode_gcb.rs` — prelude.llt is not involved.
3. **`stdlib/text.llt`** — loaded only when user code or a stdlib file declares `--- uses: ["text"]`.

**`stdlib/prelude.llt`** type declarations:

```tinct
# Type declarations — pure structural, no runtime text machinery needed
Grapheme:       [type [Cluster code-points: [List Int]]]   # nominal variant
Graphemes:      [type [List Grapheme]]
GraphemeStream: [type [Seq Grapheme]]
Bytes:          [type [List UInt8]]
ByteStream:     [type [Seq UInt8]]
Boolean:        [type True False]
List:           [type [let a] [Map Int a]]
Seq:            [type [let a]  [Cons head@a  tail@[Seq a]]  End]
```

**`stdlib/text.llt`** — header: `--- uses: ["text"]` (required for `encoding_rs`-backed normalization and case functions). Contains all GCB machinery, validators, and codec implementations.

**GCB Rust builtins — registration path:**

`src/unicode_gcb.rs` implements the property table and state machine. `src/builtins_text.rs` registers four builtins in the standard builtin table (alongside `builtin-int-eq` etc.) — available to all tinct code by name, no `--- uses:` needed to call them directly:

```tinct
builtin-gcb-property:       [fn@GcbProperty  [Int]]                              # code point → GCB property
builtin-gcb-boundary:       [fn@Boolean       [GcbProperty GcbProperty GcbState]] # prev cur state → boundary?
builtin-gcb-next-state:     [fn@GcbState      [GcbProperty GcbState]]             # advance state machine
builtin-gcb-initial-state:  [fn@GcbState      []]                                 # initial state
```

`GcbProperty` and `GcbState` are nominal variant types declared in text.llt's private section:

```tinct
GcbProperty: [type Cr Lf Control Extend Zwj RegionalIndicator Prepend SpacingMark
                   L V T Lv Lvt ExtendedPictographic Other]
GcbState:    [type GcbState0 GcbState1 ...]   # states per UAX#29 Table 3
```

text.llt's public section exports the user-facing names as thin aliases:

```tinct
gcb-property:       builtin-gcb-property
gcb-boundary?:      builtin-gcb-boundary
gcb-next-state:     builtin-gcb-next-state
gcb-initial-state:  [builtin-gcb-initial-state]
```

Higher-level functions are pure tinct calling only the above:

- `gcb-cluster-end`, `gcb-first-boundary` — pure tinct
- `grapheme?: [fn@Boolean [let cps@[List Int]] ...]` — standalone validator; NOT attached to the type in prelude
- `Utf8Bytes`, `Utf8Graphemes` codec implementations
- `unicode-nfc/nfd/nfkc/nfkd`, `unicode-upper/lower-grapheme` — `encoding_rs`-backed Rust builtins, registered similarly

**User access:** `[include %libdir "text.llt"]` in user code, or `--- uses: ["text"]` in the document header which auto-includes text.llt. The `Grapheme`, `Graphemes`, `Bytes` types are always available — they are declared in prelude.llt which loads unconditionally.

`Grapheme` is a nominal variant — codec-produced Graphemes are valid by construction. Users constructing Graphemes manually validate first with `grapheme?` then construct:

```tinct
[if [grapheme? cps] [Grapheme.Cluster code-points: cps] [raise "invalid grapheme"]]
```

**`doc/whatif/lib-net-v3.md`** covers the `Codec` typeclass, `TextEncoding`, and encoding infrastructure.

The type-checking bootstrap de-special-casing (`build_prelude_env_inner()`, `PRELUDE_CACHE`, incremental env accumulation) is in `doc/whatif/type-foundations.md` (§Bootstrap and Module Structure).

## What Would Change

### Value Representation

**Current:** `Value::String(Arc<str>)` — opaque UTF-8 bytes.
**Proposed:** Removed entirely. `Graphemes` = `Value::Dict` (integer keys), `GraphemeStream` = `Value::Seq`, `Bytes` = `Value::Dict` (integer keys), `ByteStream` = `Value::Seq`.
**Impact:** Fundamental. Every Rust match on `Value::String` is deleted. The type system gains an honest model.

### `str` → `print`

**Current:** `str` is a variadic builtin that converts args via internal type dispatch and concatenates.
**Proposed:** Removed. Replaced by `print` backed by the `Showable` typeclass:

```tinct
[class [let Showable t]  show: [Fn@Graphemes [t]]]
print: [fn@Graphemes [let ...args@Showable] [join "" [map show args]]]
```

**Impact:** Major. All call sites of `str` become `print`. All `str-*` prefixed functions either become generic `Graphemes`/`List` operations (dropping the prefix) or are removed. Interpolated string literals `i"..."` currently desugar to `[str ...]`; they must now desugar to `[print ...]`.

### Text Builtins

**Current:** `str-length`, `str-slice`, `str-contains?`, `starts-with?`, `ends-with?`, `str-chars`, `upper`, `lower`, `trim`, `pad-left`, `pad-right` operate on byte offsets or Rust's `char` iterator.

**Proposed:** Complete mapping:

| Old | New | How |
|-----|-----|-----|
| `str-length s` | `length s` | generic `[Map Int a]` — O(1) |
| `str-slice s i j` | `slice s i j` | generic `[Map Int a]` — O(1) |
| `str-chars s` | removed | code points are `Int`; use `[decode Utf8Graphemes [encode Utf8Graphemes s]]` for raw iteration |
| `upper s` | `upper s` | `[collect [flat-map unicode-upper-grapheme [each s]]]` — may expand (ß→SS) |
| `lower s` | `lower s` | `[collect [flat-map unicode-lower-grapheme [each s]]]` |
| `trim s` | `trim s` | `[collect [filter [not= _ whitespace-grapheme] [each s]]]`... actually leading/trailing only — see below |
| `pad-left s n fill` | `pad-left s n fill` | `[concat [take [- n [length s]] [cycle fill]] s]` |
| `pad-right s n fill` | `pad-right s n fill` | `[concat s [take [- n [length s]] [cycle fill]]]` |
| `starts-with? prefix s` | `starts-with? prefix s` | `[= [slice s 0 [length prefix]] prefix]` |
| `ends-with? suffix s` | `ends-with? suffix s` | `[= [slice s [- [length s] [length suffix]] [length s]] suffix]` |
| `str-contains? sub s` | `contains? sub s` | `[any? [fn [let i] [= [slice s i [+ i [length sub]]] sub]] [range 0 [length s]]]` |
| `split sep s` | `split sep s` | pure tinct: scan for separator grapheme/graphemes |
| `join sep xs` | `join sep xs` | already generic |
| `trim s` | `trim s` | `[collect [drop-while whitespace? [reverse [collect [drop-while whitespace? [each s]]]]]]` |

`unicode-upper-grapheme` and `unicode-lower-grapheme` are Rust builtins: `Fn@GraphemeStream [Grapheme]` — they return a stream because a single grapheme can expand (German ß → SS in uppercase). `whitespace?` is a predicate on `Grapheme` checking for Unicode whitespace code points.

**`str?` → `text?`:** The `str?` predicate is replaced by `text?`, which checks the runtime annotation:

```tinct
text?: [fn@Boolean [let v]
  [and [not [null? v]]
       [list? v]
       [match [head v]
         [Grapheme.Cluster _]: True
         _:                   False]]]
```

Returns `True` if `v` is a non-null list whose first element is a `Grapheme.Cluster` variant. `""` evaluates to `[]` (null — zero grapheme clusters, empty dict), so `text?` correctly returns `False` for it. The `[not [null? v]]` guard makes the `[empty? v]` check unnecessary: anything that passes null-check and list-check has at least one element. Any non-null `[List Grapheme]` is text by definition.

**Impact:** Major. The `str-*` prefix disappears. Pure tinct implementations replace byte-level Rust builtins. `upper`/`lower`/`trim` require two new Rust primitives: `unicode-upper-grapheme` and `unicode-lower-grapheme`. `str?` becomes `text?`.

### GCB Architecture — Two Separate Spaces

The GCB state machine primitives (`gcb-property`, `gcb-boundary?`, `gcb-next-state`, `gcb-initial-state`) are shared pure functions on code points. Above them, the machinery splits into two distinct spaces with different inputs and purposes:

**Segmentation space — `gcb-cluster-end`** operates on `Bytes`, scanning byte-by-byte, yielding the byte offset where the current cluster ends. Used by the `Utf8Graphemes` codec to produce `Grapheme` values lazily:

```tinct
# Scan forward from start, decoding UTF-8 one code point at a time.
# Stop at the first GCB boundary and return that byte offset.
# Returns [length b] if no boundary found before end of buffer.
gcb-cluster-end: [fn [let b@Bytes start@Integer prev-prop state]
  [n: [length b]]
  [scan: [fn [let offset prev-prop state]
    [if [>= offset n]
      offset
      [block
        [r:    [utf8-decode-one b offset]]
        [prop: [gcb-property r.value]]
        [if [gcb-boundary? prev-prop prop state]
          offset
          [scan r.next prop [gcb-next-state prop state]]]]]]]
  [r0: [utf8-decode-one b start]]
  [scan r0.next [gcb-property r0.value] [gcb-next-state [gcb-property r0.value] gcb-initial-state]]]
```

**Validation space — `gcb-first-boundary` and `grapheme?`** operate on `[List Int]` (collected code points), checking for internal boundary violations. Used as the type constraint on `Grapheme`:

```tinct
# Returns the index of the first GCB boundary in a [List Int], or Absent if
# the entire list is a single cluster. Used by grapheme? for type validation.
gcb-first-boundary: [fn [let cps@[List Int]]
  [n: [length cps]]
  [step: [fn [let i prev-prop state]
    [if [>= i n]
      Absent.Absent
      [block
        [cp:   [nth i cps]]
        [prop: [gcb-property cp]]
        [if [gcb-boundary? prev-prop prop state]
          i
          [step [+ i 1] prop [gcb-next-state prop state]]]]]]]
  [if [= n 0] Absent.Absent
    [step 1 [gcb-property [nth 0 cps]] [gcb-next-state [gcb-property [nth 0 cps]] gcb-initial-state]]]]

grapheme?: [fn@Boolean [let cps@[List Int]]
  [and [not [empty? cps]] [absent? [gcb-first-boundary cps]]]]
```

`grapheme?` does not need to fully implement UAX#29 cluster production — it only checks for violations in an already-collected `[List Int]`. The codec produces clusters; `grapheme?` verifies them.

**Impact:** Clean separation. The codec (segmentation) and the type constraint (validation) operate independently. The shared GCB primitives are the only common ground.

### Text Literals

**Current:** Parsed to `Ast::StringLit`, evaluated to `Value::String(Arc<str>)`.
**Proposed:** `"..."` is a text literal producing `Graphemes`. Parsed to `Ast::TextLit`, evaluated to `Value::Dict` (integer keys, Grapheme values) by walking the UAX#29 grapheme segmentation algorithm over the source UTF-8. No `Value::String` is produced.
**Impact:** Moderate. Eval code for text literals changes; literal production is O(n) rather than O(1).

### I/O Builtins

**Current:** `open`/`read-chunk` return `Value::String` in some cases. `env` returns `Value::String`. `%args` is a list of `Value::String`.

**Proposed:** Every builtin that previously returned `String` now returns `Graphemes` or `Bytes` as appropriate:

| Builtin | Current | Proposed | Note |
|---------|---------|----------|------|
| `read-chunk` | `String` | `Bytes` | raw bytes; decode explicitly via Codec |
| `read` (line) | `String` | `Bytes` | same; caller decodes |
| `env` | `String` | `Graphemes` | environment variable values are text |
| `%args` | `[List String]` | `[List Graphemes]` | command-line arguments are text |

Text conversion from I/O is always explicit via a Codec:

```tinct
[
  raw:  [read-chunk handle 4096]          # → Bytes
  text: [collect [decode Utf8Graphemes raw]]  # → Graphemes
]
```

**Impact:** Moderate. Any caller reading text from I/O must insert an explicit decode step. `env` and `%args` callers receive `Graphemes` directly — no decode needed since they are already text, not raw bytes.

### `Showable` Instances

The full `Showable` instance set (including `Int`, `Float`, `Boolean`, `List a`, `Map k v`) is in `doc/whatif/type-foundations.md`. Text and bytes instances:

```tinct
[instance Showable [let t@Graphemes]:
  [show: [fn [let gs] gs]]]                                # identity

[instance Showable [let t@Grapheme]:
  [show: [fn [let g] [$g]]]]                              # one-element auto-indexed dict = [List Grapheme]

[instance Showable [let t@Bytes]:
  [show: [fn [let b]
    [join " " [map [fn [let byte] [builtin-byte-to-hex byte]] [each b]]]]]]
  # → "48 65 6c 6c 6f" (lowercase hex pairs, space-separated)
```

`GraphemeStream` and `ByteStream` are NOT `Showable` — lazy potentially-infinite streams cannot be rendered to text. User-defined types implement `Showable` by declaring an instance.

`builtin-byte-to-hex: Fn@Graphemes [UInt8]` returns a two-character lowercase hex pair (`"0a"`, `"ff"`). `builtin-int-to-graphemes` and `builtin-float-to-graphemes` are new Rust primitives (defined in text-foundations context).

## Implementation Notes

See also `doc/whatif/type-foundations.md` for `builtin-eval` return type fix and runtime typeclass dispatch required changes.

### Display Visitor and Corpus Tests

The display visitor (`value_to_display_string`, used by corpus test output comparison) must apply the same annotation-of rule as all other serializers: detect `[type: Graphemes]` and encode to UTF-8 as a quoted string. If it does, corpus test output for Graphemes values continues to appear as `"hello"` and the corpus test impact is ~0 additional changes beyond the Bool/Seq migration.

**Graphemes → UTF-8 encoding.** A shared Rust function `graphemes_to_utf8(value: &Value) -> String` should hold the encoding logic. The tinct-side `encode Utf8Graphemes` codec (which the programmer calls explicitly for I/O) is a thin wrapper over this same Rust function. The display visitor calls the Rust function directly — without threading an EvalContext — so there is no need to invoke tinct code from the display path. One encoding implementation, two call sites.

### GCB Property Table — One Implementation, Multiple Call Sites

No new crate dependency is needed. The UAX#29 GCB algorithm lives in **one Rust module** — `src/unicode_gcb.rs` — the single source of truth for all grapheme cluster break operations:

```rust
// src/unicode_gcb.rs — single source of truth
pub fn gcb_property(cp: u32) -> GcbProperty { ... }   // generated static lookup table
pub fn gcb_boundary(prev: GcbProperty, cur: GcbProperty, state: GcbState) -> bool { ... }
pub fn gcb_next_state(prop: GcbProperty, state: GcbState) -> GcbState { ... }
pub fn gcb_initial_state() -> GcbState { ... }
```

The GCB property table is a generated Rust file derived from the Unicode Character Database, checked into the repository. Two call sites use `unicode_gcb`:

- **`src/builtins_text.rs`** — `gcb-property`, `gcb-boundary?`, `gcb-next-state`, `gcb-initial-state` builtins call `unicode_gcb::*` directly. The tinct-level `gcb-cluster-end` and `gcb-first-boundary` in text.llt call these builtins, and therefore also use this implementation.
- **`src/eval.rs` (`eval_text_lit`)** — calls `unicode_gcb::*` directly to segment text literals, without going through tinct.

Bare-word field name segmentation uses the same code path. Divergence between text literal segmentation and tinct-level GCB functions is **structurally impossible** — there is one implementation, two call sites.

## References

- Unicode Consortium (2023). "Unicode Standard Annex #29: Unicode Text Segmentation." [https://www.unicode.org/reports/tr29/](https://www.unicode.org/reports/tr29/) — defines grapheme cluster break rules (GCB algorithm, property tables)
- Unicode Consortium (2023). "Unicode Standard Annex #15: Unicode Normalization Forms." [https://www.unicode.org/reports/tr15/](https://www.unicode.org/reports/tr15/) — NFC, NFD, NFKC, NFKD; referenced for `unicode-nfc` design decision
- Rust `unicode-segmentation` crate. — reference implementation of UAX#29 grapheme cluster iteration in Rust; informs the GCB state machine design
- Python `str` documentation. §"Text Sequence Type." — structural equality without normalization; same design decision as tinct's
- `doc/whatif/lib-net-v3.md` — `Codec` typeclass, `Utf8Graphemes`, `TextEncoding` infrastructure
- `doc/whatif/completed/numeric-types.md` — `UInt8` as `Int@[is: [between 0 255]  repr: u8]`; same constrained nominal type pattern
