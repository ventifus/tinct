# What If: Index

Design proposals for tinct features not yet in the language. Each document
makes the best case for its feature: "What would it take to do this well?"

---

## Type System

| Proposal | Summary |
|----------|---------|
| [Type Predicates](type-predicates.md) | `$int?`, `$str?`, `$dict?` — one predicate per Value variant |
| [Path-Sensitive Narrowing](narrowing.md) | Refine variable types inside `$if` branches from equality/type guards |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | `[type [a] body]` — fresh instantiation per use site, fixing name-collision bugs |
| [Union Types](union-types.md) | `Int \| Str` — annotation-only unions for dual-dispatch builtins and nullable types |
| [Type Classes](typeclasses.md) | `Eq a => a → a → Bool` — constrained polymorphism for `$=`, `$+`, `$map` |
| [Formal Gradual Typing](gradual-typing.md) | Formalize `Any` semantics; split into `Unknown` / `Top`; add consistency relation |
| [Algebraic Subtyping](algebraic-subtypes.md) | Replace `[U-SUBSUME]` + Robinson with Simple-sub (Parreaux 2020) for inferred unions |
| [Structural Contracts](structural-contracts.md) | `$$@Type` pipeline boundary checking + `$validate` schema-as-dict runtime constraints |

## Data Types

| Proposal | Summary |
|----------|---------|
| [Algebraic Data Types](algebraic-data-types.md) | `[union [ok: a] [err: Str]]` — structural ADTs discriminated by key set; dicts-are-fundamental means no new Value variant |
| [Nominal Variants](nominal-variants.md) | `[union [Some a] None]` — opaque constructor-based variants layered on structural ADTs; `Value::Variant { tag, payload }` |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [String Interpolation](string-interpolation.md) | `i"Hello $name"` — desugars to `[call $str ...]`; formatter ergonomics |
| [`let` Binding Form](let-binding.md) | Sequential expressions inside `[fn ...]` bodies — no new keywords |
| [Pattern Matching](pattern-matching.md) | `[match $x ...]` — type dispatch + structural destructuring; 5-phase adoption |
| [Quasiquoting](quasiquoting.md) | `[quote ...]` / `[unquote ...]` — AST as data; prerequisite for macros |
| [Desugaring as Macros](macros.md) | Procedural AST macros for user-defined syntactic transformations |
| [Custom Call Aliases](call-aliases.md) | `[timed $f ...]` — macro-defined call forms; gated on macros |
| [Unified Syntax Reform](new-syntax.md) | Bare-word references + implied call + `%`-named pipeline sections |

## Runtime and Performance

| Proposal | Summary |
|----------|---------|
| [Arena Patterns](arena-patterns.md) | `Vec<Thunk>` + `ThunkId(u32)` to replace `Rc<RefCell<ThunkState>>` |
| [Numeric Types](numeric-types.md) | Range-constrained numerics; `@[min: 0 max: 65535]` → auto `u16` internally |
| [Float Dict Keys](float-dict-keys.md) | Decimal (exact base-10) keys alongside a `Decimal` type |

## I/O and Connectivity

| Proposal | Summary |
|----------|---------|
| [General I/O](io.md) | Capability-based I/O: `DirCap`, `NetCap`, `Handle`; `$open`, `$slurp`, `$write`, `$lines` |
| [TLS, PKI, and HTTP](tls.md) | mTLS, custom CA bundles, certificate pinning, ALPN, HTTP/2 via `$fetch` |
| [SQL Data Sources](sql-translation.md) | `$sql-open` returns lazy SQL source; `$filter`/`$map` push predicates to the DB |

## Language Capability

| Proposal | Summary |
|----------|---------|
| [tinct as a Templating Language](templating.md) | `$emit`, multi-file pipelines, literate tinct, template-polarity embedding |

---

## Adoption Analysis

Cross-reference of each proposal against open TODO items and gating conditions.

### Adopt Now

These proposals have no gating conditions, accepted designs, and either
eliminate a whole section of TODO or deliver standalone ergonomic value at
low implementation cost.

**[Type Predicates](type-predicates.md)** — `### stdlib-type-predicates` in TODO is
exactly this proposal: `$int?`, `$str?`, `$float?`, `$bool?`, `$dict?`, `$fn?` as
one-liners plus runtime assertion guards. Implementing as a coherent sprint closes
the whole section and provides Phase 1 of pattern matching. No dependencies, no gating.

**[String Interpolation](string-interpolation.md) Phase 1** — No existing TODO items
replaced, but high ergonomic ROI. Phase 1 (`i"..."` → desugar to `$str`) is a
standalone parser + desugar change. No dependencies.

