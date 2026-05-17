# What If: Index

Design proposals for tinct features not yet in the language. Each document
makes the best case for its feature: "What would it take to do this well?"

Completed proposals are archived in [doc/whatif/completed/](completed/).

---

## Type System

| Proposal | Summary |
|----------|---------|
| [Boolean-Algebraic Subtyping](completed/boolean-algebraic-subtyping.md) | **Accepted 2026-05-09.** Replace Rémy row variables with BAS; Boolean lattice of union/intersection/negation types; S-RcdTop + S-ClsBot; principal type inference without backtracking |
| [Constraint Annotations and fn@[...] Metadata](constraint-annotations.md) | **Accepted 2026-05-11.** Refactor `fn@[...]` as a named-key metadata dict (`return:`, `constraint:`, `doc:`); `constraint: [a: Comparable]` binding syntax for TypeVar constraints; `fn@Type` shorthand permanent |
| [Record/Map Split and Parameterized Maps](completed/parameterized-dict.md) | **Accepted 2026-05-09.** `Record` vs `Map@[K: V]` type split; `Dict = Record ∨ Map` BAS union; `get?` for safe map access; order-insensitive structural dict equality |
| [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](completed/hkt-monads.md) | **Accepted 2026-05-11.** `Kind::Operator` (`* → *`); `Type::App`/`Type::Operator`; Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable hierarchy; Maybe ADT; `[do]` inference; `sequence`/`traverse`/`forM`/`when`/`liftM2`; `Kind::Label`; `HasField` constraint with `[HAS-FIELD-UNION]`/`[HAS-FIELD-INTER]`/`[HAS-FIELD-TOP]` BAS rules; label-polymorphic `get`/`get-in` |
| [Inference Completeness](inference-completeness.md) | SCC-based binding group analysis (Tarjan) within DICT-GEN; independent generalization of non-mutually-recursive entries; polymorphic access through visible nested dicts; variadic params as `Seq(T)`; typeclass-based heterogeneous variadics (FormatResult pattern) |
| [CHR-Unified Type Constraints](chr-unification.md) | **Accepted 2026-05-16.** `normalize()` unified type simplification; `TypeStageApp` lazy FD elaboration; deferred equality for non-injective resolvers; user-declared `[class ...]` and `[instance ...]`; scope-resident ClassEnv; arithmetic classes in prelude; automatic boundary guards |
| [Advanced Typeclass Extensions](advanced-typeclasses.md) | 3-parameter `Add a b c \| (a,b)→c` MPTC for precise mixed-mode arithmetic; row-level constraint propagation over BAS intersections (`Equatable {name: Str, age: Int}` distributes automatically); ClassEnv runtime dispatch enabling user-defined types to participate in `=`, `<`, `str` |
| [Parameterized Type Annotations](completed/parameterized-dict.md) | **Accepted 2026-05-09.** `Seq@T`, `Map@[K: V]`, `Map@T`, `Record@[...]` chained `@` annotation forms; `@Map@[String: Int]` compact form; type alias composition (`T2: [type Map@T1]`); see `doc/feature/parameterized-types.md` |
| [Type Annotations v2](type-annotations-v2.md) | Comprehensive revisit: explicit union (`\|`) and intersection (`&`) separators in `@[...]`; capability type annotation; property-dict collision fix; migration from positional-union convention; builds on `parameterized-annotations` sprint |

## Reflection and Metaprogramming

| Proposal | Summary |
|----------|---------|
| [Runtime Reflection — Annotations as Value Metadata](runtime-reflection.md) | `Value::Function` carries full annotation metadata (`doc:`, `return:`, params) at runtime via `FnAnnotation`; `ast-of` Rust primitive returns the AST dict for any value; `describe`/`sig-from-ast`/`annotation-of`/`source-of` in prelude; enables REPL `:describe`, LSP doc hover, docgen, and metaprogramming |

## Internal Integrity

| Proposal | Summary |
|----------|---------|
| [builtin-* Privacy](builtin-privacy.md) | **Accepted 2026-05-11.** Restrict `builtin-*` stable aliases to prelude evaluation context; env-layer isolation + T009 type-checker warning; migrate macros.llt, path.llt, toml-lite.llt to public wrappers |

## Error Handling

