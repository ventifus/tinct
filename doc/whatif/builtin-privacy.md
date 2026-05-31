# What If: Rust Primitive Privacy via `--- uses:`

**State:** Accepted — 2026-05-28

**Note:** Original design (2026-05-11) used `[include %rust ...]` — deleted by `include-decomp-redelete` sprint. This revision uses `--- uses:` document headers, which work with the current architecture where `include` is a prelude function.

What would it take to make every Rust primitive invisible to user code by default, exposing only what tinct's stdlib explicitly re-exports?

## Current State

All 238 Rust builtins are pre-injected into the global environment at startup via `standard_builtins()` → `create_root_env()`. User programs inherit `stdlib_env`, which is a child of `bootstrap_env` (= `create_root_env()`), so they can traverse the parent chain and call any builtin by name — `builtin-write`, `builtin-eval`, `open`, `write`, `load`, and ~230 others — without going through prelude.

```rust
// src/builtins.rs:2337-2378 — create_stdlib_env_inner()
let bootstrap_env = create_root_env();          // all 238 builtins
let stdlib_env = Environment::with_parent(      // prelude loads here
    Arc::clone(&bootstrap_env)
);
// user code is a child of stdlib_env → can walk to bootstrap_env
```

The comment in `create_stdlib_env_inner()` acknowledges the gap: *"This means: user code (child of stdlib_env) can walk up to bootstrap_env and see all builtins. The prelude acts as the primary scope boundary."* The scope boundary is not enforced — it is aspirational.

Additionally, `TypeEnv::with_builtins()` (`src/type_env.rs:1224–3776`, ~2553 lines) loads type signatures for all 238 builtins unconditionally, regardless of what the program imports.

### What's Missing

1. No mechanism for stdlib files to declare their Rust dependencies explicitly
2. No enforcement that user code cannot call builtins directly
3. Type checker approves programs using `builtin-write` even without any import
4. ~50 builtins still registered under bare names without `builtin-*` prefix, including all builder ops (`make-builder`, `builder-set`, etc.) and I/O ops (`write`, `open`, etc.) (B-168)

## Design

### The Core Insight: Tinct's Own Scoping Already Provides the Isolation

Tinct's two-dict pattern already demonstrates how to hide implementation details:

```tinct
[
  builtin-if: [...]   # local scope — not exported
]
[
  if: builtin-if      # exported — only this name is visible to includers
]
```

A program that includes this gets the second dict. `builtin-if` is not in the exported value and is unreachable from the caller. No special isolation machinery is needed.

`--- uses:` works the same way. `eval-program` collects the named modules (via `builtin-module`) into a scope dict and passes it to `eval`, which seeds a fresh document-local env frame — like the first dict above. The document's exported dict contains only what it explicitly names. The `builtin-*` names used internally never appear in the exported dict and are therefore unreachable by user code.

### `--- uses:` Headers

A stdlib file declares its Rust builtin dependencies in its document header:

```tinct
--- uses: ["core"]
---
[
  map:   [fn ...]
  write: builtin-write    # re-exported with public name
  if:    builtin-if
  ...
]
```

`--- uses:` is a document header key parsed into `SurfaceDocument.uses`. At evaluation time, `eval-program` reads each document's declared module names, calls `module name` (a tinct-callable builtin — see §Builtin Module Registry) for each, merges the resulting dicts, and passes the combined dict as `scope:` to `eval`. `eval` seeds a fresh document-local env frame with those bindings before evaluating the document's expressions. The document exports only what it explicitly names.

**Doc-local scoping:** the merged module dict is the starting env frame for that document only. It does not propagate forward through the pipeline — only the document's exported tinct bindings propagate. Closures capture their builtins from this frame as usual.

**Unknown module names** produce an immediate error at the `module` call site: `unknown native module: "typo"`.

### Bootstrap Sequence (After This Whatif)

Bootstrap has four phases. Only Phase 1 is entirely Rust. Phase 2 onward is tinct.

