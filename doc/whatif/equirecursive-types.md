# What If: Equirecursive Types for tinct

**State:** Accepted — 2026-06-05

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

1. `Type::Recursive { var: String, body: Box<Type> }` and `Type::RecursiveRef(String)` — two new variants added to the existing `Type` Rust enum. `CheckerType` is the permanent boundary between the type checker and the type-stage evaluator; `from_type` converts `Type` → TypeNode Value for type-stage eval; `typenode_value_to_type` converts back. The `Type` enum is NOT replaced — see §Architectural Note in §What Would Change.
2. `TypeNode` nominal ADT with `Recursive { var: String  body: TypeNode }` and `RecursiveRef { name: String }` — equirecursive types' contribution to the primary type representation; body is a concrete TypeNode with `RecursiveRef(var)` at recursive positions, not a function
3. General `@[...]` annotation syntax — attachable to top-level bindings, type alias declarations, constructor names in `[type ...]`, record field type declarations, `[class ...]` / `[instance ...]` / `[macro ...]` / `[let ...]` positions; uniform `IndexMap<String, Value>` storage; `Value::Annotated` wrapper for non-function values; `TyConDef.annotation` for type-level positions; `annotation-of` Rust builtin working across all sites
4. `TypeNode.children`, `TypeNode.map-children`, and `TypeNode.as-type` protocol functions on the TypeNode dict — derived generically from `@Child` field annotations and constructor-level `as-type:`/`guarding:` annotations; no per-constructor implementations; requires `variant` Rust builtin for generic reconstruction; requires `object-map` in prelude (`[fn [let m f] [map-kv [fn [let k v] [pair k [f k v]]] m]]`)
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

**An open, user-extensible type system — and a runtime agnostic to prelude.** This is the deeper goal that this whatif serves. The `TypeNode` ADT makes type constructors first-class tinct values carrying their own traversal (`@Child` annotations → `map-children` for free). The Rust runtime is blind to which constructors exist — type rules for built-in forms live in prelude alongside user-defined ones.

The subtyping mechanism is **RDNF-based lattice inclusion** (S-883): `is_subtype(s, t) = is_empty(to_rdnf(s & ~t))`. RDNF normalization handles Union, Intersection, and Negation structurally as lattice operations — no per-constructor dispatch for these. Built-in structural atoms (Record, Function, App/TyCon, Recursive) are handled by `is_atom_subtype` with explicit arms. User-defined atoms participate via `as-type:` and `subtype:` annotations, dispatched from `is_atom_subtype`'s fallthrough — see §Extensibility.

This is a direct consequence of the project axiom: **the Rust runtime must be genuinely agnostic to what prelude does**. Adding a new type form requires no Rust changes — only a new TypeNode constructor with the appropriate annotations.

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
- **All other values** (`Value::String`, `Value::Int`, `Value::Dict`, etc.): A new `Value::Annotated { inner: ThunkId, annotation: Box<Value> }` wrapper carries the annotation. `inner` is lazy (ThunkId); `annotation` is materialized at annotation time. `annotation-of` dispatches on both `Value::Function` (reads `FnAnnotation`) and `Value::Annotated` (reads `.annotation`). Values without annotations return an empty dict.
- **Type-level positions** (type alias declarations, record field type annotations): The annotation is part of the type representation. `TyConDef` gains `annotation: IndexMap<String, Value>`. Record field type annotations are stored in `TypeNode.Record.field_annotations: Map String TypeNode` (each entry maps a field name to its annotation dict TypeNode, alongside the type in `fields`).

**`annotation-of` is a Rust builtin** that reads from all three storage sites uniformly, returning the annotation dict or an empty dict if no annotation is present. It is available at both runtime and in the type-stage evaluator.

**No fixed fields.** The distinction between "well-known" and "custom" is purely a matter of which code reads which keys — there is no architectural distinction. Adding `children:` to a TypeNode constructor declaration is identical to adding `my-org-field:` — both stored in the same dict, both readable via `annotation-of`.

### Representation: Rational Trees

A recursive type is represented as a **rational tree** — a finite graph with potentially cyclic edges. In tinct, recursive types are `TypeNode.Recursive` values — tinct nominal ADT values, not Rust enum variants:

```tinct
# A recursive type is a TypeNode.Recursive value:
# TypeNode.Recursive { var: "𝜇ꜱʏᴍ⧼lst⧽42", body: <TypeNode body with RecursiveRef> }

# The body is a concrete TypeNode with RecursiveRef nodes at recursive positions:
TypeNode.Recursive {
  var:  "𝜇ꜱʏᴍ⧼lst⧽42"
  body: TypeNode.Union types: [
    TypeNode.Absent
    TypeNode.Record fields: {
      "head": TypeNode.Int
      "tail": TypeNode.RecursiveRef name: "𝜇ꜱʏᴍ⧼lst⧽42"  # self-reference
    }  open: false
  ]
}
```

This is a **finite** representation of an **infinite** unrolling. The type checker unfolds `TypeNode.Recursive` on demand during subtype checking via `unfold_once`, using S-Assum sigma to detect when unfolding has returned to a previously-seen configuration.

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

`TypeNode` is the representation used by the **type-stage evaluator** — tinct code that computes and manipulates types at annotation resolution time. The type checker's internal representation is the `Type` Rust enum, which gains `Recursive` and `RecursiveRef` as new variants for equirecursive support.

`CheckerType` is the permanent boundary between these two worlds: `from_type` converts `Type` → TypeNode Value for type-stage eval; `typenode_value_to_type` converts back. The type checker never stores `CheckerType` internally — it works on `Type` throughout. `TypeVar` remains `Type::TypeVar` in the type checker; `TypeNode.TypeVar` is available in the type-stage evaluator for reflection only.

`Recursive` carries a `var` field — a globally unique gensym name generated at construction time by the `mu` combinator (via `gensym`) or by the expansion-stack resolver. This var is the sigma key used by S-Assum during subtype checking.

Each TypeNode constructor carries `as-type` and `guarding` in its constructor-level `@[...]` annotation. The traversal protocol (`children`, `map-children`) is derived automatically from field-level `@Child` annotations — no per-constructor implementation needed. `@Child` marks fields whose declared type contains TypeNode children; the traversal role is inferred from the field type (`TypeNode` → One, `[Seq TypeNode]` → Seq, `[Map K TypeNode]` → MapValues). Fields without `@Child` are non-children and pass through unchanged.

