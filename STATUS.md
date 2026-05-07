# Implementation Status

High-level guide to the current state of tinct. Updated 2026-05-07.
For completed sprint history see DONE.md. For future feature designs see `doc/whatif/`.

**All tracked implementation work is complete.** TODO.md has been fully evacuated to DONE.md. The typing cluster (all 14 sprints across phases A–D) is fully implemented.

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
| Typing cluster — Phase A (foundations) | ✓ Complete — `let` binding (A1), `[match]` with type/literal patterns (A2), dict/seq destructuring + path-key (A3), guards + or-patterns (C4) |
| Typing cluster — Phase B (type primitives) | ✓ Complete — `Type::Union` annotation-only unions (B1), `Unknown`/`Top` `Any` split (B2), parameterized type aliases (B3), constrained type variables (B4), path-sensitive narrowing (B5a/B5b) |
| Typing cluster — Phase C (algebraic types) | ✓ Complete — multi-entry `[type ...]` ADT declarations (C1), `Value::Variant` unit constructors (C2), payload constructors + `Pattern::Constructor` (C3), Maranget exhaustiveness checking (C5) |
| Typing cluster — Phase D (advanced typing) | ✓ Complete — full type classes with dictionary passing (D1), Simple-sub algebraic subtyping (D2), recursive ADTs (D3), blame tracking (D4), range/Decimal/BigInt/repr: numeric types (D5) |
| Structural contracts | ✓ Complete — `%@Type` pipeline input annotation (SC1), `validate` builtin (SC2), `tinct describe` CLI (SC3), pipeline blame (SC4) |
| Access pipeline | ✓ Complete — `\|` desugar pipe, `DotKey::Int` for `list.0`, `get`/`each`/`collect-kv` builtins |
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

All tracked implementation work is complete. No sprint items remain.

### Adopt Now (no prerequisites)

| Feature | Whatif doc | Effort |
|---------|-----------|--------|
| Eval Semantics Verification Phase 1 (proptest suite) | [eval-semantics-verification.md](doc/whatif/eval-semantics-verification.md) | ~1 sprint |
| Supplemental Stdlib Phase 1 | [lib-supplemental.md](doc/whatif/lib-supplemental.md) | ~1 sprint |
| Float Dict Keys (Decimal prerequisite now met) | [float-dict-keys.md](doc/whatif/float-dict-keys.md) | ~1 sprint |

### Wait for Trigger

| Feature | Trigger |
|---------|---------|
| String Interning | Profiling confirms dict key allocation is top-5 hotspot |
| Union-Find Substitution | Profiling confirms TypeVar chain depth ≥4 |
| eval↔builtins Boundary | Independent builtin testing is a concrete need |
| Value Serializer Visitor | A third output format (YAML, TOML) is needed |
| Pure-Tinct Regex Engine | lib-supplemental Phases 1 + 3 complete |
| Template-Polarity Embedding | Real 90%+ static foreign-format file use case |

### Strategic

**Boolean-Algebraic Subtyping** (`doc/whatif/boolean-algebraic-subtyping.md`) — Replace Rémy row variables with BAS (Chau & Parreaux 2026). Eliminates the soundness gap in D2 (algebraic subtyping). Evaluate as a post-typing-cluster research item.

**Macro-Rewrite** (`doc/whatif/macro-rewrite.md`) — Replace `src/desugar.rs` with `[defmacro]` definitions. Macros cluster is complete; this consolidates remaining desugaring. Gated on macros Phase 2 (`[defmacro]`) being fully stable.

---

## Architecture Reference

- **Pipeline:** Source → Parser (`src/parser.rs` + `src/lexer.rs`) → Desugar (`src/desugar.rs`) → TypeCheck (`src/typecheck.rs`) → Evaluator (`src/eval*.rs`) → Serializer (`src/lib.rs`)
- **Key invariants:** All thunks carry `Thunk.span` (creation span). `EvalError` carries `secondary_span: Option<(Span, String)>`. `BuiltinDef` carries `pos_strictness: &'static [Strictness]`. `ThunkState` is monotonic (no backwards transitions).
- **Security:** `--no-fs` (LSP default), `--timeout` (SIGALRM), Landlock (Linux), seccomp-bpf (Linux), rlimit caps, `--require-integrity`, `cargo audit` CI gate.
- **Fuzzing:** `just fuzz parse|eval_source|typecheck_source [seconds]` — requires nightly Rust.
- **Full design history:** `doc/whatif/` (35 proposal docs), `doc/*.md` (formal spec chapters), `DONE.md` (completed sprint archive).
