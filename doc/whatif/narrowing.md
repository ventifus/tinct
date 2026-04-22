# What If: Path-Sensitive Type Narrowing

What would it take to narrow types in conditional branches based on
equality checks and type guards?

## Status
Proposal — not approved for implementation.

**Assumes:** `typeassert-structural` sprint is complete (TypeAssert
elaboration, `ThunkState::Guarded`, proxy contracts, structural contract
validation).

## Problem

Tinct has singleton literal types (`IntLiteral(42)`,
`StringLiteral("hello")`) that are subtypes of their base types. After
an equality check, the type checker could narrow the variable's type in
the true branch:

```lisp
[if [= $x "hello"]
  $x       ;; narrowed: StringLiteral("hello") instead of Str
  $x]      ;; stays Str
```

Currently the type checker has no path sensitivity — `$x` has the same
type in both branches. `$if` is a builtin call (`Sc × Sc → Θ` in the
strictness table), not a type-level special form.

## Current State

- `$if` is typed via `check_call` like any other builtin — no
  branch-specific type environments
- `IntLiteral`/`StringLiteral` exist as types but only arise from literal
  expressions, never from narrowing
- `$=` is typed `Any → Any → Bool` — no narrowing information
- TypeAssert (post `typeassert-structural`) provides explicit narrowing:
  `[@String $x]` narrows `$x` to `String` via `ThunkState::Guarded`
  proxy contracts. This is user-driven, not automatic.

## Design

### Making `$if` a Type-Level Special Form

The type checker gains a dedicated rule for `$if` (and `$cond`, `$when`,
`$unless` by extension):

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
shared base type. With union types (see `doc/whatif/algebraic.md`), the
result is `τ₁ | τ₂`.

### Narrowing Patterns

The `narrow()` function recognizes specific condition shapes. Each pattern
has a true-branch refinement. False-branch refinement requires negation
types and is deferred (the false branch gets the original unrefined
environment).

#### Pattern 1: Equality with Literal

```lisp
[= $x "hello"]
```

- **True branch:** `$x : StringLiteral("hello")`
- **False branch:** `$x : Str` (no negation — would need `Str \ {"hello"}`)

Recognizes `$=` with one operand being a `VarRef` and the other a literal
expression. Both operand orderings are recognized (`[= "hello" $x]` and
`[= $x "hello"]`).

#### Pattern 2: Type-of Guard

```lisp
[= [type-of $x] "Int"]
```

- **True branch:** `$x : Int`
- **False branch:** `$x` unchanged

