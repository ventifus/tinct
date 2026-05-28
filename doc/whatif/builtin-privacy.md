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

`--- uses:` works the same way. The evaluator injects the named Rust builtins into the **document's local evaluation scope** before evaluating any expressions — like the first dict above. The document's exported dict contains only what it explicitly names. The `builtin-*` names used internally never appear in the exported dict and are therefore unreachable by user code.

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

`--- uses:` is a document header key — processed before any tinct expression in the document is evaluated. The evaluator calls `builtin_module(name)` for each declared name and injects the resulting `BuiltinDef` entries into the **document-local scope** (not the exported dict). The document exports only what it explicitly names.

**Doc-local scoping:** injections are scoped to the declaring document. They do not propagate forward through the pipeline — only explicitly exported tinct bindings propagate. Closures carry their captured builtins as usual.

**Unknown module names** produce an immediate error: `unknown native module: "typo"`.

### Bootstrap Sequence (After This Whatif)

```text
empty Environment (nothing pre-injected)
  ↓ prelude.llt evaluated:
      --- uses: ["core"]     → inject core_builtins() into doc-local scope
                       # "core" includes async builtins and the universal I/O primitives
                       # prelude needs (open, lines, emit, env, narrow, list-dir, etc.)
      [
        map:   [fn ...]
        write: builtin-write
        if:    builtin-if
        task:  builtin-task
        send:  builtin-send
        ...                        → only these names leave this document
      ]
  ↓
user env (inherits only prelude's exported dict)
```

`builtin-map`, `builtin-write`, `builtin-task`, `builtin-send` etc. were in prelude's local scope during evaluation but are not in prelude's exported dict (unless prelude explicitly re-exports them — as it does for `map`, `write`, `task`, `send`). User code cannot reach the raw `builtin-*` names.

### Builtin Module Registry

`standard_builtins()` is deleted. Builtins are organized into named groups. Each group is co-located with its type env in a single source file (see §File Structure):

```rust
// src/builtins.rs — only the registry; no builtin definitions
pub fn builtin_module(name: &str) -> Option<Vec<BuiltinDef>> {
    match name {
        "core"     => Some(builtins_core::core_builtins()),  // includes async + io builtins
        "datetime" => Some(builtins_datetime::datetime_builtins()),
        "net"      => Some(builtins_net::net_builtins()),
        _          => None,
    }
}

pub fn type_env_module(name: &str) -> Option<TypeEnv> {
    match name {
        "core"     => Some(builtins_core::core_type_env()),  // includes async + io type env
        "datetime" => Some(builtins_datetime::datetime_type_env()),
        "net"      => Some(builtins_net::net_type_env()),
        _          => None,
    }
}
```

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
| `src/type_env.rs` | **DELETED** | All content distributed into per-module files |
| `stdlib/async.llt` | **DELETED** | `loop-select`, `retry`, `finally`, `defer`, `with-resource` and all tinct-level async utilities move into `stdlib/prelude.llt` |

After this sprint, everything related to a module lives in one file: Rust implementation + type signatures together, no cross-file coordination with `type_env.rs`.

### TypeEnv Must Follow `--- uses:`

Without matching TypeEnv changes, a user program calling `builtin-write` would pass type checking but fail at runtime. `type_env_module()` is the parallel registry — the type checker injects the declared modules' type signatures per-document at the same phase as runtime injection. This must be done in the same sprint as the runtime changes.

**Cross-document isolation:** `typecheck_surface_program_with_env` (`src/typecheck.rs:235`) threads `env = new_env` across documents — the type env accumulated by document N becomes document N+1's starting env. `--- uses:` type sig injections must go into a **doc-local env only**, not into `result_env`. The implementation must create a per-document child TypeEnv seeded with `type_env_module()` results for that document's declared modules, use it for inference, then discard it — only the document's exported type bindings propagate forward via `result_env` as they do today. This mirrors the runtime side: `doc_env` is a fresh child per document, discarded after evaluation; only the result thunk propagates.

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

The entire bootstrap env setup is replaced:

```rust
// DELETED:
let bootstrap_env = create_root_env();
let stdlib_env = Environment::with_parent(Arc::clone(&bootstrap_env));

// REPLACED WITH:
let stdlib_env = Arc::new(RwLock::new(Environment::new()));
// prelude's --- uses: ["core"] injects core_builtins() at eval time
```

