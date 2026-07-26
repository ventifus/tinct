# Loader Design and Environment Chain

## Overview

The tinct runtime bootstraps through an accumulated environment dict.
`stdlib/loader.llt` is the first tinct code evaluated at startup — the
"main function" that loads prelude, constructs the type context, and
runs the user pipeline. `stdlib/test-loader.llt` is an alternative init
program that runs corpus tests instead of the user pipeline; it mirrors
the same chain exactly.

---

## Bootstrapping Sequence

Startup proceeds in three passes through the init program (loader.llt
or test-loader.llt). Each init program is a single document with three
dict bodies:

### Dict 1 — Private helpers (core builtins only)

The first dict body defines private helpers using only Rust builtins.
No tinct standard library is available here. Helpers defined:

- `bytes-empty?`, `read-file` — file I/O utilities
- `reduce`, `join` — collection primitives
- `make-entry`, `null?`, `get-or` — dict utilities
- `merge` — right-biased dict union
- `env-to-name-set` — build a `Dict<String,1>` name-set from an env dict
  for passing to `builtin-resolve` (replaces the old `scope-to-frames`
  walk — env is already a flat dict of all visible bindings)

### Dict 2 — Core types and pipeline infrastructure

The second dict body sees Dict 1 and the initial environment (CLI-
injected capabilities). It defines:

- `Boolean: [type True False]` — declared here so constructors are
  available throughout the init program before prelude loads
- `DocName`, `ProgramItem` — ADTs for pipeline metadata
- `uses-scope` — loads and type-checks `builtin_NAME.llt` declaration
  files for `--- uses:` module headers; uses `env-to-name-set` to build
  the name-set and `builtin-resolve` + `builtin-typecheck-doc` per doc
- `fundamental-tc` — a TypeContext seeded from `builtin_core.llt` type
  declarations (DirCap, Int, Bytes, etc.); no implicit Rust back-channel
- `include` — the bootstrap file loader (no prelude awareness):
  parse → desugar → resolve → typecheck → eval → return `{env: Dict}`.
  Used only for loading prelude itself.
- `eval-document-runtime` — per-document evaluation with full env-dict
  construction (see Environment Chain below)
- `eval-pipeline-item`, `eval-file`, `eval-expr` — pipeline dispatch
- `cli-pipeline` — the top-level pipeline reducer

### Dict 3 — Prelude, channels, formatter

The third dict body sees Dicts 1 and 2. It:

1. Calls `include %libdir "prelude.llt"` with
   `[%prelude: prelude-env  Boolean: Boolean  True: True  False: False]`
   as the `extras` dict — giving prelude access to its own accumulated
   env via `%prelude` (for mutually-recursive includes) and
   Boolean/True/False before prelude declares them.
2. Extracts `prelude-env` (a Dict) from the result: `prelude-result.env`.
3. Uses `fundamental-tc` directly as the runtime TypeContext — it is already
   enriched by the `include` call: the type-stage pass wires prelude's type-stage
   env via `builtin-tc-update-type-stage-env`, and `builtin-typecheck-doc` calls
   populate the inference_env with prelude's type schemes.
4. Allocates the emit channel and formatter.

### Final Expression

The final (non-dict) expression dispatches to `cli-pipeline` or raises
if no input was given.

---

## Formal Environment Chain

Five named levels in order from lowest (outermost) to highest (innermost,
shadows all below):

```
[builtins]          Rust-injected Value::Builtin entries
    ↑
[cli-injected caps] %cwd, %libdir, %args, %programs, user --cap flags
    ↑
[loader layer]      Dict 1 + Dict 2 + Dict 3 bindings from the init program
                    (Boolean, ProgramItem, include, fundamental-tc, ...)
    ↑
[prelude]           stdlib/prelude.llt — standard library bindings
    ↑
[per-document]      % (pipeline input), %include-dir, emit, %emit-channel,
                    %cwd/%libdir/%stdout/%stderr/%args (from closure),
                    include (ctx-include), named sections (--- %name:),
                    module builtins (--- uses: headers)
```

