# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## LSP Improvements

### lsp-caps-and-on-demand: LSP caps assumption + on-demand file loading

- [x] [Major] Skip caps validation in LSP mode — pre-seed eval env with stub cap values
- [x] [Major] On-demand hover for unopened documents — load from disk if not in document map
- [x] [Major] On-demand goto-definition for unopened documents
- [x] [Minor] Extract shared `load_doc_from_uri` helper in document.rs
- [x] [Minor] Add 3 LSP corpus tests for unopened document hover/goto/caps

---

## Doc Verification

---

---

### doc-verify-documents: doc/09-documents.md against implementation

Major items RESOLVED (2026-05-10) — doc/09-documents.md updated: `Σ` state definition, cache key type, include rules, and base_dir all corrected. These are permanent architectural decisions (B — code is correct, doc was stale).

**Remaining:**

- [ ] [Minor] Update Implementation Correspondence tables in `doc/09-documents.md` — stale file:line references. `eval_document` is at `src/eval_pipeline.rs:33`; `eval_file_with_input` at `src/eval_pipeline.rs:256`. Part 6 table says `Σ (EvalState) | eval.rs:41-45` — actual is `src/eval.rs:109-144`.

---

### doc-verify-tooling: doc/12-tooling.md and doc/16-architecture.md against implementation

Most items RESOLVED (2026-05-10) — docs updated to match implementation:
- `--no-stdin`: removed from docs (B — `%stdin` is only injected when `-i` is present; no separate flag needed)
- `just ext-package`: merged into single `just ext` command in docs (B)
- `--algo` hash flag: removed from docs; hash algorithms table reduced to BLAKE3 only (B — only BLAKE3 implemented)
- `--allow-network`: removed from docs (B — network allowed automatically with `--cap-net`)
- `RLIMIT_FSIZE`: removed from resource table (B — not implemented, not needed)
- Sandbox init order: corrected in docs to match actual sequence (B)
- `EvalConfig` struct in doc/16: updated to match current source (B)

