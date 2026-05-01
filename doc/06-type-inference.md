# Type Inference

This chapter formally specifies tinct's type inference algorithm. The notation uses standard PL conventions: Γ for type environments, ⊢ for typing judgments, S for substitutions, and τ, σ for types.

For the user-facing annotation syntax (`@`, type assertions, type expressions), see [Type Annotations](05-type-annotations.md). For TypeAssert runtime validation, row-variable unification, and the type system extension roadmap, see [Type System Extensions](07-type-extensions.md).

## Type Grammar

```
τ ::= IntLiteral(n)              literal integer type
    | StringLiteral(s)           literal string type
    | Int                        integer
    | Float                      float
    | Number                     numeric supertype of Int and Float
    | Str                        string  (internal name; user-facing annotations accept `String` as an alias)
    | Bool                       boolean
    | Fn(τ₁...τₙ → τᵣ)          function (n params, return type)
    | Seq(τ)                     lazy sequence
    | Record(f₁:τ₁...fₙ:τₙ, ρ)  record with row rest ρ
    | Proxy                      opaque proxy (field access dispatches to handler)
    | α                          type variable
    | Any                        dynamic/unknown type

ρ ::= Closed                     no additional fields (Empty)
    | RowVar(r)                  named row variable (see [Type System Extensions](07-type-extensions.md) §Row-Variable Unification)
                                 (anonymous `...` syntax in annotations generates fresh `_open{n}` names internally)
```

## Bidirectional Typing

Tinct uses bidirectional type checking (Pierce & Turner 2000; Dunfield & Krishnaswami 2021) to cleanly separate type inference from subtyping. Two modes:

- **Synthesis (⇒):** `Γ ⊢ e ⇒ τ` — infer the type of e bottom-up (what `infer_expr` does today).
- **Checking (⇐):** `Γ ⊢ e ⇐ τ` — verify e is compatible with expected type τ, using subsumption.

The **subsumption rule** bridges them:

```
Γ ⊢ e ⇒ σ,  σ <: τ
────────────────────────────────── [SUB]
Γ ⊢ e ⇐ τ
```

If an expression synthesizes type σ and σ is a subtype of the expected type τ, then checking succeeds. This is where singleton literal type promotion happens: `42 ⇒ IntLiteral(42)`, and `IntLiteral(42) <: Int`, so `42 ⇐ Int` succeeds. But `Int ≮: IntLiteral(42)`, so checking an `Int`-typed expression against `IntLiteral(42)` fails. Direction matters — subtyping is asymmetric by design.

Note: tinct's `IntLiteral(42)` and `StringLiteral("hello")` are **singleton literal types** — distinct types that are subtypes of their base types (`Int`, `Str`). These are not refinement types in the Dunfield & Pfenning (2004) sense (which use predicate logic, e.g., `{x: Int | x = 42}`). The singleton type approach is simpler and sufficient for tinct's needs; D&P's framework validates that subtyping with type refinements is sound in a bidirectional setting.

Implementation:

**Note:** The complete `check_expr` implementation handles lambda checking mode (propagating expected parameter types to unannotated lambda parameters when the expected type is fully concrete), annotated parameter type resolution, checking against expected return annotations, and subsumption via `[U-SUBSUME]`. See `src/typecheck.rs` for the full implementation (search for `fn check_expr`).

`check_expr` is used only at positions where the expected type is fully concrete (no type variables): CALL-MONO arguments, concrete return annotations (no type variables), and TypeAssert. For CALL-POLY arguments (where type variables need binding), unification with subsumptive fallback is used instead — see [U-SUBSUME] in the Unification section.

**Checking positions** (expected type fully concrete, uses `check_expr` with [SUB]):

| Position | Expected type | Mechanism |
|----------|--------------|-----------|
| Function arguments (CALL-MONO) | Parameter type | `check_expr` |
| Function body with concrete return annotation | Declared return type (no TypeVars) | `check_expr` |
| TypeAssert inner expression | Annotated type | `check_expr` |
| Lambda body (CHECK-FN mode) | Expected return type | `check_expr` |

**Unification positions** (type variables present, uses `unify` with [U-SUBSUME]):

| Position | Expected type | Mechanism |
|----------|--------------|-----------|
| Function arguments (CALL-POLY) | Instantiated param type (has type vars) | `unify` with [U-SUBSUME] fallback |
| `infer_fn` with TypeVar return annotation | Body type vs return annotation | infer body + `unify` |
| `check_expr` lambda with TypeVar param annotation | Declared param type | `unify` param into env |
| `check_expr` lambda with TypeVar return annotation | Synthesized body type vs return annotation | infer body + `unify` |

**Synthesis positions** (type flows up, no expected type):

| Position | Synthesizes |
|----------|-------------|
| Literals | `IntLiteral(n)`, `Float`, `Bool`, `StringLiteral(s)` |
| Variable references | Instantiated type scheme (VAR-POLY) |
| Dict values | Record type from letrec inference |
| Function definitions | `Fn(params → ret)` |
| Access chains (dot, bracket, range) | Field type or `Any` |

**Confluence.** CALL-POLY uses unification (not `check_expr`) for argument checking because type variables need binding. After substitution application resolves a type variable to a concrete type, subsequent unification attempts against that concrete type use [U-SUBSUME] — a bidirectional subsumption fallback that checks `is_subtype` in both directions. This ensures argument ordering does not affect whether type checking succeeds. See the Unification section for details.

## Inference Judgments: Γ ⊢ e ⇒ τ

**Literals:**

```
────────────────────────────────── [INT]
Γ ⊢ n : IntLiteral(n)

────────────────────────────────── [FLOAT]
Γ ⊢ f : Float

────────────────────────────────── [BOOL]
Γ ⊢ b : Bool

────────────────────────────────── [STR]
Γ ⊢ "s" : StringLiteral("s")
```

**Variable reference:**

```
Γ(x) = ∀α₁...αₙ. τ
τ' = instantiate_scheme(∀α₁...αₙ. τ, ℓ_current)
────────────────────────────────── [VAR-POLY]
Γ ⊢ $x : τ'
```

