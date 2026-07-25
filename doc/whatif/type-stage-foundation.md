# What If: Type-Stage Programming as the Foundation for Constructors, Typeclasses, and Pattern Matching

**State:** Proposal

What would it take to replace tinct's hardwired Rust implementations of type declarations, typeclass dispatch, and constructor pattern matching with type-stage tinct programs — reducing the Rust runtime to a genuinely minimal core and making the language self-describing?

## Goals

1. **Constructors are functions.** A named-field constructor `[ProgramItem.File path: "x" handle: h]` is an ordinary function call. No Rust special-casing in the lowerer.
2. **Pattern matching matches shape, not mechanism.** The `[case [let p] [Ctor p] body]` form works for any value implementing the Constructor protocol — auto-generated or user-defined — without AST heuristics.
3. **Typeclasses are tinct programs.** `[class ...]` and `[instance ...]` are type-stage macros that expand to dict-passing code, not Rust data structures.
4. **`[type ...]` is a type-stage macro.** A type declaration generates Constructor instances and registers nominal types — all expressible in tinct, not hardwired in `inject_adt_constructors_expr`.
5. **Minimal Rust core.** The runtime needs ~20 primitives and the CEK machine. Everything else — typeclasses, constructors, pattern protocols — lives in tinct and is understandable to users.

## Current State

Today, tinct has several hardwired Rust mechanisms that should be expressible in the language itself:

**Constructor generation (`lower.rs:inject_adt_constructors_expr`):** `[type [File path: String handle: File] ...]` emits `CoreExpr::Fn` for named-field constructors. The lowerer contains hundreds of lines of Rust to generate payload dicts, handle annotated constructors, detect unit vs named-field, etc. None of this is introspectable or extensible in tinct.

**Pattern matching heuristics (`eval_materialize.rs:eval_structural_pattern_inner`):** The 3-arg case arm evaluator uses fragile heuristics to distinguish constructors from predicates — previously checking for uppercase initial characters, then field-get de Bruijn slots, then evaluating to check if the result is a Variant. These heuristics break for named-field constructors (which evaluate to Functions, not Variants), arbitrary dot-access paths, and user-defined smart constructors.

**Typeclass dispatch (resolve.rs, typecheck.rs, lower.rs):** Class and instance declarations are processed by multiple Rust passes. Method dispatch writes to `CallDispatch` cells during type-checking, and the lowerer rewrites calls. This is correct but entirely opaque to tinct programs.

**Type checker registration:** Nominal types and their constructors are registered in Rust data structures (`TyConDef`, `tycon_env`) with no tinct-level equivalent.

### What's Missing

1. No way for users to write smart constructors that participate in pattern matching with constructor semantics.
2. No way for the language to explain its own type system primitives — they're hidden in Rust.
3. The Constructor concept has no tinct-level representation: you cannot write a tinct function that says "I'm the constructor for Shape.Circle."
4. Pattern matching on named-field constructors is broken: evaluating `ProgramItem.File` gives a Function, which hits predicate semantics, not constructor matching.

## Core Insight: Constructors Are Functions

A constructor and a function have identical shape: parameters and a return type. The only difference is that a constructor's return type is a **specific named Variant** — always the same tag. A function's return type may be any type.

This means:

```tinct
# Auto-generated constructor
Shape.Circle: [fn@Shape.Circle [let radius@Float]
  [builtin-make-variant "Shape.Circle" [radius: radius]]]

# User-defined smart constructor — same return type, same semantics
make-circle: [fn@Shape.Circle [let radius@Float]
  [builtin-make-variant "Shape.Circle" [radius: [max 0 radius]]]]
```

Both `Shape.Circle` and `make-circle` have `return_ann: Shape.Circle`. Pattern matching on either should check whether the scrutinee is a `Shape.Circle` variant and extract accordingly.

## The Polarity Problem

Pattern matching exposes a fundamental polarity mismatch:

- A **predicate** tests whether the scrutinee matches the function's **input shape**: `[int? x]` asks "does x satisfy int?'s input type?"
- A **constructor pattern** tests whether the scrutinee matches the function's **return shape**: `[Shape.Circle p]` asks "was this value produced by Shape.Circle?"

These cannot be unified by "call the function with the scrutinee." Calling a constructor with the scrutinee as input tests the wrong thing.

The resolution: **constructors carry their own inverse**. Construction and deconstruction are two halves of the same protocol.

## Design

### The Constructor Protocol

```tinct
Constructor: [class [let Constructor a]
  [tag:         [Fn@String    []]
   construct:   [Fn@a         [...]]
   deconstruct: [Fn@[Maybe a] [Unknown]]]]
```

- `tag` — the Variant tag this constructor produces (`"Shape.Circle"`)
- `construct` — the builder: takes arguments, returns a Variant
- `deconstruct` — the extractor: takes a value, returns the payload if it matches, null otherwise

`deconstruct` IS the inverse. It defines the polarity flip. For auto-generated constructors, `deconstruct` checks `builtin-tag-of` and returns `builtin-variant-payload`. For user-defined constructors, the user supplies an appropriate deconstruct.

### `[type ...]` as a Type-Stage Macro

The `[type [Circle radius: Float] [Square side: Float]]` declaration expands to:

```tinct
# 1. Nominal type registration (type-stage)
[type-register Shape [Circle radius: Float] [Square side: Float]]

# 2. Constructor protocol instances (generated by macro, normal tinct)
[instance Constructor [let a@Shape.Circle]
  [tag:         [fn [] "Shape.Circle"]
   construct:   [fn [let radius@Float]
                  [builtin-make-variant "Shape.Circle" [radius: radius]]]
   deconstruct: [fn [let v]
                  [if [= [builtin-tag-of v] "Shape.Circle"]
                    [builtin-variant-payload v]
                    []]]]]

[instance Constructor [let a@Shape.Square]
  [tag:         [fn [] "Shape.Square"]
   construct:   [fn [let side@Float]
                  [builtin-make-variant "Shape.Square" [side: side]]]
   deconstruct: [fn [let v]
                  [if [= [builtin-tag-of v] "Shape.Square"]
                    [builtin-variant-payload v]
                    []]]]]
```

The macro generates these instances. `type-register` is the only type-stage primitive — it tells the type checker about the nominal type and its constructors' signatures. Everything else is standard tinct.

### Typeclasses as Type-Stage Macros

`[class ...]` and `[instance ...]` are themselves type-stage macros that expand to:

```tinct
# [class [let Equatable a]] expands to:
# - A protocol dict type definition
# - A resolver registration (so [= x y] finds the right instance)
# - Standard tinct dict-passing glue

# [instance Equatable [let a@Integer]: [=: [fn [let x y] [builtin-eq x y]]]]
# expands to:
# - A dict value with the method implementations
# - Registration in the instance dispatch table
```

The Rust runtime provides only the CEK machine and primitives. All typeclass semantics — class declarations, instance selection, method dispatch — are tinct programs.

### Pattern Matching Semantics

The `eval_structural_pattern_inner` Call arm becomes clean:

```
[case [let bindings] [EXPR pattern-args] body]
```

1. Evaluate EXPR
2. Branch on the VALUE TYPE of the result:
   - `Value::Variant{tag, payload:None}` — unit constructor: check scrutinee tag against this tag
   - `Value::Constructor{...}` — or any value implementing Constructor protocol: call `deconstruct(scrutinee)`, if truthy use result for payload bindings
   - `Value::Function | Value::Builtin` — predicate: call EXPR(scrutinee), check truthy, bind declared names to scrutinee

3. **Named args in the pattern** (`path: x handle: y`) are NEVER call arguments — they are field binding declarations. After `deconstruct` returns the payload dict, named args extract specific fields by key.

4. **Single positional arg** (`p`) binds to the whole payload value.

5. **Zero args** is tag-check only; no payload binding.

No heuristics. No AST analysis. The runtime type of the evaluated EXPR determines semantics.

### User-Defined Smart Constructors in Patterns

