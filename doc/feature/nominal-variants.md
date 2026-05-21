# Nominal Variants

## Overview

Nominal variants extend structural ADTs with opaque, constructor-based variant
values. Where structural ADTs discriminate by key set (any dict with the right
keys satisfies the variant), nominal variants discriminate by constructor tag —
a `Value::Variant` is only created by applying a named constructor. This enables
payload-identical constructors (`Left a` and `Right a` with the same payload
type), first-class constructor functions (`[map Some items]`), and mandatory
elimination via pattern matching (`result.ok` is a type error on a
`Value::Variant`).

## Supersession Notes

- **Nominal variants are required, not optional**: Before [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09), structural key-set ADTs and nominal variants were alternatives. Under BAS, S-RcdTop collapses disjoint single-field record unions to `Top`, making nominal variants the only viable mechanism for discriminated unions in the type system. `Value::Variant` and `Pattern::Constructor` are implemented at runtime; `Type::NominalVariant` is tracked as a pending type-system addition.
- **`payload` field type**: The feature doc shows `payload: Option<Rc<Thunk>>` but the arena migration (see [arena-patterns.md](arena-patterns.md)) changed this to `Option<ThunkId>`.

## Design

### Extending `[type ...]` with Case

Nominal and structural variants coexist in a single `[type ...]` form, discriminated
by case. This reuses tinct's existing convention: **uppercase = concrete type**
(`Int`, `Str`, `Person`), **lowercase = variable or string literal**.

| Entry form | Variant kind | Meaning |
|------------|-------------|---------|
| `[ok: a]` | Structural record | Closed dict with key `"ok"`, value of type `a` |
| `ok` | Structural unit | String literal `"ok"` |
| `[Ok a]` | Nominal payload | Constructor `Ok` wrapping a value of type `a` |
| `None` | Nominal unit | Constructor `None` with no payload |

```tinct
# Pure structural (existing) — quoted strings for tag-only variants
Status:  [type "ok" "err" "pending"]
Result:  [type [ok: a] [err: Str]]

# Pure nominal (new) — uppercase bare words/forms are nominal constructors
Option:  [type [Some a] None]
Either:  [type [Left a] [Right b]]
Color:   [type Red Green Blue]

# Mixed — structural and nominal variants in one type
Outcome: [type
    [ok: a]          # structural: plain dict, JSON-transparent
    [Err Str]        # nominal: opaque error wrapper
    "pending"]       # structural unit: string literal "pending"
```

### Construction

**Structural variants** are constructed by dict literal — no change from
`doc/feature/algebraic-data-types.md`:

```tinct
success: [ok: 42]     # plain dict, structural Ok variant
status:  "pending"    # string value, structural unit variant
```

**Nominal variants** are constructed via constructor values registered in the
environment when a `[type ...]` declaration is evaluated. Unit constructors are
bound directly to `Value::Variant` values; payload constructors are bound to
closures that wrap their argument:

```tinct
wrapped:  [Some 42]       # → Variant { tag: "Some", payload: 42 }
empty:    None            # → Variant { tag: "None", payload: None }
colored:  Red             # → Variant { tag: "Red", payload: None }

# Constructor functions are first-class values
wrapped-items: [map Some items]   # wraps each item in Some
```

Unit constructors (`None`, `Red`, `Blue`) are values, not calls — the bare uppercase
word in value position evaluates to the unit variant. This matches how string
literals evaluate in value position (`ok` → `"ok"`) but for nominal unit variants.

### Pattern Matching

`[match]` patterns use the same case rule to distinguish structural from nominal:

```tinct
[match x
    [ok: v]:   ...    # structural dict pattern: dict with key "ok", bind value to v
    [Ok v]:    ...    # nominal constructor pattern: Ok wrapping payload, bind to v
    "ok":      ...    # structural unit: matches string "ok"
    None:      ...]   # nominal unit: matches None constructor

# Option type — full pattern coverage
[match maybe
    [Some v]:  v
    None:      default-value]

# Either — payload patterns nest
[match either
    [Left a]:   [handle-left a]
    [Right b]:  [handle-right b]]

# Tree — recursive nominal ADT
[match tree
    Leaf:          0
    [Node v l r]:  [+ 1 [+ [depth l] [depth r]]]]
```

