# What If: Precise Field Access Typing for tinct

**State:** Proposal

What would it take to give `get` and `get-in` precise return types — eliminating
`Unknown` for literal-key field access and enabling label-polymorphic functions
over record fields?

## Current State

`get` and `get-in` return `Unknown` in all but a few cases. The type checker
(`src/typecheck.rs` `check_get`) handles `StringLiteral + Record` → field type
for concrete, fully-resolved dict types, but returns `Unknown` for:

- Dict argument whose type is a TypeVar not yet resolved to a concrete `Record`
- Dict argument whose type is a BAS union (`{name: String} | {name: Int}`)
- Key argument whose type is `Str` rather than a specific `StringLiteral`

`get-in` does not exist as a typed special form.

```tinct
# Today — all return Unknown despite statically knowable types:
user: [name: "Alice"  age: 30]

name: [get "name" user]          # Unknown (TypeVar dict at call site)
field-or-null: [get "name" user] # Unknown
[get "name" ([name: "Alice"] | [name: "Bob"])]  # Unknown
[fn [k] [get k user]]            # Unknown — k : Str, no literal type
```

### What's Missing

1. `get "name" user` returns `Unknown` whenever `user` is a TypeVar, even
   though the field type is statically determinable once `user` resolves.
2. `get "name" (A | B)` returns `Unknown` even when both union members have
   field `"name"` — BAS union distribution over field access is unimplemented.
3. `[fn [k] [get k user]]` is untyped — no way to express label-polymorphic
   functions that select a statically-unknown-but-constrained field.
4. `get-in` has no precise typing for chained literal-key access.

## Why Precise Field Access Matters for tinct

**Configuration languages live in records.** Nearly every tinct program
accesses dict fields. `get "host" config`, `get "port" config`, `get-in
["db" "host"] config` — these are the dominant use patterns. Returning
`Unknown` from all of them silently discards structural information the type
checker has in hand.

**`Unknown` propagates.** Once a field access returns `Unknown`, every
downstream computation is also `Unknown`. A chain of three field accesses
produces `Unknown` even if each individual field type is known.

**BAS union typing requires field access precision.** When a config value
may come from two sources — `local-config | remote-config` — the result of
`get "port" (local-config | remote-config)` should be `Int | Int = Int`, not
`Unknown`. Union typing is a core BAS capability; field access that collapses
unions to `Unknown` defeats it.

**Label-polymorphic stdlib functions cannot be typed.** Functions like
`get-or`, `update-field`, and pattern-matching helpers that take a field name
as a parameter have no useful type today. They could be typed precisely if
the label is tracked at the type level.

## Design

### Theoretical Foundation: G-J for BAS

This design adapts the Gaster-Jones (1996) first-class label system to tinct's
Boolean-Algebraic Subtyping model. The key divergence from classical G-J is
that tinct has no row variables — BAS replaces open rows with width subtyping.
This eliminates the `Lacks r l` predicate entirely (tinct has no record
extension operation) and reformulates the `{l :: a | r}` open-row type as a
`HasField` qualified-type constraint checked against a closed record type via
width subtyping. The result is a system that is simultaneously more restricted
(no row variables, no record extension) and more expressive (union distribution
is a first-class BAS rule) than classical G-J.

**Note on novelty.** No published work formally combines Gaster-Jones first-
class labels with Boolean-Algebraic Subtyping. TypeScript's indexed access
types implement the union distribution behaviour in practice without formal
proof. The soundness of the union distribution rule [HAS-FIELD-UNION] and the
principal types property of `HasField` under BAS are proof obligations that
do not yet have published resolutions. This proposal includes proof sketches
for each novel claim; a full mechanized proof is deferred.

---

### Extension 1 — `Kind::Label`

Add a new kind for label types alongside `Kind::Type` and `Kind::Arrow`:

```rust
pub enum Kind {
    Type,                          // * — kind of proper types
    Arrow(Box<Kind>, Box<Kind>),   // k₁ → k₂ — type constructors (HKT)
    Label,                         // Label — kind of string label types
    Var(u32),                      // kind variable (inference)
}
```

`StringLiteral("name")` has kind `Label`. A label TypeVar `l : Label` ranges
over string identifiers — not over types. Label TypeVars and type TypeVars are
kind-separated: `l : Label` cannot appear where `τ : *` is expected and vice
versa.