```text
Phase 1 — Rust: build core dict
  core-dict = builtin_module("core")   → Value::Dict of all core builtins

Phase 2 — Rust evaluates stdlib/loader.llt with scope: core-dict
  loader-dict = eval(loader.llt, scope: core-dict)
    → { eval-program: <fn>, eval-programs: <fn> }
  (loader.llt is a privileged file; Rust hardcodes scope: core-dict for it)

Phase 3 — Rust evaluates prelude.llt directly (direct eval path, ~10x faster than eval-programs)
  prelude_env = child of loader_env (inherits core builtins) + loader_dict entries
  prelude-dict = eval_surface_file(prelude.llt, env: prelude_env)
    → prelude evaluates directly; core builtins are already in prelude_env from Phase 1
    → prelude's --- uses: ["core"] header is metadata only during bootstrap
       (it becomes machine-read in Phase 4 when T-768/T-770 CLI wiring lands)
    → prelude-dict = { map, write, if, task, module, eval-program, eval-programs, ... }
  prelude-dict held for process lifetime

Phase 4 — CLI builds program list and calls eval-programs
  programs = [input-prog, user-prog1, user-prog2, ..., output-prog]
             (-i prepends from stdlib/cli/in/, -o appends from stdlib/cli/out/)
  result = prelude-dict["eval-programs"](programs, [])
    → threads % through each program in sequence
    → return value of each becomes % for the next, regardless of how it named its output
    → all caps (%libdir, %cwd, %stdin, %emit, %stdout, ...) are in the env chain,
      accessible equally from all programs
```

`builtin-map`, `builtin-write`, `builtin-task`, `builtin-send` etc. are in prelude's env during Phase 3 but are not in `prelude-dict` unless prelude explicitly re-exports them. User code receives only `prelude-dict` and cannot reach raw `builtin-*` names.

**Prelude is not special.** Once Phase 4 CLI wiring lands (T-768/T-770), the same `eval-programs` mechanism will load prelude and run the full user pipeline. At that point prelude's `--- uses: ["core"]` header becomes machine-read and load-bearing. Until then, prelude's header is documentation that describes its dependency.

**"Cloning" the prelude scope is O(1).** `prelude-dict` is an immutable `Value::Dict`. Each user program gets a fresh env frame seeded from it — `Arc::clone` references, no copying. `STDLIB_RESULT_CACHE` (builtins.rs) caches the `(env, arena)` pair for the process lifetime; no separate `CACHED_STDLIB_ENV` is needed.

### Builtin Module Registry

`standard_builtins()` is deleted. Builtins are organized into named groups. Each group is co-located with its type env in a single source file (see §File Structure):

```rust
// src/builtins.rs — only the registry; no builtin definitions
//
// Note: builtin_module() returns Option<Vec<BuiltinDef>>, not Option<Value>.
// The Vec<BuiltinDef> → Value::Dict conversion happens in the tinct-callable
// wrapper (builtin_builtin_module in builtins_meta.rs) and in bootstrap code.
pub fn builtin_module(name: &str) -> Option<Vec<BuiltinDef>> {
    match name {
        "core"     => Some(builtins_core::core_builtins()),
        "datetime" => Some(builtins_datetime::datetime_builtins()),
        "net"      => Some(builtins_net::net_builtins()),
        _          => None,
    }
}

// The tinct-callable wrapper (registered as "builtin-module" in core):
pub fn builtin_builtin_module(args: BuiltinArgs, ctx: &Arc<EvalContext>) -> EvalResult {
    let name = require_string(&args.args[0], ctx, "builtin-module")?;
    match builtin_module(&name) {
        Some(defs) => {
            // Convert Vec<BuiltinDef> to Value::Dict
            let map = defs.into_iter()
                .map(|def| (Key::from(def.name), ctx.alloc_thunk(...)))
                .collect();
            Ok(Value::Dict(map))
        }
        None => Err(EvalError::user_error(
            format!("unknown native module: {:?}", name), args.call_span,
        )),
    }
}

pub fn type_env_module(name: &str) -> Option<TypeEnv> {
    match name {
        "core"     => Some(builtins_core::core_type_env()),
        "datetime" => Some(builtins_datetime::datetime_type_env()),
        "net"      => Some(builtins_net::net_type_env()),
        _          => None,
    }
}
```

**`builtin-module` — tinct-callable.** `builtin_module` is exposed to tinct as `builtin-module name → Dict`. It is registered in "core" so it is available immediately. `loader.llt` uses it directly as `builtin-module`; prelude re-exports it as `module: builtin-module` for user code.

**`eval-program`** and **`eval-programs`** are both defined in `loader.llt` (see §`stdlib/loader.llt`). `eval-programs` is the real CLI entry point — it takes `[Seq Program]` and threads `%` through each. `eval-program` is the single-program helper called by `eval-programs`. Prelude re-exports both.

`eval scope: dict` is a new named argument on the `eval` builtin. It seeds the document-local env frame with the entries from `dict` before evaluating the expressions. Unknown module names error at the `builtin-module` call: `[builtin-module "typo"]` → `unknown native module: "typo"`.

### Module Contents — Exact Builtin Lists

**`"core"` → `src/builtins_core.rs`** (new aggregator file; implementations stay in existing split files)