The structural vs nominal distinction in patterns is visually unambiguous:

- `[lowercase: binding]` — dict field pattern (key, colon, binding)
- `[Uppercase binding]` — constructor pattern (tag, space, binding, no colon)
- `lowercase` — string literal match
- `Uppercase` — unit constructor match

### Runtime Value

A `Value::Variant` variant in the evaluator:

> **⚠ Updated by [arena-patterns.md](arena-patterns.md) (2026-05-04):** The arena migration changed `payload` from `Option<Rc<Thunk>>` to `Option<ThunkId>`.

```rust
pub enum Value {
    // ... existing variants ...
    Variant {
        tag: String,            // constructor name: "Some", "None", "Ok", "Err"
        payload: Option<Rc<Thunk>>,  // None for unit constructors
    },
}
```

`type-of` returns `"Variant"` for all nominal variant values, consistent with
returning `"Dict"`, `"Int"`, `"Str"`, etc. The constructor tag is accessible via
`tag-of`, which returns the tag name as a string, enabling interop with code that
cannot use pattern matching.

### Serialization

Nominal variants serialize to JSON as tagged dicts, matching the structural ADT
convention where possible:

| Variant | JSON output |
|---------|------------|
| `[Some 42]` | `{"Some": 42}` |
| `None` | `{"None": null}` |
| `[Ok [+ 1 2]]` | `{"Ok": 3}` |
| `Red` | `{"Red": null}` |
| `[Left [Some 42]]` | `{"Left": {"Some": 42}}` |

`from-json` does **not** automatically reconstruct nominal variants from JSON.
External JSON `{"Some": 42}` becomes the structural dict `[Some: 42]` (a plain dict
with key `"Some"`). TypeAssert (`[@Option json-value]`) fails if `Option` uses
nominal variants. This is deliberate — nominality requires explicit construction,
not automatic inference from shape.

### Interaction with Structural ADTs

Nominal and structural variants are **separate type-system concepts** that share
the `[type ...]` declaration form. They do not interconvert:

- A structural `[ok: 42]` dict is **not** a nominal `Ok 42` variant, even if the
  tag names match (modulo case).
- A nominal `[Ok 42]` value is **not** a dict — `result.ok` is a type error.

This separation is what makes nominal variants worth having. If they interconverted,
the nominality guarantee (only constructors create variant values) would be lost.

Mixed types (`Outcome: [type [ok: a] [Err Str] "pending"]`) are valid. Nominal arms
in `[match]` check for `Value::Variant { tag }`, structural arms check for `Value::Dict`
or string equality. No ambiguity at runtime because `Value::Variant` and `Value::Dict`
are distinct runtime types.

### Interaction with Type System

The type-level representation adds `Type::NominalVariant(tag: String, payload: Option<Box<Type>>)`.
A union containing nominal constructors expands to:

```rust
Option a = Type::Union([
    NominalVariant("Some", Some(TypeVar("a"))),
    NominalVariant("None", None),
])
```

`is_subtype(NominalVariant("Some", Int), Union([NominalVariant("Some", a), NominalVariant("None", None)]))` succeeds by `[UNION-INJ-L]` with `a = Int`. `NominalVariant` is **never** a subtype of `Record` — nominal and structural are distinct in subtyping.

Constructor type signatures are registered in the type environment:

```text
Some : Fn@[Option a] [a]
None : [Option a]           (unit — a value, not a function)
```

### Lazy Evaluation

Nominal variant construction via `[Some thunk]` wraps the payload as a thunk
— the payload is not forced at construction time. Pattern matching forces the
*discriminant* (the constructor tag) but not the payload until the body uses it:

```tinct
[match [Some [/ 1 0]]   # division-by-zero in payload — not forced here
    [Some v]:  0        # body ignores v — division never executed
    None:      1]
```

This follows the same lazy semantics as structural dict pattern matching
(`doc/feature/pattern-matching.md` §Lazy Evaluation): only accessed fields/payloads
are forced.

## Implementation

### Grammar (`src/grammar.pest`)

> **⚠ Stale reference:** The parser was rewritten as a hand-written iterative parser in `src/parser.rs` — `src/grammar.pest` no longer exists. See [parser-rewrite.md](parser-rewrite.md) (2026-04-30).

