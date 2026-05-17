# What If: Distributed Evaluation for tinct

**State:** Proposal

**Depends on:** `async-eval.md` — requires `eval`/`materialize` to be `async fn`, `Arc`-based thunks, and the multi-thread Tokio runtime to be complete before distributed work is possible.

What would it take to evaluate tinct programs across a cluster of machines — transparently, with content-addressed caching so that identical computations never run twice?

## Current State

As of `async-eval.md`, tinct evaluation spans a single machine: async I/O, cooperative concurrency, multi-core parallel dict evaluation, `par`/`par-map`, `task`/`await`/`channel`. The remaining gap is scale: a large dataset that exceeds a single machine's memory or throughput cannot be processed in one tinct process.

### What's Missing

1. No way to send a computation to a remote node and await its result.
2. No content-addressed cache that persists results across runs or shares them across nodes.
3. No cluster membership, leader election, or fault-tolerant dispatch.
4. No distributed equivalents of `map`/`filter`/`reduce` over large datasets.

## Why Distributed Evaluation Matters for tinct

tinct's thunk graph is an implicit parallel execution plan. Every thunk with no data dependency on another is, by definition, safe to evaluate anywhere — on any core, on any machine. The `Arc`-based `OnceLock` thunk model (from `async-eval.md`) already makes this safe within a process. Distributed evaluation extends the same model across process boundaries.

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
- `Ref@T` is a content-addressed value that the cluster's transport layer resolves on demand.
- A thunk evaluates exactly once; independent thunks have no ordering constraint; dependent thunks wait. These semantics are identical whether evaluation runs on one core, many cores, or many machines.

**Implementation** — what a specific cluster provides:
- The transport (QUIC, TCP, shared memory, a mounted filesystem).
- The replication and leader-election mechanism.
- The storage backend for the result cache.
- The strategy for resolving `Ref@T` values.

A program written against `Cluster`/`remote-task`/`Ref` works without modification on an in-process worker pool, a three-node cluster over QUIC, or a larger deployment with a shared cache — because it expresses *what* to compute, not *how* to move data.

---

### The Distributable Thunk

A thunk is **distributable** if:
1. Its expression references no capability values (`DirCap`, `NetCap`, `Handle`, `Channel`, etc.).
2. Every free variable in its environment is a fully materialized concrete value — no live thunks, no capability references.

The serialized form of a distributable thunk is the thunk itself, encoded in the tinct-native wire format (see below). There is no translation step and no intermediate representation: the thunk *is* the task message. The worker receives it, evaluates it in a fresh `EvalContext`, and returns the result — also in the wire format.

### `remote-task`

```tinct
# Connect to a cluster — returns a Cluster handle
cluster: [connect-cluster net-cap "tinct://coordinator.internal:7777"]

# Or an in-process cluster (no network; uses local cores only)
cluster: [cluster-local [workers: 8]]

# Submit work — returns Task@T, same type as task
shard1: [remote-task cluster [fn [] [map transform data-shard-1]]]
shard2: [remote-task cluster [fn [] [map transform data-shard-2]]]
shard3: [remote-task cluster [fn [] [map transform data-shard-3]]]

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
| `0x08` | Thunk (Expr encoded as tinct dict + Env encoded as Dict) |
| `0x09` | Closure (same as Thunk; distinguished for type-checking) |
| `0x0A` | Ref (32-byte content hash + type tag) |

`Thunk` is the central tag for distributed evaluation: `remote-task` serializes the closure as a `Thunk` and sends it. The worker deserializes a `Thunk`, evaluates it, and returns any other tag as the result. The format is the execution protocol.

`Expr` encodes via `ast_to_dict` (already implemented in `ast-dict-core`), wrapped in a `Dict` value — so AST serialization reuses the existing machinery rather than defining a separate encoding.

The format makes no distinction between "task message" and "value": every message in the worker protocol is a tinct dict encoded in this format. The Raft log entries (see Coordinator Group below) are tinct dicts. Cluster state is introspectable using the language's own builtins.

---

### `Ref@T`: Transport-Agnostic Large Values

`Ref@T` is a content-addressed reference to a value. It is a first-class value in the wire format and in the evaluator: a `Ref` carries a 32-byte content hash and a type. When an expression requires the underlying value, the evaluator's transport layer resolves it.

```tinct
# Store a large value in the cluster — returns Ref@Dict
input-ref: [cluster-store cluster huge-dict]