A user can write a smart constructor that participates in pattern matching by implementing Constructor:

```tinct
[
  # Smart constructor: validates and normalizes
  make-circle: [fn@Shape.Circle [let radius@Float]
    [builtin-make-variant "Shape.Circle" [radius: [max 0 radius]]]]

  # To use in patterns, declare a named Constructor instance:
  SmartCircle: [instance Constructor [let a@Shape.Circle]
    [tag: [fn [] "Shape.Circle"]
     construct: make-circle
     deconstruct: [fn [let v]
       [if [= [builtin-tag-of v] "Shape.Circle"]
         [builtin-variant-payload v]
         []]]]]
]

# Pattern matching using the smart constructor:
[match shape
  [case [let p] [SmartCircle p] [str "circle r=" [str p.radius]]]
  [case [let p] [Shape.Square p] [str "square s=" [str p.side]]]]
```

`Shape.Circle` and `SmartCircle` match the same values (same tag). The user controls deconstruction.

### What the Minimal Rust Runtime Needs

Primitives only — no type-system knowledge:

- Value types: `Variant`, `Dict`, `Int`, `Float`, `String`, `Bytes`, `Function`, `Builtin`
- CEK machine and evaluation loop
- ~20 core primitives: `builtin-make-variant`, `builtin-tag-of`, `builtin-variant-payload`, arithmetic, dict operations, I/O, etc.
- Type-stage execution: ability to run `--- stage: type` documents before runtime
- The `type-register` primitive: tells the type checker about a nominal type (name → constructor signatures)

Everything else — `[class ...]`, `[instance ...]`, `[type ...]`, Constructor protocol, pattern matching — lives in tinct.

## Decidable P: A General Narrowing Foundation

### The Problem

Type narrowing in `if` bodies requires the type checker to know, when checking the true branch, that the condition was true — and, from that, what additional type information can be inferred about variables in scope.

The broken approach: inspect the condition AST and special-case known function names (`int?`, `str?`, `= [type-of x] "Int"`, etc.). This is an axiom violation — Rust knowing about a prelude function name — and it cannot generalize to user-defined predicates.

The correct approach: encode the narrowing in the **return type** of the predicate. The type checker reads the return type; if it witnesses a proposition, it applies that proposition as a narrowing. No function names, no AST heuristics.

### The Decidable Type

```tinct
# A proposition P is decidable if it can be either proven or refuted.
# Decidable P is a tagged witness: Yes carries a proof of P, No carries a proof of ¬P.
Decidable: [type [let p]
  [Yes proof: p]
  [No  refutation: [Not p]]]
```

`Decidable P` is not a runtime value in the ordinary sense — `p` is a **phantom type parameter**, a proposition expressed at the type level. Its constructors carry proof terms. At runtime, `Decidable.Yes` and `Decidable.No` are plain variants; the proof field is a runtime witness (or erased by the compiler). At the type level, the field's type IS the proposition.

`Boolean` is `Decidable Unit` — the specialization where the proposition is trivial ("I decided something, but the proposition carries no information"). `Decidable.Yes proof: []` is `Boolean.True`; `Decidable.No refutation: []` is `Boolean.False`. There is one type, not two. See §Boolean is Decidable Unit below.

### Predicates Encode Their Proposition

Under this design, the type predicate builtins change return type to encode what was decided:

```tinct
# Current: returns Decidable Unit — no proposition encoded, just a truth value
int?: [fn@Boolean [let x] ...]   # Boolean = Decidable Unit

# Proposed: return type encodes the specific proposition
int?: [fn@[Decidable [Int? %0]] [let x] ...]
# [Int? %0] is the proposition "parameter 0 is an Int"
# %0 is a positional reference to the first parameter at the type level
```

`[Int? %0]` is a **type-level proposition** — a phantom type that the type checker understands to mean "the scrutinee is an Int." It carries no runtime representation. The checker treats `Decidable.Yes` as introducing `[Int? x]` as a narrowing for `x` in the true branch.

