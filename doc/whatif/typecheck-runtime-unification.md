# What If: Unify Type-Checker and Runtime Type Judgments

**State:** Proposal

What would it take to make tinct's static type-checking path and its runtime type-checking path derive from a single source of truth?

## The Problem

Tinct currently maintains two distinct type judgment systems that can disagree:

**Static path** (`src/typecheck.rs`, `src/type_env.rs`): Uses hand-written `TypeEnv` signatures and `is_subtype()` for structural subtyping during type inference.

**Runtime path** (`src/eval_materialize.rs`): Uses `value_matches_type()` when `TypeAnnotationTable` has the node, or `type_name()` string comparison as a fallback for `$include`d files. These are different relations from `is_subtype()`.

This split causes four concrete classes of bugs:

1. **TypeEnv signature drift** — manually written entries diverge from what the Rust builtins actually do. `slurp` is typed `String | Bytes` but always returns `String` for text handles. `open`'s mode argument is typed `String` but the runtime expects a `Variant` flag.
2. **`check_X` special cases** — `check_open`, `check_get`, and others hardcode runtime dispatch knowledge in the type checker rather than expressing it in the type language.
3. **Two subtyping relations** — `value_matches_type()` and `is_subtype()` implement different judgments, so the same value can pass static checking but fail at runtime or vice versa.
4. **String fallback** — `RuntimeTypeCheck` uses `type_name()` string comparison for `$include`d code, diverging from the structural path with known bugs (`"Fn"` vs `"Function"`, `"Handle"` vs `"WriteHandle"`).

These aren't four separate issues. They share a root cause: **the type system is not the single authority**. The runtime re-implements type checking independently, and the TypeEnv is a second independent source of truth for builtin signatures.

## The Root: `Unknown` Conflation

A deeper issue underlies all four classes. `Type::Unknown` is used for four distinct situations that require different treatment:

| Source | Example | Should be |
|--------|---------|-----------|
| User writes `@Unknown` or leaves param unannotated | `[fn [x] x]` with no annotation | `Type::Unknown` — AGT gradual `?`, valid and permanent |
| TypeEnv entry can't express the return type | `builtin-map → Unknown` | `TypeVar` + constraint, or overloaded scheme |
| TypeVar unconstrained at generalization | `α` with no bounds | `∀α. α` — generalize to polymorphic |
| `failed_bindings` cascade | `dep-names` Unknown because `cargo-toml-text` T003'd | `Type::Error` |

Garcia & Cimini (2015) prove that `?` (Unknown) and type variables are fundamentally different: a type variable is resolved by unification and will eventually become concrete; `?` is a *permanent* gradual choice that is never resolved. Conflating them causes:

- **T010 fires incorrectly**: `@Unknown` is a valid user choice (AGT `?`), not a warning-worthy inference gap
- **E099 runtime crashes**: source 4 producing `Unknown` instead of `Type::Error` looks like a gradual type to the lowering pass, which emits `CoreExpr::Error` nodes for downstream references — crashing the runtime even though typecheck is advisory
- **`value_matches_type` can't be unified with `is_subtype`** until `Unknown` means only one thing: AGT `?`

## Design

The proposal has three mutually reinforcing components. They can be implemented incrementally but each makes the others cleaner.

### Component 1: Clean Up `Unknown`

**`Type::Unknown` means only AGT gradual `?`.** Every other use gets the correct representation:

- **Inference failure** (`failed_bindings` cascade): produce `Type::Error` for dependent entries. `Type::Error` already absorbs correctly in `unify()` and `is_subtype()` — no new machinery. This directly fixes the E099 cascade bug.
- **Inference limitation** (TypeEnv can't express the return): use `TypeVar` pending resolution by the overloading mechanism (Component 2). Only degrade to `Unknown` when no TypeVar resolution is possible and the result is genuinely gradual.
- **Unconstrained TypeVar at generalization**: generalize to `∀α. α` (already done for function params). For CHR constraints that provably cannot be satisfied, produce `Type::Error` rather than `Unknown`.

**T010 becomes meaningful**: fires when inference produces `Unknown` at a position the user annotated as concrete — a real diagnostic gap. Stays silent for unannotated positions where gradual `?` is the correct interpretation.

### Component 2: The Type System is the Single Authority for Builtin Signatures

Replace the hand-written `TypeEnv::builtin_type_env()` entries with a registration mechanism that derives type signatures from a single canonical description:

```rust
builtin_typed!(
    "slurp",
    builtin_slurp,
    [Strictness::Seq],
    overloads: [
        // Handle[Readable] (text) → String
        (Handle(Row::single("Readable", Bool)), String),
        // Handle[Readable Binary] → Bytes
        (Handle(Row::with("Readable", Bool).and("Binary", Bool)), Bytes),
        // Handle[Unknown] → String | Bytes (fallback)
        (Handle(Unknown), Union([String, Bytes])),
    ]
);
```

For dependent return types — where the return depends on an argument's precise type or capability flags — the signature is expressed as an ordered list of *overloads*. The type checker applies overloads in order, taking the first that matches. This replaces all `check_X` special cases in `typecheck.rs` with a uniform overload resolution mechanism:

```
open : (DirCap, String, #Readable)          → Handle[Readable]
     | (DirCap, String, #Writable)          → Handle[Writable]
     | (DirCap, String, #[Readable Binary]) → Handle[Readable Binary]
     | (DirCap, String, Unknown)            → Handle[Unknown]

slurp : (Handle[Readable])          → String
      | (Handle[Readable Binary])   → Bytes
      | (Handle[Unknown])           → String | Bytes

tls-layer : (Handle[α], String, Top) → Handle[α]   # row polymorphism — same cap row as input
```

The overload resolver knows what the type checker previously had to hard-code. `check_open`, `check_get`, `check_tls_layer` become unnecessary — their logic lives in the type language.

The `Indexable` typeclass (tracked separately as `indexable-typeclass` sprint) handles `get` and `get?` via MPTC functional dependencies, which is the principled version of the same idea.

### Component 3: One Subtyping Relation

With Components 1 and 2 in place, the runtime check can be unified with the static check.

**Define `value_matches_type(v, T)` as `is_subtype(ground_type_of(v), T)`**, where `ground_type_of` extracts the precise static type of a runtime value — no TypeVars, no Unknown, concrete:

```rust
fn ground_type_of(v: &Value) -> Type {
    match v {
        Value::Int(_)        => Type::Int,
        Value::String(_)     => Type::Str,
        Value::Bool(_)       => Type::Bool,
        Value::Float(_)      => Type::Float,
        Value::Bytes(_)      => Type::Bytes,
        Value::Dict(_)       => Type::Record(extract_row(v)),
        Value::Seq(_)        => Type::Seq(Box::new(Type::Unknown)), // element type erased
        Value::Function { .. } => Type::Function { .. },            // arity-matched
        Value::Handle { caps, .. } => Type::Handle(Box::new(caps.to_row_type())),
        Value::Variant { tag, .. } => Type::NominalVariant { tag: tag.clone(), .. },
        // ...
    }
}
```

This is AGT Theorem 3.5 (Garcia et al. 2016): *the runtime check is the restriction of the static relation to ground types*. One function, one correctness invariant. `is_subtype(Handle[Readable], Handle[Readable Writable])` works the same way in the static checker and at runtime.

**Delete the `RuntimeTypeCheck` string fallback** once `TypeAnnotationTable` is populated universally (tracked in existing TODO). The fallback path is the only remaining independent re-implementation; with it gone, there is no second type checking system.

## What This Achieves

- **"Lint passes = safe to run" becomes sound.** T003 in the type checker means the runtime check *will* fail; T002 means the variable *will* be missing. No surprises.
- **Adding a builtin is a single change.** The Rust implementation, the TypeEnv signature, and the runtime behavior are one registration. Drift is structurally impossible.
- **`@[Handle Readable]` TypeAssert works.** Currently broken (T000 double-write bug) because the static and runtime Handle representations don't agree. With `ground_type_of` + `is_subtype`, they use the same representation.
- **T010 warnings are actionable.** They fire only for genuine inference gaps, not for valid gradual uses of `Unknown`.
- **The E099 cascade bug is fixed.** `failed_bindings` → `Type::Error`, not `Unknown`, so the lowering pass doesn't create `CoreExpr::Error` nodes for reachable variables.

## What Would Change

### `src/type_env.rs`

**Current:** ~3500 lines of hand-written TypeEnv entries, each an independent source of truth.

**Proposed:** Entries generated by `builtin_typed!` with overload lists. The file shrinks significantly; type signatures live adjacent to the Rust implementations they describe. Overloaded schemes replace `check_X` special cases throughout.

**Impact:** Major — affects every builtin registration, but mechanical.

### `src/typecheck.rs`

**Current:** `check_open`, `check_get`, and others encode per-builtin type logic as special cases.

**Proposed:** These are replaced by the uniform overload resolver. The type checker applies overload schemes without per-builtin knowledge.

**Impact:** Moderate — removes special cases, adds overload resolution (general mechanism).

### `src/typecheck_dict.rs` and `src/type_unify.rs`

**Current:** `failed_bindings` entries produce `Type::Unknown` for dependents. Undischarged CHR constraints silently degrade to `Type::Unknown`.

**Proposed:** `failed_bindings` entries produce `Type::Error` for dependents. CHR constraints that provably cannot be satisfied produce `Type::Error`; genuinely unconstrained TypeVars generalize normally.

**Impact:** Minor code change, large correctness benefit. Directly fixes E099 cascade.

### `src/eval_materialize.rs`

**Current:** `value_matches_type` and `RuntimeTypeCheck` are independent implementations that can diverge from `is_subtype`.

**Proposed:** `value_matches_type` = `is_subtype(ground_type_of(v), T)`. `RuntimeTypeCheck` string fallback deleted once `TypeAnnotationTable` is universal.

**Impact:** Moderate — simplifies the TypeAssert evaluation path significantly.

### `src/builtins.rs` (and `src/builtins_*.rs`)

**Current:** `builtin!` macro captures runtime behavior only.

**Proposed:** `builtin_typed!` macro captures runtime function and overloaded type signature together.

**Impact:** Moderate — mechanical update to all builtin registrations.

## Open Design Questions

Panel review (computer-scientist, type-theorist, eval-engine) identified six issues requiring resolution before implementation.

### 1. Overload resolution with TypeVar arguments

The most significant gap. When `slurp h` is called and `h` is a TypeVar (not yet resolved), ordered overload resolution cannot determine which overload applies. First-match would commit to `Handle[Readable] → String`, but the most general unifier is `Handle[Unknown] → String | Bytes`. This breaks principal types.

Three options:

**(a) Ground-type restriction**: Only apply overloads when the relevant argument types are fully concrete. When a TypeVar is present, defer to the `Unknown` fallback overload. This is conservative but tractable.

**(b) Constraint-based deferral (CHR-style)**: Generate an overload constraint that is discharged when the TypeVar is eventually bound to a concrete type. Integrates with the existing CHR constraint system.

**(c) Typeclass for all dependently-typed builtins**: The `Indexable` typeclass approach (already used for `get`) is principled — use MPTC/FD for every builtin whose return type depends on argument types. Overloaded schemes are then only used as TypeEnv surface notation, not as the type inference mechanism.

Option (c) is the most consistent with tinct's existing CHR infrastructure. Options (a) or (b) may be sufficient for the Handle capability row cases (`slurp`, `open`, `tls-layer`) where the argument type is almost always known statically.

### 2. `Seq(Unknown)` in `ground_type_of` is not AGT Theorem 3.5

`ground_type_of` maps `Value::Seq` to `Type::Seq(Box::new(Type::Unknown))` because element types are erased in the lazy Seq representation. Under consistent subtyping, `Seq(?) ~<: Seq(Int)`, so `[@Seq[Int] expr]` TypeAsserts are **always** satisfied at runtime for any Seq — even one containing strings. This is a deliberate deviation from strict AGT ground types.

This is acceptable given tinct's lazy evaluation model (forcing all elements to check types would break laziness), but must be documented clearly: **Seq element TypeAsserts are static-only**. The "lint passes = safe to run" guarantee applies to tag-level checks (is the value a Seq?) but not element type checks. A T014 diagnostic ("TypeAssert on Seq[T] validates the Seq tag only — element types are checked statically but not enforced at runtime") would make this explicit.

The same applies to `Value::Dict` field types: `ground_type_of` must extract only the key set, not recurse into field values to determine their types. Recursing would force lazy thunks, violating tinct's laziness invariant. The `extract_row(v)` function must be specified as key-only extraction.

### 3. `Type::Error` semantics

The proposal's wording "Error absorbs correctly" is imprecise. `Type::Error` does not absorb — it *rejects*: `is_subtype(Error, X)` and `is_subtype(X, Error)` both return false. This is the correct behavior: a dependent entry with a failed inference cannot be used anywhere, producing a loud failure rather than a silent Unknown cascade. The phrase should be "Error *propagates* correctly — dependent entries fail loudly rather than degrading silently to Unknown."

### 4. Performance: fast-path for primitive types

`is_subtype` is a recursive structural comparison. For the common case of primitive types (`Int`, `Str`, `Bool`, `Float`, `Bytes`), a discriminant-only fast-path must precede the structural check to avoid allocation:

```rust
fn value_matches_type(v: &Value, t: &Type) -> bool {
    // Fast path: both sides are primitive — simple discriminant check
    match (v, t) {
        (Value::Int(_), Type::Int) | (Value::String(_), Type::Str) |
        (Value::Bool(_), Type::Bool) | (Value::Float(_), Type::Float) |
        (Value::Bytes(_), Type::Bytes) => true,
        // Fall through to structural is_subtype for complex types
        _ => is_subtype(&ground_type_of(v), t),
    }
}
```

For Record types, the structural check (`is_subtype` with field iteration) is unavoidable when full subtyping semantics are needed, but `ground_type_of` must lazily extract fields — only compute field types when `is_subtype` descends into them.

### 5. Dynamic TypeAsserts and the RuntimeTypeCheck fallback

Not all TypeAssert nodes have `TypeAnnotationTable` entries. Dynamically-generated TypeAsserts (from macro expansion or `$map [fn [x] [@Int x]] items` patterns) are created at runtime without corresponding AST nodes in the table. Deleting `RuntimeTypeCheck` would break these.

Two options:
- **Keep a minimal fallback** using `value_matches_type` (not the string comparison) for entries not in the table. This is better than the current string fallback but acknowledges that full AGT guarantees require a table entry.
- **Make lowering fail hard** when a TypeAssert node has no table entry. This catches cases where type info was not threaded through, forcing the issue upstream.

The proposal should not promise deletion of `RuntimeTypeCheck` until it is confirmed that every reachable TypeAssert node is guaranteed a table entry.

### 6. `Unknown` inside composite annotations

The four-source separation handles top-level `Unknown`, but `Unknown` can appear inside composite types from user annotations: `@[Seq Unknown]`, `@[Map String Unknown]`, `@[Fn@Unknown [Int]]`. Under AGT, these are valid gradual composite types where `?` fills specific positions. The proposal's `is_consistent` / `is_subtype` distinction must handle embedded `?` correctly — using `is_consistent` (not `is_subtype`) at any position where `Unknown` appears in the annotation type.

## Prerequisites

- **`TypeAnnotationTable` through `$include`**: Required to delete or simplify the `RuntimeTypeCheck` fallback. Tracked in existing TODO.
- **Overload resolution semantics**: Component 2 requires a design decision (§Open Questions §1) on how overloads interact with TypeVar arguments during HM inference.
- **`ground_type_of` specification**: Component 3 requires a precise specification of what `extract_row(v)` does for Dict values (key-only, no forced values) and what ground types are produced for each `Value` variant.
- **Fast-path for `value_matches_type`**: Component 3 requires the primitive fast-path to avoid performance regression on common TypeAssert patterns.

## References

- Garcia, R., Clark, A.M. & Tanter, É. (2016). "Abstracting Gradual Typing." *POPL '16*, pp. 429-442. — AGT Theorem 3.5: runtime checks must be the restriction of the static relation to ground types. Grounds Component 3.
- Garcia, R. & Cimini, M. (2015). "Principal Type Schemes for Gradual Programs." *POPL '15*, pp. 303-315. — Proves `?` and type variables are fundamentally distinct; `?` is never resolved, TypeVars always are. Grounds the four-source `Unknown` separation.
- Milner, R. (1978). "A Theory of Type Polymorphism in Programming." *JCSS* 17(3). — Phase consistency: static and dynamic semantics must agree on well-typed programs.
- Wadler, P. & Findler, R.B. (2009). "Well-Typed Programs Can't Be Blamed." *ESOP '09*, pp. 1-16. — Blame theorem; grounds the `Type::Error` vs `Type::Unknown` distinction at type boundaries.
- Crary, K. & Weirich, S. (2000). "Intensional Polymorphism in Type-Erasure Semantics." *JFP* 10(4). — Type erasure as the gold standard for eliminating runtime type representation overhead.
