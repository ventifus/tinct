# Typing Cluster: Implementation Plan

Comprehensive plan for the 12 typing-related whatif proposals and their
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
| 12 | Path-Sensitive Narrowing | `narrowing.md` |

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
- Tests: `fn@Null` annotation test, `[@Null []]` assertion test, `null?`
  returns true for `Null`-typed values

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

**Acceptance ordering:** Accept proposals in dependency order per §2.
Null semantics (0) first, then Phase A proposals, then Phase B, etc.
Proposals within the same phase can be accepted in any order.

**Post-acceptance:** The whatif doc is left as-is (it becomes historical
record of the design rationale). Only the `State:` line is added. No
sections are deleted or rewritten — the whatif is the "why and how we
decided" document; the `doc/*.md` spec chapters become the "what it is"
document.

**Plan updates:** If accepting a proposal reveals scope changes (e.g., a
phase was simplified), update this plan's §3 and §4 to match. The plan
is the coordination document and must stay current.

---

## 1. Why These Features Form a Cluster

These 12 proposals are not independent feature requests. They form a
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

**Group II: Type Representation Extensions** (proposals 4, 5, 6, 7, 8, 11, 12)

Union types, ADTs, nominal variants, type classes, gradual typing,
parameterized type aliases, and path-sensitive narrowing all extend the
`Type` enum, the subtyping relation, or the inference algorithm. They
answer: "what types can tinct describe?" These are the core of the
cluster. They interact heavily --- ADTs require union types, nominal
variants require ADTs, exhaustiveness requires both union types and
pattern matching, type classes require parameterized aliases for
higher-kinded types, gradual typing's `Any` split is a prerequisite
for sound union subtyping, and narrowing makes union types and pattern
matching *useful* by refining variable types inside match arms.

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
| Narrowing Phase 1-2 | Pattern matching Phase 2 (`if` as special form) |
| Narrowing Phase 3 | Type predicates (DONE) + Narrowing Phase 1-2 |

### Which Are Alternatives

Two proposals offer **alternative solutions to the same problem**
(dual-dispatch typing):

- **Type classes** (7): `Mappable f => (a -> b) -> f a -> f b`
- **Union types** (4): `(a -> b) -> (Dict a | Seq a) -> (Dict b | Seq b)`

Both solve the problem. Both can coexist. Decision: union types first
(B1), type classes follow (B4 → D1). See §6 for ordering rationale.

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
                    |    Equatable a => ...)       |  type-classes Ph3      |
                    |                             |  (full Haskell-style)  |
                    |                             |                        |
                    |   param-type-aliases Ph2 ---+--> recursive ADTs Ph4  |
                    |   ([type [a] body])          |    (Tree a = ...)     |
                    |                             |                        |
                    |   structural-contracts -----+--> numeric-types Ph1   |
                    |   (%@Type, validate)         |   (range annotations) |
                    |                             +------------------------+
                    |
                    |       PHASE B5 (narrowing)
                    |   +-----------------------------+
                    |   |                             |
                    |   |  narrowing Ph1-2 (B5a)      |
                    |   |  (if special form,          |
                    |   |   env forking,              |
                    |   |   eq + type-of guards)      |
                    |   |        |                    |
                    |   |  type-predicates (DONE) -+  |
                    |   |        |                 |  |
                    |   |        v                 |  |
                    |   |  narrowing Ph3 (B5b) <---+  |
                    |   |  (int?/str? direct          |
                    |   |   narrowing)                |
                    |   |        |                    |
                    |   +--------+--------------------+
                    |            |
                    |  union-types Ph2 -----> result type = τ₁ | τ₂
                    |                        (without B1: LUB or Any)
                    +-----------------+

  INDEPENDENT TRACKS (no cross-deps within cluster):
    - typeclasses Ph1 (deep-eq/shallow-eq) --- any time
    - structural-contracts Ph1 (%@Type) --- any time
    - numeric-types Ph1 (range annotations) --- any time
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
5. Narrowing Phase 1-2 (if special form, eq + type-of narrowing)
6. Narrowing Phase 3 (type predicate integration)
7. Union types Phase 2
8. Gradual typing Phase 2
9. ADTs Phase 2
10. Parameterized type aliases Phase 2
11. Type classes Phase 2
12. Nominal variants Phase 2
13. Pattern matching Phase 4 (guards, or-patterns)
14. Pattern matching Phase 5 + ADTs Phase 3 (exhaustiveness)
15. Recursive ADTs (Phase 4)
16. Type classes Phase 3
17. Union types Phase 3 (full algebraic subtyping)

Items 1-6 can proceed without any type representation changes. Items
7-11 are the heart of the cluster. Items 12-17 are long-term.

Note: Narrowing Phases 1-3 can ship before union types (B1). Without
B1, the join of branch types uses `Any` or the existing LUB instead of
`τ₁ | τ₂`. Full result type precision comes when B1 lands. False-branch
narrowing (negation types) is out of scope for the typing cluster entirely
— see `doc/whatif/boolean-algebraic-subtyping.md`.

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
= : Equatable a => a -> a -> Bool
+ : Numeric a => a -> a -> a
map : Mappable f => (a -> b) -> f a -> f b
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

