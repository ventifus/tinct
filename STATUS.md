# Implementation Status

High-level guide to the current state of tinct. Updated 2026-05-16.
For completed sprint history see DONE.md. For future feature designs see `doc/whatif/`.

**The core language, type system, and I/O stack are complete.** A small number of enhancement items (rich diagnostics, `cap-file` validation, stdlib reorganization) remain. See "What's Next" below.

---

## What's Complete

Every sprint from TODO.md has been implemented and moved to DONE.md. Summary of major milestones:

| Area | Status |
|------|--------|
| Parser (iterative, hand-written) | ✓ Complete — pest removed; iterative parser + lexer; AST formatter |
| Evaluator (iterative CEK machine) | ✓ Complete — defunctionalized CPS, `PendingCall`/`PendingBuiltin`, `Cont` variants |
| Sequential strict bindings | ✓ Complete — fn-body + document-level named bindings are eager (strict `let*`); `MAX_EVAL_DEPTH` removed |
| Lazy sequences | ✓ Complete — infinite sequences, `$map`/`$filter`/`$reduce` lazy via `PendingBuiltin` chains |
| Generator pipelines | ✓ Complete — `\|` pipe desugar, `each`/`each-key`/`each-kv` builtins, `collect-kv`, access pipeline |
| Type system (HM + row polymorphism) | ✓ Complete — `TypeScheme`, kinded split, Rémy rows, level-based generalization, bidirectional typing |
| TypeAssert proxy contracts | ✓ Complete — `ThunkState::Guarded` with chaperone semantics, Strickland et al. 2012 |
| Typing cluster — Phase A (foundations) | ✓ Complete — `let` binding (A1), `[match]` with type/literal patterns (A2), dict/seq destructuring + path-key (A3), guards + or-patterns (C4) |
| Typing cluster — Phase B (type primitives) | ✓ Complete — `Type::Union` annotation-only unions (B1), `Unknown`/`Top` `Any` split (B2), parameterized type aliases (B3), constrained type variables (B4), path-sensitive narrowing (B5a/B5b) |
| Typing cluster — Phase C (algebraic types) | ✓ Complete — multi-entry `[type ...]` ADT declarations (C1), `Value::Variant` unit constructors (C2), payload constructors + `Pattern::Constructor` (C3), Maranget exhaustiveness checking (C5) |
| Typing cluster — Phase D (advanced typing) | ✓ Complete — full type classes with dictionary passing (D1), Simple-sub algebraic subtyping (D2), recursive ADTs (D3), blame tracking (D4), range/Decimal/BigInt/repr: numeric types (D5) |
| Structural contracts | ✓ Complete — `%@Type` pipeline input annotation (SC1), `validate` builtin (SC2), `tinct describe` CLI (SC3), pipeline blame (SC4) |
| Object capability model | ✓ Complete — `dir-cap`/`net-cap` builtins removed; all caps flow from CLI (`--cap-fs`, `--cap-net`, `--cap-file`) or runtime injection (`%cwd`, `%libdir`, `%stdin`); `%` prefix convention |
| `---` header pragmas | ✓ Complete — `%name@Type`, `expects:`, `caps:` on document separators; type checker and runtime validation |
| `caps:` pragma type-checker awareness | ✓ Complete — cap-qualified `[include %libdir "path"]` understood by type checker; `%cwd`/`%libdir`/`%stdin` seeded in `TypeEnv` |
| TLS / HTTPS networking | ✓ Complete — `tls-layer` (Handle upgrade), CA roots (system + Mozilla + custom bundle), mTLS, ALPN, SPKI pinning |
| Composable networking v2 | ✓ Complete — transport-generic `connect` (Tcp/Udp/UnixStream/UnixDatagram/Icmp), `tls-layer`, QUIC sessions (`quic-session`), HTTP/3 (`http3-session`), HTTP/2 via reqwest (`http2-session`), `http-request` builtin; `protocols/` subdirectory (DNS, WebSocket, SOCKS5, gRPC) |
| Boolean-Algebraic Subtyping | ✓ Complete — `Union`/`Intersection`/`Negation`/`Never` type algebra; S-RcdTop, S-ClsBot; RDNF simplification; BAS negation narrowing; Rémy row variables removed |
| Nominal Result type | ✓ Complete — `Ok[T] | Error[String]` via `[type [Ok a] [Error String]]`; `try` returns nominal variants; `and-then`/`result-or`/`result-map` combinators; `Ok` serves as `pure`; `[do result ...]` monad dict |
| Record/Map type split | ✓ Complete — `Record` (known-field structural) vs `Map[K V]` (homogeneous); `Dict = Record ∨ Map` BAS union; `get?` (returns `V | Null`); `record?`/`map?` predicates; order-insensitive structural dict equality with cycle detection |
| Macros | ✓ Complete — `[defmacro]`, quasiquoting `[quote]`/`[unquote]`, string interpolation `i"..."` via `[defmacro tmpl]`, macro hygiene |
| Constraint annotations | ✓ Complete — `fn@[return: T  constraint: [a: Comparable]  doc: "..."  bind: [a]  kinds: [a: Operator]]` named-key metadata dict; `fn@Type` shorthand permanent |
| Parameterized annotations | ✓ Complete — `@Seq@Int`, `@Map@[K: V]`, `@Map@[key: K  value: V]`, `@Record@[field: Type ...]`, `@Record` (bare); chained `@` via `Annotation::Annotated` AST node |
| Inference completeness | ✓ Complete — SCC-based DICT-GEN (Tarjan + topo sort); nested dict polymorphism via `TypeScheme.inner_schemes`; variadic params typed as `Seq(T)` with call-site unification |
| Row-level constraint propagation | ✓ Complete — `[CONSTRAIN-FIELD]`/`[CONSTRAIN-INTER]`/`[CONSTRAIN-UNION]`/`[CONSTRAIN-NEVER]`/`[CONSTRAIN-TOP]` in `satisfies_constraint`; `Unknown` already done |
| Multi-parameter type classes | ✓ Complete — `Add a b c \| (a,b)→c` MPTCs with functional dependencies; `Add`/`Sub`/`Mul`/`Div` class hierarchy; arithmetic builtins re-typed; coherence check on duplicate instances |
| Higher-Kinded Types + `[do]` | ✓ Complete — `Kind::Operator` (`* → *`), `Type::App`; Functor/Applicative/Monad/Foldable/Traversable/Mappable/Appendable typeclass hierarchy; `Maybe` ADT; `[do monad ...]` explicit form; `[do ...]` inferred form from return annotation or first binding |
| Runtime typeclass dispatch | ✓ Complete — `RuntimeInstanceRegistry` in `EvalState`; `builtin_eq`/`builtin_lt`/`builtin_str`/arithmetic dispatch through user-defined instances before Rust fallback |
| Runtime reflection | ✓ Complete — `FnAnnotation { doc, return_ann, constraints, source_file, source_span }` on `Value::Function`; `ast-of` primitive; `describe`/`sig-from-ast`/`annotation-of`/`source-of` in prelude |
| Type stage | ✓ Complete — `--- stage: type` sections; `%rust "type-core"` (28 builtins); type dict schema (`[kind: "named" name: "Int"]` etc.); `type_to_dict`/`dict_to_type` round-trip; prelude `--- stage: type` section with `Int`/`Str`/`Bool`/`Float`/`Seq`/`Map`/`union`/`all` constructors |
| Supplemental stdlib | ✓ Complete — `strings.llt`, `math.llt`, `encoding.llt`, `datetime.llt`, `regex.llt`, `net.llt`, `toml-lite.llt`, `path.llt`, `io.llt`, `numeric.llt`, `macros.llt`, `protocols/`; require explicit `[include libdir "name.llt"]` |
| Stdlib boundary | ✓ Complete — `str-index-of` (O(n) substring search), `str-map-chars`, `regex-match?`, `trim-start`/`trim-end` Rust primitives; `str-contains?`/`starts-with?`/`ends-with?`/`upper`/`lower`/`copy`/`has-cap?` migrated to tinct; `builtin-sort` accepts optional comparator |
| Rich diagnostics | ✓ Partial — `T001`–`T004` error codes, Rust-style source snippets for type errors, `tinct explain T001`; parse error snippets and `= help:` suggestions still pending |
| `$include` security hardening | ✓ Complete — cap-std fd-based open, BLAKE3/SHA3 hash verification, `--require-integrity`, `llt hash` |
| Sandboxing | ✓ Complete — Landlock ACLs, seccomp-bpf, rlimit caps, `--allow-path`, `--allow-network` |
| Performance foundations | ✓ Complete — SmallVec args/frames, Cow types throughout, HashMap substitutions, lazy fast-paths |
| Builtin strictness annotations | ✓ Complete — `BuiltinDef` + `Strictness { Id, Seq, Spine }`, W1 dispatch-time optimization |
| Fuzzing infrastructure | ✓ Complete — 3 libFuzzer targets (`parse`, `eval_source`, `typecheck_source`); `just fuzz <target>` |
| Test coverage | ✓ Complete — corpus tests, critical tests, framework enhancements, tooling tests |
| Integration pipeline | ✓ Complete — `ValueVisitor` trait, cross-layer contracts documented, `builtin!` macro |