| Proposal | Summary |
|----------|---------|
| [Consistent Error Handling](completed/error-patterns.md) | **Accepted 2026-05-09.** Nominal `Ok@T\|Err@String` Result (not structural — S-RcdTop); `and-then` combinator; `[do monad ...]` macro; fallible I/O returns Result, pure functions propagate |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [Macro-Rewrite](completed/macro-rewrite.md) | Superseded — let-binding done as `Expr::Sequential`, match as `Expr::Match`. `i"..."` migrated to `[defmacro tmpl]` (`tmpl-macro` sprint complete, see DONE.md) |
| [Parse-Stage Macros](parse-stage-macros.md) | Syntax classes with context-sensitive key identity — user-defined macros can use full-annotated-expression equality for dict keys |
| [Custom Call Aliases](call-aliases.md) | `[timed f ...]` — macro-defined call forms; gated on macros |

## Runtime and Performance

| Proposal | Summary |
|----------|---------|
| [String Interning for Dict Keys](string-interning.md) | `Key::String(Spur)` via `string-interner` crate; O(1) comparison; profile-gated |
| [Union-Find for Type Substitution](union-find-substitution.md) | Path-compressed union-find for `Substitution::apply()`; worthwhile only if chain depth ≥4; profile-gated |
| [Float Dict Keys](float-dict-keys.md) | Decimal (exact base-10) keys alongside a `Decimal` type |

## Architecture and Refactoring

| Proposal | Summary |
|----------|---------|
| [Value Serializer Visitor](value-serializer-visitor.md) | Shared traversal for `value_to_json` + `value_to_display_string`; defer until a third format is needed |

## Formal Verification

| Proposal | Summary |
|----------|---------|
| [Evaluation Semantics Verification](eval-semantics-verification.md) | Part A: `proptest` bisimulation tests (PendingBuiltin ≡ Unevaluated, PendingCall ≡ inline); Part B: confluence proof via determinism argument |

## I/O and Connectivity

| Proposal | Summary |
|----------|---------|
| [TLS, PKI, and HTTP](completed/lib-tls.md) | **Accepted 2026-05-07.** Connector protocol, `tls-layer`, SpkiPin, system CA roots, mTLS, ALPN, SPKI pinning |
| [Composable Networking v2](completed/lib-net-v2.md) | **Accepted 2026-05-09.** Connector + Layer + Session model; transport-generic `connect`; Unix sockets; QUIC/HTTP/2/HTTP/3; `protocols/` subdirectory with SOCKS5, DNS, gRPC, WebSocket |
| [Directory Capability Permissions](dir-cap-permissions.md) | **Accepted 2026-05-11.** `--cap-fs name=path:r` permission flags on DirCap; `Readable`, `Statable`, `Listable`, `Writable`, `Appendable`, `Deletable`, `Renameable`; letter bundles (`r`=read+list+stat, `w`=write+append+delete+rename) + extended `:[Cap1 Cap2 ...]` syntax; row-polymorphic `DirCap@[Writable ...]` type; `narrow` for in-script attenuation; extends `--cap-file` with same extended syntax; no mode = full access |
| [SQL Data Sources](lib-sql.md) | `sql-open` returns lazy SQL source; `filter`/`map` push predicates to the DB |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [Multi-Line Strings](multi-line-strings.md) | **Accepted 2026-05-11.** `unindent` stdlib function strips indentation using last-line baseline; `"""..."""` is a parse-stage macro wrapping `[unindent "..."]`; `i"""..."""` for interpolation; no lexer changes needed |
| [Template-Polarity Embedding](template-polarity.md) | `tinct template` subcommand — `{{ expr }}` / `{% block %}` Jinja-style preprocessing of foreign-format files (nginx.conf, Dockerfile, Makefile) |

## Standard Library

| Proposal | Summary |
|----------|---------|
| [Supplemental Stdlib Modules](completed/lib-supplemental.md) | **Accepted 2026-05-07.** Strings, math, bitwise, Bytes, TOML-lite, FsCap, handle caps, StringView, path utils |
| [Date-Time Support](completed/lib-datetime.md) | **Accepted 2026-05-07.** Timestamp, Duration, ClockCap, Timezone via system zoneinfo/DirCap |
| [Pure-Tinct Regex Engine](completed/lib-regex.md) | **Accepted 2026-05-07.** Thompson NFA in pure-tinct; Pattern nominal variant; full API |


---

## Adoption Analysis

Cross-reference of each proposal against open TODO items and gating conditions.

### Accepted (in progress)

Accepted proposals with sprints in TODO.md. Not yet fully implemented.

