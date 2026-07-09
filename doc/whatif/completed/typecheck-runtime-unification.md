# What If: Unify Type-Checker and Runtime Type Judgments

**State:** Completed — 2026-05-28

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

The proposal has three mutually reinforcing components. Despite appearances, they are **not independent** — the correct implementation sequence is **2 → 1 → 3**:

- **Component 2 first**: Establish precise builtin signatures (via typeclasses + FD). Without precise signatures, removing TypeEnv `Unknown` fallbacks in Component 1 regresses builtin call sites to `Type::Error`. Note: the `failed_bindings → Type::Error` change within Component 1 is independent of Component 2 and can ship first — it only affects dict entry inference, not builtin signatures.
- **Component 1 second**: With precise signatures in place, TypeEnv `Unknown` fallbacks can be cleaned up — T010 becomes meaningful. The `failed_bindings` fix may already be done by this point.
- **Component 3 last**: Unify `value_matches_type` with `is_consistent_subtype(ground_type_of(v), T)`. Straightforward once the static types are precise (Component 1 + 2), because the static and runtime judgments now agree on what types mean.

### Component 1: Clean Up `Unknown`

**`Type::Unknown` means only AGT gradual `?`.** Every other use gets the correct representation:

- **Inference failure** (`failed_bindings` cascade): produce `Type::Error` for dependent entries. `Type::Error` is already handled correctly: `unify()` treats it as a no-op (succeeds without binding, preventing cascade errors), while `is_subtype()` rejects it in both directions. No new machinery needed. This directly fixes the E099 cascade bug.
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

