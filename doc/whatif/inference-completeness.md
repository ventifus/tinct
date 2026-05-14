# What If: Inference Completeness for tinct

**State:** Partially implemented — SCC-based binding group analysis is done (`src/typecheck_dict.rs`). Remaining: variadic `Seq(T)` and nested dict polymorphism.

What would it take to close the gaps between what tinct's HM inference engine
can express and what it actually infers — making tinct a fully polymorphic,
precisely typed general-purpose language?

## Current State

Tinct uses levels-based Hindley-Milner inference (Kiselyov 2013) with a
four-pass letrec algorithm (DICT-GEN). Three gaps were identified; the first
is now closed. Two gaps remain.

**Letrec monomorphism — Implemented.** SCC-based binding group analysis
(`src/typecheck_dict.rs`) closes this gap. Independent dict entries are
generalized before their dependents are inferred. The following now
type-checks correctly:

```tinct
[
  id:           [fn [x] x]
  result:       [id 42]       # ok — a=Int ✓
  other-result: [id "hello"]  # ok — a=Str ✓
]
```

**Nested dict polymorphism.** Even when an inner dict correctly generalizes
its entries in its own TypeEnv, outer access via dot-notation retrieves the
bare `Type` stored in the Record field — not the TypeScheme. Polymorphism is
structurally lost at the boundary.

```tinct
helpers: [
  id: [fn [x] x]    # ∀a. Fn(a→a) in helpers' TypeEnv
]
result-a: [helpers.id 42]       # ok — id unified with Int
result-b: [helpers.id "hello"]  # type error — id already bound to Int
```

**Variadic params typed as Unknown.** `...args` collects remaining arguments
into an Int-keyed Dict at runtime. The type system assigns `Unknown` because
the Int-keyed Dict has no typed element representation. Variadic args are
invisible to type checking.

```tinct
sum: [fn [...nums] [reduce [fn [a b] [+ a b]] 0 nums]]
[sum 1 "two" 3]    # no type error — should fail
```

### What's Missing

1. ~~Independent generalization of dict entries~~ — **Implemented** (`src/typecheck_dict.rs`).
2. Polymorphic scheme instantiation when accessing entries through visible nested dicts.
3. Typed collection of variadic arguments with element-type inference at call sites.
4. Typeclass-based heterogeneous variadic patterns (printf-style) for cases where argument types vary by position.

## Why Inference Completeness Matters for tinct

**Library-style dicts become polymorphic.** A dict of utility functions is the
tinct idiom for a module. Today, calling a polymorphic utility twice with different
types requires redundant annotations or splitting the dict. With SCC decomposition,
the utility is generalized before its users are checked — exactly as `let f = ...`
works in any ML.

**Nested namespaces work as expected.** `math.clamp`, `str.words`, `db.query` — dicts
nested inside dicts are tinct's namespacing idiom. Every entry should expose its full
polymorphic type to the outer context. This is the tinct equivalent of ML's module
system.

**Variadic functions are type-safe.** `[sum 1 2 3]` inferring `Seq(Int)` and rejecting
`[sum 1 "two" 3]` is basic type safety the language cannot currently provide for
variadics. The stdlib's threading macro `->`, collection predicates `all-of`/`any-of`,
and string interpolation `str` all use variadics — all should be precisely typed.

**Precise library interfaces.** Stdlib functions that today advertise `Unknown` for
their variadic parameters become self-documenting: their type signatures express
constraints, element types, and return types precisely.

## Design

### SCC-Based Binding Group Analysis — Implemented

**Status: Implemented** in `src/typecheck_dict.rs` (`compute_sccs`, `collect_dependencies`, per-SCC Pass 1/3/4 loop). The following describes the algorithm for reference.

DICT-GEN is extended with a dependency analysis phase between Pass 0 and Pass 1.

