# Loader Design and Environment Chain

## Overview

The tinct runtime bootstraps through a layered environment chain.
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
- `scope-to-frames`, `_scope-to-frames-inner` — scope chain → resolver
  frames conversion (walks `builtin-scope-parent` chain)
- `object-map` — dict value transformation

### Dict 2 — Core types and pipeline infrastructure

The second dict body sees Dict 1 and the initial environment (CLI-
injected capabilities). It defines:

- `Boolean: [type True False]` — declared here so constructors are
  available throughout the init program before prelude loads
- `DocName`, `ProgramItem` — ADTs for pipeline metadata
- `uses-scope` — loads and type-checks `builtin_NAME.llt` declaration
  files for `--- uses:` module headers
- `fundamental-tc` — a TypeContext seeded from `builtin_core.llt` type
  declarations (DirCap, Int, Bytes, etc.); no implicit Rust back-channel
- `include` — the bootstrap file loader (no prelude awareness):
  parse → desugar → scope-new → resolve → typecheck → eval → thread
  scope-id. Used only for loading prelude itself.
- `eval-document-runtime` — per-document evaluation with full scope
  construction (see Environment Chain below)
- `eval-pipeline-item`, `eval-file`, `eval-expr` — pipeline dispatch
- `cli-pipeline` — the top-level pipeline reducer

### Dict 3 — Prelude, channels, formatter

The third dict body sees Dicts 1 and 2. It:

1. Calls `include %libdir "prelude.llt"` with `[%prelude:
   prelude-result.scope-id Boolean: Boolean True: True False: False]`
   in the `extras` dict — giving prelude access to its own scope via
   `%prelude` (for mutually-recursive includes) and Boolean/True/False
   before prelude declares them.
2. Extracts `prelude-scope-id` from the result.
3. Creates the runtime TypeContext: `[builtin-tc-with-scope
   fundamental-tc prelude-scope-id]` — wires the prelude scope into the
   type checker so prelude's type schemes are available.
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

Each level is a `builtin-scope-new` layer. Higher layers shadow lower
ones by name. The `builtin-scope-parent` chain is the canonical record
of this structure at runtime.

### Why Closures Handle %cwd, %libdir, etc.

`eval-document-runtime` does NOT pass `%cwd` and `%libdir` as
parameters to each document. Instead, it captures them from the
loader's lexical scope (Dict 2/3 closure). This means:

- `%cwd` is always the CLI working directory — fixed for the session.
- `%libdir` is always the stdlib DirCap — fixed for the session.
- `%include-dir` IS threaded as a parameter because it changes per-file
  (set to the containing directory of each file being processed by
  `builtin-path-dir`).

### The Per-Document Scope

`eval-document-runtime` builds the scope in three `builtin-scope-new`
calls:

```
full-scope    = builtin-scope-new prelude-scope-id {
                  %: state.percent,       # pipeline input from previous doc
                  %include-dir: ...,
                  emit: emit-fn,
                  %emit-channel: emit-ch,
                  %cwd: %cwd,             # from closure
                  %libdir: %libdir,       # from closure
                  %stdout: %stdout,
                  %stderr: %stderr,
                  %args: %args,
                  include: ctx-include,   # prelude-aware include
                }
full-scope-r2 = builtin-scope-new full-scope state.named    # named sections
full-scope-r3 = builtin-scope-new full-scope-r2 mod-scope   # module builtins
```

Resolution and evaluation both use `full-scope-r3` so the de Bruijn
coordinates computed by `builtin-resolve` match the slots visible during
evaluation.

---

## `include` vs `ctx-include`

There are two `include` functions with different prelude awareness:

### Loader-level `include` (Dict 2)

Defined in Dict 2 of the init program. Has NO prelude in its closure —
it was defined before prelude loaded. Used only for bootstrapping:
loading prelude.llt itself. When called:

- Uses `root-scope-id` as the seed scope.
- Accepts an `extras` dict: `[%prelude: prelude-result.scope-id
  Boolean: Boolean True: True False: False]` for the prelude case.
- Returns `{scope-id: Int}`.

### `ctx-include` (per-document, inside `eval-document-runtime`)

A closure defined inside `eval-document-runtime`. Captures
`prelude-scope-id` from its outer scope. When user code calls
`[include cap path]`, they get this closure. It calls the loader
`include` with `[%prelude: prelude-scope-id]` in extras so the included
file has prelude in scope. Returns the scope-id (`.scope-id` field).

This is how include is defined ONCE in the loader (in
`eval-document-runtime`) and never needs to be redefined: the
prelude-awareness is baked into the closure at the point
`eval-document-runtime` is called for each document. Prelude does NOT
define `include`.

---

## How test-loader Mirrors the CLI Chain

`stdlib/test-loader.llt` runs corpus tests instead of the user pipeline.
It replicates the same three-level init structure:

### Same structure

- Dict 1: private helpers (identical `scope-to-frames`, `merge`, etc.)
- Dict 2 (test-loader's "Dict 1 private"): adds `make-string-handle`,
  `emit-diagnostics`, `count-errors`, `uses-scope`, `if-ok`, and
  pipeline stage functions (`load-bytes`, `parse-source`, `eval-program`,
  `typecheck-program`, `export`)
- Dict 2 (test-loader's "Dict 2 runner"): test-specific functions
  (`eval-docs`, `typecheck-docs`, `run-test-pipeline`, `run-test-file`,
  `run-test`, `find-corpus-files`)
- Dict 3: loads prelude, builds TypeContext, runs all tests

### Key difference: no `ctx-include` injection

test-loader does not inject `include` into the per-document scope.
Test programs run with `include` accessible only if prelude defines it —
but prelude does NOT define `include` (see above). Tests needing
`[include ...]` must use `--- pragma: ["no-prelude"]` and manage their
own scope, or the test program must be self-contained.

The test-loader's `eval-docs` passes `base-scope-id` (prelude scope +
caps) as the base; user code in tests does not have access to an
`include` that carries prelude awareness. This is by design: corpus
tests exercise isolated features.

### per-test prelude injection

`run-test-pipeline` passes `prelude-scope-id` and `prelude-tc` (from
the pre-loaded `_prelude-result`) as explicit parameters rather than
via closure. This allows tests to opt out of prelude via `pragma:
["no-prelude"]` by substituting `root-scope-id` and `fundamental-tc`
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
2. `[builtin-tc-with-scope fundamental-tc prelude-scope-id]` wires the
   prelude scope into the TypeContext so prelude's type schemes are
   visible during type-checking of user documents.
3. Per-document: `builtin-typecheck-doc` receives `tc` and the resolved
   document. It updates `tc`'s internal `inference_env` (merging the
   document's new type schemes) so subsequent documents see them.
4. Type-stage documents (`--- stage: "type"`) are evaluated first; their
   scope is wired into `tc` via `builtin-tc-with-scope` before
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
`builtin-typecheck-doc`, `builtin-eval`, `builtin-scope-new`,
`builtin-scope-frame`, `builtin-scope-parent`, `builtin-module`,
`builtin-tc-with-scope`, `builtin-make-type-ctx`, `builtin-channel`,
`builtin-send`, `builtin-str`, `builtin-get`, `builtin-keys`,
`builtin-dict-length`, `builtin-build-dict`, `builtin-tag-of`,
`builtin-path-dir`, and `builtin-string-concat`.
