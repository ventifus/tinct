# What If: Equirecursive Types for tinct

**State:** Proposal

What would it take to support properly recursive data types — linked lists, trees, and other self-referential structures — in tinct's type system?

## Current State

Tinct supports recursive type aliases via two-pass registration:

```tinct
# These parse and register correctly
List: [type [head: Int  tail: List]]
Tree: [type [value: Int  left: Tree  right: Tree]]
```

The type checker registers all aliases with `Type::Unknown` placeholder bodies in Pass 1, then resolves bodies in Pass 2. Self-references resolve to `Type::Unknown` at the cycle boundary, breaking the recursion structurally.

This makes shallow patterns usable, but the type loses its recursive structure:

```tinct
first-element: [fn [lst@List] lst.head]   # OK — accesses head field
second-element: [fn [lst@List] lst.tail.head]   # OK — one level deep
deep: [fn [lst@List] lst.tail.tail.tail.head]   # OK up to ~256 levels
# After depth limit: tail type becomes Unknown, further access untyped
```

The alias expansion limit (`MAX_ALIAS_DEPTH = 256`) prevents infinite expansion but produces a structural approximation rather than the true recursive type. `lst.tail` has type `Unknown` after sufficient unrolling, losing all static guarantees.

In `--- stage: type` sections, a recursive type-stage function diverges entirely — the annotation resolver forces lazy thunks while traversing the type dict, causing infinite unrolling until the depth limit fires and produces a type error.

```tinct
--- stage: type
[
  # This diverges — self-referential type-stage function
  List: [fn [a] [or Null [record head: a  tail: [List a]]]]
]
```

### Nominal Variants as a Workaround

Nominal variant declarations sidestep the structural recursion problem by breaking cycles at the constructor boundary:

```tinct
[type IntList [Cons Int IntList] Nil]
```

`Cons` is a nominal constructor that wraps `Int × IntList`; the type checker handles the recursive reference by its nominal identity rather than structural expansion. This works well for ADTs but requires explicit constructor wrapping and pattern matching everywhere — it cannot express "any record that has a `head:` and a `tail:` field recursively."

### What's Missing

1. A `Type::Recursive` (μ-type) variant representing a proper fixpoint type, distinct from alias expansion
2. A `[kind: "recursive" ...]` type dict node for the annotation resolver and `ast-of` schema
3. Cycle detection in the annotation resolver that produces μ-type nodes instead of hitting the depth limit
4. Coinductive (bisimulation-based) subtype checking for recursive types under BAS
5. A `mu` type combinator in the type prelude enabling μ-type annotations
6. Unfolding rules so the type checker can use a recursive type at any finite depth

## Why Equirecursive Types Matter for tinct

**Structural config schemas.** A recursive config — `[type ServerConfig [host: String  fallbacks: [Seq ServerConfig]]]` — is a natural pattern in configuration. Today the `fallbacks` field loses its type after two levels of nesting.

**Type-safe tree traversal.** Functions that walk recursive data structures — JSON-like nested dicts, AST nodes, dependency graphs — can express and check their invariants statically instead of typing fields as `Unknown`.

**Transparent to users.** Equirecursive types require no explicit `fold`/`unfold` operations. A function that accepts a `List` just accepts a `List`; the type checker handles the recursion transparently. This is the right model for a language that prioritizes ergonomics over ceremony.