```
Pass 0  — Key resolution: unchanged.

Pass 0a — Dependency graph: build a directed graph G over the n entries.
           For each entry kᵢ, collect all sibling-name references in eᵢ.
           Add edge kᵢ → kⱼ for each reference to sibling kⱼ.
           References to outer-scope bindings are ignored — they are
           already generalized at their own level.

Pass 0b — SCC decomposition: compute strongly connected components of G
           via Tarjan's algorithm (O(V+E)). Topologically sort the SCCs
           into groups G₁, G₂, ..., Gₘ such that all dependencies of Gᵢ
           appear in earlier groups.

Passes 1–4 — Process each group Gᵢ in topological order:
  Pass 1ᵢ: bind Gᵢ's entries to fresh TypeVars at level ℓ+1.
  Pass 2ᵢ: register type aliases within Gᵢ.
  Pass 3ᵢ: infer Gᵢ's entries (same 3a–3d sub-passes as today).
  Pass 4ᵢ: generalize Gᵢ's entries; update Γ' with TypeSchemes.
  Subsequent groups see fully generalized schemes for all Gⱼ, j < i.

Build Record(...) from finalized field types as today.
                                               [DICT-GEN-SCC]
```

**No value restriction.** Tinct is pure and lazy — no mutable references exist.
Every dict entry in a singleton SCC is unconditionally generalized (Damas &
Milner 1982). This is strictly more permissive than OCaml's relaxed value
restriction (Garrigue 2004), which is a conservative approximation for languages
with mutation. In tinct, aggressive generalization is both safe and correct.

**Polymorphic recursion is rejected with a clear error.** A function that calls
itself at a different type than its declaration site is polymorphically recursive.
Because type inference with polymorphic recursion is undecidable in general
(Henglein 1993, Kfoury et al. 1990), tinct rejects it with: "polymorphic
recursion requires an explicit type annotation." An explicit `fn@T` annotation
on the recursive function resolves the ambiguity and allows inference to proceed.

With SCC decomposition, the failing example from §Current State now works:

```tinct
# Dependency graph: {id} → ∅, {result} → {id}, {other-result} → {id}
# SCCs: singleton {id}, singleton {result}, singleton {other-result}
# Processing order: id (generalized) → result → other-result
[
  id:           [fn [x] x]
  result:       [id 42]       # instantiates a=Int → Int  ✓
  other-result: [id "hello"]  # instantiates a=Str → Str  ✓
]
```

Genuinely mutually recursive entries form an SCC and remain monomorphic within
it — correct behavior, not a loss:

```tinct
# SCC: {even, odd} — 2-cycle, processed together
[
  even: [fn [n] [if [= n 0] true  [odd  [- n 1]]]]
  odd:  [fn [n] [if [= n 0] false [even [- n 1]]]]
  # even : Fn(Int→Bool), odd : Fn(Int→Bool) — correctly monomorphic
]
```

### Polymorphic Access Through Visible Nested Dicts

When dot-access resolves to an entry of a dict literal that is visible (in
scope) to the type checker, the entry's full TypeScheme is retrieved and
instantiated via [VAR-POLY] at the current level. The Record field type is used
only as a fallback for opaque dict parameters, where the original TypeEnv is
unavailable.

**Mechanism.** `infer_dict` already returns `(Type, HashMap<String, TypeScheme>)`
(verified at `src/typecheck_dict.rs:225–231`). The scheme map is added to the
binding's `TypeScheme` as a new field `inner_schemes: Option<HashMap<String, TypeScheme>>`.
When `check_dot_access` resolves `d.f`:

1. If `d` is a `VarRef(name)`, retrieve the binding's `TypeScheme` from `env.get(name)`.
   If `scheme.inner_schemes` is `Some(ref inner)`, look up `f` in `inner` and call
   `instantiate_scheme(field_scheme, state.level, state)` — the same `[VAR-POLY]` path
   already used for top-level variable references.
