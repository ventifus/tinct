# What If: Nominal Variants for tinct

What would it take to add nominal (constructor-based) variants to tinct, layered on
top of the structural ADT system?

## Current State

tinct's structural ADT proposal (`doc/whatif/algebraic-data-types.md`) describes
sum types via `[union ...]` where variants are discriminated by key set. A dict
`[ok: 42]` satisfies the `Ok` variant of `Result` because its key set matches.
This structural approach is appropriate for the majority of config-language use
cases — external data (JSON, config files) automatically satisfies variant types
when it has the right shape.

Three things structural ADTs cannot provide:

```lisp
# Problem 1: Two constructors with the same field structure are indistinguishable
Left:  [union [value: a]]    # Left has one field "value"
Right: [union [value: a]]    # Right also has one field "value"
# [value: 42] satisfies BOTH — there is no discrimination

# Problem 2: Variant values are also plain dicts — field access always works
res: [@Result [ok: 42]]
$res.ok    # → 42 — nothing prevents this; the variant structure is invisible at runtime

# Problem 3: Constructor functions cannot be passed as values
[call $map Ok $items]   # Ok is not a first-class function in structural ADTs
```

### What's Missing

1. **Opaque construction.** Any dict with the right key set satisfies a structural
   variant. There is no way to declare that a variant value must be produced by its
   named constructor and cannot be a plain dict.
2. **Payload-identical constructors.** Two constructors with the same payload shape
   cannot coexist in a structural union — the key sets must differ.
3. **First-class constructors.** Constructor names are not values; you cannot pass
   `Ok` to `$map` or store it in a variable.
4. **Mandatory elimination.** Nothing prevents accessing `$result.ok` on an `[err: msg]`
   value; the variant wrapping is transparent.

## Why Nominal Variants Matter for tinct

**True sum types for general-purpose programming.** Structural ADTs cover config data
naturally. Nominal variants cover the cases that arise when tinct is used for
processing logic: `type Tree a = Leaf | Node a (Tree a) (Tree a)` requires two
distinct constructors (`Leaf`, `Node`) where `Leaf` is a unit with no payload and
`Node` takes three arguments. Structural ADTs cannot express this without
artificially different key names.

**Constructor functions as first-class values.** `Ok`, `Err`, `Some`, `None` become
functions that can be passed to `$map`, `$filter`, and other higher-order builtins.
`[call $map Ok $items]` wraps each item in an `Ok` constructor — a natural pattern
for monadic-style processing.

**Mandatory elimination via pattern matching.** A nominal variant value is not a dict.
`$result.ok` is a type error — the payload is only accessible through pattern matching.
This enforces complete case analysis in a way structural ADTs cannot.

**Self-hosting stdlib.** Future tinct stdlib functions that return structured results
(parse outcomes, query results, decode results) benefit from nominal variants: the
return type precisely documents which outcomes are possible, and callers cannot bypass
the constructor wrapper.

## Design

### Extending `[union ...]` with Case

Nominal and structural variants coexist in a single `[union ...]` form, discriminated
by case. This reuses tinct's existing convention: **uppercase = concrete type**
(`Int`, `Str`, `Person`), **lowercase = variable or string literal**.

| Entry form | Variant kind | Meaning |
|------------|-------------|---------|
| `[ok: a]` | Structural record | Closed dict with key `"ok"`, value of type `a` |
| `ok` | Structural unit | String literal `"ok"` |
| `[Ok a]` | Nominal payload | Constructor `Ok` wrapping a value of type `a` |
| `None` | Nominal unit | Constructor `None` with no payload |

```tinct
# Pure structural (existing)
Status:  [union ok err pending]
Result:  [union [ok: a] [err: Str]]

# Pure nominal (new)
Option:  [union [Some a] None]
Either:  [union [Left a] [Right b]]
Color:   [union Red Green Blue]

# Mixed — structural and nominal variants in one union
Outcome: [union
    [ok: a]          # structural: plain dict, JSON-transparent
    [Err Str]        # nominal: opaque error wrapper
    pending]         # structural unit: string literal "pending"
```

### Construction

**Structural variants** are constructed by dict literal — no change from
`doc/whatif/algebraic-data-types.md`:

```tinct
success: [ok: 42]     # plain dict, structural Ok variant
status:  pending      # plain string, structural unit variant
```

**Nominal variants** are constructed by calling the registered constructor function.
Each constructor name (`Some`, `None`, `Ok`, `Err`, `Left`, `Right`, etc.) is
registered as a builtin function when its `[union ...]` type is declared:

```tinct
wrapped:  [call Some 42]       # → Variant { tag: "Some", payload: 42 }
empty:    None                 # → Variant { tag: "None", payload: None }
colored:  Red                  # → Variant { tag: "Red", payload: None }

# Constructor functions are first-class values
wrapped-items: [call $map Some $items]   # wraps each item in Some
```