---

## What's Next

### Open TODO Items

| Item | Status |
|------|--------|
| `health-diagnostic-followup` — convert `eprintln!` ambiguous-constraint warnings to structured diagnostics; fix false-positive from discharged vars; aggregate multi-var HasField warning | Open (3 minor items, `src/type_env.rs`) |
| `rich-diagnostics` — parse error snippets, `= help:` suggestions, output header update | Partial — type error snippets done; parse errors and `= help:` still pending |
| `cap-file` — `--cap-file name=path:mode` single-file Handle injection | Partial — CLI parsing + Handle construction done; `--- caps:` `@Handle` validation pending |

### Adopt Now (no prerequisites)

| Feature | Whatif doc | Effort |
|---------|-----------|--------|
| Eval Semantics Verification Phase 1 (proptest suite) | [eval-semantics-verification.md](doc/whatif/eval-semantics-verification.md) | ~1 sprint |
| Float Dict Keys (Phase 2: `Key::Decimal`) | [float-dict-keys.md](doc/whatif/float-dict-keys.md) | ~1 sprint |
| Custom Call Aliases | [call-aliases.md](doc/whatif/call-aliases.md) | ~1 sprint |
| CHR-unified type constraints | [chr-unification.md](doc/whatif/chr-unification.md) | ~2 sprints — requires `type-stage-infra` + `typeclass-mptc-fundeps` (both done) |
| Isorecursive types (μ-types) | [isorecursive-types.md](doc/whatif/isorecursive-types.md) | ~2 sprints — requires `type-stage-infra` (done) |

