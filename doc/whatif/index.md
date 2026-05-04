# What If: Index

Design proposals for tinct features not yet in the language. Each document
makes the best case for its feature: "What would it take to do this well?"

---

## Type System

| Proposal | Summary |
|----------|---------|
| [Type Predicates](type-predicates.md) | `int?`, `str?`, `dict?` — one predicate per Value variant |
| [Path-Sensitive Narrowing](narrowing.md) | Refine variable types inside `if` branches from equality/type guards |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | `[type [a] body]` — fresh instantiation per use site, fixing name-collision bugs |
| [Union Types and Algebraic Subtyping](union-types.md) | `Int \| Str` annotations (Phase 2) → Simple-sub inferred unions/intersections (Phase 3) |
| [Type Classes](typeclasses.md) | `Eq a => a → a → Bool` — constrained polymorphism for `=`, `+`, `map` |
| [Formal Gradual Typing](gradual-typing.md) | Formalize `Any` semantics; split into `Unknown` / `Top`; add consistency relation |
| [Structural Contracts](structural-contracts.md) | `%@Type` pipeline boundary checking + `validate` schema-as-dict runtime constraints |

## Data Types

| Proposal | Summary |
|----------|---------|
| [Algebraic Data Types](algebraic-data-types.md) | `[union [ok: a] [err: Str]]` — structural ADTs discriminated by key set; dicts-are-fundamental means no new Value variant |
| [Nominal Variants](nominal-variants.md) | `[union [Some a] None]` — opaque constructor-based variants layered on structural ADTs; `Value::Variant { tag, payload }` |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [String Interpolation](string-interpolation.md) | `i"Hello $name"` — desugars to `[str ...]`; formatter ergonomics |
| [`let` Binding Form](let-binding.md) | Sequential expressions inside `[fn ...]` bodies — no new keywords |
| [Pattern Matching](pattern-matching.md) | `[match x ...]` — type dispatch + structural destructuring; 5-phase adoption |
| [Quasiquoting](quasiquoting.md) | `[quote ...]` / `[unquote ...]` — AST as data; prerequisite for macros |
| [Desugaring as Macros](macros.md) | Procedural AST macros for user-defined syntactic transformations |
| [Custom Call Aliases](call-aliases.md) | `[timed f ...]` — macro-defined call forms; gated on macros |
| [Iterative Parser + AST Formatter](parser-rewrite.md) | Replace pest with iterative parser; `ParseOutput` comment map; AST-based formatter (**Accepted**) |
| [Unified Syntax Reform](new-syntax.md) | Bare-word references + implied call + `%`-named pipeline sections |

## Runtime and Performance

| Proposal | Summary |
|----------|---------|
| [Arena Patterns + Flat Environments](arena-patterns.md) | `Vec<Thunk>` + `ThunkId(u32)` arena; flat `FlatEnv` with de Bruijn slot indices; variable resolution pass replacing O(depth) environment chain walks |
| [String Interning for Dict Keys](string-interning.md) | `Key::String(Spur)` via `string-interner` crate; O(1) comparison; profile-gated |
| [Union-Find for Type Substitution](union-find-substitution.md) | Path-compressed union-find for `Substitution::apply()`; worthwhile only if chain depth ≥4; profile-gated |
| [Numeric Types](numeric-types.md) | Range-constrained numerics; `@[min: 0 max: 65535]` → auto `u16` internally |
| [Float Dict Keys](float-dict-keys.md) | Decimal (exact base-10) keys alongside a `Decimal` type |

## Error Diagnostics

| Proposal | Summary |
|----------|---------|
| [Source Text Availability](source-text-availability.md) | `render_span_snippet(source, span)` helper; caller-pairs-with-source model; REPL and CLI source snippet display |
| [Circular Dependency Error Paths](circular-dep-error-paths.md) | `eval_stack: Vec<(String, Span)>` in EvalState to reconstruct full A→B→A cycle chain in error messages |

## Architecture and Refactoring

