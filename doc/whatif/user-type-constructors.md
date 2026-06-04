# What If: User-Defined N-Arity Type Constructors

**State:** Accepted — 2026-06-03

What would it take to let users declare their own parameterized type constructors — with arbitrary arity — and have them work identically to builtins like `Seq` and `Map` in annotations, typeclass constraints, and inference?

**Implementation principle:** Always take the justifiably correct approach, even when it is harder. Special cases and workarounds compound over time and create exactly the debt this whatif exists to eliminate. When a choice arises between a simpler workaround and a principled general solution, the general solution is required. Justify deviations explicitly; do not rationalize shortcuts.

## Current State

`[type ...]` is overloaded across three distinct uses that share no consistent syntax rule, making the system hard to learn and the docs inconsistent with the prelude.

### What's broken today

**Syntax inconsistencies:**

- Unit constructors are written as `Red` (bare word) in prelude but `[Red]` (bracketed) in quickstart docs — semantically identical, syntactically inconsistent.
- The `record:` keyword inside type bodies is optional but undocumented — `[type [record host: String]]` and `[type [host: String]]` both work.
- Two binding forms exist: `Color: [type Red Green Blue]` (dict-entry, name as key) and `[type Color Red Green Blue]` (inline, name inside) — which to prefer is undocumented.
- Parameterized types use lowercase identifiers in constructor bodies as implicit TypeVar parameters. This is wrong: `a` in `[type [Ok a] [Error String]]` should resolve from the enclosing scope like any other name. Without an explicit `[let a]` declaration, `a` is a scope reference, not a new TypeVar. The current implicit-TypeVar behavior contradicts tinct's explicit-over-implicit principle — `[let ...]` is how new variables are bound everywhere else.

**Semantic gaps:**

1. Users cannot declare nominal parameterized types — ones where the name is the type identity regardless of structure. All parameterized types with bodies are currently transparent aliases.
2. Users cannot declare type constructors with variance annotations.
3. Builtins (`Seq`, `Map`, `Handle`) cannot be declared in tinct source — the Rust type checker maintains a parallel string-matching dispatch table that must be kept in sync. Moving declarations to prelude deletes this Rust code entirely.
4. No first-class "absence" type — `[]` (empty dict) is overloaded as both "empty collection" and "nothing/null."

## Why User Type Constructors Matter for Tinct

- **Typeclass instances over user types.** Declare `Tree a` and write `[instance [Functor Tree] [fmap: ...]]`. Today only builtins can be Functor instances.
- **No more special-casing.** `apply_builtin_constructor` and the `resolve_type_dict` builtin string-match arms disappear. `Seq`, `Map`, and `Handle` are declared in prelude like user types.
- **`Contravariant` typeclass.** `Predicate`, `Handler`, `Comparator` — types that consume values — can declare `a@Contravariant` and participate in `Contravariant`. Without variance, these are inexpressible.
- **`Profunctor` typeclass.** Optics (lenses, prisms) require contravariant input and covariant output — inexpressible without variance.
- **Uniform annotation syntax.** `@[Tree Int]`, `@[Map String Int]`, `@[Seq Bool]` all go through the same path.
- **`Absent` type.** Optional fields, missing env vars, and empty-sequence heads all get a proper type-level representation, freeing `[]` to mean only "empty collection."

## Design

### Unified `[type ...]` Syntax

Four rules govern all `[type ...]` forms:

1. **`[let ...]` is the only way to introduce type parameters** — same as `[fn [let x y] body]`. Without `[let ...]`, lowercase names in type bodies resolve from the enclosing scope; they are not created as new TypeVars. This eliminates implicit TypeVars entirely.

2. **Unit constructors are bare uppercase words.** `Red`, `SIGTERM`, `Nil` — no brackets. A bracketed form `[UpperName ...]` is a constructor with named-field payload.

3. **Opaque types use `...` as the body.** "The body exists in Rust, not tinct." Consistent with `...` meaning "not expressible here" elsewhere.

4. **Dict-entry form is the only binding form.** `Name: [type ...]` — the name is the dict key. The inline form `[type TypeName ...]` is retired. Every type declaration follows the same scoping model as every other dict entry.

**Result: five visually distinct forms, no ambiguity:**

| Body content | Kind | Expanding? |
|---|---|---|
| Structural type expression (field dict, type name, `[or ...]`, etc.) | transparent alias | yes |
| `[let ...]` + structural body | parameterized transparent alias | yes |
| Uppercase bare words / `[UpperName ...]` forms | nominal ADT | no |
| `[let ...]` + constructors | parameterized nominal ADT | no |
| `[let ...]` + `...` | opaque constructor | no |

The `record:` keyword inside type bodies is removed — `[field: T ...]` (lowercase key, colon, type) is always and unambiguously a field dict.

**Complete syntax reference:**

```tinct
# ── TRANSPARENT ALIASES ───────────────────────────────────────────────────────

Name:    [type String]
Config:  [type [host: String  port: Int]]
Pair:    [type [let a b]  [first: a  second: b]]      # parameterized; expands at use
Either:  [type [let a b]  [or a b]]                   # union alias

# ── NOMINAL ADTs ─────────────────────────────────────────────────────────────

Signal: [type SIGTERM SIGINT SIGHUP]                  # unit constructors — bare uppercase
DotKey: [type [Ident String] [Index Int]]             # payload constructors
Span:   [type [Span start-line: Int  start-col: Int   # named-field constructor
               end-line: Int    end-col: Int]]

# ── PARAMETERIZED NOMINAL ADTs ────────────────────────────────────────────────

Result: [type [let a]                                 # [let ...] always required
  [Ok value: a]
  [Error msg: String]]

Maybe:  [type [let a]
  [Some value: a]
  None]

Either: [type [let a b]
  [Left  value: a]
  [Right value: b]]

Tree:   [type [let a@Covariant]                       # variance on param
  Leaf
  [Node value: a  left: [Tree a]  right: [Tree a]]]

Tagged: [type [let k@Phantom a@Covariant]
  [Tagged value: a]]

# ── OPAQUE ───────────────────────────────────────────────────────────────────

Map:    [type [let k@Equatable v] ...]                # runtime-backed
Handle: [type [let a] ...]                            # OS resource

# ── BUILTINS IN PRELUDE (--- stage: type) ────────────────────────────────────

Seq:    [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
Absent: [type Absent]
```

**Why `[let ...]` for type params?** Mirrors function declarations exactly:

```tinct
[fn   [let x@Int y@String]  body]    # fn params bound with [let ...]
[type [let a@Covariant b]   ctors]   # type params bound with [let ...]
```

Both bind names into a local scope. Both allow `@` variance/kind annotations on those names.

**Variance annotations** use ImmediateAt (`name@Variance`), same as `f@Operator` on class params:

| Annotation | Meaning |
|---|---|
| `a@Covariant` | `F a <: F b` when `a <: b` — producer/container position |
| `a@Contravariant` | `F a <: F b` when `b <: a` — consumer/handler position |
| `a` (none) | invariant — default; safe for opaque types |
| `a@Phantom` | `F a <: F b` always — a is type-level only, no runtime presence |

**Variance inference for transparent aliases** (those with a body): the compiler performs polarity analysis (Dolan 2017 §4) — walk the body type expression and classify each TypeVar's occurrences:

- **Covariant**: field types in records, return position of a function type, union/intersection members
- **Contravariant**: argument types in a function type (`[Fn@R [A B C]]` — A, B, C are contravariant)
- **Invariant**: appears in both covariant and contravariant positions
- **Phantom**: never appears in the body

Example: `Pair: [type [let a b] [first: a second: b]]` → a and b are both covariant. `Callback: [type [let a] [Fn@Null [a]]]` → a is contravariant. `Ref: [type [let a] [get: [Fn@a []] set: [Fn@Null [a]]]]` → a is invariant (return + argument). Explicit annotations override inference and serve as a checked declaration.

### Constructor Access and Patterns

A type declaration creates two things:

1. **A type** registered in the type system
2. **A dict value** containing all constructors as fields — the only binding created is the type name

```tinct
Color: [type Red Green Blue]
# value:  Color = {Red: <Color.Red variant>, Green: ..., Blue: ...}
# type:   Color in the type system
# NOT created: Red, Green, Blue as separate bindings
```

**Constructors are accessed via dot — full chain, same as anywhere in tinct:**

```tinct
Signal.SIGTERM           # unit constructor
Result.Ok                # payload constructor function
Net.Transport.Tcp        # multi-level
```

**Patterns use the same dot expression in constructor head position — no special restricted form:**

```tinct
[match sig
  Signal.SIGTERM: [cleanup]
  Signal.SIGINT:  [interrupt]]

[match xs
  Seq.Nil:       "empty"
  [Seq.Cons c]:  c.head]

[match frame
  [Codec.Framing.LengthPrefixed f]: f.payload]
```

**Pattern head qualification is syntactic in the parser.** `resolve.rs` only does variable-slot resolution (de Bruijn coordinates) — it has no access to the type environment and cannot look up constructor dicts.

- **Dot-access pattern heads** (`[Result.Ok v]`, `Color.Red:`): the parser assembles the qualified tag string syntactically by walking the DotAccess chain left-recursively. The function `flatten_dot_access_to_tag` is a pure structural walk over `SurfaceExpression` with no logic beyond the AST — it is defined in **`src/ast.rs`** as `pub(crate)`, alongside the types it operates on. Both callers (`src/parser.rs` and `src/typecheck_special.rs`) already import from `src/ast.rs`, so no new `use` lines are needed.

  ```rust
  // src/ast.rs
  pub(crate) fn flatten_dot_access_to_tag(expr: &SurfaceExpression) -> Option<String> {
      match expr {
          SurfaceExpression::VarRef { name, .. } =>
              Some(name.clone()),
          SurfaceExpression::DotAccess { expr, field: DotKey::Ident(s), .. } =>
              Some(format!("{}.{}", flatten_dot_access_to_tag(&expr.expr)?, s)),
          SurfaceExpression::DotAccess { field: DotKey::Int(_), .. } =>
              None,  // numeric index in a dot chain is not a constructor name
          _ => None,
      }
  }
  ```

  The field is `field: DotKey` (not `key`), where `DotKey` is an enum with `Ident(String)` and `Int(i64)` variants. Integer segments return `None` rather than silently producing a malformed tag. `Result.Ok` → `"Result.Ok"`, `Net.Transport.Tcp` → `"Net.Transport.Tcp"`. The parser produces `Pattern::Constructor { tag: "Result.Ok", binding }` directly. Tag validity is checked later by `typecheck_match.rs`.

  **Two parser sites** must call `flatten_dot_access_to_tag`:

  1. **Call-with-DotAccess-func** (`[Result.Ok v]`) — in `surface_node_to_pattern_with_guard`, the Call arm at `parser.rs:5009` guards `if let SurfaceExpression::VarRef { name, .. } = &func.expr`. The DotAccess case currently falls to the `else` branch at line 5053 ("invalid pattern"). This else branch must be split:

     ```rust
     } else if let Some(tag) = flatten_dot_access_to_tag(&func.expr) {
         (Pattern::Constructor { tag, binding: Some(payload_pattern) }, None)
     } else {
         return Err(ParseError { ... "invalid pattern" ... });
     }
     ```

  2. **Bare DotAccess** (`Color.Red:` — unit constructor, no payload) — a new arm is needed in `surface_node_to_pattern_with_guard` for bare `SurfaceExpression::DotAccess` nodes (not inside a Call):

     ```rust
     SurfaceExpression::DotAccess { .. } => {
         match flatten_dot_access_to_tag(&node.expr) {
             Some(tag) => (Pattern::Constructor { tag, binding: None }, None),
             None => return Err(ParseError { message: "invalid pattern: numeric index in dot-access".into(), span }),
         }
     }
     ```

