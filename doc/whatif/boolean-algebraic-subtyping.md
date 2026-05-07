# What If: Boolean-Algebraic Subtyping for tinct

**State:** Proposal

What would it take to rebuild tinct's record-extension foundation on Boolean-Algebraic
Subtyping (BAS), eliminating row variables and closing the soundness gap in D2?

## Current State

tinct's record model uses Rémy-style row polymorphism (Rémy 1994): open records carry
a `RowTail::RowVar(u32)` that unifies with additional fields at call sites. The
four-case `unify_remainders` algorithm handles row variable binding by partitioning
fields between two rows. This gives structural open-record typing:

```tinct
greet: [fn [person@[name: Str ...]] [str "Hello " person.name]]
greet [name: "Alice"  age: 30]   # ok — extra fields allowed via RowVar
```

The typing cluster plan (D2, `algebraic-subtyping` sprint) extends this to full
algebraic subtyping by citing Marques, Florido & Vasconcelos (2024) as the
row-variable extension to Simple-sub. The D2 soundness risk is more severe than
the plan's framing suggests: Marques et al. is a 4-page arXiv preprint
with zero formal theorems and one worked example. Their verbatim statement: "we wish
to apply our inference algorithm to more elaborate examples and pursue formal proofs
of soundness and completeness that we believe will hold, but do not have yet done."
The row-expansion mechanism they sketch is itself borrowed from Pottier (1998), which
does have formal proofs — but Pottier's setting predates algebraic subtyping and does
not cover the union/intersection interaction. This is the soundness gap documented in
`doc/whatif/plans/typing-cluster.md` §D2.

The typing cluster's algebraic data types (C1 sprint) discriminate union variants
structurally by key set: `[ok: v]` and `[err: msg]` are distinct because they have
different keys. The narrowing cluster's Phase 4 (false-branch narrowing) is gated
on negation types: after `[int? x]` fails, the type system cannot express "x is
definitely not Int."

### What's Missing

1. **Proven soundness for row extension under algebraic subtyping.** Marques et al.
   provide an implementation template but without a proof.
2. **Negation types.** `~A` as a first-class type is absent. The constraint solver
   for union/intersection types requires it internally (not just for user annotations).
3. **Typed field removal.** `[remove dict "field"]` exists in stdlib but its return
   type is the imprecise `Dict` — the type system cannot express "this dict minus the
   field `debug`."
4. **Principal types for union-returning expressions.** `[if cond [ok: v] [err: msg]]`
   infers `Any` today, not `[ok: T] | [err: Str]`.

## Why BAS

Chau & Parreaux (POPL 2026) prove BAS sound via five characteristic Boolean
homomorphisms — published at the top venue, peer-reviewed, with a working artifact.
MLstruct (Parreaux & Chau 2022, OOPSLA), the language whose type system BAS formalizes,
is a working implementation with principal type inference for union and intersection
types and pattern matching — already proven sound and complete.

BAS encodes extensible records *without row variables* using the same Boolean algebra
that encodes union and intersection. This is not a patch on top of Simple-sub; it is a
reformulation of the whole type algebra where row extension is a derived property of
the Boolean lattice rather than a specialized mechanism.

The core observation: field *absence* is a Boolean property of a type. Width subtyping
— `@[name: Str  age: Int]` satisfies `@[name: Str]` — falls directly out of conjunction
elimination: `{name: Str} ∧ {age: Int} <: {name: Str}` by definition of `∧` as the
greatest lower bound. No row variable gymnastics.

## Design

### The BAS Type Algebra

The type grammar is a distributive Boolean algebra:

| Formal | tinct annotation | Notes |
|--------|-----------------|-------|
| `A \| B` | `@[A B]` | positional entries = union members — existing |
| `A & B` | `@[[all A B]]` | `[all ...]` prefix bracket in annotation position; multiple args = intersection of all members |
| `~A` | `@[[without A]]` | `[without A]` prefix bracket in annotation position; negation of A |
| `⊤` | `@Top` | true supertype; with B2 `Any` split |
| `⊥` | `@Never` | true bottom; with B2 |
| `{f: A}` | `@[f: A]` | single-field record — existing dict annotation |
| `α` | `@a` | lowercase type variable name — existing |
| `μα.A` | `@[AliasName ...]` | recursive types are always declared with a name via `[type ...]`; users write the alias name everywhere — the μ form is a compiler-internal representation that never surfaces to users or in error messages |

Multi-field records are not a primitive. `{x: τ₁, y: τ₂}` is syntax sugar for
`{x: τ₁} ∧ {y: τ₂}` (Parreaux & Chau 2022, §2.2.2, following Reynolds 1997). Under
BAS, `@[x: τ₁  y: τ₂]` would mean exactly this — a multi-field annotation is
reinterpreted as an intersection of single-field constraints. The annotation syntax
requires no change; what changes is the internal type representation (from
`Type::Record({x:τ₁, y:τ₂}, Open)` to `{x:τ₁} ∧ {y:τ₂}`).
This is the key that eliminates row variables: field-set operations are just conjunction
and disjunction over single-field record types, governed by Boolean algebra axioms.