Each level is a flat Dict merged into the accumulated `env` dict. Higher
levels shadow lower ones via right-biased `merge`.

### Why Closures Handle %cwd, %libdir, etc.

`eval-document-runtime` does NOT pass `%cwd` and `%libdir` as
parameters to each document. Instead, it references them from the
loader's lexical scope (Dict 2/3 closure). This means:

- `%cwd` is always the CLI working directory — fixed for the session.
- `%libdir` is always the stdlib DirCap — fixed for the session.
- `%include-dir` IS threaded as a parameter because it changes per-file
  (derived by narrowing `%cwd` to the parent directory of each file's path
  via `builtin-path-dirname`).

### The Per-Document Env Dict

`eval-document-runtime` builds the env dict via flat Dict merges:

```tinct
[base-env:  [merge prelude-env state.named]]
[caps-env:  [merge base-env
              [%:            state.percent
               %include-dir:  include-dir
               emit:          emit-fn
               %emit-channel: emit-ch
               %cwd:          %cwd
               %libdir:       %libdir
               %stdout:       %stdout
               %stderr:       %stderr
               %args:         %args
               include:       ctx-include]]]
[full-env:  [merge caps-env mod-scope]]
```

Resolution and evaluation both use `full-env` so the de Bruijn
coordinates computed by `builtin-resolve` match the slots visible during
evaluation. `builtin-resolve` receives the name-set:

```tinct
[name-set:     [env-to-name-set full-env]]
[doc-resolved: [builtin-resolve doc name-set]]
```

`builtin-eval` takes the lowered CoreDocument and the full env dict,
and returns the exports Dict directly (raises on error):

```tinct
[lowered: [builtin-lower typed.doc]]
[evaled:  [builtin-eval lowered full-env]]
```

---

## `include` vs `ctx-include`

There are two `include` functions with different prelude awareness:

### Loader-level `include` (Dict 2)

Defined in Dict 2 of the init program. Has NO prelude in its closure —
it was defined before prelude loaded. Used only for bootstrapping:
loading prelude.llt itself. When called:

- Uses an empty seed env plus the `extras` dict (e.g., `[%prelude:
  prelude-env  Boolean: Boolean  True: True  False: False]` for the
  prelude case).
- Returns `{env: Dict}` — the accumulated env from all documents.

### `ctx-include` (per-document, inside `eval-document-runtime`)

A closure defined inside `eval-document-runtime`. Captures
`prelude-env` from its outer scope. When user code calls
`[include cap path]`, they get this closure. It calls the loader
`include` with `[%prelude: prelude-env]` in extras so the included
file has prelude in scope. Returns `.env` — the accumulated env dict.

This is how include is defined ONCE in the loader (in
`eval-document-runtime`) and never needs to be redefined: the
prelude-awareness is baked into the closure at the point
`eval-document-runtime` is called for each document. Prelude does NOT
define `include`.

---

## How test-loader Mirrors the CLI Chain

`stdlib/test-loader.llt` runs corpus tests instead of the user pipeline.
It replicates the same three-level init structure with the same env-dict
protocol.

### Same structure

- Dict 1: private helpers (identical `env-to-name-set`, `merge`, etc.)
- Test-specific Dict 1: adds `make-string-handle`, `emit-diagnostics`,
  `count-errors`, `uses-scope`, `if-ok`, and pipeline stage functions
  (`load-bytes`, `parse-source`, `eval-program`, `typecheck-program`,
  `export`)
- Test-specific Dict 2: test-specific functions (`eval-docs`,
  `typecheck-docs`, `run-test-pipeline`, `run-test-file`, `run-test`,
  `find-corpus-files`)
- Dict 3: loads prelude, builds TypeContext, runs all tests

### Key difference: no `ctx-include` injection

