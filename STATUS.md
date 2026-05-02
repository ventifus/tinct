# Implementation Status

High-level guide to what's done, in progress, and not yet started. Updated 2026-05-01.
For granular task tracking see TODO.md. For completed work see DONE.md.

**Current TODO.md:** 66 unchecked items across ~10 sprints.

---

## Not Started

These are substantial features with zero implementation despite having complete designs.

### Sandboxing (`sandbox`)

Design: complete — see `doc/12-tooling.md §Sandboxing & Security`.
Implementation: zero. ~13 items open across `sandbox-b` (Landlock) and `sandbox-c` (seccomp/rlimit).

What it delivers: untrusted tinct program execution via filesystem allowlist
(`--allow-path`), Landlock ACLs, seccomp-bpf network/process isolation, rlimit
resource caps. Prerequisite for the `doc/whatif/io.md` capability model.

Note: `EvalContext` refactor, `--no-fs`, `--timeout`, and file integrity hashing
(`blake3`, `sha3`, `--require-integrity`, `llt hash`) are all complete. Only the
Landlock/seccomp/rlimit and allowlist-check implementation remain.

### Builtin Strictness Annotations (`strictness-types`, `strictness-value-migration`, `strictness-dispatch-w1`)

Design: complete — see `doc/16-architecture.md §Builtin Argument Strictness Annotations`.
Implementation: zero. Three sequential sprints (24 items total).

What it delivers: `BuiltinDef` struct replaces bare `BuiltinFn` everywhere, carrying
`Strictness { Id, Seq, Spine }` (Wadler & Hughes 1987 projections) per argument position.
W1 dispatch-time optimization pre-materializes strict args, eliminating redundant
`materialize()` calls on the hot path for arithmetic, comparison, and string builtins.

---

## In Progress

These features have substantial work done but meaningful gaps remain.

### Performance Foundations (`perf-foundations`, `perf-ast-rc`)

~30 open items. Allocation reductions and fast-paths; none affect correctness.

Biggest remaining wins:
- `IndexMap→HashMap` for `Substitution` and `TypeEnv.bindings` (~20% lookup savings)
- AST clone elimination — `Expr::Fn.body`, `CallArg`, `eval_dict` entry body all
  deep-clone AST subtrees on every call/iteration (three `Rc<Spanned<Expr>>` migration sites
  in `perf-ast-rc` sprint)
- `ThunkState::PendingBuiltin.named` — `Option<IndexMap<...>>` to skip allocation for the
  ~30 sequence step sites that always pass an empty named-args map
- `builtin_map` Dict path `format!()` per entry — `Cow::Owned(format!(...))` on every
  mapped element (Critical)
- `EvalContext::with_base_dir()` fresh `Rc<EvalConfig>` per `$include` (Critical)
- `resolve_row` returning `Cow<'_, Row>` — eliminates 2×O(n) clones per `unify_rows` call
- `BTreeSet` in level-lowering — 4 allocs + 4 tree walks per TypeVar-to-complex-type binding
- Builtin strictness W1 (see above; new sprint group)

### Type System Completeness (`type-extensions`)

One design item and named-arg type-checking remain open:

| Item | Impact |
|------|--------|
| Named argument type-checking | Named arg value types inferred but not checked against param types; full fix requires `Type::Function` to carry `params: Vec<(String, Type)>` |
| `Type::Any` split | Standalone sprint: rename to `Type::Unknown` (gradual) + `Type::Top` (true supertype ⊤); prerequisites met; no observable behavior change |

Previously open items now resolved: `TypeEnv::with_builtins()`, `Type::Seq` inference
stubs, `type-seq` sprint (all 34 items), variadic param typed as `Any`, TypeVar annotation
aliasing, `Type::Error` sentinel, type alias shadowing, `TypeScheme` kinded split, and
all related doc/06–07 stale content fixes.

### Error Context Enrichment (`error-context`, `error-ux`)

~8 open items. The `error-context-include-chain` sprint is complete (include chain
threading implemented). Remaining:

- Populate `secondary_span` at three eval sites (Guarded validation, builtin require_*, `$if` condition)
- `render_span_snippet` source snippets in REPL and CLI output
- Circular dependency multi-hop cycle path reconstruction
- Source snippet support in LSP `related_information`
- Cycle path reconstruction (full A→B→A chain from `ThunkState::InProgress`)

### Test Infrastructure (`test-tooling`)

Fuzzing infrastructure is now complete (`fuzz/` directory with 3 targets: `parse`,
`eval_source`, `typecheck_source`; run with `just fuzz <target>`). Remaining:

- Property-based testing (proptest) for parser round-trip and evaluator commutativity
- Benchmarks via criterion crate

---

## Mostly Done

Core algorithms are in place. Remaining items are doc fixes, nits, and minor edge cases.

- **Iterative Evaluator** (`iterative-eval`) — Sprints a through b5 and d complete.
  The 64MB worker thread is removed. `materialize()` is fully iterative (work-stack).
  `eval_call` emits `PendingCall` thunks (no synchronous recursion). Access chains are
  iterative via `DotAccessForce`/`BracketForceTarget` continuations. `TypeAssert` uses
  `Cont::TypeAssertCheck` deferred in CEK loop. `$apply` lazy via `PendingBuiltin`.
  `deep_materialize_impl` is iterative via `DeepEntries`/`DeepSeqTail`. Remaining:
  `eval_step()` still delegates to `eval_recursive` for most `Expr` variants (stub only);
  `DictEntries`/`DocumentPipeline`/`DictBuildKey`/`BindArgDefault` Cont variants deferred.