| Subgroup | Builtins | Source file |
|----------|----------|-------------|
| Arithmetic | `+` `builtin-add` `-` `builtin-sub` `*` `builtin-mul` `/` `builtin-div` | `builtins.rs` |
| Comparison | `=` `builtin-eq` `<` `builtin-lt` `builtin-gt` `builtin-lte` `builtin-gte` | `builtins.rs` |
| Control | `if` `builtin-if` `builtin-raise` `builtin-macro-error` `builtin-try` `until` | `builtins.rs` |
| Dict | `builtin-keys` `builtin-length` `builtin-merge` `builtin-append` `builtin-get` `get?` `builtin-each` `builtin-each-key` `builtin-each-kv` `builtin-build-dict` `validate` | `builtins_dict.rs` |
| String | `builtin-str` `builtin-split` `builtin-replace` `builtin-trim` `builtin-trim-start` `builtin-trim-end` `builtin-str-length` `builtin-str-slice` `builtin-str-chars` `builtin-char-code` `builtin-chr` `builtin-str-bytes` `builtin-bytes-str` `builtin-str-index-of` `builtin-str-to-upper-char` `builtin-str-to-lower-char` `builtin-str-map-chars` `builtin-regex-match?` | `builtins_string.rs` |
| Bytes | `bytes` `bytes-find` `bytes-of` `bytes-equal?` `ct-equal?` | `builtins_bytes.rs` |
| Math | `builtin-floor` `builtin-round` `builtin-pow` `builtin-sqrt` `builtin-log` `builtin-log2` `builtin-log10` `builtin-exp` `builtin-sin` `builtin-cos` `builtin-tan` `builtin-asin` `builtin-acos` `builtin-atan` `builtin-atan2` `builtin-nan?` `builtin-inf?` `builtin-finite?` `builtin-float` `builtin-to-int` `builtin-to-float` `builtin-band` `builtin-bor` `builtin-bxor` `builtin-shl` `builtin-shr` `builtin-decimal` `builtin-big-int` | `builtins_math.rs` |
| Sequences | `builtin-seq` `builtin-head` `builtin-tail` `builtin-collect` `builtin-range` `builtin-repeat` `builtin-cycle` `builtin-iterate` `builtin-unfold` `builtin-map` `builtin-filter` `builtin-take` `builtin-drop` `builtin-reduce` `builtin-join` `builtin-concat` `builtin-first` `builtin-last` `builtin-rest` `builtin-cons` `builtin-reverse` `builtin-sort` | `builtins_seq_*.rs` |
| Meta | `materialize` `builtin-apply` `builtin-type-of` `ast-of` `expand` `builtin-expand` `load` `builtin-load` `eval` `eval-types` `include-cache-get` `include-cache-put` `blake3` `cap-identity` `builtin-gensym` `builtin-llt-repr` `builtin-tag-of` `builtin-variant` `builtin-macro-injects` | `builtins_meta.rs` |
| Async | `builtin-task` `builtin-await` `builtin-par` `builtin-par-map` `builtin-par-filter` `builtin-channel` `builtin-send` `builtin-recv` `builtin-select-once` `builtin-signal-channel` `builtin-timer-channel` `builtin-watch-channel` `builtin-context` `builtin-with-cancel` `builtin-with-timeout` `builtin-with-deadline` `builtin-with-context` `builtin-non-cancellable` `builtin-cancelled-q` `builtin-cancel-task` `builtin-cancel-root` `builtin-drain` `builtin-exit-now` | `builtins_async.rs` |
| Builder | `builtin-make-builder` `builtin-builder-set` `builtin-builder-delete` `builtin-builder-finish` `builtin-builder-snapshot` `builtin-builder-has?` `builtin-builder-get` `builtin-builder-get-or` `builtin-proxy` | `builtins_dict.rs` (builder ops) + `builtins.rs` (proxy) — renamed from bare names as part of B-168 |
| I/O | `open` `builtin-read-all` `builtin-read-chunk` `builtin-emit` `builtin-env` `builtin-narrow` `builtin-list-dir` `string-handle` `write` `write-atomic` `write-handle` `flush` `close` `raw-create` `seek` `seek-end` `position` `stat` `exists` `stat-symlink` `copy-file` `symlink` `set-permissions` `make-dir` `builtin-remove` `rename` `link` `read-link` `get-xattr` `set-xattr` `remove-xattr` `list-xattrs` `revocable` `revoke-cap` `cap-data` `has-cap?` | `builtins_io.rs` — aggregated into core. `builtin-read-all` reads a Handle to String in one call, used internally by the include pipeline; **not re-exported** from prelude (user code uses lazy `lines` or chunked `read-chunk`). Prelude re-exports only `emit`, `open`, and `lines`; all other I/O names are available only to programs that declare `--- uses: ["core"]` in stdlib files |