test-loader does not inject `include` into the per-document env. Test
programs run with `include` accessible only if prelude defines it —
but prelude does NOT define `include`. Tests needing `[include ...]`
must be self-contained or use `--- pragma: ["no-prelude"]`.

The test-loader's `eval-docs` passes `base-env` (prelude env + caps)
as the seed env; user code in tests does not have access to an
`include` that carries prelude awareness. This is by design: corpus
tests exercise isolated features.

### Per-test prelude injection

`run-test-pipeline` passes `prelude-env` and `prelude-tc` (from the
pre-loaded `_prelude-result`) as explicit parameters rather than via
closure. This allows tests to opt out of prelude via `pragma:
["no-prelude"]` by substituting an empty env and `fundamental-tc`
instead.

---

## What Each Chain Level Contributes

| Level | Contributes | Source |
|---|---|---|
| builtins | `builtin-*` functions, `Value::Builtin` entries | Rust (eval_core.rs) |
| cli-injected caps | `%cwd`, `%libdir`, `%args`, `%programs`, user `--cap` flags | main.rs / lib.rs |
| loader layer | `Boolean`, `ProgramItem`, `include`, `fundamental-tc`, `uses-scope`, etc. | loader.llt Dict 1+2+3 |
| prelude | `map`, `filter`, `reduce`, `=`, `+`, `<`, `if`, `True`, `False`, `Boolean`, type class instances, ... | prelude.llt |
| per-document | `%` (pipeline value), `%include-dir`, `emit`, named sections, module builtins | eval-document-runtime |

---

## TypeContext Threading

The TypeContext (`tc`) carries type schemes and instance declarations
for the type checker. It flows explicitly:

1. `fundamental-tc` is built once from `builtin_core.llt` — contains
   `DirCap`, `Int`, `Bytes`, etc.
2. `fundamental-tc` is used directly as the runtime TypeContext — the `include`
   call that loads prelude already enriches it: the type-stage pass calls
   `builtin-tc-update-type-stage-env` (which prepends a new scope frame to
   `TypeContextData.type_stage_scope`), and `builtin-typecheck-doc` calls
   populate `inference_env` with type schemes.
3. Per-document: `builtin-typecheck-doc` receives `tc` and the resolved
   document. It updates `tc`'s internal `inference_env` (merging the
   document's new type schemes) so subsequent documents see them.
4. Type-stage documents (`--- stage: "type"`) are evaluated first; their
   env is wired into `tc` via `builtin-tc-update-type-stage-env` before
   runtime-stage documents are type-checked. This lets `@Integer`,
   `@Float`, etc. annotations resolve correctly in the runtime stage.

---

## Axiom: Prelude Speaks the Rust Protocol

Rust never embeds prelude-specific behavior. The `include` function is
NOT a Rust builtin — it is defined entirely in the loader (`eval-
document-runtime`) as a tinct closure. Prelude works because it is
correct tinct; the loader provides the infrastructure that makes prelude
self-loading.

The only Rust primitives used by the loader are the irreducible
builtins listed in the loader's header comment:
`builtin-file-read`, `builtin-parse`, `builtin-resolve`,
`builtin-typecheck-doc`, `builtin-lower`, `builtin-eval`,
`builtin-tc-update-type-stage-env`, `builtin-make-type-ctx`, `builtin-channel`,
`builtin-send`, `builtin-str`, `builtin-dict-get`, `builtin-keys`,
`builtin-dict-length`, `builtin-build-dict`, `builtin-tag-of`,
`builtin-path-dirname`, and `builtin-string-concat`.

`builtin-resolve` takes a `Dict<String,1>` name-set (not scope frames).
`builtin-eval` takes `(CoreDocument, env Dict)` and returns the exports
Dict directly (raises on error — no `{result, scope-id, errors}`
wrapper). There are no `builtin-scope-*` or `builtin-arena-*` builtins.
