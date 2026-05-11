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

## Doc Verification (completed)

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

