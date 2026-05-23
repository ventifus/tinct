# What If: Foreign Function Interface and Native Module Extensions for tinct

**State:** Proposal

What would it take to call C and Rust libraries from tinct programs, and how should tinct's own Rust builtins be organized so that feature libraries can bring their own native code without requiring it in prelude?

This whatif covers three related but distinct approaches to the same underlying problem: tinct's extension surface. They can be adopted independently or in combination.

| Approach | Scope | Mechanism | Binary impact |
|---|---|---|---|
| [Option 1: External C/Rust FFI](#option-1-external-crust-ffi-extern-block) | Call any C ABI library | `extern` block + `libloading` | None — loaded at runtime |
| [Option 2: In-Tree Native Modules](#option-2-in-tree-native-modules-builtin-registry) | Lazy activation of compiled-in builtins | `native-module` builtin + registry | Code already in binary |
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

## Option 2: In-Tree Native Modules (Builtin Registry)

This option addresses a different problem: tinct's own Rust builtins that are already compiled into the binary but should only enter scope when the corresponding stdlib library is included. No external library loading — the code is already there, it just shouldn't be globally pre-injected.

### The Problem in More Detail

`stdlib/sql.llt` needs `sql-open`, `sql-exec`, and `proxy` to exist in the environment. Currently, these are pre-injected by `create_root_env()` at startup. If `sql.llt` is never included, those builtins were loaded for nothing. More importantly, there is no explicit link in the source between `stdlib/sql.llt` and its Rust dependencies — the dependency is implicit and invisible.

### The Builtin Module Registry

Rust code in `src/builtins.rs` (or split into separate files per feature) declares named groups of builtins:

```rust
// In builtins.rs (or src/builtins_sql.rs, included via mod)
fn sql_builtins() -> Vec<BuiltinDef> {
    vec![
        builtin!("sql-open",  builtin_sql_open,  [Strictness::Seq, Strictness::Seq], 2),
        builtin!("sql-exec",  builtin_sql_exec,  [Strictness::Seq, Strictness::Seq, Strictness::Seq], 3),
        builtin!("proxy",     builtin_proxy,     [Strictness::Seq], 1),
    ]
}

pub fn builtin_module(name: &str) -> Option<Vec<BuiltinDef>> {
    match name {
        "core"       => Some(core_builtins()),
        "collection" => Some(collection_builtins()),
        "string"     => Some(string_builtins()),
        "math"       => Some(math_builtins()),
        "io"         => Some(io_builtins()),
        "net"        => Some(net_builtins()),
        "sql"        => Some(sql_builtins()),
        "datetime"   => Some(datetime_builtins()),
        _            => None,
    }
}
```

`standard_builtins()` is replaced by `core_builtins()` — only the language core (arithmetic, comparison, control, strings, collections). Feature builtins are never pre-injected.

### The `native-module` Builtin

A new Rust builtin `native-module` takes a module name string, looks it up in `builtin_module()`, and returns a dict of `Value::Builtin` entries:

```rust
fn builtin_native_module(args: Vec<Value>, _state: &mut EvalState) -> EvalResult {
    let name = args[0].as_string()?;
    let defs = builtin_module(&name)
        .ok_or_else(|| EvalError::runtime(format!("unknown native module: {name}"), ...))?;
    let mut dict = IndexMap::new();
    for def in defs {
        dict.insert(def.name.to_string(), Value::Builtin(def));
    }
    Ok(Value::Dict(dict))
}
```

Because it returns a plain dict, it composes naturally with `include`:

```tinct
# stdlib/sql.llt — declares its own Rust dependencies explicitly
[include [native-module "sql"]]

# Now sql-open, sql-exec, proxy are in scope for the rest of this file
sql-open: [fn@Handle [path@String]
    [let [db [call $sql-open-raw path]]
    ...]]
```

`prelude.llt` similarly declares its own:

```tinct
[include [native-module "core"]]
[include [native-module "collection"]]
[include [native-module "string"]]
[include [native-module "math"]]
```

### Scoping

Builtins loaded via `[include [native-module "sql"]]` enter scope exactly like any other `include` result — they are visible to the including file and any code that includes it, but not globally. A tinct script that never includes `stdlib/sql.llt` never sees `sql-open` in its environment.

### Relationship to Option 1

Options 1 and 2 are complementary: Option 1 loads code from outside the binary; Option 2 lazily activates code already inside the binary. A tinct stdlib library might use both — `stdlib/sqlite.llt` does `[include [native-module "sql"]]` to get tinct's built-in SQL layer, and `[extern "libsqlite3.so" ...]` to reach the system library underneath.

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

`standard_builtins()` in `tinct-core` becomes `core_builtins()` and returns only the language-core entries. The feature crates are never referenced from `tinct-core` — the dependency flows one way (feature → core), not the other.

---

## Comparison and Relationships

| | Option 1: External FFI | Option 2: Native Modules | Option 3: Workspace Split |
|---|---|---|---|
| **Purpose** | Call code outside the binary | Lazily activate code inside the binary | Organize source; enable slim builds |
| **Requires rebuild?** | No | No | Yes (compile-time choice in 3A; no in 3B) |
| **ABI concern?** | C ABI only | None — same crate, same toolchain | None (3A); significant (3B) |
| **tinct-side syntax** | `[extern "lib.so" ...]` | `[include [native-module "sql"]]` | No new syntax |
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

#### `src/builtins.rs` — Registry function + `native-module` builtin

**`builtin_module(name: &str) -> Option<Vec<BuiltinDef>>`** — static match mapping module names to builtin groups. `standard_builtins()` is narrowed to `core_builtins()` (language primitives only). Feature groups (`net_builtins()`, `sql_builtins()`, etc.) move to separate `fn`s or separate files within the same crate.

**`native-module`** — new Rust builtin: takes a name string, returns a dict of `Value::Builtin` entries from the registry. Errors on unknown module names.

**Impact:** Moderate — restructures how builtins are registered; `standard_builtins()` shrinks; `create_root_env()` injects only core builtins.

#### `stdlib/prelude.llt` and feature stdlib files

`prelude.llt` adds explicit native module includes at the top:

```tinct
[include [native-module "core"]]
[include [native-module "collection"]]
[include [native-module "string"]]
[include [native-module "math"]]
[include [native-module "io"]]
```

Feature stdlib files declare their own:

```tinct
# stdlib/net.llt
[include [native-module "net"]]
```

**Impact:** Minor — additive changes to stdlib files; makes Rust dependencies explicit and visible.

#### Test suite

Tests asserting the count or names of `standard_builtins()` will need updating. Tests for feature builtins need to bootstrap via `native-module` if they no longer live in `standard_builtins()`.

**Impact:** Minor — mechanical updates to builtin count assertions.

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
- `native-module` in the runtime calls `dlopen` + symbol lookup + `tinct_register` on first access
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
