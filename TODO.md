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

---

---

### stub-network-protocols: Implement Stub Network Builtins

`doc/11a-builtins.md:381` documents that `Icmp` transport is a stub returning "not yet implemented" errors pending platform-specific socket support. `doc/11a-builtins.md:655` notes "Future work: If a session builtin needs to hold a live async connection across multiple builtin calls (e.g., streaming gRPC), the runtime must outlive the builtin call." The stubs registered in DONE.md (via `connect-v2` and `http-sessions` sprints) include `icmp-ping`, `http3-session` via `quic-session`, and a full streaming session model. No implementation sprint exists for any of these.

- [ ] [Major] Implement `icmp-ping` builtin — raw ICMP echo request/response via `socket(AF_INET, SOCK_RAW, IPPROTO_ICMP)` (requires `CAP_NET_RAW` or running as root on Linux, or using unprivileged ICMP via `SOCK_DGRAM` on Linux 3.11+); return `Handle[Readable Binary Datagram]`; update error from "not yet implemented" to real implementation in `src/builtins_io.rs`; document privilege requirements in `doc/11a-builtins.md`
- [ ] [Major] Implement `http3-session` builtin using `quinn` QUIC transport — the `quic-session` builtin is fully implemented (DONE.md:2764); `http3-session` builds on it by adding HTTP/3 framing (QUIC stream multiplexing, HEADERS frame encoding via `h3` crate); return `Handle` with `Http3Session` capability; current stub in `src/builtins_io.rs` returns "not yet implemented"
- [ ] [Minor] Design and implement persistent async handle storage for streaming sessions — `doc/11a-builtins.md:655` identifies the design requirement: for builtins that hold a live async connection across calls (e.g., streaming gRPC or HTTP/3 server-push), store the Tokio runtime alongside the connection in `Value::Handle`'s `caps` dict as an opaque `Arc<Mutex<TokioRuntime>>` resource; required before any streaming session builtin can be implemented

---

