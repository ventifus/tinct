# What If: Text Redesign — Unicode Grapheme Cluster Model

**State:** Proposal

What would it take to replace tinct's opaque UTF-8 `String` type with first-class Unicode types that are honest about their structure?

## Goals

1. **Retire `String`.** Replace it with `Graphemes = [List Grapheme]` — a collected list of Unicode grapheme clusters providing O(1) indexed access at whole-grapheme boundaries. No `Value::String` in the runtime, no `Type::Str` in the type checker, no `Key::String` in the dict key enum.

2. **Make constrained types and multiple-dispatch work correctly.** Define a typeclass hierarchy (`Equatable`, `Hashable`, `Sortable`, `Showable`) with lexically-scoped runtime dispatch via the environment chain. Instances are real runtime bindings, not type-checker annotations only.

3. **Refine what `Dict`, `Map`, and `List` are.** Establish a coherent type hierarchy: `Dict` is the fundamental heterogeneous collection; `[Map k v]` is its homogeneous typed form; `List a = [Map Int a]` is the integer-keyed specialization. Define the `Indexed` contract for O(1) access.

4. **Generic dict key constraint.** Any type satisfying `Hashable` can be a dict key — not just hardcoded `String` or `Int`. `[Map k v]` uniformly requires `k: Hashable`, enforced through the typeclass system.

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

Dict keys need `Hash + Eq` (IndexMap implementation). Three-typeclass hierarchy with multimethod dispatch:

```tinct
# ── Prelude catch-all for cross-type Equatable comparisons ───────────────────
# Lower priority than any concrete Equatable instance.
# Matches only when BOTH args satisfy Equatable — not for non-Equatable types.

=: [fn@Boolean [let a@Equatable b@Equatable] False]  # cross-type Equatable × Equatable → False
<: [fn@Boolean [let a@Sortable  b@Sortable]  False]  # cross-type Sortable × Sortable → False

# Three outcomes for [= a b]:
#   Same-type Equatable   → concrete instance fires → True or False
#   Cross-type Equatable  → catch-all fires → False
#   Non-Equatable type    → no match anywhere → type error: no = for types Foo and Bar

# ── Typeclasses ───────────────────────────────────────────────────────────────

[class [let Equatable k]
  =: [Fn@Boolean [k k]]]    # same-type equality; instances override [=] for their type

[class [let Hashable k]  [Equatable k]    # Hashable implies Equatable
  hash: [Fn@Int [k]]]                     # consistent with =: [= a b] → [= [hash a] [hash b]]

[class [let Sortable k]  [Equatable k]    # Sortable implies Equatable, NOT Hashable
  <: [Fn@Boolean [k k]]]

# ── Equatable instances (more specific → higher dispatch priority) ─────────────

[instance Equatable [let k@Int]:
  [=: [fn [let a@Int b@Int] [builtin-int-eq a b]]]]

[instance Equatable [let k@Boolean]:
  [=: [fn [let a@Boolean b@Boolean]
    [match [a b]  [True True]: True  [False False]: True  _: False]]]]

[instance Equatable [let k@Grapheme]:
  [=: [fn [let a@Grapheme b@Grapheme] [builtin-grapheme-eq a b]]]]

[instance Equatable [let k@Graphemes]:
  [=: [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-eq a b]]]]

[instance Equatable [let k@Float]:
  [=: [fn [let a@Float b@Float] [builtin-float-eq a b]]]]

# ── Dispatch semantics ────────────────────────────────────────────────────────

# [= a b] dispatch (most specific first):
#   1. Equatable Int instance       — both Int              → True/False
#   2. Equatable Graphemes instance — both Graphemes        → True/False
#   3. ... other concrete instances ...
#   4. Prelude catch-all            — both @Equatable       → False (cross-type)
#   5. No match                     — non-Equatable type    → TYPE ERROR
#
# Examples:
#   [= (Int 42) (Int 43)]    → step 1 → False
#   [= (Int 42) "hello"]     → step 4 → False  (both are Equatable)
#   [= fn1 fn2]              → step 5 → type error (Fn is not Equatable)
#
# Dict key lookup: [Map k v] requires k: Hashable (implies k: Equatable).
# All keys in a lookup are the same type k — same-type comparison only.
# HashableValue::PartialEq is called with same-type arguments; different variants
# is an invariant violation (debug_assert), not a designed fallback.
```

**Cross-type handling:** `[=]` and `[<]` are Rust builtins. They were already Rust builtins — this is not a change. Their implementation explicitly handles cross-type comparison: if both values have the same type, dispatch to the `Equatable`/`Sortable` instance; if types differ, return `False` directly. This cross-type-returns-False behaviour cannot be expressed as a tinct function because there is no "dispatch with fallback" mechanism — a failed constraint dispatch is a type error, not a graceful `False`.

The Rust backing of `[=]` receives only same-type arguments (the type checker prevents cross-type calls). The implementation:

```rust
// builtins_core.rs — [=] operator (called only with same-type, same-variant arguments)
fn builtin_eq(a: HashableValue, b: HashableValue) -> bool {
    debug_assert_eq!(std::mem::discriminant(&a), std::mem::discriminant(&b),
        "invariant violation: [=] called with different HashableValue variants");
    a == b  // uses manually-implemented PartialEq for HashableValue
}
```

`[<]` is identical in structure via `Sortable`. **Correctness invariant:** the `PartialEq` impl on `HashableValue` must produce the same result as the `Equatable` instances — both define structural comparison. This is enforced by construction: the Equatable instances delegate to the same `builtin-*-eq` primitives that the `PartialEq` impl uses.

Instances use the correct class parameter type `b@k` — the dispatch guarantees only same-type arguments reach an instance:

**`[Map k v]` requires `k: Hashable`.** Instances correctly typed — `b: k` matches the class signature `[Fn@Boolean [k k]]`:

```tinct
[instance Hashable [let k@Int]:
  [=:    [fn [let a@Int b@Int] [builtin-int-eq a b]]
   hash: [fn [let a@Int]       a]]]              # Int IS its own hash — the integer value itself

[instance Hashable [let k@Graphemes]:
  [=:    [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-eq a b]]
   hash: [fn [let a@Graphemes]             [builtin-graphemes-hash a]]]]
   # hash of Graphemes: commutative combiner over code points of each cluster
   # consistent with =: structurally equal Graphemes produce equal hashes

[instance Hashable [let k@Grapheme]:
  [=:    [fn [let a@Grapheme b@Grapheme] [builtin-grapheme-eq a b]]
   hash: [fn [let a@Grapheme]            [builtin-grapheme-hash a]]]]

[instance Hashable [let k@Boolean]:
  [=:    [fn [let a@Boolean b@Boolean] [match [a b]  [True True]: True  [False False]: True  _: False]]
   hash: [fn [let a@Boolean]           [match a  True: 1  False: 0]]]]

[instance Sortable [let k@Int]:
  [<: [fn [let a@Int b@Int]      [builtin-int-lt a b]]]]

[instance Sortable [let k@Graphemes]:
  [<: [fn [let a@Graphemes b@Graphemes] [builtin-graphemes-lt a b]]]]  # lexicographic
```

`UInt8` similarly for `Hashable` and `Sortable`. **`Float` is `Equatable` and `Sortable` but NOT `Hashable`** — IEEE 754 `NaN != NaN` violates the reflexivity law (`[= a a]` must be `True`), and `+0.0 == -0.0` would require equal hashes for different bit patterns. `Float` therefore has no `Hashable` instance, no `HashableValue::Float` variant, and cannot be used as a dict key. When `[= a b]` is called, the type class dispatcher looks up `=` in the environment and selects the most specific matching `Equatable` instance for the argument types. If no concrete instance matches (cross-type or non-Equatable), the catch-all fires or a type error is raised — `[=]` is the dispatch target, not the dispatcher.

**Sortable superclass delegation:** `Sortable` instances declare only `<:`. The `=` requirement (imposed by the `Equatable` superclass of `Sortable`) is satisfied by the independently-registered `Equatable` or `Hashable` instance for the same type. At runtime, `[=]` and `[<]` look up separate multi-valued bindings in the environment — there is no OOP-style inheritance. The type checker verifies that both an `Equatable` instance (for `=`) and a `Sortable` instance (for `<`) exist whenever a `Sortable` constraint is required. A type that has a `Sortable` instance but no `Equatable` instance is a type error at the class declaration level.

### Dict, Map, and List — The Type Hierarchy

All tinct collections are instances of one general type constructor: `[Map k v]` where `k: Hashable`. The **universal constraint is `k: Hashable`** — this replaces the hardcoded `Key { Int, String }` enum with a uniform, user-extensible rule.

Three named special cases:

| Name | Definition | Key | Value | Notes |
|------|-----------|-----|-------|-------|
| `List a` | `[Map Int a]` | `Int` | uniform `a` | ordered sequences; `collect`, auto-indexed literals |
| `Dict` | `[Map Graphemes Any]` | `Graphemes` | heterogeneous | named-field records; bare-word keys in source |
| `[Map k v]` | general | any `k: Hashable` | uniform `v` | user-defined maps with typed keys |

`Dict` is the named-field heterogeneous collection. Bare-word field names in tinct source (`[name: "Alice"  age: 30]`) produce a `Dict` whose keys are `Graphemes` values. Before this redesign, those keys were `Key::String(Rc<str>)`; after, they are `HashableValue::Dict(...)` representing the `Graphemes` for the field name.

`List a = [Map Int a]` is a transparent type alias — the type checker treats `List Int` and `[Map Int Int]` identically. `collect`, auto-indexed literals, and range results all produce `List`.

`[Seq a]` is NOT a `Map` — it is a lazy cons-list defined in tinct as a recursive algebraic type. It has no key type, is not `Indexed`, and the `k: Hashable` constraint does not apply.

Runtime: all `Map` variants are `Value::Dict(IndexMap<HashableValue, ThunkId>)`. The key type determines the `HashableValue` variant — `HashableValue::Int` for `List`, `HashableValue::Dict(...)` (a Graphemes) for named `Dict`.

`List` is `Indexed` (via the `[Map Int T]` instance since `List` is a transparent alias). `[Seq T]` is NOT `Indexed` — it has no `Indexed` instance. Types providing the `Indexed` contract (O(1) by-key access):

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
  length: [Fn@Int [s]]]

# Bytes — O(1) byte access
[instance Indexed
  [let a@Bytes b@Int c@UInt8]: [get:    builtin-bytes-get
                                 slice:  builtin-bytes-slice
                                 length: builtin-bytes-length]]

# Dict — O(1) Graphemes-keyed access
[instance Indexed
  [let a@Dict b@Graphemes c@Any]: [get:    builtin-dict-get
                                 slice:  [fn [let _ _ _] [raise "slice not defined for Dict"]]
                                 length: builtin-dict-length]]