| Proposal | Summary | Accepted |
|----------|---------|----------|
| [CHR-Unified Type Constraints](chr-unification.md) | `Type::TypeStageApp`; `normalize()` unified simplification pass; FD elaboration into equality goals; deferred equality for non-injective resolvers; `[class ...]` two-bracket form; `[instance ...]` match-arm syntax; scope-resident ClassEnv; arithmetic class migration to prelude; boundary guard elaboration | 2026-05-16 |
| [Advanced Typeclass Extensions](advanced-typeclasses.md) | MPTC `Add a b c \| (a,b)→c` for precise arithmetic; `[CONSTRAIN-FIELD/INTER/UNION/TOP/UNKNOWN/NEVER]` row-level propagation; ClassEnv runtime dispatch | 2026-05-14 |
| [Inference Completeness](inference-completeness.md) | SCC DICT-GEN (done); variadic `Seq(T)`; nested dict polymorphism via `TypeScheme.inner_schemes` | 2026-05-14 |
| [Runtime Reflection — Annotations as Value Metadata](runtime-reflection.md) | `FnAnnotation` on `Value::Function`; `ast-of` Rust primitive; `describe`/`sig-from-ast`/`annotation-of`/`source-of` in prelude | 2026-05-14 |

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
| [AST Dict Schema](completed/ast-schema.md) | Canonical `Expr` → tinct dict mapping; `ast_to_dict` / `dict_to_ast`; shared by formatter, quasiquoting, macros | 2026-05-06 — `ast-dict-core` complete; `ast-dict-source`, `dict-to-ast` in progress |
| [Tinct-Hosted Formatter](completed/tinct-hosted-formatter.md) | `tinct fmt --oneline/--nospaces/--minimize` compact modes; full layout formatter with comment preservation | 2026-05-06 — `formatter-compact` complete; `formatter-full` pending typing-cluster A1+A2 |
| [Quasiquoting](completed/quasiquoting.md) | `[quote ...]` / `[unquote ...]` — AST as data; prerequisite for macros | 2026-05-06 — `quote`, `unquote` in progress |
| [Desugaring as Macros](completed/macros.md) | Procedural AST macros via `[defmacro]`; user-defined syntactic transformations | 2026-05-06 — `defmacro`, `macro-hygiene`, `macro-integration` in progress |
| [Unified Access and Generator Pipeline](completed/access-pipeline.md) | Remove bracket access; add `\|` desugar-only pipe; `DotKey::Int` for `list.0`; `get`, `each`, `collect-kv` builtins | 2026-05-07 — access-pipeline |
| [Null Semantics](completed/null-semantics.md) | `@Null` = `Type::Record(Row::Empty)`; void-returning builtins typed precisely | 2026-05-07 — typing-cluster |
| [`let` Binding Form](completed/let-binding.md) | Sequential expressions inside `[fn ...]` bodies — no new keywords | 2026-05-07 — typing-cluster A1 |
| [Pattern Matching](completed/pattern-matching.md) | `[match x ...]` — type/literal/structural dispatch + guards; exhaustiveness checking | 2026-05-07 — typing-cluster A2/A3/C4/C5 |
| [Union Types and Algebraic Subtyping](completed/union-types.md) | `x@[Int Null]` annotation-only unions (Phase 2); Simple-sub inferred unions/intersections (Phase 3) | 2026-05-07 — typing-cluster B1/D2 |
| [Algebraic Data Types](completed/algebraic-data-types.md) | `[type [ok: a] [err: Str]]` — structural ADTs discriminated by key set | 2026-05-07 — typing-cluster C1 |
| [Nominal Variants](completed/nominal-variants.md) | `[type [Some a] None]` — opaque constructor-based variants with pattern matching | 2026-05-07 — typing-cluster C2/C3 |
| [Type Classes](completed/typeclasses.md) | `Eq a => a → a → Bool` — constrained polymorphism; full Haskell-style classes with dictionary passing | 2026-05-07 — typing-cluster B4/D1 |
| [Formal Gradual Typing](completed/gradual-typing.md) | `Unknown` + `Top` split; consistency relation; blame tracking | 2026-05-07 — typing-cluster B2/D4 |
| [Structural Contracts](completed/structural-contracts.md) | `%@Type` pipeline boundary checking + `validate` schema-as-dict runtime constraints | 2026-05-07 — typing-cluster SC1–4 |
| [Numeric Types](completed/numeric-types.md) | Range annotations + Decimal + BigInt + `repr:` storage hints | 2026-05-07 — typing-cluster N1–4 |
| [Parameterized Type Aliases](completed/parameterized-type-aliases.md) | `[type [a] body]` — fresh instantiation per use site; arity-checked type constructors | 2026-05-07 — typing-cluster B3 |
| [Path-Sensitive Narrowing](completed/narrowing.md) | Refine variable types in `if`/`match` branches from equality and type-predicate guards | 2026-05-07 — typing-cluster B5a/B5b |
| [Supplemental Stdlib Modules](completed/lib-supplemental.md) | Extended strings, math, bitwise, Bytes type, TOML-lite, FsCap protocol, capability-typed handles, StringView | 2026-05-07 — 8 sprints: `string-view` through `toml-lite-path` |
| [Date-Time Support](completed/lib-datetime.md) | Timestamp, Duration, ClockCap, Timezone via system zoneinfo | 2026-05-07 — `datetime` sprint |
| [Pure-Tinct Regex Engine](completed/lib-regex.md) | Thompson NFA in pure-tinct; Pattern nominal variant; re-compile/match/find/replace/split | 2026-05-07 — `regex` sprint |
| [TLS, PKI, and HTTP](completed/lib-tls.md) | Connector protocol, tls-connect, SpkiPin, HttpConn, system roots default, HTTP/1-3 | 2026-05-07 — `connector-tls` + `http-net` sprints |
| [Composable Networking v2](completed/lib-net-v2.md) | Connector + Layer + Session model; transport-generic `connect`; Unix sockets; QUIC/HTTP/2/HTTP/3; `protocols/` subdirectory | 2026-05-09 — `connect-v2`, `http-sessions`, `stdlib-protocols` |
| [Boolean-Algebraic Subtyping](completed/boolean-algebraic-subtyping.md) | Replace Rémy row variables with BAS; Boolean lattice of union/intersection/negation types; S-RcdTop + S-ClsBot; principal type inference | 2026-05-09 — `bas-core` |
| [Consistent Error Handling](completed/error-patterns.md) | Nominal `Ok@T\|Err@String` Result; `and-then` combinator; `[do monad ...]` macro; fallible I/O returns Result, pure functions propagate | 2026-05-09 — `result-nominal` |
| [Record/Map Split and Parameterized Maps](completed/parameterized-dict.md) | `Record` vs `Map@[K: V]` type split; `Dict = Record ∨ Map` BAS union; `get?` for safe map access; order-insensitive structural dict equality | 2026-05-09 — `record-map-split` |