Unit constructors (`None`, `Red`, `Blue`) are values, not calls — the bare uppercase
word in value position evaluates to the unit variant. This matches how string
literals evaluate in value position (`ok` → `"ok"`) but for nominal unit variants.

### Pattern Matching

`[match]` patterns use the same case rule to distinguish structural from nominal:

```lisp
[match $x
    [ok: $v]   ...    # structural dict pattern: dict with key "ok", bind value to $v
    [Ok $v]    ...    # nominal constructor pattern: Ok wrapping payload, bind to $v
    ok         ...    # structural unit: matches string "ok"
    None       ...]   # nominal unit: matches None constructor

# Option type — full pattern coverage
[match $maybe
    [Some $v]  $v
    None       default-value]

# Either — payload patterns nest
[match $either
    [Left $a]   [call $handle-left $a]
    [Right $b]  [call $handle-right $b]]

# Tree — recursive nominal ADT (Phase 4 of algebraic-data-types.md)
[match $tree
    Leaf            0
    [Node $v $l $r] [call $+ 1 [call $+ [call $depth $l] [call $depth $r]]]]
```

The structural vs nominal distinction in patterns is visually unambiguous:
- `[lowercase: $binding]` — dict field pattern (key, colon, binding)
- `[Uppercase $binding]` — constructor pattern (tag, space, binding, no colon)
- `lowercase` — string literal match
- `Uppercase` — unit constructor match

### Runtime Value

A new `Value::Variant` variant in the evaluator:

```rust
pub enum Value {
    // ... existing variants ...
    Variant {
        tag: String,            // constructor name: "Some", "None", "Ok", "Err"
        payload: Option<Rc<Thunk>>,  // None for unit constructors
    },
}
```

`$type-of` returns `"Variant"` for all nominal variant values, consistent with
returning `"Dict"`, `"Int"`, `"Str"`, etc. The constructor tag is accessible via
a new `$tag-of` builtin that returns the tag name as a string, enabling interop
with code that cannot use pattern matching.

### Serialization

Nominal variants serialize to JSON as tagged dicts, matching the structural ADT
convention where possible:

| Variant | JSON output |
|---------|------------|
| `[call Some 42]` | `{"Some": 42}` |
| `None` | `{"None": null}` |
| `[call Ok [call $+ 1 2]]` | `{"Ok": 3}` |
| `Red` | `{"Red": null}` |
| `[call Left [call Some 42]]` | `{"Left": {"Some": 42}}` |

The serialization is identical to what a structural ADT would produce for the same
shape — `[call Some 42]` serializes as `{"Some": 42}`, the same as the structural
dict `[Some: 42]`. This preserves round-trip compatibility with external JSON
consumers that don't know about tinct's variant system.

`$from-json` does **not** automatically reconstruct nominal variants from JSON.
External JSON `{"Some": 42}` becomes the structural dict `[Some: 42]` (a plain dict
with key `"Some"`). TypeAssert (`[@Option $json-value]`) would fail if `Option` uses
nominal variants. This is deliberate — nominality requires explicit construction,
not automatic inference from shape.

### Interaction with Structural ADTs

Nominal and structural variants are **separate type-system concepts** that share
the `[union ...]` declaration form. They do not interconvert:

- A structural `[ok: 42]` dict is **not** a nominal `Ok 42` variant, even if the
  tag names match (modulo case).
- A nominal `[call Ok 42]` value is **not** a dict — `$result.ok` is a type error.

This separation is what makes nominal variants worth having. If they interconverted,
the nominality guarantee (only constructors create variant values) would be lost.

Mixed unions (`Outcome: [union [ok: a] [Err Str] pending]`) are valid. Nominal arms
in `[match]` check for `Value::Variant { tag }`, structural arms check for `Value::Dict`
or string equality. No ambiguity at runtime because `Value::Variant` and `Value::Dict`
are distinct runtime types.

### Interaction with Type System

The type-level representation adds `Type::NominalVariant(tag: String, payload: Option<Box<Type>>)`.
A union containing nominal constructors expands to:

```
Option a = Type::Union([
    NominalVariant("Some", Some(TypeVar("a"))),
    NominalVariant("None", None),
])
```

`is_subtype(NominalVariant("Some", Int), Union([NominalVariant("Some", a), NominalVariant("None", None)]))` succeeds by `[UNION-INJ-L]` with `a = Int`. `NominalVariant` is **never** a subtype of `Record` — nominal and structural are distinct in subtyping.

Constructor type signatures are registered in the type environment:

```
Some : Fn@[Option a] [a]
None : [Option a]           (unit — a value, not a function)
```

