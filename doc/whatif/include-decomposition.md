# What If: A Self-Hosted Include Pipeline

**State:** Accepted — 2026-05-18

**Refines:** [`completed/builtin-privacy.md`](completed/builtin-privacy.md) — replaces the `%rust` virtual-module design with a flat `Value::Dict`; the bootstrap env isolation principle is unchanged

What would it take to make `include`, `eval-file`, and the document pipeline self-hosted — defined in tinct, built from a minimal set of Rust primitives?

## Current State

`include` is a monolithic Rust builtin performing four opaque steps: parse, macro-expand, evaluate all documents threading `%`, return the last value. There is no way to access any intermediate product — no file AST, no expanded AST, no control over the document threading. This prevents formatter testing, structural docgen, linting, and macro inspection from being expressed in tinct.

The companion builtins `eval` (deep-materialize all thunks) and `force` (materialize to WHNF) are accurately implemented but poorly named.

## The Proposal

Delete `builtin_include` entirely and replace it with eight thin Rust primitives. Express `include`, `eval-file`, and the full document pipeline in tinct. The Rust floor does exactly one thing each; all orchestration is tinct. No transition period, no backwards compatibility.

## Rust Primitives

### The Eight New Primitives

```tinct
# Parse source text to a file AST dict. No IO, no evaluation.
# name: is an opaque provenance hint for error spans (file path, "REPL", etc.)
load@[Fn [source@String name: @String] Dict]

# Run macro expansion on a file AST dict.
expand@[Fn [ast@Dict] Dict]

# Evaluate AST expression nodes in the runtime stage env (prelude env + % + env:).
# %: is the pipeline input for the stage; env: merges extra bindings (scope promotion).
# All caps (%pwd, %libdir, CLI caps) are already in the prelude env — no injection needed.
eval@[Fn [exprs@Dict  %: @Any  env: @Dict] Any]

# Evaluate AST expression nodes in type_stage_env (type-level builtins only, no %).
# Type-stage documents are definitions, not pipeline stages — no % input.
eval-types@[Fn [exprs@Dict] Any]

# Compute blake3 hash of a string — used to key the include cache.
blake3@[Fn [source@String] String]

# Return a stable identity string for a DirCap based on (st_dev, st_ino)
# from fstat on the underlying O_DIRECTORY file descriptor. Format: "dev:ino".
# Stable across renames/moves. Works correctly in mount namespaces (no path
# resolution — the fd identity is used directly). Used in the include cache key.
cap-identity@[Fn [cap@DirCap] String]

# Three-state cache entry type.
IncludeCacheEntry: [type [Missing] [Pending] [Cached Any]]

# Look up an entry in the content-addressed include cache by hash.
include-cache-get@[Fn [hash@String] IncludeCacheEntry]

# Update an entry in the include cache.
include-cache-put@[Fn [hash@String  entry@IncludeCacheEntry] []]
```

### Two Renames (Both Kept)

- **`eval`** (deep-force all thunks) → **`deep-materialize`** — frees the `eval` name; matches the Rust `deep_materialize` function.
- **`force`** (WHNF) → **`materialize`** — the common case; the shorter name.

### `%rust` — The Flat Primitive Dict

`%rust` is a plain `Value::Dict` injected into `bootstrap_env` at startup. Only prelude (evaluated in the bootstrap context) can access it. Security comes from env isolation (`builtin-privacy-complete`), not from the type. The dict contains all Rust primitives as a flat namespace — arithmetic, string, collection, IO, network, datetime, meta, and the eight new primitives listed above. Prelude scope-promotes them all with one expression, then re-exports selectively.

## Tinct Implementation

### Prelude Bootstrap