**B5. Path-Sensitive Narrowing --- `if` as Type-Level Special Form**

Refine variable types inside conditional branches based on the
condition expression. After `[int? x]`, the true branch knows
`x : Int`:

```tinct
[if [int? x]
    [+ x 1]         # x narrowed to Int
    [str-length x]]  # x still original type (no false-branch narrowing)
```

This is the mechanism that makes union types and pattern matching
useful at the type level. Without narrowing, match arm bodies are
typed against the full union or `Any`. With narrowing, each arm
gets the refined variant type.

- **Scope:** `if` becomes a type-level special form with environment
  forking. Three phases: (1) equality + type-of guards, (2) key
  presence + conjunction, (3) type predicate integration. The
  evaluator is unchanged — narrowing is purely static.
- **Formal model:** Tobin-Hochstadt & Felleisen (2010) occurrence typing.
  Dunfield & Pfenning (2004) datasort refinements for singleton literal
  narrowing. Key property: the condition is forced before both branches
  (selective materialization), so narrowing is sound — the type fact is
  established before either branch is entered.
- **Risk:** Moderate. `if` moves from the generic builtin call path to a
  dedicated `infer_if()` function. Environment cloning doubles allocation
  per conditional. For tinct's target domain (config files with few
  conditionals) this is acceptable.
- **Unlocks:** Precise types in match arm bodies (via the desugared
  `if`/`type-of` chains), reduced annotation burden (implicit narrowing
  replaces explicit `[@Type expr]`), LSP hover precision.
- **Depends on:** Pattern matching Phase 2 (A2) for desugared `if` chains
  to narrow. Type predicates (DONE) for Phase 3 integration. Union types
  (B1) for `τ₁ | τ₂` result type precision (without B1: LUB or `Any`).

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
  become nominal constructors. Unit constructor names registered as
  `Value::Variant` values in the environment (no `Expr::Constructor` ---
  constructors are regular `Expr::VarRef` nodes). Serialization:
  `{"Red": null}`.
- **Risk:** Moderate. `Value` gains a variant --- every exhaustive
  `match` on `Value` must handle it. `type-of` returns `"Variant"`.
- **JSON interop note:** `from-json` does NOT reconstruct nominal
  variants --- JSON `{"Red": null}` becomes structural dict `[Red: null]`.
  Nominality requires explicit construction, not automatic inference
  from shape.
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

- **Scope:** `Value::Variant { tag, payload: Some(_) }`. Payload
  constructor names registered as closures in the environment
  (`fn(x) → Variant { tag, payload: Some(x) }`). Constructor calls
  like `[Some 42]` are regular function application via `Expr::VarRef`
  + `Expr::Call`. `Pattern::Constructor` in match. Constructor type
  signatures registered in type environment.
- **Formal model:** Standard labeled injection into a sum type (Pierce
  2002, Chapter 11). `Ok` as `inl : a -> a + b`.
- **Risk:** Low–Moderate. Constructor registration at `[type]` eval
  time is new, but constructor *application* reuses existing call
  machinery. New pattern type for constructor matching.
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

- **Scope:** Full coverage analysis in `[match]` when scrutinee has
  a declared union type. Handles nested patterns, not just variant
  tags. Wildcard covers all remaining variants. Unreachable arm
  and inaccessible RHS warnings.
- **Formal model:** Maranget (2007) usefulness algorithm for
  exhaustiveness, redundancy, and lazy ⊥-as-constructor extension.
  Maranget (2008) for decision tree compilation. Karachalias et al.
  (2015) for the three-way partition (Covered/Divergent/Uncovered),
  guard opacity — guards (`is:` predicates) do not contribute to
  coverage analysis — and as the theoretical ceiling for GADT-aware
  exhaustiveness.
- **Risk:** Moderate. Coverage algorithm implemented in Rust
  (`src/coverage.rs`) for testability and correctness; exposed to
  the match macro as a builtin.
- **Depends on:** Union types Phase 2 (B1) + ADTs Phase 2 (C1) +
  Pattern matching Phase 3 (A3). Nominal variant exhaustiveness
  additionally depends on C3.

### Phase D: Advanced Typing

Heavyweight type system extensions with significant research and
implementation cost. All ship as part of the typing cluster.

**D1. Type Classes Phase 3 --- Full Haskell-Style**

Class declarations, instance declarations, superclass hierarchy,
dictionary passing at runtime:

```tinct
# Future syntax (illustrative)
[class [Equatable a]
    eq: [fn [a a] Bool]]

[instance [Equatable Int]
    eq: [fn [x y] [= x y]]]
```

- **Scope:** Fundamental. Class/instance declaration parsing. Dictionary
  construction and implicit threading. Runtime calling convention change
  for overloaded functions.
- **Formal model:** Wadler & Blott (1989), Hall et al. (1996). Jones
  (1993) for constructor classes (higher-kinded `Functor`).