2. Otherwise (opaque parameter, conditional result, function argument, cross-file import),
   fall through to the existing `infer_expr` path. The bare `Type` from the `Record`
   field is returned without instantiation.

This is the least invasive change: `TypeScheme` gains one new optional field; no
side-table in `InferState` is needed; the scheme map travels through the `TypeEnv`
chain naturally and works for cross-file includes. `inner_schemes` is `None` for all
non-dict bindings (function parameters, imported builtins, etc.) — the visible-literal
boundary is enforced structurally.

**`TypeScheme` change** (`src/types.rs:1509`): add `pub inner_schemes: Option<HashMap<String, TypeScheme>>`. Default `None` in `TypeScheme::mono` and all existing construction sites.

**Binding site** (`src/typecheck_dict.rs`): after Pass 4 generalization, the caller
that stores the result scheme sets `inner_schemes: Some(field_schemes)` where
`field_schemes` is the scheme map from `infer_dict`.

**`check_dot_access`** (`src/typecheck.rs:2564`): add a `VarRef` fast-path before
calling `infer_expr`. If the target is `VarRef(name)` and the binding has
`inner_schemes`, instantiate from there; otherwise use the existing path.

```tinct
# Visible literal: TypeEnv threaded to the access site
helpers: [
  id:    [fn [x] x]       # ∀a. Fn(a→a)
  const: [fn [x _] x]     # ∀a b. Fn(a → Fn(b→a))
]
result-a: [helpers.id 42]             # a=Int  ✓
result-b: [helpers.id "hello"]        # a=Str  ✓
result-c: [helpers.const 42 "ignore"] # a=Int, b=Str  ✓

# Opaque parameter: only the declared Record field type is available
use-helpers: [fn [h@[id: Fn@a [a]]] [h.id 42]]
# h.id : Fn(a→a) at the declared type — no polymorphic instantiation here.
# Full polymorphic record parameters require annotations on the parameter type.
```

This stratification mirrors ML's module system: first-class dict literals
(analogous to ML structures) carry full polymorphic schemes when visible; opaque
dict types (analogous to ML signatures) expose only their declared interface.

### Typed Variadic Parameters: Seq[T]

`...args` changes runtime representation from an Int-keyed Dict to a `Seq(T)`,
where T is a fresh TypeVar β unified against each variadic argument at call sites.

**Inference rule for variadic functions:**

```
[FN-VARIADIC]:
  Γ, p₁:τ₁, ..., pₙ₋₁:τₙ₋₁, pₙ:Seq(β) ⊢ body : τ_ret
  β fresh at current level
  ─────────────────────────────────────────────────────
  Γ ⊢ [fn [p₁ ... pₙ₋₁ ...pₙ] body] : Fn(τ₁...τₙ₋₁, Seq(β) → τ_ret)
```

**Inference rule at call sites:**

```
[CALL-VARIADIC]:
  Γ ⊢ f : Fn(τ₁...τₙ₋₁, Seq(β) → τ_ret)
  Γ ⊢ a₁ : τ₁  ...  Γ ⊢ aₙ₋₁ : τₙ₋₁
  Γ ⊢ aₙ : υₙ  ...  Γ ⊢ aₖ : υₖ   (variadic args, k ≥ n-1)
  S = compose(unify(β, υₙ), ..., unify(β, υₖ))
  ────────────────────────────────────────────
  Γ ⊢ [f a₁...aₖ] : S(τ_ret)
```

All variadic arguments are unified against the same TypeVar β. Literal types
(`IntLiteral(n)`, `FloatLiteral(n)`, `StrLiteral(s)`) are widened to their
base types (`Int`, `Float`, `Str`) before unification, so `[sum 42 1]`
succeeds with β = `Int` rather than failing on `unify(IntLiteral(42),
IntLiteral(1))`. Heterogeneous variadic arguments produce a type error.

Within the function body, the variadic parameter `args` has type `Seq(β)` and
is directly iterable with all Seq operations:

```tinct
# Before: nums : Unknown
sum: [fn [...nums] [reduce [fn [acc n] [+ acc n]] 0 nums]]

# After: nums : Seq(α) where Numeric α — precisely typed and constrained
sum: [fn@[return: α  constraint: [α: Numeric]] [...nums]
  [reduce [fn [acc n] [+ acc n]] 0 nums]]

[sum 1 2 3]        # α=Int, result : Int  ✓
[sum 1.5 2.5 3.0]  # α=Float, result : Float  ✓
[sum 1 "two" 3]    # type error: unify(Int, Str) at variadic arg 2  ✓
```

### Typeclass-Based Heterogeneous Variadics

For patterns where the function type varies by argument position — the printf
problem — a recursive typeclass expresses the full type (Kiselyov et al. 2004):

```tinct
[FormatResult: [class [r@*]
  [apply-fmt: [fn@r [template@Str collected@Seq@Str]]]]]

[FormatStr: [instance [FormatResult Str]
  [apply-fmt: [fn [t args] [str-format t args]]]]]

[FormatFn: [instance [FormatResult r  constraint: [r: FormatResult  a: FormatArg]]
               [FormatResult [fn@r [a]]]
  [apply-fmt: [fn [t args] [fn [x] [apply-fmt t [conj args [show x]]]]]]]]

format: [fn@[return: r  constraint: [r: FormatResult]] [template@Str]
  [apply-fmt template []]]
```

Each argument application peels one layer from the `FormatResult` chain:

```tinct
[format "%d"]            # : Fn(Int → Str)
[format "%d %s"]         # : Fn(Int → Fn(Str → Str))
[format "%d items: %s"   # Fn(Int → Fn(Str → Str))
  42 "apples"]           # → "42 items: apples"
```

This is expressible via multi-parameter typeclasses with functional dependencies,
which the HKT typeclass infrastructure provides. No new type system features are
required beyond what MPTCs already supply.

### Limitations

**Opaque dict parameters are not polymorphic.** A function that receives a dict
as a parameter and accesses a field sees the declared field type, not a
polymorphic scheme. Fully polymorphic access through opaque parameters requires
impredicative types (System F) — undecidable in general (Wells 1999). Leijen's
HMF (2008) provides a decidable extension with first-class polymorphism, but
is a substantial type system addition. For tinct's practical use cases, the
visible-literal path covers the common case.

**Polymorphic recursion requires annotation.** A self-recursive function that
calls itself at a different type than its declaration triggers the rejection
error. An explicit return type annotation on the function resolves it.

**Heterogeneous variadics require explicit class declarations.** The typeclass
pattern covers any heterogeneous variadic pattern expressible via instance
chains, but requires writing `FormatResult`/`FormatArg`-style class hierarchies.
Ad-hoc mixing of unrelated types in variadic position without a class hierarchy
produces a type error.

## What Would Change

### `src/typecheck_dict.rs` — DICT-GEN algorithm — **Done**

`compute_sccs()` (lines 26–130), `collect_dependencies()` (lines 132–220), and
the per-SCC Pass 1/3/4 loop are implemented. Polymorphic recursion detection
fires at `src/typecheck.rs:1478–1481` via `state.current_function`.

### `src/typecheck.rs` — dot-access resolution

**Current:** `check_dot_access` resolves field access from the Record type's bare `Type`.
**Proposed:** When the dict expression resolves to a visible literal, retrieve its TypeEnv and call `instantiate_scheme`. Fall through to the Record field type only for opaque parameters.
**Impact:** Moderate. Requires threading TypeEnv alongside Record types for dict literals in scope.

### `src/eval.rs` + `src/eval_call.rs` — variadic collection

**Current:** Variadic args collected into `Value::Dict` with integer keys `0..n`.
**Proposed:** Collect into `Value::Seq` — a `Vec` under tinct's Seq representation. All Seq operations work directly on the collected args.
**Impact:** Minor. Local change to variadic argument collection. Integer-key access to `args.0` is not a documented API.

