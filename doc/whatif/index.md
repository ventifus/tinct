# What If: Index

Design proposals for tinct features not yet in the language. Each document
makes the best case for its feature: "What would it take to do this well?"

Completed proposals are archived in [doc/whatif/completed/](completed/).

---

## Accepted

Proposals formally accepted into the project. Implementation sprints exist in the tracker.

| Proposal | Summary | Accepted |
|----------|---------|----------|
| [Type System Foundations — Primitives, Collections, and Dispatch](type-foundations.md) | `Value::Dict` as sole runtime collection primitive; `HashableValue` replaces `Key` enum (commutative-sum order-insensitive hash); `Boolean`/`Seq` as tinct-defined nominal types; `List` as lazy 2-3 finger tree; type system de-primitisation (unified env lookup, `Type::Bool`/`Number`/`Seq` deleted); `TypeContext` opaque handle; `loader.llt` bootstrap restructure; collection typeclasses with O(log n) complexity promises (`Prependable`, `Appendable`, `Concatenable`, `Indexable`, `Iterable`, `Hashable`, `Sortable`); `[Bytes N]` refinement type via `TypeNode.SizedBytes`; type-level lookup tables with compile-time constants on variants; `Codec`/`ByteStream`/`Datagram`/`Seekable` typeclasses; discriminated error unions per subsystem | 2026-06-25 |
| [Equirecursive Types](equirecursive-types.md) | `TypeNode.Recursive`/`RecursiveRef`/`TypeVar` as TypeNode constructors; `CheckerType = Node(Value)` primary type representation; S-Exp + S-Assum coinductive subtyping (Chau & Parreaux 2026); general `@[...]` annotation syntax; `@Child` field annotations with derived `children`/`map-children`; `mu` combinator; contractiveness check; TyConDef/TypeAlias merge; `expand_named` always-expand normalization; `gensym-with-scope` | 2026-06-05 |
| [User-Defined N-Arity Type Constructors](user-type-constructors.md) | `Type::TyCon(String)` + `Type::App`; unified `[type ...]` syntax with `[let ...]` params; variance annotations; nominal ADTs with qualified runtime tags; `RowTail::Uniform` column constraints; `Absent` type; scoped ClassEnv/InstanceEnv with local coherence; per-dict Substitution; `values_equal` canonical merge; `Pattern::TypeAssert`; prelude migration | 2026-06-03 |
| [Tinct Stream Format — Stdlib-Closed Normal Form](data-streaming.md) | SCN streaming format for tinct-to-tinct pipes; `emit` via `%emit` channel; concurrent output program contract; `-i stream`/`-o stream`; `to-tinct` serializer; eliminates serde_json from profiling | 2026-05-30 |

---

## Type System