**[`let` Binding Form](let-binding.md)** — No existing TODO items replaced, but
removes structural friction in every multi-step function. The nested-fn workaround
(`[call [fn [x] ...] val]`) is pervasive. No new keywords; extends the existing
sequential scoping model to `[fn ...]` bodies.

**[Structural Contracts](structural-contracts.md) Phase 1 only** — Phase 1
(`$$@Type` pipeline boundary annotation) answers open design questions around
shape/contract systems without committing to the full `$validate` schema-as-dict
system.

**[Algebraic Data Types](algebraic-data-types.md) Phase 1** — Convention
documentation only: no code changes, just establishing the structural ADT pattern
for user code and formalising the `$try` result shape. Zero-cost, immediate value.

### Already the Plan

**[Arena Patterns](arena-patterns.md)** — This document IS the design for the
`## iterative-eval` TODO sprint. No separate adoption decision; execute `iterative-eval`.

### Wait for Trigger

These proposals have accepted designs but explicit gating conditions not yet met.

| Proposal | Gating Condition |
|----------|-----------------|
| [Gradual Typing](gradual-typing.md) | `Any`-as-top-and-bottom causing a real false positive, or union types forcing the split |
| [Type Classes](typeclasses.md) | `Any` typing for dual-dispatch causing false positives, or user-defined types needing protocols |
| [Union Types](union-types.md) | Nullable types or tagged union patterns becoming common in user code |
| [Algebraic Data Types](algebraic-data-types.md) Phase 2 | `union-types.md` Phase 2 implemented (`Type::Union` exists) |
| [Nominal Variants](nominal-variants.md) | Structural ADTs Phase 2 complete; two constructors with identical payload shapes needed |
| [Algebraic Subtyping](algebraic-subtypes.md) | Union types proving insufficient without inferred unions |
| [Narrowing](narrowing.md) | `typeassert-structural-b` + let-generalization + bidirectional typing all complete |
| [Macros](macros.md) | A second syntactic desugaring beyond `$_`, or user-requested domain-specific syntax |
| [Quasiquoting](quasiquoting.md) | Macro system adoption |
| [Custom Call Aliases](call-aliases.md) | Macro system adoption |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | Name collision becomes a real type error, or recursive ADTs needed (Phase 4) |
| [Pattern Matching](pattern-matching.md) Phase 2+ | Phase 1 (type predicates) complete |

### Strategic (Not a Sprint)

**[Unified Syntax Reform](new-syntax.md)** — Bare-word references + implied call +
`%`-named pipeline sections would reduce token count ~30–40% across all tinct
code. But it breaks all existing syntax at every reference and call site. This is a
major coordinated migration requiring a dual-mode parser and explicit migration
tooling, not an incremental sprint. Adopt as a deliberate project milestone, not a
feature sprint.

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have
accepted designs; adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [General I/O](io.md) Phase 1 | `$emit` — required for all formatter/templating work |
| [TLS, PKI, and HTTP](tls.md) | mTLS and custom CA for internal-service tinct programs |
| [SQL Data Sources](sql-translation.md) | Lazy DB reads via `$filter`/`$map` predicate pushdown |
| [Numeric Types](numeric-types.md) | Range annotations + Decimal type |
| [Float Dict Keys](float-dict-keys.md) | Decimal keys; gated on Decimal type adoption |
| [Pattern Matching](pattern-matching.md) | Full match expression; Phase 1 = type predicates (adopt that first) |
| [tinct as a Templating Language](templating.md) | `$emit` + formatters + literate mode; Phase 5 (template-polarity) deferred |
| [Nominal Variants](nominal-variants.md) Phase 1 | `$tag-of` + unit constructors; independently useful as enum-like values |

---

## Dependency Graph

Reading order: each row depends on those above it in the same chain.

```
type-predicates ─────────────────────────────────────────── pattern-matching (Ph 1)
                                                                      │
let-binding ──────────────────────────────────────────────── pattern-matching (Ph 2+)
                                                                      │
union-types (Ph 2) ──── algebraic-data-types (Ph 2) ─────── pattern-matching (Ph 3+)
       │                         │                                    │
       │                  nominal-variants (Ph 2)         pattern-matching (Ph 5)
       │                                                              │
       └──── algebraic-subtypes ─── gradual-typing ─── algebraic-data-types (Ph 3)

quasiquoting ─── macros ─── call-aliases

io (Ph 1) ─── templating
           └── tls (Ph 2)

string-interpolation ─── new-syntax (Ph 4 default flip)

structural-contracts ─── numeric-types (Ph 1)
parameterized-type-aliases ─── algebraic-data-types (Ph 4, recursive ADTs)
```

---

## Conflicts and Alternative Paths

No two proposals are fully mutually exclusive in the sense that adopting one
*prevents* the other. However, several pairs represent alternative paths to the
same problem, or create tension that requires careful ordering.

