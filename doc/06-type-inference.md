# Type Inference

This chapter formally specifies tinct's type inference algorithm. The notation uses standard PL conventions: Γ for type environments, ⊢ for typing judgments, S for substitutions, and τ, σ for types.

For the user-facing annotation syntax (`@`, type assertions, type expressions), see [Type Annotations](05-type-annotations.md). For BAS (Boolean-Algebraic Subtyping), TypeAssert runtime validation, column constraints, and equirecursive types, see [Type System Extensions](07-type-extensions.md).

## Type Representation

S-1003 completed the migration to `Arc<Value>` TypeValues as the primary type representation. The `Type` Rust enum (formerly in `src/type_def.rs`) was deleted. All type checker files now use `Arc<Value>` TypeValues with `TV_*` ctor tag constants from `src/type_tags.rs`. The `TypeAnnotationTable` side-table was also deleted (T-1999); type annotation results are stored inline as `OnceLock<Option<Arc<Value>>>` on AST nodes.

**TypeValue = `Arc<Value>`.** A TypeValue is an `Arc<Value::Variant { ctor, payload }>` where `ctor` is a tag constant (e.g., `TV_REPR`, `TV_UNION`, `TV_FN`, `TV_FLOAT_LIT`). The type checker inspects TypeValues by calling `typevalue_ctor()` and matching on these constants, rather than pattern-matching on a Rust enum. Construction helpers (`make_typevalue_repr`, `make_typevalue_union`, `make_typevalue_fn`, etc.) in `src/type_infer.rs` produce correctly structured TypeValues.

