# Distributed Computation Survey: Design Decisions Across Eight Languages

**Purpose:** Design-level survey to inform tinct's distributed evaluation (`dist-eval.md`). Covers
Unison, Erlang/OTP, Cloud Haskell, Oz/Mozart, E, Chapel, Futhark, and Julia Distributed. For each
system: unit of computation, serialization, programmer interface, lazy evaluation interaction, caching,
fault tolerance, and lessons for tinct.

**Context:** tinct is a lazy functional configuration language (call-by-need thunks, HM + row
polymorphism, capability-separated I/O, content-addressed caching). Its proposed distributed model
uses tinct-native thunks as task messages over QUIC, Raft-based coordinator groups, and SHA-256
content-addressed result caching. This survey interrogates whether that design converges with or
diverges from existing art.

---

## 1. Unison

### Unit of Computation

The fundamental unit is **the Unison definition**, identified by a SHA-256 hash of its abstract
syntax tree. The hash is computed over the canonical de-named form (positional variable references,
dependency hashes in place of names). A `Remote.transfer` sends a thunk — specifically, the
continuation after the `transfer` call, desugared as a `Unit -> Remote a` function — to a recipient
node. Any missing dependency hashes are synced on demand before execution begins.

### Serialization

Serialization is **implicit and structural**. Because every definition is stored in Unison's code
database as its hash-indexed AST, there is no separate serialization step: the AST *is* the portable
representation. The sender ships the bytecode tree; the recipient inspects it for unknown hashes,
requests missing ones, caches them, then executes. Functions, closures, and data all serialize via
the same mechanism — hash identity means a value that was computed on one node and cached there
can be referenced (by hash) on another node without copying the value itself, only its hash.

### Programmer Interface

The `Remote` ability (algebraic effect handler) is the programming surface. The programmer writes
`Remote.transfer nodeRef` to hop to a different node; the current continuation becomes the remote
computation. Map-reduce patterns are ordinary Unison library code built on top of `Remote.fork`,
`Remote.at`, and `Remote.supervise`. There is no separate RPC layer or serialization annotation.
The Volturno streaming system (2024) demonstrates real distributed pipelines where computation graphs
are represented as hashes of processing-stage functions, and coordination is done via persistent
keyed logs (KLog/KStream) with hash-based partitioning (`hash(key) % totalLoglets`).

### Lazy Evaluation vs. Distribution

Unison is call-by-value, not call-by-need. This sidesteps the lazy-vs-distribution tension entirely:
values are fully evaluated before they are hashed and shipped. There are no unevaluated thunks in
the wire protocol; the `Remote.transfer` continuation is a closed expression evaluated at the
destination. Content-addressed typed storage (`Durable.store`) makes any value — including functions
— persistable and referenceable by hash, enabling a form of deferred computation but not true
call-by-need laziness over the network.

### Content-Addressing and Caching

Content-addressing is a first-class design principle, not an optimization. Because definitions never
change (hash identity is eternal), the compilation cache is never invalidated. Dependency syncing
uses the hash graph: a node that already has a hash need not receive it again. For distributed
computation results, the Unison distributed RFC describes nodes maintaining a task status map and a
durables peer map, enabling result discovery without recalculation. Hash-based partitioning in
Volturno provides deterministic routing so the same key always lands on the same loglet.

### Fault Tolerance

Each node maintains a `Map Task (Timestamp, Status, Optional Node)` tracking task state. A
supervisor "chases" a computation by following transfer links until it gets a recent enough status
update. If a node becomes unresponsive, the supervisor receives an `Unresponsive` error and can
decide to retry elsewhere. The runtime is deliberately kept "dumb" — all fault-tolerance intelligence
lives in Unison library code, not the runtime. Exactly-once processing in Volturno uses DAG barriers
and lease-based leader election (incorporating both leadership and failure detection in one mechanism)
for exactly-once snapshots.

### Lessons for tinct

- **The hash-as-identity insight is exactly what tinct's content-addressed cache key gives.** Tinct's
  cache key is SHA-256 of `{expr, env, args}` in canonical wire encoding — this is the same
  invariant Unison's hash provides. The mechanism differs (Unison hashes ASTs at definition time;
  tinct hashes at dispatch time) but the semantics are equivalent: same computation, same result,
  never re-evaluated.
- **"The thunk is the task message" is validated.** Unison ships closures (continuations) as the
  task payload, exactly as tinct proposes for `remote-task`. The content-hash mechanism that makes
  this safe in Unison is provided in tinct by the canonical wire encoding of thunks.
- **Keeping the runtime dumb is a strong principle.** Tinct's proposal to implement fault
  tolerance in `stdlib/dist.llt` rather than hardwiring it in the runtime is consistent with
  Unison's philosophy and enables the behavior to be inspected, tested, and replaced using tinct
  builtins.
- **Tinct's laziness is the main divergence.** Tinct must force environment bindings before
  serialization — the "distributable thunk condition" in `dist-eval.md` (no live thunks in the
  environment) is the correct answer to this problem. Unison avoids it by being strict; tinct solves
  it by pre-materializing the environment at dispatch time.

---

## 2. Erlang/OTP

### Unit of Computation

The fundamental unit is the **process** — a lightweight (< 500 bytes) isolated actor with a mailbox.
Processes share nothing: each has its own heap, garbage collector, and message queue. Communication
is by asynchronous message passing using process identifiers (PIDs), which are location-transparent:
the same `!` operator sends to a local or remote process without distinction.

### Serialization

All inter-node messages are encoded in the **External Term Format (ETF)**, a well-defined binary
format that encodes the full Erlang term space (atoms, tuples, lists, binaries, PIDs, references).
`term_to_binary/1` and `binary_to_term/1` are the entry points. Node-to-node communication uses
TCP/IP with a 4-byte packet header, authenticated by a shared cookie (via MD5 challenge-response;
cookies never cross the wire). All application data is transmitted in cleartext (ETF) unless TLS is
layered on. EPMD (port 4369) handles node discovery by name.