### Alternative Solutions to the Same Problem

**Dual-dispatch typing: [Type Classes](typeclasses.md) vs [Union Types](union-types.md)**

Both solve the problem of typing `$map`, `$filter`, and other dual-dispatch builtins
that accept either Dict or Seq. Type classes solve it with `Functor f => (a → b) → f
a → f b`; union types solve it with `(a → b) → (Dict a | Seq a) → (Dict b | Seq
b)`. These are genuinely alternative approaches for this specific problem:

- Adopt **type classes** first if the goal is polymorphic protocols for user-defined
  types (e.g., making user types participate in `$map`).
- Adopt **union types** first if the goal is nullable types and ADTs (`Int | Null`,
  `$try` result types) — the dual-dispatch typing is a secondary benefit.

Either path is valid. Adopting both is fine: type classes provide Functor-style
abstraction, union types provide sum-type declarations. They are not in conflict, but
for the dual-dispatch problem specifically, one solution is sufficient.

### Supersession: When One Path Makes Another Obsolete

**[Union Types](union-types.md) Phase 2 vs [Algebraic Subtyping](algebraic-subtypes.md)**

Union types Phase 2 adds annotation-only unions (`Type::Union`, checked but not
inferred). Algebraic subtyping makes unions *inferred* — `$if cond [ok: v] [err: m]`
automatically gets type `[ok: T] | [err: Str]` without annotation. If algebraic
subtyping is adopted, annotation-only unions (Phase 2) are no longer the ceiling —
they become a stepping stone that was already traversed. Concretely:

- Phase 2 (annotation-only) is still needed as the foundation that algebraic
  subtyping builds on — `Type::Union` is required by both.
- The supersession is of the *motivation*, not the implementation: if you plan to
  adopt algebraic subtyping, you can treat Phase 2 as Phase 1 of a larger migration
  rather than as an endpoint.

**[Nominal Variants](nominal-variants.md) making [Algebraic Data Types](algebraic-data-types.md) conventions redundant for some use cases**

For use cases where opaque construction matters (constructors enforce invariants,
two variants with identical payload shapes), nominal variants are strictly more
expressive. Structural ADTs remain the right choice when external JSON interop is
the priority (structural variants round-trip transparently; nominal variants don't
reconstruct from `$from-json` automatically). These coexist rather than conflict —
but for any *specific* type declaration, the user must choose structural or nominal.

### Runtime Representation Tension

**[Nominal Variants](nominal-variants.md) + [Algebraic Data Types](algebraic-data-types.md) JSON serialization**

Both serialize similarly to JSON: structural `[ok: 42]` → `{"ok": 42}`, nominal
`[call Ok 42]` → `{"Ok": 42}`. A consumer reading `{"Ok": 42}` from JSON cannot
determine whether it was originally structural or nominal — and `$from-json` always
produces structural dicts. This means: if a value crosses a JSON boundary, it loses
its nominal identity. This is by design (nominality requires explicit construction)
but it is an irrecoverable information loss. Do not use nominal variants for data
that must survive JSON round-trips; use structural ADTs instead.

### The One-Way Migration Door

**[Unified Syntax Reform](new-syntax.md)** is not mutually exclusive with any other
proposal, but once the default flip (Phase 4) is executed, all existing tinct code
and all other whatif example syntax needs to be updated to the new `$`-inverted,
bare-word-reference model. Every other whatif doc uses current syntax (`$name` for
references, `[call $fn arg]` for calls). After the migration, their example syntax
would be written differently (`name` for references, `[fn arg]` implied calls). The
proposals themselves remain valid; only the surface syntax of their examples changes.
Plan the syntax migration as a coordinated rewrite of all whatif docs, not a silent
background change.

### No Conflict (Apparent but Not Real)

**[Algebraic Data Types](algebraic-data-types.md) vs [Nominal Variants](nominal-variants.md) — both use `[union ...]`**

The same `[union ...]` form hosts both structural and nominal declarations,
distinguished by case (lowercase entries = structural, uppercase entries = nominal).
A single union can mix both. This is not a conflict — it is intentional composability.
The distinction is visually clear and semantically sound at the runtime level
(`Value::Dict` vs `Value::Variant`).

**[Structural Contracts](structural-contracts.md) vs [Type Classes](typeclasses.md) for validation**

Structural contracts provide `$validate` dict-as-schema for runtime constraint
checking. Type classes could provide a `Validate` typeclass for user-defined
validation. These address different layers: structural contracts are for boundary
validation (JSON input, pipeline boundaries), type classes are for type-level
protocols. Both can coexist; adopt structural contracts first for the immediate
use case, type classes later for polymorphic protocols.