```tinct
TypeNode: [type
  # Primitives — leaf constructors: no @Child fields
  [Int@[as-type: [fn [let t] t]  guarding: true]]
  [Float@[as-type: [fn [let t] t]  guarding: true]]
  [String@[as-type: [fn [let t] t]  guarding: true]]
  [Bool@[as-type: [fn [let t] t]  guarding: true]]
  [Absent@[as-type: [fn [let t] t]  guarding: true]]
  [Unknown@[as-type: [fn [let t] t]  guarding: true]]
  [Never@[as-type: [fn [let t] t]  guarding: true]]
  # Structural
  [Record@[as-type: [fn [let r] r]  guarding: true]
    fields@Child: [Map String TypeNode]   # @Child on Map → MapValues (map over values, preserve keys)
    open:         Bool]                   # no @Child → NonChild, passes through
  [Union@[as-type: [fn [let u] u]  guarding: false]
    types@Child: [Seq TypeNode]]          # @Child on Seq → Seq (map over elements)
  [Intersect@[as-type: [fn [let i] i]  guarding: false]
    types@Child: [Seq TypeNode]]
  # TypeConstructor — two roles (see below)
  [TypeConstructor@[as-type: [fn [let t] t]  guarding: true]
    name: String]                         # no @Child → leaf
  [TypeApplication@[as-type: [fn [let a] a]  guarding: true]
    ctor@Child:  TypeNode                 # @Child on TypeNode → One
    args@Child:  [Seq TypeNode]]          # @Child on Seq → Seq
  [Arrow@[as-type: [fn [let a] a]  guarding: true]
    params@Child: [Seq TypeNode]          # @Child on Seq → Seq
    result@Child: TypeNode]              # @Child on TypeNode → One
  # Recursive — this whatif; body is concrete TypeNode with RecursiveRef(var) at recursive positions
  [Recursive@[as-type: [fn [let r] r]  guarding: true]
    var:         String                   # no @Child → NonChild (binder name, not a child)
    body@Child:  TypeNode]               # @Child on TypeNode → One
  # Internal sentinel — leaf; always a leaf
  [RecursiveRef@[as-type: [fn [let r] r]  guarding: false]
    name: String]                         # no @Child → leaf
  # Inference variable — leaf; name and level are not TypeNode children
  # TypeVar IS a TypeNode constructor — findable by walk_type, handled uniformly
  [TypeVar@[as-type: [fn [let t] t]  guarding: false]
    name:  String                         # e.g. "_t42"
    level: Int]]                          # Kiselyov creation-time level

# Protocol functions derived generically from @Child field annotations.
TypeNode: [merge TypeNode [

  # children: collect all @Child fields into a flat Seq
  children: [fn [let t]
    [flat-map [child-fields [TypeNode-ctor t]] [fn [let field]
      [let val [get t field]]
      [match [child-role [TypeNode-ctor t] field]
        One:       [Seq val]
        Seq:       val
        MapValues: [values val]]]]]

  # map-children: apply f to each @Child field, reconstruct same-shaped variant
  map-children: [fn [let f t]
    [variant [tag-of t]
      [object-map [fields [TypeNode-ctor t]] [fn [let field val]
        [if [child-field? [TypeNode-ctor t] field]
          [match [child-role [TypeNode-ctor t] field]
            One:       [f val]
            Seq:       [map f val]
            MapValues: [map-values f val]]
          val]]]]]                         # NonChild: unchanged

  # as-type: normalize user-defined constructors to existing forms
  as-type: [fn [let t]
    [let ann [annotation-of [TypeNode-ctor t]]]
    [if [has? ann as-type] [[get ann as-type] t] t]]]]
```

**`TypeNode-ctor t`** — returns the constructor function for a TypeNode value: `[get TypeNode [last [str-split "." [tag-of t]]]]`. This is the same expression already used inline in `children`, `as-type`, and `map-children`; it can be defined as a helper or inlined everywhere.

**Constructor annotation dict** — `annotation-of(TypeNode-ctor t)` returns the constructor's complete annotation dict. This includes constructor-level keys (`as-type:`, `guarding:`) AND a `field-annotations:` key mapping each field name to its annotation dict. The `field-annotations:` entry is populated at desugar time when `@Child` field annotations are processed — the desugar pass reads the field's declared type and stores `{ "role": "Seq" }` (or `"One"` or `"MapValues"`) in `FnAnnotation.extra["field-annotations"]["field-name"]`. Example for `TypeNode.Union`:

```text
annotation-of(TypeNode.Union) → {
  as-type:           <fn>,
  guarding:          false,
  field-annotations: { "types": { "role": "Seq" } }
  # "open" has no @Child → not present in field-annotations
}
```

**Helper functions** (`child-fields`, `child-role`, `child-field?`) use this unified annotation dict:

```tinct
child-fields:  [fn [let ctor] [keys [annotation-of ctor | .field-annotations]]]
child-role:    [fn [let ctor field] [[annotation-of ctor | .field-annotations | field] .role]]
child-field?:  [fn [let ctor field] [has? [annotation-of ctor | .field-annotations] field]]
``` (stored in `TypeNode.Record.field_annotations` per the General Annotation Syntax section above). `variant` is a Rust builtin that creates a `Value::Variant` with a given tag string and payload dict — enabling generic reconstruction without enumerating constructors.

**Role inference from field type**: the `child-role` function inspects the declared field type (from the TyConDef field annotation) to determine whether the child role is `One` (field type is `TypeNode`), `Seq` (field type is `[Seq TypeNode]`), or `MapValues` (field type is `[Map K TypeNode]`).

This requires three extensions, all specified in this proposal:

**1. `@[...]` is attachable to field type declarations** and to constructor names within `[type ...]` — both covered by the General Annotation Syntax section above. The parser recognizes `fieldname@Child: Type` inside constructor declarations; field annotations are stored in `TyConDef.field_annotations: IndexMap<String, IndexMap<String, Value>>`.

**2. `FnAnnotation` and `TyConDef` store all annotation fields uniformly** — see General Annotation Syntax.

**3. `annotation-of` reads the complete annotation dict** from both `Value::Function` (FnAnnotation) and TyConDef references — see General Annotation Syntax.

**Adding a new TypeNode constructor** requires only declaring it with the correct annotations:

```tinct
--- stage: type
[
  TypeNode: [merge TypeNode [
    [MyNewType@[
      guarding:  true                        # or false if non-guarding
      as-type:   [fn [let t] t]             # identity if no normalization needed
      subtype:   [fn [let a b sigma]        # open subtyping rule
                   ...]
    ]
      field@Child: TypeNode]]               # @Child marks TypeNode-typed fields
  ]]
]
```

All walkers — traversal (`children`, `map-children`, `walk_type`, `expand_all_tycon_apps`) and semantic (`is_subtype_inner`, `unify`, `Substitution::apply`) — handle the new constructor automatically via its annotations. No Rust changes required.

**`TypeNode.TypeConstructor` has two roles** that must be distinguished:

- **Transient (pre-normalization)**: `TypeNode.TypeConstructor "Color"` — a bare type name in a type-stage expression, e.g. the result of looking up `Color` in the type-stage env. Always eliminated by normalization (expansion → body TypeNode).
- **Leaf identity (post-normalization)**: `TypeNode.TypeConstructor "Color.Red"` — a qualified constructor name (containing `.`). The nominal identity of a specific constructor. Remains after normalization; appears as leaves inside expanded union bodies. `TypeConstructor "Color.Red" <: TypeConstructor "Direction.Red"` is false because names differ.

