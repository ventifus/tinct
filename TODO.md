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

## Known Bugs (Type Checker)

### `named-arg-types`: Named argument types not checked against parameter types

`typecheck.rs:2217` (`TODO(named-arg-types)`): Named arg types are not unified against the corresponding parameter types. The type checker infers the named arg's own type (for error propagation and `type_map`) but does not validate it against the parameter's declared type. Type mismatches in named args are silently accepted by the type checker even though the evaluator validates them at runtime via `C-NAMED-VALID`.

Root cause: `Type::Function` stores `params: Vec<Type>` (positional, no names). Matching a named arg `x: expr` to the correct param requires knowing param names — currently not stored in `Type::Function`. Fix requires extending `Type::Function` to `params: Vec<(String, Type)>` or equivalent.

```tinct
# Currently: type checker accepts this even if x expects Int
[f x: "wrong-type"]  # silently passes type checking; evaluator catches at runtime
```

- [ ] Extend `Type::Function` to carry param names alongside param types: `params: Vec<(Option<String>, Type)>` where `None` = positional-only; update all construction and matching sites (`src/types.rs`, `src/typecheck.rs`)
- [ ] In `check_call` and `check_call_with_scheme`: for each named arg `x: expr`, find the param with matching name and unify the arg type against the param type; emit `TypeError` on mismatch or unknown name (`src/typecheck.rs`)
- [ ] Corpus tests: named arg with wrong type produces a type error; unknown named arg name produces a type error (`tests/corpus/`)

### `sequential-doc-scope`: Document-level Sequential bindings invisible to type checker

`imports.rs::extract_bindings_from_file` only processes the **last** expression of a multi-expression document. Earlier `[name: val]` Sequential steps' bindings are not added to the TypeEnv, so the type checker sees later references to those names as "undefined variable."

This is why `--strict` on flat-document scripts like `samples/versions.llt` reports `undefined variable: https-get` even though `https-get` is defined earlier in the same document. The evaluator handles this correctly (Sequential evaluation is strict); only the type checker is blind to it.

Fix: in `extract_bindings_from_file` (or `build_type_env`), thread the TypeEnv through all intermediate Sequential expressions in order, extracting string-keyed bindings from each step before processing the next — mirroring what the evaluator's Sequential handler does at runtime.

- [ ] In `extract_bindings_from_file` (`src/imports.rs`): for `Expr::Sequential`, process each intermediate expression in order, extracting string-keyed bindings and extending the accumulated env before moving to the next expression; currently only the last expression is processed (`src/imports.rs`)
- [ ] Corpus tests: document-level `[name: val]` binding visible to type checker in a later Sequential step; function defined in step 1 callable in step 3 (`tests/corpus/`)

### `dict-equality`: `=` always returns `false` for Dict operands

`eval.rs:2012`: `(Value::Dict(_), Value::Dict(_)) => false` — the `=` operator returns `false` for any two dicts regardless of content. Structural dict equality is not implemented. Users writing `[= config1 config2]` get a surprising result.

This affects any equality check on dict values, including in `[if [= a b] ...]`, `[filter [fn [x] [= x target]] list]`, etc.

Note: deep structural equality is non-trivial (requires forcing thunks, handling cycles). The current behavior is a safe conservative choice but should at minimum produce a runtime error or warning rather than silently returning `false`.

- [ ] Decide: implement structural dict equality (force all entries, recurse), or change the semantics to error on dict equality with a clear message (`eval.rs:2012`)
- [ ] If implementing: handle cycles (pointer-identity visited set), lazy thunk forcing, integer vs string key ordering (`src/eval.rs`)
- [ ] Document in `doc/03-data-model.md` §Equality: current behavior and the design decision (`doc/03-data-model.md`)

### `match-arm-scope`: Pattern-bound variables not scoped into arm bodies by type checker

When type-checking `[match x [ok: v] body ...]`, the type checker does not inject pattern-bound variables (`v`) into the TypeEnv for the arm body. The variable is accessible at runtime but appears as `Any` (or "undefined") to the type checker, suppressing useful type errors on the bound name.

```tinct
[match result
  [ok: v]    [str "got: " v]   # v: Any — type checker can't narrow or check v
  [err: msg] msg]              # msg: Any
```

The fix: in `check_match` / `infer_match` (`src/typecheck.rs`), for each arm, extract the pattern bindings (variable name → type from the pattern structure) and extend the TypeEnv before type-checking the arm body. Dict patterns `[ok: v]` bind `v` to the type of the `ok` field; seq/literal patterns bind their capture names similarly.

- [ ] In `infer_match` / `check_match` (`src/typecheck.rs`), extract pattern bindings per arm and extend TypeEnv with `name → inferred_type` before type-checking the arm body; the pattern structure already determines the type (e.g., `[ok: v]` on `Result([ok: T  err: E])` binds `v: T`) (`src/typecheck.rs`)
- [ ] Corpus tests: `v` has correct inferred type inside arm body; misuse of `v` produces a type error; nested patterns bind all variables (`tests/corpus/eval/`)

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

