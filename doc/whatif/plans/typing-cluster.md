# Typing Cluster: Implementation Plan

Comprehensive plan for the 11 typing-related whatif proposals and their
phased implementation. Each proposal is a separate whatif document; this
plan connects them into a coherent dependency graph, identifies the
critical path, and groups work into independently shippable phases.

**Proposals covered:**

| # | Proposal | Whatif Document |
|---|----------|----------------|
| 0 | Null Semantics | `null-semantics.md` |
| 1 | Type Predicates | `completed/type-predicates.md` |
| 2 | Pattern Matching | `pattern-matching.md` |
| 3 | Let Binding | `let-binding.md` |
| 4 | Union Types | `union-types.md` |
| 5 | Algebraic Data Types | `algebraic-data-types.md` |
| 6 | Nominal Variants | `nominal-variants.md` |
| 7 | Type Classes | `typeclasses.md` |
| 8 | Gradual Typing | `gradual-typing.md` |
| 9 | Structural Contracts | `structural-contracts.md` |
| 10 | Numeric Types | `numeric-types.md` |
| 11 | Parameterized Type Aliases | `parameterized-type-aliases.md` |

**Proposal 0 note:** Null Semantics (`@Null` = `Type::Record(Row::Empty)`) is
a prerequisite to B1. It does not need a dedicated cluster sprint — the full
acceptance scope is:

- Mark `doc/whatif/null-semantics.md` `State: Accepted`
- Add `"Null" => Ok(Type::Record(Row::Empty))` to `resolve_type_name` (`src/typecheck.rs`)
- Update void-returning builtin type signatures from `Type::Any` to `Type::Record(Row::Empty)`:
  `emit`, `write`, `write-atomic`, `revoke-cap`, `mkdir`, `delete` (`src/types.rs`)
- Add `Null` to the type conventions table in `doc/05-type-annotations.md`
- Update `doc/11a-builtins.md` void-returning builtin signatures to show `fn@Null`
- Move entry in `doc/whatif/index.md` from Adopt Now → Accepted

These tasks may be folded into an existing sprint (e.g., `type-checker-fixes`)
or executed as a standalone micro-sprint before B1. Either way they must land
before `union-types` (B1) since `x@[String Null]` is the primary nullable
annotation example and requires `@Null` to resolve correctly.

---

## 0. Cluster Acceptance Procedure

When formally accepting any proposal in this cluster, apply these steps for
**each whatif being accepted** — in addition to the proposal-specific tasks
listed in §3 and §4:

1. **Mark the whatif doc**: add `**State:** Accepted — YYYY-MM-DD` as the
   second line of `doc/whatif/<name>.md` (after the `# What If:` title).
2. **Integrate spec content**: write the design into the named `doc/*.md`
   chapter(s) listed under "Spec chapters:" for that sprint. Write in present
   tense — no "planned", "will be", or TODO references.
3. **Update `doc/whatif/index.md`**: move the proposal's entry from its
   current adoption bucket (Adopt Now / Wait for Trigger / etc.) to the
   **Accepted** section. Add acceptance date as a third column.
4. **Update `doc/17-references.md`**: add any new citations the proposal
   introduces. Keep entries sorted by author.
5. **Create implementation sprints**: use the sprint task lists in §4 as the
   blueprint. Sprints go in TODO.md under the relevant `##` section.

**Proposal #0 (null-semantics)** is a special case: its acceptance tasks are
fully enumerated in the Proposal #0 note above — do not re-run the general
procedure; it is handled there.

---

## 1. Why These Features Form a Cluster

These 11 proposals are not independent feature requests. They form a
cluster because they collectively answer a single question: **how
expressive should tinct's type system be?** Each proposal extends the
type system's ability to describe data shapes, and each extension creates
demand for (or is blocked by) other extensions. Implementing any one of
them in isolation is possible for some, but produces diminishing returns
without its neighbors.

### Three Functional Groups

The cluster decomposes into three functional groups with distinct
motivations:

**Group I: Expression-Level Foundations** (proposals 1, 2, 3)

Type predicates, pattern matching, and let binding are syntactic and
evaluator features. They do not extend the type representation
(`Type` enum) at all. They extend what users can *write* and how the
evaluator *dispatches*. These are the tools that make type-level
features usable in practice --- without pattern matching, union types
and ADTs are declarations with no ergonomic consumption mechanism.

**Group II: Type Representation Extensions** (proposals 4, 5, 6, 7, 8, 11)

Union types, ADTs, nominal variants, type classes, gradual typing, and
parameterized type aliases all extend the `Type` enum, the subtyping
relation, or the inference algorithm. They answer: "what types can tinct
describe?" These are the core of the cluster. They interact heavily ---
ADTs require union types, nominal variants require ADTs, exhaustiveness
requires both union types and pattern matching, type classes require
parameterized aliases for higher-kinded types, and gradual typing's
`Any` split is a prerequisite for sound union subtyping.

**Group III: Validation and Constraints** (proposals 9, 10)

Structural contracts and numeric types extend the *runtime validation*
layer. They sit on top of the type system but do not extend inference.
Range constraints (`@[min: 0 max: 65535]`) are runtime contracts, not
type-level refinements. These are largely independent of the type
representation group and can proceed on their own timeline.

### Which Are Independent

The following proposals have no hard prerequisites on other proposals in
this cluster:

- **Type predicates** (1) --- standalone builtins, already accepted and
  implemented.
- **Let binding** (3) --- parser/evaluator change, no type system
  interaction.
- **Type classes Phase 1** (7.1) --- `deep-eq`/`shallow-eq` builtins,
  no type system changes.
- **Structural contracts Phase 1** (9.1) --- `%@Type` annotation, builds
  on existing TypeAssert.
- **Parameterized type aliases Phase 1** (11.1) --- parser change to
  `[type ...]`, backward compatible.
- **Numeric types Phase 1** (10.1) --- range annotation validation, no
  type representation change.

### Which Have Hard Dependencies

These proposals cannot be implemented without their prerequisites:

| Proposal | Hard prerequisite |
|----------|------------------|
| Pattern matching Phase 2+ | Type predicates (1) |
| Pattern matching Phase 3 | Let binding (3) for multi-expression arm bodies |
| ADTs Phase 2 | Union types Phase 2 (`Type::Union` must exist) |
| Nominal variants Phase 2 | ADTs Phase 2 + Pattern matching Phase 2 |
| Exhaustiveness checking | Union types + ADTs + Pattern matching Phase 5 |
| Gradual typing Phase 2 | Union types Phase 2 (forces the `Any` split) |
| Type classes Phase 2 | `Any` split from gradual typing Phase 2 |
| Recursive ADTs (Phase 4) | Parameterized type aliases Phase 2 |

### Which Are Alternatives

Two proposals offer **alternative solutions to the same problem**
(dual-dispatch typing):

- **Type classes** (7): `Functor f => (a -> b) -> f a -> f b`
- **Union types** (4): `(a -> b) -> (Dict a | Seq a) -> (Dict b | Seq b)`

Both solve the problem. Both can coexist. The choice of which to
implement *first* determines the shape of the type system for years.
This is Decision Gate 1 (see Section 6).

---

## 2. Dependency Graph

### Full Graph

Reading order: an arrow `A --> B` means A must be implemented before B.

