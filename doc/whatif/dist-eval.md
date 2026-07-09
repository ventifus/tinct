# What If: Distributed Evaluation for tinct

**State:** Proposal

**Depends on:** [`runtime-v2.md`](runtime-v2.md) — requires `eval`/`materialize` to be `async fn`, `Arc`-based thunks, `CoreExpr` as the serializable evaluator-internal representation, and the multi-thread Tokio runtime to be complete before distributed work is possible.

What would it take to evaluate tinct programs across a cluster of machines — transparently, with content-addressed caching so that identical computations never run twice?

## Current State

As of `runtime-v2.md`, tinct evaluation spans a single machine: native AST value types (`AstExpr`, `AstDoc`, `AstFile`), async I/O, cooperative concurrency, multi-core parallel dict evaluation via `JoinSet` fanout, `par`/`par-map`, `task`/`await`/`channel`/`select`. The remaining gap is scale: a large dataset that exceeds a single machine's memory or throughput cannot be processed in one tinct process.

### What's Missing

1. No way to send a computation to a remote node and await its result.
2. No content-addressed cache that persists results across runs or shares them across nodes.
3. No cluster membership, leader election, or fault-tolerant dispatch.
4. No distributed equivalents of `map`/`filter`/`reduce` over large datasets.

## Why Distributed Evaluation Matters for tinct

tinct's thunk graph is an implicit parallel execution plan. Every thunk with no data dependency on another is, by definition, safe to evaluate anywhere — on any core, on any machine. The `Arc`-based `OnceLock` thunk model (from `runtime-v2.md`) already makes this safe within a process. Distributed evaluation extends the same model across process boundaries.

Concretely:

- **Cluster-scale data processing.** `dist-map` over a 100M-element dataset distributes shards across a worker pool. The program is unchanged; only the scheduler differs.
- **Incremental pipelines.** The content-addressed result cache skips recomputation for unchanged inputs — the same guarantee thunk memoization provides locally, extended to cluster scope and across program runs.
- **Pure isolation.** Because distributable tasks are pure (no I/O capabilities), they can safely run on any node. The capability model already identifies which computations are pure; the distributed scheduler exploits that boundary.

---

## Design

### Distributed Semantics, Not a Distributed System

The design distinguishes two layers:

**Semantic core** — what the language specifies:

- A `Cluster` is an opaque environment in which computations can be scheduled.
- `remote-task` submits a thunk to that environment and returns a `Task@T`.
- `GlobalThunkRef` is a cross-node pointer to a live thunk; free variables in a distributed task's environment may be `GlobalThunkRef` entries resolved lazily by the DSM layer — inputs need not be computed before the dependent task is submitted.
- `Ref@T` is a content-addressed value resolved on demand by the transport layer; large results are promoted automatically from `GlobalThunkRef` to `Ref@T` on completion.
- A thunk evaluates exactly once; independent thunks have no ordering constraint; dependent thunks wait. These semantics are identical whether evaluation runs on one core, many cores, or many machines.

**Implementation** — what a specific cluster provides:

- The transport (QUIC, TCP, shared memory, a mounted filesystem).
- The replication and leader-election mechanism.
- The storage backend for the result cache.
- The strategy for resolving `Ref@T` values.

A program written against `Cluster`/`remote-task`/`Ref` works without modification on an in-process worker pool, a three-node cluster over QUIC, or a larger deployment with a shared cache — because it expresses *what* to compute, not *how* to move data.

---

### The Distributable Thunk

A thunk is **distributable** if its `CoreExpr` body references no I/O capability values (`DirCap`, `NetCap`, `Handle`, `Channel`, etc.).

Free variables in the environment need not be materialized. A free variable bound to a live thunk is serialized as a `GlobalThunkRef` — a globally addressable pointer into the owning node's thunk arena. The worker resolves `GlobalThunkRef` entries lazily via the DSM layer when evaluation demands them. Concrete values flow inline in the wire format; live thunks flow as references.

The serialized form of a distributable thunk encodes two parts: (1) the `CoreExpr` body — encoded as a Dict via `core_expr_to_wire`; and (2) the environment — a flat Dict mapping variable coordinates or free-variable name strings to either concrete wire values or `GlobalThunkRef` entries (tag `0x0E`). The worker deserializes these, reconstructs a `FlatEnv` with `UnevaluatedState::Remote` thunks in place of `GlobalThunkRef` entries, and evaluates the `CoreExpr` in a fresh `EvalContext`.

### GlobalThunkRef: Distributed Shared Thunks

`GlobalThunkRef` is a globally unique, cross-node pointer to a live thunk on a specific node. It enables submitting a dependent task before its inputs are computed — promise pipelining across the cluster.

```rust
pub struct GlobalThunkRef {
    pub node:  NodeId,        // UUID of the owning node
    pub thunk: LocalThunkId,  // ThunkId within that node's ThunkArena
}
```

`GlobalThunkRef` is ephemeral within a program run. A `LocalThunkId` is an index into the owning node's `ThunkArena`; it carries no meaning across node restarts or between program runs. Cross-run caching is `Ref@T`'s role.

#### Demand-Fetch Protocol

When a worker needs the value of a `GlobalThunkRef(B, T)`, `src/dsm.rs` executes:

1. **Check local DSM cache.** `DsmCache: HashMap<GlobalThunkRef, DsmEntry>` where `DsmEntry` is `Pending(Vec<Waker>)` (in-flight) or `Ready(Result<Value, Arc<EvalError>>)` (resolved). If ready, return immediately — lock-free after initialization.

2. **Register as waiter or first requester.** Lock the cache entry. If `Pending`, push waker and suspend. If absent, insert `Pending([self_waker])` and proceed to step 3.

3. **Send demand request.** Send `DemandRequest { origin_node, thunk_ref, probe_chain: vec![self_thunk_id] }` to node B via the cluster transport. The `probe_chain` piggybacks the CMH cycle-detection probe (see §Distributed Cycle Detection).

4. **Owning node B** forces thunk T: `materialize(T).await`. On completion:
   - **Small result** (≤ `INLINE_THRESHOLD`, default 64 KiB): encode in wire format, send `DemandResponse { thunk_ref, payload: Inline(bytes) }`.
   - **Large result** (> `INLINE_THRESHOLD`): store as `Ref@T` in the content-addressed cache (automatic promotion), send `DemandResponse { thunk_ref, payload: Promoted(content_hash) }`.

   Node B also forwards the probe chain if it is itself suspended on a `GlobalThunkRef` (see §Distributed Cycle Detection).

5. **Requesting node A** receives `DemandResponse`, stores the result in its local DSM cache, wakes all waiters. If the payload is `Promoted`, the caller resolves `Ref@T` via the transport layer's resolution tiers (see §Ref@T).