**Crucially, functions (lambdas) are not directly serializable in standard Erlang distribution.**
This is the fundamental limitation: you can send data, but not arbitrary computations. The workaround
is to send atom names of registered functions or to use code pre-deployed on all nodes.

### Programmer Interface

The programmer uses PIDs directly: `Pid ! Message` (send), `receive ... end` (receive). OTP
abstracts this into behaviors: `gen_server`, `gen_statem`, `gen_event` provide request-reply,
state-machine, and event-bus patterns with automatic supervision, hot code upgrade, and location
transparency. Supervisors restart crashed children according to configurable strategies (one-for-one,
one-for-all, rest-for-one). The "let it crash" philosophy means processes do not defend against
failures — they crash cleanly and let the supervisor tree handle recovery.

### Lazy Evaluation vs. Distribution

Erlang is strictly evaluated (no call-by-need). Messages are fully evaluated terms. There is no
concept of a thunk crossing process boundaries. Lazy streams can be simulated with explicit closures
(passing a `fun` as the "tail" of a stream), but this requires the receiving process to have the
function's module already loaded — functions are not self-contained serializable units.

### Content-Addressing and Caching

None. Erlang has no built-in content-addressed caching mechanism for computation results. The
application is responsible for any memoization, typically via ETS (in-process/shared-memory tables)
or Mnesia (distributed database). External tools (Redis, etc.) fill this role in production. Process
mailboxes provide at-most-once delivery; no result deduplication unless the application implements it.

### Fault Tolerance

This is Erlang's signature strength. Supervision trees with configurable restart strategies, monitor
and link primitives for detecting process death, and global process registration (via `global` module)
enable extremely high-availability systems. The Ericsson AXD301 switch achieved nine nines of
availability using OTP. OTP provides hot code upgrades via `code:change/2` without stopping the
system. The `net_kernel` manages node connections; nodes rejoin automatically after network partition
recovery if they share a cookie.

### Lessons for tinct

- **Share-nothing process isolation with pre-deployment is the simplest safe model.** Tinct's
  "distributable thunk" constraint (no live thunks or capabilities in the environment) is the
  functional equivalent of Erlang's "only data crosses the wire." The key difference: tinct sends
  AST + environment rather than just data, because tinct code is serializable in a way Erlang
  lambdas are not.
- **Location-transparent task IDs are worth having.** Tinct's UUIDs in `TaskRequest` serve the
  same role as PIDs — a stable identity for a computation that lives independently of which node
  holds it at any moment.
- **"Let it crash" maps well to tinct's `Task@(Ok | Err)` model.** Workers crash on failure;
  the coordinator sees a timeout in the in-flight table and re-dispatches. This is the Erlang
  supervisor strategy applied at task granularity.
- **OTP behaviors are the right long-term target for `stdlib/dist.llt`.** The current `dist-map`
  pattern in tinct is analogous to a pool of `gen_server` workers. As tinct matures, higher-level
  distribution patterns (pub/sub, pipeline supervision) could be implemented as tinct libraries in
  the same way OTP provides them as behaviors.
- **Erlang's function-non-serialization is tinct's key advantage.** Because tinct code is data
  (AST stored as tinct dicts via `ast_to_dict`), tinct can ship computations without pre-deployment.
  This is the architectural gap that tinct's design closes.

---

## 3. Cloud Haskell (distributed-process)

### Unit of Computation

The fundamental unit is the **Process**, identical in concept to an Erlang actor. Processes run in
the `Process` monad, communicate by typed message passing, and are identified by `ProcessId`. Cloud
Haskell is explicitly modeled after Erlang's actor model, adapted to Haskell's type system.

### Serialization

The `Serializable` typeclass (`Binary a, Typeable a`) gates what can be sent. For data, this works
naturally. For **computations** (functions), Cloud Haskell introduces the `Closure a` type:

```haskell
data Closure a where
  StaticPtr :: StaticPtr b -> Closure b
  Encoded   :: ByteString  -> Closure ByteString
  Ap        :: Closure (b -> c) -> Closure b -> Closure c
```

