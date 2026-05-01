# Implementation Status

High-level guide to what's done, in progress, and not yet started. Updated 2026-05-01.
For granular task tracking see TODO.md. For completed work see DONE.md.

---

## Not Started

These are substantial features with zero implementation despite having complete designs.

### Sandboxing (`sandbox`)

Design: complete — see `doc/12-tooling.md §Sandboxing & Security`.
Implementation: zero. ~13 items open.

What it delivers: untrusted tinct program execution via filesystem allowlist
(`--allow-path`), Landlock ACLs, seccomp-bpf network/process isolation, rlimit
resource caps. Prerequisite for the `doc/whatif/io.md` capability model.

Note: `EvalContext` refactor (prerequisite for `EvalConfig::allowed_paths`) is already
complete. `--no-fs` and `--timeout` adversarial eval flags are also complete
(`eval-sandbox-flags`, `sandbox-polish-a/b/c`). Only the Landlock/seccomp/rlimit and
allowlist-check implementation remain.

---

## In Progress

These features have substantial work done but meaningful gaps remain.

### Row Unification Performance (`row-unification-perf`, `perf-foundations`)

~20 open items. All are allocation reductions and fast-paths; none affect correctness.

Biggest remaining wins:
- `IndexMap→HashMap` for `Substitution` and `TypeEnv.bindings` (~20% lookup savings)
- AST clone elimination — `Expr::Fn.body`, `CallArg`, `eval_dict` entry body all
  deep-clone AST subtrees on every call/iteration (three `Rc<Spanned<Expr>>` migration sites)
- `ThunkState::PendingBuiltin.named` — `IndexMap::new()` allocated at ~30 sequence step
  sites; change to `Option<IndexMap<...>>` to skip allocation when no named args
- `builtin_map` Dict path `format!()` per entry — `Cow::Owned(format!(...))` on every
  mapped element (Critical)
- `EvalContext::with_base_dir()` fresh `Rc<EvalConfig>` per `$include` (Critical)
- `unify_rows` closed-row fast-path — skips 5+ allocations on dominant path (Major)
- Per-variable depth limit in `Substitution::apply()` — currently only prevents
  infinite TypeVar chains; structural depth limit still missing (Critical)

Note: `row-unification-perf-b` and `perf-c` sprints are complete; the remaining items
live in `perf-foundations` and require no further design work.

### Type System Completeness (`type-extensions`, `theoretical-foundations`)

Most code fixes are done. Remaining open items:

| Item | Impact |
|------|--------|
| Named argument type-checking | Named arg value types inferred but not checked against param types; full fix requires `Type::Function` to carry `params: Vec<(String, Type)>` |
| `Substitution::apply()` depth counter conflatation | Conflates chain depth with structural width; silent wrong type for records with >256 fields |
| `check_call` CALL-POLY `unify()` re-application | `unify()` re-applies subst only once; confirm Robinson invariant holds |
| `doc/06-type-inference.md:531` stale | `collect_type_vars()` signature claim still mentions wrong BTreeSet element type |
| Gradual typing | Deferred — major research project, tracked in `doc/whatif/gradual-typing.md` |
| Type class constraints | Deferred — tracked in `doc/whatif/typeclasses.md` |

Previously open items now resolved: `TypeEnv::with_builtins()`, `Type::Seq` inference
stubs, variadic param typed as `Any`, TypeVar annotation aliasing, TypeVar-to-Any level
zeroing, `Type::Error` sentinel, type alias shadowing, `TypeScheme` kinded split, and
all related doc/06–07 stale content fixes.

### Parser Error Recovery (`parser-error-recovery`)

Phase 4 of the parser rewrite. Two items open, both effectively deferred:

- Error token insertion on bracket-level errors (`Expr::Error(Span)` AST node is ready;
  token-skipping parser refactor is significant work)
- Multi-error collection per file (requires `ParseOutput` error list + caller updates)

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
  formatter). Phase 4 (error recovery) has 2 open deferred items (see In Progress above).

- **TypeAssert Structural (Parts 1 & 2)** — Both parts complete. Proxy contracts
  implemented with chaperone semantics (Strickland et al. 2012). Critical bugs fixed:
  all `ThunkState::Guarded` failure paths now call `decorate()`; `ThunkState::Guarded`
  added to formal spec in `doc/08-evaluation.md` (7-state model, `[FORCE-GUARD]` rule).
  LSP double-typecheck panic fixed. Elaboration gap closed. Full corpus test coverage added.