Each variable reference instantiates its type scheme with fresh variables at ℓ_current. When n = 0, this returns the body directly (monomorphic binding — no allocation).

**Dict (letrec with generalization):**

Dicts are inferred in five sequential passes using the [DICT-GEN] rule — see §Let-Generalization (Levels-Based) for the full specification. The rule uses fresh type variables (not `Any`) for forward references and generalizes entry types after inference.

**Function definition:**

```
For each param pᵢ:
    if variadic (...pᵢ): σᵢ = Any                   (see Limitation #4)
    else if annotated pᵢ@σᵢ: use σᵢ
    else: σᵢ = Any
Γ' = Γ, p₁:σ₁, ..., pₙ:σₙ
If return annotation @σᵣ given:
    if has_type_vars(σᵣ):
        Γ' ⊢ body ⇒ τ_body                           [synthesis mode]
        unify(τ_body, σᵣ, S)                          [unification mode]
    else:
        Γ' ⊢ body ⇐ σᵣ                               [checking mode]
    use σᵣ as return type.
Else:
    Γ' ⊢ body ⇒ τᵣ                                   [synthesis mode]
────────────────────────────────── [FN]
Γ ⊢ [fn@σᵣ [p₁@σ₁ ... pₙ@σₙ] body] : Fn(σ₁...σₙ → σᵣ)
```

**Lambda checking mode (bidirectional):**

```
Γ ⊢ [fn@σᵣ [p₁@τ₁ ... pₙ@τₙ] body] ⇐ Fn(σ₁...σₙ → σ_exp)
    where ¬has_type_vars(Fn(σ₁...σₙ → σ_exp))       (expected type fully concrete)
For each param pᵢ:
    if variadic: use Any
    else if annotated pᵢ@τᵢ:
        if has_type_vars(τᵢ): unify(σᵢ, τᵢ, S)       (annotation with TypeVars)
        else: check σᵢ <: τᵢ                         (contravariant check)
        use τᵢ
    else: use σᵢ                                     (propagate expected type)
Γ' = Γ, p₁:τ₁, ..., pₙ:τₙ
If return annotation @σᵣ given:
    if has_type_vars(σᵣ): unify(σᵣ, σ_exp, S)       (annotation with TypeVars)
    else: check σᵣ <: σ_exp                          (covariant check)
    Γ' ⊢ body ⇐ σᵣ
Else:
    Γ' ⊢ body ⇐ σ_exp                                (check against expected return)
────────────────────────────────── [CHECK-FN]
```

**Substitution note:** σ_exp is substitution-applied (S(σ_exp)) before comparison via `state.subst.apply`, ensuring that any type variables bound during parameter checking are resolved before the subsumption check. σᵣ (the declared return annotation) is guaranteed to be concrete — no TypeVars — at the `else: check σᵣ <: σ_exp` sub-branch (the concrete-return-annotation path), because the `!declared.has_inference_vars()` guard has already fired. Applying the current substitution to σᵣ would therefore be a no-op; the code omits it correctly.

Unannotated non-variadic params get type Any. This is the source of the "Any escape hatch" — without annotations, functions have monomorphic type Fn(Any...Any → τᵣ). Polymorphism requires explicit type variable annotations (e.g., `x@a`).

When a return annotation is present, the dispatch depends on whether σᵣ contains type variables. If σᵣ is fully concrete (no type variables), the body is **checked** against it (⇐ mode): the body is synthesized, then subsumption verifies the inferred type is a subtype of the declared type. If σᵣ contains type variables (e.g., `fn@a`), the body is **synthesized** and then **unified** with σᵣ. This is necessary because type variables are not ground — `is_subtype` treats them as opaque and only matches reflexively, so `is_subtype(IntLiteral(42), TypeVar("_t5"))` would incorrectly reject valid code. Unification mode binds the type variables via constraint solving (Damas & Milner, 1982), which is the correct mechanism for annotations that introduce polymorphism.

**Function call (bidirectional):**

Three rules depending on the function type. Arity is always checked.

```
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_type_vars(Fn(...)) = false
Γ ⊢ aᵢ ⇐ σᵢ  for i = 1..n                         [checking mode]
|args| = |params|
────────────────────────────────── [CALL-MONO]
Γ ⊢ [call f a₁...aₙ] ⇒ σᵣ
```

Monomorphic path with checking: each argument is **checked** against its parameter type using subsumption. `[call $add "hello"]` where `$add : Fn(Int Int → Int)` produces a type error because `String ≮: Int`. The `check_expr` call synthesizes the argument type and applies `[SUB]`.

```
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_type_vars(Fn(...)) = true
Γ ⊢ aᵢ ⇒ τᵢ  for i = 1..n                          [synthesis]
|args| = |params|
(σ'₁...σ'ₙ → σ'ᵣ) = instantiate(σ₁...σₙ → σᵣ)
S = unify(σ'₁ ≐ τ₁, ..., σ'ₙ ≐ τₙ)                  [with U-SUBSUME]
────────────────────────────────── [CALL-POLY]
Γ ⊢ [call f a₁...aₙ] ⇒ S(σ'ᵣ)
```

Polymorphic path with unification: arguments are **synthesized** (not checked), then unified against instantiated parameter types. Unification binds type variables via [U-VAR] and handles concrete-type comparisons via [U-SUBSUME] (bidirectional subsumption fallback). This is critical for confluence: when multiple arguments constrain the same type variable with different precision (e.g., `IntLiteral(42)` and `Int`), the subsumptive fallback ensures type checking succeeds regardless of argument order. See the Unification section for [U-SUBSUME] details.

Note: CALL-POLY does NOT use `check_expr` because type variables require binding via unification. `check_expr` is reserved for fully concrete expected types (CALL-MONO, TypeAssert, return annotations).

**CALL-MONO/CALL-POLY literal type divergence.** CALL-POLY is more permissive than CALL-MONO for most literal type pairs. The divergence arises because `unify()` has bidirectional literal promotion rules (5 type pairs × 2 directions = 10 match alternatives in `src/types.rs`), while `check_expr` uses directional `is_subtype(actual, expected)`. Concrete-type pair behavior across both paths (rows marked **fails** reject under both CALL-MONO and CALL-POLY; the `IntLiteral`/`Float` pair is documented here because [U-SUBSUME] correctly rejects it after removal of the former unsound promotion arm):