# List a = Map Int a — O(1) integer-keyed access
[instance Indexed
  [let a@[Map Int T] b@Int c@T]: [get:    builtin-dict-get
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

```tinct
collect: [fn@[List a] [let s@[Seq a]] ...]
```

`collect` materializes a lazy `[Seq a]` into a `List a` = `Map Int a`. Cost: O(n) to scan the full sequence and build the integer-keyed dict. Post-collect, all accesses are O(1).

Access patterns:

| Pattern | How | Time | Memory |
|---------|-----|------|--------|
| Sequential | `head`, `tail`, `map`, `filter` | O(1) per step, lazy | O(1) |
| Single element at position n | `[head [drop n s]]` | O(n) | O(1) — no materialisation |
| Many accesses at different positions | `[ls: [collect s]]` then `[get ls i]` | O(n) once + O(1) each | O(n) |

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

**Runtime type annotations for nominal structural types.** `Grapheme`, `Graphemes`, `Bytes`, and other nominal types defined as `[type SomeStructure]` are transparent to generic operations — their runtime value is a plain `Value::Dict`. Every value of a nominal structural type carries `[type: TheType]` in its annotation dict, making it introspectable:

```tinct
[annotation-of "hello"]          # → [type: Graphemes]
[annotation-of [nth 0 "hello"]]  # → [type: Grapheme]
[annotation-of [read-chunk h n]] # → [type: Bytes]
```

**Annotation change:** `[@Type value]` currently asserts and passes the value through. **Proposed:** it additionally wraps the result in `Value::Annotated { inner: value, annotation: [type: Type] }`, making the annotation introspectable at runtime via `annotation-of`.

```tinct
[@Graphemes s]    # → Value::Annotated(s, [type: Graphemes])
foo@Graphemes: s  # binding annotation — same wrapping applied at bind time
```

`[$s]@Graphemes` is unrelated: `[$s]` constructs a 1-element dict `{0: s}`; `@Graphemes` annotates that dict. `s` itself is not wrapped.

`annotation-of` is backed by `Value::Annotated { inner, annotation }` in `value.rs` and the `make-annotated` builtin. All Value operations transparently unwrap `inner` — `annotation-of` is the only operation that observes the annotation. Infrastructure already in place; currently used for functions and unit constructors.

**Text literals carry `[type: Graphemes]` implicitly.** `"hello"` always evaluates to `Value::Annotated(Value::Dict(...), [type: Graphemes])` — no explicit annotation by the programmer required. Text IS Graphemes, so the annotation is unconditional and automatic at `eval_text_lit` time.

**All serializers** (SCN, JSON, YAML, and any future format) dispatch on `annotation-of` to determine output format. This is the universal rule: before serializing any value, check its annotation:

- `[type: Graphemes]` → serialize as a text string in the target format, encoding the grapheme clusters using the format's required encoding (UTF-8 for JSON/YAML/SCN, or whichever codec the format specifies)
- `[type: Bytes]` → format-appropriate byte representation (hex, base64, raw, etc.)
- anything else → structural output per the serializer's normal rules

JSON example: a `Value::Annotated` with `[type: Graphemes]` serializes as a JSON string `"hello"`, not as a nested array of arrays of ints. YAML similarly. The annotation is the contract between the runtime value and any serializer — the serializer is responsible for applying the correct codec for its output encoding.

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

**`Seq` is defined in tinct, not Rust.** Because tinct is lazy by default, `tail` in the Cons case is a thunk — no Rust magic needed:

```tinct
Seq: [type [let a]
  [Cons head@a  tail@[Seq a]]
  End]

head: [fn@[bind: [a]] [let s@[Seq a]]
  [match s  [Seq.Cons p]: p.head  Seq.End: [raise "head: empty sequence"]]]

tail: [fn@[bind: [a]] [let s@[Seq a]]
  [match s  [Seq.Cons p]: p.tail  Seq.End: [raise "tail: empty sequence"]]]
```

`Seq.Cons` and `Seq.End` are used with the qualified name — no prelude aliases. User code uses library functions (`cons`, `range`, `map`, `filter`) for construction and `[Seq.Cons p]` / `Seq.End` in pattern matches.

**`Boolean` is defined in tinct, not Rust:**

```tinct
Boolean: [type True False]
True:    Boolean.True
False:   Boolean.False
```

No auto-injection — the explicit bindings `True:` and `False:` are required. The tracker task implementing auto-injection of constructor names is rejected; that mechanism is a shortcut that bypasses correctness.

**Builtins that currently accept `Value::String`:** Each builtin is updated to accept `Value::Dict` (for `Graphemes`) or `Value::Seq` (for `GraphemeStream`). Pattern matching on `Value::String` is removed from the Rust evaluator entirely.

### Value-Keyed Dict — Eliminating the `Key` Enum

**Current Rust:** `Value::Dict(IndexMap<Key, ThunkId>)` where `Key` is:

```rust
enum Key { Int(i64), String(Rc<str>) }
```

**Problem:** `Key::String` is a shortcut that only allows strings as dict field names. With Graphemes replacing String as the text type, replacing it with `Key::Graphemes(Graphemes)` would be the same mistake — still special-casing one type. The correct design: any Hashable tinct value can be a dict key.

**Proposed Rust:** `Value::Dict(IndexMap<HashableValue, ThunkId>)` where `HashableValue` is:

```rust
/// A fully-materialised tinct value that implements Hash + Eq.
/// Only these Value variants may appear as dict keys.
#[derive(Clone, Debug)]
enum HashableValue {
    Int(i64),
    Bool(bool),                    // Boolean.True / Boolean.False
    Dict(Vec<(HashableValue, HashableValue)>),  // pairs in insertion order
    Variant { tag: Rc<str>, payload: Option<Box<HashableValue>> },
}
```

Note: `#[derive(Hash, PartialEq)]` is NOT used for `Dict` — see manual implementations below.

**Why `HashableValue` rather than `Value` directly:** `Value` contains `Function`, `Handle`, `Task`, and lazy `Seq` variants that cannot be hashed (no structural identity). `HashableValue` is the hashable subset — finite, fully materialised, structurally defined.

**`Hash` and `Eq` semantics — verifiable properties:**

1. **Consistency with tinct `[=]`:** `hash(a) == hash(b)` whenever `[= a b]` is `True`. This follows from:
   - `[= Int(m) Int(n)]` iff `m == n` in Rust → `hash(Int(m)) == hash(Int(n))` iff `m == n` ✓ (i64 Hash)
   - `[= Dict(a) Dict(b)]` iff same key-value pairs regardless of insertion order → hash must be ORDER-INSENSITIVE

2. **Order-insensitive hashing and equality for Dict keys:** Tinct dicts have a stable insertion order for iteration, but user-facing equality `[=]` is order-insensitive: `[= [0: "a"  1: "b"] [1: "b"  0: "a"]]` = `True`. Hash must be consistent with this. Use a commutative combiner:

   ```text
   hash(Dict(pairs)) = Σ mix(hash(k), hash(v)) for each (k, v) pair
   ```

   where `mix` is a fixed bijection (e.g. splitmix64 round). Sum is commutative — insertion order does not affect it. Dict keys are unique, so pair-cancellation (the XOR weakness) does not arise. This is the same technique used by Python's `frozenset` and Rust's `AHashSet` for unordered-collection hashing.

   **Manual `PartialEq` for `HashableValue::Dict`:** `#[derive(PartialEq)]` compares `Vec`s lexicographically (order-sensitive). Since dict equality is order-insensitive, `PartialEq` must be implemented manually:

   ```rust
   impl PartialEq for HashableValue {
       fn eq(&self, other: &Self) -> bool {
           match (self, other) {
               (HashableValue::Int(a), HashableValue::Int(b)) => a == b,
               (HashableValue::Bool(a), HashableValue::Bool(b)) => a == b,
               (HashableValue::Dict(a), HashableValue::Dict(b)) => {
                   if a.len() != b.len() { return false; }
                   // Both sides into a HashMap, compare key-by-key order-insensitively
                   let map: HashMap<_, _> = a.iter().collect();
                   b.iter().all(|(k, v)| map.get(k).map_or(false, |u| u == v))
               }
               (HashableValue::Variant { tag: t1, payload: p1 },
                HashableValue::Variant { tag: t2, payload: p2 }) => t1 == t2 && p1 == p2,
               _ => false, // different variants or cross-type
           }
       }
   }
   ```

   `Eq` is derived from `PartialEq` (dict equality is reflexive, symmetric, transitive).

   **`Hash` for all variants** — `Hash` must be manually implemented for the full enum to keep it consistent with the manual `PartialEq`:
   - `Int(n)` → delegate to `i64::hash`
   - `Bool(b)` → delegate to `bool::hash`
   - `Dict(pairs)` → commutative sum (as above)
   - `Variant { tag, payload }` → `mix(hash(tag), hash(payload))` where `hash(None) = 0`, `hash(Some(v)) = hash(v)`

3. **Cross-type:** `HashableValue::Int(42)` and `HashableValue::Dict(...)` are never equal (different enum variants). The `_ => false` arm in the manual `PartialEq` enforces this. Hash discriminants naturally differ by variant. ✓

4. **Key materialisation:** Dict keys are always fully forced before insertion. `HashableValue` has no `ThunkId` — it is only constructible from materialised values. The evaluator enforces this at dict construction time by converting the key expression to `HashableValue` before lookup/insert. If a key expression evaluates to a non-hashable `Value` variant (Function, Handle, lazy Seq), this is a runtime error: "value cannot be used as a dict key."

**Computed key expressions in dict literals:**

Because any `Hashable` value is a valid dict key, the parser supports arbitrary expressions in key position. An expression is eagerly evaluated before insertion; its result must be `Hashable`:

```tinct
[[builtin-str "%" n]: val]    # Graphemes key from string call
[[+ base offset]: val]         # Int key from arithmetic
[["fixed-string"]: val]        # Graphemes key from literal (same as [fixed-string: val])
[$computed: val]               # variable reference — already valid
```

The key expression is delimited by `]:`  — the closing bracket of the expression followed by `:` unambiguously signals a computed key entry. The evaluator materialises the key before inserting the pair into the `IndexMap<HashableValue, ThunkId>`. This replaces the `[$k: val]` variable-reference trick and eliminates helper functions like `make-entry`.

**Migration from `Key` enum:**

Every use of `Key::Int(n)` becomes `HashableValue::Int(n)`. Every use of `Key::String(s)` becomes a `HashableValue::Dict(...)` representing the Graphemes for that string — the same conversion as text literal evaluation.

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

### `Boolean`

**Current:** `Bool` is a primitive Rust type variant with `Value::Bool(true/false)`.
**Proposed:** Removed. Replaced by a tinct algebraic type in the prelude:

```tinct
Boolean: [type True False]
True:    Boolean.True
False:   Boolean.False
```

`Value::Bool` is deleted. The `if` builtin dispatches on `Variant { tag: "Boolean.True" }` vs `Variant { tag: "Boolean.False" }`. Note: constructor auto-injection is rejected — `True`/`False` are introduced via explicit bindings only.
**Impact:** Breaking. All `true`/`false` literals become `True`/`False`. The existing incomplete tracker task for constructor auto-injection is rejected.

### `Seq` — Defined in Tinct

**Current:** `Value::Seq` is a Rust-implemented lazy cons-cell structure.
**Proposed:** `Seq` is defined as a recursive algebraic type in tinct. Laziness comes from tinct's default lazy evaluation — `tail` is a thunk automatically:

```tinct
Seq: [type [let a]  [Cons head@a  tail@[Seq a]]  End]
```

The Rust `Value::Seq` implementation is removed. The evaluator handles this like any other nominal variant type.
**Impact:** The current builtin `Seq` variant is replaced by a stdlib-defined recursive type.

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
gcb-cluster-end: [fn [let b@Bytes start@Int prev-prop state]
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

### Type System — De-primitisation

**Current:** `String`, `Int`, `Bool`, `Float`, `Bytes` etc. are distinct Rust enum variants in the `Type` enum. Type resolution has two separate paths: a primitive path (`resolve_type_name`, which pattern-matches on known names and returns the corresponding variant) and an alias path (`env.get_type_alias`). A hardcoded bypass list exempts primitive names from alias lookup, routing them to the primitive path unconditionally.

**Proposed:** All primitive type variants are removed from the `Type` enum. Every former primitive becomes either a TyConDef in the root type scope, a prelude typeclass, or a prelude type declaration — resolved through the same lookup mechanism as all other types. The bypass list shrinks to a single entry. The root scope is seeded at startup with TyConDefs for `Int`, `Float`, `Bytes` (and others); prelude declares `Bool`, `Seq`, `Number`, and `Never` as tinct types.

The bypass list entries, resolved:

- `"String"` — removed entirely; `@String` is a type error
- `"Bool"` — replaced by `Boolean: [type True False]` in prelude; `@Bool` removed
- `"Seq"` — replaced by `Seq: [type [let a] [Cons head@a tail@[Seq a]] End]` in prelude
- `"Number"` — replaced by `Number: [class [let Number n]]` in prelude with instances for `Int` and `Float`; `n@Number` is a constrained TypeVar
- `"Never"` — replaced by `Never: [type]` (empty type, no constructors) in prelude; `raise` returns `Never`
- `"Any"` — stays; maps to `Type::Top` (the sound lattice ceiling — `τ <: Any` for all τ); remains as the canonical user-facing name for "accepts any type"
- `"Top"` — removed; it is a redundant internal alias for `Type::Top` that leaked into the bypass list; users write `@Any`, not `@Top`
- `"Unknown"` — becomes a TyConDef entry like all others; its special behavior (consistency relation `~`) is enforced by `is_consistent`, not by name resolution; no reason to keep it in the bypass list
- `"Int"`, `"Float"`, `"Bytes"`, `"Handle"`, `"Dict"`, `"Map"`, `"Record"`, `"Fn"` — become TyConDef entries in the root scope, backed by Rust implementations but resolved through the env

This means:

- All types resolve through a single env lookup path — no separate primitive path
- `"String"` is removed from the root scope entirely; `@String` is a type error
- `@Number` becomes a typeclass constraint, not a primitive type
- `@Never` resolves to the prelude-declared empty type
- `@Unknown` remains compiler-handled (gradual typing escape hatch)

**Impact:** Fundamental to the type checker. Every match arm that dispatches on a primitive type variant in `typecheck.rs`, `type_unify.rs`, `typecheck_annot.rs`, `type_def.rs`, and `builtins_core.rs` must be updated to go through TyCon env lookup instead. **The bypass list is deleted entirely** — `resolve_type_name_with_guard` is removed. All type names including `Any` and `Unknown` resolve through the unified path. This is the correct foundation for a language where prelude and stdlib define what named types mean.

### Type Environment Bootstrap

The type system uses two distinct environments, constructed in sequence:

**Environment 1 — Type-stage env** (`Arc<RwLock<Environment>>`, thread-local): TypeNode thunks produced by evaluating `--- stage: type` sections. Supports annotation resolution and type-level programming.

**Environment 2 — TypeEnv** (`TypeEnv` struct, `Rc`-shared): TyConDef entries from `[type ...]` declarations, type schemes from function signatures. Supports type inference and checking.

These are separate structures. `[type ...]` declarations populate TypeEnv only. The type-stage env and TypeEnv are connected only through annotation resolution (see below).

**Full bootstrap-to-user-code sequence:**

| Step | Trigger | File | Section | What happens |
|------|---------|------|---------|--------------|
| 1 | Rust bootstrap | loader.llt | uses: | process `--- uses: ["core"]` → inject `builtin_module("core")` thunks as root type-stage env |
| 2 | Rust bootstrap | loader.llt | stage:type | evaluate stage:type docs → child type-stage env (currently empty; wired for future use) |
| 3 | Rust bootstrap | loader.llt | runtime | evaluate runtime sections → define eval-programs |
| 4 | eval-programs "prelude.llt" | prelude.llt | uses: | process `--- uses:` → inject any declared modules into child type-stage env |
| 5 | eval-programs "prelude.llt" | prelude.llt | stage:type | evaluate stage:type docs → TypeNode values, combinators, arithmetic resolvers |
| 6 | eval-programs "prelude.llt" | prelude.llt | runtime | evaluate runtime sections → prelude runtime bindings + TyConDefs active |
| 7 | eval-programs user-code | user file | uses: | process `--- uses:` → inject any declared modules |
| 8 | eval-programs user-code | user file | stage:type | evaluate stage:type docs → child of prelude type-stage env |
| 9 | eval-programs user-code | user file | runtime | evaluate runtime sections → % threading, emit, output |
| 10 | `[include ...]` | included file | uses: | process `--- uses:` → inject any declared modules |
| 11 | `[include ...]` | included file | stage:type | evaluate stage:type docs → child of current type-stage env |
| 12 | `[include ...]` | included file | runtime | evaluate runtime sections |

Every file boundary follows the same three-phase pattern: uses: → stage:type → runtime. Steps 10–12 repeat for each included file, recursively.

**Construction sequence — three phases at every file boundary:**

Every file processed by the pipeline goes through the same three phases in order: `--- uses:` → stage:type → runtime. The phases are described below for each file.

**Phase description:**

*`--- uses:` phase:* The file's `--- uses:` declaration is read. For each declared module, `builtin_module(name)` returns `Vec<BuiltinDef>`. Each `BuiltinDef` is wrapped as `Value::Builtin(def)` → `Arc<Thunk::new_materialized(Value::Builtin(def), Span::origin())>` and inserted into the current type-stage env by name. The type-stage env is `Arc<RwLock<Environment>>` where `Environment.bindings: IndexMap<String, Arc<Thunk>>`. After this phase the env contains the declared module's callable Rust functions.

*stage:type phase:* The file's `--- stage: type` documents are evaluated. Each expression item is evaluated **independently** against the current type-stage env (not chained as in normal document evaluation — independent evaluation ensures all dicts' bindings are exported, not just the last). Each expression returns a `Value::Dict`; its top-level bindings are inserted into the type-stage env as `Arc<Thunk>` values (lazily forced on first access). The resulting env becomes the **parent** for the next file's type-stage env — lexical parent-chain inheritance, same as runtime scope.

*runtime phase:* The file's runtime sections are type-checked (building TyConDef entries in TypeEnv from `[type ...]` declarations, class entries from `[class ...]`, type schemes from function annotations) and evaluated (making runtime bindings active).

**Per-file details:**

**loader.llt** (steps 1–3, Rust bootstrap):

- `--- uses: ["core"]` → inject `builtin_module("core")` thunks as the root type-stage env
- stage:type → currently empty; wired uniformly for future use
- runtime → evaluate: defines `eval-programs`

**prelude.llt** (steps 4–6, driven by `eval-programs "prelude.llt"`):

- `--- uses:` → inject any declared modules into a child of the loader type-stage env
- stage:type → produces: `Int`, `Float`, `Never`, `Any: [builtin-variant "TypeNode.Any"]`, `Unknown: [builtin-variant "TypeNode.Unknown"]`, type combinators (`union`, `all`, `without`, `Seq`, `Map`, `mu`), TypeNode ADT with traversal protocol (`children`, `map-children`, `as-type`), arithmetic resolvers (`AddResult`, `SubResult`, `MulResult`, `DivResult`)
- runtime → type-check registers TyConDefs (`Grapheme`, `Graphemes`, `Boolean`, `Seq`, `Absent`, `Never`, `Number` class, etc.) and constructor schemes (`Grapheme.Cluster`, `Boolean.True`, `Boolean.False`, `Seq.Cons`, `Seq.End`) in prelude TypeEnv; evaluate makes runtime bindings active. TypeEnv cached as `PRELUDE_CACHE`.

**user file** (steps 7–9, driven by `eval-programs user-code`):

- `--- uses:` → inject declared modules (e.g. `--- uses: ["text"]` injects text.llt following steps 10–12 for that file before continuing)
- stage:type → evaluated in a child of the prelude type-stage env; user type-stage bindings shadow prelude's; all prelude type-stage bindings accessible via parent chain
- runtime → type-check registers user TyConDefs in a child TypeEnv; evaluate produces output

**included file** (steps 10–12, driven by `[include ...]`):

- `--- uses:` → inject declared modules
- stage:type → evaluated in a child of the current type-stage env
- runtime → type-check and evaluate; exports merged into the including file's env

**Annotation resolution — the unified path:**

`@Name` in any annotation position resolves through a single two-step lookup:

1. **Type-stage env lookup:** evaluate `Name` as an expression in the type-stage env. If found, the result is a TypeNode thunk; `typenode_value_to_type` converts it to `Type::*`. This handles `@Int`, `@Float`, `@Any`, `@union`, and any name declared in a `--- stage: type` section.

2. **TypeEnv tycon_defs lookup (fallback):** if not found in the type-stage env, look up `Name` in `TypeEnv.tycon_defs`. If found, convert the TyConDef to `Type::TyCon("Name")`. This handles `@Grapheme`, `@Graphemes`, `@Boolean`, `@Absent`, `@Never`, and all user-declared types.

3. **Error:** if not found in either, type error: "type `Name` is not defined."

`@Unknown` resolves via the type-stage env like all other names — `Unknown: [builtin-variant "TypeNode.Unknown"]` in prelude's stage:type section. Its special behavior (consistency relation `~`) is enforced by `is_consistent`, not by how the name is resolved.

`@String` hits neither step: `String` has no type-stage entry and no TyConDef. Result: type error. Correct.

**loader.llt and the type system:**

The prelude type-stage env (step 1) and prelude TypeEnv (step 2) are built in Rust before loader.llt executes. This is necessary because type-checking requires the prelude type-stage env, and type-checking runs before user programs execute.

User `--- stage: type` sections are different: they are built on-demand per-file during type-checking of user programs, which is invoked by `eval-programs` (from loader.llt) as part of the normal program execution pipeline. `build_user_type_stage_env()` creates a child of the prelude type-stage env and evaluates the user's `--- stage: type` documents into it, including processing their `--- uses:` declarations. This happens after loader.llt has run.

The three `--- uses:` wiring points:

| Stage | Who wires it | When |
|-------|-------------|------|
| Prelude `--- stage: type` | `create_type_stage_env()` in Rust | Before loader.llt |
| Prelude runtime-stage | `eval-programs` from loader.llt | During prelude runtime evaluation |
| User `--- stage: type` | `build_user_type_stage_env()`, invoked by `eval-programs` | During user program type-checking |

**User `--- stage: type` sections:**

```tinct
--- stage: type
[
  NullableInt: [fn [] [union Int [TypeConstructor name: "Null"]]]
]
```

Evaluated in a child environment whose parent is the prelude type-stage env. `union`, `Int`, `TypeConstructor`, and all other prelude type-stage bindings are accessible via the parent chain — the same way prelude runtime bindings are accessible to user runtime code. The resulting child env is used for annotation resolution in that file.

### Unifying Annotation Resolution with the Type-Stage

**The architectural split.** Today there are two completely separate type name resolution paths:

**Path 1 — Annotation resolver** (`typecheck_annot.rs`): when the user writes `@Any`, `@Int`, `@Graphemes`, the resolver matches the name string against a hardcoded table (`resolve_type_name`) and returns a `Type::*` Rust enum value directly. No environment lookup occurs.

**Path 2 — Type-stage evaluator**: when the user writes `Any`, `Int`, or `Graphemes` inside a type expression body (e.g. `[type [List Grapheme]]`), the tinct evaluator looks up the name in the type-stage environment, finds a `TypeNode.*` variant, and `typenode_value_to_type` converts it to `Type::*`.

These two paths have diverged. The `Any` name is the clearest diagnostic: the annotation resolver maps `"Any"` → `Type::Top` (the sound supertype), while the type-stage prelude has `Any: [builtin-variant "TypeNode.Unknown"]` (the gradual type). Same name, different semantics depending on syntactic position. This divergence is a migration artifact — the `gradual-typing-split` sprint updated Path 1 but not Path 2.

**The correct architecture.** There should be one resolution path. When the user writes `@Any`, the annotation resolver should look up `Any` in the type-stage environment — the same lookup that Path 2 uses. The type-stage env is the canonical namespace for type names. The hardcoded table in `resolve_type_name` is the architectural flaw.

**Concrete deletions required:**

In `src/typecheck_annot.rs`:

- Delete the bypass list in `resolve_type_name_with_guard` (the long `match name { "Int" | "Float" | ... }` guard)
- Delete `resolve_type_name` (the ~70-line hardcoded match returning `Type::Int`, `Type::Str`, etc.)
- Replace with: look up the name in the type-stage env → if found, convert via `typenode_value_to_type` → if not found, type error

In `src/type_def.rs` (the `Type` enum):

- Delete `Type::Str` — `String` is gone; no replacement needed
- Delete `Type::Bool` — replaced by `Boolean` nominal variant
- Delete `Type::Number` — replaced by `Number` typeclass
- Delete `Type::Seq(Box<Type>)` — replaced by prelude-declared recursive `Seq` type
- Keep `Type::Int`, `Type::Float`, `Type::Bytes` as `Type::TyCon("Int")` etc. (TyConDef-backed)
- **Rename `Type::Top` → `Type::Any`** — the internal Rust name must match the user-facing name; `Type::Top` leaks through debug output, diagnostic messages, LSP hover text, and any code path that doesn't go through the user-facing display function; after the rename there is no mismatch
- Keep `Type::Unknown`, `Type::Never` — these are lattice positions, not nominal types

In `stdlib/prelude.llt` (the type-stage namespace):

- Fix `Any: [builtin-variant "TypeNode.Unknown"]` → `Any: [builtin-variant "TypeNode.Top"]`
- Add `TypeNode.Top` as a new Rust-backed type-stage variant (the sound supertype)
- Keep `Unknown: [builtin-variant "TypeNode.Unknown"]` as the explicit gradual-type name

In `src/builtins.rs` — the type-stage preinterning list (`build_type_stage_env`, line ~1720):

**Delete this list entirely.** The preinterning pre-materialises `TypeNode.*` thunks at startup to avoid forcing a prelude thunk on first access. That cost is nanoseconds — one `builtin-variant` call, memoised after first use. The "optimisation" is not worth its costs: a Rust-side list that must be manually kept in sync with the prelude, a source of naming conflicts (the `Any`/`Unknown` divergence arose because two definitions of the same name existed in two different places), and a third bypass mechanism alongside the two being deleted in `typecheck_annot.rs`.

After deletion, the prelude's lazy evaluation handles type-stage lookups naturally: first access forces the thunk (nanoseconds), all subsequent accesses hit the memoised result in O(1). The prelude is the single source of truth — no parallel Rust-side registration.

**After these deletions**, writing `@Any` and using `Any` in a type-stage expression both resolve through the same env lookup to the same `Type::Any` result. `Type::Top` is renamed `Type::Any` throughout the Rust codebase — the existing `Type::Top => "Any".to_string()` display conversion is deleted because the internal name already matches. The `Any` inconsistency disappears not by fixing the annotation resolver but by eliminating it — the type-stage env becomes the single source of truth for all type names.

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

### `List` — Type Alias for `[Map Int a]`

```tinct
List: [type [let a] [Map Int a]]   # transparent type alias; Int satisfies Hashable
```

**Future direction:** Once dict keys are typed by constraint rather than concrete type, `[Map k v]` where `k: Hashable` becomes the general form. `List a = [Map Int a]` and `Dict a = [Map Graphemes a]` are then special cases of the same type constructor — `List` where the key type is `Int` (satisfying `Hashable`). This unification depends on the general constraint annotation mechanism being in place.

`List a` is a type alias for `[Map Int a]` — an ordered, integer-keyed, homogeneous map. Keys need not be dense or start at zero; the only requirements are that keys are `Int` and all values are type `a`. Iteration is in insertion order (IndexMap semantics).

Auto-indexed literals `["a" "b" "c"]` produce `[List T]` = `[Map Int T]` with keys 0, 1, 2 in insertion order. `collect` produces the same. Operations that create gaps (like `set` with a new key) produce a valid `[List a]` with non-contiguous keys — this is fine.

**Positional vs key access:**

```tinct
# [get list k] — access by key value, O(1)
[get ["a" "b" "c"] 1]     # → "b"  (key 1)

# [nth n list] — access by insertion-order position, O(n)  — already in prelude
[nth 1 ["a" "b" "c"]]     # → "b"  (2nd inserted)
# nth is already in prelude; works on any Dict including [List a] = [Map Int a]
```

For dense 0..n-1 lists, `[get list n]` and `[nth n list]` are equivalent. They differ only for sparse or reordered maps.

**`reindex`: normalise to dense 0..n-1**

When operations produce gaps (e.g., inserting at non-contiguous keys), `reindex` compacts to dense 0..n-1 in insertion order:

```tinct
reindex: [fn@[bind: [a]  return: [List a]] [let m@[List a]]
  [collect [map [fn [let e] e.value] [entries m]]]]

# [reindex [3: "a"  7: "b"  -5: "c"]] → [0: "a"  1: "b"  2: "c"]
# Insertion order preserved, keys renumbered 0..n-1
```

`reindex` is proposed for the prelude. It composes from existing functions: `entries` yields key-value pairs in insertion order, `map` extracts values, `collect` materialises with fresh 0..n-1 keys.

### `Showable` Instances

Canonical `show` implementations for built-in types:

```tinct
[instance Showable [let t@Int]:
  [show: [fn [let n] [builtin-int-to-graphemes n]]]]       # "42", "-5"

[instance Showable [let t@Float]:
  [show: [fn [let f] [builtin-float-to-graphemes f]]]]     # "3.14", "1.0" (always includes .)

[instance Showable [let t@Boolean]:
  [show: [fn [let b] [match b  True: "True"  False: "False"]]]]

[instance Showable [let t@Graphemes]:
  [show: [fn [let gs] gs]]]                                # identity

[instance Showable [let t@Grapheme]:
  [show: [fn [let g] [$g]]]]                                       # one-element auto-indexed dict = [List Grapheme]

[instance Showable [let t@Bytes]:
  [show: [fn [let b]
    [join " " [map [fn [let byte] [builtin-byte-to-hex byte]] [each b]]]]]]
  # → "48 65 6c 6c 6f" (lowercase hex pairs, space-separated)

[instance@[bind: [a]  constraint: [a: Showable]] Showable [let t@[List a]]:
  [show: [fn [let xs]
    [join "" ["[" [join " " [map show xs]] "]"]]]]]

[instance@[bind: [k v]  constraint: [k: Showable  v: Showable]] Showable [let t@[Map k v]]:
  [show: [fn [let m]
    [join "" ["[" [join "  " [map [fn [let e] [print e.key ": " e.value]] [entries m]]] "]"]]]]]
```

`GraphemeStream` and `ByteStream` are NOT `Showable` — lazy potentially-infinite streams cannot be rendered to text. User-defined types implement `Showable` by declaring an instance.

`builtin-int-to-graphemes`, `builtin-float-to-graphemes`, and `builtin-byte-to-hex` are new Rust primitives. `builtin-byte-to-hex: Fn@Graphemes [UInt8]` returns a two-character lowercase hex pair (`"0a"`, `"ff"`).

## Runtime Typeclass Dispatch — Required Changes

### The Fatal Gap

User-defined typeclass instance method bodies are **never called at runtime today**. `[=]`, `[<]`, `[hash]`, `[show]` all dispatch via Rust builtins (`values_equal`, `builtin_lt`, etc.) that ignore user-defined instances entirely. Equatable/Hashable/Sortable/Showable instances are **type-checker annotations only** — they verify a matching instance exists at compile time, then are discarded. This must be fixed for this whatif to work.

### Type-checker Gap: Two Independent TypeVars for `[=]`

The current `=` signature uses the **same TypeVar** for both parameters: `[fn [let x@a y@a] ...]`. This means `[= (Int 42) (Graphemes "hello")]` fails unification (`Int ≠ Graphemes`) before constraint checking runs — the catch-all is never reached.

**Required change:** Change `=` and `<` to use **two independent constrained TypeVars**:

```tinct
# Current (wrong — same TypeVar, cross-type fails at unification):
=: [fn@Boolean [let x@a y@a] ...]

# Required (correct — independent TypeVars, cross-type reaches catch-all):
=: [fn@Boolean [let x@a y@b  constraint: [a: Equatable  b: Equatable]] ...]
<: [fn@Boolean [let x@a y@b  constraint: [a: Sortable   b: Sortable]]  ...]
```

With independent TypeVars, `[= (Int 42) (Graphemes "hello")]`:

1. `a = Int`, `b = Graphemes` — unification succeeds (different vars, no conflict)
2. Constraint check: `Equatable Int` ✓, `Equatable Graphemes` ✓
3. `resolve_instance` tries concrete instances: no `Equatable k` where k = both Int and Graphemes
4. Finds catch-all `[fn [let a@Equatable b@Equatable] False]` — matches
5. Returns `False`

`resolve_instance` already scores by specificity (fewer unresolved TypeVars wins). The concrete `Equatable Int` instance scores 0 (no unresolved vars for Int×Int); the catch-all scores 2 (two unresolved vars). Concrete always wins for same-type; catch-all wins for cross-type. **No new machinery needed on the type-checker side beyond the TypeVar signature change.**

### Runtime Gap: Instance Bindings in the Environment

The evaluator must execute user-defined typeclass instance method bodies. The mechanism leverages **tinct's existing environment chain** — the same lexical scope mechanism used for all other bindings.

**Instances are bindings in the environment:**

When `[instance Equatable [let k@Int]: [=: [fn [let a@Int b@Int] ...]]]` is evaluated, it registers the `=` implementation as a **multi-valued binding** in the current environment — an ordered list of `(type-constraints, body)` pairs alongside the name `=`. Multiple instances contribute multiple entries to the same name.

This is the correct scoping model:

- Instances follow **lexical scope** automatically — the existing env chain handles shadowing and module isolation
- A locally-defined instance in user code shadows the prelude instance within its scope
- Different modules can have different instances for the same type without conflict
- No global table, no separate instance registry — just the environment

**Method dispatch at runtime:**

When `[= a b]` is evaluated:

1. Look up `=` in the environment chain — finds the ordered list of `(type-constraints, body)` pairs
2. For each entry, check if the runtime types of `a` and `b` match the type constraints (using `HashableValue` variant tags for Hashable types, or evaluating the constraint predicate)
3. Pick the **most specific** matching entry — same specificity scoring as `resolve_instance`: fewer unconstrained type positions = more specific
4. Execute its body with `a` and `b` as arguments
5. If no entry matches → type error

**Example — `[= (Int 42) (Graphemes "hello")]`:**

1. Env lookup for `=` → ordered list: `[Int×Int body, Graphemes×Graphemes body, catch-all body]`
2. `Int×Int`: a is Int ✓, b must be Int — b is Graphemes ✗
3. `Graphemes×Graphemes`: a must be Graphemes — a is Int ✗
4. `catch-all @Equatable×@Equatable`: a is Int (Equatable ✓), b is Graphemes (Equatable ✓) → **match**
5. Execute catch-all body → `False`

**Changes required:**

- `src/eval.rs` / `eval_dict.rs` — `[instance ...]` declarations populate the environment's multi-valued binding for the method name, ordered by specificity
- `src/eval_call.rs` — method call dispatch searches the multi-valued binding list for the most specific matching entry, executes its body
- `src/environment.rs` (or equivalent) — environment must support multi-valued bindings (ordered list of implementations) for method names alongside single-valued bindings for regular names
- `src/builtins_math.rs` — `builtin_eq`, `builtin_lt` become the implementations called by the prelude `Equatable` instances (not called directly by the evaluator — the evaluator now calls instance bodies, which call the builtins)
- `stdlib/prelude.llt` — `[instance Equatable ...]` bodies are now genuinely executed at runtime

**Impact:** All typeclass methods (`=`, `<`, `hash`, `show`, `compare`) become genuinely user-extensible and properly scoped. The Equatable, Hashable, Sortable, Showable instances specified in this whatif become real runtime behaviors. Instance scoping follows lexical scope naturally — no separate coherence enforcement needed.

## Implementation Notes

### `builtin-eval` Return Type

`builtin-eval` currently has `ret: Box::new(Type::Unknown)` in `src/imports.rs`. This is wrong: `Type::Unknown` is the gradual type that propagates via the consistency relation `~` and disables type checking downstream. The correct return type is not a fixed annotation at all — it is inferred from the final expression of the evaluated sequence, the same way the type checker infers the return type of any tinct function body or document.

The return type of `builtin-eval doc.expressions` is the type of the last dict expression in `doc.expressions` — the same type as `%` from that document, the same as the inferred return type of an equivalent function. The three-phase pipeline ensures the type checker has already traversed those expressions before `builtin-eval` is called on them, so the type is always known.

**Fix:** remove the hardcoded `Type::Unknown` return type. The type checker infers `builtin-eval`'s return type at each call site from the expressions argument, exactly as it infers the return type of any expression. The fallback for genuinely unknown expressions is `Type::Any` (the sound top type), not `Type::Unknown`.

This fix makes `include` return the correct type — the exported dict of the included file — without any special-casing of `include` in the type checker. The type flows naturally: `builtin-eval` → `eval-document-runtime` → `eval-document-pipeline` → `eval-file` → `include`.

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