```
                          PHASE A                           PHASE B
                       (foundations)                    (type primitives)
                    +-----------------+            +---------------------+
                    |                 |            |                     |
 type-predicates ---+---> pattern-matching Ph2 --->| pattern-matching Ph3 |
   (DONE)           |     (type+literal match)     | (dict/seq destruct)  |
                    |          |                    +---------------------+
                    |          |                              |
 let-binding -------+----------+                             |
   (no deps)        |                                        |
                    |                                        v
                    |        PHASE B                  PHASE C (algebraic)
                    |   (type primitives)         +------------------------+
                    |                             |                        |
                    |   union-types Ph2 ----------+--> ADTs Ph2            |
                    |   (Type::Union,             |    ([type T1 T2] decl) |
                    |    subtype rules)           |         |              |
                    |         |                   |         v              |
                    |         |                   |  nominal-variants Ph2  |
                    |         |                   |  (Value::Variant,      |
                    |         |                   |   constructors)        |
                    |         |                   |         |              |
                    |         v                   |         v              |
                    |   gradual-typing Ph2 -------+--> exhaustiveness      |
                    |   (Any -> Unknown+Top)      |    (PM Ph5 + ADTs Ph3) |
                    |         |                   +------------------------+
                    |         |
                    |         v                      PHASE D (advanced)
                    |   type-classes Ph2          +------------------------+
                    |   (constrained vars,        |                        |
                    |    Eq a => ...)              |  type-classes Ph3      |
                    |                             |  (full Haskell-style)  |
                    |                             |                        |
                    |   param-type-aliases Ph2 ---+--> recursive ADTs Ph4  |
                    |   ([type [a] body])          |    (Tree a = ...)     |
                    |                             |                        |
                    |   structural-contracts -----+--> numeric-types Ph1   |
                    |   (%@Type, validate)         |   (range annotations) |
                    |                             +------------------------+
                    +-----------------+

  INDEPENDENT TRACKS (no cross-deps within cluster):
    - typeclasses Ph1 (deep-eq/shallow-eq) --- any time
    - numeric-types Ph2 (Decimal) --- any time
    - param-type-aliases Ph1 (parser) --- any time
```

### Critical Path

The longest dependency chain determines the minimum calendar time:

```
type-predicates (DONE)
  --> pattern-matching Ph2 (basic match)
    --> pattern-matching Ph3 (dict/seq destructuring)
      --> union-types Ph2 (Type::Union)
        --> ADTs Ph2 ([type T1 T2 ...] declarations)
          --> nominal-variants Ph2 (constructors + match)
            --> exhaustiveness (pattern-matching Ph5 + ADTs Ph3)
```

This chain has **6 dependent steps** after type-predicates. Each step
must be independently testable and shippable.

A parallel critical path runs through the type system:

```
union-types Ph2
  --> gradual-typing Ph2 (Any split)
    --> type-classes Ph2 (constrained vars)
      --> type-classes Ph3 (full classes, dictionary passing)
```

These two paths converge at exhaustiveness checking, which requires both
pattern matching maturity and union type support.

### Topological Ordering

A valid total order respecting all dependencies:

1. Type predicates (DONE)
2. Let binding
3. Pattern matching Phase 2
4. Pattern matching Phase 3
5. Union types Phase 2
6. Gradual typing Phase 2
7. ADTs Phase 2
8. Parameterized type aliases Phase 2
9. Type classes Phase 2
10. Nominal variants Phase 2
11. Pattern matching Phase 4 (guards, or-patterns)
12. Pattern matching Phase 5 + ADTs Phase 3 (exhaustiveness)
13. Recursive ADTs (Phase 4)
14. Type classes Phase 3
15. Union types Phase 3 (full algebraic subtyping)

Items 1-4 can proceed without any type representation changes. Items
5-9 are the heart of the cluster. Items 10-15 are long-term.

---

## 3. Recommended Phased Implementation Order

### Phase A: Foundations (independent, unlock everything else)

No type representation changes. Parser, evaluator, and ergonomic
improvements that make all subsequent phases usable. Each item is
independently shippable.

**A1. Let Binding** (`let-binding.md`)

Multi-expression function bodies via sequential scoping:

```tinct
process: [fn [data]
    [cleaned: [clean data]]
    [transform cleaned]]
```

- **Scope:** Parser change to `fn` body parsing. Desugar to nested
  `Expr::Sequential` at parse time. No evaluator or type checker changes
  if desugared.
- **Formal model:** `let*` semantics (Ariola & Felleisen 1997). Non-
  recursive sequential bindings. Each step's thunks are shared (not
  copied) --- preserves Launchbury (1993) sharing invariant.
- **Risk:** None. Fully backward compatible (single-expression bodies
  are length-1 sequences).
- **Unlocks:** Multi-expression match arm bodies (Pattern matching
  Phase 3+), more ergonomic function definitions everywhere.

**A2. Pattern Matching Phase 2 --- Basic Match**

Type and literal patterns, wildcard, variable binding, and `$name` pin:

```tinct
[match x
    Int   [+ x 1]
    Str   [str "got: " x]
    42    "the answer"
    _     x]

# $name pin: match against existing variable value (no new sigil needed)
[match result
    $expected  "matched!"   # pin: result must equal current value of `expected`
    other      "no match"]  # bind: `other` gets result's value
```

- **Scope:** `match` keyword + `Expr::Match` AST node. Evaluator:
  materialize scrutinee, try arms top-to-bottom. Type checker: initially
  typed as `Any`. `$name` in pattern position resolves via normal variable
  lookup and compares by equality — no new evaluator mechanism.
- **Formal model:** Augustsson (1985) pattern compilation. Sequential
  arm testing is the simplest compilation strategy (no decision trees
  needed at this phase).
- **Risk:** Low. One new keyword, one new AST variant. Every exhaustive
  `match` on `Expr` gains one arm. `$name` pin reuses existing `$` sigil
  semantics — bare name = bind, `$name` = reference existing value.
- **Unlocks:** Type dispatch without `type-of` string comparisons.
  Foundation for all subsequent pattern matching phases.
- **Depends on:** Type predicates (DONE).

**A3. Pattern Matching Phase 3 --- Structural Destructuring**

Dict patterns, seq patterns, nested patterns, and path-key patterns:

```tinct
[match [try risky]
    [ok: v]    v
    [err: msg] [error msg]]

[match xs
    [seq h t]  [process h t]
    _          "empty"]

# Path-key: DRY deep structure matching (pure parser desugaring)
[match config
    [cluster.primary.tls: [cert: c  key: k]
     cluster.primary.host: h]
    [connect-tls c k h]
    _ [error "no tls"]]
# desugars to:
# [cluster: [primary: [tls: [cert: c  key: k]  host: h]]]
```

Path-key desugar rules:
- `[a.b.c: v]` → `[a: [b: [c: v]]]` — works for any value `v` (leaf or subtree)
- `[a.b.c: v  a.b.d: w]` → `[a: [b: [c: v  d: w]]]` — shared prefix merged
- Three granularities: `[a.b.c: v]`, `[a.b: [c: v]]`, `[a: [b: [c: v]]]` all equivalent
- Intermediate nodes always open (consistent with row polymorphism)
- Integer path segments (`a.0.name`) wait for `access-pipeline` to land

- **Scope:** `Pattern::Dict` and `Pattern::Seq` variants. Path-key
  desugaring in the parser (pure transformation, no evaluator change).
  Evaluator: recursive pattern matching with environment extension.
  Lazy dict matching --- only matched keys forced.
- **Formal model:** Maranget (2008) decision trees for nested patterns.
  Lazy forcing semantics follow Wadler (1987) views.
- **Risk:** Moderate. Recursive pattern matching interacts with lazy
  evaluation. Path-key desugaring is low risk (parser-only).
- **Unlocks:** Self-hosting dual-dispatch builtins. `try` result
  handling. ADT consumption. Readable deep config matching. This is
  the phase that makes pattern matching *useful*.
- **Depends on:** Phase A2 (basic match). Let binding (A1) for
  multi-expression arm bodies. `access-pipeline` for integer path
  segments (string-only path-key lands with A3).

### Phase B: Type System Primitives (enable type-level reasoning)

These extend `Type`, `is_subtype`, and the inference algorithm. Each is
independently shippable but they build on each other.

**B1. Union Types Phase 2 --- Annotation-Only Unions**

`Type::Union(Vec<Type>)` with subtyping rules. Unions appear only in
explicit annotations and builtin signatures --- `unify` never produces
them.