| Proposal | Summary |
|----------|---------|
| [Type::Variant for Transport Constants](completed/type-variant.md) | **Superseded.** Use nominal variants: `[union Transport [Tcp] [Udp] [Quic] [Unix]]`. See `transport-typing` sprint in TODO.md. |
| [Precise HKT Types for map/filter/reduce/each](hkt-map-filter-types.md) | **Superseded.** Implementation detail only; see `hkt-map-filter-types` sprint in TODO.md. |
| [Unify Type-Checker and Runtime Type Judgments](completed/typecheck-runtime-unification.md) | **Completed 2026-05-28.** Eliminate divergence between static and runtime type checking: `is_consistent_subtype(ground_type_of(v), T)` unified runtime check, four-source `Unknown` separation, `failed_bindings → Type::Error`, `RuntimeTypeCheck` deletion, `builtin_typed!` co-location |
| [Schema-Directed from-json](schema-directed-from-json.md) | `[from-json @[host: Str port: Int] input]` — typed JSON parse returning specific Record type; boundary guard at parse site; schema doubles as documentation |
| [Guardedness](guardedness.md) | Static detection of non-productive circular data dependencies; separates legitimate corecursion (`[cons 1 ones]`) from diverging definitions (`[x: [+ x 1]]`); assigns `Never` via BAS to rejected bindings; enables infinite lazy structures as a first-class language feature |
| [Boolean-Algebraic Subtyping](completed/boolean-algebraic-subtyping.md) | **Accepted 2026-05-09.** Replace Rémy row variables with BAS; Boolean lattice of union/intersection/negation types; S-RcdTop + S-ClsBot; principal type inference without backtracking |
| [Constraint Annotations and fn@[...] Metadata](completed/constraint-annotations.md) | **Accepted 2026-05-11.** Refactor `fn@[...]` as a named-key metadata dict (`return:`, `constraint:`, `doc:`); `constraint: [a: Comparable]` binding syntax for TypeVar constraints; `fn@Type` shorthand permanent |
| [Record/Map Split and Parameterized Maps](completed/parameterized-dict.md) | **Accepted 2026-05-09.** `Record` vs `Map[K: V]` type split with bracket application form `@[Map [K: V]]`; `Dict = Record ∨ Map` BAS union; `get?` for safe map access; order-insensitive structural dict equality |
| [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](completed/hkt-monads.md) | **Accepted 2026-05-11.** `Kind::Operator` (`* → *`); `Type::App`/`Type::Operator`; Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable hierarchy; Maybe ADT; `[do]` inference; `sequence`/`traverse`/`forM`/`when`/`liftM2`; `Kind::Label`; `HasField` constraint with `[HAS-FIELD-UNION]`/`[HAS-FIELD-INTER]`/`[HAS-FIELD-TOP]` BAS rules; label-polymorphic `get`/`get-in` |
| [User-Defined N-Arity Type Constructors](user-type-constructors.md) | `Type::TyCon(String)` + `Type::App` uniform representation; opaque `[type [let a b] ...]`; nominal `[type [let a] Ctor ...]`; unified `[type ...]` syntax with `[let ...]` for params, bare uppercase unit constructors, dict-entry form only; removes `Type::Seq`/`Type::Map`/`Type::Handle` dedicated variants and all `apply_builtin_constructor` special-casing; builtins declared in prelude; `RowTail::Uniform(V)` column constraints make Map a transparent alias `{_ : v}` |
| [Equirecursive Types](equirecursive-types.md) | `Type::Recursive` (μ-type) for structural recursive types (JsonValue, Config schemas); coinductive bisimulation subtyping; `mu`/`recvar` type prelude combinators; depends on user-type-constructors.md (nominal ADTs eliminate depth-limit problem for constructor-defined types; this covers structural recursive types) |
| [Inference Completeness](completed/inference-completeness.md) | **Accepted 2026-05-14.** SCC-based binding group analysis (Tarjan) within DICT-GEN; independent generalization of non-mutually-recursive entries; polymorphic access through visible nested dicts; variadic params as `Seq(T)`; typeclass-based heterogeneous variadics (FormatResult pattern) |
| [CHR-Unified Type Constraints](completed/chr-unification.md) | **Accepted 2026-05-16.** `normalize()` unified type simplification; `TypeStageApp` lazy FD elaboration; deferred equality for non-injective resolvers; user-declared `[class ...]` and `[instance ...]`; scope-resident ClassEnv; arithmetic classes in prelude; automatic boundary guards |
| [Advanced Typeclass Extensions](completed/advanced-typeclasses.md) | **Accepted 2026-05-14.** 3-parameter `Add a b c \| (a,b)→c` MPTC for precise mixed-mode arithmetic; row-level constraint propagation over BAS intersections (`Equatable {name: Str, age: Int}` distributes automatically); ClassEnv runtime dispatch enabling user-defined types to participate in `=`, `<`, `str` |
| [Parameterized Type Annotations](completed/parameterized-dict.md) | **Accepted 2026-05-09.** Bracket application form `@[Seq T]`, `@[Map [K: V]]`, `@Map` bare; type alias composition (`T2: [type [Map T1]]`); Record/Map split; see `doc/feature/parameterized-types.md` |
| [Type Annotations v2](completed/type-annotations-v2.md) | **Accepted 2026-05-14.** Bracket application `@[Type Arg]` replaces chained `@`; `or`/`each` type-stage combinators for union/intersection; `bind:`/`return:`/`constraint:`/`kinds:` annotation keys; TypeVar scoping via `bind:`; `@Record@[...]` and all chained-@ forms retired |

| [Matchable — Open, User-Defined Patterns](matchable-patterns.md) | `Matchable` typeclass makes patterns first-class values; `try-match` returns `Dict \| Absent`; all built-in patterns (dict, constructor, literal, wildcard, type guard) become tinct instances in prelude; pattern combinators (`or-pattern`, `guard-pattern`, `as-pattern`) as library functions; function parameters ARE Matchables; multi-clause functions via `[fn-clauses ...]`; only `builtin-try-match` + match loop + fn call dispatcher remain in Rust |

