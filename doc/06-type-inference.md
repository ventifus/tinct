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
    | Record(f₁:τ₁...fₙ:τₙ)      closed record (no row-rest parameter)
    | Proxy                      opaque proxy (field access dispatches to handler)
    | α                          type variable
    | Unknown                    gradual typing escape hatch (don't know the type)
    | Top                        universal supertype ⊤ (supertype of everything)
```

*Note:* Under BAS (Boolean-Algebraic Subtyping), all records are closed. The `ρ` (row-rest) parameter has been removed — records no longer carry a row tail. Width subtyping handles record openness via intersection and negation. The archived Rémy-style row polymorphism notation (`Record(f₁:τ₁...fₙ:τₙ, ρ)` with `ρ ::= Closed | RowVar(r)`) is documented in [Type System Extensions](07-type-extensions.md) Appendix.

**Additional types** (not expressible in annotations, used internally by inference):

| Type | Description |
|------|-------------|
| `Union(Vec<Type>)` | BAS union type (A \| B) |
| `Intersection(Vec<Type>)` | BAS intersection type (A & B) |
| `Negation(Box<Type>)` | BAS negation type (~A) |
| `Never` | Bottom type (uninhabited) |
| `Bytes` | Binary data |
| `Map(K, V)` | Homogeneous parameterized map |
| `Timestamp`, `Duration`, `Timezone` | Datetime types |
| `DirCap`, `NetCap`, `ClockCap` | Capability types (runtime-only) |
| `Handle`, `Uri` | I/O resource types (runtime-only) |
| `QuicSession`, `Http2Session`, `Http3Session`, `DatagramHandle` | Network session types (runtime-only) |

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

`check_expr` is used for both CALL-MONO and CALL-POLY argument checking (unified path as of 2026-05-11). When the expected type contains type variables (CALL-POLY), `check_expr` internally dispatches to unification to bind them. When the expected type is fully concrete (CALL-MONO), it uses subsumption checking. This unified approach eliminates verdict divergence between the two paths. See [U-SUBSUME] in the Unification section.

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
| Function arguments (CALL-POLY) | Instantiated param type (has type vars) | `check_expr` (internally dispatches to `unify`) |
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
| Access chains (dot, bracket, range) | Field type or `Unknown` |

**Confluence.** Both CALL-MONO and CALL-POLY now use `check_expr`, which internally dispatches to unification when type variables are present (CALL-POLY) or to subsumption when the expected type is fully concrete (CALL-MONO). When unification resolves a type variable to a concrete type, subsequent unification attempts against that concrete type use [U-SUBSUME] — a bidirectional subsumption fallback that checks `is_subtype` in both directions. This ensures argument ordering does not affect whether type checking succeeds. See the Unification section for details.

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
Γ ⊢ x : τ'
```

Each variable reference instantiates its type scheme with fresh variables at ℓ_current. When n = 0, this returns the body directly (monomorphic binding — no allocation).

**Dict (letrec with generalization):**

Dicts are inferred in five sequential passes using the [DICT-GEN] rule — see §Let-Generalization (Levels-Based) for the full specification. The rule uses fresh type variables (not `Any`) for forward references and generalizes entry types after inference.

**Function definition:**

```
For each param pᵢ:
    if variadic (...pᵢ): σᵢ = Unknown                   (see Limitation #3)
    else if annotated pᵢ@σᵢ: use σᵢ
    else: σᵢ = Unknown
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
    if variadic: use Unknown
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

Unannotated non-variadic params get type Unknown. This is the gradual typing escape hatch — without annotations, functions have monomorphic type Fn(Unknown...Unknown → τᵣ). Polymorphism requires explicit type variable annotations (e.g., `x@a`).

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

Monomorphic path with checking: each argument is **checked** against its parameter type using subsumption. `[add "hello"]` where `add : Fn(Int Int → Int)` produces a type error because `String ≮: Int`. The `check_expr` call synthesizes the argument type and applies `[SUB]`.

```
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_type_vars(Fn(...)) = true
Γ ⊢ aᵢ ⇒ τᵢ  for i = 1..n                          [synthesis]
|args| = |params|
(σ'₁...σ'ₙ → σ'ᵣ) = instantiate(σ₁...σₙ → σᵣ)
S = unify(σ'₁ ≐ τ₁, ..., σ'ₙ ≐ τₙ)                  [with U-SUBSUME]
────────────────────────────────── [CALL-POLY]
Γ ⊢ [call f a₁...aₙ] ⇒ S(σ'ᵣ)
```

**Implementation note:** When the function expression is a VarRef to a polymorphic scheme (e.g., `[id 42]` where `id` is bound to `∀a. Fn(a → a)`), `check_call_with_scheme` is invoked directly with the scheme, bypassing the VAR-POLY instantiation step. This optimization instantiates the scheme once instead of twice (VAR-POLY followed by CALL-POLY). For other function expressions (inline lambdas, compound access chains), the normal path applies: infer the function expression (which may instantiate a scheme via VAR-POLY), then proceed to CALL-POLY. See `src/typecheck.rs` `infer_expr` Call case for the dispatch logic (lines ~441-451).

Polymorphic path with unification: arguments are checked via `check_expr`, which internally dispatches to unification when the expected type contains type variables. Unification binds type variables via [U-VAR] and handles concrete-type comparisons via [U-SUBSUME] (bidirectional subsumption fallback). This is critical for confluence: when multiple arguments constrain the same type variable with different precision (e.g., `IntLiteral(42)` and `Int`), the subsumptive fallback ensures type checking succeeds regardless of argument order. See the Unification section for [U-SUBSUME] details.

Note: Both CALL-MONO and CALL-POLY now use `check_expr` (unified path as of 2026-05-11). `check_expr` internally dispatches to unification when the expected type has type variables (CALL-POLY), or to subsumption when fully concrete (CALL-MONO).

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

**Unified CALL-MONO/CALL-POLY path (implemented 2026-05-11).** The divergence described above has been eliminated. Both CALL-MONO and CALL-POLY now route through `check_expr`, which internally dispatches to `unify` when the expected type has inference vars (TypeVars), or to `is_subtype` when fully concrete. This ensures identical literal pairs receive consistent verdicts regardless of whether the function type has inference vars. The table above is retained for historical reference but no longer reflects implementation behavior.

```
Γ ⊢ f ⇒ Unknown
────────────────────────────────── [CALL-UNKNOWN]
Γ ⊢ [f a₁...aₙ] ⇒ Unknown
```

Calling a value typed as Unknown returns Unknown. Arguments are still synthesized (for type map population and nested error detection) but not checked against parameter types.

Named argument type checking is implemented. `Type::Function` carries `params: Vec<(Option<String>, Type)>` where `Some(name)` is a user-defined function parameter name extracted from the AST and `None` is a builtin parameter (no name exposed at the type level). Three paths check named args:

- **CALL-MONO**: for each named arg, the matching parameter is found by name via `params.iter().find_map(|(pname, pty)| if pname.as_ref() == Some(arg_name) { Some(pty) })`, the arg is inferred with `infer_expr`, and the type is unified against the parameter type. Unknown names and type mismatches emit `TypeError`.
- **CALL-POLY** (`check_call`): same name-based lookup and unify on the instantiated params.
- **`check_call_with_scheme`** Function arm: same name-based lookup and unify, applied after the positional arg unification loop, using the already-instantiated `params` from `instantiate_scheme`.

Known gap: when the callee is a letrec forward reference (same-dict scope), the type resolves to an unbound `TypeVar` and falls through to the `TypeVar` arm, which skips named-arg validation. See [Type System Extensions](07-type-extensions.md) §Completeness for the remaining gaps.

**Access chains:**

```
Γ ⊢ e : Record(... k:τ ..., ρ)
────────────────────────────────── [DOT]
Γ ⊢ e.k : τ

Γ ⊢ e ⇒ α,  β fresh
S = unify(α ≐ Record({k: β}))
────────────────────────────────── [DOT-VAR]
Γ ⊢ e.k ⇒ β
```

After unification, α is bound in S — references to α in the conclusion denote its resolved image S(α), not the original variable. The occurs check and level lowering for α, β, and ρ are handled internally by `unify()`.

*Note: The [DOT-ROWVAR] rule that previously appeared here (binding RowVar tails on field access) is archived — RowVar has been removed under BAS. Under the current system, accessing a field not present in a closed record is a static error. See [Type System Extensions](07-type-extensions.md) §Boolean-Algebraic Subtyping.*

```
Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : StringLiteral(k),  F(k) = τ
────────────────────────────────── [BRACKET-LIT]
Γ ⊢ e[key] : τ

Γ ⊢ e : Record(F, ρ),  Γ ⊢ key : Str | Int | Unknown
────────────────────────────────── [BRACKET-DYN]
Γ ⊢ e[key] : Unknown

Γ ⊢ e : Record(F, ρ),  bounds : Int | Str | Unknown
────────────────────────────────── [RANGE]
Γ ⊢ e[start..end] : Record(F, ρ)

Γ ⊢ e : Unknown
────────────────────────────────── [ACCESS-UNKNOWN]
Γ ⊢ e.k : Unknown,  Γ ⊢ e[key] : Unknown,  Γ ⊢ e[start..end] : Unknown
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
Γ ⊢ [type inner] : Unknown
```

**Annotated expression:**

```
resolve(ann) = τ
────────────────────────────────── [ANNOTATED]
Γ ⊢ name@ann : τ
```

When name = "Fn": interpret as function type constructor.

**`@Fn` vs `Fn@T` parameter annotation behavior:** Bare `@Fn` in a parameter annotation position (e.g., `f@Fn`) resolves to `Type::Unknown` via `resolve_type_name` — any value is accepted at type-check time and at any `[@Fn expr]` TypeAssert site. No callability check is performed by TypeAssert; `Type::Unknown` matches all values unconditionally, including non-callables. Callability is enforced only when the parameter is actually invoked as a function, which raises a `NotAFunction` error at the call site. `Fn@ReturnType` in a function return annotation position resolves to `Type::Function` with the specified return type via `resolve_fn_type`, which recursively resolves the return annotation and parameter types. This distinction arises from the annotation resolution dispatch: `@Fn` alone has no type parameters, so it cannot construct a `Type::Function` (which requires both params and ret); `Fn@ReturnType [ParamTypes]` has the necessary structure for full function type resolution.

**Seq types:** `Seq(τ)` exists in the type grammar and is handled by unification and subtyping. Sequence constructors (`$seq`, `$range`, etc.) infer as `Seq(τ)` — see [Type System Extensions](07-type-extensions.md) §Precision.

## Unification: unify(τ₁, τ₂, S) → S'

Unification finds a most general substitution S such that S(τ₁) = S(τ₂). Before matching, both types are normalized via S (substitution applied). Unification follows **Robinson (1965)** for structural decomposition and variable binding, extended with pragmatic promotion rules (see bidirectional literal-to-parent promotions in the implementation below). Subtyping is handled by `check_expr` via the `[SUB]` rule and `is_subtype` for directional checks, and by `[U-SUBSUME]` for bidirectional compatibility within unification. This separation follows Pierce & Turner (2000) and Dunfield & Krishnaswami (2021).

**Algorithm variant:** The overall inference algorithm is closer to **Algorithm J** (Milner 1978) than Algorithm W (Damas & Milner 1982): it uses a mutable global substitution (`InferState.subst`) accumulated across inferences with immediate unification on each constraint, rather than threading explicit substitutions compositionally. This is more efficient but harder to reason about formally. The five-pass dict inference (§Dict Inference) is a letrec extension following Tofte (1988).

```
unify(τ, τ, S) = S                              [U-REFL]
unify(Unknown, τ, S) = S                         [U-UNKNOWN-L]
unify(τ, Unknown, S) = S                         [U-UNKNOWN-R]
unify(α, τ, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-L]
unify(τ, α, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-R]
```

**Note:** When one side is a type variable α and the other is `Unknown`, the implementation fires specialized TypeVar rules ([U-UNKNOWN-VAR]/[U-VAR-UNKNOWN], see §Let-Generalization) first, which zero ℓ(α) to prevent unsound generalization of Unknown-unified variables. The general [U-UNKNOWN-L]/[U-UNKNOWN-R] rules above apply only when neither side is a TypeVar.

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
τ <: Unknown                                     [S-UNKNOWN-TOP]
Unknown <: τ                                     [S-UNKNOWN-BOT]
τ <: τ                                           [S-REFL]
IntLiteral(n) <: Int <: Number                   [S-INT]
StringLiteral(s) <: Str                          [S-STR]
Float <: Number                                  [S-FLOAT]
Seq(τ) <: Seq(σ)  if τ <: σ                      [S-SEQ]

Record(F₁) <: Record(F₂) if:
    ∀(k:σ) ∈ F₂, ∃(k:τ) ∈ F₁ with τ <: σ       (depth subtyping on shared fields)
    Under BAS, all records are closed (no RowVar tail). Width subtyping:
    a record with MORE fields is a subtype of one with FEWER fields.
    Extra fields in F₁ beyond F₂ are permitted.                      [S-REC]

Fn(p₁...pₙ→r₁) <: Fn(q₁...qₙ→r₂) if:
    |p| = |q|
    qᵢ <: pᵢ  for all i                          (contravariant params)
    r₁ <: r₂                                      (covariant return)
                                                 [S-FN]
```

**Note on Unknown and Top:** `Unknown` relates to other types via consistency (~), not subtyping (<:) — see `is_consistent()`. `Top` is the true universal supertype with `τ <: Top` for all `τ`. The pre-B2 `Any` type that served as both top and bottom (violating antisymmetry) has been eliminated by the gradual-typing-split sprint — see `doc/whatif/completed/gradual-typing.md`.

## Instantiation

```
instantiate(τ) = (S(τ), S)
    where S.type_map = {α₁ ↦ _t0, α₂ ↦ _t1, ...}  (type vars → Type)
    for each αᵢ ∈ FTV(τ), fresh type var names _tN generated
    from a shared monotonic per-file counter.

FTV(τ) collects type variables via collect_type_vars().

Under BAS, row variables have been removed. The substitution map
contains only type_map (no row_map). TypeScheme carries only
type_vars (no row_vars field). All records are closed; openness
is expressed via width subtyping in is_subtype.
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
    pub type_vars: Vec<String>,         // quantified type variable names
    pub constraints: Vec<Constraint>,   // constraints on quantified vars
    pub label_vars: Vec<String>,        // quantified label variables (Kind::Label)
    pub doc: Option<String>,            // documentation from annotations (added in constraint-annotations sprint)
    pub body: Type,
}

impl TypeScheme {
    pub fn mono(ty: Type) -> Self {
        Self {
            type_vars: vec![],
            constraints: vec![],
            label_vars: vec![],
            doc: None,
            body: ty,
        }
    }
}
```

**TypeScheme grammar:** `σ ::= ∀(α₁...αₙ). [C₁ a₁, ...] τ` where α₁...αₙ are type variables, C₁ a₁, ... are constraints, and τ is the body type. The row variable portion (`ρ₁...ρₘ`) from the original Remy-style grammar has been removed under BAS.

`PartialEq` for `TypeScheme` compares structurally (type_vars + constraints + label_vars + doc + body). `Display` shows `∀a b. [Eq a] Fn(a → b)` for constrained schemes, or the bare type for monomorphic ones. Located in `types.rs`.

**Levels.** Every type variable α carries an integer level ℓ(α). The type checker maintains a current level counter ℓ_current, incremented at each dict boundary (every `infer_dict` call):

- Fresh type variables are created at ℓ_current
- `Type::TypeVar(String)` becomes `Type::TypeVar(String, u32)` (name + level)
- `PartialEq` for `Type` is implemented manually: `TypeVar(a, _) == TypeVar(b, _)` compares names only, ignoring levels. This preserves the [U-REFL] fast path in `unify()`.
- Under BAS, all records are closed. Row variables (`RowTail::RowVar`) have been removed — the `Row` struct now contains only `fields: HashMap<String, Type>` with no tail field. Width subtyping handles record openness.
- `Display` for `TypeVar` hides the level (internal inference state, not user-facing).

**Level storage and mutation.** Levels must be mutable during unification (Kiselyov's level lowering). Since `Type` is a value type, levels are stored in a separate mutable map alongside the substitution:

```rust
pub struct InferState {
    pub name_counter: u32,   // monotonic fresh variable name counter
    pub level: u32,          // current binding depth
    pub levels: HashMap<String, u32>,  // var name → current level
    pub subst: Substitution, // global constraint accumulator for access-chain bindings
}
```

`InferState.subst` accumulates row-variable constraints from [DOT-VAR] and [DOT-ROWVAR] across the entire inference pass. During letrec inference (Pass 3b), accumulated constraints are merged into the letrec substitution: when both maps bind the same variable, the two bindings are **unified** (Algorithm W substitution composition, Damas & Milner 1982) rather than silently dropped. Colliding bindings are **unified** rather than dropped, maintaining substitution composition soundness. After merging, Pass 3d writes the fully-merged local substitution back into `state.subst` so that subsequent dicts in the same document see the letrec bindings. See the Pass 3b merge algorithm in [DICT-GEN] below for the precise pseudocode.

When a `TypeVar(name, lvl)` is created, `levels[name] = lvl` is recorded. During unification, level lowering mutates `levels[name]` without rebuilding the `Type`. `generalize()` consults `levels` for the authoritative level of each variable. The level embedded in `TypeVar(String, u32)` is the *creation-time* level; `InferState.levels` is the *current* (possibly lowered) level.

**RowVar level semantics.** `RowTail::RowVar(String, u32)` carries the same creation-time level as `TypeVar`. The level stored in the `RowVar` variant is set at creation and never mutated directly — it is the *creation-time* level. The *current* (possibly lowered) level is always read from `InferState.levels[name]`. During row-variable binding (Case 2, 3, and 4 of `unify_remainders`), `lower_row_var_levels` is called with the binding RowVar's current level (read from `state.levels`, not from the `RowVar` variant itself) to lower the levels of all type and row variables in the bound row. `generalize()` generalizes a RowVar with name `r` when `levels[r] > enclosing_level`, identically to `TypeVar`. This two-field design (creation-time level in the variant, current level in `state.levels`) matches the `TypeVar` design: the variant field is only used during construction and display; all level queries go through `state.levels`.

**Level adjustment during unification (symmetric).** Both branches of type variable unification perform level lowering:

```
unify(α, τ, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ)                         [U-VAR-LEVEL]

unify(τ, α, S) = S[α ↦ τ]
    if α ∉ FV(τ)                                   [occurs check]
    and set ℓ(β) = min(ℓ(β), ℓ(α))
        for all β ∈ FTV(τ)                         [U-VAR-LEVEL-SYM]
```

Both rules lower levels symmetrically: when binding α to τ, every type variable β and every row variable ρ inside τ has its level lowered to `min(ℓ(β or ρ), ℓ(α))`. This prevents variables from escaping their scope through either side of a unification. Row variables must be lowered because τ may contain row variables through Record nesting (e.g., τ = Record({x: Int, ...ρ})).

**Unknown-unification and generalization.** When a type variable α is unified with `Unknown`, the current [U-UNKNOWN] rules succeed without binding α. To prevent incorrect generalization of the unbound α, `unify(α, Unknown)` sets `ℓ(α) = 0` (below all binding levels):

```
unify(α, Unknown, S) = S,  set ℓ(α) = 0               [U-UNKNOWN-VAR]
unify(Unknown, α, S) = S,  set ℓ(α) = 0               [U-VAR-UNKNOWN]
unify(Unknown, τ, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ)                             [U-UNKNOWN-COMPLEX]
unify(τ, Unknown, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ)                             [U-COMPLEX-UNKNOWN]
```

This ensures Unknown-touched variables are never generalized (since `ℓ(β) = 0` is never `> ℓ` for any binding level). The [U-UNKNOWN-VAR] and [U-VAR-UNKNOWN] rules are special cases of the complex rules where FTV(α) = {α}. The [U-UNKNOWN-COMPLEX] and [U-COMPLEX-UNKNOWN] rules handle cases like `unify(Unknown, Fn(β → Int))` where β must also be zeroed to prevent over-generalization.

**Generalization.** At a dict boundary at level ℓ, after all entries in the letrec group are inferred:

```
generalize(ℓ, τ) = ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ     [GEN]
```

where ℓ(α) is read from `InferState.levels[α]` (the current, possibly lowered level). Type variables whose level exceeds the enclosing scope's level are local to the binding and can be universally quantified. Variables at or below level ℓ are free in the enclosing scope and must remain monomorphic.

Implementation signature:

```rust
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme
```

Collects type variables from ty via level-aware traversal (collect_type_vars), filters by `current_level > level`, returns `TypeScheme { type_vars, doc: None, body: ty.clone() }`.

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
         unlike the previous Unknown which silently matched everything).
Pass 2 — Type aliases: unchanged. Aliases remain monomorphic
         (IndexMap<String, Type>, not TypeScheme).
Pass 3 — Infer values: at level ℓ+1, for each non-alias
         entry kᵢ, infer Γ' ⊢ eᵢ : τᵢ, then unify(αᵢ, τᵢ).
         Apply resulting substitution S.
         
         Implementation note: Pass 3 splits into sub-passes 3a/3b/3c/3d
         to handle the two-substitution model (local + state.subst).
         3a: clone state.subst → local subst;
         3:  for each entry, infer value, unify αᵢ with inferred type into
             local subst; on success, propagate all local subst bindings
             into state.subst (per-entry) so that subsequent sibling
             infer_expr calls can resolve forward-reference TypeVars bound
             in earlier siblings — this is the mechanism that makes letrec
             sibling cross-references work (e.g. `[a: "hi"  b: [length a]]`);
         3b: merge state.subst updates → local subst (unify-based reconciliation);
         3c: apply merged subst to all field types;
         3d: merge local subst back into state.subst for subsequent dicts.

         Pass 3b merge algorithm (Algorithm W substitution composition,
         Damas & Milner 1982) — applied to type_map:

             for each (k, v) in state.subst.type_map:
                 applied_v = local_subst.apply(v)
                 if k ∈ local_subst.type_map:
                     existing = local_subst.type_map[k]
                     local_subst.type_map.remove(k)   // prevent apply() cycle k→existing→k
                     if error e = unify(existing, applied_v, local_subst, state):
                         errors.push(e)
                         local_subst.type_map[k] = existing  // restore original on failure
                         continue
                     // Re-insert resolved binding so pass 3c can apply it.
                     // (e.g. field_types["a"] = TypeVar(_tb) in [a: $b  b: 42]
                     //  needs resolution via pass 3c's apply call)
                     resolved = local_subst.apply(applied_v)
                     local_subst.type_map[k] = resolved
                 else:
                     local_subst.type_map[k] = applied_v

         The remove-before-unify step is required because `apply()` chases
         bound variables transitively: if k is in the map during unify(),
         apply() would chase k → existing → k in an infinite cycle.
         Collision means two independent inference paths (access-chain and
         letrec unification) each bound the same variable; unifying their
         bindings reconciles the constraints correctly.
         
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

**Interaction with `Unknown` and unannotated parameters:**

- Unannotated function parameters still receive type `Unknown` (not a fresh type variable). `[fn [x] x]` remains `Fn(Unknown → Unknown)`.
- `Unknown` in unification acts as a universal match ([U-UNKNOWN-L], [U-UNKNOWN-R]) but sets ℓ(α) = 0 for any type variable α it touches ([U-UNKNOWN-VAR], [U-VAR-UNKNOWN]), preventing generalization.
- Annotated type variables (e.g., `x@a`) create fresh type variables at ℓ_current. These participate in generalization normally.
- The practical effect: let-generalization benefits code that uses type annotations. `[id: [fn [x@a] x]]` generalizes `id` to `∀a. Fn(a → a)`; subsequent `[id 42]` and `[id "hello"]` each get independent instantiations.

**Interaction with CALL-POLY.** When a call expression targets a `VarRef`, the inference engine inspects the scheme directly before any instantiation. This determines the routing:

- **Polymorphic scheme** (has quantified `type_vars` or `row_vars`): routes to `check_call_with_scheme`, which calls `instantiate_scheme` once to produce a function type with fresh `_tN` variables at ℓ_current. It then checks `has_inference_vars()` on the *post-instantiation* type: if all variables were resolved (fully concrete), it takes the CALL-MONO path (bidirectional checking via `check_expr`); if type variables remain, it takes the CALL-POLY path (synthesize arguments, unify, apply substitution to return type). This avoids double instantiation — without this optimization, VAR-POLY would instantiate the scheme at the reference site, producing `_tN` variables, and then CALL-POLY's `instantiate_at_level` would freshen those into yet more `_tM` variables.
- **Monomorphic scheme** (no quantified vars): routes to `check_call`, which infers the function expression normally. Since the scheme has no quantified variables, no instantiation occurs. The inferred type is typically concrete, so the CALL-MONO path fires directly.
- **Non-VarRef function expressions** (e.g., inline lambdas): always route to `check_call`.

**Substitution name uniqueness.** `Substitution::type_map` and `Substitution::row_map` are keyed by variable name, routing type and row variable bindings to their respective maps. User-annotated type variables (e.g., `@a`) are mapped to fresh `_tN` names by `resolve_type_name` during Pass 3 inference. Each function entry maintains its own `ann_mapping` (a per-function `HashMap<String, String>`), so `@a` in one function maps to a different `_tN` than `@a` in a sibling function. Within a single function, all references to the same annotation name `@a` resolve to the same `_tN` variable (ensuring constraints are shared as intended). After Pass 4 generalization produces `TypeScheme`s, `instantiate_scheme()` renames the quantified variables to fresh `_tM` names at each call site, preventing cross-call-site interference.

**Error recovery.** If Pass 3 inference fails for an entry, `Type::Unknown` is inserted for that entry (matching current behavior). Level lowering from partial unification before the failure is retained in `InferState.levels` — this is conservative (may prevent generalization of some variables) but safe. Generalization in Pass 4 proceeds for successfully-inferred entries; failed entries get `TypeScheme::mono(Type::Unknown)`.

**Key invariants:**

1. **Level monotonicity:** ℓ_current only increases when entering binding scopes. Fresh variables are always created at ℓ_current.
2. **Generalization soundness:** Only variables with ℓ(α) > ℓ_enclosing are generalized, ensuring no variable escapes its scope. Level lowering during unification ([U-VAR-LEVEL], symmetric) prevents variables from being captured at too high a level. Unknown-touched variables have ℓ = 0, preventing generalization.
3. **Value restriction (not needed):** Tinct does not have mutable references, so the value restriction (Wright, 1995) is unnecessary. All bindings can be generalized safely.
4. **Occurs check:** Unchanged — prevents infinite types regardless of levels.
5. **Substitution idempotence:** Unchanged — transitive chasing is orthogonal to levels.
6. **Letrec monomorphism during inference:** Within a letrec group, entries see each other as monomorphic during Pass 3 (fresh type variables, not schemes). Polymorphism only becomes visible after Pass 4 generalization.
7. **PartialEq level-blindness:** `TypeVar` equality ignores levels, preserving [U-REFL] semantics. Levels are consulted only during generalization (via `InferState.levels`).

**Key implementation types:**

| Component | Specification |
|-----------|--------------|
| `Type::TypeVar` | `TypeVar(String, u32)` — manual `PartialEq` (name only, level ignored for equality) |
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
| `InferState.subst` | Accumulates constraints from [DOT-VAR]; merged into letrec substitution in Pass 3b |
| `collect_type_vars()` | `fn(&self, &mut HashSet<String>)` — collects type variables, no level |
| `Type::Display` | Shows `TypeVar` name only (level hidden) |

Polymorphic builtin signatures (e.g., `map: ∀a b. Fn(Fn(a → b) × Seq(a) → Seq(b))`) are expressed via type schemes — see [Type System Extensions](07-type-extensions.md).

**Principal types.** Tinct infers principal types for fully-annotated polymorphic functions where no type variable unifies with `Unknown`. For partially-typed code, the inferred type depends on the checking context — subsumption introduces multiple valid types for the same expression (e.g., `42` can check against `IntLiteral(42)`, `Int`, `Number`, or `Top`). Full Damas-Milner principality is not achieved because: (a) unannotated parameters receive `Unknown` rather than fresh type variables, (b) singleton literal types introduce subtyping which bidirectional checking mediates but which prevents a unique most-general type, and (c) [U-SUBSUME] in CALL-POLY means the type variable binding may be more or less precise depending on argument order (both bindings are sound, but they differ).

**References:** Kiselyov, O. (2013). "How OCaml type checker works — or what polymorphism and garbage collection have in common." Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." Wright, A. (1995). "Simple imperative polymorphism."

## Constrained Type Variables

Tinct implements **constrained type variables** to provide precise types for overloaded builtins and user-defined polymorphic functions. Constraints restrict which types can instantiate a type variable, enabling static rejection of invalid operations (e.g., `[= [fn [] 1] [fn [] 2]]`) while preserving parametric polymorphism for valid uses.

### Constraint Representation

A **constraint** is a pair `(class, var)` where `class` is the type class name (e.g., `"Equatable"`) and `var` is the type variable name (e.g., `"a"`). Type schemes carry constraints alongside quantified variables:

```
TypeScheme {
    type_vars: Vec<String>,
    constraints: Vec<Constraint>,    // NEW: constraints on type_vars
    doc: Option<String>,
    body: Type,
}
```

**Display format:** Constraints appear before the type body, separated by `=>`:

- `Equatable a => Fn@Bool [a a]` — equality requires Equatable constraint
- `Numeric a, Showable b => Fn@Str [a b]` — multiple constraints comma-separated
- `Fn@Int [Int Int]` — monomorphic schemes (no constraints) display as before

### Primitive Built-in Constraints

Four classes have primitive built-in instances whose dispatch is handled by the Rust runtime. These instances cannot be overloaded by user-defined classes for the primitive operators (`=`, `<`, `+`, `str`):

| Class | Primitive instances | Example builtins |
|-------|---------------------|-----------------|
| `Equatable` | Int, IntLiteral, Float, Str, StringLiteral, Bool, Number | `=` |
| `Comparable` | Int, IntLiteral, Float, Str, StringLiteral, Number | `<`, `>`, `<=`, `>=` |
| `Numeric` | Int, IntLiteral, Float, Number | `+`, `-`, `*`, `/` |
| `Showable` | all types except Error | `str` |

**Rationale:** Function, Seq, and Record are excluded from Equatable because structural equality would force lazy thunks, violating lazy evaluation semantics.

All other classes (`Mappable`, `Appendable`, `Functor`, `Applicative`, `Monad`, `Foldable`, `Traversable`) are declared in the stdlib using `[class ...]` and `[instance ...]` forms and are fully user-extensible — see §Higher-Kinded Types and Type Classes.

### Constraint Generation and Checking

1. **Builtin registration** (`TypeEnv::with_builtins`): Overloaded builtins are registered with constrained type schemes:

   ```
   =  : Equatable a => a → a → Bool
   <  : Comparable a => a → a → Bool
   +  : Numeric a => a → a → a
   str: Showable a => a → Str
   ```

2. **Instantiation** (`instantiate_scheme`): When a constrained scheme is instantiated, constraints are copied with renamed variables:

   ```
   scheme: Numeric a => a → a → a
   instantiate → fresh var _t0, constraint Numeric _t0
   result: Fn@_t0 [_t0 _t0]
   state.constraints += Constraint { class: "Numeric", var: "_t0" }
   ```

3. **Constraint checking** (`unify`): When binding a type variable α to a concrete type τ (U-VAR-LEVEL arm), check all constraints on α:

   ```
   For each constraint C(α) in state.constraints:
       if ¬satisfies_constraint(τ, C):
           error "type τ does not satisfy constraint C"
   ```

   Example: unifying `_t0` (with `Numeric _t0`) with `Fn@Int [Int]`:
   ```
   satisfies_constraint(Fn@Int [Int], "Numeric") → false
   → TypeError: type Fn@Int [Int] does not satisfy constraint Numeric
   ```

4. **Generalization** (`generalize`): Constraints on generalized variables are included in the resulting TypeScheme:

   ```
   state.constraints: [Numeric _t0, Showable _t1]
   generalizable vars: {_t0, _t2}
   → scheme.constraints: [Numeric a]    (only _t0 was generalized)
   ```

### Inference Flow Example

```tinct
# User code
result: [+ 1 "hello"]

# Type inference
1. Look up + → Equatable a => a → a → a
2. Instantiate → Numeric _t0, Fn@_t0 [_t0 _t0]
3. Constraint generated: state.constraints += Numeric _t0
4. Argument 1: infer(1) → IntLiteral(1)
5. Unify _t0 with IntLiteral(1):
   - Check: satisfies_constraint(IntLiteral(1), "Numeric") → true
   - Bind: _t0 ↦ IntLiteral(1)
6. Argument 2: infer("hello") → StringLiteral("hello")
7. Unify _t0 with StringLiteral("hello"):
   - Resolve _t0 via substitution → IntLiteral(1)
   - Unify IntLiteral(1) with StringLiteral("hello") → type mismatch error
```

Alternative failing case:
```tinct
result: [= [fn [] 1] [fn [] 2]]

# Type inference
1. Look up = → Equatable a => a → a → Bool
2. Instantiate → Equatable _t0, Fn@Bool [_t0 _t0]
3. Argument 1: infer([fn [] 1]) → Fn@Int []
4. Unify _t0 with Fn@Int []:
   - Check: satisfies_constraint(Fn@Int [], "Equatable") → false
   - Error: "type Fn@Int [] does not satisfy constraint Equatable"
```

### Current Limitations of Hardcoded Constraints

1. **Numeric stays hardcoded:** `Numeric` cannot be expressed as a single-parameter class because `Int + Float → Float` requires multi-parameter type classes. It remains as a fixed instance set.

2. **Primitive operators are not overloadable by user instances:** Builtin operators (`=`, `<`, `str`, etc.) dispatch via hardcoded Rust type inspection and cannot be routed through user-defined typeclass instances. User-defined instances are invoked by explicit dict method call (`inst.method args`); the primitive operators never implicitly delegate to instance dicts. This is by design — implicit dictionary threading (Haskell-style) is not supported. User-defined monads and other class instances DO dispatch at runtime via explicit instance dicts (the `[do monad ...]` form passes the monad dict explicitly).

3. **No constrained row variables:** `Equatable [name: a ...]` (requiring all fields to satisfy Equatable) requires row-level constraints (Gaster & Jones 1996).

**References:** Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." Jones, M.P. (1993). "A system of constructor classes: overloading and implicit higher-order polymorphism." Jones, M.P. (1995). *Qualified types: Theory and practice.*

## Higher-Kinded Types and Type Classes

Tinct supports rank-1 higher-kinded types (Jones 1993 constructor classes) via an extension to the kind system. This enables a generic Functor/Applicative/Monad hierarchy, `[do]` monad inference, and precisely-typed `get`/`get-in` via label polymorphism.

### Kind System

The kind grammar has four kinds:

```
Kind ::= *         -- concrete types (Int, Str, Record, ...)
       | Row        -- record field sets
       | Operator   -- type constructors (* → *, written `Operator`)
       | Label      -- type-level string labels (for HasField constraints)
```

`Operator` is notation for `* → *`. A TypeVar of kind `Operator` ranges over type constructors; a TypeVar of kind `Label` ranges over string field names.

The kind of each TypeVar is tracked in `InferState.kind_env: HashMap<String, Kind>`. TypeVars of kind `Operator` arise from `@Operator` annotations on class parameters; TypeVars of kind `Label` arise from Label annotations (`key@Label` for anonymous or `key@[label: l]` for named, per the `label-annotation-syntax` sprint).

### Type Constructor Application

Two new `Type` variants:

- `Type::App(Box<Type>, Box<Type>)` — type constructor applied to an argument: `App(Result, Int)` is `Result Int`
- `Type::Operator(String)` — a type constructor variable: `Operator("m")` for a Monad variable `m`

In annotation positions, `[f a]` (no colons) is type constructor application when `f` is Operator-kinded or a user type alias. `@[m a]` applies constructor `m` to argument `a`.

**Unification:**

```
UNIFY-OPERATOR:
  m ∉ ftv(T)    kind_env ⊢ T : *
  ──────────────────────────────────
  unify(Operator(m), T) = [m ↦ T]

  unify(Operator(m), Operator(n)) = [m ↦ Operator(n)]   (symmetric)

UNIFY-APP:
  unify(f₁, f₂) = θ₁    unify(θ₁(a₁), θ₁(a₂)) = θ₂
  ─────────────────────────────────────────────────────
  unify(App(f₁, a₁), App(f₂, a₂)) = θ₂ ∘ θ₁
```

### Typeclass Declarations and Instances

Classes are declared with `[class ...]` and instances with `[instance ...]`:

```tinct
[Functor: [class [f@Operator]
  [fmap: [fn@[f b] [fn@b [a]  [f a]]]]]]

[FunctorResult: [instance [Functor Result]
  [fmap: result-map]]]
```

**Superclass chains** use `extends`:

```tinct
[Applicative: [class [f@Operator] extends [Functor f]
  [pure:  [fn@[f a] [a]]]
  [lift2: [fn@[f c] [fn@c [a b]  [f a]  [f b]]]]]]

[Monad: [class [m@Operator] extends [Applicative m]
  [bind: [fn@[m b] [[m a]  fn@[m b] [a]]]]]]
```

The superclass chain provides method inheritance. `MonadResult` carries `bind` directly and inherits `pure`, `lift2`, and `fmap` from the superclass instances.

**Rank-1 restriction:** `App(Operator("f"), Operator("g"))` (applying one Operator variable to another) is excluded. Multiple flat Operator quantifiers in one method type are allowed — `traverse` has both `f@Applicative` and `t@Traversable` in its signature, which is rank-1.

**Instance resolution:** When a constraint `C m` where `m : Operator` is active, the type checker looks up `m` in the `ClassEnv`, finds an `[instance [C M] ...]` where `M` unifies with `m`, and substitutes the instance's method implementations. `App(Operator("m"), a)` is unified against known concrete applications (`App(Result, T)`, `App(Seq, T)`) to resolve `m`.

### The Typeclass Hierarchy

The stdlib defines the following typeclass hierarchy:

| Class | Kind | Extends | Key methods |
|-------|------|---------|-------------|
| `Functor` | `Operator` | — | `fmap : (a → b) → f a → f b` |
| `Applicative` | `Operator` | Functor | `pure : a → f a`, `lift2 : (a → b → c) → f a → f b → f c` |
| `Monad` | `Operator` | Applicative | `bind : m a → (a → m b) → m b` |
| `Foldable` | `Operator` | — | `fold : (b → a → b) → b → t a → b`, `to-seq : t a → Seq a` |
| `Traversable` | `Operator` | Functor, Foldable | `traverse : (a → f b) → t a → f (t b)` |
| `Mappable` | `Operator` | — | `map : (a → b) → f a → f b` (weaker than Functor)† |
| `Appendable` | `*` | — | `append : a → a → a`, `empty : a` |
| `Equatable` | `*` | — | `= : a → a → Bool`, `not= : a → a → Bool` |
| `Comparable` | `*` | Equatable | `< : a → a → Bool`, etc. |
| `Showable` | `*` | — | `show : a → Str` |

**†Implementation note:** `Mappable` is currently registered as a placeholder `Kind::Type` class and will be promoted to `Kind::Operator` in the `hkt-mappable-appendable` sprint.

Instances cover `Result`, `Seq`, `Maybe`, `Record` as appropriate.

### Generic Functions

**Implementation status:** The functions `sequence` and `traverse` shown below are specified but not yet in `stdlib/prelude.llt` — they require the `hkt-stdlib` sprint.

With the typeclass hierarchy, these generic functions will be available:

```tinct
# collect effects from any Traversable container
sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
  [traverse f [fn [x] x] xs]]

# map with effects over any Traversable
traverse: [fn@[f [t b]] [f@Monad  t@Traversable  fn@[f b] [a]  xs@[t a]]
  [t.traverse f xs]]

# forM, when, liftM2 also defined
```

### `[do]` Inference

**Implementation status:** The inferred `[do]` form (without explicit monad argument) is not yet implemented — it requires the `hkt-do-macro` sprint. The explicit `[do monad steps...]` form is fully implemented and available.

The `[do]` macro infers the monad from context when no explicit monad argument is given:

```tinct
# Explicit form — always works
[do result
  [r: [fetch %nc url]]
  [r.body]]

# Inferred form — @Result annotation implies result monad
[fetch-and-parse: [fn@[ok: Str  err: Str] [url@Str]
  [do
    [r:    [fetch %nc url]]
    [data: [from-json r.body]]
    [get "items" data]]]]
```

**Inference priority:**

1. If the enclosing function's return type annotation unifies with `App(m, _)` for a registered Monad `m`, use that instance
2. If the first binding's RHS infers as `App(m, a)` for a known Monad, use that instance
3. Otherwise require an explicit monad argument

The explicit `[do monad ...]` form always takes priority and is backward-compatible.

### HasField — Label-Polymorphic Field Access

`HasField l d a` is a qualified-type constraint asserting that record type `d` has a field at label `l` with type `a`. It carries a functional dependency `(l, d) → a` — given label and dict type, the field type is uniquely determined (Jones 1994).

**`get` is label-polymorphic:**

```
get : ∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a
```

Field access is precise: `[get "host" config]` returns the type of `config.host`, not `Unknown`.

**Instance resolution rules:**

```
HasField (Concrete l) Record(fields) τ         when l ∈ dom(fields) and fields(l) = τ
HasField l (τ₁ | τ₂) (a₁ | a₂)               distributes over union [HAS-FIELD-UNION]
HasField l (τ₁ & τ₂) (a₁ & a₂)               distributes over intersection
HasField l ⊤ Unknown                            for BAS-collapsed disjoint-field unions
HasField l Unknown Unknown                      gradual typing fallback
```

**Label TypeVars** are introduced by Label annotations (`key@Label` for anonymous or `key@[label: l]` for named) and tracked in `kind_env` with `Kind::Label`. They are generalized into `TypeScheme.label_vars` and re-registered at call sites.

**Union distribution** is the key BAS contribution — `get "port" (A | B)` returns `A.port | B.port`, not `Unknown`.

### BAS Interaction with HKT

BAS operates on types of kind `*`. With HKT:

- `App(f, a)` is a BAS lattice atom for each concrete `(f, a)` pair
- **Covariant functorial subtyping:** `a <: b` implies `App(f, a) <: App(f, b)` for covariant functors (all stdlib Functor instances)
- **Join (one-directional):** `App(m, a) | App(m, b) <: App(m, a | b)` — the reverse is unsound for diagonal functors

`Kind::Label` vars are phantom indices — they do not introduce BAS lattice atoms. `HasField` constraints are resolved eagerly before BAS normalization to prevent S-RcdTop from collapsing union dict types to ⊤.

**References:** Jones, M.P. (1993). "A system of constructor classes." Jones, M.P. (1994). "Qualified types." Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible records." Castagna, G. (2023). "Typing records, maps, and structs." ICFP.

## Limitations and Non-Guarantees

1. **Forward references are monomorphic within letrec.** In letrec dicts, entries that reference later siblings see a fresh type variable (from Pass 1), not the eventually-generalized type scheme. Within the letrec group, mutual references are monomorphic — each entry constrains the others through unification. Polymorphic recursion (Mycroft, 1984) would require fixpoint iteration and is not supported.

2. **Variadic params typed as Unknown.** Variadic parameters (`...args`) are assigned type `Unknown`. Annotations on variadic params are forbidden by design: the runtime collects remaining positional args into an Int-keyed Dict, but row types only describe string-keyed records, so annotations cannot participate in type inference.

3. **Nested dicts do not receive full let-polymorphism.** Only top-level dict entries are generalized in Pass 4 of the DICT-GEN rule. Inner dict entries remain at the outer level and are not independently generalized. For example, in `[outer: [inner: [fn [x] x]]]`, the `inner` entry's function receives the same level as `outer`, not a deeper level, so forward references within the nested dict do not benefit from polymorphic instantiation.