**Subtyping for label-kinded types is a flat antichain.** `StringLiteral("name")`
and `StringLiteral("age")` are incomparable — no label is a subtype of another.
`StringLiteral(s) <: StringLiteral(s)` only (reflexivity). This is consistent
with BAS; no new subtyping rule is needed.

**Unification of label TypeVars** follows the standard U-VAR rule: binding
`l : Label` to `StringLiteral(s)` produces substitution `[l ↦ StringLiteral(s)]`.
The occurs check applies. Label TypeVars participate in generalization at the
same level thresholds as type TypeVars.

---

### Extension 2 — `HasField` Qualified-Type Constraint

`HasField l d a` is a three-argument qualified-type constraint meaning "dict
type `d` has label `l` with field type `a`." It carries a functional dependency:

```
HasField l d a , HasField l d a'  ⊢  a = a'
```

Given the label `l` and the dict type `d`, the field type `a` is uniquely
determined. This is the `(l, d) → a` functional dependency in Jones (1994)
qualified-type notation.

**Instance resolution rules** (all are axioms, not derivable from others):

```
l ∈ dom(fields)    fields(l) = τ
─────────────────────────────────   [HAS-FIELD-REC]
HasField l Record(fields) τ


HasField l τ₁ a₁    HasField l τ₂ a₂
─────────────────────────────────────   [HAS-FIELD-UNION]
HasField l (τ₁ | τ₂) (a₁ | a₂)


HasField l τ₁ a₁    HasField l τ₂ a₂
─────────────────────────────────────   [HAS-FIELD-INTER]
HasField l (τ₁ & τ₂) (a₁ & a₂)


─────────────────────────────────────────────────────────────────   [HAS-FIELD-UNKNOWN]
HasField l Unknown Unknown
```

No instance for `Negation` — `HasField l (¬τ) ?` is underdetermined and
produces `Unknown` (conservative fallback). No instance for `Never` — a value
of type `⊥` cannot exist at runtime, so field access on it is vacuously sound.

**Constraint representation.** `HasField` requires extending `Constraint`
from its current single-var form to a multi-argument form:

```rust
pub enum Constraint {
    // Existing: class name + single TypeVar
    Class { class: String, var: String },
    // New: HasField with label var (Kind::Label), dict var, field var
    HasField { label: LabelRef, dict_var: String, field_var: String },
}

pub enum LabelRef {
    Concrete(String),       // StringLiteral("name") — statically resolved
    Var(String),            // label TypeVar l — polymorphic
}
```

`HasField` with `LabelRef::Concrete` is a ground constraint (no label
variable) and resolves against the dict type immediately. `HasField` with
`LabelRef::Var` is a schematic constraint carried in `TypeScheme.constraints`
for label-polymorphic functions.

---

### Extension 3 — Type Rules for `get` and `get-in`

**[GET]** — the central inference rule:

```
Γ ⊢ key ⇒ StringLiteral(l)    Γ ⊢ dict ⇒ d
fresh β (kind *)    C' = C ∪ {HasField l d β}
──────────────────────────────────────────────────   [GET]
Γ, C ⊢ [get key dict] ⇒ β, C'
```

`l` is either a `LabelRef::Concrete(s)` (when `key` synthesises to
`StringLiteral(s)`) or a `LabelRef::Var(l)` (when `key` synthesises to a
label TypeVar). `β` is a fresh type TypeVar bound to the field type. The
constraint `HasField l d β` is added to the active constraint set and resolved
eagerly when possible:

- **`d` is `Record(fields)`:** resolve immediately via [HAS-FIELD-REC].
  If `l ∈ dom(fields)`, bind `β ↦ fields(l)` and drop the constraint. If
  `l ∉ dom(fields)`, emit a type error (field not present in closed record).
- **`d` is `τ₁ | τ₂`:** apply [HAS-FIELD-UNION]; recursively resolve
  `HasField l τ₁ β₁` and `HasField l τ₂ β₂`; bind `β ↦ β₁ | β₂`.
- **`d` is `τ₁ & τ₂`:** apply [HAS-FIELD-INTER]; bind `β ↦ β₁ & β₂`.
- **`d` is a TypeVar `α`:** defer. Bind `α ↦ {l: β}` via unification
  (the minimum record requirement). Width subtyping ensures concrete dicts
  with extra fields satisfy this constraint at call sites. Multiple deferred
  `HasField` constraints on the same `α` with different labels accumulate: a
  second `HasField "age" α γ` merges `{age: γ}` into the existing `{name: β}`
  constraint to produce `α ↦ {name: β, age: γ}`. This merge is performed
  during constraint resolution, not during unification — unification of two
  closed records with different field sets would fail without it.