## Language Architecture

| Proposal | Summary |
|----------|---------|
| [Type-Stage Programming as the Foundation for Constructors, Typeclasses, and Pattern Matching](type-stage-foundation.md) | Replace hardwired Rust implementations of `[type ...]`, `[class ...]`, `[instance ...]`, and constructor pattern matching with type-stage tinct programs. Constructors become functions with a specific return type; the Constructor protocol unifies construction and deconstruction; pattern matching dispatches on the runtime value type with no AST heuristics; typeclasses are dict-passing macros; Rust runtime reduced to ~20 primitives plus CEK machine |
| [Types as First-Class Runtime Values](runtime-types.md) | `builtin-tc-add-type tc DirCap` — type values passed directly to TypeContext operations; `fundamental-tc` built by selectively injecting type values from loader's scope; pre-populated master tycon registry; `[builtin-make-type-ctx]` starts empty; `builtin-tc-add-type` uses `typenode_value_to_type` to extract name and copy TyConDef from master registry |

## Reflection and Metaprogramming

| Proposal | Summary |
|----------|---------|
| [Runtime Reflection — Annotations as Value Metadata](completed/runtime-reflection.md) | **Accepted 2026-05-14.** `Value::Function` carries full annotation metadata (`doc:`, `return:`, params) at runtime via `FnAnnotation`; `ast-of` Rust primitive returns the AST dict for any value; `describe`/`sig-from-ast`/`annotation-of`/`source-of` in prelude; enables REPL `:describe`, LSP doc hover, docgen, and metaprogramming |
| [Decomposing `include` into `load`, `expand`, and `eval`](completed/include-decomposition.md) | **Accepted 2026-05-18.** Eight Rust primitives (`load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put`); `include`, `eval-file`, `eval-document-pipeline`, `cli-pipeline` self-hosted in prelude; content-addressed cache keyed by `(dev, ino) + source`; `%include-dir` injected for sub-includes; `builtin_include` deleted |
| [Runtime v2 — AST Redesign, Native Value Types, Async Parallel Evaluation](runtime-v2.md) | **Accepted 2026-05-20.** Supersedes `ast-value-types.md` and `async-eval.md`. Three-part coherent rewrite: (1) `SurfaceExpression`/`CoreExpr` split, `NodeId` side tables, `SurfaceDeclaration` separation; (2) `Value::Program`/`Document`/`Expression` with typed match dispatch, `Expression` nominal type in prelude; (3) `async fn` eval, `Rc`→`Arc`, `OnceLock` thunk, parallel dict eval, `task`/`await`/`channel`/`select-once`, context/cancellation, `finally`. Networking depends on this; see `lib-net-v3.md` |
| [Network Serve and Connect Layers](lib-net-v3.md) | **Draft 2026-05-19.** Compositional serve/connect layer model on top of runtime-v2 async foundation; `make-serve-layer`/`make-multiplex-serve` factories; TLS/WireGuard/Noise/H2/H3/WebSocket serve and connect layers; HTTP/2 and HTTP/3 request clients; transport-agnostic application protocol pattern (DNS worked example); ICMP Ping Tunnel worked example; `stdlib/http1.llt`, `http3.llt`, `serve.llt`, `http.llt`, `dns.llt`; removes `hyper`/`reqwest`, adds `h3` crate |

## Internal Integrity

| Proposal | Summary |
|----------|---------|
| [builtin-* Privacy](builtin-privacy.md) | **Accepted 2026-05-28.** Delete `standard_builtins()` / `create_root_env()` / `TypeEnv::with_builtins()`. Replace with `--- uses: ["core"]` document headers and `builtin_module()` registry — builtins injected doc-locally into each stdlib file's scope; user code inherits only prelude's exported dict. Prerequisite: B-168. |

## Error Handling

| Proposal | Summary |
|----------|---------|
| [Consistent Error Handling](completed/error-patterns.md) | **Accepted 2026-05-09.** Nominal `[or [Ok T] [Err String]]` Result (not structural — S-RcdTop); `and-then` combinator; `[do monad ...]` macro; fallible I/O returns Result, pure functions propagate |

## Macros and Syntax