**Inference variables are `TypeValue.Var` TypeValues.** `fresh_typevar()` produces a `TV_VAR`-tagged TypeValue with a fresh name (prefix + gensym counter). The level is NOT stored in the TypeValue payload — `TypeValue.Var` carries identity only (the variable's name). `ctx.levels` maps TypeVar name → current (possibly lowered) level; `ctx.levels` is authoritative. The level is registered in `ctx.levels` at creation time via `ctx.fresh_typevar()`.

**InferenceContext replaced the old substitution model.** `state.ctx.subst` is a `HashMap<String, Arc<Value>>` (TypeVar name → TypeValue). `InferenceContext` (in `src/type_infer.rs`) holds `subst`, `levels`, `tycon_env`, `current_level`, `gensym_counter` (private), and `resolver_deferred`. `InferState` owns a `pub ctx: InferenceContext` field; all substitution and level operations go through `state.ctx`.

**TypeValue variants available to the type checker** (representative set; see `src/type_tags.rs` for all `TV_*` constants):

| TypeValue tag | Notes |
|---------|-------|
| `TV_REPR` | leaf primitive (payload: `repr` string — `"Value::Int"`, `"Value::Float"`, etc.) |
| `TV_UNION` | union A \| B (payload: `members` indexed dict) |
| `TV_INTER` | intersection A & B (payload: `members` indexed dict) |
| `TV_NEG` | complement ~A (payload: `of` TypeValue) |
| `TV_FN` | function type (payload: `params` indexed dict, `return` TypeValue, `param-names` indexed dict (optional — param name strings by index), `variadic` `"true"` Variant or absent, `typed-variadics` integer-keyed dict of `{ name: String, ty: TypeValue }` entries for typed variadic buckets (`...xs@Seq[Integer]`); absent when empty (B-671)) |
| `TV_OP` | type constructor name (payload: `name` string) |
| `TV_APP` | type constructor application (payload: `op` TypeValue, `arg` TypeValue) |
| `TV_RECURSIVE` | equirecursive type μvar.τ (payload: `body` TypeValue) |
| `TV_VAR` | inference variable (payload: `name` string) |
| `TV_INT_LIT` | integer literal type (payload: `value` Int) |
| `TV_STR_LIT` | string literal type (payload: `value` String) |
| `TV_FLOAT_LIT` | float literal type (payload: `value` Float) |
| `TV_UNKNOWN` | gradual typing escape hatch (no payload) |
| `TV_TOP` | universal supertype ⊤ (no payload) |
| `TV_NEVER` | bottom type ⊥ (no payload) |

`state.ctx.subst` maps TypeVar name `String → Arc<Value>`. `ctx.fresh_typevar(prefix)` creates a `TV_VAR` TypeValue with a fresh name and registers it in `ctx.levels` at the current level. Substitution lookup: `state.ctx.subst.get(typevar_name)`.

## Type Grammar

```text
τ ::= IntLiteral(n)              literal integer type
    | StringLiteral(s)           literal string type
    | Int                        integer
    | Float                      float
    | Number                     numeric supertype of Int and Float
    | Str                        string  (internal name; user-facing annotations accept `String` as an alias)
    | Bool                       boolean
    | Fn(τ₁...τₙ → τᵣ)          function (n params, return type)
    | TyCon(name)                concrete type constructor name ("Seq", "Tree", "Result", etc.)
    | App(τ, τ)                  curried type constructor application — left-to-right
    | Record(f₁:τ₁...fₙ:τₙ, tail)  closed record with optional uniform tail
    | Proxy                      opaque proxy (field access dispatches to handler)
    | α                          type variable  (`TV_VAR`-tagged TypeValue; level tracked separately in `ctx.levels`)
    | Unknown                    gradual typing escape hatch (don't know the type)
    | Top                        universal supertype ⊤ (supertype of everything)
    | Union(τ₁...τₙ)             union type A | B (user-expressible via `@[or A B]`)
    | Intersection(τ₁...τₙ)      intersection type A & B (user-expressible via `@[[all A B]]`)
    | Negation(τ)                negation type ~A (user-expressible via `@[[without A]]`)
    | Never                      bottom type ⊥ (uninhabited, user-expressible via `@Never`)
    | Recursive { body: τ }      equirecursive type μ.τ, de Bruijn indexed  (TypeNode.Recursive)
    | RecursiveRef { depth: n }  back-reference at de Bruijn depth n inside a Recursive body  (TypeNode.RecursiveRef)
```

`TyCon(name)` replaces the dedicated collection variants `Seq(τ)`, `Map(K,V)`, and `Handle(τ)`. All named types — builtins and user-declared — are represented uniformly:

```text
Seq Int     → App(TyCon("Seq"), Int)   # TV_APP{ op: TV_OP{name:"Seq"}, arg: TV_REPR{repr:"Value::Int"} }
Map Str Int → App(App(TyCon("Map"), Str), Int)   # curried: left-to-right
Tree a      → App(TyCon("Tree"), Var("a"))        # level tracked in ctx.levels, not in the Var payload
```

`TV_OP` (type constructor variables) is used for class params like `f` in `[class [f@Operator] ...]`; `TV_OP` with a concrete name is the equivalent of the former `TyCon` leaf.

*Note:* Under BAS (Boolean-Algebraic Subtyping), all records are closed. Records carry no row-rest parameter. Width subtyping handles record openness via intersection and negation. The historical Rémy-style row polymorphism design (with closed/uniform record tails) is documented in the archived section of [Type System Extensions](07-type-extensions.md).

**Additional types** (not expressible in annotations, used internally by inference):

| Type | Description |
|------|-------------|
| `Bytes` | Binary data |
| `Timestamp`, `Duration`, `Timezone` | Datetime types |
| `DirCap`, `NetCap`, `ClockCap` | Capability types (runtime-only) |
| `Uri` | URI resource type (runtime-only) |
| `QuicSession`, `Http2Session`, `Http3Session`, `DatagramHandle` | Network session types (runtime-only) |

## TyCon Registry

**`TyConDef`** is the unified type constructor store. All named type constructors — builtins, structural aliases, and nominal ADTs — are registered in `tycon_defs: HashMap<String, Arc<TyConDef>>` (B-343 implemented in S-856). `TyConDef` holds params, body (TypeValue), constraints, variance, and constructor metadata for subtyping and coverage. The separate `TypeAlias` map was deleted in S-1003.

```rust
pub struct TyConDef {
    /// Type parameter names (e.g., ["a", "k", "v"]). Empty for zero-parameter types.
    pub params: Vec<String>,

    /// Type body as a TypeValue (Arc<Value>). For structural aliases, this is the expanded
    /// TypeValue; for nominal ADTs, a TypeValue.Union of NominalVariants.
    /// During bootstrap, holds `unknown_type_val()` as a placeholder.
    pub body: Arc<Value>,  // S-1003: was Type pre-migration

    /// Class constraints on type parameters, populated when params carry `@ClassName` annotations.
    /// Empty for unconstrained aliases. Arc<Value> ConstraintDecl entries.
    pub constraints: Vec<Arc<Value>>,

    /// Variance per type parameter, in declaration order.
    /// Length equals the number of declared `[let ...]` params.
    /// Empty for zero-parameter types (primitives, unit ADTs, builtin-opaque types).
    pub variance: Vec<Variance>,

    /// Constructors for nominal ADTs: (qualified_tag, payload_arity).
    /// e.g., [("Color.Red", 0), ("Color.Green", 0)].
    /// Empty for structural aliases and builtin-opaque types.
    pub constructors: Vec<(String, usize)>,

    /// Builtin-type discriminant (e.g., "Seq", "Map", "Int").
    /// `Some` → builtin-opaque type; `None` → user-declared or structural alias.
    pub builtin_type: Option<String>,

    // Additional fields: `annotation`, `field_annotations`, `constructor_constants`,
    // `definition_span`. See src/type_def.rs for the complete definition.
}

pub enum Variance { Covariant, Contravariant, Invariant, Phantom }
```

`TypeEnv` carries `tycon_defs: HashMap<String, Arc<TyConDef>>` with `insert_tycon_def(name, Arc<TyConDef>)` and `lookup_tycon_def(name)` methods (parent-chain walk). When the type checker processes `Color: [type Red Green Blue]`, it stores a `TyConDef` entry (for variance and constructor metadata) in both the scoped `TypeEnv` and the flat `InferState.tycon_env`.

**Two population sites:**

1. **`register_type_aliases` in `src/typecheck.rs`** — called during dict type-checking passes (Pass 1 / Pass 3). Processes each `[type ...]` entry: builtin-type declarations insert `TyConDef { builtin_type: Some(discriminant), constructors: [], variance: [] }`; nominal ADTs insert `TyConDef { constructors: [(qualified_tag, arity), ...], variance: inferred_or_declared, builtin_type: None }`. Both `target_env.insert_tycon_def` (scoped TypeEnv) and `state.tycon_env.insert` (flat InferState accumulator) are updated together.

2. **`InferState::new()` in `src/type_infer.rs`** — seeds builtin TyCons in `tycon_env` at inference-state creation time, before any user code is processed. Currently seeds: `Seq` (1 param, Covariant), `Map` (2 params, [Invariant, Covariant]), `Handle` (1 param, Covariant). All three have `builtin_type: Some(name)` set. This enables `is_subtype` to apply variance-directed subtyping for builtin parameterized types.

**`InferState` flat accumulator.** `InferState` carries `pub tycon_env: HashMap<String, Arc<TyConDef>>` as a flat snapshot populated incrementally as each TyCon declaration is processed. This is transferred to `EvalContext` at all infer-state transfer sites via `ctx.set_tycon_env(infer_state.tycon_env)`. `EvalContext.tycon_env` is a `OnceLock<Arc<TyConEnv>>` — populated exactly once, never mutated afterward.

**Type identity.** UNIFY-TYCON currently unifies `TyCon(n1)` and `TyCon(n2)` iff `n1 == n2` (name equality). `TyConEnv` uses `Arc<TyConDef>` (migrated in B-343, S-856) so that pointer-identity-based cross-scope rejection is structurally possible. However, because UNIFY-TYCON is only reached when `n1 == n2`, both lookups hit the same HashMap slot and return the same Arc — `Arc::ptr_eq` is scaffolding for the future `TypeEnv`-threaded TyCon lookup, tracked as T-1112. Until then, name equality is the operative identity check.

**Kind registration.** TyCon kind is derived from `TyConDef.variance.len()`:

- 0 parameters → `*` (type)
- 1 parameter → `* → *` (type operator)
- 2+ parameters → `* → * → *` chain, etc.

The `Kind` Rust enum was deleted in T-1995. Kind information is now encoded implicitly via `TyConDef.variance.len()`. `state.ctx.kind_env` is a `HashMap<String, Arc<Value>>` tracking TypeVar kinds (`f@Operator` in class params).

## Annotation Resolution

**`resolve_annotation`** and its helper `resolve_type_dict` receive a `type_params: Option<&HashSet<String>>` argument:

- `Some(params)` — inside a type alias body. Only names in `params` become TypeVars; all other lowercase names are scope references. If a name is not in params and not found in scope, it is a type error.
- `None` — outside a type alias body (function annotations, constraints, etc.). Existing behavior: lowercase names become TypeVars.

`apply_builtin_constructor` has been deleted; `is_builtin_type_name` was deleted in T-1113 and both call sites now use `TyConEnv` lookup (`state.tycon_env.get(name).map_or(false, |def| def.builtin_type.is_some())`). The general lookup path — look up the name in `TyConEnv`, retrieve arity, collect arguments, produce `App(TyCon(name), args...)` or expand the alias body — is implemented.

**`{_ : V}` recognition (implemented in S-843).** When parsing a type dict expression in `resolve_type_dict` (`src/typecheck_annot.rs`), the `_` key (optionally annotated `_@K`) is recognized as a uniform tail rather than a named field. The recognizer is a single pass: accumulate named fields; when a key is `_` (plain) or an annotated `_@K` form, a uniform-tail TypeValue variant is produced. At most one `_` per row type — a duplicate raises "duplicate uniform-field sentinel `_` in row type annotation".

**Polarity analysis** for transparent alias variance inference: `infer_variance(body: &TypeValue, params: &[String], type_env: &TypeEnv) -> Vec<Variance>` implements Dolan 2017 §4. Walk the body TypeValue with a current polarity (`Positive`/`Negative`); classify each TypeVar's occurrences to determine `Covariant`, `Contravariant`, `Invariant`, or `Phantom`. Explicit `@` annotations override inference and are checked for conflicts.

**`annotation_to_variance`** — a closed 4-entry match used when processing `[let a@X]` params:

```rust
fn annotation_to_variance(name: &str) -> Option<Variance> {
    match name {
        "Covariant"     => Some(Variance::Covariant),
        "Contravariant" => Some(Variance::Contravariant),
        "Invariant"     => Some(Variance::Invariant),
        "Phantom"       => Some(Variance::Phantom),
        _               => None,
    }
}
```

If `annotation_to_variance` returns `None` AND the name is a registered class, the annotation is a typeclass constraint on the type parameter. If neither, it is a type error.

**Literal types in annotation position (T-1885).** Integer and string literal values are valid in type annotation position and resolve to singleton literal types:

- `@0` → `TV_INT_LIT`-tagged TypeValue with payload `{ value: 0 }`
- `@"foo"` → `TV_STR_LIT`-tagged TypeValue with payload `{ value: "foo" }`
- `@[or 0 1]` → `TV_UNION`-tagged TypeValue with members `[TV_INT_LIT{0}, TV_INT_LIT{1}]`

This enables precise return type annotations for boolean-result builtins. The comparison and equality builtins (`builtin-eq-int`, `builtin-eq-string`, `builtin-lt`, `builtin-lte`, `builtin-gt`, `builtin-gte`) declare return type `@[or 0 1]` rather than `@Integer`, so callers can use concrete `0:` / `1:` arms in `match` expressions and the type checker accepts them without a catch-all.

Implementation: `resolve_type_name` in `src/typecheck_annot.rs` recognizes `SurfaceExpression::Int(n)` and `SurfaceExpression::Str(s)` in annotation position and produces the corresponding `TV_INT_LIT` / `TV_STR_LIT` TypeValues directly.

## Unification: UNIFY-TYCON and UNIFY-UNIFORM

**UNIFY-TYCON:** `TyCon(n1)` and `TyCon(n2)` unify iff `n1 == n2`. Name-equality is the operative check; `Arc::ptr_eq` is also called via `tycon_env` lookup (B-343). No binding is produced — UNIFY-TYCON is a pure equality check with no substitution side-effects.

**UNIFY-TYCON-EXPAND:** `TV_OP{name:n}` unified with a `TV_APP`-rooted nominal variant TypeValue (T-1112). When a zero-arity type operator (e.g., `@Color`) is unified against a value whose type is a nominal variant TypeValue with tag `"Color.Red"`, the unifier looks up `n` in `state.ctx.tycon_env`, retrieves the registered body (the Union of nominal variant TypeValues for the type declaration), and checks membership via `is_subtype`. This enables `@Color` annotations to accept any constructed variant of `Color`. If the TyCon has no registered body (unknown or builtin opaque type), unification fails with a type mismatch. Both `(TV_OP, nominal variant)` and `(nominal variant, TV_OP)` directions are handled symmetrically.

**UNIFY-APP:** Decomposes `App(f1, a1)` and `App(f2, a2)` by first unifying constructors (`f1 ~ f2`, which dispatches to UNIFY-TYCON for `TyCon` heads), then unifying arguments (`a1 ~ a2`).

**UNIFY-UNIFORM:** Two row tail cases are handled:

- `unify(Uniform{K1?, V1}, Uniform{K2?, V2})` → `unify(V1, V2)` (and `unify(K1, K2)` if both keyed). **Implemented.**
- `unify(Empty, Uniform{K?, V})` or symmetric: **implemented (T-1024, S-856)**. Apply substitution to `V` to get `V'`, then collect named field types from both rows; (a) if `V'` is an unbound TypeVar α, unify α with the join (`T1 | T2 | ... | Tn`) of all named fields from both rows; (b) if `V'` is concrete, check `is_subtype(Ti, V')` for each named field from both rows; (c) if `V'` has inference vars but is not a bare TypeVar, defer. If neither row has named fields the case succeeds immediately (any Uniform is compatible with no named fields).

## Subtyping: Variance-Directed App

**`is_subtype` signature:** `is_subtype(sub: &TypeValue, sup: &TypeValue, tycon_env: Option<&TyConEnv>) -> bool`.

- `None`: invariant fallback for all `App(TyCon(_), _)` — safe conservative default (never unsound).
- `Some(&state.tycon_env)`: all type-checker call sites.
- `Some` from `EvalContext.tycon_env`: runtime call sites.

**Variance-directed subtyping for `App(TyCon(f), a)` — implemented:**

- `@Covariant`: `App(f, a) <: App(f, b)` when `a <: b`
- `@Contravariant`: `App(f, a) <: App(f, b)` when `b <: a`
- Invariant (default): only when `a = b`
- `@Phantom`: always

The variance lookup uses `TyCon` name string to find `TyConDef.variance[i]` in `tycon_env`. For user-declared types, `tycon_env` is populated by `register_type_aliases`; for builtins, by `InferState::new()`. `TyConEnv` uses `Arc<TyConDef>` (B-343 implemented in S-856), so cross-scope TyCon identity via `Arc::ptr_eq` is structurally in place. Name-equality is the operative identity check for `TV_OP` nodes; `Arc::ptr_eq` on `TyConDef` bodies is available but not yet used for cross-scope TyCon identity (tracked separately).

## Scoped ClassEnv and InstanceEnv

Both `ClassEnv` and `InstanceEnv` are parent-chain scoped, following the same model as `TypeEnv`:

- A `HashMap` per scope frame with a parent pointer
- Insertions go into the current frame; lookups walk the chain with inner-wins semantics
- Prelude classes and instances live in the root frame — visible everywhere
- A class or instance in an inner dict is visible only to that dict's descendants

Local coherence: within a single scope frame, at most one instance per `(Class, Type)` pair. Across scope levels, shadowing is allowed — the innermost instance wins. Two `[instance [Monad Result] ...]` in the same dict is a type error; one in an outer scope and one in an inner scope is valid (inner shadows).

**`[do ...]` monad dispatch** uses the scoped instance environment at typecheck time via `resolve_monad_from_surface(node, type_env)`. This function calls `flatten_dot_access_to_tag` for dot-access-headed calls and `type_env.resolve_constructor_tag(name)` for VarRef-headed calls to obtain the qualified tag, then extracts the TyCon name via `rfind('.')`. Lookup goes through `state.instance_env.lookup_scoped("Monad", tycon_name)` — not the global eval-time registry. The desugared `[do ...]` body embeds `bind_node` and `pure_node` references directly as expressions — no runtime instance lookup.

## Bidirectional Typing

Tinct uses bidirectional type checking (Pierce & Turner 2000; Dunfield & Krishnaswami 2021) to cleanly separate type inference from subtyping. Two modes:

- **Synthesis (⇒):** `Γ ⊢ e ⇒ τ` — infer the type of e bottom-up (what `infer_expr` does today).
- **Checking (⇐):** `Γ ⊢ e ⇐ τ` — verify e is compatible with expected type τ, using subsumption.

The **subsumption rule** bridges them:

```text
Γ ⊢ e ⇒ σ,  σ <: τ
────────────────────────────────── [SUB]
Γ ⊢ e ⇐ τ
```

If an expression synthesizes type σ and σ is a subtype of the expected type τ, then checking succeeds. This is where singleton literal type promotion happens: `42 ⇒ IntLiteral(42)`, and `IntLiteral(42) <: Int`, so `42 ⇐ Int` succeeds. But `Int ≮: IntLiteral(42)`, so checking an `Int`-typed expression against `IntLiteral(42)` fails. Direction matters — subtyping is asymmetric by design.

Note: tinct's `IntLiteral(42)` and `StringLiteral("hello")` are **singleton literal types** — distinct types that are subtypes of their base types (`Int`, `Str`). These are not refinement types in the Dunfield & Pfenning (2004) sense (which use predicate logic, e.g., `{x: Int | x = 42}`). The singleton type approach is simpler and sufficient for tinct's needs; D&P's framework validates that subtyping with type refinements is sound in a bidirectional setting.

Implementation:

**Note:** The complete `check_expr` implementation handles lambda checking mode (propagating expected parameter types to unannotated lambda parameters when the expected type is fully concrete), annotated parameter type resolution, checking against expected return annotations, and subsumption via `[U-SUBSUME]`. See `src/typecheck.rs` for the full implementation (search for `fn check_expr`).

`check_expr` is used for both CALL-MONO and CALL-POLY argument checking. When the expected type contains type variables (CALL-POLY), `check_expr` internally dispatches to unification to bind them. When the expected type is fully concrete (CALL-MONO), it uses subsumption checking. See [U-SUBSUME] in the Unification section.

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

**Confluence.** Both CALL-MONO and CALL-POLY use `check_expr`, which internally dispatches to unification when type variables are present (CALL-POLY) or to subsumption when the expected type is fully concrete (CALL-MONO). When unification resolves a type variable to a concrete type, subsequent unification attempts against that concrete type use [U-SUBSUME] — a unidirectional subsumption fallback that checks `is_subtype(σ, τ)`. Confluence is ensured by call sites establishing the correct sub/sup direction before calling unify(). See the Unification section for details.

## Inference Judgments: Γ ⊢ e ⇒ τ

**Literals:**

```text
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

```text
Γ(x) = ∀α₁...αₙ. τ
τ' = instantiate_scheme_tv(∀α₁...αₙ. τ, ℓ_current)
────────────────────────────────── [VAR-POLY]
Γ ⊢ x : τ'
```

Each variable reference instantiates its type scheme with fresh variables at ℓ_current. When n = 0, this returns the body directly (monomorphic binding — no allocation).

**Dict (letrec with generalization):**

Dicts are inferred in five sequential passes using the [DICT-GEN] rule — see §Let-Generalization (Levels-Based) for the full specification. The rule uses fresh type variables (not `Any`) for forward references and generalizes entry types after inference.

**Function definition:**

```text
For each param pᵢ:
    if variadic (...pᵢ): β fresh; σᵢ = Seq(β)           [FN-VARIADIC]
    else if annotated pᵢ@σᵢ: use σᵢ
    else: σᵢ = fresh TypeVar at current level
Γ' = Γ, p₁:σ₁, ..., pₙ:σₙ
If return annotation @σᵣ given:
    if has_inference_vars(σᵣ):
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

```text
Γ ⊢ [fn@σᵣ [p₁@τ₁ ... pₙ@τₙ] body] ⇐ Fn(σ₁...σₙ → σ_exp)
    where ¬has_inference_vars(Fn(σ₁...σₙ → σ_exp))       (expected type fully concrete)
For each param pᵢ:
    if variadic: β fresh; use Seq(β)
    else if annotated pᵢ@τᵢ:
        if has_inference_vars(τᵢ): unify(σᵢ, τᵢ, S)       (annotation with TypeVars)
        else: check σᵢ <: τᵢ                         (contravariant check)
        use τᵢ
    else: use σᵢ                                     (propagate expected type)
Γ' = Γ, p₁:τ₁, ..., pₙ:τₙ
If return annotation @σᵣ given:
    if has_inference_vars(σᵣ): unify(σᵣ, σ_exp, S)       (annotation with TypeVars)
    else: check σᵣ <: σ_exp                          (covariant check)
    Γ' ⊢ body ⇐ σᵣ
Else:
    Γ' ⊢ body ⇐ σ_exp                                (check against expected return)
────────────────────────────────── [CHECK-FN]
```

**Substitution note:** σ_exp is substitution-applied (S(σ_exp)) before comparison via `state.subst.apply`, ensuring that any type variables bound during parameter checking are resolved before the subsumption check. σᵣ (the declared return annotation) is guaranteed to be concrete — no TypeVars — at the `else: check σᵣ <: σ_exp` sub-branch (the concrete-return-annotation path), because the `!declared.has_inference_vars()` guard has already fired. Applying the current substitution to σᵣ would therefore be a no-op; the code omits it correctly.

Unannotated non-variadic params receive `TypeValue.Unknown` (gradual typing); the type checker accepts any argument without a static check, and the full Hindley-Milner alternative of assigning fresh TypeVars and inferring parameter types from usage requires explicit annotations. Variadic params without annotations also default to Unknown.

When a return annotation is present, the dispatch depends on whether σᵣ contains type variables. If σᵣ is fully concrete (no type variables), the body is **checked** against it (⇐ mode): the body is synthesized, then subsumption verifies the inferred type is a subtype of the declared type. If σᵣ contains type variables (e.g., `fn@a`), the body is **synthesized** and then **unified** with σᵣ. This is necessary because type variables are not ground — `is_subtype` treats them as opaque and only matches reflexively, so `is_subtype(IntLiteral(42), TypeVar("_t5"))` would incorrectly reject valid code. Unification mode binds the type variables via constraint solving (Damas & Milner, 1982), which is the correct mechanism for annotations that introduce polymorphism.

**Function call (bidirectional):**

Three rules depending on the function type. Arity is always checked.

```text
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_inference_vars(Fn(...)) = false
Γ ⊢ aᵢ ⇐ σᵢ                 for lambda args i      [CHECK-FN / checking mode]
Γ ⊢ aⱼ ⇒ τⱼ,  τⱼ ≤ σⱼ       for non-lambda args j  [infer + subsume]
|args| = |params|
────────────────────────────────── [CALL-MONO]
Γ ⊢ [call f a₁...aₙ] ⇒ σᵣ
```

Monomorphic path with a per-argument mode split. Lambda arguments are **checked** via `check_expr` (⇐ mode), which propagates the expected parameter type into the lambda body and enables bidirectional lambda checking mode ([CHECK-FN]). Non-lambda arguments are **synthesized** via `infer_expr` (⇒ mode), producing an inferred type τⱼ, which is then subsumed against σⱼ inline: the check passes when `τⱼ ≤ σⱼ` (i.e., `is_subtype(τⱼ, σⱼ)` holds, or when Unknown/Top is involved, `is_consistent(τⱼ, σⱼ)`). This split avoids the double-inference that would occur if `check_expr` were called on an already-inferred non-lambda expression. The net effect is the same as uniform [SUB]-based checking for ground parameter types: `[add "hello"]` where `add : Fn(Int Int → Int)` produces a type error because `String ≮: Int`.

```text
Γ ⊢ f ⇒ Fn(σ₁...σₙ → σᵣ),  has_inference_vars(Fn(...)) = true
Γ ⊢ aᵢ ⇒ τᵢ  for i = 1..n                          [synthesis]
|args| = |params|
(σ'₁...σ'ₙ → σ'ᵣ) = instantiate(σ₁...σₙ → σᵣ)
S = unify(σ'₁ ≐ τ₁, ..., σ'ₙ ≐ τₙ)                  [with U-SUBSUME]
────────────────────────────────── [CALL-POLY]
Γ ⊢ [call f a₁...aₙ] ⇒ S(σ'ᵣ)
```

**Implementation note:** When the function expression is a VarRef to a polymorphic scheme (e.g., `[id 42]` where `id` is bound to `∀a. Fn(a → a)`), `check_call_with_scheme` is invoked directly with the scheme, bypassing the VAR-POLY instantiation step. This optimization instantiates the scheme once instead of twice (VAR-POLY followed by CALL-POLY). For other function expressions (inline lambdas, compound access chains), the normal path applies: infer the function expression (which may instantiate a scheme via VAR-POLY), then proceed to CALL-POLY. See `src/typecheck.rs` `infer_expr` Call case for the dispatch logic (lines ~441-451).

Polymorphic path with unification: arguments are checked via `check_expr`, which internally dispatches to unification when the expected type contains type variables. Unification binds type variables via [U-VAR] and handles concrete-type comparisons via [U-SUBSUME] (unidirectional subsumption fallback: σ <: τ). Call sites supply the correct sub/sup order for confluence. See the Unification section for [U-SUBSUME] details.

Note: CALL-POLY routes all argument checking through `check_expr`, which internally dispatches to unification when the expected type contains type variables. CALL-MONO uses `check_expr` only for lambda arguments; non-lambda arguments take the inline infer+subsume path described above. `check_expr`'s unification dispatch is therefore relevant only on the CALL-POLY path — CALL-MONO guarantees no inference variables in the function type, so the subsumption check is always against a ground type.

**CALL-MONO/CALL-POLY literal type divergence.** CALL-POLY is more permissive than CALL-MONO for most literal type pairs. The divergence arises because `unify()` uses [U-SUBSUME] (unidirectional `is_subtype(σ, τ)` fallback) while `check_expr` uses directional `is_subtype(actual, expected)`. Concrete-type pair behavior across both paths (rows marked **fails** reject under both CALL-MONO and CALL-POLY):

| Argument type | Parameter type | `is_subtype` (CALL-MONO) | `unify` (CALL-POLY) |
|---------------|---------------|--------------------------|---------------------|
| `Int` | `IntLiteral(n)` | false (Int is wider) | **fails** (Int ≰ IntLiteral) |
| `IntLiteral(n)` | `Int` | true | succeeds via [U-SUBSUME] |
| `Number` | `Int` | false | **fails** (Number ≰ Int) |
| `Int` | `Number` | true | succeeds via [U-SUBSUME] |
| `IntLiteral(n)` | `Float` | false (no subtype relation) | **fails** (no subtype relation) |
| `Float` | `IntLiteral(n)` | false | **fails** (no subtype relation) |

In practice, this divergence rarely surfaces because CALL-MONO only fires for monomorphic function types (no type variables), and monomorphic parameter types like `IntLiteral(n)` are uncommon. The [U-SUBSUME] fallback checks `is_subtype(σ, τ)` unidirectionally for concrete type pairs; call sites are responsible for supplying the correct sub/sup order.

**CALL-MONO implementation path.** CALL-MONO uses a split dispatch: lambda arguments go through `check_expr` (bidirectional checking mode enables lambda parameter inference), while non-lambda arguments are handled by an inline infer+subsume path — `infer_expr` once, then `is_subtype`/consistency check directly — avoiding the double-inference that `check_expr` would cause if called after `infer_expr`. Since CALL-MONO's function type has no inference vars, parameter types are always ground, so unification is never needed; subsumption suffices. The table above illustrates the logical difference between subsumption and unification semantics for concrete type pairs.

```text
Γ ⊢ f ⇒ Unknown
────────────────────────────────── [CALL-UNKNOWN]
Γ ⊢ [f a₁...aₙ] ⇒ Unknown
```

Calling a value typed as Unknown returns Unknown. Arguments are still synthesized (for type map population and nested error detection) but not checked against parameter types.

**Variadic call:**

```text
Γ ⊢ f ⇒ Fn(σ₁...σₙ₋₁, Seq(β) → σᵣ)
Γ ⊢ aᵢ ⇒ τᵢ  for i = 1..n-1               (positional params)
Γ ⊢ aₙ ⇒ υₙ, ..., Γ ⊢ aₖ ⇒ υₖ            (variadic args, k ≥ n-1)
widen(υᵢ) = base type of υᵢ                (IntLiteral→Int, FloatLiteral→Float, StrLiteral→Str)
S = compose(unify(β, widen(υₙ)), ..., unify(β, widen(υₖ)))
──────────────────────────────────────── [CALL-VARIADIC]
Γ ⊢ [call f a₁...aₖ] ⇒ S(σᵣ)
```

All variadic arguments are unified against the same TypeVar β after widening to their base types. Heterogeneous variadic arguments produce a unification error. Inside the variadic function body, the collected parameter has type `Seq(β)` and supports all Seq operations.

Named argument type checking is implemented. `TypeValue.Fn` carries `params: indexed Dict` (integer-keyed, each entry a TypeValue) and `param-names: indexed Dict` (integer-keyed, each entry a String param name or absent for unnamed builtin params). Three paths check named args:

- **CALL-MONO**: for each named arg, the matching parameter is found by name via `param-names`, the arg is inferred with `infer_expr`, and the type is unified against the corresponding entry in `params`. Unknown names and type mismatches emit `TypeError`.
- **CALL-POLY** (`check_call`): same name-based lookup and unify on the instantiated params.
- **`check_call_with_scheme`** `TypeValue.Fn` arm: same name-based lookup and unify, applied after the positional arg unification loop, using the already-instantiated `params` from `instantiate_scheme`.

Known gap: when the callee is a letrec forward reference for a *non-fn* entry (same-dict scope), the type resolves to an unbound `TypeVar` and falls through to the `TypeVar` arm, which skips named-arg validation. Fn-form entries are pre-bound as `TypeValue.Fn` (B-520), so recursive calls to fn entries are handled correctly by the `TypeValue.Fn` arm. See [Type System Extensions](07-type-extensions.md) §Completeness for the remaining gaps.

**Access chains:**

```text
Γ ⊢ e : Record(... k:τ ..., ρ)
────────────────────────────────── [DOT]
Γ ⊢ e.k : τ

Γ ⊢ e ⇒ α,  β fresh
S = unify(α ≐ Record({k: β}))
────────────────────────────────── [DOT-VAR]
Γ ⊢ e.k ⇒ β
```

After unification, α is bound in S — references to α in the conclusion denote its resolved image S(α), not the original variable. The occurs check and level lowering for α, β, and ρ are handled internally by `unify()`.

*Note: Under BAS, all records are closed. Accessing a field not present in a closed record is a static error. See [Type System Extensions](07-type-extensions.md) §Boolean-Algebraic Subtyping.*

```text
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

```text
resolve(ann) = σ,  Γ ⊢ e ⇐ σ                       [checking mode]
────────────────────────────────── [ASSERT]
Γ ⊢ [@σ e] ⇒ σ

resolve(ann) = σ,  Γ ⊢ e ⇐ σ fails,  default ∈ ann
────────────────────────────────── [ASSERT-DEFAULT]
Γ ⊢ [@[type: σ  default: d] e] ⇒ σ
```

Type assertions use checking mode: the inner expression is checked against the annotated type via [SUB]. When checking fails and a `default:` property is present, the assertion succeeds silently (no type error). The default value provides a fallback at runtime.

**Default validation:** The type checker validates `default_ty <: σ` at elaboration time, ensuring defaults are type-safe regardless of whether the expression reaches the default branch. This prevents runtime type errors when the default is actually used.

**Limitation:** When the annotation resolves to a bare type variable (e.g., `[@a $x]`), the static subtype check always fails because `is_subtype` only matches type variables reflexively. Such assertions require a `default:` clause or will produce a type error. To narrow to a polymorphic type, use unification-based checking within a function parameter or return annotation context.

**Type alias:**

```text
resolve(inner) = τ,  register alias in Γ
────────────────────────────────── [ALIAS]
Γ ⊢ [type inner] : Unknown
```

**Annotated expression:**

```text
resolve(ann) = τ
────────────────────────────────── [ANNOTATED]
Γ ⊢ name@ann : τ
```

When name = "Fn": interpret as function type constructor.

**`@Fn` vs `Fn@T` parameter annotation behavior:** Bare `@Fn` in a parameter annotation position (e.g., `f@Fn`) resolves to `Type::Unknown` via `resolve_type_name` — any value is accepted at type-check time and at any `[@Fn expr]` TypeAssert site. No callability check is performed by TypeAssert; `Type::Unknown` matches all values unconditionally, including non-callables. Callability is enforced only when the parameter is actually invoked as a function, which raises a `NotAFunction` error at the call site. `Fn@ReturnType` in a function return annotation position resolves to `Type::Function` with the specified return type via `resolve_fn_type`, which recursively resolves the return annotation and parameter types. This distinction arises from the annotation resolution dispatch: `@Fn` alone has no type parameters, so it cannot construct a `Type::Function` (which requires both params and ret); `Fn@ReturnType [ParamTypes]` has the necessary structure for full function type resolution.

**Seq types:** Sequences are represented as `App(TyCon("Seq"), τ)` in the type grammar. Sequence constructors (`$seq`, `$range`, etc.) infer as `App(TyCon("Seq"), τ)` — see [Type System Extensions](07-type-extensions.md) §Precision.

## Unification: unify(τ₁, τ₂, S) → S'

Unification finds a most general substitution S such that S(τ₁) = S(τ₂). Before matching, both types are normalized via S (substitution applied). Unification follows **Robinson (1965)** for structural decomposition and variable binding. Subtyping is handled by `check_expr` via the `[SUB]` rule and `is_subtype` for directional checks, and by `[U-SUBSUME]` for unidirectional subsumption within unification. This separation follows Pierce & Turner (2000) and Dunfield & Krishnaswami (2021).

**Algorithm variant:** The overall inference algorithm is closer to **Algorithm J** (Milner 1978) than Algorithm W (Damas & Milner 1982): it uses a mutable global substitution (`InferState.subst`) accumulated across inferences with immediate unification on each constraint, rather than threading explicit substitutions compositionally. This is more efficient but harder to reason about formally. The five-pass dict inference (§Dict Inference) is a letrec extension following Tofte (1988).

```text
unify(τ, τ, S) = S                              [U-REFL]
unify(Unknown, τ, S) = S                         [U-UNKNOWN-L]
unify(τ, Unknown, S) = S                         [U-UNKNOWN-R]
unify(α, τ, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-L]
unify(τ, α, S) = S[α ↦ τ]   if α ∉ FV(τ)       [U-VAR-R]
```

**Note:** When one side is a type variable α and the other is `Unknown`, the implementation fires specialized TypeVar rules ([U-UNKNOWN-VAR]/[U-VAR-UNKNOWN], see §Let-Generalization) first, which zero ℓ(α) to prevent unsound generalization of Unknown-unified variables. The general [U-UNKNOWN-L]/[U-UNKNOWN-R] rules above apply only when neither side is a TypeVar.

Literal identity (same literal value = same type):

```text
unify(IntLiteral(m), IntLiteral(n), S) =
    S           if m = n                         [U-INTLIT-EQ]
    error       if m ≠ n                         [U-INTLIT-NEQ]

unify(StringLiteral(s), StringLiteral(t), S) =
    S           if s = t                         [U-STRLIT-EQ]
    error       if s ≠ t                         [U-STRLIT-NEQ]
```

Structural:

```text
unify(Fn₁, Fn₂, S) =
    constrain(Fn₁, Fn₂, S) then constrain(Fn₂, Fn₁, S)    [U-FN]

unify(Fn, Fn) delegates entirely to bidirectional constrain — C-FN is applied twice,
once in each direction. In each call the sub/sup roles are explicit:

  constrain(Fn₁, Fn₂): Fn₁ is sub, Fn₂ is sup.
      sup's (Fn₂) params constrained contravariantly against sub's (Fn₁) params:
          constrain(Fn₂.pᵢ, Fn₁.pᵢ) for each i
      sub's (Fn₁) return constrained covariantly against sup's (Fn₂) return:
          constrain(Fn₁.return, Fn₂.return)

  constrain(Fn₂, Fn₁): Fn₂ is sub, Fn₁ is sup.
      sup's (Fn₁) params constrained contravariantly against sub's (Fn₂) params:
          constrain(Fn₁.pᵢ, Fn₂.pᵢ) for each i
      sub's (Fn₂) return constrained covariantly against sup's (Fn₁) return:
          constrain(Fn₂.return, Fn₁.return)

The two directions together enforce equality: each param type and return type is
constrained in both directions, eliminating any asymmetry. Arity is checked inside
C-FN via check_function_arity (error if |Fn₁.params| ≠ |Fn₂.params|).
See §Constraint Generation: constrain(sub, sup) for the full C-FN specification.

unify(Seq(τ₁), Seq(τ₂), S) = unify(τ₁, τ₂, S)  [U-SEQ]  (legacy; Seq is App(TyCon("Seq"),_) post-migration)

unify(TyCon(n), TyCon(n), S) = S                 [U-TYCON]  (same name AND pointer-identical TyConDef)
unify(TyCon(n1), TyCon(n2), S) = error           [U-TYCON-NEQ]  if n1 ≠ n2 or different definitions

unify(App(f₁,a₁), App(f₂,a₂), S) =                  [U-APP]
    if extract_tycon_spine(App₁) = (n₁, [α₁…αₘ]) and extract_tycon_spine(App₂) = (n₂, [β₁…βₙ])
        and n₁ = n₂ and m = n:                          ← arity guard required
        for each i: constrain(αᵢ, βᵢ) ∧ constrain(βᵢ, αᵢ)   (bidirectional per arg)
    else (n₁ ≠ n₂, or mismatched arg count m ≠ n, or non-TyCon head):
        unify(f₁, f₂, S) then unify(a₁, a₂, S)
Note: n₁/n₂ are the TyCon names extracted from App₁/App₂; m/n are their respective arg
counts. The else branch uses f₁,f₂ and a₁,a₂ from the outer App(f₁,a₁)/App(f₂,a₂)
decomposition — these are not the spine elements α/β.
The arity guard `m = n` must hold before the bidirectional constrain loop fires;
mismatched-arity App falls to the unify(f₁,f₂)/unify(a₁,a₂) fallback.
The fallback recurses via unify (not constrain) to avoid constrain→unify→constrain
infinite loops when the App head is a TypeVar or has no registered TyConDef.

The C-App arm in constrain() owns all variance-directed dispatch: for each argument
position i it looks up TyConDef.variance[i] and applies:
    Covariant:     constrain(sub_argᵢ, sup_argᵢ)
    Contravariant: constrain(sup_argᵢ, sub_argᵢ)   (swapped)
    Invariant:     constrain(sub_argᵢ, sup_argᵢ) ∧ constrain(sup_argᵢ, sub_argᵢ)
    Phantom:       Ok(())
In unify(), all arg positions are treated symmetrically (bidirectional constrain on args).
Variance is only meaningful inside constrain().

unify(Record(r₁), Record(r₂), S) =
    constrain(r₁, r₂)                (unidirectional: all of r₂'s named fields must be in r₁)
    then unify_rowtails(r₁.tail, r₂.tail)                (tail equality, symmetric)
                                                         [U-DICT]

unify(¬T₁, ¬T₂, S) =
    constrain(T₂, T₁) ∧ constrain(T₁, T₂)   (bidirectional on inner types)
                                               [U-NEG]
Negation is contravariant in constrain() — C-Negation swaps directions. For unification
(equality), both directions are applied: constrain(T₂,T₁) from [C-Negation] applied to
¬T₁≤¬T₂, and constrain(T₁,T₂) from [C-Negation] applied to ¬T₂≤¬T₁.
```

**[U-DICT] is unidirectional by design (B-680).** Record unification calls `constrain(r₁, r₂)` only — not `constrain(r₁, r₂) ∧ constrain(r₂, r₁)`. The constraint direction means all of r₂'s named fields must be present in r₁ with unifiable types; extra fields in r₁ are allowed by width subtyping. The reverse direction `constrain(r₂, r₁)` is NOT called. Row tails are then unified symmetrically via `unify_rowtails(r₁.tail, r₂.tail)`.

**Rationale:** Record unification finds the common supertype (join/LUB), not structural equality. `unify({ts: Dict, rt: Dict}, {})` should succeed (the richer record is compatible with the empty record constraint) while `unify({}, {ts: Dict, rt: Dict})` should fail (empty record cannot satisfy the named-field requirement). Bidirectional `constrain` would incorrectly reject the first case with "missing field 'ts'" because `constrain({}, {ts: Dict, rt: Dict})` fails when the empty record cannot cover `ts`. This asymmetry is intentional: U-DICT is a supertype-finding rule, not an equality check.

**Consequence:** `unify(r₁, r₂)` is NOT symmetric for records. `unify(r₁, r₂)` succeeds when r₁ covers all of r₂'s fields; `unify(r₂, r₁)` succeeds when r₂ covers all of r₁'s fields. Call sites must supply arguments in the correct direction. The tail is unified symmetrically (`unify_rowtails`) regardless of direction. See `src/type_unify.rs` TV_RECORD arm and `constrain_rows` for the implementation. See [Type System Extensions](07-type-extensions.md) §Column Constraints for the full tail rules.

Row tail unification:

```text
unify_rows(Row{f, Empty}, Row{{}, Uniform{V}}, S):
    let V' = apply_to_fixpoint(S, V)
    if V' is unbound TypeVar(α):
        join = normalize_union(field types of f)
        S' = unify(α, join, S)                   [U-UNIFORM-VAR]
    else:
        for Ti in field types(f): assert is_subtype(Ti, V')
                                                  [U-UNIFORM-CONCRETE]

unify_rows(Row{{}, Uniform{V1}}, Row{{}, Uniform{V2}}, S):
    S' = unify(K1, K2, S)  (if both keyed, else skip)
    unify(V1, V2, S')                             [U-UNIFORM-UNIFORM]

unify_rows(Row{_, Empty}, Row{_, Uniform{..}}, S) = error  "closed row does not satisfy uniform constraint"
```

Subsumptive fallback for concrete types (no type variables on either side):

```text
unify(σ, τ, S) where ¬has_inference_vars(σ) ∧ ¬has_inference_vars(τ):
    if is_subtype(σ, τ): S    [U-SUBSUME]  (unidirectional: σ must be subtype of τ)
    else: error               [U-FAIL]
```

[U-SUBSUME] is the bridge between unification and subtyping. It fires after all other rules (structural decomposition, type variable binding) have been tried — it is ordered last as a fallback, not a catch-all. Structural rules ([U-FN], [U-SEQ], [U-REC], literal identity) take priority over subsumptive matching. When two concrete types remain and σ is a subtype of τ, unification succeeds without modifying the substitution. This is essential for **CALL-POLY**: when a type variable α is bound to `IntLiteral(42)` by one argument and later compared against `Int` by another (via substitution resolution), [U-SUBSUME] recognizes `IntLiteral(42) <: Int` and succeeds. The check is unidirectional (σ <: τ); call sites supply the correct order.

**Relationship to Robinson unification.** Robinson (1965) is purely syntactic — it has no notion of subtyping, so `unify(IntLiteral(42), Int)` would simply fail (different constructors). [U-SUBSUME] extends Robinson with a ground-type compatibility check: when both sides are concrete and in a subtype relationship, unification succeeds without modifying the substitution. This is a pragmatic middle ground — Robinson handles structural decomposition and variable binding; [U-SUBSUME] handles the subtype lattice at ground types. The substitution is not modified by [U-SUBSUME], so existing variable bindings (which may carry literal precision) are preserved. This is the same approach Rust's type inference uses: subtyping constraints between concrete types are resolved as compatibility checks rather than LUB computation (Dolan & Mycroft 2017 describe the full alternative — algebraic subtyping — which tinct intentionally does not adopt; see `doc/whatif/algebraic-subtypes.md`).

[U-SUBSUME] is unidirectional: it checks `is_subtype(σ, τ)` only. The caller is responsible for argument order — in contravariant positions, the arguments are swapped before calling unify(). `unify(IntLiteral(42), Int)` succeeds (IntLiteral(42) <: Int); `unify(Int, IntLiteral(42))` goes to [U-FAIL]. This is intentional: unify() is not inherently symmetric at the ground-type level; its symmetry comes from the call sites using bidirectional constrain(). The substitution is unchanged because there are no type variables to bind.

**Interaction with [SUB]:** At CALL-MONO sites (fully concrete types, no unification needed), `check_expr` uses directional subsumption via `is_subtype(actual, expected)` — only the correct direction is checked. [U-SUBSUME] is also unidirectional (σ must be subtype of τ) — the correct argument order is established before calling unify().

Literal-to-parent promotions are handled entirely by [U-SUBSUME] — there are no separate fast-path arms in `unify()`.

All other non-structural, non-subsumable combinations: error [U-FAIL]

**Interaction with CALL-POLY:** Polymorphic call checking synthesizes all argument types, then unifies each against the corresponding instantiated parameter type. Type variable binding comes from [U-VAR]; concrete type compatibility (after substitution resolves variables) comes from [U-SUBSUME]. Confluence at CALL-POLY sites is ensured by call sites establishing the correct sub/sup direction before calling unify() — [U-SUBSUME] itself is unidirectional.

**`TypeStageApp` unification rules.** After `normalize()` runs, `unify_normalized` may still encounter irreducible `TypeStageApp` nodes (non-ground args). Four cases:

```text
unify(TypeStageApp("F", a₁), TypeStageApp("F", a₂))   # same function, F injective:
  → bidirectional constrain on args: constrain(aᵢ,bᵢ) ∧ constrain(bᵢ,aᵢ) for each i
                                                       [U-TSA-CONGRUENCE]

unify(TypeStageApp("F", a₁), TypeStageApp("F", a₂))   # same function, F non-injective:
  → defer to InferState.deferred_equalities           [U-TSA-DEFER]

unify(TypeStageApp("F", _), TypeStageApp("G", _))      # different functions:
  → TypeError (apart — Eisenberg et al. 2014)         [U-TSA-APART]

unify(TypeStageApp("F", args), ConcreteType)           # stuck application:
  → TypeError("type-stage application has unresolved arguments")  [U-TSA-STUCK]

unify(TypeStageApp("F", args), TypeVar(α))             # TypeVar binding:
  → S[α ↦ TypeStageApp("F", args)], occurs check traverses args  [U-TSA-VAR]
```

The FD elaboration case (`c ~ TypeStageApp("AddResult", [a, b])`) uses [U-TSA-VAR] — `c` is the mediating TypeVar and is never stuck against a non-TypeVar.

## Unification for Recursive Types

Unifying `TypeNode.Recursive` and `TypeNode.TypeVar` types requires five match arms in a precise ordering. **Match ordering is critical**: TypeVar binding arms come before the asymmetric Recursive opening arms. Without this ordering, `unify(Recursive, TypeVar)` would hit the asymmetric arm, open the Recursive, and bind the TypeVar to the opened body — losing the recursive structure.

### The Five Arms

```rust
match (a, b) {
    // Arm 1 (symmetric): both are Recursive — open with ONE shared fresh TypeVar.
    // Shared var produces a direct result: no extra indirection in error messages.
    (TypeNode.Recursive { var: v1, body: b1 },
     TypeNode.Recursive { var: v2, body: b2 }) => {
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        let b_open = substitute(b2, v2, &fresh);
        constrain(a_open, b_open)?;   // bidirectional: opened_a ≤ opened_b
        constrain(b_open, a_open)     //               opened_b ≤ opened_a
    }

    // Arm 2 (TypeVar left): bind TypeVar to the right side.
    // Must come BEFORE the asymmetric Recursive arms.
    (TypeNode.TypeVar { name, .. }, b) => {
        occurs_check(name, &b)?;
        subst.bind(name, b);
        Ok(())
    }

    // Arm 3 (TypeVar right): bind TypeVar to the left side.
    (a, TypeNode.TypeVar { name, .. }) => {
        occurs_check(name, &a)?;
        subst.bind(name, a);
        Ok(())
    }

    // Arm 4 (asymmetric left): left is Recursive, right is concrete (not TypeVar, not Recursive).
    (TypeNode.Recursive { var: v1, body: b1 }, other) => {
        let fresh = state.fresh_type_var();
        let a_open = substitute(b1, v1, &fresh);
        constrain(a_open, other)?;   // bidirectional: opened_a ≤ other
        constrain(other, a_open)     //               other ≤ opened_a
    }

    // Arm 5 (asymmetric right): right is Recursive, left is concrete.
    (other, TypeNode.Recursive { var: v2, body: b2 }) => {
        let fresh = state.fresh_type_var();
        let b_open = substitute(b2, v2, &fresh);
        constrain(other, b_open)?;   // bidirectional: other ≤ opened_b
        constrain(b_open, other)     //               opened_b ≤ other
    }

    // Structural cases for concrete non-Recursive, non-TypeVar types...
}
```

### Ordering Rationale

Arms 2 and 3 (TypeVar binding) must appear before Arms 4 and 5 (asymmetric Recursive opening). The key case: `unify(Recursive, TypeVar)`. With the correct ordering, Arm 2 or 3 fires and binds the TypeVar to the full Recursive type. With the wrong ordering, Arm 4 fires, opens the Recursive, and the TypeVar is bound to the opened body — the recursive structure is lost.

### Termination

- **Arms 2 and 3** terminate immediately — one substitution entry added, no recursive call.
- **Arms 4 and 5** replace `RecursiveRef(var)` with `TypeNode.TypeVar { name: fresh }` via `substitute`. After opening, the former recursive positions hold TypeVars. The two bidirectional `constrain()` calls then descend into the opened body. When descent reaches the fresh TypeVar paired against any type, Arms 2 or 3 fire. No further Recursive arm fires on that side. Termination follows by structural induction on the non-Recursive sides.
- **Arm 1** (symmetric): the shared fresh TypeVar is substituted into both opened bodies. The bidirectional `constrain()` calls descend; when the TypeVar is reached in either body, Arms 2 or 3 fire immediately.

`unfold_once` — which replaces `RecursiveRef` with the full `Recursive` type, making the tree larger — is used only in subtype checking (where S-Assum prevents divergence), not in unification.

### C-Recursive Arms in constrain()

Three analogous arms in `constrain(sub, sup)` handle the equirecursive cases directionally:

```text
constrain(μv₁.body₁, μv₂.body₂) =
    let α = fresh TypeVar
    constrain(body₁[α/v₁], body₂[α/v₂])   [C-Recursive-Both]

constrain(μv₁.body₁, other) where other is not Recursive =
    let α = fresh TypeVar
    constrain(body₁[α/v₁], other)           [C-Recursive-Left]

constrain(other, μv₂.body₂) where other is not Recursive =
    let α = fresh TypeVar
    constrain(other, body₂[α/v₂])           [C-Recursive-Right]
```

Note that the C-Recursive arms in `constrain()` are directional (one call each), while `unify()` Arms 1/4/5 apply bidirectional constrain. The unify arms use bidirectional constrain because unification requires equality; the constrain arms are directional because constrain is the subtyping judgment (sub ≤ sup).

## Constraint Generation: constrain(sub, sup)

`constrain(sub, sup)` is the **directional** subtyping judgment: it asserts `sub ≤ sup`. Unlike `unify()`, which is symmetric and produces a substitution, `constrain()` propagates bounds into `InferState` and returns `Ok(())` or a type error. It is the primary structural judgment for all compound types; `unify()` delegates all compound-type structural decomposition to `constrain()`.

**Judgment form:** `constrain(sub, sup, state, constraints, span) → Result<()>`

### Structural Arms

```text
constrain(Fn(p₁…pₙ→r₁), Fn(q₁…qₙ→r₂)) =
    Special case — zero-param variadic ("any-function": params=[], rest=Some(...)):
        if sub is any-function or sup is any-function:
            constrain(r₁, r₂)   (covariant return only — arity check skipped, params not constrained)
        Any-function = params=[], rest=Some(...) (i.e., a zero-param variadic, used as the fn? marker).
    General case (in code order: arity check fires first, then param/return constrain):
        error if |p| ≠ |q|  (arity mismatch, via check_function_arity — fail fast before any constrain)
        constrain(qᵢ, pᵢ) for each i   (contravariant params: sub params must accept sup params)
        constrain(r₁, r₂)               (covariant return)          [C-FN]

constrain(Dict(r₁), Dict(r₂)) =
    constrain_rows(r₁, r₂)          (directional: sub_row ≤ sup_row)    [C-Dict]

constrain(T·C{r₁}, T·C{r₂}) where same tycon T and ctor C =
    constrain_rows(r₁, r₂)          (directional field subtyping)        [C-NominalVariant]

constrain(App(F, a₁…aₙ), App(F, b₁…bₙ)) where F has registered TyConDef =
    for each i, dispatch on TyConDef.variance[i]:
        Covariant:     constrain(aᵢ, bᵢ)                                 [C-App-Cov]
        Contravariant: constrain(bᵢ, aᵢ)   (swapped)                     [C-App-Contra]
        Invariant:     constrain(aᵢ, bᵢ) ∧ constrain(bᵢ, aᵢ)            [C-App-Inv]
        Phantom:       Ok(())                                             [C-App-Phantom]
    error if i ≥ |TyConDef.variance|  (more args than declared variance positions)
    Note: different TyCon names or no TyConDef → fall through to unify() for structural
    recursion on components (not an error; avoids constrain→unify→constrain loop).

constrain(μv₁.body₁, μv₂.body₂) =
    let α = fresh TypeVar
    constrain(body₁[α/v₁], body₂[α/v₂])   (open both, share fresh var)  [C-Recursive-Both]

constrain(μv₁.body₁, other) where other is not Recursive =
    let α = fresh TypeVar
    constrain(body₁[α/v₁], other)                                         [C-Recursive-Left]

constrain(other, μv₂.body₂) where other is not Recursive =
    let α = fresh TypeVar
    constrain(other, body₂[α/v₂])                                         [C-Recursive-Right]

constrain(¬T₁, ¬T₂) =
    constrain(T₂, T₁)   (contravariant: sub's negation ≤ sup's negation iff sup ≤ sub)
                                                                           [C-Negation]

constrain(TypeStageApp("F", a₁…aₙ), TypeStageApp("F", b₁…bₙ)) where same function F =
    constrain(aᵢ, bᵢ) ∧ constrain(bᵢ, aᵢ) for each i   (invariant: bidirectional)
                                                           [C-TypeStageApp]
```

### constrain_rows(sub_row, sup_row)

`constrain_rows` implements directional record subtyping (sub_row ≤ sup_row):

```text
constrain_rows(sub_row, sup_row):
    # Step 1: Width subtyping + depth subtyping on shared fields.
    for each (k, sup_ty) in sup_row.fields:
        if k ∈ sub_row.fields:
            constrain(sub_row[k], sup_ty)          (covariant field subtyping)
        elif sub_row.tail = RowTail.Uniform { value-type: v }:
            constrain(v, sup_ty)                   (uniform tail covers missing field)
        else:
            error "missing field k"                (no Uniform tail, no field: error)

    # Step 2: Tail compatibility.
    (_, closed_tail)             → Ok(())          (sub may have more fields: width subtyping)
    (sub_tail, RowTail.Uniform { value-type: sup_v }) →
        for each sub_field_ty in sub_row.fields:
            constrain(sub_field_ty, sup_v)         (all sub fields ≤ sup uniform value)
        if sub_tail = RowTail.Uniform { value-type: sub_v }:
            constrain(sub_v, sup_v)                (sub uniform value ≤ sup uniform value)
            if sup has key type sup_k:
                constrain(sub_k, sup_k)  or error  (key type ≤)
```

### Invariant

**Every compound `TypeValue` constructor that carries structural sub-terms must have an explicit `constrain()` arm (e.g., `TV_RECORD`, `TV_FN`, `TV_APP`).** Falling through to `unify()` is permitted only for:

- `TypeValue.Var`-to-`TypeValue.Var` binding (C-Var1/C-Var2 handle union/intersection TV_VAR cases above)
- Atomic mismatches (e.g., `Repr("Value::Int")` vs `Repr("Value::String")`) — `unify()` generates the structured error
- Ground-type pairs with no inference variables — `constrain()` applies the BAS `is_subtype` check first, then falls through to `unify()` for error generation on failure

If a new compound TypeValue constructor is added to builtin_core.llt, a `constrain()` arm must be added before introducing the constructor. Relying on `unify()`'s U-SUBSUME fallthrough for TV_VAR-containing compound types is unsound — variance would be lost.

### Match Arm Narrowing for Dict Patterns (B-617)

When a `[match ...]` arm uses a dict pattern (`[case [let v] [k₁: τ₁ ... kₙ: τₙ] body]`), the type checker narrows the scrutinee type for all subsequent arms. After processing an arm that structurally matches on static keys `k₁...kₙ`, the remaining scrutinee is narrowed by intersecting with the negation of an open dict containing those keys:

```text
narrowed_scrutinee = scrutinee ∩ ¬{k₁: Any, ..., kₙ: Any, _: Any}
```

The negation type `¬{k₁: Any, ..., kₙ: Any, _: Any}` (a `TypeValue.Neg` wrapping an open `TypeValue.Record` with a `RowTail.Uniform` tail) represents values that do NOT have all of `k₁...kₙ` as fields. The wildcard arm `...:` therefore sees only values not matched by any preceding dict arm.

Only statically-known string keys from the arm pattern contribute to narrowing (dynamic keys via `[bracket ...]` forms are ignored for soundness). The narrowing is applied in `setup_match_arm_env` in `src/typecheck.rs` and uses `TypeValue.Inter` with `TypeValue.Neg` to represent the residual type.

## Contractiveness

A recursive type `TypeNode.Recursive { var, body }` is **contractive** iff every path in `body` from the root to an occurrence of `RecursiveRef(var)` passes through at least one guarding constructor. Guarding constructors are those whose `@[guarding: true]` annotation is set: `TypeNode.Dict`, `TypeNode.Arrow`, `TypeNode.TypeApplication`. Non-guarding constructors — `TypeNode.Union`, `TypeNode.Intersect` — are logical combinators that do not structurally interpose between the binder and its reference.

### The `is_contractive` Check

```text
is_contractive(node, var):
  # Case 1: direct self-reference — non-contractive
  if node is TypeNode.RecursiveRef { name } and name == var:
    return false

  # Case 2: guarding constructor — any RecursiveRef(var) under this node is safely guarded.
  # Reads from the constructor's @[guarding: Bool] annotation.
  if annotation_of(TypeNode_constructor_for(node)).guarding:
    return true

  # Case 3: non-guarding (Union, Intersect) — recurse into children.
  return all(TypeNode.children(node), c → is_contractive(c, var))
```

### Construction-Time Rejection

The contractiveness check runs at construction time in two places:

1. **In `mu`**: after `[let body [f TypeNode.RecursiveRef name: var]]`, before constructing `TypeNode.Recursive`. If `not(is_contractive(body, var))`, emit `TypeError(NonContractive)` pointing at the `mu` call site.
2. **In `expand_named`**: after expanding the alias body, before wrapping in `TypeNode.Recursive`. If `not(is_contractive(expanded, fresh_var))`, emit `TypeError(NonContractive)`.

Non-contractive types are rejected at construction rather than at use sites. This gives a clear diagnostic at the point of definition and eliminates the need for `▷` ("later") modality tracking inside `is_subtype_bas` / `is_atom_subtype`. The tradeoff is explicit: non-contractive types like `μa.a` (the fixed point of the identity function) are ill-formed and rejected, not valid types that subtype nothing.

**Why construction-time, not checker-time `▷` modality.** The checker-time alternative — `▷` tracking through every BAS arm — would allow non-contractive types to be constructed but would require additional state per arm and would produce confusing errors at use sites rather than definition sites. Given that non-contractive types have no practical use in tinct (they are semantically ⊥), construction-time rejection is strictly better for users. The flat `HashSet` for sigma (without `▷`) is sound precisely because all `TypeNode.Recursive` values reaching `is_subtype_bas` / `is_atom_subtype` are contractive by construction — contractiveness guarantees that S-Exp unfolding always reaches a guarding constructor before sigma keys match again (Chau & Parreaux 2026).

**Examples:**

- `[mu [fn [let self] self]]` → body is `RecursiveRef(var)` → non-contractive → error ✓
- `[mu [fn [let self] [or self Int]]]` → body is `Union([RecursiveRef, Int])` → Union is non-guarding → non-contractive → error ✓
- `[mu [fn [let self] [record head: Int  tail: self]]]` → body is `Record(...)` → guarding → contractive → accepted ✓
- `[mu [fn [let self] [or Absent [record head: Int  tail: self]]]]` → `Union([Absent, Record(...)])` → contractive in both branches → accepted ✓

## Subtyping: τ <: σ

Subtyping is a pure predicate (no substitution mutation). Used for TypeAssert validation and return type checking.

```text
τ <: Unknown                                     [S-UNKNOWN-TOP]
Unknown <: τ                                     [S-UNKNOWN-BOT]
τ <: τ                                           [S-REFL]
IntLiteral(n) <: Int <: Number                   [S-INT]
StringLiteral(s) <: Str                          [S-STR]
Float <: Number                                  [S-FLOAT]
Seq(τ) <: Seq(σ)  if τ <: σ                      [S-SEQ]  (legacy; use App(TyCon("Seq"),_) post-migration)

App(TyCon(f), a) <: App(TyCon(f), b)
    where variance(f) = Covariant  if a <: b     [S-APP-COV]
    where variance(f) = Contravariant  if b <: a [S-APP-CONTRA]
    where variance(f) = Invariant  if a = b      [S-APP-INV]
    where variance(f) = Phantom    always         [S-APP-PHANTOM]
    (when tycon_env = None: treat all as Invariant — conservative, never unsound)

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

**Note on Unknown and Top:** `Unknown` relates to other types via consistency (~), not subtyping (<:) — see `is_consistent()`. `Top` is the true universal supertype with `τ <: Top` for all `τ`. `Unknown` and `Top` are distinct: `Unknown` is the gradual typing escape hatch (consistent with all types), while `Top` is the genuine lattice top (a supertype of all types without special consistency rules). See `doc/whatif/completed/gradual-typing.md`.

## Coinductive Subtyping: S-Exp + S-Assum

Subtyping for equirecursive types uses the **S-Exp + S-Assum** framework (Chau & Parreaux 2026; Amadio & Cardelli 1993), which extends BAS with a bisimulation to prevent divergence when comparing structurally equivalent recursive types.

### Two-Level Design

Sigma (`Σ`) is a `HashSet<(String, String)>` of visited binder-name pairs. It is allocated once per top-level subtype check and threaded through every recursive call. The Rust type system enforces sigma threading structurally — `sigma: &mut HashSet<(String, String)>` is a required parameter of `is_atom_subtype` (in `src/bas.rs`), so any recursive call that omits it fails to compile.

```rust
/// Public entry point (src/type_def.rs) — delegates to RDNF-based BAS subtyping.
/// A <: B  iff  is_empty(to_rdnf(A & ~B))
/// `is_subtype_bas` is the primary entry called by the type checker; it allocates sigma.
/// `is_subtype` (also in type_def.rs) is the caller-facing wrapper that handles TypeVar
/// and other pre-RDNF guards, then delegates to `is_subtype_bas`.
pub fn is_subtype_bas(
    sub: &Type,
    sup: &Type,
    tycon_env: Option<&TyConEnv>,
    sigma: &mut HashSet<(String, String)>,
) -> bool { ... }

/// Per-atom comparison (src/bas.rs) — sigma and depth are passed to EVERY recursive call.
/// No arm allocates a new sigma. `depth` caps coinductive unfolding (MAX_ATOM_SUBTYPE_DEPTH=256).
pub fn is_atom_subtype(
    sub: &Atom,
    sup: &Atom,
    tycon_env: Option<&TyConEnv>,
    depth: usize,
    sigma: &mut HashSet<(String, String)>,
) -> bool { ... }
```

### S-Assum and S-Exp Rules

Two rules govern recursive types. They fire in order at the top of every `is_atom_subtype` call for `Recursive` atoms:

**S-Assum** — coinductive hypothesis. When both sides are `TypeValue.Recursive`, check and insert the pair before proceeding:

```text
(ptr(a), ptr(b)) ∈ Σ
────────────────────────────────────── [S-ASSUM]
Recursive(a) <: Recursive(b)  (return true immediately)
```

If the pair is not yet in Σ, insert `(ptr(a), ptr(b))` into Σ and continue.

**S-Exp** — structural unfolding. When one side is `TypeValue.Recursive`, unfold it once and re-enter `is_atom_subtype`. Sigma already contains the pair when S-Exp fires (inserted by S-Assum), so when the unfolded body's recursive positions are reached, S-Assum terminates the check immediately:

```text
unfold_once(Recursive(body)) = body[RecursiveRef(depth=0) ↦ Recursive(body)]
```

```text
a = Recursive(_),   sigma ← sigma ∪ {(ptr(a), ptr(b))}
──────────────────────────────────────────────────────── [S-EXP-L]
a <: b  iff  unfold_once(a) <: b  (sigma threaded)

b = Recursive(_),   sigma ← sigma ∪ {(ptr(a), ptr(b))}
──────────────────────────────────────────────────────── [S-EXP-R]
a <: b  iff  a <: unfold_once(b)  (sigma threaded)
```

The sigma key is `(ptr(a), ptr(b))` — a `(String, String)` pair of `Arc` pointer addresses formatted as decimal strings via `format!("{:p}", Arc::as_ptr(tv))`. Arc pointer equality gives O(1) lookup and correctly identifies self-comparison (S-Assum fires for `Arc::ptr_eq`). Note: two separately allocated structurally-identical `Recursive` values have different pointers and won't trigger S-Assum — see B-668 for the known limitation.

### Sigma Threading Through All BAS Arms

Sigma is passed to every recursive call in every BAS arm. This is the load-bearing invariant: without it, a coinductive hypothesis established for `(μa.T[a], A ∨ B)` would be unavailable inside the distribution sub-checks for `(μa.T[a], A)` and `(μa.T[a], B)`. Representative arms:

```rust
// Emptiness check in is_rdnf_empty (src/bas.rs): sigma passed through is_atom_subtype
// for every Recursive atom comparison.
// Union / intersection distribution is handled in to_rdnf before atom comparison.
// Record field checks, Arrow param/return, App variance — is_atom_subtype arms:
// every recursive call passes sigma
```

The invariant — sigma is always passed, never recreated — is what Chau & Parreaux (2026) prove sound for BAS with equirecursive types. A hard depth limit (`MAX_ATOM_SUBTYPE_DEPTH = 256` in `src/bas.rs:36`) provides a safety backstop against non-termination from deeply nested recursive types: when `depth >= MAX_ATOM_SUBTYPE_DEPTH`, `is_atom_subtype` returns `false` conservatively. Sigma alone guarantees termination for well-formed recursive types; the depth limit guards against degenerate inputs or implementation bugs that produce pathologically deep recursion.

### Why S-Exp + S-Assum for BAS

The naive "distribute over union before unfolding" approach fails because the hypothesis established for `(μa.T[a], A ∨ B)` is keyed on that exact pair. After distribution, sub-checks for `(μa.T[a], A)` and `(μa.T[a], B)` have different keys — the hypothesis is unavailable. S-Assum fires at the start of every call and is available inside all BAS decomposition rules, preventing this failure.

## Instantiation

```text
instantiate(τ) = (S(τ), S)
    where S.type_map = {α₁ ↦ _t0, α₂ ↦ _t1, ...}  (type vars → Type)
    for each αᵢ ∈ FTV(τ), fresh type var names _tN generated
    from a shared monotonic per-file counter.

FTV(τ) collects type variables via ctx.free_vars(ty) — walks settled
Dict payloads recursively, returning unbound TypeVar names.

Under BAS, all records are closed. TypeValue.Scheme carries only
type variable names in its vars dict. Record openness is expressed
via width subtyping in is_subtype_bas_with_sigma.
```

This is alpha-renaming for call-site freshening. Each polymorphic call site gets independent type variables so unification at one site does not constrain another. With let-generalization (below), instantiation also handles let-bound polymorphic type schemes.

## Let-Generalization (Levels-Based)

Tinct uses levels-based let-generalization following Kiselyov (2013) to support polymorphic let-bindings. This extends annotation-driven polymorphism with automatic generalization at dict entry boundaries.

**Type schemes.** The type environment Γ maps names to *type schemes* σ rather than bare types τ:

```text
σ ::= ∀(α₁...αₙ, ρ₁...ρₘ). τ    (n,m ≥ 0; when both zero, equivalent to monomorphic τ)
```

Implementation: the runtime environment stores schemes as `TypeValue.Scheme`, the tinct-side variant declared in `stdlib/builtin_core.llt`:

```text
TypeValue.Scheme {
    vars:        Dict,              -- var-name → VarDecl { name: String, kind: TypeValue }
    constraints: Dict,              -- reserved for typeclass constraint integration (currently empty)
    body:        TypeValue,         -- the quantified type body
    narrowings?: Dict,              -- optional; index → TypeValue narrowing hint per parameter
    doc?:        String,            -- optional; docstring for the generalized binding
}
```

Monomorphic bindings (no generalizable variables) are stored as bare `TypeValue` values without a `TypeValue.Scheme` wrapper. The `Env` type stores schemes as `EnvSlot.scheme: Option<TypeValue>` — `None` for non-scheme bindings, `Some(TypeValue.Scheme {...})` for polymorphic ones.

**Scheme grammar:** `σ ::= ∀(α₁...αₙ). τ` where α₁...αₙ are the keys of the `vars` dict and τ is `body`. Under BAS, all records are closed and schemes carry only type variables — no row variable quantifier.

**Levels.** Every type variable α carries an integer level ℓ(α). The type checker maintains a current level counter ℓ_current, incremented at each dict boundary (every `infer_dict` call):

- Fresh type variables are created at ℓ_current
- Level is NOT stored in `TypeValue.Var` — the `TypeValue.Var` payload carries only `{ name: String }`
- Under BAS, all records are closed. Width subtyping handles record openness.

**Level storage and mutation.** Levels must be mutable during unification (Kiselyov's level lowering). Levels live entirely in `InferenceContext`, which is stored as `state.ctx`:

```rust
pub struct InferenceContext {
    pub current_level: u32,                                           // current binding depth
    pub levels: HashMap<String, u32>,                                 // TypeVar name → current (possibly lowered) level
    pub subst: HashMap<String, Arc<Value>>,                           // monotonic TypeVar → TypeValue binding
    pub tycon_env: HashMap<String, Arc<TyConDef>>,                    // type constructor definitions for BAS variance
    pub resolver_deferred: Vec<(Arc<Value>, Arc<Value>)>,             // deferred equality pairs for non-injective resolver FDs; drained by run_fd_improvement_fixpoint
    gensym_counter: u64,                                              // private, for globally unique fresh names
}
```

`InferenceContext.subst` is a monotonic binding map: once a TypeVar is bound, the binding is never removed or overwritten. Level lowering mutates `ctx.levels[name]` without touching `ctx.subst`. The level in `ctx.levels[name]` is the *current* (possibly lowered) level; it is this value that `generalize_tv` consults, not any level embedded in the TypeValue itself.

`InferState.subst` and `InferState.levels` no longer exist as top-level fields — all substitution and level state is accessed via `state.ctx`.

When a fresh TypeVar is created (`ctx.fresh_typevar(prefix)`), its name is registered in `ctx.levels` at the current level. During unification, level lowering mutates `ctx.levels[name]` without rebuilding the TypeValue. This matches the authoritative-level model from Kiselyov (2013): the level embedded at creation time is a lower bound; the authoritative current level is always in `ctx.levels`.

**Level adjustment during unification (symmetric).** Both branches of type variable unification perform level lowering:

```text
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

**Unknown-unification and generalization.** When a type variable α is unified with `Unknown`, the current [U-UNKNOWN] rules succeed without binding α. To prevent incorrect generalization of the unbound α, `unify(α, Unknown)` sets `ℓ(α) = 0` (below all binding levels):

```text
unify(α, Unknown, S) = S,  set ℓ(α) = 0               [U-UNKNOWN-VAR]
unify(Unknown, α, S) = S,  set ℓ(α) = 0               [U-VAR-UNKNOWN]
unify(Unknown, τ, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ)                             [U-UNKNOWN-COMPLEX]
unify(τ, Unknown, S) = S,  set ℓ(β) = 0
    for all β ∈ FTV(τ)                             [U-COMPLEX-UNKNOWN]
```

This ensures Unknown-touched variables are never generalized (since `ℓ(β) = 0` is never `> ℓ` for any binding level). The [U-UNKNOWN-VAR] and [U-VAR-UNKNOWN] rules are special cases of the complex rules where FTV(α) = {α}. The [U-UNKNOWN-COMPLEX] and [U-COMPLEX-UNKNOWN] rules handle cases like `unify(Unknown, Fn(β → Int))` where β must also be zeroed to prevent over-generalization.

**Generalization.** At a dict boundary at level ℓ, after all entries in the letrec group are inferred:

```text
generalize(ℓ, τ) = ∀{α | α ∈ FTV(τ), ℓ(α) > ℓ}. τ     [GEN]
```

where ℓ(α) is read from `ctx.levels[α]` (the current, possibly lowered level). Type variables whose level exceeds the enclosing scope's level are local to the binding and can be universally quantified. Variables at or below level ℓ are free in the enclosing scope and must remain monomorphic.

Implementation signature:

```rust
pub fn generalize_tv(enclosing_level: u32, ty: &TypeValue, ctx: &InferenceContext) -> TypeValue
```

Calls `ctx.free_vars(ty)` to collect unbound TypeVar names, filters by `ctx.get_level(name) > enclosing_level`, and returns a `TypeValue.Scheme` wrapping the original TypeValue. If no TypeVars are generalizable (monomorphic fast path), the original TypeValue is returned unchanged without wrapping. Located in `type_env.rs`.

**[VAR-POLY] rule:** See §Inference Judgments: Γ ⊢ e ⇒ τ above. Variable references instantiate the type scheme stored in Γ at ℓ_current.

Implementation signature:

```rust
pub fn instantiate_scheme_tv(scheme: &TypeValue, ctx: &mut InferenceContext, level: u32) -> Option<TypeValue>
```

Matches `TypeValue.Scheme` by ctor, extracts the `vars` dict and `body` TypeValue synchronously via settled-thunk inspection, builds a renaming from var-name to `ctx.fresh_typevar()` at `level`, and applies it via `apply_typevalue_renaming`. Returns `None` if the scheme is malformed (wrong ctor, unsettled payload). Located in `type_env.rs`.

**Modified dict inference (letrec with generalization):**

```text
Pass 0 — Key resolution: unchanged
Pass 1 — Bind all (B-520):
         For each fn-form entry kᵢ (SurfaceExpression::Fn):
           αᵢ = Fn([fresh βⱼ for each param j], variadic) → fresh γᵢ  at level ℓ+1
           Recursive calls see a Function-shaped callee so the
           TypeValue.Fn arm handles them without a return annotation.
           Variadic params get Dict(Uniform { value: fresh δ }) as βⱼ
           rather than a bare TypeVar, mirroring infer_fn's own binding.
         For each non-fn entry kⱼ:
           αⱼ = fresh TypeVar at level ℓ+1
           Forward references see αⱼ (participates in unification,
           unlike the previous Unknown which silently matched everything).
         Γ' = Γ, k₁:α₁, ..., kₙ:αₙ
Pass 2 — Type aliases: unchanged. Aliases remain monomorphic
         (IndexMap<String, TypeValue>, not a TypeValue.Scheme).
Pass 3 — Infer values: at level ℓ+1, for each non-alias
         entry kᵢ, infer Γ' ⊢ eᵢ : τᵢ, then unify(αᵢ, τᵢ).
         Apply resulting substitution S.

         Implementation note: substitution is handled through `state.ctx: InferenceContext`.
         `ctx.subst` is a monotonic map — bindings accumulate and are never removed. When
         a TypeVar is unified with a TypeValue, `ctx.bind(name, val)` records the binding.
         Forward-reference TypeVars from Pass 1 are resolved by `ctx.apply_subst()` when
         their binding propagates from an earlier sibling. This is the mechanism that makes
         letrec sibling cross-references work (e.g. `[a: "hi"  b: [length a]]`): `a`'s
         TypeVar is bound during its own inference step; when `b` is inferred, `ctx.apply_subst`
         chases the binding to the concrete String type.

Pass 4 — Generalize: for each entry kᵢ,
         σᵢ = generalize_tv(ℓ, ctx.apply_subst(αᵢ), ctx)
         Update Γ'(kᵢ) = σᵢ
Build Record(k₁:ctx.apply_subst(α₁)...kₙ:ctx.apply_subst(αₙ), Closed).
────────────────────────────────── [DICT-GEN]
```

Non-Dict Record expressions at document boundaries follow the same level-increment + generalize protocol as [DICT-GEN]:

```text
Γ, ℓ ⊢ save ℓ_enc = ℓ; increment ℓ to ℓ+1
Γ, ℓ+1 ⊢ e : Record(k₁:τ₁ … kₙ:τₙ, tail)
         restore ℓ to ℓ_enc
         for each kᵢ: σᵢ = generalize_tv(ℓ_enc, τᵢ, ctx)
         Γ' = Γ[k₁ ↦ σ₁, …, kₙ ↦ σₙ]
──────────────────────────────────────────── [NON-DICT-GEN]
```

This rule applies when `typecheck_document` processes a non-Dict expression (e.g., a `[call ...]`) that produces a `Record` type. The level increment ensures type variables introduced during inference are at ℓ+1 and therefore generalizable at ℓ_enc. The restore before generalization guarantees ℓ_current is in its correct state for subsequent expressions.

The Record type uses monomorphic (substitution-applied) types for the type map and downstream structural checks. The type schemes σᵢ live in Γ and are instantiated at each reference via [VAR-POLY].

**Level increments at document boundaries.** Each `infer_dict` call increments ℓ_current (via `ctx.current_level`), and `typecheck_document` also increments ℓ_current before inferring any non-Dict expression at a document boundary (both last and non-last positions). For `[a: [b: 42]]`, the outer dict runs at ℓ+1 and the inner dict at ℓ+2. For a non-Dict Record expression at a document boundary, ℓ_current is incremented to ℓ+1 before inference and restored to ℓ afterward, following the same protocol as `infer_dict`. This matches standard HM let-nesting: each binding scope increments the level.

**Forward references within letrec.** Within a single dict (letrec group), all entries share level ℓ+1 during Pass 3 inference. Forward references see the monomorphic αᵢ from Pass 1 — these are fresh TypeVars that participate in unification via `ctx.subst`, producing binding constraints. After Pass 4, downstream consumers of the dict see polymorphic schemes.

Mutually recursive entries constrain each other through unification during Pass 3. This is standard monomorphic letrec (OCaml, Haskell `let rec`) — entries see each other as monomorphic during inference, not polymorphic. Polymorphic recursion (Mycroft 1984) is not supported: it would require fixpoint iteration to convergence, which is more complex and can diverge. The monomorphic restriction is sufficient for tinct's use cases.

**Document-level scheme threading.** `typecheck_document` splats dict Record fields into the parent environment for downstream document expressions. To preserve polymorphism across `---` boundaries, schemes are stored in `Env` slots as `EnvSlot.scheme: Option<TypeValue>`. `typecheck_document` inserts these into the parent `Env` so downstream document expressions see polymorphic types via `Env.get_scheme()`.

**Interaction with `Unknown` and unannotated parameters:**

- Unannotated function parameters still receive type `Unknown` (not a fresh type variable). `[fn [x] x]` remains `Fn(Unknown → Unknown)`.
- `Unknown` in unification acts as a universal match ([U-UNKNOWN-L], [U-UNKNOWN-R]) but sets ℓ(α) = 0 for any type variable α it touches ([U-UNKNOWN-VAR], [U-VAR-UNKNOWN]), preventing generalization.
- Annotated type variables (e.g., `x@a`) create fresh type variables at ℓ_current. These participate in generalization normally.
- The practical effect: let-generalization benefits code that uses type annotations. `[id: [fn [x@a] x]]` generalizes `id` to `∀a. Fn(a → a)`; subsequent `[id 42]` and `[id "hello"]` each get independent instantiations.

**Interaction with CALL-POLY.** When a call expression targets a `VarRef`, the inference engine inspects the scheme directly before any instantiation. This determines the routing:

- **Polymorphic scheme** (`TypeValue.Scheme` with a non-empty `vars` dict): routes to `check_call_with_scheme`, which calls `instantiate_scheme_tv` once to produce a function TypeValue with fresh TypeVars at ℓ_current. It then checks whether inference variables remain in the post-instantiation type: if all variables were resolved (fully concrete), it takes the CALL-MONO path (bidirectional checking via `check_expr`); if TypeVars remain, it takes the CALL-POLY path (synthesize arguments, unify, apply substitution to return type). This avoids double instantiation.
- **Monomorphic binding** (bare TypeValue, no `TypeValue.Scheme` wrapper): routes to `check_call`, which infers the function expression normally. No instantiation occurs. The inferred type is typically concrete, so the CALL-MONO path fires directly.
- **Non-VarRef function expressions** (e.g., inline lambdas): always route to `check_call`.

**Substitution name uniqueness.** `ctx.subst` is keyed by TypeVar name. User-annotated type variables (e.g., `@a`) are mapped to fresh names by `resolve_type_name` during Pass 3 inference. Each function entry maintains its own `ann_mapping` (a per-function `HashMap<String, String>`), so `@a` in one function maps to a different name than `@a` in a sibling function. Within a single function, all references to the same annotation name `@a` resolve to the same TypeVar (ensuring constraints are shared as intended). After Pass 4 generalization produces `TypeValue.Scheme` values, `instantiate_scheme_tv` renames the quantified variables to fresh names at each call site, preventing cross-call-site interference.

**Error recovery.** If Pass 3 inference fails for an entry, `TypeValue.Unknown` is used for that entry. Level lowering from partial unification before the failure is retained in `ctx.levels` — this is conservative (may prevent generalization of some variables) but safe. Generalization in Pass 4 proceeds for successfully-inferred entries; failed entries use the bare `TypeValue.Unknown` without a scheme wrapper.

**Key invariants:**

1. **Level monotonicity:** ℓ_current only increases when entering binding scopes. Fresh variables are always created at ℓ_current.
2. **Generalization soundness:** Only variables with ℓ(α) > ℓ_enclosing are generalized, ensuring no variable escapes its scope. Level lowering during unification ([U-VAR-LEVEL], symmetric) prevents variables from being captured at too high a level. Unknown-touched variables have ℓ = 0, preventing generalization.
3. **Value restriction (not needed):** Tinct does not have mutable references, so the value restriction (Wright, 1995) is unnecessary. All bindings can be generalized safely.
4. **Occurs check:** Unchanged — prevents infinite types regardless of levels.
5. **Substitution idempotence:** Unchanged — transitive chasing is orthogonal to levels.
6. **Letrec monomorphism during inference:** Within a letrec group, entries see each other as monomorphic during Pass 3 (fresh type variables, not schemes). Polymorphism only becomes visible after Pass 4 generalization.
7. **TypeVar equality:** `TypeValue.Var` equality is by name (the `name` field in the payload dict). Levels are consulted only during generalization (via `ctx.levels`) — they are not compared for equality.

**Key implementation types:**

| Component | Specification |
|-----------|--------------|
| `TypeValue.Var` | `Variant { ctor: "TypeValue.Var", payload: { name: String } }` — level stored in `ctx.levels`, not in the value |
| `EnvSlot.scheme` | `Option<TypeValue>` — `None` for monomorphic, `Some(TypeValue.Scheme {...})` for polymorphic |
| `TyConEnv.tycon_defs` | `HashMap<String, Arc<TyConDef>>` — unified type declaration store; `Arc` wrapping enables pointer-identity checks in UNIFY-TYCON |
| `Env::get_scheme()` | Returns `Option<TypeValue>` — the stored scheme (if any) |
| `Env::insert_scheme()` | `fn(name, Option<TypeValue>)` |
| `infer_expr` VAR case | `instantiate_scheme_tv(env.get_scheme(name)?, &mut state.ctx, state.ctx.current_level)` |
| `infer_dict` | Passes 0–4: key resolution, placeholder binding, type-alias pass, value inference+unify, generalize |
| `typecheck_document` | Splats `EnvSlot.scheme` values into parent env across `---` boundaries |
| `instantiate_scheme_tv()` | `fn(scheme: &TypeValue, ctx: &mut InferenceContext, level: u32) -> Option<TypeValue>` — `type_env.rs` |
| `generalize_tv()` | `fn(enclosing_level: u32, ty: &TypeValue, ctx: &InferenceContext) -> TypeValue` — `type_env.rs` |
| `constrain()` U-VAR | Bind via `ctx.bind()` + symmetric level lowering via `ctx.lower_var_level()` |
| `constrain()` U-ANY + TypeVar | Level set to 0 via `ctx.lower_var_level(name, 0)` to prevent generalization |
| `InferenceContext` | `{ current_level: u32, levels: HashMap<String, u32>, subst: HashMap<String, Arc<Value>>, tycon_env: HashMap<String, Arc<TyConDef>>, resolver_deferred: Vec<(Arc<Value>, Arc<Value>)>, gensym_counter: u64 }` — in `type_infer.rs` |
| `ctx.free_vars()` | Collects unbound TypeVar names from a TypeValue; walks settled Dict payloads recursively |

Polymorphic builtin signatures (e.g., `map: ∀a b. Fn(Fn(a → b) × Seq(a) → Seq(b))`) are expressed via type schemes — see [Type System Extensions](07-type-extensions.md).

**Principal types.** Tinct infers principal types for fully-annotated polymorphic functions where no type variable unifies with `Unknown`. For partially-typed code, the inferred type depends on the checking context — subsumption introduces multiple valid types for the same expression (e.g., `42` can check against `IntLiteral(42)`, `Int`, `Number`, or `Top`). Full Damas-Milner principality is not achieved because: (a) unannotated parameters receive `Unknown` rather than fresh type variables, (b) singleton literal types introduce subtyping which bidirectional checking mediates but which prevents a unique most-general type, and (c) [U-SUBSUME] in CALL-POLY means the type variable binding may be more or less precise depending on argument order (both bindings are sound, but they differ).

**References:** Kiselyov, O. (2013). "How OCaml type checker works — or what polymorphism and garbage collection have in common." Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." Wright, A. (1995). "Simple imperative polymorphism."

## Constrained Type Variables

Tinct implements **constrained type variables** to provide precise types for overloaded builtins and user-defined polymorphic functions. Constraints restrict which types can instantiate a type variable, enabling static rejection of invalid operations (e.g., `[= [fn [] 1] [fn [] 2]]`) while preserving parametric polymorphism for valid uses.

### Constraint Representation

A **constraint** is a `ConstraintDecl` TypeValue (Arc<Value>) where the payload specifies the type class name (e.g., `"Equatable"`) and the type variable name (e.g., `"a"`). Type schemes (TypeValue.Scheme) carry constraints alongside quantified variables:

```text
TypeValue.Scheme {
    type_vars: Vec<String>,
    constraints: Vec<Arc<Value>>,    // ConstraintDecl entries (Arc<Value>)
    doc: Option<String>,
    body: Arc<Value>,                // the type body as TypeValue
}
```

**Display format:** Constraints appear before the type body, separated by `=>`:

- `Equatable a => Fn@Boolean [a a]` — equality requires Equatable constraint
- `Numeric a, Castable String b => Fn@String [a b]` — multiple constraints comma-separated
- `Fn@Integer [Integer Integer]` — monomorphic schemes (no constraints) display as before

### Primitive Built-in Constraints

Four classes have primitive built-in instances whose dispatch is handled by the Rust runtime. These instances cannot be overloaded by user-defined classes for the primitive operators (`=`, `<`, `+`, `str`):

| Class | Primitive instances | Example builtins |
|-------|---------------------|-----------------|
| `Equatable` | Int, IntLiteral, Float, Str, StringLiteral, Bool, Number | `=` |
| `Comparable` | Int, IntLiteral, Float, Str, StringLiteral, Number | `<`, `>`, `<=`, `>=` |
| `Numeric` | Int, IntLiteral, Float, Number | `+`, `-`, `*`, `/` |
| `Castable` | String target: Int, Float, Str, Bool, Bytes, Dict, Map | `cast`, `str` |

**Rationale:** Function, Seq, and Record are excluded from Equatable because structural equality would force lazy thunks, violating lazy evaluation semantics.

All other classes (`Mappable`, `Appendable`, `Functor`, `Applicative`, `Monad`, `Foldable`, `Traversable`) are declared in the stdlib using `[class ...]` and `[instance ...]` forms and are fully user-extensible — see §Higher-Kinded Types and Type Classes.

### Constraint Propagation over BAS Types

When a constraint `C(τ)` is checked and τ is a compound BAS type, propagation rules distribute the constraint over the type's structure (Garcia, Clark & Tanter 2016; Castagna & Lanvin 2017):

```text
[CONSTRAIN-FIELD]   C({f: τ}) ⊢ satisfied    iff    C(τ) ⊢ satisfied
[CONSTRAIN-INTER]   C(τ₁ & τ₂) ⊢ satisfied  iff    C(τ₁) ⊢ satisfied ∧ C(τ₂) ⊢ satisfied
[CONSTRAIN-UNION]   C(τ₁ | τ₂) ⊢ satisfied  iff    C(τ₁) ⊢ satisfied ∧ C(τ₂) ⊢ satisfied
[CONSTRAIN-TOP]     Castable(⊤) ⊢ satisfied
                    C(⊤) ⊢ error   for C ∈ {Equatable, Comparable, Numeric, Mappable, Appendable}
[CONSTRAIN-UNKNOWN] C(?) ⊢ satisfied             (AGT existential — deferred to runtime ClassEnv)
[CONSTRAIN-NEVER]   C(⊥) ⊢ satisfied             (⊥ is uninhabited — vacuously true)
```

**[CONSTRAIN-FIELD]** applies only to built-in classes with compositional/structural semantics (`Equatable`, `Comparable`, `Castable`, `Numeric`, `Mappable`, `Appendable`). User-defined classes do not automatically propagate over record fields unless declared with the appropriate instance.

**[CONSTRAIN-UNION]** direction is `∧` (ALL members) — a union-typed value could be either alternative at runtime, so both branches must satisfy the constraint. Implementors: use `all()`, not `any()`.

**[CONSTRAIN-TOP]** distinction: `⊤` concretizes only to itself (`γ(⊤) = {⊤}`), so `Equatable(⊤)` requires Top to be a literal Equatable instance — it is not. `Castable` is the sole exception because `str`/`cast` is defined as a total function by policy. `?` concretizes to all static types (`γ(?) = STypes`), so `Equatable(?)` is existentially satisfied and deferred to runtime ClassEnv dispatch.

**Normalization ordering:** BAS normalization must complete before constraint propagation fires. `satisfies_constraint` is called on already-normalized types.

### Constraint Generation and Checking

1. **Builtin registration** (`build_builtins_type_env()` in `builtins.rs`): Overloaded builtins are registered with constrained type schemes:

   ```text
   =  : Equatable a => a → a → Bool
   <  : Comparable a => a → a → Bool
   +  : Numeric a => a → a → a
   str:  Castable String a => a → Str
   cast: Castable target a => a → target
   ```

2. **Instantiation** (`instantiate_scheme`): When a constrained scheme is instantiated, constraints are copied with renamed variables:

   ```text
   scheme: Numeric a => a → a → a
   instantiate → fresh var _t0, constraint Numeric _t0
   result: Fn@_t0 [_t0 _t0]
   state.constraints += ConstraintDecl { class: "Numeric", var: "_t0" }  (Arc<Value>)
   ```

3. **Constraint checking** (`unify`): When binding a type variable α to a concrete type τ (U-VAR-LEVEL arm), check all constraints on α:

   ```text
   For each constraint C(α) in state.constraints:
       if ¬satisfies_constraint(τ, C):
           error "type τ does not satisfy constraint C"
   ```

   Example: unifying `_t0` (with `Numeric _t0`) with `Fn@Integer [Integer]`:

   ```text
   satisfies_constraint(Fn@Integer [Integer], "Numeric") → false
   → TypeError: type Fn@Integer [Integer] does not satisfy constraint Numeric
   ```

4. **Generalization** (`generalize`): Constraints on generalized variables are included in the resulting TypeValue.Scheme:

   ```text
   state.constraints: [Numeric _t0, Castable String _t1]
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
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 1:2:7
  |
  2 | result: [+ 1 "hello"]
    |       ^
```

Alternative failing case:

```tinct
result: [= [fn [] 1] [fn [] 2]]

# Type inference
1. Look up = → Equatable a => a → a → Bool
2. Instantiate → Equatable _t0, Fn@Boolean [_t0 _t0]
3. Argument 1: infer([fn [] 1]) → Fn@Integer []
4. Unify _t0 with Fn@Integer []:
   - Check: satisfies_constraint(Fn@Integer [], "Equatable") → false
   - Error: "type Fn@Integer [] does not satisfy constraint Equatable"
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 2:1:7
  |
  1 | result: [= [fn [] 1] [fn [] 2]]
    |       ^
```

### Multi-Parameter Type Classes and Functional Dependencies

Tinct's MPTC/FD system is grounded in **Constraint Handling Rules** (CHRs, Sulzmann et al. 2007), which unify functional dependencies (propagation rules `==>`) and type-stage functions (simplification rules `<=>`). The central mechanism is `normalize()`, called before every `unify` step.

**`TypeValue.StageApp` — lazy type-stage application.** When FD improvement fires and the determining positions are not yet ground, the type checker produces `TypeStageApp { fn_name, args }` rather than calling the resolver eagerly. `normalize()` reduces it to a concrete type when args become ground. When any determining position is `Unknown`, the result is `Unknown` directly (not deferred indefinitely).

**`NormCtxt` — normalization context.** `normalize()` takes a `NormCtxt` carrying everything needed for a complete reduction pass: the current substitution chain (`subst`), the type-stage environment for calling resolver functions (`type_stage_env`), the alias table (`alias_env`), the current depth and max depth for the step limit, and the in-progress resolver call stack for cycle detection (`call_stack`). A normalization cache (`resolver_cache`) memoizes ground-arg results — same inputs always produce the same output under resolver purity. A fresh `NormCtxt` is constructed from the current `InferState` before every `unify` call.

**FD elaboration into equality goals.** When `[$Addable a b c]` is registered with FD `(a,b)→c` and resolver `AddResult`, `c` is immediately unified with `TypeStageApp("AddResult", [a, b])`. As `a`, `b` become ground, `normalize()` fires the resolver via `improve_functional_dependency` and `c` takes on a concrete type. `c`'s level is set to `max(enclosing_level, max(ℓ_a, ℓ_b))` at constraint-creation time. The FD constraint propagates in the type scheme alongside `c` (Jones 1995 qualified types), so the FD fires correctly at every call site.

**BAS-aware deferral.** `improve_functional_dependency` fires only when all determining positions are atomic named monotypes (`Int`, `Float`, `Str`, etc.). Union, intersection, negation, `Unknown`, and free TypeVars in a determining position defer improvement — the resolver is not called until the position resolves to a concrete ground type. This is the conservative, sound approach under Boolean-Algebraic Subtyping: distributing improvement over union types (e.g., `Add (Int|Float) Int c ⟹ c = Int|Float`) requires proving the resolver is covariant on the subtype lattice and is deferred to future work.

**Arithmetic operators** use `Addable`, `Subtractable`, `Multipliable`, `Divisible` classes with FD `(a,b)→c` and resolver functions (`AddResult`, etc.) declared in `stdlib/prelude.llt`. Instances cover Int/Float combinations. User-defined numeric types add instances — no code change required.

**Deferred equality for non-injective resolvers.** When `unify_normalized` encounters two `TypeStageApp("F", _)` nodes from different elaboration sites, behavior splits on `ClassDecl.resolver_injective` (computed during the batch instance coherence check):

- **Injective F:** unify args pairwise (congruence — sound).
- **Non-injective F** (all arithmetic classes — `AddResult(Int,Float)=Float=AddResult(Float,Float)`): add `(lhs, rhs)` to `InferState.deferred_equalities`. After each `unify` call, if both sides have reduced to concrete types, fire `unify(concrete_lhs, concrete_rhs)`. This prevents false type errors on `[= [+ 1 2.0] [+ 1.5 2.5]]` (both produce `Float` via different arg types).

**Class and instance declarations.** Classes use the two-bracket form; instances use match-arm syntax:

```tinct
--- stage: type
[AddResult: [fn [...args] [match ... ]]]   # resolver function
---
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]

[instance Addable
  [pattern [a@Integer  b@Integer   c@Integer  ]]: [+: [fn@Integer   [x@Integer   y@Integer  ] [builtin-add x y]]]
  [pattern [a@Integer  b@Float c@Float]]: [+: [fn@Float [x@Integer   y@Float] [builtin-add x y]]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 3:4:8
  |
  4 | Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
    |        ^
```

The structural metadata bracket (`[determines: ... resolver: ...]`) is the second positional argument to `[class ...]`. Classes are **scope-resident values** in the TypeEnv, not global registry entries. `[$Addable a b c]` resolves the class via the `$`-sigil from current scope at constraint-creation time.

**Instance soundness.** At the batch instance coherence check (after all `[instance ...]` forms for a class are processed): disjointness (no two arms match the same type tuple), coverage (no fresh TypeVars in determined positions), and consistency (if two arms' determining positions unify, their determined types must agree) are all verified. Violations are rejected with diagnostic messages naming the conflicting arms.

**Coherence.** Two instances with the same determining-position tuple but different determined types are rejected (consistency condition, Jones 2000). Instances are closed within a compilation unit — cross-file includes contribute arms, and the full accumulated set is checked before the first constrained expression.

**Cross-arity entailment.** `Addable a b c` does not automatically entail `Numeric a`. The closed instance set restricts operands to numeric types — any non-numeric operand produces a type error at the call site. Same-arity superclass entailment (`Comparable a` entails `Equatable a`) traverses `ClassDecl.superclasses`.

See `doc/feature/chr-unification.md` for the complete formal specification including normalization algorithm, resolver soundness obligations, TypeStageApp unification rules, and boundary guard elaboration.

### Nested Dict Polymorphism

When a dict literal is bound by name (e.g., `helpers: [id: [fn [x] x] ...]`), each entry in the dict is individually generalized at the dict boundary. The outer binding stores a `TypeValue.Scheme` whose `body` is a `TypeValue.Record` with field types that are themselves `TypeValue.Scheme` values. Dot-access on a `VarRef` target retrieves the field's scheme from the body record and instantiates it via `instantiate_scheme_tv`:

```text
Γ(d) = TypeValue.Scheme { vars: ..., constraints: ..., body: Record(fields) }
fields(f) = TypeValue.Scheme { vars: α₁...αₙ, constraints: ..., body: τ_f }
τ' = instantiate_scheme_tv(fields(f), ℓ_current)
──────────────────────────────────────── [DOT-POLY]
Γ ⊢ d.f : τ'
```

This applies only when `d` resolves to a visible dict literal (VarRef to a dict-binding in scope). For opaque function parameters, cross-file imports via opaque types, or non-VarRef expressions, the bare record field type from the TypeNode is used — no polymorphic instantiation. This mirrors ML's structure/signature distinction: visible literals carry full polymorphic schemes; opaque dict parameters expose only their declared interface (Wells 1999).

**References:** Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc." Jones, M.P. (1995). *Qualified types: Theory and practice.* Jones, M.P. (2000). "Type classes with functional dependencies." Sulzmann, M. et al. (2007). "Understanding FDs via CHR." Garcia, R. et al. (2016). "Abstracting gradual typing." Castagna, G. & Lanvin, V. (2017). "Gradual typing with union and intersection types."

## Higher-Kinded Types and Type Classes

Tinct supports rank-1 higher-kinded types (Jones 1993 constructor classes) via an extension to the kind system. This enables a generic Functor/Applicative/Monad hierarchy, `[do]` monad inference, and precisely-typed `get`/`get-in` via label polymorphism.

### Kind System

The kind grammar has four kinds:

```text
Kind ::= *         -- concrete types (Int, Str, Record, ...)
       | Row        -- record field sets
       | Operator   -- type constructors (* → *, written `Operator`)
       | Label      -- type-level string labels (for HasField constraints)
```

`Operator` is notation for `* → *`. A TypeVar of kind `Operator` ranges over type constructors; a TypeVar of kind `Label` ranges over string field names.

The kind of each TypeVar is tracked in `InferState.kind_env: HashMap<String, Kind>`. TypeVars of kind `Operator` arise from `@Operator` annotations on class parameters; TypeVars of kind `Label` arise from Label annotations (`key@Label` for anonymous or `key@[label: l]` for named).

### Type Constructor Application

Two TypeValue variants for higher-kinded types:

- `TypeValue.App` (TV_APP) — type constructor applied to an argument: `App(Result, Int)` is `Result Int`
- `TypeValue.Op` (TV_OP) — a type constructor variable: `Operator("m")` for a Monad variable `m`

In annotation positions, `[f a]` (no colons) is type constructor application when `f` is Operator-kinded or a user type alias. `@[m a]` applies constructor `m` to argument `a`.

**Unification:**

```text
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

Classes are declared with `[class [params] [structural-metadata] methods...]`. The second positional bracket carries `determines:`, `resolver:`, `kinds:`, and `superclasses:` — omit it for classes with no FDs or kind constraints:

```tinct
Functor: [class [f]  [kinds: [f: Operator]]
  fmap: [fn@[return: [f b]] [g@[Fn@b [a]]  xs@[f a]]]]

Equatable: [class [a]
  eq?: [fn@Boolean [a a]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 4:1:8
  |
  1 | Functor: [class [f]  [kinds: [f: Operator]]
    |        ^
```

Instances use match-arm syntax with `[pattern [...]]` arm keys:

```tinct
[instance Functor
  [pattern [f@Seq  ]]: [fmap: [fn@[return: [Seq b]]   [g@[Fn@b [a]]  xs@[Seq a]] [map g xs]]]
  [pattern [f@Maybe]]: [fmap: [fn@[return: [Maybe b]] [g@[Fn@b [a]]  m@[Maybe a]]
                 [match m  [Some v]: [Some [g v]]  None: None]]]]
=== error
type errors:
  undefined type: Maybe at 3:15-3:20

```

**Superclasses** use `superclasses:` in the structural bracket:

```tinct
Comparable: [class [a]  [superclasses: [Equatable]]
  lt?: [fn@Boolean [a a]]]

Monad: [class [m]  [kinds: [m: Operator]  superclasses: [Applicative]]
  bind: [fn@[return: [m b]] [ma@[m a]  k@[Fn@[return: [m b]] [a]]]]]
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 6:1:11
  |
  1 | Comparable: [class [a]  [superclasses: [Equatable]]
    |           ^
```

The superclass chain provides constraint entailment. Functions constrained by `[a: Comparable]` can call `eq?` from `Equatable` without an additional explicit constraint. Superclass instances must exist before a subclass instance can be declared.

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
| `Castable` | `* → *` | — | `cast : source → target` (two params: target, source) |

**†`Mappable` kind:** `Mappable` is registered with `Kind::Operator` (type constructor kind `* → *`). User-defined types can declare themselves `Mappable`.

Instances cover `Result`, `Seq`, `Maybe`, `Record` as appropriate.

### Generic Functions

The following generic functions are defined in `stdlib/prelude.llt`:

```tinct
# collect effects from any Traversable container
sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
  [traverse f [fn [x] x] xs]]

# map with effects over any Traversable
traverse: [fn@[f [t b]] [f@Monad  t@Traversable  fn@[f b] [a]  xs@[t a]]
  [t.traverse f xs]]

# forM, when, liftM2 also defined
=== error
error: `:` can only appear in dict, call, class, instance, or match forms
 --> block 7:2:9
  |
  2 | sequence: [fn@[f [t a]] [f@Monad  t@Traversable  xs@[t [f a]]]
    |         ^
```

### `[do]` Inference

The `[do]` macro supports both an explicit monad argument and an inferred form. The inferred form omits the monad argument; the type checker deduces the monad from context and wires it to the evaluator via a `%do-infer` sentinel in the desugared AST.

```tinct
# Explicit form — always works
[do result
  [r: [fetch %nc url]]
  [r.body]]

# Inferred form — return annotation has ok+err fields → type checker resolves result monad
[fn@[ok: String  err: String] [url@String]
  [do
    [r:    [Ok "hello"]]
    [Ok r.body]]]
```

**Inference rules (applied in order):**

1. **Rule 1 — annotation:** If the enclosing function's return type annotation has `ok` or `err` fields (structural Result-like record), resolve to the `result` monad dict.
2. **Rule 2 — first binding (type-level):** If the first binding's RHS infers as a Record with `ok` or `err` fields, resolve to `result`.
3. **Rule 2b — first binding (syntactic fallback):** If Rule 2 type-level resolution fails, inspect the first binding's RHS AST. If it is an implied constructor call to `Ok` or `Error` (i.e., `Expr::Call { func: VarRef("Ok" | "Error"), implied: true, .. }`), resolve to the `result` monad. This handles nominal variant types whose constructor types are not yet tracked. The check is case-sensitive and only recognizes uppercase constructor names.
4. **Rule 3 — failure:** If no rule succeeds, emit `T_DO_INFER` type error and leave `%do-infer` unresolved; the evaluator produces `[E002] undefined variable: %do-infer`.

The explicit `[do monad ...]` form always takes priority and is backward-compatible.

**Known limitations:**

- Rule 2 (type-level) recognizes structural records (`{ok: x}`) but not nominal variant calls (`[Ok x]`). Nominal variant dispatch requires precise constructor types in the type environment; `[do result ...]` is the workaround for result-monad patterns.
- Rule 2b (syntactic fallback) recognizes only `Ok` and `Error` constructors for the `result` monad.
- `Maybe` monad inference and other HKT monads require full `App(m, a)` type constructor tracking; use explicit `[do monad ...]` form for these cases.

### HasField — Label-Polymorphic Field Access

`HasField l d a` is a qualified-type constraint asserting that record type `d` has a field at label `l` with type `a`. It carries a functional dependency `(l, d) → a` — given label and dict type, the field type is uniquely determined (Jones 1994).

**`get` is label-polymorphic:**

```text
get : ∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a
```

Field access is precise: `[get "host" config]` returns the type of `config.host`, not `Unknown`.

**Instance resolution rules:**

```text
HasField (Concrete l) Record(fields) τ         when l ∈ dom(fields) and fields(l) = τ
HasField l (τ₁ | τ₂) (a₁ | a₂)               distributes over union [HAS-FIELD-UNION]
HasField l (τ₁ & τ₂) (a₁ & a₂)               distributes over intersection
HasField l ⊤ Unknown                            for BAS-collapsed disjoint-field unions
HasField l Unknown Unknown                      gradual typing fallback
```

**Label TypeVars** are introduced by Label annotations (`key@Label` for anonymous or `key@[label: l]` for named) and tracked in `kind_env` as label-kinded TypeVars. They are generalized into the `vars` dict of the enclosing `TypeValue.Scheme` (with a label kind stored in their `VarDecl`) and re-instantiated at call sites via `instantiate_scheme_tv`.

**Union distribution** is the key BAS contribution — `get "port" (A | B)` returns `A.port | B.port`, not `Unknown`.

### BAS Interaction with HKT

BAS operates on types of kind `*`. With HKT:

- `App(f, a)` is a BAS lattice atom for each concrete `(f, a)` pair
- **Covariant functorial subtyping:** `a <: b` implies `App(f, a) <: App(f, b)` for covariant functors (all stdlib Functor instances)
- **Join (one-directional):** `App(m, a) | App(m, b) <: App(m, a | b)` — the reverse is unsound for diagonal functors

Label-kinded TypeVars are phantom indices — they do not introduce BAS lattice atoms. `HasField` constraints are resolved eagerly before BAS normalization to prevent S-RcdTop from collapsing union dict types to ⊤.

**References:** Jones, M.P. (1993). "A system of constructor classes." Jones, M.P. (1994). "Qualified types." Gaster, B.R. & Jones, M.P. (1996). "A polymorphic type system for extensible records." Castagna, G. (2023). "Typing records, maps, and structs." ICFP.

## Type Classes and Higher-Kinded Types — Formal Rules

This section formalizes the kind system, type constructor application, constraint generation, and instance resolution for higher-kinded types and type classes. The implementation supports rank-1 higher-kinded types (Jones 1993) with kind inference for class parameters.

### Kind System

Kinds classify types by their arity. The kind grammar:

```text
κ ::= *           concrete types (Int, Str, Record, TypeVar)
    | Row         record field sets (internal, not user-exposed)
    | Operator    type constructors (* → *, written Operator)
    | Label       type-level string labels (for HasField constraints)
```

Kinds are tracked in `InferState.kind_env: HashMap<String, Kind>`. TypeVars default to kind `*`. Operator-kinded TypeVars arise from `@Operator` annotations on class parameters. Label-kinded TypeVars arise from Label annotations (`key@Label` or `key@[label: l]`).

### KIND-CLASS-PARAM — Class Parameter Kind Registration

When a ClassDecl is processed, class parameters with `@Operator` annotations are registered in `kind_env`:

```text
Γ ⊢ [class [C f@Operator] ...]
For each param (name, Kind::Operator) in ClassDecl.params:
    kind_env[name] ← Kind::Operator
```

**Implementation:** `src/typecheck.rs:1869-1874`. After ClassDecl registration in `class_env`, a loop populates `state.kind_env` for Operator-kinded params.

**Current limitation:** The parser (`src/parser.rs:4564`) extracts plain identifiers from class headers and does not preserve `@Operator` annotations. The kind environment is seeded via hardcoded mappings in `InferState::new()` (e.g., `Mappable` → `Kind::Operator`). User-defined class declarations cannot declare `Operator`-kinded parameters via `@Operator` syntax; only built-in classes pre-registered in `InferState` carry the correct kind.

### KIND-OPERATOR — Type Constructor Application

Type constructor application `[f a]` (no colons) resolves to `TypeValue.App(Operator(f), a)` when `f` is Operator-kinded in `kind_env`. This check occurs before the union type path to prevent `[m Int]` from being parsed as `Union(Operator("m"), Int)`.

```text
resolve_type_expr([f a], env, state, ...) where all_positional ∧ len = 2:
    if kind_env[f] = Operator:
        f_type ← Operator(f)
        a_type ← resolve_type_expr(a, ...)
        if a_type is Operator(_):
            error "rank-2 type constructor application not supported"  [rank-1 restriction]
        return App(f_type, a_type)
    else:
        ... fall through to union type path
```

**Implementation:** `src/typecheck_annot.rs:1703-1730`. Checks `state.kind_env.get(f_name)` for `Kind::Operator` before union path. Rejects `App(Operator(_), Operator(_))` (rank-2 application) with a user-facing error.

**Rank-1 restriction:** `App(Operator("f"), Operator("g"))` (applying one Operator variable to another) is excluded. Multiple flat Operator quantifiers in one method type are allowed — `traverse` has both `f@Applicative` and `t@Traversable` in its signature, which is rank-1. The restriction prevents rank-2 polymorphism (higher-rank constructors), which would require impredicative instantiation and significantly complicates inference.

### UNIFY-OPERATOR / UNIFY-APP — Unification Rules

Type constructor variables unify via occurs check and binding, analogous to `TypeVar` unification:

```text
UNIFY-OPERATOR:
  m ∉ ftv(T)    (occurs check prevents infinite kinds)
  ──────────────────────────────────
  unify(Operator(m), T) = S[m ↦ T]

  unify(Operator(m), Operator(n)) = S[m ↦ Operator(n)]   (symmetric)
```

Type constructor applications unify via decomposition (standard Robinson, constructor then argument):

```text
UNIFY-APP:
  unify(f₁, f₂) = θ₁    unify(θ₁(a₁), θ₁(a₂)) = θ₂
  ─────────────────────────────────────────────────────
  unify(App(f₁, a₁), App(f₂, a₂)) = θ₂ ∘ θ₁
```

**Implementation:** `src/type_unify.rs` — `unify()` UNIFY-OPERATOR and UNIFY-APP arms. UNIFY-OPERATOR binds Operator variables in `subst.type_map` with occurs check (`m ∉ ftv(T)`). UNIFY-APP delegates to recursive `unify()` calls, relying on substitution application at the top of `unify()` to thread bindings from constructor unification into argument unification.

**Normalization:** After unification resolves an `Operator` variable, the substitution is applied to the App structure recursively. `App(TyCon("Seq"), T)` remains as a TypeNode — `Seq` is represented uniformly as `App(TyCon("Seq"), T)` without a dedicated special case. See `apply_type()` in `src/type_unify.rs` for the implementation.

### Constraint Generation

Constraints are generated when a constrained type scheme is instantiated at a call site. Each constraint in the scheme is copied with renamed type variables.

```text
instantiate_scheme_tv(σ, ℓ_current, state) where σ = ∀(α₁...αₙ). [C₁ a₁, ...] τ:
  For each αᵢ: fresh_var ← TypeVar(_tN, ℓ_current)
               S[αᵢ ↦ fresh_var]
               ctx.levels[_tN] ← ℓ_current
  For each constraint Cᵢ aᵢ in σ.constraints:
      state.constraints.push(ConstraintDecl { class: Cᵢ, var: S(aᵢ) })  (Arc<Value>)
  return S(τ)
```

**Example:** Instantiating `Numeric a => Fn@a [a a]` produces a fresh `_t0`, constraint `Numeric _t0`, and type `Fn@_t0 [_t0 _t0]`.

**Implementation:** `src/type_infer.rs:instantiate_scheme_tv`. Constraints are stored in `InferState.constraints: Vec<Arc<Value>>` (ConstraintDecl entries) and checked during unification.

### Entailment — Constraint Checking

Constraints are checked during type variable binding (U-VAR-LEVEL arm of `unify`). For each active constraint `C(α)`, when binding `α ↦ τ`:

```text
check_constraints_on_var(α, τ, state):
  For each constraint C(α) in state.constraints:
      if ¬satisfies(τ, C, state):
          error "type {τ} does not satisfy constraint {C}"
```

**Implementation:** `src/type_unify.rs` — `check_constraints_on_var()` function. Called from U-VAR-LEVEL after binding `α` in `subst`.

**Primitive vs dynamic resolution:**

- **Primitive classes** (`Numeric`, `Comparable`): hardcoded in `satisfies_constraint()` via allowlists (see `satisfies_constraint()` function in `src/type_unify.rs`). These are checked early, before prelude instances are loaded.
- **User-defined classes** (`Mappable`, `Appendable`, `Functor`, etc.): resolved via `InstanceEnv::resolve_instance()` (see `resolve_instance()` method in `src/type_env.rs`). Requires prelude instances to be propagated into `InferState.instance_env`.

**Entailment via superclass closure:** When simplifying constraints during generalization, `entails(class_env, context, target)` checks if `target` is directly present in `context` or implied via superclass relationships (see `entails()` function in `src/type_unify.rs`). For example, `Comparable a` entails `Equatable a` because Comparable has Equatable as a superclass.

### Dictionary Elaboration

Instance declarations register method implementations in `InstanceEnv`:

```text
Γ ⊢ [instance [C T] [method₁: impl₁] [method₂: impl₂] ...]
InstanceDecl {
    class_name: "C"
    instance_type: resolve_type_expr(T)
    method_types: { method₁ ↦ infer_expr(impl₁), ... }
}
InstanceEnv.instances[(C, T)] ← decl
```

**Implementation:** `src/typecheck.rs:1882-1952`. Instance methods are inferred as ordinary expressions; their types are stored in `InstanceDecl.method_types: HashMap<String, Arc<Value>>`.

**Superclass method inheritance:** Instance declarations may use `extends` to declare superclass relationships (`[class [Monad m@Operator] extends [Applicative m] ...]`). The `Monad` instance implicitly carries `bind` plus inherited `pure`, `lift2`, and `fmap` from the Applicative and Functor instances. The ClassEnv stores superclass chains (`ClassDecl.superclasses: Vec<Arc<Value>>` ConstraintDecl entries); instance resolution follows the chain to retrieve inherited methods.

### Parameterized Instance Head Resolution

When a constraint `C m` is active and `m` is later unified with a concrete type `T`, instance resolution looks up `[instance [C T'] ...]` and attempts unification:

```text
resolve_instance(class_name, target_type, state):
  For each instance decl with decl.class_name = class_name:
      freshened_inst_type ← instantiate_at_level(decl.instance_type, state)
      temp_subst ← clone(state.subst)
      if unify(freshened_inst_type, target_type, temp_subst, state) succeeds:
          freshened_methods ← apply temp_subst to decl.method_types
          return InstanceDecl { instance_type: freshened_inst_type, method_types: freshened_methods }
  return None
```

**Freshening:** Instance type variables are freshened at each resolution attempt (`src/type_env.rs:962`) to prevent variable leakage. For example, `AppendableSeq: [instance [Appendable [Seq b]] ...]` has `b` freshened to `_tN` at each resolution, then unified with the target type.

**Method substitution threading:** After unification succeeds, the `temp_subst` (which now binds instance type variables) is applied to method types (`src/type_env.rs:979-986`). This threads concrete types from the target into the method signatures.

**Implementation:** `src/type_env.rs:931-997`. Tries each candidate in order; returns the first that unifies. No overlap detection or backtracking — overlapping instances are undefined behavior.

### Inference Flow Example

```tinct
# User code
[fmap: [fn@[m b] [m@Monad  f@b [a]  xs@[m a]]
  [m.bind xs [fn [x@a] [m.pure [f x]]]]]]
=== error
error: unmatched closing bracket
 --> block 9:3:41
  |
  3 |   [m.bind xs [fn [x@a] [m.pure [f x]]]]]]
    |                                         ^
```

**Inference steps:**

1. **Constraint generation:** `m@Monad` annotation generates `Monad m` constraint, stored in `state.constraints`.
2. **TYPE-OPERATOR:** `[m a]` in return annotation resolves to `App(Operator("m"), TypeVar("b"))` via KIND-OPERATOR.
3. **Constraint checking:** When `m` is instantiated at a call site (e.g., `[fmap inc [Ok 42]]`), the evaluator infers `target_type = Result` and calls `resolve_instance("Monad", Result, state)`.
4. **Instance resolution:** `resolve_instance` finds `[instance [Monad Result] ...]`, freshens its type, unifies `Result` with the target, and returns the instance dict with method types.
5. **Method dispatch:** The `m.bind` field access resolves to the `bind` method from the MonadResult instance dict.

**References:** Jones, M.P. (1993). "A system of constructor classes: overloading and implicit higher-order polymorphism." Robinson, J.A. (1965). "A machine-oriented logic based on the resolution principle." Wadler, P. & Blott, S. (1989). "How to make ad-hoc polymorphism less ad hoc."

## Type-Stage Resolvers

A **resolver** is a tinct function declared in a `--- stage: type` section. It receives the determining type dicts as arguments and returns the determined type dict. The type checker calls the resolver at type-check time — not at runtime — when all determining positions are ground.

### Writing a Resolver

```tinct
--- stage: type
[
  AddResult: [fn [...args]
    [match [[builtin-dict-get 0 args]  [builtin-dict-get 1 args]]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Int"]]:   [kind: "named" name: "Int"]
      [[kind: "named" name: "Int"]    [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Int"]]:   [kind: "named" name: "Float"]
      [[kind: "named" name: "Float"]  [kind: "named" name: "Float"]]: [kind: "named" name: "Float"]
      ...:                                                             [kind: "named" name: "Unknown"]]]
]
---
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]
```

The resolver receives all determining types as a positional sequence (via `...args`); `[builtin-dict-get 0 args]` extracts the first, `[builtin-dict-get 1 args]` extracts the second. Each argument and the return value are **type dicts** in the standard schema (see §Type Dict Schema in [Type Annotations](05-type-annotations.md) §16).

### Naming a Resolver

The `resolver:` key in the class structural-metadata bracket specifies the resolver by name. The name must be bound in the type-stage Env — either the prelude `--- stage: type` section or the program's own `--- stage: type` sections:

```tinct
Addable: [class [a b c]  [determines: [[[a b] c]]  resolver: AddResult]
  +: [fn@c [a b]]]
```

The resolver `AddResult` must be declared in a `--- stage: type` section before the class declaration.

### Multi-Output Resolvers

For FDs with multiple determined variables, the resolver returns a `multi-output` dict keyed by variable name:

```tinct
--- stage: type
[
  DivModResult: [fn [...args]
    [match [[builtin-dict-get 0 args]  [builtin-dict-get 1 args]]
      [[kind: "named" name: "Int"]  [kind: "named" name: "Int"]]:
        [kind: "multi-output"
         q: [kind: "named" name: "Int"]
         r: [kind: "named" name: "Int"]]]]
]
---
DivMod: [class [a b q r]  [determines: [[[a b] [q r]]]  resolver: DivModResult]
  divmod: [fn@[record q: q  r: r] [a b]]]
```

### Composing Resolvers

Resolvers are ordinary type-stage functions — they can call other type-stage functions:

```tinct
--- stage: type
[
  NullableAddResult: [fn [...args]
    [or [AddResult [builtin-dict-get 0 args] [builtin-dict-get 1 args]]
        [kind: "named" name: "Null"]]]
]
```

### Depth Limit

If a resolver's type-stage evaluation exceeds the recursion limit (256 frames), the type checker raises:

```text
type-stage reduction depth exceeded while computing AddResult(...)
  check resolver for infinite recursion or increase --type-stage-depth
```

## `[do]` Desugaring

The `[do]` form is syntactic sugar for monadic bind chains. It is implemented as a macro transformer in `stdlib/prelude.llt`.

### Desugaring Rule

Each `[bind x: expr]` binding in a `[do ...]` form desugars to a `[>>= expr [fn [let x] body]]` chain:

```text
[do monad
  [bind x: expr₁]
  [bind y: expr₂]
  final-expr]

desugars to:

[monad.bind expr₁ [fn [x]
  [monad.bind expr₂ [fn [y]
    final-expr]]]]
```

In the concrete tinct syntax, bindings use named dict entry syntax. The monad dict provides `bind` and `pure` methods. The `[do]` macro threads the monad value through the chain:

```tinct
# Explicit monad argument
[do result
  [r:    [fetch %nc url]]
  [data: [from-json r.body]]
  [get "items" data]]

# Expands to:
[result.bind [fetch %nc url] [fn [r]
  [result.bind [from-json r.body] [fn [data]
    [get "items" data]]]]]
```

Entries without a key are plain expression steps (the value is ignored — used for effects):

```tinct
[do result
  [[validate-url url]]    # plain step — result discarded
  [r: [fetch %nc url]]
  r.body]

# Expands to:
[result.bind [validate-url url] [fn [_]
  [result.bind [fetch %nc url] [fn [r]
    r.body]]]]
```

### Monad Inference

When no explicit monad argument is given (`[do [bind x: expr] ...]`), the type checker infers the monad:

1. If the enclosing function has an explicit return type annotation `@T` where `T` unifies with `App(m, _)` for a registered `Monad m` instance, use that instance.
2. If the first binding's RHS infers as `App(m, a)` for a known `Monad m`, use that instance.
3. If neither provides context, require an explicit monad argument.

The explicit `[do monad ...]` form always takes priority. Backward-compatible — all existing `[do monad ...]` forms are unaffected.

### Result Monad Example

```tinct
fetch-and-parse: [fn@[ok: String  err: String] [url@String]
  [do
    [r:    [fetch %nc url]]         # inferred: Result monad from return type
    [data: [from-json r.body]]
    [get "items" data]]]
```

The return annotation `@[ok: Str  err: Str]` unifies with `App(Result, Str)`, so the monad is inferred as `result`. No explicit monad argument needed.

### Maybe Monad Example

```tinct
safe-lookup: [fn@[Maybe String] [config@Dict key@String]
  [do
    [section: [get? "section" config]]
    [value:   [get? key section]]
    value]]
# → [Some value-str] | [None] — short-circuits on first absent key
```

## Limitations and Non-Guarantees

1. **Mutually recursive entries are monomorphic with respect to each other.** Entries that form a cycle in the dependency graph are inferred together as a single letrec group; each constrains the others through unification. Non-mutually-recursive entries are SCC-decomposed and generalized independently before their dependents are inferred — this is the standard behavior. Polymorphic recursion (a function calling itself at a different type than its declaration) requires an explicit return type annotation and is rejected without one (Mycroft 1984, Henglein 1993).