A `Closure (Process a)` is a serializable computation. It is built by combining a `StaticPtr`
(a compile-time reference to a top-level function, via Template Haskell's `remotable` macro or
GHC's `-XStaticPointers` extension) with a serialized environment. The environment must be
`Serializable`; the code reference must be `Static` (compile-time constant). The `remotable` TH
macro generates a remote table mapping names to functions for lookup at runtime on remote nodes.

**Key limitation:** `Static` in GHC is "not the true static" — runtime type class evidence
(dictionaries) cannot be reified as static, requiring both typeclass constraint and explicit
serialization dictionary simultaneously. Arbitrary closures cannot be serialized if they capture
non-`Serializable` values. This makes Cloud Haskell closure composition more complex than
Unison's content-addressed approach.

### Programmer Interface

`spawnLocal`, `spawn`, `spawnRemote` create processes. `send` and `receive` pass typed messages.
`mkClosure` wraps a top-level function for remote execution. The programmer annotates functions with
`remotable` to register them in the remote table. A failure of the approach surfaced in practice:
"allowing arbitrary closures to be serialised, while convenient, exposes APIs which aren't very
stable." Production users eventually moved toward explicit endpoints with stable APIs rather than
arbitrary closure shipping.

### Lazy Evaluation vs. Distribution

This is Cloud Haskell's most significant design tension. Haskell is call-by-need; messages to local
processes are serialized anyway ("to ensure that no unevaluated thunks are passed to the receiver")
because passing a thunk that evaluates to an error at the receiver is a debugging nightmare. Safe
send forces evaluation via serialization; unsafe send (`sendChan` with `unsafeSendChan`) skips this
but may transfer unevaluated thunks across process boundaries.

The `NFData`-based approach (force to normal form before sending) is used by
`distributed-process-platform` via `NFSerializable`, but cannot be unified with `Binary` safely
because the two instances may force to different depths.

**The lesson:** Lazy evaluation and distributed message passing have fundamentally different
evaluation models. The only safe resolution is to eagerly evaluate (force to NF) before crossing the
boundary. Tinct reaches the same conclusion: the distributable thunk condition requires a fully
materialized environment.

### Content-Addressing and Caching

None built-in. The remote table is a compile-time registry, not a content-addressed cache. No
computation deduplication. Applications implement caching at the application layer.

### Fault Tolerance

Erlang-style monitors and links: `monitor`, `link`, `unlink`, `monitorNode`. The Process abstraction
receives a `ProcessMonitorNotification` when a monitored process dies. Supervisor trees can be
built from these primitives (or via higher-level packages). Node disconnect/reconnect is handled by
the transport layer.

### Lessons for tinct

- **The `Static` pointer problem is a fundamental unsolved tension in typed distributed systems.**
  Tinct sidesteps it entirely by treating code as data: `ast_to_dict` serializes any tinct expression
  (including closures) into a `Dict` value without needing a static registry or compile-time table.
  This is Unison's insight applied to tinct's AST-as-data model.
- **Force-before-send is mandatory for call-by-need + distribution.** The distributable thunk
  condition in `dist-eval.md` is independently confirmed by Cloud Haskell's experience. Pre-forcing
  the environment is the correct answer.
- **Arbitrary closure shipping creates unstable APIs in practice.** Tinct should consider whether
  `remote-task` should accept arbitrary closures (maximum flexibility, Cloud Haskell's path) or
  named stdlib functions with explicit argument passing (maximum stability, the direction Cloud
  Haskell users moved toward). The current tinct design chooses closures; this is acceptable for
  configuration workloads where code changes are infrequent.
- **Typed message channels are safer than untyped mailboxes.** Cloud Haskell's typed `Channel`
  (typed send/receive via `sendChan`/`receiveChan`) is superior to Erlang's untyped `!`. Tinct's
  `channel` primitive should remain typed.

---

## 4. Oz/Mozart

### Unit of Computation

The fundamental unit is the **thread**, an extremely lightweight (Mozart can run 100,000 threads
concurrently) dataflow-synchronized execution context. Threads are not isolated actors (they share
a store); they are concurrent computations that block on unbound **dataflow variables** (logic
variables). A thread proceeds only when all values it needs are available — blocking automatically
on unbound variables rather than failing.

### Serialization

Mozart implements **network-transparent distribution** with a principled distinction between entity
types:

- **Stateless entities** (records, numbers, atoms, closures, functors, classes): These never change
  and are copied across the network in a single message. Functions/closures are stateless and
  serialize freely — no registration table required. This is the key difference from Cloud Haskell.
- **Stateful entities** (objects, ports, cells, unbound variables): These have an owner site.
  Remote access uses an owner-proxy protocol: the proxy forwards operations to the owner, which
  executes them and returns results. Objects can be made "mobile" (migrate to the accessing site),
  "stationary" (always execute at home), or "cache-able" (replicate reads, serialize writes) via
  mobility annotations independent of the object's definition.
- **Logic variables**: Distributed via a single-assignment protocol. When a variable is bound on
  one site, the binding propagates to all sites that hold a reference to that variable.

### Programmer Interface

The programmer writes sequential-looking Oz code; the language adds concurrency transparently via
dataflow blocking. For distribution, two sites connect using `Connection.offer`/`Connection.take`
(exchanging a ticket string out-of-band). After connection, variables, objects, and procedures can
be passed between sites as first-class values — no RPC layer, no serialization annotation. Functors
(module specifications, analogous to `.mli` interfaces) can be referenced by URL and loaded on
demand. The distribution model is transparent: a program's behavior is (in principle) independent
of how it is partitioned among sites.

### Lazy Evaluation vs. Distribution

Oz supports both eager and lazy evaluation modes. In lazy mode, closures are created but not
immediately applied; demand-driven evaluation is expressed via explicit "by-need" synchronization.
Lazy values are **futures**: a read-only view of an unbound variable. A future distributed to another
site blocks the accessing thread on that site until the original site binds it. This is the cleanest
form of distributed lazy evaluation: the laziness is encoded as an unbound variable in the store,
and the distributed protocol for logic variables handles the cross-network case uniformly.

### Content-Addressing and Caching

No content-addressed caching of computation results. Stateless entities are cached at the receiving
site after first transfer (copy semantics). The module/functor system loads by URL with conventional
caching (file system). No hash-based deduplication.

### Fault Tolerance

Mozart provides orthogonal fault detection via **watchers** on distributed entities. A watcher
receives notification when a referenced entity or site becomes unavailable. Fault handling is
**explicit** — the programmer decides what to do on failure — rather than the Erlang supervisor
tree model of automatic restart. This makes fault handling more composable but requires more
programmer effort.

### Lessons for tinct

- **Stateless-vs-stateful is the right classification for distribution.** Tinct's "distributable
  thunk condition" is exactly Oz's stateless classification: no mutable state, no live references.
  Oz makes this distinction explicit in the type/entity system; tinct should too (via the
  `distributable?` predicate and, eventually, static purity analysis).
- **Logic variables as distributed lazy futures are the theoretically cleanest model.** Tinct's
  `Task@T` / `await` pattern is an applicative approximation of this: a `Task@T` is a future for
  a remote computation. The key difference is that tinct futures are created-at-dispatch (when
  `remote-task` is called), while Oz futures are created when a variable is introduced. Oz's model
  is more compositional but harder to implement; tinct's is simpler and more predictable.
- **Network-transparent load-by-URL for functors is relevant for tinct's include system.** When
  tinct's `$include` mechanism eventually supports remote URLs (importing a tinct file from a
  content-addressed store or HTTP server), it should adopt the Oz model: stateless module values,
  cached at first fetch, identified by content hash rather than URL.
- **Mobility annotations are a useful future direction.** For tinct's capability types (`DirCap`,
  `NetCap`), the Oz distinction between mobile, stationary, and proxied objects maps directly to
  tinct's capability delegation models (pure, delegated, proxied in `dist-eval.md`).

