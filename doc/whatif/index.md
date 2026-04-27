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
| [Formal Gradual Typing](gradual-typing.md) | Formalize `Any` semantics; split into `AnyGradual` / `AnyPoly`; add consistency relation |
| [Algebraic Subtyping](algebraic-subtypes.md) | Replace `[U-SUBSUME]` + Robinson with Simple-sub (Parreaux 2020) for inferred unions |
| [Structural Contracts](structural-contracts.md) | `$$@Type` pipeline boundary checking + `$validate` schema-as-dict runtime constraints |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [String Interpolation](string-interpolation.md) | `i"Hello $name"` — desugars to `[call $str ...]`; formatter ergonomics |
| [`let` Binding Form](let-binding.md) | Sequential expressions inside `[fn ...]` bodies — no new keywords |
| [Pattern Matching](pattern-matching.md) | `[match $x [Int: ...] [Str: ...]]` — type dispatch + destructuring |
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

Cross-reference of each proposal against open TODO items. The question: is
adopting a whatif faster than working through the TODO items it would replace?

### Adopt Now

These proposals have no gating conditions, accepted designs, and either
eliminate a whole section of TODO or deliver standalone ergonomic value at
low implementation cost.

**[Type Predicates](type-predicates.md)** — `### stdlib-type-predicates` in TODO is
exactly this proposal: `is-int?`, `is-str?`, `is-float?`, `is-bool?`, `is-dict?`,
`is-fn?` as one-liners plus runtime assertion guards. Implementing the whatif as
a coherent sprint (6 Rust predicate builtins + stdlib wrappers) closes the whole
section and provides Phase 1 of pattern matching. No dependencies, no gating.

**[String Interpolation](string-interpolation.md) Phase 1** — No existing TODO items
replaced, but high ergonomic ROI. Phase 1 (`i"..."` → desugar to `$str`) is a
standalone parser + desugar change. Formatter and template code becomes
significantly more readable. No dependencies.

**[`let` Binding Form](let-binding.md)** — No existing TODO items replaced, but
removes structural friction in every multi-step function. The nested-fn workaround
(`[call [fn [x] ...] val]`) is pervasive. No new keywords; extends the existing
sequential scoping model to `[fn ...]` bodies. Also a prerequisite for pattern
matching arm bodies (Phase 3+).

**[Structural Contracts](structural-contracts.md) Phase 1 only** — doc/*.md has
three open questions with no corresponding TODO sprints: shape/contract system,
OpenAPI integration, lazy vs. eager validation. Phase 1 (`$$@Type` pipeline
boundary annotation) answers all three in a principled way without committing to
the full `$validate` schema-as-dict system.

### Already the Plan

**[Arena Patterns](arena-patterns.md)** — This document IS the design for the
`## iterative-eval` TODO sprint. The iterative-eval section references it
throughout as the accepted design. No separate adoption decision; execute
`iterative-eval`.

### Wait for Trigger

These proposals have accepted designs but explicit gating conditions that have
not yet been met.

| Proposal | Gating Condition |
|----------|-----------------|
| [Gradual Typing](gradual-typing.md) | `Any`-as-top-and-bottom causing a real false positive, or union types forcing the split |
| [Type Classes](typeclasses.md) | `Any` typing for dual-dispatch causing false positives, or user-defined types needing protocols |
| [Union Types](union-types.md) | Nullable types or tagged union patterns becoming common in user code |
| [Algebraic Subtyping](algebraic-subtypes.md) | Union types proving insufficient without inferred unions |
| [Narrowing](narrowing.md) | `typeassert-structural-b` complete + let-generalization + bidirectional typing |
| [Macros](macros.md) | A second syntactic desugaring beyond `$_`, or user-requested domain-specific syntax |
| [Quasiquoting](quasiquoting.md) | Macro system adoption |
| [Custom Call Aliases](call-aliases.md) | Macro system adoption |

### Strategic (Not a Sprint)

**[Unified Syntax Reform](new-syntax.md)** — Bare-word references + implied call +
`%`-named pipeline sections would reduce token count ~30–40% across all tinct
code and eliminate the two most common LLM generation errors (`$` and `call`).
But it breaks all existing syntax at every reference and call site. This is a
major coordinated migration, not an incremental sprint.

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have
accepted designs and no blocking conditions — adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [General I/O](io.md) Phase 1 | `$emit` — required for all formatter/templating work |
| [TLS, PKI, and HTTP](tls.md) | mTLS and custom CA for internal-service tinct programs |
| [SQL Data Sources](sql-translation.md) | Lazy DB reads via `$filter`/`$map` predicate pushdown |
| [Numeric Types](numeric-types.md) | Range annotations + Decimal type |
| [Float Dict Keys](float-dict-keys.md) | Decimal keys; gated on Decimal type adoption |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | Fresh type-var instantiation; gated on name collision becoming a real problem |
| [Pattern Matching](pattern-matching.md) | Full match expression; Phase 1 = type predicates (adopt that first) |
| [tinct as a Templating Language](templating.md) | `$emit` + formatters + literate mode; Phase 5 (template-polarity) deferred |