`[all ...]` and `[without ...]` are valid type expressions wherever a type expression is valid:
as inline annotations (`v@[[all A B]]`), as the body of a `[type ...]` alias declaration
(`T: [type [all A B]]`), and in TypeAssert position (`[@[all A B] expr]`). They are not
infix — the bracket is the expression.

**Disambiguation from union annotation:** positional entries in `@[...]` are union
members when there are multiple entries (`@[Int Str]` → `Int | Str`). When the
single positional entry is itself a list whose first token is `all` or `without`, it
is an intersection or negation type, not a union member. Examples:
`@[[all Int Num]]` → `Int & Num`; `@[[without Int]]` → `~Int`;
`@[[all Int Num] Str]` → `(Int & Num) | Str` (intersection entry unioned with Str).

Subtyping: `A <: B` iff `A & ~B` is uninhabited. Subtyping is decidable and always
terminates on well-formed inputs (Chau & Parreaux 2026, Theorem 7.6); the precise
worst-case complexity class is not established in the BAS paper.

### Why Negation Is Not Optional

Negation types (`~A`) are essential to the constraint solver, not just a user-facing
annotation. The core difficulty with union/intersection types in constraint solving is
constraints of the form:

```
τ₁ ≤ τ₂ ∨ α     (union on the right — a type variable α appears in a union)
α ∧ τ₁ ≤ τ₂     (intersection on the left — type variable α is intersected)
```

These arise in ordinary tinct programs:

```tinct
# τ₁ ≤ τ₂ ∨ α — a Str value flowing into a [Int Str] position
x: "hello"
[@[Int Str] x]   # @Str satisfies @[Int a]  (where a is fresh, resolves to @Str)

# α ∧ τ₁ ≤ τ₂ — false-branch narrowing
process: [fn [x@[Int Str]]
    [if [int? x]
        [+ x 1]          # x : Int  (true branch)
        [str-upper x]]]  # x : @[[all [Int Str] [without Int]]] — satisfies @Str
```

Without negation, the solver must backtrack or guess which disjunct to pursue. With
negation, these are rewritten losslessly using Boolean algebra (Parreaux & Chau 2022,
§3.2.1, C-Var1/2 rules — formal notation):

```
τ₁ ≤ τ₂ ∨ α   →   τ₁ & ~τ₂ ≤ α     (move τ₂ to left side as ~τ₂)
α & τ₁ ≤ τ₂   →   α ≤ τ₂ | ~τ₁     (move τ₁ to right side as ~τ₁)
```

For the false-branch example above the solver resolves, in tinct type notation:

```tinct
# @[[all [Int Str] [without Int]]] satisfies @Str
#   → @[Int Str] satisfies @[[all Str [without [without Int]]]]
#   double-negation elimination: [without [without Int]] = Int (Boolean algebra axiom)
#   → @[Int Str] satisfies @[Str Int]  ✓
# x in the false branch: @[[all [Int Str] [without Int]]]  — simplifies to @Str
```

The user writes `@[Int Str]` on the parameter. The narrowed type
`@[[all [Int Str] [without Int]]]` appears in LSP hover over `x` in the false branch;
the type simplifier reduces it to `Str` before display. If a user needs to annotate
with an intersection or negation explicitly, the forms are `x@[[all Int Str]]` and
`x@[[without Int]]`.

The result is always a constraint with a type variable alone on one side, so bounds
can be updated directly. The algorithm does not backtrack and yields principal types
(Parreaux & Chau 2022, §5.2, §5.3). This is the source of MLstruct's "no
backtracking" guarantee that Simple-sub shares but which would be lost without `¬τ`.

Consequence for tinct: `Type::Negation` is not a user convenience — it is load-bearing
infrastructure for the inference algorithm. Adding negation types in isolation (without
the associated constraint rewriting) would not be sufficient; the constraint solver
must be designed around the C-Var1/2 rewriting from the start.

### Width Subtyping and Open Records

Single-field record types `{f: τ}` are atoms of the Boolean algebra. Multi-field
records are their intersections. Width subtyping is then a theorem — a dict with more
fields is a subtype of one with fewer, because intersection elimination gives `A & B <: A`:

```
{name: Str} & {age: Int}  <:  {name: Str}
```

In tinct: any dict satisfying `@[name: Str  age: Int]` also satisfies `@[name: Str]`:

```tinct
person: [name: "Alice"  age: 30]   # inferred type: @[name: Str  age: Int]

greet: [fn [p@[name: Str]] [str "Hello " p.name]]

[greet person]   # ok — @[name: Str  age: Int] satisfies @[name: Str]
```