## Known Bugs (Runtime — Registered Builtins That Always Fail)

These builtins are registered in `standard_builtins()` and appear in the public API, but calling them produces a runtime error. Users have no warning at call time.

### `stub-builtins`: Registered builtins that always error at runtime

- [ ] `socks5-connect` (`src/builtins_io.rs:3590-3592`) — always returns `"not yet implemented"`; remove from registry or implement
- [ ] `proxy-connect` (`src/builtins_io.rs:3597-3599`) — always returns `"not yet implemented"`; remove from registry or implement
- [ ] `open` write (`"w"`) and append (`"a"`) modes (`src/builtins_io.rs:195-201, 365-370`) — return `"not yet implemented (Phase 1 is read-only)"`; `--cap-file w` works but `open "w"` inside a script does not — asymmetry with CLI
- [ ] `tls-connect` Handle form (`src/builtins_io.rs:2451-2454`) — the 3-arg `tls-connect handle sni opts` form is documented but always errors `"Handle form not yet supported — use Connector form"`; remove the form or implement it
- [ ] `connect` UDP transport (`src/builtins_io.rs:612`) — always errors `"UDP not yet supported, use Tcp"`; document or implement
- [ ] `--cap-net` CIDR range entries (`src/main.rs:692-695`) — always error `"CIDR ranges are not yet implemented"`; document or implement

### `spki-pinning-wrong`: SPKI pinning hashes whole certificate, not the public key

`src/builtins_io.rs:3075-3077` — `compute_spki_hash` hashes the full DER certificate bytes (`cert_der.as_ref()`) instead of extracting and hashing the SubjectPublicKeyInfo field. This makes `spki-pin` semantically incorrect: it does not pin the public key, it pins the whole certificate. SPKI pinning's value (surviving CA rotation) is lost. Additionally, `tls-peer-cert` returns placeholder strings for all cert fields (`src/builtins_io.rs:3172-3205`).

- [ ] Fix `compute_spki_hash` to extract the SPKI field from the DER certificate before hashing (use `x509-parser` or `rustls-pki-types` to parse the cert and extract `subject_public_key_info`); the hash must match what `spki-pin` generates (`src/builtins_io.rs`)
- [ ] Implement `tls-peer-cert` subject/issuer parsing — currently returns placeholder strings `"(certificate parsing not yet implemented)"` (`src/builtins_io.rs:3172-3205`)

## Known Bugs (Evaluator)

### `variant-payload-eq`: Variant payload equality returns false for non-unit constructors

`src/builtins_math.rs:199` — `TODO(C3)`: The `=` operator on `Variant` values checks tag equality and returns `false` if either has a payload (`Some(_)`). Payload constructors with identical payloads compare as unequal. Only unit constructors (no payload) can be compared with `=`.

```tinct
[match [= [Ok 42] [Ok 42]]  # returns false — payload comparison broken
  true  "equal"
  false "not equal"]         # always "not equal" for payload variants
```

- [ ] Implement recursive payload equality for `Variant` values in the `=` builtin: if both are `Variant { tag, payload: Some(p1) }` and `Variant { tag, payload: Some(p2) }` with matching tags, force and compare `p1` and `p2` recursively (`src/builtins_math.rs:199`)

### `guard-default-missing`: `default:` annotation not applied on guard failures

