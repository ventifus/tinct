# What If: User-Defined N-Arity Type Constructors

**State:** Proposal

What would it take to let users declare their own parameterized type constructors — with arbitrary arity — and have them work identically to builtins like `Seq` and `Map` in annotations, typeclass constraints, and inference?

## Current State

`[type ...]` is overloaded across three distinct uses that share no consistent syntax rule, making the system hard to learn and the docs inconsistent with the prelude.

### Use 1 — Type alias (structural expansion)

`[type Body]` where body is a structural type expression. The name is a shorthand; the type expands on every use:

```tinct
Name: [type String]                          # alias for String
Config: [type [host: String  port: Int]]     # alias for a record type
```

### Use 2 — Nominal ADT (creates constructors)

`[type Ctor1 Ctor2 ...]` where each entry is either a bare uppercase word (unit constructor) or a bracketed form (constructor with payload). The name is the type identity; it does not expand:

```tinct
Signal: [type SIGTERM SIGINT SIGHUP]         # unit constructors
Result: [type [Ok a] [Error String]]         # payload constructors; `a` is implicit TypeVar
Span:   [type [Span start-line: Int  start-col: Int  end-line: Int  end-col: Int]]
```

### Use 3 — Parameterized type (implicit TypeVar parameters)

When lowercase identifiers appear in constructor field positions, they are treated as implicit type parameters. This works but provides no way to declare variance or create opaque/builtin-backed types:

```tinct
Maybe:  [type [Some a] None]                 # implicit param `a`
Result: [type [Ok a] [Error String]]         # implicit param `a`
```

### What's broken today

**Syntax inconsistencies** across docs and prelude:

- Unit constructors are written as `Red` (bare word) in prelude but `[Red]` (bracketed) in quickstart docs. Semantically identical, syntactically inconsistent.
- Parameterized aliases use `[a b]` as a "parameter list" at the head of the type body, which is visually indistinguishable from a constructor named `a`.
- The `record:` keyword inside type bodies is optional but undocumented — `[type [record host: String]]` and `[type [host: String]]` both work but the difference isn't explained.
- The inline form `[type Color Red Green Blue]` (name inside) and the dict-entry form `Color: [type Red Green Blue]` (name as key) both exist; which to prefer is undocumented.

**Semantic gaps:**

1. Users cannot declare a nominal parameterized type — one where the name is the type identity regardless of structure. Today all parameterized types with bodies are transparent aliases.
2. Users cannot declare type constructors with variance (covariant, contravariant, phantom).
3. Opaque type constructors (for builtin-backed types like `Seq`, `Map`, `Handle`) cannot be declared in tinct source — forcing the Rust type checker to maintain a parallel string-matching dispatch table (`apply_builtin_constructor`, `resolve_type_dict` builtin arms) that must be kept in sync with any future builtin additions. Moving declarations to prelude deletes this Rust code entirely.
4. Builtins are hardcoded in Rust with string-matching special-cases, not declarable uniformly.

### What's Missing

1. A single, coherent disambiguation rule for all `[type ...]` forms.
2. Explicit type parameter declaration that can carry variance annotations.
3. Opaque type constructors (parameter declaration, no body).
4. Nominal parameterized ADTs (parameter declaration + constructors = non-expanding name).
5. Uniform type representation removing `apply_builtin_constructor` and the builtin string-match.
6. Variance-directed BAS subtyping for all type constructor applications.

## Why User Type Constructors Matter for Tinct

- **Typeclass instances over user types.** A user can declare `Tree A` and write `[instance [Functor Tree] [fmap: ...]]`. Today only builtins like `Seq` can be Functor instances because `Functor` requires an Operator-kinded type constructor.

- **No more special-casing in the type system.** The string-matching `apply_builtin_constructor` function and `resolve_type_dict` builtin arms disappear. `Seq`, `Map`, and `Handle` are declared in prelude just like user types.

- **`Contravariant` typeclass becomes expressible.** `Predicate`, `Handler`, `Comparator` — types that consume values — can be declared with `A@Contravariant` and participate in the `Contravariant` class. Without variance, the type checker can't reason about consumer-position parameters at all.