#### OnceLock Coherence

Tinct thunks evaluate exactly once. DSM coherence is write-once: no invalidation, no versioning, no memory barriers across writes. Once a `DemandResponse` is received, the result is cached permanently in the DSM cache. Any subsequent demand for the same ref is served locally. For large promoted results, RDMA one-sided reads allow a node to DMA the bytes directly from the owning node's memory after the `OnceLock` is set — the owning node's CPU is not involved in the transfer.

#### Promise Pipelining

A dependent task can be submitted before its inputs complete:

```tinct
# A and B submitted immediately — B does not wait for A to finish
a: [remote-task cluster [fn [let] [phase-1 input]]]
b: [remote-task cluster [fn [let] [phase-2 a]]]   # 'a' serialized as GlobalThunkRef
result: [await b]
```

Worker B runs immediately on its assigned node. When it demands `a`, it sends a `DemandRequest` to A's owning node and suspends. A's result flows directly to B — the coordinator is not involved in the data transfer. For pipeline-shaped computations this eliminates one coordinator round-trip per stage.

`dist-map` pipelines automatically: the combiner step is submitted as soon as any shard's `GlobalThunkRef` exists, and wakes incrementally as shards complete.

#### GlobalThunkRef Lifecycle

```text
Created (thunk submitted via remote-task)
  → Pending  (owning node computing; waiters suspended)
    → Inline  (result small; sent directly in DemandResponse)
    → Promoted (result large; stored as Ref@T; content hash sent)
  → Dead     (owning node failed — all waiters receive EvalError::node_failed)
```

If the owning node fails before computing T, the coordinator detects this via Raft heartbeat and broadcasts `NodeFailed { node_id }`. `src/dsm.rs` on each node wakes all waiters for that node's refs with `EvalError::node_failed`. There is no automatic re-dispatch of a failed `GlobalThunkRef` — the original `remote-task` call fails, and re-submission is the caller's responsibility. Because the serialized thunk body is still held at the call site, re-dispatch requires only a new `remote-task` call.

---

### `remote-task`

```tinct
# Connect to a cluster — returns a Cluster handle
cluster: [connect-cluster net-cap "tinct://coordinator.internal:7777"]

# Or an in-process cluster (no network; uses local cores only)
cluster: [cluster-local [workers: 8]]

# Submit work — returns Task@T, same type as task
shard1: [remote-task cluster [fn [let] [map transform data-shard-1]]]
shard2: [remote-task cluster [fn [let] [map transform data-shard-2]]]
shard3: [remote-task cluster [fn [let] [map transform data-shard-3]]]

# Await results
combined: [combine [await shard1] [await shard2] [await shard3]]
```

`remote-task` returns `Task@T` — the same type as `task`. `await`, `await-all`, `timeout`, and cancellation contexts work identically on remote tasks.

---

### The Tinct-Native Wire Format

The wire format is a binary encoding of tinct's value space — not a translation to JSON or any external format. It is self-describing (each value carries a tag), length-prefixed (no need to read a full message before beginning to parse), and covers every type in the evaluator's value enum:

| Tag | Value |
|-----|-------|
| `0x00` | Null |
| `0x01` | Bool |
| `0x02` | Int (i64, varint-encoded) |
| `0x03` | Float (f64, IEEE 754) |
| `0x04` | String (u32 length + UTF-8 bytes) |
| `0x05` | List (u32 count + elements) |
| `0x06` | Dict (u32 count + (String, Value) pairs) |
| `0x07` | Error (ErrorKind tag + String message + [StackFrame]) |
| `0x08` | Thunk (CoreExpr encoded as Dict + flat Env encoded as Dict) |
| `0x09` | Closure (same as Thunk; distinguished for type-checking) |
| `0x0A` | Ref (32-byte content hash + type tag) |
| `0x0B` | AstExpr (`SurfaceNode` encoded as Dict — via `ast_to_dict_expr`) |
| `0x0C` | AstDoc (`SurfaceDoc` encoded as Dict) |
| `0x0D` | AstFile (`SurfaceFile` encoded as Dict) |
| `0x0E` | GlobalThunkRef (u32 node-id-string length + UTF-8 node-id bytes + u64 thunk-id) |

`Thunk` (tag `0x08`) is the central tag for distributed evaluation. Its environment Dict may contain both concrete wire values and `GlobalThunkRef` entries (tag `0x0E`); concrete values flow inline, live thunks flow as cross-node references resolved lazily by the DSM layer.

