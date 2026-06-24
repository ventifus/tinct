# What If: Type System Foundations — Primitives, Collections, and Dispatch

**State:** Proposal

These architectural changes address the general collection type hierarchy, typeclass dispatch, runtime annotation machinery, and bootstrap de-special-casing.

The `doc/whatif/type-foundations/generator/` directory contains a tinct program that reads these typed declarations and generates Rust `TypeScheme` / `TyConDef` registrations. The declaration IS the spec; the Rust output is derived from it.

## The Single Rust Primitive

**Rust knows about exactly one collection type: `Value::Dict(IndexMap<HashableValue, ThunkId>)`.**

`Map`, `List`, `Record`, `Seq`, `Graphemes`, `Bytes` — none of these exist in Rust. They are all names declared in `stdlib/prelude.llt` as tinct `[type ...]` declarations. The Rust evaluator is blind to them. A user could write a completely prelude-free tinct program using only raw `Value::Dict` values, define their own sequence type with a different name and different semantics, and it would work. A user could replace the entire prelude with different collection names and the runtime would not care.

The implication: there is no "type-level vs runtime" split for `Map`, `List`, `Seq`, etc. There is ONE thing: the `[type ...]` declaration in prelude.llt. The type checker learns about `List` by processing that declaration. The runtime produces `Value::Variant` or `Value::Dict` values as appropriate. No separate Rust-side special-casing is needed or permitted.

## Goals

3. **`Map`, `List`, `Seq`, and `Record` are purely prelude-level.** The only Rust primitive collection is `Value::Dict`. Everything else is a `[type ...]` declaration in prelude that the user could freely replace or omit. The `k: Hashable` constraint on dict keys is universal — any type satisfying `Hashable` can be a dict key.

5. **Remove prelude special cases.** Prelude is loaded by `loader.llt` and is user code. Its type-checking must use the same pipeline as any other file — no Rust-level bootstrap special-casing, no silently discarded type errors.

## Key Constraints — Equatable, Hashable, Sortable

**Typeclass design principle: typeclasses are complexity promises, not possibility predicates.** If an operation is a typeclass method, it is O(log n) or better. Operations that require O(n) traversal (map, filter, reduce, collect, length of Seq) are plain prelude functions, not typeclass methods. You can "search for an element" in a Seq but that doesn't make it `Indexable`.

### Full typeclass set

| Typeclass | Complexity guarantee | Notes |
|-----------|---------------------|-------|
| `Prependable` | O(1) amortized push-left, peek-left, pop-left | |
| `Appendable` | O(1) amortized push-right, peek-right, pop-right | |
| `Concatenable` | O(log n) or better join of two collections | renamed from `Semigroup` for the collection hierarchy |
| `Indexable` | O(log n) or better keyed get + O(1) length | |
| `Hashable` | O(1) hash consistent with `=` | |
| `Equatable` | `=` | already exists |
| `Sortable` | `<` | already exists |
| `Printable` | `display` | already exists |

**Not typeclasses** (O(n), live as plain prelude functions): `map`, `filter`, `reduce`/`foldMap`, `collect`, `sort`, `concat` on Seq, `length` of Seq.

Dict keys need `Hash + Eq` (IndexMap implementation). `Equatable` and `Comparable` are declared in prelude and fully implemented — they provide `=` and `<` for all primitive types. `Hashable` and `Sortable` extend them and are not yet declared:

```tinct
# ── Typeclasses ───────────────────────────────────────────────────────────────

# Equatable and Comparable are implemented in prelude (shown here as context).
# Hashable and Sortable are NOT yet declared.

[class [let Hashable k]  [Equatable k]    # Hashable implies Equatable
  hash: [Fn@Int [k]]]                     # consistent with =: [= a b] → [= [hash a] [hash b]]

# Sortable does NOT imply Equatable. Float is Sortable (< is defined for floats)
# but NOT Equatable — NaN != NaN breaks the reflexivity law [= a a] = True.
# Sortable and Equatable are orthogonal typeclasses. A type that needs both
# declares both independently.
[class [let Sortable k]
  <: [Fn@Boolean [k k]]]
```

**`[Map k v]` requires `k: Hashable`.** The `Hashable` typeclass is not yet declared, which means the `Key { Int, String }` enum has not yet been replaced with the `HashableValue` approach. When `Hashable` is declared, these instances are needed:

```tinct
[instance Hashable [let k@Int]:
  [=:    [fn [let a@Int b@Int] [builtin-int-eq a b]]
   hash: [fn [let a@Int]       a]]]              # Int IS its own hash — the integer value itself


# Boolean: no pair-match — use nested match for two-value dispatch.
# Patterns must use qualified names Boolean.True / Boolean.False.
[instance Hashable [let k@Boolean]:
  [=:    [fn [let a b]
           [match a
             Boolean.True:  [match b  Boolean.True: true   _: false]
             Boolean.False: [match b  Boolean.False: true  _: false]]]
   hash: [fn [let a] [match a  Boolean.True: 1  Boolean.False: 0]]]]

[instance Sortable [let k@Int]:
  [<: [fn [let a b] [builtin-lt a b]]]]

[instance Sortable [let k@Float]:
  [<: [fn [let a b] [builtin-lt a b]]]]   # NaN behavior follows IEEE 754 — no Equatable

```

`UInt8` similarly for `Hashable` and `Sortable`. **`Float` is `Sortable` but NOT `Equatable` and NOT `Hashable`** — IEEE 754 `NaN != NaN` violates the reflexivity law (`[= a a]` must be `True`), and `+0.0 == -0.0` would require equal hashes for different bit patterns. `Float` therefore has no `Equatable` or `Hashable` instance, no `HashableValue::Float` variant, and cannot be used as a dict key. `[= a b]` on floats is a type error; use `[builtin-float-eq a b]` with explicit NaN awareness when needed. When `[= a b]` is called, the type class dispatcher looks up `=` in the environment and selects the most specific matching `Equatable` instance for the argument types. If no concrete instance matches (cross-type or non-Equatable), the catch-all fires or a type error is raised — `[=]` is the dispatch target, not the dispatcher.

**Sortable and Equatable are orthogonal.** At runtime, `[=]` and `[<]` look up separate multi-valued bindings in the environment — there is no superclass inheritance. A type may have `Sortable` without `Equatable` (e.g. `Float`), `Equatable` without `Sortable` (e.g. `Boolean` — booleans have no natural ordering), or both (e.g. `Int`, `Graphemes`).

## `Dict` is the Sole Runtime Collection Primitive

`Value::Dict(IndexMap<HashableValue, ThunkId>)` is the only collection type the Rust runtime knows about. `Seq`, `Map`, `List`, `Record` are tinct abstractions built on top of it — they exist in `stdlib/prelude.llt`, not in Rust. The Rust evaluator is entirely blind to these names.

This is not a cosmetic distinction. It means:
- Adding `Seq`-specific handling to Rust would be a design violation
- Optimizing for `List` in Rust would be a design violation  
- Any pattern-matching on collection type names in Rust (other than `Dict`) is a violation of the architecture

`[Map k v]` is a prelude type constructor. When a `[Map Int Str]` value is evaluated, the runtime produces `Value::Dict` with `HashableValue::Int` keys. Rust does not know or care that the tinct code called it `Map` — it just sees a dict.

```tinct
# These declarations are in stdlib/prelude.llt.
# Rust knows nothing about these names. A user could replace them entirely.

Map:    [type [let k@Hashable v] ...]       # homogeneous keyed collection
List:   [type [let a] [FingerTree a]]       # lazy 2-3 finger tree; NOT [Map Int a]
Seq:    [type [let a] [Cons head: a  tail: [Seq a]]  End]  # lazy recursive cons-list
```

`Seq` is a nominal ADT declared in tinct. Its values are `Value::Variant { tag: "Seq.Cons", ... }` / `Seq.End` — the same representation as any other user-declared nominal type. `head`, `tail`, `map`, `filter`, `reduce` over sequences are tinct-defined functions, not Rust builtins:

```tinct
head: [fn [let s@[Seq a]] [match s  [Seq.Cons p]: p.head  Seq.End: [raise "head: empty"]]]
tail: [fn [let s@[Seq a]] [match s  [Seq.Cons p]: p.tail  Seq.End: [raise "tail: empty"]]]
map:  [fn [let f s@[Seq a]]   # cons-recursive — lazy, safe on infinite Seqs
  [match s
    [Seq.Cons p]: [Seq.Cons head: [f p.head]  tail: [map f p.tail]]
    Seq.End: Seq.End]]
```

`Record` is not a Rust type at all — it is the ordinary `Value::Dict` value produced by dict literals. The name "Record" appears only in the type checker's structural type expressions; at runtime there is just a dict.

`Dict` (as a prelude name) is `[Map String Any]` — a dict whose string keys map to heterogeneous values. `HashableValue::Str(Rc<str>)` replaces the old `Key::String(Rc<str>)` as the Rust representation.

The `k: Hashable` constraint is the universal rule: anything satisfying `Hashable` can be a dict key. This is enforced by the type checker (via the `Hashable` typeclass) and by the `HashableValue` Rust enum. Adding a new hashable key type means adding a `Hashable` instance in prelude — no Rust changes.

`List` is `Indexable` via `ft-index` (O(log n), using size annotations). `[Seq T]` is NOT `Indexable` — it has no `Indexable` instance; `[get 0 seq]` is a compile-time type error. Types with `Indexable` instances:

| Type | Key | Access | Notes |
|------|-----|--------|-------|
| `List a` (finger tree) | `Int` | O(log n) | produced by `collect` |
| `Map k v` / `Dict` | `Hashable k` | O(1) hash | named/keyed dicts |

## Collection Typeclass Hierarchy

The `-able` suffix on every collection typeclass is intentional: it names a computational promise, not a category membership. A type that implements `Prependable` promises O(1) amortized prepend — not merely "prepending is possible in some sense."

### Instance table

| Type | Prependable | Appendable | Concatenable | Indexable |
|------|-------------|------------|--------------|-----------|
| `Seq a` | ✓ O(1) exact | — | — | — |
| `List a` (finger tree) | ✓ O(1) amortized | ✓ O(1) amortized | ✓ O(log n) | ✓ O(log n) |
| `Dict` / `Map k v` | — | — | — | ✓ O(1) hash |

`Dict` has **no** `Concatenable` instance. `merge` (right-biased union) is O(n) — it violates the O(log n) complexity promise that `Concatenable` makes. Users who need to merge two dicts call `merge` explicitly. The `<>` operator is not overloaded for `Dict`.

### Deque — no separate typeclass

A **deque** (double-ended queue) is any type that is BOTH `Prependable` AND `Appendable`. No separate `Dequeable` class exists — the combination of the two typeclasses already expresses the contract precisely. `List` is a deque. `Seq` is not (it is `Prependable` only).

```
Seq a       — Prependable only
List a      — Prependable + Appendable + Concatenable + Indexable  (deque)
Dict        — Concatenable + Indexable
```

### Why `Seq` is `Prependable` but not `Appendable`

`Seq` is a lazy cons-list. `push-left` (cons) is O(1) exact — it allocates one `Seq.Cons` node and suspends the tail as a thunk. `push-right` (append) requires traversing the entire spine to find the end, which is O(n) and also forces the entire lazy structure. An O(n) operation cannot be a typeclass method under the complexity-promise principle. Callers who need O(1) append should use `List` (finger tree).

`[Seq T]` — including `GraphemeStream` and `ByteStream` — has no `Indexable` instance. `[get gs 4]` on a `GraphemeStream` is a type error. Two patterns for positional access on streams:

## `collect`

```tinct
collect: [fn [let s@[Seq a]]
  [reduce [fn [let tree x] [push-right x tree]] FingerTree.FTEmpty s]]
```

`collect` materializes a lazy `[Seq a]` into a `[List a]` (finger tree) via successive `push-right` calls. Cost: O(n) — each `push-right` is O(1) amortized. Post-collect, `length` is O(1) and `get i` is O(log n).

Access patterns:

| Pattern | How | Time | Memory |
|---------|-----|------|--------|
| Sequential | `head`, `tail`, `map`, `filter` | O(1) per step, lazy | O(1) |
| Single element at position n | `[head [drop n s]]` | O(n) | O(1) — no materialisation |
| Many accesses at different positions | `[ls: [collect s]]` then `[get ls i]` | O(n) once + O(1) each | O(n) |