| Proposal | Summary |
|----------|---------|
| [eval↔builtins Boundary](eval-builtins-boundary.md) | Extract `src/eval_core.rs` to break circular dependency; gate on concrete need for independent builtin testing |
| [Value Serializer Visitor](value-serializer-visitor.md) | Shared traversal for `value_to_json` + `value_to_display_string`; defer until a third format is needed |

## Formal Verification

| Proposal | Summary |
|----------|---------|
| [Evaluation Semantics Verification](eval-semantics-verification.md) | Part A: `proptest` bisimulation tests (PendingBuiltin ≡ Unevaluated, PendingCall ≡ inline); Part B: confluence proof via determinism argument |

## I/O and Connectivity

| Proposal | Summary |
|----------|---------|
| [General I/O](io.md) | Capability-based I/O: `DirCap`, `NetCap`, `Handle`; `open`, `slurp`, `write`, `lines` |
| [TLS, PKI, and HTTP](lib-tls.md) | mTLS, custom CA bundles, certificate pinning, ALPN, HTTP/2 via `fetch` |
| [SQL Data Sources](lib-sql.md) | `sql-open` returns lazy SQL source; `filter`/`map` push predicates to the DB |

## Standard Library

| Proposal | Summary |
|----------|---------|
| [Supplemental Stdlib Modules](lib-supplemental.md) | Extended strings, math builtins, bitwise primitives, base64/hex encoding — 3-phase plan |
| [Pure-Tinct Regex Engine](lib-regex.md) | Thompson NFA simulation entirely in pure-tinct; depends on lib-supplemental Phases 1 + 3 |

## Language Capability

| Proposal | Summary |
|----------|---------|
| [tinct as a Templating Language](templating.md) | `emit`, multi-file pipelines, literate tinct, template-polarity embedding |

---

## Adoption Analysis

Cross-reference of each proposal against open TODO items and gating conditions.

### Accepted

These proposals have been formally accepted: `State: Accepted` marked, spec integrated, implementation sprints created in TODO.md.

| Proposal | Summary | Accepted | Implemented |
|----------|---------|---------|-------------|
| [Iterative Parser + AST Formatter](parser-rewrite.md) | Replace pest with `Vec<StackFrame>` iterative parser; `ParseOutput` comment map; AST-based formatter rewrite | 2026-04-28 | Complete — `parser-lexer`, `parser-core`, `formatter-ast` |
| [Unified Syntax Reform](new-syntax.md) | Bare-word references + implied call + `%`-named pipeline sections; three-sprint implementation plan | 2026-05-01 | Complete — `new-syntax-docs` through `new-syntax-migrate` |
| [Circular Dependency Error Paths](circular-dep-error-paths.md) | `eval_stack` in EvalState for full A→B→A cycle chain in error display | 2026-05-04 | Phase 1 complete — `error-context` sprint |
| [Source Text Availability](source-text-availability.md) | `render_span_snippet` helper; caller-pairs-with-source; REPL + CLI wiring | 2026-05-04 | Phase 1 partial — REPL/CLI done; LSP snippet display is Phase 3 |
| [Arena Patterns + Flat Environments](arena-patterns.md) | `Vec<Thunk>` + `ThunkId(u32)` arena; `FlatEnv` with de Bruijn slot indices; variable resolution pass | 2026-05-04 | Not started — 5 sprints created: `arena-resolve` through `arena-migrate` |

### Adopt Now

These proposals have no gating conditions and deliver standalone value at low cost.

**[Type Predicates](type-predicates.md)** — `### stdlib-type-predicates` in TODO is exactly this proposal: `int?`, `str?`, `float?`, `bool?`, `dict?`, `fn?` as one-liners plus runtime assertion guards. No dependencies, no gating.

**[String Interpolation](string-interpolation.md) Phase 1** — High ergonomic ROI. Phase 1 (`i"..."` → desugar to `str`) is a standalone parser + desugar change. No dependencies.

**[`let` Binding Form](let-binding.md)** — Removes structural friction in every multi-step function. No new keywords; extends existing sequential scoping model to `[fn ...]` bodies. No dependencies.

**[Supplemental Stdlib Modules](lib-supplemental.md) Phase 1** — Pure-tinct `stdlib/strings.llt`. At most one new Rust builtin. No new crates, no gating. Phases 2 and 3 follow.

