# What If: Union Types for tinct

What would it take to add union types to tinct's type system?

## Current State

tinct cannot express "a value that is either type A or type B." The main
consequence: dual-dispatch builtins (`$map`, `$filter`, `$take`, `$drop`,
`$reduce`, `$join`) accept both Dict and Seq but are typed as `Any` because
`Dict | Seq` is not expressible.

```
# Current: imprecise
$map : Any → Any → Any

# Desired: precise
$map : (a → b) → (Dict a | Seq a) → (Dict b | Seq b)
```

Other uses for union types:
- `$if` return type: `$if $cond $then-branch $else-branch` should have type
  `type($then) | type($else)`, not `Any`
- Nullable values: `Int | Null` instead of `Any`
- Error returns: `Result a = [ok: a] | [err: String]`

## Approaches

### 1. Simple Union Types (TypeScript-style)

Add `Type::Union(Vec<Type>)` with subtyping:

```
τ <: τ | σ                    [UNION-INJ-L]
σ <: τ | σ                    [UNION-INJ-R]
τ | σ <: ρ  if τ <: ρ ∧ σ <: ρ  [UNION-ELIM]
```

Union elimination (narrowing) requires control flow analysis or explicit
type guards.

**Pros:** Directly solves the dual-dispatch typing problem.
**Cons:** Interacts with unification — `unify(α, Int | Str)` needs union
constraint solving, not Robinson unification. This is exactly the problem
algebraic subtyping (see `doc/whatif/algebraic.md`) solves.

### 2. Algebraic Subtyping (Dolan & Mycroft 2017)

Union types as part of a full algebraic subtyping system. Type variables
carry upper and lower bounds. Unions appear in positive (output) positions,
intersections in negative (input) positions.

See `doc/whatif/algebraic.md` for the full analysis. Summary: 9 subsystems
change, `Any` must split into Top/Bottom/Gradual, error messages degrade.

### 3. Overloaded Functions (No Union Types)

Don't add union types. Instead, type dual-dispatch builtins using overloaded
function types (requires type classes):

```
$map : Functor f => (a → b) → f a → f b
```

where `Functor` has instances for both Dict and Seq.

See `doc/whatif/typeclasses.md` for the type classes analysis.

### 4. Tagged Unions via Records (No Type System Change)

Use record types with a tag field as a convention:

```lisp
# Convention: tagged union via records
result: [ok: $value]     # or [err: "message"]
# Type: [ok: a] or [err: String] — but no union type to express this
```

**Pros:** No type system change. Already works in tinct.
**Cons:** No static checking. `$if` still returns `Any`.

## Interaction with Existing Type System

### Unification

`unify(Int, Str)` currently fails. With union types, what should happen?

- **Option A:** Still fails — unions only appear in annotations, not from
  inference. `unify` never produces unions.
- **Option B:** Produces `Int | Str` — unification becomes join in the
  type lattice. This requires algebraic subtyping.

Option A is simpler but means `$if true 1 "hello"` still infers `Any`
(no union from inference). Option B requires the full algebraic subtyping
migration.

### Row Polymorphism

Union types interact with records: `[x: Int] | [y: Str]` — is this a
record type? Width subtyping? The interaction is complex and underspecified
in most union type systems. Lorenz et al. (2024) address this for algebraic
subtyping specifically.

### `Any` Semantics

`Any` is currently used where union types would be more precise. Splitting
`Any` usage into `Union(...)` and `Unknown` (gradual typing) would require
auditing all `Any` sites. See `doc/whatif/gradual-typing.md`.

## Recommendation

**Don't adopt now.** Union types require either algebraic subtyping (heavy)
or type classes (moderate) to be useful. The `Any` typing for dual-dispatch
builtins is imprecise but not causing real errors — the runtime dispatch
catches type mismatches.

**Revisit when:**
- The `Any` typing for dual-dispatch builtins causes a real false positive
  (program type-checks but fails at runtime because `$map` was called on
  a non-Dict/non-Seq value and the type checker didn't catch it)
- Nullable types are needed (currently tinct uses `$if` with Null checks)
- Tagged union / sum type patterns become common in user code

**If we do adopt,** the path of least resistance is:
1. Add type classes first (see `doc/whatif/typeclasses.md`)
2. Type dual-dispatch builtins with `Functor f => ...` constraints
3. If more expressiveness is still needed, consider Simple-sub
   (Parreaux 2020) for union types with algebraic subtyping

## References

- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type
  inference in MLsub." In *POPL '17*, pp. 228–242. ACM.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In
  *ICFP '20*, Article 124. ACM.
- Lorenz, J. et al. (2024). "Towards algebraic subtyping for extensible
  records." arXiv:2407.06747.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  Chapter 15 (subtyping), Chapter 24 (existential types).