- **`Profunctor` typeclass.** Optics (lenses, prisms, traversals) require a type that is contravariant in one parameter and covariant in another: `[type [Optic S@Contravariant A@Covariant]]`. Without this the `Profunctor` class and all lens-style composition is inexpressible.

- **Annotation syntax becomes uniform.** `@[Tree Int]`, `@[Map String Int]`, `@[Seq Bool]` all go through the same path.

- **Algebraic data types with type parameters.** `[type [Result A B] [Ok value: A] [Err error: B]]` becomes a genuine 2-parameter type constructor, not just a name for a union body.

## Design

### Unified `[type ...]` Syntax

A single rule governs all `[type ...]` forms based on what appears in the body:

| Body content | Kind | Expanding? |
|---|---|---|
| Structural type expression (lowercase field dict, existing type name) | transparent alias | yes |
| Uppercase bare words and/or `[UpperName ...]` bracket forms | nominal ADT | no |
| `[let ...]` followed by `...` | opaque constructor | no |
| `[let ...]` + structural body | parameterized transparent alias | yes |
| `[let ...]` + constructors | parameterized nominal ADT | no |

**Three rules that resolve every ambiguity:**

1. **`[let ...]` declares type parameters** — borrowed from function declarations (`[fn [let x y] body]`). When present, it is always the first entry, and it unambiguously signals "these are the bound type names for this scope." No `[let ...]` = no explicit params (either non-parameterized, or implicit TypeVars).

2. **Unit constructors are bare uppercase words.** A bracketed form `[UpperName ...]` is a constructor with a payload (named fields). A bare `UpperName` is a unit constructor. No brackets around unit constructors.

3. **Opaque types use `...` as the body.** `...` (the placeholder) signals "the body exists in Rust, not tinct — I cannot express it here." This is consistent with `...` meaning "implementation deferred" elsewhere in tinct. The three forms are then visually unambiguous: `...` = opaque, constructor list = nominal, structural expression = transparent alias.

The `record:` keyword inside type bodies is removed — `[field: T ...]` (lowercase key, colon, type) is always and unambiguously a field dict (record type). The `record` prefix was redundant.

**The dict-entry form is the only way to bind a type name.** `Name: [type ...]` is canonical — the name is the dict key, exactly like all other tinct bindings. The inline form `[type TypeName ...]` (name as first argument to `type`) is retired. There are no edge cases: mutual recursion works because both types go in the same dict (letrec-scoped), and local types inside function bodies use intermediate dict entries.

```tinct
# ── TRANSPARENT ALIASES ───────────────────────────────────────────────────────

Name:    [type String]                       # simple alias
Config:  [type [host: String  port: Int]]    # alias for a record type

Pair:    [type [let a b]  [first: a  second: b]]   # parameterized alias — expands
Either:  [type [let a b]  [or a b]]                # parameterized alias using `or`

# ── NOMINAL ADTs (non-parameterized) ─────────────────────────────────────────

Signal: [type SIGTERM SIGINT SIGHUP]         # unit constructors — bare uppercase words
Result: [type [Ok a] [Error String]]         # payload constructors; a is implicit TypeVar

Span:   [type [Span                          # single constructor with named fields
  start-line: Int  start-col:  Int
  end-line:   Int  end-col:    Int]]

Annotation: [type                            # multiple named-field constructors
  [Simple       text: String  name: String]
  [PropertyDict text: String  doc: Any  return: Any]
  [Annotated    text: String  name: String  inner: String]]

# ── PARAMETERIZED NOMINAL ADTs ────────────────────────────────────────────────

Either: [type [let a b]                      # [let ...] = explicit params; non-expanding
  [Left  value: a]
  [Right value: b]]

Tree:   [type [let a@Covariant]              # variance annotation on param
  Leaf
  [Node value: a  left: [Tree a]  right: [Tree a]]]

Tagged: [type [let k@Phantom a@Covariant]    # phantom + covariant
  [Tagged value: a]]

# ── OPAQUE (params + ...; body exists in Rust, not expressible in tinct) ─────

Map:    [type [let a b@Covariant] ...]       # runtime-backed; not inductively structured
Handle: [type [let a] ...]                   # OS resource; values from builtins only

# ── BUILTINS DECLARED IN PRELUDE (in --- stage: type block) ──────────────────

Seq:    [type [let a@Covariant]              # nominal — Seq is inductively structured
  Nil
  [Cons head: a  tail: [Seq a]]]
```