```tinct
# prelude.llt — first expression scope-promotes ALL Rust primitives
%rust

# Public API — selective re-exports and tinct-level wrappers
[

# Arithmetic wrappers (validate types, provide error messages)
+: [fn@[return: Number  doc: """Add two numbers."""] [let a@Number b@Number]
  [+ a b]]

# ... (all prelude definitions follow using the scope-promoted Rust primitives)

# The pipeline functions — defined here so they're available to all tinct code

# Helper: evaluate one runtime-stage document and advance the pipeline state.
# Extracted because match arm bodies must be single expressions.
# include-dir is passed explicitly (not via state.named) so that a section header
# named --- %include-dir cannot accidentally overwrite the loading DirCap.
eval-document-runtime: [fn@[return: Dict  doc: """
  Evaluate one [Runtime] stage document and return the updated pipeline state.
"""] [let state doc include-dir]
  [result: [eval
    doc.expressions
    %:   state.prev
    env: [merge
           [if [dict? state.prev] state.prev []]
           state.named
           ["%include-dir": include-dir]]]]   # always wins — not in state.named
  [prev:  result
   named: [if [str? doc.name]
            [merge state.named [[str "%" doc.name]: result]]
            state.named]]]

eval-document-pipeline: [fn@[return: Any  doc: """
  Evaluate a file's documents, threading % and named sections through each.
  include-dir (the DirCap that loaded this file) is injected as %include-dir
  into every document's scope via the env: merge — NOT via state.named, so
  it cannot be overwritten by a --- %include-dir@Type section header.
  Scope chain promotion: if prev is a dict, its string-keyed entries are visible
  in the next doc. Named sections (%foo from --- %foo@Type headers) accumulate.
  Type stage documents are skipped.
"""] [let initial docs include-dir]
  [get "prev"
    [reduce
      [fn@[return: Dict] [let state doc]
        [match doc.stage
          Type:    state
          Runtime: [eval-document-runtime state doc include-dir]]]
      [prev: initial  named: []]
      docs]]]

eval-file: [fn@[return: Any  doc: """
  Evaluate a parsed file AST dict.
  initial: the pipeline input (% for the first document).
  include-dir: the DirCap used to load this file; injected as %include-dir
  so the file can [include %include-dir "sibling.llt"].
"""] [let ast@Dict initial include-dir]
  [eval-document-pipeline initial ast.documents include-dir]]

# Helpers for include-evaluate-and-cache: match arm bodies must be single expressions,
# so the two-step Ok/Error handling is extracted into named helpers.
include-cache-success: [fn@[return: Any] [let hash result]
  [include-cache-put hash [Cached result]]
  result]

include-cache-failure: [fn@[return: Any] [let hash e]
  [include-cache-put hash [Missing]]   # reset so retries work
  [raise e]]

include-evaluate-and-cache: [fn@[return: Any  doc: """
  Evaluate a file and update the content-addressed cache.
  Marks [Pending] before evaluation; resets to [Missing] on error so retries work.
  Passes cap as %include-dir and [] as initial % (included files don't inherit
  the caller's pipeline input — they start fresh).
"""] [let source path hash cap]
  [include-cache-put hash [Pending]]
  [match [try [fn [] [eval-file [expand [load source name: path]] [] cap]]]
    [Ok result]: [include-cache-success hash result]
    [Error e]:   [include-cache-failure hash e]]]

include: [fn@[return: Any  doc: """
  Load, expand, and evaluate a tinct file from a DirCap.
  Content-addressed memoization (blake3 hash of source) provides both result
  caching and circular include detection via a three-state cache:
    [Missing]      — not yet loaded (or failed — reset after error)
    [Pending]      — currently being evaluated (circular if seen again)
    [Cached value] — previously evaluated result

  The DirCap is injected as %include-dir so the included file can sub-include.
  Included files start with % = [] (they do not inherit the caller's pipeline input).
"""] [let cap@DirCap path@String]
  [source: [slurp [open cap path Readable]]]
  [hash:   [blake3 [str [cap-identity cap] "|" source]]]
  [match [include-cache-get hash]
    [Cached result]: result
    Pending:         [raise [str "circular include: " path]]
    Missing:         [include-evaluate-and-cache source path hash cap]]]

]
```

### `include` Pipeline Step by Step

1. `[open cap path Readable]` → Handle — capability-gated file open; `cap` confines filesystem access
2. `[slurp handle]` → String — read the source text
3. `[blake3 [str [cap-identity cap] "|" source]]` → String — compute cache key: cap identity + content hash
4. `[include-cache-get hash]` → cache state:
   - `[Cached result]` → return cached value immediately (memoization)
   - `[Pending]` → circular include; raise error with path for diagnostics
   - `[Missing]` → not seen; continue