# The ref can be passed to remote tasks; the worker fetches it when needed
result: [remote-task cluster [fn [] [process input-ref]]]
```

The transport layer's resolution strategy is implementation-defined:
- Check the local thunk cache (fastest — value already computed on this node).
- Pull from the current leader's cache.
- Pull from whichever node last computed the value (peer-to-peer, if the cluster tracks this).
- Read from a mounted shared filesystem or any other configured store.

The language specifies only the contract: given a hash, produce a value or fail. Programs are written against `Ref@T`; they do not know or care how resolution works.

**Type system interaction:** The type checker distinguishes `Ref@T` from `T`. A `Ref@Int` cannot be used as an `Int` without resolution. In practice the evaluator resolves `Ref` transparently when forcing a thunk that contains one — analogous to how it forces a nested thunk. Whether `Ref@T` and `T` unify in the type checker or require explicit `[deref ref]` is an open question; the simpler path (transparent forcing, type checker treats them as equivalent) is preferable unless explicit control proves necessary.

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

### Capability Delegation

Three models, selected per call:

**Pure (default):** The node receives no capabilities. Any capability-requiring builtin call produces a capability error at the worker, propagated back as a task failure.

```tinct
[remote-task cluster [fn [] [map [fn [x] [* x 2]] data]]]
```

**Delegated:** Specific capabilities are granted in the task message. Nodes validate grants against their own policy before exercising them. A node configured to deny `NetCap` grants rejects the task with a policy error.

```tinct
[remote-task cluster worker-fn  caps: [DirCap "/shared/input" r]]
```

**Proxied (future):** Capability-requiring operations at the worker are forwarded back to the originating node as sub-requests. The worker has no direct resource access; the originator exercises all I/O on its behalf. Requires a bidirectional RPC sub-protocol within the task channel.

---

### Content-Addressed Result Cache

Every remote task has a **cache key**: SHA-256 of the canonical tinct-native encoding of `{expr, env, args}`. The coordinator checks this cache before dispatching. A hit returns the result immediately without involving any worker.

```
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
# TaskRequest
[task-id: <uuid-str>  payload: <Thunk>  term: <Int>  caps: <List>]

# TaskResult — success
[task-id: <uuid-str>  result: <TinctWire-value>]

# TaskResult — failure
[task-id: <uuid-str>  error: <Error>]

# Ping / Pong
[ping:  <uuid-str>]
[pong:  <uuid-str>  load: <Int>  uptime-ms: <Int>]

# Membership
[worker-hello:  node-id: <Str>  cores: <Int>  caps: <List>]
[worker-join:   node-id: <Str>  addr: <Str>]
[worker-leave:  node-id: <Str>]
```

All messages are encoded in the tinct-native wire format. The protocol has no external schema: it is tinct dicts, and cluster management code can be written in tinct itself.

Workers are stateless between tasks. A worker that crashes mid-task causes the leader to re-dispatch after the deadline in the in-flight table expires. Workers register on startup with `worker-hello`; the coordinator adds the registration to the Raft log before acknowledging.

---

### `dist-map`: Distributed Data-Parallel Map

```tinct
# stdlib/dist.llt
dist-map: [fn [cluster f seq]
  [let
    [n:       cluster.worker-count
     shards:  [partition n seq]
     tasks:   [map [fn [s] [remote-task cluster [fn [] [map f s]]]] shards]
     results: [await-all tasks]]
    [flatten results]]]