No row variable, no unification case, no `unify_remainders`.

**All structural record annotations are open.** Under BAS, `{name: Str}` means "any
value with at least a `name: Str` field." There is no structural closed-record type
in the Boolean algebra — closing a record would require intersecting with the negation
of every conceivable absent field, which is not expressible over an open set of labels.
This is the correct semantics for a configuration language: extra fields are tolerated,
not rejected.

The `...` and `...rest` annotation syntax is retired:

- `...` (anonymous open marker) — was a signal that a record is open. Redundant under
  BAS; all structural record annotations are open by default. Removed.
- `...rest` (named row variable) — was used to thread extra fields through a function
  by naming the row remainder. Not used anywhere in `stdlib/`. Under BAS, the full
  polymorphism is inferred via type variable bounds; the explicit name was never needed
  in practice. Removed.
- `...name` in `[fn]` parameter position (variadic argument collector) — a completely
  separate feature at the value level. Unaffected; remains valid.

The open/closed distinction for ADT discrimination is provided by nominal class tags
(C2/C3), not by structural record annotation syntax. Exact-field validation at pipeline
boundaries belongs to structural contracts, not to the type annotation layer.

### Pattern Matching and Precise Arm Types

Case expressions (pattern matching) are typed by the I-Case3 rule (Parreaux & Chau
2022, Fig. 5), which introduces two fresh type variables and intersects the scrutinee
type with `#C` (matched class tag) in the true branch and `¬#C` in the false branch.
The result is a union of the two branch types.

Applied to tinct's `[match]`:

```tinct
result: [try risky]
[match result
    [ok: v]    v           # arm narrows scrutinee to #Ok variant
    [err: msg] [error msg]] # arm narrows scrutinee to @[[without #Ok]] = #Err variant
# Inferred result type: @[T Never] = @T   (union with Never simplifies — all variants covered)
```