```text
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

**Define `value_matches_type(v, T)` as `is_consistent_subtype(ground_type_of(v), T)`**, where `ground_type_of` extracts the ground type of a runtime value and `is_consistent_subtype` is the AGT consistent subtyping relation (`~<:`).

Plain `is_subtype` cannot be used here: `ground_type_of` necessarily produces `Type::Unknown` at erased positions (Seq elements, Dict field values), and `is_subtype(Unknown, X)` = `false` by design — Unknown is not in the subtype lattice. The consistent subtyping relation `~<:` correctly handles this: `Unknown ~<: T` and `T ~<: Unknown` hold for all `T`, so erased positions pass without forcing.

```rust
/// The AGT consistent subtyping relation (Garcia et al. 2016, Proposition 22): A ~<: B.
/// Used for value_matches_type: ground types carry Unknown at erased positions
/// (Seq elements, Dict field values, deferred capability types). Plain is_subtype
/// rejects Unknown; this relation treats Unknown as consistent with all types at any depth.
pub fn is_consistent_subtype(sub: &Type, sup: &Type) -> bool {
    // Unknown on either side: consistent (? ~<: T and T ~<: ? for all T)
    if matches!(sub, Type::Unknown) || matches!(sup, Type::Unknown) {
        return true;
    }
    // Unresolved TypeVar in annotation position: treat as Unknown (gradual)
    if matches!(sup, Type::TypeVar(_, _)) {
        return true;
    }
    // Error is never a consistent subtype of anything
    if matches!(sub, Type::Error) || matches!(sup, Type::Error) {
        return false;
    }
    match (sub, sup) {
        // Primitives: exact match
        (Type::Int,   Type::Int)
        | (Type::Str,   Type::Str)
        | (Type::Bool,  Type::Bool)
        | (Type::Float, Type::Float)
        | (Type::Bytes, Type::Bytes) => true,
        // Top accepts everything
        (_, Type::Top) => true,
        // Structural recursion — consistent subtyping throughout all composite types.
        // The _ => is_subtype fallthrough is NOT used for any composite constructor;
        // every type that can structurally contain Unknown gets its own arm here.
        (Type::Seq(a), Type::Seq(b)) => is_consistent_subtype(a, b),
        (Type::Map(k1, v1), Type::Map(k2, v2)) => {
            is_consistent_subtype(k1, k2) && is_consistent_subtype(v1, v2)
        }
        (Type::Record(sub_row), Type::Record(sup_row)) => {
            // Width subtyping: sub must supply every field sup requires.
            // Field types use consistent subtyping: Unknown field ~<: any annotation.
            sup_row.fields.iter().all(|(field, sup_ty)| {
                sub_row.fields.get(field)
                    .map(|sub_ty| is_consistent_subtype(sub_ty, sup_ty))
                    .unwrap_or(false) // field absent in sub → fails
            })
        }
        // Function: contravariant params, covariant return.
        // ground_type_of erases param/return types to Unknown; consistent subtyping
        // accepts Function([Unknown..], Unknown) against any concrete function annotation.
        (Type::Function { params: sub_p, ret: sub_r, .. },
         Type::Function { params: sup_p, ret: sup_r, .. }) => {
            sub_p.len() == sup_p.len() &&
            sub_p.iter().zip(sup_p.iter()).all(|((_, sub_ty), (_, sup_ty))|
                is_consistent_subtype(sup_ty, sub_ty)) && // contravariant
            is_consistent_subtype(sub_r, sup_r)
        }
        // Union in sup: value is c.s. subtype of union if c.s. subtype of any member
        (_, Type::Union(members)) => members.iter().any(|m| is_consistent_subtype(sub, m)),
        // Intersection in sup: value must be c.s. subtype of all members
        (_, Type::Intersection(members)) => members.iter().all(|m| is_consistent_subtype(sub, m)),
        // Remaining cases (NominalVariant, Handle at static level, etc.): fall to is_subtype.
        // Safe because ground_type_of never produces these with Unknown at structural depth
        // (Handle → Unknown, Variant fields → empty row).
        _ => is_subtype(sub, sup),
    }
}
```

`ground_type_of` extracts the ground type for each `Value` variant. Erased positions become `Type::Unknown`; `is_consistent_subtype` then accepts them against any annotation:

```rust
fn ground_type_of(v: &Value) -> Type {
    match v {
        Value::Int(_)           => Type::Int,
        Value::String { .. }    => Type::Str,
        Value::Bool(_)          => Type::Bool,
        Value::Float(_)         => Type::Float,
        Value::Bytes(_)         => Type::Bytes,
        Value::Dict(map)        => Type::Record(extract_row(map)),
        // Overlay is a lazy right-biased merge: key set cannot be read without forcing.
        // Return a closed empty record — required-field checks correctly fail,
        // consistent with Overlay field validation being static-only.
        Value::Overlay(..)      => Type::Record(Row { fields: HashMap::new() }),
        // Element type erased (lazy Seq — forcing all elements would break laziness).
        // is_consistent_subtype accepts Seq(Unknown) ~<: Seq(T) for any T.
        Value::Seq(_)           => Type::Seq(Box::new(Type::Unknown)),
        // Param/return types erased — consistent subtyping accepts Function([Unknown..], Unknown)
        // against any function annotation with matching arity.
        Value::Function { params, .. } => Type::Function {
            params: params.iter().map(|_| (None, Type::Unknown)).collect(),
            ret: Box::new(Type::Unknown),
            variadic: false,
        },
        // Capability types: row translation deferred to capability-runtime-validation sprint.
        // Unknown → is_consistent_subtype accepts against any Handle/DirCap/NetCap annotation,
        // preserving current accept-all behavior while the sprint is pending.
        Value::Handle { .. }
        | Value::WriteHandle { .. }
        | Value::DirCap { .. }
        | Value::RevocableDirCap { .. }
        | Value::NetCap { .. }  => Type::Unknown,
        // Payload types erased (payload ThunkId has no static type without the schema).
        Value::Variant { tag, .. } => Type::NominalVariant {
            tag: tag.clone(),
            fields: Row { fields: HashMap::new() },
        },
        // Decimal/BigInt: no Type::Decimal/Type::BigInt in the type system yet.
        // Unknown preserves current behavior (matches @Number) until those variants are added.
        Value::Decimal(_)
        | Value::BigInt(_)      => Type::Unknown,
        // Builtin functions and Proxy values: Unknown accepts any function/type annotation.
        Value::Builtin(..)
        | Value::Proxy(..)      => Type::Unknown,
        // Builder is a transient construction artifact — produce Top (type mismatch error)
        // rather than panicking; Builder can reach TypeAssert via e.g. [@Integer [make-builder]].
        Value::Builder(..)      => Type::Top,
        // AST values, async primitives, and other runtime-only types have no static equivalent.
        _                       => Type::Top,
    }
}