**Syntax:** Positional entries in `@[...]` annotations are collected
and unioned. No infix operator (avoids collision with `|` pipe operator
from access-pipeline). Named entries remain metadata.

```tinct
x@[Int Null]                          # type: Int | Null
x@[String Null default: ""]          # type: String | Null, with default
nullable-name: [fn [x@[Int Null]] ...]
map: [fn [f@[Fn@b [a]]  xs@[Dict Seq]] ...]
Result: [type [ok: a] [err: Str]]     # union of two record types
```

**Desugar rule** (annotation resolution, not a parser pass):
```
x@[T1 T2 ...named...]  →  x@[type: [T1 T2]  ...named...]
x@[T]                  →  x@[type: T]    (single positional unwraps)
x@T                    →  x@[type: T]    (existing shorthand, unchanged)
```
The type resolver handles `type: [T1 T2]` as `Union(T1, T2)`. Existing
`x@[type: Number  default: 30]` (no positional entries) is unchanged.

- **Scope:** New `Type::Union` variant. Three subtyping rules:
  `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` (Pierce 2002,
  Chapter 15). Annotation resolver collects positional entries into
  union — no parser change needed. Normalize unions to canonical form
  (sorted, deduplicated, flattened).
- **Formal model:** Standard covariant union subtyping (Pierce 2002,
  §15.7). Decidable, preserves transitivity. Principal types preserved
  because unions only appear in annotations — inference never produces
  them, so Robinson's (1965) MGU guarantee is unaffected.
- **Risk:** Moderate. `Type::Union` propagates through `is_subtype`,
  `apply_substitution`, `collect_type_vars`, `display`, `occurs_in`.
  Every exhaustive `match` on `Type` gains one arm.
- **Unlocks:** ADTs Phase 2, nullable types, `try` return type
  precision, dual-dispatch builtin signatures.
- **Key files:** `src/types.rs`, `src/typecheck.rs` (`resolve_annotation`).

**B2. Gradual Typing Phase 2 --- Split `Any`**

Replace `Type::Any` with `Type::Unknown` (gradual) + `Type::Top`
(true supertype). Add `is_consistent()` alongside `is_subtype()`:

```rust
// Before                    // After
Type::Any                    Type::Unknown  // ? --- consistency, not subtyping
                             Type::Top      // top of subtype lattice
```

- **Scope:** Audit every use of `Type::Any` (~40 sites in types.rs and
  typecheck.rs) and reclassify. Unannotated params become `Unknown`.
  TypeAssert upper bound becomes `Top`. New `is_consistent()` function
  (~30 lines). Replace `is_subtype(_, Any)` calls with `is_consistent`.
- **Formal model:** Siek & Taha (2006) consistency relation. Garcia
  et al. (2016) AGT for systematic derivation. Key property: consistency
  is NOT transitive --- `Int ~ Unknown` and `Unknown ~ Str` but
  `Int ~ Str` does not hold. This prevents the lattice collapse that
  `Any`-as-top-and-bottom currently causes.
- **Risk:** High. This is the single highest-risk change in the cluster.
  Every `Type::Any` use must be individually classified. Incorrect
  reclassification breaks programs. Mitigated by: (a) the migration is
  mechanical once the classification is documented, (b) the corpus test
  suite catches regressions.
- **Unlocks:** Sound union subtyping (union types need a proper lattice),
  type classes Phase 2 (constraints need `Unknown` vs `Top` distinction),
  blame tracking.
- **Depends on:** Can be done before or after B1, but is *required* for
  sound union semantics.
- **Key files:** `src/types.rs` (every `Type::Any` match arm),
  `src/typecheck.rs` (every `Any` default).

**B3. Parameterized Type Aliases Phase 2 --- Application**

`[type [a] body]` with fresh instantiation per use site:

```tinct
Pair: [type [a] [first: a  second: a]]
pair-of-ints: [fn@[Pair Int] [] [first: 1  second: 2]]
```

- **Scope:** Parser: detect `[type [lowercase-words] body]` as
  parameterized. Type checker: store `TypeAlias { params, body }`,
  resolve `[AliasName Arg1 ...]` by building substitution and applying.
  Arity checking.
- **Formal model:** Type synonyms (Haskell Report Section 4.2.2). Textual
  expansion with substitution, no quantification. Aliases expand before
  unification --- principal types trivially preserved (Pierce 2002,
  Chapter 11).
- **Risk:** Low. Fully backward compatible (zero-parameter aliases
  unchanged). Alias expansion happens before inference.
- **Unlocks:** Recursive ADTs (Phase 4), higher-kinded type variables
  (type classes Phase 3), arity-checked type constructors.
- **Key files:** `src/parser.rs`, `src/typecheck.rs`.

**B4. Type Classes Phase 2 --- Constrained Type Variables**

Elm-style fixed constraints on overloaded builtins:

```
= : Eq a => a -> a -> Bool
+ : Num a => a -> a -> a
map : Functor f => (a -> b) -> f a -> f b
```

- **Scope:** `Vec<Constraint>` in `TypeScheme`. Constraint generation
  during inference. Constraint checking during instantiation. Fixed
  instance sets (hardcoded, not user-extensible).
- **Formal model:** Wadler & Blott (1989) type classes, restricted to
  Elm's approach (no class declarations, no dictionary passing). Jones
  (1995) qualified types for constraint propagation through
  let-generalization.
- **Risk:** Major. Adds a new dimension (constraint tracking) to the
  inference engine. Every call to an overloaded builtin generates
  constraints. Error messages need constraint provenance.
- **Unlocks:** Static rejection of invalid operations (`[= fn1 fn2]`
  rejected), precise dual-dispatch typing, foundation for full type
  classes.
- **Depends on:** Gradual typing Phase 2 (B2) --- constraints interact
  with `Unknown` semantics. Let-generalization must be complete
  (constraints propagate through type schemes).
- **Key files:** `src/types.rs` (`TypeScheme`), `src/typecheck.rs`
  (inference engine), `src/builtins.rs` (builtin signatures).

### Phase C: Algebraic Types (ADTs, variants, pattern matching)

These build the user-facing type declaration and consumption story.
They require Phase B's type primitives.

**C1. ADTs Phase 2 --- Multi-Entry `[type ...]` Declarations**

Named structural union types via multi-entry `[type ...]` — no new keyword,
no new parser rule. Two-line type checker extension:

```tinct
Result: [type [ok: a] [err: Str]]
Status: [type "ok" "err" "pending"]
Event: [type
    [click: [x: Int  y: Int]]
    [key:   [code: Str]]
    "resize"]
```

- **Scope:** Type checker extension only — multi-entry `[type ...]` body expands to
  `Type::Union(vec![...])`, registered as type alias. `Expr::Str` in type position → `Type::StringLiteral`. `[@Result expr]` enforces variant membership.
- **Formal model:** Structural discrimination by key set --- parallels
  TypeScript's discriminated unions and OCaml's polymorphic variants
  (Garrigue 1998). No new `Value` variant needed --- ADTs are dicts
  at runtime.
- **Risk:** Low (given B1 is done). The `Type::Union` infrastructure
  from B1 does the heavy lifting. This phase is primarily parser and
  alias registration work.
- **Unlocks:** `try` result typing, user-defined sum types, nominal
  variants, exhaustiveness checking.
- **Depends on:** Union types Phase 2 (B1).
- **Key files:** `src/parser.rs`, `src/typecheck.rs`.

**C2. Nominal Variants Phase 1 --- Unit Constructors**

`Value::Variant` for enum-like values:

```tinct
Color: [type Red Green Blue]
selected: Red                        # Value::Variant { tag: "Red" }
name: [tag-of selected]             # "Red"
```

- **Scope:** New `Value::Variant { tag, payload: None }` variant. New
  `tag-of` builtin. Parser: uppercase bare words in union declarations
  become nominal constructors. Serialization: `{"Red": null}`.