`reduce_dict_step` and `reduce_seq_step` are **removed from `standard_builtins()`** entirely. They are Rust continuation helpers invoked only via `Thunk::new_pending_builtin` with embedded function pointers — never looked up by name from tinct code. No env registration required.

**`"datetime"` → `src/builtins_datetime.rs`** (extend existing file; add `datetime_type_env()`)

`parse-timestamp` `format-timestamp` `timestamp->unix` `unix->timestamp` `now` `fixed-clock` `timestamp-add` `timestamp-diff` `timestamp<?` `timestamp>?` `timestamp=?` `timestamp-year` `timestamp-month` `timestamp-day` `timestamp-hour` `timestamp-minute` `timestamp-second` `timestamp-parts` `duration-nanos` `duration-seconds` `duration-minutes` `duration-hours` `duration-days` `duration->seconds` `duration->nanos` `load-tz` `timestamp-in-tz` `local->timestamp` `local-tz-name`

**`"net"` → `src/builtins_net.rs`** (new file; pulls from `builtins_io.rs` + `builtins_uri.rs`)

`connect` `tls-layer` `tls-peer-cert` `send-datagram` `recv-datagram` `uri` `url` `urn` `quic-session` `quic-open-stream` `quic-open-datagram` `http2-session` `http3-session` `http-request` `icmp-ping`

### File Structure Reorganization

The builtins implementation is already well-split. The key change is co-locating each module's type env with its builtins:

| File | Status | Change |
|------|--------|--------|
| `src/builtins_core.rs` | **NEW** | Aggregates ALL existing split files (including `builtins_async.rs` and `builtins_io.rs`) into `core_builtins()` + `core_type_env()` (from type_env.rs) |
| `src/builtins_io.rs` | **IMPLEMENTATION ONLY** | Rust implementations stay; builtins are aggregated into `core_builtins()` by `builtins_core.rs`; type signatures go into `core_type_env()` |
| `src/builtins_async.rs` | **IMPLEMENTATION ONLY** | Same — aggregated into core |
| `src/builtins_datetime.rs` | **EXTEND** | Add `pub fn datetime_builtins() -> Vec<BuiltinDef>` + `pub fn datetime_type_env() -> TypeEnv` |
| `src/builtins_net.rs` | **NEW** | Net builtins extracted from `builtins_io.rs` + all of `builtins_uri.rs`; add `net_type_env()` |
| `src/builtins_uri.rs` | **ABSORBED** | Contents move into `builtins_net.rs`; file deleted |
| `src/builtins.rs` | **SLIMMED** | Retains only: `builtin_module()`, `type_env_module()`, `create_stdlib_env_inner()`, `create_type_stage_env()`, bootstrap logic; no builtin definitions |
| `src/type_env.rs` | **SLIMMED** | TypeEnv infrastructure remains; only `TypeEnv::with_builtins()` deleted (T-722). File not deleted. |
| `stdlib/async.llt` | **DELETED** | `loop-select`, `retry`, `finally`, `defer`, `with-resource` and all tinct-level async utilities move into `stdlib/prelude.llt` |

After this sprint, everything related to a module lives in one file: Rust implementation + type signatures together, no cross-file coordination with `type_env.rs`.

### TypeEnv Must Follow `--- uses:`

Without matching TypeEnv changes, a user program calling `builtin-write` would pass type checking but fail at runtime. `type_env_module()` is the parallel registry — the type checker must apply the same per-document scope logic as the runtime.

The type checker mirrors the runtime design. `typecheck_surface_program_with_env` (`src/typecheck.rs:235`) threads `result_env` across documents. Per-document `--- uses:` type sig injections go into a **doc-local TypeEnv child only** — seeded from `type_env_module()` for each declared module, used for inference, then discarded. Only the document's exported type bindings propagate forward via `result_env`.

The bootstrap parallel: the type checker receives the prelude TypeEnv (produced by typechecking prelude with a core-only starting TypeEnv) as the base for all subsequent documents. `TypeEnv::with_builtins()` is deleted; prelude's TypeEnv is the new baseline, produced the same way runtime prelude-dict is — by running prelude through the type checker with `type_env_module("core")` as the seed.

### Prelude's `--- uses:` Declaration

Prelude declares `--- uses: ["core"]` only. Async builtins (`task`, `await`, `send`, `recv`, etc.) are part of "core" and universally available. All I/O builtins are also in "core" and prelude re-exports the ones user programs commonly need: `open`, `emit`, `lines`, `slurp`, `env`, `list-dir`, `narrow`, `string-handle`, `write`, `flush`, `close`, `stat`, `exists`, `make-dir`, `rename`, `read-chunk`. `builtin-read-all` is used internally by prelude's include pipeline but **not re-exported** — user code uses lazy `lines` or chunked `read-chunk`.