`TypeNode.TypeApplication` is **always transient** — it exists during type-stage computation but is always eliminated by normalization before the type checker sees the result. After normalization, the type checker works only with: primitives, Record, Union, Intersect, Arrow, Recursive, RecursiveRef, TypeVar, and qualified `TypeConstructor` leaves.

### Self-Hosted Type Traversal

The `TypeNode.children`, `TypeNode.map-children`, and `TypeNode.as-type` protocol functions (declared in the TypeNode ADT above) are all derived generically from `@Child` field annotations:

- **`TypeNode.children`** — flattens all `@Child` fields into a Seq, used by `walk_type` for pure structural traversal. No per-constructor implementation; role (One/Seq/MapValues) inferred from declared field type.
- **`TypeNode.map-children`** — applies `f` to each `@Child` field and reconstructs the same-shaped variant, used by `expand_all_tycon_apps`. No per-constructor implementation; uses the `variant` Rust builtin for generic reconstruction.
- **`TypeNode.as-type`** — normalizes user-defined TypeNode constructors to an existing form. Built-in constructors return themselves (identity). For constructors with distinct subtyping semantics, `as-type` returns `t` unchanged and the `subtype:` annotation handles dispatch.
- **`subtype:` annotation** — `Fn@[pair Bool sigma] [TypeNode TypeNode sigma]`. Registered on **user-defined TypeNode constructors only**. Built-in atoms (Int, Float, Record, Union, Intersect, Arrow, Recursive, TypeVar) are handled by `is_atom_subtype` with direct Rust pattern matching — they do not carry `subtype:` annotations. User-defined atoms that `as-type:` leaves unchanged fall through to `annotation-of(ctor)["subtype:"]`. Sigma is threaded as an immutable dict (pair → bool) when crossing the Rust/tinct boundary.

There is no `unify:` annotation. Unification (`unify()`) is a fixed Rust function — it involves TypeVar binding, occurs checks, level management, and Recursive-type opening, none of which are per-constructor user-extensible operations. For user-defined atoms, unification is structural: same constructor tag + pairwise `@Child` field recursion. This is identical to how Record and App/TyCon already work. No case in the type theory literature (Robinson 1965, Dolan 2016, Parreaux & Chau 2022, Chau & Parreaux 2026) requires user-defined constructors to override the structural unification default.

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

**Uniform representation.** `TypeNode.Recursive { var, body: TypeNode }` has `body@Child` — `walk_type` enters the body and naturally finds `TypeNode.TypeVar` nodes via traversal. `TypeNode.RecursiveRef` and `TypeNode.TypeVar` are leaves (`[]` children). All TypeNode forms including TypeVar are found by `walk_type` automatically.

`unfold_once(Recursive { var, body })` uses `typenode_map_children(body, |c| if is_recvar(c, var) { Recursive.clone() } else { unfold_once_inner(c, var, Recursive) })` — a predicate check, not an exhaustive TypeNode enumeration. `contains_recvar(body, name)` and `body_contains_tycon_ref(body)` similarly use `walk_type` with predicates. None of these require explicit TypeNode arm enumeration.

All existing type-stage combinators (`or`, `record`, `arrow`, `seq`, `map`, etc.) return the corresponding `TypeNode` constructor. Migration from `kind:`-keyed dicts is atomic — partial migration leaves the type checker in an inconsistent state.

**No walker needs explicit Rust arms for specific constructors in the walker-dispatch loop** — all traversal dispatch is uniform. (`is_atom_subtype` does have explicit Rust arms for built-in atoms, but that is atom-level comparison, not walker-loop dispatch.)

| Walker | Explicit walker-dispatch arm? | Mechanism |
|--------|---------------|-----------|
| `collect_type_vars` | No | `walk_type` + `typenode_tag(t) == "TypeNode.TypeVar"` predicate |
| `has_inference_vars` | No | Same |
| `check_kind_wellformed` | No | Pure `walk_type` |
| `unfold_once` | No | `typenode_map_children` + RecursiveRef name predicate |
| `contains_recvar` | No | `walk_type` + RecursiveRef name predicate |
| `body_contains_tycon_ref` | No | `walk_type` + TypeConstructor tag predicate |
| `is_subtype` | No | RDNF lattice inclusion (`is_empty(to_rdnf(s & ~t))`); `is_atom_subtype` handles built-in atoms; user-defined atoms dispatch via `annotation-of(ctor)["subtype:"]` in `is_atom_subtype` fallthrough |
| `unify` | Fixed Rust | TypeVar binding + occurs check + level management + Recursive opening; user-defined atoms use structural `@Child` pairwise recursion — no per-constructor extensibility |
| `Substitution::apply` | No | All via `typenode_map_children`; TypeVar's `apply:` annotation handles subst lookup |
| `PartialEq` | No | Structural TypeNode equality — field-by-field, no constructor special-cases |

**`collect_type_vars` reads level from `state.levels[name]`**, not from `payload["level"]`. The payload carries the creation-time level (fixed at `fresh_type_var()` call time); `state.levels` is the authoritative mutable current level (updated by level lowering). DICT-GEN generalization checks `state.levels[name] > enclosing_level` — always use `state.levels`, never the payload.

`RecursiveRef` does not need explicit arms anywhere: `Substitution::apply` passes it through unchanged via `typenode_map_children` (no @Child fields); it should not reach `is_subtype_inner` or `unify` directly (only appears inside Recursive bodies, unfolded by S-Exp).

### Extensibility

Users can freely compose existing `TypeNode` constructors into new type-stage functions — `non-empty-list`, weighted unions, constrained aliases — and the resolver handles them without changes. Traversal (`walk_type`) picks up new constructors automatically via `@Child` annotations with no Rust changes.

For genuinely new `TypeNode` constructors that introduce new atoms (new type forms with novel static semantics — capability types, session types, unit types), three annotations govern participation in the type system:

**`as-type:` (normalization to existing atoms).** If the user atom is definitionally equivalent to a combination of built-in types, `as-type:` maps it there. `to_rdnf` calls `as-type:` when it encounters a user-defined atom, reducing it before normalization. The mapping must be **monotone** (if A ≤ B in the user ordering, then `as-type(A)` ≤ `as-type(B)`) and **conservative** (`as-type(A)` is a supertype of `A`). Identity `as-type:` (returning the atom unchanged) signals an opaque atom with no further reduction.

**`subtype:` (atom-level subtype rule).** When `is_atom_subtype` encounters a user-defined atom that `as-type:` left unchanged, it dispatches to `annotation-of(ctor)["subtype:"]`. Signature: `Fn@[pair Bool sigma] [TypeNode TypeNode sigma]`. The correct implementation pattern chains via `[is-subtype ExistingAtom other sigma]` to inherit full transitivity through built-in types without enumerating chains manually:

```tinct
# PositiveInt is a subtype of Int (and therefore Number, etc.) — inherits all transitivity
subtype: [fn [let self other sigma]
  [is-subtype Int other sigma]]
```

**Sigma must be threaded.** The `subtype:` function must forward the received sigma when calling `[is-subtype ...]` internally. Dropping sigma silently breaks coinductive checking for recursive types that appear in the supertype argument.

**BAS §3.7 proof obligations.** These are user responsibilities — they cannot be mechanically enforced. Any `subtype:` implementation must satisfy: (1) **consistency preservation** — the rule must not make a consistent type inconsistent; (2) **depth decrease** — recursive application must terminate (no infinite subtype chains); (3) **equal-depth consistency** — types at the same depth remain consistent. A `subtype:` that violates transitivity or produces contradictory answers depending on the RDNF decomposition path breaks the Boolean algebra's unique complementation property (BAS Theorem 3.2).

**Component disjointness default.** User-defined atoms belong to their own lattice component — disjoint from all built-in atoms and from each other by default (analogous to S-ClsBot for nominal class tags). `atoms_are_component_disjoint(UserAtom, AnyBuiltinAtom)` returns true unless the user's `as-type:` or `subtype:` establishes a relationship.

**Conservative default.** A user atom with no `as-type:` or `subtype:` annotation is treated as opaque and nominally disjoint from everything except `Top`. This is correct: a new atom type is in its own component of the lattice (Dolan 2016, §3.2.3) until the user declares otherwise.

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

  # Step 2b: Early exit for zero-param types with no TypeConstructor references in body.
  # body_contains_tycon_ref: walk_type over body; returns true iff any node has
  # typenode_tag == "TypeNode.TypeConstructor" AND name.contains('.') == false.
  # (Bare = transient = unexpanded reference. Qualified leaves are already normalized.)
  # No gensym, no stack push, no expand_all_tycon_apps needed — Arc::clone of body suffices.
  # Covers @Int, @Float, @Bool, @Absent, @Unknown, @Never and simple zero-param aliases.
  if decl.params.is_empty() and not body_contains_tycon_ref(decl.body):
    return CheckerType(Arc::clone(&decl.body))

  # Step 3: Builtin-opaque types stay as App leaves — no structural expansion
  if decl.is_builtin_opaque:
    base = TypeNode.TypeConstructor name: name
    return apply_args(base, args)    # → TypeNode.TypeApplication { ctor: TypeConstructor(name), args: args }

  # Step 4: Expansion stack cycle detection.
  # lookup_tycon_def returns Arc<TyConDef>; Arc::ptr_eq gives stable identity
  # across nested scope lookups — two lookups of the same alias always compare equal.
  tycon_arc = TypeEnv::lookup_tycon_def(name)
  if let Some(pre_name) = stack.get_pre_assigned_name_by_ptr(&tycon_arc):
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

  # Case 2: guarding constructor — any RecursiveRef(var) under this node is safely guarded.
  # Reads from the constructor's @[guarding: Bool] annotation — no exhaustive match needed.
  ctor_ann = annotation_of(TypeNode_constructor_for(node))
  if ctor_ann.guarding:
    return true

  # Case 3: non-guarding (Union, Intersect, foreign RecursiveRef) — recurse into children.
  # TypeNode.children uses @Child field annotations to flatten children — no exhaustive match.
  return all(TypeNode.children(node), c → is_contractive(c, var))
```

Three cases — no exhaustive TypeNode variant enumeration. New constructors declare their guardedness in `@[guarding: Bool]` and are handled automatically. `TypeNode.Recursive` has `guarding: true` — an inner Recursive binder guards the outer var (any occurrence of the outer var under `μb.T` is separated from the outer binder by at least one type constructor).

This check runs in two places:

1. **In `mu`**: after `[let body [f TypeNode.RecursiveRef name: var]]`, before constructing `TypeNode.Recursive`. If `not(is_contractive(body, var))`, emit `TypeError(NonContractive)` with a diagnostic naming the var.
2. **In `expand_named`**: after step 7 (`expand_all_tycon_apps`), before wrapping in `TypeNode.Recursive`. If `not(is_contractive(expanded, fresh_var))`, emit `TypeError(NonContractive)`.

**Why construction-time, not checker-time via `▷` modality.**

Chau & Parreaux (2026, Fig. 3) use a `▷` ("later") modality on S-Assum: the sigma hypothesis `(A, B) ∈ Σ` is usable only after passing through at least one guarding constructor, preventing immediate use on non-contractive types. An alternative design would omit construction-time contractiveness checking and instead implement `▷` in `is_subtype_inner` — allowing non-contractive types to be constructed and handling them "gracefully" in the checker.

**This alternative is rejected for tinct.** The tradeoffs:

| | Construction-time `is_contractive` | Checker-time `▷` modality |
|--|--|--|
| **Error location** | At the `mu` call site — exactly where the problem was written | At a use site downstream — confusing ("why doesn't this type match anything?") |
| **Checker complexity** | Simple flat `HashSet` for sigma | `▷` tracking through every BAS arm |
| **Non-contractive semantics** | `μa.a` is ill-formed and rejected | `μa.a` is a valid type that subtypes nothing (like `Never`, but harder to explain) |
| **Termination** | Guaranteed — checker never sees non-contractive types | Requires depth limit as backstop for S-Exp loops that never discharge `▷` |
| **Use cases** | No legitimate use for non-contractive types in tinct | Would support domain-theory and proof-system uses |

Non-contractive types have no practical meaning in tinct's configuration language context: `μa.a` is the fixed point of the identity function — semantically `⊥` (uninhabited or bottom). Nobody intentionally writes this. Catching it at construction with a clear `NonContractive` error pointing at the `mu` expression is strictly better UX than allowing construction and silently failing to subtype downstream.

**Soundness of the flat `HashSet` without `▷`.**

Omitting `▷` is sound given the construction-time invariant: all `TypeNode.Recursive` values reaching `is_subtype_inner` are contractive. After S-Exp unfolds `μa.T[a]` via `unfold_once`, the substituted `μa.T[a]` nodes appear only at former `RecursiveRef(a)` positions — which by contractiveness are all under at least one guarding constructor. The type checker always traverses at least one guarding BAS rule (Record field decomposition, Arrow param/return, TypeApplication variance) before the sigma key `(a.var, b.var)` matches. The `▷` modality is satisfied by construction, not by explicit tracking. (This invariant is load-bearing: if a non-contractive type were to bypass construction-time checking, the checker could diverge.)

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

The annotation resolver detects the `JsonValue` self-reference via the expansion stack and wraps the type in `TypeNode.Recursive` automatically — no explicit `mu` needed. For inline annotation positions, `mu` provides the same type without naming it:

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

**Why equirecursive types and not the current workaround?** The current type checker loses the `JsonValue` type after ~4 levels of nesting — `v.0.0.0.key` types as `Unknown`. With `TypeNode.Recursive`, the type checker unfolds on demand to any finite depth, always returning `JsonValue`. `count-numbers` is correctly typed regardless of how deeply nested the input is.

### Coinductive Subtype Checking

#### Globally Unique RecursiveRef Names

Every `TypeNode.Recursive` value carries a globally unique `.var` name, regardless of how it was produced:

- **`mu` combinator**: calls `[gensym-with-scope "𝜇" "rec"]` in tinct — the prelude wrapper over `builtin-gensym`. Produces `"𝜇ꜱʏᴍ⧼rec⧽N"` via the single global counter shared across all gensym call sites.
- **Named-alias expansion stack**: calls `gensym_fresh('𝜇', alias_name)` in Rust — `builtins_meta::gensym_fresh`, same global counter. Produces `"𝜇ꜱʏᴍ⧼EvenList⧽N"`. The alias name is embedded as the tag; error messages display `μEvenList.T`.

This eliminates variable capture in `unfold_once` (no two binders share a name) and eliminates sigma false positives from shadowed alias names.

#### S-Exp + S-Assum (Chau & Parreaux 2026)

The bisimulation uses the **S-Exp + S-Assum** framework, which is proven sound for BAS with equirecursive types. Unlike the naive "distribute before unfolding" approach, the sigma context Σ is threaded through **all** subtyping rules — so the coinductive hypothesis is available inside union, intersection, record, and App sub-checks.

Two rules govern recursive types within the standard `is_subtype_inner`:

- **S-Assum**: at the start of every call, if `(a.var, b.var)` ∈ Σ, return `true` immediately (coinductive hypothesis). Add the pair to Σ before proceeding.
- **S-Exp**: if `a` is `TypeNode.Recursive`, unfold it once and continue — Σ already contains the original pair.

**Two-level design.** Sigma is allocated once per top-level subtype check and threaded through every recursive call. It is never recreated within `is_subtype_inner`. The Rust type system enforces this structurally — `sigma: &mut HashSet<...>` is a required parameter, so any recursive call that omits it fails to compile.

```rust
/// Public entry point — allocates sigma once for the entire check.
pub fn is_subtype(a: CheckerType, b: CheckerType) -> bool {
    let mut sigma = HashSet::new();
    is_subtype_inner(a, b, &mut sigma)
}