**[Pure-Tinct Regex Engine](lib-regex.md) Phase 1** — Thompson NFA in `stdlib/regex.llt`. No Rust builtins, no crates. Requires lib-supplemental Phases 1 + 3.

**[Structural Contracts](structural-contracts.md) Phase 1** — `%@Type` pipeline boundary annotation. Note: `%` is now the pipeline variable in new-syntax (replacing `$$`); Phase 1 syntax needs reconciliation before implementation — likely `%name@Type` or an `@Type` output annotation on named sections.

**[Algebraic Data Types](algebraic-data-types.md) Phase 1** — Convention documentation only. Zero-cost, immediate value.

**[Evaluation Semantics Verification](eval-semantics-verification.md) Phase 1** — Confluence proof sketch to `doc/08-evaluation.md` is done (research + doc complete); core proptest suite (200 lines + `proptest` dev-dep) still pending implementation.

### Wait for Trigger

These proposals have accepted designs but explicit gating conditions not yet met.

| Proposal | Gating Condition |
|----------|-----------------|
| [Gradual Typing](gradual-typing.md) | whatif not yet accepted; `Any`-as-top-and-bottom causing a real false positive, or union types forcing the split. Note: `Type::Any` split (`Unknown`+`Top`) is a standalone sprint independent of this whatif. |
| [Type Classes](typeclasses.md) | Phase 1 (`deep-eq`/`shallow-eq` builtins) ships now; Phase 2 (constrained type vars) after `Type::Any` split |
| [Union Types and Algebraic Subtyping](union-types.md) Phase 2 | Nullable types or tagged union patterns becoming common in user code |
| [Union Types and Algebraic Subtyping](union-types.md) Phase 3 | Annotation-only unions proving insufficient; `if` return types need inferred unions |
| [Algebraic Data Types](algebraic-data-types.md) Phase 2 | `union-types.md` Phase 2 implemented (`Type::Union` exists) |
| [Nominal Variants](nominal-variants.md) | Structural ADTs Phase 2 complete; two constructors with identical payload shapes needed |
| [Narrowing](narrowing.md) | `typeassert-structural-b` + let-generalization + bidirectional typing all complete |
| [Macros](macros.md) | A second syntactic desugaring beyond `_`, or user-requested domain-specific syntax |
| [Quasiquoting](quasiquoting.md) | Macro system adoption |
| [Custom Call Aliases](call-aliases.md) | Macro system adoption |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | Name collision becomes a real type error, or recursive ADTs needed (Phase 4) |
| [Pattern Matching](pattern-matching.md) Phase 2+ | Phase 1 (type predicates) complete |
| [String Interning](string-interning.md) | Profiling confirms `String` allocation/comparison is top-5 hotspot on real workloads |
| [Union-Find for Type Substitution](union-find-substitution.md) | Profiling confirms average TypeVar chain depth ≥4 on real programs |
| [eval↔builtins Boundary](eval-builtins-boundary.md) | Independent builtin testing is a concrete need, OR evaluator refactor where decoupling reduces blast radius |
| [Value Serializer Visitor](value-serializer-visitor.md) | A third output format (YAML, TOML) is implemented and traversal duplication becomes maintenance burden |
| [Evaluation Semantics Verification](eval-semantics-verification.md) Phase 2+ | Phase 1 complete with zero failures; formal semantics in doc/08-evaluation.md |

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have accepted designs; adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [General I/O](io.md) Phase 1 | `emit` — required for all formatter/templating work |
| [TLS, PKI, and HTTP](lib-tls.md) | mTLS and custom CA for internal-service tinct programs |
| [SQL Data Sources](lib-sql.md) | Lazy DB reads via `filter`/`map` predicate pushdown |
| [Numeric Types](numeric-types.md) | Range annotations + Decimal type |
| [Float Dict Keys](float-dict-keys.md) | Decimal keys; gated on Decimal type adoption |
| [Pattern Matching](pattern-matching.md) | Full match expression; Phase 1 = type predicates (adopt that first) |
| [tinct as a Templating Language](templating.md) | `emit` + formatters + literate mode; Phase 5 (template-polarity) deferred |
| [Nominal Variants](nominal-variants.md) Phase 1 | `tag-of` + unit constructors; independently useful as enum-like values |

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
       └──── union-types (Ph 3) ─── gradual-typing ─── algebraic-data-types (Ph 3)