### Wait for Trigger

| Feature | Trigger |
|---------|---------|
| String Interning | Profiling confirms dict key allocation is top-5 hotspot |
| Union-Find Substitution | Profiling confirms TypeVar chain depth ≥4 — path compression already in place |
| Value Serializer Visitor | A third output format (YAML, TOML) is needed |
| Template-Polarity Embedding | Real 90%+ static foreign-format file use case |

### Strategic

| Feature | Whatif doc | Notes |
|---------|-----------|-------|
| `builtin-*` privacy | [builtin-privacy.md](doc/whatif/builtin-privacy.md) | Restrict `builtin-*` aliases to prelude; type-checker warning for non-prelude callers |
| Stdlib reorganization | `doc/whatif/runtime-reflection.md §Stdlib Reorganization` | Move `stdlib/in/` → `stdlib/cli/in/`, `stdlib/out/` → `stdlib/cli/out/`; typed `[include]` return |

---

## Architecture Reference

- **Pipeline:** Source → Parser (`src/parser.rs` + `src/lexer.rs`) → Expand (`src/expand.rs`) → Desugar (`src/desugar.rs`) → TypeCheck (`src/typecheck.rs` + `src/type_env.rs`) → Evaluator (`src/eval*.rs`) → Serializer (`src/lib.rs`)
- **Evaluator split:** `src/eval.rs` (core), `src/eval_call.rs` (function calls), `src/eval_materialize.rs` (forcing), `src/eval_access.rs` (dot/bracket), `src/eval_dict.rs` (dict construction), `src/eval_pipeline.rs` (document pipeline), `src/eval_deep.rs` (deep materialize)
- **Builtins split:** `src/builtins.rs` (registry + core), `src/builtins_io.rs` (I/O, connect, TLS), `src/builtins_math.rs`, `src/builtins_string.rs`, `src/builtins_meta.rs`, `src/builtins_bytes.rs`, `src/builtins_uri.rs`, `src/builtins_datetime.rs`, `src/builtins_dict.rs`, `src/builtins_seq_prim.rs`, `src/builtins_seq_xform.rs`, `src/builtins_seq_gen.rs`, `src/builtins_seq_reduce.rs`
- **Key invariants:** All thunks carry `Thunk.span` (creation span). `EvalError` carries `secondary_span: Option<(Span, String)>`. `BuiltinDef` carries `pos_strictness: &'static [Strictness]`. `ThunkState` is monotonic (no backwards transitions). Sequential named bindings are forced to WHNF at bind time.
- **Capabilities:** All caps flow from CLI (`--cap-fs`, `--cap-net`, `--cap-file`) or runtime injection (`%cwd`, `%libdir`, `%stdin`). `%` prefix is added by tinct; user-supplied names have no prefix.
- **Security:** `--no-fs` (LSP default), `--timeout` (SIGALRM), Landlock (Linux), seccomp-bpf (Linux), rlimit caps, `--require-integrity`, `cargo audit` CI gate.
- **Fuzzing:** `just fuzz parse|eval_source|typecheck_source [seconds]` — requires nightly Rust.
- **Full design history:** `doc/whatif/` (proposal docs + `completed/` archive), `doc/*.md` (formal spec chapters), `DONE.md` (completed sprint archive).