```

`dist-filter` and `dist-reduce` follow the same pattern. `dist-reduce` with an associative combinator uses tree reduction: workers reduce local shards; the leader combines partial results. These live in `stdlib/dist.llt`.

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
| Multi-node | `remote-task` / `dist-map` | One node per shard |
| Federated (future) | Cross-cluster coordinators | One cluster per region |

Semantics are identical at every level: a thunk evaluates exactly once; independent thunks have no ordering constraint; dependent thunks wait.

---

## What Would Change

### New Builtins

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `connect-cluster` | `NetCap → Str → Cluster` | Connect to any node in an existing cluster. |
| `cluster-local` | `Dict → Cluster` | In-process worker pool (no network). |
| `cluster-bootstrap` | `NetCap → Dict → Cluster` | Form a new single-node cluster; others join via `cluster-join`. |
| `cluster-join` | `NetCap → Str → Null` | Join this node to an existing cluster quorum. |
| `cluster-store` | `Cluster → any → Ref@T` | Store a value in the cluster; returns a content-addressed `Ref`. |
| `remote-task` | `Cluster → Fn@[]@T → Task@T` | Submit a thunk to the cluster. Returns `Task@T`. |
| `distributable?` | `any → Bool` | True if value contains no capabilities and no live thunks. |
| `worker-serve` | `NetCap → Int → Null` | Run this process as a cluster worker on the given port. |

`dist-map`, `dist-filter`, `dist-reduce`, `partition` live in `stdlib/dist.llt`.

### New Types

- `Type::Cluster` — opaque cluster handle.
- `Type::Ref(Box<Type>)` — content-addressed reference; `Ref@T` in source syntax.

`remote-task cluster fn@Fn@[]@T` infers `Task@T` from the closure return type. `cluster-store cluster v@T` infers `Ref@T`. `distributable?` is `any → Bool` (runtime check; static purity analysis is future work).

### Serialization (`src/serialize.rs` — new)

Tinct-native binary encoding: tag byte dispatch over the value enum, varint integers, length-prefixed strings and collections. `encode(value: &Value) -> Bytes` and `decode(bytes: &[u8]) -> Result<Value, DecodeError>`. AST encoding reuses `ast_to_dict`/`dict_to_ast` — AST values are `Dict` in the wire format. No JSON path for distributed tasks.

**Estimated:** ~400 lines.

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

New flags: `--worker`, `--coordinator`, `--join`, `--bootstrap`, `--cluster-cache-dir`.

**Impact:** Minor.

### Dependencies (`Cargo.toml`)

- `dashmap` — concurrent hashmap for the result cache.
- `sha2` — SHA-256 for cache keys.
- `uuid` — task IDs in the worker protocol.
- `quinn` — already present; QUIC transport for cluster communication.
- `openraft` (or similar) — Raft consensus for the coordinator group.

---

## Prerequisites

- **`async-eval.md`** — mandatory. `Arc`-based thunks, async `eval`/`materialize`, multi-thread Tokio runtime, `task`/`await`/`channel`/`context` all required before distribution makes sense.
- **`ast_to_dict` / `dict_to_ast`** — required for thunk serialization. Already implemented (`ast-dict-core` sprint).
- **QUIC / `Http3Session`** — already implemented; used as one possible cluster transport.
- **`runtime-reflection.md`** — `ast-of` provides function-body-to-dict for serializing closures.
- **`error-patterns.md`** — `remote-task` returns `Task@(Ok@T | Err@String)`.

---

## References

- Dean, J. & Ghemawat, S. (2004). "MapReduce: Simplified Data Processing on Large Clusters." *OSDI '04*, pp. 137–150. — `dist-map` is a first-class MapReduce instance.
- Zaharia, M. et al. (2012). "Resilient Distributed Datasets: A Fault-Tolerant Abstraction for In-Memory Cluster Computing." *NSDI '12*. — Spark's lazy lineage model; tinct's `[map fn seq]` over a large dataset is semantically a lazy RDD transformation.
- Dolstra, E., de Jonge, M. & Visser, E. (2004). "Nix: A Safe and Policy-Free System for Software Deployment." *LISA '04*. — Content-addressed storage; tinct's `Ref@T` applies the same hash-of-inputs scheme at value granularity, with pluggable resolution.
- Isard, M., Budiu, M., Yu, Y., Birrell, A. & Fetterly, D. (2007). "Dryad: Distributed Data-Parallel Programs from Sequential Building Blocks." *EuroSys '07*. — DAG-based distributed execution from sequential code; tinct's thunk graph maps directly to Dryad's computation DAG.
- Armstrong, J. (2003). "Making Reliable Distributed Systems in the Presence of Software Errors." PhD thesis, KTH. — Erlang's share-nothing process model; tinct's pure-only distributed tasks apply the same isolation.
- Ongaro, D. & Ousterhout, J. (2014). "In Search of an Understandable Consensus Algorithm." *USENIX ATC '14*. — Raft; the implementation basis for the coordinator group's leader election and log replication.
- Weil, S.A. et al. (2006). "Ceph: A Scalable, High-Performance Distributed File System." *OSDI '06*. — Homogeneous node model where every storage node participates in cluster decisions; tinct's coordinator group (every node runs both worker and coordinator) follows the same architecture.
