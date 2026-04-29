# Implementation Status

High-level guide to what's done, in progress, and not yet started. Updated 2026-04-28.
For granular task tracking see TODO.md. For completed work see DONE.md.

---

## Not Started

These are substantial features with zero implementation despite having complete designs.

### Iterative Evaluator / CEK Machine (`iterative-eval`)

The biggest remaining feature. Design: complete — see `doc/16-architecture.md §Iterative Evaluator`.
Implementation: zero. All work items still open.

What it delivers: removes the 64MB worker thread stack workaround, replaces
`MAX_EVAL_DEPTH` with configurable resource limits, enables tail-call optimization,
fixes lazy `eval_call` and `$apply` (currently force their arguments eagerly).

Key tasks: convert `materialize()` and `eval()` hot paths to explicit `Vec<Frame>`,
implement TCO for `call` expressions and recursive stdlib functions, verify thunk
lifecycle invariants carry over.

### Sandboxing (`sandbox`)

Design: complete — see `doc/12-tooling.md §Sandboxing & Security`.
Implementation: zero. ~13 items open.

What it delivers: untrusted tinct program execution via filesystem allowlist
(`--allow-path`), Landlock ACLs, seccomp-bpf network/process isolation, rlimit
resource caps. Prerequisite for the `doc/whatif/io.md` capability model.

### Parser Rewrite (`Parser Rewrite E2`)

Status: **Completed** (sprints parser-core-a through parser-core-c3). The hand-written iterative parser (`src/parser.rs` + `src/lexer.rs`) is now the production parser. The pest PEG parser was removed in sprint parser-core-c3 (commit cc8333c).

Remaining work: Phase 3 (AST-based formatter, blocked on bare-word vs quoted-string preservation) and Phase 4 (error recovery).

---

## In Progress

These features have substantial work done but meaningful gaps remain.

### Row Unification Correctness (`row-unification-h`, `h-b`)

Status: algorithm complete (subsprints c–g merged), correctness gaps remain.

Open issues:
- `check_call` missing TypeVar arm for letrec forward references — `[call $forward-fn ...]`
  produces "expected function type" error when callee is not yet bound (`src/typecheck.rs:864`)
- `ann_mapping` cross-kind collision — TypeVar and RowVar names share one HashMap, violating
  Rémy (1994) sort separation; `[fn [x@a y@[name: a ...a]] ...]` is mishandled
- `Pass 3b or_insert` discards `state.subst` binding when both maps bind the same variable
- Bracket access `$x["field"]` does not generate row constraints; dot access `$x.field` does —
  inconsistency means bracket form infers less precisely than dot form
- 6 items in h, 4 in h-b

### Row Unification Performance (`row-unification-perf`, `perf-b`, `perf-c`)

~17 open items. All are allocation reductions and fast-paths; none affect correctness.

Biggest wins: `IndexMap→HashMap` for `Substitution` and `TypeEnv` (20% lookup savings),
empty-substitution fast-paths (avoid 2 HashSet allocs per expression on the common empty
path), closed-row fast-path in `unify_rows` (skips 5+ collection allocs on the dominant path).

### Type System Completeness (`type-extensions`, `theoretical-foundations`)

Major open items:

| Item | Impact |
|------|--------|
| `TypeEnv::with_builtins()` | Builtins not in type env — all corpus typecheck tests ignored |
| `Type::Seq` inference | Sequence builtins infer as `Any`; blocks LSP hover for seq results |
| Named argument unification | Named args never checked against param types |
| IntLiteral→Float unsound arm | `unify(IntLiteral, Float)` succeeds; `is_subtype` rejects — inconsistency |
| Any-complex-type level zeroing | Vars inside `Fn/Record` unified with `Any` not zeroed; can over-generalize |
| Variadic param typed wrong | `...args` typed as `Record([], Closed)` — should be `Any` |
| TypeVar annotation aliasing | Same `@a` in two sibling dict entries overwrites levels, incorrect schemes |
| `Type::Error` sentinel | No cascading-error suppression; one type error produces 5–10 follow-ons |
| Type alias shadowing | Policy decided (allow), implementation not done |
| doc/06 + doc/07 stale content | 8+ open doc fixes for removed `Open` variant, TypeScheme field names, etc. |

### TypeAssert Structural Part 2 (`typeassert-structural-b`)

Two open Critical bugs:
- All three `ThunkState::Guarded` failure paths skip `decorate()` — user sees only the
  `[@Type ...]` definition-site span, never the access site where the type mismatch occurred