/// Recursive worker — sigma is passed to EVERY recursive call within this function.
/// No arm allocates a new sigma. The Rust compiler enforces sigma threading:
/// any recursive call missing sigma is a compile error.
fn is_subtype_inner(a: CheckerType, b: CheckerType,
                    sigma: &mut HashSet<(String, String)>) -> bool {
    // S-Assum: check hypothesis before anything else
    if let (CheckerType::Node(TypeNode.Recursive { var: v1, .. }),
            CheckerType::Node(TypeNode.Recursive { var: v2, .. })) = (&a, &b) {
        let key = (v1.clone(), v2.clone());
        if sigma.contains(&key) { return true; }
        sigma.insert(key);
    }

    match (&a, &b) {
        // S-Exp: unfold; sigma already contains (v1, v2)
        (CheckerType::Node(TypeNode.Recursive { .. }), _) =>
            is_subtype_inner(unfold_once(a), b, sigma),
        (_, CheckerType::Node(TypeNode.Recursive { .. })) =>
            is_subtype_inner(a, unfold_once(b), sigma),

        // BAS rules — sigma passed to every recursive call
        (_, CheckerType::Node(TypeNode.Union { types })) =>
            types.iter().any(|t| is_subtype_inner(a.clone(), t.clone(), sigma)),
        (CheckerType::Node(TypeNode.Union { types }), _) =>
            types.iter().all(|t| is_subtype_inner(t.clone(), b.clone(), sigma)),
        // Record field checks, Arrow param/return, TypeApplication variance,
        // Intersect, negation, uniform tail — all follow the same pattern:
        // every recursive call passes sigma.
        ...
    }
}
```

The comment "Record, App, Arrow, ... — same pattern" is the full specification: every BAS arm passes sigma to every recursive call. The ~15–20 such call sites in the full implementation are an implementation detail; the invariant — sigma is always passed, never recreated — is the load-bearing property, and Rust enforces it.

**Sigma key**: `(a.var, b.var)` — a `(String, String)` pair of binder names. O(1), thread-safe, globally unique (no false positives from shadowed aliases or mu-counter collisions).

**`unfold_once`**: replace `TypeNode.Recursive { var, body }` with `body[RecursiveRef(var) ↦ self]` — substituting all `RecursiveRef` occurrences with the full recursive type. After substitution, the `Recursive` node at each recursive position carries the same `.var` name as the original binder. When `is_subtype_inner` encounters those positions, S-Assum fires immediately — the hypothesis `(v1, v2)` is already in Σ.

**Why S-Exp + S-Assum is necessary for BAS**: the naive "distribute over union first" approach fails because the hypothesis established for `(μa.T[a], A ∨ B)` is keyed on that exact pair. After distribution, sub-checks for `(μa.T[a], A)` and `(μa.T[a], B)` have different keys — the hypothesis is unavailable. S-Assum fires at the start of every call and is available inside all BAS decomposition rules, preventing this failure.

### Unification

Five match arms cover all cases involving `Recursive` and `TypeVar` types. **Match ordering is critical**: TypeVar binding arms must come BEFORE the asymmetric Recursive opening arms. Without this, `unify(Recursive, TypeVar)` would hit Arm 4, open the Recursive, and bind the TypeVar to the opened body — losing the recursive structure. With the correct ordering, Arm 3 fires and binds the TypeVar to the full Recursive type.

```rust
match (a, b) {
    // Arm 1 (symmetric): both are Recursive — open with ONE shared fresh TypeVar.
    (CheckerType(TypeNode.Recursive { var: v1, body: b1 }),
     CheckerType(TypeNode.Recursive { var: v2, body: b2 })) => {
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        let b_open = substitute(b2, v2, &fresh);
        unify(a_open, b_open, subst, state)
    }

    // Arm 2 (TypeVar left): bind TypeVar to the right side.
    // Must come BEFORE the asymmetric Recursive arms — otherwise
    // (Recursive, TypeVar) would be handled by Arm 4 incorrectly.
    (CheckerType(TypeNode.TypeVar { name, .. }), b) => {
        occurs_check(name, &b)?;
        subst.bind(name, b);
        Ok(())
    }

    // Arm 3 (TypeVar right): bind TypeVar to the left side.
    (a, CheckerType(TypeNode.TypeVar { name, .. })) => {
        occurs_check(name, &a)?;
        subst.bind(name, a);
        Ok(())
    }

    // Arm 4 (asymmetric left): left is Recursive, right is concrete (not TypeVar, not Recursive).
    (CheckerType(TypeNode.Recursive { var: v1, body: b1 }), other) => {
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        unify(a_open, other, subst, state)
    }

    // Arm 5 (asymmetric right): right is Recursive, left is concrete.
    (other, CheckerType(TypeNode.Recursive { var: v2, body: b2 })) => {
        let fresh = state.fresh_type_var();
        let b_open = substitute(b2, v2, &fresh);
        unify(other, b_open, subst, state)
    }

    // Structural cases for concrete non-Recursive, non-TypeVar types...
    ...
}
```

**Termination argument.** Arms 2 and 3 (TypeVar binding) terminate immediately — one substitution entry added, no recursive call. Arms 4 and 5 (Recursive opening) terminate because `substitute(body, var, &fresh)` replaces `RecursiveRef(var)` with `TypeNode.TypeVar { name: fresh }`. After opening, the former recursive positions hold TypeVars. When unification descends and encounters `fresh` paired against any type, Arm 2 or 3 fires and binds — no further Recursive arm fires on that side. Structural induction on the non-Recursive sides handles all remaining sub-checks.

**Why `other` may contain `Recursive` sub-terms without causing divergence.** The claim "`other` contains no RecursiveRef — Recursive arms cannot re-fire" was imprecise. `other` CAN contain `TypeNode.Recursive` nodes as sub-terms (e.g., `Union([Recursive { v3, b3 }, Int])`). When structural descent reaches such a node, one of Arms 2 or 3 fires — but for a DIFFERENT binder (v3, not v1). This binder is opened with a new fresh TypeVar, and the same argument applies recursively. Termination follows by structural induction: each arm firing eliminates one Recursive node from the top of one side, and substitution with TypeVar does not re-introduce Recursive at the top.

**Why shared fresh var (not sequential opening) for the symmetric case.** Two sequential opens (`fresh1` for left, `fresh2` for right) would work but produce `fresh1 → fresh2` in the substitution — an extra indirection that surfaces in error messages and type display. The shared fresh var produces a direct result: `μ_t0.T[_t0]` where `_t0` is immediately the representative. Both approaches produce the same principal type; the shared fresh var is more direct.

`unfold_once` — which replaces `RecursiveRef` with the full `Recursive` type, making the tree **larger** — is used only in subtype checking (where S-Assum prevents divergence), not in unification.

### Mutual Recursion

Mutually recursive type aliases — where `A` references `B` and `B` references `A` — require no explicit `mu` from the user. The annotation resolver's expansion stack detects the cycle automatically. Users write plain type aliases:

```tinct
EvenList: [type [or Absent [record head: Int  tail: OddList]]]
OddList:  [type [or Absent [record head: Int  tail: EvenList]]]
```

Each entry pushed to the stack is pre-assigned a fresh name via `fresh_rec_var_with_source` at push time. Expansion of `EvenList` proceeds as follows (using `"𝜇ꜱʏᴍ⧼EvenList⧽42"` and `"𝜇ꜱʏᴍ⧼OddList⧽43"` as generated internal names, with source names `"EvenList"` and `"OddList"` stored for diagnostics):

1. Push `(arc_EvenList, "𝜇ꜱʏᴍ⧼EvenList⧽42")` to stack; begin expanding EvenList's body
2. The body references `OddList` — push `(arc_OddList, "𝜇ꜱʏᴍ⧼OddList⧽43")`; begin expanding OddList's body
3. OddList's body references `EvenList` — `arc_EvenList` is already in stack (via `Arc::ptr_eq`) → emit `TypeNode.RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")`
4. Pop `OddList`: expanded body = `Absent | {head: Int, tail: RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")}`. Does the body contain `RecursiveRef("𝜇ꜱʏᴍ⧼OddList⧽43")`? **No** — OddList is not the cycle origin. Return the body **as-is**, without wrapping.
5. Pop `EvenList`: expanded body = `Absent | {head: Int, tail: (Absent | {head: Int, tail: RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")})}`. Does the body contain `RecursiveRef("𝜇ꜱʏᴍ⧼EvenList⧽42")`? **Yes** — EvenList is the cycle origin. Wrap: `TypeNode.Recursive { var: "𝜇ꜱʏᴍ⧼EvenList⧽42", body: <full body> }`.

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