- **Parser Rewrite E2** — Phase 1–3 complete (production iterative parser + AST-based
  formatter). Phase 4 (error recovery) sprint is complete (`Expr::Error(Span)` AST node
  + formatter verbatim rendering). Further Phase 4 work (token-skipping error recovery,
  multi-error collection per file) is deferred — tracked in doc but no open sprint items.

- **TypeAssert Structural (Parts 1 & 2)** — Both parts complete. Proxy contracts
  implemented with chaperone semantics (Strickland et al. 2012). `ThunkState::Guarded`
  in formal spec in `doc/08-evaluation.md` (7-state model, `[FORCE-GUARD]` rule).

- **Row Unification a–h** — Algorithm and all sprints complete through sprint h.
  All correctness gaps resolved. Perf sprints (a–c) complete; remaining allocation
  reductions tracked in `perf-foundations`.

- **Security: $include Hardening** (`file-sandbox-security`) — Complete. `cap-std`
  adopted; TOCTOU fixed (fd-based single-open flow); `(dev, ino)` cache keys; file-type
  guards; hash integrity (`blake3`, `sha3-256/512`, `sha256`) with `--require-integrity`
  flag and `llt hash` subcommand; cargo-fuzz targets; `cargo audit` CI gate.

- **Float NaN/Infinity Invariant** — Complete. All five arithmetic result sites reject
  NaN/Infinity via shared `check_float_result()` helper.

- **Sequence Correctness Gaps** — Complete (`seq-resource-safety`, `eval-lazy-fixes`).

- **include-fd-hardening** — Complete (part of file-sandbox-security above).

- **Underscore desugaring** — `src/desugar.rs` complete.

- **Bidirectional typing** — `check_expr` implemented; all correctness gaps resolved.

- **Type System Core** (`type-seq`) — All 34 items complete. Gradual typing and type
  class phases deferred to their whatif acceptance.

- **Stdlib Documentation** (`stdlib-docs`) — All 51 items complete. Type annotations,
  inline assertion examples, and doc/11-stdlib.md accuracy fixes all done.

- **Theoretical Foundations** — Proof obligations complete. Bisimulation proof sketch,
  confluence argument, and `doc/proofs/` directory all added.

---

## Ongoing (not blocking)

These areas have continuous work but are not blocking major feature completion.

**Stdlib Expansion** — Several convenience functions still open: `zip-with`,
`map-indexed`, `map-keys`, `sort-on`, `flatten-all`, `range-step`, `$has?` Rust
primitive, `substr`/`slice-str`, `starts-with?`, `ends-with?`, `chars`, runtime
assertion guards. Also pending: `$deep-eq`/`$shallow-eq` builtins (type class Phase 1
decision; no prerequisites).

**API Hygiene** — `eval↔builtins` circular dependency (extract `eval_core.rs`),
`Type::Any` split into `Type::Unknown` + `Type::Top` (standalone sprint; prerequisites
met), `cargo audit` CI gate (already implemented).

**Performance Foundations** (`perf-foundations`) — Arena allocation, flat environments,
string interning, AST `Rc<Spanned<Expr>>` migration, SmallVec for args/frames. All
benefit from the now-complete iterative evaluator as a natural prerequisite for
environment reuse and arena scoping.

**Test Infrastructure** — Property-based testing (proptest for eval bisimulation per
`doc/whatif/eval-semantics-verification.md`), benchmarks (criterion). Fuzzing is done.

**Integration** — eval↔builtins boundary audit, value serializer visitor pattern, `cargo
audit` CI gate, `Type::AnyGradual`/`AnyPoly` split (now `Unknown`/`Top`).

**Documentation Divergences** — Various doc/code mismatches identified by specialist
agents. None are correctness-critical.

---

## Suggested Completion Order

Based on dependencies and unblocking value:

1. **Builtin Strictness Annotations** (~24 items, 3 sprints) — no dependencies, pure
   performance win on the evaluator hot path; sets up W2 (call-creation time
   optimization) and the arena migration
2. **`Type::Any` split** (~8 items, standalone sprint) — prerequisites met; no observable
   behavior change; unblocks type class Phase 2 (constrained type variables)
3. **perf-foundations** — `IndexMap→HashMap` for `Substitution`, AST `Rc<Spanned<Expr>>`
   migration (3 sites in `perf-ast-rc`), `PendingBuiltin.named` Option, `builtin_map`
   static label, `resolve_row` Cow; all high-value, no design work needed
4. **Sandboxing** (~13 items) — closes the largest remaining security gap; self-contained
   after `EvalContext`/`--no-fs`/`--timeout` are done
5. **Type system remaining** — named arg type-checking (requires `Type::Function` param
   names); `Substitution::apply()` depth counter fix
6. **Error context enrichment** — secondary span, source snippets, cycle path reconstruction
7. **`$deep-eq`/`$shallow-eq`** builtins (type class Phase 1) — no prerequisites
8. **Test infrastructure** — proptest bisimulation suite, criterion benchmarks

### Whatif "Adopt Now" Items are Independent

The `doc/whatif/index.md §Adopt Now` items (Type Predicates, String Interpolation Phase 1,
Let Binding, Structural Contracts Phase 1, ADTs Phase 1 convention, Source Text snippets,
Circular Dep error paths, Eval Semantics Verification Phase 1) do not depend on any of the
major incomplete features above. They can be implemented in any order, in parallel with
or after steps 1–4.