- `ThunkState::Guarded` absent from formal spec in `doc/08-evaluation.md` — state set,
  DAG, transition table, and FORCE-* rules all enumerate 6 states; Guarded is a live 7th

Additional open: LSP double-typecheck panic on AST reuse, `$filter` empty Dict returns
`Dict({})` instead of Seq, TypeAssert corpus test coverage nearly zero.

### Float NaN/Infinity Invariant (`float-nan-infinity`)

Policy decided: reject NaN/Infinity at arithmetic result sites and `$from-json` entry
("all floats are finite" invariant, matching Jsonnet/Nickel/CUE). Implementation: zero.

Five straightforward tasks: add `is_nan()||is_infinite()` check to `$+`, `$-`, `$*`,
`$/`, and the `$from-json` JSON number parser path. Shared helper `check_float_result()`
reduces duplication.

### Sequence Correctness Gaps (`seq-resource-safety`, `eval-lazy-fixes`)

Open issues:
- `$filter` seq-step depth accumulation: N consecutive predicate failures consume ~2N
  depth, hitting `MAX_EVAL_DEPTH` at ~128 consecutive misses
- `$take` depth not incremented per step — constrains composed pipelines
- `filter_dict_step` depth not incremented (vs `filter_seq_step` which correctly uses `depth+1`)
- `TypeAssert` forces materialization in `eval()` even when result is never used
- `$filter` empty Dict returns wrong type (Dict instead of Seq)
- `$filter` dict path O(n) clone per call (use `Rc::clone` instead)

---

## Mostly Done

Core algorithms are in place. Remaining items are doc fixes, nits, and minor edge cases.

- **include-fd-hardening** — 7 items open (TOCTOU fix, inode-keyed cache, file-type guards)
- **TypeAssert structural part 1** — proxy contracts implemented; open items are tests and chaperone semantics
- **Row unification a–g** — algorithm complete; open items are doc fixes and nit-level cleanup
- **Underscore desugaring** — `src/desugar.rs` complete; one or two minor follow-ons
- **Bidirectional typing** — `check_expr` implemented; a few correctness gaps tracked in `type-extensions`

---

## Ongoing (not blocking)

These areas have continuous work but are not blocking major feature completion.

**Stdlib Expansion** — ~25 missing convenience functions identified by cross-language
analysis (Jsonnet, jq, Nix, Dhall). All implementable in tinct. Includes: `partition`,
`flat-map`, `group-by`, `deep-merge`, `walk`, `take-while`, `drop-while`, `zip-with`,
`sort-on`, string predicates, numeric primitives.

**Error Context Enrichment** — Richer error messages, structured error types (replacing
raw `EvalError::new()` calls), source-accurate spans in more error paths.

**API Hygiene** — `lib.rs` re-exports for `ErrorKind`, `EvalConfig`, `EvalState`;
error type migrations; doc comment fixes.

**Performance Foundations** (`perf-foundations`) — Arena allocation, flat environments,
string interning. All deferred behind the iterative evaluator, which is a natural
prerequisite for environment reuse.

**Test Infrastructure** — Corpus coverage gaps (letrec mutual recursion, forward
reference, parse depth limit), ignored typecheck corpus tests (blocked on
`TypeEnv::with_builtins()`).

**Documentation Divergences** — Various doc/code mismatches identified by specialist
agents. None are correctness-critical.

---

## Suggested Completion Order

Based on dependencies and unblocking value:

1. **Float NaN/Infinity** (5 items) — closes a correctness/security invariant with minimal effort
2. **TypeAssert Part 2 criticals** — fixes two Critical span bugs; unblocks error quality
3. **Type-extensions code fixes** — IntLiteral-Float soundness, TypeEnv::with_builtins(),
   TypeVar aliasing; unblocks corpus typecheck tests and LSP hover
4. **Row unification h + h-b** — closes correctness gaps in the type system's biggest
   recent feature; prerequisite for reliable polymorphic inference
5. **Row unification perf** — allocation hotspot cleanup; improves inference throughput
6. **Sequence correctness gaps** — closes depth-tracking bugs in filter/take chains
7. **Iterative evaluator** — largest single remaining sprint; delivers TCO, configurable
   depth, lazy eval_call

### Whatif "Adopt Now" Items are Independent

The `doc/whatif/index.md §Adopt Now` items (Type Predicates, String Interpolation Phase 1,
Let Binding, Structural Contracts Phase 1, ADTs Phase 1 convention) do not depend on any
of the major incomplete features above. They can be implemented in any order, in parallel
with or after steps 1–4.