- **Row unification a–h** — Algorithm complete through sprint h. All correctness gaps
  resolved: `check_call` TypeVar arm for letrec forward refs, `ann_mapping` cross-kind
  collision, Pass 3b `or_insert` discard, bracket access now generates row constraints.
  Perf sprints (a–c) complete; remaining allocation reductions tracked in perf-foundations.

- **Float NaN/Infinity Invariant** — Complete. All five arithmetic result sites (`$+`,
  `$-`, `$*`, `$/` float paths and `$from-json` number parser) reject NaN/Infinity via
  shared `check_float_result()` helper. "All floats are finite" invariant enforced.

- **Sequence Correctness Gaps** — Complete (`seq-resource-safety`, `eval-lazy-fixes`).
  `$filter` seq-step depth accumulation fixed (internal loop). `$take` and
  `filter_dict_step` depth correctly incremented. `TypeAssert` premature materialization
  gated. `$filter` dict path O(n) clone fixed. `$filter` empty Dict correct (not a bug).

- **include-fd-hardening** — Complete. `cap-std` adopted; three-path TOCTOU replaced
  with single fd-based flow; `include_guard`/`include_cache` keys use `(dev, ino)` pairs;
  file-type guards reject FIFOs and device nodes. Hash integrity checking (`blake3`,
  `sha3-256`, `sha3-512`, `sha256`) with `--require-integrity` flag and `llt hash`
  subcommand complete.

- **Underscore desugaring** — `src/desugar.rs` complete.
- **Bidirectional typing** — `check_expr` implemented; all correctness gaps resolved.

---

## Ongoing (not blocking)

These areas have continuous work but are not blocking major feature completion.

**Stdlib Expansion** — Several convenience functions still open: `zip-with`,
`map-indexed`, `map-keys`, `sort-on`, `flatten-all`, `range-step`, `$has?` Rust
primitive, `substr`/`slice-str`, `starts-with?`, `ends-with?`, `chars`, runtime
assertion guards. Most one-liners over existing primitives.

**Error Context Enrichment** — `$include` chain threading (nested include path in errors),
secondary span support ("evaluated to this" labels), cycle path reconstruction, source
snippets with carets (rustc-style), REPL span-aware recovery still open.

**API Hygiene** — `eval↔builtins` circular dependency (extract `eval_core.rs` interface
or trait objects), builtin registration macro to prevent name/arity drift, `Type::Any`
split into `AnyGradual`/`AnyPoly` for gradual typing correctness, `cargo audit` CI gate.

**Performance Foundations** (`perf-foundations`) — Arena allocation, flat environments,
string interning, AST `Rc<Spanned<Expr>>` migration, SmallVec for args/frames. All
benefit from the now-complete iterative evaluator as a natural prerequisite for
environment reuse and arena scoping.

**Test Infrastructure** — Fuzzing targets (`fuzz/`), property-based testing (proptest),
benchmarks (criterion), LSP corpus tests, per-file test functions (Nickel
`test_resources!` pattern), insta snapshot tests, pretty-print round-trip idempotence,
error corpus span assertions, `$_` edge-case corpus, include caching corpus.

**Documentation Divergences** — Various doc/code mismatches identified by specialist
agents. None are correctness-critical.

---

## Suggested Completion Order

Based on dependencies and unblocking value:

1. **Sandboxing** (~13 items) — closes the largest remaining security gap; `EvalContext`
   refactor and `--no-fs`/`--timeout` flags are already done, so the Landlock/seccomp/
   rlimit implementation is self-contained
2. **perf-foundations** — `IndexMap→HashMap` for `Substitution`, AST `Rc<Spanned<Expr>>`
   migration (three sites), `PendingBuiltin.named` Option, `builtin_map` static label,
   `unify_rows` closed-row fast-path; all high-value, no design work needed
3. **Type system remaining** — `Substitution::apply()` depth counter fix (Critical),
   named arg type-checking (requires `Type::Function` param-name extension)
4. **Test infrastructure** — fuzzing targets, LSP corpus, property tests (proptest);
   improves regression safety for ongoing work
5. **Stdlib expansion** — remaining convenience functions; `$has?` Rust primitive unblocks
   lazy has-checking
6. **Error context enrichment** — include chain threading and source snippets; improves
   debug experience substantially

### Whatif "Adopt Now" Items are Independent

The `doc/whatif/index.md §Adopt Now` items (Type Predicates, String Interpolation Phase 1,
Let Binding, Structural Contracts Phase 1, ADTs Phase 1 convention) do not depend on any
of the major incomplete features above. They can be implemented in any order, in parallel
with or after steps 1–4.