| Argument type | Parameter type | `is_subtype` (CALL-MONO) | `unify` (CALL-POLY) |
|---------------|---------------|--------------------------|---------------------|
| `Int` | `IntLiteral(n)` | false (Int is wider) | succeeds (explicit arm: IntLiteral ↔ Int) |
| `Number` | `IntLiteral(n)` | false | succeeds (explicit arm: IntLiteral ↔ Number) |
| `Number` | `Int` | false | succeeds (explicit arm: Int ↔ Number) |
| `Number` | `Float` | false | succeeds (explicit arm: Float ↔ Number) |
| `Str` | `StringLiteral(s)` | false | succeeds (explicit arm: StringLiteral ↔ Str) |
| `IntLiteral(n)` | `Float` | false (no subtype relation) | **fails** (no subtype relation in either direction) |
| `Float` | `IntLiteral(n)` | false | **fails** (no subtype relation in either direction) |

In practice, this divergence rarely surfaces because CALL-MONO only fires for monomorphic function types (no type variables), and monomorphic parameter types like `IntLiteral(n)` are uncommon — they arise only from singleton literal type annotations, not from normal inference. The divergence is harmless for correctness today because it only makes CALL-POLY more lenient, never more restrictive. The [U-SUBSUME] fallback in `unify()` checks `is_subtype` in both directions for concrete type pairs, producing the same result as the explicit promotion arms for all valid subtype relationships. The `IntLiteral`/`Float` pair correctly fails under [U-SUBSUME] because they are in different branches of the numeric lattice (`IntLiteral <: Int <: Number` and `Float <: Number`, but no `IntLiteral <: Float` rule exists). Full divergence elimination (making CALL-MONO and CALL-POLY agree on all cases) requires directional [U-SUBSUME] — threading actual/expected roles through unification (Pierce & Turner 2000, local type inference), which is a more substantial change.

```
Γ ⊢ f ⇒ Any
────────────────────────────────── [CALL-ANY]
Γ ⊢ [call f a₁...aₙ] ⇒ Any
```

Calling a value typed as Any returns Any. Arguments are still synthesized (for type map population and nested error detection) but not checked against parameter types.

Named arguments are synthesized but not checked against parameter types — `Type::Function` has only a `Vec<Type>` for parameters with no name field. Named argument checking against parameter types is unimplemented; see [Type System Extensions](07-type-extensions.md) §Completeness for the planned extension.

**Access chains:**

```
Γ ⊢ e : Record(... k:τ ..., ρ)
────────────────────────────────── [DOT]
Γ ⊢ e.k : τ

Γ ⊢ e ⇒ α,  β fresh,  ρ fresh
S = unify(α ≐ Record({k: β}, RowVar(ρ)))
────────────────────────────────── [DOT-VAR]
Γ ⊢ e.k ⇒ β
```

After unification, α is bound in S — references to α in the conclusion denote its resolved image S(α), not the original variable. The occurs check and level lowering for α, β, and ρ are handled internally by `unify()`.

```
Γ ⊢ e : Record(F, RowVar(ρ)),  k ∉ F,  β fresh,  ρ' fresh
Precondition: ρ ∉ FRV(Row({k: β}, RowVar(ρ')))    (occurs check)
Side-effect: ∀v ∈ FV(Row({k: β}, RowVar(ρ'))). level(v) ← min(level(v), level(ρ))
S[ρ ↦ Row({k: β}, RowVar(ρ'))]
────────────────────────────────── [DOT-ROWVAR]
Γ ⊢ e.k ⇒ β
```

**[DOT-VAR] vs [DOT-ROWVAR] asymmetry.** [DOT-VAR] delegates to `unify()`, which handles the occurs check and level lowering internally as part of Robinson unification. [DOT-ROWVAR] performs explicit row-variable binding with its own occurs check (`row_var_occurs_pub`) and level lowering (`lower_row_var_levels_pub`), because the binding is inserted directly into `state.subst.row_map` without going through `unify()`. Both paths maintain the same invariants (no infinite types, level monotonicity) but through different mechanisms.

```
Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : StringLiteral(k),  F(k) = τ
────────────────────────────────── [BRACKET-LIT]
Γ ⊢ e[key] : τ

Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : Str | Int | Any
────────────────────────────────── [BRACKET-DYN]
Γ ⊢ e[key] : Any

Γ ⊢ e : Record(F, ρ),  bounds : Int | Str | Any
────────────────────────────────── [RANGE]
Γ ⊢ e[start..end] : Record(F, ρ)

Γ ⊢ e : Any
────────────────────────────────── [ACCESS-ANY]
Γ ⊢ e.k : Any,  Γ ⊢ e[key] : Any,  Γ ⊢ e[start..end] : Any
```

**Type assertion (checking mode):**

```
resolve(ann) = σ,  Γ ⊢ e ⇐ σ                       [checking mode]
────────────────────────────────── [ASSERT]
Γ ⊢ [@σ e] ⇒ σ

resolve(ann) = σ,  Γ ⊢ e ⇐ σ fails,  default ∈ ann
────────────────────────────────── [ASSERT-DEFAULT]
Γ ⊢ [@[type: σ  default: d] e] ⇒ σ
```

Type assertions use checking mode: the inner expression is checked against the annotated type via [SUB]. When checking fails and a `default:` property is present, the assertion succeeds silently (no type error). The default value provides a fallback at runtime.

**Limitation:** When the annotation resolves to a bare type variable (e.g., `[@a $x]`), the static subtype check always fails because `is_subtype` only matches type variables reflexively. Such assertions require a `default:` clause or will produce a type error. To narrow to a polymorphic type, use unification-based checking within a function parameter or return annotation context.

**Type alias:**

```
resolve(inner) = τ,  register alias in Γ
────────────────────────────────── [ALIAS]
Γ ⊢ [type inner] : Any
```