- **`d` is `Unknown`:** return `Unknown` (gradual typing).

**[GET-IN]** — chained literal-key access, expanded inline at the call site:

```
──────────────────────────────────────────   [GET-IN-NIL]
Γ, C ⊢ [get-in [] dict] ⇒ type(dict), C


Γ, C ⊢ [get h dict] ⇒ τ, C'
Γ, C' ⊢ [get-in path τ] ⇒ a, C''
──────────────────────────────────────────────   [GET-IN-CONS]
Γ, C ⊢ [get-in (h :: path) dict] ⇒ a, C''
```

The path must be a `Seq` of `StringLiteral` values known at compile time.
If any element of the path is not a `StringLiteral`, `get-in` returns
`Unknown` (non-literal path is underdetermined). `check_get_in` special-cases
this: if the first argument resolves to a `Seq` literal whose elements are all
`StringLiteral`, it unfolds into a chain of `[GET]` applications. Otherwise
it falls through to `Unknown`.

---

### Extension 4 — Label Polymorphism

Label TypeVars are generalized at the same binding boundaries as type TypeVars.
A function that receives a label as a parameter can have a label-polymorphic
type scheme:

```tinct
# Inferred type: ∀ (l : Label) d a. HasField l d a => StringLiteral(l) → d → a
[fn [key@StringLiteral(l)  dict] [get key dict]]

# Inferred type: ∀ (l : Label) d a. HasField l d a => StringLiteral(l) → d → a | Unknown
get-or: [fn [key@StringLiteral(l)  dict  default] [or [get key dict] default]]
```

The `TypeScheme` for a label-polymorphic function carries label TypeVars
alongside type TypeVars:

```
get : ∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a
```

Display: `HasField l d a => Fn@a [StringLiteral(l)  d]`

At a call site `[get "name" user]`:
1. Instantiate the scheme: fresh label var `l'`, fresh type vars `d'`, `a'`,
   constraint `HasField l' d' a'`.
2. Unify `StringLiteral(l')` with the key's type `StringLiteral("name")`:
   bind `l' ↦ StringLiteral("name")`.
3. Unify `d'` with `user`'s type `{name: String, age: Int}`.
4. Resolve `HasField StringLiteral("name") {name: String, age: Int} a'`:
   via [HAS-FIELD-REC], bind `a' ↦ String`.
5. Return type: `String`. ✓

---

### Proof Obligations and Novel Claims

The following claims are not covered by existing literature and require
verification before implementation:

**P1 — Soundness of [HAS-FIELD-UNION].** Claim: if `x : τ₁ | τ₂` and
`HasField l τ₁ a₁` and `HasField l τ₂ a₂`, then `x.l : a₁ | a₂`.

*Proof sketch:* By BAS union elimination [UNION-ELIM], a value `x : τ₁ | τ₂`
is either a `τ₁`-value or a `τ₂`-value at runtime. In the first case,
`x.l : a₁` by [HAS-FIELD-REC]; `a₁ <: a₁ | a₂` by [UNION-INJ-L]. In the
second case, `x.l : a₂ <: a₁ | a₂` by [UNION-INJ-R]. Therefore `x.l : a₁ | a₂`
by [UNION-ELIM]. □

This argument is straightforward and corresponds to TypeScript's observed
behavior for union indexed access. The sketch constitutes a sufficient informal
proof; mechanization is deferred.

**P2 — Functional dependency uniqueness under BAS union.**
`HasField l (τ₁ | τ₂) a` is unique: `a = a₁ | a₂` where `a₁` and `a₂` are
uniquely determined by [HAS-FIELD-REC] applied to the union members. BAS union
normalization ensures `a₁ | a₂` is in canonical form.

*Concern:* If `a₁ = a₂ = String`, then `a = String | String = String` by
deduplication. The functional dependency holds: `(l, τ₁ | τ₂) → String` is
unique. If `a₁ = Int` and `a₂ = Number`, then `a = Int | Number`. Under BAS,
`Int | Number = Number` (since `Int <: Number`). The functional dependency
still holds. □

**P3 — Principal types under label polymorphism.** Adding `HasField` with
functional dependency `(l, d) → a` to the qualified type system. Jones (1994)
proves that qualified types with functional dependencies preserve principal
types when the dependency is *confluent* (every derivation reaches the same
result). Confluence holds here: [HAS-FIELD-REC] is deterministic (field lookup
in `HashMap` is a function), [HAS-FIELD-UNION] applies the same rule to each
member, and [HAS-FIELD-INTER] similarly.