type-classes (Ph 1: deep-eq/shallow-eq) ── type-classes (Ph 2: constrained vars)
                                                    │
                                           Any-split (Unknown + Top) ── gradual-typing

quasiquoting ─── macros ─── call-aliases

io (Ph 1) ─── templating
           └── tls (Ph 2)

string-interpolation ─── new-syntax (accepted; $ as interpolation marker inside i"..." is compatible)

structural-contracts ─── numeric-types (Ph 1)
parameterized-type-aliases ─── algebraic-data-types (Ph 4, recursive ADTs)

arena-patterns (Ph 1: variable-resolution-pass)
    └── arena-patterns (Ph 2: ThunkArena + FlatEnv)
            └── arena-patterns (Ph 3: CEK machine)
                    └── arena-patterns (Ph 4: --- boundary migration)

eval-semantics-verification (Ph 1) ─── eval-semantics-verification (Ph 2+)
```

---

## Conflicts and Alternative Paths

No two proposals are fully mutually exclusive — adopting one never prevents the other. However, several pairs represent alternative paths or create ordering tension.

### Alternative Solutions to the Same Problem

**Dual-dispatch typing: [Type Classes](typeclasses.md) vs [Union Types](union-types.md)**

Both solve the problem of typing `map`, `filter`, and other dual-dispatch builtins. Type classes solve it with `Functor f => (a → b) → f a → f b`; union types solve it with `(a → b) → (Dict a | Seq a) → (Dict b | Seq b)`.

- Adopt **type classes** first if the goal is polymorphic protocols for user-defined types.
- Adopt **union types** first if the goal is nullable types and ADTs (`Int | Null`, `try` result types).

Either path is valid. Both can coexist; for the dual-dispatch problem specifically, one solution is sufficient.

### Supersession

**[Nominal Variants](nominal-variants.md) making [Algebraic Data Types](algebraic-data-types.md) conventions redundant for some use cases**

For use cases where opaque construction matters, nominal variants are strictly more expressive. Structural ADTs remain correct for JSON interop (structural variants round-trip; nominal variants don't reconstruct from `from-json`). These coexist — but for any specific type declaration, the user must choose.

### Runtime Representation Tension

**[Nominal Variants](nominal-variants.md) + JSON serialization**

Nominal `[Ok 42]` → `{"Ok": 42}`. `from-json` always produces structural dicts, losing nominal identity. Do not use nominal variants for data that must survive JSON round-trips; use structural ADTs instead.

### The One-Way Migration Door

**[Unified Syntax Reform](new-syntax.md)** — Accepted 2026-05-01. Implementation is a clean internal cutover (no user code) in three sprints: `new-syntax-a` (% pipeline), `new-syntax-b` (core migration), `new-syntax-c` (polish). After Phase 2 commits, all existing tinct examples and other whatif docs need syntax updates.

### No Conflict (Apparent but Not Real)

**[Algebraic Data Types](algebraic-data-types.md) vs [Nominal Variants](nominal-variants.md) — both use `[union ...]`**

The same `[union ...]` form hosts both structural and nominal declarations, distinguished by case. A single union can mix both. This is intentional composability.

**[Structural Contracts](structural-contracts.md) vs [Type Classes](typeclasses.md) for validation**

Structural contracts are for boundary validation (JSON input, pipeline boundaries); type classes are for type-level protocols. Both can coexist; adopt structural contracts first for the immediate use case.

**[String Interning](string-interning.md) vs [Arena Patterns](arena-patterns.md)**

String interning replaces `Key::String(String)` with `Key::String(Spur)`. Arena patterns replace `Rc<Thunk>` with `ThunkId`. Both are perf migrations that change the representation of different types — they don't conflict and could be done in either order. Arena migration is higher-leverage and already planned; string interning should be profiled before committing.