| Proposal | Summary |
|----------|---------|
| [Self-Hosted Macro Expander](self-hosted-expander.md) | Replace `src/expand.rs` with a tinct program; single `Expr.*` AST representation; parser converts `SurfaceExpression` to `Expr.*` once at parse boundary; expander, and progressively resolver/type-checker, work with `Expr.*` values throughout |
| [Macro-Rewrite](completed/macro-rewrite.md) | Superseded — let-binding done as `Expr::Sequential`, match as `Expr::Match`. `i"..."` migrated to `[defmacro tmpl]` (`tmpl-macro` sprint complete, see DONE.md) |
| [Macro System v2](completed/macros-v2.md) | **Completed 2026-05-27.** `macro` unified form with `[let ...]` patterns; `inject: name` for anaphoric binding with dict-key override; `splice` for multi-form output; `macro-error`/`span-of`; `syntax-class`; `flatten-args`; parser enforcement moved to type checker. Supersedes `parse-stage-macros.md`. |
| [Custom Call Aliases](call-aliases.md) | `[timed f ...]` — macro-defined call forms; gated on macros |

## Concurrency and Distribution

| Proposal | Summary |
|----------|---------|
| [Async, Parallel, and Stdlib](async-eval.md) | `async fn` + `Rc`→`Arc` + `OnceLock` thunk + multi-thread Tokio; parallel dict eval; `par`/`par-map`; `task`/`await`/`channel`/`select`/`context`/`timeout`/`finally`/`exit`; serve+connect layer composition (`make-serve-layer`, `make-multiplex-serve`); Rust/tinct boundary; stdlib module map |
| [Distributed Evaluation](dist-eval.md) | `remote-task` / `cluster-local`; thunk serialization; content-addressed result cache (SHA-256); `dist-map`/`dist-reduce`; capability delegation (pure/delegated/proxied); worker protocol over QUIC; automatic distribution. Depends on: async-eval |

## Runtime and Performance

| Proposal | Summary |
|----------|---------|
| [String Interning for Dict Keys](string-interning.md) | `Key::String(Spur)` via `string-interner` crate; O(1) comparison; profile-gated |
| [Union-Find for Type Substitution](union-find-substitution.md) | Path-compressed union-find for `Substitution::apply()`; worthwhile only if chain depth ≥4; profile-gated |
| [Float Dict Keys](float-dict-keys.md) | Decimal (exact base-10) keys alongside a `Decimal` type |

## Type System Architecture

| Proposal | Summary |
|----------|---------|
| [Self-Hosted Type Checker](self-hosted-typechecker.md) | Implement HM type inference entirely in tinct type-stage code, replacing the `Type` Rust enum and inference engine. Single canonical TypeNode Value representation throughout; extensible type system; self-hosting. Requires type-foundations (done), equirecursive-types (done), and a bootstrapping strategy. |

## Architecture and Refactoring

