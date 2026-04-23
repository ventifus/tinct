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
    | Str                        string
    | Bool                       boolean
    | Fn(τ₁...τₙ → τᵣ)          function (n params, return type)
    | Seq(τ)                     lazy sequence
    | Record(f₁:τ₁...fₙ:τₙ, ρ)  record with row rest ρ
    | α                          type variable
    | Any                        dynamic/unknown type

ρ ::= Closed                     no additional fields
    | Open                       arbitrary additional fields
    | RowVar(r)                  named row variable (see [Type System Extensions](07-type-extensions.md) §Row-Variable Unification)
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

```rust
fn check_expr(
    expr: &Spanned<Expr>,
    expected: &Type,
    env: &Rc<TypeEnv>,
    state: &mut InferState,
    type_map: &mut Option<&mut TypeMap>,
) -> Result<(), Vec<TypeError>> {
    let actual = infer_expr(expr, env, state, type_map)?;
    if !Type::is_subtype(&actual, expected) {
        Err(vec![TypeError::type_mismatch(expected, &actual, expr.span)])
    } else {
        Ok(())
    }
}
```

`check_expr` is used only at positions where the expected type is fully concrete (no type variables): CALL-MONO arguments, return annotations, and TypeAssert. For CALL-POLY arguments (where type variables need binding), unification with subsumptive fallback is used instead — see [U-SUBSUME] in the Unification section.

**Checking positions** (expected type fully concrete, uses `check_expr` with [SUB]):

| Position | Expected type | Mechanism |
|----------|--------------|-----------|
| Function arguments (CALL-MONO) | Parameter type | `check_expr` |
| Function body with return annotation | Declared return type | `check_expr` |
| TypeAssert inner expression | Annotated type | `check_expr` |

**Unification positions** (type variables present, uses `unify` with [U-SUBSUME]):