2. New `Value::Annotated { inner: ThunkId, annotation: Box<Value> }` — wraps any non-function value with an annotation dict. `annotation-of` dispatches on both variants. All other Value operations unwrap `Annotated` transparently (pattern matching, equality, display).

3. `TyConDef` gains `annotation: IndexMap<String, Value>` — for type alias and type constructor declaration annotations. `annotation-of` on a TyConDef reference returns this dict.

4. `TypeNode.Record` gains `field_annotations: Map String TypeNode` — each key maps a field name to its annotation dict expressed as a TypeNode dict. Used by record field type declarations (`host@[required: true]: String`).

**Impact:** Moderate — new Value variant (transparent in most match arms); FnAnnotation extension; TyConDef annotation field; parser/desugar changes at each annotatable grammar position.

### `src/type_def.rs` — Two new `Type` variants

### Architectural Note

**The `Type` Rust enum remains as the type checker's internal representation.** `InferState`, `TypeScheme`, `TypeEnv.bindings`, and `Substitution` continue to use `Type`. No internal migration is required.

**Dispatch architecture.** `is_subtype` uses RDNF lattice inclusion. Built-in atoms (Union, Intersect, Record, Arrow, TypeVar, Recursive) are handled by `is_atom_subtype` with direct Rust pattern matching — no arena, no annotation dispatch. User-defined atoms that `as-type:` leaves unchanged fall through to `annotation-of(ctor)["subtype:"]`; a call-local `ThunkArena` is allocated only at that fallthrough point to construct the TypeNode arguments. `unify()` is fixed Rust with structural `@Child` pairwise recursion for user-defined atoms — there is no `unify:` annotation. See §Dispatch Protocol for the full design.

`typenode_value_to_type` converts TypeNode results back to `Type` for storage in `InferState`. Traversal (`@Child` → `map-children`) is equally uniform.

**Two Rust checks remain as dispatch infrastructure** (not type rules, not migratable):
- Depth guard: `if depth >= MAX_SUBTYPE_DEPTH { return false; }` — prevents stack overflow on pathological types
- Error sentinel: `if matches!(sub | sup, Type::Error) { return false; }` — `Type::Error` is a Rust-internal sentinel for failed inference, not a user-visible TypeNode constructor

### Dispatch Protocol

**`is_subtype` needs no arena.** RDNF normalization (`to_rdnf`) decomposes only the Boolean skeleton of a type — it never constructs new TypeNode values and never calls tinct. Built-in atom comparison (`is_atom_subtype`) operates on `Type` values directly with Rust pattern matching. No `ThunkArena` is involved in the main subtype path.