- **Risk:** Moderate. `Value` gains a variant --- every exhaustive
  `match` on `Value` must handle it. `type-of` returns `"Variant"`.
- **Unlocks:** Nominal payload constructors, first-class constructor
  functions, mandatory elimination.
- **Depends on:** ADTs Phase 1 (convention) --- minimal, effectively
  no dependency.

**C3. Nominal Variants Phase 2 --- Payload Constructors**

Full nominal variant system with pattern matching:

```tinct
Option: [type [Some a] None]

found: [match [lookup config "timeout"]
    [Some v]:  v
    None      30]
```

- **Scope:** `Value::Variant { tag, payload: Some(_) }`. Constructor
  registration in type environment. `Pattern::Constructor` in match.
- **Formal model:** Standard labeled injection into a sum type (Pierce
  2002, Chapter 11). `Ok` as `inl : a -> a + b`.
- **Risk:** Moderate. New evaluation path for constructor application.
  New pattern type for constructor matching.
- **Depends on:** C2 (unit constructors) + Pattern matching Phase 2
  (A2).
- **Key files:** `src/value.rs`, `src/eval.rs`, `src/parser.rs`,
  `src/typecheck.rs`.

**C4. Pattern Matching Phase 4 --- Guards and Or-Patterns**

```tinct
[match x
    n@[is: [> _ 0]]:   "positive"
    n@[is: [< _ 0]]:   "negative"
    _:                  "zero"]

[match result
    [ok: v] | [success: v]   v
    [err: msg]               [error msg]]
```

- **Scope:** `MatchArm.guard` field. `Pattern::Or` variant with
  variable-binding consistency check (both branches must bind the same
  set of variables).
- **Risk:** Low. Additive extensions to existing match infrastructure.
- **Depends on:** Pattern matching Phase 3 (A3).

**C5. Exhaustiveness Checking --- ADTs Phase 3 + Pattern Matching Phase 5**

```tinct
res: [@Result [try risky]]

[match res
    [ok: v]  v]
# Warning: non-exhaustive match on Result --- missing [err: Str]
```

- **Scope:** Variant coverage analysis in `[match]` when scrutinee has
  a declared union type. Wildcard covers all remaining variants.
  Unreachable arm warnings.
- **Formal model:** Maranget (2008) for exhaustiveness and redundancy
  analysis via pattern matrices. Karachalias et al. (2015) for guard
  opacity — guards (`is:` predicates) do not contribute to coverage
  analysis — and as the theoretical ceiling for GADT-aware exhaustiveness.
- **Risk:** Moderate. Requires tracking the variant set from the
  scrutinee type, then checking coverage against the arm patterns.
- **Depends on:** Union types Phase 2 (B1) + ADTs Phase 2 (C1) +
  Pattern matching Phase 3 (A3). Nominal variant exhaustiveness
  additionally depends on C3.

### Phase D: Advanced Typing (long-term features)

These are heavyweight type system extensions with significant research
and implementation cost. Each is gated on adoption feedback.

**D1. Type Classes Phase 3 --- Full Haskell-Style**

Class declarations, instance declarations, superclass hierarchy,
dictionary passing at runtime:

```tinct
# Future syntax (illustrative)
[class [Eq a]
    eq: [fn [a a] Bool]]

[instance [Eq Int]
    eq: [fn [x y] [= x y]]]
```

- **Scope:** Fundamental. Class/instance declaration parsing. Dictionary
  construction and implicit threading. Runtime calling convention change
  for overloaded functions.
- **Formal model:** Wadler & Blott (1989), Hall et al. (1996). Jones
  (1993) for constructor classes (higher-kinded `Functor`).
- **Risk:** Very high. Dictionary passing changes the evaluation model.
  Higher-kinded types (`Functor f`) require extending the kind system.
  Row-level constraints (Gaster & Jones 1996) add further complexity.
- **Depends on:** Type classes Phase 2 (B4). Parameterized type aliases
  Phase 2 (B3) for higher-kinded types. Let-generalization complete.
- **Decision gate:** Only proceed if user-defined types need to
  participate in equality, comparison, or mapping protocols (see
  Decision Gate 2).

**D2. Union Types Phase 3 --- Full Algebraic Subtyping**

Replace Robinson unification + `[U-SUBSUME]` with Simple-sub (Parreaux
2020) constraint solving:

```tinct
# Inferred union types --- no annotation needed
result: [if cond [ok: v] [err: msg]]
# Inferred type: [ok: T] | [err: Str]
```

- **Scope:** Fundamental. `unify()` becomes `constrain(t1 <: t2)`.
  Type variables carry `TypeVarBounds { lower, upper }`. Union and
  intersection types emerge from bound compaction. `[U-SUBSUME]`
  eliminated.
- **Formal model:** Dolan & Mycroft (2017), Parreaux (2020). Marques
  et al. (2024) for row variable extension. Principal types proven
  (Dolan 2016, Theorem 4.1).
- **Risk:** Very high. Every `unify()` call site changes. Every `match`
  on `Type` gains `Intersection` variant. Error messages must use
  constraint provenance chains instead of point-of-failure reporting.
- **Depends on:** Gradual typing Phase 2 (B2) --- `Any` split required
  for lattice soundness. Union types Phase 2 (B1). Row polymorphism
  stable.
- **Decision gate:** Only proceed if annotation-only unions prove
  insufficient --- specifically when `if` return types and other
  inferred positions need unions (see Decision Gate 3).

**D3. Recursive ADTs**

```tinct
Tree: [type Leaf [node: a  left: [Tree a]  right: [Tree a]]]
```

- **Scope:** Equi-recursive type unfolding with depth guard. Amadio &
  Cardelli (1993) prove decidability with a depth bound --- tinct's
  existing `MAX_APPLY_DEPTH` extends to alias expansion.
- **Depends on:** Parameterized type aliases Phase 2 (B3) + ADTs
  Phase 2 (C1).
- **Decision gate:** Only proceed when a concrete use case for recursive
  types in tinct code appears (most likely in stdlib, not user config).

**D4. Gradual Typing Phase 3 --- Blame Tracking**

Full blame provenance for typed/untyped boundaries:

```
type mismatch at line 12: add expected Int for first argument
  blame: value from line 7 (from-json result, Unknown type)
  untyped boundary at line 7 is responsible
```

- **Scope:** `BlameLabel` struct. `ThunkState::Guarded` extended with
  blame metadata. Co-natural blame strategy (Greenman et al. 2019) for
  O(1) space per thunk. Phase 3b: automatic guard insertion at all
  `Unknown -> Concrete` boundaries.
- **Formal model:** Wadler & Findler (2009) blame calculus. Findler &
  Felleisen (2002) proxy contracts (already implemented as TypeAssert).
- **Depends on:** Gradual typing Phase 2 (B2).

**D5. Numeric Types**

Range-constrained numerics with Decimal and BigInt:

```tinct
Port: [type Int@[min: 0  max: 65535]]
Price: [type Decimal@[precision: 2]]
```

- **Scope:** Phase 1 (range annotations) connects to structural
  contracts. Phase 2 (Decimal) is independent. Phase 3 (auto-sizing)
  and Phase 4 (BigInt) are performance/capability extensions.
- **Risk:** Phase 1 is low (runtime validation only). Phases 2-4 change
  `Value::Int` and `Value::Float` representation --- major impact on
  every numeric builtin.
- **Depends on:** Structural contracts Phase 1 (for the `@[min/max]`
  annotation system). Numeric types Phase 1 is independent for pure
  validation.

---

## 4. Sprint Plan

### Phase A: Foundations

#### A1. `let-binding`