**Annotated expression:**

```
resolve(ann) = τ
────────────────────────────────── [ANNOTATED]
Γ ⊢ name@ann : τ
```

When name = "Fn": interpret as function type constructor.

**Seq types:** `Seq(τ)` exists in the type grammar and is handled by unification and subtyping. Sequence constructors (`$seq`, `$range`, etc.) infer as `Seq(τ)` — see [Type System Extensions](07-type-extensions.md) §Precision.

## Unification: unify(τ₁, τ₂, S) → S'

Unification finds a most general substitution S such that S(τ₁) = S(τ₂). Before matching, both types are normalized via S (substitution applied). Unification follows **Robinson (1965)** for structural decomposition and variable binding, extended with pragmatic promotion rules (see bidirectional literal-to-parent promotions in the implementation below). Subtyping is handled by `check_expr` via the `[SUB]` rule and `is_subtype` for directional checks, and by `[U-SUBSUME]` for bidirectional compatibility within unification. This separation follows Pierce & Turner (2000) and Dunfield & Krishnaswami (2021).

```
unify(τ, τ, S) = S                              [U-REFL]
unify(Any, τ, S) = S                             [U-ANY-L]
unify(τ, Any, S) = S                             [U-ANY-R]
unify(α, τ, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-L]
unify(τ, α, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-R]
```

**Note:** When one side is a type variable α and the other is `Any`, the implementation fires specialized TypeVar rules ([U-ANY-VAR]/[U-VAR-ANY], see §Let-Generalization) first, which zero ℓ(α) to prevent unsound generalization of Any-unified variables. The general [U-ANY-L]/[U-ANY-R] rules above apply only when neither side is a TypeVar.

Literal identity (same literal value = same type):

```
unify(IntLiteral(m), IntLiteral(n), S) =
    S           if m = n                         [U-INTLIT-EQ]
    error       if m ≠ n                         [U-INTLIT-NEQ]

unify(StringLiteral(s), StringLiteral(t), S) =
    S           if s = t                         [U-STRLIT-EQ]
    error       if s ≠ t                         [U-STRLIT-NEQ]
```

Bidirectional literal-to-parent promotions are implemented as explicit match arms in `unify()` (e.g., `IntLiteral(_)` with `Int`, `Float` with `Number`). These are fast-path optimizations for common subtype pairs that avoid the `has_inference_vars` guard and `is_subtype` call in [U-SUBSUME]. The [U-SUBSUME] rule below provides the general fallback for any concrete type pair.

Structural:

```
unify(Fn(p₁...pₙ → r₁), Fn(q₁...qₙ → r₂), S) =
    let S' = unify(p₁,q₁, ... pₙ,qₙ, S)
    unify(r₁, r₂, S')                           [U-FN]
    error if |p| ≠ |q|

unify(Seq(τ₁), Seq(τ₂), S) = unify(τ₁, τ₂, S)  [U-SEQ]

unify(Record(r₁), Record(r₂), S) = unify_rows(r₁, r₂, S)     [U-REC]
```

Record unification delegates entirely to row unification — see [Type System Extensions](07-type-extensions.md) §Row-Variable Unification for the full `unify_rows` algorithm.

Subsumptive fallback for concrete types (no type variables on either side):

```
unify(σ, τ, S) where ¬has_type_vars(σ) ∧ ¬has_type_vars(τ):
    if is_subtype(σ, τ) ∨ is_subtype(τ, σ): S   [U-SUBSUME]
    else: error                                  [U-FAIL]
```

[U-SUBSUME] is the bridge between unification and subtyping. It fires after all other rules (structural decomposition, type variable binding) have been tried — it is ordered last as a fallback, not a catch-all. Structural rules ([U-FN], [U-SEQ], [U-REC], literal identity) take priority over subsumptive matching. When two concrete types remain and they are in a subtype relationship in either direction, unification succeeds without modifying the substitution. This is essential for **confluence in CALL-POLY**: when a type variable α is bound to `IntLiteral(42)` by one argument and later compared against `Int` by another (via substitution resolution), [U-SUBSUME] recognizes `IntLiteral(42) <: Int` and succeeds regardless of argument order.

**Relationship to Robinson unification.** Robinson (1965) is purely syntactic — it has no notion of subtyping, so `unify(IntLiteral(42), Int)` would simply fail (different constructors). [U-SUBSUME] extends Robinson with a ground-type compatibility check: when both sides are concrete and in a subtype relationship, unification succeeds without modifying the substitution. This is a pragmatic middle ground — Robinson handles structural decomposition and variable binding; [U-SUBSUME] handles the subtype lattice at ground types. The substitution is not modified by [U-SUBSUME], so existing variable bindings (which may carry literal precision) are preserved. This is the same approach Rust's type inference uses: subtyping constraints between concrete types are resolved as compatibility checks rather than LUB computation (Dolan & Mycroft 2017 describe the full alternative — algebraic subtyping — which tinct intentionally does not adopt; see `doc/whatif/algebraic-subtypes.md`).

[U-SUBSUME] checks both directions because unification is symmetric — the two types arrive without a designated "actual" vs "expected" role. The bidirectional check covers both orderings: `unify(IntLiteral(42), Int)` succeeds (IntLiteral(42) <: Int) and `unify(Int, IntLiteral(42))` also succeeds (IntLiteral(42) <: Int, checked as `is_subtype(τ, σ)`). The substitution is unchanged because there are no type variables to bind.

**Interaction with [SUB]:** At CALL-MONO sites (fully concrete types, no unification needed), `check_expr` uses directional subsumption via `is_subtype(actual, expected)` — only the correct direction is checked. [U-SUBSUME] is bidirectional because it operates within unification where the original directionality is lost after structural decomposition. This is sound because the substitution is not modified — the bidirectional check only determines compatibility, not binding direction.