5. `[include-cache-put hash [Pending]]` — mark in-flight before evaluating
6. `[load source name: path]` → FileDictAST — parse; `name:` records provenance for error spans
7. `[expand ast]` → FileDictAST — macro expansion; produces the final AST
8. `[eval-file ast [] cap]` → Any — evaluate documents; `[]` as initial `%`; `cap` becomes `%include-dir`
9. `[include-cache-put hash [Cached result]]` — memoize for future calls

**Cache key:** `blake3(cap-identity + "|" + source)` where `cap-identity` returns `"dev:ino"` from `fstat` on the DirCap's O_DIRECTORY fd. Same source under the same directory (by filesystem identity) shares one cache entry. Same source under different directories gets distinct entries — necessary because they may sub-include different siblings via `%include-dir`. The key is stable across renames and moves, correct under Linux mount namespaces (no path resolution — fd identity used directly), mirroring the `(dev, ino)` keying used by the current `builtin_include` cache.

### `---` and `|`: Related but Separate Mechanisms

`|` and `---` both thread a value as `%` to the next stage, but they are implemented independently:

- **`|`** is desugar-only — `lhs | rhs` rewrites to `[rhs lhs]` before evaluation. The evaluator never sees `Expr::Pipe`. No `%` binding occurs; the value is passed as a function argument. See `doc/feature/access-pipeline.md`.
- **`---`** is eval-time — `eval-document-pipeline` calls `eval` for each document with `%:` and `env:` parameters. Scope chain promotion (prev dict entries visible in next doc) is expressed via the `env:` merge. Named sections accumulate across documents.

For bare `---` (no section headers), the two are conceptually equivalent: `a | f | g  ≡  a --- [f %] --- [g %]`. But they share the *concept*, not code.

### CLI Multi-File Pipeline

`tinct run file1.llt file2.llt file3.llt` is the same mechanism as `---` applied across files: each file's final value becomes `%` for the next. With `eval-file` taking an explicit `initial`, the CLI pipeline is simply `eval-document-pipeline` over a list of files:

```tinct
# CLI multi-file pipeline — same mechanism as --- within a file.
# pwd: the DirCap for the working directory (passed explicitly by main.rs,
# not captured from closure — %pwd is in the user runtime env, not prelude scope).
# Each file gets pwd as its %include-dir so it can sub-include sibling files.
cli-pipeline: [fn@[return: Any] [let files initial pwd]
  [reduce
    [fn@[return: Any] [let prev file-path]
      [eval-file
        [expand [load [slurp [open pwd file-path Readable]] name: file-path]]
        prev
        pwd]]
    initial
    files]]
```

The current Rust `run_eval` function becomes this reduce. `cli-pipeline` receives `%pwd` as an explicit parameter (`pwd`) from `main.rs` — it cannot capture it from the closure since `%pwd` is injected into the user runtime env after prelude loads, not into the prelude scope. `|` is desugar-only and does not share code with `---` or the CLI pipeline; the three share the *concept* of sequential `%` threading but not a code substrate.

### `eval-types` for Type-Stage Documents

`--- stage: type` documents define type-level resolver functions for CHR constraint solving. They are not pipeline stages — no `%`, no memoization, no scope promotion. `eval-document-pipeline` skips them; the type checker evaluates them separately:

```tinct
# type checker calls this for each stage: type document
[eval-types [get "expressions" doc]]
```

`eval-types` uses `type_stage_env` as its base: type-level evaluation builtins only, no IO, no caps, no runtime API. `eval` and `eval-types` are siblings sharing the evaluation mechanism with different base environments.

### Unlocked Use Cases

```tinct
# Formatter corpus test — parse without evaluating
[format-file [load "source text" name: "reference.llt"]]

# Docgen — inspect AST structurally instead of string-parsing source
[extract-docs [load [slurp [open cap "stdlib/strings.llt" Readable]] name: "strings.llt"]]

# Linting — parse and expand without evaluation
[typecheck [expand [load source name: path]]]

# Macro inspection — inspect post-expansion AST
[expand [load source name: path]]

# Inline eval from string ([] = empty initial %, %libdir = include-dir for sub-includes)
[eval-file [expand [load "[+ 1 2]" name: "inline"]] [] %libdir]
```


## What Would Change

No transition periods. No backwards compatibility. Code is deleted, not shimmed.

### Add: Eight New Rust Primitives

