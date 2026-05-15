# What If: CHR-Unified Type Constraints for tinct

**State:** Proposal

What would it take to unify functional dependencies and type-level computation into a single, coherent constraint system grounded in Constraint Handling Rules?

## Current State

Tinct's type constraint system handles two distinct kinds of type-level reasoning through two separate, ad-hoc mechanisms:

**Mechanism 1 — Functional dependency improvement (propagation).**  
During HM unification, when a multi-parameter constraint's determining type variables become ground, the type checker propagates the determined variable's binding via `improve_functional_dependency()` in `src/type_unify.rs`. Currently this works only for arithmetic classes (`Addable`, `Subtractable`, `Multipliable`, `Divisible`) through a hardcoded 9-entry lookup table (`lookup_arithmetic_instance`):

```tinct
# This works — Addable a b c constraint with FD (a,b)→c
[fn [x@Int y@Float] [+ x y]]   # infers Float — FD fires: Addable Int Float → Float
```

**Mechanism 2 — Type-stage functions (simplification).**  
The `--- stage: type` sections define type-level functions that compute type dicts at annotation-resolution time, before inference runs:

```tinct
--- stage: type
[or: [fn [...types] [kind: "union"  members: types]]]
---
x@[or Int Null]   # or called pre-inference; resolves to Type::Union([Int, Null])
```

**User-defined `[class ...]` declarations** always produce `ClassDecl` with `determines: vec![]` — there is no syntax to declare functional dependencies on a user-defined class. The arithmetic FDs are hardcoded in Rust and not accessible to user code.

### What's Missing