Type predicates encode a specific proposition; plain comparisons and runtime tests return `Decidable Unit` (= `Boolean`) since the proposition is not structurally useful to the type checker:

| Expression | Return type | Proposition introduced |
|------------|-------------|------------------------|
| `[int? x]`    | `Decidable [Int? %0]`    | `x : Int` in true branch    |
| `[str? x]`    | `Decidable [Str? %0]`    | `x : Str` in true branch    |
| `[dict? x]`   | `Decidable [Record? %0]` | `x : Record(open)` in true branch |
| `[fn? x]`     | `Decidable [Fn? %0]`     | `x : Callable` in true branch  |
| `[null? x]`   | `Decidable [Null? %0]`   | `x : Null` in true branch   |
| `[seq? x]`    | `Decidable [Seq? %0]`    | `x : Seq(Unknown)` in true branch |
| `[= x 5]`     | `Decidable Unit`          | none (plain boolean)        |
| `[> x 0]`     | `Decidable Unit`          | none (plain boolean)        |

### How `if` Uses It

`if` is typed to consume `Decidable P` and pass the proof/refutation to each branch:

```tinct
if: [fn@a [let d@[Decidable p] yes@[fn p a] no@[fn [Not p] a]]
  [match d
    [Decidable.Yes payload]: [yes payload.proof]
    [Decidable.No  payload]: [no  payload.refutation]]]
```

When the type checker sees `[if cond then-expr else-expr]`:

1. Infer `cond` — if the result type is `Decidable P`, extract proposition `P`
2. Type-check `then-expr` in an environment extended with `P` as a narrowing
3. Type-check `else-expr` in an environment extended with `¬P` as a narrowing

No special-casing by name. Any function whose return type is `Decidable P` grants narrowing to `if` bodies — including user-defined predicates.

```tinct
# User-defined predicate — gets narrowing for free
is-admin?: [fn@[Decidable [IsAdmin? %0]] [let user@Dict]
  [if [= user.role "admin"]
    [Decidable.Yes proof: [AdminProof user]]   # runtime proof witness
    [Decidable.No  refutation: []]]]

# The type checker narrows user to [IsAdmin? user] in the true branch
has-permission: [fn [let user]
  [if [is-admin? user]
    [grant-all-access user]   # user narrowed to IsAdmin? user here
    [grant-basic-access user]]]
```

### How `match` Uses It — Connecting to the Constructor Protocol

The Constructor protocol's `deconstruct` method currently returns `Maybe payload` — either the payload dict or null. This is informationally identical to `Decidable (v is Shape.Circle)`, but without the type-level proposition.

Under the `Decidable` design, `deconstruct` returns `Decidable`:

```tinct
Constructor: [class [let Constructor a]
  [tag:         [Fn@String   []]
   construct:   [Fn@a        [...]]
   deconstruct: [Fn@[Decidable [Ctor? %0 a]] [Unknown]]]]
```

The proposition `[Ctor? %0 a]` means "the scrutinee was produced by constructor `a`." When the pattern match arm evaluates `deconstruct(scrutinee)` and it returns `Decidable.Yes`, the type checker narrows the scrutinee's type to the constructor's payload type — exactly as it does for `if`.

The auto-generated Constructor instance:

```tinct
[instance Constructor [let a@Shape.Circle]
  [tag:         [fn [] "Shape.Circle"]
   construct:   [fn [let radius@Float]
                  [builtin-make-variant "Shape.Circle" [radius: radius]]]
   deconstruct: [fn [let v]
                  [if [= [builtin-tag-of v] "Shape.Circle"]
                    [Decidable.Yes proof: [builtin-variant-payload v]]
                    [Decidable.No  refutation: []]]]]]
```

`match` arm evaluation becomes: call `deconstruct(scrutinee)`, get `Decidable (Ctor? v Shape.Circle)`, and check if `Decidable.Yes`. If so, the payload type is known and the arm body is type-checked with the scrutinee narrowed to `Shape.Circle`.

### The Unified Mechanism

Both `if` and `match` are consumers of `Decidable P`:

- **`if`**: condition has type `Decidable P`; branches receive `P` and `¬P` as narrowings
- **`match` arm**: `deconstruct` has type `Decidable (Ctor? v T)`; arm body receives payload narrowing

The type checker's narrowing logic reduces to a single rule:

> **When a `Decidable P` value branches, the true path is type-checked with `P` in scope as a narrowing, and the false path with `¬P` in scope.**

This rule applies uniformly to `if`, `match`, and any other construct built on `Decidable`. It requires no knowledge of specific function names, no AST pattern matching on conditions, and no special registration of predicates.

### Boolean is Decidable Unit

`Decidable.Yes` with a trivial proposition is exactly `Boolean.True`. In prelude, `Boolean` is a type alias for `Decidable Unit`:

```
Decidable.Yes proof: []      ≡   Boolean.True    ([] is the unit/trivial proof)
Decidable.No  refutation: [] ≡   Boolean.False
```

`Boolean` does not need its own `[type ...]` declaration. `Boolean.True` and `Boolean.False` become qualified names for `Decidable.Yes` and `Decidable.No` with unit payload.

**No coercion is needed.** `filter`, `any`, `all`, `and`, `or`, `not` all accept `Decidable p` uniformly. When `p = Unit`, they operate on plain booleans with no type narrowing. When `p = [Int? x]`, the proposition flows through:

```tinct
[and [int? x] [= x 5]]
# [int? x] : Decidable [Int? x]
# [= x 5]  : Decidable Unit
# and       : Decidable [And [Int? x] Unit]
# [And P Unit] reduces to P — Unit adds no information
# result    : Decidable [Int? x]
```

**Proposition forgetting** — the only "coercion" is `Decidable P → Decidable Unit`, erasing the proposition while keeping the truth value. As a `Castable` instance:

```tinct
[instance Castable [let target@Boolean source@[Decidable p]]
  [cast: [fn [let d]
    [match d
      [Decidable.Yes _]: Boolean.True
      [Decidable.No  _]: Boolean.False]]]]
```

Call site: `[@Boolean [cast d]]` or just `[cast d]` when the target type is inferred.

**Runtime representation change.** `Boolean.True` currently produces `Variant{tag: "Boolean.True", payload: None}`. Under this design it becomes `Variant{tag: "Decidable.Yes", payload: {proof: []}}`. All match arms on `Boolean.True`/`Boolean.False` migrate to `Decidable.Yes`/`Decidable.No`. The `bool->int` bridge in prelude checks `"Decidable.Yes"` instead of `"Boolean.True"`. This is a significant but principled migration — tinct has no backwards compatibility obligation.

### What This Requires

This design is a significant type system extension beyond what tinct currently supports:

1. **Type-level propositions** — phantom types `[Int? %0]`, `[Ctor? %0 a]` that the type checker interprets as narrowings. These are new syntax at the type level, not expressible in current tinct annotations.

2. **Positional references in return type annotations** — `%0` referring to the first parameter value. This is a limited form of dependent return type: the return type mentions an input parameter. Full dependent types are not required — only the specific case of `Decidable P` where `P` mentions parameters.

3. **Negation propositions** — `[Not p]` as the negation of a proposition. For structural narrowings (type predicates), `¬(x : Int)` in the false branch means the type checker knows `x` is not `Int`, which enables proper false-branch narrowing without the current Unknown-skip guard.

4. **Boolean as type alias** — `Boolean` in prelude becomes `Decidable Unit` rather than an independent `[type True False]` declaration. The `Boolean.True`/`Boolean.False` constructors are aliases for `Decidable.Yes`/`Decidable.No` with unit payload. The `bool->int` bridge, pattern matching, and all code that currently checks `"Boolean.True"` must migrate to `"Decidable.Yes"`.

The payoff: a single mechanism powers narrowing for `if`, `match`, and any user-defined construct that returns `Decidable`. No Rust special-cases. User-defined predicates get narrowing for free by declaring the right return type.

### References