Today the same expression infers `Any` because `unify` cannot represent the union of
branch types. Under BAS, `flatMap` (analogous to tinct's `[try]`/`[match]` pattern)
infers `∀α,β. (α → β) → (Some[α] ∨ None) → (β ∨ None)` — the precise type that
tells the caller exactly what they will receive (Parreaux & Chau 2022, §1).

### ADT Discrimination via Nominal Tags

BAS's S-RcdTop rule (Parreaux & Chau 2022, §2.2.2) collapses unions of records with
disjoint field sets to the top type:

```
⊤  ≤  {x: τ} ∨ τ'    when field x is not present in τ'
```

This gives `{ok: a} ∨ {err: Str} ≡ ⊤` — the union is indistinguishable from "any
value at all." S-RcdTop is not a restriction that can be relaxed: it is required for the Boolean
algebra to remain well-formed, which in turn makes RDNF normalization correct (MLstruct
extended, Lemma 7.3) and principal type inference work (Theorem 7.7). Theorem B.88 is
the separate type safety invariant (subtyping consistency — progress and preservation)
that also depends on S-RcdTop. Key-set discrimination of ADT variants is not expressible
in BAS's Boolean algebra.

Nominal class tags solve this directly. `#Ok` and `#Err` are nominal identities that
remain disjoint under the Boolean algebra via S-ClsBot:

```
#Ok & #Err ≤ Never   (unrelated nominal tags annihilate — no value can be both)
```

In tinct: a value can be `[Ok 42]` or `[Err "oops"]` but never both simultaneously,
so the union `Ok[a] | Err[Str]` has two genuinely distinct members. In annotation syntax:
`@[Ok[a] Err[Str]]`. Pattern matching refines the scrutinee's type in each arm using
the nominal tag (see §`infer_match()` Style Enforcement).

So `(#Ok & {v: a}) | (#Err & {msg: Str})` is a proper discriminated union —
pattern matching refines each arm type via the nominal tag rather than the key set.

This aligns with what the typing cluster's C2/C3 nominal variant sprints already
plan. Under BAS, nominal constructors are not an optional layer over structural ADTs
— they are the ADT encoding. Users write `[Ok v]` and `[Err msg]` to create nominal
ADT values. `[ok: v]` remains valid as a plain structural dict but carries no nominal
tag and does not match nominal union arms.

`from-json '{"ok": 42}'` produces structural `{ok: Int}`, not `Ok[Int]`. Structural
dicts from JSON must be explicitly lifted into nominal ADT values via constructor calls.
`[match]` arm syntax unambiguously determines the dispatch mode — no scrutinee type
annotation needed at runtime. `[ok: v]` (lowercase, colon) is a structural field
pattern; `[Ok v]` (uppercase, no colon) is a nominal constructor pattern. The type
checker enforces consistency: when the scrutinee has a declared union type, arm
patterns must use the style that matches the union's encoding, or a type error is
produced. At `Any` scrutinee type both styles compile without error.

## Scope of Changes to tinct

### Type Representation (`src/types.rs`) — Fundamental

`RowTail::RowVar(u32)` is removed. The four-case `unify_remainders` algorithm is
removed. In their place:

- `Type::Negation(Box<Type>)` is added as a first-class type variant
- Multi-field records become syntactic sugar: `{x: τ₁, y: τ₂}` → `{x: τ₁} ∧ {y: τ₂}`
  at the type representation level
- `RowTail::Empty` and `RowTail::RowVar` are both removed — the open/closed distinction
  does not exist at the structural level under BAS; all record annotations are open
- `is_subtype` is replaced by the BAS subtyping judgment (RDNF normalization + the
  algebraic subtyping rules from Fig. 4 of Parreaux & Chau 2022)

The existing `Type::Union` and (from D2) `Type::Intersection` remain; `Type::Negation`
joins them as the third Boolean operation, and together they form the complete algebra.

BAS's formal development uses string labels. Tinct's integer-keyed records (`{0: "a",
1: "b"}`) extend the label set; since integer and string labels are always disjoint,
S-RcdTop applies across them as expected and the proof obligations carry over naturally.

### Constraint Solver (`src/typecheck.rs`) — Fundamental

The Robinson unification + row-variable binding approach is replaced by the MLstruct
constraint solver:

- Each type variable carries `lower: Vec<Type>` and `upper: Vec<Type>` bounds, as in
  Simple-sub / D2
- New: bounds can contain negated types (`lower: [~Int]` = "definitely not Int")
- **C-Var1/2 rewriting**: when a constraint has a union on the right or an intersection
  on the left involving a type variable, the constraint solver uses `¬τ` to move terms
  across the inequality, producing a constraint with the type variable isolated
- Constraint normalization to RDNF (Reduced Disjunctive Normal Form) ensures the
  algorithm terminates and yields unique results without backtracking
- `unify_remainders` is gone; field-set operations are just conjunction over single-field
  record types

### ADT Encoding (`src/typecheck.rs`, `src/eval.rs`) — Major

The typing cluster's C1 (structural ADTs) and C2/C3 (nominal variants) converge under
BAS. C1's key-set discrimination is superseded: under BAS, `[Ok v]` and `[Err msg]`
are nominal values carrying `#Ok` and `#Err` tags (exactly C2/C3 `Value::Variant`).
`[match]` arm discrimination uses the nominal tag via I-Case3, not key-set presence.

C1 structural ADT declarations (`Result: [type [ok: a] [err: Str]]`) are reinterpreted:
the `ok`/`err` field names become the nominal constructor names `Ok`/`Err`, and the
field payload type becomes the constructor's argument type. C1 as a standalone sprint
is subsumed by BAS — the typing cluster's C1 sprint registers the type alias and the
union type, but the runtime discrimination model is C2/C3 nominal.

Structural dicts from `from-json` are not ADT values and do not match nominal union
arms. Explicit lifting is required.

### `infer_match()` Style Enforcement (`src/typecheck.rs`) — Major Extension

The C5 exhaustiveness sprint already plans `infer_match()` to extract the scrutinee's
`Type::Union`, correlate arms to union members, and check coverage. Under BAS,
`infer_match()` gains a second responsibility alongside coverage: **pattern style
enforcement**.

The `Type::Union` members already encode the expected arm style in their variant:

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

When `find_compatible_member` returns `None`, a style mismatch error is produced with
a specific correction:

```
error: pattern style mismatch
  --> config.llt:5:5
  |
5 |     [ok: v]    [process v]
  |     ^^^^^^^  structural field pattern
  |
  Result is a nominal union — use constructor pattern [Ok v], not field pattern [ok: v]
  note: Result declared as [type [Ok a] [Err Str]] at line 1
```

The correlation result also drives arm body narrowing: the matched union member
provides the precise type to narrow into the arm's environment (`v : TypeVar(a)` for
`[Ok v]`, or `v : TypeVar(a)` for `[ok: v]`). This replaces any "try all members"
fallback with a precise single-member narrowing.

**Mixed unions** work naturally — each arm's pattern is correlated to whichever member
has a compatible style, regardless of position:

```tinct
Mixed: [type [ok: a] [Err Str] "pending"]

[match x
    [ok: v]    [process v]      # → correlates to Record([ok:a])
    [Err msg]  [handle msg]     # → correlates to NominalVariant("Err", Str)
    "pending"  [wait]           # → correlates to StringLiteral("pending")
    _          [error "???"]]   # → wildcard, compatible with all
```

**At `Any` scrutinee**: `infer_match()` skips union correlation entirely. Both pattern
styles compile, no style errors, no exhaustiveness checking, arm bodies typed as the
join of their body types.