- **Sprint slug:** `let-binding`
- **Estimated tasks:** 4
  1. Parser: `fn` body accepts expression sequence until closing `]`
  2. Desugar: multi-expression body to nested `Expr::Sequential`
  3. Type checker: if desugared, no change; verify `infer_sequential`
     handles nested case
  4. Tests: 6+ corpus tests (single-body unchanged, multi-body with
     intermediate bindings, shadowing, lazy intermediate thunks)
- **Dependencies:** None
- **Key files:** `src/parser.rs` (fn body parsing), `src/ast.rs`
  (if `Expr::Fn` body changes), tests
- **Spec chapters:** `doc/04-functions.md` (§Let Binding — multi-expression fn bodies, sequential scoping)
- **Unlocks:** Pattern matching Phase 3 (multi-expression arm bodies)

#### A2. `pattern-matching-basic`

- **Sprint slug:** `pattern-matching-basic`
- **Estimated tasks:** 8
  1. `match` added to keyword denylist
  2. `Expr::Match`, `MatchArm`, `Pattern`, `LiteralPattern` AST types
  3. Parser: `[match scrutinee arm1 arm2 ...]` parsing
  4. Pattern parsing mode (bare names as bindings, capitalized words as
     type tags)
  5. Evaluator: materialize scrutinee, try arms top-to-bottom
  6. Type checker: typed as `Any` initially
  7. Formatter: round-trip `Expr::Match`
  8. Tests: 10+ corpus tests (type patterns, literal patterns, wildcard,
     variable binding, nested match, no-match error)
- **Dependencies:** Type predicates (DONE)
- **Key files:** `src/parser.rs`, `src/ast.rs`, `src/eval.rs`,
  `src/typecheck.rs`, `src/formatter.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Match Expression — syntax, arm forms, pin operator), `doc/08-evaluation.md` (§Pattern Matching — arm testing, scrutinee materialization)
- **Unlocks:** Pattern matching Phase 3, basic type dispatch

#### A3. `pattern-matching-destructure`

- **Sprint slug:** `pattern-matching-destructure`
- **Estimated tasks:** 7
  1. `Pattern::Dict` with field patterns and `rest` flag
  2. `Pattern::Seq` with head/tail patterns
  3. Nested pattern support (patterns inside patterns)
  4. Evaluator: recursive pattern matching with environment extension
  5. Lazy dict matching (only matched keys forced)
  6. Seq matching (force head, bind tail thunk)
  7. Tests: 10+ corpus tests (dict destructure, nested destructure,
     seq head/tail, open vs closed dict matching, lazy field access,
     `try` result handling)
- **Dependencies:** A2 (`pattern-matching-basic`), A1 (`let-binding`)
  for multi-expression arm bodies
- **Key files:** `src/parser.rs`, `src/eval.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Structural Patterns — dict/seq/nested patterns, path-key desugar), `doc/08-evaluation.md` (§Structural Pattern Matching — lazy dict forcing, recursive binding)
- **Unlocks:** Self-hosting dual-dispatch builtins, `try` result
  ergonomics, ADT consumption

### Phase B: Type System Primitives

#### B1. `union-types`

- **Sprint slug:** `union-types`
- **Estimated tasks:** 8
  1. `Type::Union(Vec<Type>)` variant
  2. `normalize_union()` --- sort, dedup, flatten nested unions
  3. `is_subtype` gains `[UNION-INJ-L]`, `[UNION-INJ-R]`,
     `[UNION-ELIM]`
  4. `apply_substitution` handles `Union`
  5. `occurs_in` handles `Union`
  6. `collect_type_vars` handles `Union`
  7. Annotation resolver: collect positional entries from
     `Annotation::PropertyDict` into `type:` value as list; resolve
     `type: [T1 T2]` as `Union(normalize(T1), normalize(T2))` in
     `resolve_annotation` (`src/typecheck.rs`) — no parser change needed
  8. Tests: 10+ (union creation, subtyping injection/elimination,
     union in function signatures, union in TypeAssert, union display,
     duplicate elimination, nested union flattening, positional desugar
     `x@[Int Null]`)
- **Dependencies:** None (can be done in parallel with Phase A)
- **Key files:** `src/types.rs`, `src/typecheck.rs`
- **Spec chapters:** `doc/05-type-annotations.md` (§Union Types — `@[T1 T2]` positional syntax, desugar rule, `type: [T1 T2]` resolution), `doc/06-type-inference.md` (§Union Subtyping — `[UNION-INJ-L]`, `[UNION-INJ-R]`, `[UNION-ELIM]` rules), `doc/17-references.md` (Pierce 2002 Ch.15 already present)
- **Unlocks:** ADTs Phase 2, nullable types, dual-dispatch signatures

#### B2. `gradual-typing-split`

- **Sprint slug:** `gradual-typing-split`
- **Estimated tasks:** 10
  1. Document: catalog all `Type::Any` uses with reclassification
  2. `Type::Unknown` replaces `Type::Any` (gradual)
  3. `Type::Top` added (true supertype)
  4. `is_consistent()` function (~30 lines)
  5. `is_subtype`: remove `[S-ANY-TOP]` and `[S-ANY-BOT]`; add
     `tau <: Top`
  6. Audit and update every `match` on `Type::Any` in `types.rs`
  7. Audit and update every `Any` default in `typecheck.rs`
  8. Update `unify()` to use consistency for `Unknown`
  9. Update doc/06-type-inference.md
  10. Tests: full corpus regression + 8 targeted tests (Unknown
      consistency, Top subtyping, non-transitivity of consistency,
      TypeAssert with Top)
- **Dependencies:** None technically, but recommended after B1 so that
  union subtyping can immediately use the proper lattice
- **Key files:** `src/types.rs` (every `Type::Any` arm), `src/typecheck.rs`
- **Spec chapters:** `doc/06-type-inference.md` (§Gradual Typing — `Unknown` vs `Top`, consistency relation), `doc/07-type-extensions.md` (§Gradual Typing extension roadmap), `doc/17-references.md` (Siek & Taha 2006, Garcia et al. 2016)
- **Unlocks:** Sound union subtyping, type classes Phase 2, blame
  tracking

#### B3. `param-type-aliases`

- **Sprint slug:** `param-type-aliases`
- **Estimated tasks:** 6
  1. `TypeAlias { params: Vec<String>, body: Type }` in type checker
  2. Parser: detect `[type [lowercase-words] body]` as parameterized
  3. Type checker: resolve `[AliasName Arg1 ...]` with arity check
  4. Substitution: build `{param -> arg}` and apply to body
  5. Error: arity mismatch error with expected/actual counts
  6. Tests: 6+ (parameterized alias, instantiation, arity error,
     row variable in alias body, backward compat for zero-param)
- **Dependencies:** None
- **Key files:** `src/parser.rs`, `src/typecheck.rs`
- **Spec chapters:** `doc/05-type-annotations.md` (§Parameterized Type Aliases — `[type [a] body]` syntax, instantiation semantics)
- **Unlocks:** Recursive ADTs, higher-kinded types (type classes Phase 3)

#### B4. `type-classes-constrained`

- **Sprint slug:** `type-classes-constrained`
- **Estimated tasks:** 10
  1. `Constraint` type: `Class(String, String)` pairs
  2. `TypeScheme.constraints: Vec<Constraint>` field
  3. Fixed instance sets: `Eq`, `Ord`, `Num`, `Show`, `Functor`,
     `Foldable`, `Filterable`
  4. Constraint generation during inference for overloaded builtins
  5. Constraint checking during instantiation
  6. Builtin signatures updated with constrained type schemes
  7. Display: `Eq a => Fn(a, a -> Bool)` format
  8. Error messages: "type X does not satisfy constraint Y"
  9. doc/06-type-inference.md: constrained type variables section
  10. Tests: 10+ (constraint generation, satisfaction, violation errors,
      overloaded builtins, let-generalization with constraints)
- **Dependencies:** Gradual typing Phase 2 (B2), let-generalization
  complete