For `GraphemeStream` specifically: `collect` on `[each "hello"]` produces `[0: Grapheme.Cluster{code-points:[104]} ...]` — each value is a `Grapheme` variant (single code point per ASCII character).

## Value-Keyed Dict — Eliminating the `Key` Enum

**Current Rust:** `Value::Dict(IndexMap<Key, ThunkId>)` where `Key` is:

```rust
enum Key { Int(i64), String(Rc<str>) }
```

**Problem:** `Key::String` only allows strings and integers as dict keys. The correct design: any type satisfying `Hashable` can be a dict key. This unlocks nominal types (e.g. `Boolean.True`/`Boolean.False`) as keys without special-casing.

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

`Key::Int(n)` → `HashableValue::Int(n)`. `Key::String(s)` → `HashableValue::Str(s)`. `HashableValue::Variant` is new: enables `Boolean.True`/`Boolean.False` and other nominal types as dict keys.

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

## Runtime Type Annotations for Nominal Structural Types

`Grapheme`, `Graphemes`, `Bytes`, and other nominal types defined as `[type SomeStructure]` are transparent to generic operations — their runtime value is a plain `Value::Dict`. Every value of a nominal structural type carries `[type: TheType]` in its annotation dict, making it introspectable:

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

**All serializers** (SCN, JSON, YAML, and any future format) dispatch on `annotation-of` to determine output format. This is the universal rule: before serializing any value, check its annotation:

- `[type: Graphemes]` → serialize as a text string in the target format, encoding the grapheme clusters using the format's required encoding (UTF-8 for JSON/YAML/SCN, or whichever codec the format specifies)
- `[type: Bytes]` → format-appropriate byte representation (hex, base64, raw, etc.)
- anything else → structural output per the serializer's normal rules

JSON example: a `Value::Annotated` with `[type: Graphemes]` serializes as a JSON string `"hello"`, not as a nested array of arrays of ints. YAML similarly. The annotation is the contract between the runtime value and any serializer — the serializer is responsible for applying the correct codec for its output encoding.

## What Would Change

### `Boolean`

**Current:** `Bool` is a primitive Rust type variant with `Value::Bool(true/false)`.
**Proposed:** Removed. Replaced by a tinct algebraic type declared in loader.llt (available to all user programs via prelude scope injection):

```tinct
Boolean: [type True False]
```

`Value::Bool` is deleted. `Token::BoolLit`, `SurfaceExpression::Bool`, `CoreExpr::Bool`, and `LiteralPattern::Bool` are all deleted — the lowercase `true`/`false` literals had no purpose beyond serving `Value::Bool`. Tinct constructors are uppercase.

**Both value position and pattern position always use `Boolean.True`/`Boolean.False`.** Prelude does NOT export short aliases `True`/`False` — this avoids any accidental shortcut that would blur the distinction between constructor names and variable captures in patterns. Pattern matching is not scope resolution; bare uppercase identifiers in pattern position would not automatically resolve through prelude bindings anyway. The qualified form is the only form.

```tinct
x: Boolean.True              # value position — qualified
[if Boolean.True "yes" "no"] # value position — qualified

[match x
  Boolean.True:  "yes"       # pattern position — qualified
  Boolean.False: "no"]       # pattern position — qualified
```

`builtin-if` dispatches on `Variant { tag: "Boolean.True" }` vs anything else.

### `Seq` — Defined in Tinct

**Current:** `Value::Seq` is a Rust-implemented lazy cons-cell structure.
**Proposed:** `Seq` is defined as a recursive algebraic type in tinct. Laziness comes from tinct's default lazy evaluation — `tail` is a thunk automatically:

```tinct
Seq: [type [let a]  [Cons head@a  tail@[Seq a]]  End]
```

The Rust `Value::Seq` implementation is removed. The evaluator handles this like any other nominal variant type.
**Impact:** The current builtin `Seq` variant is replaced by a stdlib-defined recursive type.

### Collection Typeclasses — Complexity Promises, not Possibility Predicates

**Current:** The prelude has `Appendable` (heterogeneous element-by-element append), `Semigroup` (same-type combine), and `Builder` (folded construction). `map`, `filter`, `reduce`, and `collect` are sometimes discussed as typeclass candidates.

**Proposed:** The collection typeclass hierarchy is defined exclusively by **O(log n) or better** complexity guarantees. The typeclasses are:

- `Prependable` — O(1) amortized: `push-left`, `peek-left`, `pop-left`
- `Appendable` — O(1) amortized: `push-right`, `peek-right`, `pop-right`
- `Concatenable` — O(log n) or better: `<>` (join two collections of the same type)
- `Indexable` — O(log n) get + O(1) length

`Concatenable` replaces `Semigroup` as the name for same-type collection joining. `Semigroup` may still be the appropriate name for other algebraic uses (e.g., non-collection monoids), but for the collection hierarchy the `-able` convention names a computational promise, not a category-theory concept.

**O(n) operations are NOT typeclasses.** `map`, `filter`, `reduce`, `collect`, `sort`, `concat` on `Seq`, and `length` of `Seq` remain plain prelude functions. Making them typeclasses would imply an O(log n) bound that cannot be delivered.

**`Foldable`, `Traversable`, `Filterable`, and `Buildable` are NOT declared as typeclasses in this design.** These are O(n) operations by nature — a `Foldable` traversal requires visiting every element. They live as prelude functions (`reduce`, `map`, `filter`, `collect`) with concrete type signatures. The Haskell-style `Foldable`/`Traversable` hierarchy conflates "a traversal is possible" with "a traversal is cheap" — tinct's complexity-promise principle rejects this conflation.

**Impact:** Any existing `Semigroup`, `Foldable`, `Traversable`, or `Filterable` typeclass declarations in the prelude are replaced by plain function definitions. The dispatch mechanism for `map`, `filter`, and `reduce` is based on argument type (Seq vs List vs Dict), not typeclass resolution.

### Type System — De-primitisation

**Current:** `String`, `Int`, `Bool`, `Float`, `Bytes` etc. are distinct Rust enum variants in the `Type` enum. Type resolution has two separate paths: a primitive path (`resolve_type_name`, which pattern-matches on known names and returns the corresponding variant) and an alias path (`env.get_type_alias`). A hardcoded bypass list exempts primitive names from alias lookup, routing them to the primitive path unconditionally.

**Proposed:** All primitive type variants are removed from the `Type` enum. Every former primitive becomes either a TyConDef in the root type scope, a prelude typeclass, or a prelude type declaration — resolved through the same lookup mechanism as all other types. The bypass list shrinks to a single entry. The root scope is seeded at startup with TyConDefs for `Int`, `Float`, `Bytes` (and others); prelude declares `Bool`, `Seq`, `Number`, and `Never` as tinct types.

The bypass list entries, resolved:

- `"String"` — becomes a TyConDef entry in the root scope backed by `Type::Str`, same as `Int`, `Float`, etc. `@String` remains valid. Removal is deferred to the string-redesign whatif, which lands after type-foundations.
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
- `"String"` resolves through the TyConDef path to `Type::Str` — `@String` remains valid
- `@Number` becomes a typeclass constraint, not a primitive type
- `@Never` resolves to the prelude-declared empty type
- `@Unknown` remains compiler-handled (gradual typing escape hatch)

**Impact:** Fundamental to the type checker. Every match arm that dispatches on a primitive type variant in `typecheck.rs`, `type_unify.rs`, `typecheck_annot.rs`, `type_def.rs`, and `builtins_core.rs` must be updated to go through TyCon env lookup instead. **The bypass list is deleted entirely** — `resolve_type_name_with_guard` is removed. All type names including `Any` and `Unknown` resolve through the unified path. This is the correct foundation for a language where prelude and stdlib define what named types mean.

### Runtime Bootstrap Refactoring

**End state:** Rust evaluates `loader.llt` once. Everything else — prelude loading, type-checking, user file evaluation, output formatting — is tinct orchestrating tinct. Prelude is not privileged. Type-checking happens when tinct requests it, not when Rust decides to. Output emission is a side effect of pipeline evaluation, not a Rust serialization step.

### CLI Initial Environment Construction

The CLI constructs the initial dictionary environment before loader.llt runs. This environment is as though Rust had evaluated a tinct dict — its entries are accessible as bare names throughout loader.llt and captured by all closures. No named reference to the whole dict is needed; the entries simply are in scope.

The `%` prefix on names is convention for "came from the runtime," not a type marker.

**Construction sequence (order is security-critical):**

1. **Parse argv** — extract file paths, `-e` expressions, `--output`, `--strict`, `--cap name=path` flags
2. **Determine %cwd** — `getcwd()`, the working directory the CLI was invoked from; fixed for the lifetime of execution
3. **Locate %libdir** — the stdlib directory (next to the tinct binary, or `TINCT_STDLIB` env var); must be resolved to an absolute path before sandboxing
4. **Open %stdout** — the stdout write handle
5. **Open %stdin if `-i`/`--input`** — stdin as a readable handle
6. **Open all positional file handles** — each `file.llt` argument opened with `O_RDONLY` and its absolute path recorded; must happen **before sandboxing** because files may be outside `%cwd`
7. **Open all `--cap name=path` directories** — each `--cap` flag opens a `DirCap` to the specified path; again pre-sandbox
8. **Apply security sandbox** — Landlock file-access restriction limits future `open()` calls to within `%cwd` and `%libdir`; previously opened handles remain valid
9. **Build `%programs`** — integer-keyed Dict of `ProgramItem` values, one per file/expression, in CLI argument order
10. **Build `%args`** — Dict of parsed flags
11. **Assemble the initial dict** — all of the above merged into one dict
12. **Evaluate `stdlib/loader.llt`** — with the initial dict as the initial scope; all output happens via side effects; Rust exits when evaluation completes

**Initial dict contents (CLI invocation):**

| Entry | Type | Value |
|-------|------|-------|
| `%cwd` | DirCap | Working directory; ReadWrite; fixed |
| `%libdir` | DirCap | Stdlib directory; ReadOnly |
| `%stdout` | Handle | Writable stdout handle |
| `%stdin` | Handle | Readable stdin handle (only if `-i`/`--input` flag given) |
| `%programs` | Dict | Integer-keyed ProgramItem values (see below) |
| `%args` | Dict | Parsed CLI flags (see below) |
| `%name` | DirCap | One entry per `--cap name=/path` flag; ReadWrite unless `--cap-ro` specified |

No `%type-ctx` — TypeContext is accessed via `[builtin-get-type-context]`.

**`%programs` structure:**

Integer-keyed Dict, 0-indexed, preserving CLI argument order. Each value is a `ProgramItem` variant (the ADT is declared in loader.llt; Rust uses the qualified tag names as a bootstrapping contract):

```
%programs = {
  0: ProgramItem.File { path: "/abs/path/file1.llt", handle: <ReadableHandle> }
  1: ProgramItem.Expr { src: "[ + 1 2 ]" }
  2: ProgramItem.File { path: "/abs/path/file2.llt", handle: <ReadableHandle> }
}
```

- **File** — `path` is the absolute path (for error reporting and `%include-dir` computation); `handle` was opened pre-sandbox
- **Expr** — `src` is the raw source string from the `-e` flag; no handle needed

**`%args` structure:**

```
%args = {
  output: "json"    # output formatter name; matches stdlib/cli/out/<name>.llt
  strict: false     # abort on type errors rather than warning
}
```

`output` defaults to `"json"`. Valid values are the filenames in `stdlib/cli/out/` without the `.llt` extension. Additional flags may be added; loader.llt accesses `%args` via `[builtin-get "output" %args]` etc.

**User capability flags (`--cap name=path`):**

```
tinct run file.llt --cap data=/var/myapp/data --cap config=/etc/myapp
```

Produces `%data` and `%config` as DirCap entries in the initial dict. The name after `--cap` becomes the `%name` variable. Paths are resolved to absolute before sandboxing. User programs access these as `%data`, `%config` etc. — bare names in scope, same as `%cwd`.

**What Rust does NOT do:**

- Does not load or parse prelude — that is loader.llt's job
- Does not type-check any file — that is builtin-typecheck's job, called by loader.llt
- Does not serialize output — that is the output formatter's job, last in the pipeline
- Does not create a TypeContext capability — TypeContext is accessed implicitly via `builtin-get-type-context`