**Variance on parameters** — ImmediateAt annotation, same as `f@Operator` on class params:

```tinct
#  @Covariant      F a <: F b when a <: b  (producer/container)
#  @Contravariant  F a <: F b when b <: a  (consumer/handler)
#  @Phantom        F a <: F b always       (type-level tag only)
#  (none)          invariant               (default for opaque params)
```

**Using parameterized types in annotations** — `@[Name Args...]`, the same form that builtins already use:

```tinct
xs@[Tree Int]: ...
f@[Either String Bool]: ...
h@[Handle DirCap]: ...

# Inline TypeAssert
[@[Seq Int] some-value]
```

**Why `[let ...]` for params?** It mirrors function declarations exactly:

```tinct
[fn  [let x@Int y@String]  body]    # fn params declared with [let ...]
[type [let a@Covariant b]  ctors]   # type params declared with [let ...]
```

Both use `[let ...]` to bind names into a scope. Both allow `@` annotations on those names. The cognitive model is the same.

### Builtin Declarations

**`Seq` — nominal** (inductively structured):
```tinct
Seq: [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
```
`Nil` = `Value::Dict(empty)`. `Cons` = `Value::Seq{head, tail}`. `cons` builtin becomes alias for `Cons`. Spread pattern `[h ...t]` remains as sugar.

**`Map` — opaque** (not inductively structured; key-value collections are not spine-recursive):
```tinct
Map: [type [let a b@Covariant] ...]
```

**`Handle` — opaque** (OS resource; no user-writable constructor exists):
```tinct
Handle: [type [let a] ...]
```

### Type Representation

Replace all dedicated collection variants with `Type::App` chains using a new `Type::TyCon(String)` constructor node:

```rust
pub enum Type {
    // ... existing variants ...

    // Replaces Type::Seq(Box<Type>), Type::Map(Box<Type>, Box<Type>), Type::Handle(Box<Type>)
    TyCon(String),                          // a type constructor: Seq, Map, Tree, etc.
    App(Box<Type>, Box<Type>),              // already exists — curried application
    // App(App(TyCon("Map"), K), V) represents Map K V
}
```

`Type::Seq(T)` becomes `Type::App(Type::TyCon("Seq"), T)`.
`Type::Map(K, V)` becomes `Type::App(Type::App(Type::TyCon("Map"), K), V)`.

Transparent aliases continue to expand to their body — they produce no `TyCon` node.

The `Type::App` variant already exists. `Type::TyCon(String)` is the only new variant required. The existing `Type::Operator(String)` remains for type constructor *variables* (class params like `f` in `[class [f@Operator] ...]`); `TyCon` is for *names* (concrete constructors).

### Annotation Resolution

`resolve_type_dict` replaces the builtin string-match and the `kind_env` lookup with a single general path: check the type environment for a registered constructor by name, retrieve its arity, collect that many type arguments, and produce the appropriate type.

For builtins (declared with a Rust-backed TyCon): produce `Type::App(TyCon("Seq"), arg)` etc.  
For user-defined opaques: produce `Type::App(TyCon("Tree"), arg)` etc.  
For transparent aliases: expand body (existing path, unchanged).

`apply_builtin_constructor` is deleted. No builtin needs special construction — `App(TyCon(name), args...)` is the representation for all of them.

### Unification

`UNIFY-TYCON`: Two `TyCon` nodes unify iff they have the same name. No binding occurs (constructors are not variables).

`UNIFY-APP` (already exists in `src/type_unify.rs`): decompose `App(f1, a1)` and `App(f2, a2)` by unifying `f1/f2` then `a1/a2`. Combined with UNIFY-TYCON, this correctly handles:

```
unify(App(App(TyCon("Map"), K1), V1), App(App(TyCon("Map"), K2), V2))
  → unify(App(TyCon("Map"), K1), App(TyCon("Map"), K2))  [UNIFY-APP]
    → unify(TyCon("Map"), TyCon("Map"))                    [UNIFY-TYCON — succeeds]
    → unify(K1, K2)
  → unify(V1, V2)
```