- **Risk:** Very high. Dictionary passing changes the evaluation model.
  Higher-kinded types (`Mappable f`) require extending the kind system.
  Row-level constraints (Gaster & Jones 1996) add further complexity.
- **Depends on:** Type classes Phase 2 (B4). Parameterized type aliases
  Phase 2 (B3) for higher-kinded types. Let-generalization complete.
- **Ordering:** Ships after B4 (constrained vars) and B3 (parameterized
  aliases).

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
  (Dolan 2016, Theorem 4.1). **Caution:** Marques et al. (2024)
  soundness is *conjectured, not proven* (§1: "proofs of soundness
  and completeness that we believe will hold, but do not have yet
  done"). Chau & Parreaux (POPL 2026) offer a proven alternative:
  Boolean-Algebraic Subtyping encodes extensible records without row
  variables. BAS evaluation deferred to a separate whatif
  (post-typing-cluster) --- see TODO.md §Language Design Research.
- **Risk:** Very high. Every `unify()` call site changes. Every `match`
  on `Type` gains `Intersection` variant. Error messages must use
  constraint provenance chains instead of point-of-failure reporting.
  Additionally, the row variable extension foundation (Marques et al.)
  lacks formal soundness proofs.
- **Depends on:** Gradual typing Phase 2 (B2) --- `Any` split required
  for lattice soundness. Union types Phase 2 (B1). Row polymorphism
  stable.
- **Ordering:** Ships after B1 (annotation-only unions) and B2 (`Any`
  split).

**D3. Recursive ADTs**

```tinct
Tree: [type Leaf [node: a  left: [Tree a]  right: [Tree a]]]
```

- **Scope:** Equi-recursive type unfolding with depth guard. Amadio &
  Cardelli (1993) prove decidability with a depth bound --- tinct's
  existing `MAX_APPLY_DEPTH` extends to alias expansion.
- **Depends on:** Parameterized type aliases Phase 2 (B3) + ADTs
  Phase 2 (C1).
- **Ordering:** Ships after B3 (parameterized aliases) and C1 (ADTs).

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
Port: [type Int@[is: [between 0 65535]]]
Price: [type Decimal@[precision: 2]]
```

- **Scope:** Phase 1 (range annotations via `is:` predicate) is
  stdlib-only --- no Rust changes. Phase 2 (Decimal) is independent.
  Phase 3 (BigInt) and Phase 4 (storage hints / `repr:`) are
  performance/capability extensions.
- **Risk:** Phase 1 is low (runtime validation only). Phases 2-3 change
  `Value::Int` and `Value::Float` representation --- major impact on
  every numeric builtin. Phase 4 is storage-only.
- **Depends on:** Phase 1 is independent. Structural contracts Phase 1
  provides the `is:` annotation infrastructure that Phase 1 validates
  against, but Phase 1 can ship as pure stdlib range-checking without
  it.

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
- **Dependencies:** B1 (union types) — ships after B1 so union subtyping
  immediately uses the proper lattice
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
  3. Fixed instance sets: `Equatable`, `Comparable`, `Numeric`,
     `Showable`, `Mappable`, `Foldable`, `Filterable`, `Appendable`
  4. Constraint generation during inference for overloaded builtins
  5. Constraint checking during instantiation
  6. Builtin signatures updated with constrained type schemes
  7. Display: `Equatable a => Fn(a, a -> Bool)` format
  8. Error messages: "type X does not satisfy constraint Equatable"
  9. doc/06-type-inference.md: constrained type variables section
  10. Tests: 10+ (constraint generation, satisfaction, violation errors,
      overloaded builtins, let-generalization with constraints)
- **Dependencies:** Gradual typing Phase 2 (B2), let-generalization
  complete
- **Key files:** `src/types.rs`, `src/typecheck.rs`, `src/builtins.rs`
- **Spec chapters:** `doc/06-type-inference.md` (§Constrained Type Variables — `Equatable a =>`, fixed instance sets), `doc/07-type-extensions.md` (§Dual-Dispatch Builtins — update with constrained signatures), `doc/17-references.md` (Wadler & Blott 1989, Jones 1995)
- **Unlocks:** Static rejection of invalid operations, precise builtin
  typing

#### B5a. `narrowing-basic`

- **Sprint slug:** `narrowing-basic`
- **Estimated tasks:** 8
  1. `if` special form: detect `if` calls in `infer_expr` and dispatch
     to dedicated `infer_if(cond, then_expr, else_expr, env, state)`
     instead of generic `check_call` (`src/typecheck.rs`)
  2. `Narrowing` enum: `EqLiteral { var, ty }`, `TypeOf { var, ty }`,
     `HasKey { var, key }` (`src/typecheck.rs`)
  3. `extract_narrowings(cond: &Expr) -> Vec<Narrowing>`: pattern match
     on condition AST shape; recognize `[= x lit]`, `[= [type-of x] "Int"]`,
     `[has? x "key"]` (`src/typecheck.rs`)
  4. Environment forking: clone `env` into `env_true`, apply narrowings;
     `env_false` = clone `env` unmodified (`src/typecheck.rs`)
  5. Branch type join: infer `then_expr` in `env_true`, `else_expr` in
     `env_false`; result type is LUB (without B1) or `Union(τ₁, τ₂)`
     (with B1) (`src/typecheck.rs`)
  6. Conjunction support: `[and cond1 cond2]` applies both narrowings
     to `env_true` (`src/typecheck.rs`)
  7. Update type map with narrowed types for LSP hover precision
     (`src/typecheck.rs`)
  8. Tests: 10+ (equality narrowing, type-of guard, has? key narrowing,
     conjunction, both operand orderings for `=`, no false-branch
     narrowing, nested if chains, narrowing not leaking across branches,
     type map has narrowed type for LSP hover)
- **Dependencies:** Scheduled after A3 (pattern-matching-destructure).
  No hard technical dep, but match arms produce the `if` chains that
  narrowing refines. Result type precision improves when B1 (unions)
  lands.
- **Key files:** `src/typecheck.rs`
- **Spec chapters:** `doc/06-type-inference.md` (§Path-Sensitive Narrowing — `if` as type-level special form, `narrow()` function, environment forking), `doc/17-references.md` (Tobin-Hochstadt & Felleisen 2010, Dunfield & Pfenning 2004)
- **Unlocks:** Narrowing Phase 3 (type predicates), precise match arm
  body types

#### B5b. `narrowing-predicates`

- **Sprint slug:** `narrowing-predicates`
- **Estimated tasks:** 5
  1. Extend `extract_narrowings` to recognize `[int? x]`, `[str? x]`,
     `[dict? x]`, `[bool? x]`, `[float? x]`, `[fn? x]`, `[null? x]`,
     `[seq? x]` as direct narrowing triggers — map each predicate to its
     corresponding `Type` (`src/typecheck.rs`)
  2. Predicate-to-type mapping: `int?` → `Type::Int`, `str?` → `Type::Str`,
     `dict?` → `Type::Record(Row::Open)`, `seq?` → `Type::Seq(Any)`,
     `fn?` → `Type::Any`, `null?` → `Type::Record(Row::Empty)`,
     `float?` → `Type::Float`, `bool?` → `Type::Bool` (`src/typecheck.rs`)
  3. `num?` narrowing: `num?` → `Type::Number` (supertype of Int | Float)
     (`src/typecheck.rs`)
  4. `cond` narrowing: extend narrowing to `cond` — each condition-body
     pair narrows independently (optional — can defer to `if`-only)
     (`src/typecheck.rs`)
  5. Tests: 8+ (each predicate narrows correctly, num? supertype narrowing,
     predicate inside `and`, predicate with variable binding, match
     desugared to if/int?/str? chain has correct arm body types, LSP
     hover shows narrowed type in match arm)
- **Dependencies:** Narrowing Phase 1-2 (B5a). Type predicates (DONE).
- **Key files:** `src/typecheck.rs`
- **Spec chapters:** `doc/06-type-inference.md` (§Type Predicate Narrowing — `int?` as direct narrowing trigger)
- **Unlocks:** Match arm bodies get refined types from the desugared
  `if [int? x] ...` chains. This is the connection point between
  pattern matching and the type system.

### Phase C: Algebraic Types

#### C1. `adts`

- **Sprint slug:** `adts`
- **Estimated tasks:** 6
  1. Type checker: multi-entry `[type ...]` body → `Type::Union(vec![...])`
  2. Type checker: `Expr::Str` in type-expression position → `Type::StringLiteral(s)`
  3. Type alias registration for named union types, stored as `TypeScheme`
     (not bare `Type`) so type variables are properly generalized per call
     site — prevents cross-site variable sharing
  4. `try` return type updated to `Union([ok: a], [err: Str])`
  5. Type alias instantiation: `res@Result` instantiates the `TypeScheme`
     with fresh type variables via existing `instantiate()` mechanism
  6. Tests: 8+ (union declaration, tag-only variants, mixed variants,
     TypeAssert enforcement, `try` result type, type alias usage,
     two call sites don't share type variables)
- **Dependencies:** Union types Phase 2 (B1)
- **Key files:** `src/typecheck.rs`
- **Spec chapters:** `doc/05-type-annotations.md` (§Union Declarations — multi-entry `[type ...]` syntax, string literal type variants), `doc/03-data-model.md` (§Algebraic Data Types — structural discrimination, runtime representation)
- **Unlocks:** Nominal variants, exhaustiveness checking

#### C2. `nominal-variants-unit`

- **Sprint slug:** `nominal-variants-unit`
- **Estimated tasks:** 7
  1. `Value::Variant { tag: String, payload: Option<Rc<Thunk>> }`
  2. `type-of` returns `"Variant"` for nominal values
  3. `tag-of` builtin: `Variant -> Str`
  4. Parser: uppercase bare words in `[type ...]` multi-entry position as nominal
     constructors
  5. Environment registration: unit constructor names bound to
     `Value::Variant { tag, payload: None }` (no `Expr::Constructor` —
     constructors are regular `Expr::VarRef` lookups)
  6. Serialization: `Value::Variant` to JSON as `{"Tag": null}`
  7. Tests: 6+ (unit constructor creation, tag-of, serialization,
     type-of, equality, constructor as VarRef)
- **Dependencies:** ADTs Phase 1 (convention --- effectively none)
- **Key files:** `src/value.rs`, `src/builtins.rs`, `src/eval.rs`
- **Spec chapters:** `doc/03-data-model.md` (§Nominal Variants — `Value::Variant`, `tag-of`, serialization as `{"Tag": null}`), `doc/05-type-annotations.md` (§Nominal Constructors — uppercase bare words in union declarations)
- **Unlocks:** Payload constructors

#### C3. `nominal-variants-full`

- **Sprint slug:** `nominal-variants-full`
- **Estimated tasks:** 8
  1. Payload constructor registration: bind name to closure
     `fn(x) → Variant { tag, payload: Some(x) }` in environment +
     type signature in type environment
  2. Constructor call is regular function application (no special eval
     path — `[Some 42]` is `Expr::Call(Expr::VarRef("Some"), [42])`)
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

**Exhaustiveness design (decided):** Exhaustiveness checking is performed
in the **type checker** when typing `Expr::Match`. The type checker has
access to the scrutinee's inferred type — if it is a `Type::Union`, the
arm patterns are checked for coverage. If the scrutinee has a TypeAssert
(`[@Result res]`), the declared type is used. If the scrutinee's type is
inferred as a union (from narrowing or prior unification), coverage is
checked automatically without annotation.

```tinct
# Inferred type — exhaustiveness checked automatically
result: [try risky]    # inferred type: [ok: a] | [err: Str]
[match result
    [ok: v]:    v
    [err: msg]: [error msg]]    # ✓ all variants covered

# Explicit TypeAssert — same behavior, explicit annotation
[match [@Result res]
    [ok: v]:    v]              # ✗ type error: [err: Str] not covered
```

The type checker's `infer_match()` function:
1. Infers the scrutinee type (or uses the TypeAssert annotation)
2. If the type is a `Type::Union`, extracts the variant set
3. Calls the coverage algorithm (`src/coverage.rs`) with the arm pattern
   matrix and constructor signature
4. Emits a type error if any variant is uncovered

**Coverage algorithm:** Full Maranget (2007) usefulness algorithm over
the complete pattern matrix, including nested sub-patterns — not just
variant-tag-level checking. The algorithm is implemented in Rust
(`src/coverage.rs`) and called directly by the type checker. The
algorithm is stable (20+ years, well-proven) and benefits from Rust
unit testing.

**Lazy extension:** ⊥ is treated as an additional constructor in the
signature (Maranget 2007, §4). Wildcards match ⊥; constructor patterns
do not. This yields the three-way partition (Covered/Divergent/Uncovered)
from Karachalias et al. (2015, §3.1): an arm with Divergent non-empty
but Covered empty has an inaccessible RHS (dead code warning). The lazy
extension adds ~10 lines to the core algorithm.

Without a union-typed scrutinee, no coverage analysis is performed —
the match is dynamically correct but statically unverified. This is
honest: coverage can only be verified when the scrutinee type is known
to be a union. Consistent with Karachalias et al. (2015): opaque guards
(`is:` predicates) never contribute to coverage; type-tag arms
(`n@Int:`) do.

- **Sprint slug:** `exhaustiveness`
- **Estimated tasks:** 9
  1. `Pattern` enum in Rust: `Constructor { tag, sub_patterns }`, `Wildcard`, `Or(Box<Pattern>, Box<Pattern>)` (`src/coverage.rs`)
  2. Extract patterns from `Expr::Match` arm patterns → `Pattern` representation (`src/coverage.rs`)
  3. Core usefulness algorithm: `specialize(c, matrix)`, `default_matrix(matrix)`, `useful(matrix, pattern_vector, sig)` — full Maranget recursive descent over nested patterns (`src/coverage.rs`)
  4. Lazy extension: add ⊥ to constructor signature; wildcards match ⊥, constructors don't; `divergent_useful()` for inaccessible RHS detection (`src/coverage.rs`)
  5. `infer_match()` integration: when scrutinee type is `Type::Union`, extract variant set and call coverage algorithm; emit type error for uncovered variants (`src/typecheck.rs`)
  6. Non-exhaustive error: type error listing uncovered pattern witnesses
  7. Redundancy + inaccessible RHS warnings: flag unreachable arms and divergent-but-inaccessible arms
  8. Nominal variant coverage: constructor set from `Type::Union` containing `Type::NominalVariant` entries (depends on C3)
  9. Tests: 15+ — Rust unit tests for coverage algorithm (complete coverage, missing variant, wildcard, or-pattern, nested pattern exhaustiveness, nested pattern redundancy, inaccessible RHS with lazy ⊥, guard opacity); corpus tests for type checker integration (union scrutinee triggers check, non-union skips, TypeAssert works, error messages show uncovered witnesses)
- **Dependencies:** Union types (B1) + ADTs (C1) + Pattern matching Phase 3 (A3). Nominal exhaustiveness additionally depends on C3.
- **Key files:** `src/coverage.rs` (new — Maranget algorithm), `src/typecheck.rs` (`infer_match()` calls coverage)
- **Spec chapters:** `doc/06-type-inference.md` (§Exhaustiveness Checking — Maranget usefulness algorithm, lazy ⊥ extension, coverage witnesses), `doc/07-type-extensions.md` (§Pattern Matrix — Maranget 2007/2008 reference, Karachalias et al. 2015 three-way partition)
- **Unlocks:** Compiler-verified case coverage, correct nested pattern exhaustiveness, inaccessible RHS warnings

### Phase D: Advanced Typing

Sprint slugs and estimated task counts (detailed task lists in the
respective whatif documents):

| Sprint | Slug | Est. Tasks | Depends On | Spec Chapters |
|--------|------|-----------|------------|---------------|
| Type Classes Phase 3 | `type-classes-full` | 15+ | B4, B3 | `doc/06-type-inference.md` (§Type Classes), `doc/07-type-extensions.md`, `doc/17-references.md` |
| Algebraic Subtyping | `algebraic-subtyping` | 20+ | B1, B2 | `doc/06-type-inference.md` (§Algebraic Subtyping — Simple-sub, constraint solving, `Any` split integration), `doc/17-references.md` |
| Recursive ADTs | `recursive-adts` | 6 | B3, C1 | `doc/05-type-annotations.md` (§Recursive Type Aliases), `doc/07-type-extensions.md` |
| Blame Tracking | `blame-tracking` | 12+ | B2 | `doc/10-errors.md` (§Blame — provenance, typed/untyped boundary), `doc/08-evaluation.md` (§Blame Labels) |
| Structural Contracts Ph1 | `structural-contracts-input` | 4 | None | `doc/05-type-annotations.md` (§Pipeline Input Types) |
| Structural Contracts Ph2 | `structural-contracts-validate` | 6 | SC Ph1 | `doc/11a-builtins.md` (§validate), `doc/05-type-annotations.md` (§Schema Validation) |
| Structural Contracts Ph3 | `structural-contracts-describe` | 4 | SC Ph2 | `doc/16-architecture.md` (§CLI — tinct describe) |
| Structural Contracts Ph4 | `structural-contracts-blame` | 6 | SC Ph3, D4 | `doc/10-errors.md` (§Pipeline Blame) |
| Numeric Types Ph1 (range) | `numeric-range` | 6 | None (stdlib-only) | `doc/05-type-annotations.md` (§Range Annotations) |
| Numeric Types Ph2 (Decimal) | `numeric-decimal` | 6 | Independent | `doc/03-data-model.md` (§Decimal Type) |
| Numeric Types Ph3 (BigInt) | `numeric-bigint` | 6 | Ph2 | `doc/03-data-model.md` (§BigInt) |
| Numeric Types Ph4 (repr:) | `numeric-repr` | 6 | Ph3 | `doc/05-type-annotations.md` (§Storage Hints) |

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
   type variable (`Equatable a`), the AGT approach says: accept statically
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
   **Proof obligation (Jones 1995, §8.3):** when instantiating a scheme
   `∀a. C ⇒ τ`, each fresh variable `a'` carries exactly the
   constraints from `C` that mention `a` (substituted to mention `a'`).
   Constraints must not accumulate from prior unification --- only the
   constraints recorded in the scheme at generalization time participate.
   A bug here would be copying constraints from the current inference
   state (where `a` may have acquired additional constraints after
   generalization) into the fresh variable.

5. **Parameterized aliases + row variables.** An alias body containing
   a row variable (`[type [a] [name: Str ...a]]`) must ensure that `a`
   is substituted with a row-kinded type. Substitution with a non-row
   type (`[Extensible Int]`) produces an ill-formed type. The type
   checker should report this at the alias application site, not deep
   inside unification.

6. **Narrowing + match.** `Expr::Match` is a first-class AST node, so
   the type checker's `infer_match()` narrows the scrutinee type
   directly per-arm — no need to reverse-engineer narrowing from
   desugared `if` chains. Each arm's pattern provides a narrowing
   constraint (e.g., `n@Int` narrows `n : Int`), and the type checker
   applies it to the arm body's environment. This is cleaner and more
   robust than narrowing on desugared `if`/`int?` chains.

7. **Narrowing + union result types.** Without B1 (union types), the
   result type of `[if cond then else]` with narrowed branches is the
   LUB of `τ₁` and `τ₂`, which is often `Any`. With B1, the result is
   `τ₁ | τ₂`, which is precise. Narrowing delivers maximum value when
   B1 is also present, but provides correct (if imprecise) results
   without it.

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
- Constraint violations are caught (`[= fn1 fn2]` with `Equatable` constraint)
- Non-exhaustive matches produce warnings
- Arity mismatches on parameterized aliases produce errors

For pattern matching (Phase A), add **evaluation tests** --- programs
that exercise pattern matching at runtime, including:
- Match failure (no arm matches) produces `MatchError`
- Lazy forcing (only matched keys/fields are forced)
- Nested pattern binding
- Guard evaluation in pattern-extended environment

---

## 6. Ordering Rationale

### Union types before type classes for dual-dispatch

Dual-dispatch builtins (`map`, `filter`) could be typed via type classes
(`Mappable f => ...`) or union types (`Dict a | Seq a`). Decision: **B1
(union types) first**. Union types are simpler, have fewer prerequisites,
and cover tinct's two container types (Dict and Seq). Full type classes
(D1) follow later, providing extensibility for user-defined types.

### Phase ordering summary

All phases ship. The ordering reflects dependency chains, not gates:

- **B1 → B2:** Union types before `Any` split (annotation-only unions
  don't conflict with `Any`-as-top-and-bottom)
- **B4 → D1:** Constrained vars before full type classes (D1 extends B4)
- **B1 + B2 → D2:** Annotation-only unions before algebraic subtyping
  (D2 replaces `unify` with `constrain`, requires `Any` split for
  lattice soundness)
- **B3 + C1 → D3:** Parameterized aliases + ADTs before recursive ADTs
- **B2 → D4:** `Any` split before blame tracking
- **A3 → B5a:** Pattern matching before narrowing (match produces the
  `if` chains that narrowing refines)
- **C1 → C2 → C3:** Structural ADTs → unit constructors → payload
  constructors

---

## 7. Implementation Calendar

A rough ordering assuming one sprint per week, with parallelism where
dependencies allow:

```
Week 1-2:  A1 (let-binding) + B1 (union-types)         [parallel]
Week 3-4:  A2 (pattern-matching-basic)                  [needs A1 done]
Week 5-6:  A3 (pattern-matching-destructure)            [needs A2]
Week 7:    B5a (narrowing-basic)                        [after A3]
Week 8:    B5b (narrowing-predicates)                   [needs B5a]
Week 9-10: B2 (gradual-typing-split) + B3 (param-aliases) [parallel]
Week 11:   C1 (adts)                                    [needs B1]
Week 12:   C2 (nominal-variants-unit)                   [needs C1]
Week 13-14: B4 (type-classes-constrained)               [needs B2]
Week 15:   C3 (nominal-variants-full)                   [needs C2, A2]
Week 16:   C4 (pattern-matching-guards)                 [needs A3]
Week 17-18: C5 (exhaustiveness)                         [needs B1, C1, A3]
Week 19-20: D1 (type-classes-full)                      [needs B4, B3]
Week 21-23: D2 (algebraic-subtyping)                    [needs B1, B2]
Week 24:    D3 (recursive-adts)                         [needs B3, C1]
Week 25-26: D4 (blame-tracking)                         [needs B2]
Week 27-28: D5 (numeric-types phases 1-4)               [needs structural-contracts]
```

Narrowing (B5a-b) is scheduled after A3: match arms desugar to
`if [int? x] ...` chains that narrowing refines. No hard technical
dependency, but the practical value of narrowing depends on match
being available.

Structural contracts and numeric types run on their own track and can
be interleaved with any of the above:

```
Any time:  Type classes Ph1 (deep-eq/shallow-eq builtins)
Any time:  Structural contracts Ph1 (%@Type)
After SC1: Structural contracts Ph2 (validate)
After SC2: Structural contracts Ph3 (tinct describe)
After SC3: Structural contracts Ph4 (pipeline blame) — also needs D4
Any time:  Numeric types Ph1 (range annotations, stdlib-only)
Any time:  Numeric types Ph2 (Decimal, independent)
After N2:  Numeric types Ph3 (BigInt)
After N3:  Numeric types Ph4 (repr: storage hints)
```

---

## 8. Summary: What Ships When

| Phase | What Ships | What Users Get |
|-------|-----------|----------------|
| **A** (foundations) | `let-binding`, `match` with type/literal/dict/seq patterns | Multi-step functions, `try` result destructuring, type dispatch without string comparison |
| **B** (type primitives) | `Type::Union`, `Any` split, parameterized aliases, constrained vars, path-sensitive narrowing | Nullable types (`x@[Int Null]`), precise builtin types, `[= fn fn]` rejected statically, generic type aliases, automatic type refinement in `if`/`match` branches |
| **C** (algebraic types) | Multi-entry `[type ...]` ADT declarations, `Value::Variant`, exhaustiveness | Named sum types (`Result`, `Option`), first-class constructors (`[map Some items]`), compiler-verified case coverage with nested pattern support |
| **D** (advanced) | Full type classes, algebraic subtyping, recursive ADTs, blame, numeric types, structural contracts | User-extensible protocols, inferred union types, `Tree a`, actionable type error provenance, range validation, Decimal/BigInt, schema validation |
| **Independent** | Type classes Ph1, structural contracts Ph1, numeric types Ph1 | `deep-eq`/`shallow-eq`, `%@Type` pipeline annotations, range annotations — ship any time |

All phases ship. Phase A delivers immediate ergonomic value. Phase B
delivers type safety. Phase C delivers expressiveness. Phase D delivers
completeness. Independent items ship on their own timeline.

---

## References

Papers cited in the individual whatif documents that are load-bearing
for this plan:

- Amadio, R.M. & Cardelli, L. (1993). Subtyping recursive types. *ACM TOPLAS*, 15(4), 575-631. — Decidability of equi-recursive type equality with depth guard. Foundation for D3 recursive ADTs.
- Ariola, Z.M. & Felleisen, M. (1997). The call-by-need lambda calculus. *J. Functional Programming*, 7(3), 265-301. — `let*` semantics for sequential binding (A1).
- Augustsson, L. (1985). Compiling pattern matching. *FPCA '85*, LNCS 201, pp. 368-381. — Sequential arm testing (A2).
- Dunfield, J. & Pfenning, F. (2004). Tridirectional typechecking. *POPL '04*, pp. 281-292. — Datasort refinements, singleton literal narrowing (B5).
- Chau, C.Y. & Parreaux, L. (2026). The simple essence of Boolean-algebraic subtyping. *Proc. ACM Program. Lang.*, 10(POPL), pp. 1353-1382. doi:10.1145/3776689. — Proves BAS soundness; encodes extensible records without row variables. Proven alternative to Marques et al. for D2; evaluate via separate whatif.
- Dolan, S. & Mycroft, A. (2017). Polymorphism, subtyping, and type inference in MLsub. *POPL '17*, pp. 228-242. — Algebraic subtyping (D2).
- Findler, R.B. & Felleisen, M. (2002). Contracts for higher-order functions. *ICFP '02*, pp. 48-59. — Proxy contracts and blame (D4).
- Garcia, R., Clark, A.M. & Tanter, E. (2016). Abstracting gradual typing. *POPL '16*, pp. 429-442. — AGT framework for `Any` split (B2).
- Garrigue, J. (1998). Programming with polymorphic variants. *ML Workshop '98*. — Structural discrimination (C1).
- Greenman, B., Felleisen, M. & Dimoulas, C. (2019). Complete monitors for gradual types. *Proc. ACM Program. Lang.* 3, OOPSLA, Article 122. doi:10.1145/3360548. — Co-natural blame (D4).
- Jones, M.P. (1993). A system of constructor classes. *FPCA '93*, pp. 52-61. — Higher-kinded type constructors for `Mappable` (D1).
- Jones, M.P. (1995). *Qualified types: Theory and practice.* Cambridge University Press. — Constraint propagation through let-generalization (B4).
- Karachalias, G., Schrijvers, T., Vytiniotis, D. & Peyton Jones, S. (2015). GADTs meet their match. *ICFP '15*, pp. 424-436. — Guards treated as opaque for exhaustiveness (C5).
- Launchbury, J. (1993). A natural semantics for lazy evaluation. *POPL '93*, pp. 144-154. — Sharing preservation for let binding (A1).
- Maranget, L. (2007). Warnings for pattern matching. *J. Functional Programming*, 17(3), 387-421. — Usefulness algorithm for exhaustiveness and redundancy; lazy extension treating ⊥ as constructor (C5).
- Maranget, L. (2008). Compiling pattern matching to good decision trees. *ML '08*, pp. 35-46. — Decision tree compilation for pattern matching (C5).
- Marques, R., Florido, M. & Vasconcelos, P. (2024). Towards algebraic subtyping for extensible records. arXiv:2407.06747. — Row variables under algebraic subtyping (D2).
- Parreaux, L. (2020). The simple essence of algebraic subtyping. *ICFP '20*, Article 124. — Simple-sub algorithm (D2).
- Pierce, B.C. (2002). *Types and Programming Languages.* MIT Press. — Union subtyping rules §15.7 (B1), labeled variants §11 (C1, C3).
- Pierce, B.C. & Turner, D.N. (2000). Local type inference. *ACM TOPLAS*, 22(1), 1-44. — Checking mode for union annotations (B1).
- Robinson, J.A. (1965). A machine-oriented logic based on the resolution principle. *JACM*, 12(1), 23-41. — MGU guarantee preserved by annotation-only unions (B1).
- Siek, J.G. & Taha, W. (2006). Gradual typing for functional languages. *Scheme Workshop*, pp. 81-92. — Consistency relation (B2).
- Tobin-Hochstadt, S. & Felleisen, M. (2010). Logical types for untyped languages. *ICFP '10*, pp. 117-128. — Occurrence typing: path-sensitive narrowing in conditionals (B5), arm narrowing for pattern matching (C1).
- Wadler, P. & Blott, S. (1989). How to make ad-hoc polymorphism less ad hoc. *POPL '89*, pp. 60-76. — Type classes (B4, D1).
- Wadler, P. & Findler, R.B. (2009). Well-typed programs can't be blamed. *ESOP '09*, LNCS 5502, pp. 1-16. — Blame theorem (D4).
