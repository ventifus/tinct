# Implementation Status

High-level guide to the current state of tinct. Updated 2026-05-01.
For completed sprint history see DONE.md. For future feature designs see `doc/whatif/`.

**All tracked implementation work is complete.** TODO.md has been fully evacuated to DONE.md.

---

## What's Complete

Every sprint from TODO.md has been implemented and moved to DONE.md. Summary of major milestones:

| Area | Status |
|------|--------|
| Parser (iterative, hand-written) | ✓ Complete — pest removed; iterative parser + lexer; AST formatter |
| Evaluator (iterative CEK machine) | ✓ Complete — defunctionalized CPS, `PendingCall`/`PendingBuiltin`, `Cont` variants |
| Lazy sequences | ✓ Complete — infinite sequences, `$map`/`$filter`/`$reduce` lazy via `PendingBuiltin` chains |
| Type system (HM + row polymorphism) | ✓ Complete — `TypeScheme`, kinded split, Rémy rows, level-based generalization, bidirectional typing |
| TypeAssert proxy contracts | ✓ Complete — `ThunkState::Guarded` with chaperone semantics, Strickland et al. 2012 |
| `$include` security hardening | ✓ Complete — cap-std fd-based open, BLAKE3/SHA3 hash verification, `--require-integrity`, `llt hash` |
| Sandboxing | ✓ Complete — Landlock ACLs, seccomp-bpf, rlimit caps, `--allow-path`, `--allow-network` |
| Error context enrichment | ✓ Complete — `$include` chain threading, secondary spans, source snippets (design), cycle paths (design) |
| Stdlib documentation | ✓ Complete — type annotations, inline assertion examples, all 51 functions documented |
| Performance foundations | ✓ Complete — SmallVec args/frames, Cow types throughout, HashMap substitutions, lazy fast-paths |
| Builtin strictness annotations | ✓ Complete — `BuiltinDef` + `Strictness { Id, Seq, Spine }`, W1 dispatch-time optimization |
| Fuzzing infrastructure | ✓ Complete — 3 libFuzzer targets (`parse`, `eval_source`, `typecheck_source`); `just fuzz <target>` |
| Test coverage | ✓ Complete — corpus tests, critical tests, framework enhancements, tooling tests |
| Integration pipeline | ✓ Complete — `ValueVisitor` trait, cross-layer contracts documented, `builtin!` macro |

---

## What's Next

All remaining work is optional features from `doc/whatif/`. No sprint items remain.

### Adopt Now (no prerequisites)

These features have complete designs and no gating conditions. Any can start immediately:

| Feature | Whatif doc | Effort |
|---------|-----------|--------|
| Type Predicates (`$int?`, `$str?`, etc.) | [type-predicates.md](doc/whatif/type-predicates.md) | ~1 sprint |
| String Interpolation Phase 1 (`i"..."`) | [string-interpolation.md](doc/whatif/string-interpolation.md) | ~1 sprint |
| `let` Binding Form | [let-binding.md](doc/whatif/let-binding.md) | ~1 sprint |
| Structural Contracts Phase 1 (`$$@Type`) | [structural-contracts.md](doc/whatif/structural-contracts.md) | ~1 sprint |
| ADTs Phase 1 (convention docs) | [algebraic-data-types.md](doc/whatif/algebraic-data-types.md) | ~1 sprint |
| Source Text Snippets Phase 1 | [source-text-availability.md](doc/whatif/source-text-availability.md) | ~1 sprint |
| Circular Dep Error Paths Phase 1 | [circular-dep-error-paths.md](doc/whatif/circular-dep-error-paths.md) | ~1 sprint |
| Eval Semantics Verification Phase 1 | [eval-semantics-verification.md](doc/whatif/eval-semantics-verification.md) | ~1 sprint |
| `$deep-eq` / `$shallow-eq` builtins | [typeclasses.md](doc/whatif/typeclasses.md) Phase 1 | ~1 sprint |
| Supplemental Stdlib Phase 1 | [lib-supplemental.md](doc/whatif/lib-supplemental.md) | ~1 sprint |

### Triggered (concrete condition required)

| Feature | Trigger |
|---------|---------|
| Type Classes Phase 2 (constrained vars) | After `Type::Any` → `Unknown`/`Top` split |
| Gradual Typing | Accept `doc/whatif/gradual-typing.md` via `/rnd accept` |
| Union Types | Nullable types or tagged unions needed in user code |
| Pattern Matching Phase 2+ | Type Predicates (Phase 1) complete |
| Arena Allocation + Flat Environments | After strictness-dispatch-w1 settles |
| String Interning | Profiling confirms dict key allocation is top-5 hotspot |
| Union-Find Substitution | Profiling confirms TypeVar chain depth ≥4 |

### Strategic

**Unified Syntax Reform** (`doc/whatif/new-syntax.md`) — Bare-word references + implied call + `%` pipeline naming. ~30–40% token reduction. Clean cutover (no user code). Requires parser-rewrite Phase 3 (AST formatter) ✓. Adopt as a deliberate project milestone.

---

## Architecture Reference

- **Pipeline:** Source → Parser (`src/parser.rs` + `src/lexer.rs`) → Desugar (`src/desugar.rs`) → TypeCheck (`src/typecheck.rs`) → Evaluator (`src/eval*.rs`) → Serializer (`src/lib.rs`)
- **Key invariants:** All thunks carry `Thunk.span` (creation span). `EvalError` carries `secondary_span: Option<(Span, String)>`. `BuiltinDef` carries `pos_strictness: &'static [Strictness]`. `ThunkState` is monotonic (no backwards transitions).
- **Security:** `--no-fs` (LSP default), `--timeout` (SIGALRM), Landlock (Linux), seccomp-bpf (Linux), rlimit caps, `--require-integrity`, `cargo audit` CI gate.
- **Fuzzing:** `just fuzz parse|eval_source|typecheck_source [seconds]` — requires nightly Rust.
- **Full design history:** `doc/whatif/` (35 proposal docs), `doc/*.md` (formal spec chapters), `DONE.md` (completed sprint archive).