**`%programs` is an integer-keyed `Dict` of `ProgramItem` values (built pre-sandbox).** `ProgramItem` is declared in `loader.llt` — Rust uses the same qualified tag names as a bootstrapping contract:
- `[ProgramItem.File path: "/abs/path.llt"  handle: h]` — file opened by Rust; path included for error reporting and %include-dir computation
- `[ProgramItem.Expr src: "[ + 1 2 ]"]` — from `-e` flag

**`loader.llt` is the tinct "main function"** (`doc/whatif/type-foundations/loader.llt`).

A single document with three consecutive dicts (no `---` separators). The full implementation is in the whatif directory. Key design points:

- Dict 1: `read-handle` — private helper using only core builtins
- Dict 2: `Boolean`, `ProgramItem`, `DocName`, `include`, `expand`, `eval-document-runtime`, `eval-pipeline-item`, `eval-file`, `eval-expr`, `cli-pipeline` — all closures capture the initial environment (`%cwd`, `%libdir`, `%stdout`, `%args`) as bare names
- Dict 3: loads `_prelude`, creates `emit-ch`, constructs the `formatter` ProgramItem, calls `cli-pipeline`

The formatter is passed as a separate parameter to `cli-pipeline` (not appended to `%programs`), so `cli-pipeline` runs all user programs first via `builtin-reduce`, then runs the formatter last. The formatter materializes `%` and drains `%emit-channel`, driving the evaluation cascade.

```tinct
# Abbreviated — see doc/whatif/type-foundations/loader.llt for full version

cli-pipeline: [fn [let programs formatter initial include-dir prelude emit-ch]
  [result: [builtin-reduce
    [fn [let prev item] [eval-pipeline-item item prev include-dir prelude emit-ch]]
    initial
    programs]]
  [eval-pipeline-item formatter result include-dir prelude emit-ch]]
```

`%cwd` is used as the initial `include-dir`; `eval-file` overrides it per-file with `[builtin-path-dir item.path]` so includes resolve relative to each file's own directory.

**The output formatter drives the materialization cascade.**

The pipeline is lazy. Nothing evaluates until something forces it. The output formatter is that forcing agent — it is what causes the entire pipeline to materialize by accessing two things:

1. **`%`** — the final user program's output value. Accessing this forces the last user program to evaluate, which forced the previous program (it needed `%` as input), which forced the one before it, cascading all the way back through the pipeline. User programs execute in strict left-to-right order because each `prev` must be forced before it can be passed to the next `eval-pipeline-item`.

2. **`%emit-channel`** — the shared channel. Draining the channel forces all lazy `emit` thunks in user programs that haven't yet been forced. Any `[emit v]` call that the user put in an auto-indexed dict entry (lazy) is only forced when the formatter drains the channel.

Without the formatter, the entire pipeline stays inert. This is intentional: a tinct program with no output formatter simply does nothing. The formatter is not passive bookkeeping — it is the engine.

**`%emit-channel` threading — how it flows through the pipeline:**

`emit-ch` is created once in loader.llt dict 3 and threaded through the entire pipeline. Every call to `eval-document-runtime` injects it into the user document's scope as `%emit-channel`:

```tinct
scope: [builtin-merge
  prelude
  [builtin-merge
    [%cwd: %cwd  %stdout: %stdout  %args: %args  ...   # initial env (closure)
     %include-dir:  include-dir
     emit:          [fn [let v] [builtin-send emit-ch v]]
     %emit-channel: emit-ch]   # same channel for ALL programs in the pipeline
    ...]]
```

User code calls `[emit v]` which sends to `emit-ch`. The formatter (the last `ProgramItem.File` run by `cli-pipeline`) receives `%emit-channel` in its scope and drains it:

```tinct
# json.llt — drains %emit-channel, writes JSON to %stdout
[result: [recv-all %emit-channel]]
[write %stdout [to-json result]]
```

The formatter runs after all user programs have completed (it receives the final `%`). By then, all synchronous `emit` calls have already been sent — draining the channel collects them all in order.

**Two new Rust primitives — load and eval separated:**

The pipeline is split into maximally granular stages so tinct tools can stop at any point:

```tinct
# Stage 1: builtin-parse   — Bytes + path → raw AST (List of Document)
# Stage 2: expand          — tinct-controlled macro expansion loop
# Stage 3: builtin-resolve — name resolution, de Bruijn levels
# Stage 4: builtin-typecheck — resolved AST × TypeContext → typed Program
# Stage 5: builtin-eval    — execution (side effects: emit, I/O, channel sends)

# include = all five stages (the common runtime case)
include: [fn [let cap path]
  [handle:   [builtin-open cap path Readable]]
  [bytes:    [read-handle handle [builtin-bytes]]]
  [raw:      [builtin-parse bytes path]]
  [expanded: [expand raw]]
  [resolved: [builtin-resolve expanded]]
  [typed:    [builtin-typecheck resolved [builtin-get-type-context]]]
  [builtin-eval typed]]

# LSP usage — stop at stage 4 (no execution):
# [typed: [builtin-typecheck resolved [builtin-get-type-context]]]
# → type errors, hover info, completions available from typed; no side effects

# Isolated type-checking (e.g. sandbox analysis):
# [ctx: [builtin-make-type-ctx]]
# [builtin-typecheck resolved ctx]   # uses fresh isolated context
```

The separation enables: LSP calls `builtin-load` to get type information without executing anything. `include` combines both for runtime use. Introspection tools can load and inspect without triggering I/O.

**Goal:** maximize what is defined in tinct, minimize Rust-level special cases. The bootstrap functions (`create_stdlib_env_with_arena`, `create_type_stage_env`, `eval_surface_fn`) are refactored or simplified, not structurally replaced.

**True Rust primitives** — cannot be defined in tinct, needed before any tinct code runs:
- `builtin-if` — branching (dispatches on Variant tag, not `Value::Bool`)
- `builtin-variant`, `builtin-tag-of` — Variant construction and tag inspection
- `builtin-dict-get/set/merge` — the Dict primitive operations
- `builtin-int-eq`, `builtin-lt`, `builtin-add`, etc. — scalar arithmetic
- `builtin-eval` / `builtin-apply` — function application machinery

**`Boolean` moves to `loader.llt`:**

`Boolean: [type True False]` is declared in `loader.llt`'s runtime section (step 3 of the bootstrap), making `True` and `False` available before prelude.llt runs. This means:
- The type-stage prelude (step 5) can use `True`/`False` without Rust injection
- No special-case Boolean handling needed anywhere in `create_type_stage_env`
- `true`/`false` literals desugar to `[Boolean.True]`/`[Boolean.False]` constructor calls

**`builtin-if` dispatches on Variant tag:**

`builtin-if` changes from dispatching on `Value::Bool(true/false)` to dispatching on `Value::Variant { tag: "Boolean.True" }` vs everything else. One-line Rust change. `Value::Bool` is removed from the `Value` enum entirely.

**`create_type_stage_env` simplification:**

The S-885 operator alias injection (which injected `=`, `<`, `+`, `-`, `*`, `/` into the type-stage env so that `AddResult`/`SubResult`/`MulResult` could use `=`) is deleted entirely — those functions are dead code and are removed. `create_type_stage_env` becomes: inject core builtins → evaluate `--- stage: type` documents. No operator aliases, no Boolean injection (Boolean comes from loader.llt).

**`Value::Seq` removed:**

`Value::Seq` is removed from the `Value` enum. Seq values are `Value::Variant { tag: "Seq.Cons", ... }` and `Value::Variant { tag: "Seq.End" }` — same representation as any other nominal ADT. The following Rust builtins are removed from the registry (they are now defined in prelude.llt):
- `builtin-head`, `builtin-tail`, `builtin-cons`, `builtin-seq`
- `builtin-collect`, `builtin-range`
- `builtin-map`, `builtin-filter`, `builtin-reduce`, `builtin-sort` (seq-specific variants)

The canonical bootstrap sequence is in §Type Environment Bootstrap below. The two changes from today: step 3 (loader.llt runtime) gains `Boolean: [type True False]`; step 5 (prelude.llt stage:type) loses `AddResult`/`SubResult`/`MulResult`/`DivResult`.

**`eval_surface_fn`** — unchanged structurally. Macro transformer bodies now produce `Value::Variant` for Seq/Boolean rather than `Value::Seq`/`Value::Bool`, which is handled uniformly by the evaluator.

**Implementation steps (dependency-ordered):**

1. Delete `AddResult`/`SubResult`/`MulResult`/`DivResult` from prelude type-stage (dead code — done)
2. Add `builtin-load-eval` Rust builtin (parse+typecheck+eval a file handle)
3. Add `Boolean: [type True False]` to loader.llt; update `builtin-if` to dispatch on Variant tag; update true/false literal desugaring
4. Remove `Value::Bool` from Value enum; remove `create_type_stage_env` operator injection
5. Remove Phase 3 fast path from `create_stdlib_env_inner` — loader.llt includes prelude via `include %libdir "prelude.llt"` instead
6. Move program parsing from Rust to pre-sandbox: open files, collect `-e` exprs, build `%programs` List
7. Remove output serialization from main.rs — output formatter is last item in `%programs`
8. Remove `Value::Seq` from Value enum; remove seq-specific Rust builtins from registry; `Seq` defined in prelude.llt as nominal ADT
9. Unify `create_type_stage_env` and `create_stdlib_env_inner` — both become "evaluate loader.llt"

**Impact:** `Value::Bool` and `Value::Seq` removed from Rust enum. `create_stdlib_env_inner` and `create_type_stage_env` deleted entirely — replaced by `eval_file("loader.llt", env)`. ~10 Rust builtins removed from registry. main.rs loses all program-parsing and output-serialization logic. `builtin-typecheck` (with explicit `TypeContext`) becomes the single type-checking entry point. `builtin-make-type-ctx`, `builtin-fork-type-ctx` give tinct full lifecycle control over type contexts. Prelude has no privileged path. TypeEnv and type-stage env are unified behind the opaque TypeContext handle.

### TypeContext — Unified Type Environment Handle

The type system's two previously separate environments (type-stage env and TypeEnv) are unified behind a single opaque `TypeContext` handle. Tinct holds and threads the handle; Rust manages the internals.

**`TypeContext` is an opaque handle.** Tinct cannot peer inside it. It is analogous to `%emit-channel` or `%stdout` — an opaque reference to Rust-managed state. Tinct cannot inspect or modify TypeContext directly; specific query operations (e.g. for LSP hover) would be separate named builtins, not general introspection.

**There is no `%type-ctx` capability.** Rust does NOT inject a TypeContext into loader.llt's environment. Instead, tinct retrieves the current context via `[builtin-get-type-context]` — a zero-arg call that reads the context from the EvalContext. This eliminates threading through every function signature.

**Tinct is fully in control of the TypeContext lifecycle:**

```tinct
# Normal case — retrieve and use the current context implicitly:
[builtin-typecheck resolved [builtin-get-type-context]]

# Fresh isolated context (LSP, sandboxed analysis):
[analysis-ctx: [builtin-make-type-ctx]]
[builtin-typecheck user-file-ast analysis-ctx]  # isolated; doesn't affect global context
# analysis-ctx is discarded when the let scope ends

# Forked child context (experimental; mutations don't propagate up):
[child-ctx: [builtin-fork-type-ctx [builtin-get-type-context]]]
[builtin-typecheck experimental-ast child-ctx]
# current context unaffected
```

**TypeContext primitives:**
- `[builtin-get-type-context]` — retrieve current TypeContext from EvalContext (zero-arg)
- `[builtin-get-type-context x]` — force `x` (for its side effects), then return the current TypeContext. Used in `eval-file` as `[builtin-get-type-context prelude]` to guarantee prelude is loaded before type-checking a user file: forcing `prelude` runs `include` on prelude.llt, which updates TypeContext as a side effect, so the returned TypeContext already contains prelude's type declarations.
- `[builtin-make-type-ctx]` — creates a fresh TypeContext seeded with core type definitions only
- `[builtin-fork-type-ctx ctx]` — creates a child TypeContext inheriting from `ctx`; mutations don't propagate upward
- `[builtin-typecheck resolved ctx]` — type-checks the resolved AST using `ctx`, updates `ctx` in place with all new type declarations, returns typed program

**TypeContext internally unifies the two previously separate structures:**