### Adopt Now

These proposals have no gating conditions and deliver standalone value at low cost.

**[Custom Call Aliases](call-aliases.md)** — `[timed f ...]` macro-defined call forms. Macros cluster is complete — prerequisite met.

**[Evaluation Semantics Verification](eval-semantics-verification.md) Phase 1 (partial)** — Confluence proof sketch to `doc/08-evaluation.md` is done; core proptest suite (200 lines + `proptest` dev-dep) still pending implementation.

### Wait for Trigger

These proposals have accepted designs but explicit gating conditions not yet met.

| Proposal | Gating Condition |
|----------|-----------------|
| [String Interning](string-interning.md) | Profiling confirms `String` allocation/comparison is top-5 hotspot on real workloads |
| [Union-Find for Type Substitution](union-find-substitution.md) | Profiling confirms average TypeVar chain depth ≥4 on real programs |
| [Value Serializer Visitor](value-serializer-visitor.md) | A third output format (YAML, TOML) is implemented and traversal duplication becomes maintenance burden |
| [Template-Polarity Embedding](template-polarity.md) | A real 90%+ static foreign-format file (nginx.conf, Dockerfile, Makefile) with ≤10 tinct substitutions where data-first is unreasonably awkward |
| [Parse-Stage Macros](parse-stage-macros.md) | When a second macro needs context-sensitive key identity or argument-position parse modes |
| [Evaluation Semantics Verification](eval-semantics-verification.md) Phase 2+ | Phase 1 proptest suite implemented with zero failures (doc proof sketch done; proptest pending) |

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have accepted designs; adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [SQL Data Sources](lib-sql.md) | Lazy DB reads via `filter`/`map` predicate pushdown |
| [Float Dict Keys](float-dict-keys.md) | `Key::Decimal` — Phase 1 (`Value::Decimal`) complete; Phase 2 (key extension) open |