---

## 5. E Language

### Unit of Computation

The fundamental unit is the **vat** — a single-threaded event loop that contains a set of objects
and processes messages from a queue. Vats are isolated from each other (no shared memory). A
computation is a sequence of **eventual sends** (`<-` operator) that enqueue messages to objects,
possibly in other vats. The response to a message is a **promise** that resolves when the handler
returns.

### Serialization

E does not serialize closures directly. Remote communication is between **vat references**: a
reference to an object in another vat (which may be in another OS process or on another machine).
The E runtime transparently encrypts all inter-vat communication (using Noise protocol variants).
Messages are serialized as capabilities — unforgeable references that grant authority by possession.
The architecture is object-capability (ocap): **a reference is a capability**, and the only way to
communicate with an object is to hold a reference to it.

Distributed E ("secure communication between vats on different machines") works by exchanging
introduction references out-of-band (via SturdyRefs — serializable persistent URLs with unguessable
tokens). From that introduction, further capability references can be derived.

### Programmer Interface

- **Immediate calls** (`.` operator): synchronous, local-only, like a method call.
- **Eventual sends** (`<-` operator): asynchronous, work for both local and remote objects, return
  a promise immediately.
- **Promise pipelining**: `x <- foo() <- bar()` chains eventual sends without waiting for
  intermediate resolution. The chain can be batched into a single network round-trip — "enormous
  queues of operations can be shipped across the poor-latency comm network while waiting for a
  result."

Deadlock is structurally impossible: eventual sends never block the sender. This is guaranteed by
the vat/event-loop model: a vat processes one message at a time, and it never holds a lock while
waiting for a remote response.

### Lazy Evaluation vs. Distribution

E is strictly evaluated (call-by-value within a vat). Promises are the mechanism for deferred
results — they are not lazy values in the call-by-need sense, but rather futures that resolve
asynchronously. The event loop model ensures that a vat's computation is never blocked by waiting
for a promise; it is suspended (yielding the event loop) and resumed when the promise resolves.
This is structurally different from tinct's call-by-need thunks but achieves a similar effect:
work is deferred until a result is needed.

### Content-Addressing and Caching

None built-in. E's security model relies on unforgeable capability references, not content hashing.
SturdyRefs include unguessable random tokens to prevent unauthorized access, but this is a
security mechanism, not a caching mechanism.

### Fault Tolerance

E uses **broken references** for fault signaling: a promise or capability that fails to resolve
becomes a broken reference, which propagates the failure to any computation that uses it. The
programmer can catch broken references with `when`/`catch` handlers. There is no automatic restart;
failure handling is explicit, consistent with the capability security model (authority must be
explicitly delegated, including the authority to retry).

### Lessons for tinct

- **Promise pipelining is directly applicable to tinct.** Tinct's `[await-all tasks]` pattern
  could be extended to support pipelining: chaining `remote-task` calls so that the result of
  task A is passed directly to task B without returning to the coordinator, in a single batched
  dispatch. This would reduce round-trip latency for pipeline-shaped computations.
- **Deadlock freedom by design.** Tinct's `remote-task` already returns `Task@T` immediately
  (non-blocking), mirroring E's eventual send semantics. The coordinator-worker protocol is
  request-response, so deadlock is not a structural concern — but E's analysis confirms the
  pattern is sound.
- **Capability references as first-class values is tinct's model.** Tinct's `DirCap`, `NetCap`,
  and similar capability types play the same role as E's capability references. The "delegated"
  capability model in `dist-eval.md` (explicitly granting specific caps in a task message) is
  E's ocap model applied to distributed tinct tasks.
- **Promise pipelining as a future optimization:** For tinct's current design, `await-all` followed
  by computation at the coordinator is simpler. Pipelining would be valuable for chains of
  `remote-task` calls where intermediate results are large (avoiding transfer back through the
  coordinator). This is worth noting as a phase-2 optimization.
- **SturdyRef = Ref@T.** E's persistent, serializable capability references that survive node
  restarts map directly to tinct's `Ref@T`: a content-addressed hash that can be passed to a
  remote task, which fetches the underlying value independently. The difference is that tinct uses
  content hashing rather than unguessable tokens for identity, which provides stronger deduplication
  guarantees.

---

## 6. Chapel

### Unit of Computation

Chapel's units are **tasks** (explicit) and **array iterations** (implicit via `forall`). The
language provides a tiered parallelism model:

- `begin`: fire-and-forget task.
- `cobegin`: structured parallel block — wait for all named sub-tasks.
- `coforall`: each loop iteration becomes a distinct task; the enclosing scope waits.
- `forall`: data-parallel loop, execution potentially parallel over array elements; may be
  serialized if the iterable does not support parallelism.

For distributed computing, the **locale** is the primary abstraction: a locale is a unit of uniform
memory access (e.g., a NUMA node, a machine, a rack). Computation is placed on a locale via the
**`on` clause**: `on Locales[i] do expr` executes `expr` on locale `i`.

### Serialization

Chapel uses a **Partitioned Global Address Space (PGAS)** model. There is conceptually one global
address space; Chapel's runtime handles the physical data movement transparently. When a task
running on locale A accesses a variable that lives on locale B, the Chapel runtime issues a remote
GET/PUT (using GASNet or similar). The programmer does not serialize explicitly; the runtime
serializes values as needed for remote access.

