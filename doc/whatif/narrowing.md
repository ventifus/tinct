# What If: Path-Sensitive Type Narrowing for tinct

What would it take to narrow types in conditional branches based on
equality checks and type guards?

## Current State

tinct has singleton literal types (`IntLiteral(42)`,
`StringLiteral("hello")`) that are subtypes of their base types, but the
type checker has no path sensitivity — a variable has the same type in
both branches of an `if`.

```tinct
[if [= x "hello"]
  x       # still typed as Str, not StringLiteral("hello")
  x]      # still typed as Str
```

- `if` is typed via `check_call` like any other builtin — no
  branch-specific type environments
- `IntLiteral`/`StringLiteral` exist as types but only arise from literal
  expressions, never from narrowing
- `=` is typed `Any → Any → Bool` — no narrowing information flows from
  equality checks to the type environment
- TypeAssert (`[@Type expr]`) provides explicit narrowing via
  `ThunkState::Guarded` proxy contracts, but this is user-driven, not
  automatic

### What's Missing

1. **No branch-specific type environments.** The type checker uses a single
   environment for both branches of `if`.
2. **No condition analysis.** The type checker does not inspect the condition
   expression to extract type information.
3. **No automatic narrowing.** Users must write explicit `[@Type expr]`
   assertions to narrow types, even when the condition already establishes
   the type fact.
4. **`if` is not a type-level special form.** It is dispatched through
   the generic builtin call path, which cannot fork the type environment.

## What Path-Sensitive Narrowing Would Provide

1. **More precise types in conditional branches.** After `[= x "hello"]`,
   the true branch knows `x` is exactly `"hello"`, enabling better type
   checking for subsequent operations.
2. **Better LSP hover information.** Hovering over `x` in a narrowed
   branch shows the refined type, not the original broad type.
3. **Foundation for pattern matching.** If tinct gains sum types or tagged
   unions, narrowing is the mechanism for discriminated union checking.
4. **Reduced annotation burden.** Guards that the user currently writes
   explicitly (`[@String x]`) can be inferred automatically from
   conditional context.

## Design

### Making `if` a Type-Level Special Form

The type checker gains a dedicated rule for `if` (and `cond`, `when`,
`unless` by extension):

```
Γ ⊢ cond : Bool
Γ_T = narrow(Γ, cond, true)
Γ_F = narrow(Γ, cond, false)
Γ_T ⊢ then_branch : τ₁
Γ_F ⊢ else_branch : τ₂
────────────────────────────────
Γ ⊢ [if cond then_branch else_branch] : τ₁ ∨ τ₂
```

Where `narrow(Γ, cond, polarity)` returns a modified type environment
with refined bindings for variables mentioned in `cond`.

Without union types, the result type uses the LUB (least upper bound) of
`τ₁` and `τ₂`. With the current type lattice, this is often `Any` or the
shared base type. With union types (see `doc/whatif/union-types.md`), the
result is `τ₁ | τ₂`.

### Narrowing Patterns

The `narrow()` function recognizes specific condition shapes. Each pattern
has a true-branch refinement. False-branch refinement requires negation
types and is deferred (the false branch gets the original unrefined
environment).

#### Pattern 1: Equality with Literal

```tinct
[= x "hello"]
```