### `src/typecheck.rs` + `src/type_env.rs` — variadic type inference

**Current:** Variadic params bound to `Type::Unknown`.
**Proposed:** Variadic param `...p` bound to `Type::Seq(β)` where β is a fresh TypeVar. [CALL-VARIADIC] unifies β against each supplied variadic argument.
**Impact:** Minor. Straightforward extension; existing [U-SEQ] handles `Seq(β)` unification.

### `doc/06-type-inference.md` — DICT-GEN rule and limitations

**Current:** DICT-GEN shows a single-group algorithm; §Limitations §1–§3 document the gaps.
**Proposed:** Update [DICT-GEN] to [DICT-GEN-SCC]. Remove §Limitations §1 (letrec monomorphism) and §3 (nested let-polymorphism). Update variadic param description to reflect `Seq(T)`.
**Impact:** Minor. Documentation only.

## Prerequisites

- `hkt-mappable-appendable` — multi-parameter typeclasses are required for the `FormatResult`/`FormatArg` heterogeneous variadic pattern. SCC decomposition and Seq-typed variadics have no HKT dependency and can land independently.

## References

- Damas, L. & Milner, R. (1982). "Principal type-schemes for functional programs." *POPL '82*, pp. 207-212. ACM. — [HM inference; the let/letrec distinction that SCC decomposition resolves]
- Garrigue, J. (2004). "Relaxing the value restriction." *FLOPS 2004*, LNCS 2998. Springer. — [why tinct's pure model is strictly more permissive than OCaml's relaxed restriction]
- Henglein, F. (1993). "Type inference with polymorphic recursion." *ACM TOPLAS*, 15(2), 253-289. — [undecidability of polymorphic recursion; why tinct rejects it and requires annotation]
- Jones, M.P. (1999). "Typing Haskell in Haskell." *Haskell Workshop*. — [SCC-based binding group analysis; the authoritative reference for the algorithm tinct adopts]
- Kfoury, A.J., Tiuryn, J. & Urzyczyn, P. (1990). "The undecidability of the semi-unification problem." *STOC '90*, pp. 468-476. ACM. — [semi-unification ≡ polymorphic recursion; formal undecidability]
- Kiselyov, O. (2013). "How OCaml type checker works." — [levels-based let-generalization; tinct's current inference model]
- Kiselyov, O., Lämmel, R. & Schupke, K. (2004). "Strongly typed heterogeneous collections." *Haskell '04*, pp. 96-107. ACM. — [PrintfType typeclass pattern; basis for tinct's FormatResult design]
- Leijen, D. (2008). "HMF: Simple type inference for first-class polymorphism." *ICFP '08*. ACM. — [decidable extension of HM with impredicative record fields; defines the boundary of the opaque-parameter limitation]
- Mycroft, A. (1984). "Polymorphic type schemes and recursive definitions." *LNCS 167*, pp. 217-228. Springer. — [polymorphic recursion semantics; why it requires fixpoint iteration]
- Pottier, F. & Rémy, D. (2005). "The essence of ML type inference." In *ATTAPL*, ch. 10. MIT Press. — [constraint-based framework; SCC interaction with constraint generation]
- Tarjan, R.E. (1972). "Depth-first search and linear graph algorithms." *SIAM Journal on Computing*, 1(2), 146-160. — [the SCC algorithm used in DICT-GEN-SCC Pass 0b]
- Wells, J.B. (1999). "Typability and type checking in System F are equivalent and undecidable." *Annals of Pure and Applied Logic*, 98(1-3), 111-156. — [why full polymorphic record fields through opaque parameters require System F]
- Wright, A.K. (1995). "Simple imperative polymorphism." *Lisp and Symbolic Computation*, 8(4), 343-355. — [the value restriction paper; tinct's purity means this restriction is unnecessary]