**Arena at `is_atom_subtype` fallthrough only.** When `is_atom_subtype` encounters a user-defined atom and dispatches to `annotation-of(ctor)["subtype:"]`, it must pass the atom as a TypeNode `Value::Variant` to the tinct function. A call-local `ThunkArena` is allocated at this point, used to construct the TypeNode argument, and discarded when the tinct call returns. Leaf atoms (Int, Float, Bool, etc.) have `payload: None` and bypass arena allocation.

**`type_to_typenode_full`** — converts a `Type` atom to its TypeNode `Value::Variant` for passing into the user-defined `subtype:` tinct function. Called only at the `is_atom_subtype` fallthrough, not throughout `is_subtype`. Must satisfy `typenode_value_to_type(type_to_typenode_full(t)) == t` for all atom types.

**Sigma wire format:** Sigma (the coinductive visited-pairs set) crosses the Rust/tinct boundary as a nested dict:

```tinct
# sigma: { v1 → { v2 → true } }
sigma-has?: [fn [let sigma v1 v2] [get-or false v2 [get-or [] v1 sigma]]]
sigma-add:  [fn [let sigma v1 v2]
  [merge sigma [[[v1]: [merge [get-or [] v1 sigma] [[[v2]: true]]]]]]]
```

Sigma is always returned from `subtype:` functions — even on failure. S-Assum inserts a pair before the recursive call; discarding it on failure would allow a different recursive path to loop on the same pair.

**`subtype:` function signature:** `Fn@[pair Bool sigma] [TypeNode TypeNode sigma]`

**Re-entrant dispatch:** User-defined `subtype:` functions call `[is-subtype ExistingAtom other sigma]` to delegate to built-in subtype rules, inheriting full transitivity through the RDNF machinery. `is-subtype` is exposed as a Rust builtin in the type-stage environment. The tinct `subtype:` for `TypeNode.Recursive` similarly calls `[is-subtype ...]` for S-Exp unfolding. Depth is managed entirely on the Rust side.

**`unify` is fixed Rust — no dispatch.** User-defined atoms unify structurally: same constructor tag + pairwise `@Child` field recursion. No custom `unify:` annotation exists. Research across the type theory literature found no case where user-defined constructors need non-structural unification (variance is a subtyping property, not a unification one; FDs are constraint-solver rules; GADTs emit pattern-typing equalities; TypeApplication is always normalized before reaching the unifier). The `unify()` function handles TypeVar binding, occurs checks, level management, and Recursive-type opening internally — these are built-in concerns not suitable for user extension.

**Error reporting:** When a `subtype:` annotation crashes, returns malformed sigma, or returns a non-boolean, the type checker raises a `TypeRuleError` — a new named typed variant in `TypeErrorTyped` (defined in `src/type_errors.rs`), following the existing per-error-struct convention. Fields: `constructor: String`, `sub: Type`, `sup: Type`, `reason: String`, `span: Span`, `call_stack: Vec<TypeSpanFrame>`. This gives users a clear diagnostic pointing at the faulty annotation rather than an opaque eval error.

---

**Current:** `is_subtype_inner` and `unify` use per-constructor Rust match arms for all built-in type forms.

**Proposed:** Two new `Type` variants (already added in S-860):

```rust
Type::Recursive { var: String, body: Box<Type> }
Type::RecursiveRef(String)
```

`TypeNode.Recursive` and `TypeNode.RecursiveRef` are full members of the TypeNode ADT:

```tinct
TypeNode: [type
  ...
  [Recursive@[guarding: true] var: String  body: TypeNode]
  [RecursiveRef name: String]]
```

`is_subtype_inner` becomes pure dispatch: for every constructor, `is_atom_subtype` handles built-in atoms with explicit Rust arms; user-defined atoms fall through to `annotation-of(ctor)["subtype:"]`. `unify` remains fixed Rust — structural `@Child` pairwise recursion for user-defined atoms, no per-constructor dispatch.

`Substitution::apply`, `collect_type_vars`, `has_inference_vars`, and all other walkers use `walk_type` with predicates via `map-children`. No per-constructor arms.

**Impact:** All per-constructor Rust arms in `is_subtype_inner`, `unify`, and walker functions deleted. Call-local arena added at dispatch boundary. `Type` enum and internal state (`InferState`, `Substitution`, `TypeScheme`) unchanged.

### `src/type_env.rs` — Merged `TyConDef` (eliminates `TypeAlias`)

**Current:** Two separate stores in `TypeEnv`: `type_aliases: HashMap<String, TypeAlias>` (params + body `Type`) and `tycon_defs: HashMap<String, TyConDef>` by value (variance + constructors + builtin_type). Both are registered for every `[type ...]` declaration. `TyConDef.constructors` is dead storage — never populated at any creation site.

**Proposed:** Merge into a single unified store: `tycon_defs: HashMap<String, Arc<TyConDef>>`. `TypeAlias` struct and `type_aliases` map are eliminated. The storage uses `Arc<TyConDef>` (not value) for two reasons:

1. **Stable pointer identity for the expansion stack.** `expand_named` uses `Arc::ptr_eq` to detect cycles — two lookups of the same alias from different nested scopes must produce the same Arc pointer. With value storage, `HashMap::get` returns a reference with a shorter lifetime that cannot be stored in the stack; with `Arc<TyConDef>`, cloned Arc handles are stable indefinitely.

2. **Thread-safe sharing for parallel type checking.** `Arc<TyConDef>` is `Send + Sync`. When the type checker runs parallel workers (checking different dicts concurrently), each worker holds its own `Arc<TyConDef>` clone with no copying of data. Value storage in a `HashMap` cannot be directly shared across thread boundaries without going through the full `Arc<TypeEnv>` indirection.

Migration: `insert_tycon_def` takes `Arc<TyConDef>`. Every `TyConDef { ... }` construction site wraps in `Arc::new(...)`. `lookup_tycon_def` returns `Arc<TyConDef>` (a clone of the stored Arc).