All tinct-level async utilities from `stdlib/async.llt` move into prelude. `loop-select-impl` and `retry-impl` (private helpers) go into prelude's **first (private) dict**; `loop-select`, `retry`, `finally`, `await-all`, `recv-all`, `par-map`, `par-filter`, `exit`, `graceful-exit` (public API) go into prelude's **second (public) dict**. `stdlib/async.llt` is deleted. (`defer` and `with-resource` appear in the ffi.md whatif spec but were never implemented in the current codebase.)

**IO in core** — There is no separate "io" module. All I/O builtins are part of "core" and re-exported directly from prelude. B-168 renames all bare I/O names to `builtin-*`. `builtin-read-all` is a new primitive (`Handle → String`) used internally by the include pipeline — not re-exported (user code uses lazy `lines` or chunked `read-chunk`).

**Datetime** bare names (`now`, `parse-timestamp`, etc.) — B-168 renames them; `stdlib/datetime.llt` (declaring `--- uses: ["datetime"]`) wraps them. None belong in prelude.

**Net** bare names (`connect`, `tls-layer`, `uri`, etc.) — wrappers go in the net stdlib file (declaring `--- uses: ["net"]`). None belong in prelude.

## What Gets Deleted

### `src/builtins.rs`

| Deleted | Lines | Replacement |
|---------|-------|-------------|
| `standard_builtins()` | 1189–2121 (~932 lines) | Contents split into per-group functions under `builtin_module()` |
| `create_root_env()` | 2172–2185 | Deleted entirely — bootstrap starts from `Environment::new()` |
| `builtin_primary_names()` | 2144–2164 | Replaced by iterating `builtin_module()` groups |
| Test: `test_all_standard_builtins_registered()` | ~2688 | Replaced with per-group count tests |
| Test: `standard_builtins_count()` (asserts 238) | ~6399–6412 | Replaced |
| Test: `create_root_env_has_all_builtins()` | ~8123 | Replaced |

### `src/builtins.rs` — `create_stdlib_env_inner()`

The entire bootstrap env setup is replaced. There is no `bootstrap_env`, no `create_root_env()`, and no `CACHED_STDLIB_ENV` thread-local. Rust runs a two-step bootstrap: evaluate `loader.llt` to get `eval-programs`, then use it to load prelude:

```rust
// DELETED:
let bootstrap_env = create_root_env();
let stdlib_env = Environment::with_parent(Arc::clone(&bootstrap_env));

// REPLACED WITH (Phase 1–3 of bootstrap):

// Phase 1: build core dict
let core_dict = builtin_module("core").expect("core module must exist");

// Phase 2: evaluate loader.llt with core as scope
// loader.llt is an inline string constant — no filesystem access needed at bootstrap.
let loader_prog = parse_and_expand(LOADER_TINCT)?;
let loader_dict = eval_document(loader_prog.documents[0].expressions, scope: core_dict, ctx)?;
// loader_dict = { eval-program: <fn>, eval-programs: <fn> }

// Phase 3: use loader's eval-programs to load prelude
let prelude_prog = parse_and_expand(PRELUDE_TINCT)?;
let eval_programs = loader_dict.get("eval-programs").expect("loader must export eval-programs");
let prelude_dict = invoke_fn(eval_programs, [
    Value::Seq::singleton(Value::Program(Arc::new(prelude_prog))),
    Value::nil(),
], ctx)?;
// prelude_dict = { map, write, if, module, eval-program, eval-programs, ... }
// Held for process lifetime. Each program run seeds a fresh env frame from it.
```

**`LOADER_TINCT`** is an inline `&'static str` constant in `src/builtins.rs` (or a dedicated `src/loader.rs`). Inlining avoids filesystem access during bootstrap and makes the bootstrap self-contained.

`create_type_stage_env()` (builtins.rs:2420) gets the same treatment: replaced with the type-stage prelude TypeEnv, produced by running the `--- stage: type` section through the type checker with `type_env_module("core")` as seed.

### `src/expand.rs`

Currently uses `create_root_env()` for the macro expander's bootstrap env (the `depth > 0` fallback). Replaced with `prelude_dict` (the result of Phase 3). Macro bodies have full prelude access; no `CACHED_STDLIB_ENV` thread-local is needed.

### `src/type_env.rs`

| Deleted | Lines | Replacement |
|---------|-------|-------------|
| `TypeEnv::with_builtins()` | 1224–3776 (~2553 lines) | Split into per-group functions under `type_env_module()` |

### All call sites