| Proposal | Summary |
|----------|---------|
| [Stdlib Architecture — The Rust/tinct Boundary](stdlib-architecture.md) | **Merged into async-eval.md.** See §Rust/tinct Boundary, §Serve and Connect Layers, §Stdlib Module Map. |
| [Value Serializer Visitor](value-serializer-visitor.md) | Shared traversal for `value_to_json` + `value_to_display_string`; defer until a third format is needed |
| [Tinct Stream Format — Stdlib-Closed Normal Form](data-streaming.md) | **Accepted 2026-05-30.** See §Accepted above. |
| [Literate Mode v2 — Self-Hosted Block Evaluation and Codec Pipeline](literate-v2.md) | Per-block output format via ` ```tinct \| json ` fence info strings; codec objects with encode/decode; `stdlib/literate.llt` replaces Rust serialization; `\| json` equivalent to `-o json` |

## Formal Verification

| Proposal | Summary |
|----------|---------|
| [Evaluation Semantics Verification](eval-semantics-verification.md) | Part A: `proptest` bisimulation tests (PendingBuiltin ≡ Unevaluated, PendingCall ≡ inline); Part B: confluence proof via determinism argument |

## Security

| Proposal | Summary |
|----------|---------|
| [Information Flow Control](information-flow.md) | `Tainted` and `Secret` type-level labels; label propagation through type inference; sanitizers strip taint after structural validation; declassifiers allow deliberate secret use; all network receive operations return `Tainted`; crypto primitives accept and return `Secret` key material; zero runtime overhead (erased at compile time). Depends on lib-net-v3. |

## I/O and Connectivity

| Proposal | Summary |
|----------|---------|
| [TLS, PKI, and HTTP](completed/lib-tls.md) | **Accepted 2026-05-07.** Connector protocol, `tls-layer`, SpkiPin, system CA roots, mTLS, ALPN, SPKI pinning |
| [Composable Networking v2](completed/lib-net-v2.md) | **Accepted 2026-05-09.** Connector + Layer + Session model; transport-generic `connect`; Unix sockets; QUIC/HTTP/2/HTTP/3; `protocols/` subdirectory with SOCKS5, DNS, gRPC, WebSocket |
| [Directory Capability Permissions](completed/dir-cap-permissions.md) | **Accepted 2026-05-11.** `--cap-fs name=path:r` permission flags on DirCap; `Readable`, `Statable`, `Listable`, `Writable`, `Appendable`, `Deletable`, `Renameable`; letter bundles (`r`=read+list+stat, `w`=write+append+delete+rename) + extended `:[Cap1 Cap2 ...]` syntax; row-polymorphic `[DirCap [Writable ...]]` type; `narrow` for in-script attenuation; extends `--cap-file` with same extended syntax; no mode = full access |
| [SQL Data Sources](lib-sql.md) | `sql-open` returns lazy SQL source; `filter`/`map` push predicates to the DB |
| [FFI and Native Module Extensions](ffi.md) | Three options: (1) `[extern "lib.so" ...]` external C FFI; (2) `[include [native-module "sql"]]` lazy builtin registry for in-tree feature builtins; (3) Cargo workspace split with static (3A) or dynamic plugin (3B) linking |
| [Structured Logging](structured-logging.md) | `trace` builtin + `stdlib/log.llt`; output model (default to `=== out` vs `=== info`); redirect mechanism; literate/corpus integration |

## Syntax and Ergonomics

| Proposal | Summary |
|----------|---------|
| [Multi-Line Strings](completed/multi-line-strings.md) | **Accepted 2026-05-11.** `unindent` stdlib function strips indentation using last-line baseline; `"""..."""` is a parse-stage macro wrapping `[unindent "..."]`; `i"""..."""` for interpolation; no lexer changes needed |
| [Unified Binding Declarations](completed/unified-bindings.md) | **Completed 2026-05-27.** `[let ...]` universal binding form for fn/class/type/instance/case; `[case ...]` explicit match arms; `...` placeholder expression; constructor payload registry; parsing invariant: `[let ...]` = binding, everything else = expression |
| [Template-Polarity Embedding](template-polarity.md) | `tinct template` subcommand — `{{ expr }}` / `{% block %}` Jinja-style preprocessing of foreign-format files (nginx.conf, Dockerfile, Makefile) |

## Standard Library

| Proposal | Summary |
|----------|---------|
| [Supplemental Stdlib Modules](completed/lib-supplemental.md) | **Accepted 2026-05-07.** Strings, math, bitwise, Bytes, TOML-lite, FsCap, handle caps, StringView, path utils |
| [Date-Time Support](completed/lib-datetime.md) | **Accepted 2026-05-07.** Timestamp, Duration, ClockCap, Timezone via system zoneinfo/DirCap |
| [Pure-Tinct Regex Engine](completed/lib-regex.md) | **Accepted 2026-05-07.** Thompson NFA in pure-tinct; Pattern nominal variant; full API |
| [Linear Accumulators](completed/linear-accumulators.md) | **Accepted 2026-05-22.** `build-dict` O(n) keyed-dict construction; `Value::Builder` O(1) transient accumulation; stdlib rewrite eliminating O(n²) `append`/`merge` loops; dist-eval wire serialization fix |

---

## Adoption Analysis

Cross-reference of each proposal against open TODO items and gating conditions.

### Accepted (in progress)

Accepted proposals with sprints in TODO.md. Not yet fully implemented.

| Proposal | Summary | Accepted |
|----------|---------|----------|
| [Runtime v2 — AST Redesign, Native Value Types, Async Parallel Evaluation](runtime-v2.md) | `SurfaceExpression`/`CoreExpr` split; `NodeId` side tables; `ResolutionTable`/`TypeAnnotationTable`; `Value::Program`/`Document`/`Expression`; `async fn` eval; `Rc`→`Arc`; `OnceCell` thunk; parallel dict eval; `task`/`await`/`channel`/`select-once`; context/cancellation | 2026-05-20 |

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
| [Macro System v2](completed/macros-v2.md) | `[macro ...]` with `[let ...]` patterns; `inject:`; `syntax-class`; `splice`; `macro-error`/`span-of`; typed `Expr` variants; `flatten-args`; meta-macros | 2026-05-27 |
| [Unified Binding Declarations](completed/unified-bindings.md) | `[let ...]` universal binding form; `[case ...]` match arms; `...` placeholder; constructor payload registry; structural test `[let v: Ok]` | 2026-05-27 |
| [Program Profiling and Call Tracing](completed/profiling.md) | `--profile spans.ndjson` span collection; `scripts/profile/` analysis scripts; dual attribution; stall attribution; Criterion benchmarks | 2026-05-27 |
| [Parameterized Type Aliases](completed/parameterized-type-aliases.md) | `[type [a] body]` — fresh instantiation per use site; arity-checked type constructors | 2026-05-07 — typing-cluster B3 |
| [Path-Sensitive Narrowing](completed/narrowing.md) | Refine variable types in `if`/`match` branches from equality and type-predicate guards | 2026-05-07 — typing-cluster B5a/B5b |
| [Supplemental Stdlib Modules](completed/lib-supplemental.md) | Extended strings, math, bitwise, Bytes type, TOML-lite, FsCap protocol, capability-typed handles, StringView | 2026-05-07 — 8 sprints: `string-view` through `toml-lite-path` |
| [Date-Time Support](completed/lib-datetime.md) | Timestamp, Duration, ClockCap, Timezone via system zoneinfo | 2026-05-07 — `datetime` sprint |
| [Pure-Tinct Regex Engine](completed/lib-regex.md) | Thompson NFA in pure-tinct; Pattern nominal variant; re-compile/match/find/replace/split | 2026-05-07 — `regex` sprint |
| [TLS, PKI, and HTTP](completed/lib-tls.md) | Connector protocol, tls-connect, SpkiPin, HttpConn, system roots default, HTTP/1-3 | 2026-05-07 — `connector-tls` + `http-net` sprints |
| [Composable Networking v2](completed/lib-net-v2.md) | Connector + Layer + Session model; transport-generic `connect`; Unix sockets; QUIC/HTTP/2/HTTP/3; `protocols/` subdirectory | 2026-05-09 — `connect-v2`, `http-sessions`, `stdlib-protocols` |
| [Boolean-Algebraic Subtyping](completed/boolean-algebraic-subtyping.md) | Replace Rémy row variables with BAS; Boolean lattice of union/intersection/negation types; S-RcdTop + S-ClsBot; principal type inference | 2026-05-09 — `bas-core` |
| [Consistent Error Handling](completed/error-patterns.md) | Nominal `[or [Ok T] [Err String]]` Result; `and-then` combinator; `[do monad ...]` macro; fallible I/O returns Result, pure functions propagate | 2026-05-09 — `result-nominal` |
| [Record/Map Split and Parameterized Maps](completed/parameterized-dict.md) | `Record` vs `Map[K: V]` type split with bracket application form `@[Map [K: V]]`; `Dict = Record ∨ Map` BAS union; `get?` for safe map access; order-insensitive structural dict equality | 2026-05-09 — `record-map-split` |
| [Constraint Annotations and fn@[...] Metadata](completed/constraint-annotations.md) | `fn@[return: T constraint: [a: Comparable] doc: "..."]`; TypeVar constraint binding syntax; `fn@Type` shorthand permanent; `TypeScheme.doc` for LSP hover | 2026-05-11 — `constraint-annotations` |
| [builtin-* Privacy](builtin-privacy.md) | Env isolation not yet achieved — moved to Accepted; see Internal Integrity above | — |
| [Multi-Line Strings](completed/multi-line-strings.md) | `"""..."""` triple-quoted strings; `unindent` stdlib strips indentation via last-line baseline; `i"""..."""` interpolation; parse-stage macro desugaring | 2026-05-11 — `prelude-triple-quote` (ongoing); lexer + parser complete |
| [Directory Capability Permissions](completed/dir-cap-permissions.md) | `DirPerms` struct with 7 flags; `--cap-fs name=path:mode` CLI parsing; `from_letter` bundles (`r`/`w`/`a`/`s`/`l`); row-polymorphic `[DirCap [Readable ...]]` type | 2026-05-11 — `dir-cap-permissions` |
| [Inference Completeness](completed/inference-completeness.md) | SCC-based DICT-GEN; variadic `Seq(T)` typed params; nested dict polymorphism via `TypeScheme.inner_schemes`; typeclass-based heterogeneous variadics | 2026-05-14 — `inference-completeness-variadic`, `inference-completeness-nested-dict` |
| [Advanced Typeclass Extensions](completed/advanced-typeclasses.md) | MPTC `Add a b c \| (a,b)→c` for mixed-mode arithmetic; `[CONSTRAIN-FIELD/INTER/UNION/TOP/UNKNOWN/NEVER]` propagation; ClassEnv runtime dispatch for user-defined `=`, `<`, `str` | 2026-05-14 — `typeclass-constraint-propagation`, `typeclass-mptc-fundeps`, `typeclass-runtime-dispatch` |
| [Runtime Reflection — Annotations as Value Metadata](completed/runtime-reflection.md) | `FnAnnotation` on `Value::Function`; `ast-of` Rust primitive; `describe`/`sig-from-ast`/`annotation-of`/`source-of` in prelude; LSP hover + docgen | 2026-05-14 — `runtime-reflection-core`, `runtime-reflection-include` |
| [Decomposing `include` into `load`, `expand`, and `eval`](completed/include-decomposition.md) | Eight Rust primitives (`load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put`); `include`/`eval-file`/`eval-document-pipeline`/`cli-pipeline` self-hosted in prelude; content-addressed cache; `builtin_include` deleted; `reduce-cont-step` O(N) stack fix | 2026-05-19 — `include-decomp-primitives`, `include-decomp-eval-primitives`, `include-decomp-prelude`, `include-decomp-redelete`, `include-decomp-eval-types-fix`, `reduce-cont-step` |
| [Higher-Kinded Types, Monadic `[do]`, and Precise Field Access](completed/hkt-monads.md) | `Kind::Operator` (`* → *`); `Type::App`/`Type::Operator`; Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable hierarchy; Maybe ADT; `[do]` inference; `sequence`/`traverse`/`forM`/`when`/`liftM2`; `Kind::Label`; `HasField` constraint with `[HAS-FIELD-UNION]`/`[HAS-FIELD-INTER]`/`[HAS-FIELD-TOP]` BAS rules; label-polymorphic `get`/`get-in` | 2026-05-11 |
| [Linear Accumulators](completed/linear-accumulators.md) | `build-dict` O(n) keyed-dict construction; `Value::Builder` transient O(1) amortized accumulation; stdlib rewrite eliminating all O(n²) `append`/`merge` loops; `EvalError::builder_already_finished` one-shot invariant; `group-by` via Builder | 2026-05-22 — `linear-accumulators-seq`, `linear-accumulators-build-dict`, `linear-accumulators-transient`, `linear-accumulators-fixes` |
| [CHR-Unified Type Constraints](completed/chr-unification.md) | `Type::TypeStageApp`; `NormCtxt` + `normalize()` unified simplification pass; FD elaboration into equality goals; deferred equality for non-injective resolvers; `[class ...]` two-bracket form with `determines:`/`resolver:`/`injective:`; `[instance ...]` match-arm syntax; scope-resident ClassEnv; arithmetic class migration to prelude; `process_deferred_equalities`; boundary guard elaboration | 2026-05-16 — `chr-module-split`, `chr-normalization`, `chr-class-instance`, `chr-prelude`, `chr-gaps`, `chr-instances-gaps`, `type-inference-cleanup`, `chr-corpus-fixes` |
| [Type Annotations v2](completed/type-annotations-v2.md) | `--- stage: type` sections; `%rust "type-core"` curated env; `type_to_dict`/`dict_to_type` (in type_normalize.rs); `bind:`/`kinds:`/`constraint:`/`return:`/`doc:` annotation keys; TypeVar scoping via `bind:`; `or`/`each`/`all`/`without` type-stage combinators; `is:` soft guard in match arms; `Annotation::Annotated` variant; positional union `@[T1 T2]` retired | 2026-05-14 — `constraint-annotations`, `fn-type-params`, `ctor-app`, `type-stage-infra`, `type-ann-v2-resolver`, `type-ann-v2-match`, `type-ann-v2-constraints` |

### Adopt Now

These proposals have no gating conditions and deliver standalone value at low cost.

**[Custom Call Aliases](call-aliases.md)** — `[timed f ...]` macro-defined call forms. Macros cluster is complete — prerequisite met.

**[Evaluation Semantics Verification](eval-semantics-verification.md) Phase 1 (partial)** — Confluence proof sketch to `doc/08-evaluation.md` is done; core proptest suite (200 lines + `proptest` dev-dep) still pending implementation.

### Wait for Trigger

These proposals have accepted designs but explicit gating conditions not yet met.

| Proposal | Gating Condition |
|----------|-----------------|
| [String Interning](string-interning.md) | `profiling.md` Criterion `bench_deep_scope` confirms `String` allocation/comparison is top-5 hotspot on real workloads |
| [Union-Find for Type Substitution](union-find-substitution.md) | `profiling.md` Criterion `bench_deep_scope` confirms average TypeVar chain depth ≥4 on real programs |
| [Value Serializer Visitor](value-serializer-visitor.md) | A third output format (YAML, TOML) is implemented and traversal duplication becomes maintenance burden |
| [Template-Polarity Embedding](template-polarity.md) | A real 90%+ static foreign-format file (nginx.conf, Dockerfile, Makefile) with ≤10 tinct substitutions where data-first is unreasonably awkward |
| [Macro System v2](completed/macros-v2.md) | **Completed 2026-05-27** — see Syntax and Ergonomics table above |
| [Evaluation Semantics Verification](eval-semantics-verification.md) Phase 2+ | Phase 1 proptest suite implemented with zero failures (doc proof sketch done; proptest pending) |

### Additive Capability (No TODO Replacement)

These proposals open new ground rather than closing existing work. All have accepted designs; adopt when the use case is ready.

| Proposal | Key Unlock |
|----------|-----------|
| [SQL Data Sources](lib-sql.md) | Lazy DB reads via `filter`/`map` predicate pushdown |
| [FFI and Native Module Extensions](ffi.md) | External C FFI + lazy builtin registry + workspace split — three complementary extension approaches |
| [Float Dict Keys](float-dict-keys.md) | `Key::Decimal` — Phase 1 (`Value::Decimal`) complete; Phase 2 (key extension) open |

---

## Dependency Graph

Reading order: each row depends on those above it in the same chain.

```text
### Typing cluster ✓ Complete (2026-05-07)
type-predicates ✓ ─── let-binding ✓ ─── pattern-matching ✓ ─── union-types ✓
                                              │                        │
                                         narrowing ✓           ADTs ✓ ─── nominal-variants ✓
                                                                │
                                       gradual-typing ✓ ─── type-classes ✓
                                       parameterized-type-aliases ✓
                                       structural-contracts ✓ ─── numeric-types ✓

