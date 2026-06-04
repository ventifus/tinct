# What If: Equirecursive Types for tinct

**State:** Proposal

What would it take to support properly recursive data types — linked lists, trees, and other self-referential structures — in tinct's type system?

## Current State

User-type-constructors gives recursive type aliases a correct foundation. Each alias is stored as a `TyConDef` in the scoped `TypeEnv`; self-references in the body are represented as `Type::App(TyCon("name"), args)` rather than structural expansions. Field access and pattern matching on recursive types return the correct named type at any depth:

```tinct
List:       [type [or Absent [record head: Int    tail: List]]]
JsonValue:  [type [or Int Float String Bool Absent [Seq JsonValue] [Map String JsonValue]]]
ServerConf: [type [host: String  fallbacks: [Seq ServerConf]]]

process: [fn [lst@List] lst.tail.tail.tail.head]  # lst.tail: List — correct at any depth ✓
```

Nominal ADTs with constructors (`[type [Cons val: Int tail: IntList] Nil]`) also work correctly at any depth — constructor references break expansion cycles at the nominal boundary.

### What Remains Unsolved

Two problems persist that TypeConstructor references alone cannot address.

**Inline recursive type annotations.** A recursive type used at an annotation site without a named alias has no way to express the self-reference. Users must always create a named alias first, even for single-use structural patterns:

```tinct
# Forced to name the type just to annotate one function parameter
TreeShape: [type [or Absent [record val: Int  left: TreeShape  right: TreeShape]]]
depth: [fn@Int [tree@TreeShape] ...]
```

**Structural subtype checking between distinct recursive TyCons.** Checking `A <: B` where both are structural recursive types with the same shape requires comparing expanded bodies. The expanded body of `A` contains `App(TyCon("A"), [])` and the expanded body of `B` contains `App(TyCon("B"), [])`. Without a coinductive visited-pairs algorithm, the type checker must unfold these again — and diverges. This matters when user code defines a type structurally equivalent to a library type and passes one where the other is expected.

### What's Missing

1. `CheckerType` — replaces the `Type` Rust enum; wraps either a `TypeNode` tinct value or a `TypeVar` inference variable; `TypeVar` is the sole Rust-internal type artifact
2. `TypeNode` nominal ADT with `Recursive { var: String  body: TypeNode }` and `RecursiveRef { name: String }` — equirecursive types' contribution to the primary type representation; body is a concrete TypeNode with `RecursiveRef(var)` at recursive positions, not a function
3. General `@[...]` annotation syntax — attachable to top-level bindings, type alias declarations, constructor names in `[type ...]`, record field type declarations, `[class ...]` / `[instance ...]` / `[macro ...]` / `[let ...]` positions; uniform `IndexMap<String, Value>` storage; `Value::Annotated` wrapper for non-function values; `TyConDef.annotation` for type-level positions; `annotation-of` Rust builtin working across all sites
4. `TypeNode.children` and `TypeNode.as-type` protocol functions on the TypeNode dict — read per-constructor protocol from `annotation-of`; drive `walk_type` and normalization without Rust dispatch tables
5. `eval_type_stage_expr` in the annotation resolver — evaluates type-stage annotations as ordinary tinct code; expansion-stack cycle detection for named aliases; `TypeNodeResolve.as-type` normalization
6. Coinductive subtype checking (S-Exp + S-Assum, sigma threaded through all BAS arms) — prevents divergence when checking structural equivalence between distinct recursive TypeNodes
7. `mu: [fn [let f] TypeNode.Recursive var: [gensym-with-scope "𝜇" "rec"]  body: f]` — inline recursive type constructor; self-generates its binder var via gensym
8. Contractiveness check in `mu` and `expand_named` — rejects non-contractive recursive types (`μa.a`, `μa.(a | Int)`) at construction time before they reach `is_subtype_inner`

## Why Equirecursive Types Matter for tinct

**Inline recursive type annotations.** Named recursive aliases work correctly post-user-type-constructors, but require a module-level name even for one-off annotation sites. The `mu` combinator lets recursive types appear inline — as function parameter annotations, `TypeAssert` expressions, or anywhere a `TypeNode` is expected — without polluting the namespace with a name used only once.

**Safe subtype checking between structurally equivalent recursive types.** BAS is structural: two types with the same shape should be subtypes of each other, even if they carry different names. Without a coinductive algorithm, checking `A <: B` between distinct recursive TypeConstructors diverges. The visited-pairs bisimulation ensures this check terminates and gives the correct answer.

**Transparent to users.** Equirecursive types require no explicit `fold`/`unfold` operations. A function that accepts a `List` just accepts a `List`; the type checker handles the recursion transparently.