---

## Dependency Graph

Reading order: each row depends on those above it in the same chain.

```
# Typing cluster ✓ Complete (2026-05-07)
type-predicates ✓ ─── let-binding ✓ ─── pattern-matching ✓ ─── union-types ✓
                                              │                        │
                                         narrowing ✓           ADTs ✓ ─── nominal-variants ✓
                                                                │
                                       gradual-typing ✓ ─── type-classes ✓
                                       parameterized-type-aliases ✓
                                       structural-contracts ✓ ─── numeric-types ✓

# Macros cluster ✓ Complete
quasiquoting ✓ ─── macros ✓ ─── call-aliases (adopt now)
                              ─── macro-rewrite ✓ superseded (tmpl done — see DONE.md)
                              ─── parse-stage-macros

# I/O
io ✓ Complete ─── templating ✓ Complete ─── template-polarity
io ✓ Complete ─── lib-tls ✓ ─── lib-net-v2 ✓ (connect-v2, http-sessions, stdlib-protocols)

# Standard library
lib-supplemental ✓ ─── lib-regex ✓

# Formal verification
eval-semantics-verification (Ph 1) ─── eval-semantics-verification (Ph 2+)

# Post typing-cluster type system research
union-types ✓ ─── boolean-algebraic-subtyping ✓ ─── record-map-split (parameterized-dict) ✓
                                               └─── error-patterns ✓ (nominal Result)

# Profile-gated (no deps, waiting for profiling data)
string-interning, union-find-substitution

# No deps, can adopt now
float-dict-keys (Phase 1 Decimal ✓ — Phase 2 Key::Decimal open)
value-serializer-visitor
```

---

## Conflicts and Alternative Paths

No two proposals are fully mutually exclusive — adopting one never prevents the other. However, several pairs represent alternative paths or create ordering tension.

### Alternative Solutions to the Same Problem

**Dual-dispatch typing: [Type Classes](completed/typeclasses.md) vs [Union Types](completed/union-types.md)**

Both solve the problem of typing `map`, `filter`, and other dual-dispatch builtins. Type classes solve it with `Functor f => (a → b) → f a → f b`; union types solve it with `(a → b) → (Dict a | Seq a) → (Dict b | Seq b)`. Both are now implemented and coexist — union types handle nullable types and ADTs (`Int | Null`, `try` result types), while type classes provide constrained polymorphism for `=`, `+`, and user-extensible protocols.

### Supersession

**[Nominal Variants](completed/nominal-variants.md) making [Algebraic Data Types](completed/algebraic-data-types.md) conventions redundant for some use cases**

For use cases where opaque construction matters, nominal variants are strictly more expressive. Structural ADTs remain correct for JSON interop (structural variants round-trip; nominal variants don't reconstruct from `from-json`). These coexist — but for any specific type declaration, the user must choose.

### Runtime Representation Tension

**[Nominal Variants](completed/nominal-variants.md) + JSON serialization**

Nominal `[Ok 42]` → `{"Ok": 42}`. `from-json` always produces structural dicts, losing nominal identity. Do not use nominal variants for data that must survive JSON round-trips; use structural ADTs instead.

### The One-Way Migration Door

**[Unified Syntax Reform](completed/new-syntax.md)** — Complete 2026-05-05. Implemented in sprints `new-syntax-docs` through `new-syntax-migrate`. All existing tinct examples and other whatif docs have been updated to the new syntax.

### No Conflict (Apparent but Not Real)

**[Algebraic Data Types](completed/algebraic-data-types.md) vs [Nominal Variants](completed/nominal-variants.md) — both use `[union ...]`**

The same `[union ...]` form hosts both structural and nominal declarations, distinguished by case. A single union can mix both. This is intentional composability.

**[Structural Contracts](completed/structural-contracts.md) vs [Type Classes](completed/typeclasses.md) for validation**

Structural contracts are for boundary validation (JSON input, pipeline boundaries); type classes are for type-level protocols. Both can coexist; adopt structural contracts first for the immediate use case.

**[String Interning](string-interning.md) vs [Arena Patterns](completed/arena-patterns.md)**

String interning replaces `Key::String(String)` with `Key::String(Spur)`. Arena patterns replaced `Rc<Thunk>` with `ThunkId` (complete). Both are perf migrations that change the representation of different types — they don't conflict. Arena migration is done; string interning should be profiled before committing.