```
unify(App(TyCon("Seq"), T1), App(TyCon("Map"), K, V))
  → unify(TyCon("Seq"), ...)                               [UNIFY-TYCON — fails, different names]
```

### Variance Annotations

Each type parameter is annotated with its variance using the `name@variance` ImmediateAt form — the same syntax already used for kind annotations on class params (`f@Operator`). No annotation means invariant.

| Annotation | Name | Meaning |
|---|---|---|
| `A@Covariant` | covariant | `F A <: F B` when `A <: B` — producer position |
| `A@Contravariant` | contravariant | `F A <: F B` when `B <: A` — consumer position |
| `A` (no annotation) | invariant | `F A <: F B` only when `A = B` |
| `A@Phantom` | bivariant (phantom) | `F A <: F B` always — A is type-level only |

```tinct
Seq:     [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]   # nominal
Handler: [type [let a@Contravariant]  [Handler fn: [Fn@Null [a]]]]      # nominal
Ref:     [type [let a]  [Ref value: a]]                                  # nominal, invariant
Tagged:  [type [let k@Phantom a@Covariant]  [Tagged value: a]]           # nominal
Map:     [type [let a b@Covariant] ...]                                  # opaque
```

The default for an unannotated parameter is **invariant** — the safe choice for opaque constructors whose internal use of the parameter is unknown.

For transparent aliases (those with a body), the variance can be inferred automatically from the body: if `A` appears only in covariant positions (field types, return types), the compiler infers `@Covariant`; only in contravariant positions (function argument types), `@Contravariant`; both, invariant; never, `@Phantom`. Explicit annotations can override inference and serve as a checked declaration.

### Subtyping

**Covariant (`A@Covariant`):** `F A <: F B` when `A <: B`.

In BAS: union distributes outward. `F (Int | String) = F Int | F String`. This enables:

```tinct
Seq: [type [Seq A@Covariant]]
# Seq Int <: Seq Number  (since Int <: Number)
# [fn [xs@[Seq Number]] ...] accepts a [Seq Int] argument
```

**Contravariant (`A@Contravariant`):** `F A <: F B` when `B <: A` (flipped).

In BAS: union becomes intersection. `F (Int | String) = F Int & F String` — a consumer of `Int | String` must handle both, so it's the intersection of handlers. This enables:

```tinct
Handler:   [type [Handler A@Contravariant]]
Predicate: [type [Predicate A@Contravariant]]
Comparator: [type [Comparator A@Contravariant]]

# Handler Number <: Handler Int
# (a handler that processes any Number also handles Ints — Int <: Number)

# [fn [h@[Handler Number]] [h 42]]  -- passes Int 42 to Number handler ✓
# [fn [h@[Handler Int]] [h 3.14]]   -- passes Float to Int handler ✗ (error)
```

Concrete value: a `sort-by: [fn [cmp@[Comparator A@Contravariant]  xs@[Seq A]] [Seq A]]` can accept a `Comparator Number` when sorting a `Seq Int`, because `Comparator Number <: Comparator Int` (the number comparator handles all numbers, including integers).

**Bivariant phantom (`A@Phantom`):** `F A <: F B` for any A, B. The parameter is never used at runtime — it exists only to carry type information.

```tinct
Tagged: [type [let k@Phantom a@Covariant]  [Tagged value: a]]
# Tagged UserIdTag String <: Tagged PostIdTag String  (phantom K is unconstrained)
# Useful for wrapping a String with a type-level marker without changing the runtime value
```

**Invariant (no annotation):** `F A <: F B` only if `A = B`. The only safe default for opaque constructors.

### What Full Variance Unlocks

**Without variance annotations, the following are impossible or unsound:**

1. **`Contravariant` typeclass.** The dual of `Functor` — types that consume values rather than containing them:
   ```tinct
   Contravariant: [class [f@Operator]
     [contramap: [fn@[f A] [fn@B [A]  [f B]]]]]
   ```
   Instances: `Predicate`, `Handler`, `Comparator`. Without the `-A` annotation, the type checker cannot verify that `contramap`'s argument direction is correct.

2. **Type-safe observer/callback patterns.** A `subscribe: [fn [h@[Handler Message@Contravariant]] ...]` can accept a `Handler Any` (handles everything) where a `Handler Message` is expected — sound because `Message <: Any` reverses for contravariant position.

