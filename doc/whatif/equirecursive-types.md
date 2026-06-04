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

Two problems persist that TyCon references alone cannot address.

**Inline recursive type annotations.** A recursive type used at an annotation site without a named alias has no way to express the self-reference. Users must always create a named alias first, even for single-use structural patterns:

```tinct
# Forced to name the type just to annotate one function parameter
TreeShape: [type [or Absent [record val: Int  left: TreeShape  right: TreeShape]]]
depth: [fn@Int [tree@TreeShape] ...]
```

**Structural subtype checking between distinct recursive TyCons.** Checking `A <: B` where both are structural recursive types with the same shape requires comparing expanded bodies. The expanded body of `A` contains `App(TyCon("A"), [])` and the expanded body of `B` contains `App(TyCon("B"), [])`. Without a coinductive visited-pairs algorithm, the type checker must unfold these again — and diverges. This matters when user code defines a type structurally equivalent to a library type and passes one where the other is expected.

### What's Missing

1. `Type::Recursive { var, body }` and `Type::RecVar(String)` variants in `src/type_def.rs` — the internal representation of inline recursive types
2. `TypeNode` nominal ADT in the prelude covering all type-stage value forms; equirecursive types contribute `Recursive` and `RecVar` constructors
3. Expansion-stack cycle detection in the annotation resolver — produces `Type::Recursive` when an alias references itself via the expansion stack; handles the `mu` combinator for inline positions
4. Coinductive subtype checking via visited-pairs bisimulation — prevents divergence when checking structural equivalence between distinct recursive TyCons
5. `mu: [fn [let f] TypeNode.Recursive body: f]` in the type prelude — inline recursive type constructor
6. Unfolding rules (`unfold_once`) so the type checker can work with recursive types at any finite depth

## Why Equirecursive Types Matter for tinct

**Inline recursive type annotations.** Named recursive aliases work correctly post-user-type-constructors, but require a module-level name even for one-off annotation sites. The `mu` combinator lets recursive types appear inline — as function parameter annotations, `TypeAssert` expressions, or anywhere a `TypeNode` is expected — without polluting the namespace with a name used only once.

**Safe subtype checking between structurally equivalent recursive types.** BAS is structural: two types with the same shape should be subtypes of each other, even if they carry different names. Without a coinductive algorithm, checking `A <: B` between distinct recursive TyCons diverges. The visited-pairs bisimulation ensures this check terminates and gives the correct answer.

**Transparent to users.** Equirecursive types require no explicit `fold`/`unfold` operations. A function that accepts a `List` just accepts a `List`; the type checker handles the recursion transparently.