All callers of `standard_builtins()` and `create_root_env()` are updated:

| File | Line | Change |
|------|------|--------|
| `src/builtins.rs:2342` | `create_stdlib_env_inner` | Remove `create_root_env()` call |
| `src/builtins.rs:2420` | `create_type_stage_env` | Remove `create_root_env()` call |
| `src/builtins.rs:2521` | test `test_ctx()` | Inject `builtin_module("core")` directly |
| `src/expand.rs` | expander bootstrap | Use `prelude_dict` from Phase 3 |

All callers of `TypeEnv::with_builtins()`:

| File | Change |
|------|--------|
| `src/typecheck.rs` | Use prelude TypeEnv as baseline; per-document `--- uses:` injection happens via the type checker's parallel of `eval-program` (a tinct-driven type check loop seeded from `type_env_module()` per document) |
| `src/imports.rs:928` | `%libdir` include path: replace `TypeEnv::with_builtins()` with prelude TypeEnv as baseline; `--- uses:` doc-local injection happens inside the type checker's document loop |
| `src/typecheck.rs:68` (`typecheck_surface_program_annotation_table`) | Receives prelude TypeEnv as baseline — callers unchanged; the baseline is now prelude TypeEnv (produced once at startup) rather than `TypeEnv::with_builtins()` |

All callers of `standard_builtins()` in LSP:

| File | Change |
|------|--------|
| `src/lsp/analysis.rs:3596` (`builtin_completions()`) | Iterate all `builtin_module()` groups instead of `standard_builtins()` |
| `src/lsp/analysis.rs:3624` (`prelude_completions()`) | Same — replaces `standard_builtins()` filter set |

## What Gets Added

### `builtin-module` + `eval scope:` — new Rust primitives

**`builtin-module name → Dict`** (`src/builtins_meta.rs`):

```rust
// Registered in "core" so loader.llt and prelude can call it.
// Returns Value::Dict mapping builtin name → Value::Builtin.
fn builtin_builtin_module(args: BuiltinArgs, ctx: &Arc<EvalContext>) -> EvalResult {
    let name = require_string(&args.args[0], ctx, "builtin-module")?;
    match builtin_module(&name) {
        Some(dict) => Ok(dict),
        None => Err(EvalError::user_error(
            format!("unknown native module: {:?}", name),
            args.call_span.clone(),
        )),
    }
}
```

**`eval scope: dict`** (extend existing `eval` in `src/builtins_meta.rs`):

```rust
// New optional named arg on builtin_eval.
// Seeds a fresh env frame with the scope dict before evaluating expressions.
if let Some(scope_thunk) = args.named("scope") {
    let scope = materialize(&scope_thunk, None, ctx)?;
    if let Value::Dict(entries) = scope {
        for (key, thunk_id) in entries {
            env_frame.insert(key.as_str().to_string(), thunk_id);
        }
    }
}
// ... then evaluate doc.expressions in env_frame as before ...
```

### `stdlib/loader.llt` — the bootstrap loader

`loader.llt` is the first tinct code evaluated. It defines `eval-program` (single program) and `eval-programs` (the real CLI entry point — a seq of programs). It is stored as an inline `&'static str` constant via `include_str!` rather than read from disk, making the bootstrap self-contained.

```tinct
--- uses: ["core"]
[
  # Evaluate a single program's documents, threading % through them.
  eval-program: [fn [let prog initial-input]
    [builtin-reduce
      [fn [let percent doc]
        [builtin-eval doc.expressions
          scope:   [builtin-reduce builtin-merge [] [builtin-map builtin-module doc.uses]]
          program: prog
          %:       percent]]
      initial-input
      prog.documents]]

  # Evaluate a pipeline of programs (a Seq of Value::Program), threading %
  # from one to the next. This is the real CLI entry point.
  # % flows: initial-input → prog1-result → prog2-result → ...
  # The return value of each program becomes % for the next, regardless of
  # how the program internally named its output dict entries.
  eval-programs: [fn [let programs initial-input]
    [builtin-reduce
      [fn [let percent prog]
        [eval-program prog percent]]
      initial-input
      programs]]
]
```

Core primitives used: `builtin-reduce`, `builtin-merge`, `builtin-map`, `builtin-module`, `builtin-eval` (all registered in "core"). No bare aliases — prelude is not loaded yet.

Rust evaluates loader.llt in Phase 2 with `scope: builtin_module("core")`. This is the only place Rust hardcodes a scope. loader.llt changes only if the loading mechanism itself changes.

### Source files (see §File Structure Reorganization for full details)