Recognizes `$=` where one operand is `[call $type-of var]` and the other
is a string literal matching a known type name. Maps `"Int"` → `Type::Int`,
`"Float"` → `Type::Float`, `"String"` → `Type::Str`, `"Bool"` →
`Type::Bool`, `"Dict"` → `Type::Record([], Open)`, `"Seq"` →
`Type::Seq(Any)`, `"Function"` → `Type::Any` (can't narrow further).

#### Pattern 3: Key Presence

```lisp
[has? $x "name"]
```

- **True branch:** `$x : Record([name: α], Open)` where `α` is fresh —
  the record is known to have a `name` field
- **False branch:** `$x` unchanged

Recognizes `$has?` with a `VarRef` and a string literal key. Narrows
the record type to include the key with a fresh type variable. If `$x`
already has a record type, the key is added to the existing fields.

#### Pattern 4: Boolean Conjunction

```lisp
[and [= $x "hello"] [has? $y "name"]]
```

Conjunction (`$and`) applies both narrowings to the true-branch
environment. Disjunction (`$or`) applies the intersection of narrowings
(only narrow if both branches agree — rare, usually no narrowing).

### Implementation

#### Type Checker Changes (`src/typecheck.rs`)

1. **Detect `$if` calls.** In `check_call` or `infer_expr`, when the
   callee resolves to the `$if` builtin, dispatch to a dedicated
   `infer_if(cond, then_expr, else_expr, env, state, type_map)` function
   instead of the generic call inference path.

2. **Extract narrowing constraints.** `extract_narrowings(cond_expr, env)`
   → `Vec<Narrowing>` where:
   ```rust
   enum Narrowing {
       EqLiteral { var: String, ty: Type },
       TypeOf { var: String, ty: Type },
       HasKey { var: String, key: String },
   }
   ```
   Pattern matching on the condition AST to detect the recognized shapes.

3. **Fork the type environment.** Create `env_true` by cloning `env` and
   applying narrowings. Create `env_false` as a clone of `env` (no
   false-branch narrowing initially).

4. **Infer branches independently.** `infer_expr(then, &env_true, ...)`
   and `infer_expr(else, &env_false, ...)`.

5. **Join result types.** Compute LUB of the two branch types. Without
   union types, this is the nearest common supertype in the lattice.

#### Stdlib Narrowing (`$cond`, `$when`, `$unless`)

These are stdlib functions defined in terms of `$if`. Two options:

- **Inline narrowing:** Make `$cond`, `$when`, `$unless` also special
  forms in the type checker. More precise but more hardcoded forms to
  maintain.
- **Defer:** Only narrow for `$if`. Users who need narrowing in `$cond`
  branches can use explicit TypeAssert. Simpler.

Recommended: start with `$if` only.

#### Interaction with TypeAssert

TypeAssert (`[@Type expr]`) already narrows via elaboration + proxy
contracts. Path-sensitive narrowing complements it:

- **TypeAssert:** explicit, user-written, works anywhere, runtime-checked
- **Path narrowing:** implicit, automatic in `$if` branches, static only

Both can coexist. If `$x` is narrowed by a condition AND has a TypeAssert,
the narrower type wins (intersection of the two refinements).

#### Interaction with Lazy Evaluation

Narrowing assumes the condition is evaluated before the branches. This
is semantically true for `$if` (strict in condition, selective in
branches — DESIGN.md §Selective Materialization), so the narrowing is
sound: the condition has been forced, establishing the type fact, before
either branch is entered.

For `$cond`, the same applies to each condition-value pair (conditions
are forced sequentially).

### Cost Model

- **Per-`$if` cost:** One environment clone per branch (two clones total).
  In programs with nested conditionals, this multiplies. A chain of 10
  nested `$if`s creates 20 environment clones.
- **Pattern matching cost:** `extract_narrowings` walks the condition AST
  once per `$if`. Negligible for simple conditions, linear in condition
  complexity for conjunctions.
- **No runtime cost.** Narrowing is purely static — no additional guards
  or checks at runtime beyond what TypeAssert already provides.

### Limitations

1. **No false-branch narrowing.** Requires negation types (`Str \ {"hello"}`)
   which tinct does not have. The false branch gets the unrefined type.
   Revisit if algebraic subtyping is adopted (`doc/whatif/algebraic.md`).

2. **Only `$if` initially.** `$cond`, `$when`, `$unless`, and user-defined
   conditional patterns are not narrowed. Explicit TypeAssert is the
   fallback.

3. **Shallow pattern recognition.** Only the four patterns above are
   recognized. Complex conditions (`[and [= $x 1] [= $y 2]]` nested
   inside function calls) are not decomposed.

4. **No narrowing across function boundaries.** If a function returns
   `Bool` based on a type check, the caller can't narrow based on the
   return value. This requires type predicates / type guard declarations
   (TypeScript's `x is SomeType`), which are not proposed here.

5. **Let-binding interaction.** `[let [ok: [= $x "hello"]] [if $ok ...]]`
   — the narrowing is lost because `$ok` is a variable, not the original
   condition expression. Narrowing only works when the condition is inline
   in `$if`.

## What We'd Gain

1. **More precise types in conditional branches.** After `[= $x "hello"]`,
   the true branch knows `$x` is exactly `"hello"`, enabling better type
   checking for subsequent operations.

2. **Better LSP hover information.** Hovering over `$x` in a narrowed
   branch shows the refined type, not the original broad type.

3. **Foundation for pattern matching.** If tinct gains sum types or tagged
   unions, narrowing is the mechanism for discriminated union checking.

4. **Interplay with TypeAssert.** Guards that the user currently writes
   explicitly (`[@String $x]`) can be inferred automatically from
   conditional context, reducing annotation burden.

## What We'd Lose / Risk

1. **Type checker complexity.** `$if` becomes a special form instead of a
   regular builtin call. The type checker gains AST pattern matching for
   conditions. Environment forking doubles allocation in conditional code.

2. **Maintenance burden.** Each narrowing pattern is hardcoded — new
   builtins or patterns require type checker changes.

3. **Surprise narrowing.** Users may not expect the type to change between
   branches. Error messages must explain narrowing clearly ("type narrowed
   to StringLiteral("hello") because of equality check at line 5").

4. **Low ROI for config.** Typical config files have few conditionals.
   The feature primarily benefits programs with significant branching
   logic, which is uncommon in tinct's target domain.

## Recommendation

Implement path-sensitive narrowing for `$if` with the four patterns above.
The implementation cost is moderate (one special form, AST pattern matching,
environment forking) and the infrastructure pays forward: if tinct gains
sum types, narrowing becomes essential for discriminated union pattern
matching.

Start with `$if` only, equality-with-literal and type-of-guard patterns.
Add has-key and conjunction as follow-ups. Defer false-branch narrowing
and cross-function narrowing.

## Trigger

Implement after:
- `let-generalization` (narrowing refines type schemes)
- `bidirectional-typing` (narrowing feeds into checking mode)
- `typeassert-structural` (narrowing complements explicit contracts)

Revisit false-branch narrowing if algebraic subtyping is adopted (negation
types become available).

## References

- Dunfield, J. & Pfenning, F. (2004). "Tridirectional typechecking."
  In *POPL '04*, pp. 281–292. ACM.
  — Datasort refinements, singleton types as refinements of base types.
- TypeScript Handbook. "Narrowing."
  — Practical narrowing patterns: typeof guards, equality narrowing,
  truthiness narrowing, discriminated unions.
- Tobin-Hochstadt, S. & Felleisen, M. (2010). "Logical types for untyped
  languages." In *ICFP '10*, pp. 117–128. ACM.
  — Occurrence typing in Typed Racket: type narrowing via predicates in
  conditionals, foundational for flow-sensitive typing.
