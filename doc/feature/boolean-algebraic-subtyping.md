# Boolean-Algebraic Subtyping

Replaces Rémy-style row variables with Boolean-Algebraic Subtyping (BAS); the type algebra becomes a distributive Boolean lattice of union, intersection, and negation types.

## Overview

BAS rebuilds tinct's record-extension foundation on a proven theoretical basis.
Chau & Parreaux (POPL 2026) prove BAS sound via five characteristic Boolean
homomorphisms — published at the top venue, peer-reviewed, with a working artifact.
MLstruct (Parreaux & Chau 2022, OOPSLA), the language whose type system BAS
formalizes, is a working implementation with principal type inference for union and
intersection types and pattern matching — proven sound and complete.

BAS encodes extensible records *without row variables* using the same Boolean algebra
that encodes union and intersection. Field *absence* is a Boolean property of a type.
Width subtyping — `@[name: Str  age: Int]` satisfies `@[name: Str]` — falls directly
out of conjunction elimination: `{name: Str} ∧ {age: Int} <: {name: Str}` by
definition of `∧` as the greatest lower bound. No row variable gymnastics.

The key capabilities gained:

- **Principal union types** for `if` and `[match]` — `[if cond [ok: v] [err: msg]]` infers `Ok@T | Err@Str`, not `Any`
- **False-branch narrowing** — after `[if [int? x] ...]`, the false branch knows `x : ~Int`
- **Exhaustiveness without annotation** — pattern match arms checked without requiring `[@Result res]` TypeAssert
- **Typed field removal** — `[remove dict "field"]` gets a precise return type
- **Proven soundness** — D2's conjectured-soundness row-extension is replaced with a published proof

## Design

### The BAS Type Algebra

The type grammar is a distributive Boolean algebra:

| Formal | tinct annotation | Notes |
|--------|-----------------|-------|
| `A \| B` | `@[A B]` | positional entries = union members — existing |
| `A & B` | `@[[all A B]]` | `[all ...]` prefix bracket in annotation position |
| `~A` | `@[[without A]]` | `[without A]` prefix bracket in annotation position |
| `⊤` | `@Top` | true supertype |
| `⊥` | `@Never` | true bottom |
| `{f: A}` | `@[f: A]` | single-field record — existing dict annotation |
| `α` | `@a` | lowercase type variable name — existing |
| `μα.A` | `@[AliasName ...]` | recursive types declared with a name via `[type ...]`; the μ form is compiler-internal |

Multi-field records are not a primitive. `{x: τ₁, y: τ₂}` is syntax sugar for
`{x: τ₁} ∧ {y: τ₂}` (Parreaux & Chau 2022, §2.2.2). Under BAS, `@[x: τ₁  y: τ₂]`
means exactly this — a multi-field annotation is an intersection of single-field
constraints. The annotation syntax requires no change; what changes is the internal
type representation.

**Disambiguation from union annotation:** positional entries in `@[...]` are union
members when there are multiple entries (`@[Int Str]` → `Int | Str`). When the
single positional entry is itself a list whose first token is `all` or `without`, it
is an intersection or negation type, not a union member. Examples:
`@[[all Int Num]]` → `Int & Num`; `@[[without Int]]` → `~Int`;
`@[[all Int Num] Str]` → `(Int & Num) | Str`.

Subtyping: `A <: B` iff `A & ~B` is uninhabited. Subtyping is decidable and always
terminates on well-formed inputs (Chau & Parreaux 2026, Theorem 7.6).

Subtyping: `A <: B` iff `A & ~B` is uninhabited. Subtyping is decidable and always
terminates on well-formed inputs (Chau & Parreaux 2026, Theorem 7.6).

### Why Negation Is Not Optional

Negation types (`~A`) are essential to the constraint solver, not just a user-facing
annotation. The core difficulty with union/intersection types in constraint solving is
constraints of the form:

```text
τ₁ ≤ τ₂ ∨ α     (union on the right — a type variable α appears in a union)
α ∧ τ₁ ≤ τ₂     (intersection on the left — type variable α is intersected)
```

Without negation, the solver must backtrack or guess which disjunct to pursue. With
negation, these are rewritten losslessly using Boolean algebra (Parreaux & Chau 2022,
§3.2.1, C-Var1/2 rules):

```text
τ₁ ≤ τ₂ ∨ α   →   τ₁ & ~τ₂ ≤ α     (move τ₂ to left side as ~τ₂)
α & τ₁ ≤ τ₂   →   α ≤ τ₂ | ~τ₁     (move τ₁ to right side as ~τ₁)
```

The result is always a constraint with a type variable alone on one side, so bounds
can be updated directly. The algorithm does not backtrack and yields principal types
(Parreaux & Chau 2022, §5.2, §5.3). `Type::Negation` is load-bearing infrastructure
for the inference algorithm.