- **Bare uppercase words in pattern position** (`None:`, `Tcp:`): the parser produces `Pattern::Constructor { tag: "None", binding: None }` — unqualified. The type checker's pattern-checking pass (`typecheck_match.rs`) validates that the tag resolves to a constructor of the scrutinee's declared type and rewrites the tag to its qualified form (`"Maybe.None"`, `"Transport.Tcp"`).

- **Rebound constructors** (`Ok: Result.Ok` in scope, then `[Ok v]` as a pattern): at runtime, `Ok` in scope has value `Result.Ok` (a `Value::Variant { tag: "Result.Ok" }`). Pattern matching evaluates `Ok` via normal scope lookup and compares against the scrutinee's tag. The type checker's pass follows the binding to validate and rewrite the tag.

**Constructors are first-class values.** Rebinding is normal value binding. **Dot-access on a constructor dict triggers scheme instantiation** (Damas & Milner 1982): `Result.Ok` evaluates to the `Ok` constructor function from `Result`'s dict, and each use gets a fresh instantiation of its polymorphic type scheme — so `[Result.Ok value: 42]` gets `Result Int` and `[Result.Ok value: "hello"]` gets `Result String` independently, without sharing a single type variable between uses.

```tinct
Ok:    Result.Ok     # same as: double: [* _ 2]
Error: Result.Error

r: [Ok value: 42]
[match r
  [Ok v]:    v.value
  [Error e]: [log e]]
```

**Common prelude aliases** for the most-used constructors — pure rebinding, no special mechanism:

```tinct
Ok:    Result.Ok
Error: Result.Error
Some:  Maybe.Some
None:  Maybe.None
```

Code using bare `Ok`, `Error`, `Some`, `None` continues to work via scope lookup through these aliases. Qualified forms are always available for clarity.

**Qualified runtime tags.** `Value::Variant { tag: "Result.Ok" }` not `"Ok"`. Required for soundness — two types sharing a constructor name (e.g., `Result.Ok` and `Validated.Ok`) must be distinguishable at runtime.

### Builtin Type Declarations

**Scalar primitive type stubs** — declared as opaque types in prelude, making the type environment the complete source of truth for all types:

**All Rust-backed types declared in prelude using `builtin-type`**, making TyConEnv the complete and authoritative registry of all types and their Rust-level representations:

```tinct
--- stage: type
Int:    [type [builtin-type "Int"]]     # Value::Int
Str:    [type [builtin-type "Str"]]     # Value::String
Bool:   [type [builtin-type "Bool"]]    # Value::Bool
Float:  [type [builtin-type "Float"]]   # Value::Float
Bytes:  [type [builtin-type "Bytes"]]   # Value::Bytes
Dict:   [type [builtin-type "Dict"]]    # Value::Dict
Fn:     [type [builtin-type "Fn"]]      # Value::Function | Value::Builtin
Handle: [type [builtin-type "Handle"]]  # Value::Handle

Number: [type [or Int Float]]           # transparent alias — union of numeric primitives
```

`builtin-type` is a type-stage primitive recognized by `typecheck_annot.rs` (alongside `or`, `all`, `without`). When the type body is `[builtin-type "X"]`, the TyConDef is marked with discriminant `"X"` and no user-visible constructors. `...` (Placeholder body) becomes unused for type declarations — `builtin-type` is the correct form for Rust-backed types.

The `typecheck_match.rs` elaboration pass has zero hardcoded lists: TyConDef with constructors → nominal constructor pattern; TyConDef with `builtin-type` discriminant → `Pattern::TypeAssert`; not found → type error.

The Rust-side `value_matches_type` dispatches on the discriminant string extracted from the TyConDef — not a table of TyCon names:

```rust
match builtin_type_discriminant.as_str() {
    "Int"    => matches!(v, Value::Int(_)),
    "Str"    => matches!(v, Value::String { .. }),
    "Bool"   => matches!(v, Value::Bool(_)),
    "Float"  => matches!(v, Value::Float(_)),
    "Bytes"  => matches!(v, Value::Bytes(_)),
    "Dict"   => matches!(v, Value::Dict(_)),
    "Fn"     => matches!(v, Value::Function { .. } | Value::Builtin { .. }),
    "Handle" => matches!(v, Value::Handle(_)),
    _        => false,
}
```

Adding a new Rust-backed type requires: one line in prelude (`NewType: [type [builtin-type "NewType"]]`) AND one arm in this dispatch. Both changes are in the right place and neither is hidden.

`Null` is `[]` (the empty dict value); `null?` handles it — not a separate type stub.

**`Seq` — nominal** (genuinely inductively structured):

```tinct
Seq: [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
```