| Previously | Now (inside TypeContext) |
|-----------|--------------------------|
| Type-stage env (`Arc<RwLock<Environment>>`) — TypeNode thunks from `--- stage: type` sections | Combined in TypeContext; same handle serves both annotation resolution and type inference |
| TypeEnv (`Rc<TypeEnv>`) — TyConDefs, TypeSchemes, ClassDecls from `[type ...]` declarations | Combined in TypeContext |

The annotation resolver no longer needs two separate lookups (type-stage env → fallback to TypeEnv). All type information is in TypeContext.

**Migration path:** TypeContext currently holds both structures internally. As the type-checker migrates to TypeNode values (per equirecursive-types whatif), TypeEnv is absorbed into the type-stage env and TypeContext becomes a single unified env. Tinct's API never changes — it's always an opaque handle.

**`create_type_stage_env` is deleted.** Its work is done by `builtin-make-type-ctx` (creates a seed context) and `builtin-typecheck` (updates it as files are processed). The two-structure bootstrap is replaced by one handle; tinct retrieves it via `builtin-get-type-context` without threading it through function parameters.

**Full bootstrap-to-user-code sequence:**

**Full bootstrap-to-user-code sequence:**

| Step | Who | What happens |
|------|-----|--------------|
| 1 | Rust | Follow the CLI construction sequence above: parse argv, open files pre-sandbox, apply Landlock, build `%programs`/`%args` |
| 2 | Rust | Assemble the initial dict; seed it with `builtin_module("core")` builtins; evaluate `stdlib/loader.llt` with this dict as the initial scope |
| 3 | loader.llt dict 1 | `read-handle` defined; sees initial-env entries (`%cwd`, `%libdir`, etc.) as bare names |
| 4 | loader.llt dict 2 | `Boolean`, `ProgramItem`, `DocName`, `include`, `expand`, `cli-pipeline`, `eval-*` defined. All closures capture the initial env in their scope chain. |
| 5 | loader.llt dict 3 | `_prelude: [include %libdir "prelude.llt"]` thunk created. `emit-ch`, `formatter` defined. `[cli-pipeline %programs formatter [] %cwd _prelude emit-ch]` is the auto-indexed entry. |
| 6 | cli-pipeline forced | `builtin-reduce` iterates `%programs` via `eval-pipeline-item` |
| 7 | eval-file (per user file) | `[builtin-get-type-context prelude]` forces `_prelude` — loads prelude.llt, updates TypeContext. Then: `builtin-parse` → `expand` → `builtin-resolve` → `builtin-typecheck` → `builtin-eval` per document |
| 8 | eval-document-runtime | Builds user doc scope: prelude + `%cwd`/`%libdir`/`%stdout`/`%args` (from closure) + `%include-dir` (file's own dir) + emit machinery |
| 9 | output formatter | `eval-pipeline-item` runs it last with the final `%`; formatter drains `%emit-channel`, writes to `%stdout` |

Rust exits after step 2's `eval_file` returns. All output has already occurred via side effects.

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
- stage:type → produces: `Int`, `Float`, `Never`, `Any: [builtin-variant "TypeNode.Any"]`, `Unknown: [builtin-variant "TypeNode.Unknown"]`, type combinators (`union`, `all`, `without`, `Seq`, `Map`, `mu`), TypeNode ADT with traversal protocol (`children`, `map-children`, `as-type`). The arithmetic resolvers `AddResult`/`SubResult`/`MulResult`/`DivResult` are deleted (dead code — no callers since FD resolver design was superseded).
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

`@String` resolves via the TyConDef path to `Type::Str`. It remains valid — removal is string-redesign's concern, not type-foundations.

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

- Keep `Type::Str` — `String` becomes a TyConDef in the root scope backed by `Type::Str`; `@String` remains valid until string-redesign whatif
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

### `List` — Lazy 2-3 Finger Tree

`List` is a lazy 2-3 finger tree (Hinze & Paterson 2006), declared entirely in tinct with no Rust-level collection primitive beyond `Value::Dict`. It provides O(1) amortized push-left and push-right, O(log n) random access, and O(log n) concatenation.

```tinct
# All types declared in prelude — Rust sees only Value::Dict and Value::Variant

Digit: [type [let a]
  [One a: a]  [Two a: a  b: a]  [Three a: a  b: a  c: a]  [Four a: a  b: a  c: a  d: a]]

# FNode carries precomputed subtree leaf count (enabling O(1) total-size, O(log n) index)
FNode: [type [let a]
  [FNode2 size: Int  left: a  right: a]
  [FNode3 size: Int  left: a  mid: a   right: a]]

# FTDeep.size = total leaf count at this level (maintained by push-left/push-right/concat)
FingerTree: [type [let a]
  FTEmpty
  [FTSingle value: a]
  [FTDeep size: Int  prefix: [Digit a]  spine: [FingerTree [FNode a]]  suffix: [Digit a]]]

List: [type [let a] [FingerTree a]]
```

**Why finger tree instead of `[Map Int a]`:** `[Map Int a]` backed by a hash map would require O(n) key renumbering on every `push-left` — all existing integer keys would need to shift up by 1. Finger trees avoid this entirely: positions are implicit (computed from size annotations), so prepend is O(1) amortized with zero renumbering.

**Size invariant:** `FTDeep.size` stores the TOTAL LEAF COUNT at every level of the tree. This enables:
- `length list` — O(1), reads `FTDeep.size` from the root
- `get i list` — O(log n), navigates by comparing `i` against subtree sizes

**The `Iterable` typeclass and `each`:** `each` is a typeclass method of `Iterable`, not a plain function. The collection type determines the element type via functional dependency:

```tinct
Iterable: [class [let Iterable c a] [determines: [[[c] a]]  resolver: IterResult]
  each: [Fn@[Seq a] [c]]]

# List instance — O(1) to initiate, O(log n) amortized per element consumed
[instance Iterable [let c@[List a] a]:
  [each: [fn [let list] [ft-to-seq list]]]]

# Map/Dict instance — produces Seq of [key: k  value: v] entry records
[instance Iterable [let c@[Map k v] [key: k  value: v]]:
  [each: [fn [let d] [entries d]]]]
```

`Seq` has **no** `Iterable` instance — `Seq` is already the streaming form; applying `each` to a `Seq` is a category error.

`keys` and `values` are plain functions derived from `[each dict]`:

```tinct
keys:   [fn [let d@[Map k v]] [map [fn [let e] e.key]   [each d]]]
values: [fn [let d@[Map k v]] [map [fn [let e] e.value] [each d]]]
```

Both return lazy `Seq` directly. `[each [keys d]]` is therefore redundant — `[keys d]` already produces a `Seq`.

**The `each` bridge for List** — apply `map`, `filter`, `reduce` to a `List`:

```tinct
# collect: O(n) — fold Seq into finger tree via push-right
collect: [fn [let s@[Seq a]]
  [reduce [fn [let tree x] [push-right x tree]] FingerTree.FTEmpty s]]

# Pattern:
[collect [filter pred [each my-list]]]    # List → Seq (lazy) → List (materialized)
[$my-list | each | [filter pred] | collect]  # pipeline form
```

**Index access:**

```tinct
[get 2 my-list]   # O(log n) via ft-index; dispatches through Indexable instance
[length my-list]  # O(1) via ft-size; reads precomputed size from root
```

`Seq` has **no** `Indexable` instance — `[get 2 my-seq]` is a type error. This forces the caller to either use `[head [drop 2 seq]]` (O(n)) or `[collect seq]` first, making the O(n) cost visible and voluntary.

### Implementation Changes — File by File

The following is a precise mapping from the proposed design to the current codebase, derived from auditing the actual source files. Each item names specific functions, enum variants, and line ranges where relevant.

#### `src/lexer.rs`, `src/parser.rs`, `src/ast.rs`

**Chain 1 — Boolean literal retirement (all deleted):**

| Item | Location | Change |
|------|----------|--------|
| `Token::BoolLit(bool)` | `lexer.rs:52,1111-1118` | **Delete.** `true`/`false` constructors are uppercase; no special token needed. |
| `SurfaceExpression::Bool(bool)` | `ast.rs:634` | **Delete.** No production site once `BoolLit` is gone. |
| `CoreExpr::Bool(bool)` | `ast.rs:1035` | **Delete.** No lowering site once `SurfaceExpression::Bool` is gone. |
| `LiteralPattern::Bool(bool)` | `ast.rs:235` | **Delete.** Boolean patterns are constructor patterns like any other nominal type. |
| `fmt_bool` | `lexer.rs:1684-1690` | **Delete.** SCN serialization of Boolean values uses the variant tag. |
| `key_to_string` Bool arm | `parser.rs:96-108` | **Delete** the `Bool` arm. |
| BoolLit → Pattern arm | `parser.rs:5187` | **Delete** `Bool(b) => Pattern::Literal(LiteralPattern::Bool(...))` arm. |
| BoolLit → SurfaceExpr arm | `parser.rs:3016-3039` | **Delete** the `BoolLit` arm; `True`/`False` arrive as `Token::Identifier` and produce `VarRef`. |
| `injective: SurfaceExpression::Bool` | `parser.rs:2473` | **Change** — class parser currently checks `SurfaceExpression::Bool(b)` for the `injective` field. Must become `VarRef("True")`/`VarRef("False")` equality check (or `VarRef("Boolean.True")`) once BoolLit is gone. |

**Chain 2 — `"Seq"` pattern special case (retire):**

`parser.rs:5250` has `("Seq", 2) => { /* seq-destructure pattern */ }` in `surface_node_to_pattern`. The parser recognizes `[Seq h t]` as a 2-arg seq-destructure by matching the string `"Seq"` — parser knows a domain type by name. Under type-foundations where Seq is fully tinct-defined, this is a design violation.

**Fix:** Remove the `("Seq", 2)` special case. `[Seq h t]` becomes an ordinary 2-arg constructor pattern handled generically by the evaluator's pattern matching — the same path as `[Color.Red payload]` or `[Option.Some payload]`. No parser change needed to "understand" Seq; it just works as a nominal variant.

**Chain 3 — `"tmpl"` / `"unindent"` — implementation drift, not new proposal:**

Triple-quoted strings and interpolated strings were always designed to be implemented by tinct-side macros. The current implementation — `InterpolatedPart` in the lexer, `tmpl`/`unindent` called from the parser — is a deviation that crept in during implementation. This section restores the original intent.

**`"""..."""` (triple-quoted strings):**
- **Current (deviation):** Lexer scans and strips indentation; parser desugars to `[unindent "raw"]`; `unindent` is a prelude function.
- **Correct (original intent):** Lexer recognizes `"""` delimiters and captures raw content only. Parser produces `SurfaceExpression::TripleQuotedString(raw)` — no desugaring. A tinct macro in prelude receives the raw string, computes the indentation anchor (whitespace of the closing `"""` line), strips it from content lines, returns `SurfaceExpression::Str(clean)`.

**`i"..."` (interpolated strings):**
- **Current (deviation):** Lexer scans for `$name` boundaries, splits into `Vec<InterpolatedPart>`; parser desugars to `[tmpl "raw" exprs...]`; `tmpl` is a prelude macro.
- **Correct (original intent):** Lexer recognizes the `i"..."` prefix and captures raw content as a plain string — no `$name` scanning, no `InterpolatedPart`. Parser produces `SurfaceExpression::InterpolatedStringLiteral(raw)` — no desugaring. A tinct macro in prelude scans for `$identifier` patterns using tinct string operations, assembles `[str part1 name part2 ...]` via `quote`/`unquote` AST construction, returns the call expression.

**Specific deletions:**
- `Token::InterpolatedString(Vec<InterpolatedPart>)` — delete (lexer no longer scans)
- `Token::TripleInterpolatedString(Vec<InterpolatedPart>)` — delete
- `InterpolatedPart` type — delete
- `tmpl` prelude function — delete (replaced by macro mechanism)
- `unindent` prelude function — delete (replaced by macro mechanism)
- `parser.rs:204` (`"tmpl"` hardcode) — delete with desugaring
- `parser.rs:3112,3154` (`"unindent"` hardcodes) — delete with desugaring

**Where the macros live:** Prelude defines both macros. In tinct's demand-driven expander, the expander forces macro thunks from the environment on use — even if defined in the same letrec dict. Prelude can define and use `i"..."` / `"""..."""` within the same source. User files work identically. Loader.llt is the only exception (runs before anything is loaded, macros don't exist yet) but has no need for these forms.