### Width Subtyping and Open Records

Single-field record types `{f: τ}` are atoms of the Boolean algebra. Multi-field
records are their intersections. Width subtyping is then a theorem — a dict with more
fields is a subtype of one with fewer, because intersection elimination gives `A & B <: A`:

```text
{name: Str} & {age: Int}  <:  {name: Str}
```

In tinct: any dict satisfying `@[name: Str  age: Int]` also satisfies `@[name: Str]`.
No row variable, no unification case, no `unify_remainders`.

**All structural record annotations are open.** Under BAS, `{name: Str}` means "any
value with at least a `name: Str` field." There is no structural closed-record type
in the Boolean algebra — closing a record requires intersecting with the negation of
every conceivable absent field, which is not expressible over an open set of labels.
This is the correct semantics for a configuration language: extra fields are tolerated.

The `...` and `...rest` annotation syntax is retired:

- `...` (anonymous open marker) — redundant under BAS; all structural record annotations are open by default. Removed.
- `...rest` (named row variable) — not used anywhere in `stdlib/`. Under BAS, full polymorphism is inferred via type variable bounds. Removed.
- `...name` in `[fn]` parameter position (variadic argument collector) — a completely separate feature at the value level. Unaffected; remains valid.

### Pattern Matching and Precise Arm Types

Case expressions (pattern matching) are typed by the I-Case3 rule (Parreaux & Chau
2022, Fig. 5), which introduces two fresh type variables and intersects the scrutinee
type with `#C` (matched class tag) in the true branch and `¬#C` in the false branch.

```tinct
result: [try risky]
[match result
    [ok: v]:    v           # arm narrows scrutinee to #Ok variant
    [err: msg]: [error msg]] # arm narrows scrutinee to @[[without #Ok]] = #Err variant
# Inferred result type: @[T Never] = @T   (union with Never simplifies — all variants covered)
```

### ADT Discrimination via Nominal Tags

BAS's S-RcdTop rule collapses unions of records with disjoint field sets to the top type:

```text
⊤  ≤  {x: τ} ∨ τ'    when field x is not present in τ'
```

This gives `{ok: a} ∨ {err: Str} ≡ ⊤` — the union is indistinguishable from "any
value at all." S-RcdTop is required for the Boolean algebra to remain well-formed.

Nominal class tags solve this. `#Ok` and `#Err` are nominal identities that remain
disjoint under the Boolean algebra via S-ClsBot:

```text
#Ok & #Err ≤ Never   (unrelated nominal tags annihilate — no value can be both)
```

So `(#Ok & {v: a}) | (#Err & {msg: Str})` is a proper discriminated union — pattern
matching refines each arm type via the nominal tag rather than the key set.

`from-json '{"ok": 42}'` produces structural `{ok: Int}`, not `Ok@Int`. Structural
dicts from JSON must be explicitly lifted into nominal ADT values via constructor calls.
`[match]` arm syntax unambiguously determines the dispatch mode: `[ok: v]` (lowercase,
colon) is a structural field pattern; `[Ok v]` (uppercase, no colon) is a nominal
constructor pattern.

### False-Branch Narrowing

After `[if [int? x] ...]`, the false branch knows `x : ~Int`. Combined with a union
annotation, this is precise:

```tinct
process: [fn [x@[Int Str]]
    [if [int? x]
        [+ x 1]        # x : Int  (true branch — already works today)
        [str-upper x]]]  # x : Str  (false branch — requires ~Int narrowing)
```

With BAS, `(Int | Str) & ~Int = Str` — the precise type that makes the `str-upper`
call statically verifiable.

Field presence guard:

```tinct
[if [has? "debug" config]
    [process config.debug]    # config has field debug
    [use-defaults]]           # config : ~{debug: ⊤}  — guaranteed no debug field
```

### `$merge` Gets a Precise Type

`$merge` produces a value with *all* fields from both inputs combined — its type
is the *intersection* of the two input types, not their union. Under BAS, `[merge
{name: "Alice"} {age: 30}]` has type `{name: Str} ∧ {age: Int}` = `{name: Str,
age: Int}` — a precise multi-field record type. S-RcdTop collapses *union* types over
disjoint-field records to `⊤`, but `$merge` produces an intersection, which is not
affected by S-RcdTop.

### Enforcing Closed Dicts via Structural Contracts

When exact field sets matter — pipeline formatters, strict schema validation — the
runtime `validate` layer handles it:

```tinct
# Layer 1: type annotation (static, open)
# Guarantees %  has at least port: Int and hostname: Str.
%@[port: Int  hostname: Str]

# Layer 2: runtime exact validation (dynamic, closed)
[validate [
  port:     [type: "Int"  min: 1  max: 65535]
  hostname: [type: "Str"  pattern: "^[a-z0-9.-]+$"]
  exact:    true    # no fields beyond those declared in this schema
] %]
```