`Seq` is nominal because its structure is defined by its constructors. The current `Value::Seq { head: ThunkId, tail: ThunkId }` is a performance optimization, not a correctness requirement. Migrating sequence builtins to produce `Value::Variant { tag: "Seq.Cons" }` is correct — laziness is preserved (payload dict contains ThunkIds for head and tail). The existing `[seq h t]` pattern syntax (which produces `Pattern::Seq` in the parser's `("seq", 2)` arm at `parser.rs:5011`) is updated to produce `Pattern::Constructor { tag: "Seq.Cons", binding: Some(dict-destructure) }` instead.

**`Map` — transparent alias** (column constraint, not a constructive type):

```tinct
Map: [type [let k@Equatable v]  [_ : v]]
```

`Map K V` is a structural constraint — any dict whose values are all of type V and whose keys all satisfy K satisfies it. `k` is the key type (constrained to equatable types) and `v` is the value type. The column constraint `{_@K : V}` (and its shorthand `{_ : V}` when the key type is unconstrained) is specified in §Column Constraints below. Runtime key-type enforcement requires T-921 (Key enum generalization); compile-time key-type checking is part of this whatif.

**`Handle` — opaque** (OS resource; no tinct-expressible constructor):

```tinct
Handle: [type [let a] ...]
```

**`Absent` — unit nominal type** (see §Absent below):

```tinct
Absent: [type Absent]
```

**`Variance` — annotation vocabulary type** (pure ADT, no Rust backing):

```tinct
Variance: [type Covariant Contravariant Invariant Phantom]
```

Used in `[let a@Covariant]` type parameter annotations. The type checker resolves variance annotations to `Variance.*` constructors via normal tinct name resolution, then maps to the internal Rust `Variance` enum for `TyConDef` storage.

### Type Representation

Replace dedicated collection variants with `Type::TyCon(String)` + `Type::App`:

```rust
pub enum Type {
    // Replaces Type::Seq(Box<Type>), Type::Map(Box<Type>, Box<Type>), Type::Handle(Box<Type>)
    TyCon(String),              // concrete type constructor name: "Seq", "Tree", etc.
    App(Box<Type>, Box<Type>),  // already exists — curried application
}
```

`Type::Seq(T)` → `Type::App(TyCon("Seq"), T)`.
`Type::Map(K, V)` → `Type::App(App(TyCon("Map"), K), V)`.

`Type::Operator(String)` remains for type constructor *variables* (class params like `f` in `[class [f@Operator] ...]`); `TyCon` is for concrete *names*.

**`TyConDef` is stored in `TypeEnv` alongside the type alias entry — in a new third map on `TypeEnv`.** The existing `TypeEnv` struct (at `src/type_env.rs`) has two maps: `bindings: HashMap<String, TypeScheme>` (value bindings) and `type_aliases: HashMap<String, TypeAlias>` (type aliases). A third map is added: `tycon_defs: HashMap<String, TyConDef>`. New methods: `insert_tycon_def(name, def)` and `lookup_tycon_def(name)` (walks the parent chain, same pattern as `lookup_type_alias`).

When the type checker processes `Color: [type Red Green Blue]`, it stores the `TypeAlias` in `type_aliases` AND the `TyConDef` in `tycon_defs`, both in the current scope frame. Lookups during type checking go through the scope chain: a type declared in an inner dict is only visible to descendants. **Value aliases** (`Ok: Result.Ok`) go only into `bindings`; they have no `TyConDef`.

**Flat accumulator in `InferState`.** `TyConEnv` lookups during type checking use the scoped `TypeEnv`. At the same time, the type checker accumulates all registered TyCons into a flat `pub tycon_env: HashMap<String, TyConDef>` field added to `InferState` — same pattern as `boundary_guards` and `do_infer_resolutions`. This accumulator is populated as each TyCon declaration is processed (no parent-chain walk needed at extraction time), and transferred to `EvalContext` at the transfer sites via `ctx.set_tycon_env(infer_state.tycon_env)`.

The flat snapshot walks the full parent chain for completeness: if a TyCon was declared in the prelude (an ancestor frame), it must appear in the accumulator. The `insert_tycon_def` call at each TyCon declaration site ensures the accumulator is populated incrementally as the type checker descends into nested scopes.

`TyConDef` is defined in `src/type_def.rs` (new file) alongside `Type` and `Row`:

```rust
// src/type_def.rs
pub enum Variance { Covariant, Contravariant, Invariant, Phantom }

pub struct TyConDef {
    pub variance: Vec<Variance>,            // per-parameter, same order as [let ...] params
    pub constructors: Vec<(String, usize)>, // (qualified-tag, payload-arity); empty for aliases/opaques
    pub builtin_type: Option<String>,       // "Int", "Str", etc. for builtin-type declarations; None otherwise
}
```

**`Variance` declared in prelude:** The Rust `Variance` enum is an internal representation. Its user-visible counterpart is a tinct prelude type — a pure sum type, no `builtin-type` needed:

```tinct
Variance: [type Covariant Contravariant Invariant Phantom]
```

When the type checker encounters `a@Covariant` in `[let a@Covariant]`, it classifies the annotation via a direct name match in `typecheck_annot.rs` — the same structure as the `builtin-type` 8-entry dispatch:

```rust
fn annotation_to_variance(name: &str) -> Option<Variance> {
    match name {
        "Covariant"     => Some(Variance::Covariant),
        "Contravariant" => Some(Variance::Contravariant),
        "Invariant"     => Some(Variance::Invariant),
        "Phantom"       => Some(Variance::Phantom),
        _               => None,
    }
}
```

`Variance: [type Covariant Contravariant Invariant Phantom]` in prelude exists for reflection (`[describe Variance]` works, users can inspect the type). It does NOT power the dispatch — the dispatch is a closed 4-entry table. No TyConEnv lookup for "Variance" by name; no special TyCon handling. Adding a new variance annotation requires one constructor in prelude + one arm here — the same two-place change required for a new `builtin-type`.

**Distinguishing `@Covariant` from `@Equatable` in `[let ...]` params:** `typecheck_annot.rs` reads stored annotations from `params: Vec<(String, Option<Spanned<Annotation>>)>` and classifies:

- `annotation_to_variance(name)` returns `Some(v)` → variance entry in `TyConDef.variance`
- bare (no annotation) → `Variance::Invariant` (default)
- `annotation_to_variance(name)` returns `None` AND name is a registered class → typeclass constraint; register the parameter as phantom in `TyConDef.variance` AND add a `Constraint::Class { class: name, var: param_name }` to the alias's constraint set for instance-checking
- Neither → type error: "unknown variance or typeclass annotation `@X` on type parameter `name`"

### Annotation Resolution

The builtin string-match in `resolve_type_dict` is replaced with a single general path: look up the name in the type environment, retrieve the registered TyCon's arity, collect arguments, produce `App(TyCon(name), args...)` or expand the alias body. `apply_builtin_constructor` is deleted.

### Unification

- **UNIFY-TYCON**: `TyCon(n1)` and `TyCon(n2)` unify iff `n1 == n2` AND both names resolve to the same `TyConDef` in the current scoped `TypeEnv` (pointer identity of the definition, not just name equality). Two `Color` types in different scope frames have the same name but different `TyConDef` entries — they are distinct types. The scoped `TypeEnv` is the source of type identity; the name string is a lookup key, not an identity token. No binding (constructors are not variables).
- **UNIFY-APP**: already exists — decomposes `App(f1, a1)` and `App(f2, a2)` by unifying constructors then arguments.
- **UNIFY-UNIFORM**: new — see §Column Constraints.

### Subtyping

**`is_subtype` signature change:** `is_subtype(sub: &Type, sup: &Type) -> bool` becomes `is_subtype(sub: &Type, sup: &Type, tycon_env: Option<&TyConEnv>) -> bool`.

- `None`: safe conservative fallback — treat all `App(TyCon(_), _)` arguments as **invariant** (equality check: `a == b`). Covariant default is unsound: a user-declared `Ref: [type [let a] ...]` is invariant, and covariant default would allow `Ref Int <: Ref Number` — the classical Java array covariance error. Invariant default is incomplete (may reject valid covariant subtyping queries at call sites without env) but never unsound. Call sites that have `TyConEnv` always pass `Some` and get correct variance behavior.
- `Some(&state.tycon_env)`: all type-checker call sites (InferState always has it).
- `Some` from `EvalContext.tycon_env`: runtime call sites (`value_matches_type` etc.).
- ~100 call site updates, mechanical.
- Thread-local explicitly rejected: hides a dependency, makes `is_subtype(A, B)` non-deterministic on the same inputs, fragile in async runtimes.

**Variance-directed subtyping for `App(TyCon, _)`:** look up variance from `TyConEnv`:

- `@Covariant`: `App(TyCon(f), a) <: App(TyCon(f), b)` when `a <: b`. BAS join holds: `F a | F b <: F (a | b)`. The split direction (`F (a | b) <: F a | F b`) is NOT derivable and is unsound — verified by the hkt-bas sprint.
- `@Contravariant`: `App(TyCon(f), a) <: App(TyCon(f), b)` when `b <: a` (flipped).
- Invariant: `App(TyCon(f), a) <: App(TyCon(f), b)` only when `a == b`.
- `@Phantom`: `App(TyCon(f), a) <: App(TyCon(f), b)` always.

### Column Constraints — `RowTail::Uniform`

`RowTail::Uniform(V)` is NOT the `RowVar` that BAS eliminated. BAS removed row variables — polymorphic extension points expressing "more fields, type unknown." `RowTail::Uniform(V)` is a *constraint* — deterministic, not polymorphic — expressing "whatever fields are present, their values have type V." BAS's finite conjunction cannot express `∀l. {l: V}` (infinite); `RowTail::Uniform` is the correct finite representation. Nickel implements the identical subtyping rule and proves it sound. (See 2026-06-02 panel research report — computer-scientist agent.)

**The motivating case: mixed named + uniform.** `{host: String, _ : Int}` — a dict where `host` is specifically `String` and all other fields are `Int`. No other approach handles this.

```rust
pub enum RowTail {
    Empty,                              // closed record — current behavior, default
    Uniform {                           // column constraint
        key: Option<Box<Type>>,         // None = {_ : V}; Some(K) = {_@K : V}
        value: Box<Type>,               // all present fields have this value type
    },
}
pub struct Row {
    pub fields: HashMap<String, Type>,
    pub tail: RowTail,    // new field; RowTail::Empty for all current Row constructions
}
```

`key: None` is the unkeyed form `{_ : V}` — value constraint only. `key: Some(K)` is the typed-key form `{_@K : V}` — both key and value constrained. All existing `{_ : V}` constructions use `RowTail::Uniform { key: None, value: V }`.

**Syntax:**

```tinct
config@{_ : String}             # all values String (key: None)
counts@{_ : Int}                # all values Int (key: None)
mixed@{host: String  _ : Int}   # host is String; all other fields are Int (key: None)
data@{_@String : Int}           # String keys, Int values (key: Some(String))
```

**User-defined column constraint types** — normal alias form:

```tinct
Map:      [type [let k@Equatable v]  [_@k : v]]   # typed-key uniform dict
Headers:  [type  [_ : String]]                     # all values String
Counter:  [type  [_ : Int]]                        # frequency/count dict
```

**Subtyping rules (validated by Nickel):**

- `{f1:T1, ..., fn:Tn, tail:Empty} <: {tail:Uniform{key:None, value:V}}` when all Ti <: V
- `{tail:Uniform{key:None, value:V1}} <: {tail:Uniform{key:None, value:V2}}` when V1 <: V2 (covariant)
- `{f1:T1, tail:Uniform{key:None, value:V1}} <: {tail:Uniform{key:None, value:V2}}` when T1 <: V2 and V1 <: V2
- **Typed-key rules:** `{_@K1 : V1} <: {_@K2 : V2}` when `K1 <: K2` (covariant in key — more specific key type is a subtype) and `V1 <: V2`
- `{_@K : V} <: {_ : V}` always (keyed is a subtype of unkeyed — a dict constrained to keys of type K satisfies the unconstrained form, which imposes no key requirement)

**Unification rules — TypeVar vs concrete split:**

The handling of `unify(Row { fields, tail: Empty }, Row { fields: {}, tail: Uniform(V) })` first applies the current substitution to `V`, then branches on the result:

1. **Apply substitution first:** let `V' = subst.apply(V)` (applying to fixpoint — `apply` follows chains). Branch on `V'`, not on `V` directly. This handles the case where `V` is a TypeVar `α` that is already bound to a concrete type in the substitution — without this step, a bound TypeVar would incorrectly enter the TypeVar branch. After applying to fixpoint, if the result is still a `TypeVar`, that TypeVar is definitionally not in the substitution domain (unbound). The level check on that TypeVar is handled by the existing `unify` path's level-lowering logic — the uniform row unification calls `unify(α, join)` through the same `unify` function that already performs level lowering (preventing premature generalization per Kiselyov 2013).

2. **TypeVar case** (`V'` is a `TypeVar(α)` after applying substitution to fixpoint — definitionally unbound): compute the normalized join (union) of all named field types — `T1 | T2 | ... | Tn` — via `normalize_union(&[T1, T2, ..., Tn])`, then unify `α` with the join: `unify(α, join)`. This is lower-bound accumulation — the TypeVar is constrained to the smallest type that all fields could flow into. Consistent with BAS. Principality holds when `normalize_union` computes the least upper bound.

3. **Concrete type case** (`V'` is a concrete type after applying substitution): check `is_subtype(Ti, V')` for each named field `Ti`. If any field fails, type error: `"field 'f' has type T, expected V' (from uniform constraint {_ : V})"`. Do NOT call `unify(Ti, V')` — subtyping check only.

- `unify(Uniform{key:K1,value:V1}, Uniform{key:K2,value:V2})` → `unify(K1,K2)` (if both Some) or skip key unification (if either None) AND `unify(V1,V2)`
- `unify(Empty, Uniform{..})` or `unify(Uniform{..}, Empty)` → type error: "closed row does not satisfy uniform constraint" (subtyping question, not unification)

**Runtime: proxy contracts, not eager O(n) walk.** `[@{_ : Int} d]` wraps each field access in a guard thunk checked on demand. Preserves tinct's lazy evaluation guarantee. The substitution-first branching above applies only at compile time in `type_unify.rs`; at runtime, `value_matches_type` uses `is_subtype` (no substitution), and uniform row matching goes through the proxy contract path.

**Typed-key column constraint `{_@K : V}`:** `Map K V` expands to `{_@K : V}` — a uniform dict where all values have type `V` and all keys satisfy the `@Equatable` constraint `K`. Subtyping: `{_@K1 : V1} <: {_@K2 : V2}` iff `K1 <: K2` (covariant in key — accepting a more specific key type is a subtype) and `V1 <: V2`. Unification: `unify({_@K1 : V1}, {_@K2 : V2})` → `unify(K1, K2)` and `unify(V1, V2)`.

**Runtime key-type enforcement requires T-921** (generalizing `Key` from `String | Int` to any equatable value). Until T-921 ships, key-type checking is compile-time only — the type checker verifies that the declared key type satisfies `@Equatable`, but at runtime the proxy contract only checks the value type (not the key type). This is a static soundness gap bounded by T-921, not a design gap. `Map String Int` and `Map Int Int` both expand to `{_@k : Int}` with different key constraints — the type checker distinguishes them, runtime does not until T-921.

### `Absent` — First-Class Absence

`[]` (the empty dict) is overloaded as "empty collection" AND "nothing/null." `Absent` separates these: `[]` is only "empty collection," `Absent.Absent` is "this thing is not here."

```tinct
Absent: [type Absent]                         # prelude unit nominal type
absent?: [fn@Bool [let x@Unknown] [match x Absent.Absent: true  _: false]]  # predicate
```

No named `absent` binding. Test by pattern matching (`Absent.Absent: ... _: ...`) or via `[absent? x]`. `absent?` has type `Unknown → Bool` — it accepts any runtime value and checks if it is `Absent.Absent`. The `@Unknown` annotation on `x` is correct and intentional: `absent?` is a type-erasing runtime predicate, in the same category as `null?`. Both need `@Unknown` because their purpose is precisely to be called on values whose compile-time type is not known. Do NOT implement as `[= x Absent.Absent]` — that constrains `x` to `NominalVariant{"Absent.Absent"}` making it useless for mixed-type inputs.

**`[or Absent T]` is structural `Optional T`:**

```tinct
[or Absent String]                    # optional String
Absent & String                       # Never — exclusive
[or Absent String] | [or Absent Int]  # = [or Absent String Int]
```

**Optional fields** fall out without special syntax:

```tinct
config@[host: [or Absent String]  port: Int]   # host may be absent; if present, String
```

**Narrowing via pattern matching:**

```tinct
[has? "host" d]   # true:  d.host : String   [false: d.host : Absent]

[match x
  Absent.Absent:  "missing"    # x : Absent in this arm
  _:              "present"]   # x : T in this arm
```

`[has? "host" d]` narrows via the existing narrowing pass. `[absent? x]` as a function call does not narrow — tinct's narrowing is syntactic (inspects the source expression at the `[if ...]` call site) and does not perform interprocedural analysis. Use pattern matching directly when the narrowed type is needed.

**Builtins updated:** `get?`, `env`, `head`, `get-in?` return `[or Absent V]` instead of `[]` or erroring on missing.

**No special-casing.** `Absent.Absent` = `Value::Variant { tag: "Absent.Absent" }`. Standard nominal variant matching. `absent?` is a prelude function.

### Constructor Generation

The `VARIANT_TAG_MARKER` mechanism in `src/eval_call.rs` is an erroneous workaround — constructors secretly behaved differently from ordinary functions via a hidden marker. Deleted entirely.

**`variant` builtin revised to two direct modes:**

- `[variant "Tag"]` → `Value::Variant { tag, payload: None }` — unit value
- `[variant "Tag" payload]` → `Value::Variant { tag, payload: Some(alloc(payload)) }` — with payload

All constructors are generated by `inject_adt_constructors_expr` (`src/desugar.rs:86`) as ordinary tinct values or functions. This consolidates the current split: unit constructors were previously excluded from the desugar pass (comment at desugar.rs:325-326) and handled by `eval_dict_core`'s `CoreExpr::TypeDecl` pre-scan (`eval_dict.rs:128-161`). After this change, desugar handles ALL constructors; `CoreExpr::TypeDecl` (ast.rs:958) and the pre-scan are deleted. **These must land atomically** — if the desugar expansion and pre-scan deletion are separate commits, unit constructors will be injected twice causing E030 duplicate key.

```tinct
Color.Red:  [variant "Color.Red"]                         # unit — dict key: "Red", tag: "Color.Red"
Seq.Nil:    [variant "Seq.Nil"]
Result.Ok:  [fn@[Result a] [let value@a]                  # payload — ordinary function
  [variant "Result.Ok" [value: value]]]
Seq.Cons:   [fn@[Seq a] [let head@a tail@[Seq a]]         # named-field — same pattern
  [variant "Seq.Cons" [head: head  tail: tail]]]
```

**Changes to `inject_adt_constructors_expr` at `src/desugar.rs:86`:**

The function currently calls `extract_surface_adt_ctor_names_from_expr` (returns `Vec<String>`) and always injects `CtorName: [variant "CtorName"]`. This must be redesigned:

1. **Extract the type name from `se.node.key`** — the dict entry key is `Option<Arc<SurfaceNode>>`, not a String. Extract via:

   ```rust
   let type_name = match se.node.key.as_ref()?.expr {
       SurfaceExpression::Str(ref s) => s.clone(),
       SurfaceExpression::VarRef { ref name, .. } => name.clone(),
       _ => return,  // computed key — not a type declaration, skip
   };
   ```

   Both `Str` and `VarRef` are valid key forms for named dict entries (`"Result": ...` and `Result: ...` are both legal). A positional entry (no key) or a computed key (DotAccess, Call, etc.) means this is not a TypeAlias declaration — skip injection.
2. Inspect the type body to classify each constructor:
   - Bare uppercase `VarRef` → **unit constructor**: inject `"CtorName": [variant "TypeName.CtorName"]`
   - `Call { func: VarRef(UpperName), named_args, .. }` → **named-field constructor**: inject an `fn` expression (params from named_args, body calls `[variant "TypeName.CtorName" [...]]`)
3. The injected dict entry key is always `"CtorName"` (unqualified — it's the field of the type's dict value `Color.Red` where `Red` is the key). The variant tag INSIDE is `"TypeName.CtorName"` (qualified).

No VARIANT_TAG_MARKER. No special-casing in `invoke_function`. Constructors are values or functions.

### Pattern AST

**`Pattern::TypeTag` is deleted entirely.** Primitive type predicates (`Int:`, `Str:`, `Bool:`) desugar to TypeAssert patterns — the general existing mechanism for "does this value have this type?" No lookup table. No list of primitive names. Any future type works automatically.

**Two pattern variants added to `src/ast.rs`** — surface form (pre-elaboration) and core form (post-elaboration):

```rust
// Surface form — parser produces this; only exists before typecheck_match.rs elaboration
Pattern::TypeAssertPending { annotation: Spanned<Annotation>, inner: Option<Box<Spanned<Pattern>>> }

// Core form — typecheck_match.rs elaboration produces this; reaches the evaluator
Pattern::TypeAssert { resolved_type: Type, inner: Option<Box<Spanned<Pattern>>> }
```

`inner: None` = no binding (bare `Int:` sugar). `inner: Some(pat)` = bind the matched value through `pat`. `TypeAssertPending` never reaches the evaluator — it is an invariant violation (typecheck always runs before eval in normal pipelines).

**New arm added to `surface_node_to_pattern_with_guard` at `src/parser.rs:4843`:**

```rust
SurfaceExpression::TypeAssert { annotation, expr } => {
    let inner_pat = surface_node_to_pattern(Arc::clone(expr))?;  // guard-discarding wrapper
    (
        Pattern::TypeAssertPending {
            annotation: annotation.clone(),
            inner: Some(Box::new(inner_pat)),
        },
        None,
    )
}
```

`surface_node_to_pattern` (not `surface_node_to_pattern_with_guard`) is the guard-discarding wrapper at `src/parser.rs:4837` that returns `Result<Spanned<Pattern>>` — matching the type of `inner: Option<Box<Spanned<Pattern>>>`. Guards inside a TypeAssert inner pattern surface through the outer `Annotated` arm, not through the TypeAssert arm.

**`typecheck_match.rs` elaboration resolves `TypeAssertPending` → `TypeAssert`** when elaborating patterns: for each `Pattern::TypeAssertPending { annotation, inner }`, call `resolve_annotation(&annotation.node, &env, annotation.span, state, ...)` — the full compile-time resolver with all its arguments. This handles ALL annotation forms (`Simple`, `Annotated`, `PropertyDict` for union/record types) correctly. Store the resolved `Type` and replace the node with `Pattern::TypeAssert { resolved_type: the_type, inner }`. `Pattern::TypeAssertPending` never appears after elaboration.

**No `resolve_annotation_at_runtime` function.** The full resolution is done once at typecheck time; the runtime arm uses the pre-resolved `Type` directly.

The evaluator's `match_pattern` arm:

```rust
Pattern::TypeAssert { resolved_type, inner } => {
    if !value_matches_type(value, resolved_type) {   // note: value first, type second
        return None;
    }
    match inner {
        None => Some(env),
        Some(pat) => match_pattern(pat, value, env, ctx),
    }
}
```

`value_matches_type` at `eval.rs:1029` is `pub(crate)` and takes `(value: &Value, expected: &Type) -> bool` — directly callable. The `resolved_type` covers all annotation forms including `[@[or Int String] x]` (resolved to `Type::Union([Int, String])`) and `[@[Seq Int] x]` (resolved to `Type::App(TyCon("Seq"), Int)`) — no special runtime dispatch needed.

**`Pattern::TypeAssert` bypasses the elaboration pass** — `Pattern::TypeAssertPending` patterns produced by explicit `[@Type x]` source syntax go through `TypeAssertPending → TypeAssert` rewriting. After that rewrite, they are not inspected again by the elaboration pass. The elaboration pass's `Pattern::Constructor` classification logic does not apply to TypeAssert patterns.

`Pattern::TypeTag` is deleted from `src/ast.rs` and `src/eval.rs`. All constructor patterns use `Pattern::Constructor { tag: String, binding: Option<Box<Pattern>> }` for nominal variants only.

Dot-access patterns (`[Result.Ok v]`, `Color.Red:`) → `Pattern::Constructor` with syntactically assembled qualified tag. Bare uppercase words that resolve to nominal constructors → `Pattern::Constructor`; bare type names (`Int`, `Str`) → `Pattern::TypeAssert`.

**`Pattern::Seq` deleted** — erroneous fast path. The existing `[seq h t]` pattern syntax (parser.rs `("seq", 2)` arm at line 5011) is updated to produce `Pattern::Constructor { tag: "Seq.Cons", binding: Some(dict-destructure-of-head-and-tail) }` instead of `Pattern::Seq`.

### Opaque Types at Runtime

`Map: [type [let a b@Covariant] ...]` binds `Map` to `Placeholder` — exactly as `...` works everywhere. `Placeholder` errors only when forced as a value; type annotations are resolved at type-check time and never force `$Map`. Reflection: `ast-of` and function doc strings work via `Value::Function.annotation.doc`; key annotations (`Map@[doc: "..."]`) are properties of the declaring dict entry, not accessible from the bound value.

### Kind Registration

Type constructor arity determines the kind:

- 1 parameter → `Kind::Operator` (`* → *`)
- 2 parameters → `Kind::Arrow(Kind::Type, Kind::Operator)` (`* → * → *`)
- n parameters → n-deep `Kind::Arrow` chain

TyCon kind is derived from `TyConDef.variance.len()` — no separate TyCon kind registry needed. When `resolve_type_dict` or typeclass instance checking needs the kind of a TyCon, it looks up `TyConDef` through `TypeEnv` (scoped) or `EvalContext.tycon_env` (runtime) and reads the arity from `variance.len()`.

**`InferState.kind_env` is unaffected.** That field tracks *TypeVar* kinds — e.g., `f@Operator` in `[class [f@Operator] ...]` gives `f` kind `Kind::Operator`. This is entirely separate from TyCon kinds. `kind_env` must not be eliminated.

### Typeclass Instances

User-defined type constructors participate in typeclasses automatically once registered with the appropriate kind:

```tinct
[instance [Functor Tree]
  [fmap: [fn@[Tree B] [f@[Fn@B A]  t@[Tree A]] ...]]]
```

## What Would Change

### `src/typecheck_annot.rs` — Eliminate implicit TypeVar creation in alias bodies

**Current behavior:** Any lowercase name encountered in type annotation position (inside `resolve_annotation`) that is not already in scope becomes a fresh TypeVar. This makes `[type [Ok a] [Error String]]` silently create `a` as a TypeVar.

**New behavior:** Inside a type alias body, only names listed in the `[let ...]` params become TypeVars. All other lowercase names are scope references — looked up in the type environment as type aliases or TyCons. If a lowercase name is not in the params and not found in scope, it is a type error: `"undefined type variable 'x'; if you want a type parameter, declare it with [let x]"`.

**Implementation:** `resolve_annotation` (and its helper `resolve_type_dict`) receive a `type_params: Option<&HashSet<String>>` argument alongside the existing type environment:

- `Some(params)` — inside a type alias body; `params` is the set of names from `[let ...]`. Only these names become TypeVars.
- `None` — outside a type alias body (function annotations, typeclass constraints, etc.); existing behavior preserved — lowercase names become TypeVars as before.

Call sites: the TypeAlias inference pass in `typecheck.rs` passes `Some(&param_names)` when invoking `resolve_annotation` on the alias body. All other call sites pass `None`.

**No `InferState` field needed.** Passing `type_params` as an explicit argument keeps the function pure and avoids hidden state changes. The parameter threads through the recursive calls in `resolve_annotation` naturally.

**Impact:** Minor — add one parameter to `resolve_annotation` and its recursive calls; add one error case. No structural change.

### `src/type_def.rs` — Remove Seq/Map/Handle; add TyCon, RowTail::Uniform, TyConEnv

**Deleted:** `Type::Seq(Box<Type>)`, `Type::Map(Box<Type>, Box<Type>)`, `Type::Handle(Box<Type>)` — ~300 match arm occurrences total across `type_def.rs`, `type_unify.rs`, `type_normalize.rs`, `typecheck.rs`, `typecheck_annot.rs`, `typecheck_call.rs`, `typecheck_narrow.rs`, `eval.rs`, `eval_materialize.rs`, `imports.rs`, `builtins_core.rs`, `type_env.rs`, `type_class.rs`, `coverage.rs`, and test files.

**Added:**

- `Type::TyCon(String)` — one new arm in every exhaustive `match ty`.
- `RowTail` enum (created from scratch; does not exist in current codebase):

  ```rust
  pub enum RowTail {
      Empty,
      Uniform { key: Option<Box<Type>>, value: Box<Type> },
  }
  ```

  `key: None` = `{_ : V}` (value constraint only). `key: Some(K)` = `{_@K : V}` (key and value constrained). `Row` gains `tail: RowTail` field. Every `Row { fields: ... }` construction site adds `tail: RowTail::Empty`. All Row-traversing functions (`is_subtype`, `unify`, `collect_type_vars`, `has_type_vars`, `apply_inner`, `occurs_in`, `Display`) gain `Uniform` handling, including the optional `key` field.
- `TyConEnv: HashMap<String, TyConDef>` — flat snapshot type, used only in `EvalContext` for runtime lookup. Not stored in `InferState` as a live registry; derived from `TypeEnv` after type checking completes.

**Impact:** Major.

### `src/type_def.rs` (is_subtype) — Add `tycon_env` parameter

`is_subtype(sub: &Type, sup: &Type) -> bool` → `is_subtype(sub: &Type, sup: &Type, tycon_env: Option<&TyConEnv>) -> bool`.

`None` = invariant fallback for all `App(TyCon, _)` (safe conservative default — see §Subtyping). `Some(&state.tycon_env)` at type-checker call sites. `Some` from `EvalContext.tycon_env` at runtime call sites. ~100 mechanical call site updates.

**Impact:** Moderate — all call sites updated.

### `src/typecheck_annot.rs` — Delete builtin dispatch; add polarity analysis

**Deleted:**

- `fn apply_builtin_constructor(...)` — entire function
- `fn is_builtin_type_name` — entries "Seq", "Map", "Handle"
- `resolve_annotation` arms for "Seq" (lines ~168–179), "Handle" (lines ~180–198)
- `Annotation::Annotated` arms for "Seq" (line ~1053), "Map" (line ~1059), "Handle" (line ~1169)
- `resolve_type_dict` string-match arms for "Seq" (line ~2444), "Map" (line ~2461), "Handle" (line ~2505)
- Bare name resolution `"Seq"` → `Type::Seq(Unknown)`, etc. (lines ~1632–1652)

**Added:**

- Single general lookup path: look up name in type environment → produce `App(TyCon(name), args...)` or expand alias body.
- **`{_ : V}` recognition algorithm** in `typecheck_annot.rs` (`resolve_type_dict`): when parsing a type dict expression in annotation position, walk its key-value pairs and classify each key. Four sub-cases, all handled by the same annotation-dict walker:

  1. **Pure uniform** — only a `_` key, no named fields: `{_ : V}` → `Row { fields: HashMap::new(), tail: RowTail::Uniform { key: None, value: resolve(V) } }`.
  2. **Mixed named + uniform** — named fields AND a `_` key: `{host: String  _ : Int}` → `Row { fields: {"host": String}, tail: RowTail::Uniform { key: None, value: Int } }`. The `_` key is consumed as the uniform constraint; all other keys become named fields. At most one `_` per row type — duplicate `_` keys → type error "duplicate uniform-field sentinel in row type".
  3. **Typed-key uniform** — `{_@K : V}`: same recognition as case 1, but the `_` key carries an `@K` annotation via the existing ImmediateAt pattern. The recognizer extracts both the key-type annotation and the value type: `RowTail::Uniform { key: Some(resolve(K)), value: resolve(V) }`. No new syntax forms. The key-type annotation is resolved through the type environment the same as any other annotation. Compile-time type checking enforces that `K` satisfies `@Equatable`. Runtime key-type enforcement is deferred until T-921 (Key enum generalization); until then, proxy contracts check value type only.
  4. **Annotation shorthand** — `config@{_ : V}` or `f@{host: String  _ : Int}`: the `{_ : V}` expression appears after `@` as a type annotation. Recognition is identical — the same `resolve_type_dict` call applies; the surrounding `@` just places it as a TypeAssert annotation rather than a standalone type expression. No special-casing.

  The recognizer is a single pass over the key-value pairs: accumulate named fields; if a key is `_` (or `_@annot`), set the uniform tail. All type argument expressions are resolved via `resolve_type(v, env)` — same resolution path as named fields.

- **Polarity analysis pass** for transparent alias variance inference: new function `infer_variance(body: &Type, params: &[String], type_env: &TypeEnv) -> Vec<Variance>` in `typecheck_annot.rs` (Dolan 2017 §4). `TypeEnv` is the scoped type environment — TyConDef lookup for `App(TyCon(f), a)` goes through it, respecting lexical scope. Walks the body type, accumulates per-TypeVar polarity. Complete rule table:

  | Position | Rule |
  |---|---|
  | `TypeVar(x)` at top level | `x` is Covariant |
  | `TypeVar(x)` at top level, negated context | `x` is Contravariant |
  | `Record { fields }` | each field type is in covariant position |
  | `Function { params, ret }` | each param is in contravariant position; `ret` is covariant |
  | `App(TyCon(f), a)` | look up `f` in `TyConEnv`; if `f`'s parameter is `@Covariant`: `a` is in current polarity; if `@Contravariant`: flip current polarity for `a`; if `Invariant`: `a` is invariant regardless; if `@Phantom`: `a` contributes nothing |
  | `App(Operator(x), a)` | `x` is unknown variance → treat `a` as invariant (safe conservative — invariant can only reject valid subtyping, never accept invalid) |
  | `Union { members }` | each member is in the current polarity (join preserves polarity direction) |
  | `Intersection { members }` | each member is in the current polarity (meet preserves polarity direction) |
  | Negation (union/intersection complement) | flips polarity — covariant becomes contravariant and vice versa |
  | `TypeVar(x)` appears in both polarities | result is Invariant |
  | `TypeVar(x)` never appears | result is Phantom |

  The accumulator for each param starts at `None`. Seeing a covariant occurrence sets it to `Covariant`; contravariant → `Contravariant`; if both polarities are observed, override to `Invariant`. Final `None` → `Phantom`. Explicit `@` annotations (from `[let a@Covariant]`) override inference and serve as a checked declaration — the inferred variance is compared against the declared variance and a type error is raised if they conflict (e.g., `a@Covariant` but analysis finds `a` in contravariant position).

  This algorithm does not fall out of existing infrastructure — it is a new pass. Implement as a recursive function with a `Polarity { Positive, Negative }` parameter that tracks the current context polarity.

**Impact:** Major deletions; moderate additions.

### `src/type_unify.rs` — Add UNIFY-TYCON and uniform-row rules

**Added:**

- `UNIFY-TYCON`: `TyCon(n1)` and `TyCon(n2)` unify iff `n1 == n2` AND both names resolve to the same `TyConDef` in the current scoped `TypeEnv` (pointer identity). See §Unification in the design section for the full rule.
- Uniform row unification and subtyping (see §Column Constraints for rules).

**Deleted:** App normalization paths `App(Operator("Seq"), T) → Seq(T)` and similar.

**Impact:** Moderate.

### `src/value.rs` — Seq migrates from Value::Seq to Value::Variant

**Deleted:** `Value::Seq { head: ThunkId, tail: ThunkId }` — all match sites (~50 occurrences) in `eval.rs`, `eval_materialize.rs`, `builtins_seq_prim.rs`, `builtins_seq_gen.rs`, `builtins_seq_xform.rs`, `builtins_seq_reduce.rs`, `builtins.rs`.

**Added:** Seq values become `Value::Variant { tag: "Seq.Cons", payload: ThunkId }` (head/tail dict as payload) and `Value::Variant { tag: "Seq.Nil" }`. All sequence-producing builtins (`cons`, `range`, `iterate`, `repeat`, `cycle`, `concat`, etc.) migrate to `Value::Variant`.

**Impact:** Major.

### `src/desugar.rs` and `src/eval_dict.rs` — Consolidate constructor injection

**`src/desugar.rs:86`** (`inject_adt_constructors_expr`): redesign to handle both unit and named-field constructors, reading the type name from the enclosing dict entry key. See §Constructor Generation above for the complete specification.

**`src/eval_dict.rs:128-161`** (pre-scan for `CoreExpr::TypeDecl`): **deleted** once desugar handles all constructors.

**`src/ast.rs:958`** (`CoreExpr::TypeDecl { unit_constructors: Vec<String> }`): **deleted**. All constructor injection moves to the desugar pass as tinct-level expressions; the Rust-level pre-scan mechanism is no longer needed.

**`src/lower.rs`** (TypeAlias body lowering): Remove the `CoreExpr::TypeDecl` production site; TypeAlias `Decl` entries are already desugared and their constructors appear as ordinary `SurfaceEntry` nodes — the lowering pass processes them normally.

**Impact:** Major — complete redesign of constructor injection; atomic with VARIANT_TAG_MARKER deletion.

### `src/eval_call.rs` — Delete VARIANT_TAG_MARKER

`VARIANT_TAG_MARKER` constant and the entire special-casing block (~50 lines at ~152–198) deleted. `variant` builtin updated to two modes. Constructors become ordinary values/functions.

**Impact:** Moderate.

### `src/ast.rs` and `src/eval.rs` — Pattern AST cleanup

**Deleted:**

- `Pattern::TypeTag` — erroneous fast path for unit constructors.
- `Pattern::Seq` — erroneous fast path for sequence patterns. Spread pattern `[h ...t]` desugars to `Pattern::Constructor { tag: "Seq.Cons" }` + dict destructuring.

**Added:**

- `Pattern::TypeAssertPending { annotation: Spanned<Annotation>, inner: Option<Box<Spanned<Pattern>>> }` — surface form; parser-only, never reaches the evaluator.
- `Pattern::TypeAssert { resolved_type: Type, inner: Option<Box<Spanned<Pattern>>> }` — core form; typecheck elaboration produces this from `TypeAssertPending`.

All constructor patterns use `Pattern::Constructor { tag: String, binding: Option<Box<Pattern>> }`.

**Impact:** Moderate.

### `src/ast.rs` and `src/parser.rs` — TypeAlias params carry variance

**Critical:** The parser's TypeAlias push_value handler (`parser.rs:5253-5258`) currently does `params.push(name.clone())` for both plain `VarRef` and `Annotated` bindings — silently discarding `@Covariant` etc. No variance feature works until this is fixed. Three required changes:

1. `SurfaceDeclaration::TypeAlias { params: Vec<String>, body }` → `params: Vec<(String, Option<Spanned<Annotation>>)>`
2. Parser at `parser.rs:5253-5258`: capture annotation alongside name — `params.push((name.clone(), Some(annotation.clone())))` for `Annotated`; `params.push((name.clone(), None))` for `VarRef`
3. Type checker: read annotations to populate `TyConEnv` variance entries

All three must land together before any variance sprint. Complete set of downstream change sites (mechanical pattern updates — change `params.push(name)` → `params.push((name, ann))` and `params.iter()` → `params.iter().map(|(n, _)| n)` where only the name is needed):

- `src/ast.rs:393` — `SurfaceDeclaration::TypeAlias { params, body }` match arm (display/debug)
- `src/parser.rs:2093` — construction: `SurfaceDeclaration::TypeAlias { params, body }`
- `src/parser.rs:5253-5258` — push_value handler (the primary change site)
- `src/parser.rs:6610`, `6946`, `7015`, `7038`, `7059` — match arms that destructure `params`
- `src/typecheck.rs:937` — reads `params` to build type param scope
- `src/typecheck_dict.rs:380` — reads `params` to register alias
- `src/eval.rs:1555` — processes TypeAlias params
- `src/eval_core.rs:394` — processes TypeAlias params
- `src/surface_convert.rs:1820` — converts TypeAlias

**Impact:** Moderate — 12 sites, all mechanical; one semantic change (type checker reads annotation field).

### `src/parser.rs` — Pattern AST changes

Two new pattern variants in `src/ast.rs` (see §Pattern AST for full spec):

- `Pattern::TypeAssertPending { annotation: Spanned<Annotation>, inner }` — surface form; parser produces this
- `Pattern::TypeAssert { resolved_type: Type, inner }` — core form; typecheck elaboration produces this

**New arm in `surface_node_to_pattern_with_guard` (parser.rs:4843)** for `SurfaceExpression::TypeAssert`:

```rust
SurfaceExpression::TypeAssert { annotation, expr } => {
    let inner_pat = surface_node_to_pattern(Arc::clone(expr))?;   // guard-discarding wrapper
    (Pattern::TypeAssertPending { annotation: annotation.clone(), inner: Some(Box::new(inner_pat)) }, None)
}
```

**DotAccess pattern arms** — two new sites in `surface_node_to_pattern_with_guard`:

1. Before the `else { "invalid pattern" }` branch at line 5053, split the Call arm to handle `DotAccess` func heads via `flatten_dot_access_to_tag`
2. New bare `SurfaceExpression::DotAccess` arm for unit constructor patterns (`Color.Red:`)

See §Constructor Access and Patterns for full specification of `flatten_dot_access_to_tag` and both insertion points.

**`typecheck_match.rs` elaboration pass** — two kinds of work:

For `Pattern::TypeAssertPending { annotation, inner }`:

- Call `resolve_annotation(&annotation.node, &env, annotation.span, state, ...)` — the full compile-time resolver
- Produce `Pattern::TypeAssert { resolved_type: resolved_type, inner }`

For `Pattern::Constructor { tag, binding }` (bare uppercase or dot-assembled):

1. Look up `tag` in `TyConEnv` — nominal (has constructors) → keep as `Pattern::Constructor`, qualify via scrutinee type
2. Look up `tag` in `TyConEnv` — primitive (`builtin_type` set, no constructors) → rewrite to `Pattern::TypeAssert { resolved_type: Type::TyCon(tag.clone()), inner: binding }`
3. Not found → type error: "undefined type name or constructor `X` in pattern position"

No hardcoded list. TyConEnv is the single source of truth.

**`Pattern::Constructor.tag`:** stores the qualified tag string. Propagates through `eval.rs`, `typecheck_match.rs`, `coverage.rs`.

**Impact:** Moderate — two new AST variants, three new parser arms, elaboration pass changes.

### `src/eval_materialize.rs` — value_matches_type

**Deleted:** `Type::Seq(_)` and `Type::Map(_,_)` arms — Seq uses nominal variant tags; Map is a transparent alias using uniform-row checking.

**Kept:** One Rust-side dispatch in `value_matches_type` on the `builtin-type` discriminant string extracted from the TyConDef. The discriminant strings are defined in prelude (`Int`, `Str`, `Bool`, `Float`, `Bytes`, `Dict`, `Fn`, `Handle`); the Rust dispatch table maps those strings to value predicates. All other TyCons use the nominal variant tag check. Adding a new Rust-backed type requires one prelude line and one dispatch arm — no other code changes.

**Added:** Uniform-row matching `{_ : V}` via proxy contracts.

**Impact:** Moderate.

### `src/value.rs` — Qualified variant tags

All `Value::Variant` constructions with bare tags change to qualified:

- `builtins_meta.rs:337` — `"Ok"` → `"Result.Ok"`
- `builtins_meta.rs:355` — `"Error"` → `"Result.Error"`
- All ADT constructor injection sites in `eval_dict.rs` — prefix tag with type name
- All `match variant_tag { "Ok" => ...}` comparisons — updated to qualified form

**Impact:** Major — every nominal variant usage in the codebase.

### `src/coverage.rs` — Exhaustiveness for all nominal TyCons

**Current:** `check_coverage` has a hardcoded arm `Type::Seq(_) => constructors.push(TypeTag("Seq".into()), 0)`. All other TyCon exhaustiveness is absent.

**New approach:** For any `App(TyCon(name), _)` or `TyCon(name)` scrutinee, look up `name` in `TyConEnv`:

```rust
Type::TyCon(name) | Type::App(box Type::TyCon(name), _) => {
    if let Some(def) = tycon_env.get(name) {
        for (qualified_tag, arity) in &def.constructors {
            constructors.push(ConstructorSignature {
                tag: qualified_tag.clone(),
                arity: *arity,
            });
        }
    }
    // builtin-type TyCons (Int, Str, etc.) have empty constructors vec → no exhaustiveness
}
```

**Arity-only suffices for coverage.** `check_coverage` (Maranget 2007) needs only the constructor tag and its arity — not the field types. Whether `Result.Ok` carries a payload dict with field `value: Int` or `value: String` does not affect whether the pattern `[Result.Ok _]` is present. Field types are irrelevant to matrix decomposition; they matter only for type-checking the bindings within a pattern arm, which is handled separately in `typecheck_match.rs`. So `Vec<(String, usize)>` in `TyConDef` is exactly the right level of detail — no field-type substitution needed.

**Exhaustiveness is undecidable only for builtin-type TyCons** (`Int`, `Str`, etc.) because they have no finite constructor set. A match on `Int` without a wildcard arm is incomplete — the same behavior as today.

**Impact:** Moderate — replaces hardcoded Seq arm with a general TyConEnv lookup; removes one hardcoded type name from coverage.rs.

### `src/eval.rs` — ground_type_of, values_equal, lowering

`ground_type_of(Value::Seq { .. })` → updated to use variant tag lookup. Field type annotations in ADT bodies (`[Seq a]` in `tail: [Seq a]`) are type-checker-only — not lowered to runtime `CoreExpr`.

**Merge all three `values_equal` variants into one canonical function.**

Tinct currently has three equality implementations, all divergent:

| Function | Location | Used by | Handles |
|---|---|---|---|
| `values_equal` | `eval.rs:3531` | `CaseArmExactValueCheck` (exact-value pattern arms) | async; Dict, nullary Variant, Seq; **missing payload-Variant** |
| `values_eq_impl` | `builtins_math.rs:488` | `=` builtin (`builtin_eq`) | async; most complete; has payload-Variant |
| `values_equal` | `builtins_meta.rs:2937` | `CaseArmExactValueCheck` (actual call site) | **sync**; only Int/Float/String/Bool/empty-dict |

The actual call site for exact-value pattern arms uses the **sync** `builtins_meta` version — the most limited. This means `[match [Result.Ok value: 1]  [Result.Ok value: 1]: "hit"  _: "miss"]` silently produces `"miss"`. Exact-value arms never match on any Variant, non-empty Dict, or Seq. This is the same class of divergence bug that motivated this analysis.

The invariant that must hold: **`[= a b]` returns true if and only if `a: arm` matches `b`**. All three paths must implement the same semantics or this invariant is violated.

**Migration:** `values_equal` in `src/eval.rs` becomes the single canonical implementation. `values_eq_impl` in `src/builtins_math.rs` and `values_equal` in `src/builtins_meta.rs` are **both deleted**. All callers updated:

- `builtin_eq` in `builtins_math.rs` → calls `eval::values_equal`
- `CaseArmExactValueCheck` → calls `eval::values_equal` (via a new async CEK continuation, replacing the current sync call)

**Complete canonical `values_equal`** after this sprint (signature unchanged — already async):

The Dict arm carries forward the existing cycle detection from `values_eq_impl` (`builtins_math.rs:527-535`): a `visited: Arc<Mutex<HashSet<(usize, usize)>>>` parameter threads through recursive calls to detect circular dict references (possible via letrec). Without it, `[= x x]` where `x: [a: x]` diverges.

Arms:

- `(Int, Int)`, `(Float, Float)`, `(Bool, Bool)`, `(String, String)` — existing, unchanged
- `(Variant { payload: None }, Variant { payload: None })` — existing unit-constructor arm, unchanged
- `(Variant { tag: t1, payload: Some(p1) }, Variant { tag: t2, payload: Some(p2) })` — **new**: tag check then materialize payloads and recurse
- `(Dict, Dict)` — existing structural equality with cycle detection, unchanged
- `(Overlay, ..)` — check whether `Value::Overlay` still exists post-migration; if so, carry forward from `values_eq_impl`
- `_` — `Ok(false)` for all cross-type comparisons

**`Value::Seq` arm deleted** — erroneous fast path. Seq values are now `Value::Variant`; they fall into the unit or payload-Variant arms above. **Must land atomically with the Seq migration** — deleting the Seq arm before Seq values migrate to Variant makes all Seq equality return false.

**`CaseArmExactValueCheck` fix** — no new continuation variant. The handler at `eval_materialize.rs:3413` calls `crate::builtins_meta::values_equal(...)` synchronously today. Replace that single call with `eval::values_equal(pattern_value, scrutinee_value, span, Arc::clone(&ctx)).await?` in the existing handler — the handler is already inside `async fn run_cek`. Pre-clone `scrutinee_value` before the call so the no-match branch can still pass it to `MatchDispatch`.

**`builtins_meta.rs:2789`** — enum constraint handler (`enum:` schema key) calls sync `values_equal` in a synchronous context. The `validate` builtin is called from within the CEK machine, which is an async context. The enum handler must be updated to use `eval::values_equal(...).await?` — the same pattern as `CaseArmExactValueCheck`. A thin sync wrapper would be wrong (it would block the async runtime). The enum handler's surrounding code must become `async` if it is not already; follow the same pattern as `builtin_eq` in `builtins_math.rs`.

**Impact:** Moderate — two functions deleted; `builtin_eq`, `CaseArmExactValueCheck`, and `builtins_meta.rs:2789` call sites updated; `values_equal` gains payload-Variant arm and cycle-detection parameter; `Value::Seq` arm removed.

### `src/eval.rs` — EvalContext.tycon_env

`EvalContext` gains `tycon_env: OnceLock<Arc<TyConEnv>>` — not `RwLock`, because the snapshot is populated exactly once at the transfer site and never mutated afterward. `OnceLock` makes the immutability invariant machine-checkable. `boundary_guards` and `do_infer_resolutions` use `RwLock` because they are populated incrementally; `tycon_env` is set once.

**All 9 EvalContext constructors** must be updated (`with_base_dir_and_path` at line 784 is the 9th; it delegates to `with_base_dir` and inherits the change automatically):

| Constructor | Line | Init |
|---|---|---|
| `new` | 587 | delegates to `new_with_options` — no direct change |
| `new_empty` | 611 | `tycon_env: OnceLock::new()` |
| `new_with_options` | 647 | `tycon_env: OnceLock::new()` |
| `new_sharing_arena` | 705 | `tycon_env: OnceLock::new()` |
| `with_base_dir` | 756 | see child pattern below |
| `with_cancel_token` | 800 | see child pattern below |
| `with_explicit_cancel` | 829 | see child pattern below |
| `with_timeout_ms` | 854 | see child pattern below |
| `with_base_dir_and_path` | 784 | delegates to `with_base_dir` — no direct change |

**Child constructor pattern** (for `with_base_dir`, `with_cancel_token`, `with_explicit_cancel`, `with_timeout_ms`):

```rust
tycon_env: {
    let child_lock = OnceLock::new();
    if let Some(env) = self.tycon_env.get() {
        child_lock.set(Arc::clone(env)).ok(); // ok() — child_lock is freshly created, set always succeeds
    }
    child_lock
}
```

**`set_tycon_env` implementation:**

```rust
pub fn set_tycon_env(&self, env: HashMap<String, TyConDef>) {
    self.tycon_env.set(Arc::new(env)).expect("tycon_env already set — set_tycon_env called twice");
}
```

Panics on double-set because calling `set_tycon_env` twice is a programming error (the type checker should only run once per EvalContext). Child contexts that were pre-populated from a parent never have `set_tycon_env` called on them.

Root constructors initialize to empty `OnceLock::new()`; the TyConEnv is filled by `set_tycon_env` at the transfer sites (see below).

**Transfer sites:** The `TyConEnv` snapshot is the `infer_state.tycon_env` flat accumulator (populated incrementally during type checking, one entry per TyCon declaration encountered, including prelude). It covers TyCons from all pipeline documents because the type checker processes them sequentially with the accumulator persisting across `---` boundaries. Transferred to `EvalContext` at all infer-state transfer points:

- `src/lib.rs:341-342` — the primary lib.rs path (`eval_source`):

  ```rust
  ctx.set_boundary_guards(infer_state.boundary_guards);
  ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
  ctx.set_tycon_env(infer_state.tycon_env);         // ← add this line
  ```

- `src/lib.rs:483-484` — the second lib.rs path (`eval_source_with_cap_net`), same pattern — `set_tycon_env` must be added here too.
- `src/main.rs:2268-2269` — the CLI path:

  ```rust
  eval_ctx.set_boundary_guards(infer_state.boundary_guards);
  eval_ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
  eval_ctx.set_tycon_env(infer_state.tycon_env);    // ← add this line
  ```

- `src/repl.rs:249` — the REPL path calls `typecheck_surface_program_with_env` which builds `infer_state` (returned as `_state`, currently fully discarded). The REPL is missing all three transfers. Add all three:

  ```rust
  repl_ctx.set_boundary_guards(infer_state.boundary_guards);
  repl_ctx.set_do_infer_resolutions(infer_state.do_infer_resolutions);
  repl_ctx.set_tycon_env(infer_state.tycon_env);
  ```

When `--no-typecheck` is used, `tycon_env` is never populated — `EvalContext` holds an empty `TyConEnv`, and `is_subtype` falls back to invariant (None path).

**Macro expansion contexts get empty TyConEnv by design.** `expand.rs` creates fresh EvalContexts for macro evaluation. These contexts intentionally have no TyConEnv — macro expansion runs before type inference, so TyConEnv does not yet exist. Correct behavior, not a gap.

**`do_infer_resolutions` is eliminated by the `[do ...]` desugaring change.** When `[do ...]` desugaring produces a complete AST at typecheck time, there are no sentinel variables and no runtime side-channel. The `set_do_infer_resolutions` calls at all transfer sites are removed, not added.

**Impact:** Minor — `tycon_env` field + OnceLock + transfer calls; `do_infer_resolutions` field and transfer calls removed.

### `src/type_infer.rs` and `src/type_unify.rs` — Per-dict InferState scoping

**`InferState` restructured** to replace globally-accumulated fields with per-dict scoped equivalents:

**Deleted from `InferState`:**

- `subst: Substitution` — replaced by per-dict substitution passed as parameter to each `infer_dict` call
- `name_counter: u32` — replaced by per-dict local counter (no global uniqueness required)
- `levels: HashMap<String, u32>` — replaced by per-dict levels map (follows from per-dict names)

**`Substitution` restructured** (`src/type_unify.rs`):

```rust
pub struct Substitution {
    pub type_map: RefCell<HashMap<String, Type>>,
    pub parent: Option<Arc<Substitution>>,
    pub creation_level: u32,  // Kiselyov level at which this frame was created
}
```

- Each `infer_dict` call creates a fresh `Substitution::child(outer, current_level)` where `current_level` is the level pushed for this dict
- `apply(TypeVar(name, level))`: check local map first (path-compression cache); if not found, walk parent chain
- `bind(TypeVar(name, level), T)`: walk the parent chain to find the `Substitution` frame whose `creation_level == level` and write there. No new field in `TypeVar(String, u32)` — the existing `u32` is already the Kiselyov creation-time level. Since tinct's SCC loop processes one entry at a time, only one frame exists at each level during processing — the level is a sufficient frame discriminant.
- **The stated invariant "inner dicts never bind outer TypeVars" is false** (rejected by computer-scientist and confirmed by skeptic reading the actual code). The correct invariant: inner dicts MAY bind outer TypeVars (e.g., when A and B are in the same SCC and B contains an inner dict that processes while A's TypeVar is still unbound). The mechanism correctly routes these bindings to the outer frame via level-keyed chain walk.
- When inner dict finishes: extract TypeSchemes (fully generalized); discard inner substitution; bindings to outer-frame TypeVars have already been written to the outer frame directly and persist.

**Constraint queue made explicit:**

- `constraints: Vec<Constraint>` removed from `InferState`
- Becomes a local `Vec<Constraint>` parameter threaded through `infer_dict` and its callees
- Removes the manual `std::mem::take`/`replace` save/restore pattern at `typecheck_dict.rs:588`; scoping is now enforced by the type system rather than by convention

**`InferState` retains** all fields that are genuinely cross-dict: `class_env`, `instance_env` (now scoped via parent chain — see §InstanceEnv and §ClassEnv), `tycon_env` accumulator, `boundary_guards`, `diagnostics`, `scheme_map`, `type_annotation_table`, `registered_nominal_tags` (now scope-local), `expects_resolved`.

**Impact:** Major — restructures the core type inference pass. Every function in `typecheck.rs`, `typecheck_dict.rs`, `typecheck_annot.rs`, `typecheck_call.rs`, `typecheck_match.rs`, `typecheck_narrow.rs` that takes `&mut InferState` and accesses `state.subst`, `state.name_counter`, or `state.constraints` requires updating. The five-pass `[DICT-GEN]` algorithm's SCC loop passes a per-SCC substitution rather than relying on the global one.

### `stdlib/prelude.llt` — Full migration

One coherent sprint rewrites the entire prelude:

**New declarations:**

```tinct
--- stage: type
Variance: [type Covariant Contravariant Invariant Phantom]
Seq:    [type [let a@Covariant]  Nil  [Cons head: a  tail: [Seq a]]]
Map:    [type [let k@Equatable v]  [_@k : v]]
Handle: [type [let a] ...]
Absent: [type Absent]
absent?: [fn@Bool [let x@Unknown] [match x Absent.Absent: true  _: false]]
```

**`do-desugar-inferred` and `do-var-node` deleted.** These two prelude functions (`prelude.llt:3496-3498` and `prelude.llt:3398-3399`) power the current inferred-monad `[do ...]` path — `do-var-node` generates sentinel `VarRef` nodes (`ℊꜱʏᴍ⧼do-infer⧽N`), `do-desugar-inferred` assembles them into the `[do ...]` body. Once `[do ...]` desugaring produces a complete AST at typecheck time (with `bind_node` and `pure_node` embedded inline), no sentinel variables appear in the emitted AST and these prelude functions are unreachable. Delete both. The `[macro do ...]` declaration itself stays; only its inferred-monad branch is replaced by the typecheck-time elaboration path.

**Testing unit constructors with `=` is the correct idiom.** Unit constructors like `Absent.Absent`, `Seq.Nil`, `Maybe.None`, `Color.Red` are plain `Value::Variant` values with no payload. Value equality (`=`) compares them structurally: same tag, both payload-less → equal. Writing `[= x Seq.Nil]` is exact, typed, and embeds no string literals. Pattern matching handles multi-case dispatch. `tag-of` is for dynamic introspection — when the specific type is not known at write time, or when a serializer needs the tag string — not for typed predicates.

`absent?` is a named predicate because it expresses a ubiquitous semantic concept (missing dict field, unset env var) that users encounter at every optional boundary. `seq-nil?` is not added — users doing recursive sequence processing use pattern matching; users calling `map`/`filter`/`reduce` never inspect `Nil` directly; and `[= tail Seq.Nil]` is already clear without a new name.

`Seq.Nil` was previously `Value::Dict({})`, so `[null? tail]` returned true at end of sequence. After migration, `Seq.Nil` is `Value::Variant { tag: "Seq.Nil" }` and `null?` returns false. All `[null? tail]`, `[null? [rest xs]]`, and similar end-of-sequence checks in prelude migrate to `[= tail Seq.Nil]`.

**Type declarations migrated to `[let ...]`:**

```tinct
Result: [type [let a]  [Ok value: a]  [Error msg: String]]
Maybe:  [type [let a]  [Some value: a]  None]
# All other parameterized types: add [let ...] as needed
```

**Common aliases:**

```tinct
Ok: Result.Ok   Error: Result.Error   Some: Maybe.Some   None: Maybe.None
```

**All constructor usages** in prelude rewritten to qualified form: `Ok` → `Result.Ok`, `[Ok v]` → `[Result.Ok v]`, `None:` → `Maybe.None:`, etc.

**Builtin returns updated:** `get?`, `env`, `head`, `get-in?` return `Absent.Absent` instead of `[]` for missing/absent values. These Rust builtins must emit `Value::Variant { tag: "Absent.Absent", payload: None }` instead of `Value::Dict(empty)`.

**`null?` stays; `absent?` is added.** `null?` checks for `[]` (empty dict, meaning "empty collection"), which is still a valid value. The two predicates are NOT interchangeable:

- `null?` — `[match x []: true  _: false]` — true iff the value is `[]`
- `absent?` — `[fn@Bool [let x@Unknown] [match x Absent.Absent: true _: false]]` — true iff the value is `Absent.Absent`; `Unknown → Bool`

After the migration, `[get? "missing-key" d]` returns `Absent.Absent`, not `[]`. Code that tests the result with `null?` will silently fail (get false instead of true). All such call sites in prelude must be migrated to `absent?`.

**`null?` migration scope in prelude.llt** — current call sites using `null?` to mean "value is absent":

- `prelude.llt:2860` — `[null? integrity-hash]` checks a value that may be absent; → `[absent? integrity-hash]`
- All `get?`/`env`/`get-in?`/`head` result checks elsewhere in prelude and stdlib that use `null?` → `absent?` (search: `[null? [get?` and `[null? [env`)

User code that uses `null?` to check for empty dicts (e.g., `[null? [keys d]]`) is NOT broken — those values are `[]` before and after.

**Impact:** Major — every type declaration, every constructor usage, every pattern match in prelude; plus Rust builtin return-value changes and null? → absent? call site migration.

**Atomicity requirement: Rust tag changes + prelude migration must land in one commit.** The runtime tags `"Ok"`, `"Error"`, `"Some"`, `"None"` are produced by Rust builtins AND consumed by tinct pattern matches in prelude. Any half-state where Rust produces `"Ok"` but prelude expects `"Result.Ok"` (or vice versa) makes all `try`-using code fail at runtime. The required atomic bundle:

1. **Rust tag production sites** — all files producing bare variant tags must be updated together:
   - `builtins_meta.rs:337` — `"Ok"` → `"Result.Ok"`
   - `builtins_meta.rs:355` — `"Error"` → `"Result.Error"`
   - `builtins_async.rs:549, 590, 652, 846, 1242` — `"Ok"` (recv/send/select result tags)
   - `builtins_net.rs:2666, 2811` — `"Ok"` (HTTP response); `2824` — `"Error"`
2. **Rust tag consumption sites** — all Rust code comparing against bare tags:
   - `formatter.rs:155, 175` — `tag == "Ok"` / `tag == "Error"` → `"Result.Ok"` / `"Result.Error"`
   - All `match variant_tag { "Ok" => ... }` comparisons elsewhere
3. `stdlib/prelude.llt`: all constructor definitions, pattern arms, and usages updated to qualified form
4. The common aliases `Ok: Result.Ok`, `Error: Result.Error`, `Some: Maybe.Some`, `None: Maybe.None` added

Steps 1–4 are a single commit. The test suite must pass in full at that commit boundary — no intermediate broken state.

**`typecheck_special.rs:729` — `[do ...]` monad detection must be generalized.** `resolve_monad_from_surface` currently identifies the monad by checking `name == "Ok" || name == "Error"` on VarRef-headed Call nodes — hardcoded constructor names. This works for unqualified `[Ok x]` (the prelude alias path) but fails silently for the qualified form `[Result.Ok x]` (a DotAccess-headed Call, never inspected).

The correct fix generalizes the detection to work for any constructor of any nominal type. `resolve_monad_from_surface` currently has signature `fn resolve_monad_from_surface(node: &Arc<SurfaceNode>) -> Option<String>` with no environment access. The function is called as "Rule 2b — AST fallback" inside `typecheck_special.rs` where `state: &mut InferState` is available. Add a `type_env: &TypeEnv` parameter and update the call site at line 580 to pass it.

**Generalized implementation:**

```rust
pub(crate) fn resolve_monad_from_surface(
    node: &Arc<SurfaceNode>,
    type_env: &TypeEnv,
) -> Option<String> {
    let qualified_tag = match &node.expr {
        SurfaceExpression::Call { func, implied: true, .. } => match &func.expr {
            SurfaceExpression::VarRef { name, .. } =>
                // Resolve alias through TypeEnv to get qualified constructor tag
                type_env.resolve_constructor_tag(name)
                    .unwrap_or_else(|| name.clone()),
            dot_expr => flatten_dot_access_to_tag(dot_expr)?,
        },
        _ => return None,
    };
    // "TypeName.CtorName" → "typename"; bare unresolved name → None
    let tycon = qualified_tag.rfind('.').map(|pos| &qualified_tag[..pos])?;
    Some(tycon.to_lowercase())
}
```

No hardcoded strings, no fallback to bare name matching. `[Ok x]` → TypeEnv resolves `Ok` to `"Result.Ok"` → TyCon name `"Result"`. `[Result.Ok x]` → `flatten_dot_access_to_tag` → `"Result.Ok"` → TyCon name `"Result"`. A bare unresolvable name → `rfind('.')` returns `None` → function returns `None` → Rule 3 fires → type error.

`TypeEnv.resolve_constructor_tag(name: &str) -> Option<String>` is a new helper on `TypeEnv` (see §`src/typecheck_annot.rs` for the implementation sketch). Must land same sprint as the qualified-tag migration.

**After this change, `resolve_monad_from_surface` returns the TyCon name (`"Result"`, not `"result"`).** The monad dispatch then looks up the `Monad` instance for that TyCon in the instance registry — NOT by constructing a lowercase dict name and searching the environment. The current name-based lookup (`"result"` → `env.get("result")`) is a workaround from before the typeclass system existed; it constrains how users name their monad dicts and prevents custom monad types from working without knowing the naming convention. That workaround is replaced here.

**Correct monad dispatch — typecheck-time resolution, lexically scoped:**

`[do ...]` desugaring happens in `typecheck_special.rs` during the typecheck pass, where the scoped type environment (`InferState`) is available. Instance lookup must use this scoped environment — NOT the global eval-time `instance_registry` in `EvalContext.state`. The global registry is mutable and unscoped; an instance registered in a local dict would leak globally and could shadow instances in unrelated scopes.

The correct mechanism:

1. `resolve_monad_from_surface` identifies the TyCon (e.g., `"Result"`)
2. Look up the `Monad Result` instance in **`state`'s current lexically-scoped instance environment** — same scope chain as type aliases and variable bindings
3. Extract the `bind` and `pure` method references from that instance declaration
4. Desugar `[do ...]` into a direct call using those specific method references — embedding them as `VarRef` or closure expressions in the output AST
5. The evaluator sees a plain expression — no runtime instance lookup, no global registry

```rust
// typecheck_special.rs — during [do ...] elaboration
// state.instance_env is InferState's existing instance environment;
// it must be made scoped (new infrastructure — see below)
let instance = state.instance_env.lookup_scoped("Monad", tycon_name)
    .ok_or_else(|| TypeError::new(format!("no Monad instance for {tycon_name}")))?;
// instance.bind_expr() returns the parsed bind method as a SurfaceNode reference
// [do x <- m; body] desugars to [bind_expr m [fn [x] body]] embedded in the output AST
let bind_node = instance.bind_expr();
let pure_node = instance.pure_expr();
```

**`InferState.instance_env` must be made scoped.** The current implementation comment says "Globally registered: coherence requires global uniqueness" — this reflects Haskell's constraint, not tinct's. In tinct:

- There is no separate compilation requiring a globally unique dispatch token
- Dispatch resolves at elaboration time using the in-scope instance at the definition site (lexical scoping)
- Functions that capture a method capture a specific closure, not a dispatch token to be resolved later

The correct coherence rule for tinct is **local (scope-level) coherence**: within a single scope frame, at most one instance per `(Class, Type)` pair. Across scope levels, shadowing is allowed — the innermost instance wins, exactly like all other bindings. Two `[instance [Monad Result] ...]` in the same dict is a type error (duplicate within one frame); one in an outer scope and one in an inner scope is fine (inner shadows outer).

**Both `InstanceEnv` and `ClassEnv` must become parent-chain scoped**, following the same model as `TypeEnv`:

- A `HashMap` per scope frame with a parent pointer
- Insertions go into the current frame; lookups walk the chain with inner-wins semantics
- Prelude classes and instances live in the root frame — visible everywhere
- A class or instance in an inner dict is visible only to that dict's descendants

**Class shadowing semantics:** An inner `[class [Eq a] ...]` in a nested scope SHADOWS the outer `Eq` class for all code within that scope — inner wins, same as all other bindings. This is not an error and not silently discarded. Users can locally specialize a class interface (different method signatures, different constraints). Instance resolution and constraint checking within the inner scope use the inner class declaration. Code outside the inner scope continues to use the outer class. Two `[class [Eq a]]` declarations in the SAME scope frame (same dict, two sibling entries) ARE a type error — ambiguous within that scope, same rule as duplicate value bindings.

This eliminates the distinction between "monad dispatch" and "constraint solving" — they're all in the same scoped environment. The "Globally registered: coherence requires global uniqueness" comment in `type_class.rs` reflects Haskell's constraint; it does not apply to tinct. Global coherence is not required when:

- There is no separate compilation (the whole program is checked together)
- Dispatch resolves at elaboration time using the in-scope instance at the definition site
- Functions capture their method closures at definition, not at call site

**Local coherence is sufficient and is the only coherent option in tinct.** Implicit use-site dispatch is architecturally impossible in tinct's model: the *only* things that cross a call boundary are explicit function arguments. Any mechanism for injecting a different instance at a call site — hidden dictionary, thread-local, global registry lookup — would violate the scoping model. If a user wants call-site-variable behavior, they pass the discriminating function as an explicit argument:

```tinct
# Explicit — caller chooses; this is not typeclass dispatch, just function arguments
f: [fn [x@Color y@Color eq@[Fn@Bool [Color Color]]] [eq x y]]
[f red blue Color.eq-by-rgb]
[f red blue Color.eq-by-name]
```

The typeclass system is an inference convenience that eliminates writing the instance argument when it is unambiguous in scope. The underlying mechanism is always explicit at the value level. Local coherence is not a design choice that might change — it is a consequence of the scoping model.

**For macro-synthesized `[do ...]` blocks**, the captured monad instance is the one in scope at the macro EXPANSION site — standard lexical behavior, since typecheck runs after macro expansion. This is correct: the macro expands into source that is then type-checked in the scope where the expansion occurred.

**Class/instance seeding at `imports.rs:539-566`:** The seeding path iterates `class_env.iter_classes()` and calls `insert_if_absent` to populate the user frame with prelude classes. With framed ClassEnv/InstanceEnv: `iter_classes()` is a frame-local iterator (does not traverse parent chain). `insert_if_absent` must check the FULL parent chain before inserting — not just the current frame — to avoid re-inserting prelude classes into the user frame when they're already visible via the root frame's parent chain. The seeding is a root-frame initialization; no double-insertion occurs if the seeding frame IS the root. Verify in the sprint that seeding always runs against the root frame before any user frames are created.

**`InferState.registered_nominal_tags`** — the W042 duplicate-tag warning — must be made scope-local. The runtime tag string (`"Color.Red"`) does NOT encode the enclosing scope path — that would be wrong for the same reason encoding module paths in variable names is wrong. The SCOPE CHAIN is the disambiguation mechanism, not the string.

Two `Color` types in different scope frames both produce runtime tag `"Color.Red"`. This is correct and expected: the type checker uses the scoped `TyConDef` lookup to distinguish them (UNIFY-TYCON requires pointer identity of the resolved `TyConDef`, not just name equality). At runtime (post-type-erasure), `Value::Variant { tag: "Color.Red" }` values from different scopes are structurally equivalent — the type checker has already ensured they are never mixed in typed code.

W042 fires only when the same tag string is declared twice within the SAME scope frame — that is a genuine collision, because within one scope, `Color` resolves to two different declarations and pattern matching `Color.Red:` becomes ambiguous. Cross-scope same-name types are not errors; the scope chain provides unambiguous resolution at each use site.

**Dead code to delete:** `EvalState.class_registry`, `EvalState.instance_registry`, `EvalState.registered_classes` — never populated, never read. Remnants of an abandoned runtime typeclass design. Delete them.

**Type inference state must become per-dict scoped.** The current `InferState` carries globally shared state that is logically local to each dict's checking pass: the unification substitution (`InferState.subst`), type variable level map (`InferState.levels`), and name counter (`name_counter`). These are currently global across all dicts in a checking session as an implementation convenience, not a theoretical requirement. The scoping guarantees specified in this whatif (InstanceEnv, ClassEnv, TyConDef all following lexical scope) are hollow if the underlying type inference machinery is still globally shared across dicts. The end state this whatif describes requires per-dict scoping throughout.

The correct scope for each:

- **Substitution** — each dict gets a fresh `Substitution` during its SCC checking pass. TypeVars from outer scopes are resolved via parent-chain lookup (inner scopes can see outer scopes through the TypeEnv parent chain). When the inner dict is done, only its exported TypeSchemes (fully generalized, no raw TypeVars) cross back to the outer scope. The inner substitution is discarded. Two workers type-checking sibling dicts never touch the same substitution.
- **Type variable names** — each dict uses local numbering (`_t0`, `_t1`...). No global uniqueness is required once substitutions are per-dict — the same name in different dict-substitutions never collides. The global `name_counter` is eliminated.
- **Level counter** — levels are already pushed/popped at dict boundaries. The `levels: HashMap` accumulates globally only because TypeVar names are currently global. With per-dict names, it becomes per-dict too.
- **Constraint queue** — already effectively per-entry via `std::mem::take`/`replace` at `typecheck_dict.rs:588`. Make this explicit: the constraint queue becomes a parameter threaded through inference functions rather than a hidden `InferState` field. Removes the manual save/restore pattern.

**Scoping follows the dict model exactly.** An `[instance [Monad MyResult] ...]` declared inside a local dict is visible to that dict's entries and their descendants — not to parent scopes or siblings. Two nested scopes can declare different `Monad MyResult` instances; each `[do ...]` block gets the one in scope at the point of elaboration. This is correct lexical behavior, the same as any other binding.

The prelude declares the Result monad via the normal instance mechanism:

```tinct
[instance [Monad Result]
  [bind: [fn [m@[Result a] f@[Fn@[Result b] [a]]]
    [match m
      [Result.Ok v]:    [f v.value]
      [Result.Error _]: m]]]
  [pure: Result.Ok]]
```

A user-defined monad works the same way — declare an instance in scope, no naming convention required:

```tinct
[instance [Monad MyResult]
  [bind: ...]
  [pure: MyResult.Ok]]
```

**`do_infer_resolutions` is eliminated.** When `[do ...]` desugaring produces a complete expression at typecheck time — with `bind_node` and `pure_node` embedded inline as ordinary function calls — there are no sentinel variables (`ℊꜱʏᴍ⧼do-infer⧽N`) in the emitted AST and no runtime side-channel lookup needed. The evaluator sees plain `[bind m [fn [x] body]]` expressions; it does not consult `do_infer_resolutions` at all. This eliminates:

- `InferState.do_infer_resolutions: HashMap<String, String>`
- `EvalContext.do_infer_resolutions: RwLock<HashMap<String, String>>`
- The runtime consumer at `eval.rs:1767-1785` and `eval_core.rs:522-529`
- The `set_do_infer_resolutions` transfer call at lib.rs, main.rs, repl.rs

This is a larger removal than "Moderate" — update the impact rating accordingly and remove `set_do_infer_resolutions` from the transfer site list.

This change is in `typecheck_special.rs`, `type_env.rs`, and `stdlib/prelude.llt` — the prelude replaces the `result:` monad dict with a `[instance [Monad Result] ...]` declaration. Must land same sprint as the qualified-tag migration.

### T-920 — Superseded

T-920's `apply_builtin_constructor` step is deleted by this feature. `Kind::Arrow` remains in scope.

## Prerequisites

- `Kind::Arrow` from T-920 (for multi-arg kind registration)
- T-919 (delete dead `SurfaceExpression::TypeApp`)
- The parameterized type alias system (already complete)

## References

- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. Ch. 29. — Type constructors, kinding, curried application; `TyCon`/`App` representation.
- Cardelli, L. & Wegner, P. (1985). "On Understanding Types, Data Abstraction, and Polymorphism." *ACM Computing Surveys 17(4)*. — Type constructors as functions from types to types.
- Dolan, S. (2017). "Algebraic Subtyping." PhD thesis, University of Cambridge. — BAS covariance/contravariance distribution rules; polarity analysis (§4) for variance inference.
- Parreaux, L. & Chau, C.Y. (2022). "MLstruct: Principal Type Inference in a Boolean Algebra of Structural Types." *OOPSLA '22*. — BAS `App` subtyping rules; validation that split direction is unsound.
- Rémy, D. (1994). "Type Inference for Records." *TAOOP*, MIT Press. — Row polymorphism; basis for `RowTail` design.
- Tate, R. (2013). "The sequential semantics of producer effect systems." *POPL '13*. — Variance in the presence of effects; contravariance for consumers.
- Greenman, B. & Felleisen, M. (2018). "A Spectrum of Type Soundness and Performance." *OOPSLA '18*. — Phantom-type bivariance under gradual typing.
- Jones, M.P. (1993). "A System of Constructor Classes." *FPCA '93*. — Constructor classes (Functor, Monad) over arbitrary type constructors.
- Gaster, B.R. & Jones, M.P. (1996). "A Polymorphic Type System for Extensible Records and Variants." YALEU/DCS/RR-1104. — Label polymorphism; extends naturally to uniform-field rows.
- Castagna, G. & Peyrot, L. (2025). "Polymorphic Records for Dynamic Languages." *OOPSLA1*. — BAS with records; covariance rules for uniform rows.
