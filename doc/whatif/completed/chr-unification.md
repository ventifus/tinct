# What If: CHR-Unified Type Constraints for tinct

**State:** Accepted — 2026-05-16

What would it take to unify functional dependencies and type-level computation into a single, coherent constraint system grounded in Constraint Handling Rules?

## Current State

Tinct's type constraint system handles two distinct kinds of type-level reasoning through two separate, ad-hoc mechanisms:

**Mechanism 1 — Functional dependency improvement (propagation).**  
During HM unification, when a multi-parameter constraint's determining type variables become ground, the type checker propagates the determined variable's binding via `improve_functional_dependency()` in `src/type_unify.rs`. Currently this works only for arithmetic classes (`Addable`, `Subtractable`, `Multipliable`, `Divisible`) through a hardcoded 9-entry lookup table (`lookup_arithmetic_instance`):

```tinct
# This works — Addable a b c constraint with FD (a,b)→c
[fn [x@Integer y@Float] [+ x y]]   # infers Float — FD fires: Addable Int Float → Float
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

```text
TypeStageApp("F", [T₁, T₂]) <=> type-stage-eval F(T₁, T₂)   # fires when all args are ground
```

**Propagation rules** (functional dependencies) — fire when guard becomes ground, adding an equality to the constraint store while retaining the original constraint (CHR `==>` rule):

```text
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
    class_env: &'a TypeEnv,           // for FD lookups — scope-resident, not a global registry
    depth: usize,                     // current reduction depth (step limit analogous to GHC's -freduction-depth)
    max_depth: usize,                 // default: 256
    call_stack: Vec<String>,          // in-progress resolver names for cycle detection
}
```

**What `normalize` handles, in application order:**

```text
normalize(ty, ctx):

  1. Substitution: apply ctx.subst to ty (follow TypeVar chains to fixpoint)

  2. TypeStageApp reduction:
     if ty = TypeStageApp(fn, args):
       args' = args.map(|a| normalize(a, ctx))
       if args'.any(|a| a == Unknown):
         # If any determining position is permanently Unknown, the result is Unknown.
         # Deferring indefinitely would leave the determined TypeVar free forever.
         if args'.all(|a| a == Unknown or is_ground(a)):
           return Unknown
         else:
           return TypeStageApp(fn, args')   # some args still TypeVars — defer
       if fn in ctx.call_stack:
         # Cycle detected: F → ... → F. Return unreduced and let depth limit handle it.
         return TypeStageApp(fn, args')
       if args'.all(is_ground) and ctx.depth < ctx.max_depth:
         type_dicts = args'.map(type_to_dict)  # applies literal widening: IntLiteral→Int etc.
         result = eval(ctx.type_stage_env.get(fn), type_dicts, ctx.with_call(fn))
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

**Invariant:** every step that produces a new type containing `TypeStageApp` must re-enter `normalize()` on the result. Step 2 satisfies this (normalizes args and recursively normalizes the resolver's result). Step 5 satisfies this (`normalize(expand_alias(...), ctx)`). Steps 3, 4, 6 never produce `TypeStageApp` nodes. Any future extension adding a new type producer must re-enter.

**Termination:** normalization terminates within each call: TypeStageApp reduction is depth-limited; the Unknown-in-args rule terminates immediately; alias expansion uses rational-tree detection; BAS simplification strictly reduces type structure. Normalization is **not** idempotent across `unify` calls — each call constructs a fresh `NormCtxt` with depth reset to 0, so an irreducible `TypeStageApp` (due to depth limit or non-ground args) may reduce on the next call as more substitution bindings accumulate. This is correct: the substitution grows monotonically, so re-attempts only succeed as new information arrives.

**Cache invariant:** entries are written only when `args'.all(is_ground)` and the result is fully reduced. Deferred cases (non-ground args, Unknown args, depth-limit hits) do not write to the cache. Ground type keys are permanently stable — once `Type::Int`, always `Type::Int` — so no cache invalidation is needed when the substitution grows. The cache is monotonically growing and deterministic (same ground-arg key always maps to the same reduced type), both guaranteed by the purity of type-stage functions.

**`TypeStageApp` unification rules in `unify_normalized`:**

After normalization, `unify_normalized` may still encounter irreducible `TypeStageApp` nodes (non-ground args). Four cases:

1. `unify(TypeStageApp("F", args₁), TypeStageApp("F", args₂))` — same function, both irreducible after normalization. Two sub-cases based on `F`'s injectivity (recorded in `ClassDecl.resolver_injective`):
   - **Injective F:** unify args pairwise (congruence). Sound because equal outputs imply equal inputs.
   - **Non-injective F** (e.g., `AddResult(Int, Float) = Float = AddResult(Float, Float)`): add `(TypeStageApp("F", args₁), TypeStageApp("F", args₂))` to `state.deferred_equalities`. Do NOT unify args — different arg tuples may legally produce the same result. After each `unify` call, process the queue: normalize both sides; if both reduce to concrete types, unify the concrete types (success or TypeError); if still irreducible, keep deferred. Arithmetic classes (`AddResult`, `SubResult`, `MulResult`, `DivResult`) are all non-injective and always use deferred equality.

   **Why deferred equality is correct:** `[= [+ 1 2.0] [+ 1.5 2.5]]` — both sums produce `Float`. When `a=Int,b=Float` and `e=Float,f=Float` are resolved, normalization gives `Float ~ Float` → ✓. If `a=Int,b=Int` (sum is `Int`) vs `e=Float,f=Float` (sum is `Float`): deferred equality fires `Int ~ Float` → TypeError, correctly identifying the mismatch.
2. `unify(TypeStageApp("F", _), TypeStageApp("G", _))` where `F ≠ G` — different functions: `TypeError`. Distinct type families are "apart" (Eisenberg et al. 2014) — they cannot be assumed equal.
3. `unify(TypeStageApp("F", args), ConcreteType)` where `args` is non-ground — stuck application: `TypeError("cannot unify: type-stage application has unresolved arguments — add type annotations to help inference")`. This is the GHC behavior for stuck type family applications. The FD elaboration case (`c ~ TypeStageApp("AddResult", [a, b])`) avoids this because `c` is a TypeVar mediating the equality — it falls into Case 4, not Case 3.
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
# → when f is called with x@Integer, normalization fires: AddResult(Int, Int) → Type::Int

# FD-driven (no annotation): FD elaboration produces TypeStageApp
[fn [x@Integer y@Float] [+ x y]]
# → [$Addable a b c] with a=Int, b=Float → c unified with TypeStageApp("AddResult", [Int, Float])
# → normalization fires immediately → c = Float
```

**Structural combinators (`or`, `each`, `without`) are always eager** — they take type dicts (never TypeVars in type-context) and reduce immediately. The annotation resolver calls them eagerly as before. `TypeStageApp` is produced only for named type-family functions (uppercase by convention) whose arguments may contain TypeVars.

The unifying insight: both rules call the same type-stage function. The only difference is *when* they fire.

### Class Declaration with FDs and Resolver

Instance declarations use **match-arm syntax**: each arm pairs a `[pattern [...]]` type-parameter pattern with a method dict. Multiple arms for different instance heads can be bundled under a single `[instance ClassName ...]` form.

For readability with complex method bodies, the recommended style is to name the implementation functions first and reference them by name in the instance arms:

```tinct
int-add: [fn@Integer [x@Integer y@Integer] [builtin-add x y]]
[instance Addable
  [pattern [a@Integer b@Integer c@Integer]]: [+: int-add]]
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
      _:                                                               [kind: "named" name: "Unknown"]]]
]
---
# Class declaration: ClassEnv (persistent — needed at type-check time AND runtime for dispatch)
# [class ...] routes to ClassEnv by its declaration form, no --- stage: marker needed
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# Instance declarations: InstanceEnv + runtime method implementations
# Each arm is a [pattern [...]]: method-dict pair.
# Multiple instances of the same class can be bundled in one [instance ...] form.
[instance Addable
  [pattern [a@Integer b@Integer   c@Integer  ]]: [+: [fn@Integer   [x@Integer   y@Integer]   [builtin-add x y]]]
  [pattern [a@Integer b@Float c@Float]]: [+: [fn@Float [x@Integer   y@Float] [builtin-add x y]]]]
```

### `kinds:` — Explicit Kind Declarations

Kind constraints declare that a TypeVar has a specific kind (`Operator` for `* → *`, `Label` for field labels, etc.). Currently this is expressed via annotation: `f@Operator` in a class param list. This is the only environment populated implicitly rather than by an explicit `[keyword ...]` declaration form.

`kinds:` makes kind constraints explicit and symmetric with `constraint:`:

```tinct
# constraint: maps TypeVar names to class constraints
# kinds:     maps TypeVar names to kind constraints (same structure, different level)

Functor: [class [f]  [kinds: [f: Operator]]                # f is of kind * → * (type constructor)
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
  [pattern [f@Seq  ]]: [fmap: [fn@[return: [Seq b]] [g@[Fn@b [a]]  xs@[Seq a]]
                  [map g xs]]]
  [pattern [f@Maybe]]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]]
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
  lt?: [fn@Boolean [a a]]]

Monad: [class [m]  [kinds: [m: Operator]  superclasses: [Applicative]]
  bind: [fn@[return: [m b]] [ma@[m a]  k@[Fn@[return: [m b]] [a]]]]]