Register `load`, `expand`, `eval`, `eval-types`, `blake3`, `cap-identity`, `include-cache-get`, `include-cache-put` in `standard_builtins()`. Add `EvalState::include_cache: HashMap<String, IncludeCacheEntry>` (content-addressed, keyed by `blake3(cap-identity + "|" + source)`). Rename `eval` → `deep-materialize` and `force` → `materialize` in `standard_builtins()` and all call sites. Add `type_stage_env: Rc<RefCell<Environment>>` to `EvalConfig`; built once at startup alongside `stdlib_env` using `build_type_stage_env()` from the typechecker.

### Delete: `builtin_include` and All Include Infrastructure

- **Delete** `builtin_include` (`src/builtins_meta.rs`) — the entire function, all 350+ lines
- **Delete** `EvalState::include_guard: HashSet<(u64, u64)>` — replaced by `[Pending]` cache state in tinct
- **Delete** `EvalState::include_cache` (the old inode-keyed cache) — replaced by content-addressed cache
- **Delete** `Value::RustRegistry` variant (`src/value.rs`) — `%rust` is now a plain `Value::Dict`
- **Delete** `rust_module()` dispatcher and all module grouping (`"core"`, `"io"`, `"net"`, etc.) from `src/builtins.rs`
- **Delete** any per-file capability injection in `builtin_include` — the entire function is deleted, so all its injection logic goes with it
- **Delete** the `builtin-*` aliases (`builtin-add`, `builtin-if`, etc.) from module group setup — `%rust` flat dict makes them unnecessary
- **Delete** the `[include %rust "module"]` special-case in the include resolver

The bootstrap path that loads prelude at startup remains as a private Rust function — not a registered builtin, not callable from tinct code.

### Delete: `src/eval_pipeline.rs`

The entire file is superseded by tinct code:

- **Delete** `eval_file_with_input` — replaced by tinct `eval-file`
- **Delete** `eval_document` — sequential let\* loop extracted into `eval_expressions` helper, then file deleted
- **Delete** `run_eval` — replaced by tinct `cli-pipeline`
- **Delete** `src/eval_pipeline.rs` entirely once all public functions are removed

### Modify: `src/ast_dict.rs` — Add `dict_to_file`

Add `dict_to_file(val: &Value, ctx: &Rc<EvalContext>) -> Result<File, AstError>` as an internal function (not a registered builtin). It is the file-level inverse of `ast_to_dict`, used by the `expand` builtin to bridge the Dict boundary.

The existing `dict_to_ast` handles individual `Expr` nodes. `dict_to_file` reconstructs the full `File` struct from the schema emitted by `ast_to_dict`:

- `documents`: iterate the `documents` list; for each document dict:
  - `expressions`: iterate the seq, call `dict_to_ast` on each entry
  - `name`: string field or `None` if `[]`
  - `stage`: read the nominal variant — `[Runtime]` → `Some(Stage::Runtime)`, `[Type]` → `Some(Stage::Type)`
  - `output_type`, `expects`: annotation fields, or `None` if `[]`
  - `caps`: always `None` — not serialized by `document_to_dict`
- Spans: recovered from each node's `span:` dict via the existing `extract_span` helper; `Span::origin()` if absent

### Modify: `src/eval.rs` — Expose `eval` and `eval-types`; Add `type_stage_env`

Add `type_stage_env: Rc<RefCell<Environment>>` to `EvalConfig`. Built once at startup alongside `stdlib_env` using `build_type_stage_env()` from the typechecker. This avoids rebuilding during typecheck and eliminates the infinite-recursion hazard (building the env during typecheck triggers `typecheck_file()` → loop).

Extract `eval_expressions(exprs: &[Spanned<Expr>], env: Rc<RefCell<Environment>>, ctx: &Rc<EvalContext>) -> EvalResult<Rc<Thunk>>` as a shared helper from the body of `eval_document`. This is the sequential let\* loop reused by both the `eval` builtin and the bootstrap prelude-load path.

Expose `eval` and `eval-types` as registered builtins.

#### `eval` — Runtime-stage evaluation

Positional args: `exprs` (positional Dict of expression AST dicts — the `expressions` field from a document dict). Named args: `%:` (pipeline input bound as `$`), `env:` (extra bindings dict, defaults to `[]`).