`create_type_stage_env()` (builtins.rs:2420) gets the same treatment — it currently calls `create_root_env()` to bootstrap the type-stage evaluator; replaced with empty env. The type-stage prelude document (the `--- stage: type` section, currently first in `stdlib/prelude.llt`) adds `--- uses: ["core"]` — core contains `if`, `=`, and string comparison which is all the type-stage prelude needs.

### `src/expand.rs`

Currently uses `create_root_env()` for the macro expander's bootstrap env (the `depth > 0` fallback at line 485). Replaced with the cached `stdlib_env` from the `depth == 0` pass, so macro bodies always have full prelude access.

The depth>0 arm becomes:

```rust
} else {
    let env = CACHED_STDLIB_ENV.with(|c| c.borrow().clone())
        .unwrap_or_else(|| unreachable!(
            // INVARIANT: depth>0 is only reachable after depth==0 has run and
            // set the cache. This holds because load_stdlib_module does not call
            // expand_surface_program. If that changes, this must be revisited.
            "depth>0 reached before cache populated"
        ));
    let ctx = EvalContext::new_empty(base_dir, Arc::clone(&env), no_fs);
    (env, ctx)
};
```

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
| `src/expand.rs` | expander bootstrap | Use cached stdlib_env |

All callers of `TypeEnv::with_builtins()`:

| File | Change |
|------|--------|
| `src/typecheck.rs` | Use `type_env_module()` per document (doc-local only — see §TypeEnv Must Follow) |
| `src/imports.rs:928` | `%libdir` include path: replace `TypeEnv::with_builtins()` with `type_env_module()` seeded from the included file's `--- uses:` declaration |
| `src/typecheck.rs:68` (`typecheck_surface_program_annotation_table`) | Called from `builtins_meta.rs:1606`, `formatter.rs:97`, and 7+ sites in `main.rs`. The call sites do NOT change — the `build_prelude_env()` baseline remains correct. The `type_env_module()` injection happens inside `typecheck_surface_document` reading `doc.uses`, so all callers benefit automatically |

All callers of `standard_builtins()` in LSP:

| File | Change |
|------|--------|
| `src/lsp/analysis.rs:3596` (`builtin_completions()`) | Iterate all `builtin_module()` groups instead of `standard_builtins()` |
| `src/lsp/analysis.rs:3624` (`prelude_completions()`) | Same — replaces `standard_builtins()` filter set |

## What Gets Added

### Source files (see §File Structure Reorganization for full details)

- `src/builtins_core.rs` (new) — `core_builtins()` (aggregates all: core + async + io) + `core_type_env()`
- `src/builtins_datetime.rs` (extend) — `datetime_builtins()` + `datetime_type_env()`
- `src/builtins_net.rs` (new) — `net_builtins()` + `net_type_env()`
- `src/builtins.rs` (slim) — `builtin_module()` + `type_env_module()` registries only

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

### `src/eval_pipeline.rs` — `--- uses:` injection

In `eval_surface_document`, before expression evaluation (parallel to `--- caps:` validation at line 197):

```rust
if let Some(ref uses) = doc.node.uses {
    let mut env_write = env.write().unwrap();
    for module_name in &uses.node {
        match builtin_module(module_name) {
            Some(defs) => {
                for def in defs {
                    let name = def.name.to_string();  // capture before move
                    let thunk = Arc::new(Thunk::new_materialized(
                        Value::Builtin(def), uses.span.clone()
                    ));
                    env_write.insert(name, thunk);
                }
            }
            None => return Err(EvalError::user_error(
                format!("unknown native module: {:?}", module_name),
                module_name.span.clone(),  // per-element span, not whole-list span
            ).into()),
        }
    }
}
```

### `stdlib/prelude.llt`

`--- uses: ["core"]` header added at the top of the runtime prelude document (the second document, after `--- stage: type`). Async builtins are part of "core" — no separate "async" module declaration needed.

### All stdlib files with Rust deps

`--- uses:` headers added to files that own or directly call Rust builtins:
- `stdlib/prelude.llt` — `--- uses: ["core"]` (second document; all builtins including I/O are in "core")
- `stdlib/macros.llt` — `--- uses: ["core"]` (calls `builtin-variant` by raw name directly; `tag-of` and `gensym` are accessed via prelude-exported wrappers and don't require `--- uses:`, but `--- uses: ["core"]` ensures `builtin-variant` is in scope)
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