**Dual-path promotion design.** `unify()` also implements bidirectional promotion arms for 5 type pairs (`IntLiteral` with `Int` and `Number`; `Int` with `Number`; `Float` with `Number`; `StringLiteral` with `Str`) plus literal identity checks, directly as Robinson unification match cases. These are symmetric (either direction succeeds) and fire before [U-SUBSUME]. The dual-path design is intentional: promotions in `unify()` handle CALL-POLY argument matching where type variables have been resolved to concrete types by substitution, while [U-SUBSUME] via `is_subtype` handles the general fallback for concrete type pairs not covered by explicit arms. Both paths produce the same result for overlapping cases — neither modifies the substitution — but the explicit arms avoid the `has_inference_vars` guard and `is_subtype` call overhead for the most common promotion patterns. Note: `IntLiteral` with `Float` is intentionally NOT a promotion arm because `IntLiteral` is not a subtype of `Float` — they are in different branches of the numeric lattice (`IntLiteral <: Int <: Number` and `Float <: Number`).

All other non-structural, non-subsumable combinations: error [U-FAIL]

**Interaction with CALL-POLY:** Polymorphic call checking synthesizes all argument types, then unifies each against the corresponding instantiated parameter type. Type variable binding comes from [U-VAR]; concrete type compatibility (after substitution resolves variables) comes from [U-SUBSUME]. The bidirectional subsumption in [U-SUBSUME] ensures confluence — argument order does not affect whether type checking succeeds, only the precision of the resulting binding.

## Subtyping: τ <: σ

Subtyping is a pure predicate (no substitution mutation). Used for TypeAssert validation and return type checking.

```
τ <: Any                                         [S-ANY-TOP]
Any <: τ                                         [S-ANY-BOT]
τ <: τ                                           [S-REFL]
IntLiteral(n) <: Int <: Number                   [S-INT]
StringLiteral(s) <: Str                          [S-STR]
Float <: Number                                  [S-FLOAT]
Seq(τ) <: Seq(σ)  if τ <: σ                      [S-SEQ]

Record(F₁,ρ₁) <: Record(F₂,ρ₂) if:
    ∀(k:σ) ∈ F₂, ∃(k:τ) ∈ F₁ with τ <: σ       (width+depth)
    If ρ₂ = Empty:
        If ρ₁ = Empty: keys(F₁) ⊆ keys(F₂)
            (with width condition above this enforces keys(F₁) = keys(F₂))
        If ρ₁ = RowVar: false
            (Rémy 1994 — a RowVar tail may be instantiated with additional
            fields that the closed supertype rejects. This is the sound
            pre-unification behavior; post-unification, the RowVar is bound
            to Empty by unify() and the (Empty, Empty) arm applies. See
            test_is_subtype_consistency_open_sub_closed_sup_exact_known_fields.)
    If ρ₂ = RowVar: always ok                     [S-REC]

Fn(p₁...pₙ→r₁) <: Fn(q₁...qₙ→r₂) if:
    |p| = |q|
    qᵢ <: pᵢ  for all i                          (contravariant params)
    r₁ <: r₂                                      (covariant return)
                                                 [S-FN]
```

**Note on [S-ANY-TOP] and [S-ANY-BOT]:** Having Any as both the top and bottom of the type lattice violates antisymmetry (τ <: σ ∧ σ <: τ ⇒ τ = σ) and makes the subtype relation unsound as a partial order. This is intentional for tinct's gradual type system — Any marks the boundary between typed and untyped code (see Limitation #2).

## Instantiation

```
instantiate(τ) = (S(τ), S)
    where S has two kinded maps:
      S.type_map = {α₁ ↦ _t0, α₂ ↦ _t1, ...}  (type vars → Type)
      S.row_map  = {ρ₁ ↦ Row{...}, ...}       (row vars → Row)
    for each αᵢ ∈ FTV(τ), fresh type var names _tN generated
    for each ρᵢ ∈ FRV(τ), fresh row var names _tM generated
    from a shared monotonic per-file counter.

FTV(τ) collects type variables via collect_type_vars().
FRV(τ) collects row variables via collect_row_vars().
Both can be collected in a single pass via collect_all_vars().

Kinded substitution (Rémy 1994): type variables and row variables
inhabit distinct kinds, enforced structurally by separate maps.
type_map binds type variable names to Type; row_map binds row
variable names to Row. A name cannot appear in both maps in
well-formed substitutions. TypeScheme carries separate quantifier
lists (type_vars: Vec<String>, row_vars: Vec<String>), and
instantiate_scheme() routes each through its corresponding map.
```

This is alpha-renaming for call-site freshening. Each polymorphic call site gets independent type variables so unification at one site does not constrain another. With let-generalization (below), instantiation also handles let-bound polymorphic type schemes.

## Let-Generalization (Levels-Based)

Tinct uses levels-based let-generalization following Kiselyov (2013) to support polymorphic let-bindings. This extends annotation-driven polymorphism with automatic generalization at dict entry boundaries.

**Type schemes.** The type environment Γ maps names to *type schemes* σ rather than bare types τ:

```
σ ::= ∀(α₁...αₙ, ρ₁...ρₘ). τ    (n,m ≥ 0; when both zero, equivalent to monomorphic τ)
```

Implementation: `TypeEnv.bindings` changes from `HashMap<String, Type>` to `HashMap<String, TypeScheme>`. The `TypeScheme` struct:

```rust
#[derive(Debug, Clone)]
pub struct TypeScheme {
    pub type_vars: Vec<String>,  // quantified type variable names
    pub row_vars: Vec<String>,   // quantified row variable names
    pub body: Type,
}

impl TypeScheme {
    pub fn mono(ty: Type) -> Self {
        Self {
            type_vars: vec![],
            row_vars: vec![],
            body: ty,
        }
    }
}
```

`PartialEq` for `TypeScheme` compares structurally (type_vars + row_vars + body). `Display` shows `∀a b. Fn(a → b)` for polymorphic schemes, or the bare type for monomorphic ones. Located in `types.rs`.

**Levels.** Every type variable α carries an integer level ℓ(α). The type checker maintains a current level counter ℓ_current, incremented at each dict boundary (every `infer_dict` call):