- **True branch:** `x : StringLiteral("hello")`
- **False branch:** `x : Str` (no negation — would need `Str \ {"hello"}`)

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
`Type::Seq(Any)`, `"Function"` → `Type::Any` (can't narrow further).

When type predicates are available (see `doc/whatif/type-predicates.md`),
this pattern extends to recognize `[int? x]` directly, without the
`type-of` indirection.

#### Pattern 3: Key Presence

```tinct
[has? x "name"]
```

- **True branch:** `x : Record([name: α], Open)` where `α` is fresh —
  the record is known to have a `name` field
- **False branch:** `x` unchanged

Recognizes `has?` with a `VarRef` and a string literal key. Narrows
the record type to include the key with a fresh type variable. If `x`
already has a record type, the key is added to the existing fields.

#### Pattern 4: Boolean Conjunction

```tinct
[and [= x "hello"] [has? y "name"]]
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
2. `env_false` = clone `env` (no false-branch narrowing initially)

Per-`if` cost: one environment clone per branch (two clones total). In
programs with nested conditionals, this multiplies — a chain of 10 nested
`if`s creates 20 environment clones. For tinct's target domain (config
files with few conditionals), this is acceptable.

### Interaction with TypeAssert

TypeAssert (`[@Type expr]`) already narrows via elaboration + proxy
contracts. Path-sensitive narrowing complements it:

- **TypeAssert:** explicit, user-written, works anywhere, runtime-checked
- **Path narrowing:** implicit, automatic in `if` branches, static only

Both can coexist. If `x` is narrowed by a condition AND has a TypeAssert,
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

Recommended: start with `if` only.

### Limitations

1. **No false-branch narrowing.** Requires negation types (`Str \ {"hello"}`)
   which tinct does not have. The false branch gets the unrefined type.
   Revisit if algebraic subtyping is adopted (`doc/whatif/algebraic-subtypes.md`).

2. **Only `if` initially.** `cond`, `when`, `unless`, and user-defined
   conditional patterns are not narrowed. Explicit TypeAssert is the
   fallback.

3. **Shallow pattern recognition.** Only the four patterns above are
   recognized. Complex conditions (`[and [= x 1] [= y 2]]` nested
   inside function calls) are not decomposed.

4. **No narrowing across function boundaries.** If a function returns
   `Bool` based on a type check, the caller can't narrow based on the
   return value. This requires type predicates / type guard declarations
   (TypeScript's `x is SomeType`), which are not proposed here.

5. **Let-binding interaction.** `[let [ok: [= x "hello"]] [if ok ...]]`
   — the narrowing is lost because `ok` is a variable, not the original
   condition expression. Narrowing only works when the condition is inline
   in `if`.

## What Would Change

### Type Checker (`src/typecheck.rs`)

**Current:** `if` is dispatched through `check_call` like any other
builtin. The type checker uses a single environment for both branches.
**Proposed:** Detect `if` calls and dispatch to a dedicated
`infer_if(cond, then_expr, else_expr, env, state, type_map)` function.
Extract narrowing constraints from the condition AST. Fork the type
environment into `env_true` and `env_false`. Infer branches independently.
Join result types via LUB.
**Impact:** Major — `if` becomes a special form. New AST pattern matching
infrastructure for condition analysis. Environment forking doubles
allocation in conditional code.

### Type Representation (`src/types.rs`)

**Current:** No narrowing-related types.
**Proposed:** No changes to `Type` enum — narrowing uses existing types
(`IntLiteral`, `StringLiteral`, `Record`). The `Narrowing` enum is local
to the type checker.
**Impact:** None.

### Evaluator (`src/eval.rs`)

**Current:** No runtime narrowing.
**Proposed:** No changes. Narrowing is purely static — no additional
guards or checks at runtime beyond what TypeAssert already provides.
**Impact:** None.

### Parser / Grammar

**Current:** No changes needed.
**Proposed:** No changes — narrowing is a type checker feature, not a
syntax feature.
**Impact:** None.

### Type Map (LSP integration)

**Current:** Each expression has one type in the type map.
**Proposed:** Variables in narrowed branches get their refined type in
the type map. LSP hover shows the narrowed type.
**Impact:** Minor — type map entries are updated with narrowed types, no
structural changes to the type map itself.

## Phased Adoption

### Phase 1: Equality and Type-of Narrowing

Add `if` as a type-level special form. Implement Pattern 1
(equality with literal) and Pattern 2 (type-of guard). These two
patterns cover the most common narrowing scenarios and validate the
environment-forking infrastructure.

### Phase 2: Key Presence and Conjunction

Add Pattern 3 (key presence via `has?`) and Pattern 4 (boolean
conjunction via `and`). Key presence narrowing is particularly valuable
for config validation — checking whether a dict has a required field
before accessing it.

### Phase 3: Type Predicate Integration

When type predicates are available (see `doc/whatif/type-predicates.md`),
extend Pattern 2 to recognize `[int? x]`, `[str? x]`, etc. as
direct narrowing triggers without the `type-of` indirection.

### Phase 4: False-Branch Narrowing (deferred)

If algebraic subtyping is adopted (see `doc/whatif/algebraic-subtypes.md`),
negation types become available, enabling false-branch narrowing:
after `[= x "hello"]`, the false branch knows
`x : Str \ StringLiteral("hello")`.

### Prerequisites

- Phase 1 requires `let-generalization` (narrowing refines type schemes),
  `bidirectional-typing` (narrowing feeds into checking mode), and
  `typeassert-structural` (narrowing complements explicit contracts)
- Phase 2 has no additional prerequisites beyond Phase 1
- Phase 3 requires `type-predicates` Phase 1 (see
  `doc/whatif/type-predicates.md`)
- Phase 4 requires `algebraic-subtypes` adoption (see
  `doc/whatif/algebraic-subtypes.md`)

### Trigger

- Begin Phase 1 when `bidirectional-typing` and `typeassert-structural`
  sprints are complete — narrowing builds directly on both.
- Begin Phase 1 when user code contains repeated `[@Type expr]` assertions
  in `if` branches that could be inferred from the condition — narrowing
  eliminates this boilerplate.
- Revisit Phase 4 (false-branch narrowing) if algebraic subtyping is
  adopted and negation types become available.

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