**Consistency with BAS.** BAS is structural: `μa.T[a]` and `T[μa.T[a]]` are structurally equal — there is no "recursive wrapper" that needs a name. This is the approach taken by DOT (Scala's formal foundation), OCaml, and TypeScript.

**External structural data.** Data from `from-json` and `from-yaml` arrives as plain structural values and cannot be wrapped in nominal constructors after the fact. For types that must be expressed structurally (because they round-trip through JSON), inline `mu` annotations express the recursive shape without requiring a separately declared nominal type.

## Design

### General Annotation Syntax

`@[...]` annotations become attachable to every significant grammar position. No annotation fields are fixed or privileged — tinct core reads specific well-known fields (`doc:`, `return:`, `children:`, `as-type:`, `guarding:`, etc.) by name, but they are stored and retrieved identically to user-defined fields. Users can attach arbitrary metadata anywhere; tooling, macros, and user code can read it via `annotation-of`.

**Grammar positions where `@[...]` is now valid:**

```tinct
# Top-level bindings
my-fn@[doc: "..."  version: "1.0"  deprecated: false]: [fn ...]
Pi@[doc: "mathematical constant"]: 3.14159

# Type alias declarations
JsonValue@[doc: "..."  schema-id: "json-value"]: [type [or Int Float ...]]

# Constructor names in [type ...] declarations
TypeNode: [type
  [Union@[children: [fn [let u] u.types]  as-type: [fn [let u] u]  guarding: false]
    types: [Seq TypeNode]]
  ...]

# Record/dict field type declarations
Config: [type [record
  host@[required: true  doc: "hostname"]:         String
  port@[default: 8080   doc: "port number"]:       Int
  timeout@[default: 30  doc: "seconds"]:           Int]]

# [class ...] and [instance ...] declarations
[class@[doc: "structural children of a TypeNode"] [TypeNodeChildren t]
  [fn@[Seq TypeNode] [children [let _@t]]]]

[instance@[doc: "Union: children are its member types"] [TypeNodeChildren TypeNode.Union]
  [fn [let u] u.types]]

# [macro ...] declarations
[macro@[doc: "..."  inject: it: default-expr] my-macro [let pattern] body]

# [let ...] bindings inside expressions
[let x@[doc: "intermediate result"] [+ a b]]
```

**Storage model.** All annotations are stored as `IndexMap<String, Value>` — a uniform open dict. There are three storage sites:

- **`Value::Function` values**: `FnAnnotation.extra: IndexMap<String, Value>` alongside existing `doc`, `return_type`, and params fields. Functions currently carry `FnAnnotation`; all annotation fields — well-known and custom — now live there uniformly.
- **All other values** (`Value::String`, `Value::Int`, `Value::Dict`, etc.): A new `Value::Annotated { inner: Arc<Thunk>, annotation: Value /* dict */ }` wrapper carries the annotation. `annotation-of` dispatches on both `Value::Function` (reads `FnAnnotation`) and `Value::Annotated` (reads `.annotation`). Values without annotations return an empty dict.
- **Type-level positions** (type alias declarations, record field type annotations): The annotation is part of the type representation. `TyConDef` gains `annotation: IndexMap<String, Value>`. Record field type annotations are stored in `TypeNode.Record.field_annotations: Map String TypeNode` (each entry maps a field name to its annotation dict TypeNode, alongside the type in `fields`).

**`annotation-of` is a Rust builtin** that reads from all three storage sites uniformly, returning the annotation dict or an empty dict if no annotation is present. It is available at both runtime and in the type-stage evaluator.

**No fixed fields.** The distinction between "well-known" and "custom" is purely a matter of which code reads which keys — there is no architectural distinction. Adding `children:` to a TypeNode constructor declaration is identical to adding `my-org-field:` — both stored in the same dict, both readable via `annotation-of`.

### Representation: Rational Trees

A recursive type is represented as a **rational tree** — a finite graph with potentially cyclic edges. In tinct's `Type::*` representation:

```rust
// New variant — the μ-binder
Type::Recursive {
    var: String,         // "a" — the recursion variable
    body: Box<Type>,     // T[a] — the body, may reference RecVar(var)
}

// New variant — a reference to the enclosing μ-binder's variable
Type::RecVar(String)     // "a"
```

Example — a linked list of Int, using the post-user-type-constructors `Type` representation:

```rust
Type::Recursive {
    var: "lst",
    body: Box::new(Type::Union(vec![
        Type::App(Box::new(Type::TyCon("Absent".into())), vec![]),
        Type::Record(Row::Fields({
            "head": Type::Int,
            "tail": Type::RecVar("lst".into()),  // self-reference
        }))
    ]))
}
```

This is a **finite** representation of an **infinite** unrolling. The type checker unfolds `Type::Recursive` on demand during subtype checking, using a visited-pairs set to detect when unfolding has returned to a previously seen configuration.

### Type-Stage Evaluation

Type annotations that contain expressions — `@[or Int String]`, `@[mu [fn [let self] ...]]`, `@[my-combinator args]` — are evaluated by `eval_type_stage_expr` before the annotation resolver converts the result to a `Type`. This means type-stage code **actually executes**: users can define their own type-stage functions and the resolver will call them. There are no hardcoded special cases for `or`, `record`, `arrow`, or `mu` in the resolver — all are ordinary type-stage functions that return `TypeNode` values.

```text
annotation @[mu [fn [let self] [or Absent [record head: Int tail: self]]]]
           ↓
eval_type_stage_expr([mu [fn [let self] ...]], type_stage_env)
           ↓                               # mu calls f(RecursiveRef sentinel) EAGERLY
TypeNode.Recursive {
  var:  "𝜇ꜱʏᴍ⧼rec⧽0",
  body: TypeNode.Union [TypeNode.Absent,
          TypeNode.Record {head: TypeNode.Int,
                           tail: TypeNode.RecursiveRef "𝜇ꜱʏᴍ⧼rec⧽0"}]}
           ↓
CheckerType::Node(the above) — concrete TypeNode passed directly to type checker
```

`mu` evaluates its body function eagerly: the `Fn` is called with a `TypeNode.RecursiveRef` sentinel inside `mu` itself, producing a concrete TypeNode immediately. No tinct function is stored in `Recursive.body`. `eval_type_stage_expr` uses `materialize_sync`; no async boundary is crossed. Type-stage functions are terminating with benign counter side effects (gensym); the result is deterministic given the counter state.

### Annotation Syntax

`mu` is a type-stage function in the prelude — nothing more:

```tinct
--- stage: type
[
  # mu generates the binder var via gensym, calls the body function eagerly,
  # and stores the concrete TypeNode result — no deferred Fn stored.
  mu: [fn [let f]
    [let var  [gensym-with-scope "𝜇" "rec"]]       # → 𝜇ꜱʏᴍ⧼rec⧽N
    [let body [f TypeNode.RecursiveRef name: var]]        # eager: call f now, store TypeNode
    TypeNode.Recursive var: var  body: body]
]
```

When the annotation resolver encounters `TypeNode.Recursive body: f`, it generates a fresh sentinel, calls `f` with the sentinel, and wraps the result. Users can compose `mu` with their own type-stage functions freely:

```tinct
--- stage: type
[
  # User-defined type combinator — works identically to built-ins
  non-empty-list: [fn [let elem] [mu [fn [let self] [or elem [Seq self]]]]]
]
---
# Named alias (expansion-stack cycle detection, no explicit mu)
IntList: [type [or Absent [record head: Int  tail: IntList]]]

# Inline mu
JsonValue: [type [mu [fn [let self] [or Int String Bool Absent [Seq self] [Map String: self]]]]]

# User-defined combinator — resolved identically to built-ins
words: [fn [lst@[non-empty-list String]] ...]

# In function annotations
depth: [fn@Int [tree@[mu [fn [let self] [or Absent [record value: Int  left: self  right: self]]]]]]
  [if [absent? tree] 0 [+ 1 [max [depth tree.left] [depth tree.right]]]]
```

Named `self` (or any identifier) is used for the body parameter instead of `$_` — `$_` desugaring binds at the nearest enclosing argument position, not at the `mu` boundary, giving the wrong wrapping for nested calls.

For common patterns, named aliases are the ergonomic form:

```tinct
IntList:  [type [or Absent [record head: Int    tail: IntList]]]
StrList:  [type [or Absent [record head: String  tail: StrList]]]
JsonVal:  [type [or Int String Bool Absent [Seq JsonVal] [Map String: JsonVal]]]
BinTree:  [type [or Absent [record val: Int  left: BinTree  right: BinTree]]]

process: [fn@Absent [tree@BinTree] ...]
```

### `TypeNode`: The Primary Type Representation

`TypeNode` is not merely a type-stage value format — it is the type system's primary representation. The type checker works directly on `TypeNode` values; there is no separate `Type` Rust enum to convert into. The only inference artifact that does not correspond to a `TypeNode` constructor is `TypeVar` — a unification variable created by the Rust type checker during inference, with no type-stage meaning.

Equirecursive types contribute `Recursive` and `RecursiveRef`. `Recursive` carries a `var` field — a globally unique gensym name generated at construction time by the `mu` combinator (via `gensym`) or by the expansion-stack resolver. This var is the sigma key used by S-Assum during subtype checking.

Each constructor carries its protocol — the `children`, `as-type`, and `guarding` fields — directly in its `@[...]` annotation metadata, using the same annotation mechanism as `doc:`, `return:`, and `constraint:`. These are fields on the constructor's `FnAnnotation`, not fields on every instance value. `guarding: Bool` declares whether this constructor acts as a guard in contractiveness checking (Record, Arrow, TypeApplication are guarding; Union, Intersect are not).

```tinct
TypeNode: [type
  # Primitives — leaves: no children, identity normalization
  [Int@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [Float@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [String@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [Bool@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [Absent@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [Unknown@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  [Never@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]]
  # Structural
  [Record@[children: [fn [let r] [values r.fields]]
            as-type:  [fn [let r] r]
            guarding: true]             # structural constructor — guards recursion
    fields: [Map String TypeNode]  open: Bool]
  [Union@[children: [fn [let u] u.types]
          as-type:  [fn [let u] u]
          guarding: false]              # logical combinator — does not guard recursion
    types: [Seq TypeNode]]
  [Intersect@[children: [fn [let i] i.types]
               as-type:  [fn [let i] i]
               guarding: false]         # logical combinator — does not guard recursion
    types: [Seq TypeNode]]
  # TypeConstructor — two roles (see below)
  [TypeConstructor@[children: [fn [let _] [Seq]]  as-type: [fn [let t] t]  guarding: true]
    name: String]
  [TypeApplication@[children: [fn [let a] [cons a.ctor a.args]]
        as-type:  [fn [let a] a]
        guarding: true]                 # structural constructor — guards recursion
    ctor: TypeNode  args: [Seq TypeNode]]
  # Function
  [Arrow@[children: [fn [let a] [append a.params [Seq a.result]]]
           as-type:  [fn [let a] a]
           guarding: true]              # structural constructor — guards recursion
    params: [Seq TypeNode]  result: TypeNode]
  # Recursive — this whatif
  # body is concrete TypeNode with RecursiveRef(var) at recursive positions
  [Recursive@[children: [fn [let r] [Seq r.body]]
               as-type:  [fn [let r] r]
               guarding: true]          # inner Recursive guards the outer var
    var: String  body: TypeNode]
  # Internal sentinel — references the enclosing Recursive's var; always a leaf
  [RecursiveRef@[children: [fn [let _] [Seq]]
            as-type:  [fn [let r] r]
            guarding: false]            # not a guarding constructor
    name: String]]

# Protocol dispatch functions — read children/as-type from each constructor's annotation.
# annotation-of returns the FnAnnotation dict of the constructor function.
TypeNode: [merge TypeNode [
  children: [fn [let t]
    [let ann [annotation-of [get TypeNode [last [str-split "." [tag-of t]]]]]]
    [if [has? ann children] [[get ann children] t] [Seq]]]

  as-type: [fn [let t]
    [let ann [annotation-of [get TypeNode [last [str-split "." [tag-of t]]]]]]
    [if [has? ann as-type] [[get ann as-type] t] t]]]]
```

This requires two extensions, both specified in this proposal:

**1. `@[...]` is attachable to constructor names within `[type ...]` declarations.** This is a grammar extension — `@[...]` can already be attached to function definitions (`fn@[...]`), parameter names (`x@Type`), and expression positions (`[@T expr]`). Extending it to constructor names within `[type ...]` blocks follows the same principle: `@[...]` is open-ended metadata that any code can read. Tinct core reads specific well-known fields (`children:`, `as-type:`, `guarding:`); user code can attach and read arbitrary fields for its own purposes. Implementation: the parser recognizes `ConstructorName@[...]` inside a `[type ...]` frame; the desugar pass evaluates the annotation fields in the current eval context (the `--- stage: type` section has a full eval context) and stores them in the constructor function's `FnAnnotation`.

**2. `FnAnnotation` stores all annotation fields uniformly.** `FnAnnotation` holds ALL fields from `@[...]` annotations as a single `IndexMap<String, Value>`. Well-known fields (`doc:`, `return:`, `constraint:`, `kinds:`, `children:`, `as-type:`, `guarding:`) are identical in storage to user-defined custom fields — no privileged storage, no special cases. When `fn@[return: T  children: fn_val  my-custom-field: "hello"]` is evaluated, all three fields are stored uniformly.

**3. `annotation-of` is a Rust builtin reading `FnAnnotation`.** The `annotation-of` builtin reads the complete `FnAnnotation` dict from a `Value::Function` and returns it as a tinct dict. The current prelude implementation (`annotation-of: [fn [let val] [get "return-ann" [ast-of val]]]`) is a partial prototype — it reads only `return-ann` via the AST representation. This must be replaced with a proper Rust builtin that returns all annotation fields uniformly. `annotation-of` must be available in the type-stage evaluator (`eval_type_stage_expr` / `eval_type_stage_value` context), not just at runtime.

**Adding a new TypeNode constructor** requires: (1) declare the constructor in `TypeNode` with `@[children: fn  as-type: fn]`, (2) add explicit arms only to the four semantically-special walkers (`is_subtype_inner`, `unify`, `Substitution::apply`, `PartialEq`). Pure-traversal walkers via `walk_type` pick up the new constructor automatically through `TypeNode.children`.

**`TypeNode.TypeConstructor` has two roles** that must be distinguished:

- **Transient (pre-normalization)**: `TypeNode.TypeConstructor "Color"` — a bare type name in a type-stage expression, e.g. the result of looking up `Color` in the type-stage env. Always eliminated by normalization (expansion → body TypeNode).
- **Leaf identity (post-normalization)**: `TypeNode.TypeConstructor "Color.Red"` — a qualified constructor name (containing `.`). The nominal identity of a specific constructor. Remains after normalization; appears as leaves inside expanded union bodies. `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` is false because names differ.

`TypeNode.TypeApplication` is **always transient** — it exists during type-stage computation but is always eliminated by normalization before the type checker sees the result. After normalization, the type checker works only with: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified `TypeConstructor` leaves.

### Self-Hosted Type Traversal

The `TypeNode.children` and `TypeNode.as-type` protocol functions (declared in the TypeNode ADT above) serve double duty:

- **`TypeNode.children`** — returns the TypeNode children of a given value, used by `walk_type` for pure structural traversal.
- **`TypeNode.as-type`** — normalizes user-defined TypeNode constructors to an existing form before the type checker applies subtyping or unification rules. Built-in constructors return themselves (identity). For open ADTs (see R-12), a user-defined constructor annotates its own `as-type` that reduces it to existing forms — ensuring soundness by construction (user constructors can only express combinations of existing forms).

The Rust `walk_type` generic looks up `TypeNode.children` once at type-stage init time, then calls it per node:

```rust
fn walk_type<F: FnMut(&Value)>(node: &Value, env: &TypeStageEnv, f: &mut F) {
    f(node);
    let children = eval_type_stage_value(env.typenode_children_fn, [node.clone()]);
    for child in typenode_seq_iter(children) {
        walk_type(&child, env, f);
    }
}
```

Pure-traversal walkers enumerate no TypeNode variants themselves:

```rust
fn has_inference_vars(node: &Value, env: &TypeStageEnv) -> bool {
    let mut found = false;
    walk_type(node, env, &mut |t| {
        if is_typevar(t) { found = true; }
    });
    found
}
```

**Uniform representation.** `TypeNode.Recursive { var, body: TypeNode }` has `children` returning `[Seq body]` — the body IS a traversable TypeNode child. `walk_type` enters the body, finds TypeVars inside, enabling correct `collect_type_vars` and `Substitution::apply`. `TypeNode.RecursiveRef` leaves yield `[]` children — traversal stops cleanly without infinite loops.

All existing type-stage combinators (`or`, `record`, `arrow`, `seq`, `map`, etc.) return the corresponding `TypeNode` constructor. Migration from `kind:`-keyed dicts is atomic — partial migration leaves the type checker in an inconsistent state.

**Which walkers need explicit Rust arms:**

| Walker | Explicit arm needed? | Reason |
|--------|----------------------|--------|
| `collect_type_vars` | No | Pure traversal via `walk_type` + `TypeNode.children` |
| `has_inference_vars` | No | Pure traversal via `walk_type` + `TypeNode.children` |
| `check_kind_wellformed` | No | Pure traversal via `walk_type` + `TypeNode.children` |
| `is_subtype_inner` | Yes | S-Exp unfolding is semantically special |
| `unify` | Yes | Simultaneous opening is semantically special |
| `Substitution::apply` | Yes | RecursiveRef passes through; TypeVar is looked up |
| `PartialEq` | Yes | Alpha-equivalence for binder names |

### Extensibility

Users can freely compose existing `TypeNode` constructors into new type-stage functions — `non-empty-list`, weighted unions, constrained aliases — and the resolver handles them without changes.

For genuinely new `TypeNode` constructors (new type forms with new semantics): declare the constructor with `@[children: fn  as-type: fn]` in the TypeNode ADT, then add explicit arms only to the four semantically-special walkers above. The `as-type` annotation is the soundness gate — a user-defined constructor can only reduce to existing TypeNode forms, preventing `TypeNode.AlwaysSubtype` or other invariant-violating forms. Traversal (`walk_type`) picks up the new constructor automatically via `TypeNode.children` with no Rust changes.

### Annotation Resolver

The resolver produces **normalized TypeNode values** — no `TypeNode.TypeApplication` or bare `TypeNode.TypeConstructor` references remain after resolution. The type checker receives only concrete forms: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified TypeConstructor leaves.

**Path 1 — Named annotation** (`@Int`, `@ListA`, `@Color`, `@Maybe Int`):

**Data model: `TypeDecl`.** The current codebase has two separate stores in `TypeEnv`: `TypeAlias` (params + body `Type`) and `TyConDef` (variance + constructors + builtin_type). Both are registered for every `[type ...]` declaration — the split is an implementation accident. `TyConDef.constructors` is **currently dead storage** (never populated at any creation site); all constructor information lives in `TypeAlias.body` as `Type::Union([NominalVariant(...)])`. A unified `TypeDecl` view abstracts over both:

```rust
/// A unified view over a named type declaration — covers both structural aliases
/// and nominal ADTs. Computed by TypeEnv::lookup_type_decl; no new storage.
pub struct TypeDecl {
    pub params: Vec<String>,       // type parameter names (e.g., ["a", "k", "v"])
    pub body: CheckerType,         // parametric TypeNode body; params appear as
                                   // TypeNode.TypeConstructor(param_name) tokens to be substituted
    pub is_builtin_opaque: bool,   // true for Seq, Map, Handle etc. — no structural expansion
}
```

`params` stores declared parameter names as bare strings; in the stored `body`, parameters appear as `TypeNode.TypeConstructor(param_name)` (bare, unqualified). These are substituted by `expand_named` with concrete arg TypeNodes before expansion. They are NOT inference TypeVars and are never registered in `state.levels`. For zero-parameter aliases, `params` is empty and body is returned directly.

`is_builtin_opaque = true` for builtin types (`Seq`, `Map`, `Handle`, capability types). These retain their `App(TyCon("Seq"), args)` form after expansion — there is no structural body to inline. `is_subtype_inner`'s App arm handles them via UNIFY-TYCON (variance-directed comparison by TypeConstructor name).

For **nominal ADTs**, `body` is `TypeNode.Union [TypeNode.TypeConstructor "Color.Red"  TypeNode.TypeConstructor "Color.Green" ...]` — synthesized at declaration time by converting each `NominalVariant("Red", {})` to the corresponding qualified constructor leaf. This is the same body that always-expand produces when `@Color` is resolved: a union of qualified `TypeConstructor` leaves that preserve nominal identity through the names, not through opacity.

**`expand_named` algorithm:**

```text
expand_named(name, args, stack, env):
  # Step 1: Unified lookup — covers primitives, structural aliases, and nominal ADTs.
  # Primitive TypeNode constructors (Int, Float, Bool, etc.) ARE registered as TyConDef
  # entries with params=[], body=pre-interned TypeNode value, builtin_type=None.
  # No separate name-list fast path; all named types go through the same path.
  decl = TypeEnv::lookup_type_decl(name)
  if decl is None: error(UndefinedType(name))
  if args.len() != decl.params.len(): error(ArityMismatch(name, ...))

  # Step 3: Builtin-opaque types stay as App leaves — no structural expansion
  if decl.is_builtin_opaque:
    base = TypeNode.TypeConstructor name: name
    return apply_args(base, args)    # → TypeNode.TypeApplication { ctor: TypeConstructor(name), args: args }

  # Step 4: Expansion stack cycle detection (returns to parent on recursive self-reference)
  tycon_ptr = TypeEnv::tycon_def_ptr(name)   # Arc<TyConDef> pointer — stable within session
  if let Some(pre_name) = stack.get_pre_assigned_name(tycon_ptr):
    return TypeNode.RecursiveRef name: pre_name

  # Step 5: Pre-assign fresh binder name BEFORE expanding (needed by Step 4 for nested refs)
  fresh_var = gensym_fresh('𝜇', name)         # e.g., "𝜇ꜱʏᴍ⧼EvenList⧽42"
  stack.push(tycon_ptr, fresh_var)

  # Step 6: Substitute type args into the parametric body
  # param tokens (TypeNode.TypeConstructor(param_name)) → concrete arg TypeNodes
  param_subst = zip(decl.params, args)
  body_substituted = substitute_typenode(decl.body, param_subst)

  # Step 7: Recursively expand all TypeApplication/TypeConstructor references in the substituted body
  expanded = expand_all_tycon_apps(body_substituted, stack, env)

  stack.pop()

  # Step 8: Wrap in Recursive ONLY at cycle-origin level (only if fresh_var appears in result)
  if contains_recvar(expanded, fresh_var):
    return TypeNode.Recursive { var: fresh_var, body: expanded }
  return expanded


expand_all_tycon_apps(node, stack, env):
  match node:
    TypeNode.TypeApplication { ctor: TypeNode.TypeConstructor { name }, args }:
      # Expand each arg first, then expand the named type with expanded args
      expanded_args = args.map(a -> expand_all_tycon_apps(a, stack, env))
      expand_named(name, expanded_args, stack, env)

    TypeNode.TypeConstructor { name } where NOT name.contains('.'):
      # Bare (transient) TypeConstructor — expand with no args
      expand_named(name, [], stack, env)

    TypeNode.TypeConstructor { name } where name.contains('.'):
      # Qualified constructor leaf — preserve as-is (nominal identity)
      node

    _:
      # All other TypeNode constructors: recurse into children via TypeNode.children.
      # Reconstruction uses typenode_map_children — the TypeNode functor map
      # (the natural inverse of TypeNode.children; kept alongside the TypeNode declaration).
      # No exhaustive variant enumeration; new constructors are handled automatically.
      typenode_map_children(node, c -> expand_all_tycon_apps(c, stack, env))
```

**Substitution correctness.** `substitute_typenode` replaces bare `TypeNode.TypeConstructor(param_name)` tokens (where `param_name` is in `decl.params`) with the corresponding arg. It must NOT substitute `TypeNode.RecursiveRef` nodes (which have `𝜇ꜱʏᴍ⧼...⧽N` names in a completely distinct namespace from param names). Namespace separation — param names are short user identifiers (`"a"`, `"k"`); RecursiveRef names are gensym'd Unicode strings — guarantees no collision.

**Content hash and TypeVar-in-body.** Stored `TypeDecl.body` contains param-token `TypeNode.TypeConstructor(param_name)` nodes, not inference TypeVars. These are structural TypeNode nodes (`CheckerType::Node`), never `CheckerType::Var`. `TyConDef.content_hash` can therefore be computed on the stored body at any time via de Bruijn normalization (replace param tokens with their positional index, recurse into Recursive bodies replacing RecVar by depth). No inference TypeVars appear in stored bodies — they are created during type inference and live in `state.subst`, never in `TypeEnv`.

Non-self-referential aliases expand directly to their body. Self-referential aliases produce `TypeNode.Recursive`. Nominal ADT bodies expand to unions of **qualified constructor leaves** (`TypeNode.TypeConstructor "Color.Red"`), preserving nominal identity through constructor names rather than through keeping the TypeApplication opaque.

**Why this resolves structural subtype checking between distinct recursive TypeConstructors (M2)**: `TypeApplication(TypeConstructor("ListA")) <: TypeApplication(TypeConstructor("ListB"))` never reaches the type checker. Both annotations expand to `TypeNode.Recursive` values. The type checker compares two Recursive values via S-Exp + S-Assum — sigma key `(listA_var, listB_var)` prevents divergence and correctly returns true for structurally equivalent bodies.

**Why nominal ADTs remain distinct**: `@Color` expands to `TypeNode.Union [TypeNode.TypeConstructor "Color.Red", ...]`. `@Direction` expands to `TypeNode.Union [TypeNode.TypeConstructor "Direction.Red", ...]`. Structural subtype check: `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` → name inequality → false. Nominal identity is preserved through qualified constructor names in the expanded body, not through keeping `App(TyCon("Color"))` opaque.

**Path 2 — Expression annotation** (`@[or Int String]`, `@[mu [fn [let self] ...]]`, `@[user-fn args]`):

```text
resolve_annotation_expr(expr, type_stage_env, expansion_stack):
  typenode = eval_type_stage_expr(expr, type_stage_env)
  normalized = eval_type_stage_value(env.as_type_fn, [typenode])  # TypeNodeResolve dispatch
  expanded = expand_all_tycon_apps(normalized, expansion_stack)    # normalize TypeApplication references
  return expanded
```

`eval_type_stage_expr` evaluates synchronously via `materialize_sync`. User-defined TypeNode constructors go through `TypeNodeResolve.as-type`. Any `TypeNode.TypeApplication` references in the result are expanded by `expand_all_tycon_apps` using the same expansion stack. The type checker receives only normalized TypeNode values.

No name-based special-casing exists in any path. `or`, `record`, `mu`, and user-defined type-stage functions are handled identically.

### Contractiveness

A recursive type `μa.T` is **contractive** iff every path in `T` from the root to an occurrence of `RecursiveRef(a)` passes through at least one guarding constructor. Guarding constructors are: `TypeNode.Record`, `TypeNode.Arrow`, `TypeNode.TypeApplication(TypeConstructor, _)`. `TypeNode.Union` and `TypeNode.Intersect` are **not** guarding — they are logical combinators that do not structurally interpose between the binder and its reference.

Non-contractive types — where the body IS the RecursiveRef, or where the RecursiveRef is reachable without passing through a guard — diverge under S-Exp even with S-Assum, because after unfolding the non-Recursive side of the check prevents the sigma hypothesis from firing.

```text
is_contractive(node, var):
  # Case 1: direct self-reference — non-contractive
  if node is TypeNode.RecursiveRef { name } and name == var:
    return false

  # Case 2: guarding constructor — RecursiveRef under this node is safely guarded
  # Read from the constructor's @[guarding: Bool] annotation — no exhaustive match needed.
  ctor_ann = annotation_of(TypeNode_constructor_for(node))
  if ctor_ann.guarding:
    return true

  # Case 3: non-guarding (Union, Intersect, foreign RecursiveRef) — recurse into children
  return all(TypeNode.children(node), c → is_contractive(c, var))
```

Three cases — no exhaustive TypeNode variant enumeration. New constructors declare their guardedness in `@[guarding: Bool]` and are handled automatically. `TypeNode.Recursive` has `guarding: true` — an inner Recursive binder guards the outer var (any occurrence of the outer var under `μb.T` is separated from the outer binder by at least one type constructor).

This check runs in two places:

1. **In `mu`**: after `[let body [f TypeNode.RecursiveRef name: var]]`, before constructing `TypeNode.Recursive`. If `not(is_contractive(body, var))`, emit `TypeError(NonContractive)` with a diagnostic naming the var.
2. **In `expand_named`**: after step 7 (`expand_all_tycon_apps`), before wrapping in `TypeNode.Recursive`. If `not(is_contractive(expanded, fresh_var))`, emit `TypeError(NonContractive)`.

Examples:

- `[mu [fn [let self] self]]` → body is `RecursiveRef(var)` → `is_contractive = false` → error ✓
- `[mu [fn [let self] [or self Int]]]` → body is `Union([RecursiveRef, Int])` → `is_contractive(Union) = is_contractive(RecursiveRef) = false` → error ✓
- `[mu [fn [let self] [record head: Int  tail: self]]]` → body is `Record({..., tail: RecursiveRef})` → guarding → `is_contractive = true` → accepted ✓
- `[mu [fn [let self] [or Absent [record head: Int  tail: self]]]]` → `Union([Absent, Record(...)])` → `is_contractive(Absent) = true`, `is_contractive(Record) = true` → accepted ✓

### Worked Example: `JsonValue`

JSON data from `from-json` is structurally recursive: an array is a sequence of JSON values; an object is a map from strings to JSON values; the values themselves can be ints, strings, booleans, null, more arrays, or more objects. This cannot be expressed as a nominal ADT — `from-json` produces plain structural values that must be typed as they arrive, without imposing constructor wrappers the data doesn't have.

The named alias form is the natural expression:

```tinct
JsonValue: [type [or Int Float String Bool Absent [Seq JsonValue] [Map String JsonValue]]]
```

The annotation resolver detects the `JsonValue` self-reference via the expansion stack and wraps the type in `Type::Recursive` automatically — no explicit `mu` needed. For inline annotation positions, `mu` provides the same type without naming it:

```tinct
# Inline annotation using mu
transform: [fn [f@[fn [let x@JsonValue] JsonValue]]
            [raw@[mu [fn [let self] [or Int Float String Bool Absent [Seq self] [Map String self]]]]]]
  ...

# A recursive function that counts all numeric values in a JSON tree
count-numbers: [fn@Int [v@JsonValue]
  [match v
    Int:                  1
    Float:                1
    [Seq items]:          [sum [map count-numbers items]]
    [Map String val]:     [sum [map count-numbers [values val]]]
    _:                    0]]
```

**Why not a nominal ADT?** `from-json` returns plain structural values — ints, strings, sequences, dicts. There is no tinct constructor wrapping the data, and there should not be: nominal variants do not round-trip through JSON (`[from-json [to-json v]]` must recover the original structure, not wrap it in constructors). Equirecursive structural typing expresses the actual shape.

**Why equirecursive types and not the current workaround?** The current type checker loses the `JsonValue` type after ~4 levels of nesting — `v.0.0.0.key` types as `Unknown`. With `Type::Recursive`, the type checker unfolds on demand to any finite depth, always returning `JsonValue`. `count-numbers` is correctly typed regardless of how deeply nested the input is.

### Coinductive Subtype Checking

#### Globally Unique RecursiveRef Names

Every `Type::Recursive` node carries a globally unique `.var` name, regardless of how it was produced:

- **`mu` combinator**: calls `[gensym-with-scope "𝜇" "rec"]` in tinct — the prelude wrapper over `builtin-gensym`. Produces `"𝜇ꜱʏᴍ⧼rec⧽N"` via the single global counter shared across all gensym call sites.
- **Named-alias expansion stack**: calls `gensym_fresh('𝜇', alias_name)` in Rust — `builtins_meta::gensym_fresh`, same global counter. Produces `"𝜇ꜱʏᴍ⧼EvenList⧽N"`. The alias name is embedded as the tag; error messages display `μEvenList.T`.

This eliminates variable capture in `unfold_once` (no two binders share a name) and eliminates sigma false positives from shadowed alias names.

#### S-Exp + S-Assum (Chau & Parreaux 2026)

The bisimulation uses the **S-Exp + S-Assum** framework, which is proven sound for BAS with equirecursive types. Unlike the naive "distribute before unfolding" approach, the sigma context Σ is threaded through **all** subtyping rules — so the coinductive hypothesis is available inside union, intersection, record, and App sub-checks.

Two rules govern recursive types within the standard `is_subtype_inner`:

- **S-Assum**: at the start of every call, if `(a.var, b.var)` ∈ Σ, return `true` immediately (coinductive hypothesis). Add the pair to Σ before proceeding.
- **S-Exp**: if `a` is `Type::Recursive`, unfold it once and continue — Σ already contains the original pair.

```rust
fn is_subtype_inner(a: &Type, b: &Type,
                    sigma: &mut HashSet<(String, String)>, ...) -> bool {
    // S-Assum: check hypothesis before anything else
    if let (Type::Recursive { var: v1, .. },
            Type::Recursive { var: v2, .. }) = (a, b) {
        let key = (v1.clone(), v2.clone());
        if sigma.contains(&key) { return true; }
        sigma.insert(key);
    }

    match (a, b) {
        // S-Exp: unfold; sigma already contains (v1, v2)
        (Type::Recursive { var: _, body }, _) =>
            is_subtype_inner(unfold_once(a), b, sigma, ...),

        (_, Type::Recursive { .. }) =>
            is_subtype_inner(a, unfold_once(b), sigma, ...),

        // BAS rules — sigma threaded into every recursive call
        (_, Type::Union(types)) =>
            types.iter().any(|t| is_subtype_inner(a, t, sigma, ...)),
        (Type::Union(types), _) =>
            types.iter().all(|t| is_subtype_inner(t, b, sigma, ...)),
        // Record, App, Arrow, ... — same pattern
        ...
    }
}
```

**Sigma key**: `(a.var, b.var)` — a `(String, String)` pair of binder names. This is O(1) (just read the `.var` field), works across threads (strings are `Send + Sync`), and matches the theoretical model directly (Amadio & Cardelli, Chau & Parreaux). Because all `.var` names are globally unique, there are no false positives from shadowed aliases or mu-counter collisions.

**`unfold_once`**: replace `Type::Recursive { var, body }` with `body[RecursiveRef(var) ↦ self]` — substituting all `RecursiveRef` occurrences with the full recursive type. After substitution, the `Recursive` node at each recursive position carries the same `.var` name as the original binder. When `is_subtype_inner` encounters those positions, S-Assum fires immediately — the hypothesis `(v1, v2)` is already in Σ.

**Why S-Exp + S-Assum is necessary for BAS**: the naive "distribute over union first" approach fails because the hypothesis established for `(μa.T[a], A ∨ B)` is keyed on that exact pair. After distribution, sub-checks for `(μa.T[a], A)` and `(μa.T[a], B)` have different keys — the hypothesis is unavailable. S-Assum fires at the start of every call and is available inside all BAS decomposition rules, preventing this failure.

### Unification

Both unification cases use **simultaneous opening**: replace the `RecursiveRef` binder with a shared fresh `TypeVar`, then unify the opened bodies. This terminates because the fresh `TypeVar` is a unification variable — not a `Type::Recursive` — so neither arm fires again on the opened bodies.

```rust
match (a, b) {
    (Type::Recursive { var: v1, body: b1 },
     Type::Recursive { var: v2, body: b2 }) => {
        // Open both binders with one shared fresh TypeVar.
        // The fresh var is a unification variable — not Recursive —
        // so this arm cannot fire again on the opened bodies.
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        let b_open = substitute(b2, v2, &fresh);
        unify(a_open, b_open, subst, state)
    }
    (Type::Recursive { var: v1, body: b1 }, other) => {
        // Treat `other` as μ_fresh.other (trivial recursive type,
        // binder does not appear in body). Open the real binder
        // with a fresh TypeVar and unify the opened body with other.
        // The fresh var replaces RecVar(v1) in b1; other contains
        // no RecVar — so Recursive arms cannot fire again.
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        unify(a_open, other, subst, state)
    }
    ...
}
```

The symmetric case terminates: both opened bodies contain only `fresh` (a `TypeVar`) at recursive positions — standard Robinson unification applies. The asymmetric case terminates for the same reason: `a_open` replaces `RecursiveRef(v1)` with `fresh`, a `TypeVar`; `other` contains no `RecursiveRef`; the `Recursive` arm cannot re-fire.

`unfold_once` — which replaces `RecursiveRef` with the full `Recursive` type, making the tree **larger** — is used only in subtype checking (where the visited-pairs set prevents divergence), not in unification.

### Mutual Recursion

Mutually recursive type aliases — where `A` references `B` and `B` references `A` — require no explicit `mu` from the user. The annotation resolver's expansion stack detects the cycle automatically. Users write plain type aliases:

```tinct
EvenList: [type [or Absent [record head: Int  tail: OddList]]]
OddList:  [type [or Absent [record head: Int  tail: EvenList]]]
```

Each entry pushed to the stack is pre-assigned a fresh name via `fresh_rec_var_with_source` at push time. Expansion of `EvenList` proceeds as follows (using `"𝜇ꜱʏᴍ⧼EvenList⧽42"` and `"𝜇ꜱʏᴍ⧼OddList⧽43"` as generated internal names, with source names `"EvenList"` and `"OddList"` stored for diagnostics):

1. Push `(ptr_EvenList, "𝜇ꜱʏᴍ⧼EvenList⧽42")` to stack; begin expanding EvenList's body
2. The body references `OddList` — push `(ptr_OddList, "𝜇ꜱʏᴍ⧼OddList⧽43")`; begin expanding OddList's body
3. OddList's body references `EvenList` — `ptr_EvenList` is already in stack → emit `Type::RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")`
4. Pop `OddList`: expanded body = `Absent | {head: Int, tail: RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")}`. Does the body contain `RecursiveRef("𝜇ꜱʏᴍ⧼OddList⧽43")`? **No** — OddList is not the cycle origin. Return the body **as-is**, without wrapping.
5. Pop `EvenList`: expanded body = `Absent | {head: Int, tail: (Absent | {head: Int, tail: RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")})}`. Does the body contain `RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")`? **Yes** — EvenList is the cycle origin. Wrap: `Type::Recursive { var: "𝜇ꜱʏᴍ⧼EvenList⧽42", body: <full body> }`.

The wrapping rule: **wrap `Recursive` only when popping the stack entry whose fresh name appears in the expanded body.** Intermediate entries whose fresh names do not appear pass through their bodies unchanged.

The result is the correct single-μ encoding (displayed with source names):

```text
μEvenList. (Absent | {head: Int, tail: (Absent | {head: Int, tail: μEvenList})})
```

OddList's structure is inlined into EvenList's body — not wrapped in its own binder. Expanding `OddList` separately produces the symmetric result with `"𝜇ꜱʏᴍ⧼OddList⧽43"` (displayed as `μOddList`) as the binder, which is structurally equivalent (verified by S-Assum) but not alpha-equivalent — the two expansions have different binder names.

Subtype checking `EvenList <: OddList` uses S-Assum: at the start of the check, insert `("𝜇ꜱʏᴍ⧼EvenList⧽42", "𝜇ꜱʏᴍ⧼OddList⧽43")` into sigma. When the bodies are compared and the recursive positions are reached, sigma fires — correctly terminating the check as true.

**Restriction on non-symmetric mutual recursion.** The single-μ encoding is correct when the mutual recursion is symmetric — every cycle goes back to the origin. For non-symmetric mutual recursion (e.g., `A` references both `B` and `C`; `B` references `A`; `C` references `A` but not `B`) the encoding anchors at the first cycle-origin encountered, inlining all intermediate types. This is always sound: the inlined form is semantically equivalent to the simultaneous-μ encoding for any acyclic unwinding depth. Users should prefer named aliases over inline `mu` for complex mutual recursion; inline `mu` covers the common single-binder case.

Explicit `mu` in annotation positions uses `[fn [let self] ...]` with `self` as the self-reference. The depth limit remains as a last-resort safety net; the expansion stack detects genuine cycles before the limit fires.

## What Would Change

### `src/value.rs` — `Value::Annotated` and `FnAnnotation` extension

**Current:** `Value::Function` carries `FnAnnotation { doc, return_type, params }`. Non-function values have no annotation storage. `@[...]` annotations on non-function positions are parsed but the non-standard fields are discarded after type-checking.

**Proposed:**

1. `FnAnnotation` gains `extra: IndexMap<String, Value>` — all annotation fields are stored uniformly. Standard fields (`doc:`, `return:`, etc.) may continue to have dedicated typed fields for performance but are ALSO stored in `extra` for uniformity. `annotation-of` reads `extra` as the canonical annotation dict.

2. New `Value::Annotated { inner: Arc<Thunk>, annotation: Value }` — wraps any non-function value with an annotation dict. `annotation-of` dispatches on both variants. All other Value operations unwrap `Annotated` transparently (pattern matching, equality, display).

3. `TyConDef` gains `annotation: IndexMap<String, Value>` — for type alias and type constructor declaration annotations. `annotation-of` on a TyConDef reference returns this dict.

4. `TypeNode.Record` gains `field_annotations: Map String TypeNode` — each key maps a field name to its annotation dict expressed as a TypeNode dict. Used by record field type declarations (`host@[required: true]: String`).

**Impact:** Moderate — new Value variant (transparent in most match arms); FnAnnotation extension; TyConDef annotation field; parser/desugar changes at each annotatable grammar position.

### `src/type_def.rs` — `CheckerType` replaces `Type`

**Current:** The type checker operates on a `Type` Rust enum. `TypeNode` (tinct Value) is a separate format converted from `Type` during annotation resolution.

**Proposed:** Introduce `CheckerType` — the type checker's working type is either a TypeNode value or a TypeVar inference variable:

```rust
enum CheckerType {
    Node(Value),      // a TypeNode tinct value (Union, Record, Recursive, RecursiveRef, TypeApplication, …)
    Var(String, u32), // TypeVar — sole inference artifact; name + Kiselyov level
}
```

The `Type` Rust enum is eliminated. `Substitution` maps `String → CheckerType`. All type checker operations (`is_subtype_inner`, `unify`, `collect_type_vars`, etc.) take `CheckerType` arguments. For `CheckerType::Node(v)`, pattern match on the TypeNode variant tag. For `CheckerType::Var`, apply existing TypeVar logic unchanged.

`TypeNode.Recursive { var, body }` and `TypeNode.RecursiveRef { name }` are `CheckerType::Node` values. `var` carries a gensym name generated at `mu` call time (in tinct) or at expansion-stack wrap time (in Rust). Pure-traversal walkers use `walk_type` dispatching through `TypeNodeChildren` — no exhaustive match. Only `is_subtype_inner`, `unify`, `Substitution::apply`, and `PartialEq` have explicit arms.

`TypeVar` is the sole type not representable as a TypeNode constructor. All other type forms that previously lived in the `Type` enum — `TypeConstructor`, `TypeApplication`, `Union`, `Record`, `Recursive`, `RecursiveRef`, etc. — are now TypeNode values.

**Impact:** Major — replaces `Type` enum with `CheckerType`. The actual migration scope is substantially larger than the ~40 match sites in `type_def.rs` and `type_unify.rs`. Full scope includes: `TypeScheme` (holds `Type` in its body field), `TypeEnv.bindings` (maps names to `TypeScheme`), `InferState.subst` (`Substitution` maps `String → Type`), builtin type signature registration in `builtins_core.rs` (~50 sites), `value_matches_type` in `eval_materialize.rs`, the LSP `TypeMap = HashMap<(usize,usize), Type>` public API, and all type normalisation functions in `type_normalize.rs`. Estimate: 150–200 affected sites across 10+ files. Migration must be incremental: introduce `CheckerType` alongside `Type`, migrate one subsystem at a time (annotation resolver → unifier → inference engine → builtins → LSP), retire `Type` last. Pure walkers simplified via `walk_type` once migration is complete.

### `src/type_env.rs` — Merged `TyConDef` (eliminates `TypeAlias`)

**Current:** Two separate stores in `TypeEnv`: `type_aliases: HashMap<String, TypeAlias>` (params + body `Type`) and `tycon_defs: HashMap<String, Arc<TyConDef>>` (variance + constructors + builtin_type). Both are registered for every `[type ...]` declaration. `TyConDef.constructors` is dead storage — never populated at any creation site.

**Proposed:** Merge into a single `TyConDef`. `TypeAlias` struct and `type_aliases` map are eliminated.

```rust
pub struct TyConDef {
    /// Declared type parameter names (e.g., ["a", "k", "v"]).
    /// In `body`, parameters appear as TypeNode.TypeConstructor(param_name) tokens — NOT TypeVars.
    /// These are structural placeholders eliminated by expand_named at use sites.
    pub params: Vec<String>,

    /// Parametric TypeNode body. Parameter names appear as TypeNode.TypeConstructor(param_name)
    /// tokens (unqualified, bare). Qualified TypeConstructor leaves (containing '.') are
    /// constructor identity markers — never substituted.
    ///
    /// INVARIANT: body contains no CheckerType::Var (inference) variables.
    /// Param tokens are TypeNode.TypeConstructor nodes (structural), not inference variables.
    pub body: Value,   // TypeNode tinct Value

    /// Constructors for nominal ADTs: (qualified_tag, payload_arity).
    /// e.g., [("Color.Red", 0), ("Color.Green", 0)].
    /// Empty for structural aliases. Used by inject_adt_constructor_schemes and
    /// exhaustiveness checking — NOT used by expand_named.
    pub constructors: Vec<(String, usize)>,

    /// Variance per type parameter. Inferred by polarity analysis (Dolan 2017 §4).
    pub variance: Vec<Variance>,

    /// Builtin-type discriminant (e.g., "Seq", "Map"). When Some, expand_named
    /// returns TypeNode.TypeApplication(TypeConstructor(name), expanded_args) without structural expansion.
    pub builtin_type: Option<String>,

    /// Content hash: de Bruijn normalization of body + BLAKE3.
    /// Replace each TypeNode.TypeConstructor(param_name) with DeBruijn(param_index),
    /// each TypeNode.RecursiveRef with DeBruijn(depth). Alpha-invariant across param renames.
    pub content_hash: std::sync::OnceLock<[u8; 32]>,  // OnceLock (not OnceCell) — TyConDef is Arc-shared across threads
}
```

For **nominal ADTs** (`Color: [type [Red] [Green] [Blue]]`), body is:

```text
TypeNode.Union types: [TypeNode.TypeConstructor "Color.Red"  TypeNode.TypeConstructor "Color.Green"  TypeNode.TypeConstructor "Color.Blue"]
```

Synthesized at declaration time from constructor declarations. Qualified names carry nominal identity; `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` is false by name inequality.

For **structural aliases** (`Maybe: [type [let a] [or Absent a]]`), body is:

```text
TypeNode.Union types: [TypeNode.TypeConstructor "Absent"  TypeNode.TypeConstructor "a"]
```

where `TypeNode.TypeConstructor "a"` is a param token (unqualified, no '.') substituted by `expand_named`.

**Impact:** Major — eliminates `TypeAlias` struct and `type_aliases` map; ~25 call sites across 5 files migrated (typecheck_annot.rs × 5, builtins_core.rs × 7, typecheck.rs × 2, typecheck_dict.rs × 2, type_env.rs definition). Atomic migration required.

### `src/typecheck_annot.rs` — Alias expansion and resolver

**Current:** Two separate expansion paths: structural aliases via `instantiate_type_alias` + `expand_alias_body_guarded`; TypeConstructor references stay as `TypeApplication(TypeConstructor)`. Expression annotations are pattern-matched by name — hardcoded special cases.

**Proposed:** Four functions producing fully normalized TypeNode values:

1. **`expand_named(name, args, stack) -> CheckerType`**: Unified lookup via `env.lookup_tycon_def(name)`. All named types — primitives, structural aliases, nominal ADTs — are registered as TyConDef entries; there is no separate name-list fast path. Primitives (`Int`, `Float`, `Bool`, etc.) have `params=[]`, `body=pre-interned TypeNode.Int` etc., `builtin_type=None`. Uses the ordered `IndexSet<(Arc<TyConDef> ptr, String)>` expansion stack for cycle detection; wraps in `TypeNode.Recursive` only at the cycle-origin level. See §Annotation Resolver for the complete algorithm.

2. **`expand_all_tycon_apps(node, stack) -> CheckerType`**: Recursively eliminates transient `TypeNode.TypeApplication`/bare `TypeNode.TypeConstructor` by calling `expand_named`. Uses Rust-level TypeNode tag matching — NOT `eval_type_stage_value` (hot path; per-node tinct evaluation overhead is unacceptable here).

3. **`eval_type_stage_expr(expr, env) -> CheckerType`**: Evaluates expression annotations via `materialize_sync`. Result goes through `TypeNode.as-type` normalization + `expand_all_tycon_apps`.

4. **Remove `resolve_typenode`**: Both paths return `CheckerType::Node(normalized)` directly.

All `TypeNode.Recursive` `.var` names are globally unique (`𝜇ꜱʏᴍ⧼...⧽N`), enabling collision-free sigma keys.

**Impact:** Moderate — four new functions replace `instantiate_type_alias`, `expand_alias_body_guarded`, and all name-based expression annotation dispatch.

### `src/type_unify.rs` — `is_subtype` and `unify`

**Current:** Handles `TypeConstructor`/`TypeApplication` via `UNIFY-TYCON` (name equality). No handling of `TypeNode.Recursive` or `TypeNode.RecursiveRef`. After normalization, `TypeApplication(TypeConstructor)` never reaches `is_subtype_inner` — the UNIFY-TYCON arm becomes unreachable and is removed. `TypeNode.TypeConstructor "Color.Red"` (qualified constructor leaf) uses name equality, which is correct for constructor identity.
**Proposed:** Extend `is_subtype_inner` with S-Exp + S-Assum: add `sigma: &mut HashSet<(String, String)>` threaded through all arms. At the top of each call, when both sides are `CheckerType::Node(v)` where `v` is `TypeNode.Recursive`, check and update sigma. Add S-Exp arm that calls `unfold_once(a)` — pure TypeNode structural substitution replacing every `TypeNode.RecursiveRef name: var` in `body` with the full `Recursive` node — and re-enters. No tinct evaluation needed. All BAS arms pass sigma through.

Add unification arms using simultaneous opening (§Unification). `TypeNode.RecursiveRef` never reaches the unifier directly — only appears inside a Recursive body during S-Exp unfolding, resolved by the sigma context.

`Substitution::apply` for `CheckerType::Node(v)`: recurse into TypeNode children using **Rust-level TypeNode tag matching** (NOT `eval_type_stage_value` — `apply` is called in the unification hot path where per-node tinct evaluation overhead is unacceptable). `TypeNode.RecursiveRef` nodes pass through unchanged (their `𝜇ꜱʏᴍ` gensym names are never in `subst.type_map`). For `CheckerType::Var(name, level)`: apply existing TypeVar substitution logic.

`TypeNode.Recursive` and `TypeNode.RecursiveRef` appear in type error messages using the alias name embedded in the gensym tag (e.g., `μEvenList.T`). Never written in source.

**Impact:** Moderate — sigma threading through ~20 call sites; `CheckerType::Node` arms dispatch on TypeNode variant tags instead of `Type::*` Rust variants.

### `stdlib/prelude.llt` — `TypeNode` ADT and `mu` combinator

**Current:** Type-stage functions return plain dicts with `kind:` string discriminators. The annotation resolver pattern-matches these by name — a hidden list of hardcoded special cases. Users cannot add new type-stage combinators that the resolver will honour.

**Proposed:**

1. Declare the `TypeNode` nominal ADT in the `--- stage: type` section of the prelude (full declaration in §TypeNode), with `@[children: fn  as-type: fn]` annotations on every constructor. Requires two extensions: `@[...]` annotations on constructor names within `[type ...]` blocks (stored in each constructor's `FnAnnotation`), and `annotation-of` available in the type-stage evaluator. All existing type-stage combinators (`or`, `record`, `arrow`, `seq`, `map`, etc.) return the corresponding `TypeNode` constructor. **Atomic** — partial migration breaks the type checker.

2. Declare `TypeNode.children`, `TypeNode.as-type`, and `TypeNode.map-children` on the TypeNode dict (via `[merge TypeNode [...]]` as shown in §TypeNode). `TypeNode.children` and `TypeNode.as-type` read `annotation-of` on each constructor. `TypeNode.map-children` is the TypeNode functor map — applies a function to each child and reconstructs the node; it is the natural inverse of `children` and is used by `expand_all_tycon_apps` for structural-recursion without an exhaustive Rust match. Like `children`, `map-children` dispatches through `annotation-of` per constructor.

3. Add `mu`:

   ```tinct
   mu: [fn [let f]
     [let var  [gensym-with-scope "𝜇" "rec"]]
     [let body [f TypeNode.RecursiveRef name: var]]
     TypeNode.Recursive var: var  body: body]
   ```

4. The annotation resolver calls `eval_type_stage_expr` + `TypeNode.as-type` normalization. `resolve_typenode` is eliminated — the resolver returns `CheckerType::Node(typenode)` directly.

**Impact:** Major — atomic migration of all type-stage combinators; resolver loses ~10 name-based special cases; users gain full extensibility of the type-stage language.

### `src/type_def.rs` — TyConDef content hash

`TyConDef` gains a lazily-computed content hash field:

```rust
pub struct TyConDef {
    pub variance: Vec<Variance>,
    pub constructors: Vec<(String, usize)>,
    pub builtin_type: Option<String>,
    pub content_hash: OnceCell<[u8; 32]>,   // BLAKE3, computed on first use
}
```

The hash is computed by **de Bruijn normalization** followed by BLAKE3:

1. **Normalize**: traverse the body type, replacing each `RecVar(name)` with `DeBruijn(depth)` where `depth` counts the number of `Recursive` binders between the reference and its binding site (0 = innermost). This makes the hash alpha-invariant — `μa.{head: Int, tail: a}` and `μb.{head: Int, tail: b}` produce the same hash.

2. **Serialize**: emit a canonical byte sequence for the normalized type — one tag byte per variant, followed by field content. Union members and record fields are sorted before serialization to ensure order-independence.

3. **Hash**: BLAKE3 over the serialized bytes → `[u8; 32]`.

This gives content-addressable type identity: two TyConDefs with structurally identical bodies produce the same hash regardless of which machine, process, or thread computed them, what binder names were chosen, or what order fields were declared in. The hash is computed once via `OnceCell` and reused for all subsequent comparisons.

**Why this matters for parallelism and distribution**: the sigma context (binder-name pairs) is local to a single `is_subtype` call and is not shared. But TyConDef instances ARE shared — between threads within a process and potentially between workers in distributed evaluation. The content hash is the identifier that makes this sharing correct:

- **Multithreading**: `Arc<TyConDef>` in the scoped TypeEnv enables sharing across threads. The hash provides a stable, lock-free identity for deduplication.
- **Distributed evaluation** (dist-eval): workers can exchange type definitions over the network keyed by content hash. A worker receiving `TyConDef` from a peer recomputes the hash locally to verify identity — no trust in the name string or pointer.
- **Content-addressed type cache**: type-checking results (e.g., "these two TyConDefs are subtypes") can be cached keyed by `(hash_a, hash_b)` and reused across calls, sessions, or machines — the same infrastructure as tinct's `blake3`-keyed include cache.

The sigma key (`String` binder names) handles the ephemeral, in-call bisimulation. The content hash handles the durable, distributable type identity. Neither subsumes the other.

**Impact:** Minor — new field on TyConDef; one new traversal function (`debruijn_normalize`); one new method (`content_hash()`).

### Type checker performance

**Current:** Subtype checking terminates quickly (no coinductive loop).
**Proposed:** Sigma set grows by one entry per `(Recursive, Recursive)` pair encountered. For typical config schemas (finite mutual recursion depth), the set stays small. Sigma is allocated per top-level `is_subtype` call and dropped on return. The `content_hash` field on TyConDef amortizes cross-call deduplication — a `(hash_a, hash_b)` cache can avoid re-running the coinductive check for previously-seen pairs.

**Primitive TypeNode interning.** `CheckerType::Node(TypeNode.Int)` is a heap-allocated tinct Value — a regression from `Type::Int` which was a zero-allocation Rust enum variant. The 7 payload-free primitive constructors (`Int`, `Float`, `String`, `Bool`, `Absent`, `Unknown`, `Never`) must be pre-interned in `TypeStageEnv` as shared `Rc<ThunkState>` values. `TypeStageEnv::primitive_node(name)` returns `Rc::clone(&env.int_node)` etc. — an atomic reference-count bump with no heap allocation. All call sites that produce primitive TypeNodes use this path rather than constructing new Values.

**Impact:** Moderate — new per-call sigma allocation (negligible for non-recursive programs); primitive TypeNode interning eliminates allocation regression for the common case.

## Downstream: validate-tinct-rewrite

Once equirecursive types land, `validate_value` in `src/builtins_meta.rs` (~267 lines) can be rewritten as a tinct stdlib function. `regex-match?` is already available; the only missing piece is a recursive type alias to type the schema dict.

- Define the schema dict type in `stdlib/prelude.llt` using a `mu`-type alias covering all schema keys: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum`
- Rewrite `validate` as a tinct function: call `regex-match?` for `pattern`, recurse on `fields:` and `items:` entries, collect violations into a Seq; remove `validate_value` from `src/builtins_meta.rs`
- Keep `validate` registered as a thin Rust stub that calls the tinct function and maps errors to `SchemaViolation` error kind
- Tests: all existing `validate` corpus tests pass after rewrite; validate over 1000-entry dict completes in <100ms

## Prerequisites

- **user-type-constructors** — already accepted and in implementation (S-842–S-851). `Type::TyCon`, `Type::App`, `RowTail::Uniform`, and the scoped `TyConDef` registry are the baseline this feature builds on. Equirecursive types extend the type system with `Type::Recursive` and `Type::RecVar` for the two cases TypeConstructor references alone cannot handle: inline recursive annotations and safe subtype checking between distinct recursive TypeConstructors.
- `type-ann-v2-infra` sprint — three deliverables required before equirecursive types:
  1. `eval_type_stage_expr` and `eval_type_stage_value` sync evaluation functions in `src/typecheck_annot.rs`
  2. `TypeNode` ADT declared in the `--- stage: type` section of the prelude
  3. Atomic migration of all existing type-stage combinators (`or`, `record`, `arrow`, `seq`, `map`, etc.) from `kind:`-keyed dicts to `TypeNode` constructors, with the resolver switched from name-based dispatch to `resolve_typenode`

  Equirecursive types then adds `TypeNode.Recursive`/`TypeNode.RecursiveRef` to the already-working TypeNode machinery and `mu` to the prelude.

## References

- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM Transactions on Programming Languages and Systems*, 15(4), 575–631. — [foundational coinductive subtype algorithm; proven sound for function and base types; S-Assum/S-Hyp rules generalized by Chau & Parreaux for BAS]
- Chau, T. & Parreaux, L. (2026). "Boolean-Algebraic Subtyping with Equirecursive Types." §3.3.1. — [S-Exp + S-Assum framework proven sound for BAS union/intersection/negation; Σ context threading through all derivation rules; adopted by this design]
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. §21 "Recursive Types." — [equirecursive vs isorecursive comparison; rational tree representation; unfolding semantics; simultaneous-opening for recursive type unification (§21.8)]
- Ancona, D. & Zucca, E. (2002). "A Theory of Mixin Modules." *ACM TOPLAS*, 24(5), 578–637. — [equirecursive types in structural object systems, closely related to BAS]
- Huet, G. (1976). "Résolution d'Équations dans des Langages d'Ordre 1, 2, ..., ω." Ph.D. thesis. Université Paris VII. — [rational tree unification; de Bruijn normalization for content-addressable type identity]