Implementation:
1. Deserialize `exprs`: iterate in key order, call `dict_to_ast` on each entry → `Vec<Spanned<Expr>>`
2. Build env chain: `ctx.config.stdlib_env` as root → child env with `env:` entries injected → child env with `"$"` bound to the `%:` thunk
3. Call `eval_expressions`; return its result lazily
4. Empty `exprs` returns an empty Dict thunk

`caps:` validation is not performed — it is a static document annotation, not part of `eval`'s runtime contract.

#### `eval-types` — Type-stage evaluation

Positional args: `exprs` (positional Dict of expression AST dicts). No `%:` or `env:` parameters.

Same deserialization and `eval_expressions` call as `eval`. Base env is `ctx.config.type_stage_env` instead of `ctx.config.stdlib_env`. Contains type-level builtins only — no IO, no caps, no runtime API.

### Modify: `src/main.rs` — `tinct run` Uses `cli-pipeline`

**Delete** the Rust `run_eval` call. After prelude loads, call the tinct `cli-pipeline` function directly:

```rust
// files_thunk: positional Dict of String file paths (constructed from Vec<String>)
// initial_thunk: stdin value or empty dict
// pwd_thunk: %pwd DirCap (the user's working directory)
let cli_pipeline = stdlib_env.get("cli-pipeline").expect("prelude must define cli-pipeline");
let result = invoke_function(&cli_pipeline, &[files_thunk, initial_thunk, pwd_thunk], &ctx)?;
```

`---` (within-file) and multi-file CLI pipeline share `eval-document-pipeline` as their substrate. `|` remains desugar-only per the access-pipeline design.

### Modify: `src/expand.rs` — Expose `expand`, Delete Shadow Guard

**Expose** `expand` as a user-callable builtin via a round-trip through the Dict representation:
1. Receive the file AST dict (the `Dict` returned by `load`)
2. `dict_to_file(ast_dict, ctx)` → `File` — deserialize; schema errors surface as user errors with the `AstError` message
3. `crate::expand::expand(&file, ctx)` → `File` — run macro expansion
4. `ast_to_dict(&expanded, &AstToDictOpts::default(), ctx)` → return as Dict

**Delete** the shadow guard (`src/expand.rs:174`) entirely — the capability model is the real security boundary.

### `expects:` — Static Contract Only

`eval-document-pipeline` ignores `expects:` at runtime. Static type annotation for the type checker only. No Rust change needed.

### `builtin_reduce` — Remove Materialize Call

**Delete** the `materialize` call on the accumulator between iterations (`builtins_seq_reduce.rs:80-81`). Pass each step result as a thunk directly. Makes document boundaries lazy; arithmetic reductions unaffected (they force naturally).

### Formatter and Docgen

Both become direct consumers of `load`. Formatter feeds source text directly; docgen inspects AST structurally, eliminating all string-based doc annotation extraction.

## Prerequisites

- **`builtin-privacy-complete` sprint** — removes `standard_builtins()` re-injection; `%rust` (now a flat `Value::Dict`) becomes structurally unreachable from user code via env isolation; unblocks `eval` exposure
- **`eval`/`force` rename** — `eval` → `deep-materialize`, `force` → `materialize`; must complete before adding new `eval`
- **Stable file AST dict schema** — `load` and `expand` require a stable `ast_to_dict` format (from `runtime-reflection` sprint); `document_to_dict` must emit `stage: [Runtime] | [Type]` as a nominal variant (not currently emitted)
- **`dict_to_file` in `src/ast_dict.rs`** — file-level inverse of `ast_to_dict`; required by the `expand` builtin for the Dict → File round-trip. The expression-level `dict_to_ast` already exists; `dict_to_file` adds the `Document` and `File` layers above it.

## References

- Abelson, H. & Sussman, G.J. (1996). *Structure and Interpretation of Computer Programs*, 2nd ed. MIT Press. §4.1 "The Metacircular Evaluator."
- Kelsey, R., Clinger, W., Rees, J. (1998). "Revised⁵ Report on the Algorithmic Language Scheme." — `letrec*` as the foundational sequential scoping form.
- Launchbury, J. (1993). "A Natural Semantics for Lazy Evaluation." *POPL '93*, pp. 144-154. — lazy letrec vs strict let\* at document boundaries.
- Landin, P.J. (1964). "The Mechanical Evaluation of Expressions." *Computer Journal*, 6(4). — arguments evaluated in caller scope, body in closure scope (let, not letrec).
