# What If: Index

Design proposals for tinct features not yet in the language. Each document
makes the best case for its feature: "What would it take to do this well?"

Completed proposals are archived in [doc/whatif/completed/](completed/).

---

## Type System

| Proposal | Summary |
|----------|---------|
| [Null Semantics](null-semantics.md) | `@Null` annotation = `Type::Record(Row::Empty)`; void-returning builtins typed precisely |
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
| [`let` Binding Form](let-binding.md) | Sequential expressions inside `[fn ...]` bodies — no new keywords |
| [Pattern Matching](pattern-matching.md) | `[match x ...]` — type dispatch + structural destructuring; 5-phase adoption |
| [Macro-Rewrite](macro-rewrite.md) | Replace `src/desugar.rs` with `[defmacro]` definitions; land `let`, `match`, `union`, `i"..."` as macros instead of Rust AST variants |
| [Parse-Stage Macros](parse-stage-macros.md) | Syntax classes with context-sensitive key identity — `[match]` arms use full-annotated-expression equality so `n@Int` and `n@String` coexist as distinct pattern keys |
| [Custom Call Aliases](call-aliases.md) | `[timed f ...]` — macro-defined call forms; gated on macros |

## Runtime and Performance

| Proposal | Summary |
|----------|---------|
| [String Interning for Dict Keys](string-interning.md) | `Key::String(Spur)` via `string-interner` crate; O(1) comparison; profile-gated |
| [Union-Find for Type Substitution](union-find-substitution.md) | Path-compressed union-find for `Substitution::apply()`; worthwhile only if chain depth ≥4; profile-gated |
| [Numeric Types](numeric-types.md) | Range-constrained numerics; `@[min: 0 max: 65535]` → auto `u16` internally |
| [Float Dict Keys](float-dict-keys.md) | Decimal (exact base-10) keys alongside a `Decimal` type |

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
| [TLS, PKI, and HTTP](lib-tls.md) | mTLS, custom CA bundles, certificate pinning, ALPN, HTTP/2 via `fetch` |
| [SQL Data Sources](lib-sql.md) | `sql-open` returns lazy SQL source; `filter`/`map` push predicates to the DB |

## Templating

| Proposal | Summary |
|----------|---------|
| [Template-Polarity Embedding](template-polarity.md) | `tinct template` subcommand — `{{ expr }}` / `{% block %}` Jinja-style preprocessing of foreign-format files (nginx.conf, Dockerfile, Makefile) |

## Standard Library

| Proposal | Summary |
|----------|---------|
| [Supplemental Stdlib Modules](lib-supplemental.md) | Extended strings, math builtins, bitwise primitives, base64/hex encoding — 3-phase plan |
| [Pure-Tinct Regex Engine](lib-regex.md) | Thompson NFA simulation entirely in pure-tinct; depends on lib-supplemental Phases 1 + 3 |


---

## Adoption Analysis

Cross-reference of each proposal against open TODO items and gating conditions.

### Completed

These proposals are fully implemented. Source documents are archived in [doc/whatif/completed/](completed/).

| Proposal | Summary | Completed |
|----------|---------|-----------|
| [Iterative Parser + AST Formatter](completed/parser-rewrite.md) | Replace pest with `Vec<StackFrame>` iterative parser; `ParseOutput` comment map; AST-based formatter rewrite | 2026-05-05 — `parser-lexer`, `parser-core`, `formatter-ast` |
| [Unified Syntax Reform](completed/new-syntax.md) | Bare-word references + implied call + `%`-named pipeline sections | 2026-05-05 — `new-syntax-docs` through `new-syntax-migrate` |
| [Circular Dependency Error Paths](completed/circular-dep-error-paths.md) | `eval_stack` in EvalState for full A→B→A cycle chain in error display | 2026-05-05 — `error-context` sprint |
| [Source Text Availability](completed/source-text-availability.md) | `render_span_snippet` helper; caller-pairs-with-source; REPL + CLI + LSP wiring | 2026-05-05 — all phases including LSP `related_information` |
| [Arena Patterns + Flat Environments](completed/arena-patterns.md) | `Vec<Thunk>` + `ThunkId(u32)` arena; `FlatEnv` with de Bruijn slot indices; variable resolution pass | 2026-05-05 — `arena-resolve`, `arena-types`, `arena-eval`, `arena-cek`, `arena-migrate` |
| [Type Predicates](completed/type-predicates.md) | `int?`, `str?`, `dict?`, `fn?` — one predicate per Value variant | 2026-05-05 — `type-predicates` sprint |
| [General I/O](completed/io.md) | Capability-based I/O: `DirCap`, `NetCap`, `Handle`; `open`, `slurp`, `write`, `lines` | 2026-05-05 — all phases done: `io-phase1` through `io-phase4` + `io-include-cap` |
| [tinct as a Templating Language](completed/templating.md) | `emit`, multi-file pipelines, formatters, string interpolation, literate mode | 2026-05-05 — all phases done: `templating-phase1` through `templating-phase4` |
| [String Interpolation](completed/string-interpolation.md) | `i"Hello $name"` — desugars to `[str ...]`; formatter ergonomics | 2026-05-05 — implemented as `templating-phase3`: `i"..."` + `${expr}` + formatter roundtrip |

### Accepted

These proposals have been formally accepted: `State: Accepted` marked, spec integrated, implementation sprints created in TODO.md. Not yet fully implemented.