**Consistency with structural typing.** Tinct uses BAS, which is structural. Equirecursive types fit naturally because `μa.T[a]` and `T[μa.T[a]]` are structurally equal — there is no "recursive wrapper" that needs a name. This is the approach taken by DOT (Scala's formal foundation), OCaml, and TypeScript.

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

Example — a linked list of Int:
```
Type::Recursive {
    var: "lst",
    body: Type::Union([
        Type::Record(Row::Empty),            // Null — the empty list
        Type::Record({
            head: Type::Int,
            tail: Type::RecVar("lst")        // self-reference
        })
    ])
}
```

This is a **finite** representation of an **infinite** unrolling. The type checker unfolds `Type::Recursive` on demand during subtype checking, using a visited-pairs set to detect when unfolding has returned to a previously seen configuration.

### Annotation Syntax

A new `mu` type combinator in the type prelude enables μ-type annotations:

```tinct
--- stage: type
[
  mu: [fn [var body]
    [kind: "recursive"  var: var  body: body]]
  recvar: [fn [name]
    [kind: "recvar"  name: name]]
]
---
# A recursive list of Int
IntList: [type [mu "lst" [or Null [record head: Int  tail: [recvar "lst"]]]]]

# A recursive JSON-like value
JsonValue: [type [mu "v" [or
  Int
  String
  Bool
  Null
  [Seq [recvar "v"]]
  [Map String: [recvar "v"]]]]]

# Usage in function annotations
depth: [fn@Int [tree@[mu "t" [or Null [record value: Int  left: [recvar "t"]  right: [recvar "t"]]]]]]
  [if [null? tree] 0 [+ 1 [max [depth tree.left] [depth tree.right]]]]
```

For common patterns, type aliases are the ergonomic form:

```tinct
IntList:  [type [mu "lst" [or Null [record head: Int   tail: [recvar "lst"]]]]]
StrList:  [type [mu "lst" [or Null [record head: String tail: [recvar "lst"]]]]]
JsonVal:  [type [mu "v"   [or Int String Bool Null [Seq [recvar "v"]] [Map String: [recvar "v"]]]]]
BinTree:  [type [mu "t"   [or Null [record val: Int left: [recvar "t"] right: [recvar "t"]]]]]

# Use the alias — no mu/recvar noise at the call site
process: [fn@Null [tree@BinTree] ...]
```

### Type Dict Schema Extensions

Two new `kind:` entries in the canonical type dict schema:

```tinct
[kind: "recursive"  var: "a"  body: <type-dict>]   # μa.T[a] — binder
[kind: "recvar"     name: "a"]                      # reference to enclosing binder's variable
```

These appear in `ast-of` output, annotation resolution results, and anywhere type dicts are used.

### Annotation Resolver: Cycle Detection

The annotation resolver currently expands type aliases iteratively, hitting `MAX_ALIAS_DEPTH` on cycles. With equirecursive support, cycle detection produces μ-type nodes instead:

```
resolve_type_alias("List", args=[]):
  if "List" in expansion_stack:
    return [kind: "recvar"  name: "List"]   # cycle — emit recvar
  push "List" to expansion_stack
  body = expand "List" body with expansion_stack
  pop "List" from expansion_stack
  if body contains [kind: "recvar"  name: "List"]:
    return [kind: "recursive"  var: "List"  body: body]   # wrap in mu
  return body
```

The result is a `[kind: "recursive" ...]` node whenever an alias is truly self-referential, and a plain type dict otherwise. The depth limit remains as a safety net for mutual recursion chains.

The `mu` type combinator in the type prelude produces the same structure explicitly for annotation-position use.

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

During HM unification, `unify(Type::Recursive, Type::Recursive)` unfolds both sides once and continues:

```rust
match (a, b) {
    (Type::Recursive { var: v1, body: b1 },
     Type::Recursive { var: v2, body: b2 }) => {
        // Unfold both, unify under the coinductive hypothesis
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        let b_open = substitute(b2, v2, &fresh);
        unify(a_open, b_open, subst, state)
    }
    (Type::Recursive { .. }, other) => {
        let unfolded = unfold_once(a);
        unify(&unfolded, other, subst, state)
    }
    ...
}
```

Unifying a recursive type with a non-recursive type unfolds the recursive type once and re-tries. This terminates because each unfold is structurally smaller (the `RecVar` references become the concrete type the variable is unified with, eliminating the recursive reference).

### Mutual Recursion

Mutually recursive types (`A` references `B`, `B` references `A`) require simultaneous μ-binders:

```tinct
# Even and Odd as mutually recursive types
[class [EvenList a] [type [mu "e" [or Null [record head: a  tail: [OddList a]]]]]]
[class [OddList a]  [type [mu "o" [or Null [record head: a  tail: [EvenList a]]]]]]
```

For the annotation resolver, mutual recursion is detected when the expansion stack contains two or more names. The resolver produces nested μ-binders with cross-references using `recvar` for both names. The depth limit still applies as a safety net; genuinely mutually recursive types are detected before the limit fires.

## What Would Change

### `src/types.rs` — `Type` enum

**Current:** No recursive type variant; alias expansion with depth limit.  
**Proposed:** Add `Type::Recursive { var: String, body: Box<Type> }` and `Type::RecVar(String)`. Update all exhaustive `match` arms throughout the codebase (~40 sites based on existing pattern).  
**Impact:** Major — touches every type operation (subtype, unify, collect_type_vars, apply_inner, display).

### `src/typecheck_annot.rs` — Alias expansion and resolver

**Current:** `expand_alias_body_guarded` halts at `MAX_ALIAS_DEPTH` and returns `Type::Unknown`.  
**Proposed:** Add expansion stack tracking; detect cycles and produce `Type::Recursive` nodes; `mu` combinator in type prelude maps to `Type::Recursive`.  
**Impact:** Moderate — alias expansion + `mu`/`recvar` resolver arms.

### `src/type_unify.rs` — `is_subtype` and `unify`

**Current:** No handling of `Type::Recursive`; would hit unreachable arms.  
**Proposed:** Add coinductive `is_subtype_recursive` with visited-pairs set; add unify arms that unfold recursive types.  
**Impact:** Moderate — new algorithm; performance implications for subtype-heavy programs.

### `src/types.rs` — Type dict schema

**Current:** No `kind: "recursive"` or `kind: "recvar"` schema entries.  
**Proposed:** Document and handle these two new `kind:` values throughout the type dict mapping code.  
**Impact:** Minor — schema extension; annotation resolver and `ast-of` conversion.

### `stdlib/prelude.llt` — `mu` and `recvar` in type prelude

**Current:** No recursive type combinators.  
**Proposed:** Add `mu: [fn [var body] [kind: "recursive"  var: var  body: body]]` and `recvar: [fn [name] [kind: "recvar"  name: name]]` to the `--- stage: type` section.  
**Impact:** Minor — two new type-stage functions.

### Type checker performance

**Current:** Subtype checking terminates quickly (no coinductive loop).  
**Proposed:** Visited-pairs set adds overhead proportional to the depth of mutual recursive unfolding. For typical config schemas (finite depth), this is bounded. For pathological mutual recursion, the visited set size grows. A cache keyed by structural type identity amortizes repeated checks.  
**Impact:** Moderate — performance regression in subtype-heavy programs; acceptable for config-scale programs.

## Downstream: validate-tinct-rewrite

Once isorecursive types land, `validate_value` in `src/builtins_meta.rs` (~267 lines) can be rewritten as a tinct stdlib function. `regex-match?` is already available; the only missing piece is a recursive type alias to type the schema dict.

- Define the schema dict type in `stdlib/prelude.llt` using a `mu`-type alias covering all schema keys: `type`, `min`, `max`, `min-length`, `max-length`, `pattern`, `required`, `default`, `items`, `fields`, `enum`
- Rewrite `validate` as a tinct function: call `regex-match?` for `pattern`, recurse on `fields:` and `items:` entries, collect violations into a Seq; remove `validate_value` from `src/builtins_meta.rs`
- Keep `validate` registered as a thin Rust stub that calls the tinct function and maps errors to `SchemaViolation` error kind
- Tests: all existing `validate` corpus tests pass after rewrite; validate over 1000-entry dict completes in <100ms

## Prerequisites

- `type-ann-v2-infra` sprint — establishes the `--- stage: type` environment where `mu` and `recvar` are defined; establishes the type dict schema that `[kind: "recursive" ...]` extends

## References

- Amadio, R.M. & Cardelli, L. (1993). "Subtyping Recursive Types." *ACM Transactions on Programming Languages and Systems*, 15(4), 575–631. — [foundational coinductive subtype algorithm for equirecursive types; the bisimulation approach this design uses]
- Pierce, B.C. (2002). *Types and Programming Languages*. MIT Press. §21 "Recursive Types." — [equirecursive vs isorecursive comparison; rational tree representation; unfolding semantics]
- Ancona, D. & Zucca, E. (2002). "A Theory of Mixin Modules." *ACM TOPLAS*, 24(5), 578–637. — [equirecursive types in structural object systems, closely related to BAS]
- Huet, G. (1976). "Résolution d'Équations dans des Langages d'Ordre 1, 2, ..., ω." Ph.D. thesis. Université Paris VII. — [rational tree unification; the mathematical foundation for representing recursive types as finite cyclic graphs]
