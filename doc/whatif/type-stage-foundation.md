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

# [instance Equatable [let a@Int]: [=: [fn [let x y] [builtin-eq x y]]]]
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

## References

- Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." *POPL '89*. — [original typeclass paper; the dictionary-passing translation we're implementing in tinct]
- Odersky, M. et al. (2007). "An Overview of the Scala Programming Language." — [extractors/unapply as the dual of constructors]
- Haskell Report §4.3. Class and Instance Declarations. — [class/instance as compilation to dicts]
- Liskov, B. & Zilles, S. (1974). "Programming with Abstract Data Types." — [protocol as the right abstraction boundary]