- **Key files:** `src/types.rs`, `src/typecheck.rs`, `src/builtins.rs`
- **Spec chapters:** `doc/06-type-inference.md` (§Constrained Type Variables — `Eq a =>`, fixed instance sets), `doc/07-type-extensions.md` (§Dual-Dispatch Builtins — update with constrained signatures), `doc/17-references.md` (Wadler & Blott 1989, Jones 1995)
- **Unlocks:** Static rejection of invalid operations, precise builtin
  typing

### Phase C: Algebraic Types

#### C1. `adts`

- **Sprint slug:** `adts`
- **Estimated tasks:** 5
  1. Type checker: multi-entry `[type ...]` body → `Type::Union(vec![...])`
  2. Type checker: `Expr::Str` in type-expression position → `Type::StringLiteral(s)`
  3. Type alias registration for named union types
  4. `try` return type updated to `Union([ok: a], [err: Str])`
  6. Tests: 8+ (union declaration, tag-only variants, mixed variants,
     TypeAssert enforcement, `try` result type, type alias usage)
- **Dependencies:** Union types Phase 2 (B1)
- **Key files:** `src/typecheck.rs`
- **Spec chapters:** `doc/05-type-annotations.md` (§Union Declarations — multi-entry `[type ...]` syntax, string literal type variants), `doc/03-data-model.md` (§Algebraic Data Types — structural discrimination, runtime representation)
- **Unlocks:** Nominal variants, exhaustiveness checking

#### C2. `nominal-variants-unit`

- **Sprint slug:** `nominal-variants-unit`
- **Estimated tasks:** 6
  1. `Value::Variant { tag: String, payload: Option<Rc<Thunk>> }`
  2. `type-of` returns `"Variant"` for nominal values
  3. `tag-of` builtin: `Variant -> Str`
  4. Parser: uppercase bare words in `[type ...]` multi-entry position as nominal
     constructors
  5. Serialization: `Value::Variant` to JSON as `{"Tag": null}`
  6. Tests: 6+ (unit constructor creation, tag-of, serialization,
     type-of, equality)
- **Dependencies:** ADTs Phase 1 (convention --- effectively none)
- **Key files:** `src/value.rs`, `src/builtins.rs`, `src/eval.rs`
- **Spec chapters:** `doc/03-data-model.md` (§Nominal Variants — `Value::Variant`, `tag-of`, serialization as `{"Tag": null}`), `doc/05-type-annotations.md` (§Nominal Constructors — uppercase bare words in union declarations)
- **Unlocks:** Payload constructors

#### C3. `nominal-variants-full`

- **Sprint slug:** `nominal-variants-full`
- **Estimated tasks:** 8
  1. Payload constructor registration in type environment
  2. Constructor call evaluation (`[Some 42]` -> `Variant`)
  3. `Pattern::Constructor { tag, binding }` for match
  4. Type: `Type::NominalVariant { tag, payload }`
  5. Subtype rules: NominalVariant vs Union, never vs Record
  6. Constructor type signatures (`Some : a -> Option a`)
  7. Lazy payload semantics (payload as thunk, not forced)
  8. Tests: 10+ (payload construction, pattern matching, constructor
     as value for `map`, lazy payload, mixed nominal/structural union,
     serialization)
- **Dependencies:** C2 + Pattern matching Phase 2 (A2)
- **Key files:** `src/value.rs`, `src/eval.rs`, `src/parser.rs`,
  `src/types.rs`, `src/typecheck.rs`
- **Spec chapters:** `doc/03-data-model.md` (§Nominal Variant Payloads — constructor application, lazy payload, serialization), `doc/08-evaluation.md` (§Constructor Evaluation), `doc/05-type-annotations.md` (§Constructor Types — `Some : a -> Option a`)
- **Unlocks:** `[map Some items]`, mandatory elimination

#### C4. `pattern-matching-guards`

- **Sprint slug:** `pattern-matching-guards`
- **Estimated tasks:** 4
  1. `MatchArm.guard: Option<Box<Spanned<Expr>>>` field
  2. `Pattern::Or(Vec<Spanned<Pattern>>)` variant
  3. Or-pattern variable binding consistency check
  4. Tests: 6+ (guards, or-patterns, mixed guard+or, variable
     binding errors)
- **Dependencies:** Pattern matching Phase 3 (A3)
- **Key files:** `src/parser.rs`, `src/eval.rs`
- **Spec chapters:** `doc/02-syntax.md` (§Pattern Guards — `when` syntax, or-patterns), `doc/08-evaluation.md` (§Guard Evaluation)
- **Unlocks:** More expressive pattern matching

#### C5. `exhaustiveness`

**Exhaustiveness design (decided):** Exhaustiveness checking is performed at
**macro expansion time** when the scrutinee is wrapped in a TypeAssert —
the Dhall model (`merge` requires an explicit union type). No new `Expr::Match`
AST variant is introduced.

```tinct
# Without TypeAssert — dynamically correct, statically unverified
[match res
    [ok: v]:    v
    [err: msg]: [error msg]]

# With TypeAssert — exhaustiveness verified at macro expansion time
[match [@Result res]
    [ok: v]:    v
    [err: msg]: [error msg]]
# Non-exhaustive → macro expansion error
```

When `[defmacro match]` detects a TypeAssert scrutinee `[@Type expr]`, it:
1. Looks up `Type` in the type alias registry (accessible at expansion time)
2. Extracts the `Type::Union` variant set from the registered alias
3. Performs Maranget-style coverage analysis on the arm keys against that variant set
4. Emits an expansion-time error if any variant is uncovered

Without TypeAssert, match expands normally without coverage analysis — same as
today. This makes exhaustiveness opt-in but honest: coverage can only be verified
when the union type is statically declared. Consistent with Karachalias et al.
(2015): opaque guards (`is:` predicates) never contribute to coverage; type-tag
arms (`n@Int:`) do.

The macro expander must have read access to the type alias registry at expansion
time — a new requirement for the expansion infrastructure.

- **Sprint slug:** `exhaustiveness`
- **Estimated tasks:** 7
  1. Expose type alias registry to macro expander at expansion time (`src/typecheck.rs` or a shared registry module)
  2. `[defmacro match]` extension: detect TypeAssert scrutinee `[@Type expr]`; look up `Type` alias; extract `Type::Union` variant set (`stdlib/macros.llt`)
  3. Maranget-style coverage matrix: arm keys vs variant set; wildcard/variable arms cover all remaining variants; or-pattern arms (`Pipe` key) cover both sub-patterns (`stdlib/macros.llt`)
  4. Non-exhaustive error: expansion-time error listing uncovered variants when scrutinee has TypeAssert
  5. Unreachable arm warning: arm after wildcard `_:` is unreachable
  6. Nominal variant coverage: constructor set from `Type::Union` containing `Type::NominalVariant` entries (depends on C3)
  7. Tests: 10+ (complete coverage passes; missing variant fails; wildcard covers; or-pattern coverage; unreachable arm; `is:` guard arms treated as opaque/not covering; nominal exhaustiveness; no TypeAssert → no check)
- **Dependencies:** Union types (B1) + ADTs (C1) + Pattern matching Phase 3 (A3) + type alias registry accessible at expansion time. Nominal exhaustiveness additionally depends on C3.
- **Key files:** `stdlib/macros.llt` (coverage check in `[defmacro match]`), `src/typecheck.rs` (registry exposure)
- **Spec chapters:** `doc/06-type-inference.md` (§Exhaustiveness Checking — variant coverage analysis, non-exhaustive warnings, unreachable arm warnings), `doc/07-type-extensions.md` (§Pattern Matrix — Maranget 2008 reference)
- **Unlocks:** Compiler-verified case coverage

### Phase D: Advanced Typing

#### D1-D5 are individually scoped in the whatif documents.

Sprint slugs and estimated task counts:

| Sprint | Slug | Est. Tasks | Depends On | Spec Chapters |
|--------|------|-----------|------------|---------------|
| Type Classes Phase 3 | `type-classes-full` | 15+ | B4, B3 | `doc/06-type-inference.md` (§Type Classes), `doc/07-type-extensions.md`, `doc/17-references.md` |
| Algebraic Subtyping | `algebraic-subtyping` | 20+ | B1, B2 | `doc/06-type-inference.md` (§Algebraic Subtyping — Simple-sub, constraint solving, `Any` split integration), `doc/17-references.md` |
| Recursive ADTs | `recursive-adts` | 6 | B3, C1 | `doc/05-type-annotations.md` (§Recursive Type Aliases), `doc/07-type-extensions.md` |
| Blame Tracking | `blame-tracking` | 12+ | B2 | `doc/10-errors.md` (§Blame — provenance, typed/untyped boundary), `doc/08-evaluation.md` (§Blame Labels) |
| Numeric Types Phases 1-4 | `numeric-*` | 6 per phase | 9.1 for Phase 1 | `doc/05-type-annotations.md` (§Range Annotations, §Decimal Type), `doc/03-data-model.md` (§Numeric Types) |

---

## 5. Cross-Cutting Concerns

### Type Inference Interactions

Every Phase B change touches the inference algorithm. The interactions
that require special care:

1. **Union types + unification.** `unify` must never produce a union ---
   only `is_subtype` and annotation resolution create unions. If `unify`
   accidentally creates unions (e.g., as a fallback for incompatible
   types), principal types are lost because the resulting substitution
   is no longer most-general in Robinson's sense.

2. **Gradual typing + constraints.** When `Unknown` meets a constrained
   type variable (`Eq a`), the AGT approach says: accept statically
   (some concretization satisfies the constraint), insert runtime check.
   The implementation must handle `is_consistent(Unknown, constrained_a)`
   as a special case.

3. **Union types + row polymorphism.** A union of open records
   `[ok: a ...] | [err: Str ...]` interacts with row unification.
   Marques et al. (2024) address this combination specifically ---
   row variables in union members must track which fields are shared
   across variants. For Phase B1 (annotation-only), this is
   manageable because unions are pre-declared. For Phase D2 (inferred
   unions), this becomes the central complexity.

4. **Let-generalization + constraints.** Constraints on generalized
   variables become part of the type scheme. `TypeScheme { type_vars,
   row_vars, constraints, body }`. Instantiation creates fresh variables
   with the same constraints. This is well-studied (Jones 1995) but
   must be implemented carefully to avoid constraint duplication.

5. **Parameterized aliases + row variables.** An alias body containing
   a row variable (`[type [a] [name: Str ...a]]`) must ensure that `a`
   is substituted with a row-kinded type. Substitution with a non-row
   type (`[Extensible Int]`) produces an ill-formed type. The type
   checker should report this at the alias application site, not deep
   inside unification.

### Runtime Overhead Considerations

- **Phase A:** No runtime overhead. Pattern matching adds one
  `materialize` call per match (same as `type-of`).
- **Phase B1 (unions):** No runtime overhead. Unions are erased at
  runtime.
- **Phase B2 (Any split):** No runtime overhead. `Unknown` and `Top`
  are compile-time concepts.
- **Phase B4 (type classes):** Phase 2 (constrained vars) has no
  runtime overhead --- constraints are checked at type-check time only.
  Phase 3 (dictionary passing) adds one implicit dict argument per
  overloaded call --- moderate overhead, mitigated by specialization.
- **Phase C (ADTs):** Structural ADTs have zero runtime overhead (dicts
  at runtime). Nominal variants add one tag comparison per match arm.
- **Phase D2 (algebraic subtyping):** Potentially significant inference
  time increase. Simple-sub is designed to be practical (~500 lines)
  but constraint solving is inherently more expensive than Robinson
  unification.

### Breaking Changes Risk

| Change | Breaking? | Mitigation |
|--------|-----------|-----------|
| `match` keyword | Yes --- `match` can no longer be a variable name | Low risk: `match` is unlikely as a user-chosen name. Keyword denylist expansion. |
| `union` keyword | Not introduced — `[type T1 T2 ...]` replaces `[union ...]`; no new keyword needed | — |
| `Any` split | Potentially | If any user code relies on `Any`-as-bottom behavior (e.g., `[@Any expr]` passing where a concrete type is expected), the split to `Top` changes semantics. Mitigated by: (a) `Any` usage is rare in user code, (b) `[@Any expr]` becomes `[@Top expr]` with the same behavior. |
| `Type::Union` | No | Additive --- new `Type` variant, existing types unchanged. |
| `Value::Variant` | No | Additive --- new `Value` variant. Existing values unchanged. |

### Test Strategy

Each sprint produces tests at three levels:

1. **Corpus tests** (`tests/*.llt`): end-to-end programs exercising the
   feature in realistic scenarios. Each sprint adds 6-10 corpus tests.

2. **Unit tests** (inline `#[test]` in source): targeted tests for
   specific functions (`normalize_union`, `is_consistent`,
   `pattern_matches`).

3. **Regression tests**: the existing corpus suite runs on every change.
   Any failure indicates a breaking change that must be addressed before
   merging.

For type system changes (Phase B), add **negative tests** --- programs
that should be rejected by the type checker. Verify that:
- Incorrect union membership is caught (`[@Result [x: 1]]`)
- Constraint violations are caught (`[= fn1 fn2]` with `Eq` constraint)
- Non-exhaustive matches produce warnings
- Arity mismatches on parameterized aliases produce errors

For pattern matching (Phase A), add **evaluation tests** --- programs
that exercise pattern matching at runtime, including:
- Match failure (no arm matches) produces `MatchError`
- Lazy forcing (only matched keys/fields are forced)
- Nested pattern binding
- Guard evaluation in pattern-extended environment

---

## 6. Decision Gates

### Gate 1: Type Classes vs Union Types for Dual-Dispatch

**Decision point:** Before implementing B4 (type classes constrained)
vs starting with B1 (union types).

**Question:** Should dual-dispatch builtins (`map`, `filter`) be typed
using type classes (`Functor f => ...`) or union types
(`Dict a | Seq a`)?

**Factors:**

| Factor | Type Classes | Union Types |
|--------|-------------|-------------|
| Precision | Maximally precise (`Functor f` abstracts over container) | Precise enough for two containers |
| Extensibility | User-defined types can implement `Functor` | New containers require updating the union |
| Complexity | Major (constraint tracking, dictionary passing) | Moderate (three subtype rules) |
| Error messages | "Type X does not implement Functor" | "Expected Dict or Seq, got X" |
| Prerequisite cost | Let-generalization + `Any` split | None |
| Prior art | Haskell, PureScript, Elm (constrained vars) | TypeScript, Flow |

**Recommendation:** Implement union types first (B1). They are simpler,
have fewer prerequisites, and provide 90% of the value for tinct's use
case (two container types: Dict and Seq). Type classes can follow later
for the extensibility story.

**When to revisit:** If a third container type is added (e.g., `Set`),
union types become increasingly unwieldy (`Dict a | Seq a | Set a`)
and type classes become the right abstraction.

### Gate 2: Full Type Classes Adoption

**Decision point:** After B4 (constrained vars) is deployed and
adoption patterns are observed.

**Question:** Do users need user-defined type class instances, or are
the fixed built-in constraints sufficient?

**Trigger for proceeding:** A user-defined type needs to participate
in equality, comparison, or mapping protocols. For example: a custom
record type that should be comparable via `deep-eq`, or a custom
container that should support `map`.

**If yes:** Proceed to D1 (full type classes with dictionary passing).
**If no:** The Elm-style constrained vars (B4) are sufficient. Do not
add the complexity of class declarations and dictionary passing.

### Gate 3: Algebraic Subtyping Adoption

**Decision point:** After B1 (annotation-only unions) has been in use
for one or more releases.