### Lazy Evaluation

Nominal variant construction via `[call Some $thunk]` wraps the payload as a thunk
— the payload is not forced at construction time. Pattern matching forces the
*discriminant* (the constructor tag) but not the payload until the body uses it:

```lisp
[match [call Some [call $/ 1 0]]   # division-by-zero in payload — not forced here
    [Some $v]  0                    # body ignores $v — division never executed
    None       1]
```

This follows the same lazy semantics as structural dict pattern matching
(`doc/whatif/pattern-matching.md` §Lazy Evaluation): only accessed fields/payloads
are forced.

## What Would Change

### Grammar (`src/grammar.pest`)

**Current:** No nominal variant syntax. Uppercase bare words in value position are
strings. `[Uppercase ...]` inside `[]` would parse as a dict with positional entries.

**Proposed:** In `[union ...]` declaration position, the parser distinguishes
uppercase entries as nominal constructor declarations. In `[match]` pattern position,
`[Uppercase $binding]` is a constructor pattern and uppercase bare words are unit
constructor patterns. In value expression position, uppercase bare words that name
registered constructors evaluate to unit variant values (similar to how `true` and
`false` evaluate to booleans).

**Impact:** Moderate. New parsing rules required in three positions: union
declaration, match arm pattern, value expression. Uppercase bare word disambiguation
requires symbol table lookup during parsing — the parser must know which uppercase
names are constructors. This is the same mechanism used for `true`/`false`/`null`.

### AST (`src/ast.rs`)

**Current:** `Pattern` enum (from `doc/whatif/pattern-matching.md`) has no
constructor pattern variant.

**Proposed:** Add `Pattern::Constructor { tag: String, binding: Option<Box<Spanned<Pattern>>> }`
for nominal patterns. Add `Expr::Constructor { tag: String, payload: Option<Box<Spanned<Expr>>> }`
for unit constructor values in expression position (unit variants as literals, parallel
to `Expr::Bool`).

**Impact:** Minor. Two new AST variants in well-isolated positions.

### Value Representation (`src/value.rs`)

**Current:** No `Value::Variant`. Nominal variants cannot be represented.

**Proposed:** Add `Value::Variant { tag: String, payload: Option<Rc<Thunk>> }`.
Extend `$type-of` to return `"Variant"`. Add `$tag-of` builtin returning the
constructor tag as a string. Extend serialization (`value_to_json`) with the
tagged-dict encoding.

**Impact:** Moderate. `Value` gains a new variant; every exhaustive `match` on
`Value` must handle it. Serialization and display gain new cases. `$type-of`,
`$tag-of` are new builtins.

### Type Representation (`src/types.rs`)

**Current:** No `Type::NominalVariant`.

**Proposed:** Add `Type::NominalVariant { tag: String, payload: Option<Box<Type>> }`.
Constructor signatures are registered in the type environment at union declaration
time. `is_subtype` gains rules for `NominalVariant` (never a subtype of `Record`,
subtype of a `Union` containing the matching `NominalVariant`).

**Impact:** Moderate. New type variant, new subtype rules, new constructor
registration logic.

### Type Checker (`src/typecheck.rs`)

**Current:** No handling for constructor patterns or nominal variant types.

**Proposed:** At `[union ...]` declaration time with nominal entries: register
constructor functions in the type environment (`Some : Fn@[Option a] [a]`, `None :
[Option a]`). In `[match]` arm type-checking: for `[Some $v]` patterns, narrow
the scrutinee to `NominalVariant("Some", _)` and bind `$v` to the payload type.
Exhaustiveness (Phase 2) checks that nominal constructor arms cover all constructors.

**Impact:** Moderate.

### Evaluator (`src/eval.rs`)

**Current:** No constructor application or nominal variant dispatch.

**Proposed:** Constructor calls `[call Some 42]` are handled as builtin-style
calls: the evaluator looks up `Some` in the environment, finds a constructor entry,
and creates `Value::Variant { tag: "Some", payload: Some(thunk) }`. `[match]` arm
evaluation: for `Pattern::Constructor`, materialize the scrutinee, check if it is
`Value::Variant { tag }` with the matching tag, bind the payload thunk to the
pattern variable.

**Impact:** Moderate. Constructor application is a new evaluation path; constructor
pattern matching is a new case in the pattern evaluator.

## Phased Adoption

### Phase 1: Unit Constructors and `$tag-of`

Add `Value::Variant { tag, payload: None }`, unit constructor values, and `$tag-of`.
No payload constructors, no pattern matching yet. Unit constructors are usable as
enum-like values:

```tinct
Color:    [union Red Green Blue]
selected: Red                           # Value::Variant { tag: "Red", payload: None }
name:     [call $tag-of $selected]      # → "Red"
is-red:   [call $= [call $tag-of $selected] Red]  # → true
```