**Consistency with BAS.** BAS is structural: `μa.T[a]` and `T[μa.T[a]]` are structurally equal — there is no "recursive wrapper" that needs a name. This is the approach taken by DOT (Scala's formal foundation), OCaml, and TypeScript.

**External structural data.** Data from `from-json` and `from-yaml` arrives as plain structural values and cannot be wrapped in nominal constructors after the fact. For types that must be expressed structurally (because they round-trip through JSON), inline `mu` annotations express the recursive shape without requiring a separately declared nominal type.

## Design

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

### Annotation Syntax

A `mu` type combinator in the type prelude enables μ-type annotations. It takes a single function — the body — and passes the self-reference as the function's argument. The self-reference is bound to a named parameter (`self` by convention), making it a real lexically-scoped name with an unambiguous wrapping boundary. The combinator returns a `TypeNode.Recursive` value — a nominal constructor, not a string-keyed dict:

```tinct
--- stage: type
[
  mu: [fn [let f] TypeNode.Recursive body: f]
]
---
# A recursive list of Int — named alias (mu not needed; expansion stack handles it)
IntList: [type [or Absent [record head: Int  tail: IntList]]]

# A recursive JSON-like value — inline mu with named self-reference
JsonValue: [type [mu [fn [let self] [or Int String Bool Absent [Seq self] [Map String: self]]]]]

# Usage in function annotations
depth: [fn@Int [tree@[mu [fn [let self] [or Absent [record value: Int  left: self  right: self]]]]]]
  [if [absent? tree] 0 [+ 1 [max [depth tree.left] [depth tree.right]]]]
```

The annotation resolver calls the function with a freshly-generated internal `RecVar` sentinel, builds the body, and wraps it in `Type::Recursive`. The generated name (`μ0`, `μ1`, …) is internal-only — users never write or see it in source. Named `self` (or any identifier) is preferred over `$_` because `$_` desugaring binds at the nearest enclosing argument position, not at the `mu` boundary — giving the wrong wrapping in any body expression that contains nested calls.

For common patterns, named aliases are the ergonomic form. The expansion-stack cycle detector produces `Type::Recursive` automatically without any `mu`:

```tinct
IntList:  [type [or Absent [record head: Int    tail: IntList]]]
StrList:  [type [or Absent [record head: String  tail: StrList]]]
JsonVal:  [type [or Int String Bool Absent [Seq JsonVal] [Map String: JsonVal]]]
BinTree:  [type [or Absent [record val: Int  left: BinTree  right: BinTree]]]

# Use the alias — no mu at the call site
process: [fn@Absent [tree@BinTree] ...]
```

### `TypeNode`: The Type-Stage Value Type

All type-stage functions return `TypeNode` values — a nominal ADT declared in the prelude. This gives exhaustiveness checking, type-safe dispatch in the annotation resolver, and compile-time errors on typos. Equirecursive types contribute two new constructors: `Recursive` and `RecVar`.

```tinct
TypeNode: [type
  # Primitives
  [Int]  [Float]  [String]  [Bool]  [Absent]  [Unknown]  [Never]
  # Structural
  [Record    fields: [Map String TypeNode]  open: Bool]
  [Union     types: [Seq TypeNode]]
  [Intersect types: [Seq TypeNode]]
  # Constructors — from user-type-constructors
  [TyCon     name: String]
  [App       ctor: TypeNode  args: [Seq TypeNode]]
  # Function
  [Arrow     params: [Seq TypeNode]  result: TypeNode]
  # Recursive — this whatif
  [Recursive body: Fn]
  # Internal sentinel — produced by the annotation resolver during mu expansion;
  # not for direct use
  [RecVar    name: String]]
```

Existing type-stage combinators (`or`, `record`, `arrow`, etc.) are updated to return the corresponding `TypeNode` constructor rather than a `kind:`-keyed dict. The annotation resolver in Rust dispatches on `Value::Variant { tag: "TypeNode.*", ... }` — any unrecognised variant produces "expected TypeNode, got TypeNode.X" rather than silently accepting malformed input.

`RecVar name: String` carries an internally-generated name (`"μ0"`, `"μ1"`, …) — never written in source, produced only by the resolver as the sentinel passed to the `mu` body function.

### Annotation Resolver: Cycle Detection

The annotation resolver handles recursive types via two paths:

**Named aliases** use an expansion stack. When expanding a `TyConDef` whose name is already in the stack, the resolver emits `Type::RecVar(name)` instead of recursing further. After expansion, if the body contains any `RecVar(name)`, the body is wrapped in `Type::Recursive { var: name, body }`:

```text
expand_tycon("List", args=[], stack):
  if "List" in stack:
    return Type::RecVar("List")          # cycle — emit bound var
  push "List" to stack
  body = expand tycon_def body with stack
  pop "List" from stack
  if body contains RecVar("List"):
    return Type::Recursive { var: "List", body }
  return body
```

**The `mu` combinator** handles inline annotation positions. When the resolver sees `TypeNode.Recursive body: f`, it generates a fresh internal name, calls `f` with a `TypeNode.RecVar` sentinel, and wraps the result:

```text
resolve_typenode(TypeNode.Recursive body: f, stack):
  name = fresh_mu_name()                     # "μ0", "μ1", … — internal only
  sentinel = TypeNode.RecVar name: name
  body_result = resolve_typenode(call(f, sentinel), stack)
  return Type::Recursive { var: name, body: body_result }

resolve_typenode(TypeNode.RecVar name: n, stack):
  return Type::RecVar(n)                     # pass through to Rust representation
```

The Rust resolver matches on `Value::Variant { tag, payload }` — any tag not in the `TypeNode` ADT produces a clear type error rather than silent failure. The depth limit remains as a safety net. Named aliases never need explicit `mu`; the expansion stack handles them automatically.

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

Under BAS, `is_subtype(Type::Recursive, Type::Recursive)` uses a **bisimulation** algorithm:

```rust
fn is_subtype_recursive(a: &Type, b: &Type, visited: &mut HashSet<(TypeId, TypeId)>) -> bool {
    let key = (type_id(a), type_id(b));
    if visited.contains(&key) {
        return true;   // coinductive hypothesis: assume true, check body
    }
    visited.insert(key);

    let a_unfolded = unfold_once(a);
    let b_unfolded = unfold_once(b);
    is_subtype_impl(a_unfolded, b_unfolded, visited)
}
```

**Unfold once**: replace `Type::Recursive { var: "a", body }` with `body[RecVar("a") ↦ self]` — substituting all `RecVar` occurrences with the full recursive type. This produces one layer of the infinite unrolling.

**Visited set**: pairs of type IDs (by structural identity, not pointer identity). When a pair is seen again, the coinductive hypothesis holds — the bisimulation has closed. This is the standard algorithm for equirecursive subtyping (Pierce, TAPL §21; Amadio & Cardelli 1993).

**Interaction with BAS**: union/intersection types unfold before recursive types — `is_subtype(μa.T[a], A|B)` distributes over the union first, then recurses into each arm with the visited set.

### Unification

Both unification cases use **simultaneous opening**: replace the `RecVar` binder with a shared fresh `TypeVar`, then unify the opened bodies. This terminates because the fresh `TypeVar` is a unification variable — not a `Type::Recursive` — so neither arm fires again on the opened bodies.

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

The symmetric case terminates: both opened bodies contain only `fresh` (a `TypeVar`) at recursive positions — standard Robinson unification applies. The asymmetric case terminates for the same reason: `a_open` replaces `RecVar(v1)` with `fresh`, a `TypeVar`; `other` contains no `RecVar`; the `Recursive` arm cannot re-fire.

`unfold_once` — which replaces `RecVar` with the full `Recursive` type, making the tree **larger** — is used only in subtype checking (where the visited-pairs set prevents divergence), not in unification.

### Mutual Recursion

Mutually recursive type aliases — where `A` references `B` and `B` references `A` — require no explicit `mu` from the user. The annotation resolver's expansion stack detects the cycle automatically. Users write plain type aliases:

```tinct
EvenList: [type [or Absent [record head: Int  tail: OddList]]]
OddList:  [type [or Absent [record head: Int  tail: EvenList]]]
```

Expansion of `EvenList` proceeds as follows:

1. Push "EvenList" to the expansion stack; begin expanding the body
2. The body references `OddList` — push "OddList"; begin expanding its body
3. The body of `OddList` references `EvenList` — "EvenList" is already in the stack → emit `Type::RecVar("EvenList")`
4. Pop "OddList": its body contains `RecVar("EvenList")` → wrap: `Type::Recursive { var: "EvenList", body: <OddList-body> }`
5. Pop "EvenList": its body contains the wrapped OddList form → wrap: `Type::Recursive { var: "EvenList", body: <full EvenList body> }`

The result is a nested-μ encoding of the mutually recursive type. For symmetric mutual recursion, the nested encoding and the simultaneous encoding are semantically equivalent (Pierce, TAPL §21.8). The μ binder is anchored to the first name in the stack at the cycle point ("EvenList" in this case); cross-references from `OddList` back to `EvenList` use `RecVar("EvenList")`.

Explicit `mu` in annotation positions (function parameters, `TypeAssert`) can express the same structure directly when no named alias exists — using `[fn [let self] ...]` with `self` as the self-reference. The depth limit still applies as a safety net; the expansion stack detects genuine cycles before the limit fires.

## What Would Change

### `src/type_def.rs` — `Type` enum

**Current:** `Type::TyCon(String)`, `Type::App(Box<Type>, Vec<Type>)`, and `RowTail::Uniform { key, value }` are present. No recursive type variant.
**Proposed:** Add `Type::Recursive { var: String, body: Box<Type> }` and `Type::RecVar(String)` alongside the existing variants. Update all exhaustive `match` arms throughout the codebase (~40 sites).

`Type::RecVar` is a **bound** variable with an internal generated name (`"μ0"`, `"μ1"`, …) — distinct from `TypeVar(String, u32)` (a unification variable). `TypeVar` participates in per-dict `Substitution` chains and is resolved by `Substitution::apply()`. `Type::RecVar` never enters the substitution; it is eliminated by `unfold_once()` via a separate capture-avoiding structural traversal that replaces all `RecVar(var)` occurrences in `body` with the full `Recursive` type. These two substitution mechanisms are independent and do not interfere. The generated `RecVar` name is never written in source; at call sites users name the parameter explicitly (`self` by convention) in `[fn [let self] ...]`.

`Type::Recursive` always has kind `Kind::Star`. `Type::RecVar` has kind `Kind::Star` (tinct only supports μ-types at kind `*`; higher-kinded μ-binders are not needed).

**Impact:** Major — touches every type operation (subtype, unify, collect_type_vars, apply_inner, display).

### `src/typecheck_annot.rs` — Alias expansion and resolver

**Current:** Type alias resolution goes through `TypeEnv::lookup_tycon_def`, returning a `TyConDef` with its stored body type. Expansion is straightforward — TyCon self-references in the body are `App(TyCon("name"), [])` and are not expanded further.
**Proposed:** Add a `Vec<String>` expansion stack threaded through all alias expansion calls. When `lookup_tycon_def(name)` is about to expand a body, check first whether `name` is already in the stack — if so, return `Type::RecVar(name)` immediately. After expanding, if the result contains any `RecVar(name)`, wrap in `Type::Recursive { var: name, body }`. Also add a resolver arm for `TypeNode.Recursive body: f` — the `mu` combinator path for inline annotation positions.
**Impact:** Moderate — expansion stack threading + `mu` resolver arm.

### `src/type_unify.rs` — `is_subtype` and `unify`

**Current:** Handles `Type::TyCon`/`Type::App` via `UNIFY-TYCON`. No handling of `Type::Recursive` or `Type::RecVar`; would hit unreachable match arms.
**Proposed:** Add coinductive `is_subtype_recursive` with a visited-pairs set (bisimulation). Add unify arms that unfold recursive types. Unfold order: if either side of `is_subtype` or `unify` is `Type::Recursive`, unfold via `unfold_once()` first, then re-enter — `Type::Recursive` is never directly compared to a `TyCon`. `Type::RecVar` never reaches the unifier; it only appears inside `Recursive` bodies and is eliminated by `unfold_once()`.
**Impact:** Moderate — new coinductive algorithm; performance cost proportional to mutual recursion depth.

### `src/type_def.rs` — No new user-facing schema

The internal `Type::Recursive { var, body }` and `Type::RecVar(String)` Rust types have no stable user-facing form — they appear in type error messages and type reflection output with generated names (`μ0`, `μ1`) but are not written in source. The user-facing type-stage value type is `TypeNode` (a tinct nominal ADT in the prelude), not a schema of string-keyed dicts.
**Impact:** Minor — annotation resolver update to handle `TypeNode.Recursive` dispatch.

### `stdlib/prelude.llt` — `TypeNode` ADT and `mu` combinator

**Current:** Type-stage functions return plain dicts with `kind:` string discriminators. No `TypeNode` ADT, no `mu` combinator.
**Proposed:**

1. Declare the `TypeNode` nominal ADT in the `--- stage: type` section of the prelude (shown in full above). All existing type-stage combinators (`or`, `record`, `arrow`, etc.) are updated to return the corresponding `TypeNode` constructor instead of a `kind:`-keyed dict. This migration is atomic — all combinators must switch together.
2. Add `mu: [fn [let f] TypeNode.Recursive body: f]`. The self-reference is the body's named parameter (`self` by convention); the resolver passes a `TypeNode.RecVar` sentinel internally.
3. Update the annotation resolver in `src/typecheck_annot.rs` to dispatch on `Value::Variant { tag: "TypeNode.*", ... }` throughout — replacing all `kind:` string checks with exhaustive nominal variant matching.
**Impact:** Moderate — atomic migration of all type-stage combinators; any partial state diverges.

### Type checker performance

**Current:** Subtype checking terminates quickly (no coinductive loop).
**Proposed:** Visited-pairs set adds overhead proportional to the depth of mutual recursive unfolding. For typical config schemas (finite depth), this is bounded. For pathological mutual recursion, the visited set size grows. A cache keyed by structural type identity amortizes repeated checks.
**Impact:** Moderate — performance regression in subtype-heavy programs; acceptable for config-scale programs.

## Downstream: validate-tinct-rewrite

Once equirecursive types land, `validate_value` in `src/builtins_meta.rs` (~267 lines) can be rewritten as a tinct stdlib function. `regex-match?` is already available; the only missing piece is a recursive type alias to type the schema dict.

- Define the schema dict type in `stdlib/prelude.llt` using a `mu`-type alias covering all schema keys: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum`
- Rewrite `validate` as a tinct function: call `regex-match?` for `pattern`, recurse on `fields:` and `items:` entries, collect violations into a Seq; remove `validate_value` from `src/builtins_meta.rs`
- Keep `validate` registered as a thin Rust stub that calls the tinct function and maps errors to `SchemaViolation` error kind
- Tests: all existing `validate` corpus tests pass after rewrite; validate over 1000-entry dict completes in <100ms

## Prerequisites

- **user-type-constructors** — already accepted and in implementation (S-842–S-851). `Type::TyCon`, `Type::App`, `RowTail::Uniform`, and the scoped `TyConDef` registry are the baseline this feature builds on. Equirecursive types extend the type system with `Type::Recursive` and `Type::RecVar` for the two cases TyCon references alone cannot handle: inline recursive annotations and safe subtype checking between distinct recursive TyCons.
- `type-ann-v2-infra` sprint — establishes the `--- stage: type` environment and the resolver infrastructure that `TypeNode` and `mu` extend. `TypeNode` must be declared before any type-stage combinator migration can proceed.

## References

- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM Transactions on Programming Languages and Systems*, 15(4), 575–631. — [foundational coinductive subtype algorithm for equirecursive types; the bisimulation approach this design uses]
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. §21 "Recursive Types." — [equirecursive vs isorecursive comparison; rational tree representation; unfolding semantics]
- Ancona, D. & Zucca, E. (2002). "A Theory of Mixin Modules." *ACM TOPLAS*, 24(5), 578–637. — [equirecursive types in structural object systems, closely related to BAS]
- Huet, G. (1976). "Résolution d'Équations dans des Langages d'Ordre 1, 2, ..., ω." Ph.D. thesis. Université Paris VII. — [rational tree unification; the mathematical foundation for representing recursive types as finite cyclic graphs]
