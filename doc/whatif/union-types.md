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

**Three-phase adoption: type classes first, then annotation-only unions,
then inferred unions via Simple-sub.**

### Phase 1: Type Classes (no union types)

Solve the dual-dispatch typing problem without union types by adding
constrained polymorphism (see `doc/whatif/typeclasses.md`):

```
$map : Functor f => (a → b) → f a → f b
```

where `Functor` has instances for Dict and Seq. This covers the primary
motivation (precise typing for `$map`, `$filter`, etc.) without any union
type machinery.

### Phase 2: Annotation-Only Unions

Add `Type::Union(Vec<Type>)` with subtyping rules [UNION-INJ-L],
[UNION-INJ-R], [UNION-ELIM]. Unions appear only in explicit type
annotations and builtin signatures — `unify` never produces them
(Option A from §Unification above).

This enables:
- `Int | Null` for nullable values
- `[ok: a] | [err: String]` for result types
- Explicit union annotations on user-defined functions

Implementation: add `Union` variant to `Type`. Extend `is_subtype` with
three new rules. Extend `is_consistent` (from gradual typing, see
`doc/whatif/gradual-typing.md`) for union consistency. No changes to
`unify` — unions are not inferred, only checked.

### Phase 3: Inferred Unions (Simple-sub)

If annotation-only unions prove insufficient — specifically, if `$if`
return types and other inferred positions need unions — adopt Simple-sub
(Parreaux 2020) for full algebraic subtyping with inferred union and
intersection types. See `doc/whatif/algebraic-subtypes.md` for the
complete analysis.

This is the heaviest phase: `unify` becomes `constrain`, type variables
carry bounds instead of bindings, and `Any` must split into
Top/Bottom/Gradual. Only adopt if Phases 1–2 are insufficient.

### Prerequisites

- Phase 1 requires `let-generalization` and `builtin-type-signatures`
- Phase 2 requires Phase 1 (type classes provide the Functor abstraction
  that makes unions less urgent) and `gradual-typing` Phase 2 (splitting
  `Any` into `Unknown` + `Top`)
- Phase 3 requires `doc/whatif/algebraic-subtypes.md` adoption

### Trigger

Phase 1 (type classes): see `doc/whatif/typeclasses.md` §Trigger.

Phase 2 (annotation unions): begin when:
- Nullable types are needed (`Int | Null` instead of `Any`)
- Tagged union / sum type patterns become common in user code
- `$try` result types need `[ok: a] | [err: String]` precision

Phase 3 (inferred unions): begin when:
- `$if` return types need inferred unions (not just annotated)
- Annotation-only unions create too much annotation burden
- Row polymorphism needs width subtyping built into the lattice

## References

- Dolan, S. & Mycroft, A. (2017). "Polymorphism, subtyping, and type
  inference in MLsub." In *POPL '17*, pp. 228–242. ACM.
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In
  *ICFP '20*, Article 124. ACM.
- Lorenz, J. et al. (2024). "Towards algebraic subtyping for extensible
  records." arXiv:2407.06747.
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press.
  Chapter 15 (subtyping), Chapter 24 (existential types).