| Proposal | Summary | Accepted |
|----------|---------|----------|
| [Unified Access and Generator Pipeline](access-pipeline.md) | Remove bracket access; add `\|` desugar-only pipe; `DotKey::Int` for `list.0`; `get`, `each`, `collect-kv` builtins | 2026-05-05 |
| [AST Dict Schema](ast-schema.md) | Canonical `Expr` → tinct dict mapping; `ast_to_dict` / `dict_to_ast`; shared by formatter, quasiquoting, macros | 2026-05-05 |
| [Quasiquoting](quasiquoting.md) | `[quote ...]` / `[unquote ...]` — AST as data; prerequisite for macros | 2026-05-05 |
| [Desugaring as Macros](macros.md) | Procedural AST macros via `[defmacro]`; user-defined syntactic transformations | 2026-05-05 |
| [Tinct-Hosted Formatter](tinct-hosted-formatter.md) | `tinct fmt` delegated to `stdlib/formatter/`; speculative rendering; shared `ast_to_dict` infrastructure | 2026-05-05 |

### Adopt Now

These proposals have no gating conditions and deliver standalone value at low cost.

**[Null Semantics](null-semantics.md) Phase 1** — One arm in `resolve_type_name`: `"Null" => Type::Record(Row::Empty)`. Zero new runtime machinery. `fn@Null` for void-returning builtins. Trigger already met — `type-checker-fixes` sprint asks for it.

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
| [Custom Call Aliases](call-aliases.md) | Macro system adoption |
| [Parameterized Type Aliases](parameterized-type-aliases.md) | Name collision becomes a real type error, or recursive ADTs needed (Phase 4) |
| [Pattern Matching](pattern-matching.md) Phase 2+ | Phase 1 gate met (type-predicates complete); Phase 2+ gated on let-binding and union types |
| [String Interning](string-interning.md) | Profiling confirms `String` allocation/comparison is top-5 hotspot on real workloads |
| [Union-Find for Type Substitution](union-find-substitution.md) | Profiling confirms average TypeVar chain depth ≥4 on real programs |
| [eval↔builtins Boundary](eval-builtins-boundary.md) | Independent builtin testing is a concrete need, OR evaluator refactor where decoupling reduces blast radius |
| [Value Serializer Visitor](value-serializer-visitor.md) | A third output format (YAML, TOML) is implemented and traversal duplication becomes maintenance burden |
| [Template-Polarity Embedding](template-polarity.md) | A real 90%+ static foreign-format file (nginx.conf, Dockerfile, Makefile) with ≤10 tinct substitutions where data-first is unreasonably awkward |
| [Macro-Rewrite](macro-rewrite.md) | When macros Phase 2 (`[defmacro]`) ships — implement before any typing-cluster A1/A2/A3/C1 Rust sprints |
| [Parse-Stage Macros](parse-stage-macros.md) | When `[defmacro match]` Phase 2 lands — `n@Int` and `n@String` as distinct pattern keys requires context-sensitive key identity at parse time |
| [Evaluation Semantics Verification](eval-semantics-verification.md) Phase 2+ | Phase 1 complete with zero failures; formal semantics in doc/08-evaluation.md |

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have accepted designs; adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [TLS, PKI, and HTTP](lib-tls.md) | mTLS and custom CA for internal-service tinct programs |
| [SQL Data Sources](lib-sql.md) | Lazy DB reads via `filter`/`map` predicate pushdown |
| [Numeric Types](numeric-types.md) | Range annotations + Decimal type |
| [Float Dict Keys](float-dict-keys.md) | Decimal keys; gated on Decimal type adoption |
| [Pattern Matching](pattern-matching.md) | Full match expression; Phase 1 gate met (type predicates complete); Phase 2+ next |
| [Nominal Variants](nominal-variants.md) Phase 1 | `tag-of` + unit constructors; independently useful as enum-like values |

---

## Dependency Graph

Reading order: each row depends on those above it in the same chain.

```
type-predicates ✓ Complete ──────────────────────────────── pattern-matching (Ph 1: unblocked)
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

io ✓ Complete ─── templating ✓ Complete
io ✓ Complete ─── tls (Ph 2)

string-interpolation ✓ Complete (templating-phase3)
new-syntax ✓ Complete

structural-contracts ─── numeric-types (Ph 1)
parameterized-type-aliases ─── algebraic-data-types (Ph 4, recursive ADTs)

arena-patterns ✓ Complete (all phases: variable-resolution-pass, ThunkArena + FlatEnv, CEK machine, --- boundary migration)

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

**[Unified Syntax Reform](completed/new-syntax.md)** — Complete 2026-05-05. Implemented in sprints `new-syntax-docs` through `new-syntax-migrate`. All existing tinct examples and other whatif docs have been updated to the new syntax.

### No Conflict (Apparent but Not Real)

**[Algebraic Data Types](algebraic-data-types.md) vs [Nominal Variants](nominal-variants.md) — both use `[union ...]`**

The same `[union ...]` form hosts both structural and nominal declarations, distinguished by case. A single union can mix both. This is intentional composability.

**[Structural Contracts](structural-contracts.md) vs [Type Classes](typeclasses.md) for validation**

Structural contracts are for boundary validation (JSON input, pipeline boundaries); type classes are for type-level protocols. Both can coexist; adopt structural contracts first for the immediate use case.

**[String Interning](string-interning.md) vs [Arena Patterns](completed/arena-patterns.md)**

String interning replaces `Key::String(String)` with `Key::String(Spur)`. Arena patterns replaced `Rc<Thunk>` with `ThunkId` (complete). Both are perf migrations that change the representation of different types — they don't conflict. Arena migration is done; string interning should be profiled before committing.