**Domain maps** control where arrays live. A `blockDist`-distributed domain partitions an array
across locales so that `forall` iterations automatically distribute computation to the locale
holding each element. The programmer declares the distribution; the runtime manages communication.

### Programmer Interface

The programmer writes global-view code that looks like serial code with parallelism annotations.
Data distributions are declared separately from algorithms. A matrix multiplication over a
block-distributed matrix looks nearly identical to serial code. The `on` clause makes placement
explicit when needed; the global address space makes it optional.

The "multiresolution" design philosophy means experts can define custom domain maps (controlling
communication patterns, data layout, and iterator behavior) while application programmers use
predefined distributions (block, cyclic, block-cyclic) without knowing the details.

### Lazy Evaluation vs. Distribution

Chapel is strictly evaluated (imperative/functional mix, always eager). No lazy evaluation. Arrays
are concrete values. The `sync` and `atomic` types provide synchronization between tasks, but these
are synchronization primitives, not lazy values.

### Content-Addressing and Caching

None. Chapel is designed for HPC workloads where computations are not typically idempotent or
memoizable — the science code changes inputs, and results are written to output files. No
content-addressed caching.

### Fault Tolerance

Chapel's fault tolerance is minimal: the language targets HPC clusters with reliable interconnects
and batch schedulers that handle checkpointing at the job level. Individual task failure is not
automatically recovered; the application must checkpoint and restart.

### Lessons for tinct

- **The `on` clause is the explicit version of tinct's `remote-task`.** Both are programmer-visible
  placement annotations. The key difference is that Chapel's `on` clause places *execution* at a
  locale, while tinct's `remote-task` submits *a task description* to a cluster (the cluster chooses
  the executing node). Chapel's model gives more control; tinct's gives more flexibility.
- **Domain maps are the distributed equivalent of tinct's shard-and-map pattern.** Tinct's
  `dist-map` manually partitions a sequence and maps over shards. Chapel's distributed domains
  encode this distribution in the data structure itself, making `forall` loops automatically
  parallel-and-distributed. For tinct, this suggests a future direction: distributed collections
  (types that encode their own distribution policy) would allow `map`/`filter`/`reduce` to
  distribute automatically without explicit `dist-map`.
- **PGAS transparency vs. explicit `Ref@T`.** Chapel hides data movement behind the global address
  space. Tinct's `Ref@T` is explicitly explicit: the programmer knows they are passing a reference,
  not a value. Both approaches have merit; tinct's explicit model is safer for a language where
  transparency can hide expensive operations.
- **Multiresolution design is tinct's correct long-term architecture.** Application code uses
  `dist-map`/`dist-filter` from `stdlib/dist.llt`. Advanced users who need custom partitioning,
  routing, or fault-tolerance strategies implement them in tinct itself. This is Chapel's
  multiresolution model applied to a configuration language.

---

## 7. Futhark

### Unit of Computation

The fundamental unit is the **array operation** — `map`, `reduce`, `scan`, `transpose`, `zipWith` —
applied over regular (rectangular) arrays. There are no processes, actors, or tasks. Parallelism
is **implicit** in the data structure: applying `map f arr` in Futhark means "apply `f` to every
element of `arr` in parallel." The compiler determines how to partition and schedule these operations
across GPU threads/warps/blocks; the programmer sees none of this.

### Serialization

Futhark compiles to GPU kernels (OpenCL or CUDA). Data movement between CPU and GPU is explicit
at the FFI boundary (the Futhark program is called as a library from a host language), but within
a Futhark program, array transfers between GPU memory regions are managed by the compiler. There
is no network distribution — Futhark targets single-machine parallelism (multi-GPU is not in scope).

### Programmer Interface

The programmer writes purely functional array transformations. The language imposes strong
restrictions to ensure GPU compilation is possible: no dynamic memory allocation inside kernels,
regular arrays only (no irregular/jagged arrays), no general recursion (only tail recursion via
`loop`), and higher-order functions are defunctionalized at compile time. These restrictions are
not accidental limitations — they are the design contract that makes the flattening transformation
and code generation tractable.