| Position | Expected type | Mechanism |
|----------|--------------|-----------|
| Function arguments (CALL-POLY) | Instantiated param type (has type vars) | `unify` with [U-SUBSUME] fallback |

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
    if variadic (...pᵢ): σᵢ = Record([], Closed)   (see Limitation #5)
    else if annotated pᵢ@σᵢ: use σᵢ
    else: σᵢ = Any
Γ' = Γ, p₁:σ₁, ..., pₙ:σₙ
If return annotation @σᵣ given:
    Γ' ⊢ body ⇐ σᵣ                                  [checking mode]
    use σᵣ as return type.
Else:
    Γ' ⊢ body ⇒ τᵣ                                   [synthesis mode]
────────────────────────────────── [FN]
Γ ⊢ [fn@σᵣ [p₁@σ₁ ... pₙ@σₙ] body] : Fn(σ₁...σₙ → σᵣ)
```

Unannotated non-variadic params get type Any. This is the source of the "Any escape hatch" — without annotations, functions have monomorphic type Fn(Any...Any → τᵣ). Polymorphism requires explicit type variable annotations (e.g., `x@a`).

When a return annotation is present, the body is **checked** against it (⇐ mode): the body is synthesized, then subsumption verifies the inferred type is a subtype of the declared type. This replaces the previous `is_subtype` check with the unified bidirectional mechanism.

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

```
Γ ⊢ f ⇒ Any
────────────────────────────────── [CALL-ANY]
Γ ⊢ [call f a₁...aₙ] ⇒ Any
```

Calling a value typed as Any returns Any. Arguments are still synthesized (for type map population and nested error detection) but not checked against parameter types.

Named arguments are checked against parameter types; `Type::Function` carries parameter names for this purpose — see [Type System Extensions](07-type-extensions.md) §Completeness.

**Access chains:**

```
Γ ⊢ e : Record(... k:τ ..., ρ)
────────────────────────────────── [DOT]
Γ ⊢ e.k : τ

Γ ⊢ e : Record(F, Open | RowVar(_)),  k ∉ F
────────────────────────────────── [DOT-OPEN]
Γ ⊢ e.k : Any

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

Unification finds a most general substitution S such that S(τ₁) = S(τ₂). Before matching, both types are normalized via S (substitution applied). Unification is **pure Robinson** — it handles type variable binding and structural decomposition only. Subtyping (literal promotion, numeric widening) is handled by `check_expr` via the `[SUB]` rule and `is_subtype`. This separation follows Pierce & Turner (2000) and Dunfield & Krishnaswami (2021).

```
unify(τ, τ, S) = S                              [U-REFL]
unify(Any, τ, S) = S                             [U-ANY-L]
unify(τ, Any, S) = S                             [U-ANY-R]
unify(α, τ, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-L]
unify(τ, α, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-R]
```

Literal identity (same literal value = same type):

```
unify(IntLiteral(m), IntLiteral(n), S) =
    S           if m = n                         [U-INTLIT-EQ]
    error       if m ≠ n                         [U-INTLIT-NEQ]

unify(StringLiteral(s), StringLiteral(t), S) =
    S           if s = t                         [U-STRLIT-EQ]
    error       if s ≠ t                         [U-STRLIT-NEQ]
```

No explicit literal-to-parent promotion rules in unification. The previous bidirectional silent coercion rules (`[U-INTLIT-UP]`, `[U-INTLIT-DN]`, `[U-INT-NUM]`, `[U-NUM-INT]`, `[U-FLT-NUM]`, `[U-NUM-FLT]`, `[U-INTLIT-FLT]`, `[U-FLT-INTLIT]`, `[U-STRLIT]`, `[U-STR-STRLIT]`) are all removed. Subtyping relationships between concrete types are handled by [U-SUBSUME] below.

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

[U-SUBSUME] is the bridge between unification and subtyping. It fires after all other rules (structural decomposition, type variable binding) have been tried. When two concrete types remain and they are in a subtype relationship in either direction, unification succeeds without modifying the substitution. This is essential for **confluence in CALL-POLY**: when a type variable α is bound to `IntLiteral(42)` by one argument and later compared against `Int` by another (via substitution resolution), [U-SUBSUME] recognizes `IntLiteral(42) <: Int` and succeeds regardless of argument order.

**Relationship to Robinson unification.** Robinson (1965) is purely syntactic — it has no notion of subtyping, so `unify(IntLiteral(42), Int)` would simply fail (different constructors). [U-SUBSUME] extends Robinson with a ground-type compatibility check: when both sides are concrete and in a subtype relationship, unification succeeds without modifying the substitution. This is a pragmatic middle ground — Robinson handles structural decomposition and variable binding; [U-SUBSUME] handles the subtype lattice at ground types. The substitution is never modified by [U-SUBSUME], so existing variable bindings (which may carry literal precision) are preserved. This is the same approach Rust's type inference uses: subtyping constraints between concrete types are resolved as compatibility checks rather than LUB computation (Dolan & Mycroft 2017 describe the full alternative — algebraic subtyping — which tinct intentionally does not adopt; see `doc/whatif/algebraic-subtypes.md`).

[U-SUBSUME] checks both directions because unification is symmetric — the two types arrive without a designated "actual" vs "expected" role. The bidirectional check covers both orderings: `unify(IntLiteral(42), Int)` succeeds (IntLiteral(42) <: Int) and `unify(Int, IntLiteral(42))` also succeeds (IntLiteral(42) <: Int, checked as `is_subtype(τ, σ)`). The substitution is unchanged because there are no type variables to bind.

**Interaction with [SUB]:** At CALL-MONO sites (fully concrete types, no unification needed), `check_expr` uses directional subsumption via `is_subtype(actual, expected)` — only the correct direction is checked. [U-SUBSUME] is bidirectional because it operates within unification where the original directionality is lost after structural decomposition. This is sound because the substitution is not modified — the bidirectional check only determines compatibility, not binding direction.

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
    If ρ₂ = Closed: keys(F₁) ⊆ keys(F₂)
        (combined with width condition, enforces exact key equality)
    If ρ₂ = Open | RowVar: always ok              [S-REC]

Fn(p₁...pₙ→r₁) <: Fn(q₁...qₙ→r₂) if:
    |p| = |q|
    qᵢ <: pᵢ  for all i                          (contravariant params)
    r₁ <: r₂                                      (covariant return)
                                                 [S-FN]
```

**Note on [S-ANY-TOP] and [S-ANY-BOT]:** Having Any as both the top and bottom of the type lattice violates antisymmetry (τ <: σ ∧ σ <: τ ⇒ τ = σ) and makes the subtype relation unsound as a partial order. This is intentional for tinct's gradual type system — Any marks the boundary between typed and untyped code (see Limitation #3).

## Instantiation

```
instantiate(τ) = (S(τ), S)
    where S = {α₁ ↦ _t0, α₂ ↦ _t1, ...}
    for each αᵢ ∈ FTV(τ), fresh names _tN generated
    from a monotonic per-file counter.

FTV(τ) includes both type variables (α) and row variables
(RowVar(r)). Tinct conflates these into a single namespace —
both are collected by collect_type_vars() and renamed by
instantiate(). In Rémy (1994), row variables inhabit a
distinct kind from type variables; tinct does not enforce
this distinction.
```

This is alpha-renaming for call-site freshening. Each polymorphic call site gets independent type variables so unification at one site does not constrain another. With let-generalization (below), instantiation also handles let-bound polymorphic type schemes.

## Let-Generalization (Levels-Based)

Tinct uses levels-based let-generalization following Kiselyov (2013) to support polymorphic let-bindings. This extends annotation-driven polymorphism with automatic generalization at dict entry boundaries.

**Type schemes.** The type environment Γ maps names to *type schemes* σ rather than bare types τ:

```
σ ::= ∀α₁...αₙ. τ    (n ≥ 0; when n = 0, equivalent to monomorphic τ)
```

Implementation: `TypeEnv.bindings` changes from `IndexMap<String, Type>` to `IndexMap<String, TypeScheme>`. The `TypeScheme` struct:

```rust
#[derive(Debug, Clone)]
pub struct TypeScheme {
    pub vars: Vec<String>,  // quantified variable names
    pub body: Type,
}

impl TypeScheme {
    pub fn mono(ty: Type) -> Self {
        Self { vars: vec![], body: ty }
    }
}
```

`PartialEq` for `TypeScheme` compares structurally (vars + body). `Display` shows `∀a b. Fn(a → b)` for polymorphic schemes, or the bare type for monomorphic ones. Located in `types.rs`.

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
}
```

When a `TypeVar(name, lvl)` is created, `levels[name] = lvl` is recorded. During unification, level lowering mutates `levels[name]` without rebuilding the `Type`. `generalize()` consults `levels` for the authoritative level of each variable. The level embedded in `TypeVar(String, u32)` is the *creation-time* level; `InferState.levels` is the *current* (possibly lowered) level.

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

Both rules lower levels symmetrically: when binding α to τ, every type variable β inside τ has its level lowered to `min(ℓ(β), ℓ(α))`. This prevents variables from escaping their scope through either side of a unification.

**Any-unification and generalization.** When a type variable α is unified with `Any`, the current [U-ANY] rules succeed without binding α. To prevent incorrect generalization of the unbound α, `unify(α, Any)` sets `ℓ(α) = 0` (below all binding levels):

```
unify(α, Any, S) = S,  set ℓ(α) = 0               [U-ANY-VAR]
unify(Any, α, S) = S,  set ℓ(α) = 0               [U-VAR-ANY]
```

This ensures Any-touched variables are never generalized (since `ℓ(α) = 0` is never `> ℓ` for any binding level). The variable remains free and resolves to its eventual binding (if any) or stays unconstrained.

**Generalization.** At a dict boundary at level ℓ, after all entries in the letrec group are inferred:

```
generalize(ℓ, τ) = ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ     [GEN]
```

where ℓ(α) is read from `InferState.levels[α]` (the current, possibly lowered level). Type variables whose level exceeds the enclosing scope's level are local to the binding and can be universally quantified. Variables at or below level ℓ are free in the enclosing scope and must remain monomorphic. Row variables participate identically — `RowVar(r, _)` with `levels[r] > ℓ` is generalized.

Implementation signature:

```rust
pub fn generalize(level: u32, ty: &Type, state: &InferState) -> TypeScheme
```

Collects FTV(ty) via a level-aware traversal returning `Vec<(String, u32)>` pairs, filters by `current_level > level`, returns `TypeScheme { vars, body: ty.clone() }`.

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
Pass 4 — Generalize (NEW): for each entry kᵢ,
         σᵢ = generalize(ℓ, S(αᵢ), state)
         Update Γ'(kᵢ) = σᵢ
Build Record(k₁:S(α₁)...kₙ:S(αₙ), Closed).
────────────────────────────────── [DICT-GEN]
```

The Record type uses monomorphic (substitution-applied) types for the type map and downstream structural checks. The type schemes σᵢ live in Γ and are instantiated at each reference via [VAR-POLY].

**Nested dicts increment levels.** Each `infer_dict` call increments ℓ_current. For `[a: [b: 42]]`, the outer dict runs at ℓ+1 and the inner dict at ℓ+2. This matches standard HM let-nesting: each `let` increments the level.

**Forward references within letrec.** Within a single dict (letrec group), all entries share level ℓ+1 during Pass 3 inference. Forward references see the monomorphic αᵢ from Pass 1 — these are fresh type variables that participate in unification, producing binding constraints. This is more precise than the previous behavior (binding to `Any`): forward references now produce real type constraints rather than silently succeeding. After Pass 4, downstream consumers of the dict see polymorphic schemes.

Mutually recursive entries constrain each other through unification during Pass 3. This is standard monomorphic letrec (OCaml, Haskell `let rec`) — entries see each other as monomorphic during inference, not polymorphic. Polymorphic recursion (Mycroft 1984) is not supported: it would require fixpoint iteration to convergence, which is more complex and can diverge. The monomorphic restriction is sufficient for tinct's use cases.

**Document-level scheme threading.** `typecheck_document` splats dict Record fields into the parent environment for downstream document expressions. To preserve polymorphism across `---` boundaries, the splat must carry type schemes alongside the Record type. Implementation: `infer_dict` returns both the `Record` type (for structural checks) and an `IndexMap<String, TypeScheme>` (for environment threading). `typecheck_document` inserts the schemes into the parent `TypeEnv`.

**Interaction with `Any` and unannotated parameters:**

- Unannotated function parameters still receive type `Any` (not a fresh type variable). `[fn [x] $x]` remains `Fn(Any → Any)`.
- `Any` in unification acts as a universal match ([U-ANY-L], [U-ANY-R]) but sets ℓ(α) = 0 for any type variable α it touches ([U-ANY-VAR], [U-VAR-ANY]), preventing generalization.
- Annotated type variables (e.g., `x@a`) create fresh type variables at ℓ_current. These participate in generalization normally.
- The practical effect: let-generalization benefits code that uses type annotations. `[id: [fn [x@a] $x]]` generalizes `id` to `∀a. Fn(a → a)`; subsequent `[call $id 42]` and `[call $id "hello"]` each get independent instantiations.

**Interaction with CALL-POLY.** VAR-POLY instantiates type schemes at reference sites. For call expressions, the instantiated type typically has no remaining type variables (the fresh `_tN` variables are monomorphic instances), so CALL-POLY sees `has_type_vars() = false` and takes the CALL-MONO fast path. Double instantiation only occurs when a polymorphic function *returns* a polymorphic function — rare in practice. No optimization needed for the common case.

**Substitution name uniqueness.** `Substitution::map` is keyed by variable name. User-annotated type variables (e.g., `@a`) are not globally unique, but `instantiate_scheme()` renames them to fresh `_tN` names before any substitution sharing occurs. Within a single letrec group during Pass 3, each entry's annotation-derived variables are instantiated independently, preventing collision.

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
| `TypeEnv.bindings` | `IndexMap<String, TypeScheme>` |
| `TypeEnv.type_aliases` | `IndexMap<String, Type>` — aliases stay monomorphic |
| `TypeEnv::get()` | Returns `&TypeScheme` |
| `TypeEnv::insert_scheme()` | `fn(name, TypeScheme)` |
| `infer_expr` VAR case | `instantiate_scheme(env.get(name)?, ...)` |
| `infer_dict` | 5 passes (0-4), bind to fresh αᵢ, generalize in Pass 4 |
| `infer_dict` return | `(Type, IndexMap<String, TypeScheme>)` |
| `typecheck_document` | Splats `TypeScheme`s into parent env across `---` boundaries |
| `instantiate()` | `fn(Type, &mut u32) → (Type, Subst)` — for CALL-POLY call-site freshening |
| `instantiate_scheme()` | `fn(TypeScheme, u32, &mut InferState) → Type` |
| `generalize()` | `fn(u32, Type, &InferState) → TypeScheme` |
| `unify()` U-VAR | Bind + symmetric level lowering |
| `unify()` U-ANY + TypeVar | Set ℓ(α) = 0 to prevent generalization |
| `InferState` | `{ name_counter: u32, level: u32, levels: HashMap<String, u32> }` |
| `collect_type_vars()` | Returns `BTreeSet<(String, u32)>` (name + level) |
| `Type::Display` | Shows `TypeVar` name only (level hidden) |

Polymorphic builtin signatures (e.g., `map: ∀a b. Fn(Fn(a → b) × Seq(a) → Seq(b))`) are expressed via type schemes — see [Type System Extensions](07-type-extensions.md).

**Principal types.** Tinct infers principal types for fully-annotated polymorphic functions where no type variable unifies with `Any`. For partially-typed code, the inferred type depends on the checking context — subsumption introduces multiple valid types for the same expression (e.g., `42` can check against `IntLiteral(42)`, `Int`, `Number`, or `Any`). Full Damas-Milner principality is not achieved because: (a) unannotated parameters receive `Any` rather than fresh type variables, (b) singleton literal types introduce subtyping which bidirectional checking mediates but which prevents a unique most-general type, and (c) [U-SUBSUME] in CALL-POLY means the type variable binding may be more or less precise depending on argument order (both bindings are sound, but they differ).

**References:** Kiselyov, O. (2013). "How OCaml type checker works — or what polymorphism and garbage collection have in common." Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." Wright, A. (1995). "Simple imperative polymorphism."

## Limitations and Non-Guarantees

1. **Literal promotion handled by bidirectional checking (not unification).** Literal-to-parent type compatibility (e.g., `IntLiteral(42) <: Int`) is handled exclusively by `is_subtype` in checking mode via the [SUB] rule — see §Bidirectional Typing. Unification is pure Robinson: `unify(IntLiteral(42), Int)` fails because these are distinct types. The previous bidirectional silent coercion rules have been removed. This preserves type precision and properly separates subtyping from unification (Pierce & Turner 2000).

2. **Named args not unified.** Named arguments in `[call ...]` are type-checked (values inferred) but not unified against function parameter types. Requires extending `Type::Function` to carry parameter names.

3. **Any is both top and bottom.** `Any <: τ` and `τ <: Any` for all τ. In subtyping theory, this makes `Any` simultaneously the top and bottom element of the type lattice, which is unsound in general. In tinct's advisory type system this is intentional — `Any` marks the boundary between typed and untyped code, and `[@Type expr]` is the explicit narrowing mechanism.

4. **Forward references are monomorphic within letrec.** In letrec dicts, entries that reference later siblings see a fresh type variable (from Pass 1), not the eventually-generalized type scheme. Within the letrec group, mutual references are monomorphic — each entry constrains the others through unification. Polymorphic recursion (Mycroft, 1984) would require fixpoint iteration and is not supported.

5. **Variadic params typed as closed empty record.** Variadic parameters (`...args`) are assigned type `Record([], Closed)` but should be `Any`. Annotations on variadic params are forbidden by design: the runtime collects remaining positional args into an Int-keyed Dict, but row types only describe string-keyed records, so annotations cannot participate in type inference. When `Seq` types are used for variadic collection, variadic params may collect into a typed `Seq<T>` instead.
