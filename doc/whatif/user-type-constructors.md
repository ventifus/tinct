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

Seq:    [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
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

**`Seq` — nominal** (genuinely inductively structured):

```tinct
Seq: [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
```

`Seq` is nominal for the same reason any inductive type is nominal — its structure is defined by its constructors. Migrating sequence builtins (`cons`, `range`, `iterate`, etc.) to produce `Value::Variant { tag: "Seq.Cons" }` rather than the current `Value::Seq` struct is a migration, not a fundamental change. The `Value::Seq` specialization is a performance optimization; it is not required for correctness. Laziness is preserved: the payload dict contains ThunkIds for head and tail exactly as today. The spread pattern `[h ...t]` becomes sugar for `[Seq.Cons c]` with `c.head` and `c.tail`.

**`Map` — transparent alias** using the column constraint mechanism described below. `Map K V` is not a constructive type — it is a structural constraint that any dict whose values are all of type `V` satisfies. It is expressed as:

```tinct
Map: [type [let k v]  [_ : v]]    # uniform-value dict; k documents the key type
```

`{_ : v}` is the column constraint syntax — a record type where `_` is the "uniform field" key, meaning all present fields have type `v`. This is a transparent alias: any dict with all-`v` values qualifies, without having been created by a `Map` constructor.

**`Handle` — opaque** (OS resource; no tinct-expressible constructor exists):

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

### Column Constraints — `RowTail::Uniform(V)`

The type system currently expresses structural constraints as named-field record types (`{host: String, port: Int}`) and open records (`{host: String ...r}`). The goal is to make the type system as expressive as the runtime `validate` schema, bringing more structural contract enforcement to compile time. This section introduces the first extension: the **column constraint**, which asserts a uniform value type across all present fields.

This is the start of a continuum. Subsequent extensions to the row system could enable:
- `{_@k : v}` — typed-key column constraint. **Required to fully express `Map k v`**: until this exists, `k` in `Map: [type [let k v] [_ : v]]` is phantom — documentation only, not enforced. With `{_@k : v}`, the key type is statically checked: `[get "host" d]` is only valid on a `Map String V`, not `Map Int V`. The key type parameter `k` is constrained by `@Equatable` — any type that implements equality can serve as a key, keeping the design open to future key types beyond the current String and Int:

  ```tinct
  Map: [type [let k@Equatable v]  [_@k : v]]
  ```

  `RowTail::Uniform` carries an optional key type (`Type::Unknown` when unconstrained, concrete type otherwise). The `{_ : v}` form (unconstrained key) remains valid for the gradual case where you know values are uniform but don't care about key type.

  Note: the `k@Equatable` constraint also exposes a deeper limitation — the runtime `Key` enum is currently `String | Int` rather than any equatable value. Fully realizing `Map k v` with arbitrary key types requires generalizing `Key` to support any equatable value (T-921).
- Optional field markers — `[host: [or Absent String]  port: Int]` expresses "host may be absent; if present it is String." `Absent` (defined below) handles this without new syntax — it is simply a type in a union annotation.
- Value predicates at the type level — `[port: Int@[min: 1  max: 65535]]` bringing more of the runtime `validate` schema into static checking

`RowTail::Uniform(V)` is the first step in this direction. It extends the structural contract expressiveness of the type system without requiring dependent types or a separate schema language for the common case of uniform-value dicts.

Structural contracts apply to any annotation site — parameters, local bindings, pipeline inputs — and column constraints simply add a new type that can appear in those annotations. See `doc/whatif/completed/structural-contracts.md`.

The mechanism: extend `RowTail` with a `Uniform(V)` variant meaning "all fields in this row have type V":

```rust
pub enum RowTail {
    Empty,
    RowVar(u32),            // open row variable (existing)
    Uniform(Box<Type>),     // NEW: all fields have this type
}
```

**Syntax in annotations** — `{_ : V}` where `_` is the uniform-field sentinel:

```tinct
# These structural types become expressible:
config@{_ : String}              # any dict, all values String
counts@{_ : Int}                 # any dict, all values Int
mixed@{host: String  _ : Int}    # host is String; all other fields are Int
```

**User-defined column constraint types** follow the normal alias form:

```tinct
# Standard aliases
Map: [type [let k v]  [_ : v]]          # uniform-value dict
StringMap: [type [let v]  [_ : v]]      # String-keyed by convention
Headers: [type  [_ : String]]           # HTTP headers: all values String

# Parametric constraint on a specific value type
Counter: [type  [_ : Int]]              # frequency/count dict
```

**Unification**: `unify(Row{tail: Uniform(V1)}, Row{tail: Uniform(V2)})` → `unify(V1, V2)`. A named row `{a: Int, b: Int}` unifies with `{_ : Int}` by checking each named field against the uniform type.

**Subtyping**: `{a: V, b: V} <: {_ : V}` when all field types unify with `V`. Under BAS, covariance distributes: `{_ : Int} <: {_ : Number}`.

**Runtime**: `value_matches_type({_ : V}, d)` verifies the constraint by checking each entry — O(n) in the number of dict entries. For gradual typing, this check fires only at explicit TypeAssert sites (`[@{_ : Int} d]`), not during normal dict access. The type annotation is a programmer-asserted contract, verified incrementally at access time rather than with a full scan at assertion time.

This eliminates the Rust-side `value_matches_type` special case for `TyCon("Map")` — Map becomes a transparent alias obeying general row-matching rules.

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

Opaque types (`Map: [type [let a b@Covariant] ...]`) bind the name to the evaluation of their body — `...` (`Placeholder`). The evaluator binds `Map` to a `Placeholder` thunk, exactly as it does with any other `...` in expression position.

`Placeholder` errors only when forced. Code that uses `Map` only in type annotation position (`xs@[Map String Int]`) never forces `$Map` — the annotation is resolved by the type checker at type-check time, not at runtime. Code that uses `Map` as a runtime value (`$Map`, `[f Map]`) fires the error: "placeholder `...` was evaluated."

**Reflection works uniformly.** `describe` reads the annotation on the dict entry key — it does not force the value. Opaque and nominal types behave identically:

```tinct
Color@[doc: "RGB color"]: [type Red Green Blue]
Map@[doc: "Key-value map"]: [type [let a b@Covariant] ...]

[describe Color].doc   # → "RGB color"     — reads key annotation, never forces $Color
[describe Map].doc     # → "Key-value map"  — reads key annotation, never forces $Map
```

No special evaluator handling is required: the body of `[type ...]` is always evaluated and bound to the name, whether it produces a constructor dict or a `Placeholder`.

### Recursive ADTs and the Lowering Pass

Field type annotations in ADT bodies (`[Seq a]` in `tail: [Seq a]`) are type-checker-only — they are resolved during type checking and not lowered to runtime `CoreExpr`. This is the same behavior as all other type annotations in tinct: `@Int`, `fn@String`, and field types in record type expressions are all type-stage constructs that the evaluator never sees. No special handling is required by the lowering pass beyond what it already does for type annotations.

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

### `Absent` — First-Class Absence

`[]` (the empty dict) is currently overloaded as both "empty collection" and "nothing/null." This conflation is inelegant: an empty dict is a meaningful value, not nothing. `Absent` separates these concerns, giving "this thing is not here" a proper type-level representation.

**`Absent` is a unit nominal type declared in prelude:**

```tinct
Absent: [type Absent]
```

This creates one value: `Absent.Absent` — the singleton representing absence. No named `absent` binding is needed, for the same reason there is no named `null` binding: you never produce absence explicitly (it is produced by builtins like `get?` and `env`), and you test for it with a predicate, not with equality:

```tinct
absent?: [fn@Bool [let x] [= [type-name x] "Absent.Absent"]]
```

`[= x absent]` is not the pattern, just as `[= x []]` is not how you test for null. Use `[absent? x]`.

**`[or Absent T]` in the type algebra — structural `Optional T`:**

```tinct
[or Absent String]           # optional String
[or Absent String Int]       # absent, or String, or Int
Absent & String              # Never — cannot be both absent and present

# Absent distributes through union:
[or Absent String] | [or Absent Int]  =  [or Absent String Int]
```

**Optional record fields** fall out without special syntax:

```tinct
config@[host: [or Absent String]  port: Int]
# host may be absent; if present, must be String
```

**Narrowing with existing predicates — no new syntax:**

```tinct
[has? "host" d]    # true branch:  d.host : String   (Absent narrowed away)
                   # false branch: d.host : Absent
[absent? x]        # true branch:  x : Absent
                   # false branch: x : T             (Absent narrowed away)
```

**`[]` regains its clean meaning.** With `Absent` carrying the "nothing" role, `[]` means only "empty dict/collection" — a valid, meaningful value. Builtins that currently return `[]` to signal missing now return `Absent.Absent`:

| Builtin | Current return for missing | With Absent |
|---|---|---|
| `get?` | `[]` (null) | `[or Absent V]` |
| `env "VAR"` | error or `[]` | `[or Absent String]` |
| `head []` | error | `[or Absent a]` |
| `get-in?` path | `[]` | `[or Absent V]` |

**No special-casing.** `Absent.Absent` is a standard `Value::Variant { tag: "Absent.Absent" }`. `value_matches_type(Absent, v)` uses the standard nominal variant matching. `absent?` is a prelude function, not a builtin. The type checker handles `[or Absent T]` via the existing union type machinery. No new Rust primitives are required.

**Relationship to `Maybe`.** `[or Absent T]` is the structural equivalent of `Maybe T` — without wrapper types, without unwrapping, integrated into the existing union algebra. Where Haskell requires `case x of { Nothing -> ...; Just v -> ... }`, tinct uses `[match x absent: ... v: ...]` or `[if [absent? x] ... ...]`, with narrowing providing the type refinement.

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

**One unavoidable special case:** `value_matches_type` for opaque types requires a Rust-side entry for `Handle` → handle?. Map is no longer in this table — it is a transparent alias `{_ : v}` whose value matching follows from general uniform-row matching rules. `Handle` remains opaque because it has no structural tinct representation. The entry is ~3 lines in `eval_materialize.rs`.

Every other aspect of the design is fully general: annotation resolution, unification, variance, constructor access, pattern elaboration, and reflection all treat user-defined and builtin types identically. Seq participates as a standard nominal ADT. Map participates as a transparent alias.

## What Would Change

### `src/type_def.rs` — Remove Seq/Map/Handle; add TyCon and RowTail::Uniform

**Deleted variants:**
- `Type::Seq(Box<Type>)` — ~100 match arm occurrences across the codebase
- `Type::Map(Box<Type>, Box<Type>)` — ~100 match arm occurrences
- `Type::Handle(Box<Type>)` — ~50 match arm occurrences

Every exhaustive `match ty { ... }` in the codebase loses these three arms. Affected files include `type_def.rs`, `type_unify.rs`, `type_normalize.rs`, `typecheck.rs`, `typecheck_annot.rs`, `typecheck_call.rs`, `typecheck_narrow.rs`, `eval.rs`, `eval_materialize.rs`, `imports.rs`, `builtins_core.rs`, `type_env.rs`, `type_class.rs`, `coverage.rs`, and test files — estimated 300+ match sites total.

**Added:**
- `Type::TyCon(String)` — new variant replacing all three. One new arm in every exhaustive `match ty`.
- `RowTail::Uniform { key_type: Type, value_type: Box<Type> }` — new row tail variant. One new arm in every exhaustive `match tail`.
- `TyConEnv: HashMap<String, Vec<Variance>>` — per-TyCon variance table, populated from declarations.

**Impact:** Major.

### `src/typecheck_annot.rs` — Delete builtin dispatch; replace with general lookup

**Deleted code:**
- `fn apply_builtin_constructor(...)` — entire function deleted
- `fn is_builtin_type_name(name: &str)` — entries `"Seq"`, `"Map"`, `"Handle"` removed (line ~1622)
- `resolve_annotation` arms for `"Seq"` (lines ~168–179), `"Handle"` (lines ~180–198)
- `Annotation::Annotated` match arms for `"Seq"` (line ~1053), `"Map"` (line ~1059), `"Handle"` (line ~1169)
- `resolve_type_dict` string-match arms for `"Seq"` (line ~2444), `"Map"` (line ~2461), `"Handle"` (line ~2505)
- Bare name resolution `"Seq"` → `Type::Seq(Unknown)`, `"Map"` → `Type::Map(...)`, `"Handle"` → `Type::Handle(Unknown)` (lines ~1632–1652)

**Added:** Single general lookup path — check type environment for registered TyCon name, retrieve arity, produce `App(TyCon(name), args...)` or expand alias body. ~15 lines replacing all deleted code.

**RowTail::Uniform parsing:** New arm in `resolve_type_dict` that recognizes `{_ : V}` syntax (where `_` is the uniform-field sentinel) and produces `RowTail::Uniform { key_type: Unknown, value_type: V }`. For `{_@k : V}`, key_type is resolved from the annotation.

**Impact:** Major deletions; moderate additions.

### `src/type_unify.rs` — Add UNIFY-TYCON and uniform-row unification

**Added:**
- `UNIFY-TYCON`: `unify(TyCon(n1), TyCon(n2))` succeeds iff `n1 == n2`. One new arm.
- Uniform row unification: `unify(Uniform{k1,v1}, Uniform{k2,v2})` → `unify(k1,k2)` then `unify(v1,v2)`. Named-field row vs. uniform row: check each named field against the uniform value type.
- Uniform row subtyping in `is_subtype`: `{a:V, b:V} <: {_:V}` when all field types unify with V.

**Deleted:** App normalization paths `App(Operator("Seq"), T) → Seq(T)` and similar (lines ~1335, ~2277) — no longer needed since Seq is a nominal type, not a builtin TyCon.

**Impact:** Moderate.

### `src/value.rs` — Seq migrates from Value::Seq to Value::Variant

**Deleted:** `Value::Seq { head: ThunkId, tail: ThunkId }` — all match sites (~50 occurrences) across `eval.rs`, `eval_materialize.rs`, `builtins_seq_prim.rs`, `builtins_seq_gen.rs`, `builtins_seq_xform.rs`, `builtins_seq_reduce.rs`, `builtins.rs`.

**Added:** No new runtime variant — Seq values become `Value::Variant { tag: "Seq.Cons", payload: ThunkId }` and `Value::Variant { tag: "Seq.Nil" }` (unit). All Seq-producing builtins (`cons`, `range`, `iterate`, `repeat`, `cycle`, `concat`, and every function that returns a sequence) migrate to producing `Value::Variant` instead of `Value::Seq`.

**Impact:** Major — every sequence-producing and sequence-consuming site in the builtins.

### `src/eval.rs` and `src/eval_materialize.rs` — Pattern matching for Seq

**Deleted:**
- `Pattern::Seq { head, tail }` match arm — the spread pattern `[h ...t]` currently matches `Value::Seq`. After migration, it matches `Value::Variant { tag: "Seq.Cons", payload }` and `Value::Variant { tag: "Seq.Nil" }`.
- `ground_type_of(Value::Seq { .. })` → `Type::Seq(Unknown)` — updated to use `Value::Variant` tag lookup.
- `values_equal` Seq comparison — updated to compare via variant tags.

**`value_matches_type` changes:**
- **Deleted:** `Type::Seq(_)` → check `Value::Seq` (removed with variant)
- **Deleted:** `Type::Map(_,_)` → check `Value::Dict` (Map is now transparent alias; match goes through uniform-row checking)
- **Kept (one remaining special case):** `TyCon("Handle")` → check `Value::Handle` (~3 lines)
- **Added:** Uniform-row matching `{_ : V}` → iterate dict entries, check each value against V

**Impact:** Major.

### `src/value.rs` — Qualified variant tags

All `Value::Variant { tag: "..." }` constructions with bare unqualified tags change to qualified tags. Specifically:

- `builtins_meta.rs:337` — `try` success: `tag: "Ok"` → `tag: "Result.Ok"`
- `builtins_meta.rs:355` — `try` failure: `tag: "Error"` → `tag: "Result.Error"`
- All ADT constructor injection sites in `eval_dict.rs` — tags must be prefixed with the type name
- All `match variant_tag { "Ok" => ...}` comparisons in pattern matching — updated to qualified form

**Impact:** Major — affects every nominal variant usage across the codebase.

### `src/parser.rs` — Pattern head for qualified constructors

**Changed:** `surface_node_to_pattern_with_guard` (line ~4843):
- Current: accepts only `SurfaceExpression::VarRef { name }` in constructor pattern head (`[Ok v]`)
- Proposed: accepts `SurfaceExpression::DotAccess { .. }` chains in addition — `[Result.Ok v]` where `Result.Ok` is parsed as `DotAccess(VarRef("Result"), "Ok")`
- Unit constructor patterns (`Color.Red:` in match arms) require a `DotAccess` arm in pattern key handling

**`Pattern::Constructor` in `src/ast.rs`:** tag field changes from bare string `"Ok"` to qualified string `"Result.Ok"`. Propagates through all pattern-matching code in `eval.rs`, `typecheck_match.rs`, `coverage.rs`.

**Impact:** Moderate — parser and AST changes, pattern evaluator updates.

### `src/ast.rs` — TypeAlias params preserve variance

**Changed:** `SurfaceDeclaration::TypeAlias { params: Vec<String>, body: Arc<SurfaceNode> }` → `params: Vec<(String, Option<Spanned<Annotation>>)>` (or a `TypeParam` struct).

**Impact:** Moderate — all TypeAlias construction/match sites in `typecheck.rs`, `typecheck_annot.rs`, `type_env.rs`, `imports.rs`, `expand.rs` (~30 occurrences).

### `src/coverage.rs` — Exhaustiveness for nominal Seq

**Changed:** `Type::Seq(_) => constructors.push((ConstructorTag::TypeTag("Seq".into()), 0))` (line ~217) → updated to handle `App(TyCon("Seq"), _)` and enumerate `Seq.Nil` and `Seq.Cons` as the two constructors for exhaustiveness checking.

**Impact:** Minor.

### `stdlib/prelude.llt` — Builtin declarations and Absent

**Added:**
```tinct
Absent: [type Absent]                                       # unit nominal type
absent?: [fn@Bool [let x] ...]                             # presence predicate
Seq:    [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
Map:    [type [let k@Equatable v]  [_@k : v]]              # transparent column constraint
Handle: [type [let a] ...]                                  # opaque
```

**Changed:** `get?`, `env`, `head`, `get-in?` — return `[or Absent V]` instead of `[]` or erroring on missing. Existing callers using `null?` to check for missing values need updating to `absent?`.

**Impact:** Moderate for declarations; significant downstream impact on any code using `get?`, `env`, `head` with null-checking patterns.

### T-920 — Superseded (narrowed scope)

T-920's `apply_builtin_constructor` step is deleted by this feature. `Kind::Arrow` (for multi-arg kind registration in `kind_env`) is still needed and remains in scope.

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