- Wadler, P. (1989). "Theorems for free!" *FPCA '89*. — [phantom type parameters and propositions-as-types]
- Pfenning, F. & Davies, R. (2001). "A judgmental reconstruction of modal logic." *Mathematical Structures in Computer Science.* — [proof terms as first-class values]
- The Lean 4 Reference Manual §6.2. "If-then-else with Decidable." — [if h : P then ... else ... as the practical spelling of proof-passing if]
- Garcia, R. et al. (2016). "Abstracting Gradual Typing." *POPL '16*. — [Decidable as a refinement of Bool in a gradual setting]

## What Would Change

### lower.rs — `inject_adt_constructors_expr`

**Current:** ~200 lines of Rust generating `CoreExpr::Fn` and `CoreExpr::Variant` for each constructor, with special handling for annotations, unit vs named-field, `ConstructorInfo`, etc.

**Proposed:** Eliminated. The `[type ...]` macro generates Constructor instances in tinct. The lowerer sees only standard tinct code.

**Impact:** Fundamental — the function disappears entirely.

### eval_materialize.rs — `eval_structural_pattern_inner` Call arm

**Current:** Heuristics for detecting constructor vs predicate (was: uppercase check; became: field-get slot detection; then: evaluate to Variant). Breaks for named-field constructors (Functions).

**Proposed:** Evaluate EXPR; branch on `Value::Constructor` vs `Value::Function/Builtin`. No heuristics. Call `deconstruct` for constructors, call directly for predicates.

**Impact:** Major — the arm simplifies significantly.

### typecheck.rs / resolve.rs — Typeclass dispatch

**Current:** `CallDispatch` cells written during type-checking, rewritten by lowerer. Multiple Rust passes cooperate to route method calls to the right instance.

**Proposed:** Type-stage tinct programs generate instance dicts. Dispatch is ordinary dict lookup in tinct.

**Impact:** Fundamental — the Rust typeclass machinery is replaced by tinct code.

### eval.rs — Variant calling convention

**Current:** Added a special arm for `Value::Variant{payload:None}` called with named args (to handle named-field constructors that were reverted from Function form). This arm is incorrect.

**Proposed:** Remove the named-arg Variant calling arm. Named-field constructors are Functions again. Construction is a normal function call. The Constructor protocol handles pattern extraction separately.

**Impact:** Minor — remove the mistaken arm.

### value.rs — `Value::Constructor`

**Current:** Does not exist. `Value::Variant{payload:None}` is overloaded as both "unit constructor at rest" and "named-field constructor at rest."

**Proposed:** `Value::Constructor { tag, construct, deconstruct }` as a distinct value type — or equivalently, a tinct dict implementing the Constructor protocol. Pattern matching detects this and calls `deconstruct`.

**Impact:** Moderate — new value type or protocol dispatch.

## Prerequisites

- Type-stage evaluation (`--- stage: type` documents executing before runtime) — partially implemented
- Macro system capable of expanding `[type ...]` and `[class ...]` — see `doc/feature/macros.md`
- `builtin-make-variant`, `builtin-tag-of`, `builtin-variant-payload` primitives — mostly exist
- `type-register` primitive for type checker registration — new primitive, replaces Rust type registration
- **Type-level propositions** — phantom types `[Int? %0]`, `[Ctor? %0 a]` with narrowing semantics; new type annotation syntax, not expressible today
- **Positional parameter references in return type annotations** — `%0`, `%1` referring to parameter values; limited dependent return type needed only for `Decidable` return types
- **Negation propositions** — `[Not p]` in the type system; required for false-branch narrowing via `Decidable.No`

## References

- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." *POPL '89*. — [original typeclass paper; the dictionary-passing translation we're implementing in tinct]
- Odersky, M. et al. (2007). "An Overview of the Scala Programming Language." — [extractors/unapply as the dual of constructors]
- Haskell Report §4.3. Class and Instance Declarations. — [class/instance as compilation to dicts]
- Liskov, B. & Zilles, S. (1974). "Programming with Abstract Data Types." — [protocol as the right abstraction boundary]