1. Syntax for declaring functional dependencies in `[class ...]` bodies
2. A `ClassDecl.fundeps` field to store declared FDs
3. A `resolver:` link connecting a class's FD to a type-stage function that computes the determined type
4. A generalized `improve_functional_dependency` that calls a type-stage resolver instead of a hardcoded lookup table
5. Type ↔ type dict conversion to call type-stage functions during inference (inference runs on `Type::*`; type-stage runs on type dicts)
6. Depth-limited normalization (step limit analogous to GHC's `-freduction-depth`; declaration-time termination checking is not required — the step limit is the only guard)
7. BAS-aware improvement deferral (don't fire FD improvement when determining positions are union/intersection types)

## Why CHR Unification Matters for tinct

**User-defined arithmetic extensions.** A custom `Decimal` type can participate in `[+ dec dec]` with a correctly inferred result type — today only the 9 hardcoded instances work.

**Type-safe config merging.** A `Merge a b c` class with FD `(a,b)→c` can express that merging two specific config schemas produces a third, with the result type inferred at call sites rather than annotated manually.

**One mental model.** Today, `@[or Int Null]` and `[$Addable a b c]` look superficially similar (both are bracket expressions in type context) but behave completely differently: one fires pre-inference, the other fires during inference. The CHR model gives both a unified name — constraint rules — that fires at the appropriate time based on what information is available.

**Generalized FD machinery.** The `lookup_arithmetic_instance` hardcoded table is replaced by calling the same type-stage function that handles explicit `@[AddResult Int Float]` annotations. The implementation machinery becomes user-visible, not Rust-internal.

## Design

### The CHR Framework

Tinct's type constraints are Constraint Handling Rules (CHRs). Two rule forms:

**Simplification rules** (type families / type-stage functions) — fire unconditionally, replacing the expression with its normal form (CHR `<=>` rule):
```
TypeStageApp("F", [T₁, T₂]) <=> type-stage-eval F(T₁, T₂)   # fires when all args are ground
```

**Propagation rules** (functional dependencies) — fire when guard becomes ground, adding an equality to the constraint store while retaining the original constraint (CHR `==>` rule):
```
Addable a b c, a ≈ T₁, b ≈ T₂ ==> c ≈ TypeStageApp("AddResult", [T₁, T₂])
```

Standard CHR notation (Frühwirth 1998): `<=>` for simplification (head replaced), `==>` for propagation (head retained, body added).

The unifying mechanism: FD propagation produces `TypeStageApp` nodes; simplification reduces them. Both rules drive the same normalization machinery.

### `Type::TypeStageApp` — Lazy Type-Stage Application

A new type variant represents an unevaluated type-stage function application:

```rust
Type::TypeStageApp {
    fn_name: String,         // "AddResult", "or", "Seq", ...
    args: Vec<Type>,         // may contain TypeVars
}
```

`TypeStageApp` appears in two situations:

**1. Explicit annotations with TypeVar arguments.** When the annotation resolver encounters `@[AddResult a b]` and `a`, `b` are TypeVars, it cannot evaluate eagerly. Instead of failing, it produces `TypeStageApp("AddResult", [TypeVar("a"), TypeVar("b")])`. This node is reduced by the normalizer when `a`, `b` become ground through subsequent unification.

**2. FD elaboration.** When `[$Addable a b c]` is registered with FD `(a,b)→c` and resolver `AddResult`, the determined variable `c` is immediately unified with `TypeStageApp("AddResult", [a, b])`. As `a`, `b` become ground, normalization reduces the `TypeStageApp` and `c` takes on a concrete type.

`TypeStageApp` nodes with all-ground arguments are always reducible — they never persist in fully-inferred types. In a type scheme, `TypeStageApp("AddResult", [TypeVar("a"), TypeVar("b")])` may appear when `a`, `b` are still generalized TypeVars; the node is reduced at each call site when `a`, `b` are instantiated.

### Normalization — The Central Mechanism

Normalization is the unified type simplification pass. It is called before every unification step, on every type stored in a scheme, on error messages, and in `ast-of` output. There is one canonical normalization function:

```rust
fn normalize(ty: Type, ctx: &NormCtxt) -> Type
```

**`NormCtxt`** carries everything normalization needs:

```rust
struct NormCtxt<'a> {
    subst: &'a Substitution,          // current substitution chain
    type_stage_env: Rc<Environment>,  // for calling resolver functions
    alias_env: &'a AliasTable,        // for expanding type aliases
    class_env: &'a ClassEnv,          // for FD lookups
    depth: usize,                     // current reduction depth (step limit analogous to GHC's -freduction-depth)
    max_depth: usize,                 // default: 256
}
```

**What `normalize` handles, in application order:**

```
normalize(ty, ctx):

  1. Substitution: apply ctx.subst to ty (follow TypeVar chains to fixpoint)

  2. TypeStageApp reduction:
     if ty = TypeStageApp(fn, args):
       args' = args.map(|a| normalize(a, ctx))
       if args'.all(is_ground) and ctx.depth < ctx.max_depth:
         type_dicts = args'.map(type_to_dict)
         result = eval(ctx.type_stage_env.get(fn), type_dicts)
         return normalize(dict_to_type(result), ctx.with_depth(ctx.depth + 1))
       else:
         return TypeStageApp(fn, args')

  3. BAS simplification:
     Union(members): remove Never; if single member, unwrap; sort for canonical form
     Intersection(members): remove Top; if single member, unwrap
     Negation(Never) → Top; Negation(Top) → Never

  4. Literal widening:
     IntLiteral(n) → Int (in non-singleton contexts)
     StringLiteral(s) → Str
     FloatLiteral(f) → Float

  5. Type alias expansion:
     TypeAlias(name, args) → normalize(expand_alias(name, args, ctx.alias_env), ctx)
     (rational tree detection: if expansion produces the same alias, emit Type::Recursive node)

  6. Recursive normalization: normalize all child types
```

Normalization is idempotent with respect to a fixed `NormCtxt`: `normalize(normalize(ty, ctx), ctx) = normalize(ty, ctx)` provided `ctx` (including `subst` and depth budget) is unchanged between calls. Across unification steps where `subst` grows, re-normalization is necessary and correct — this is why normalization is called before every `unify` step rather than once. It terminates because: TypeStageApp reduction is depth-limited; alias expansion uses rational tree detection; BAS simplification strictly reduces the type structure.

**Cache invariant:** cache entries are only written when `args'.all(is_ground)` is true after substitution application. Deferred cases (non-ground args) do not write to the cache. Ground type keys are permanently stable — once `Type::Int`, always `Type::Int` — so no cache invalidation is needed when the substitution grows. Partial results for unevaluated `TypeStageApp` nodes are never cached.

**`TypeStageApp` unification rules in `unify_normalized`:**

After normalization, `unify_normalized` may still encounter irreducible `TypeStageApp` nodes (non-ground args). Four cases:

1. `unify(TypeStageApp("F", args₁), TypeStageApp("F", args₂))` — same function: unify args pairwise (congruence). Sound because `F` is functional: equal inputs imply equal outputs.
2. `unify(TypeStageApp("F", _), TypeStageApp("G", _))` where `F ≠ G` — different functions: `TypeError`. Distinct type families are "apart" (Eisenberg et al. 2014) — they cannot be assumed equal.
3. `unify(TypeStageApp("F", args), ConcreteType)` where `args` is non-ground — stuck application. The `TypeStageApp` is retained in the substitution as a deferred equality goal. When its args later become ground through other unifications, the next normalization step reduces it and the equality is resolved.
4. `unify(TypeStageApp("F", args), TypeVar(α))` — bind `α` to `TypeStageApp("F", args)` (standard TypeVar binding), subject to occurs-check: `occurs_in(α, arg)` must traverse `TypeStageApp.args`.

Case 3 is the key case: the FD elaboration `c ~ TypeStageApp("AddResult", [a, b])` puts a TypeStageApp in the substitution. When the user annotates a return type and triggers `unify(TypeStageApp("AddResult", [a, b]), SomeType)` before `a`, `b` are ground, this is a stuck equality that defers automatically via the substitution chain.

**Normalize before every unification:**

```rust
fn unify(a: Type, b: Type, subst: &mut Substitution, state: &mut InferState) -> Result<(), TypeError> {
    let norm = NormCtxt::from(subst, state);
    let a' = normalize(a, &norm);
    let b' = normalize(b, &norm);
    unify_normalized(a', b', subst, state)
}
```

This replaces the current scattered literal-widening and alias-expansion calls. All type simplification flows through one path.

**Distributes-over-union:** when `TypeStageApp(fn, args)` contains a union type at a determining position, normalization distributes the application: `TypeStageApp("AddResult", [Int | Float, Int])` normalizes by calling the resolver for each member and taking the union of results: `normalize(TypeStageApp("AddResult", [Int, Int])) | normalize(TypeStageApp("AddResult", [Float, Int]))` → `Int | Float`. This is sound when the resolver is **union-distributive**: `F(A|B) = F(A)|F(B)` for all inputs in the instance domain. For finite-domain resolvers, this is verified at class declaration time by exhaustive enumeration of all instance-type combinations — if every union of inputs maps to the corresponding union of outputs, distribution is sound. For open-domain resolvers, distribution is not assumed and the conservative deferral applies.

### FD Elaboration into Equality Goals

When a `[$Addable a b c]` constraint is registered with FD `(a,b)→c` and resolver `AddResult`:

1. The constraint `Constraint::Class { class: "Addable", vars: ["a", "b", "c"] }` is added to `state.constraints` for typeclass evidence.
2. Simultaneously, `c` is unified with `TypeStageApp("AddResult", [TypeVar("a"), TypeVar("b")])`. This produces the equality goal `c ~ TypeStageApp("AddResult", [a, b])` handled by normal unification.
3. The normalization pass immediately attempts to reduce `TypeStageApp("AddResult", [a, b])`. If `a`, `b` are TypeVars (not yet ground), normalization returns `TypeStageApp("AddResult", [a, b])` unchanged — the node is stored and reduction deferred.
4. As inference proceeds and `a`, `b` become ground through other unifications, the substitution chains resolve them. The next call to `normalize` on any type containing `c` will reduce `TypeStageApp("AddResult", [Int, Float])` → `Float`, propagating `c = Float` throughout the inferred type.

This is the **GHC flattening** approach: FD constraints are elaborated into type equality goals involving `TypeStageApp` nodes. The FD improvement loop becomes the normalization pass, fired on every unification step. No separate "improvement phase" is needed — normalization handles it continuously.

**Level management:** when `c` is unified with `TypeStageApp("AddResult", [a, b])` at constraint-registration time — before any subsequent binding of `c` — its level must be lowered to `max(enclosing_level, max(ℓ_a, ℓ_b))`. This prevents `c` from escaping into a scope where either `a` or `b` is not visible. The enclosing_level bound prevents independent generalization; the max-of-arg-levels bound prevents `c` from being generalized beyond the scope of its determining TypeVars. The Jones (1995) scheme `∀a b c. Add a b c ⇒ a → b → c` is represented as `∀a b. a → b → TypeStageApp("AddResult", [a, b])` — type-family semantics rather than FD semantics. The constraint `Add a b c` is kept for typeclass evidence (dictionary passing at runtime) but `c` in the type signature is the `TypeStageApp`, not an independent TypeVar.

### Annotation Resolution with TypeStageApp

The annotation resolver handles `@[TypeStageFn args...]` at annotation sites:

```tinct
# Explicit annotation, ground args: evaluate eagerly
result@[AddResult Int Float]
# → resolver evaluates AddResult(Int, Float) → Type::Float immediately (no TypeStageApp)

# Explicit annotation, TypeVar args: produce TypeStageApp
f: [fn@[bind: [a]  return: [AddResult a Int]]  [x@a] ...]
# → return type = TypeStageApp("AddResult", [TypeVar("a"), Int])
# → when f is called with x@Int, normalization fires: AddResult(Int, Int) → Type::Int

# FD-driven (no annotation): FD elaboration produces TypeStageApp
[fn [x@Int y@Float] [+ x y]]
# → [$Addable a b c] with a=Int, b=Float → c unified with TypeStageApp("AddResult", [Int, Float])
# → normalization fires immediately → c = Float
```

**Structural combinators (`or`, `each`, `without`) are always eager** — they take type dicts (never TypeVars in type-context) and reduce immediately. The annotation resolver calls them eagerly as before. `TypeStageApp` is produced only for named type-family functions (uppercase by convention) whose arguments may contain TypeVars.

The unifying insight: both rules call the same type-stage function. The only difference is *when* they fire.

### Class Declaration with FDs and Resolver

Instance declarations use **match-arm syntax**: the type-parameter list is the arm pattern, followed by `:` and the method dict. Multiple arms for different instance heads can be bundled under a single `[instance ClassName ...]` form.

For readability with complex method bodies, the recommended style is to name the implementation functions first and reference them by name in the instance arms:

```tinct
int-add: [fn@Int [x@Int y@Int] [builtin-add x y]]
[instance Addable
  [Int Int Int]: [+: int-add]]
```

A complete class + instance declaration:

```tinct
--- stage: type
[
  # Resolver function: type-stage Env (transient — needed before inference, discarded at runtime)
  AddResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Int"]]:   [kind: "named" name: "Int"]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Int"]]:   [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      _:                                                               [kind: "named" name: "Number"]]]
]
---
# Class declaration: ClassEnv (persistent — needed at type-check time AND runtime for dispatch)
# [class ...] routes to ClassEnv by its declaration form, no --- stage: marker needed
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# Instance declarations: InstanceEnv + runtime method implementations
# Each arm is a type-parameter pattern: method-dict pair.
# Multiple instances of the same class can be bundled in one [instance ...] form.
[instance Addable
  [Int Int Int]:   [+: [fn@Int   [x@Int   y@Int]   [builtin-add x y]]]
  [Int Float Float]: [+: [fn@Float [x@Int   y@Float] [builtin-add x y]]]]
```

### `kinds:` — Explicit Kind Declarations

Kind constraints declare that a TypeVar has a specific kind (`Operator` for `* → *`, `Label` for field labels, etc.). Currently this is expressed via annotation: `f@Operator` in a class param list. This is the only environment populated implicitly rather than by an explicit `[keyword ...]` declaration form.

`kinds:` makes kind constraints explicit and symmetric with `constraint:`:

```tinct
# constraint: maps TypeVar names to class constraints
# kinds:     maps TypeVar names to kind constraints (same structure, different level)

Functor: [class [f]  [kinds: [f: Operator]]                # f is of kind * → * (type constructor)
  constraint: [f: Functor]       # f must satisfy the Functor class
  fmap: [fn@[f b] [[f a]]]]

# In function annotations — both keys appear in the same bracket
fmap-generic: [fn@[bind: [a b f]
                   kinds: [f: Operator]
                   constraint: [f: Functor]
                   return: [f b]]
  [fn@b [a]  xs@[f a]]
  [fmap fn xs]]
```

The parallel structure:

| Key | Maps | Value shape | Populates |
|-----|------|-------------|-----------|
| `bind:` | TypeVar names → fresh TypeVars | `[a b f]` positional | `ann_mapping` |
| `constraint:` | TypeVar names → class constraints | `[a: Comparable]` keyed | `state.constraints` |
| `kinds:` | TypeVar names → kind constraints | `[f: Operator]` keyed | `kind_env` |
| `superclasses:` | Superclass names | `[Equatable  Showable]` | `ClassDecl.superclasses` |
| `determines:` | FD structure | `[[[a b] c]]` pair-list | `ClassDecl.determines` |

**Processing order** within a metadata bracket: `bind:` first, then `kinds:` (kind constraints on declared TypeVars), then `constraint:`, then `return:`/`type:`, then runtime keys.

The existing `f@Operator` annotation form — in class param lists and standalone annotations — is retired in favour of `kinds:`. The annotation resolver recognises `kinds:` in `[class ...]` bodies and `fn@[...]` brackets and routes the entries to `kind_env`.

**Complete example:**

```tinct
--- stage: type
[
  FmapResult: [fn [...args] ...]]   # resolver for fmap result type
]
---
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

[instance Functor
  [Seq]:   [fmap: [fn@[return: [Seq b]] [g@[Fn@b [a]]  xs@[Seq a]]
                  [map g xs]]]
  [Maybe]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]]
                  [match m
                    [Some v]: [Some [g v]]
                    None:     None]]]]
```

### Method-Level TypeVars

A method signature may introduce TypeVar names not in the class's param list. These are **method-level universals** — implicitly universally quantified at the method level, not instance dispatch indices.

```tinct
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]
#              ^                               ^  a, b = method-level universals
```

Only `f` is the class param. `a` and `b` are fresh at each call site and invisible to instance selection. `instance Functor [Seq]: [...]` covers `Seq Int`, `Seq String`, and all parameterizations — no separate instance per element type.

When a method-level TypeVar needs a kind or class constraint, the method's own `fn@[bind: ...]` carries it:

```tinct
Traversable: [class [t]  [kinds: [t: Operator]  superclasses: [Functor  Foldable]]
  traverse: [fn@[bind: [f]  kinds: [f: Operator]  constraint: [f: Applicative]
                 return: [f [t b]]]
             [g@[Fn@[return: [f b]] [a]]  xs@[t a]]]]
```

**Rule:** Any lowercase name in a method signature not in the class's param list is a method-level universal. Purely type-inferred universals (`a`, `b` above) need no declaration. Those needing constraints use the method's own `fn@[bind: ... kinds: ... constraint: ...]`.

### `superclasses:` — Class Hierarchy

The `superclasses:` key in the structural metadata bracket declares that one class extends another. It specifies that any type satisfying the subclass constraint also satisfies the superclass constraint, and that any subclass instance requires a corresponding superclass instance to exist.

```tinct
Comparable: [class [a]  [superclasses: [Equatable]]
  lt?: [fn@Bool [a a]]]

Monad: [class [m]  [kinds: [m: Operator]  superclasses: [Applicative]]
  bind: [fn@[return: [m b]] [ma@[m a]  k@[Fn@[return: [m b]] [a]]]]]
```

**Semantics:**

- **Constraint entailment**: `Comparable a` in the constraint context implies `Equatable a`. Functions constrained by `[a: Comparable]` can call `eq?` from `Equatable` without an additional `[a: Equatable]` constraint. The `entails()` function in `src/type_unify.rs` already implements transitive superclass lookup for constraint simplification.

- **Instance requirement**: declaring `[instance Comparable [Int]: [...]]` requires that `[instance Equatable [Int]: [...]]` already exists. The instance checker verifies superclass instances are present at declaration time.

- **Dictionary passing**: at runtime, the instance dictionary for a subclass includes access to superclass method implementations via the superclass instance lookup.

**Syntax:** `superclasses:` takes a list of class names. The type parameters in the superclass relationship are the same as the subclass's own params in declaration order. For `Comparable [a]` with `superclasses: [Equatable]`, the entailment is `Comparable a => Equatable a`. For `Monad [m]` with `superclasses: [Applicative]`, the entailment is `Monad m => Applicative m`.

**Multiple superclasses:**

```tinct
Traversable: [class [t]  [kinds: [t: Operator]  superclasses: [Functor  Foldable]]
  traverse: [fn@[bind: [f]  kinds: [f: Operator]  constraint: [f: Applicative]
                 return: [f [t b]]]
             [g@[Fn@[return: [f b]] [a]]  xs@[t a]]]]
```

`superclasses: [Functor  Foldable]` — both are superclasses; both constraints are entailed; instances of both must exist before declaring a `Traversable` instance.

**Implementation:** `ClassDecl.superclasses: Vec<(String, Vec<String>)>` stores `(class_name, param_list)` pairs. The param list maps the subclass's own params to the superclass params by **positional correspondence** — the first subclass param maps to the first superclass param, the second to the second, and so on. For `Monad [m]` with `superclasses: [Applicative]`, the entry is `("Applicative", vec!["m"])` — m maps to Applicative's single param. For a hypothetical `BiClass [a b]` with `superclasses: [PairClass]`, the entry is `("PairClass", vec!["a", "b"])`. The `entails()` function traverses the superclass chain transitively, substituting actual type arguments from the subclass constraint into the superclass params using the positional mapping.

### Why No Coercion System

GHC requires coercions — proof terms witnessing type reductions — because GHC compiles to a typed intermediate language (Core/System FC) where every type reduction must be witnessed by a coercion for the optimizer to perform sound transformations. Tinct is interpreted (CEK machine); type information is used during inference and erased before evaluation. There is no typed IR, no optimizer, and no `unsafeCoerce`. Tinct's type system is advisory — type errors are diagnostics, not compilation failures preventing code generation. For these reasons, a coercion system is unnecessary and is not part of this design.

### Multi-Output FD Resolver Convention

For a class with multiple determined variables — `DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]]` — the resolver function receives the determining type dicts and must return a **record type dict** with the determined vars as named fields:

```tinct
DivModResult: [fn [...args]
  [match [[builtin-get 0 args]  [builtin-get 1 args]]
    [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
      [q: [kind: "named" name: "Int"]  r: [kind: "named" name: "Int"]]
    ...]]
```

The `dict_to_type` conversion, when it sees a dict without a `kind:` key, interprets it as a multi-output resolver result and destructures the fields by name, mapping `q → Type::Int` and `r → Type::Int`. The `determines:` list `[[q r]]` provides the field names and their ordering. Each determined TypeVar is unified with the corresponding field from the resolver's output record.

### Class Body Structure — Two-Bracket Form

The class body separates **structural metadata** from **method declarations** using two positional elements:

```tinct
Name: [class [type-params]  [structural-metadata]  method-signatures...]
        ^^^^^^^^^^^^^^^^^^^^^^ ^^^^^^^^^^^^^^^^^^^^^^^ ^^^^^^^^^^^^^^^^^^
        1st positional:         2nd positional:          keyed entries:
        type param names        determines/resolver/kinds  method sigs
```

The second positional bracket is **optional** — omit it for classes with no FDs or kind constraints:

```tinct
# With structural metadata
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# Without — just params and methods
Equatable: [class [a]
  eq?: [fn@Bool [a a]]]
```

This eliminates reserved-word conflicts: `determines`, `resolver`, and `kinds` are only structural when they appear as keys in the second positional bracket. Any method name (including `determines` or `resolver`) is valid as a keyed entry after the structural bracket.

**The `determines:` key** takes a list of dependency specifications inside the structural bracket. Each entry is a **two-element list** `[[determining-vars] determined]`:

```tinct
Addable: [class [a b c]    [determines: [[[a b] c]]       resolver: AddResult]              ...]
Convert: [class [a b]      [determines: [[[a] b]]          resolver: ToStringResult]         ...]
DivMod:  [class [a b q r]  [determines: [[[a b] [q r]]]   resolver: DivModResult]           ...]
```

Multiple FDs in the structural bracket require one resolver per FD, in the same order as the `determines:` list:
```tinct
BiConvert: [class [a b]  [determines: [[[a] b]  [[b] a]]  resolver: [AtoB  BtoA]]  ...]
```

When there is exactly one FD, `resolver:` takes a single name. When there are N FDs, `resolver:` takes a list of N names.

`kinds:` also goes in the structural bracket:
```tinct
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[f b] [[f a]]]]

FunctorWithFD: [class [f]  [kinds: [f: Operator]  determines: [[[f a] b]]  resolver: FmapR]
  fmap: [fn@[f b] [[f a]]]]
```

The parser distinguishes the structural bracket from method declarations unambiguously: a `[...]` token after the param list is the structural bracket (positional); `key: value` entries that follow are methods (keyed). No lookahead into the value needed.

### Instance Syntax — Match-Arm Form

Instance declarations use a **match-arm syntax** that pairs each type-parameter pattern with its method dictionary:

```tinct
[instance ClassName
  [Type1 Type2 ...]: [method-key: implementation ...]
  [Type3 Type4 ...]: [method-key: implementation ...]]
```

The type-parameter list `[Type1 Type2 ...]` is the arm pattern. After `:`, the method dict `[method-key: impl ...]` supplies the implementations for that particular type combination. Multiple instances of the same class are bundled as additional arms under the same `[instance ClassName ...]` form.

**Parsing rule for type-pattern brackets:** Instance arm brackets are parsed as a **flat positional list** of N type expressions, one per class parameter. This is NOT implied-call syntax — the outer bracket is never interpreted as a function call regardless of what name appears first. Each whitespace-delimited element is one type expression:

- A bare uppercase name is a type constant: `Int`, `Float`, `Seq`, `Maybe`
- A bare lowercase name is a type variable pattern: `a`, `t`
- An inner bracket is a composite type: `[Seq Int]`, `[or Int Null]`, `[record head: Int]`

```tinct
# Addable [a b c] — 3 class params:
[instance Addable
  [Int Int Int]:             [+: ...]   # three bare names — three types
  [Int Float Float]:         [+: ...]
  [Int Int [or Int Null]]:   [+: ...]]  # third element is a composite type

# Functor [f] — 1 class param:
[instance Functor
  [Seq]:   [fmap: ...]   # one bare name — one type
  [Maybe]: [fmap: ...]]

# An arm matching Seq applied to Int — use an inner bracket:
# [instance SomeClass [[Seq Int]]: [...]]  ← [[Seq Int]] is one type expression
```

The arity check (exactly N elements for a class with N params) is performed at the semantic layer, not during parsing. The parser produces a flat list of whatever type expressions it finds; the instance checker validates the count.

For readability, complex method bodies should be named and extracted before the instance declaration:

```tinct
int-add: [fn@Int [x@Int y@Int] [builtin-add x y]]
[instance Addable
  [Int Int Int]: [+: int-add]]
```

Instances are anonymous — they are never referenced by name. The instance dict is selected automatically by the constraint solver at every call site and passed as an implicit argument at runtime.

### Resolver Linking

The `resolver:` key names the type-stage function(s) used by the normalization pass when reducing `TypeStageApp(class_name, ground_args)`. For a class with one FD, `resolver:` takes a single name. For a class with N FDs, `resolver:` takes a list of N names in the same order as the `determines:` list — the Kth resolver is called when the Kth FD's determining positions become ground.

Each resolver is called via:
1. Convert `Type::*` to type dicts: `Type::Int → [kind: "named" name: "Int"]`
2. Look up the resolver by name in the type-stage Env (carried by `NormCtxt`)
3. Call `eval(resolver_fn, type_dicts, type_stage_env)` — returns a type dict for the determined position(s)
4. Convert result back to `Type::*` via the type dict → Type mapping

**Normalization cache:** `NormCtxt` carries a `HashMap<(String, Vec<TypeKey>), Type>` keyed on `(fn_name, type_keys_of_args)`. Arithmetic class results are pre-populated from the existing `lookup_arithmetic_instance` table (O(1) cache hits). User-declared classes warm the cache on first reduction.

**`NormCtxt` and `InferState`:** `InferState` carries `norm_ctxt: NormCtxt` which includes `type_stage_env: Rc<Environment>`. This creates a `types.rs → value.rs` dependency — an intentional architectural decision, consistent with GHC's unified `TcM` monad where type checking, type family reduction, and constraint solving share the same context.

### Resolver Soundness Obligations

Calling `eval(resolver_fn, ...)` during unification requires the resolver to satisfy four obligations:

1. **Totality**: the resolver must terminate. If a user writes a divergent resolver (e.g., mutual recursion in type-stage), normalization diverges. The depth counter in `NormCtxt` (default 256) limits reduction depth. When `ctx.depth >= ctx.max_depth`, normalization returns the unreduced `TypeStageApp` unchanged — it does NOT produce a type error inline. Instead, if an unreduced `TypeStageApp` node reaches a position where a concrete type is required (e.g., as a function return type at a call site that has ground args), the unification at that point produces a `TypeError` with a "type-stage reduction depth exceeded" note.

2. **Determinism**: the resolver must return the same output for the same inputs across all call sites. This is guaranteed by the purity of type-stage functions — they are evaluated in an isolated environment with no mutable state or I/O. Builtins that would break this (e.g., `$include`, `$emit`) are not in scope in `--- stage: type` sections.

3. **Well-formedness**: if the resolver returns a malformed type dict (missing `kind:` key, unrecognized `kind:` value, structurally invalid), the type-dict-to-`Type::*` conversion produces a `TypeError` with a "resolver returned invalid type dict" message — not a crash.

4. **Confluence**: Sulzmann et al. (2007, Theorem 4.2) prove that CHR improvement is confluent under the consistency condition. FD improvement is deterministic when instances satisfy coverage and consistency (see §Instance Soundness Conditions below). Non-coherent resolvers (where improvement could derive conflicting bindings) are caught by unification conflict — a standard `TypeError` fires, not silent unsoundness.

### Instance Soundness Conditions

At instance declaration time, every instance arm for a class must satisfy three conditions:

- **Disjointness condition**: no two arms of the same class may match the same ground type tuple. If two arms' type-parameter lists can be simultaneously unified under some substitution θ (i.e., they overlap), the instance declaration is rejected at declaration time with a "overlapping instance arms" error. Instance dispatch is therefore **always unambiguous** — for any ground type tuple at most one arm matches. There is no first-match semantics and no ordering among arms.

  Examples: `[Int Int Int]` and `[Int Float Float]` are disjoint (no θ unifies them). `[Int a b]` and `[Int Int c]` overlap (θ = {a=Int}) and are rejected. `[Int a b]` and `[Float a b]` are disjoint.

- **Coverage condition** (for classes with FDs): for each FD `(d) → (r)`, every type variable appearing in the determined positions of the instance arm must also appear in the determining positions. This prevents improvement from introducing fresh unknowns that cannot be resolved. Example: arm `[a b b]` with FD `(a,b)→c` — `c` binds to `b`, which appears in the determining positions, so coverage holds.

- **Consistency condition** (for classes with FDs, Jones 2000, Definitions 7–8): if two arms' determining positions unify under some substitution θ, their determined positions must also unify under θ. This guarantees improvement is confluent. Example: arms `[Int Int Int]` and `[Int Int Float]` violate consistency — both have determining positions `(Int, Int)` but different determined types `Int` vs `Float`. This is rejected at declaration time.

  Note: the consistency condition is independent of the disjointness condition. Two arms can be disjoint on their determining positions but still require consistency checking when the FD spec allows the determining positions to overlap in theory. The disjointness condition applies to the **full** type-parameter list; consistency applies to the **determining** positions only.

**Acyclic dependency graphs**: instances with cyclic determination (A determines B, B determines A) require a confluence check beyond pairwise consistency. The `determines:` syntax allows bidirectional specs (as shown above) but the implementation validates the critical-pair condition (Sulzmann et al. 2007) at class declaration time and rejects cycles that cannot be proven confluent.

Instances violating any of these conditions are rejected with a type error at instance declaration time.

### BAS-Aware Improvement Deferral

Under BAS, a TypeVar bound during unification may resolve to a union type (`Int | Float`) rather than a ground monotype. FD improvement must not fire when any determining position contains a union, intersection, or negation type — only atomic named types trigger improvement.

```
improve_functional_dependency conditions:
  ∀ position p ∈ determining(FD):
    type[p] is Type::Int | Type::Float | Type::Bool | Type::Str | ...  (named primitive)
    NOT Type::Union(...) | Type::Intersection(...) | Type::Negation(...) | Type::TypeVar(_)
```

This is the conservative, sound approach. Distribution over union types (e.g., `Add (Int|Float) Int c ⟹ c = Int|Float`) would require proving the resolver function is covariant on the subtype lattice — a proof obligation deferred to future work.

**Future extension — `distributes-over-union:`**: for finite-domain resolvers (like arithmetic), distribution is verifiable by exhaustive case analysis. A future `distributes-over-union: true` flag on a class declaration permits the checker to distribute improvement over union members, eliminating deferral in those cases. For open-domain resolvers, the conservative deferral remains correct.

### Generalization with FD Constraints

TypeVars in determined positions (`c` in `[$Addable a b c]`) are generalized normally under the Jones (1995) qualified types model. The FD constraint is propagated as part of the type scheme and fires at each call site when the determining positions are instantiated to ground types.

Consider `[fn [x y] [+ x y]]` — the principal type is `∀a b c. Add a b c ⇒ a → b → c`. Here `c` IS included in `type_vars`. The constraint `Add a b c` in the scheme is not a free universal quantification over `c` — it is a qualified type where `c` is determined by `(a, b)`. At a call site `[f 1 2.0]`, instantiation creates fresh copies `a=Int, b=Float, c=?`; FD improvement fires immediately and resolves `c=Float`. This is the standard Haskell treatment and is sound (Jones 2000).

Two cases at let-generalization:

1. **FD has already fired** — `c` is unified with a concrete type and does not appear free in the scheme. Generalization produces `∀a b. Add a b Float ⇒ a → b → Float` (with concrete `c`).
2. **FD has not fired** (determining positions are still free TypeVars) — `c` remains a TypeVar and is included in the generalized scheme with the constraint: `∀a b c. Add a b c ⇒ a → b → c`. FD fires at each call site.

**Level management**: when a `[$Class a b c]` constraint is registered with FD `(a,b)→c`, the determined TypeVar `c`'s level must be lowered to `enclosing_level` at constraint-creation time. This ensures `c` cannot escape into an outer scope independently of the constraint — it can only be used where the constraint is visible.

## Worked Examples

### A: Defining and Using a Single-Output MPTC

```tinct
--- stage: type
[
  # Trivial: resolver for a 2-param class
  AddResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Int"]]:   [kind: "named" name: "Int"]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Int"]]:   [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      _:                                                               [kind: "named" name: "Number"]]]
]
---
# Trivial: class + instances
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# Instances bundled in one form; each arm is a type-pattern: method-dict pair.
# For readability, name the implementation functions first:
int-int-add:   [fn@Int   [x@Int   y@Int]   [builtin-add x y]]
int-float-add: [fn@Float [x@Int   y@Float] [builtin-add x y]]

[instance Addable
  [Int Int Int]:     [+: int-int-add]
  [Int Float Float]: [+: int-float-add]]

# Instances are anonymous — they register in the InstanceEnv and are selected
# automatically by the constraint solver at every call site. The instance dictionary
# is passed as an implicit argument at runtime.

# Trivial: FD-driven inference — result type inferred without annotation
[+ 1 2]          # → Int   (Add[Int,Int,Int] instance selected, FD fires: c = Int)
[+ 1 2.0]        # → Float (Add[Int,Float,Float] instance selected, FD fires: c = Float)
[+ 1.0 2.0]      # → Float (Add[Float,Float,Float] instance selected, FD fires: c = Float)

# Nontrivial: polymorphic function — FD fires at each call site
# $Multipliable is the Multipliable class (declared analogously to Addable, with resolver MulResult)
scale: [fn@[bind: [a b c]  return: c  constraint: [a: Numeric  b: Numeric  [$Multipliable a b c]]]
  [x@a  factor@b]
  [* x factor]]

[scale 10 2]      # c = Int   (Mul Int Int Int — FD fires)
[scale 10 2.5]    # c = Float (Mul Int Float Float — FD fires)
[scale 1.5 2]     # c = Float (Mul Float Int Float — FD fires)
```

### B: Explicit TypeStageApp Annotations with TypeVar Arguments

```tinct
--- stage: type
[
  AddResult: [fn [...args] ...]   # as above
]
---
# Trivial: ground args — evaluated eagerly, no TypeStageApp produced
result@[AddResult Int Float]     # resolves immediately to Type::Float

# Trivial: TypeVar arg — produces TypeStageApp, reduced at call site
promote: [fn@[bind: [a]  return: [AddResult a Float]]  [x@a] ...]
[promote 1]      # a=Int → AddResult(Int, Float) normalizes → Float
[promote 1.5]    # a=Float → AddResult(Float, Float) normalizes → Float

# Nontrivial: TypeStageApp in scheme body, used in multiple positions
wrap-pair: [fn@[bind: [a b]  return: [record first: [AddResult a b]  second: [AddResult b a]]]
  [x@a  y@b]
  [first: [+ x y]  second: [+ y x]]]

[wrap-pair 1 2.0]
# a=Int, b=Float
# return: {first: AddResult(Int,Float)=Float, second: AddResult(Float,Int)=Float}
# → {first: Float, second: Float}

[wrap-pair 1 2]
# a=Int, b=Int
# return: {first: AddResult(Int,Int)=Int, second: AddResult(Int,Int)=Int}
# → {first: Int, second: Int}
```

### C: Bidirectional FDs

```tinct
--- stage: type
[
  ToStringResult: [fn [...args]
    [match [builtin-get 0 args]
      [kind: "named" name: "Int"]:   [kind: "named" name: "String"]
      [kind: "named" name: "Float"]: [kind: "named" name: "String"]
      [kind: "named" name: "Bool"]:  [kind: "named" name: "String"]
      _:                             [kind: "named" name: "Unknown"]]]
  FromStringResult: [fn [...args]
    [match [builtin-get 0 args]
      [kind: "named" name: "String"]: [kind: "named" name: "Unknown"]  # open-ended
      _:                              [kind: "named" name: "Unknown"]]]
]
---
# Trivial: bidirectional — a determines b AND b determines a
# Two FDs → two resolvers, one per FD in determines: order
Convert: [class [a b]  [determines: [[[a] b]  [[b] a]]  resolver: [ToStringResult  FromStringResult]]
  show: [fn@b [a]]
  parse: [fn@a [b]]]

[instance Convert
  [Int String]: [show: [fn@String [x@Int] [int-to-str x]]
                 parse: [fn@Int [s@String] [str-to-int s]]]]

# Trivial: forward inference
stringify: [fn@[bind: [a b]  return: b  constraint: [$Convert a b]]  [x@a]  [show x]]
[stringify 42]       # a=Int, b=String → "42"

# Nontrivial: roundtrip function, both directions must typecheck
roundtrip: [fn@[bind: [a b]  return: a  constraint: [$Convert a b]]
  [x@a]
  [parse [show x]]]  # show: a→b, parse: b→a; return type is a

[roundtrip 42]       # a=Int, b=String → parse(show(42)) = parse("42") = 42
```

### D: Multi-Output FDs

```tinct
--- stage: type
[
  DivModResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
        [quotient: [kind: "named" name: "Int"]
         remainder: [kind: "named" name: "Int"]]]]
]
---
# Trivial: multi-output — (a,b) jointly determines (q, r)
DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]
  divmod: [fn@[record quotient: q  remainder: r] [a b]]]

[instance DivisibleMod
  [Int Int Int Int]: [divmod: [fn [x@Int y@Int]
                        [quotient: [/ x y]  remainder: [% x y]]]]]

# Trivial: both q and r inferred simultaneously
[divmod 17 5]    # q=Int, r=Int → {quotient: 3, remainder: 2}

# Nontrivial: using both outputs in a function
euclidean-gcd: [fn@[bind: [a]  return: a  constraint: [a: Numeric  [$DivMod a a a a]]]
  [x@a  y@a]
  [if [= y 0]
    x
    [result: [divmod x y]]
    [euclidean-gcd y result.remainder]]]

[euclidean-gcd 48 18]    # → 6
```

### E: Union Types in Determining Positions (distributes-over-union)

```tinct
# Trivial: union type in determining position
# With distributes-over-union declared on Add class:
[+ x@[or Int Float] y@Int]
# distributes: Add(Int|Float, Int, c) →
#   Add(Int, Int, Int) | Add(Float, Int, Float) → c = Int | Float

# Without distributes-over-union (conservative): FD deferred
# c remains TypeStageApp("AddResult", [Int|Float, Int]) until union resolves

# Nontrivial: config file with mixed numeric types
config: [
  timeout: 30           # Int
  scale: 1.5            # Float
]

compute-scaled-timeout: [fn [cfg]
  [+ cfg.timeout cfg.scale]]
# timeout: Int, scale: Float → result: Float (via Add Int Float Float FD)
```

### F: User-Defined Type Class Without Arithmetic

```tinct
--- stage: type
[
  MergeResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "record" fields: [host: _  port: _]]
       [kind: "record" fields: [timeout: _  retries: _]]]:
        [kind: "record"  fields: [host:    [kind: "named" name: "String"]
                                  port:    [kind: "named" name: "Int"]
                                  timeout: [kind: "named" name: "Int"]
                                  retries: [kind: "named" name: "Int"]]]
      _: [kind: "named" name: "Unknown"]]]
]
---
# Trivial: non-arithmetic MPTC — merging two record types
Merge: [class [a b c]  [determines: [[[a b] c]]  resolver: MergeResult]
  merge: [fn@c [a b]]]

ServerBase: [type [host: String  port: Int]]
ServerOpts: [type [timeout: Int  retries: Int]]
ServerFull: [type [host: String  port: Int  timeout: Int  retries: Int]]

[instance Merge
  [ServerBase ServerOpts ServerFull]: [merge: [fn [base@ServerBase opts@ServerOpts]
                                         [host: base.host  port: base.port
                                          timeout: opts.timeout  retries: opts.retries]]]]

# Trivial: FD inference determines result type
base: [host: "api.example.com"  port: 443]
opts: [timeout: 30  retries: 3]
[merge base opts]   # → ServerFull (inferred via FD)

# Nontrivial: generic merge pipeline
apply-defaults: [fn@[bind: [a b c]  return: c  constraint: [$Merge a b c]]
  [config@a  defaults@b]
  [merge defaults config]]   # defaults merged first, then overridden by config

[apply-defaults
  [port: 8080]                              # partial config (a)
  [host: "localhost"  port: 80  timeout: 5  retries: 1]  # full defaults (b)
]
# → {host: "localhost"  port: 8080  timeout: 5  retries: 1}   (c = ServerFull)
```

### G: Interaction with Pattern Matching and Nominal Variants

```tinct
--- stage: type
[AddResult: [fn [...args] ...]]   # as above
---
# Trivial: MPTC result type used in pattern matching
Add: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]  +: [fn@c [a b]]]

safe-add: [fn@[bind: [a b c]  return: [or c Null]  constraint: [a: Numeric  b: Numeric  [$Addable a b c]]]
  [x@a  y@b]
  [if [> [+ x y] 1000000]
    []        # Null — overflow guard
    [+ x y]]] # c — the inferred result type

# result: or(AddResult(Int,Int), Null) = or(Int, Null)
[match [safe-add 100 200]
  x@[is: int?]: [str "sum: " x]
  _:            "overflow"]

# Nontrivial: nominal variant wrapping MPTC result
Result: [type [Ok a] [Err String]]

safe-divide: [fn@[bind: [a b c]  return: [or [Ok c] [Err String]]
                    constraint: [a: Numeric  b: Numeric  [$Addable a b c]]]
  [x@a  y@b]
  [if [= y 0]
    [Err "division by zero"]
    [Ok [/ x y]]]]   # return type: Ok(c) where c = AddResult(a,b)

[match [safe-divide 10 3]
  [Ok result]: [str "result: " result]   # result: Float
  [Err msg]:   [str "error: " msg]]
```

### H: Interaction with `--- stage: type` Type Prelude

```tinct
--- stage: type
[
  # AddResult resolver composes with existing type prelude combinators
  AddResult: [fn [...args] ...]

  # A resolver can call other type-stage functions
  NullableAddResult: [fn [...args]
    [or [AddResult [builtin-get 0 args] [builtin-get 1 args]]
        [kind: "named" name: "Null"]]]
]
---
# Trivial: MPTC resolver uses type prelude or/Seq
SafeAdd: [class [a b c]  [determines: [[[a b] c]]  resolver: NullableAddResult]   # c = AddResult(a,b) | Null
  +?: [fn@c [a b]]]

[instance SafeAdd
  [Int Int [or Int Null]]: [+?: [fn@[or Int Null] [x@Int y@Int]
                              [if [> [+ x y] 1000000] [] [+ x y]]]]]

result@[SafeAdd Int Int]   # → or(Int, Null)

# Nontrivial: MPTC constraining Seq element types
--- stage: type
[
  ZipResult: [fn [...args]
    [kind: "seq"
     element: [kind: "record"
               fields: [left: [builtin-get 0 args]
                        right: [builtin-get 1 args]]]]]
]
---
Zip: [class [a b c]  [determines: [[[a b] c]]  resolver: ZipResult]
  zip: [fn@[Seq c] [[Seq a] [Seq b]]]]

[instance Zip
  [Int String [record left: Int  right: String]]:
    [zip: [fn [xs@[Seq Int]  ys@[Seq String]]
            [map [fn [pair] [left: [first pair]  right: [second pair]]]
                 [zip-lists xs ys]]]]]

[zip [1 2 3] ["a" "b" "c"]]
# c = {left: Int, right: String} (inferred via FD)
# → [{left:1 right:"a"} {left:2 right:"b"} {left:3 right:"c"}]
```

### I: Interaction with BAS and the Type Checker

```tinct
--- stage: type
[AddResult: [fn [...args] ...]]
---
# Trivial: MPTC result participates in BAS subtyping
Add: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]  +: [fn@c [a b]]]

# Int | Float <: Number — BAS width subtyping
sum: [fn@[bind: [a b c]  return: c  constraint: [a: Numeric  b: Numeric  [$Addable a b c]]]
  [xs@[Seq a]  ys@[Seq b]]
  [reduce [fn [acc x] [+ acc x]]
          [reduce [fn [acc x] [+ acc x]] 0 xs]
          ys]]

[sum [1 2 3] [1.0 2.0]]
# xs: Seq[Int], ys: Seq[Float]
# inner reduce: acc=Int, x=Int → Add Int Int Int → c=Int
# outer reduce: acc=Int, x=Float → Add Int Float Float → c=Float
# → 9.0 (Float <: Number ✓)

# Nontrivial: MPTC with HasField constraint (label-polymorphic field access)
FieldAdd: [class [record key c]  [determines: [[[record key] c]]  resolver: FieldAddResult]
  field-add: [fn@c [record@record  key@key  rhs@c]]]

# Interacts with HasField: type checker generates HasField constraint from key@Label
# field-add checks that record[key] and rhs can be added via the Add class
accumulate: [fn@[bind: [r k c]  return: r
                  constraint: [[$FieldAdd r k c]]]
  [records@[Seq r]  key@k  initial@c]
  [reduce [fn [acc rec] [update acc key [+ [get key acc] [get key rec]]]]
          initial records]]
```

### J: Interaction with `ast-of` and Runtime Reflection

```tinct
--- stage: type
[AddResult: [fn [...args] ...]]
---
Add: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]  +: [fn@c [a b]]]

# Trivial: ast-of on a polymorphic function returns post-normalization types
typed-sum: [fn@[bind: [a b c]  return: c
                 constraint: [a: Numeric  b: Numeric  [$Addable a b c]]
                 doc: "Add two numeric values with type inference"]
  [x@a  y@b]
  [+ x y]]

[ast-of typed-sum]
# → [type: "fn"
#    return-ann: [kind: "type-stage-app"  fn: "AddResult"
#                 args: [[kind: "named" name: "a"] [kind: "named" name: "b"]]]
#    params: [[name: "x"  annotation: [kind: "named" name: "a"]]
#             [name: "y"  annotation: [kind: "named" name: "b"]]]
#    doc: "Add two numeric values with type inference"]
# Note: TypeStageApp in return-ann is stored as-is (TypeVars not yet ground)
# When called with concrete types, the stored type is the pre-normalization scheme form

# Nontrivial: reflecting on classes and their instances at runtime
# Classes are named — describe the class directly
[describe Add]
# → [type: "class"  name: "Add"  params: ["a" "b" "c"]
#    determines: [[[0 1] 2]]  resolver: "AddResult"
#    methods: [+: "fn@c [a b]"]]

# Enumerate all registered instances of a class
[instances-of Add]
# → [[params: ["Int" "Int" "Int"]  methods: [+: ...]]
#    [params: ["Int" "Float" "Float"]  methods: [+: ...]]
#    [params: ["Float" "Float" "Float"]  methods: [+: ...]]]

# Instances themselves are anonymous — identified by class + type params, not by name
# You cannot [describe AddIntInt] because AddIntInt doesn't exist as a binding

# Check if a value's type satisfies an Add relationship
check-addable: [fn@Bool [x y]
  [and [int? x] [float? y]]]   # runtime predicate — type system cannot check at this level
# Note: the Add class constraint is static; is: predicates provide runtime validation

[typed-sum@[is: check-addable] 1 2.0]   # runtime check: x is int?, y is float?
# → 3.0 (Float)
```

## Prelude Class Declarations

All tinct typeclass infrastructure in final form. Resolver functions in `--- stage: type`; class and instance declarations in program stage.

```tinct
--- stage: type
[
  AddResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Int"]]:   [kind: "named" name: "Int"]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Int"]]:   [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      _:                                                               [kind: "named" name: "Number"]]]

  SubResult:  [fn [...args] ...]    # same dispatch as AddResult
  MulResult:  [fn [...args] ...]    # same dispatch as AddResult
  DivResult:  [fn [...args] [kind: "named" name: "Float"]]   # division always yields Float
]
---

# ── Single-parameter classes ──────────────────────────────────────────────

Equatable: [class [a]
  eq?: [fn@Bool [a a]]]

[instance Equatable
  [Int]:   [eq?: [fn@Bool [x@Int   y@Int  ] [builtin-eq x y]]]
  [Float]: [eq?: [fn@Bool [x@Float y@Float] [builtin-eq x y]]]
  [Str]:   [eq?: [fn@Bool [x@Str   y@Str  ] [builtin-eq x y]]]
  [Bool]:  [eq?: [fn@Bool [x@Bool  y@Bool ] [builtin-eq x y]]]
  [Null]:  [eq?: [fn@Bool [x@Null  y@Null ] true]]]

Comparable: [class [a]  [superclasses: [Equatable]]
  lt?: [fn@Bool [a a]]]

[instance Comparable
  [Int]:   [lt?: [fn@Bool [x@Int   y@Int  ] [builtin-lt x y]]]
  [Float]: [lt?: [fn@Bool [x@Float y@Float] [builtin-lt x y]]]
  [Str]:   [lt?: [fn@Bool [x@Str   y@Str  ] [builtin-lt x y]]]]

Showable: [class [a]
  str: [fn@String [a]]]

[instance Showable
  [Int]:   [str: [fn@String [x@Int  ] [builtin-int-to-str x]]]
  [Float]: [str: [fn@String [x@Float] [builtin-float-to-str x]]]
  [Str]:   [str: [fn@String [x@Str  ] x]]
  [Bool]:  [str: [fn@String [x@Bool ] [if x "true" "false"]]]
  [Null]:  [str: [fn@String [_      ] "null"]]]

Numeric: [class [a]  [superclasses: [Comparable  Showable]]]
  # No additional methods — marks a type as numeric for arithmetic constraints

[instance Numeric  [Int]: []  [Float]: []]
# Number is a BAS union alias (Int | Float), not a concrete type.
# Class instances are for concrete types — Number participates via BAS
# width subtyping when Int or Float resolves at the call site.

Appendable: [class [a]
  # Monoid: concat + empty identity element (concat x empty = x = concat empty x)
  concat: [fn@a [a a]]
  empty:  [fn@a []]]

[instance Appendable
  [Str]:     [concat: [fn@Str     [x@Str     y@Str    ] [builtin-str-concat x y]]
              empty:  [fn@Str     []                     ""]]
  [Seq a]:   [concat: [fn@[Seq a] [xs@[Seq a] ys@[Seq a]] [builtin-seq-concat xs ys]]
              empty:  [fn@[Seq a] []                       []]]
  [Map k v]: [concat: [fn@[Map k v] [a@[Map k v] b@[Map k v]] [merge a b]]
              empty:  [fn@[Map k v] []                          []]]]

# ── MPTC classes with functional dependencies ─────────────────────────────

Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

[instance Addable
  [Int   Int   Int  ]: [+: [fn@Int   [x@Int   y@Int  ] [builtin-add x y]]]
  [Float Float Float]: [+: [fn@Float [x@Float y@Float] [builtin-add x y]]]
  [Int   Float Float]: [+: [fn@Float [x@Int   y@Float] [builtin-add x y]]]
  [Float Int   Float]: [+: [fn@Float [x@Float y@Int  ] [builtin-add x y]]]]

Subtractable: [class [a b c]  [determines: [[[a b] c]]  resolver: SubResult]
  -: [fn@c [a b]]]

[instance Subtractable
  [Int   Int   Int  ]: [-: [fn@Int   [x@Int   y@Int  ] [builtin-sub x y]]]
  [Float Float Float]: [-: [fn@Float [x@Float y@Float] [builtin-sub x y]]]
  [Int   Float Float]: [-: [fn@Float [x@Int   y@Float] [builtin-sub x y]]]
  [Float Int   Float]: [-: [fn@Float [x@Float y@Int  ] [builtin-sub x y]]]]

Multipliable: [class [a b c]  [determines: [[[a b] c]]  resolver: MulResult]
  *: [fn@c [a b]]]

[instance Multipliable
  [Int   Int   Int  ]: [*: [fn@Int   [x@Int   y@Int  ] [builtin-mul x y]]]
  [Float Float Float]: [*: [fn@Float [x@Float y@Float] [builtin-mul x y]]]
  [Int   Float Float]: [*: [fn@Float [x@Int   y@Float] [builtin-mul x y]]]
  [Float Int   Float]: [*: [fn@Float [x@Float y@Int  ] [builtin-mul x y]]]]

Divisible: [class [a b c]  [determines: [[[a b] c]]  resolver: DivResult]
  /: [fn@c [a b]]]

[instance Divisible
  [Int   Int   Float]: [/: [fn@Float [x@Int   y@Int  ] [builtin-div x y]]]
  [Float Float Float]: [/: [fn@Float [x@Float y@Float] [builtin-div x y]]]
  [Int   Float Float]: [/: [fn@Float [x@Int   y@Float] [builtin-div x y]]]
  [Float Int   Float]: [/: [fn@Float [x@Float y@Int  ] [builtin-div x y]]]]

# ── Higher-kinded classes (f@Operator) ────────────────────────────────────

Mappable: [class [f]  [kinds: [f: Operator]]
  map: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

[instance Mappable
  [Seq]:    [map: [fn@[return: [Seq b]]    [g@[Fn@b [a]]  xs@[Seq a]   ] [builtin-map g xs]]]
  [Dict]:   [map: [fn@[return: [Dict k b]] [g@[Fn@b [a]]  d@[Dict k a] ] [builtin-map-dict g d]]]]

Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

[instance Functor
  [Seq]:   [fmap: [fn@[return: [Seq b]]   [g@[Fn@b [a]]  xs@[Seq a]   ] [map g xs]]]
  [Maybe]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]  ]
                 [match m  [Some v]: [Some [g v]]  None: None]]]]

Applicative: [class [f]  [kinds: [f: Operator]  superclasses: [Functor]]
  pure:  [fn@[return: [f a]] [x@a]]
  lift2: [fn@[return: [f c]] [g@[Fn@c [a b]]  fa@[f a]  fb@[f b]]]]

[instance Applicative
  [Maybe]: [pure:  [fn@[return: [Maybe a]] [x@a] [Some x]]
             lift2: [fn@[return: [Maybe c]] [g@[Fn@c [a b]]  ma@[Maybe a]  mb@[Maybe b]]
                  [match [ma mb]
                    [[Some a] [Some b]]: [Some [g a b]]
                    _:                   None]]]]

Monad: [class [m]  [kinds: [m: Operator]  superclasses: [Applicative]]
  bind: [fn@[return: [m b]] [ma@[m a]  k@[Fn@[return: [m b]] [a]]]]]

[instance Monad
  [Maybe]: [bind: [fn@[return: [Maybe b]] [m@[Maybe a]  k@[Fn@[return: [Maybe b]] [a]]]
                 [match m  [Some v]: [k v]  None: None]]]]

Foldable: [class [t]  [kinds: [t: Operator]]
  fold:   [fn@b  [f@[Fn@b [b a]]  init@b  xs@[t a]]]
  to-seq: [fn@[return: [Seq a]] [xs@[t a]]]]

[instance Foldable
  [Seq]:   [fold:   [fn@b [f@[Fn@b [b a]]  init@b  xs@[Seq a]] [builtin-fold f init xs]]
             to-seq: [fn@[return: [Seq a]] [xs@[Seq a]] xs]]
  [Maybe]: [fold:   [fn@b [f@[Fn@b [b a]]  init@b  m@[Maybe a]]
                [match m  [Some v]: [f init v]  None: init]]
             to-seq: [fn@[return: [Seq a]] [m@[Maybe a]]
                [match m  [Some v]: [v]  None: []]]]]

Traversable: [class [t]  [kinds: [t: Operator]  superclasses: [Functor  Foldable]]
  traverse: [fn@[bind: [f]  kinds: [f: Operator]  constraint: [f: Applicative]
                 return: [f [t b]]]
             [g@[Fn@[return: [f b]] [a]]  xs@[t a]]]]
```

## What Would Change

### `src/types.rs` — New `Type::TypeStageApp` variant

**Current:** No lazy type-stage application node. Type-stage functions are always called eagerly at annotation resolution time.  
**Proposed:** Add `Type::TypeStageApp { fn_name: String, args: Vec<Type> }`. Update all exhaustive `match` arms across `src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs` (~40–60 sites). Add `[kind: "type-stage-app"  fn: String  args: [<type-dict> ...]]` to the **type dict schema** used by the annotation resolver — this is distinct from `ast-of` output (which serializes `Value` nodes, not `Type::*` nodes). Types stored in runtime structs (`FnAnnotation`, etc.) are always post-normalization — no live `TypeStageApp` nodes survive into runtime representations. `ast-of` on a function returns pre-computed type dicts from `FnAnnotation`; it does not need a `NormCtxt` at call time.  
**Impact:** Major — touches every type operation, but each arm is mechanical (normalize before proceeding).

### `src/types.rs` — `NormCtxt` and normalization subsystem

**Current:** Type simplification is scattered: `promote_literal_for_constrained_var`, `widen_literal_for_constraint`, `expand_alias_body_guarded`, `type_key()` for FD dispatch. Each call site has its own logic.  
**Proposed:** Unified `normalize(ty: Type, ctx: &NormCtxt) -> Type` function in a new `src/type_normalize.rs`. `NormCtxt` carries substitution, type-stage env, alias table, class env, normalization cache, and depth counter. All unification calls preface with `normalize`. The existing literal-widening and alias-expansion code is consolidated into `normalize`.  
**Impact:** Major — consolidates scattered logic; `types.rs` imports `value.rs` (Rc<Environment> in NormCtxt). New module.

### `src/types.rs` — `ClassDecl` (single source of truth for FDs)

**Current:** `ClassDecl { name, params, superclasses, methods }` — no `fundeps` or `resolver` field. Separately, `Constraint::Class { class, vars, fundeps }` carries a copy of the FD structure per constraint site.  
**Proposed:** Add `determines: Vec<(Vec<usize>, Vec<usize>)>` and `resolver: Option<String>` to `ClassDecl`. Change `superclasses: Vec<(String, String)>` to `superclasses: Vec<(String, Vec<String>)>` to correctly represent MPTC superclass relationships — the second element is the list of subclass params that map positionally to the superclass params. **Remove** `fundeps` from `Constraint::Class` — `ClassDecl` becomes the single source of truth. `improve_functional_dependency` looks up FDs from `ClassDecl` via `state.class_env.get(class_name)` instead of reading them from the constraint. `Constraint::Class` retains only `class: String` and `vars: Vec<String>`.  
**Impact:** Minor struct extension + removal of FD duplication; reduces confusion about which source is authoritative.

### Module Restructuring — Breaking the `value.rs → types.rs` Circular Dependency

**Current:** `value.rs` imports `Type` from `types.rs` (`use crate::types::Type`). Adding `Rc<Environment>` (from `value.rs`) to `NormCtxt` (which would live in `types.rs` or `type_normalize.rs`) would create a `types.rs → value.rs → types.rs` circular import — rejected by the Rust compiler.

**Proposed:** Extract `Type` and its immediate structural dependencies into a new `src/type_def.rs` that neither `value.rs` nor `types.rs` needs to import from. This breaks the cycle:

```
type_def.rs       →  [nothing internal]
value.rs          →  type_def.rs           (no longer imports types.rs)
types.rs          →  type_def.rs           (re-exports Type for back-compat)
type_normalize.rs →  type_def.rs, value.rs, types.rs
types.rs          →  type_normalize.rs     (NormCtxt in InferState — no cycle)
```

The chain `types.rs → type_normalize.rs → value.rs → type_def.rs` is acyclic. All existing call sites using `use crate::types::Type` continue to work because `types.rs` re-exports: `pub use type_def::*;`.

**What moves to `src/type_def.rs`:** the `Type` enum, `Row`, `RowTail`, `TypeKey`, and the purely structural type methods (`collect_type_vars`, `has_type_vars`, `occurs_in`, `Display`). Everything touching `InferState`, `Substitution`, `TypeScheme`, `ClassDecl`, `Constraint` remains in `types.rs`.

**Further splitting opportunity:** since `types.rs` is large, this restructuring is a natural moment to split it into focused modules:

| File | Content |
|------|---------|
| `src/type_def.rs` | `Type`, `Row`, `RowTail`, `TypeKey` — the data model |
| `src/type_scheme.rs` | `TypeScheme`, let-generalization support |
| `src/type_class.rs` | `ClassDecl`, `Constraint`, `ClassEnv`, `InstanceEnv` |
| `src/type_infer.rs` | `InferState`, `Substitution`, `Levels` |
| `src/type_normalize.rs` | `NormCtxt`, `normalize()` |
| `src/types.rs` | thin `mod` + re-exports of all the above |

`types.rs` becomes a façade — all existing `use crate::types::...` call sites are unchanged. Smaller files are easier to navigate and the logical groupings (data vs inference vs class system vs normalization) are clearer.

**Impact:** Major refactor of the `types.rs`/`value.rs` boundary; all call sites unchanged due to re-exports; enables `InferState` to carry `NormCtxt` without circular imports.

**Implementation tooling:** the `mcp__toolbox__fs_move_range` tool moves line ranges between files (use for extracting structs/enums into new modules), and `mcp__toolbox__fs_bulk_edit` applies multiple edits in one call (use for updating `use` declarations across many files). These are the recommended tools for executing this refactor.

### `src/types.rs` / `src/typecheck_annot.rs` — `kinds:` key

**Current:** Kind constraints are populated by `f@Operator` annotation in class param lists and function annotations. `KindEnv` is the only environment populated implicitly (via annotation) rather than by an explicit declaration form.  
**Proposed:** Add `kinds:` as a recognised metadata bracket key alongside `constraint:`. In `[class ...]` bodies and `fn@[...]` annotation brackets, `kinds: [f: Operator  key: Label]` registers kind constraints in `kind_env` for each named TypeVar. Processing order: after `bind:`, before `constraint:`. The existing `f@Operator` annotation form is retired; `kinds:` is the canonical form.  
**Impact:** Minor — new key recognised in existing annotation bracket parsing paths; routing to `kind_env` already exists.

### `src/ast.rs` — `Expr::ClassDecl` fields

**Current:** `Expr::ClassDecl { name, params, superclasses, methods: Vec<Spanned<Entry>> }` — no separate fields for `determines:` or `resolver:`.  
**Proposed:** Add `determines: Vec<Spanned<Expr>>` and `resolver: Option<Spanned<Expr>>` to `Expr::ClassDecl`. These hold raw parsed values before semantic validation. `StackFrame::ClassDecl` in `src/parser.rs` gains matching accumulators; the `push_value` arm detects `determines:` and `resolver:` by **string comparison on the pending key** (consistent with how other special dict keys like `type:`, `default:`, `doc:` are recognized — none of those are parser keywords either). `determines` and `resolver` are NOT added as lexer keywords: they are reserved only in `[class ...]` body position, not globally. A method named `determines` or `resolver` is simply impossible in `[class ...]` bodies — the keys are always routed to the dedicated AST fields.  
**Impact:** Moderate — AST extension + parser routing change.

### `src/typecheck.rs` — `Expr::ClassDecl` handler

**Current:** Parses `[class [Name a b c]  methods...]` with the class name embedded in the first bracket — the new syntax `Name: [class [a b c]  methods...]` extracts the name from the surrounding dict key instead. No `determines:` or `resolver:` key recognition.  
**Proposed:** After extracting `determines` and `resolver` from the AST: (1) validate `determines:` entries — each must be a 2-element list, first is a list of known param names, second is a name or list of names; (2) resolve param names to positional indices; (3) validate coverage and consistency conditions; (4) validate that `resolver` name exists in the type-stage Env and is callable (if the type-stage env is not yet populated at class declaration time, defer this check to first use — but emit a warning). A misspelled resolver name must produce a type error at class declaration time, not a silent runtime failure.  
**Impact:** Moderate — semantic validation + resolver name lookup.

### `src/parser.rs` — `StackFrame::InstanceDecl`

**Current:** No `StackFrame::InstanceDecl` exists.  
**Proposed:** A new parser stack frame that handles the `[instance ClassName arm1: dict1  arm2: dict2 ...]` form. The frame uses the same bracket-then-colon mechanism as `StackFrame::Match`:

```rust
StackFrame::InstanceDecl {
    class_name: String,               // captured from the first expression in the bracket
    arms: Vec<(Vec<Spanned<Expr>>, Vec<Spanned<Entry>>)>,  // completed (pattern, methods) pairs
    pending_arm_pattern: Option<Vec<Spanned<Expr>>>,        // bracket contents waiting for ':'
    pending_arm_methods: Option<Vec<Spanned<Entry>>>,       // method dict waiting to close
}
```

**Parsing sequence:**
1. Parser opens `[instance` bracket → pushes `StackFrame::InstanceDecl { class_name: "", arms: [], ... }`
2. First expression pushed is the class name (a `VarRef`) → stored in `class_name`
3. Next bracket `[T1 T2 ...]` is pushed as a child frame in **type-pattern mode**: all expressions are parsed as type references (no implied-call), closing the bracket returns `Vec<Spanned<Expr>>`
4. The returned Vec is stored in `pending_arm_pattern`
5. Token `:` arrives → `pending_arm_pattern` transitions to the arm-pattern-captured state
6. Next bracket `[method: impl ...]` is a keyed dict → parsed normally, returns `Vec<Spanned<Entry>>`
7. When the method dict bracket closes, the `(pending_arm_pattern, method_entries)` pair is pushed to `arms`; state resets for the next arm
8. When the outer `]` closes, the complete `arms` list becomes `Expr::InstanceDecl { class_name, arms }`

**Type-pattern mode** for step 3: the inner bracket `[T1 T2 ...]` is parsed as a flat list in type-expression position. All expressions in this bracket are parsed via `parse_type_expr()` (the same path used in annotation resolution), not as value expressions. This ensures that bare names are type constants, not callable heads. Inner brackets within the arm pattern (e.g., `[or Int Null]`) are parsed recursively as composite type expressions.

**Impact:** Moderate — new StackFrame variant + type-pattern parsing mode; mirrors existing Match arm handling.

### `src/typecheck.rs` — `Expr::InstanceDecl` handler

**Current:** No `[instance ...]` expression form; instances are not user-declarable.  
**Proposed:** Validate and register instances from `Expr::InstanceDecl { class_name, arms }`. For each arm: (1) the type-parameter count must match the class's declared param count; (2) disjointness is checked against all previously registered arms for the same class (pairwise unification of type-parameter lists); (3) coverage and consistency conditions are checked for classes with FDs; (4) each method key must correspond to a method declared in the class body; (5) each method implementation is typechecked against the expected method signature with the arm's type parameters substituted. Passing validation, each arm is registered as an entry in `InstanceEnv` keyed by `(class_name, type_key_tuple)`.  
**Impact:** Moderate — new AST node, new parser stack frame, new typecheck handler.

### `src/type_unify.rs` — `improve_functional_dependency`

**Current:** Calls `lookup_arithmetic_instance()` — hardcoded 9-entry match on `(type_key(a), type_key(b))`.  
**Proposed:** Look up `class_decl.resolver` in the type-stage Env; if present, convert determining `Type::*` values to type dicts, call `eval(resolver_fn, dicts, type_stage_env)`, convert result back to `Type::*`, unify. Fall back to `lookup_arithmetic_instance` when resolver is absent (arithmetic built-ins).  
**Impact:** Moderate — requires Type ↔ type dict conversion at unification time; access to type-stage Env from unifier.

### `src/type_unify.rs` — BAS deferral

**Current:** `all_det_ground` check uses `!ty.has_inference_vars()` — fires for any concrete type including unions.  
**Proposed:** Strengthen to require atomic named monotypes in all determining positions before firing.  
**Impact:** Minor — predicate change; prevents silent improvement failures on union types.

### `src/types.rs` — Generalization with FD constraints

**Current:** `generalize()` does not consider FD constraints. Determined TypeVars that remain free at generalization time are generalized independently (incorrect for FD semantics).  
**Proposed:** No change to `generalize()` for the determined-var case — the Jones (1995) qualified types model generalizes `c` alongside `a` and `b`, with the constraint `Add a b c` included in the scheme. The one addition: at constraint-creation time for MPTC constraints, lower the determined TypeVar's level to `enclosing_level` so it cannot escape into an outer scope without the constraint. This is a small change to the MPTC constraint registration path.  
**Impact:** Minor — level assignment at constraint creation; no changes to the generalization algorithm itself.

### `stdlib/prelude.llt` — Arithmetic class migration

**Current:** `Add`, `Sub`, `Mul`, `Div` pre-registered in Rust (`src/types.rs:1686-1707`) with no methods and a hardcoded lookup table (`lookup_arithmetic_instance`).  
**Proposed:** Declare in `stdlib/prelude.llt` with `determines:`, `resolver:`, and method declarations. Arithmetic instances declared as `[instance ...]` blocks using match-arm syntax. The 9 primitive instances become arms under `[instance Addable ...]`, `[instance Subtractable ...]`, etc., using `builtin-add`/`builtin-sub`/`builtin-mul`/`builtin-div` as implementations. The Rust lookup table (`lookup_arithmetic_instance`) is **retained as a performance fast path** — when the class is a known built-in arithmetic class, the O(1) match table is used instead of calling `eval()`. The resolver call path is used only for user-declared classes.  
**Impact:** Major structurally (moves class/instance to tinct); Minor for runtime performance (fast path preserved for arithmetic).

## Prerequisites

- `type-ann-v2-infra` sprint — establishes the `--- stage: type` environment and the type-stage evaluator that resolver functions run in
- `type-ann-v2-resolver` sprint — establishes Type ↔ type dict conversion (annotation resolver) which CHR unification reuses at inference time

## References

- Dijkstra, A., Fokker, J. & Swierstra, S.D. (2008). "The Architecture of the Utrecht Haskell Compiler." *Haskell '08*, pp. 93–104. — [concrete CHR-based type class resolution implementation; implementation reference for the resolver call path]
- Eisenberg, R.A., Vytiniotis, D., Peyton Jones, S. & Weirich, S. (2014). "Closed Type Families with Overlapping Equations." *POPL 2014*, pp. 671–683. — [closed type families; ordered overlapping equations; CHR simplification rules]
- Frühwirth, T. (1998). "Theory and Practice of Constraint Handling Rules." *J. Logic Programming*, 37(1-3), 95–138. — [original CHR paper; foundational for the entire framework; defines `==>` propagation and `<=>` simplification rule notation]
- Jones, M.P. (1995). *Qualified Types: Theory and Practice.* Cambridge University Press. — [principal types with FD constraints in schemes; coverage condition; propagating constraints to call sites rather than blocking generalization]
- Jones, M.P. (2000). "Type Classes with Functional Dependencies." *ESOP 2000*, LNCS 1782, pp. 230–244. — [FD improvement in HM; coverage and consistency conditions (not "Paterson conditions")]
- Chakravarty, M.M.T., Keller, G. & Peyton Jones, S. (2005). "Associated Type Synonyms." *ICFP 2005*, pp. 241–253. — [type families as the alternative to FDs; inter-encoding argument; simplification vs propagation distinction]
- Schrijvers, T., Peyton Jones, S., Sulzmann, M. & Vytiniotis, D. (2009). "Complete and Decidable Type Inference for GADTs." *ICFP '09*, pp. 341–352. — [OutsideIn(X) framework; touchability conditions on when improvement may fire; relevant to BAS-aware deferral]
- Stuckey, P.J. & Sulzmann, M. (2005). "A Theory of Overloading." *ACM TOPLAS*, 27(6), 1216–1269. — [CLP(H) foundation for CHR-based typeclass resolution; formal basis for constraint store and improvement]
- Sulzmann, M., Duck, G.J., Peyton Jones, S. & Stuckey, P.J. (2007). "Understanding Functional Dependencies via Constraint Handling Rules." *Journal of Functional Programming*, 17(1), 83–129. — [foundational CHR unification of FDs and type families; Theorem 4.2 (confluence); coverage and consistency conditions; the theoretical basis for this design]