In `[type ...]` declaration position, the parser distinguishes uppercase entries as
nominal constructor declarations. In `[match]` pattern position, `[Uppercase binding]`
is a constructor pattern and uppercase bare words are unit constructor patterns. In
value expression position, uppercase bare words that name registered constructors
evaluate to unit variant values (similar to how `true` and `false` evaluate to
booleans).

Impact: Low–Moderate. New parsing rules in two positions: union declaration
(uppercase entries are nominal constructors) and match arm pattern (`[Uppercase binding]`
is a constructor pattern). In value expression position, no parser change is needed —
constructor names are regular variable references (`Expr::VarRef`) that resolve to
constructor values in the environment at eval time.

### AST (`src/ast.rs`)

Add `Pattern::Constructor { tag: String, binding: Option<Box<Spanned<Pattern>>> }`
for nominal patterns. No new `Expr` variant is needed — constructor names in
expression position are regular `Expr::VarRef` nodes that resolve to constructor
values (unit variants or constructor closures) in the environment. This is the
standard ML/Haskell approach: constructors are values, not special syntax.

Impact: Minor. One new AST variant (`Pattern::Constructor`), well-isolated.

### Value Representation (`src/value.rs`)

`Value::Variant { tag: String, payload: Option<Rc<Thunk>> }` is added.
`type-of` returns `"Variant"`. `tag-of` builtin returns the constructor tag as a
string. Serialization (`value_to_json`) gains the tagged-dict encoding.

Impact: Moderate. `Value` gains a new variant; every exhaustive `match` on
`Value` must handle it. Serialization and display gain new cases. `type-of`,
`tag-of` are new builtins.

### Type Representation (`src/types.rs`)

`Type::NominalVariant { tag: String, payload: Option<Box<Type>> }` is added.
Constructor signatures are registered in the type environment at union declaration
time. `is_subtype` gains rules for `NominalVariant` (never a subtype of `Record`,
subtype of a `Union` containing the matching `NominalVariant`).

Impact: Moderate. New type variant, new subtype rules, new constructor
registration logic.

### Type Checker (`src/typecheck.rs`)

At `[type ...]` declaration time with nominal entries: register constructor
functions in the type environment (`Some : Fn@[Option a] [a]`, `None :
[Option a]`). In `[match]` arm type-checking: for `[Some v]` patterns, narrow
the scrutinee to `NominalVariant("Some", _)` and bind `v` to the payload type.
Exhaustiveness checking verifies that nominal constructor arms cover all constructors.

Impact: Moderate.

### Evaluator (`src/eval.rs`)

When a `[type ...]` declaration with nominal entries is evaluated, register
constructor values in the environment:

- Unit constructors (`None`, `Red`): bind to `Value::Variant { tag, payload: None }`
- Payload constructors (`Some`, `Ok`): bind to a closure
  `fn(x) → Value::Variant { tag, payload: Some(x) }`

Constructor calls like `[Some 42]` are regular function application — the
evaluator looks up `Some` via `Expr::VarRef`, finds the constructor closure,
and applies it. No special evaluation path is needed.

`[match]` arm evaluation: for `Pattern::Constructor`, materialize the scrutinee,
check if it is `Value::Variant { tag }` with the matching tag, bind the payload
thunk to the pattern variable.

Impact: Low–Moderate. Constructor registration at `[type]` declaration time
is new. Constructor application reuses existing function call machinery. Constructor
pattern matching is a new case in the pattern evaluator.

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
  variants. Constructor functions as first-class values follow the first-class case
  model where constructors are values in the expression language.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Chapter 11
  (variants as labeled sum types) and Chapter 23 (universal types and type
  abstraction for opaque types). — Standard formulation of nominal variants as
  labeled injections into a sum type. Constructor `Ok` as `inl : a → a + b`.
- Wadler, P. (1989). "Theorems for free!" In *FPCA '89*, pp. 347–359. ACM. —
  Parametricity: a function polymorphic in `a` cannot inspect the contents of
  `[Some a]` without pattern matching. Motivates why nominal variants with opaque
  payloads are the natural pairing with polymorphic type parameters.