**Not special cases (confirmed correct):**
- `null` — zero parser knowledge; `null?` is a prelude function; `[]` is just an empty dict
- `_` wildcard — appropriate string equality check in `surface_node_to_pattern`
- `$` prefix, `@` annotation, `|` pipe, `%` caps prefix — all purely syntactic, no semantic knowledge
- `"determines"`, `"resolver"`, `"injective"` class keys — part of the `[class ...]` grammar, not arbitrary user-space names (though `injective` has the BoolLit dependency noted above)
- All structural special forms (`fn`, `match`, `type`, `class`, `instance`, etc.) — correct keyword dispatch

**Bool literal pipeline — the entire chain is a special case that should not exist:**
```
lexer.rs:1111  "true" → Token::BoolLit(true)       ← delete entirely
parser.rs:3016     → SurfaceExpression::Bool(true)  ← delete entirely
lower.rs:180       → CoreExpr::Bool(true)           ← delete entirely
eval_core.rs:770   → Value::Bool(true)              ← deleted with Value::Bool
```

There is no reason for lowercase `true`/`false` to exist. Tinct constructors are uppercase. `Boolean: [type True False]` produces constructors `Boolean.True` and `Boolean.False`. The short injected names are `True` and `False` (uppercase) — already valid as constructor patterns because tinct's parser treats uppercase identifiers in pattern position as constructor patterns, exactly like `Color.Red` or `Option.None`.

**Correct fix:** Delete `Token::BoolLit` entirely. Delete the lexer's `true`/`false` special-case (`lexer.rs:1111-1118`). User code writes `True`/`False` (short injected names from the Boolean type declaration) or `Boolean.True`/`Boolean.False` (qualified). No prelude alias for lowercase `true`/`false` needed — the lowercase forms served the old `Value::Bool` and nothing else.

#### `src/value.rs`

| Change | Current | Required |
|--------|---------|----------|
| Delete `Value::Bool(bool)` | `value.rs:529` | Remove variant. **~25 construction sites** across `builtins_math.rs`, `builtins_io.rs`, `builtins_net.rs`, `builtins_bytes.rs`, `builtins_dict.rs`, `builtins_datetime.rs`, `builtins_async.rs`, `eval_materialize.rs`, `main.rs` |
| Delete `Value::Bool` match arms | `eval.rs:1051,1170,1219,4030`; `eval_core.rs:700`; `eval_materialize.rs:4169,4193,4278`; `builtins_math.rs:322,412,450`; `builtins_seq_xform.rs:384,500`; `surface_convert.rs:707,944`; `value.rs:1233` | Replace with variant tag matching for `"Boolean.True"`/`"Boolean.False"` |
| `Value::Seq` | **Already removed** — Seq is `Value::Variant` | No change needed |
| Delete `value.rs:872-873` | `Key::String("head".into())`, `Key::String("tail".into())` for Seq.Cons payload | Delete with `builtins_seq_prim.rs` — these are the Seq violation |
| Add `HashableValue` enum | Does not exist | New enum: `Int(i64)`, `Bool(bool)`, `Dict(Vec<(HashableValue, HashableValue)>)`, `Variant { tag, payload }` with manual `Hash` (commutative sum for Dict) and `PartialEq` (order-insensitive for Dict) |
| Change `Value::Dict` key type | `IndexMap<Key, ThunkId>` | `IndexMap<HashableValue, ThunkId>` |
| Delete `Key` enum | `value.rs:115-155` | Remove entirely |
| Delete `StrKey` wrapper | `value.rs:157-182` | **Already unused** in the codebase — delete without replacement concern |
| Update `Builder` | Uses `IndexMap<Key, ThunkId>` | Switch to `IndexMap<HashableValue, ThunkId>` |
| Delete `make_seq_cons` | `value.rs:872-873` area | Helper for Seq.Cons construction — deleted with `builtins_seq_prim.rs` |

**Effort: Large.** `Key::String` and `Key::Int` appear in hundreds of match arms. `Value::Bool` appears in ~25 construction sites and ~18 pattern-match sites. Mechanical but high line count.

#### `src/builtins.rs`

| Change | Current | Required |
|--------|---------|----------|
| Delete `create_type_stage_env()` | Lines 1580–1726 (148 lines) | Remove entirely; TypeContext becomes tinct-controlled via new builtins |
| Delete Phase 3 fast path in `create_stdlib_env_inner` | Lines 1422–1531 | Prelude loads via `include %libdir "prelude.llt"` from loader.llt; **blocked on B-309** |
| Delete S-885 operator alias injection | Lines 1644–1663 | Remove (dead code once prelude type-stage `AddResult`/`SubResult`/`MulResult`/`DivResult` are confirmed gone) |
| Delete pre-intern TypeNode thunks | Lines 1693–1722 | Remove; TypeContext is opaque and tinct-controlled |

#### `src/builtins_core.rs` and `src/builtins_math.rs`

**`builtin-if` changes** (`builtins_math.rs:482-499` + `builtins_core.rs:187-198,3673`):
- Dispatch: change `Value::Bool(true)` / `Value::Bool(false)` arms to match `Value::Variant { tag }` where tag is `"Boolean.True"` / `"Boolean.False"`
- Type signature at `builtins_core.rs:3673`: change `(None, Type::Bool)` to `(None, Type::TyCon("Boolean"))` (or equivalent after de-primitisation)
- Both `"if"` (line 188) and `"builtin-if"` (line 194) are registered — both need updating

**NOT operations** (`builtins_math.rs:412,450`): change `Value::Bool(!b)` to construct the appropriate Boolean variant.

**Ordering of Bool** (`builtins_math.rs:322`): `false < true` comparison needs variant-aware implementation.

**New builtins to add:**

| Builtin | Status | Notes |
|---------|--------|-------|
| `builtin-parse` | Missing | Split from current `builtin-load`; Bytes + path → raw `Value::Program` only |
| `builtin-resolve` | Missing | Split from current `builtin-load`; name resolution + de Bruijn |
| `builtin-typecheck` | Missing | ResolvedAST × TypeContext → TypedAST; replaces `builtin-eval-types` with TypeContext param |
| `builtin-get-type-context` | Missing | Zero-arg: retrieve TypeContext from EvalContext; one-arg: force arg then return TypeContext |
| `builtin-make-type-ctx` | Missing | Fresh TypeContext seeded with core type defs |
| `builtin-fork-type-ctx` | Missing | Child TypeContext; mutations don't propagate up |
| `builtin-bytes-concat` | Missing | Concatenate two `Value::Bytes` |
| `builtin-path-dir` | Missing | `String → DirCap`; extract containing directory from a file path |
| `builtin-encode` | Missing | `ByteOrder × a → Bytes` — general machine-level encoding for any numeric type (Int, Float, UInt8, UInt16, UInt32, UInt64). Replaces the float-specific `builtin-float-bits`. Enables `float-to-string` (Ryu), binary protocol encoding, and bit manipulation from pure tinct. |

**Bare-name violations to fix** (all core exports must use `builtin-*`; prelude re-exports as user-facing names). `channel`, `send`, `recv` and other async builtins are already correctly registered as `builtin-channel`, `builtin-send` etc. — no change needed for those.

| Bare name | Rust function | Fix | Location |
|-----------|---------------|-----|----------|
| `"if"` (dual) | `builtin_if` | Remove bare `"if"`; `"builtin-if"` already exists at line 194 | `builtins_core.rs:188` |
| `"get?"` | `builtin_get_optional` | Rename to `"builtin-get?"` | `builtins_core.rs:215` |
| `"materialize"` | `builtin_force` | Rename to `"builtin-materialize"` | `builtins_core.rs:464` |
| `"until"` | `builtin_until` | Rename to `"builtin-until"` | `builtins_core.rs:478` |
| `"validate"` | `builtin_validate` | Rename to `"builtin-validate"` | `builtins_core.rs:483` |
| `"bytes"` | `builtin_bytes` | Rename to `"builtin-bytes"` | `builtins_core.rs:374` |
| `"bytes-find"` | `builtin_bytes_find` | Rename to `"builtin-bytes-find"` | `builtins_core.rs:375` |
| `"bytes-of"` | `builtin_bytes_of` | Rename to `"builtin-bytes-of"` | `builtins_core.rs:381` |
| `"bytes-equal?"` | `builtin_bytes_equal` | Rename to `"builtin-bytes-equal?"` | `builtins_core.rs:382` |
| `"ct-equal?"` | `builtin_ct_equal` | Rename to `"builtin-ct-equal?"` | `builtins_core.rs:388` |

Each renamed builtin needs a corresponding prelude export (`get?: builtin-get?`, `materialize: builtin-materialize`, etc.) so existing user code continues to work. The `"if"` case is unique: it already has a `"builtin-if"` alias, so only the bare `"if"` registration is removed.

**Seq-specific builtins to delete** (entire `builtins_seq_prim.rs` file):

`builtin-seq` (line 699), `builtin-head` (700), `builtin-tail` (701), `builtin-collect` (702), `builtin-range` (704), `builtin-map` (714), `builtin-filter` (720), `builtin-reduce` (738), `builtin-cons` (759).

These **walk Seq's internal tag structure** (`"Seq.Cons"`, `"Seq.Nil"`) — Rust design violations. They are replaced by tinct prelude implementations. Cannot be deleted until the tinct replacements are verified end-to-end.

Also delete from `builtins_bytes.rs:202-223`: Seq-walking bytes construction code. And `builtins_meta.rs:1513`: `type_name()` Bool special-case → `"Bool"` becomes irrelevant once Value::Bool is gone. Also `builtins_meta.rs:1516`: `"Seq.Cons"`/`"Seq.Nil"` special-case in `type_name()` — delete with seq builtins.

#### `src/coverage.rs` and `src/typecheck.rs`

The pattern exhaustiveness checker (`coverage.rs`) hardcodes Bool as a primitive type with literal constructors:

| Change | Current | Required |
|--------|---------|----------|
| `ConstructorTag::LiteralBool` | `coverage.rs:59` — used for `true`/`false` pattern matching | Replace with `ConstructorTag::Variant("Boolean.True")` / `Variant("Boolean.False")` |
| `Type::Bool` expansion | `coverage.rs:278-283` — expands to `[LiteralBool(true,0), LiteralBool(false,0)]` | Change to TyCon lookup that returns `[Variant("Boolean.True",0), Variant("Boolean.False",0)]` via the tinct type declaration |
| `LiteralPattern::Bool` match | `coverage.rs` (multiple sites) | Replace with `Variant` tag pattern |
| Hardcoded Bool signature | `typecheck.rs:2569-2574` — `LiteralBool(true,0)` and `LiteralBool(false,0)` in signature construction | Route through TyCon lookup |

The key insight: once `Boolean: [type True False]` is a TyConDef in the type env, the generic `Type::TyCon` handler at `coverage.rs:336-374` will automatically handle exhaustiveness checking via the registered constructors. The `Type::Bool` special case and `LiteralBool` constructor tag can then both be deleted.

#### `src/type_def.rs`

| Change | Current | Required |
|--------|---------|----------|
| Delete `Type::Bool` | Line 230 | Remove; update all match arms in `is_consistent`, `is_subtype`, `PartialEq`, `Display` (~15 sites in `type_def.rs` alone) |
| Keep `Type::Str` | Line 228 | Retained — `String` becomes a TyConDef backed by `Type::Str`; `@String` remains valid until string-redesign whatif |
| Delete `Type::Number` | Line 237 | Remove; replaced by `Number: [class [let Number n]]` with `[instance Number [let n@Int]: []]` and `[instance Number [let n@Float]: []]` in prelude. `@Number` becomes a typeclass constraint checked via instance existence in TypeEnv — no dispatch, no methods. Future numeric types add instances without changing the type checker core. |
| `Type::Seq` | **Already `Type::App(TyCon("Seq"), elem)`** via `Type::seq()` helper | No change needed — already aligned |
| Update `is_numeric` check | Line 350: `matches!(&expected, Type::Int | Type::Number | Type::Float)` | Remove `Type::Number` arm |

#### `src/typecheck_annot.rs`

| Change | Current location | Required |
|--------|-----------------|----------|
| Delete bypass list | `resolve_type_name_with_guard`, lines 2228–2300 | Remove entirely; replace with single env lookup path |
| Remove `"Seq"` special cases | Lines 384, 412, 1452 | Route through parameterized TyCon path |
| Remove `"Bool"`, `"String"`, `"Number"` from validation guard | Lines 2106–2113 | Update to reflect only genuinely primitive type names |