**Remaining (aspirational -- implement, don't fix doc):**

- [ ] [Minor] (aspirational) Add SHA3-256, SHA3-512, and SHA-256 hash verification support to `parse_integrity_hash()` in `src/builtins_meta.rs` — currently only BLAKE3 is supported. Doc has been reduced to BLAKE3-only; if additional algorithms are implemented, update doc/12-tooling.md hash table accordingly.

---

### doc-verify-type-extensions: doc/07-type-extensions.md vs BAS implementation divergence

Partially RESOLVED (2026-05-10) — BAS is the permanent type system (B). Section header and Row struct code block updated. Stale `src/typecheck_annot.rs` comment fixed. BAS supersedes Remy permanently.

**Remaining doc cleanup (all B — update doc to match code):**

- [ ] [Major] Complete archival of Rémy Parts 1-9 content in doc/07-type-extensions.md — header and Row struct updated, but `Substitution` (row_map), `TypeScheme` (row_vars), Access Chain pseudocode (RowVar generation), Row Display (tail-based output), and `unify_rows` description still show stale Rémy content. Update each to document BAS-era behavior or mark as archived.
- [ ] [Minor] Document the `repr:` annotation property in doc/07-type-extensions.md — fully implemented at `src/typecheck_annot.rs:100-127`: accepts `"u8"`, `"i8"`, `"u16"`, `"i16"`, `"u32"`, `"i32"`, `"u64"`, `"i64"` and enforces numeric type. Not mentioned in doc/07 or doc/05.
- [ ] [Minor] (aspirational — implement, don't fix doc) `Dict = Record ∨ Map[K V]` at doc/07:806-814 — `Type::Map` exists but `@Map` annotation is unimplemented. Mark as aspirational or move to `doc/whatif/`.
- [ ] [Minor] (aspirational — implement, don't fix doc) Nominal Result Type at doc/07:794-803 — runtime returns `Value::Variant { tag: "Ok"/"Err" }` but `Type::Variant` does not exist yet. Static typing uses `Unknown`. Add aspirational note.

---

### doc-verify-eval08: doc/08-evaluation.md divergences from implementation

Most items RESOLVED (2026-05-10) — doc/08-evaluation.md updated. The iterative CEK machine is the permanent architecture (B). All depth-related changes are permanent design decisions:
- `[MATERIALIZE-DEPTH]` rule: removed. Depth tracking paragraph replaced with CEK machine description.
- All judgment forms: `d` parameter removed throughout (`materialize(θ)`, `eval(e, ρ, Σ)`, delta rules).
- `PendingBuiltin` `pd` field: removed from rules and prose. Depth semantics rationale paragraph removed.
- `[MATERIALIZE-GUARD-DEPTH]` and `[MATERIALIZE-GUARD-OUTER-DEPTH]`: consolidated into `[MATERIALIZE-GUARD-NONCACHEABLE]`.
- `Action`/`Cont` pseudocode: updated to match current 3-variant `Action` and 6-variant `Cont`.
- `run()` signature: updated to current `pub(crate) fn run(initial: Action, ctx: &Rc<EvalContext>) -> EvalResult<Value>`.
- Semantic Commitment 3: rewritten for CEK machine (no MAX_EVAL_DEPTH).
- Strictness table intro: "all 59 builtins" corrected to "core evaluation and collection builtins".
- Backward edge description: updated to reference builtins-only origin for DepthExceeded.
- `Cont` size budget note: now references compile-time assertion correctly.

**Remaining:**

- [ ] [Minor] Document the strict let* semantics of document scope chain in doc/08-evaluation.md — `eval_document` eagerly materializes named bindings (`src/eval_pipeline.rs:108-155`), unlike letrec dict entries which remain lazy. This semantic distinction is undocumented.
- [ ] [Minor] Update `deep_materialize` description in doc/08-evaluation.md to reference `MAX_COLLECT_SIZE` (1,000,000) instead of `MAX_EVAL_DEPTH` (256) for Seq spine guards.
- [ ] [Minor] Update TypeAssert strictness exception in doc/08-evaluation.md — validation is now via `Cont::TypeAssertCheck` continuation, not immediate blocking materialization.
- [ ] [Minor] Update the planned-but-unimplemented Cont variants table (lines ~1281-1292) — mark `CallForceFunc`, `DocumentScope`, `DictBuildKey`, etc. as future work (aspirational — implement, don't fix doc).

---

## Planned Features (from doc spec)

### mappable-appendable-constraints: Implement Mappable and Appendable Type Class Constraints

`doc/06-type-inference.md:733-734` documents two type classes — `Mappable` (instances: Record, Seq) and `Appendable` (instances: Str, StringLiteral, Record, Seq) — as "not yet constrained". The class declarations exist in the class table but are not wired into constraint generation or checking. As a result, functions constrained with `Mappable` or `Appendable` are not type-checked: any value passes where these constraints are expected. `src/type_env.rs:1872` contains a comment "Note: Mappable constraint requires higher-kinded types (Phase 3 / D1 scope)" — `Appendable` has the same blocker. `Mappable` in particular requires higher-kinded type support (`f :: * → *`) to express `map : (a → b) → f a → f b`, which is outside the current H-M system.

- [ ] [Major] Implement `Appendable` constraint checking in `src/typecheck.rs` — `Appendable` has concrete (non-HKT) instances (Str, StringLiteral, Record, Seq), so constraint resolution can be added without HKT support; when a `Appendable a` constraint is generated, verify `a` unifies with one of the known instance types and emit a type error if not; register concrete overloaded forms (`++` / `append`) with `Appendable a` constraints in `src/type_env.rs`
- [ ] [Major] Design and implement `Mappable` constraint checking — `Mappable` requires higher-kinded types (`f :: * → *`); see `src/type_env.rs:1872` and `doc/06-type-inference.md:733`; either add a restricted HKT representation (kind-indexed type variables) or special-case `Mappable` for its two known instances (Dict, Seq) similarly to how `Equatable` is handled; write a design note in `doc/whatif/` before implementing

---

### allow-host-sandbox: Per-Host Network Capability Filtering

`doc/12-tooling.md:661` documents `--allow-host <host:port>` for fine-grained network control as "future — requires application-level checking since seccomp cannot filter by host". Currently, when `--cap-net` is present, any host is reachable; the only granularity is all-or-nothing per capability. The `doc/16-architecture.md:726` EvalConfig struct shows `// future: allowed_hosts: Vec<String>` as a placeholder. This feature would enable sandbox policies like "this program may only connect to api.example.com:443".

- [ ] [Major] Add `allowed_hosts: Vec<String>` to `EvalConfig` (`src/eval.rs`) and expose `--allow-host <host:port>` CLI flag that populates it (`src/main.rs`); multiple `--allow-host` flags are additive; empty list means all hosts allowed (current behavior, preserving backward compatibility)
- [ ] [Major] Implement host/port allowlist check in `builtin_connect` and `builtin_http_request` — before opening a TCP connection, parse the target host and port; if `EvalConfig::allowed_hosts` is non-empty, reject connections to hosts not in the list with `ErrorKind::CapabilityViolation` (`src/builtins_io.rs`)
- [ ] [Minor] Update `doc/12-tooling.md` §Network Sandbox section and `doc/16-architecture.md` EvalConfig struct to describe `--allow-host` as implemented (remove "future" qualifier and the comment placeholder in the struct)

---

### literate-full-substitution: Full Result Substitution in Weave Mode

`doc/09-documents.md:953` documents a "future refinement" for the `tinct weave` command: "Full result substitution (replacing inline markers in prose)". Currently weave annotates code blocks with `<!-- tinct-result: ... -->` HTML comments, but does not replace inline `<!-- tinct-result -->` markers embedded in the Markdown prose itself. A full substitution mode would find `<!-- tinct-result: (emit) -->` markers in prose and replace them with the evaluated result of the preceding or named code block.

- [ ] [Major] Implement inline marker substitution in `src/weave.rs` (or wherever weave is implemented) — after evaluating all code blocks, make a second pass over the Markdown source replacing `<!-- tinct-result: ... -->` markers in prose with the JSON/display-string result of the corresponding block; use block name (from `%name@Type` header) for named references
- [ ] [Minor] Add `--no-substitute` flag to `tinct weave` to disable marker replacement (for environments that consume the HTML comments programmatically), preserving current behavior as an opt-out (`src/main.rs`)
- [ ] [Minor] Add corpus tests for `tinct weave` with inline markers — verify marker replacement, named block reference, and `--no-substitute` passthrough (`tests/corpus/`)

---

### span-corrections-remaining: Remaining Span Assignment Issues

`doc/10-errors.md:930-944` documents a "Known Span Assignment Issues" table with a note "These corrections are not yet implemented." Several rows in this table were addressed in earlier sprints (`span-builtins`, `span-errors`), but at least two remain unimplemented: (1) "Depth limit errors lack call-site context" — `DepthExceeded` errors use `def_span` pointing to the thunk being materialized, but do not include `mat_span` pointing to the call site that triggered the limit; (2) "Desugared lambda spans" — `wrap_expr_in_lambda` at `src/desugar.rs:158,174` assigns the outer expression span to both the generated `Fn` node and its body, so type errors inside `$_.field` lambdas point to the whole outer call site rather than the inner expression.

- [ ] [Minor] Fix `DepthExceeded` error to include call-site materialization span — in `src/eval_materialize.rs` where `DepthExceeded` is constructed for builtins-only depth violations, chain `.with_materialization_span(call_span)` so the error shows both the thunk definition site and the call site that triggered the limit (`src/eval_materialize.rs`, `src/error.rs`)
- [ ] [Minor] Fix `wrap_expr_in_lambda` in `src/desugar.rs` to assign the inner expression's span to the generated `Fn` body — currently `desugar.rs:158,174` assigns `expr.span` to the entire `Fn` node; use the inner `_.field` sub-expression span for the body so type errors point to the actual failing sub-expression rather than the outer call site (`src/desugar.rs:158,174`)
- [ ] [Minor] Update `doc/10-errors.md` §Known Span Assignment Issues table to mark the two addressed rows as implemented and remove the blanket "not yet implemented" note for rows that were fixed in `span-builtins` and `span-errors` sprints

---

### stub-network-protocols: Implement Stub Network Builtins

`doc/11a-builtins.md:381` documents that `Icmp` transport is a stub returning "not yet implemented" errors pending platform-specific socket support. `doc/11a-builtins.md:655` notes "Future work: If a session builtin needs to hold a live async connection across multiple builtin calls (e.g., streaming gRPC), the runtime must outlive the builtin call." The stubs registered in DONE.md (via `connect-v2` and `http-sessions` sprints) include `icmp-ping`, `http3-session` via `quic-session`, and a full streaming session model. No implementation sprint exists for any of these.

- [ ] [Major] Implement `icmp-ping` builtin — raw ICMP echo request/response via `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` (requires `CAP_NET_RAW` or running as root on Linux, or using unprivileged ICMP via `SOCK_DGRAM` on Linux 3.11+); return `Handle[Readable Binary Datagram]`; update error from "not yet implemented" to real implementation in `src/builtins_io.rs`; document privilege requirements in `doc/11a-builtins.md`
- [ ] [Major] Implement `http3-session` builtin using `quinn` QUIC transport — the `quic-session` builtin is fully implemented (DONE.md:2764); `http3-session` builds on it by adding HTTP/3 framing (QUIC stream multiplexing, HEADERS frame encoding via `h3` crate); return `Handle` with `Http3Session` capability; current stub in `src/builtins_io.rs` returns "not yet implemented"
- [ ] [Minor] Design and implement persistent async handle storage for streaming sessions — `doc/11a-builtins.md:655` identifies the design requirement: for builtins that hold a live async connection across calls (e.g., streaming gRPC or HTTP/3 server-push), store the Tokio runtime alongside the connection in `Value::Handle`'s `caps` dict as an opaque `Arc<Mutex<TokioRuntime>>` resource; required before any streaming session builtin can be implemented

---

### stale-doc-aspirational: Fix Stale "Planned/Not Yet Implemented" References

Several `doc/*.md` sections use "planned", "not yet implemented", or reference sprint names for features that have since been completed. These stale markers violate the "final end state" principle established in the doc reframing session (2026-04-23) and create confusion when developers read the spec.

- [ ] [Minor] Fix `doc/08-evaluation.md:913,919` — `$merge` strictness table says "See merge-lazy-overlay sprint in TODO.md for planned lazy overlay upgrade" and `$update` says "Lazy overlay planned. See merge-lazy-overlay sprint in TODO.md." Both references are stale; the `merge-lazy-overlay` sprint is complete (DONE.md). Update table to describe actual current behavior without sprint references.
- [ ] [Minor] Fix `doc/11-stdlib.md:973,1060-1062` — line 973 says "See Part 5 for a planned lazy overlay optimization" and Part 5 opens with "NOT YET IMPLEMENTED. The current `merge` eagerly materializes both operands." Both are stale. Update to describe the completed `Overlay(L, R)` implementation (DONE via `merge-lazy-overlay`); Part 5 spec can remain as the behavioral reference but drop the NOT YET IMPLEMENTED banner.
- [ ] [Minor] Fix `doc/12-tooling.md:7` — formatter status note says AST-based formatter is "accepted but not yet implemented". The `parser-formatter` sprint is complete (DONE.md). Remove the status note or update to reflect implementation.
- [ ] [Minor] Fix `doc/12-tooling.md:138-142` — A grammar-cleanup agent incorrectly rewrote "A future tinct-hosted formatter will be implemented in `stdlib/formatter/format.llt`" to "The tinct-hosted formatter is implemented in `stdlib/formatter/format.llt`". `stdlib/formatter/format.llt` does NOT exist (only `compact.llt` and `pretty.llt` exist). Revert the section to say compact/pretty modes are done (`stdlib/formatter/compact.llt`, DONE.md:4839) and the full `format.llt` is not yet implemented.
- [ ] [Minor] Fix `doc/12-tooling.md:257` — Says "cross-file resolution for `$include` and prelude names is not yet implemented" but this is stale. Cross-file include resolution was completed in `lsp-workspace-index` (DONE.md:4794) and prelude name awareness in `lsp-include-prelude` (DONE.md:4786). Update the Go To Definition capability description to remove "not yet implemented" and describe the actual cross-file support.
- [ ] [Minor] Fix `doc/04-functions.md:258` — WRAP-PIPE row in `$_` desugaring table says "Planned — pipe `|` syntax exists as `Expr::Pipe` in AST but is not yet user-documented". The pipe desugar rule was implemented in the dot-head sprint (DONE.md:4651). Remove the "Planned" qualifier and update to describe current behavior.
- [ ] [Minor] Fix `doc/05-type-annotations.md:272` — says "Full support for recursive algebraic data types requires parameterized type aliases (future work)". The `recursive-adts` sprint implemented parameterized type aliases with `[type [a b] body]` syntax (DONE.md:4974-4977). Update the limitation note to reflect the implemented capability.
- [ ] [Minor] Fix `doc/16-architecture.md:541-545` — "Security Hardening Roadmap" lists three features as "not yet implemented": import integrity hashes, file descriptor-based `$include`, and `cargo audit`. All three are now done: `--require-integrity` flag added (DONE.md:2492), cap-std fd-based include replacing TOCTOU-prone `canonicalize()` flow (DONE.md:3915), and `cargo audit` CI gate (DONE.md:2487). Remove the roadmap list or move it to implemented features; update `doc/16-architecture.md:568` to reflect `cargo audit` is in CI.
- [ ] [Minor] Fix `doc/08-evaluation.md:1031-1141` — Allocation strategy section describes arena Phase 2/3 as follows: "Phase 2: ThunkArena/EnvArena exist in EvalContext but are unused (#[allow(dead_code)])" and "Phase 3: full arena-based allocation (future)". Both are stale. The `arena-eval` sprint (DONE.md:4543) fully migrated Value variants to ThunkId handles — `Value::Dict(IndexMap<Key, ThunkId>)`, `Value::Seq { head: ThunkId, tail: ThunkId }`, `Value::Overlay(ThunkId, ThunkId)` all use arena ThunkIds. Rewrite the allocation strategy section to describe the current implemented arena architecture.