3. **`Profunctor` typeclass** (contravariant in first arg, covariant in second):
   ```tinct
   Profunctor: [class [p@Operator]
     [dimap: [fn@[p A B] [fn@C [A]  [fn@B [D]  [p C D]]]]]
   ```
   Enables optics (lenses, prisms) as typed abstractions over field access and transformation.

4. **Phantom type safety.** `[type [let k@Phantom a@Covariant] [Tagged value: a]]` lets library code attach type-level brands to values (user IDs vs. post IDs, both strings) that the compiler enforces but that vanish at runtime — `[Tagged value: myString]` creates a `Tagged UserId String` that is structurally just a string but nominally distinct.

5. **Seq and user containers compose correctly.** `[Seq [Handler A@Contravariant]]` — a sequence of message handlers. The type checker can reason about this only if both `Seq` covariance and `Handler` contravariance are specified. Without them, the outer `Seq` can't distribute union types inward through `Handler`.

### Constructor Access and Pattern Syntax

A type declaration is a dict entry whose value is the constructor dict. The type system also registers the type name from the same dict entry. The only binding created is the dict key — the type name. Constructors are fields of that dict, accessed normally via dot.

```tinct
Color: [type Red Green Blue]
# Creates:
#   type    — Color in the type system
#   value   — Color = {Red: <Color.Red variant>, Green: <Color.Green variant>, Blue: <Color.Blue variant>}
# Does NOT create Red, Green, Blue as separate bindings.

Signal.SIGTERM    # access unit constructor
Seq.Cons          # access payload constructor function
Option.Some       # access payload constructor function
```

**Constructor access uses dot expressions — the full chain, same as anywhere in tinct.** This includes multi-level access when types are nested inside module dicts:

```tinct
Net.Transport.Tcp        # three levels
Codec.Framing.LengthPrefixed
```

**Patterns use the same dot expression in constructor position.** The grammar allows any tinct dot-access expression in pattern head position — there is no special restricted form:

```tinct
# Unit constructor pattern — literal value match on the qualified constructor
[match sig
  Signal.SIGTERM: [cleanup]
  Signal.SIGINT:  [interrupt]]

# Payload constructor pattern — [DotExpr binding]
[match xs
  Seq.Nil:       "empty"
  [Seq.Cons c]:  c.head]

# Multi-level
[match transport
  Net.Transport.Tcp: handle-tcp
  Net.Transport.Udp: handle-udp]

[match frame
  [Codec.Framing.LengthPrefixed f]: f.payload]
```

**Pattern heads are resolved at type-check time, not at match runtime.** The type checker elaborates each pattern head — a dot-access expression — to a resolved qualified tag before the evaluator runs. At runtime, pattern matching uses the pre-computed qualified tag, identical to today's static string comparison. This is what makes exhaustiveness checking sound: the type checker knows which tags are reachable without evaluating expressions.

**`[_ v]` in pattern head position** matches any constructor and binds the payload to `v`. `_` in pattern head position is the "match any constructor" wildcard — it does not perform a scope lookup.

**Qualified tags in the runtime.** The runtime representation stores qualified tags: `Value::Variant { tag: "Result.Ok" }`, not bare `"Ok"`. This is required for soundness — without qualification, two distinct types sharing a constructor name (e.g., `Result.Ok` and `Validated.Ok`) would be indistinguishable at runtime. All existing variant-producing builtins (`try`, etc.) must emit qualified tags after this change.

This applies at **every constructor usage site**: construction, pattern matching, equality checks, passing as values. There are no sites where bare unqualified constructor names are accepted.

Constructors are first-class values. Rebinding them works exactly like rebinding any other value — no special mechanism, no import syntax:

```tinct
[
  Ok:    Result.Ok     # same as: double: [* _ 2]
  Error: Result.Error

  r: [Ok value: 42]   # construction via rebound name

  v: [match r
    [Ok v]:    v.value   # pattern — type checker elaborates Ok → Result.Ok tag at compile time
    [Error e]: [log e]]
]
```

Rebinding a constructor is normal value binding. The type checker follows the indirection (`Ok` → `Result.Ok`) during elaboration. There is no aliasing mechanism — this is just how values and scope work in tinct.

### Opaque Types at Runtime