#### `src/eval_core.rs` and `src/lower.rs`

Seq violations in `eval_core.rs` that need deletion:
- `eval_core.rs:154-158`: `dict.get(&Key::String("head".into()))` and `dict.get(&Key::String("tail".into()))` — Rust code walking Seq.Cons by knowing its payload field names. Delete with the seq builtin removal.
- `eval_core.rs:98`: `dict.contains_key(&Key::String("type".into()))` — checks for a `type` key to detect AST dict structure. This may need updating depending on how AST values are structured post-migration.
- `eval_core.rs:82`: `Value::Bool(b) => make_node(SurfaceExpression::Bool(*b))` in the unquote bridge — delete with `Value::Bool`.
- `eval_core.rs:700-773`: `CoreExpr::Bool(b) => Value::Bool(*b)` — change to produce `Value::Variant { tag: "Boolean.True/False", payload: None }`. **Or**: change `lower.rs:180` to desugar `Bool` into a variant call before it reaches eval — cleaner single-point fix.

`lower.rs:180`: `SurfaceExpression::Bool(b) => CoreExpr::Bool(*b)` — the preferred fix point. Change to desugar into `CoreExpr::Variant("Boolean.True/False")` here so no other eval site needs updating.

#### Display, debug, and test output

When `Value::Bool` is replaced by `Value::Variant { tag: "Boolean.True", payload: None }`:
- `value.rs:998` Debug: currently `Bool(true)` → will become `Variant("Boolean.True")` — update debug output tests
- `value.rs:1085` Display: currently `true` → needs special-casing for Boolean variants to keep printing `true`/`false` (or update all test expectations)
- Tests expecting `"Bool(true)"` string output: `repl.rs:704`, `builtins_math.rs:1240,1253`, `value.rs:2780-2781`

#### `src/main.rs`

| Change | Current | Required |
|--------|---------|----------|
| Pre-open all file handles | Files opened lazily during eval by `builtin-open` | Open all positional file arguments before Landlock fires |
| Build `%programs` dict | Does not exist; uses Rust `Vec<PipelineStage>` | Integer-keyed Dict of `ProgramItem` values |
| Build `%args` dict | Does not exist; output format is a Rust variable | Dict with `output`, `strict` keys |
| Assemble initial env dict | Caps injected individually into `stdlib_env` | Single initial dict; all caps + programs + args together |
| Remove Rust pipeline loop | `run_eval()` loops over `Vec<PipelineStage>` | Single call to evaluate `loader.llt` with the initial env |
| Remove `build_type_stage_env()` call | Line 1454 | Delete; TypeContext becomes tinct-controlled |
| Move formatter addition to loader.llt | `run_eval` lines 1381–1431 add formatter Rust-side | Formatter is `ProgramItem.File` passed as separate param to `cli-pipeline` |
| Landlock — pre-sandbox file opening | Currently Landlock fires after stdlib creation; user files not pre-opened | Must pre-open all user files before Landlock |

#### `stdlib/loader.llt`

| Change | Current | Required |
|--------|---------|----------|
| Full rewrite | Two-dict file (~97 lines) defining `uses-scope`, `update-named`, `eval-program`, `eval-programs` | Three-dict file (~270 lines) per `doc/whatif/type-foundations/loader.llt` |
| Add | — | `Boolean`, `ProgramItem`, `DocName`, `include`, `eval-document-runtime`, `eval-file`, `eval-expr`, `cli-pipeline` |

#### `stdlib/prelude.llt`

| Change | Current | Required |
|--------|---------|----------|
| `Seq.Nil` → `Seq.End` | `Seq.Nil` in `prelude.llt:1697` (tinct code only) | Rename in prelude.llt — the Rust references are deleted (see below), not renamed |
| Delete `builtins_seq_prim.rs` | Entire file — `builtin_seq`, `builtin_head`, `builtin_tail`, `builtin_collect`, `builtin_seq?` all walk `"Seq.Cons"`/`"Seq.Nil"` by direct tag matching | **Delete the entire file.** Seq is tinct-defined; Rust has no business knowing its tag names. These operations become tinct prelude functions. |
| Delete Seq-walking in `builtins_bytes.rs` | Lines 202,209,219,223 — bytes-from-seq construction | Delete; replace with a tinct prelude function using `reduce` |
| Delete `type_name()` Seq special-case | `builtins_meta.rs:1516`: checks `"Seq.Cons"\|\|"Seq.Nil"` to return `"Seq"` | Delete; Seq values report their own variant tags like any other nominal type |
| Delete `make_seq_cons` | `value.rs` helper constructing Seq.Cons in Rust | Delete once `builtins_seq_prim.rs` is gone |
| Rewrite `head`, `tail`, `map`, `filter`, `reduce`, `collect`, `range`, `cons` | Currently wrap Rust builtins from `builtins_seq_prim.rs` | Pure tinct recursive implementations already designed in `doc/whatif/type-foundations/prelude.llt`; the Rust builtins are then deleted |
| Add typeclass declarations | `Hashable`, `Sortable`, `Prependable`, `Appendable`, `Concatenable`, `Indexable` are absent or incomplete | Declare per whatif design |
| Rename `Semigroup` → `Concatenable` | `Semigroup` exists for same-type combine | Rename for the collection hierarchy; `Semigroup` may coexist for non-collection uses |
| `Boolean` location | Declared in prelude runtime section | The whatif declares it in loader.llt dict 2; loader.llt's closure makes it available to user programs. Decide whether to keep a copy in prelude or reference the one from loader.llt's scope |
| `AddResult`/`SubResult`/`MulResult`/`DivResult` | **Already removed** from prelude type-stage | No change needed |

#### `stdlib/cli/out/*.llt`

No changes needed. The formatter files are already valid tinct and already use `%emit-channel` and `%stdout`. The only change is that they become `ProgramItem.File` entries orchestrated by loader.llt rather than being added Rust-side.

#### Dependency Order

1. **Seq terminal constructor rename** (`Seq.Nil` → `Seq.End` in `prelude.llt` only) — independent; tinct-only change; no Rust changes (the Rust Seq references are being *deleted*, not renamed)
2. **Type enum de-primitisation** (`type_def.rs`, `typecheck_annot.rs`) — independent; no blockers
3. **New builtins** (parse/resolve/typecheck split, TypeContext builtins, bytes-concat, path-dir) — independent
4. **`Value::Bool` removal** — depends on Bool literal desugaring fix (`lower.rs:180`); affects coverage.rs, eval.rs, builtins_math.rs, ~25 construction sites
5. **Coverage checker** (`coverage.rs`) — depends on `Type::Bool` deletion and Boolean TyConDef registration
6. **`HashableValue` + Key enum removal** — depends on nothing but has the largest blast radius; blocks nothing else
7. **Resolve B-309** (macro expansion circular recursion) — unblocks the loader.llt rewrite
8. **`builtins_seq_prim.rs` deletion** — depends on prelude tinct seq implementations working end-to-end; also deletes the Seq-walking code in `builtins_bytes.rs` and `eval_core.rs`
9. **loader.llt rewrite + bootstrap restructure** (`create_stdlib_env_inner` Phase 3 removal) — depends on B-309
10. **`main.rs` rewrite** (initial env construction, pre-sandbox file opening, remove pipeline loop) — depends on loader.llt rewrite

Items 1–3 can proceed in parallel immediately. B-309 is the critical path for items 9–10.

## `Showable` Instances

Canonical `show` implementations for non-text built-in types:

```tinct
[instance Showable [let t@Int]:
  [show: [fn [let n] [int-to-string n]]]]       # pure tinct; defined in prelude

[instance Showable [let t@Float]:
  [show: [fn [let f] [float-to-string f]]]]     # pure tinct using builtin-float-bits

[instance Showable [let t@Boolean]:
  [show: [fn [let b] [match b  True: "True"  False: "False"]]]]

[instance@[bind: [a]  constraint: [a: Showable]] Showable [let t@[List a]]:
  [show: [fn [let xs]
    [join "" ["[" [join " " [map show xs]] "]"]]]]]

[instance@[bind: [k v]  constraint: [k: Showable  v: Showable]] Showable [let t@[Map k v]]:
  [show: [fn [let m]
    [join "" ["[" [join "  " [map [fn [let e] [print e.key ": " e.value]] [entries m]]] "]"]]]]]
```

`int-to-string` is a pure tinct function in prelude — extract digits via `mod`/`/`, look up character strings from a table, concatenate with `builtin-str`. No Rust needed.

`float-to-string` is also a pure tinct function in prelude, implementing a shortest-decimal algorithm (Ryu or equivalent). The algorithm uses: `builtin-encode` to obtain the float's byte representation, integer arithmetic to reassemble those bytes into a 64-bit integer, `builtin-big-int` for 128-bit-wide multiplications, bit operations (`builtin-band`, `builtin-shr`, `builtin-shl`), and precomputed tables as tinct dicts. The rest is pure tinct.

The irreducible primitive is **`builtin-encode: ByteOrder × a → Bytes`** — a general machine-level encoding primitive that converts any machine numeric type to its byte representation with explicit endianness. Not float-specific: the same primitive handles Int, Float, UInt8, UInt16, UInt32, UInt64. The bias is always toward generalizable functionality.

```tinct
# Encode any machine numeric to its byte representation
[builtin-encode ByteOrder.NativeEndian 3.14@Float]   # → Bytes (8 bytes, IEEE 754 bit pattern)
[builtin-encode ByteOrder.BigEndian 443@UInt16]      # → Bytes (2 bytes, network order)
[builtin-encode ByteOrder.LittleEndian 42@Int]       # → Bytes (8 bytes, little-endian)

# For Ryu: get the IEEE 754 bits as an integer
float-bits: [fn [let f@Float]
  [bytes->int [builtin-encode ByteOrder.NativeEndian f]]]  # bytes->int is pure tinct arithmetic
```

`ByteOrder` is declared in prelude:
```tinct
ByteOrder: [type BigEndian  LittleEndian  NativeEndian  Network]
# Network = BigEndian by convention (RFC 791 / all IP protocols)
# NativeEndian for bit manipulation — not for cross-system interop
```

The decode direction (Bytes → numeric type) is also needed for binary protocol parsing and `int-to-float`-bits (the inverse of Ryu). It is expressed as pure tinct arithmetic (reassembling bytes into a numeric value) for fixed-width types, or as a companion `builtin-decode: ByteOrder × Bytes → a` for cases where Rust must do the reconstruction. This is addressed in the binary protocol handling work.

User-defined types implement `Showable` by declaring a tinct instance.

## Bootstrap and Module Structure

### Type-Checking Bootstrap — Eliminating the Prelude Special Case

**Current problem:** The type-checking bootstrap is handled entirely in Rust, independently of the runtime bootstrap described above. `src/imports.rs` contains `build_prelude_env_inner()`, `typecheck_and_merge_stdlib_module()`, and a `PRELUDE_CACHE` (`OnceLock`) that type-check `prelude.llt` using embedded source and cache the resulting `TypeEnv`. This is ~400 lines of Rust that exist solely to special-case prelude. Crucially, prelude type errors are silently discarded (`_type_errors` binding), so bugs in prelude are invisible to users — only downstream cascades in user code surface.

This violates the axiom "the Rust runtime must be genuinely agnostic to what prelude does." Prelude is loaded by `loader.llt` and is user code; its type errors must surface through the same pipeline.

**Target architecture:** The type-checking bootstrap mirrors the runtime bootstrap exactly:

1. **Seed env** — raw builtins only (`core_builtins_env()` types, no prelude abstractions). This is the only Rust-privileged type env. It is sufficient to type-check `loader.llt` itself (which uses only `--- uses: ["core"]`).

2. **`loader.llt` type-checking** — type-checked against the seed env. Its `$include %libdir "prelude.llt"` call is handled by the normal include processing path: prelude is type-checked and its bindings merged into the running env. Prelude type errors surface through the same `Vec<TypeError>` path as any other include.

3. **Incremental env accumulation** — the type-checker's include handling participates in the main type-checking pass (not just the LSP analysis path). Each `$include` call type-checks the included file and merges its type schemes into the active env for subsequent documents.

