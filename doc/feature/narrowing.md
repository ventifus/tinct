# Path-Sensitive Type Narrowing

## Overview

Path-sensitive type narrowing gives the type checker branch-specific
knowledge derived from conditional guards. After `[= x "hello"]`, the
true branch knows `x` is exactly `"hello"`. After `[= [type-of x] "Int"]`,
the true branch knows `x` is `Int`. After `[has? "name" x]`, the true
branch knows `x` has a `name` field.

This produces more precise types in conditional branches, improves LSP
hover information, and reduces the need for explicit `[@Type expr]`
annotations that restate what the condition already establishes.
Narrowing is the foundational mechanism for discriminated union checking
when sum types are added.

## Supersession Notes

- **False-branch narrowing**: The false branch is not unrefined — `apply_negation_narrowings` (`src/typecheck.rs`) produces `Negation(T)` in the false branch for type-predicate guards. This was implemented as part of [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09), which added `Type::Negation`.
- **`Type::Any` references**: `Type::Any` no longer exists. The codebase uses `Type::Unknown` (gradual opt-out) and `Type::Top` (supertype). Any section referencing `Type::Any`, `Type::Seq(Any)`, or `"Function" → Type::Any` is stale.

## Design

### Making `if` a Type-Level Special Form

The type checker has a dedicated rule for `if` (and by extension `cond`,
`when`, `unless`):

```text
Γ ⊢ cond : Bool
Γ_T = narrow(Γ, cond, true)
Γ_F = narrow(Γ, cond, false)
Γ_T ⊢ then_branch : τ₁
Γ_F ⊢ else_branch : τ₂
────────────────────────────────
Γ ⊢ [if cond then_branch else_branch] : τ₁ ∨ τ₂
```

`narrow(Γ, cond, polarity)` returns a modified type environment with
refined bindings for variables mentioned in `cond`.

Without union types, the result type uses the LUB (least upper bound) of
`τ₁` and `τ₂`. With the current type lattice, this is often `Any` or the
shared base type. With union types, the result is `τ₁ | τ₂`.

### Narrowing Patterns

The `narrow()` function recognizes specific condition shapes. Each pattern
has a true-branch refinement.

> **⚠ Superseded by [boolean-algebraic-subtyping.md](boolean-algebraic-subtyping.md) (2026-05-09):** False-branch narrowing is implemented — `apply_negation_narrowings` in `src/typecheck.rs` produces `Negation(T)` in the false branch for type-predicate guards (e.g., after `[int? x]` in the false branch, `x : ~Int`).

The false branch receives an environment refined by negation of the guard's type constraint.

#### Pattern 1: Equality with Literal

```tinct
[= x "hello"]
```

- **True branch:** `x : StringLiteral("hello")`
- **False branch:** `x : Str` (unchanged)

Recognizes `=` with one operand being a `VarRef` and the other a literal
expression. Both operand orderings are recognized (`[= "hello" x]` and
`[= x "hello"]`).

#### Pattern 2: Type-of Guard

```tinct
[= [type-of x] "Int"]
```

- **True branch:** `x : Int`
- **False branch:** `x` unchanged

Recognizes `=` where one operand is `[type-of var]` and the other
is a string literal matching a known type name. Maps `"Int"` → `Type::Int`,
`"Float"` → `Type::Float`, `"String"` → `Type::Str`, `"Bool"` →
`Type::Bool`, `"Dict"` → `Type::Record([], Open)`, `"Seq"` →
`Type::Seq(Unknown)`, `"Function"` → `Type::Unknown` (can't narrow further).

When type predicates are available (see [type-predicates.md](type-predicates.md)),
this pattern extends to recognize `[int? x]` directly, without the
`type-of` indirection.

#### Pattern 3: Key Presence

```tinct
[has? "name" x]
```

- **True branch:** `x : Record([name: α], Open)` where `α` is fresh —
  the record is known to have a `name` field
- **False branch:** `x` unchanged

Recognizes `has?` with a string literal key and a `VarRef`. Narrows
the record type to include the key with a fresh type variable. If `x`
already has a record type, the key is added to the existing fields.

#### Pattern 4: Boolean Conjunction

```tinct
[and [= x "hello"] [has? "name" y]]
```

Conjunction (`and`) applies both narrowings to the true-branch
environment. Disjunction (`or`) applies the intersection of narrowings
(only narrow if both branches agree — rare, usually no narrowing).

### Narrowing Extraction

The type checker extracts narrowing constraints by pattern matching on
the condition AST:

```rust
enum Narrowing {
    EqLiteral { var: String, ty: Type },
    TypeOf { var: String, ty: Type },
    HasKey { var: String, key: String },
}

fn extract_narrowings(cond: &Expr, env: &TypeEnv) -> Vec<Narrowing> {
    // Pattern match on cond AST shape
    // Return empty vec for unrecognized patterns
}
```

### Environment Forking

The type checker creates forked environments for each branch:

1. `env_true` = clone `env`, apply narrowings
2. `env_false` = clone `env` (unrefined — false-branch narrowing is out of scope)

Per-`if` cost: one environment clone per branch (two clones total). In
programs with nested conditionals, this multiplies — a chain of 10 nested
`if`s creates 20 environment clones. For tinct's target domain (config
files with few conditionals), this is acceptable.

### Interaction with TypeAssert

TypeAssert (`[@Type expr]`) already narrows via elaboration + proxy
contracts. Path-sensitive narrowing complements it:

- **TypeAssert:** explicit, user-written, works anywhere, runtime-checked
- **Path narrowing:** implicit, automatic in `if` branches, static only

Both coexist. If `x` is narrowed by a condition AND has a TypeAssert,
the narrower type wins (intersection of the two refinements).

### Interaction with Lazy Evaluation

Narrowing assumes the condition is evaluated before the branches. This
is semantically true for `if` (strict in condition, selective in
branches — doc/08-evaluation.md §Selective Materialization), so the narrowing is
sound: the condition has been forced, establishing the type fact, before
either branch is entered.

For `cond`, the same applies to each condition-value pair (conditions
are forced sequentially).

### Stdlib Narrowing (`cond`, `when`, `unless`)

These are stdlib functions defined in terms of `if`. Two options:

- **Inline narrowing:** Make `cond`, `when`, `unless` also special
  forms in the type checker. More precise but more hardcoded forms to
  maintain.
- **Defer:** Only narrow for `if`. Users who need narrowing in `cond`
  branches can use explicit TypeAssert. Simpler.

The current implementation narrows `if` only.

### Annotation-Based Narrowing (T-1761)

Any function — not just prelude predicates — can declare that calling it with a single variable argument narrows that variable to a type in the true branch. Two annotation forms are supported:

#### `@[narrows: T]` key annotation

Attach to the binding key of a predicate function:

```tinct
my-int?@[narrows: Int]:
  [fn@Boolean [let x] [match x [@Integer _]: Boolean.True _: Boolean.False]]
```

When `[my-int? x]` is true in an `if` condition, `x` is narrowed to `Int` in the true branch.

#### `@[is: T]` parameter annotation

Attach to the first parameter of the predicate's function body:

```tinct
my-int?: [fn@Boolean [let x@[is: Int]] [match x [@Integer _]: Boolean.True _: Boolean.False]]
```

The semantics are identical: `[my-int? x]` being true narrows `x` to `Int`.

**Mechanism:** Both forms store `TypeScheme.param_narrowings[0] = Some(T)` when the predicate's type scheme is constructed. `extract_narrowings` (in `src/typecheck_narrow.rs`) looks up the called function's type scheme in the environment and reads `param_narrowings`. The narrowing logic is fully annotation-driven — no predicate names are hardcoded in Rust.

**Limitation (B-545):** Structural narrowing patterns (`=`, `has?`, `and`, `type-of`) still use hardcoded function name matching. A protocol or annotation extension is needed to make those prelude-agnostic as well.

### Limitations

1. **No false-branch narrowing.** The false branch gets the original unrefined
   environment. False-branch narrowing (`x : ~Int` after `[int? x]` fails) is out
   of scope — see `doc/feature/boolean-algebraic-subtyping.md`.

2. **Only `if`.** `cond`, `when`, `unless`, and user-defined
   conditional patterns are not narrowed. Explicit TypeAssert is the
   fallback.

3. **Shallow pattern recognition.** Only the four patterns above are
   recognized. Complex conditions (`[and [= x 1] [= y 2]]` nested
   inside function calls) are not decomposed.

4. **No narrowing across function boundaries.** If a function returns
   `Bool` based on a type check, the caller can't narrow based on the
   return value. This requires type predicates / type guard declarations
   (TypeScript's `x is SomeType`), which are not in scope here.

5. **Let-binding interaction.** `[let [ok: [= x "hello"]] [if ok ...]]`
   — the narrowing is lost because `ok` is a variable, not the original
   condition expression. Narrowing only works when the condition is inline
   in `if`.

## Implementation

### Type Checker (`src/typecheck.rs`)

`if` is dispatched to a dedicated `infer_if(cond, then_expr, else_expr, env, state, type_map)` function rather than through `check_call` like other builtins. The function extracts narrowing constraints from the condition AST, forks the type environment into `env_true` and `env_false`, infers branches independently, and joins result types via LUB.

New AST pattern matching infrastructure handles condition analysis. Environment forking doubles allocation in conditional code.

### Type Representation (`src/types.rs`)

No changes to the `Type` enum — narrowing uses existing types
(`IntLiteral`, `StringLiteral`, `Record`). The `Narrowing` enum is local
to the type checker.

### Evaluator (`src/eval.rs`)

No changes. Narrowing is purely static — no additional guards or checks
at runtime beyond what TypeAssert already provides.

### Parser / Grammar

No changes — narrowing is a type checker feature, not a syntax feature.

### Type Map (LSP integration)

Variables in narrowed branches get their refined type in the type map.
LSP hover shows the narrowed type. No structural changes to the type map
itself.

## References

- Dunfield, J. & Pfenning, F. (2004). "Tridirectional typechecking."
  In *POPL '04*, pp. 281–292. ACM.
  — Datasort refinements, singleton types as refinements of base types.
  The formal model for narrowing literal types in branches.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped
  languages." In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing in Typed Racket: type narrowing via predicates in
  conditionals. The foundational work on path-sensitive type narrowing,
  directly applicable to tinct's `if`-based narrowing.
- TypeScript Handbook. "Narrowing."
  — Practical narrowing patterns: typeof guards, equality narrowing,
  truthiness narrowing, discriminated unions. Production reference for
  the pattern catalog approach.
- Wright, A.K. & Cartwright, R. (1997). "A practical soft type system
  for Scheme." *ACM TOPLAS*, 19(1), pp. 87–152. ACM.
  — Soft typing: inferring type information from predicates in untyped
  programs. Early work on condition-driven type refinement that informs
  the narrowing-from-guards approach.