Opaque types (`Map: [type [let a b@Covariant] ...]`) are **not bound at the value level**. The type system registers the name and its kind, but the evaluator creates no runtime dict. `Map.Cons` in value position would be a scope error ("undefined variable Map"), not a dict-field-not-found error. The `...` body signals to the type checker "this is Rust-backed" and to the evaluator "do not create a value binding."

This is distinct from transparent aliases and nominal ADTs, both of which produce a value binding.

### Lowering Constraint for Recursive ADTs

Field types in ADT bodies are **type-checker-only annotations** — they are not lowered to runtime `CoreExpr`. In `[Cons head: a tail: [Seq a]]`, the `[Seq a]` in the `tail` field type is resolved during type checking; the evaluator never sees it as a runtime expression. This prevents forcing `Seq.Cons` from triggering a circular dependency on `$Seq`.

The lowering pass must explicitly skip field type annotations when lowering ADT constructor bodies to `CoreExpr`.

```tinct
# Construction
p: [Geometry.Point x: 1.0  y: 2.0]

# Pattern
[match p  [Geometry.Point v]: v.x  Geometry.Origin: 0.0]

# Equality
[= color Color.Red]

# Passing as value
[map [fn [let x] [= x Color.Red]] colors]
```

### Kind Registration

When a type constructor is declared, its kind is inferred from its parameter count:
- 1 parameter → `Kind::Operator` (= `* → *`)
- 2 parameters → `Kind::Arrow(Kind::Type, Kind::Operator)` (= `* → * → *`)
- n parameters → n-deep `Kind::Arrow` chain

The kind is registered in `kind_env` for use by `resolve_type_dict` and typeclass instance checking. No explicit kind annotation is required by the user.

### Typeclass Instances

`[instance [Functor Tree] [fmap: [fn@[Tree B] [f@[Fn@B A]  t@[Tree A]] ...]]]` declares a Functor instance for the user-defined `Tree` constructor. The type checker's instance resolution already handles `Kind::Operator`-kinded constructors; user-defined constructors registered with `Kind::Operator` participate automatically.

### Builtins Declared in Prelude

`Seq`, `Map`, and `Handle` move from hardcoded Rust strings to prelude declarations:

```tinct
--- stage: type
Seq:    [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
Map:    [type [let a b@Covariant] ...]               # opaque — not inductively structured
Handle: [type [let a] ...]                           # opaque — OS resource, no constructors
```

Runtime value matching (`value_matches_type`) is handled by a Rust-side table mapping TyCon names to Value predicates — `Nil` → empty dict, `Cons` → `Value::Seq`, `Map` → `Value::Dict`. This table is the only remaining builtin-specific code.

## What Would Change

### `src/type_def.rs` — New `TyCon` variant and variance table

**Current:** `Type::Seq(Box<Type>)`, `Type::Map(Box<Type>, Box<Type>)`, `Type::Handle(Box<Type>)` as dedicated variants. No variance information in the type system.  
**Proposed:** `Type::TyCon(String)` replaces all three. `Type::App` (already exists) is used for application. A new `TyConEnv` table (parallel to `TypeEnv` for types) stores per-TyCon variance: `HashMap<String, Vec<Variance>>` where `Variance` is `Covariant | Contravariant | Invariant | Bivariant`. Populated from declarations and prelude.  
**Impact:** Major — every exhaustive match on `Type` gains one arm (TyCon); three arms (Seq, Map, Handle) are removed. Net change is neutral in match count. Variance table adds ~30 lines of infrastructure used only in `is_subtype`.

### `src/typecheck_annot.rs` — Uniform constructor lookup

**Current:** String-match on "Seq", "Map", "Handle"; `apply_builtin_constructor`; separate alias instantiation path.  
**Proposed:** Single path: look up name in type environment (either as TyCon or as alias), retrieve arity, collect arguments, produce `App(TyCon(name), args...)` or expanded alias body.  
**Impact:** Moderate — the string match and `apply_builtin_constructor` are deleted; replaced by a 15-line general lookup. The two existing paths (alias and builtin) merge into one.

### `src/type_unify.rs` — UNIFY-TYCON