- `src/builtins_core.rs` (new) — `core_builtins()` (aggregates all: core + async + io) + `core_type_env()`
- `src/builtins_datetime.rs` (extend) — `datetime_builtins()` + `datetime_type_env()`
- `src/builtins_net.rs` (new) — `net_builtins()` + `net_type_env()`
- `src/builtins.rs` (slim) — `builtin_module()` returning `Value::Dict` + `type_env_module()` registries only

### `src/builtins_io.rs` — `builtin-read-all` (new)

New Rust builtin: `builtin-read-all handle → String`. Reads a readable Handle to EOF and returns the full contents as a `Value::String`. Replaces `[join "\n" [collect [lines [open ...]]]]` in the include pipeline (prelude.llt:2878, 2906) with `[builtin-read-all [open ...]]`.

**Not re-exported from prelude.** User code uses lazy `lines` (line-by-line) or `read-chunk` (chunked) instead. The restriction is by convention: `builtin-read-all` is in "core" and therefore in prelude's doc-local scope, but prelude's public dict does not include it.

**Error cases:** non-Handle argument → `EvalError::type_mismatch`; I/O failure → `EvalError::user_error`.

### `src/ast.rs` — `SurfaceDocument.uses` field

New field alongside existing `stage`, `name`, `items`, `output_type`, `expects`, `caps`:

```rust
pub struct SurfaceDocument {
    pub stage: Option<Stage>,
    pub name: Option<String>,
    pub items: Vec<SurfaceItem>,
    pub output_type: Option<Spanned<Annotation>>,
    pub expects: Option<Spanned<Annotation>>,
    pub caps: Option<Spanned<Vec<(String, Annotation)>>>,
    pub uses: Option<Spanned<Vec<Spanned<String>>>>,  // ← new; per-element spans for precise error messages
}
```

Per-element `Spanned<String>` (not bare `String`) allows the "unknown native module" error to point at the specific bad name within `["core" "typo" "net"]` rather than the whole bracket list.

### `src/parser.rs` — `--- uses:` header parsing

New case in the document header parser (after the `--- caps:` arm, which ends around line 3732), parsing `uses: ["sql" "net"]` as a list of string literals.

**Implementation notes for the parser sprint:**
- `parse_value()` is not available in the document header scanner — the header parser is a hand-rolled token loop, not the recursive expression parser. The uses list requires a bespoke inner loop: consume `Token::OpenBracket`, loop collecting `Token::QuotedString(s)` values into a `Vec<Spanned<String>>`, consume `Token::CloseBracket`. Any other token type inside the brackets is an immediate parse error.
- Must explicitly reject `Token::InterpolatedString`, `Token::TripleQuotedString`, and bare identifiers — only `Token::QuotedString` is valid.
- Add `let mut next_doc_uses: Option<Spanned<Vec<Spanned<String>>>> = None;` alongside the existing `next_doc_caps` (line 1211) and `next_doc_expects` (line 1210) declarations — the accumulator block is at `parser.rs:1204-1212`, not near the header dispatch loop.
- All three `SurfaceDocument { }` construction sites at `parser.rs:3469`, `4442`, `4466` must include `uses: next_doc_uses.take()`.

### `src/eval_materialize.rs` — `document.uses` field access

`eval-program` accesses `doc.uses` on a `Value::Document`. This field access must be added to the document field handler in `eval_materialize.rs` (alongside the existing `document.expressions` arm):

```rust
"uses" => {
    // Return doc.uses as [Seq String] (or [] if no --- uses: header).
    match &doc.uses {
        None => Value::Dict(IndexMap::new()),  // empty [] sentinel
        Some(spanned_uses) => {
            // Build Seq of String values from the module name list.
            let mut tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::Dict(IndexMap::new()),
                access_span.clone(),
            )));
            for name in spanned_uses.node.iter().rev().skip(1) {
                let head_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::String(name.node.clone().into()),
                    access_span.clone(),
                )));
                tail_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                    Value::Seq { head: head_id, tail: tail_id },
                    access_span.clone(),
                )));
            }
            let head_id = ctx.alloc_thunk(Arc::new(Thunk::new_materialized(
                Value::String(spanned_uses.node[0].node.clone().into()),
                access_span.clone(),
            )));
            Value::Seq { head: head_id, tail: tail_id }
        }
    }
}
```

When `--- uses:` is absent, `doc.uses` returns `[]` — `[map builtin-module []]` produces `[]`, `[reduce merge [] []]` returns `[]`, and `[eval exprs scope: []]` evaluates with no extra bindings. The mechanism works correctly for documents with no declared modules.

### `stdlib/prelude.llt` — `eval-program`, `eval-programs`, and `module`

Prelude re-exports `eval-program` and `eval-programs` from loader (same implementation, using `module` alias) and exports `module: builtin-module`:

```tinct
--- uses: ["core"]
# First (private) dict — helpers not exported
[
  # ... loop-select-impl, retry-impl, and other private helpers ...
]
# Second (public) dict — the prelude API
[
  # Loading primitives — re-exported from loader, using module alias
  module:        builtin-module

  eval-program:  [fn [let prog initial-input]
    [builtin-reduce
      [fn [let percent doc]
        [builtin-eval doc.expressions
          scope:   [builtin-reduce builtin-merge [] [builtin-map module doc.uses]]
          program: prog
          %:       percent]]
      initial-input
      prog.documents]]

  eval-programs: [fn [let programs initial-input]
    [builtin-reduce
      [fn [let percent prog]
        [eval-program prog percent]]
      initial-input
      programs]]

  # Core re-exports
  if:    builtin-if
  map:   builtin-map
  # ... etc ...
]
```

`eval-programs` is the real CLI entry point. `tinct run -i json u1.llt u2.llt -o json` builds `[json-in, u1, u2, json-out]` and calls `eval-programs` on that list. `-i` prepends from `stdlib/cli/in/`, `-o` appends from `stdlib/cli/out/`. All programs are equal — the return value of each becomes `%` for the next regardless of how it named its output.

No Rust code reads `doc.uses` for injection — that is `eval-program`'s job entirely.

### `stdlib/prelude.llt` — header and new exports

`--- uses: ["core"]` header added to the runtime prelude document (the second document, after `--- stage: type`). Prelude's `--- uses:` is handled by loader's `eval-program` in Phase 3 — it is machine-read and load-bearing, not just documentation.

See §`stdlib/prelude.llt — eval-program and module` above for the full implementation. The key exports added by this whatif: `module` (alias for `builtin-module`) and `eval-program` (multi-document loader loop).

### All stdlib files with Rust deps

`--- uses:` headers added to files that own or directly call Rust builtins:
- `stdlib/prelude.llt` — `--- uses: ["core"]` (second document; all builtins including I/O are in "core"; also includes macro transformer definitions formerly in `macros.llt` which call `builtin-variant` directly)
- `stdlib/datetime.llt` — `--- uses: ["datetime"]`
- `stdlib/sql.llt` — `--- uses: ["sql"]` (future sprint — `src/builtins_sql.rs` does not yet exist)
- `stdlib/regex.llt` — `--- uses: ["regex"]` (future sprint — `stdlib/regex.llt` is currently pure-tinct with no Rust builtins; header added when Rust regex builtins land)
- `stdlib/crypto.llt` — `--- uses: ["crypto"]` (future sprint — `src/builtins_crypto.rs` does not yet exist)
- `stdlib/async.llt` — **DELETED** (merged into prelude)

Pure tinct stdlib files (path.llt, codecs/*.llt, protocols/*.llt, strings.llt, etc.) need no `--- uses:` — they are implemented entirely in tinct using prelude-exported names.

### `tests/corpus/eval/stdlib/async_stdlib_basic.llt-eval`

**Current:** line 11 contains `[include %libdir "async.llt"]`.

**Proposed:** remove the include — all async functions (`loop-select`, `retry`, `finally`, etc.) are available from prelude after this sprint. This change must land **before** `stdlib/async.llt` is deleted so the test suite stays green throughout.

## Prerequisites

**B-168** — rename all bare-named builtins to `builtin-*` and add prelude wrappers. Required before this sprint. Includes:
- I/O bare names: `write` → `builtin-write`, `write-atomic` → `builtin-write-atomic`, `write-handle` → `builtin-write-handle`, `read-link` → `builtin-read-link`, and ~46 others
- Builder ops: `make-builder` → `builtin-make-builder`, `builder-set` → `builtin-builder-set`, `builder-delete` → `builtin-builder-delete`, `builder-finish` → `builtin-builder-finish`, `builder-snapshot` → `builtin-builder-snapshot`, `builder-has?` → `builtin-builder-has?`, `builder-get` → `builtin-builder-get`, `builder-get-or` → `builtin-builder-get-or`

After `standard_builtins()` is deleted, any bare name not in a module and not wrapped in prelude becomes inaccessible to user code. B-168 must be complete and all prelude references to old bare names updated before this sprint lands.

## References

- `doc/whatif/ffi.md` Option 2 ("In-Tree Native Modules") — full `--- uses:` specification including cross-document scoping, `builtin_module()` registry structure, and TypeEnv split. This whatif is the implementation; ffi.md is the design reference.
- `doc/whatif/completed/include-decomposition.md` — the sprint that deleted `%rust` and made `include` a prelude function, requiring the `--- uses:` approach.