This is an extension of the C5 sprint's `infer_match()`, not a separate sprint. The
coverage algorithm in `src/coverage.rs` is unchanged; the style check runs in the
type checker layer that calls it.

### `$merge` Gets a Precise Type

`$merge` produces a value with *all* fields from both inputs combined — its type
is the *intersection* of the two input types, not their union. Under BAS, `[merge
{name: "Alice"} {age: 30}]` has type `{name: Str} ∧ {age: Int}` = `{name: Str,
age: Int}` — a precise multi-field record type. S-RcdTop collapses *union* types over
disjoint-field records to `⊤`, but `$merge` produces an intersection, which is not
affected by S-RcdTop.

When the input field sets are type variables (open records), `$merge`'s result type
is the intersection of the two type variables, which BAS can express directly as
`T & U`. This is at least as precise as the current row-variable encoding, and
arguably cleaner — the result type names both input types explicitly rather than
threaded through a shared row variable.

The S-RcdTop limitation only surfaces if code tries to form a *union* of possible
`$merge` inputs (e.g. "the result might be either `{x:a}` or `{y:b}`") — which is
the ADT discrimination scenario, not the merge scenario.

### NSSE: Polymorphic Signature Checking

MLstruct's non-structural subtype entailment (NSSE) is undecidable (Parreaux & Chau
2022, §6). NSSE arises when checking whether an implementation satisfies a
user-provided polymorphic type annotation containing universal quantifiers. In
practice: if a user writes `f@[Fn@b [a]]`, the type checker verifies structural
entailment but cannot always resolve the full polymorphic constraint.

TypeAssert under BAS verifies structural entailment; quantified annotations are
accepted on a best-effort basis. This matches both MLstruct's position and tinct's
existing TypeAssert behavior — annotations with universal quantifiers were never
deeply verified. Under BAS the behavior is formally characterized rather than
incidentally permitted.

### Error Messages