**Current:** `UNIFY-OPERATOR` handles type constructor variables. `UNIFY-APP` handles application decomposition.  
**Proposed:** Add `UNIFY-TYCON`: two `TyCon` nodes unify iff they have the same name. No other changes — `UNIFY-APP` already handles decomposition.  
**Impact:** Minor — one new match arm in `unify`.

### `src/type_subtype.rs` — Variance-aware subtyping

**Current:** BAS distribution `App(m, a) | App(m, b) <: App(m, a|b)` is hardcoded for specific types only.  
**Proposed:** Variance-directed BAS distribution for all `App(TyCon(name), ...)` applications. When a constructor parameter is marked `+A`, union distributes outward (`F (A|B) = F A | F B`). When marked `-A`, union becomes intersection (`F (A|B) = F A & F B`). When invariant, no distribution.

A variance table mapping TyCon names to per-parameter variance tags is populated from declarations: `Vec<Variance>` where `Variance` is `Covariant | Contravariant | Invariant | Bivariant`.

For `App(App(TyCon("Map"), k), v)`, the variance table says Map has `[Invariant, Covariant]` (keys are invariant, values are covariant), so `Map String Int <: Map String Number`.

**Impact:** Moderate — the variance table is new; `is_subtype` for `App(TyCon(_), ...)` gains a variance-directed decomposition rule; BAS union/intersection distribution is generalized.

### `src/eval_materialize.rs` — `value_matches_type`

**Current:** Matches `Type::Seq(_)` against `Value::Seq{...}`, `Type::Map(_, _)` against `Value::Dict(...)`, etc.  
**Proposed:** Matches `Type::App(Type::TyCon("Seq"), _)` etc. A small Rust table maps TyCon names to Value predicates for builtins. User-defined types match by nominal tag (existing NominalVariant logic).  
**Impact:** Moderate — match arms update; TyCon table added (~10 lines).

### `stdlib/prelude.llt` — Declare builtins as type constructors

**Current:** `Seq`, `Map`, `Handle` are recognized only via Rust string-matching.  
**Proposed:** Declared in prelude (type stage) so the type checker registers them as TyCon entries.  
**Impact:** Minor — three `[type [Name ...]]` declarations in prelude, no body.

### T-920 — Simplified

**Current (T-920 design):** Adds `Kind::Arrow`, pre-registers builtins in `kind_env`, `apply_builtin_constructor`.  
**Proposed:** T-920 is superseded. `Kind::Arrow` is still needed for multi-arg constructors in `kind_env`, but `apply_builtin_constructor` and the builtin string-match are replaced by the general TyCon path.  
**Impact:** T-920 scope narrows — keep `Kind::Arrow` and kind registration; delete `apply_builtin_constructor` step.

## Prerequisites

- `Kind::Arrow` from T-920 (for multi-arg kind registration — still needed)
- T-919 (delete dead `SurfaceExpression::TypeApp` code — clears the way for a cleaner type representation)
- The parameterized type alias system (already complete — transparent aliases continue unchanged)

## References

- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Ch. 29 (Type Operators and Kinding). — Formal treatment of type constructors, kinding, and curried application; basis for the `TyCon`/`App` representation.
- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction, and Polymorphism." *ACM Computing Surveys 17(4)*. — Type constructors as functions from types to types; the theoretical basis for n-arity constructors.
- Peyton Jones, S., et al. (2003). "Haskell 98 Language Report." Ch. 4.2 (Type Constructors). — Production implementation of opaque type constructors with arbitrary arity; variance via `newtype` vs `data`.
- Dolan, S. (2017). "Algebraic Subtyping." PhD thesis, University of Cambridge. — BAS covariance and contravariance distribution rules; basis for variance-directed union/intersection distribution over `App(TyCon, ...)`.
- Tate, R. (2013). "The sequential semantics of producer effect systems." *POPL '13*. — Formal treatment of variance in the presence of effects; contravariance for consumer types.
- Greenman, B. & Felleisen, M. (2018). "A Spectrum of Type Soundness and Performance." *OOPSLA '18*. — Practical variance in gradual type systems; basis for phantom-type bivariance under gradual typing.
- Jones, M.P. (1993). "A System of Constructor Classes." *FPCA '93*. — Constructor classes (Functor, Monad) over arbitrary type constructors; directly applicable to the typeclass instance section.