*Concern:* The interaction of label TypeVars (kind `Label`) with HM
generalization. Label TypeVars at level `n > enclosing_level` must be
generalized into the scheme alongside type TypeVars. The generalization
algorithm already handles multiple TypeVar kinds (type and row vars in the
pre-BAS system); extending it to label vars follows the same pattern.

*Open question:* Does the presence of `HasField` constraints with label
TypeVars in the scheme create ambiguity at call sites? A constraint
`HasField l d a` where `l` is a label TypeVar and `d` is also a TypeVar is
ambiguous if neither is instantiated by the call site — the constraint cannot
be resolved. This is the classic ambiguity problem in Haskell type classes.
Tinct's conservative resolution: emit a type error at the generalisation site
if a `HasField` constraint remains unresolved with no concrete binding for
`l`. In practice, label TypeVars are always bound by the key argument's type
at the call site.

**P4 — Constraint merge soundness.** When two `HasField` constraints on the
same TypeVar dict are accumulated (`HasField "name" α β` and `HasField "age" α γ`),
the merge binds `α ↦ {name: β, age: γ}`. Soundness requires that any concrete
dict that satisfies both constraints must have both fields. By [HAS-FIELD-REC],
a dict has field `"name": β` iff `"name" ∈ dom(fields)` and `fields("name") = β`,
and similarly for `"age"`. The merge is just the union of both field requirements.
Width subtyping ensures concrete dicts with additional fields satisfy both. □

---

### BAS-Specific Contributions

Classical G-J cannot express these rules — they arise specifically from the
BAS lattice structure:

**Union distribution** [HAS-FIELD-UNION] is the most important. G-J with row
variables cannot derive `HasField l (τ₁ | τ₂) (a₁ | a₂)` because row variables
are not union-typed. In BAS, this is a structural rule on the type lattice.

**Intersection field typing** [HAS-FIELD-INTER]: field access distributes over
intersections. If `x : A & B` and both A and B have field `l`, then `x.l :
a₁ & a₂`. Under BAS normalization, `a₁ & a₂` reduces if `a₁ <: a₂` or
`a₂ <: a₁`. This gives more precise types for intersection-typed dicts than
G-J provides.

**No `Lacks` predicate.** G-J requires `Lacks r l` (row `r` does not contain
label `l`) to safely extend records. Tinct has no record extension operation —
`set` replaces an existing field's value but does not add new fields at the
type level. `Lacks` is therefore unnecessary, simplifying the system
significantly.

**Width subtyping subsumes open rows.** G-J's `{l :: a | r}` is an open row
type carrying the `r` remainder. In BAS, a closed record `{l: a}` with width
subtyping accomplishes the same: any concrete dict with field `l: a` and
additional fields is a subtype of `{l: a}`, so functions typed against `{l: a}`
accept richer dicts without requiring `r` to be named. The `HasField l d a`
constraint on a TypeVar `d` is more general than the G-J open row: `d` can
unify with any record type that satisfies the constraint, including union and
intersection types.

---

## What Would Change

### `src/types.rs` — Kind, Constraint, TypeScheme

**Current:** `Kind` has `Type`, `Arrow`, `Var` (all `#[allow(dead_code)]`).
`Constraint` is `{ class: String, var: String }`. `TypeScheme` has `type_vars:
Vec<String>`.

**Proposed:**
- Add `Kind::Label` variant
- Change `Constraint` to an enum: `Class { class, var }` and
  `HasField { label: LabelRef, dict_var, field_var }`
- Add `label_vars: Vec<String>` to `TypeScheme` for generalized label TypeVars

**Impact:** Moderate. Every match on `Constraint` and `TypeScheme` needs updating.

### `src/type_unify.rs` — HasField resolution

**Current:** `satisfies_constraint` handles six hardcoded classes. No multi-arg
constraints.

**Proposed:** Add `resolve_has_field(label, dict_type, state) → Option<Type>`
implementing [HAS-FIELD-REC], [HAS-FIELD-UNION], [HAS-FIELD-INTER], deferred
TypeVar constraint accumulation, and field-set merge on the same TypeVar.

**Impact:** Moderate. New function; no changes to existing `satisfies_constraint`
for the six existing classes.

### `src/typecheck.rs` — `check_get`, new `check_get_in`

