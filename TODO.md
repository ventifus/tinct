# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Known Bugs

### ~~`iife-parse`: `[[fn ...] args]` parsed as Dict, not Call~~ (BY DESIGN)

`[[fn [x] body] arg]` is parsed as a 2-element data array, not a function call.
This is intentional: a bracket expression in head position produces data (Priority 7 fallback),
preserving the `[[condition-result] value]` pair pattern used by `cond` and similar constructs
(30+ occurrences in stdlib). Changing the fallback to "call" would break all of these.

IIFEs are largely unnecessary in tinct: fn-body Sequential and document-level Sequential
both provide local bindings natively. For rare inline cases, use `[call [fn [x] body] arg]`.

Documented in `doc/02-syntax.md` §3.3.1a (head-position rule) and §3.3.1b (IIFE patterns).

### ~~`sequential-lazy`: Sequential fn-body bindings are lazy, not eager~~ (FIXED)

Fixed in `sequential-strict` sprint: Sequential named bindings are now forced to WHNF at binding time (strict `let*` semantics). See `doc/09-documents.md` §[SEQ-SCOPE].

### ~~`depth-limit-toml`: `parse-toml-lite` exceeds depth on large TOML files~~ (RESOLVED)

Resolved by removing `MAX_EVAL_DEPTH` in the `sequential-strict` sprint. The recursive
tinct parser in `stdlib/toml-lite.llt` required ~900 depth levels for a 60-line TOML file;
without a depth limit this is no longer a concern.

---

## Known Bugs (Type Signatures)

### `length-narrow-type`: `length` typed as Dict-only but accepts String and Bytes

`type_env.rs` registers `length` with parameter type `[...]` (open record — Dict only). At runtime, `builtin_length` dispatches on `Value::String` and `Value::Bytes` in addition to `Value::Dict`. The correct parameter type is `Dict | String | Bytes` — not `Unknown` (which would accept Int, Float, etc. that actually error at runtime). Confirmed in `samples/versions.llt` errors at lines 60 and 69.

- [ ] Change `length` parameter type in `type_env.rs` to `Type::Union(vec![Type::Record(open_row), Type::String, Type::Bytes])`, accurately reflecting the three dispatch branches in `builtin_length` (`src/type_env.rs`) — **blocked**: requires `unify()` in `type_unify.rs` to gain a `(Union, T)` match arm; the unifier currently falls through to the wildcard error for Union-containing-RowVar params. A comment with the full Union form is left in `type_env.rs:length` as a forward reference. Currently uses `Type::Unknown` (accepts any arg).
- [x] Corpus test: `[length "hello"]` and `[length [str-bytes "hi"]]` eval correctly (`tests/corpus/eval/builtins/length_string.llt-eval`, `length_bytes.llt-eval`)

## Diagnostics

### `rich-diagnostics`: Rust-style error reporting with source context

Type errors and parse errors currently print bare messages with coordinates:
```
arity mismatch: expected 1 argument(s), got 2 (2 positional, 0 named) at 10:1-10:32
undefined variable: https-get at 34:18-34:27
```

Runtime errors already use `render_span_snippet` (see `src/main.rs:1224`). Type errors call `format!("{e}")` with no source context (`src/main.rs:1433`, `src/lib.rs:246`). The goal is Rust-style output:

```
error[T002]: undefined variable: `https-get`
 --> samples/versions.llt:34:18
  |
34 |   [rust-toml-raw: [https-get "static.rust-lang.org" 443
  |                    ^^^^^^^^^
  = note: `https-get` is defined earlier in the document but Sequential
          bindings are not visible to the type checker at this point

error[T001]: arity mismatch
 --> samples/versions.llt:10:1
  |
10 | [include %libdir "strings.llt"]
   | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
  = note: `include` expects 1 argument, got 2 (2 positional, 0 named)
  = help: the 2-argument cap-qualified form `[include cap "path"]` is
          the correct form; single-argument `[include "path"]` is removed
```