```

**Semantics:**

- **Constraint entailment**: `Comparable a` in the constraint context implies `Equatable a`. Functions constrained by `[a: Comparable]` can call `eq?` from `Equatable` without an additional `[a: Equatable]` constraint. The `entails()` function in `src/type_unify.rs` already implements transitive superclass lookup for constraint simplification.

- **Instance requirement**: declaring `[instance Comparable [pattern [a@Integer]]: [...]]` requires that `[instance Equatable [pattern [a@Integer]]: [...]]` already exists. The instance checker verifies superclass instances are present at declaration time.

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

For a class with multiple determined variables — `DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]]` — the resolver function receives the determining type dicts and must return a **keyed dict** `[q: <type-dict>  r: <type-dict>]` where the keys are exactly the names listed in the determined position of the `determines:` spec:

```tinct
DivModResult: [fn [...args]
  [match [[builtin-get 0 args]  [builtin-get 1 args]]
    [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
      [kind: "multi-output"  q: [kind: "named" name: "Int"]  r: [kind: "named" name: "Int"]]
    ...]]
```

Multi-output resolvers must return `[kind: "multi-output"  q: <type-dict>  r: <type-dict>]` — an explicit `kind: "multi-output"` sentinel distinguishes them from single-output resolvers. Without this sentinel, a buggy single-output resolver that returns a dict missing its `kind:` key would be silently misread as a multi-output result. The `dict_to_type` conversion, when it sees `kind: "multi-output"`, destructures the remaining fields by the determined-variable names from `determines:` — here `q` and `r`. Each determined TypeVar is unified with the corresponding named field. The key names must match the declared determined-variable names exactly.

When a resolver returns `[kind: "named" name: "Unknown"]` (open-domain fallback), `dict_to_type` produces `Type::Unknown`. The implementation binds the determined TypeVar directly to `Type::Unknown` in the substitution (not via `is_consistent`) so the Unknown-ness propagates concretely. A **warning-level diagnostic** is emitted: `"FD for class {ClassName} returned Unknown for inputs ({T1}, {T2}) — this may indicate a missing instance arm"`. This is not a `TypeError`; the program continues with the determined TypeVar bound to `Unknown`.

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
  eq?: [fn@Boolean [a a]]]
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

Instance declarations use a **match-arm syntax** pairing each type-parameter pattern with its method dictionary. The arm pattern uses the `[pattern [...]]` form — the same keyword used in `[match ...]` arms — with one `paramName@TypePattern` entry per class parameter:

```tinct
[instance ClassName
  [pattern [p1@Type1  p2@Type2  ...]]: [method-key: implementation ...]
  [pattern [p1@Type3  p2@Type4  ...]]: [method-key: implementation ...]]
```

Each `paramName@TypePattern` declares a fresh TypeVar binding (`paramName`) constrained to match `TypePattern`. Uppercase names in `TypePattern` are type references; lowercase names are implicitly fresh TypeVars introduced in this arm's scope (available in method bodies).

Type applications use bracket syntax: `a@[Seq elem]` declares class param `a` must be `Seq` applied to fresh TypeVar `elem`. Simple concrete types need no inner brackets: `a@Integer`, `f@Seq` (bare constructor for HKT params).

After `:`, the method dict `[method-key: impl ...]` supplies the implementations for that particular type combination. Multiple instances of the same class are bundled as additional arms under the same `[instance ClassName ...]` form.

```tinct
# Addable [a b c] — 3 class params, named a/b/c per class declaration:
[instance Addable
  [pattern [a@Integer   b@Integer   c@Integer  ]]: [+: ...]
  [pattern [a@Integer   b@Float c@Float]]: [+: ...]
  [pattern [a@Integer   b@Integer   c@[or Int Null]]]: [+: ...]]  # composite type in TypePattern

# Functor [f] — 1 HKT class param, named f per class declaration:
[instance Functor
  [pattern [f@Seq  ]]: [fmap: ...]
  [pattern [f@Maybe]]: [fmap: ...]]

# Appendable [a] — concrete or parametric:
[instance Appendable
  [pattern [a@String      ]]: [concat: ...  empty: ...]
  [pattern [a@[Seq elem]]]: [concat: ...  empty: ...]   # a must be Seq of elem
  [pattern [a@[Map k v]]]: [concat: ...  empty: ...]]   # a must be Map k v
```

Instances are anonymous — they register in the InstanceEnv and are selected automatically at call sites.

### Resolver Linking

The `resolver:` key names the type-stage function(s) used by the normalization pass when reducing `TypeStageApp(class_name, ground_args)`. For a class with one FD, `resolver:` takes a single name. For a class with N FDs, `resolver:` takes a list of N names in the same order as the `determines:` list — the Kth resolver is called when the Kth FD's determining positions become ground.

Each resolver is called via:

1. Convert `Type::*` to type dicts with **literal widening**: `IntLiteral(_) → [kind: "named" name: "Int"]`, `FloatLiteral(_) → [kind: "named" name: "Float"]`, `StringLiteral(_) → [kind: "named" name: "Str"]`, then all other types by their kind tag. Widening happens before conversion so that call sites `[+ 1 2]` (where args are `IntLiteral(1)`, `IntLiteral(2)`) produce the same resolver inputs as explicitly-annotated `Int` args.
2. Look up the resolver by name in the type-stage Env (carried by `NormCtxt`)
3. Call `eval(resolver_fn, type_dicts, type_stage_env)` — returns a type dict for the determined position(s). If `eval()` encounters an `InProgress` thunk (resolver cycle not caught by `call_stack`), the resulting `EvalError` is caught at the `normalize()` call site and converted to `TypeError("type-stage evaluation failed: {message}")`. This is not catchable by user `$try` — TypeErrors are static diagnostics, not runtime errors.
4. Convert result back to `Type::*` via the type dict → Type mapping

**Normalization cache:** `NormCtxt` carries a `HashMap<(String, Vec<TypeKey>), Type>` keyed on `(fn_name, type_keys_of_args)`. Arithmetic class results are pre-populated from the existing `lookup_arithmetic_instance` table (O(1) cache hits). User-declared classes warm the cache on first reduction.

**`NormCtxt` and `InferState`:** `InferState` carries `norm_ctxt: NormCtxt` which includes `type_stage_env: Rc<Environment>`. This creates a `types.rs → value.rs` dependency — an intentional architectural decision, consistent with GHC's unified `TcM` monad where type checking, type family reduction, and constraint solving share the same context.

### Resolver Soundness Obligations

Calling `eval(resolver_fn, ...)` during unification requires the resolver to satisfy four obligations:

1. **Totality**: the resolver must terminate. Mutual recursion is caught by `call_stack` in `NormCtxt` and returns the unreduced `TypeStageApp` (the depth counter then handles the termination). When `ctx.depth >= ctx.max_depth`, normalization returns the unreduced `TypeStageApp` unchanged. If that node later reaches a position where a concrete type is required, unification produces `TypeError("type-stage reduction depth exceeded while computing {fn_name}({arg_types})")` — the error includes the resolver name and argument types from the `TypeStageApp` node so the user can identify which resolver hit the limit.

2. **Determinism**: the resolver must return the same output for the same inputs across all call sites. This is guaranteed by the purity of type-stage functions — they are evaluated in an isolated environment with no mutable state or I/O. Builtins that would break this (e.g., `$include`, `$emit`) are not in scope in `--- stage: type` sections.

3. **Well-formedness**: if the resolver returns a malformed type dict (missing `kind:` key, unrecognized `kind:` value, structurally invalid), the type-dict-to-`Type::*` conversion produces `TypeError("resolver {fn_name} returned invalid type dict at call site {span}")` carrying the call-site span (the expression that triggered normalization). This error is not catchable by `$try`.

4. **Confluence**: Sulzmann et al. (2007, Theorem 4.2) prove that CHR improvement is confluent under the consistency condition. FD improvement is deterministic when instances satisfy coverage and consistency (see §Instance Soundness Conditions below). Non-coherent resolvers (where improvement could derive conflicting bindings) are caught by unification conflict — a standard `TypeError` fires, not silent unsoundness.

### Instance Soundness Conditions

At instance declaration time, every instance arm for a class must satisfy three conditions:

- **Disjointness condition**: no two arms of the same class may match the same ground type tuple. If two arms' type-parameter lists can be simultaneously unified under some substitution θ (i.e., they overlap), the instance declaration is rejected at declaration time with a "overlapping instance arms" error. Instance dispatch is therefore **always unambiguous** — for any ground type tuple at most one arm matches. There is no first-match semantics and no ordering among arms.

  Examples: `[pattern [a@Integer b@Integer c@Integer]]` and `[pattern [a@Integer b@Float c@Float]]` are disjoint (no θ unifies them). `[pattern [a@Integer b@t1 c@t2]]` and `[pattern [a@Integer b@Integer c@t3]]` overlap (`θ = {t1=Int, t2=t3}`) and are rejected. `[pattern [a@Integer b@t1 c@t2]]` and `[pattern [a@Float b@t1 c@t2]]` are disjoint.

  **Error message:** `"overlapping instance arms for class {ClassName}: arm [pattern [...]] at line {N} overlaps with arm [pattern [...]] at line {M} under substitution {θ}"` — the second arm's `[pattern ...]` span is primary; the first arm is secondary context.

- **Coverage condition** (for classes with FDs): for each FD `(d) → (r)`, every type variable appearing in the determined positions of the instance arm must also appear in the determining positions. This prevents improvement from introducing fresh unknowns that cannot be resolved. Example: arm `[pattern [a@t1 b@t2 c@t2]]` with FD `(a,b)→c` — `c` binds to the same TypeVar as `b` (both `t2`), which appears in the determining positions, so coverage holds.

  **Error message:** `"coverage violation in instance arm for class {ClassName}: variable {v} appears in determined position of FD ({determining}→{determined}) but not in any determining position"` — span on the offending arm's `[pattern ...]`.

- **Consistency condition** (for classes with FDs, Jones 2000, Definitions 7–8): if two arms' determining positions unify under some substitution θ, their determined positions must also unify under θ. This guarantees improvement is confluent. Example: arms `[pattern [a@Integer b@Integer c@Integer]]` and `[pattern [a@Integer b@Integer c@Float]]` violate consistency — both have determining positions `(Int, Int)` but different determined types `Int` vs `Float`. This is rejected at declaration time.

  Note: the consistency condition is independent of the disjointness condition. Two arms can be disjoint on their full type-parameter lists but their **determining** positions may still be unifiable — consistency checks only the determining positions.

  **Error message:** `"consistency violation for class {ClassName}: arms at lines {N} and {M} both match determining positions ({T1}, {T2}) but disagree on determined type: {TypeA} vs {TypeB}"` — second arm is primary span, first arm is secondary context.

**Acyclic dependency graphs and cross-FD consistency**: for classes with multiple FDs sharing variables, if FDs `D₁` and `D₂` both fire simultaneously when all their determining positions are ground, their results must be mutually consistent. The critical-pair condition (Sulzmann et al. 2007): for each pair of FDs `(d₁ → r₁)` and `(d₂ → r₂)` where `r₁` overlaps with `d₂`, verify that composing the resolvers is consistent — for every ground type tuple in the instance domain, applying resolver₁ and then resolver₂ must return the original input. For finite-domain resolvers this is checked at declaration time by exhaustive enumeration. For open-domain resolvers the check is deferred and the depth limit serves as the termination guard.

**Instance world assumption**: instances are **closed within a compilation unit**. All `[instance ClassName ...]` forms for a class, including those contributed by `[include ...]`'d files, are resolved at type-check time for the including file. The full accumulated arm set is checked for disjointness, coverage, and consistency as a batch before the first constrained expression in the file is type-checked. This avoids orphan-instance and cross-module coherence complexity.

Instances violating any of these conditions are rejected with a type error at instance declaration time.

### BAS-Aware Improvement Deferral

Under BAS, a TypeVar bound during unification may resolve to a union type (`Int | Float`) rather than a ground monotype. FD improvement must not fire when any determining position contains a union, intersection, or negation type — only atomic named types trigger improvement.

```text
improve_functional_dependency conditions:
  ∀ position p ∈ determining(FD):
    type[p] is Type::Int | Type::Float | Type::Bool | Type::Str | ...  (named primitive)
    NOT Type::Union(...) | Type::Intersection(...) | Type::Negation(...) | Type::TypeVar(_)
```

This is the conservative, sound approach. Distribution over union types (e.g., `Add (Int|Float) Int c ⟹ c = Int|Float`) would require proving the resolver function is covariant on the subtype lattice — a proof obligation deferred to future work.

**Future extension — `distributes-over-union:`**: for finite-domain resolvers (like arithmetic), distribution is verifiable by exhaustive case analysis. A future `distributes-over-union: true` flag on a class declaration permits the checker to distribute improvement over union members, eliminating deferral in those cases. For open-domain resolvers, the conservative deferral remains correct.

### Automatic Boundary Guards

The `normalize()` subsystem is the enabling infrastructure for automatic insertion of `ThunkState::Guarded` at every `Unknown → Concrete` boundary. This belongs here because it requires `normalize()` to produce concrete guard types — the evaluator has no `NormCtxt` and cannot reduce `TypeStageApp` nodes at runtime.

`[@Type expr]` TypeAssert sites insert guards explicitly at annotation sites. The elaboration pass described here covers all remaining implicit boundaries: every point where a value of type `Unknown` flows into a context expecting a concrete type. Guard types must be concrete before emission, so guard insertion runs as a post-inference pass after all TypeVars are ground and normalization is complete.

**Boundary catalog.**

| Boundary | Example | Guard inserted on |
|----------|---------|-------------------|
| Function argument | `[f x]`, `f: Int→Int`, `x: Unknown` | `x` with expected type `Int` |
| Builtin argument | `[+ n m]`, `m: Unknown` | `m` with expected numeric type |
| Field access on `Unknown` | `data.port`, `data: Unknown` | result with field's declared type |
| `$match` scrutinee | `[match x [Int n]: ...]`, `x: Unknown` | `x` with the inferred union of arm types |
| `---` pipeline crossing | downstream section expects typed input from untyped upstream | each crossing binding |

**Note on `$apply`:** `[apply f args]` where `f: Unknown` — the argument types expected by `f` are not statically known, so guards cannot be inserted on the argument positions. `$apply` with an `Unknown` function is a limitation: blame fires only if the function itself is not callable, not if arguments don't match parameter types. This is documented as a known limitation of the automatic guard system.

**Post-inference elaboration pass.**

Guard insertion is a post-inference elaboration pass, not an inline operation during type checking. It runs after `infer_dict` completes and all TypeVars are ground. The pass walks the type map produced by inference, finds every expression where the inferred type is `Unknown` and the contextual expected type is concrete, and emits a guard:

```text
for each expression e in the type map:
  τ_e   = inferred type of e
  τ_ctx = contextual expected type of e (from call site, param annotation, etc.)
  if τ_e = Unknown and τ_ctx ≠ Unknown:
    expected = normalize(τ_ctx, NormCtxt::final(subst, type_stage_env, ...))
    if not is_concrete(expected):  # is_concrete: not TypeVar, not TypeStageApp, not Unknown
      return TypeError("type-stage application could not be reduced at boundary — \
        add type annotations or check resolver depth limit")
    annotate expression e with expected concrete type (write to RefCell field)
    # eval() reads this annotation during its normal AST walk and creates Guarded thunk
```

The `normalize()` call is the load-bearing step: it reduces any `TypeStageApp` nodes in the expected type to concrete types before the guard is emitted. An irreducible `TypeStageApp` at this point means a depth-limit hit or a TypeVar that escaped inference — both should have already produced a `TypeError` earlier.

**Blame labels.** All guards — both TypeAssert and automatic — carry `BlameLabel { origin_span, boundary_span, polarity }`. Polarity is `Negative` (the untyped provider is responsible) for call arguments and `Positive` (the consuming context is responsible) for return-value uses. The co-natural strategy (Greenman et al. 2019) applies: when a guarded value crosses a second boundary, the outer label is discarded and the inner (most recent) label is kept — O(1) space overhead, most actionable provenance preserved.

**`Unknown` in CHR determining positions — defer.**

`Type::Unknown` is not an atomic named monotype. FD improvement must not fire when any determining position is `Unknown`. Add it to the deferral predicate alongside `Type::TypeVar(_)`:

```text
improve_functional_dependency fires only when all determining positions are atomic:
  type[p] is NOT Union(...) | Intersection(...) | Negation(...) | TypeVar(_) | Unknown
```

This is consistent with the gradual typing consistency rule `is_consistent(Unknown, τ) = true`: `Unknown` does not determine a concrete type at the type level — the determination defers to runtime and is caught by the guard if wrong.

**`unify` call ordering.**

The sequencing within `unify` must be: `normalize` first, then BAS-aware deferral check, then `is_consistent`. A `TypeStageApp` that normalizes to a union type must trigger deferral *before* the consistency check fires. Reversing the order would allow `TypeStageApp("AddResult", [Unknown, Int])` to pass consistency (`Unknown ~ anything`) before normalization reveals a union in the determining position — causing incorrect FD firing.

```rust
fn unify(a: Type, b: Type, subst: &mut Substitution, state: &mut InferState) -> Result<(), TypeError> {
    let norm = NormCtxt::from(subst, state);
    let a' = normalize(a, &norm);   // 1. normalize — reduces TypeStageApp, widens literals
    let b' = normalize(b, &norm);
    unify_normalized(a', b', subst, state)
    // inside unify_normalized:
    //   2. BAS-aware deferral check (union/intersection/Unknown in FD positions)
    //   3. is_consistent check for Unknown ~ τ paths
}
```

### Generalization with FD Constraints

TypeVars in determined positions (`c` in `[$Addable a b c]`) are generalized normally under the Jones (1995) qualified types model. The FD constraint is propagated as part of the type scheme and fires at each call site when the determining positions are instantiated to ground types.

Consider `[fn [x y] [+ x y]]` — the principal type is `∀a b c. Add a b c ⇒ a → b → c`. Here `c` IS included in `type_vars`. The constraint `Add a b c` in the scheme is not a free universal quantification over `c` — it is a qualified type where `c` is determined by `(a, b)`. At a call site `[f 1 2.0]`, instantiation creates fresh copies `a=Int, b=Float, c=?`; FD improvement fires immediately and resolves `c=Float`. This is the standard Haskell treatment and is sound (Jones 2000).

Two cases at let-generalization:

1. **FD has already fired** — `c` is unified with a concrete type and does not appear free in the scheme. Generalization produces `∀a b. Add a b Float ⇒ a → b → Float` (with concrete `c`).
2. **FD has not fired** (determining positions are still free TypeVars) — `c` remains a TypeVar and is included in the generalized scheme with the constraint: `∀a b c. Add a b c ⇒ a → b → c`. FD fires at each call site.

**Level management**: when a `[$Class a b c]` constraint is registered with FD `(a,b)→c`, the determined TypeVar `c`'s level must be set to `max(enclosing_level, max(l_a, l_b))` at constraint-creation time. This ensures `c` cannot escape into an outer scope independently of the constraint — it can only be used where the constraint is visible, and also prevents `c` from being generalized beyond the scope of its determining TypeVars.

## Open Design Questions

### ~~Congruence Rule for Non-Injective Resolvers~~ — Resolved

**Resolution:** Deferred equality for non-injective resolvers. Case 1 of `unify_normalized` now splits on `ClassDecl.resolver_injective`:

- Injective F: pairwise congruence (sound — equal outputs imply equal inputs)
- Non-injective F: add to `state.deferred_equalities`; process after each `unify` call when args become ground

See §`src/type_infer.rs — Deferred equality queue` in What Would Change for the implementation. All arithmetic classes are non-injective and use deferred equality. Injective classes (e.g., `Convert [a b]` where each `a` maps to a unique `b`) use congruence.

### ~~Level Propagation for Late Unifications~~ — Resolved (not a soundness issue)

**Finding:** One-shot level assignment — `c`'s level is set to `max(enclosing_level, max(ℓ_a, ℓ_b))` once and not recomputed if `a` or `b` are later lowered by [U-VAR-LEVEL].

**Analysis:** This is a **precision concern, not a soundness bug.** The Jones (1995) qualified-types model guarantees that the FD constraint `Add a b c` is always carried in the type scheme alongside `c`. At every call site, the constraint fires FD improvement and correctly determines `c` from the instantiated `a` and `b`. Level-propagation issues can cause `c` to be generalized at a deeper scope than strictly necessary (the scheme's `∀` quantifier includes `c` when it need not), making the scheme slightly over-general — but the FD constraint in the scheme prevents any incorrect type from being inferred.

**Concrete example:** `[fn [x] [fn [y] [+ x y]]]` — without level propagation, the inner function's scheme is `∀b c. Add α b c ⇒ Fn(b → c)` rather than the tighter `∀b. Add α b (AddResult α b) ⇒ Fn(b → AddResult α b)`. Both are correct. The FD fires at each call site and produces the same concrete types either way.

**The `level_deps` fix** (`HashMap<TypeVarName, Vec<TypeVarName>>` in `InferState` to propagate level lowering) would produce tighter schemes with fewer universally-quantified variables, but provides no correctness benefit. Track as a follow-on improvement if scheme precision matters for LSP hover display or error messages.

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
      _:                                                               [kind: "named" name: "Unknown"]]]
]
---
# Trivial: class + instances
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

# Instances bundled in one form; each arm is a [pattern [...]]: method-dict pair.
# For readability, name the implementation functions first:
int-int-add:   [fn@Integer   [x@Integer   y@Integer]   [builtin-add x y]]
int-float-add: [fn@Float [x@Integer   y@Float] [builtin-add x y]]

[instance Addable
  [pattern [a@Integer b@Integer   c@Integer  ]]: [+: int-int-add]
  [pattern [a@Integer b@Float c@Float]]: [+: int-float-add]]

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
  [pattern [a@Integer b@String]]: [show: [fn@Stringing [x@Integer] [int-to-str x]]
                                parse: [fn@Integer [s@String] [str-to-int s]]]]

# Trivial: forward inference
stringify: [fn@[bind: [a b]  return: b  constraint: [$Convert a b]]  [x@a]  [show x]]
[stringify 42]       # a=Int, b=String → "42"

# Nontrivial: roundtrip function, both directions must typecheck
roundtrip: [fn@[bind: [a b]  return: a  constraint: [$Convert a b]]
  [x@a]
  [parse [show x]]]  # show: a→b, parse: b→a; return type is a

[roundtrip 42]       # a=Int, b=String → parse(show(42)) = parse("42")
                     # Return type is Unknown (not Int): FromStringResult(String) = Unknown,
                     # so parse's return type is Unknown. The call succeeds statically
                     # but a boundary guard is inserted at the call site if a concrete
                     # type is expected. Use a concrete FromStringResult (e.g. → Maybe Int)
                     # to get a precise return type.
```

### D: Multi-Output FDs

```tinct
--- stage: type
[
  DivModResult: [fn [...args]
    [match [[builtin-get 0 args]  [builtin-get 1 args]]
      [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
        [kind: "multi-output"
         q: [kind: "named" name: "Int"]
         r: [kind: "named" name: "Int"]]]]
]
---
# Trivial: multi-output — (a,b) jointly determines (q, r)
# The resolver returns [q: <type-dict>  r: <type-dict>] — keys match the determined-var names in determines:
DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]
  divmod: [fn@[record q: q  r: r] [a b]]]

[instance DivMod
  [pattern [a@Integer b@Integer q@Integer r@Integer]]: [divmod: [fn [x@Integer y@Integer]
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
[+ x@[or Int Float] y@Integer]
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
  [pattern [base@ServerBase opts@ServerOpts result@ServerFull]]: [merge: [fn [base@ServerBase opts@ServerOpts]
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
                    constraint: [a: Numeric  b: Numeric  [$Divisible a b c]]]
  [x@a  y@b]
  [if [= y 0]
    [Err "division by zero"]
    [Ok [/ x y]]]]   # return type: Ok(c) where c = DivResult(a,b) = Float

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
  [pattern [a@Integer b@Integer c@[or Int Null]]]: [+?: [fn@[or Int Null] [x@Integer y@Integer]
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
  [pattern [a@Integer b@String c@[record left: Int  right: String]]]:
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
check-addable: [fn@Boolean [x y]
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
      _:                                                               [kind: "named" name: "Unknown"]]]

  SubResult:  [fn [...args] ...]    # same dispatch as AddResult
  MulResult:  [fn [...args] ...]    # same dispatch as AddResult
  DivResult:  [fn [...args] [kind: "named" name: "Float"]]   # division always yields Float
]
---

# ── Single-parameter classes ──────────────────────────────────────────────

Equatable: [class [a]
  eq?: [fn@Boolean [a a]]]

[instance Equatable
  [pattern [a@Integer  ]]: [eq?: [fn@Boolean [x@Integer   y@Integer  ] [builtin-eq x y]]]
  [pattern [a@Float]]: [eq?: [fn@Boolean [x@Float y@Float] [builtin-eq x y]]]
  [pattern [a@String  ]]: [eq?: [fn@Boolean [x@String   y@String  ] [builtin-eq x y]]]
  [pattern [a@Boolean ]]: [eq?: [fn@Boolean [x@Boolean  y@Boolean ] [builtin-eq x y]]]
  [pattern [a@Null ]]: [eq?: [fn@Boolean [x@Null  y@Null ] true]]]

Comparable: [class [a]  [superclasses: [Equatable]]
  lt?: [fn@Boolean [a a]]]

[instance Comparable
  [pattern [a@Integer  ]]: [lt?: [fn@Boolean [x@Integer   y@Integer  ] [builtin-lt x y]]]
  [pattern [a@Float]]: [lt?: [fn@Boolean [x@Float y@Float] [builtin-lt x y]]]
  [pattern [a@String  ]]: [lt?: [fn@Boolean [x@String   y@String  ] [builtin-lt x y]]]]

Showable: [class [a]
  str: [fn@Stringing [a]]]

[instance Showable
  [pattern [a@Integer  ]]: [str: [fn@Stringing [x@Integer  ] [builtin-int-to-str x]]]
  [pattern [a@Float]]: [str: [fn@Stringing [x@Float] [builtin-float-to-str x]]]
  [pattern [a@String  ]]: [str: [fn@Stringing [x@String  ] x]]
  [pattern [a@Boolean ]]: [str: [fn@Stringing [x@Boolean ] [if x "true" "false"]]]
  [pattern [a@Null ]]: [str: [fn@Stringing [_      ] "null"]]]

Numeric: [class [a]  [superclasses: [Comparable  Showable]]]
  # No additional methods — marks a type as numeric for arithmetic constraints

[instance Numeric  [pattern [a@Integer]]: []  [pattern [a@Float]]: []]
# Number is a BAS union alias (Int | Float), not a concrete type.
# Class instances are for concrete types — Number participates via BAS
# width subtyping when Int or Float resolves at the call site.

Appendable: [class [a]
  # Monoid: concat + empty identity element (concat x empty = x = concat empty x)
  concat: [fn@a [a a]]
  empty:  [fn@a []]]

[instance Appendable
  [pattern [a@String      ]]: [concat: [fn@String     [x@String     y@String    ] [builtin-str-concat x y]]
                            empty:  [fn@String     []                     ""]]
  [pattern [a@[Seq elem]]]: [concat: [fn@[Seq elem] [xs@[Seq elem] ys@[Seq elem]] [builtin-seq-concat xs ys]]
                             empty:  [fn@[Seq elem] []                             []]]
  [pattern [a@[Map k v]]]: [concat: [fn@[Map k v] [x@[Map k v] y@[Map k v]] [merge x y]]
                            empty:  [fn@[Map k v] []                          []]]]

# ── MPTC classes with functional dependencies ─────────────────────────────

Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

[instance Addable
  [pattern [a@Integer   b@Integer   c@Integer  ]]: [+: [fn@Integer   [x@Integer   y@Integer  ] [builtin-add x y]]]
  [pattern [a@Float b@Float c@Float]]: [+: [fn@Float [x@Float y@Float] [builtin-add x y]]]
  [pattern [a@Integer   b@Float c@Float]]: [+: [fn@Float [x@Integer   y@Float] [builtin-add x y]]]
  [pattern [a@Float b@Integer   c@Float]]: [+: [fn@Float [x@Float y@Integer  ] [builtin-add x y]]]]

Subtractable: [class [a b c]  [determines: [[[a b] c]]  resolver: SubResult]
  -: [fn@c [a b]]]

[instance Subtractable
  [pattern [a@Integer   b@Integer   c@Integer  ]]: [-: [fn@Integer   [x@Integer   y@Integer  ] [builtin-sub x y]]]
  [pattern [a@Float b@Float c@Float]]: [-: [fn@Float [x@Float y@Float] [builtin-sub x y]]]
  [pattern [a@Integer   b@Float c@Float]]: [-: [fn@Float [x@Integer   y@Float] [builtin-sub x y]]]
  [pattern [a@Float b@Integer   c@Float]]: [-: [fn@Float [x@Float y@Integer  ] [builtin-sub x y]]]]

Multipliable: [class [a b c]  [determines: [[[a b] c]]  resolver: MulResult]
  *: [fn@c [a b]]]

[instance Multipliable
  [pattern [a@Integer   b@Integer   c@Integer  ]]: [*: [fn@Integer   [x@Integer   y@Integer  ] [builtin-mul x y]]]
  [pattern [a@Float b@Float c@Float]]: [*: [fn@Float [x@Float y@Float] [builtin-mul x y]]]
  [pattern [a@Integer   b@Float c@Float]]: [*: [fn@Float [x@Integer   y@Float] [builtin-mul x y]]]
  [pattern [a@Float b@Integer   c@Float]]: [*: [fn@Float [x@Float y@Integer  ] [builtin-mul x y]]]]

Divisible: [class [a b c]  [determines: [[[a b] c]]  resolver: DivResult]
  /: [fn@c [a b]]]

[instance Divisible
  [pattern [a@Integer   b@Integer   c@Float]]: [/: [fn@Float [x@Integer   y@Integer  ] [builtin-div x y]]]
  [pattern [a@Float b@Float c@Float]]: [/: [fn@Float [x@Float y@Float] [builtin-div x y]]]
  [pattern [a@Integer   b@Float c@Float]]: [/: [fn@Float [x@Integer   y@Float] [builtin-div x y]]]
  [pattern [a@Float b@Integer   c@Float]]: [/: [fn@Float [x@Float y@Integer  ] [builtin-div x y]]]]

# ── Higher-kinded classes (f@Operator) ────────────────────────────────────

Mappable: [class [f]  [kinds: [f: Operator]]
  map: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

[instance Mappable
  [pattern [f@Seq ]]: [map: [fn@[return: [Seq b]]    [g@[Fn@b [a]]  xs@[Seq a]   ] [builtin-map g xs]]]
  [pattern [f@Dict]]: [map: [fn@[return: [Dict k b]] [g@[Fn@b [a]]  d@[Dict k a] ] [builtin-map-dict g d]]]]

Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

[instance Functor
  [pattern [f@Seq  ]]: [fmap: [fn@[return: [Seq b]]   [g@[Fn@b [a]]  xs@[Seq a]   ] [map g xs]]]
  [pattern [f@Maybe]]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]  ]
                 [match m  [Some v]: [Some [g v]]  None: None]]]]

Applicative: [class [f]  [kinds: [f: Operator]  superclasses: [Functor]]
  pure:  [fn@[return: [f a]] [x@a]]
  lift2: [fn@[return: [f c]] [g@[Fn@c [a b]]  fa@[f a]  fb@[f b]]]]

[instance Applicative
  [pattern [f@Maybe]]: [pure:  [fn@[return: [Maybe a]] [x@a] [Some x]]
                        lift2: [fn@[return: [Maybe c]] [g@[Fn@c [a b]]  ma@[Maybe a]  mb@[Maybe b]]
                                  [match [ma mb]
                                    [[Some a] [Some b]]: [Some [g a b]]
                                    _:                   None]]]]

Monad: [class [m]  [kinds: [m: Operator]  superclasses: [Applicative]]
  bind: [fn@[return: [m b]] [ma@[m a]  k@[Fn@[return: [m b]] [a]]]]]

[instance Monad
  [pattern [m@Maybe]]: [bind: [fn@[return: [Maybe b]] [m@[Maybe a]  k@[Fn@[return: [Maybe b]] [a]]]
                 [match m  [Some v]: [k v]  None: None]]]]

Foldable: [class [t]  [kinds: [t: Operator]]
  fold:   [fn@b  [f@[Fn@b [b a]]  init@b  xs@[t a]]]
  to-seq: [fn@[return: [Seq a]] [xs@[t a]]]]

[instance Foldable
  [pattern [t@Seq  ]]: [fold:   [fn@b [f@[Fn@b [b a]]  init@b  xs@[Seq a]] [builtin-fold f init xs]]
                        to-seq: [fn@[return: [Seq a]] [xs@[Seq a]] xs]]
  [pattern [t@Maybe]]: [fold:   [fn@b [f@[Fn@b [b a]]  init@b  m@[Maybe a]]
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
**Proposed:** Add `Type::TypeStageApp { fn_name: String, args: Vec<Type> }`. Update all exhaustive `match` arms across `src/types.rs`, `src/type_unify.rs`, `src/type_env.rs`, `src/typecheck.rs`, `src/typecheck_annot.rs`, `src/typecheck_dict.rs` (~40–60 sites). Add `[kind: "type-stage-app"  fn: String  args: [<type-dict> ...]]` to the **type dict schema** used by the annotation resolver — this is distinct from `ast-of` output (which serializes `Value` nodes, not `Type::*` nodes). `collect_type_vars`, `has_type_vars`, `occurs_in` in `src/type_def.rs` must recurse into `TypeStageApp.args` — without this, TypeVars inside TypeStageApp nodes escape occurs-check and let-generalization, enabling infinite types and monomorphic FD-dependent bindings. `generalize()` in `src/type_infer.rs` must also collect TypeVars from `TypeStageApp.args` as candidates for quantification. `entails()` in `src/type_unify.rs` must call `normalize()` before comparing constraint types when those types may contain `TypeStageApp` nodes — it currently runs during inference where `NormCtxt` is available, so the function signature must accept a `NormCtxt` reference. Exception: for superclass chain traversal (reading `ClassDecl.superclasses` without touching type-stage functions), a minimal `NormCtxt` with an empty type-stage env is acceptable since no resolver calls are needed — superclass entailment is structural ClassDecl traversal, not type-family reduction. Types stored in runtime structs (`FnAnnotation`, etc.) are always post-normalization for ground TypeVars — no `TypeStageApp` nodes with fully-ground args survive into runtime representations. However, `FnAnnotation` for polymorphic functions stores scheme bodies that may contain `TypeStageApp` nodes with generalized TypeVar args (e.g., `TypeStageApp("AddResult", [TypeVar("a"), TypeVar("b")])`). `ast-of` on such a function returns these as `[kind: "type-stage-app"  fn: "AddResult"  args: [...]]` in the output. At each call site, `normalize()` reduces the TypeStageApp when args become ground. Add a `debug_assert!(!ty.contains_ground_type_stage_app())` at guard-creation time (in the boundary guard elaboration pass) to enforce that no fully-reducible TypeStageApp escapes into a `Guarded.expected` field.  
**Impact:** Major — touches every type operation, but each arm is mechanical (normalize before proceeding).

### `src/types.rs` — `NormCtxt` and normalization subsystem

**Current:** Type simplification is scattered: `promote_literal_for_constrained_var`, `widen_literal_for_constraint`, `expand_alias_body_guarded`, `type_key()` for FD dispatch. Each call site has its own logic.  
**Proposed:** Unified `normalize(ty: Type, ctx: &NormCtxt) -> Type` function in a new `src/type_normalize.rs`. `NormCtxt` carries substitution, type-stage env, alias table, class env, normalization cache, and depth counter. All unification calls preface with `normalize`. The existing literal-widening and alias-expansion code is consolidated into `normalize`.  
**Impact:** Major — consolidates scattered logic; `types.rs` imports `value.rs` (`Rc<Environment>` in NormCtxt). New module.

### `src/types.rs` — `ClassDecl` (single source of truth for FDs)

**Current:** `ClassDecl { name, params, superclasses, methods }` — no `fundeps` or `resolver` field. Separately, `Constraint::Class { class, vars, fundeps }` carries a copy of the FD structure per constraint site.  
**Proposed:** Add `determines: Vec<(Vec<usize>, Vec<usize>)>`, `resolver: Option<String>`, and `resolver_injective: bool` to `ClassDecl`. `resolver_injective` defaults to `false` and is **not** computed at class declaration time — instance arms do not exist yet (they follow `[class ...]` in source order) and the type-stage Env may not be fully populated. It is computed during the **batch instance coherence check** (see `src/typecheck.rs — Expr::ClassDecl handler` §Batch coherence phase) after all `[instance ...]` forms for the class have been processed and the type-stage Env is available. At that point: for finite-domain (closed instance set) classes, exhaustively call the resolver for each pair of distinct determining-position type tuples across declared instance arms — if any two arms produce the same determined type for different inputs, `resolver_injective = false`; otherwise `true`. For open-domain resolvers (wildcard arm can return anything): `resolver_injective = false` conservatively. All arithmetic classes (`AddResult`, `SubResult`, `MulResult`, `DivResult`) are non-injective. Change `superclasses: Vec<(String, String)>` to `superclasses: Vec<(String, Vec<String>)>` to correctly represent MPTC superclass relationships — the second element is the list of subclass params that map positionally to the superclass params. **Remove** `fundeps` from `Constraint::Class` — `ClassDecl` becomes the single source of truth. `improve_functional_dependency` reads FDs directly from the `ClassDecl` carried in `Constraint::Class` (no global ClassEnv lookup needed). `Constraint::Class` retains `class: ClassDecl` (the full decl, not just a name string) and `vars: Vec<String>`.

Note: this change also requires updating the superclass extraction in `push_expr_to_parent` ClassDecl — currently superclasses are extracted inline from the header bracket (lines `parser.rs:4730–4828`); after the structural-bracket redesign, they are extracted from the structural metadata bracket in the `CloseBracket` handler.

**Impact:** Minor struct extension + removal of FD duplication; reduces confusion about which source is authoritative.

### Module Restructuring — Breaking the `value.rs → types.rs` Circular Dependency

**Current:** `value.rs` imports `Type` from `types.rs` (`use crate::types::Type`). Adding `Rc<Environment>` (from `value.rs`) to `NormCtxt` (which would live in `types.rs` or `type_normalize.rs`) would create a `types.rs → value.rs → types.rs` circular import — rejected by the Rust compiler.

**Proposed:** Extract `Type` and its immediate structural dependencies into a new `src/type_def.rs` that neither `value.rs` nor `types.rs` needs to import from. This breaks the cycle:

```text
type_def.rs       →  [nothing internal]
value.rs          →  type_def.rs
type_infer.rs     →  type_def.rs           (InferState, Substitution, Levels — top-level module)
type_normalize.rs →  type_def.rs, value.rs, type_infer.rs
type_class.rs     →  type_def.rs, type_infer.rs
type_unify.rs     →  type_def.rs, type_infer.rs, type_normalize.rs, type_class.rs
type_scheme.rs    →  type_def.rs, type_infer.rs
types.rs          →  [re-exports from all above — thin façade, no circular imports]
```

`InferState` and `Substitution` must live in a **top-level** `src/type_infer.rs`, not as a submodule of `types.rs`. The previous design had `types.rs → type_normalize.rs` (for `NormCtxt` in `InferState`) AND `type_normalize.rs → types.rs` (for `InferState` and `unify()`) — a circular import Rust rejects. Making `type_infer.rs` top-level breaks this: `type_normalize.rs` imports `type_infer.rs` directly, not `types.rs`. `type_unify.rs` (currently a submodule of `types.rs`) must also become top-level, as it is the integration point for FD improvement and imports from multiple layers. `normalize_union()` and `normalize_intersection()` move to `type_normalize.rs`; `Type::Display` calls them via a function exported from `type_normalize.rs` rather than implementing display in `type_def.rs` directly.

All existing call sites using `use crate::types::Type` continue to work because `types.rs` re-exports everything: `pub use type_def::*;` etc.

**What moves to `src/type_def.rs`:** the `Type` enum, `Row`, `RowTail`, `TypeKey`, and purely structural type methods (excluding `Display`, which moves to `type_normalize.rs`).

**Further splitting opportunity:** since `types.rs` is large, this restructuring is a natural moment to split it into focused modules:

| File | Content |
|------|---------|
| `src/type_def.rs` | `Type`, `Row`, `RowTail`, `TypeKey`, structural type methods except `Display` |
| `src/type_scheme.rs` | `TypeScheme`, let-generalization support |
| `src/type_class.rs` | `ClassDecl`, `Constraint`, `ClassEnv`, `InstanceEnv` |
| `src/type_infer.rs` | `InferState`, `Substitution`, `Levels` (top-level module) |
| `src/type_normalize.rs` | `NormCtxt`, `normalize()`, `normalize_union()`, `normalize_intersection()`, `Type::Display` — Display normalizes before printing when a NormCtxt is available: shows the reduced type if ground (e.g., `Float`), or `FnName(arg1, arg2, ...)` in lowercase-functional notation if irreducible (e.g., `AddResult(a, b)`) |
| `src/type_unify.rs` | `unify()`, `unify_normalized()`, `improve_functional_dependency()`, `satisfies_constraint()` (top-level module) |
| `src/types.rs` | thin `mod` + re-exports of all the above |

`types.rs` becomes a façade — all existing `use crate::types::...` call sites are unchanged. Smaller files are easier to navigate and the logical groupings (data vs inference vs class system vs normalization) are clearer.

**Impact:** Major refactor of the `types.rs`/`value.rs` boundary; all call sites unchanged due to re-exports; enables `InferState` to carry `NormCtxt` without circular imports.

**Implementation tooling:** the `mcp__toolbox__fs_move_range` tool moves line ranges between files (use for extracting structs/enums into new modules), and `mcp__toolbox__fs_bulk_edit` applies multiple edits in one call (use for updating `use` declarations across many files). These are the recommended tools for executing this refactor.

### `src/types.rs` / `src/typecheck_annot.rs` — `kinds:` key

**Current:** Kind constraints are populated by `f@Operator` annotation in class param lists and function annotations. `KindEnv` is the only environment populated implicitly (via annotation) rather than by an explicit declaration form.  
**Proposed:** Add `kinds:` as a recognised metadata bracket key alongside `constraint:`. In `[class ...]` bodies and `fn@[...]` annotation brackets, `kinds: [f: Operator  key: Label]` registers kind constraints in `kind_env` for each named TypeVar. Processing order: after `bind:`, before `constraint:`. The existing `f@Operator` form is retired in **class param lists and `fn@[...]` annotation brackets**. Two distinct routing mechanisms apply: in `[class ...]` structural brackets, `kinds:` is extracted post-hoc from the structural metadata bracket entries in the `CloseBracket` ClassDecl handler; in `fn@[...]` annotation brackets, `kinds:` is a new recognised key in the annotation resolver's property-dict dispatch (`src/typecheck_annot.rs`), routed to `kind_env`. Any existing code using `f@Operator` outside these two positions (e.g., in type alias bodies) requires a migration error: when the annotation resolver sees a SimpleAnnotation named `"Operator"` that is not in the type alias table, emit `"did you mean \`kinds: [f: Operator]\`?"` rather than an undefined-type error.  
**Impact:** Minor — new key recognised in existing annotation bracket parsing paths; routing to `kind_env` already exists.

### `src/ast.rs` — `Expr::ClassDecl` fields

**Current:** `Expr::ClassDecl { name, params, superclasses, methods: Vec<Spanned<Entry>> }` — no separate fields for `determines:` or `resolver:`.  
**Proposed:** Add `determines: Vec<Spanned<Expr>>` and `resolver: Option<Spanned<Expr>>` to `Expr::ClassDecl`. These hold raw parsed values before semantic validation. `StackFrame::ClassDecl` in `src/parser.rs` gains matching accumulators; `determines:` and `resolver:` are NOT routed by `push_value` key-string comparison (unlike `type:`/`default:`/`doc:` which live inside annotation property dicts processed by `parse_annotation()`). Instead, they are keyed entries inside the **structural metadata bracket** (the second positional argument to `[class ...]`). The `CloseBracket` ClassDecl handler extracts them post-hoc by inspecting `structural_metadata.entries` for entries whose key string is `"determines"` or `"resolver"`. This is a structural-bracket semantic extraction step, not an inline parse-loop routing step.

Add `structural_metadata: Option<Spanned<Expr>>` to `StackFrame::ClassDecl`. In the `push_expr_to_parent` ClassDecl arm, after `name` is set, route the next positional `Expr::Dict` to `structural_metadata` rather than erroring. The current arm at `parser.rs:~4819–4827` hard-errors on any second positional expression — this must be changed. Extract `determines:`, `resolver:`, `kinds:`, `superclasses:` from `structural_metadata.entries` in the `CloseBracket` ClassDecl handler.  
**Impact:** Moderate — AST extension + parser routing change.

### `src/typecheck.rs` — `Expr::ClassDecl` handler

**Current:** Parses `[class [Name a b c]  methods...]` with the class name embedded in the first bracket — the new syntax `Name: [class [a b c]  methods...]` extracts the name from the surrounding dict key instead. No `determines:` or `resolver:` key recognition.  
**Proposed:** After extracting `determines` and `resolver` from the AST: (1) validate `determines:` entries — each must be a 2-element list, first is a list of known param names, second is a name or list of names; (2) resolve param names to positional indices; (3) validate coverage and consistency conditions; (4) validate that `resolver` name exists in the type-stage Env and is callable (if the type-stage env is not yet populated at class declaration time, defer this check to first use — but emit a warning). A misspelled resolver name must produce a type error at class declaration time, not a silent runtime failure.  
**Impact:** Moderate — semantic validation + resolver name lookup.

### `src/ast.rs` / `src/parser.rs` — `Expr::InstanceDecl` redesign

**Current:** `Expr::InstanceDecl { class_name: String, instance_type: Box<Spanned<Expr>>, methods: Vec<Spanned<Entry>> }` — the class name and instance type are bundled in a bracket header; all methods are flat at the top level. `StackFrame::InstanceDecl` in `parser.rs` requires the first expression to be an `Expr::Dict(len>=2)` or `Expr::Call` (the header bracket); bare `VarRef` class names cause a parse error.

**Proposed — `Expr::InstanceDecl`:**

```rust
Expr::InstanceDecl {
    class_name: String,
    arms: Vec<(Spanned<Expr>, Vec<Spanned<Entry>>)>,  // (Expr::PatternDecl, method entries)
}
```

`instance_type` is removed (patterns carry type info); `methods` becomes per-arm. All exhaustive-match sites must be updated: `eval.rs`, `typecheck.rs`, `formatter.rs`, `desugar.rs`, `resolve.rs`, `lsp/analysis.rs`, `ast_dict.rs`, `expand.rs` (~8 files, mechanical arm addition).

**Proposed — `StackFrame::InstanceDecl`:** Replace `instance_type`/`methods`/`pending_key` fields with:

```rust
StackFrame::InstanceDecl {
    class_name: String,
    arms: Vec<(Spanned<Expr>, Vec<Spanned<Entry>>)>,
    pending_arm_key: Option<Spanned<Expr>>,   // [pattern [...]] waiting for ':'
    current_arm_methods: Vec<Entry>,            // accumulates method entries for current arm
    span_start: Position,
}
```

`pending_methods` (as previously sketched) is architecturally wrong — method dicts arrive as completed `Expr::Dict` nodes via `push_value`, not entry-by-entry. The `push_value` InstanceDecl arm must handle two distinct sub-cases:

1. `pending_arm_key.is_some()` AND the incoming value is an `Expr::Dict` — this is the **arm's method dict** (the bracket `[method-key: impl ...]` that follows `:`). Pair it with `pending_arm_key`, push `(key, entries)` to `arms`, clear `pending_arm_key` and `current_arm_methods`.
2. `pending_arm_key.is_some()` AND the incoming value is a scalar (a method implementation expression for an already-open method key in `current_arm_methods`) — accumulate into `current_arm_methods` via the normal pending-key mechanism. Note: `StackFrame::InstanceDecl` needs its own `pending_method_key: Option<Spanned<Expr>>` for individual method-key/value pairs within the arm's method dict, distinct from `pending_arm_key` (the pattern-arm level separator). Alternatively, the design simplifies by requiring method dicts to always be written as bracket forms `[+: impl  *: impl]` — the inner `StackFrame::Dict` handles key/value accumulation and delivers a completed `Expr::Dict` to the InstanceDecl frame. This is the recommended approach: no `current_arm_methods` accumulation needed; the `push_value` arm simply receives `Expr::Dict` and treats it as the arm's method dict.

**`push_expr_to_parent` InstanceDecl arm:** Add a `VarRef` branch that sets `class_name` (the proposed syntax has a bare class name, not a bracket-header).

**Colon handler for InstanceDecl:** The `:` in `[pattern [...]]:` is an arm separator. Update the error message from `"':' without a method name"` to `"':' without a pattern arm key or method name"` and handle both `Expr::PatternDecl` (arm key) and other expressions (method key) as distinct sub-cases.

**`InstanceDecl` Display impl** (`ast.rs:~563–575`): Note it uses the old bracket-header syntax and must be updated to render the match-arm form.

**Impact:** Breaking AST change; ~8 exhaustive-match sites; two StackFrame fields removed, three added.

### `src/parser.rs` — `StackFrame::PatternDecl`

**Current:** No `StackFrame::PatternDecl` or `pattern` keyword exists.  
**Proposed:** A new keyword `pattern` pushes `StackFrame::PatternDecl`, which works exactly like `StackFrame::Fn` but collects only a binding list — no body:

```rust
StackFrame::PatternDecl {
    bindings: Vec<Spanned<Expr>>,   // the name@TypePattern entries
}
```

**Parsing sequence:**

1. `[pattern` → push `StackFrame::PatternDecl { bindings: [] }`
2. The following `[...]` bracket opens a standard Dict frame producing `Expr::Annotated` nodes (via the existing `ImmediateAt` mechanism — `a@Integer`, `a@[Seq elem]`, etc. are parsed as annotated identifiers). This is NOT the same path as `StackFrame::Fn` params, which uses the eager synchronous `parse_param_list()` function before the frame is pushed. `PatternDecl` uses the iterative frame protocol; `push_expr_to_parent` for the frame converts the `Expr::Annotated` nodes into `bindings: Vec<Spanned<Expr>>`.
3. Inner brackets within annotations (`a@[Seq elem]`, `c@[or Int Null]`) are parsed recursively as composite type expressions using the same annotation bracket rules already implemented
4. No body expression is collected (unlike `Fn`)
5. `]` closes → `Expr::PatternDecl { bindings }` — a complete expression that can serve as a dict key

**No new parsing mechanisms required.** The `pattern` keyword is lexed as `Token::Identifier("pattern")` and recognized in the same dispatch table as `fn`, `match`, `class`, `instance`. **Colon-ahead rejection rule:** Like all keyword dispatch arms, the `pattern` keyword dispatch must include the guard `!matches!(peek_next_horizontal(...), Some((Token::Colon, _)))` so that `[pattern: x]` remains a valid dict entry rather than being parsed as a malformed `PatternDecl` frame. `Expr::Annotated` nodes (from `name@TypeExpr`) are already produced by the parser.

**Impact:** Minor — new keyword recognition + StackFrame variant that reuses existing fn-param machinery; no new token types or parsing modes.

### `src/ast.rs` — `Expr::PatternDecl`

**Current:** No `Expr::PatternDecl` variant.  
**Proposed:**

```rust
Expr::PatternDecl {
    bindings: Vec<Spanned<Expr>>,   // each is Expr::Annotated { name, annotation }
}
```

Used as the arm key in `Expr::InstanceDecl`. Also usable as arm key in `Expr::Match` (optional, for the unified form). The existing `StackFrame::Match` arm handling (`pending_pattern_expr`) already accepts any expression as a key — `Expr::PatternDecl` slots in with zero match changes.

**Impact:** Minor — one new AST variant; updates to the AST match arms in `src/typecheck.rs` and `src/formatter.rs` (mechanical).

### `src/typecheck.rs` — `Expr::InstanceDecl` handler

**Current:** No `[instance ...]` expression form; instances are not user-declarable.  
**Proposed:** Validate and register instances from `Expr::InstanceDecl { class_name, arms }`. For each arm: (1) the type-parameter count must match the class's declared param count; (2) disjointness is checked against all previously registered arms for the same class (pairwise unification of type-parameter lists); (3) coverage and consistency conditions are checked for classes with FDs; (4) each method key must correspond to a method declared in the class body; (5) each method implementation is typechecked against the expected method signature with the arm's type parameters substituted.

**Scope-aware class and instance registration** (design constraint from the unified-bindings whatif):

Classes and instances are **values in scope**, not entries in global registries. `Addable: [class ...]` places the ClassDecl in the local TypeEnv as a value — the same scoping mechanism as any other dict entry. Two dicts defining independent `Addable` classes have independent class environments. `[instance Addable ...]` registers instance arms in the scope-local InstanceEnv, not a global one.

When `[$Addable a b c]` is processed as a constraint, `$Addable` uses the `$`-sigil scope reference to resolve the Addable VALUE from the current scope. The resulting `Constraint::Class` stores the ClassDecl directly (not just a string name) — this is how `improve_functional_dependency` accesses the FD info without a global ClassEnv lookup.

Concretely:

- `ClassEnv` is **not** a global `HashMap<String, ClassDecl>`. It is scope-resident: classes are looked up via the TypeEnv, the same as type aliases and other type-environment entries.
- `InstanceEnv` is similarly scope-local. Instances declared in one dict don't automatically apply in another dict's scope. To share instances across dicts, they are imported via normal scoping.
- `Constraint::Class { class: ClassDecl, vars: Vec<String> }` — the constraint carries the ClassDecl directly, extracted from the scope-resident value at constraint-creation time. No string-keyed global lookup needed at resolution time.

This design is consistent with the "no global registries" principle: everything follows scoping rules.

**Impact:** Moderate — new AST node, new parser stack frame, new typecheck handler; ClassEnv and InstanceEnv are scope-resident structures (TypeEnv entries), not global HashMaps.

### `src/type_unify.rs` — `improve_functional_dependency`

**Current:** Calls `lookup_arithmetic_instance()` — hardcoded 9-entry match on `(type_key(a), type_key(b))`.  
**Proposed:** Look up `class_decl.resolver` in the type-stage Env; if present, convert determining `Type::*` values to type dicts (with literal widening), call `eval(resolver_fn, dicts, type_stage_env)`, convert result back to `Type::*`, unify. Fall back to `lookup_arithmetic_instance` when resolver is absent (arithmetic built-ins).  
**Impact:** Moderate — requires Type ↔ type dict conversion at unification time; access to type-stage Env from unifier.

### `src/type_infer.rs` — Deferred equality queue

**Current:** No deferred equality mechanism.  
**Proposed:** Add `deferred_equalities: Vec<(Type, Type)>` to `InferState`. Populated by Case 1 of `unify_normalized` for non-injective resolvers (where `ClassDecl.resolver_injective = false`). Processed after each call to `unify()`:

```rust
fn unify(a: Type, b: Type, subst: &mut Substitution, state: &mut InferState, span: Span) -> Result<(), TypeError> {
    let norm = NormCtxt::from(subst, state);
    let a' = normalize(a, &norm);
    let b' = normalize(b, &norm);
    unify_normalized(a', b', subst, state, span)?;
    // Process deferred equalities — retry any that may now be reducible
    let mut i = 0;
    while i < state.deferred_equalities.len() {
        let (lhs, rhs) = &state.deferred_equalities[i];
        let lhs' = normalize(lhs.clone(), &NormCtxt::from(subst, state));
        let rhs' = normalize(rhs.clone(), &NormCtxt::from(subst, state));
        if !lhs'.has_type_stage_app() && !rhs'.has_type_stage_app() {
            state.deferred_equalities.remove(i);
            unify(lhs', rhs', subst, state, span)?;  // concrete ~ concrete
        } else {
            i += 1;
        }
    }
    Ok(())
}
```

**Termination:** The loop terminates because concrete-concrete `unify()` cannot trigger Case 1 of `unify_normalized` (both sides are concrete, not TypeStageApp). If the recursive `unify(lhs', rhs')` at line 1415 encounters a new non-injective `TypeStageApp` pair at a deeper call, those are appended to `state.deferred_equalities` — the outer loop will see them on subsequent iterations (correct, intentional). The queue drains monotonically: the substitution grows only, so args that are TypeVars today either remain TypeVars (entry stays deferred) or become ground (entry fires and is removed); entries are never re-added with the same TypeVars. **Worst-case complexity:** O(k × n) where k = max queue depth and n = constraint count. For config-language programs, k is bounded to a small constant because direct comparisons of two arithmetic subexpressions (the only trigger for Case 1) are rare.

**At let-generalization time:** any remaining deferred equalities whose `TypeStageApp` nodes contain only generalized TypeVars are **discarded** — they will be re-established at each call site when the scheme is instantiated and those TypeVars become ground. This is correct: the deferred equality `(TypeStageApp("F", [a, b]), TypeStageApp("F", [c, d]))` with generalized `a,b,c,d` becomes `(TypeStageApp("F", [a', b']), TypeStageApp("F", [c', d']))` with fresh instances at each call site, where FD improvement resolves them correctly.

**Impact:** Small addition to `InferState` and `unify()`; ~25 lines.

### `src/type_unify.rs` — BAS deferral

**Current:** `all_det_ground` check uses `!ty.has_inference_vars()` — fires for any concrete type including unions.  
**Proposed:** Strengthen to require atomic named monotypes in all determining positions before firing.  
**Impact:** Minor — predicate change; prevents silent improvement failures on union types.

### `src/types.rs` — Generalization with FD constraints

**Current:** `generalize()` does not consider FD constraints. Determined TypeVars that remain free at generalization time are generalized independently (incorrect for FD semantics).  
**Proposed:** No change to `generalize()` for the determined-var case — the Jones (1995) qualified types model generalizes `c` alongside `a` and `b`, with the constraint `Add a b c` included in the scheme. The one addition: at constraint-creation time for MPTC constraints, lower the determined TypeVar's level to `max(enclosing_level, max(l_a, l_b))` so it cannot escape into an outer scope without the constraint, and cannot be generalized beyond the scope of its determining TypeVars. This is a small change to the MPTC constraint registration path.  
**Impact:** Minor — level assignment at constraint creation; no changes to the generalization algorithm itself.

### `stdlib/prelude.llt` — Arithmetic class migration

**Current:** `Add`, `Sub`, `Mul`, `Div` pre-registered in Rust (`src/types.rs:1686-1707`) with no methods and a hardcoded lookup table (`lookup_arithmetic_instance`).  
**Proposed:** Declare in `stdlib/prelude.llt` with `determines:`, `resolver:`, and method declarations. Arithmetic instances declared as `[instance ...]` blocks using match-arm syntax. The 9 primitive instances become arms under `[instance Addable ...]`, `[instance Subtractable ...]`, etc., using `builtin-add`/`builtin-sub`/`builtin-mul`/`builtin-div` as implementations. The Rust lookup table (`lookup_arithmetic_instance`) is **retained as a performance fast path** — when the class is a known built-in arithmetic class, the O(1) match table is used instead of calling `eval()`. The resolver call path is used only for user-declared classes.  
**Impact:** Major structurally (moves class/instance to tinct); Minor for runtime performance (fast path preserved for arithmetic).

### `src/typecheck.rs` — Post-inference boundary guard elaboration pass

**Current:** `ThunkState::Guarded` nodes are inserted only at explicit `[@Type expr]` TypeAssert sites, inline during `infer_expr`. `Unknown → Concrete` boundaries at call arguments, builtin args, field accesses, and `---` crossings produce no guards — mismatches surface only at the point of forced materialization with no blame provenance.  
**Proposed:** After `infer_dict` completes (all TypeVars ground, full substitution available), run `elaborate_boundary_guards` — a post-inference pass that writes guard annotations into AST `RefCell` fields (the same mechanism used by `Expr::TypeAssert`'s `resolved_type: RefCell<Option<Type>>`), not a thunk-wrapping operation. Thunks do not exist during typecheck; `eval()` creates `Guarded` thunks when it reads these annotations during its normal AST walk.

For each expression where the inferred type is `Unknown` and the contextual expected type is concrete: call `normalize(expected, NormCtxt::final(...))` to reduce any remaining `TypeStageApp` nodes, assert the result is `is_concrete` (defined as: not `TypeVar`, not `TypeStageApp`, not `Unknown` — union/intersection/named types all qualify), then write the normalized expected type into the expression's guard annotation `RefCell`. `eval()` reads this annotation and wraps the expression's result thunk in `Guarded(inner, expected_concrete, BlameLabel)`.

`---` boundary crossings: inject a call to `wrap_with_nominal_validation()` (already used for explicit `expects:` pragma guards) at each `---` where downstream expected types are concrete and upstream bindings are `Unknown`. The existing `GuardedValidate` continuation in `eval_materialize.rs` handles `Value::Overlay` correctly via `guard_ctx` extraction.

Polarity: `Negative` for argument positions (untyped provider blamed), `Positive` for return-value consumers. `---` pipeline crossings: `Positive`. The upstream section is treated as a *producer* (analogous to a function returning an Unknown value); the downstream section that specifies the type expectation carries the boundary label. This matches co-natural blame: when the value later crosses a second boundary, the inner (most recent) label is kept — the downstream section's label is the most actionable, pointing at where the typed expectation was imposed. The fix is to annotate or correct the upstream producer, but the boundary is labelled at the consumer. This is the standard treatment for return-value blame (Wadler & Findler 2009).

The co-natural O(1) space claim requires that when constructing `Guarded { inner, ... }` and `inner`'s state is already `Guarded { inner: inner2, ... }`, use `inner2` as the actual inner thunk — collapsing the nesting. Without this optimization, N boundary crossings create O(N) nested `Guarded` thunks. Either implement the constructor optimization or note the O(N) cost is acceptable if boundaries are rare.  
**Impact:** New pass after inference; requires `NormCtxt` construction from the final substitution; adds `elaborate_boundary_guards(type_map, subst, type_stage_env) -> Result<(), TypeError>` to `src/typecheck.rs` or a new `src/typecheck_elaborate.rs`.

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
- Greenman, B., Felleisen, M. & Dimoulas, C. (2019). "Complete Monitors for Gradual Types." *Proc. ACM Program. Lang.* 3, OOPSLA, Article 122. doi:10.1145/3360548. — [co-natural blame strategy; O(1) space overhead for boundary guards; proves co-natural is sufficient for the blame theorem]
- Stuckey, P.J. & Sulzmann, M. (2005). "A Theory of Overloading." *ACM TOPLAS*, 27(6), 1216–1269. — [CLP(H) foundation for CHR-based typeclass resolution; formal basis for constraint store and improvement]
- Sulzmann, M., Duck, G.J., Peyton Jones, S. & Stuckey, P.J. (2007). "Understanding Functional Dependencies via Constraint Handling Rules." *Journal of Functional Programming*, 17(1), 83–129. — [foundational CHR unification of FDs and type families; Theorem 4.2 (confluence); coverage and consistency conditions; the theoretical basis for this design]
- Wadler, P. & Findler, R.B. (2009). "Well-Typed Programs Can't Be Blamed." *ESOP '09*, LNCS 5502, pp. 1–16. — [blame theorem; proves well-typed components are never blamed; foundation for polarity assignment in boundary guards]