```rust
pub struct TyConDef {
    /// Declared type parameter names (e.g., ["a", "k", "v"]).
    /// In `body`, parameters appear as `Type::TypeVar` sentinels — distinct names from inference vars.
    /// Eliminated by substitution in expand_named at use sites.
    pub params: Vec<String>,

    /// Parametric Type body. Parameter names appear as `Type::TypeVar` with a distinct naming
    /// convention (e.g. param prefix) to distinguish them from inference variables.
    ///
    /// INVARIANT: body contains no inference TypeVars (those live in InferState.subst only).
    pub body: Type,   // Type Rust enum — the type checker's internal representation

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

1. **`expand_named(name, args, stack) -> CheckerType`**: Unified lookup via `env.lookup_tycon_def(name)` which returns `Arc<TyConDef>`. All named types — primitives, structural aliases, nominal ADTs — are registered as `Arc<TyConDef>` entries; there is no separate name-list fast path. The expansion stack is `IndexSet<(Arc<TyConDef>, String)>` — cycle detection uses `Arc::ptr_eq` on the stored Arc handles, giving stable identity across nested scope lookups. Wraps in `TypeNode.Recursive` only at the cycle-origin level. See §Annotation Resolver for the complete algorithm.

2. **`expand_all_tycon_apps(node, stack) -> CheckerType`**: Recursively eliminates transient `TypeNode.TypeApplication`/bare `TypeNode.TypeConstructor` by calling `expand_named`. The TypeApplication and TypeConstructor arms use Rust-level tag matching (the frequent hot cases). The `_:` fallthrough uses `typenode_map_children` — a pre-cached tinct function resolved once at init, not `eval_type_stage_value` on a raw AST expression per node. New TypeNode constructors are handled by the fallthrough automatically.

3. **`eval_type_stage_expr(expr, env) -> CheckerType`**: Evaluates expression annotations via `materialize_sync`. Result goes through `TypeNode.as-type` normalization + `expand_all_tycon_apps`.

4. **Remove `resolve_typenode`**: Both paths return `CheckerType::Node(normalized)` directly.

All `TypeNode.Recursive` `.var` names are globally unique (`𝜇ꜱʏᴍ⧼...⧽N`), enabling collision-free sigma keys.

**Impact:** Moderate — four new functions replace `instantiate_type_alias`, `expand_alias_body_guarded`, and all name-based expression annotation dispatch.

### `src/type_unify.rs` — `is_subtype` and `unify`

**Current:** Handles `TypeConstructor`/`TypeApplication` via `UNIFY-TYCON` (name equality). No handling of `TypeNode.Recursive` or `TypeNode.RecursiveRef`. After normalization, `TypeApplication(TypeConstructor)` never reaches `is_subtype_inner` — the UNIFY-TYCON arm becomes unreachable and is removed. `TypeNode.TypeConstructor "Color.Red"` (qualified constructor leaf) uses name equality, which is correct for constructor identity.
**Proposed:** Extend `is_subtype_inner` with S-Exp + S-Assum: add `sigma: &mut HashSet<(String, String)>` threaded through all arms. At the top of each call, when both sides are `CheckerType::Node(v)` where `v` is `TypeNode.Recursive`, check and update sigma. Add S-Exp arm that calls `unfold_once(a)` — pure TypeNode structural substitution replacing every `TypeNode.RecursiveRef name: var` in `body` with the full `Recursive` node — and re-enters. No tinct evaluation needed. All BAS arms pass sigma through.

Add unification arms using simultaneous opening (§Unification). `TypeNode.RecursiveRef` never reaches the unifier directly — only appears inside a Recursive body during S-Exp unfolding, resolved by the sigma context.

`Substitution::apply`: one explicit arm for `TypeNode.TypeVar` (subst lookup by name; if unbound, return unchanged). All other TypeNode constructors — including Recursive, RecursiveRef, Union, Record, Arrow — are handled by `typenode_map_children(node, |c| apply(c, subst))`. RecursiveRef passes through correctly because it has no @Child fields. Recursive body is substituted correctly because `body@Child` causes `typenode_map_children` to apply `apply` to it. No additional explicit arms needed.

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

### Type checker performance

**Current:** Subtype checking terminates quickly (no coinductive loop).
**Proposed:** Sigma set grows by one entry per `(Recursive, Recursive)` pair encountered. For typical config schemas (finite mutual recursion depth), the set stays small. Sigma is allocated per top-level `is_subtype` call and dropped on return.

**Primitive TypeNode interning.** `CheckerType::Node(TypeNode.Int)` is a heap-allocated tinct Value — a regression from `Type::Int` which was a zero-allocation Rust enum variant. The 7 payload-free primitive constructors (`Int`, `Float`, `String`, `Bool`, `Absent`, `Unknown`, `Never`) must be pre-interned in `TypeStageEnv` as shared `Arc<Thunk>` values. `TypeStageEnv::primitive_node(name)` returns `Arc::clone(&env.int_node)` etc. — an atomic reference-count bump with no heap allocation. All call sites that produce primitive TypeNodes use this path rather than constructing new Values.

**Impact:** Moderate — new per-call sigma allocation (negligible for non-recursive programs); primitive TypeNode interning eliminates allocation regression for the common case.

## Downstream: validate-tinct-rewrite

Once equirecursive types land, `validate_value` in `src/builtins_meta.rs` (~267 lines) can be rewritten as a tinct stdlib function. `regex-match?` is already available; the only missing piece is a recursive type alias to type the schema dict.

- Define the schema dict type in `stdlib/prelude.llt` using a `mu`-type alias covering all schema keys: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum`
- Rewrite `validate` as a tinct function: call `regex-match?` for `pattern`, recurse on `fields:` and `items:` entries, collect violations into a Seq; remove `validate_value` from `src/builtins_meta.rs`
- Keep `validate` registered as a thin Rust stub that calls the tinct function and maps errors to `SchemaViolation` error kind
- Tests: all existing `validate` corpus tests pass after rewrite; validate over 1000-entry dict completes in <100ms

## Prerequisites

- **user-type-constructors** — already accepted and in implementation (S-842–S-851). `TypeConstructor`, `TypeApplication`, `RowTail::Uniform`, and the scoped `TyConDef` registry (as TypeNode values / `Arc<TyConDef>`) are the baseline this feature builds on. Equirecursive types add `TypeNode.Recursive` and `TypeNode.RecursiveRef` — the two TypeNode constructors that cannot be expressed without equirecursive support — plus the coinductive subtype checking algorithm (S-Exp + S-Assum) and the `mu` combinator.
All other infrastructure required by this proposal — annotation system, TypeNode ADT, TyConDef merge, CheckerType boundary wiring (`from_type`, `typenode_value_to_type`), eval_type_stage_expr, etc. — is fully specified in the §Design and §What Would Change sections above. It is part of this proposal's implementation, not a prerequisite.

## References

- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM Transactions on Programming Languages and Systems*, 15(4), 575–631. — [foundational coinductive subtype algorithm; proven sound for function and base types; S-Assum/S-Hyp rules generalized by Chau & Parreaux for BAS]
- Chau, T. & Parreaux, L. (2026). "Boolean-Algebraic Subtyping with Equirecursive Types." §3.3.1. — [S-Exp + S-Assum framework proven sound for BAS union/intersection/negation; Σ context threading through all derivation rules; adopted by this design]
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. §21 "Recursive Types." — [equirecursive vs isorecursive comparison; rational tree representation; unfolding semantics; simultaneous-opening for recursive type unification (§21.8)]
- Ancona, D. & Zucca, E. (2002). "A Theory of Mixin Modules." *ACM TOPLAS*, 24(5), 578–637. — [equirecursive types in structural object systems, closely related to BAS]
- Huet, G. (1976). "Résolution d'Équations dans des Langages d'Ordre 1, 2, ..., ω." Ph.D. thesis. Université Paris VII. — [rational tree unification; the mathematical foundation for representing recursive types as finite cyclic graphs]