The `exact: true` schema key walks the data value and rejects any key not named in
the schema dict. It is the runtime equivalent of what closed record types provided
statically with `RowTail::Empty`.

### Type Simplification

Inferred types under BAS can be complex Boolean expressions. BAS adoption includes
a type simplification pass: pushing types into disjunctive normal form, inlining
non-cyclic bounds, reducing complements. MLstruct's simplifier is approximately
1000 lines of the 5000-line implementation. Without simplification, inferred types
in error messages and LSP hover are unreadable.

RDNF normalization is invoked during inference at constraint simplification steps,
not only at subtyping check time. The normalization is exponential in the number of
distinct atomic types in the worst case; MLstruct mitigates this by computing RDNF
lazily and maintaining a cache of currently-processed subtyping relationships.

## What Changes for Users

### Inferred Union Types for `if` and `[match]`

```tinct
# Before BAS: result type is Any
result: [if cond [ok: v] [err: "failed"]]

# With BAS: result type is Ok@T | Err@String
result: [if cond [ok: v] [err: "failed"]]

# Consequence: exhaustive match is type-checked without annotation
[match result
    [ok: v]:    [process v]
    [err: msg]: [log msg]]   # ✓ — all variants provably covered
```

### ADT Construction Uses Nominal Constructors

```tinct
# Structural dict (open record, without an ADT value):
result: [ok: computed-value]     # type: {ok: T}

# Nominal ADT value (matches union arms):
result: [Ok computed-value]      # type: Ok@T  — carries #Ok tag

# JSON input is always structural; must be lifted explicitly:
raw: [from-json input]           # type: {ok: Int}  — structural, not Ok@Int
result: [Ok [get "ok" raw]]      # type: Ok@Int  — explicit lift
```

### Record Annotations Are Always Open

`@[name: Str  host: Str]` means "at least these fields" — a richer config dict with
additional fields satisfies the annotation. `...` is no longer written in annotations.
Existing programs that use bare `[name: Str]` annotations see no behavioral change.

## What This Reformulation Loses

### Closed Structural Record Types

The most concrete loss. Under BAS all structural record annotations are open, so
`[name: Str, age: Int]` (without `...`) becomes "at least name: Str and age: Int."
Exact field enforcement moves to the runtime `validate` layer with `exact: true`.

### `...` and `...rest` Annotation Syntax

Both retire. `...` was the signal that a record was open — redundant when openness
is the default. `...rest` was a named row variable for threading extra fields through
a function explicitly — never used in `stdlib/`, with no concrete use case identified.
The function-parameter `...name` (variadic argument collector) is unaffected.

### Quantified Annotation Checking (NSSE)

TypeAssert with polymorphic annotations (`f@[Fn@b [a]]`) is checked at the structural
entailment level only. Verifying that an implementation satisfies a universally
quantified annotation is undecidable in BAS (NSSE, Parreaux & Chau 2022, §6). The
behavior is unchanged from tinct's existing TypeAssert semantics — the limitation is
now formally characterized rather than incidentally permitted.

### Union Precision for Records with Disjoint Field Sets

Rémy-style row variables can express that a value might be *either* a record with
fields `{x:a}` *or* a record with fields `{y:b}` — the union `{x:a} | {y:b}` is
meaningful and discriminable via the row variable. Under BAS, this union collapses to
`⊤` via S-RcdTop. The resolution is nominal class tags.

## Implementation

### Type Representation (`src/types.rs`)

`RowTail::RowVar(String, u32)` is removed. The four-case `unify_remainders` algorithm
is removed. In their place:

- `Type::Negation(Box<Type>)` is added as a first-class type variant
- Multi-field records become syntactic sugar: `{x: τ₁, y: τ₂}` → `{x: τ₁} ∧ {y: τ₂}` at the type representation level
- `RowTail::Empty` and `RowTail::RowVar` are both removed — the open/closed distinction does not exist at the structural level under BAS
- `is_subtype` is replaced by the BAS subtyping judgment (RDNF normalization + algebraic subtyping rules from Fig. 4 of Parreaux & Chau 2022)

The existing `Type::Union` and `Type::Intersection` remain; `Type::Negation` joins
them as the third Boolean operation.

### Constraint Solver (`src/typecheck.rs`)

The Robinson unification + row-variable binding approach is replaced by the MLstruct
constraint solver:

- Each type variable carries `lower: Vec<Type>` and `upper: Vec<Type>` bounds, as in Simple-sub / D2
- New: bounds can contain negated types (`lower: [~Int]` = "definitely not Int")
- **C-Var1/2 rewriting**: when a constraint has a union on the right or an intersection on the left involving a type variable, the constraint solver uses `¬τ` to move terms across the inequality, producing a constraint with the type variable isolated
- Constraint normalization to RDNF (Reduced Disjunctive Normal Form) ensures the algorithm terminates and yields unique results without backtracking
- `unify_remainders` is gone; field-set operations are just conjunction over single-field record types