The **flattening transformation** (inspired by Blelloch's NESL work) converts nested parallel
operations into flat operations on arrays with segment descriptors. "Moderate flattening" uses
heuristics to sequentialize excessive nested parallelism rather than blindly exploiting all
available parallelism (which would cause polynomial space blowup). "Incremental flattening" (PPoPP
2019) improves on this by profiling-guided selection of parallelism degree at runtime.

### Lazy Evaluation vs. Distribution

Futhark is strictly evaluated and has no distributed computation concept. The "distribution" in
Futhark is data-parallel distribution across GPU threads — an entirely different meaning of the
word. However, Futhark's design is highly relevant for tinct because it represents the logical
endpoint of what happens when you restrict a purely functional language to guarantee efficient
parallel compilation.

### Content-Addressing and Caching

None. Futhark programs are compiled and then run; there is no result caching between runs.

### Fault Tolerance

None. GPU execution is assumed reliable; the host handles failure at the application level.

### Lessons for tinct

- **Futhark's restrictions are what tinct must avoid.** Futhark's irregular-array prohibition,
  no-recursion rule, and defunctionalized higher-order functions are the costs of GPU-first design.
  Tinct's distributed model should not impose these restrictions — tinct distributes over a cluster
  of general-purpose nodes, not over GPU threads. The lesson is negative: these restrictions
  make Futhark a narrow specialist tool, not a general configuration language.
- **The flattening transformation is the right model for tinct's `dist-map` over nested
  structures.** When a tinct program maps over a sequence of sequences (e.g., a list of
  configurations each requiring independent computation), the distribution strategy should "flatten"
  the nested structure: partition the outer sequence across workers, letting each worker handle its
  inner sequence sequentially. This is Futhark's moderate flattening applied at cluster granularity.
- **Pure functional semantics enable zero-overhead distribution.** Futhark achieves GPU performance
  competitive with hand-written CUDA precisely because purity eliminates aliasing and ordering
  constraints. Tinct's "distributable thunk" purity requirement enables the same optimization: pure
  tasks can be scheduled, cached, and re-ordered without affecting correctness.
- **Uniqueness types for in-place mutation are worth noting.** Futhark's uniqueness type system
  allows efficient in-place array updates while preserving referential transparency. For tinct,
  this is less relevant (configuration values are immutable), but if tinct ever adds mutable
  streaming buffers, the uniqueness type approach is the principled solution.
- **Defunctionalization at compile time.** Futhark eliminates higher-order functions by
  defunctionalization before code generation. Tinct already stores functions as AST values
  (`ast_to_dict`), which is an alternative approach: functions are data, and the interpreter
  handles dispatch at runtime. No compile-time defunctionalization is needed.

---

## 8. Julia Distributed

### Unit of Computation

Julia's distributed model has two units:

- **`@spawnat` / `@spawn`**: A single expression or function call dispatched to a specific worker
  process (or any available worker). Returns a `Future`.
- **`@distributed for` / `pmap`**: Bulk data-parallel operations. `@distributed` divides loop
  iterations equally across workers (static allocation); `pmap` uses dynamic load balancing
  (workers pull tasks from a queue as they become available).

Workers are separate OS processes with separate memory spaces, communicating via Julia's
`Serialization` stdlib over TCP (for local workers) or SSH/sockets (for remote workers).

### Serialization

Julia serializes values using the `Serialization` stdlib, which uses a format internal to Julia
(not a public standard like ETF or protobuf). The format handles most Julia types, including
closures. **Closures are serializable in Julia** — but with important caveats:

- For closures capturing **global variables**, only the binding is captured (not the value); the
  receiving worker resolves the global in its own `Main` namespace.
- For closures capturing **local variables** via `let` blocks, the values are copied into the
  closure and serialized with it.
- The first time a closure is sent to a worker, if it references module-level functions, the
  entire module may need to be loaded on the worker first (via `@everywhere`).
- `CachingPool` caches serialized closures on workers to avoid repeated serialization overhead for
  closures that capture large amounts of data.

A `RemoteChannel` serializes as an identifier (a reference to a remote `Channel`), not as the
channel's contents — enabling workers to share a channel by reference, not by copy.

### Programmer Interface

- `@spawnat w expr`: Run `expr` on worker `w`; returns `Future`.
- `fetch(future)`: Block until result is available; fetch and cache locally.
- `pmap(f, collection)`: Parallel map with dynamic load balancing.
- `@distributed (op) for i in range`: Parallel reduction loop.
- `remotecall_fetch(f, w, args...)`: RPC-style: send and wait for result.
- `@everywhere expr`: Broadcast an expression to all workers (define functions/load modules).

The model is explicitly RPC-oriented. Workers are identified by integer IDs. There is no
location transparency (you often specify which worker to use), though `pmap` and `@distributed`
abstract over the assignment.

### Lazy Evaluation vs. Distribution

Julia is strictly evaluated (multi-dispatch with eager semantics). No lazy evaluation. `Future`
is the closest construct to a deferred value: the computation runs eagerly on the worker, and the
`Future` on the caller side blocks (via `fetch`) until the result is ready. This is
futures/promises, not call-by-need.

### Content-Addressing and Caching

`CachingPool` caches serialized closures (avoiding repeated transfer of large captured data).
`Future` caches its result locally after first `fetch`. No content-addressed deduplication of
computation results — two identical function calls on the same data are dispatched twice.

### Fault Tolerance

Julia's distributed model has minimal fault tolerance. Worker failure is not automatically
recovered; exceptions propagate through `Future` and `pmap`. `ClusterManagers.jl` integrates with
HPC batch schedulers (Slurm, PBS) which handle job-level fault tolerance. In Julia 1.11, the
`Distributed` stdlib was decoupled from the core system image, signaling that it is a building
block rather than a first-class feature.

### Lessons for tinct

- **Closure serializability in Julia demonstrates it's tractable.** Julia serializes closures
  successfully in practice, even though it requires care (global vs. local capture, `@everywhere`
  for module loading). Tinct's AST-as-data approach is a cleaner mechanism for the same goal.
- **`CachingPool` = tinct's `Ref@T`.** Both address the problem of large captured data being
  repeatedly transferred. `CachingPool` caches on each worker by identity; tinct's `Ref@T` caches
  by content hash across the entire cluster. Tinct's approach is more powerful (deduplicates
  across workers and across runs) but more complex to implement.
- **Static vs. dynamic load balancing (`@distributed` vs. `pmap`).** Tinct's `dist-map` currently
  uses static sharding (partition by `cluster.worker-count`). Dynamic load balancing (`pmap`-style:
  workers pull shards from a queue) would be more efficient when shard costs are uneven. This is
  a worthwhile phase-2 addition.
- **`RemoteChannel` = tinct's `channel` extended to cross-node scope.** Julia's `RemoteChannel`
  provides a shared-reference channel that multiple workers can write to and read from. Tinct's
  current `channel` is intra-process; a distributed channel (backed by the coordinator) would
  enable worker-to-worker communication patterns that `remote-task` + `await` cannot express.
- **`Future` write-once is a design constraint tinct already upholds.** Tinct thunks evaluate
  exactly once (via `OnceLock`); `remote-task` returns `Task@T` which resolves once. The write-once
  property is foundational to both content-addressed caching and concurrent evaluation safety.

---

## Synthesis: Cross-Language Design Patterns and Recommendations for tinct

### Common Patterns Across All Eight Systems