**Question:** Are annotation-only unions sufficient, or do inferred
positions (e.g., `if` return types) need unions?

**Trigger for proceeding:** Users frequently annotate `if` branches
or `match` results with union types that the type checker could infer
automatically. The annotation burden is a recurring complaint.

**If yes:** Proceed to D2 (Simple-sub algebraic subtyping). This is a
fundamental rewrite of the constraint solver.
**If no:** Annotation-only unions remain. `[U-SUBSUME]` continues to
handle ground-type compatibility.

### Gate 4: Nominal vs Structural ADTs

**Decision point:** After C1 (structural ADTs) is deployed.

**Question:** Do users need opaque constructors, or is structural
discrimination sufficient?

**Trigger for proceeding:** Two constructors with identical payload
shapes need to coexist (e.g., `Left a` and `Right a` both wrapping
a single value), or users need mandatory elimination (accessing a
variant's payload only through pattern matching).

**If yes:** Proceed to C2/C3 (nominal variants).
**If no:** Structural ADTs cover the use case. Nominal variants add
a `Value` variant and increase implementation surface without
proportional benefit.

### Gate 5: Recursive ADTs

**Decision point:** After B3 (parameterized type aliases) and C1
(ADTs) are both deployed.

**Question:** Do users need recursive type definitions (`Tree a`)?

**Trigger for proceeding:** A concrete use case for recursive types
in tinct code appears --- most likely a stdlib function written in tinct
(tree traversal, expression parser) rather than user configuration.

**If yes:** Proceed to D3 (equi-recursive unfolding).
**If no:** Non-recursive unions (`Result`, `Event`, `Status`) cover
configuration use cases without the complexity of recursive type
checking.

---

## 7. Implementation Calendar

A rough ordering assuming one sprint per week, with parallelism where
dependencies allow:

```
Week 1-2:  A1 (let-binding) + B1 (union-types)         [parallel]
Week 3-4:  A2 (pattern-matching-basic)                  [needs A1 done]
Week 5-6:  A3 (pattern-matching-destructure)            [needs A2]
Week 7-8:  B2 (gradual-typing-split) + B3 (param-aliases) [parallel]
Week 9:    C1 (adts)                                    [needs B1]
Week 10:   C2 (nominal-variants-unit)                   [needs C1]
Week 11-12: B4 (type-classes-constrained)               [needs B2]
Week 13:   C3 (nominal-variants-full)                   [needs C2, A2]
Week 14:   C4 (pattern-matching-guards)                 [needs A3]
Week 15-16: C5 (exhaustiveness)                         [needs B1, C1, A3]

--- DECISION GATES evaluate here ---

Week 17+:  D1-D5 as gated by adoption feedback
```

Structural contracts and numeric types run on their own track and can
be interleaved with any of the above:

```
Any time:  Type classes Ph1 (deep-eq/shallow-eq builtins)
Any time:  Structural contracts Ph1 (%@Type)
Any time:  Numeric types Ph1 (range annotations)
After SC:  Numeric types Ph2+ (Decimal, BigInt)
```

---

## 8. Summary: What Ships When

| Phase | What Ships | What Users Get |
|-------|-----------|----------------|
| **A** (foundations) | `let-binding`, `match` with type/literal/dict/seq patterns | Multi-step functions, `try` result destructuring, type dispatch without string comparison |
| **B** (type primitives) | `Type::Union`, `Any` split, parameterized aliases, constrained vars | Nullable types (`x@[Int Null]`), precise builtin types, `[= fn fn]` rejected statically, generic type aliases |
| **C** (algebraic types) | Multi-entry `[type ...]` ADT declarations, `Value::Variant`, exhaustiveness | Named sum types (`Result`, `Option`), first-class constructors (`[map Some items]`), compiler-verified case coverage |
| **D** (advanced) | Full type classes, algebraic subtyping, recursive ADTs, blame | User-extensible protocols, inferred union types, `Tree a`, actionable type error provenance |

Each phase is independently shippable. Phase A delivers immediate
ergonomic value. Phase B delivers type safety. Phase C delivers
expressiveness. Phase D delivers completeness.

---

## References

Papers cited in the individual whatif documents that are load-bearing
for this plan:

- Amadio, R.M. & Cardelli, L. (1993). Subtyping recursive types. *ACM TOPLAS*, 15(4), 575-631. — Decidability of equi-recursive type equality with depth guard. Foundation for D3 recursive ADTs.
- Ariola, Z.M. & Felleisen, M. (1997). The call-by-need lambda calculus. *J. Functional Programming*, 7(3), 265-301. — `let*` semantics for sequential binding (A1).
- Augustsson, L. (1985). Compiling pattern matching. *FPCA '85*, LNCS 201, pp. 368-381. — Sequential arm testing (A2).
- Dolan, S. & Mycroft, A. (2017). Polymorphism, subtyping, and type inference in MLsub. *POPL '17*, pp. 228-242. — Algebraic subtyping (D2).
- Findler, R.B. & Felleisen, M. (2002). Contracts for higher-order functions. *ICFP '02*, pp. 48-59. — Proxy contracts and blame (D4).
- Garcia, R., Clark, A.M. & Tanter, E. (2016). Abstracting gradual typing. *POPL '16*, pp. 429-442. — AGT framework for `Any` split (B2).
- Garrigue, J. (1998). Programming with polymorphic variants. *ML Workshop '98*. — Structural discrimination (C1).
- Greenman, B., Felleisen, M. & Dimoulas, C. (2019). Complete monitors for gradual types. *ICFP '19*, Article 122. — Co-natural blame (D4).
- Jones, M.P. (1993). A system of constructor classes. *FPCA '93*, pp. 52-61. — Higher-kinded type constructors for `Functor` (D1).
- Jones, M.P. (1995). *Qualified types: Theory and practice.* Cambridge University Press. — Constraint propagation through let-generalization (B4).
- Karachalias, G., Schrijvers, T., Vytiniotis, D. & Peyton Jones, S. (2015). GADTs meet their match. *ICFP '15*, pp. 424-436. — Guards treated as opaque for exhaustiveness (C5).
- Launchbury, J. (1993). A natural semantics for lazy evaluation. *POPL '93*, pp. 144-154. — Sharing preservation for let binding (A1).
- Maranget, L. (2008). Compiling pattern matching to good decision trees. *ML '08*, pp. 35-46. — Exhaustiveness and redundancy analysis (C5).
- Marques, R., Florido, M. & Vasconcelos, P. (2024). Towards algebraic subtyping for extensible records. arXiv:2407.06747. — Row variables under algebraic subtyping (D2).
- Parreaux, L. (2020). The simple essence of algebraic subtyping. *ICFP '20*, Article 124. — Simple-sub algorithm (D2).
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. — Union subtyping rules §15.7 (B1), labeled variants §11 (C1, C3).
- Pierce, B.C. & Turner, D.N. (2000). Local type inference. *ACM TOPLAS*, 22(1), 1-44. — Checking mode for union annotations (B1).
- Robinson, J.A. (1965). A machine-oriented logic based on the resolution principle. *JACM*, 12(1), 23-41. — MGU guarantee preserved by annotation-only unions (B1).
- Siek, J.G. & Taha, W. (2006). Gradual typing for functional languages. *Scheme Workshop*, pp. 81-92. — Consistency relation (B2).
- Tobin-Hochstadt, S. & Felleisen, M. (2010). Logical types for untyped languages. *ICFP '10*, pp. 117-128. — Occurrence typing for arm narrowing (C1).
- Wadler, P. & Blott, S. (1989). How to make ad-hoc polymorphism less ad hoc. *POPL '89*, pp. 60-76. — Type classes (B4, D1).
- Wadler, P. & Findler, R.B. (2009). Well-typed programs can't be blamed. *ESOP '09*, LNCS 5502, pp. 1-16. — Blame theorem (D4).