`src/eval.rs:1634-1638` — When a `TypeAssert` guard fails (the value doesn't match the annotated type), the evaluator does not fall back to the `default:` value in the annotation. The `Guarded` thunk captures the constraint but not the default. Guard failures propagate as errors even when a sensible default exists.

- [ ] In `ThunkState::Guarded` and the guard-failure path: check if the annotation carries a `default:` value; if so, return that value instead of propagating the error (`src/eval.rs`, `src/eval_materialize.rs`)

## Known Bugs (Parser)

### `pin-patterns`: Pin patterns (`$name`) not implemented in `match` arms

`src/parser.rs:3915` — `TODO: Pin patterns ($name) require tracking whether the VarRef came from Token::EscapedRef or Token::Identifier, which is lost after expr parsing.` Writing `$x` in a match arm intending to pin (match against the *value* of `x`) silently binds a new `x` instead of testing equality.

- [ ] Distinguish `$name` (pin — match against variable value) from bare `name` (bind — introduce new variable) in `Pattern::VarRef`; the token kind (`EscapedRef` vs `Identifier`) must be preserved through the pattern-parsing path (`src/parser.rs`)

## Known Bugs (Tests)

### `ignored-tests`: Disabled tests masking real gaps

- [ ] `test_typecheck_corpus` (`tests/corpus_tests.rs:271`) is `#[ignore]`d because `get` (a prelude function) lacks a type signature in `TypeEnv`, and `merge`/`+` produce row-polymorphism false positives — add `get`, `merge`, and `+` type signatures to `TypeEnv::with_builtins()` or `build_prelude_env()` so the typecheck corpus can be re-enabled (`src/type_env.rs`, `src/imports.rs`)
- [ ] `test_tco_tail_recursive_function` (`src/eval.rs:8188`) is `#[ignore]`d because full TCO is not wired — PendingCall thunks still accumulate call depth; re-enable once the CEK `Action::Eval` dispatch eliminates depth accumulation (`src/eval.rs`)

## Known Bugs (Correctness)

### `int-float-precision`: Integer → Float promotion silently loses precision for integers > 2^53

`src/builtins_math.rs:191, 252` — When `Int` and `Float` operands are mixed in arithmetic, `Int` is promoted to `Float` via `as f64`. Integers beyond 2^53 lose precision silently. The comment notes this matches Jsonnet behavior, but users have no warning.

- [ ] Add a runtime warning (or error in strict mode) when an integer beyond `2^53` is promoted to `f64` in arithmetic builtins (`src/builtins_math.rs`)

## Known Bugs (CLI)

### `e-flag-ordering`: `-e` flag expressions don't interleave with file arguments

`src/main.rs:750-754` — The CLI spec says `-e expr` should be processed in the order it appears relative to file arguments (so `tinct run a.llt -e '[transform %]' b.llt` pipelines through the transform). The implementation collects all files first then all `-e` expressions, ignoring relative ordering.

- [ ] Track relative order of file and `-e` arguments in the CLI parser; build the pipeline stage list in declaration order rather than files-then-expressions (`src/main.rs`)

## Standard Library

### `stdlib-doc-annotations`: Add `@[doc: "..."]` to all exported stdlib functions

The `doc-annotations` sprint wired up the full infrastructure (DocMap extraction, `:describe` in REPL, LSP hover, `tinct describe` CLI) and seeded 8 functions in `prelude.llt` as examples. The remaining exported functions across all stdlib files have no doc annotations. Only the **last dict** in each multi-expression file is exported — internal helpers in earlier dicts should not be annotated.

Annotation format: `fn-name@[doc: "One-line description"]: [fn ...]` for public entries; param docs go on the `fn` annotation: `[fn@ReturnType [param@[type: T doc: "Description"]] ...]`.

- [ ] `stdlib/prelude.llt` — annotate all ~190 exported public functions (the last-dict entries); skip `-impl`, `-step`, `-check` helpers in earlier dicts (`stdlib/prelude.llt`)
- [ ] `stdlib/strings.llt` — `pad-left`, `pad-right`, `str-find`, `str-reverse`, `str-repeat` (`stdlib/strings.llt`)
- [ ] `stdlib/math.llt` — `pi`, `e`, `phi`, `hypot`, `deg->rad`, `rad->deg`, `log-base` (`stdlib/math.llt`)
- [ ] `stdlib/encoding.llt` — `base64-encode`, `base64-decode`, `hex-encode`, `hex-decode`, `mask-apply`, `bytes-reverse`, `bytes-repeat` (`stdlib/encoding.llt`)
- [ ] `stdlib/numeric.llt` — all exported numeric utility functions (`stdlib/numeric.llt`)
- [ ] `stdlib/path.llt` — `basename`, `dirname`, `path-join`, `extension`, `path-parts` (`stdlib/path.llt`)
- [ ] `stdlib/io.llt` — `write-line`, `write-file`, `write-file-atomic`, `read-file`, `read-lines` (`stdlib/io.llt`)
- [ ] `stdlib/net.llt` — `http-get`, `fetch`, `parse-url`, `build-http-request`, `parse-http-response` and helpers (`stdlib/net.llt`)
- [ ] `stdlib/datetime.llt` — all exported datetime functions (`stdlib/datetime.llt`)
- [ ] `stdlib/regex.llt` — all exported regex functions (`stdlib/regex.llt`)
- [ ] `stdlib/toml-lite.llt` — `parse-toml-lite` and its exported helpers (`stdlib/toml-lite.llt`)
- [ ] `stdlib/macros.llt` — `tmpl-transformer` and any other exported macros (`stdlib/macros.llt`)
- [ ] `stdlib/formatter/compact.llt`, `stdlib/formatter/pretty.llt` — exported formatting functions (`stdlib/formatter/`)
- [ ] `stdlib/out/*.llt` — the public `json`, `yaml`, `csv`, `toml`, `env`, `llt`, `raw` formatter entry points (`stdlib/out/`)
- [ ] Verify `:describe` and LSP hover return doc strings for all newly annotated functions after each file (`tinct repl`)

## Codebase Health