The `Thunk` body is `CoreExpr` (de Bruijn coordinates, no `SurfaceExpr`) — encoded by `core_expr_to_wire` / `core_expr_from_wire` in `src/serialize.rs`. This is distinct from `ast_to_dict_expr` (retained in `runtime-v2` for `deep-materialize` and JSON output, incompatible with `CoreExpr`'s coordinate space). The `dict_to_ast` deleted in `runtime-v2` has no wire-format equivalent; `src/serialize.rs` provides its own `wire_to_surface_node` decoder.

`AstExpr`, `AstDoc`, `AstFile` tags (`0x0B`–`0x0D`) allow AST values to round-trip across the cluster — e.g. when a macro transformer runs as a distributed task and returns an `AstExpr` result. Encoding reuses `ast_to_dict_expr` (retained in `runtime-v2`).

`GlobalThunkRef` (tag `0x0E`) is only valid within the current program run on the current cluster. It must never appear in persistent storage, content-addressed caches, or cross-run messages.

The format makes no distinction between "task message" and "value": every message in the worker protocol is a tinct dict encoded in this format. The Raft log entries (see Coordinator Group below) are tinct dicts. Cluster state is introspectable using the language's own builtins.

---

### `Ref@T`: Transport-Agnostic Large Values

`Ref@T` is a content-addressed reference to a value. It is a first-class value in the wire format and in the evaluator: a `Ref` carries a 32-byte content hash and a type. When an expression requires the underlying value, the evaluator's transport layer resolves it.

```tinct
# Store a large value in the cluster — returns Ref@Dict
input-ref: [cluster-store cluster huge-dict]

# The ref can be passed to remote tasks; the worker fetches it when needed
result: [remote-task cluster [fn [let] [process input-ref]]]
```

**Resolution tiers** (in order of cost):

1. **Local DSM cache** — if this node already has the result (received via `DemandResponse`), return immediately.
2. **Local Ref cache** — `DashMap<[u8; 32], Arc<Value>>` on this node, keyed by content hash. Populated from prior computations.
3. **RDMA one-sided read** (hardware-conditional) — if the cluster transport supports RDMA and the owning node has the value in a registered memory region, issue an RDMA READ directly into the local buffer. Zero kernel copies, owning-node CPU not involved. Falls back to tier 4 if unavailable.
4. **Peer pull via QUIC** — send a `RefRequest { hash }` to the node holding the value (known from the coordinator's cache index). Owning node sends `RefResponse { hash, bytes }`.
5. **Configured external store** — pluggable backend (mounted filesystem, S3-compatible API, etc.). Slowest; survives full cluster restart.

The language specifies only the contract: given a hash, produce a value or fail. Programs are written against `Ref@T`; they do not know or care which tier resolves the ref.

**Type system decision:** `Ref@T` unifies transparently with `T` in the type checker. The evaluator resolves `Ref` when forcing a thunk that contains one — exactly as it forces a nested thunk. User code never writes `[deref ref]`; the type checker treats `Ref@T` and `T` as equivalent in unification. `cluster-store cluster v@T` returns `Ref@T`; anywhere `T` is expected, `Ref@T` is accepted. The distinction exists in the wire format and runtime (for routing and caching), not in the language's type lattice.

---

### Distributed Cycle Detection

Cross-node cycles in the thunk demand graph are programmer errors — circular data dependencies that would also produce `EvalError::circular_dependency` locally (e.g., `[x: x]`). The distributed case extends the local cycle detection to the multi-node scenario. Because each thunk waits on at most one `GlobalThunkRef` at a time, the wait-for graph (WFG) is a **functional graph** — at most one outgoing wait edge per node. Cycles are simple directed loops, not AND/OR combinations.

#### Chandy-Misra-Haas Probe Algorithm

Detection uses the CMH AND-model probe algorithm (Chandy & Misra, 1983). Probes piggyback on `DemandRequest` messages — no extra message type required.

**Protocol:**

1. When node A suspends on `GlobalThunkRef(B, T)`, it appends `(A, self_thunk_id)` to the probe chain and sends `DemandRequest { thunk_ref: (B, T), probe_chain: [(A, a_thunk_id)] }` to B.

2. Node B, on receiving the demand, checks: is it itself suspended on a `GlobalThunkRef(C, T2)`?
   - **No** (B is computing or complete): B does not forward the probe. The chain terminates — no cycle.
   - **Yes**: B appends `(B, b_thunk_id)` to the chain and forwards `DemandRequest { thunk_ref: (C, T2), probe_chain: [..., (B, b_thunk_id)] }` to C.

3. If any node in the chain receives a probe whose chain already contains itself (O(n) scan, bounded by cluster size), a cycle is detected. That node returns `EvalError::circular_dependency` with the full cycle listed in the error message. The error propagates back through the chain, waking all waiters with the same error.

**Correctness:** The probe travels along wait edges. If a cycle exists, the probe returns to its origin. If no cycle exists, the probe terminates at a non-blocked node. The chain accumulates in the `DemandRequest` message — no additional round-trips, no coordinator involvement.

**Optimization — path doubling (Kshemkalyani & Singhal, 1994):** For long wait chains, path doubling detects cycles in O(log n) probe rounds instead of O(n). Each node tracks its last-seen probe position; on receiving a probe it has seen before, it doubles its forward pointer. Worthwhile for clusters with deep dependency chains (>100 nodes deep). Implemented in `src/dsm.rs` as a configurable option (`--cycle-detect [linear|doubling]`, default `linear`).

#### Integration with Local Cycle Detection

Within a single node, cycle detection already runs via the task-local `HashSet<*const Thunk>` in the async evaluator. `UnevaluatedState::Remote` thunks (backed by `GlobalThunkRef`) are included in this set — a task that demands a `GlobalThunkRef` it is already waiting on (within the same Tokio task's call chain) is caught locally before a `DemandRequest` is sent.

The two detectors compose cleanly: local detection catches intra-task cycles at zero cost; CMH probes catch inter-node cycles with one extra message field per demand request.

---

### Coordinator Group & Leader Election

A `Cluster` is not a single coordinator process — it is a **coordinator group**: a set of nodes that replicate state via consensus and elect a leader to serve requests. Any node in the group can act as leader; any node can simultaneously accept work as a worker.

**Homogeneous nodes.** Every node in a tinct cluster runs two components:

- **Worker runtime** — evaluates tasks, the same as a dedicated worker.
- **Coordinator module** — replicates cluster state, redirects clients, routes tasks when leading.

On the current leader, the coordinator module is active: it routes `remote-task` submissions to available workers, commits log entries, and serves cache lookups. On followers, the coordinator module replicates the log and redirects clients to the leader. There are no dedicated coordinator machines; any node that can do work can also coordinate.

**Quorum.** For a cluster of 2k+1 nodes, k+1 constitute a quorum. The cluster tolerates k simultaneous failures while remaining operational.

| Nodes | Quorum | Tolerated failures |
|-------|--------|--------------------|
| 1 | 1 | 0 (no fault tolerance) |
| 3 | 2 | 1 |
| 5 | 3 | 2 |
| 7 | 4 | 3 |

A three-node cluster — where workers are coordinators — is the minimum fault-tolerant deployment.

**Replicated state.** The coordinator group replicates three categories of state:

1. **Membership** — which nodes are live, their capability policies and core counts.
2. **In-flight task table** — `task_id → (node_id, deadline)` — needed to re-dispatch if the assigned node fails.
3. **Content-addressed result cache index** — cache keys and which node holds each value.

All state changes commit as tinct dicts in the replication log. Log entries are tinct values — they can be inspected and processed using the language's own builtins. The implementation uses Raft for leader election and log replication; this is an implementation detail, not a semantic constraint.

**Fencing.** Every `TaskRequest` carries the current leader's term (a monotonically increasing integer). A worker that receives a request from a stale leader (lower term than its last-seen) rejects it. This prevents split-brain double-dispatch during a leadership transition.

**Client connection.** `connect-cluster` connects to any node in the cluster. That node serves the request (if leader) or redirects to the current leader. The `Cluster` handle stores a peer list for reconnection after leader rotation. `cluster-join` adds a running node to an existing quorum; `cluster-bootstrap` forms a new quorum.

---

### Pool Process Model

A pool node is a persistent tinct runtime that hosts programs, not a transient compute substrate that a locally-running process borrows. Nodes are started with:

```sh
tinct pool --role coordinator --seeds "peer1:7777,peer2:7777"
tinct pool --role worker      --seeds "peer1:7777,peer2:7777"
```

Both roles run the same binary and can coexist on the same node (see Coordinator Group). The `--seeds` list is the initial contact for joining the quorum; any reachable seed suffices to introduce the new node to the current leader. For LAN clusters, `--discover mdns` enables automatic peer discovery without hardcoded addresses.

**Programs as pool objects.** Programs are submitted to the pool for evaluation. Any node or external client can submit a program; the coordinator parses, typechecks, and evaluates it, distributing thunks to workers as they become evaluable:

```tinct
# From within tinct: submit a program to a pool
prog: [pool-submit pool program-source]

# Inspect and manage running programs
running: [pool-list pool]
[pool-drain pool prog]   # wait for in-flight thunks to complete, then stop
[pool-stop  pool prog]   # cancel in-flight work and stop immediately
```

From the command line: `tinct pool submit --pool coordinator:7777 program.llt` submits a file to an existing pool.

**Namespaces.** Each submitted program runs in an isolated `EvalContext`. Its namespace is identified by the content hash of the program source — submitting the same program twice is idempotent; the second call returns the existing handle. Programs in different namespaces cannot directly access each other's bindings. Inter-program communication uses the pool's distributed channel registry.

**Rolling updates.** Because namespaces are isolated and tinct programs are pure, two versions of a program can run simultaneously at zero coordination cost:

1. Version 1 (`myapp@abc123`) is running and handling work.
2. Submit version 2 (`myapp@def456`) — it begins evaluating in a new namespace immediately.
3. Call `[pool-drain pool v1]` — no new work is routed to version 1; it completes its in-flight thunks.
4. Once drained, the namespace is removed from the coordinator's membership log.

This benefit applies to single-node pools too: a new version of a long-running evaluation can start alongside the old one, which completes rather than being killed.

---

### Capability Delegation

Three models, selected per call:

**Pure (default):** The node receives no capabilities. Any capability-requiring builtin call produces a capability error at the worker, propagated back as a task failure.

```tinct
[remote-task cluster [fn [let] [map [fn [let x] [* x 2]] data]]]
```

**Delegated:** Specific capabilities are granted in the task message. Nodes validate grants against their own policy before exercising them. A node configured to deny `NetCap` grants rejects the task with a policy error.

```tinct
[remote-task cluster worker-fn  caps: [DirCap "/shared/input" r]]
```

**Proxied:** Capability-requiring operations at the worker are forwarded back to the originating node as sub-requests. The worker has no direct resource access; the originator exercises all I/O on its behalf via a bidirectional RPC sub-protocol within the task's QUIC stream.

```tinct
[remote-task cluster worker-fn  caps: [DirCap "/shared/output" w]  proxy: true]
```

With `proxy: true`, the worker receives a `ProxiedCap` handle instead of a real capability. When the worker calls a builtin that requires the cap (e.g., `write-file`), the builtin serializes the operation as a `ProxyRequest { cap_id, op, args }` and sends it back to the coordinator over a side channel embedded in the task's QUIC stream. The coordinator forwards to the originating node, which executes the I/O and returns a `ProxyResponse`. The worker blocks on the round-trip for each proxied operation. This is slower than delegated caps but allows workers without local resource access to perform I/O on behalf of the originator.

---

### Content-Addressed Result Cache

Every remote task has a **cache key**: SHA-256 of the canonical tinct-native encoding of `{expr, env, args}`. The coordinator checks this cache before dispatching. A hit returns the result immediately without involving any worker.

```text
request → hash(thunk) → cache hit? → return cached
                       → cache miss? → dispatch → node evaluates → cache result → return
```

**Cache storage tiers:**

1. **In-node** — `DashMap<[u8; 32], TinctWire>` local to each node; lost on exit.
2. **Coordinator-replicated** — the cache index (key → node_id) is replicated in the Raft log; values are fetched from the holding node on demand. Survives leader rotation.
3. **Persisted** — the leader serializes the cache to a `DirCap` directory at shutdown and reloads on startup. Survives full cluster restart.
4. **External** (optional) — the transport layer can delegate `Ref` resolution to any configured store. This is not required; it is one possible implementation of the pluggable transport.

---

### Worker Protocol

The coordinator-worker protocol is a sequence of tinct dicts over the cluster's transport:

```tinct
# Task dispatch
[task-id: <uuid>  payload: <Thunk>  term: <Int>  caps: <List>  requires: <List@String>]
[task-id: <uuid>  result: <TinctWire-value>]
[task-id: <uuid>  error: <Error>]

# Health
[ping: <uuid>]
[pong: <uuid>  load: <Int>  active: <Int>  queued: <Int>  uptime-ms: <Int>]

# Membership — caps is list of capability name strings
[worker-hello:  node-id: <Str>  addr: <Str>  cores: <Int>  caps: <List@String>]
[worker-join:   node-id: <Str>  addr: <Str>]
[worker-leave:  node-id: <Str>]

# Channels
[chan-reg:   chan-id: <hash>  name: <Str>  capacity: <Int>  durable: <Bool>]
[chan-send:  chan-id: <hash>  value: <TinctWire>  term: <Int>]
[chan-recv:  chan-id: <hash>  consumer-id: <uuid>  term: <Int>]
[chan-ack:   chan-id: <hash>  consumer-id: <uuid>  term: <Int>]
[chan-drop:  chan-id: <hash>]
```

All messages are encoded in the tinct-native wire format. The protocol has no external schema: it is tinct dicts, and cluster management code can be written in tinct itself.

Workers are stateless between tasks. A worker that crashes mid-task causes the leader to re-dispatch after the deadline in the in-flight table expires. Workers register on startup with `worker-hello`; the coordinator adds the registration to the Raft log before acknowledging.

---

### Pool Channels

The existing `channel` primitive is intra-process. Pool channels extend the model to the full node cluster: any node in the pool can send to or receive from a pool channel, and the coordinator manages the buffer.

```tinct
# Anonymous pool channel (coordinator-buffered, ephemeral)
ch: [pool-channel pool]

# Named pool channel — stable across namespaces and programs
ch: [pool-channel pool "pipeline-queue"]

# Bounded — senders block when full (backpressure)
ch: [pool-channel pool "pipeline-queue"  capacity: 100]

# Durable — messages persisted in Raft log, survive coordinator restart
ch: [pool-channel-durable pool "pipeline-queue"]
```

All existing channel operations work unchanged on pool channels: `channel-send`, `channel-recv`, `channel-close`. Non-blocking variants for backpressure-aware code:

```tinct
ok:    [channel-try-send ch value]  # → Bool — false if full, never blocks
item:  [channel-try-recv ch]        # → Null | value — Null if empty, never blocks
```

**Named vs. anonymous.** A named channel is registered in the coordinator's membership log. Any program in the pool that knows the name can reference the same channel — including programs in different namespaces. This is the mechanism for inter-program communication (e.g., an HTTP server namespace writing to a channel that a processor namespace reads from). An anonymous channel is referenced only by handle and is GC'd when all handles are dropped.

**Ephemeral vs. durable.** Ephemeral channels (default) hold messages in the coordinator's in-memory buffer. If the coordinator restarts, in-flight messages are lost. Durable channels write each message to the Raft log before acknowledging the sender. Consumers explicitly acknowledge receipt (`channel-ack`); unacknowledged messages are redelivered on reconnect, providing at-least-once delivery.

```tinct
# Durable consumer: acknowledge after processing
item: [channel-recv durable-ch]
[process item]
[channel-ack durable-ch]   # message removed from log
```

**Wire protocol additions:**

```tinct
[chan-reg:    chan-id: <hash>  name: <Str>  capacity: <Int>  durable: <Bool>]
[chan-send:   chan-id: <hash>  value: <TinctWire>  term: <Int>]
[chan-recv:   chan-id: <hash>  consumer-id: <uuid>  term: <Int>]
[chan-ack:    chan-id: <hash>  consumer-id: <uuid>  term: <Int>]
[chan-drop:   chan-id: <hash>]
```

A channel's ID is the SHA-256 of its registration parameters (name, capacity, durable flag); anonymous channels use a UUID. All messages use the tinct-native wire format.

---

### Node Topology and Placement

A program running in the pool can inspect the pool's membership and explicitly control where computations run.

```tinct
# NodeRef fields: {id: Str  addr: Str  cores: Int  load: Int  caps: [Seq Str]}
# caps is the list of capability names the node declared at join time (not the cap values)

nodes:  [pool-nodes pool]       # → [Seq NodeRef] — all live nodes
this:   [pool-this-node pool]   # → NodeRef — the node executing this thunk
leader: [pool-leader pool]      # → NodeRef — the current Raft leader
```

`pool-this-node` inside a `remote-task` body returns the node where that body is running. This enables self-aware tasks: a task can learn its own location and make decisions accordingly (e.g., discovering which local capabilities are available).

**Explicit placement with `on-node`.** `remote-task` is load-balanced; the coordinator picks the worker. `on-node` pins a task to a specific node:

```tinct
# Pin a task to a specific node — same return type as remote-task
result: [on-node pool node-ref [fn [let] [do-work data]]]   # → Task@T
```

The coordinator validates that the target node is live and routes the thunk directly. If the node is unreachable, `on-node` fails immediately rather than re-dispatching elsewhere — the caller chose this node for a reason (data locality, capability, etc.). Use `remote-task` when placement doesn't matter; use `on-node` when it does.

**Common pattern: route to the node with a specific capability.**

```tinct
# Find nodes that have declared access to db.internal
db-nodes: [filter [fn [let n] [elem "NetCap:db.internal" n.caps]] [pool-nodes pool]]
db-node:  [first db-nodes]

# Pin the task there
result: [on-node pool db-node [fn [let] [query-database ...]]]
```

---

### Capability Routing

Nodes declare their local capability names at join time in `worker-hello`. A capability name is a string encoding the type and key: `"NetCap:db.internal"`, `"DirCap:/var/data"`. The actual capability object stays local to the node — the declaration is an administrative claim, not a cryptographic proof (appropriate for a trusted cluster under unified administrative control).

```tinct
# Updated worker-hello protocol message
[worker-hello  node-id: "b"  cores: 8
  caps: ["NetCap:db.internal" "DirCap:/var/data"]]
```

`remote-task` gains an optional `requires:` field for automatic routing to a capable node:

```tinct
# Coordinator routes only to nodes that declared "NetCap:db.internal"
result: [remote-task pool [fn [let] [query-db ...]]  requires: ["NetCap:db.internal"]]
```

If no live node has declared all required capabilities, `remote-task` fails immediately with `no-capable-node` rather than queuing indefinitely. This is distinct from capability *delegation* (the coordinator passing a cap value in the task message). Capability routing selects *which node* runs the task; capability delegation grants *authority* to a node that otherwise wouldn't have it. Both can be used together:

```tinct
# Route to a node that has db access AND delegate a specific dir cap
result: [remote-task pool fn
  requires: ["NetCap:db.internal"]
  caps:     [DirCap "/shared/output" rw]]
```

---

### Capability Lifecycle

Capabilities are not static declarations. Nodes acquire and lose capabilities at runtime — database connection pools change, mounts come and go, external services fail over. The coordinator tracks three states per (node, capability) pair:

```text
absent → active → draining → absent
           ↑                     |
           └─────────────────────┘  (re-acquired)
```

**Active**: coordinator routes matching tasks to this node.
**Draining**: coordinator stops routing new tasks here; in-flight tasks complete; node confirms with `cap-lost` when done.
**Absent**: not routed. Tasks queued with `cap-timeout` (see Backpressure) wait here until a node re-enters active.

Nodes do not announce a "pending" state while warming up (establishing connections, running preflight checks). They simply call `pool-cap-add` once they are ready. The coordinator never sees the warmup period.

**Node-driven capability management.** `pool-cap-add` and `pool-cap-drain` are builtins that a program calls on behalf of its own node. Capability management is itself a tinct program, reacting to whatever external signals the node cares about:

```tinct
# Running on each db-adjacent node — monitors health, updates pool membership
[pool: [connect-cluster net-cap "tinct://pool:7777"]]
[task [loop [fn [let]
  [status: [fetch net-cap db-health-url].body]
  [match status.state
    [case "healthy"   [pool-cap-add   pool "NetCap:db.primary"]]
    [case "degraded"  [pool-cap-drain pool "NetCap:db.primary"]]
    [case "gone"      [pool-cap-drain pool "NetCap:db.primary"  deadline: [seconds 5]]]]
  [sleep [seconds 1]]]]]
```

`pool-cap-drain` without a `deadline` waits indefinitely for in-flight tasks to complete. With a `deadline`, tasks still running at the deadline are re-dispatched to other capable nodes or failed if none exist.

**Coordinator-side administrative override.** An administrative program can force a node into draining regardless of the node's own state — useful for maintenance or security incidents:

```tinct
[pool-cap-revoke pool node-ref "NetCap:db.primary"  deadline: [seconds 30]]
```

**Subscribing to capability events.** Any program in the pool can subscribe to a stream of capability changes for a glob pattern:

```tinct
events: [pool-cap-events pool "NetCap:db.*"]
# → Channel@{type: Str  node: NodeRef  cap: Str}
# type: "gained" | "draining" | "lost"

# Reactive connection pool — tracks which nodes can reach the database
[task [loop [fn [let]
  [ev: [channel-recv events]]
  [match ev.type
    [case "gained"   [on-node pool ev.node [fn [let] [warm-connection ev.cap]]]]
    [case "draining" [stop-routing-to ev.node ev.cap]]
    [case "lost"     [close-connection ev.node ev.cap]]]]]]
```

Events arrive in Raft log order — totally ordered, no duplicates. A program that subscribes mid-run receives a synthetic `"gained"` event for every currently active capability matching the pattern before receiving live updates, so it never misses the current topology.

**`requires:` semantics.** The list is AND: every listed cap must be present on the same node. Within each string, glob patterns are matched against the coordinator's live capability index:

```tinct
# AND: node must have both
requires: ["NetCap:db.primary"  "DirCap:/tmp/scratch"]

# Glob: any db replica qualifies
requires: ["NetCap:db.replica-*"]
```

Prioritised fallback (try primary, fall back to replica) is application logic built on `remote-task-try`, not a protocol feature:

```tinct
result: [or
  [remote-task-try pool fn  requires: ["NetCap:db.primary"]]
  [remote-task     pool fn  requires: ["NetCap:db.replica-*"]]]
```

The coordinator resolves glob patterns against a `Map<CapName, Set<NodeId>>` derived from the Raft log, updated atomically on each `cap-gained` / `cap-lost` entry.

**Wire protocol additions:**

```tinct
[cap-gained:   node-id: <Str>  cap: <Str>  term: <Int>]
[cap-draining: node-id: <Str>  cap: <Str>  deadline: <Int>  term: <Int>]
[cap-lost:     node-id: <Str>  cap: <Str>  term: <Int>]
[cap-revoke:   node-id: <Str>  cap: <Str>  deadline: <Int>  term: <Int>]  # coordinator → node
```

All four are Raft log entries — capability state is replicated and survives leader rotation.

---

### Backpressure

`remote-task` blocks until a worker is available. For programs that need to stay responsive under load, non-blocking and timeout variants are available:

```tinct
# Default — blocks until a worker accepts the task
task: [remote-task pool fn]

# Non-blocking — returns Err immediately if the pool queue is full
result: [remote-task-try pool fn]    # → [or [Ok [Task T]] [Err String]]

# Timeout — returns Err if no worker accepts within the deadline
task: [remote-task pool fn  timeout: [seconds 5]]

# Cap-timeout — waits for a capable node to become available (see Capability Lifecycle)
# Covers the window during failover when no node currently has the required cap
task: [remote-task pool fn  requires: ["NetCap:db.primary"]  cap-timeout: [seconds 10]]
```

The coordinator's task queue has a configurable maximum depth set at pool startup (`--queue-depth N`). When the queue is full, `remote-task` (default) causes the submitting task to yield in Tokio until space opens. This is cooperative backpressure: the pool slows down submitters rather than silently dropping work.

Programs that need to make their own load-shedding decisions can query the pool directly:

```tinct
load: [pool-load pool]
# → {queued: Int  active: Int  workers: Int  utilization: Float}

# Example: shed load by returning a cached result if pool is saturated
result: [if [> load.utilization 0.9]
  [cached-response]
  [await [remote-task pool fn]]]
```

---

### `dist-map`: Distributed Data-Parallel Map

```tinct
# stdlib/dist.llt
dist-map: [fn [let cluster f seq]
  [n:       [pool-load cluster].workers]
  [shards:  [partition n seq]]
  [tasks:   [map [fn [let s] [remote-task cluster [fn [let] [map f s]]]] shards]]
  [results: [await-all tasks]]
  [flatten results]]
```

`dist-filter` and `dist-reduce` follow the same pattern. `dist-reduce` with an associative combinator uses tree reduction: workers reduce local shards; the coordinator combines partial results. These live in `stdlib/dist.llt`, alongside `partition` (split seq into n roughly-equal shards — distinct from `seq.llt`'s predicate-based `partition`).

**Dynamic load balancing.** Static equal-partition (`partition` by worker count) fails badly when shard costs are heterogeneous — one slow shard serializes the entire `dist-map`. `dist-map` therefore uses a work-stealing scheduler by default: the coordinator maintains a shard queue; workers pull from it as they become free (pmap-style). `partition n seq` is used only when `--static-sharding` is set explicitly. The worker count (`[pool-load cluster].workers`) sets the initial parallelism budget; the actual number of shards submitted is `max(n, shard-count)` where `shard-count` is user-configurable (default: `4 × workers`, giving 4× over-decomposition for load balance).

**Promise pipelining** (built-in via `GlobalThunkRef`). The combiner step of `dist-map` is submitted as a `remote-task` immediately — its input `results` is a Seq of `GlobalThunkRef` entries, not a materialized Seq. The combiner wakes incrementally as shards complete. For pipeline-shaped computations (output of one `dist-map` fed into another), the second stage's tasks receive `GlobalThunkRef` entries from the first stage and pipeline automatically — no coordinator involvement in the data transfer between stages.

### Automatic Distribution

When a `Cluster` handle is in scope and automatic distribution is enabled, the scheduler transparently applies `remote-task` to large pure independent dict entries:

```tinct
[connect-cluster net-cap "tinct://host:7777"]
---
[
  a: [pure-expensive-1 huge-input]   # dispatched to remote node automatically
  b: [pure-expensive-2 huge-input]   # dispatched to remote node automatically
  c: [merge a b]
]
```

Criteria: the entry is unevaluated, the expression is pure (no capability references in the AST), all inputs are materialized or are `Ref@T` values, and estimated cost exceeds a configurable threshold. Auto-distribution is disabled per-scope with `[no-distribute expr]`.

---

## Evaluation Across the Full Spectrum

| Scope | Mechanism | Unit |
|-------|-----------|------|
| Single thunk | `materialize` async | One `.await` |
| Dict entries | `JoinSet` fanout | One Tokio task per entry |
| Explicit tasks | `task` / `par` | One Tokio spawn |
| Multi-core | Multi-thread Tokio | OS thread pool |
| Cross-node live | `GlobalThunkRef` DSM demand-fetch | One network request per demanded ref |
| Multi-node pure | `remote-task` / `dist-map` | One worker node per shard |
| Federated | Cross-cluster coordinator bridges | One cluster per region |

Semantics are identical at every level: a thunk evaluates exactly once; independent thunks have no ordering constraint; dependent thunks wait.

---

## What Would Change

### New Builtins

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `connect-cluster` | `NetCap → Str → Cluster` | Connect to any node in an existing pool. |
| `cluster-local` | `Dict → Cluster` | In-process worker pool (no network). |
| `cluster-store` | `Cluster → any → Ref@T` | Store a value in the pool; returns a content-addressed `Ref`. |
| `remote-task` | `Cluster → [Fn@T []] → Task@T` | Submit a thunk; coordinator routes to an available (and capable) worker. |
| `remote-task-try` | `Cluster → [Fn@T []] → [or [Ok [Task T]] [Err Str]]` | Non-blocking submit; returns `Err` immediately if pool queue is full. |
| `on-node` | `Cluster → NodeRef → [Fn@T []] → Task@T` | Pin a thunk to a specific node; fails if node is unreachable. |
| `pool-nodes` | `Cluster → [Seq NodeRef]` | All live nodes in the pool with their metadata. |
| `pool-this-node` | `Cluster → NodeRef` | The node currently executing this thunk. |
| `pool-leader` | `Cluster → NodeRef` | The current Raft leader. |
| `pool-load` | `Cluster → Dict` | Pool load: `{queued, active, workers, utilization}`. |
| `pool-channel` | `Cluster → Channel@T` | Anonymous ephemeral cross-node channel. |
| `pool-channel` | `Cluster → Str → Channel@T` | Named ephemeral channel; stable across namespaces. |
| `pool-channel` | `Cluster → Str → Dict → Channel@T` | Named channel with options (`capacity`, `durable`). |
| `pool-channel-durable` | `Cluster → Str → Channel@T` | Named durable channel; messages persisted in Raft log. |
| `channel-try-send` | `Channel@T → T → Bool` | Non-blocking send; returns false if channel is full. |
| `channel-try-recv` | `Channel@T → [or Null T]` | Non-blocking recv; returns Null if channel is empty. |
| `channel-ack` | `Channel@T → Null` | Acknowledge receipt on a durable channel. |
| `pool-cap-add` | `Cluster → Str → Null` | Announce that this node has gained a named capability. |
| `pool-cap-drain` | `Cluster → Str → Null` | Begin graceful relinquishment; stops new routing, waits for in-flight tasks. |
| `pool-cap-drain` | `Cluster → Str → Dict → Null` | Drain with options: `deadline: Duration` for forced completion. |
| `pool-cap-revoke` | `Cluster → NodeRef → Str → Dict → Null` | Admin override: force a node into draining for a given cap. |
| `pool-cap-events` | `Cluster → Str → Channel@Dict` | Subscribe to capability lifecycle events matching a glob pattern. |
| `pool-submit` | `Cluster → Str → ProgramHandle` | Submit a tinct program (source string) to the pool for evaluation. |
| `pool-list` | `Cluster → [Seq ProgramHandle]` | List all active program namespaces in the pool. |
| `pool-drain` | `Cluster → ProgramHandle → Task@Null` | Signal no new work; wait for in-flight thunks to complete. |
| `pool-stop` | `Cluster → ProgramHandle → Null` | Cancel in-flight work and remove the namespace immediately. |
| `distributable?` | `any → Bool` | True if value contains no capabilities; live thunks are allowed (they become `GlobalThunkRef` entries). |
| `dsm-stats` | `Cluster → Dict` | DSM cache stats for this node: `{cached, pending, demand-sent, demand-recv, promoted}`. |

`dist-map`, `dist-filter`, `dist-reduce`, `partition` live in `stdlib/dist.llt`. `partition n seq` (split into n equal shards) lives in `stdlib/dist.llt` not `stdlib/seq.llt`, to distinguish it from `seq.llt`'s predicate-based partition.

### New Types

- `Type::Cluster` — opaque pool handle.
- `Type::Ref(Box<Type>)` — content-addressed reference; `Ref@T` in source syntax. Unifies transparently with `T` in the type checker; distinct only in the wire format and at runtime.
- `Type::NodeRef` — opaque reference to a pool node; has known fields `id`, `addr`, `cores`, `load`, `caps` (row type, so the type checker allows dot access).
- `Type::ProgramHandle` — opaque handle to a submitted program namespace.

`GlobalThunkRef` is not a user-visible type. It exists only in the wire format (tag `0x0E`) and in `UnevaluatedState::Remote` inside the evaluator. Tinct code never names or inspects `GlobalThunkRef` directly — free variables in distributed tasks are transparent references resolved by the DSM layer.

`remote-task cluster fn@[Fn@T []]` infers `Task@T`. `cluster-store cluster v@T` infers `Ref@T`. `distributable?` is `any → Bool` (runtime check; the capability check walks the `CoreExpr` tree at `remote-task` submission time).

### Serialization (`src/serialize.rs` — new)

Tinct-native binary encoding: tag byte dispatch over the value enum, varint integers, length-prefixed strings and collections. `encode(value: &Value) -> Bytes` and `decode(bytes: &[u8]) -> Result<Value, DecodeError>`. No JSON path for distributed tasks.

Four encoding paths:

- **Primitive values** (`Null`, `Bool`, `Int`, `Float`, `String`, `List`, `Dict`, `Error`, `Ref`) — direct encoding.
- **Thunk/Closure** (`0x08`/`0x09`) — `CoreExpr` encoded by `core_expr_to_wire`; `core_expr_from_wire` decodes on the worker. Environment Dict entries use concrete wire values or `GlobalThunkRef` (tag `0x0E`).
- **GlobalThunkRef** (`0x0E`) — u32 node-id-string length + UTF-8 bytes + u64 thunk-id. Valid only within a live program run; must not be persisted.
- **AST values** (`0x0B`–`0x0D`) — encoded via `ast_to_dict_expr` (retained from `runtime-v2`); decoded via a new `wire_to_surface_node` (expression-level, distinct from the deleted file-level `dict_to_ast`).

**Estimated:** ~550 lines.

### DSM Layer (`src/dsm.rs` — new)

`DsmClient` manages the `GlobalThunkRef` demand-fetch protocol for a single node.

```rust
pub struct DsmClient {
    cache:     DashMap<GlobalThunkRef, DsmEntry>,
    transport: Arc<ClusterTransport>,
    self_id:   NodeId,
}

enum DsmEntry {
    Pending(Vec<Waker>),
    Ready(Result<Value, Arc<EvalError>>),
}
```

**Public interface:**

- `async fn demand(thunk_ref: GlobalThunkRef, self_thunk_id: LocalThunkId) -> Result<Value, Arc<EvalError>>` — demand-fetch with CMH probe. Returns immediately if the result is already cached.
- `fn notify_ready(thunk_ref: GlobalThunkRef, result: Result<Value, Arc<EvalError>>)` — called by the transport when a `DemandResponse` arrives; wakes all waiters.
- `fn notify_node_failed(node_id: NodeId)` — called by the coordinator on heartbeat failure; wakes all waiters for that node's refs with `EvalError::node_failed`.
- `fn probe_forward(request: DemandRequest)` — called by the transport on demand request receipt; if this node is itself blocked on a `GlobalThunkRef`, forward the probe chain; otherwise force the thunk and respond.

The CMH probe chain is carried in `DemandRequest.probe_chain: SmallVec<[GlobalThunkRef; 8]>`. The 8-element stack allocation covers the common case of short chains without heap allocation.

**RDMA integration:** If `ClusterTransport` has RDMA capability, `DsmClient::demand` uses RDMA one-sided READ (via `async-rdma`) for large results after receiving a `Promoted(content_hash)` response — the node's memory region address is included in the response for RDMA-capable peers.

**Estimated:** ~450 lines.

### Distributed Cache (`src/dist_cache.rs` — new)

`DistributedCache`: `DashMap<[u8; 32], Arc<Value>>` keyed by SHA-256 of canonical tinct-native encoding of `{expr, env, args}`. `lookup(key)` and `store(key, value)`. Optional persistence to `DirCap` directory on shutdown/startup. The cache index (keys and holding-node IDs) is replicated in the Raft log; values are held locally and fetched on demand.

**Estimated:** ~200 lines.

### Coordinator Group (`src/coordinator.rs` — new)

Raft-based coordinator group: leader election, log replication for membership/task-table/cache-index. Raft messages are tinct dicts in the native wire format. The implementation uses an existing Raft library (e.g. `openraft`) rather than implementing consensus from scratch.

**Estimated:** ~600 lines (excluding Raft library).

### Worker Runtime (`src/worker.rs` — new)

`tinct --worker --join tinct://host:7777`. Connects via the cluster transport. Receives `TaskRequest` (a tinct dict containing a `Thunk`), deserializes, evaluates in a fresh `EvalContext`, serializes result, sends `TaskResult`. Validates capability grants. Registers with `worker-hello` on startup; the leader commits the registration to the log.

**Estimated:** ~400 lines; entirely reuses existing eval infrastructure.

### CLI (`src/main.rs`)

New subcommand: `tinct pool`. Flags: `--role [coordinator|worker]`, `--seeds <addr,...>`, `--discover [mdns]`, `--bootstrap` (form a new single-node pool), `--cluster-cache-dir`. Additional subcommand: `tinct pool submit --pool <addr> <file.llt>` to submit a program from the command line.

**Impact:** Moderate — new subcommand tree under `pool`.

### Dependencies (`Cargo.toml`)

- `dashmap` — concurrent hashmap for the result cache and DSM cache.
- `sha2` — SHA-256 for content-addressed cache keys.
- `uuid` — task IDs and node IDs in the worker protocol.
- `quinn` — already present; QUIC transport for cluster communication and DSM demand-fetch.
- `openraft` (or similar) — Raft consensus for the coordinator group.
- `async-rdma` — optional; RDMA kernel-bypass transport for large `Ref@T` resolution on InfiniBand/RoCE clusters. Falls back to QUIC peer-pull (tier 4) when unavailable.
- `smallvec` — zero-allocation CMH probe chains for short dependency depth (≤8 nodes).

---

## Prerequisites

- **`runtime-v2.md`** — mandatory (all three parts complete). Requires: `SurfaceExpr`/`CoreExpr` AST split; native `AstExpr`/`AstDoc`/`AstFile` value types (`0x0B`–`0x0D` wire tags); `Arc`-based `OnceLock` thunks; async `eval`/`materialize`; multi-thread Tokio runtime; `task`/`await`/`channel`/`context`/`ast-of` primitives. `include-decomp-*` is a transitive prerequisite (required by `runtime-v2`).
- **`ast_to_dict_expr`** — retained from `runtime-v2`; used by `src/serialize.rs` for `AstExpr`/`AstDoc`/`AstFile` wire encoding (`0x0B`–`0x0D`). The deleted `dict_to_ast` has no wire-format equivalent; `src/serialize.rs` provides its own `wire_to_surface_node` and `core_expr_from_wire` decoders.
- **QUIC** — already present (`quinn`); one possible cluster transport.
- **`error-patterns.md`** — `remote-task` returns `Task@(Ok@T | Err@String)`.

---

## References

- Dean, J. & Ghemawat, S. (2004). "MapReduce: Simplified Data Processing on Large Clusters." *OSDI '04*, pp. 137–150. — `dist-map` is a first-class MapReduce instance.
- Zaharia, M. et al. (2012). "Resilient Distributed Datasets: A Fault-Tolerant Abstraction for In-Memory Cluster Computing." *NSDI '12*. — Spark's lazy lineage model; tinct's `[map fn seq]` over a large dataset is semantically a lazy RDD transformation.
- Dolstra, E., de Jonge, M. & Visser, E. (2004). "Nix: A Safe and Policy-Free System for Software Deployment." *LISA '04*. — Content-addressed storage; tinct's `Ref@T` applies the same hash-of-inputs scheme at value granularity, with pluggable resolution.
- Isard, M., Budiu, M., Yu, Y., Birrell, A. & Fetterly, D. (2007). "Dryad: Distributed Data-Parallel Programs from Sequential Building Blocks." *EuroSys '07*. — DAG-based distributed execution from sequential code; tinct's thunk graph maps directly to Dryad's computation DAG.
- Armstrong, J. (2003). "Making Reliable Distributed Systems in the Presence of Software Errors." PhD thesis, KTH. — Erlang's share-nothing process model; tinct's pure-only distributed tasks apply the same isolation.
- Ongaro, D. & Ousterhout, J. (2014). "In Search of an Understandable Consensus Algorithm." *USENIX ATC '14*. — Raft; the implementation basis for the coordinator group's leader election and log replication.
- Weil, S.A. et al. (2006). "Ceph: A Scalable, High-Performance Distributed File System." *OSDI '06*. — Homogeneous node model where every storage node participates in cluster decisions; tinct's pool (every node runs both worker and coordinator) follows the same architecture.
- Miller, M.S., Tribble, E.D. & Shapiro, J. (2005). "Concurrency Among Strangers." *TGC '05*. — E language's promise pipelining and vat/event-loop model; direct basis for `GlobalThunkRef` demand-fetch and tinct's capability delegation design.
- Epstein, J., Black, A.P. & Peyton Jones, S. (2011). "Towards Haskell in the Cloud." *Haskell Symposium '11*. — Cloud Haskell's `Closure` type and `Static` pointer mechanism; the force-before-send requirement motivates the `GlobalThunkRef` model as the looser alternative.
- Chandy, M.M. & Misra, J. (1983). "Distributed deadlock detection." *ACM TOPLAS 5(1)*. — CMH probe algorithm; direct basis for §Distributed Cycle Detection.
- Kshemkalyani, A.D. & Singhal, M. (1994). "Efficient detection and resolution of generalized distributed deadlocks." *IEEE TSE 20(1)*. — Path-doubling optimization for O(log n) probe rounds in functional-graph WFGs.
- Knapp, E. (1987). "Deadlock detection in distributed databases." *ACM Computing Surveys 19(4)*. — Comprehensive survey of AND/OR WFG models; confirms tinct's functional-graph (max out-degree 1) as the simplest tractable case.
- See `dist-eval-survey.md` in this directory. — Eight-language survey (Unison, Erlang/OTP, Cloud Haskell, Oz/Mozart, E, Chapel, Futhark, Julia) confirming: distributable-thunk condition, thunk-as-task-message, content-addressed caching, promise pipelining, and dynamic load balancing.
