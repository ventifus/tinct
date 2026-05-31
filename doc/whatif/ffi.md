# What If: Foreign Function Interface and Native Module Extensions for tinct

**State:** Proposal

What would it take to call C and Rust libraries from tinct programs, and how should tinct's own Rust builtins be organized so that feature libraries can bring their own native code without requiring it in prelude?

This whatif covers three related but distinct approaches to the same underlying problem: tinct's extension surface. They can be adopted independently or in combination.

| Approach | Scope | Mechanism | Binary impact |
|---|---|---|---|
| [Option 1: External C/Rust FFI](#option-1-external-crust-ffi-extern-block) | Call any C ABI library | `extern` block + `libloading` | None — loaded at runtime |
| [Option 2: In-Tree Native Modules](#option-2-in-tree-native-modules-----uses) | Lazy activation of compiled-in builtins | `native-module` builtin + registry | Code already in binary |
| [Option 3: Cargo Workspace Split](#option-3-cargo-workspace-split) | Separate Rust crates per feature | Workspace crates, static or dynamic link | Structurally separate |

## Current State

tinct has no mechanism for extending the runtime without rebuilding the interpreter. All builtins are compiled into a single `src/builtins.rs` file and registered unconditionally at startup via `standard_builtins()` / `create_root_env()`. A previous `rust_module()` / `%rust` system that grouped builtins by name was deleted in the `include-decomp-redelete` sprint (2026-05-20) in favor of pre-injecting everything.

```tinct
# Current: no way to call libcurl, libssl, libsqlite3 without a rebuild
# Must shell out, pre-process, and pipe JSON in:
[include "results.json"]   # result of some external command
---
[map [fn [r] r.name] %]
```

All ~191 builtins — including those only used by `stdlib/net.llt`, `stdlib/sql.llt`, datetime, etc. — are loaded into every tinct process regardless of what the program imports.

### What's Missing

1. No way to load a shared library (`.so` / `.dylib` / `.dll`) at runtime from tinct code
2. No way to declare and call C ABI symbols from tinct code
3. No opaque handle type — pointer-sized values must be stuffed into `Number` (lossy on 64-bit)
4. No drop hook model for resource lifecycle (open handles, allocated memory)
5. No per-feature builtin grouping — all builtins load even when the feature isn't used
6. No way for a `.llt` stdlib file to declare its Rust dependencies explicitly
7. No source-level separation between core builtins and feature builtins

## Why This Matters for tinct

tinct is designed to be embedded in larger pipelines and to function as a data-transformation layer across many contexts. A well-defined extension surface serves several goals:

- **System libraries become available without a rebuild.** `libsqlite3`, `libcurl`, `libssl`, `libjpeg` — anything with a C ABI becomes callable by declaring types.
- **Startup cost scales with what's used.** Loading datetime builtins in a script that never uses dates is pure overhead.
- **tinct libraries become self-contained.** `stdlib/sql.llt` can declare its own Rust dependencies rather than relying on those builtins being globally injected.
- **The binary can be made slim.** A `tinct-core` build with only core language builtins is possible if feature code lives in separate crates.
- **Gradual integration.** A tinct script can call a single C function to experiment before anyone writes a formal Rust builtin.

## Option 1: External C/Rust FFI (`extern` block)

An `extern` block in a tinct program calls into a shared library compiled separately from tinct — any `.so`/`.dylib`/`.dll` with a C ABI. The library does not need to know anything about tinct.

### The `extern` Block

An `extern` block declares a shared library path and a set of named symbols with their tinct-visible types:

```tinct
[extern "libsqlite3.so"
    sqlite3-open:   [fn@Number  [path@String]]
    sqlite3-exec:   [fn@Number  [db@Number    sql@String   cb@Null]]
    sqlite3-close:  [fn@Null    [db@Number]]
]
```

After the `extern` block, `$sqlite3-open`, `$sqlite3-exec`, and `$sqlite3-close` are bound in the current scope as `Value::FfiFunction` values. Calling them materializes all arguments, marshals them to C types, calls the symbol via `libloading`, and converts the return value back to a tinct `Value`.

The `extern` block is evaluated eagerly at load time (it opens the library). The bindings it produces are first-class values — they can be stored in dicts, passed to functions, and returned from expressions like any other tinct value.

**Symbol name mapping:** tinct uses kebab-case; C uses snake_case. The loader maps `sqlite3-open` to the C symbol `sqlite3_open` by replacing `-` with `_`. An explicit `as` override is available for symbols whose names don't follow this convention:

```tinct
[extern "libpng.so"
    png-write-image:  [fn@Null  [as: "png_write_image_v2"  ptr@Number  rows@Number]]
]
```

**Library search:** The path is resolved in order: as given (absolute or relative to the tinct file), then `LD_LIBRARY_PATH`, then system library directories. Paths ending in `.so`, `.dylib`, or `.dll` are loaded directly; a bare name like `"sqlite3"` is expanded to the platform-appropriate filename.

### Type Mapping

Only the scalar tinct types cross the FFI boundary cleanly. Complex types require annotation-guided marshalling:

| tinct annotation | C type | Notes |
|---|---|---|
| `Number` | `double` | Default — lossless for IEEE 754 doubles |
| `Int` | `int64_t` | Truncates `Number` to integer; errors on non-integer |
| `Int32` | `int32_t` | As `Int`, additionally range-checked |
| `String` | `const char*` | Materialized, null-terminated copy. Lifetime: until call returns. |
| `Bool` | `int` | `true` → 1, `false` → 0 |
| `Null` | `void` | Return type only |
| `Handle` | `uintptr_t` | Pointer-sized opaque integer. See `Value::OpaqueHandle`. |
| `Ptr` | `void*` | Raw pointer; same as `Handle` at C level. |

`Handle` is the primary mechanism for managing opaque C resources. When a C function returns a `Handle`-annotated value, tinct wraps the raw pointer in `Value::OpaqueHandle` — a newtype that carries the pointer as a `usize` but is distinct from `Number` and cannot be mistakenly used in arithmetic.

```tinct
[extern "libsqlite3.so"
    sqlite3-open:   [fn@Handle  [path@String]]
    sqlite3-exec:   [fn@Int32   [db@Handle   sql@String   cb@Null]]
    sqlite3-close:  [fn@Int32   [db@Handle]]
]

[
    db:      [call $sqlite3-open "/tmp/test.db"]
    result:  [call $sqlite3-exec db "SELECT 1" null]
    _:       [call $sqlite3-close db]
]
```

### Drop Hooks

`Value::OpaqueHandle` optionally carries a drop function registered at the `extern` site with `drop:`:

```tinct
[extern "libsqlite3.so"
    sqlite3-open:  [fn@Handle  [path@String]  drop: sqlite3_close]
    sqlite3-exec:  [fn@Int32   [db@Handle     sql@String  cb@Null]]
]
```

When the `OpaqueHandle` is garbage collected (its reference count reaches zero), the runtime calls `sqlite3_close` with the stored pointer. This makes resource lifetime automatic for the common case where a library has a symmetric open/close pair.

Drop hooks are optional. Without one, the handle is simply dropped when unreachable — the C resource leaks unless explicitly closed.

### Error Handling

C functions conventionally signal errors through return codes or null returns. tinct does not automatically interpret these — the caller is responsible:

```tinct
[
    rc:  [call $sqlite3-exec db sql null]
    _:   [call $if [call $!= rc 0]
              [call $error [call $str "sqlite3 error: " rc]]
              null]
]
```

A `check-rc` helper can be provided in a wrapping tinct library to make this idiomatic. There is no automatic error propagation at the FFI boundary — this matches C's own model and avoids surprising early returns in lazy evaluation.

### Laziness Boundary

FFI calls are **strict**: all arguments are fully materialized before the call. Thunks are forced, lists are realized, dicts are collapsed. This is a hard semantic boundary — C functions cannot receive lazily-evaluated values.

The return value is wrapped in a thunk only if the call site is in a lazy position (e.g., a dict value). The FFI call itself always executes eagerly when observed.

```tinct
[
    # 'result' is a thunk here; the FFI call runs when result is observed
    result: [call $sqlite3-exec db sql null]
]
```

Side effects (I/O, mutation through pointers) occur at observation time, not at binding time. This matches tinct's existing eager-at-observation model for builtins.

### Stdlib Wrappers

Raw `extern` bindings are low-level. The expected pattern is to wrap them in a tinct file that provides a safe, idiomatic interface:

```tinct
# stdlib/sqlite.llt — wraps extern bindings with error checking and resource management

[extern "libsqlite3.so"
    sqlite3-open-raw:  [fn@Handle  [path@String]             drop: sqlite3_close]
    sqlite3-exec-raw:  [fn@Int32   [db@Handle   sql@String   cb@Null]]
    sqlite3-errmsg:    [fn@String  [db@Handle]]
]

sqlite3-open: [fn@Handle [path@String]
    [let [db [call $sqlite3-open-raw path]]
    [call $if [call $null? db]
        [call $error [call $str "failed to open: " path]]
        db]]]

sqlite3-exec: [fn@Int32 [db@Handle  sql@String]
    [let [rc [call $sqlite3-exec-raw db sql null]]
    [call $if [call $!= rc 0]
        [call $error [call $sqlite3-errmsg db]]
        rc]]]
```

This keeps `extern` blocks out of user code and provides the right abstraction level.

---

## Option 2: In-Tree Native Modules (`--- uses:`)

This option addresses a different problem: tinct's own Rust builtins that are already compiled into the binary but should only enter scope when the corresponding stdlib library is included. No external library loading — the code is already there, it just shouldn't be globally pre-injected.

### The Problem in More Detail

`stdlib/sql.llt` needs `sql-open`, `sql-exec`, and `proxy` to exist in the environment. Currently, ~306 builtins are pre-injected by `create_root_env()` at startup via `standard_builtins()`. If `sql.llt` is never included, those builtins were loaded for nothing. More importantly, there is no explicit link in the source between `stdlib/sql.llt` and its Rust dependencies — the dependency is implicit and invisible.

### The Evaluator Has No Builtin Dependencies

The key insight behind this design: **the evaluator itself needs zero pre-loaded builtins.** It takes a value and an environment. `+` is just a `Value::Builtin` it finds when looking up `+` in the env. No specific builtins need to exist globally; what gets pre-injected is entirely a policy choice, not a technical requirement. This means `standard_builtins()` and `create_root_env()` — which currently inject all ~306 builtins globally — can be eliminated entirely. Every document becomes self-describing about its Rust dependencies.

### The `--- uses:` Declaration

A stdlib file declares its Rust builtin dependencies in its document header alongside `--- caps:`:

```tinct
--- uses: ["sql"]
---
[
  # sql-open, sql-exec, proxy are now in scope
  sql-open: [fn@Handle [path@String] ...]
  sql-exec: [fn@Int32  [db@Handle  sql@String] ...]
]
```

`prelude.llt` declares only what it owns — the core language builtins:

```tinct
--- uses: ["prelude"]
---
[
  map:   [fn ...]
  split: [fn ...]
  ...
]
```

`--- uses:` is document metadata — not a tinct function call. It avoids any circular dependency with `include`, which is itself defined in prelude.

### Design Intent: Self-Contained Feature Libraries

`--- uses:` is not just about startup cost — it makes feature libraries self-contained. Under the previous builtin-privacy design, all Rust builtins had to be wrapped and exported by `prelude.llt`, making prelude the sole gateway. `--- uses:` partially reverses this: `lib-net-v3.llt` can declare its own Rust deps directly rather than requiring networking builtins to clutter prelude first. Each feature library becomes its own authority over the Rust layer it depends on.

This is what enables `lib-net-v3.md` to be implemented cleanly: the networking library declares `--- uses: ["net" "async"]` and is self-contained. User code that never includes `lib-net-v3` never sees those builtins.

### The Builtin Module Registry

`standard_builtins()` is deleted. Groups are defined per feature library, with granularity matching the natural self-containment boundary of each stdlib file:

```rust
pub fn builtin_module(name: &str) -> Option<Vec<BuiltinDef>> {
    match name {
        // Core language — used by prelude.llt only.
        // Arithmetic, comparison, string ops, collection ops, sequences,
        // type checking, basic I/O (open/slurp/write/emit), meta (load/eval/ast-of),
        // blake3/include-cache-get/include-cache-put (used by prelude's include pipeline).
        // ~120-140 builtins.
        "prelude"  => Some(prelude_builtins()),

        // Concurrency primitives — used by async.llt and lib-net-v3.llt.
        // task, await, channel, send, recv, select-once, par, par-map, par-filter,
        // signal-channel, timer-channel, watch-channel, context, with-cancel,
        // with-timeout, cancel-task, drain, exit-now + stable builtin-* aliases.
        // ~32 builtins.
        "async"    => Some(async_builtins()),

        // Date/time — used by datetime.llt.
        // now, parse-timestamp, format-timestamp, timestamp-*, duration-*, load-tz.
        // ~23 builtins.
        "datetime" => Some(datetime_builtins()),

        // Networking — used by lib-net-v3.llt.
        // http2-session, http3-session, quic-session, quic-open-stream,
        // quic-open-datagram, icmp-ping, tls-layer, tls-peer-cert.
        // ~18 builtins.
        "net"      => Some(net_builtins()),

        // SQL — used by sql.llt.
        // sql-open, sql-exec, proxy.
        "sql"      => Some(sql_builtins()),

        // Regex — used by regex.llt.
        "regex"    => Some(regex_builtins()),

        // Crypto — used by crypto.llt (hmac, etc.).
        // Note: blake3 is in "prelude" because prelude's include pipeline calls it directly.
        "crypto"   => Some(crypto_builtins()),

        // Type-stage modules (dispatched separately via type_builtin_module)
        // "type-resolvers", "type-classes", etc.

        _          => None,
    }
}
```

No `core_builtins()`. No global pre-injection. Every builtin enters scope only through an explicit `--- uses:` declaration on a document that needs it.

**One group per feature library.** Most stdlib .llt files are pure tinct (datetime.llt, io.llt, path.llt, async.llt tinct wrappers, codecs/json.llt, protocols/*.llt) and call only the `builtin-*` stable aliases already in the "prelude" group. They require no special group of their own. The feature-specific Rust groups (`"async"`, `"datetime"`, `"net"`, `"sql"`) exist precisely for the libraries that OWN those Rust builtins and provide the user-facing interface to them.

### macros.llt Merged into prelude.llt

`stdlib/macros.llt` is merged into `stdlib/prelude.llt` and deleted. The separation provided no benefit: macros.llt was always loaded unconditionally by the bootstrap (never opt-in), and macro transformer registration (`register_stdlib_macros_from_env`) works by name lookup from `stdlib_env` regardless of which source file defined them. Merging eliminates a second `load_stdlib_module` call, a second `include_str!`, and a conceptual boundary that existed only for organization.

The macro definitions (`[defmacro tmpl ...]`, `[defmacro do ...]`, `[defmacro begin ...]`, etc.) move into `prelude.llt` after the prelude functions they depend on.

### Processing — Evaluation Time Only

`--- uses:` is processed at **evaluation time only** (`eval_surface_document`): before evaluating any expressions, inject builtins from declared modules into the document environment. This mirrors how `--- caps:` injects `DirCap` values.

`--- uses:` does NOT need to be processed at expansion time, because macro transformer bodies already have full prelude access through their captured closure environments (see §Theoretically-Complete Expansion Environment below).

**Cross-document scoping (doc-local only):** `--- uses:` injections are scoped to the declaring document's evaluation env and must NOT accumulate forward to subsequent pipeline documents. Regular tinct dict bindings (exported values like `db: [sql-open ...]`) continue to propagate forward as always — this is how pipeline documents share context. The split is:

```text
Doc N evaluation:
  doc_env  = parent_env + --- uses: injections   ← doc-local, not forwarded
  evaluate N's expressions in doc_env
  forwarded_env = parent_env + N's exported dict bindings
  Doc N+1 baseline = forwarded_env               ← no raw builtins from --- uses:
```

This is correct because a value like `db` already carries its builtins in its closure (`[sql-open "/tmp/test.db"]` closed over `sql-open` when it was evaluated in doc N's env). Doc N+1 can use `db` without needing `sql-open` in scope. If doc N+1 needs raw access to `sql-exec` directly, it must declare `--- uses: ["sql"]` — making the dependency explicit and preventing accidental cross-document builtin leakage.

The TypeEnv follows the same split: `doc_typeenv = parent_typeenv + type_env_module(name)` for type-checking the current document; only the document's exported type bindings propagate to subsequent documents' `parent_typeenv`. This ensures T002 fires correctly if a later document uses a raw builtin without declaring it.

`standard_builtins()` is deleted. `create_root_env()` is made private/internal — it remains used by the bootstrap functions (`create_stdlib_env_inner`, `create_type_stage_env`) that ARE the initial bootstrap and cannot themselves be `--- uses:`-driven. User-facing code never calls it.

Unknown module names produce an error: `unknown native module: "typo"`.

### Theoretically-Complete Expansion Environment

The expander uses `EXPAND_MACROS_DEPTH` to prevent infinite recursion: at `depth == 0` it loads the full stdlib env; at `depth > 0` (re-entrant, triggered when `builtin_expand` is called from a macro body) it currently falls back to `create_root_env()` — raw builtins only, no prelude tinct functions.

This depth > 0 degradation is fixed by caching the `stdlib_env` after the depth == 0 bootstrap completes, then reusing it at depth > 0:

```rust
thread_local! {
    static CACHED_STDLIB_ENV: RefCell<Option<Arc<RwLock<Environment>>>> =
        RefCell::new(None);
}

// In expand_surface_program, depth == 0 branch:
let (env, arena) = create_stdlib_env_with_arena()?;
CACHED_STDLIB_ENV.with(|c| *c.borrow_mut() = Some(Arc::clone(&env)));

// In depth > 0 branch (replacing create_root_env() fallback):
let env = CACHED_STDLIB_ENV.with(|c| c.borrow().clone())
    .unwrap_or_else(|| builtins::create_root_env()); // fallback: pre-bootstrap edge case
```

**`expand_surface_program` is made async** as part of this sprint — the natural completion of the runtime-v2 migration for the expander. Currently the expander bridges into the async eval engine via `invoke_function_sync` and `materialize_sync`; these become `.await` calls. Once the expander is async:

- `builtin_expand` (already async) calling `expand_surface_program` (now async) is natural heap-allocated async mutual recursion — no call stack frames consumed
- The `em_depth > 10` guard is no longer needed for stack overflow prevention
- It is replaced by a simple logical depth counter in `EvalContext` (no thread-local, no RAII guard) that provides a clean error for pathological infinite macro expansion rather than an OOM

This sprint's change to expand.rs: `pub fn expand_surface_program` → `pub async fn expand_surface_program`; all `invoke_function_sync` → `.await`; all `materialize_sync` → `.await`; all callers updated.

### The Clean Bootstrap Sequence

```text
1. Parse prelude.llt           → SurfaceProgram (doc.uses = ["prelude"])
2. create_stdlib_env_inner:
     create_root_env()         → minimal bootstrap env (private)
     load prelude.llt          → evaluates all prelude + macro definitions
     cache stdlib_env          → CACHED_STDLIB_ENV for depth > 0 reuse
3. eval_surface_document (prelude):
     inject doc.uses builtins  → builtin_module("prelude") into document env
     evaluate prelude body     → map, filter, split, tmpl, do, ... defined here
4. User code evaluates         → inherits prelude env
     eval_surface_document:    → inject user's --- uses: builtins
     macro expansion:          → uses CACHED_STDLIB_ENV (full prelude always available)
```

No circular dependency. Each document is fully self-describing at evaluation time.

### Scoping

Builtins injected via `--- uses:` are visible to the declaring file and any code that includes it, but not globally. A tinct script that never includes `stdlib/sql.llt` never sees `sql-open` in its environment. The scope rules are identical to any other name binding.

### Type-Stage Documents

The same `--- uses:` mechanism applies to `--- stage: Type` documents, which run under a separate type-stage evaluator. Type-stage modules use distinct names prefixed with `"type-"`:

```tinct
--- stage: Type
--- uses: ["type-resolvers" "type-classes"]
---
[
  AddResult: [fn ...]
  SubResult: [fn ...]
]
```

The evaluator dispatches based on document stage: runtime documents call `builtin_module(name)`, type-stage documents call `type_builtin_module(name)`. Same syntax, different registries. The `"type-"` prefix makes the distinction unambiguous in source.

### TypeEnv Must Follow `--- uses:`

The type checker currently loads all builtin type signatures unconditionally from `TypeEnv::with_builtins()` (~3500 lines in `src/type_env.rs`). If `--- uses:` is implemented only at the runtime level without matching TypeEnv changes, the type checker would know about `sql-open` even when sql.llt is never included — a program using `sql-open` without `--- uses: ["sql"]` would pass type checking but fail at runtime. This violates the core correctness invariant: if the type checker approves a program, the runtime should not fail with an undefined-variable error for a builtin.

`type_env.rs` must be split into parallel group functions matching `builtin_module()`:

```rust
pub fn type_env_module(name: &str) -> Option<TypeEnv> {
    match name {
        "prelude"  => Some(prelude_type_env()),
        "async"    => Some(async_type_env()),
        "datetime" => Some(datetime_type_env()),
        "net"      => Some(net_type_env()),
        "sql"      => Some(sql_type_env()),
        "regex"    => Some(regex_type_env()),
        "crypto"   => Some(crypto_type_env()),
        _          => None,
    }
}
```

The type checker processes `--- uses:` from each document and extends the TypeEnv with the declared modules' type signatures before type-checking that document's expressions. This is the same mechanical split as `builtins.rs` — tedious in volume (~3500 lines) but not architecturally complex, and must be done in the same sprint as the runtime changes to preserve the correctness invariant.

**Cross-document scoping constraint:** `--- uses:` TypeEnv extensions are doc-local — they must NOT propagate forward via the `env = new_env` accumulation. Only the document's exported type bindings propagate. The implementation creates a per-document `doc_typeenv = parent_typeenv + type_env_module(name)` for checking that document, then discards the `--- uses:` extensions when computing the next document's `parent_typeenv`. This matches the runtime scoping exactly: doc-local builtin injection, forwarded tinct bindings.

**T002 heuristic update required:** `builtin_primary_names()` in `src/builtins.rs:2134` iterates `standard_builtins()`. Deleting `standard_builtins()` silently disables T002 detection for all builtins. Replace with aggregation across all `type_env_module()` groups, or a static `HashMap<&str, &str>` mapping builtin name → module name. This also enables a better T002 hint: `"builtin sql-open requires --- uses: [\"sql\"] in this document's header"` rather than the current suggestion to use `[include %libdir ...]`.

**Note on T002 severity:** T002 currently fires at `DiagnosticLevel::Warn` — evaluation proceeds regardless. The correctness invariant is advisory, not blocking. Hardening to a type error is a separate policy decision.

**`typecheck_surface_program_annotation_table` must respect `doc.uses`:** Called from `builtin_load` at `src/builtins_meta.rs:1763`, this function hardcodes `build_prelude_env()` at `src/typecheck.rs:74`, ignoring the loaded file's `--- uses:` header. It must be updated to build the TypeEnv per-document from `doc.uses`, or accept an externally-constructed TypeEnv from the caller.

**TypeEnv call sites (~10):** `TypeEnv::with_builtins()` is called at `src/lib.rs`, `src/imports.rs`, `src/typecheck.rs`, `src/lsp/analysis.rs`, `src/formatter.rs`, and `src/repl.rs`. All must be migrated when `with_builtins()` is deleted. The LSP completion functions at `src/lsp/analysis.rs:3530,3558` call `standard_builtins()` directly and must be updated to use `builtin_module()` groups filtered by the current document's `--- uses:` context. Capability type aliases (`Handle`, `NetCap`, `DirCap`, `Url`) registered in `with_builtins()` must move to `prelude_type_env()`.

### Relationship to Option 1

Options 1 and 2 are complementary: Option 1 loads code from outside the binary; Option 2 lazily activates code already inside the binary. A tinct stdlib library might use both — `stdlib/sqlite.llt` uses `--- uses: ["sql"]` to get tinct's built-in SQL layer, and `[extern "libsqlite3.so" ...]` to reach the system library underneath.

---

## Option 3: Cargo Workspace Split

This option addresses source organization and binary composition: feature code that is currently in `src/builtins.rs` (a single ~10k-line file) moves into separate Rust crates in a Cargo workspace. Each crate is paired with its corresponding stdlib `.llt` file.

### Workspace Structure

```text
tinct/
  Cargo.toml            # [workspace] members = ["crates/*", "."]
  Cargo.lock

  crates/
    tinct-core/         # parser, evaluator, type checker, core builtins (+, -, if, map, ...)
      Cargo.toml
      src/
        lib.rs          # pub use tinct_core::*

    tinct-net/          # network builtins (tcp-connect, tls-wrap, http-get, ...)
      Cargo.toml        # [dependencies] tinct-core = { path = "../tinct-core" }
      src/lib.rs        # pub fn net_builtins() -> Vec<BuiltinDef>

    tinct-sql/          # sql builtins (sql-open, sql-exec, proxy)
      Cargo.toml
      src/lib.rs

    tinct-datetime/     # datetime builtins
      Cargo.toml
      src/lib.rs

  src/                  # the tinct binary
    main.rs             # depends on tinct-core + feature crates

  stdlib/
    prelude.llt         # paired with tinct-core
    net.llt             # paired with tinct-net
    sql.llt             # paired with tinct-sql
    datetime.llt        # paired with tinct-datetime
```

Each feature crate depends on `tinct-core` and exposes a single public function:

```rust
// crates/tinct-net/src/lib.rs
pub fn net_builtins() -> Vec<tinct_core::BuiltinDef> {
    vec![
        builtin!("tcp-connect", builtin_tcp_connect, [...], 2),
        builtin!("tls-wrap",    builtin_tls_wrap,    [...], 1),
    ]
}
```

The `builtin_module()` registry in `tinct-core` is populated by the binary crate at startup:

```rust
// src/main.rs
fn main() {
    tinct_core::register_module("net",      tinct_net::net_builtins);
    tinct_core::register_module("sql",      tinct_sql::sql_builtins);
    tinct_core::register_module("datetime", tinct_datetime::datetime_builtins);
    // ...
    tinct_core::run();
}
```

### Static vs Dynamic Linking

#### Option 3A: Static linking (recommended first step)

All feature crates are statically linked into the `tinct` binary at compile time. A minimal binary (`tinct-minimal`) can be built by simply not linking the feature crates. No runtime changes — the `native-module` registry from Option 2 provides the lazy scoping.

Cargo feature flags make this configurable:

```toml
# Cargo.toml (binary crate)
[features]
default = ["net", "sql", "datetime"]
net      = ["dep:tinct-net"]
sql      = ["dep:tinct-sql"]
datetime = ["dep:tinct-datetime"]
```

#### Option 3B: Dynamic linking (plugin model)

Feature crates compile as `cdylib`. The binary `dlopen`s them at runtime when a `native-module` call is first made for that module name. Each crate exports a C ABI registration function:

```rust
// crates/tinct-net/src/lib.rs
#[no_mangle]
pub extern "C" fn tinct_register(registry: *mut tinct_core::BuiltinRegistry) {
    unsafe {
        (*registry).register("tcp-connect", builtin_tcp_connect, &[...], 2);
    }
}
```

This allows distributing `tinct-net.so` separately from the tinct binary. The cost: `BuiltinRegistry` must be `#[repr(C)]` and ABI-stable. The `abi_stable` crate solves this but adds complexity. Host and plugin **must be compiled with the same Rust toolchain** or ABI mismatches silently corrupt memory.

**Option 3B is significantly more complex and should only be adopted if distributing feature crates independently from the tinct binary is a concrete requirement.**

### File Organization Within `tinct-core`

The current monolithic `src/builtins.rs` (~10k lines) splits naturally:

| New file | Contents |
|---|---|
| `crates/tinct-core/src/builtins_core.rs` | Arithmetic, comparison, control, `if`, `error`, `raise` |
| `crates/tinct-core/src/builtins_collections.rs` | `map`, `filter`, `reduce`, `take`, `drop`, `keys`, `merge` |
| `crates/tinct-core/src/builtins_strings.rs` | `str`, `split`, `join`, `trim`, `starts-with`, `ends-with` |
| `crates/tinct-core/src/builtins_io.rs` | `load`, `expand`, `eval-ast`, `blake3`, cache primitives |
| `crates/tinct-net/src/lib.rs` | `tcp-connect`, `tls-wrap`, `http-get`, async channel ops |
| `crates/tinct-sql/src/lib.rs` | `sql-open`, `sql-exec`, `proxy` |
| `crates/tinct-datetime/src/lib.rs` | `now`, `timestamp`, `duration`, `format-date` |

`tinct-core` exposes `builtin_module(name)` — the same registry function used by `--- uses:` processing. Feature crates register their groups into this registry. The feature crates are never referenced from `tinct-core` — the dependency flows one way (feature → core), not the other.

---

## Comparison and Relationships

| | Option 1: External FFI | Option 2: Native Modules | Option 3: Workspace Split |
|---|---|---|---|
| **Purpose** | Call code outside the binary | Lazily activate code inside the binary | Organize source; enable slim builds |
| **Requires rebuild?** | No | No | Yes (compile-time choice in 3A; no in 3B) |
| **ABI concern?** | C ABI only | None — same crate, same toolchain | None (3A); significant (3B) |
| **tinct-side syntax** | `[extern "lib.so" ...]` | `--- uses: ["sql"]` header | No new syntax |
| **Scope of adoption** | User scripts + stdlib | stdlib files | Source organization |
| **Dependencies** | `libloading`, optionally `libffi` | None | Cargo workspace restructure |
| **Risk** | Medium (unsafe marshalling) | Low (pure Rust, same binary) | Low (3A) / High (3B) |

Options 2 and 3A are designed to be adopted together: the workspace split (3A) provides source separation, and the native module registry (2) provides the runtime scoping mechanism. Neither requires the other, but they compose cleanly.

Option 1 (external FFI) is independent and addresses a different user story: a tinct script author who wants to call a system library without writing any Rust at all.

## What Would Change

### Option 1 Changes

#### `src/value.rs` — Two new variants

**`Value::OpaqueHandle { ptr: usize, drop_fn: Option<unsafe extern "C" fn(usize)> }`** — distinct from `Number`, no arithmetic, optional drop hook. One new variant; all existing match arms need a branch (compile error catches omissions).

**`Value::FfiFunction { name: String, symbol: libloading::Symbol<'static, unsafe extern "C" fn()>, sig: FfiSig }`** — the symbol is kept alive by a `Library` handle stored in `EvalState`. `FfiSig` is a small enum encoding return type + parameter types.

**Impact:** Minor — two new variants.

#### `src/eval.rs` — New dispatch arm

Add a `Value::FfiFunction` arm to function application. Before the call: force all arguments, marshal to C types. After: unmarshal return based on `FfiSig`. Marshalling lives in `src/ffi.rs`.

**Impact:** Minor — one new match arm.

#### `src/ffi.rs` — New module

~300–500 lines encapsulating library loading, type marshalling, symbol resolution (kebab→snake), `OpaqueHandle` drop machinery.

**Impact:** Moderate — new file; pulls in `libloading`.

#### Parser / Grammar

Preferred: `[extern "path" name: sig ...]` desugars at parse time to `[call $ffi-load "path" [name: sig ...]]`. `ffi-load` is a Rust builtin that opens the library and returns a dict of `Value::FfiFunction` bindings. No evaluator awareness of `extern` needed.

**Impact:** Minor — one new parse rule.

#### `src/typecheck.rs`

`FfiFunction` values are assigned function types from `FfiSig`. `Handle` maps to a new primitive type distinct from `Number` — the type checker rejects `Handle` where `Number` is expected.

**Impact:** Minor — new primitive type.

#### `Cargo.toml` — New dependencies

- `libloading = "0.8"` — cross-platform `dlopen`/`LoadLibrary` wrapper
- `libffi = "3.2"` — optional; only needed for variadic functions or struct-return conventions

---

### Option 2 Changes

#### `src/builtins.rs` — Delete `standard_builtins()`, add registry

**Delete:** `standard_builtins()` — no global pre-injection.

**Make private:** `create_root_env()` — retained as a private bootstrap function used only by `create_stdlib_env_inner()` and `create_type_stage_env()`, which are the initial stdlib load entry points and cannot themselves be `--- uses:`-driven. Not called from user-facing code.

**Add:** `builtin_module(name: &str) -> Option<Vec<BuiltinDef>>` — static match across all named groups. Each group is a `fn` returning `Vec<BuiltinDef>`: `arithmetic_builtins()`, `string_builtins()`, `collection_builtins()`, `io_builtins()`, `math_builtins()`, `meta_builtins()`, `net_builtins()`, `async_builtins()`, `sql_builtins()`, `datetime_builtins()`, `crypto_builtins()`, `regex_builtins()`. These `fn`s can remain in the same file or be split into separate files (the latter pairs naturally with Option 3A workspace split).

**Impact:** Moderate — `standard_builtins()` deleted; all call sites of `create_root_env()` updated.

#### `src/expand.rs` — Expander environment from `--- uses:`

**Replace:** the `depth > 0` fallback to `create_root_env()` with the cached `CACHED_STDLIB_ENV` (see §Theoretically-Complete Expansion Environment). The `EXPAND_MACROS_DEPTH` thread-local and `DepthGuard` are retained; only the env constructed in the `depth > 0` branch changes. The `em_depth > 10` guard is also retained.

**No expansion-time `--- uses:` processing needed:** macro transformer bodies close over the full `stdlib_env` at registration time, so they always have prelude available regardless of the document's `--- uses:` declaration. `--- uses:` is evaluation-time only.

**Impact:** Moderate — removes the re-entrant special case; simplifies the expander significantly.

#### `src/ast.rs` — `SurfaceDocument.uses` field

New field: `pub uses: Option<Spanned<Vec<String>>>` alongside the existing `expects`, `caps`, `name`, `stage` fields.

**Impact:** Minor — one new field; parser and all `SurfaceDocument` construction sites updated.

#### `src/parser.rs` — `--- uses:` header parsing

Parse `uses: ["sql" "net"]` in the document header block (same pass as `caps:` and `expects:`). The value is a list of string literals.

**Impact:** Minor — one new header key.

#### `src/eval.rs` — `--- uses:` injection at evaluation time

`eval_surface_document` reads `doc.uses` before evaluating any expressions. For each module name, calls `builtin_module(name)` and injects resulting `BuiltinDef` entries into the document environment. Unknown names error immediately. Timing mirrors `--- caps:` processing.

**Impact:** Minor — a few lines in the document evaluation path.

#### `stdlib/macros.llt` — deleted

Merged into `prelude.llt`. Remove from `src/builtins.rs`: the `load_stdlib_module(macros_source, "macros", ...)` call and its `include_str!`. Update `src/expand.rs`: `register_stdlib_macros_from_env` removes the `include_str!("../stdlib/macros.llt")` scan — it already works by looking up macro names from `stdlib_env` by name.

**Impact:** Minor cleanup — one fewer stdlib file, one fewer `load_stdlib_module` call.

#### `stdlib/prelude.llt` and feature stdlib files

`prelude.llt` absorbs macros.llt content and adds `--- uses:` header:

```tinct
--- uses: ["prelude"]
---
[
  map:   [fn ...]
  ...
  # macro definitions (formerly macros.llt)
  tmpl:  [defmacro ...]
  do:    [defmacro ...]
]
```

Feature libraries declare only their own Rust deps — no prelude builtins needed since those arrive via `[include prelude]`:

```tinct
# lib-net-v3.llt — owns its networking and concurrency Rust builtins directly
--- uses: ["net" "async"]
---
[
  http-get:     [fn ...]
  tcp-connect:  [fn ...]
  make-serve-layer: [fn ...]
]

# datetime.llt — owns its date/time Rust builtins
--- uses: ["datetime"]
---
[
  now:             [fn ...]
  parse-timestamp: [fn ...]
]

# sql.llt — owns its SQL Rust builtins
--- uses: ["sql"]
---
[
  sql-open: [fn ...]
  sql-exec: [fn ...]
]
```

Pure tinct libraries (path.llt, codecs/json.llt, protocols/dns.llt, etc.) need no `--- uses:` at all — they are implemented entirely in tinct using prelude functions.

**Impact:** Minor — additive header change to each stdlib file; makes Rust dependencies explicit and visible in source.

#### `src/type_env.rs` — TypeEnv split (same sprint)

`TypeEnv::with_builtins()` is split into per-group functions mirroring `builtin_module()`: `prelude_type_env()`, `async_type_env()`, `datetime_type_env()`, `net_type_env()`, `sql_type_env()`, etc. The type checker extends the TypeEnv with declared modules' type signatures when processing each document's `--- uses:` header — same phase as the runtime injection. `TypeEnv::with_builtins()` is deleted alongside `standard_builtins()`.

**Impact:** Major in volume (~3500 lines reorganized), moderate in code — mechanical split, same groups as `builtins.rs`. Must be done in the same sprint as the runtime changes to preserve the correctness invariant (type checker approval implies runtime availability).

#### Test suite

Tests asserting the count or names of `standard_builtins()` need updating. Tests for feature builtins that no longer live in `standard_builtins()` must ensure their test environment loads the relevant module via `--- uses:` or direct `builtin_module()` injection.

**Impact:** Minor — mechanical updates to builtin count assertions and test environment setup.

---

### Option 3A Changes (Workspace Split, Static Linking)

#### `Cargo.toml` — Workspace root

New `[workspace]` Cargo.toml with members `[".", "crates/*"]`. Feature crates added to `[dependencies]` of the binary crate, gated by feature flags.

**Impact:** Moderate — repository restructure; CI and build scripts need updating.

#### `crates/tinct-core/` — Extracted core library

Parser, evaluator, type checker, and core builtins extracted from `src/` into a library crate. The binary crate (`src/main.rs`) becomes a thin wrapper that links everything together and calls `tinct_core::run()`.

**Impact:** Major for repository structure; Moderate for code (mostly `Cargo.toml` + `mod` changes, few actual logic changes).

#### `crates/tinct-{feature}/` — Feature crates

Each feature crate:

- `[dependencies]` on `tinct-core` only (no circular deps)
- Exposes `pub fn {feature}_builtins() -> Vec<tinct_core::BuiltinDef>`
- Paired with `stdlib/{feature}.llt`

**Impact:** Minor per crate once workspace is established.

#### Option 3B additional changes (Dynamic Linking)

- Each feature crate compiles as `cdylib` (`[lib] crate-type = ["cdylib"]`)
- Exports `#[no_mangle] pub extern "C" fn tinct_register(registry: *mut BuiltinRegistry)`
- `BuiltinRegistry` must be `#[repr(C)]` and ABI-stable; consider `abi_stable` crate
- `--- uses:` processing in the runtime calls `dlopen` + symbol lookup + `tinct_register` on first access for dynamically-linked modules
- **Hard constraint:** host and plugin compiled with identical Rust toolchain version

**Impact:** Major — ABI stability concerns, `abi_stable` dependency, significantly more complex CI (each feature crate has its own artifact).

## Prerequisites

### Option 1 (External FFI)

- No blocking prerequisites — independent of type system, async, stdlib expansion.
- Capability model: FFI library loading should be gated like filesystem access. Not blocking for initial implementation.
- Effect system: if tinct gains formal effects, FFI calls should be tagged effectful. Not blocking.

### Option 2 (Native Modules)

- No blocking prerequisites — purely additive restructuring of existing code.
- Naturally adopted alongside or before Option 3A.

### Option 3A (Workspace, Static)

- Option 2 is not required but should be implemented first so that the module registration protocol exists before the crates are split.
- No external prerequisites.

### Option 3B (Workspace, Dynamic)

- Option 3A complete — dynamic is a progression of static, not a replacement.
- Concrete use case: a reason to distribute feature crates independently from the tinct binary. Without this, the complexity is not justified.

## References

- `libloading` crate documentation. — Cross-platform `dlopen`/`LoadLibrary` wrapper; foundation of Option 1 and Option 3B.
- `abi_stable` crate documentation. — Stable vtable and repr-C types for Rust-to-Rust plugin boundaries; required for Option 3B.
- LuaJIT FFI design. — Closest precedent for Option 1: `ffi.cdef` declares C types inline; `ffi.load` opens a library. tinct's `extern` block is a simplified version of this model.
- Python `ctypes` documentation. — Mature scalar-only FFI: `c_int`, `c_double`, `c_char_p`, `c_void_p`. Type annotation table in Option 1 draws from `ctypes` conventions.
- Rust Reference. "FFI." — C ABI conventions tinct must produce on the calling side; `extern "C"` function pointers and `#[repr(C)]` layout rules.
- Rust Reference. "Cargo Workspaces." — Workspace member configuration, inter-crate dependencies, feature flags for optional members.