### ADT Encoding (`src/typecheck.rs`, `src/eval.rs`)

The typing cluster's C1 (structural ADTs) and C2/C3 (nominal variants) converge under
BAS. C1's key-set discrimination is superseded: under BAS, `[Ok v]` and `[Err msg]`
are nominal values carrying `#Ok` and `#Err` tags (exactly C2/C3 `Value::Variant`).
`[match]` arm discrimination uses the nominal tag via I-Case3, not key-set presence.

C1 structural ADT declarations (`Result: [type [ok: a] [err: Str]]`) are reinterpreted:
the `ok`/`err` field names become the nominal constructor names `Ok`/`Err`.

### `infer_match()` Style Enforcement (`src/typecheck.rs`)

`infer_match()` gains a second responsibility alongside coverage: **pattern style
enforcement**. The `Type::Union` members already encode the expected arm style:

```rust
// Structural union member → expects Pattern::Dict arms
Type::Record(fields: {ok: TypeVar(a)}, tail: Open)

// Nominal union member → expects Pattern::Constructor arms
Type::NominalVariant { tag: "Ok", payload: Some(TypeVar(a)) }

// Literal union member → expects Pattern::StringLiteral arms
Type::StringLiteral("pending")
```

For each arm, `infer_match()` calls `find_compatible_member()` to correlate the
arm's pattern to a union member by style:

```rust
fn find_compatible_member<'a>(pattern: &Pattern, members: &'a [Type]) -> Option<&'a Type> {
    members.iter().find(|member| match (pattern, member) {
        (Pattern::Dict(_),                    Type::Record(..))          => true,
        (Pattern::Constructor { tag, .. },    Type::NominalVariant { tag: t, .. }) => tag == t,
        (Pattern::StringLiteral(s),           Type::StringLiteral(t))    => s == t,
        (Pattern::Wildcard,                   _)                         => true,
        _                                                                 => false,
    })
}
```

**Mixed unions** work naturally — each arm's pattern is correlated to whichever member
has a compatible style:

```tinct
Mixed: [type [ok: a] [Err Str] "pending"]

[match x
    [ok: v]:   [process v]      # → correlates to Record([ok:a])
    [Err msg]: [handle msg]     # → correlates to NominalVariant("Err", Str)
    "pending": [wait]           # → correlates to StringLiteral("pending")
    _:         [error "???"]]   # → wildcard, compatible with all
```

## References

- Chau, C.Y. & Parreaux, L. (2026). "The simple essence of Boolean-algebraic
  subtyping: semantic soundness for algebraic union, intersection, negation, and
  equi-recursive types." *Proc. ACM Program. Lang.*, 10(POPL), pp. 1353–1382.
  doi:10.1145/3776689. Preprint: <https://lptk.github.io/files/boolean-algebraic-subtyping.pdf>.
  Artifact: <https://github.com/fo5for/sebas> — Proves BAS sound via five characteristic
  Boolean homomorphisms; encodes extensible records without row variables.
  [algebraic subtyping, row polymorphism]
- Parreaux, L. & Chau, C.Y. (2022). "MLstruct: Principal type inference in a Boolean
  algebra of structural types." *Proc. ACM Program. Lang.*, 6(OOPSLA2), Article 141.
  doi:10.1145/3563304. Artifact: <https://github.com/hkust-taco/mlstruct> —
  Working implementation of BAS-style inference. Source of the C-Var1/2 constraint
  rewriting rules, RDNF normalization, I-Case3 pattern matching typing rule, and the
  S-RcdTop collapse of disjoint-field-set record unions. Direct implementation reference.
  [algebraic subtyping, row polymorphism, inference algorithm]
- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge.
  — Principal type proof (Theorem 4.1) for the non-row fragment. Co-NP-hard subtyping
  complexity class established here — BAS matches this, not worsens it. [algebraic subtyping]
- Marques, R., Florido, M. & Vasconcelos, P. (2024). "Towards algebraic subtyping
  for extensible records." arXiv:2407.06747. — D2's former row-extension foundation;
  soundness conjectured. BAS supersedes this.
  [algebraic subtyping, row polymorphism]
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM. — Simple-sub: the bisubstitution algorithm that MLstruct extends.
  D2's inference algorithm; BAS builds on this foundation. [algebraic subtyping]
- Rémy, D. (1994). Type inference for records in natural extension of ML. In
  *Theoretical Aspects of Object-Oriented Programming*, pp. 291–346. MIT Press.
  — Former row polymorphism foundation; `RowTail::RowVar` and `unify_remainders`
  implemented Rémy's approach. BAS adoption removes this mechanism. [row polymorphism]