### Macros cluster ✓ Complete
quasiquoting ✓ ─── macros ✓ ─── call-aliases (adopt now)
                              ─── macro-rewrite ✓ superseded (tmpl done — see DONE.md)
                              ─── parse-stage-macros

### I/O
io ✓ Complete ─── templating ✓ Complete ─── template-polarity
io ✓ Complete ─── lib-tls ✓ ─── lib-net-v2 ✓ (connect-v2, http-sessions, stdlib-protocols)

### Standard library
lib-supplemental ✓ ─── lib-regex ✓
linear-accumulators ✓

### Formal verification
eval-semantics-verification (Ph 1) ─── eval-semantics-verification (Ph 2+)

### Post typing-cluster type system research
union-types ✓ ─── boolean-algebraic-subtyping ✓ ─── record-map-split (parameterized-dict) ✓
                                               └─── error-patterns ✓ (nominal Result)
                                               └─── guardedness (Never propagation; requires runtime-v2)

### Type annotation chain
type-annotations-v2 (completed) ─── unified-bindings (accepted, in progress)

### Concurrency and stdlib chain (async-eval absorbed stdlib-architecture)
async-eval (async fn + Arc + OnceLock + multi-thread + task/channel/context/par/serve-layers)
  └─── dist-eval (cluster/remote-task/content-addressed-cache)
async-eval ─── lib-net-v2 ✓ (tcp-listen/quic-listen/tls-layer serve/connect layers)

### Profile-gated (no deps, waiting for profiling data)
profiling ─── string-interning, union-find-substitution

### No deps, can adopt now
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