- Fresh type variables are created at ℓ_current
- `Type::TypeVar(String)` becomes `Type::TypeVar(String, u32)` (name + level)
- `PartialEq` for `Type` is implemented manually: `TypeVar(a, _) == TypeVar(b, _)` compares names only, ignoring levels. This preserves the [U-REFL] fast path in `unify()`.
- `RowTail::RowVar(String)` becomes `RowTail::RowVar(String, u32)` — row variables carry levels and participate in generalization identically to type variables.
- `Display` for `TypeVar` and `RowVar` hides the level (internal inference state, not user-facing).

**Level storage and mutation.** Levels must be mutable during unification (Kiselyov's level lowering). Since `Type` is a value type, levels are stored in a separate mutable map alongside the substitution:

```rust
pub struct InferState {
    pub name_counter: u32,   // monotonic fresh variable name counter
    pub level: u32,          // current binding depth
    pub levels: HashMap<String, u32>,  // var name → current level
    pub subst: Substitution, // global constraint accumulator for access-chain bindings
}
```

`InferState.subst` accumulates row-variable constraints from [DOT-VAR] and [DOT-ROWVAR] across the entire inference pass. During letrec inference (Pass 3b), accumulated constraints are merged into the letrec substitution: when both maps bind the same variable, the two bindings are **unified** (Algorithm W substitution composition, Damas & Milner 1982) rather than silently dropped. Colliding bindings are **unified** (unify) rather than dropped, maintaining substitution composition soundness.

When a `TypeVar(name, lvl)` is created, `levels[name] = lvl` is recorded. During unification, level lowering mutates `levels[name]` without rebuilding the `Type`. `generalize()` consults `levels` for the authoritative level of each variable. The level embedded in `TypeVar(String, u32)` is the *creation-time* level; `InferState.levels` is the *current* (possibly lowered) level.

**Level adjustment during unification (symmetric).** Both branches of type variable unification perform level lowering:

```
unify(α, τ, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ) ∪ FRV(τ)                [U-VAR-LEVEL]

unify(τ, α, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ) ∪ FRV(τ)                [U-VAR-LEVEL-SYM]
```

Both rules lower levels symmetrically: when binding α to τ, every type variable β and every row variable ρ inside τ has its level lowered to `min(ℓ(β or ρ), ℓ(α))`. This prevents variables from escaping their scope through either side of a unification. Row variables must be lowered because τ may contain row variables through Record nesting (e.g., τ = Record({x: Int, ...ρ})).

**Any-unification and generalization.** When a type variable α is unified with `Any`, the current [U-ANY] rules succeed without binding α. To prevent incorrect generalization of the unbound α, `unify(α, Any)` sets `ℓ(α) = 0` (below all binding levels):

```
unify(α, Any, S) = S,  set ℓ(α) = 0               [U-ANY-VAR]
unify(Any, α, S) = S,  set ℓ(α) = 0               [U-VAR-ANY]
unify(Any, τ, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ) ∪ FRV(τ)                    [U-ANY-COMPLEX]
unify(τ, Any, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ) ∪ FRV(τ)                    [U-COMPLEX-ANY]
```

This ensures Any-touched variables are never generalized (since `ℓ(β) = 0` is never `> ℓ` for any binding level). The [U-ANY-VAR] and [U-VAR-ANY] rules are special cases of the complex rules where FTV(α) = {α}. The [U-ANY-COMPLEX] and [U-COMPLEX-ANY] rules handle cases like `unify(Any, Fn(β → Int))` where β must also be zeroed to prevent over-generalization.

**Generalization.** At a dict boundary at level ℓ, after all entries in the letrec group are inferred:

```
generalize(ℓ, τ) = ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ     [GEN]
```

where ℓ(α) is read from `InferState.levels[α]` (the current, possibly lowered level). Type variables whose level exceeds the enclosing scope's level are local to the binding and can be universally quantified. Variables at or below level ℓ are free in the enclosing scope and must remain monomorphic. Row variables participate identically — `RowVar(r, _)` with `levels[r] > ℓ` is generalized.

**Note on FTV and FRV:** FTV(τ) collects type variables only (TypeVar nodes). FRV(τ) collects row variables only (RowVar nodes in RowTail positions). The two sets are disjoint by construction — a variable name is either a type variable or a row variable, never both. This disjointness holds for fresh variables generated by `InferState.fresh_type_var()` and `fresh_row_var_name()`; names introduced via user annotations (`ann_mapping`) may violate this invariant (known open bug, see STATUS.md). The generalization formula above applies to type variables; row variables are generalized via an analogous rule `∀{ρ | ρ ∈ FRV(τ), ℓ(ρ) > ℓ}`, and both sets of quantified variables are stored in the TypeScheme.

Implementation signature:

```rust
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme
```

Collects type variables and row variables from ty via level-aware traversals (collect_type_vars and collect_row_vars, or combined via collect_all_vars), filters by `current_level > level`, returns `TypeScheme { type_vars, row_vars, body: ty.clone() }`.

**[VAR-POLY] rule:** See §Inference Judgments: Γ ⊢ e ⇒ τ above. Variable references instantiate the type scheme stored in Γ at ℓ_current.

Implementation signature:

```rust
pub fn instantiate_scheme(
    scheme: &TypeScheme,
    level: u32,
    state: &mut InferState,
) -> Type
```

Creates fresh `TypeVar(_tN, level)` for each quantified variable, registers them in `state.levels`, applies the renaming substitution to the scheme body.

**Modified dict inference (letrec with generalization):**

```
Pass 0 — Key resolution: unchanged
Pass 1 — Bind all: Γ' = Γ, k₁:α₁, ..., kₙ:αₙ
         where each αᵢ is a fresh type variable at level ℓ+1.
         Forward references see αᵢ (participates in unification,
         unlike the previous Any which silently matched everything).
Pass 2 — Type aliases: unchanged. Aliases remain monomorphic
         (IndexMap<String, Type>, not TypeScheme).
Pass 3 — Infer values: at level ℓ+1, for each non-alias
         entry kᵢ, infer Γ' ⊢ eᵢ : τᵢ, then unify(αᵢ, τᵢ).
         Apply resulting substitution S.
         
         Implementation note: Pass 3 splits into sub-passes 3a/3b/3c
         to handle the two-substitution model (local + state.subst).
         3a: clone state.subst → local subst; 3: unify into local;
         3b: merge state.subst updates → local; 3c: apply merged → fields.
         
Pass 4 — Generalize (NEW): for each entry kᵢ,
         σᵢ = generalize(ℓ, S(αᵢ), state)
         Update Γ'(kᵢ) = σᵢ
Build Record(k₁:S(α₁)...kₙ:S(αₙ), Closed).
────────────────────────────────── [DICT-GEN]
```

Non-Dict Record expressions at document boundaries follow the same level-increment + generalize protocol as [DICT-GEN]:

```
Γ, ℓ ⊢ save ℓ_enc = ℓ; increment ℓ to ℓ+1
Γ, ℓ+1 ⊢ e : Record(k₁:τ₁ … kₙ:τₙ, tail)
         restore ℓ to ℓ_enc
         for each kᵢ: σᵢ = generalize(ℓ_enc, τᵢ, state)
         Γ' = Γ[k₁ ↦ σ₁, …, kₙ ↦ σₙ]
──────────────────────────────────────────── [NON-DICT-GEN]
```

This rule applies when `typecheck_document` processes a non-Dict expression (e.g., a `[call ...]`) that produces a `Record` type. The level increment ensures type variables introduced during inference are at ℓ+1 and therefore generalizable at ℓ_enc. The restore before generalization guarantees ℓ_current is in its correct state for subsequent expressions.

The Record type uses monomorphic (substitution-applied) types for the type map and downstream structural checks. The type schemes σᵢ live in Γ and are instantiated at each reference via [VAR-POLY].

**Level increments at document boundaries.** Each `infer_dict` call increments ℓ_current, and `typecheck_document` also increments ℓ_current before inferring any non-Dict expression at a document boundary (both last and non-last positions). For `[a: [b: 42]]`, the outer dict runs at ℓ+1 and the inner dict at ℓ+2. For a non-Dict Record expression at a document boundary, ℓ_current is incremented to ℓ+1 before inference and restored to ℓ afterward, following the same protocol as `infer_dict`. This matches standard HM let-nesting: each binding scope increments the level.

**Forward references within letrec.** Within a single dict (letrec group), all entries share level ℓ+1 during Pass 3 inference. Forward references see the monomorphic αᵢ from Pass 1 — these are fresh type variables that participate in unification, producing binding constraints. This is more precise than the previous behavior (binding to `Any`): forward references now produce real type constraints rather than silently succeeding. After Pass 4, downstream consumers of the dict see polymorphic schemes.

Mutually recursive entries constrain each other through unification during Pass 3. This is standard monomorphic letrec (OCaml, Haskell `let rec`) — entries see each other as monomorphic during inference, not polymorphic. Polymorphic recursion (Mycroft 1984) is not supported: it would require fixpoint iteration to convergence, which is more complex and can diverge. The monomorphic restriction is sufficient for tinct's use cases.

**Document-level scheme threading.** `typecheck_document` splats dict Record fields into the parent environment for downstream document expressions. To preserve polymorphism across `---` boundaries, the splat must carry type schemes alongside the Record type. Implementation: `infer_dict` returns both the `Record` type (for structural checks) and a `HashMap<String, TypeScheme>` (for environment threading). `typecheck_document` inserts the schemes into the parent `TypeEnv`.

**Interaction with `Any` and unannotated parameters:**

- Unannotated function parameters still receive type `Any` (not a fresh type variable). `[fn [x] $x]` remains `Fn(Any → Any)`.
- `Any` in unification acts as a universal match ([U-ANY-L], [U-ANY-R]) but sets ℓ(α) = 0 for any type variable α it touches ([U-ANY-VAR], [U-VAR-ANY]), preventing generalization.
- Annotated type variables (e.g., `x@a`) create fresh type variables at ℓ_current. These participate in generalization normally.
- The practical effect: let-generalization benefits code that uses type annotations. `[id: [fn [x@a] $x]]` generalizes `id` to `∀a. Fn(a → a)`; subsequent `[call $id 42]` and `[call $id "hello"]` each get independent instantiations.

**Interaction with CALL-POLY.** When a call expression targets a `VarRef`, the inference engine inspects the scheme directly before any instantiation. This determines the routing:

- **Polymorphic scheme** (has quantified `type_vars` or `row_vars`): routes to `check_call_with_scheme`, which calls `instantiate_scheme` once to produce a function type with fresh `_tN` variables at ℓ_current. It then checks `has_inference_vars()` on the *post-instantiation* type: if all variables were resolved (fully concrete), it takes the CALL-MONO path (bidirectional checking via `check_expr`); if type variables remain, it takes the CALL-POLY path (synthesize arguments, unify, apply substitution to return type). This avoids double instantiation — without this optimization, VAR-POLY would instantiate the scheme at the reference site, producing `_tN` variables, and then CALL-POLY's `instantiate_at_level` would freshen those into yet more `_tM` variables.
- **Monomorphic scheme** (no quantified vars): routes to `check_call`, which infers the function expression normally. Since the scheme has no quantified variables, no instantiation occurs. The inferred type is typically concrete, so the CALL-MONO path fires directly.
- **Non-VarRef function expressions** (e.g., inline lambdas): always route to `check_call`.

**Substitution name uniqueness.** `Substitution::type_map` and `Substitution::row_map` are keyed by variable name, routing type and row variable bindings to their respective maps. User-annotated type variables (e.g., `@a`) are mapped to fresh `_tN` names by `resolve_type_name` during Pass 3 inference. Each function entry maintains its own `ann_mapping` (a per-function `HashMap<String, String>`), so `@a` in one function maps to a different `_tN` than `@a` in a sibling function. Within a single function, all references to the same annotation name `@a` resolve to the same `_tN` variable (ensuring constraints are shared as intended). After Pass 4 generalization produces `TypeScheme`s, `instantiate_scheme()` renames the quantified variables to fresh `_tM` names at each call site, preventing cross-call-site interference.

**Error recovery.** If Pass 3 inference fails for an entry, `Type::Any` is inserted for that entry (matching current behavior). Level lowering from partial unification before the failure is retained in `InferState.levels` — this is conservative (may prevent generalization of some variables) but safe. Generalization in Pass 4 proceeds for successfully-inferred entries; failed entries get `TypeScheme::mono(Type::Any)`.

**Key invariants:**

1. **Level monotonicity:** ℓ_current only increases when entering binding scopes. Fresh variables are always created at ℓ_current.
2. **Generalization soundness:** Only variables with ℓ(α) > ℓ_enclosing are generalized, ensuring no variable escapes its scope. Level lowering during unification ([U-VAR-LEVEL], symmetric) prevents variables from being captured at too high a level. Any-touched variables have ℓ = 0, preventing generalization.
3. **Value restriction (not needed):** Tinct does not have mutable references, so the value restriction (Wright, 1995) is unnecessary. All bindings can be generalized safely.
4. **Occurs check:** Unchanged — prevents infinite types regardless of levels.
5. **Substitution idempotence:** Unchanged — transitive chasing is orthogonal to levels.
6. **Letrec monomorphism during inference:** Within a letrec group, entries see each other as monomorphic during Pass 3 (fresh type variables, not schemes). Polymorphism only becomes visible after Pass 4 generalization.
7. **PartialEq level-blindness:** `TypeVar` equality ignores levels, preserving [U-REFL] semantics. Levels are consulted only during generalization (via `InferState.levels`).

**Key implementation types:**

| Component | Specification |
|-----------|--------------|
| `Type::TypeVar` | `TypeVar(String, u32)` — manual `PartialEq` (name only, level ignored for equality) |
| `RowTail::RowVar` | `RowVar(String, u32)` — levels for row generalization |
| `TypeEnv.bindings` | `HashMap<String, TypeScheme>` |
| `TypeEnv.type_aliases` | `HashMap<String, Type>` — aliases stay monomorphic |
| `TypeEnv::get()` | Returns `&TypeScheme` |
| `TypeEnv::insert_scheme()` | `fn(name, TypeScheme)` |
| `infer_expr` VAR case | `instantiate_scheme(env.get(name)?, ...)` |
| `infer_dict` | 5 passes (0-4), bind to fresh αᵢ, generalize in Pass 4 |
| `infer_dict` return | `(Type, HashMap<String, TypeScheme>)` |
| `typecheck_document` | Splats `TypeScheme`s into parent env across `---` boundaries |
| `instantiate()` | `fn(Type, &mut u32) → (Type, Subst)` — `#[cfg(test)]` only; not used in production |
| `instantiate_at_level()` | `fn(Type, &mut InferState) → Type` — live CALL-POLY implementation; registers fresh vars in `state.levels` |
| `instantiate_scheme()` | `fn(TypeScheme, u32, &mut InferState) → Type` |
| `generalize()` | `fn(u32, Type, &InferState) → TypeScheme` |
| `unify()` U-VAR | Bind + symmetric level lowering |
| `unify()` U-ANY + TypeVar | Set ℓ(α) = 0 to prevent generalization |
| `InferState` | `{ name_counter: u32, level: u32, levels: HashMap<String, u32>, subst: Substitution }` |
| `InferState.subst` | Accumulates row-variable constraints from [DOT-VAR] and [DOT-ROWVAR]; merged into letrec substitution in Pass 3b |
| `collect_type_vars()` | `fn(&self, &mut HashSet<String>)` — collects type variables, no level |
| `collect_row_vars()` | `fn(&self, &mut HashSet<String>)` — collects row variables, no level |
| `collect_all_vars()` | `fn(&self, &mut HashSet<String>, &mut HashSet<String>)` — collects both in one pass |
| `Type::Display` | Shows `TypeVar` name only (level hidden) |

Polymorphic builtin signatures (e.g., `map: ∀a b. Fn(Fn(a → b) × Seq(a) → Seq(b))`) are expressed via type schemes — see [Type System Extensions](07-type-extensions.md).

**Principal types.** Tinct infers principal types for fully-annotated polymorphic functions where no type variable unifies with `Any`. For partially-typed code, the inferred type depends on the checking context — subsumption introduces multiple valid types for the same expression (e.g., `42` can check against `IntLiteral(42)`, `Int`, `Number`, or `Any`). Full Damas-Milner principality is not achieved because: (a) unannotated parameters receive `Any` rather than fresh type variables, (b) singleton literal types introduce subtyping which bidirectional checking mediates but which prevents a unique most-general type, and (c) [U-SUBSUME] in CALL-POLY means the type variable binding may be more or less precise depending on argument order (both bindings are sound, but they differ).

**References:** Kiselyov, O. (2013). "How OCaml type checker works — or what polymorphism and garbage collection have in common." Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." Wright, A. (1995). "Simple imperative polymorphism."

## Limitations and Non-Guarantees

1. **Named args not unified.** Named arguments in `[call ...]` are type-checked (values inferred) but not unified against function parameter types. Requires extending `Type::Function` to carry parameter names.

2. **Any is both top and bottom.** `Any <: τ` and `τ <: Any` for all τ. In subtyping theory, this makes `Any` simultaneously the top and bottom element of the type lattice, which is unsound in general. In tinct's advisory type system this is intentional — `Any` marks the boundary between typed and untyped code, and `[@Type expr]` is the explicit narrowing mechanism.

3. **Forward references are monomorphic within letrec.** In letrec dicts, entries that reference later siblings see a fresh type variable (from Pass 1), not the eventually-generalized type scheme. Within the letrec group, mutual references are monomorphic — each entry constrains the others through unification. Polymorphic recursion (Mycroft, 1984) would require fixpoint iteration and is not supported.

4. **Variadic params typed as Any.** Variadic parameters (`...args`) are assigned type `Any`. Annotations on variadic params are forbidden by design: the runtime collects remaining positional args into an Int-keyed Dict, but row types only describe string-keyed records, so annotations cannot participate in type inference. When `Seq` types are used for variadic collection, variadic params may collect into a typed `Seq<T>` instead.