- [x] Add error codes to `TypeError` enum variants: `T001` (arity mismatch), `T002` (undefined variable), `T003` (cannot unify), `T004` (type assert failure), etc. — parallel to runtime `E0xx` codes (`src/typecheck.rs`, `src/error.rs`)
- [x] Implement `fn format_type_error(err: &TypeError, source: &str) -> String` that renders the Rust-style multi-line format: `error[Txxx]: message\n --> file:line:col\n  |\nNN | source\n   | ^^^^` using existing `render_span_snippet` infrastructure (`src/error.rs`)
- [x] Apply `format_type_error` to type error output in `run_eval` (`src/main.rs:1433`) and `typecheck_source` (`src/lib.rs:246`); the source string is already available in both call sites
- [ ] Apply same snippet rendering to parse errors in strict mode (parse errors currently also lack source context) (`src/main.rs`, `src/error.rs`)
- [x] Add contextual `= note:` lines for common type errors: arity mismatch (show expected vs got); undefined variable (note if name appears in a later Sequential step — "defined later at line N"); cannot unify (show both types with labels "expected" / "found") (`src/typecheck.rs`, `src/error.rs`)
- [ ] Add `= help:` suggestions for actionable fixes: arity mismatch on `include` → "use cap-qualified form `[include %libdir \"path\"]`"; undefined variable that looks like a Sequential scope issue → "group definitions with `[call [fn [] ...]]`" (`src/error.rs`)
- [x] Add `tinct explain T001` subcommand (like `rustc --explain`) — each error code has a long-form explanation with examples (`src/main.rs`, new `src/explain.rs`)
- [ ] Update `tinct run --strict` output header from bare `type checking failed with N error(s)` to `error: type checking failed with N error(s) (use --strict to make fatal)` in non-strict mode, or `error: type checking failed — cannot evaluate` in strict mode (`src/main.rs`)
- [x] Corpus tests for error message format: verify snippet is present, correct line/col, correct caret length; verify note/help text for known error patterns (`tests/corpus/`)

## Capabilities

### `cap-file`: Single-file capability via `--cap-file name=path:mode`

`--cap-fs` injects a `DirCap` granting access to an entire directory. For pinpoint access to a single file, the right primitive is `Handle` — an already-open file descriptor. A new `--cap-file name=path:mode` CLI flag pre-opens the file and injects it as `%name` (the `%` prefix is added by tinct, same as `--cap-net nc=...` → `%nc`).

```bash
# User writes on command line (no % prefix):
tinct run --cap-file config=Cargo.toml:r script.llt

# Tinct injects as %config (Handle[Readable Text]) in root env
```

```tinct
# In the script:
--- caps: [%config: @Handle]
[slurp %config]   # can only read this one file
```

Mode suffix: `r` → `{Readable, Text}`, `rb` → `{Readable, Binary}`, `w` → `{Writable, Text}`, `wb` → `{Writable, Binary}`.

- [x] Parse `--cap-file name=path:mode` entries in CLI (repeatable, same pattern as `--cap-net`); validate mode suffix (`r`, `rb`, `w`, `wb`); auto-prefix `%` to the name (`src/main.rs`)
- [x] Open the file using `cap_std::ambient_authority()` + appropriate `OpenOptions`; wrap as `Value::Handle` with correct caps `{Readable/Writable, Text/Binary}` and `write_inner: None/Some` depending on mode (`src/main.rs`, `src/builtins_io.rs`)
- [x] Ensure `--no-fs` (already exists) also suppresses `--cap-file`-injected Handles; verify the flag correctly blocks all filesystem caps: `%pwd`, `%libdir`, `--cap-fs`, and `--cap-file` (`src/main.rs`)
- [ ] Extend `--- caps:` pragma runtime validation to handle `@Handle` type: emit `%config@Handle is required but not provided\n  inject it with:  tinct run --cap-file config=PATH:r ...` — the flag hint is derived from the cap name (strip `%`) and defaults to `:r` mode; see `doc/09-documents.md` §caps: pragma for the full error format (`src/eval.rs` or `src/main.rs`)
- [x] Update `doc/09-documents.md` §caps: pragma: add `@Handle` to the type table alongside `@NetCap` and `@DirCap`, showing the corresponding `--cap-file` flag hint in error messages (`doc/09-documents.md`)
- [x] Update `doc/12-tooling.md`: document `--cap-file name=path:mode` alongside `--cap-fs` and `--cap-net`; document `--no-fs` as the coarse-grained filesystem suppressor (blocks `%pwd`, `%libdir`, all `--cap-fs`, all `--cap-file`); note that mode determines read/write capability and text/binary encoding (`doc/12-tooling.md`)
- [x] Corpus / CLI tests: `--cap-file` injects readable Handle; `[slurp %handle]` reads the file; write mode allows `[write-handle %handle ...]`; missing file errors clearly (`tests/`)

## Codebase Health