Row-unification errors ("cannot unify open record with row variable r₁ against closed
record") are replaced by Boolean constraint violation messages. Crucially, the
constraint solver tracks which constraints originated from which source spans, so error
messages can show the chain of constraints that led to a bound conflict — not just the
point where unification failed. This is the constraint-provenance approach that MLsub
lacked and that Simple-sub improved; MLstruct/BAS extends it further with negation-aware
provenance.

### Type Simplification

Inferred types under BAS can be complex Boolean expressions. BAS adoption includes
a type simplification pass: pushing types into disjunctive normal form, inlining
non-cyclic bounds, reducing complements. MLstruct (§3.4) does the same — its
simplifier is approximately 1000 lines of the 5000-line implementation. Without
simplification, inferred types in error messages and LSP hover would be unreadable.

RDNF normalization is invoked during inference at constraint simplification steps,
not only at subtyping check time. The normalization is exponential in the number of
distinct atomic types in the worst case; MLstruct mitigates this by computing RDNF
lazily and maintaining a cache of currently-processed subtyping relationships (Parreaux
& Chau 2022, §5.3). Type simplification correctness (simplified type is equivalent to
original) is empirically validated via MLstruct's 4000+ test suite, not formally proven;
tinct inherits this status.

## What Changes for Users

### Inferred Union Types for `if` and `[match]`

The most user-visible improvement: expressions that return different types in different
branches get precise union types instead of `Any`.

```tinct
# Today: result type is Any
result: [if cond [ok: v] [err: "failed"]]

# With BAS: result type is Ok[T] | Err
result: [if cond [ok: v] [err: "failed"]]

# Consequence: exhaustive match is type-checked without annotation
[match result
    [ok: v]    [process v]
    [err: msg] [log msg]]   # ✓ — all variants provably covered
```

This removes the need for `[@Result expr]` TypeAssert annotations in common patterns.

### ADT Construction Uses Nominal Constructors

Nominal ADT values require explicit constructor calls. `[ok: v]` is a plain structural
dict; `[Ok v]` is a nominal ADT value carrying the `#Ok` tag and matching `[match]`
arms on union types:

```tinct
# Structural dict (open record, without an ADT value):
result: [ok: computed-value]     # type: {ok: T}

# Nominal ADT value (matches union arms):
result: [Ok computed-value]      # type: Ok[T]  — carries #Ok tag

# JSON input is always structural; must be lifted explicitly:
raw: [from-json input]           # type: {ok: Int}  — structural, not Ok[Int]
result: [Ok [get "ok" raw]]      # type: Ok[Int]  — explicit lift
```

`[match]` dispatch mode is syntactically encoded in each arm: `[ok: v]` is always
a structural field check; `[Ok v]` is always a nominal tag check. The two can
coexist in a single `[match]` on a mixed union. When the scrutinee type is a
declared union, the type checker correlates each arm's pattern to the appropriate
union member by style — a style mismatch is a type error. See §`infer_match()` Style
Enforcement below.

### False-Branch Narrowing

After `[if [int? x] ...]`, the false branch knows `x : ~Int`. Combined with a union
annotation, this is precise:

```tinct
process: [fn [x@[Int Str]]
    [if [int? x]
        [+ x 1]        # x : Int  (true branch — already works today)
        [str-upper x]]]  # x : Str  (false branch — requires ~Int narrowing)
```

Today the false branch types `x` as `Int | Str` (unchanged). With BAS, `(Int | Str) &
~Int = Str` — the precise type that makes the `str-upper` call statically verifiable.

### Field Removal as a Typed Operation

`[remove dict "field"]` already exists in stdlib, but today its return type is the
imprecise `Dict`. Under BAS, `remove` gets a precise type: the input type intersected
with the negation of the removed field.

```tinct
config-without-debug: [remove config "debug"]
# Inferred type: @[[all Config [without [debug: Top]]]]
# Any consumer that expects config-without-debug to lack a debug field is satisfied.
```

No new syntax — `remove` is an existing stdlib function. The improvement is purely
in the inferred return type, which under BAS can express "this dict, minus the `debug`
field."

### Record Annotations Are Always Open

`@[name: Str, host: Str]` means "at least these fields" — a richer config dict with
additional fields satisfies the annotation. There is no annotation syntax for an exact
field set at the structural level. `...` is no longer written in annotations (it was
redundant under BAS). Existing programs that use bare `[name: Str]` annotations see no
behavioral change — they were already open in effect, since callers routinely passed
richer dicts.

### Enforcing Closed Dicts via Structural Contracts

When exact field sets matter — pipeline formatters, strict schema validation — the
runtime `validate` layer handles it. This is a two-layer pattern:

```tinct
# Layer 1: type annotation (static, open)
# Guarantees %  has at least port: Int and hostname: Str.
# Extra fields are accepted — type checker won't complain.
%@[port: Int  hostname: Str]

# Layer 2: runtime exact validation (dynamic, closed)
# Rejects any % that has fields beyond port and hostname.
[validate [
  port:     [type: "Int"  min: 1  max: 65535]
  hostname: [type: "Str"  pattern: "^[a-z0-9.-]+$"]
  exact:    true    # no fields beyond those declared in this schema
] %]
```

The `exact: true` schema key — to be added to the structural contracts `validate`
builtin (see `doc/whatif/structural-contracts.md` §Phase 2) — walks the data value
and rejects any key not named in the schema dict. It is the runtime equivalent of
what the current closed record type `[port: Int  hostname: Str]` (with `RowTail::Empty`)
provides statically today.

The two layers are complementary and usually both present at pipeline boundaries:
the type annotation catches structural errors at type-check time (wrong types,
missing fields), and `validate` catches exactness violations and domain constraints
at runtime (extra fields, out-of-range values). Callers who produce the dict never
need to know about the `exact:` constraint — it fires at the boundary, with blame
pointing to the producing stage.

### Better Error Messages for Record Mismatches

Row variable names (`r₁`, `r₂`) disappear from error messages. Constraint provenance
chains show exactly which field constraints conflict and why, traced back to their
source locations.

## What This Reformulation Unlocks

### Principal Types for the Full Language

The headline result of MLstruct (Parreaux & Chau 2022, §5): the type inference algorithm
yields principal types for programs with union types, intersection types, negation types,
and pattern matching — simultaneously. No other system achieves all of these with
principal type inference. Today, tinct's `if` expression returns `Any` for
mixed-type branches. With BAS, `[if cond [ok: v] [err: msg]]` gets a principal type
`[ok: T] | [err: Str]` that is the most specific type the expression can have.

### Exhaustiveness Without Annotation

With principal union types, `[match]` arms can be checked for exhaustiveness without
requiring a `[@Result res]` TypeAssert on the scrutinee. The scrutinee's inferred type
is already a union; the type checker has the variant set. Exhaustiveness checking
becomes the default for pattern matching, not a bonus for annotated code.

### False-Branch Narrowing

False-branch narrowing is the missing half of occurrence typing. Narrowing Phases 1–3
(in the typing cluster) handle the true branch: after `[int? x]`, the true branch
knows `x : Int`. But the false branch still carries `x`'s original type unchanged.
This is the gap BAS closes.

#### Why the Typing Cluster Can't Do It

Simple-sub and Marques et al. have no negation types. The constraint `x : ~Int` —
"x is definitely not Int" — cannot be expressed in D2's type algebra. Even if the
type checker forks `env_false`, there is no type to put in it. The false branch
must conservatively keep the original type.

#### How BAS Provides It

Under BAS, `~A` is a first-class type, and the I-Case3 rule (Parreaux & Chau 2022,
Fig. 5) naturally produces `¬#C` in the false branch when pattern matching on a
class tag. For primitive type guards, C-Var1/2 constraint rewriting propagates the
negation: after `[int? x]` fails, the constraint `x ≤ ~Int` is added to the false
branch's constraining context.

The key payoff is **union elimination**: when `x : A | B` and the true branch
establishes `x : A`, the false branch computes:

```
x : (A | B) & ~A  =  B    (when A and B are disjoint)
```

The intersection with `~A` eliminates the matched variant precisely, leaving just
`B`. This holds when A and B are disjoint — nominal tags via S-ClsBot, or unrelated
type constructors (e.g. `Int` and `Str`). For overlapping types (e.g. `A = Int`,
`B = Number`), the result is `B & ~A = Number & ~Int`, which does not simplify
further but is still a valid and expressible type. This is direct, exact, and requires
no annotation.

#### Concrete Patterns

**Type predicate guard:**

```tinct
process: [fn [x@[Int Str]]
    [if [int? x]
        [+ x 1]            # x : Int   (true branch — works in typing cluster)
        [str-upper x]]]    # x : Str   (false branch — (Int|Str) & ~Int = Str)
```

Without false-branch narrowing, the `else` branch types `x` as `Int | Str` and
`str-upper` produces a type error or requires `[@Str x]`.

**Match arms on declared unions:**

```tinct
Result: [type [ok: a] [err: Str]]
result: [@Result [try risky]]

[match result
    [ok: v]    [process v]     # arm context: result ∩ #ok  → v : a
    [err: msg] [handle msg]]   # arm context: result ∩ ¬#ok → msg : Str
```

Under BAS each arm's type is exact. The second arm knows `result : ~#ok ∧ {msg:Str}`
— not "some union member that isn't `[ok: v]`" but the specific `[err: Str]` shape.

**Field presence guard:**

```tinct
[if [has? config "debug"]
    [process config.debug]    # config has field debug
    [use-defaults]]           # config : ~{debug: ⊤}  — guaranteed no debug field
```

The false branch knows the field is absent, enabling exhaustive structural dispatch
without explicit closed-record annotations.

#### What This Enables Beyond Narrowing

With false-branch narrowing in place, **exhaustiveness checking on inferred unions**
works without TypeAssert. Today the exhaustiveness checker (C5 sprint) needs a
`[@Result res]` annotation to know the union type to check against. Under BAS,
if `res` was produced by `[try ...]` and `[try]` infers `[ok: T] | [err: Str]`,
the scrutinee already carries a union type and exhaustiveness is checked automatically.
No annotation required.

The interaction with `[match]` completes the picture: each arm narrows the scrutinee
with its pattern's tag, and the complement of matched tags accumulates implicitly.
A wildcard arm in a match over a union type carries `x : ⊥` when all variants are
already covered — the type checker can flag the wildcard as unreachable.

### The Soundness Gap Is Closed

D2 proceeds with an accepted risk: Marques et al. soundness is conjectured. BAS removes
that risk with a published, peer-reviewed soundness proof that covers the record-extension
mechanism. tinct can cite a proven foundation for its full type system.

### A Complete Theoretical Basis

BAS + MLstruct gives tinct a type system proven sound and complete over the full algebra:
unions, intersections, negations, records, recursive types, and row extension — within
one framework. As new features are added (numeric refinements, structural contracts,
gradual types), they extend a proven foundation rather than an accumulation of
independent mechanisms.

## What This Reformulation Loses

Every meaningful type system change trades capabilities. BAS is not an exception.

### Closed Structural Record Types

The most concrete loss. Today `[name: Str, age: Int]` (without `...`) is a closed
record type — it accepts only values with exactly those two fields. Under BAS all
structural record annotations are open, so the same annotation becomes "at least
name: Str and age: Int." There is no structural type syntax for exact field sets;
closing a record in BAS's Boolean algebra would require intersecting with the negation
of every conceivable absent field name, which is not expressible over an open label set.

Exact field enforcement moves to the runtime `validate` layer with `exact: true` (see
§Enforcing Closed Dicts via Structural Contracts). For a configuration language this
is the right tradeoff — accepting extra fields is correct behavior for config that
evolves — but it is a real semantic change for any code that relied on closed record
types to reject unexpected fields at type-check time.

### `...` and `...rest` Annotation Syntax

Both retire. `...` was the signal that a record was open — redundant when openness is
the default. `...rest` was a named row variable for threading extra fields through a
function explicitly — never used in `stdlib/`, with no concrete use case identified.
The function-parameter `...name` (variadic argument collector) is a separate feature
and is unaffected.

### Quantified Annotation Checking (NSSE)

TypeAssert with polymorphic annotations (`f@[Fn@b [a]]`) is checked at the structural
entailment level only. Verifying that an implementation satisfies a universally
quantified annotation is undecidable in BAS (NSSE, Parreaux & Chau 2022, §6). In
practice this means quantified annotations are accepted on a best-effort basis, which
matches tinct's existing TypeAssert semantics — the behavior is unchanged, but the
limitation is now formally characterized rather than incidentally permitted.

### Union Precision for Records with Disjoint Field Sets

Rémy-style row variables can express that a value might be *either* a record with
fields `{x:a}` *or* a record with fields `{y:b}` — the union `{x:a} | {y:b}` is
meaningful and discriminable via the row variable. Under BAS, this union collapses to
`⊤` via S-RcdTop. The loss is specific to *union* types over records with disjoint
fields — the ADT discrimination scenario. Merge and intersection of disjoint-field
records are unaffected and remain precise. The resolution is nominal class tags, which
make discriminated unions expressible again.

## Prerequisites and Trigger

**Prerequisites:** D2 (`algebraic-subtyping` sprint) complete. BAS requires the
algebraic subtyping infrastructure D2 delivers: bisubstitution, bounds-carrying type
variables, `Type::Union`, `Type::Intersection`. C2/C3 (nominal variant sprints) should
also be complete or in progress — BAS adoption formalizes their encoding as the standard
ADT model.

BAS does not stack on D2's Marques et al. row-extension mechanism — it replaces it.
D2 delivers the bisubstitution infrastructure and union/intersection types; BAS reuses
these and supersedes the conjectured-soundness row-variable component. Implementors
should treat BAS adoption as completing and redirecting D2's row mechanism, not
extending it. The `RowTail::RowVar` infrastructure introduced by D2 is removed.

BAS adopts equi-recursive types (μα.A), matching MLstruct's proof foundation. Tinct's
current depth-limit guard becomes a performance heuristic rather than a correctness
mechanism; the C-Hyp hypothesis caching in the constraint solver handles termination
for recursive subtyping.

**Trigger — any one of:**
- Narrowing Phase 4 (false-branch narrowing) becomes a concrete need — it is only
  achievable via BAS, not via D2/Simple-sub
- The inferred-`Any` problem for `if`/`match` branch types becomes a measurable source
  of annotation burden in real tinct programs
- The nominal-variant adoption (C2/C3) reaches the point where nominal-tag ADTs are
  the default and structural-key discrimination is already deprecated in practice
- Phase 1 evaluation confirms worst-case subtyping paths are rare on real tinct programs

## References

- Chau, C.Y. & Parreaux, L. (2026). "The simple essence of Boolean-algebraic
  subtyping: semantic soundness for algebraic union, intersection, negation, and
  equi-recursive types." *Proc. ACM Program. Lang.*, 10(POPL), pp. 1353–1382.
  doi:10.1145/3776689. Preprint: https://lptk.github.io/files/boolean-algebraic-subtyping.pdf.
  Artifact: https://github.com/fo5for/sebas — Proves BAS sound via five characteristic
  Boolean homomorphisms; encodes extensible records without row variables.
  [algebraic subtyping, row polymorphism]
- Parreaux, L. & Chau, C.Y. (2022). "MLstruct: Principal type inference in a Boolean
  algebra of structural types." *Proc. ACM Program. Lang.*, 6(OOPSLA2), Article 141.
  doi:10.1145/3563304. Artifact: https://github.com/hkust-taco/mlstruct —
  Working implementation of BAS-style inference. Source of the C-Var1/2 constraint
  rewriting rules, RDNF normalization, I-Case3 pattern matching typing rule, and the
  S-RcdTop collapse of disjoint-field-set record unions. Direct implementation reference.
  [algebraic subtyping, row polymorphism, inference algorithm]
- Dolan, S. (2016). *Algebraic Subtyping.* PhD thesis, University of Cambridge.
  — Principal type proof (Theorem 4.1) for the non-row fragment. Co-NP-hard subtyping
  complexity class established here — BAS matches this, not worsens it. [algebraic subtyping]
- Marques, R., Florido, M. & Vasconcelos, P. (2024). "Towards algebraic subtyping
  for extensible records." arXiv:2407.06747. — D2's current planned row-extension
  foundation; soundness conjectured. BAS supersedes this if adopted.
  [algebraic subtyping, row polymorphism]
- Parreaux, L. (2020). "The simple essence of algebraic subtyping." In *ICFP '20*,
  Article 124. ACM. — Simple-sub: the bisubstitution algorithm that MLstruct extends.
  D2's inference algorithm; BAS builds on this foundation. [algebraic subtyping]
- Rémy, D. (1994). Type inference for records in natural extension of ML. In
  *Theoretical Aspects of Object-Oriented Programming*, pp. 291–346. MIT Press.
  — Current row polymorphism foundation; `RowTail::RowVar` and `unify_remainders`
  implement Rémy's approach. BAS adoption removes this mechanism. [row polymorphism]