**1. The unit of distribution is a closure or continuation.**
Every system, without exception, distributes computation as a serialized function application:
Unison transfers continuation thunks, Erlang sends PIDs to pre-deployed functions, Cloud Haskell
ships `Closure (Process a)`, Oz transfers stateless closures, E sends capability-bearing messages
to remote object references, Chapel executes `on`-clause blocks, Julia serializes closures via
`@spawnat`. The details vary enormously, but the invariant holds: the unit of distributed work
is always an expression (or its serialized equivalent) paired with an environment.

**2. Strict evaluation before distribution is universal.**
No system in this survey ships unevaluated call-by-need thunks across process boundaries. Cloud
Haskell forces evaluation via serialization. Unison is call-by-value throughout. Erlang,
Oz, E, Chapel, Julia, and Futhark are all strict. The theoretical result — lazy evaluation and
distribution do not compose without forcing — is universally confirmed in practice. Tinct's
distributable-thunk condition (force the environment before shipping) is the correct and necessary
approach.

**3. Content-addressing is used only where code identity is unstable.**
Unison uses content-addressing as its primary identity mechanism because names are intentionally
decoupled from definitions. Erlang, Cloud Haskell, Oz, E, Chapel, Julia, and Futhark do not use
content-addressing for computation results — their code identity is stable (pre-deployed or
registered). Tinct occupies a middle position: code is not pre-deployed on workers, but it is
stable within a single evaluation run. The SHA-256 cache key is most valuable for incremental
re-evaluation across runs (same config file, changed one value), which is tinct's primary use case
and not addressed by any other system in this survey.

**4. Fault tolerance is almost always handled outside the core mechanism.**
Erlang/OTP is the outlier (built-in supervision trees are a core feature). Unison, Cloud Haskell,
Oz, E, Chapel, Julia, and Futhark all push fault handling to the application layer or an external
system (batch scheduler, HPC checkpointing). Tinct's approach — in-flight task table with re-dispatch
on timeout, with higher-level patterns in `stdlib/dist.llt` — is consistent with the majority design
and avoids hardwiring a specific fault model.

**5. The programmer interface that survives is the typed, stable API.**
Cloud Haskell's experience is a cautionary tale: allowing arbitrary closure shipping "exposes APIs
which aren't very stable," and production users moved to explicit stable endpoints. Erlang/OTP
behaviors (stable interfaces), Chapel domain maps (stable distribution declarations), and Julia's
`pmap` (stable parallel map semantics) all provide stable programmer surfaces. Tinct's `dist-map`,
`dist-filter`, and `dist-reduce` in `stdlib/dist.llt` should be the primary programmer interface;
`remote-task` is the low-level primitive that library authors use.

### Approaches Unique to Specific Designs

- **Unison's perpetual hash**: No other system in the survey uses content-addressed code identity
  as the primary name for definitions. This uniquely enables "ship any computation anywhere without
  pre-deployment" — the strongest possible form of transparent distribution.
- **Erlang's "let it crash"**: No other system makes process failure a normal, expected event and
  builds a recovery architecture (supervision trees) around that assumption. Every other system
  treats failure as exceptional.
- **E's promise pipelining + deadlock freedom**: No other system structurally guarantees deadlock
  impossibility while also enabling latency-hiding pipeline chains. Cap'n Proto and Agoric
  SwingSet carry this design forward into production use.
- **Oz's dataflow variables as distributed futures**: The closest thing to "distributed call-by-need"
  in this survey. A dataflow variable is an unbound future that blocks readers across the network
  until it is bound — exactly the semantics tinct would want for `await` extended across nodes.
  No other system achieves this as cleanly.
- **Futhark's restriction-as-feature**: Every other system tries to support a general programming
  model and pay performance costs for generality. Futhark deliberately restricts the language to
  make compilation to a specific target (GPU) trivially tractable. This is the right philosophy
  for a domain-specific target, but the wrong philosophy for a general configuration language.
- **Chapel's global-view / multiresolution**: No other system provides both a high-level
  global-view abstraction and a low-level domain-map customization layer in the same language.
  This is the design space tinct should target: `dist-map` as the global-view layer, custom
  stdlib partitioning as the domain-map layer.

### Concrete Recommendations for tinct's dist-eval Design