**Current:** `check_get` at line 2125 handles `StringLiteral + Record` only.
TypeVar dicts return `Unknown`. No `check_get_in`.

**Proposed:**
- Extend `check_get` with TypeVar arm (defer `HasField`) and Union arm
  (distribute via [HAS-FIELD-UNION])
- Add `check_get_in` implementing [GET-IN-CONS] for literal-sequence paths,
  falling back to `Unknown` for variable paths

**Impact:** Moderate. `check_get` grows by ~50 lines; `check_get_in` is new
(~40 lines).

### `src/type_env.rs` — `get` and `get-in` signatures

**Current:** `get` is a special form dispatched in `infer_expr` (not a
registered builtin). `get-in` does not exist.

**Proposed:** Register the label-polymorphic `get` scheme:
```
∀ (l : Label) (d : *) (a : *). HasField l d a => StringLiteral(l) → d → a
```
Register `get-in` as a special form in `infer_expr` with `check_get_in`.

**Impact:** Minor. Scheme registration; new special-form dispatch arm.

### `src/typecheck_annot.rs` — label TypeVar annotation

**Current:** `fn@StringLiteral(l)` does not exist as a form.

**Proposed:** Recognise `StringLiteral(l)` in annotation position (as part of
the `fn@[...]` metadata dict from `constraint-annotations`) where the key is
`StringLiteral` and the value is a lowercase TypeVar name — this establishes
`l` as a label TypeVar in `ann_mapping` with `Kind::Label`.

**Impact:** Minor. Two new arms in `resolve_type_name`.

### `stdlib/prelude.llt` — annotation migration

**Current:** `get : fn@Unknown`, `get-or : fn@Unknown`.

**Proposed:**
```tinct
get:    [fn@[return: a  constraint: [HasField l d a]  doc: "Field access"] [key@StringLiteral(l)  dict@d] ...]
get-or: [fn@[return: a  constraint: [HasField l d a]] [key@StringLiteral(l)  dict@d  default@a] ...]
get-in: [fn@[doc: "Chained field access — return type inferred from literal path"] [path  dict] ...]
```

`get-in` does not receive a constraint annotation — its return type is
always inferred via [GET-IN-CONS] unfolding. The annotation is documentation-only.

**Impact:** Minor (stdlib-only changes after the type checker is ready).

## Prerequisites

**`hkt-foundation`** — adds `Kind::Arrow` and `Type::App`/`Type::Operator`.
`Kind::Label` is a natural parallel addition in the same sprint; it should be
included there rather than deferred.

**`hkt-mappable-appendable`** — migrates `Constraint` from the current
single-var struct to the enum form that `HasField` requires. The `HasField`
constraint is a natural addition in the same sprint.

**`constraint-annotations`** — the `fn@[...]` metadata dict refactoring enables
`fn@[constraint: [HasField l d a] ...]` syntax for library authors annotating
label-polymorphic functions.

## References

- Gaster, B.R. & Jones, M.P. (1996). "A Polymorphic Type System for Extensible
  Records and Variants." Technical Report NOTTCS-TR-96-3, University of
  Nottingham. — [first-class labels and `Lacks` predicate; foundational design
  adapted here for BAS without row variables or `Lacks`]
- Jones, M.P. (1994). "A System of Constructor Classes: Overloading and
  Implicit Higher-Order Polymorphism." *Journal of Functional Programming
  5*(1), 1–35. — [qualified types with functional dependencies; basis for
  `HasField` constraint and principal types argument]
- Jones, M.P. (1995). "Simplifying and Improving Qualified Types." *FPCA '95*,
  160–169. — [constraint simplification and entailment; covers functional
  dependency disambiguation]
- Dolan, S. (2017). "Algebraic Subtyping." PhD thesis, University of Cambridge.
  — [BAS foundation; closed records with width subtyping; union and intersection
  type algebra that enables [HAS-FIELD-UNION] and [HAS-FIELD-INTER]]
- Gundry, A. (2015). "A New Look at Generalized Algebraic Data Types." PhD
  thesis. / GHC Proposal: `HasField` type class (GHC 8.2). — [production
  implementation of `HasField` with functional dependency in HM; confirms
  principal types under functional dependencies]
- Microsoft TypeScript Team. "Indexed Access Types." *TypeScript Handbook*. —
  [production evidence for union distribution `(A|B)["k"] = A["k"] | B["k"]`;
  no formal proof but extensive empirical validation]