4. **Content-hash cache** — the `OnceLock` performance optimization is replaced by an async-native `DashMap<Blake3Hash, Arc<TypeEnv>>`. This gives the same amortized O(1) cache hit without the sync constraint of `OnceLock::get_or_init`. On a cache miss the type-checking pipeline runs asynchronously (no executor spin-up) and the result is inserted atomically. Because the cache is content-addressed, it is correct across concurrent compilations and correct when the same file is included from multiple paths.

**Async implication:** `OnceLock::get_or_init` requires a sync closure — it is the last remaining production sync bridge after the pervasive-async migration. The content-hash cache eliminates this constraint entirely. `build_prelude_env()` becomes `async fn`, `build_prelude_env_inner()` is deleted, and the one remaining `block_on_anywhere` call in `imports.rs` disappears. The entire type-checking and runtime bootstrap then runs on the single async executor with no sync bridges in production code.

**What gets deleted:** `build_prelude_env_inner()`, `typecheck_and_merge_stdlib_module()`, `PRELUDE_CACHE` (`OnceLock`), `PRELUDE_INSTANCE_CACHE`, `in_prelude_load` flag (currently skips instance method body inference as an optimization — eliminated when prelude is no longer special). The `OnceLock` → `DashMap` replacement is the technical enabler; the architectural cleanup is the goal.

**The main engineering challenge:** The type-checker currently requires a fully-built `parent_env` before it starts. Making env accumulate during type-checking of a pipeline (across `$include` calls) requires the include handling in the main type-checking pass to match what the LSP analysis path at `imports.rs:1412` already does. This is architecturally correct — `$include` is an expression that produces a type env delta, and that delta flows forward in the pipeline.

### Async Bootstrap — Why This Order Matters

The pervasive-async migration (completed 2026-06) made every production code path through the evaluator and type checker async — `normalize()`, `unify()`, `infer_surface_expr()`, `typecheck_surface_program_annotation_table()`, all annotation resolvers, the module loader. The one remaining sync bridge was `OnceLock::get_or_init` in `build_prelude_env_inner()`.

The bootstrap refactoring in this document resolves that final bridge by eliminating the separate prelude type-checking path entirely — prelude is type-checked through the same async pipeline as any other `[include ...]`ed file. This means:

- No `block_on_anywhere` in production code anywhere in the type-checking or evaluation stack
- A single async executor drives everything from the moment the program starts
- Prelude type errors surface through the normal advisory type-checking path — no silent discard
- The cache is content-addressed and async-safe, usable by concurrent LSP requests without locking

The two migrations (pervasive-async and bootstrap unification) are independent in implementation order but together constitute the complete removal of sync-from-async bridges. Either can land first; the OnceLock bridge is eliminated only when both are complete.

## Implementation Notes

### `builtin-eval` Return Type

`builtin-eval` currently has `ret: Box::new(Type::Unknown)` in `src/imports.rs`. This is wrong: `Type::Unknown` is the gradual type that propagates via the consistency relation `~` and disables type checking downstream. The correct return type is not a fixed annotation at all — it is inferred from the final expression of the evaluated sequence, the same way the type checker infers the return type of any tinct function body or document.

The return type of `builtin-eval doc.expressions` is the type of the last dict expression in `doc.expressions` — the same type as `%` from that document, the same as the inferred return type of an equivalent function. The three-phase pipeline ensures the type checker has already traversed those expressions before `builtin-eval` is called on them, so the type is always known.

**Fix:** remove the hardcoded `Type::Unknown` return type. The type checker infers `builtin-eval`'s return type at each call site from the expressions argument, exactly as it infers the return type of any expression. The fallback for genuinely unknown expressions is `Type::Any` (the sound top type), not `Type::Unknown`.

This fix makes `include` return the correct type — the exported dict of the included file — without any special-casing of `include` in the type checker. The type flows naturally: `builtin-eval` → `eval-document-runtime` → `eval-document-pipeline` → `eval-file` → `include`.

---

## `Printable` Typeclass

User-readable string representation for any type. Follows the `-able` typeclass naming convention (`Seekable`, `Addable`, etc.). Method: `print` returns a human-readable `String`. No existing `print` function in tinct — the name is unambiguous. `emit` remains the output primitive; `print` is the conversion to string.

```tinct
[class [let Printable t]
  print: [Fn@String [t]]]
```

**Primitive instances** (declared top-level, not in a dict, like all instances):

```tinct
[instance Printable [let a@Int]:    [print: builtin-int->string]]
[instance Printable [let a@Float]:  [print: builtin-float->string]]
[instance Printable [let a@Bool]:   [print: [fn [let b] [if b "true" "false"]]]]
[instance Printable [let a@String]: [print: identity]]
```

**`str` generalised through `Printable`:** The variadic `str` builtin is replaced by a tinct function that dispatches over `Printable`. Any type implementing `Printable` works automatically:

```tinct
# str: variadic string concatenation via Printable.
# Replaces the str Rust builtin — any type implementing Printable works automatically.
str: [fn@String [let ...args]
  [join "" [map print args]]]
```

User-defined types implement `Printable` by declaring an instance in tinct — no Rust changes required:

```tinct
Color: [type Red Green Blue]

[instance Printable [let a@Color]:
  [print: [fn [let c]
    [match c  Color.Red: "red"  Color.Green: "green"  Color.Blue: "blue"]]]]

[str "The color is: " Red]  # → "The color is: red"
```

---

## `Codec` Typeclass

Data transformations that sit between IO layers — enabling encryption, compression, framing, and serialization. Every codec bridges two IO shapes: `ByteStream`, `Datagram`, `MessageStream`, or another codec stage.

### Typeclass Declaration

```tinct
[class [let Codec c input output]
  encode:     [Fn@output [c input]]
  decode:     [Fn@{value: input  next: Int}
               [c output Int [or [CodecInvalidHandler input output] Absent]]]
              # Absent = "use the codec's own default". Pass a handler to override.
              # Each instance checks [absent? handler] and substitutes its own named default.
  codec-name: [Fn@String [c]]]
```

The `decode` method takes `(codec, buffer, offset, on-invalid)` and returns `{value: decoded, next: offset-after-frame}`. The `on-invalid` handler is codec-specific — each codec declares its own defaults; callers may override with a 4th argument. For whole-buffer codecs (text encodings), call with `offset=0`; `next` = `[length buf]`. For framing codecs (QUIC, H2, H3), `offset` advances through a packet payload.

```tinct
# Handler type — parameterized by the same input/output types as the Codec.
CodecInvalidHandler: [type [let input output] [Fn@{value: input  next: Int} [output Int]]]
```

**`Codec` is the bridge between IO shapes** — the `input` and `output` type parameters tell you which shapes are connected:

| Instance | input | output | Bridge |
|---|---|---|---|
| `NdjsonCodec` | `Any` | `Bytes` | MessageStream → ByteStream |
| `GzipCodec` | `Bytes` | `Bytes` | ByteStream → ByteStream |
| `TlsRecordCodec` | `Bytes` | `Bytes` | ByteStream → ByteStream |
| `HpackCodec` | `Headers` | `Bytes` | MessageStream → ByteStream |
| `H2FrameCodec` | `H2Frame` | `Bytes` | MessageStream → ByteStream |
| `DtlsCodec` | `Datagram` | `Datagram` | Datagram → Datagram |

### Composition Semantics

Codec layers compose by type — the `output` type of one codec is the `input` type of the next. This enables separating framing from serialization into distinct composable stages:

```tinct
# Stage 1 — serialization: Any → String (JSON text)
[instance [Codec JsonCodec Any String]
  encode: json.to-json
  decode: json.from-json]

# Stage 2 — framing: String → Bytes (NDJSON: add \n, encode as bytes)
[instance [Codec NdjsonFramer String Bytes]
  encode: [fn [let _ s] [bytes [str s "\n"]]]
  decode: [fn [let _ b] [str-from-bytes [str-trim-end b "\n"]]]]

# Pipe pipeline — stages compose with | and read left to right:
[rebind %emit
  [%emit | [codec JsonCodec] | [codec NdjsonFramer] | [sink %stdout]]]
```

Two-stage decomposition enables independent swapping: replace `JsonCodec` with `CborCodec` for binary encoding while keeping NDJSON framing, or replace `NdjsonFramer` with `LengthPrefixFramer` while keeping JSON. Adding gzip is one more `| [codec ...]` stage:

```tinct
[rebind %emit
  [%emit
    | [codec JsonCodec]
    | [codec NdjsonFramer]
    | [codec GzipCodec]
    | [sink %stdout]]]
```

The type chain: `MessageStream@Any → MessageStream@String → ByteStream → ByteStream → %stdout`. Each stage is independently swappable and the pipeline reads the same as the data flow.

The `Codec` typeclass declaration lives in `stdlib/net.llt` alongside `ByteStream`, `Datagram`, and `MessageStream`. Net-specific codec instances (`TlsRecordCodec`, `DtlsCodec`, `H2FrameCodec`, `HpackCodec`) are defined in their respective protocol files.

---

## `ByteStream`, `Datagram`, and `Seekable` Typeclasses

Three abstract IO shapes. Every transport, tunnel, and framing layer is either a byte stream, a datagram socket, or a seekable stream — no other fundamental shapes exist. These typeclasses go in `stdlib/prelude.llt` because they apply to any program doing I/O.

### `ByteStream`

An ordered, reliable, connection-oriented byte pipe. Reading and writing are symmetric; addressing was resolved at connection time.

```tinct
# Uses Bytes (= List UInt8), not Seq UInt8.
# Bytes is a concrete List — provides O(1) length and O(log n) random access.
# Seq would lose size information needed by most I/O protocols.
[class [let ByteStream h]
  read:  [Fn@Bytes [h Int]]   # reads up to n bytes; returns Bytes (may be fewer than n)
  write: [Fn@h     [h Bytes]]]
```

`Handle` is the opaque Rust I/O handle — the unified type for any sequential stream (TCP, file, pipe). It is the base `ByteStream` instance backed by `builtin-read-chunk`/`builtin-write`:

```tinct
[instance ByteStream
  [let h@Handle]: [read:  [fn [let handle n] [builtin-read-chunk handle n]]
                   write: [fn [let handle bytes] [builtin-write handle bytes]]]]
```

Protocol layers above produce new `ByteStream` instances — tinct records wrapping the layer below. All `*-accept` and `*-layer` functions are parametric over `ByteStream`, enabling arbitrary layering.

### `Seekable`

Random-access extension of `ByteStream`. Only files implement this — network streams and pipes do not. `seek` returns the new absolute position.

```tinct
[class [let Seekable h] [ByteStream h]
  seek: [Fn@Int [h SeekFrom]]]
```

`tell = [seek h [SeekFrom.Current 0]]`. `size = [seek h [SeekFrom.End 0]]` (seek back after if needed). `SeekFrom` is defined in `stdlib/prelude.llt`:

```tinct
SeekFrom: [union
  [Start   pos@Int]
  [End     pos@Int]
  [Current pos@Int]]
```

### `Datagram`

An unordered, unreliable packet socket. Each send and receive is a discrete unit with its own source/destination address.

```tinct
[class [Datagram d]
  send: [Fn [d@d addr@SocketAddress data@Bytes] Null]
  recv: [Fn [d@d] UdpDatagram]]
```

`UdpSocket` (from `udp-socket`) is the base instance. The net-specific instances and implementations are defined in `stdlib/net.llt`.

### Summary: the IO shapes

| Typeclass | Direction | Framing | Base instance |
|---|---|---|---|
| `ByteStream` | `n` raw bytes | caller's job | `Handle` |
| `Seekable` | random access | caller's job | file `Handle` only |
| `Datagram` | packet + address | per-packet | `UdpSocket` |

---

## `[Bytes N]` Refinement Type

`[Bytes N]` is a fixed-size byte sequence where `N` is a natural number literal in the type annotation. Without it, all fixed-width binary contracts (crypto key sizes, address widths) are documentation-only and every wrong-sized value is a runtime error rather than a type error.

### Conceptual Foundation — the Closed-Map Interpretation

A fixed-size byte string of length N is isomorphic to a closed Map from integer keys `{0, 1, …, N-1}` to `UInt8` values — the same structure as a C array `uint8_t key[32]`. The subtyping relationship is that of refinement types: `[Bytes N]` is `Bytes` refined by the constraint `length = N`. A more constrained type is a subtype of its base type (the same relationship as `UInt8 <: Int`): **`[Bytes N] <: Bytes`** — a fixed-size byte sequence is a valid `Bytes` value and can be used anywhere variable-length `Bytes` are accepted.