**R1. Keep "thunk is the task message" — it is validated.**
Unison's distributed RFC and Cloud Haskell's closure mechanism both confirm that shipping a
serialized AST + environment as the task payload is the right abstraction. Tinct's `ast_to_dict`
enables this without a compile-time registration step (unlike Cloud Haskell's `remotable`) or a
content-addressed code database (unlike Unison). The existing design is correct; implement it.

**R2. The distributable-thunk condition is necessary and sufficient.**
Confirmed by Cloud Haskell (force-before-send), Erlang (only data crosses the wire), Oz (stateless
entities only), and Julia (closures serialize by value). Tinct's condition — no live thunks and
no capability references in the environment — is the minimal correct invariant. The `distributable?`
predicate should be the first check in `remote-task`; static analysis (AST purity check) should
gate auto-distribution.

**R3. `Ref@T` is the right design for large-value transfer; improve the resolution protocol.**
E's SturdyRef and Julia's `RemoteChannel`-by-identifier both confirm that passing a reference rather
than a value is the correct design for large data. Tinct's `Ref@T` with content-hash identity is
superior to both (it deduplicates). The resolution protocol (local cache → coordinator → peer pull)
should be implemented in this order of priority. The "peer pull" step (a node directly fetches from
the node that last computed a value without going through the coordinator) reduces coordinator load
significantly — Unison's durables peer map is the precedent.

**R4. Add promise pipelining as a phase-2 optimization.**
E demonstrates that `task-a <- result -> remote-task cluster task-b` chains can be batched into one
coordinator round-trip when the coordinator knows the chain structure. For tinct, this means: if
the result of `remote-task A` is the sole input to `remote-task B`, the coordinator can schedule
A and B on the same worker with direct value passing, eliminating one round-trip. This is a
significant latency optimization for pipeline-shaped computations.

**R5. Dynamic load balancing (`pmap`-style) should replace static sharding in `dist-map`.**
Julia's `pmap` vs. `@distributed` experience shows that static equal-sharding is wrong when shard
costs are heterogeneous. Tinct's current `dist-map` uses static `partition`. The coordinator should
maintain a work queue of unassigned shards, and workers pull from the queue as they become
available. This is more complex to implement (requires coordinator-side queue management) but
eliminates the case where one slow shard serializes the entire `dist-map`. A simple version:
the coordinator assigns shards to workers with available capacity, re-queuing if a worker is slow.

**R6. The coordinator-group design (homogeneous nodes, Raft) is confirmed correct.**
Ceph (referenced in `dist-eval.md`), Volturno (Unison's streaming system with lease-based
leadership), and Erlang's global process registry all use variants of the same pattern:
homogeneous nodes, consensus-based coordination, no dedicated coordinator machines. The Raft
implementation choice is well-supported by the literature. The in-flight task table with
term-fenced re-dispatch on timeout is the correct fault-tolerance mechanism for stateless workers.

**R7. Capability delegation should follow E's object-capability principles precisely.**
E's ocap model confirms: authority travels with explicit references, never implicitly. Tinct's
pure/delegated/proxied capability models are correct. The "proxied" model (capability-requiring
operations forwarded back to the originator as sub-requests) is E's cross-vat I/O pattern —
implement it via a bidirectional RPC sub-protocol within the task QUIC stream. This enables
workers to perform reads on behalf of the coordinator without holding the capability locally.

**R8. `stdlib/dist.llt` should implement OTP-style supervisors, not just `dist-map`.**
Erlang's durability comes from OTP behaviors. As tinct's distribution matures, `stdlib/dist.llt`
should add: a pool pattern (fixed set of workers, task queue, automatic re-dispatch on failure),
a pipeline pattern (chained `remote-task` with intermediate result caching at each stage), and
a supervisor pattern (monitor a cluster of running tasks; restart failed ones up to a retry limit).
These are all implementable in pure tinct on top of `remote-task`, `await`, `task`, and `channel`.

**R9. Typed stable APIs over arbitrary closure APIs.**
Cloud Haskell's warning is explicit: arbitrary closure shipping "exposes APIs which aren't very
stable." Tinct should encourage users to use named functions from `stdlib/dist.llt` rather than
raw `remote-task` with inline closures. In practice, this means `dist-map`, `dist-filter`, and
`dist-reduce` should cover 95% of distributed use cases. Raw `remote-task` with an inline closure
should be the escape hatch for unusual cases.

**R10. Oz's stateless classification is the right long-term static analysis target.**
Tinct's current `distributable?` is a runtime check. The ultimate target is a static analysis pass
(perhaps integrated with the type checker) that marks pure, closed expressions as statically
distributable at parse/typecheck time. This enables the auto-distribution scheduler to make
decisions without evaluating a runtime predicate. Oz's factored design (stateless vs. stateful
classified by entity type) is the model; tinct's equivalent is a purity flag on function types
inferred from capability-absence in the function body.

---

## Reference Map

The following sources were consulted for this survey. Authoritative documentation is noted where
accessed directly.

- Unison language: [The Big Idea](https://www.unison-lang.org/docs/the-big-idea/), [Volturno
  design](https://www.unison-lang.org/blog/volturno-design/), [Distributed Programming
  RFC](https://github.com/unisonweb/unison/blob/trunk/docs/distributed-programming-rfc.markdown),
  [Simplifying distributed programming](https://www.unison-lang.org/docs/tour/_big-technical-idea/)
- Erlang/OTP: [Distributed Erlang](https://www.erlang.org/doc/system/distributed.html),
  [Distribution Protocol](https://www.erlang.org/doc/apps/erts/erl_dist_protocol.html),
  [EEF Security WG](https://erlef.github.io/security-wg/secure_coding_and_deployment_hardening/distribution.html)
- Cloud Haskell: [HaskellWiki](https://wiki.haskell.org/Cloud_Haskell),
  [distributed-process Hackage](https://hackage.haskell.org/package/distributed-process),
  [distributed-closure Hackage](https://hackage.haskell.org/package/distributed-closure-0.5.0.0/docs/Control-Distributed-Closure.html),
  [Towards Haskell in the Cloud (Epstein et al.)](https://www.microsoft.com/en-us/research/wp-content/uploads/2016/07/remote.pdf)
- Oz/Mozart: [Mozart features](https://www.mozart-oz.org/features/),
  [Distribution Model tutorial](http://mozart2.org/mozart-v1/doc-1.4.0/dstutorial/node2.html),
  [Wikipedia](https://en.wikipedia.org/wiki/Oz_(programming_language))
- E language: [E in a Walnut](http://www.skyhunter.com/marcs/ewalnut.html),
  [Spritely core paper](https://files.spritely.institute/papers/spritely-core.html),
  [awesome-ocap](https://github.com/dckc/awesome-ocap)
- Chapel: [Language Overview](https://chapel-lang.org/docs/language/spec/language-overview.html),
  [Task Parallelism](https://chapel-lang.org/docs/language/spec/task-parallelism-and-synchronization.html),
  [Chapel overview](https://chapel-lang.org/overview.html)
- Futhark: [Why Futhark](https://futhark-lang.org/), [PLDI 2017 paper](https://futhark-lang.org/publications/pldi17.pdf),
  [PPoPP 2019 incremental flattening](https://futhark-lang.org/publications/ppopp19.pdf),
  [Language design blog](https://futhark-lang.org/blog/2016-09-03-language-design.html)
- Julia Distributed: [Julia docs](https://docs.julialang.org/en/v1/manual/distributed-computing/),
  [Distributed.jl GitHub](https://github.com/JuliaLang/Distributed.jl)