/// Extract the ground record type from a Dict: key names only, field types erased to Unknown.
/// MUST NOT force any ThunkId — field types are static-only (same tradeoff as Seq elements).
/// is_consistent_subtype then handles width subtyping: {a: Unknown} ~<: {a: Int} holds
/// because Unknown ~<: Int. Field presence is checked structurally; field types are not.
fn extract_row(map: &IndexMap<Key, ThunkId>) -> Row {
    let fields = map
        .keys()
        .filter_map(|k| match k {
            Key::String(name) => Some((name.to_string(), Type::Unknown)),
            // Integer-keyed entries are explicit [0: x 1: y] dict constructs, not record fields.
            Key::Int(_) => None,
        })
        .collect::<HashMap<String, Type>>();
    Row { fields }
}
```

This implements the dynamic semantics of Garcia et al. (2016) Proposition 22 (Type Safety): *the runtime check is the restriction of the static relation to ground types* — here, the consistent subtyping restriction. One function, one correctness invariant.

**Delete `RuntimeTypeCheck` entirely** once the prerequisites in §Open Questions §5 are met. With it gone, there is no second type checking system — every TypeAssert goes through the same `is_consistent_subtype(ground_type_of(v), T)` path.

## What This Achieves

- **"Lint passes = safe to run" becomes sound.** T003 in the type checker means the runtime check *will* fail; T002 means the variable *will* be missing. No surprises.
- **Adding a builtin is a single change.** The Rust implementation, the TypeEnv signature, and the runtime behavior are one registration. Drift is structurally impossible.
- **`@[Handle Readable]` TypeAssert works.** Currently broken (T000 double-write bug) because the static and runtime Handle representations don't agree. With `ground_type_of` + `is_consistent_subtype`, they use the same representation.
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

**Proposed:** `value_matches_type` = `is_consistent_subtype(ground_type_of(v), T)`. `RuntimeTypeCheck` string fallback deleted once prerequisites are met (see §Open Questions §5).

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

**Option (c) is required for soundness.** Options (a) and (b) both break principal types when argument types are TypeVars that will be bound at the call site: (a) silently degrades to the Unknown fallback overload even when the argument type will become concrete, discarding precision; (b) requires constraint deferral with ordered first-match semantics, which is undecidable in the general case. Option (c) — the same mechanism already used for `get`/`get?` via `Indexable` — is the only path that preserves principal types under HM inference.

### 2. `Seq(Unknown)` in `ground_type_of` is not AGT Theorem 3.5

`ground_type_of` maps `Value::Seq` to `Type::Seq(Box::new(Type::Unknown))` because element types are erased in the lazy Seq representation. Under consistent subtyping, `Seq(?) ~<: Seq(Int)`, so `[@Seq[Int] expr]` TypeAsserts are **always** satisfied at runtime for any Seq — even one containing strings. This is a deliberate deviation from strict AGT ground types.

This is acceptable given tinct's lazy evaluation model (forcing all elements to check types would break laziness). **Seq element TypeAsserts are static-only** — documented in `doc/07-type-extensions.md`. The "lint passes = safe to run" guarantee applies to tag-level checks (is the value a Seq?) but not element type checks.

The same applies to `Value::Dict` field types: `ground_type_of` extracts only the key set, not field values. Recursing into values would force lazy thunks. Field presence is checked structurally; field types are static-only.

### 3. `Type::Error` semantics

`Type::Error` has deliberately asymmetric behavior across the two type operations:

- **`unify(Error, T) = Ok(())`** — succeeds silently without binding (`src/type_unify.rs`). No substitution is modified; the other type is left unchanged. This is correct: if an inference step failed, we don't want to cascade spurious unification errors through all dependent expressions. Inference continues; the error is already recorded at the original failure site.
- **`is_subtype(Error, X) = false` and `is_subtype(X, Error) = false`** — rejection in both directions (`src/type_def.rs:396-399`). A TypeAssert with a `Type::Error` resolved type always fails validation.

The "loud failure" for a `failed_bindings` entry comes from the existing `undefined_variable` error path at use-sites, which attaches a `failed_bindings` note explaining why the variable's type is unavailable. It does not come from `Type::Error` propagating through `unify`.

**One additional implication**: after Component 1, `Type::Error` can appear in the `TypeAnnotationTable` (a `failed_bindings` entry has `resolved_type: Type::Error`). `lower.rs` must detect this case and emit `CoreExpr::RuntimeTypeCheck` (pass-through) rather than `CoreExpr::TypeAssert { resolved_type: Type::Error }`, which would always silently fail in release builds with a misleading runtime error rather than the original static type error.

### 4. Performance

`value_matches_type` must be implemented uniformly as `is_consistent_subtype(ground_type_of(v), T)` with no fast-path bypass. A special-case before `is_consistent_subtype` would recreate a two-path structure — a smaller version of the exact problem Component 3 eliminates. If primitive checks prove slow in profiling, optimize `is_consistent_subtype` itself, which benefits every call site across the codebase.

`ground_type_of` must lazily extract fields for Record values — only compute field types when `is_subtype` descends into them, never by forcing thunks upfront.

### 5. Dynamic TypeAsserts and the RuntimeTypeCheck fallback

**Decision: delete `RuntimeTypeCheck` entirely.** Three apparent sources of untyped TypeAssert nodes were audited:

- **`$eval`** (`src/builtins_meta.rs`): Already handled — callers pass `program:` which carries the TypeAnnotationTable. The empty-table path is expected and correct for bare expressions without context.
- **Macro expansion** (`src/lower.rs`): Macro expansion already runs before typechecking (`src/main.rs` pipeline order). The actual gap is macros invoked dynamically at eval-time (e.g., from `$eval`). These produce TypeAssert nodes after the typecheck pass has run and will lack table entries. Fix: dynamic eval-time macro invocation must either run a typecheck pass on the expanded output, or the expansion must be restricted to not produce TypeAssert nodes without resolved types.
- **Pipeline `expects:` contracts** (`src/eval_pipeline.rs`): These schema checks currently use `RuntimeTypeCheck` AST nodes with an unresolved `Annotation`. The restructure keeps Guarded-thunk wrapping (so `%` still passes lazily — forcing it at the `---` boundary would violate the laziness invariant), but replaces the string-comparison validation with `value_matches_type`. The key challenge: `expects:` is stored as `SurfaceDocument.expects: Option<Spanned<Annotation>>` — `Annotation` is not a `SurfaceNode` and has no `NodeId`, so it cannot be keyed into `TypeAnnotationTable`. **Resolution**: add `resolved_type: Option<Type>` to `CoreExpr::RuntimeTypeCheck`. The typecheck pass already resolves `expects:` via `resolve_annotation` (`src/typecheck.rs:307-341`) — instead of discarding the result, store it:

  ```rust
  // In src/ast.rs — add resolved_type field:
  RuntimeTypeCheck {
      annotation: Spanned<Annotation>,
      expr: Arc<Spanned<CoreExpr>>,
      default: Option<Arc<Spanned<CoreExpr>>>,
      resolved_type: Option<Type>,  // populated by typecheck pass for expects: contracts
  }

  // In src/typecheck.rs — after resolve_annotation succeeds (line ~317):
  // Thread resolved expected_type to the CoreExpr::RuntimeTypeCheck node via a new
  // side table: state.expects_resolved: HashMap<DocumentId, Type>
  // Then in eval_pipeline.rs, look up the resolved type when building the node.

  // In src/eval_pipeline.rs — wrap_with_nominal_validation becomes:
  fn wrap_with_nominal_validation(
      inner: Arc<Thunk>,
      annotation: &Spanned<Annotation>,
      resolved_type: Option<Type>,      // pre-resolved by typecheck
      validation_span: Span,
      ctx: &Arc<EvalContext>,
  ) -> Arc<Thunk> {
      // Build RuntimeTypeCheck { resolved_type, ... }
      // At force time: if resolved_type is Some(ty), call value_matches_type(v, ty)
      //                if None (legacy/untyped path), fall back to structural field checking
  }
  ```

  No `TypeEnv` threading required — the resolved type is computed once during the typecheck pass and carried forward. After Component 3, when `RuntimeTypeCheck` is deleted, `expects:` contracts use `CoreExpr::TypeAssert { resolved_type }` directly.

With macro expansion reordered and pipeline contracts restructured, every reachable `TypeAssert` node is guaranteed a `TypeAnnotationTable` entry. `RuntimeTypeCheck` becomes dead code and is deleted. Any future TypeAssert node without a table entry is a bug, caught at evaluation time by a hard failure.

### 6. `Unknown` inside composite annotations

The four-source separation handles top-level `Unknown`, but `Unknown` can appear inside composite types from user annotations: `@[Seq Unknown]`, `@[Map String Unknown]`, `@[Fn@Unknown [Int]]`. Under AGT, these are valid gradual composite types where `?` fills specific positions.

**Already handled.** `is_consistent` (`src/type_def.rs:760`) recursively descends into `Seq`, `Map`, `Function`, `Record`, `Union`, and `Intersection`, checking both sides for `Unknown` at every level. Embedded `Unknown` at any depth triggers the consistency path. No new machinery required.

## Prerequisites

- **Dynamic eval-time macro TypeAssert resolution**: Macros invoked from `$eval` at runtime produce TypeAssert nodes after the typecheck pass. These must either run a typecheck pass on the expanded output or avoid producing TypeAssert nodes without resolved types. Prerequisite for deleting `RuntimeTypeCheck`.
- **Pipeline contract restructure**: Add `resolved_type: Option<Type>` to `CoreExpr::RuntimeTypeCheck`. Thread the typecheck-resolved `expects:` type forward via `state.expects_resolved` (new side table) → `eval_pipeline.rs` → `RuntimeTypeCheck::resolved_type`. At force time, use `value_matches_type` with the resolved type instead of string comparison. No TypeEnv threading required. Prerequisite for deleting `RuntimeTypeCheck`.
- **`lower.rs` Type::Error guard**: When `TypeAnnotationTable.get(&id) == Some(Type::Error)`, `lower.rs` must not emit `CoreExpr::TypeAssert { resolved_type: Type::Error }` — that always fails silently in release builds. During Components 1–2, emit `CoreExpr::RuntimeTypeCheck` instead. After Component 3 deletes `RuntimeTypeCheck`, the guard emits `CoreExpr::TypeAssert { resolved_type: Type::Unknown }` — consistent subtyping then accepts the value (gradual pass-through), and the real error surfaces as the `undefined_variable` error at the failed-binding use-site.
- **Overload resolution semantics**: Component 2 requires a design decision (§Open Questions §1) on how overloads interact with TypeVar arguments during HM inference.
- **`ground_type_of` specification**: Component 3 requires a precise specification of what `extract_row(v)` does for Dict values (key-only, no forced values) and what ground types are produced for each `Value` variant.

## References

- Garcia, R., Clark, A.M. & Tanter, É. (2016). "Abstracting Gradual Typing." *POPL '16*, pp. 429-442. — Proposition 22 (Type Safety): the runtime check is the restriction of the static consistent subtyping relation to ground types. Grounds Component 3 (`is_consistent_subtype`).
- Garcia, R. & Cimini, M. (2015). "Principal Type Schemes for Gradual Programs." *POPL '15*, pp. 303-315. — Proves `?` and type variables are fundamentally distinct; `?` is never resolved, TypeVars always are. Grounds the four-source `Unknown` separation.
- Milner, R. (1978). "A Theory of Type Polymorphism in Programming." *JCSS* 17(3). — Phase consistency: static and dynamic semantics must agree on well-typed programs.
- Wadler, P. & Findler, R.B. (2009). "Well-Typed Programs Can't Be Blamed." *ESOP '09*, pp. 1-16. — Blame theorem; grounds the `Type::Error` vs `Type::Unknown` distinction at type boundaries.
- Crary, K. & Weirich, S. (2000). "Intensional Polymorphism in Type-Erasure Semantics." *JFP* 10(4). — Type erasure as the gold standard for eliminating runtime type representation overhead.