This is independently useful without pattern matching: `$tag-of` enables dispatch
via `$cond` chains. Unit constructors can replace some string enumerations where
the nominal guarantee (only declared values are valid) is desired.

**Prerequisites:** `doc/whatif/algebraic-data-types.md` Phase 1 (convention
established). `Value::Variant` runtime type. Serialization as tagged dict.

### Phase 2: Payload Constructors and Pattern Matching

Add `Value::Variant { tag, payload: Some(_) }`, payload constructor registration,
and `Pattern::Constructor` matching in `[match]`. This is the full nominal variant
system:

```tinct
Option: [union [Some a] None]

lookup: [fn [dict@[...] key@Str]
    [call $if [call $has? $dict $key]
        [call Some [call $get $dict $key]]
        None]]

# Pattern match on the result
found: [match [call lookup $config timeout]
    [Some $v]   $v
    None        30]      # default
```

**Prerequisites:** Phase 1 complete. `doc/whatif/pattern-matching.md` Phase 2
(basic `[match]` with type/literal patterns). Constructor functions registered in
the type environment.

### Phase 3: Exhaustiveness for Nominal Unions

Exhaustiveness checking in `[match]` for unions containing nominal constructors:

```lisp
[match $maybe         # Option a
    [Some $v]  $v]   # Error: non-exhaustive — missing arm for None
```

This reuses the exhaustiveness infrastructure from `doc/whatif/algebraic-data-types.md`
Phase 3. Nominal constructors are a finite, statically-known set, making
exhaustiveness checking straightforward.

**Prerequisites:** Phase 2 complete. `doc/whatif/pattern-matching.md` Phase 5
(exhaustiveness infrastructure). `doc/whatif/algebraic-data-types.md` Phase 3.

### Prerequisites

| Phase | Prerequisites |
|-------|--------------|
| Phase 1 | `algebraic-data-types.md` Phase 1 |
| Phase 2 | Phase 1 complete; `pattern-matching.md` Phase 2 |
| Phase 3 | Phase 2 complete; `pattern-matching.md` Phase 5; `algebraic-data-types.md` Phase 3 |

### Trigger

**Phase 1** (unit constructors): adopt when:
- Structural tag-only variants (`Status: [union ok err pending]`) cause confusion
  because string values can accidentally satisfy them
- Any declared "enum" needs to be provably confined to declared values

**Phase 2** (payload constructors): adopt when:
- Two constructor shapes would be identical under structural discrimination
- `$map Ok $items` / `$map Some $items` patterns are needed in stdlib or user code
- tinct programs use pattern matching heavily enough that mandatory elimination is valued

**Phase 3** (exhaustiveness): adopt together with `algebraic-data-types.md` Phase 3 —
they share the same type-checker infrastructure and should ship together.

## References

- Rémy, D. (1989). "Typechecking records and variants in a natural extension of
  ML." In *POPL '89*, pp. 77–88. ACM. — Records and variants as dual row types.
  Nominal constructors correspond to the "present" variant tag in Rémy's full system,
  where each constructor name appears in the variant row with a presence flag.
- Garrigue, J. (1998). "Programming with polymorphic variants." In *ML Workshop
  '98*. — OCaml's polymorphic variants use structural discrimination (`` `Foo `` is
  a tag, not a constructor). tinct's nominal variants are closer to OCaml's standard
  (nominal) variants than to polymorphic variants — the tag is opaque, the
  constructor is the only way to produce the value.
- Kennedy, A. & Russo, C. (2005). "Generalized algebraic data types and
  object-oriented programming." In *OOPSLA '05*, pp. 21–40. ACM. — GADT
  constructors as the general case of nominal variants. tinct's nominal variants are
  the monomorphic / non-GADT case; this paper provides the theoretical ceiling
  if the type system eventually needs constructor-level type refinement.
- Blume, M., Acar, U.A. & Chae, W. (2006). "Extensible programming with first-class
  cases." In *ICFP '06*, pp. 239–250. ACM. — First-class cases and extensible
  variants. Constructor functions as first-class values (Phase 2) follow the
  first-class case model where constructors are values in the expression language.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Chapter 11
  (variants as labeled sum types) and Chapter 23 (universal types and type
  abstraction for opaque types). — Standard formulation of nominal variants as
  labeled injections into a sum type. Constructor `Ok` as `inl : a → a + b`.
- Wadler, P. (1989). "Theorems for free!" In *FPCA '89*, pp. 347–359. ACM. —
  Parametricity: a function polymorphic in `a` cannot inspect the contents of
  `[Some a]` without pattern matching. Motivates why nominal variants with opaque
  payloads are the natural pairing with polymorphic type parameters.