### Type System Change

`[Bytes N]` is implemented as a new TypeNode constructor declared entirely in tinct — no new Rust type variants, no new kind system extensions:

```tinct
# Added to the TypeNode ADT declaration alongside Recursive, TypeVar, etc.:
[SizedBytes@[supertype: TypeNode.Bytes  as-type: [fn [let b] b]  guarding: false]
  n: Int]
```

- `[Bytes 4]` in annotation position invokes `expand_named("Bytes", [4])` — `4` is a plain `Int` value in the type stage — producing `TypeNode.SizedBytes { n: 4 }`
- `TypeNode.SizedBytes { n: N } <: TypeNode.Bytes` — the `supertype: TypeNode.Bytes` annotation carries an actual TypeNode value; the type checker reads it and applies the subtype rule generically, with no Rust special-case for `SizedBytes`
- `TypeNode.SizedBytes { n: M }` is incompatible with `TypeNode.SizedBytes { n: N }` when `M ≠ N` — the generic TypeNode structural equality check compares the `n` field; `4 ≠ 16` → type error with no special unification arm needed
- A `Bytes` value narrows to `[Bytes N]` at a `TypeAssert` boundary when the `is:` predicate validates the length — the same runtime validation mechanism used by `UInt8`, `Port`, etc.

`N` is a plain `Int` value in the type stage — no `Kind::Nat` Rust enum variant is needed. `[Bytes N]` in annotation position is a type-stage function application: `Bytes` is a `TyConDef` whose body is `[fn [let n] TypeNode.SizedBytes n: n]`, applied to the integer literal `N`. Type-level arithmetic uses the existing type-stage evaluator's Int arithmetic: `[+ 32 32]` → `64` at type-check time.

### Construction and Operations

`[Bytes N]` values are constructed via the TypeAssert boundary (the `is:` predicate validates size), exactly as `UInt8` and `Port` work:

```tinct
key@[Bytes 32]:  [192 168 1 1 ...]    # annotation validates: length = 32
addr@[Bytes 4]:  [192 168 1 1]        # [Seq UInt8] narrows to [Bytes 4] at TypeAssert
nonce:           [crypto-random 12]   # inferred as [Bytes 12] — dependent return type
```

`get` and `slice` are generic indexed-access operations via the `Indexable` typeclass:

```tinct
[class [Indexable s e]
  get:    [Fn [s@s i@Int] e]
  slice:  [Fn [s@s start@Int len@Int] s]
  length: [Fn [s@s] Int]]

[instance [Indexable [Bytes N] UInt8] ...]
```

`concat` propagates size arithmetic through the type-stage evaluator:

```tinct
# [concat a b] where both a: [Bytes 32] and b: [Bytes 32] → [Bytes 64]
# Type-stage: [+ 32 32] → 64, yielding TypeNode.SizedBytes { n: 64 }
# When any argument is variable-length Bytes, result is Bytes.
concat: [fn@[bind: [t]  return: t  constraint: [t: Appendable]] [...args@t]
  [reduce append empty args]]
```

Integer ↔ bytes conversion always names the byte order explicitly — there is no way to create a multi-byte integer representation without specifying endianness:

```tinct
[uint32 ByteOrder.BigEndian ip-bytes]               # [Bytes 4] → UInt32
[bytes ByteOrder.BigEndian [@UInt16 443]]           # UInt16 → [Bytes 2]
[bytes ByteOrder.LittleEndian nonce@UInt64]         # UInt64 → [Bytes 8] — little-endian counter
```

### What Changes in the Implementation

- **TypeNode ADT**: add `[SizedBytes@[supertype: TypeNode.Bytes  as-type: [fn [let b] b]  guarding: false]  n: Int]` to the `TypeNode` declaration in `stdlib/prelude.llt`; register `Bytes` as a `TyConDef` with `params: ["n"]` and body `[fn [let n] TypeNode.SizedBytes n: n]`
- **Parser**: `[Bytes N]` in annotation position — `N` is parsed as a type-stage Int expression (integer literal); no new syntax or kind annotation required
- **Type checker**: the `supertype:` annotation on `SizedBytes` is read as a TypeNode value (`TypeNode.Bytes`) and applied generically by the subtype checker — no Rust special-case for `SizedBytes`; size inequality (`M ≠ N`) falls out of generic TypeNode structural equality on the `n` field
- **Unification**: handled entirely by the generic TypeNode structural equality path — `SizedBytes { n: M }` vs `SizedBytes { n: N }` compares field `n` by value; `SizedBytes <: Bytes` via `supertype:` annotation
- **Eval/materialize**: no change — runtime representation is `Value::Bytes`; size is checked at `TypeAssert` boundaries via `is:` predicate on `bytes-length`
- **Builtins**: update any builtin return type registrations that use fixed-size bytes; `Indexable` typeclass gets a `[Bytes N]` instance

---

## Type-Level Lookup Tables

Every enumeration with associated constants (wire codes, error codes, opcodes, bit widths) can embed those constants directly in variant declarations. The constants travel with the type — they cannot get out of sync, and they eliminate all lookup functions.

### Syntax

Variants may carry named compile-time constants (lowercase identifiers followed by `:` and a literal value) alongside or instead of runtime payload fields (names followed by `@Type`). Constants and payload fields may appear in any order in the same variant:

```tinct
DnsRcode: [type
  [NoError  rcode: 0  description: "No Error"]
  [FormErr  rcode: 1  description: "Format Error"]
  [ServFail rcode: 2  description: "Server Failure"]
  [NXDomain rcode: 3  description: "Non-Existent Domain"]
  [NotImpl  rcode: 4  description: "Not Implemented"]
  [Refused  rcode: 5  description: "Query Refused"]]
```

Variants may also mix constants with runtime payload fields:

```tinct
WsFrame: [type
  [Text   opcode: 0x01  data@String]
  [Binary opcode: 0x02  data@Bytes]
  [Close  opcode: 0x08  code@WsCloseCode  reason@String]
  [Ping   opcode: 0x09  data@Bytes]
  [Pong   opcode: 0x0A  data@Bytes]]
```

### Access Patterns

**Forward lookup (constant from variant):** dot-access on a variant instance or on the type name itself:

```tinct
DnsRcode.ServFail.rcode         # → 2 (from type name: DnsRcode.VariantName.field)
DnsRcode.ServFail.description   # → "Server Failure"
some-rcode.rcode                # → the rcode constant for whatever variant some-rcode is
frame.opcode                    # → 0x01 for Text, 0x02 for Binary, etc. — no match needed
```

**Reverse lookup (variant from constant):** a new form of `get` that takes a named-field query and a type name:

```tinct
[get rcode: 2 DnsRcode]         # → DnsRcode.ServFail
[get rcode: 99 DnsRcode]        # → Absent.Absent (unknown value)
[get opcode: 0x01 WsFrame]      # → WsFrame.Text (unit lookup — no payload fields resolved)
```

`[get field: value TypeName]` returns `Absent.Absent` for unknown constants, so callers use `[or ...]` or pattern match to handle that case.

### Wire Encoding

Constants plug directly into existing byte-encoding primitives — no new magic:

```tinct
# Encode: use the constant directly
[bytes ByteOrder.Network [@UInt16 DnsRcode.ServFail.rcode]]   # → [0x00 0x02]
[bytes ByteOrder.Network [@UInt8 frame.type.code]]             # → opcode byte from H2FrameType variant

# Decode: reverse lookup with fallback
[rcode: [or [get rcode: [band flags 0x000F] DnsRcode] DnsRcode.ServFail]]
[ct:    [or [get code: [get header 0] ContentType] [error "TLS: unknown content type"]]]
```

### Eliminated Boilerplate

Every `*->int` and `int->*` function across protocol or enumeration files is replaced by this pattern. Before/after comparison:

```tinct
# Before — two lookup functions, risk of mismatch:
qtype->int: [fn [let qt@DnsQtype]
  [match qt
    [DnsQtype.A]: 1    [DnsQtype.AAAA]: 28  ...]]

int->rcode: [fn [let n@Int]
  [match n  0: DnsRcode.NoError  1: DnsRcode.FormErr  ... _: DnsRcode.ServFail]]

# After — zero lookup functions, constants live on the type:
DnsQtype: [type [A code: 1] [AAAA code: 28] ...]
DnsRcode: [type [NoError rcode: 0] [FormErr rcode: 1] ...]

# Encode:
[bytes ByteOrder.Network [@UInt16 q.qtype.code]]

# Decode:
[or [get rcode: rcode-int DnsRcode] DnsRcode.ServFail]
```

### Generalised `Indexable`: Lookup by Any Key Type

`[get field: value TypeName]` is not special syntax — it is a natural extension of the `Indexable` typeclass generalised to any key type `k`:

```tinct
[class [let Indexable s k v]
  get:    [Fn@[or v Absent] [s k]]
  slice:  [Fn@s [s Int Int]]
  length: [Fn@Int [s]]]
```

The key type determines which instance fires:

| Expression | `s` | `k` | Instance |
|---|---|---|---|
| `[get 0 list]` | `[List T]` | `Int` | List by integer index (O(log n)) |
| `[get "key" dict]` | `Dict` | `Graphemes` | Dict by string key (O(1)) |
| `[get rcode: 2 DnsRcode]` | `[Seq DnsRcode]` | `[Map String Any]` | Type lookup table |

Note: `[Seq T]` has no `Indexable` instance. `[get 0 seq]` is a compile-time type error — use `[head [drop n seq]]` for positional Seq access (O(n)), or `[collect seq]` to produce an `Indexable` `List` first.

For type lookup tables, the type name evaluates at runtime to a `Seq` of all its variants (each carrying their compile-time constants as accessible fields). The named-argument dict `{rcode: 2}` is the key — `get` finds the first variant where all selector fields match. `Absent.Absent` for no match, consistent with `get?` on dicts. This unifies three previously separate concepts under one typeclass: sequential access, dictionary access, and enumeration lookup.

---

## Discriminated Error Unions Per Subsystem

Operations fail in predictable, distinguishable ways. `try` in tinct is for exceptional and unexpected failures; expected failure modes return typed discriminated unions that callers can pattern-match directly. This is the Rust/Haskell model: one typed error union per subsystem, no string matching.

```tinct
[match [lookup-ips cap DnsQtype.A host]
  [Result.Ok addrs]:           [happy-connect cap port addrs]
  [DnsError.NXDomain name]:    [error [str "no such host: " name]]
  [DnsError.Timeout]:          [retry-with-tcp cap host]
  [DnsError.Refused]:          [error "nameserver refused query"]
  [DnsError.ServerFailure]:    [try-next-nameserver cap host]]
```

Each subsystem defines its own error type; callers handle only the failures relevant to their context and let others propagate.

### Design Rules

1. **One discriminated union per subsystem.** Each domain (`Dns`, `Tls`, `Http`, `Net`, etc.) defines a `FooError` type whose variants enumerate every distinguishable failure mode.

2. **Variants are named after the failure, not the mechanism.** `DnsError.NXDomain` not `DnsError.ResponseCode3`. The variant name is what the caller cares about, not how the wire encodes it.

3. **`try` is for unexpected failures only.** If a failure is predictable (host not found, connection refused, authentication failed), it belongs in a typed return value, not in `try`. `try` is the last resort for crashes, bugs, and truly unrecoverable states.

4. **Payloads carry context.** `[DnsError.NXDomain name]` carries the queried name. `[TlsError.CertificateExpired cert]` carries the cert. Callers get what they need to produce a meaningful error message or decide on a recovery strategy — without string parsing.

### Pattern

```tinct
# Define the error union for a subsystem
FooError: [type
  [NotFound   key@String]
  [Forbidden  reason@String]
  [Timeout    after@Int]
  [BadFormat  offset@Int]]

# Return it alongside success values
do-thing: [fn@[or FooResult FooError] [let input]
  ...]

# Callers pattern-match on named failure modes
[match [do-thing my-input]
  [FooResult payload]:        [use payload.value]
  [FooError.NotFound key]:    [error [str "not found: " key]]
  [FooError.Timeout after]:   [retry-after after]
  [FooError.Forbidden reason]: [raise reason]
  [FooError.BadFormat offset]: [log-and-skip offset]]
```

The net-specific error unions (`DnsError`, `TlsError`, `NetError`, `HttpError`, `WsError`, `QuicError`) are defined in their respective stdlib files.
