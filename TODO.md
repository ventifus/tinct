# Implementation Roadmap

See DONE.md for the full history of completed sprints.

---

## Known Bugs

### `iife-parse`: `[[fn ...] args]` parsed as Dict, not Call

When the first token inside `[` is another `[`, the parser uses priority 5
(Dict/data fallback) instead of recognising the expression as a function call.
So `[[fn [x] body] arg]` produces a 2-entry array `{0: fn-literal, 1: arg}`
rather than applying the fn to the arg. Users must write `[call [fn [x] body] arg]`.

This is a significant ergonomic issue for IIFEs and any pattern where a
computed function is immediately applied. The fix is to detect the `[expr args...]`
pattern (where expr is any bracketed sub-expression, not just an identifier)
and emit `StackFrame::Call` instead of `StackFrame::Dict`.

### `sequential-lazy`: Sequential fn-body bindings are lazy, not eager

`Expr::Sequential` (multi-expression fn bodies) materializes the outer dict at
each step to extract string-key bindings, but the binding VALUES remain as
unevaluated thunks in the child env. Pre-computing an expensive value as a
Sequential step does NOT make it cache at a shallow depth; it will be forced
lazily at whatever depth first demands it.

This can cause `[E040] maximum evaluation depth exceeded (256)` in scripts that
combine: complex lazy chains (map/join/str) + lazy thunks for heavy computations
(large file parses, network fetches). The workaround is to avoid lazy accumulation
— use string-search (`str-contains?` + `split`) instead of stateful reduce dicts,
and avoid any pattern where large computations are forced deep inside call chains.

A real fix would force binding values during Sequential materialization, or
expose an explicit `force` builtin.

### `depth-limit-toml`: `parse-toml-lite` exceeds depth on large TOML files

The recursive tinct parser in `stdlib/toml-lite.llt` uses ~15 depth levels per
TOML line (via `parse-lines-impl` → `parse-line-dispatch` → `parse-key-value` →
`parse-value-try-int` → `try-or` → `try`). On a Cargo.toml with 60 non-blank
lines, it requires ~900 depth levels — far exceeding the `MAX_EVAL_DEPTH = 256`
limit when called from any non-trivial lazy evaluation chain.

Fix options: (a) increase `MAX_EVAL_DEPTH`, (b) rewrite the TOML parser using
`builtin-reduce` (Rust-level iteration resets depth per iteration), or (c) add a
`parse-toml-lite-iter` builtin that processes line-by-line in Rust.

---

## Research

### `research-parameterized-dict`

Investigate whether tinct's type system should support a parameterized
`Dict[K V]` type constructor — algebraic type constructors with kind
`Type → Type → Type`. Motivated by the need to type `transitions` in
`stdlib/regex.llt` as `Dict[Int Seq@Int]` (char-code → successor state
ids) rather than the current unparameterized `@Dict` with a runtime
invariant comment.

**The gap:** BAS (`doc/whatif/boolean-algebraic-subtyping.md`) encodes
multi-field records as intersections of single-field types and handles
union/intersection over specific named fields — but cannot express "all
values in this dict are of type T" because that requires universal
quantification over field labels (∀f. {f: T}), which is outside BAS's
scope. The `transitions` and `groups` dicts in `NfaState`/`NfaDict`
(lib-regex.md) are the concrete cases that remain untyped.

**Questions for the research phase:**

- [ ] Survey how comparable languages type parameterized maps: Haskell
  `Map k v`, TypeScript `Record<K, V>`, Nickel's contract-based approach,
  CUE's structural constraints (`{[string]: int}`). Which model fits
  tinct's use cases?
- [ ] Can BAS accommodate a `Dict[K V]` constructor as a primitive
  type constructor (not derived from records)? What interaction does
  `Dict[K V]` have with union/intersection (`Dict[Int Str] | Dict[Str Int]`)?
- [ ] Is `Dict[K V]` the right primitive, or should tinct distinguish
  between structural records (field names known statically) and dynamic
  maps (keys are runtime values)? The current `Dict` conflates both.
- [ ] Identify all stdlib functions whose type signatures benefit from
  `Dict[K V]`: `transitions` in regex NFA, `groups` in NFA, the `stat`
  return dict, `tls-peer-cert` result, `list-dir` entry dict.
- [ ] Write a `doc/whatif/parameterized-dict.md` proposal.

**Depends on:** BAS adoption (`doc/whatif/boolean-algebraic-subtyping.md`),
since the interaction between `Dict[K V]` and union/intersection types
requires the full BAS constraint solver to be sound.

## Standard Library

### `typecheck-cap-awareness`: Type checker cap and include awareness

Three type checker gaps found via `tinct run --strict samples/versions.llt`.

**1. Cap-qualified include: `[include cap "path"]`**

`collect_include_paths` (src/imports.rs:192) only handles `args.len() == 1` with a literal string. The 2-arg form `[include %libdir "strings.llt"]` is skipped, so functions from included files appear undefined. The cap-qualified form is the only currently allowed form.

- [ ] Extract `find_libdir_path()` from `src/main.rs` into `src/lib.rs` so `src/imports.rs` can call it (`src/main.rs`, `src/lib.rs`)
- [ ] `collect_include_paths_from_expr`: handle `args.len() == 2` where arg[0] is a `VarRef` and arg[1] is a string literal; collect `(span, cap_name, path)` (`src/imports.rs`)
- [ ] `resolve_includes`: accept `libdir: Option<&Path>`; when cap name is `%libdir`, resolve path relative to libdir; skip other cap var names silently (`src/imports.rs`)
- [ ] `build_type_env`: call `find_libdir_path()` and pass result to `resolve_includes` (`src/imports.rs`)
- [ ] Remove 1-arg `[include "path"]` handling from `collect_include_paths_from_expr`; emit a type warning suggesting the cap-qualified form (`src/imports.rs`)

**2. Seed TypeEnv with always-injected cap types**

`%pwd`, `%libdir`, `%stdin` are always in the root env but unknown to the type checker.

- [ ] Add `%pwd: DirCap`, `%libdir: DirCap`, `%stdin: Handle` to the TypeEnv in `build_type_env` (or `build_prelude_env`) before include resolution (`src/imports.rs`, `src/type_env.rs`)
- [ ] Ensure `DirCap`, `NetCap` are registered as known type names in `TypeEnv::with_builtins()` if not already present (`src/type_env.rs`)

**3. `caps:` pragma on `---` header**

Scripts that require runtime-injected caps (e.g., `%nc @NetCap`) have no way to declare this. The type checker sees those variables as undefined. The `---` header already supports `expects:` and `%name@Type` pragmas via the same dispatch mechanism (`Token::Identifier(s) if s == "expects"` in `src/parser.rs:3053`). Adding `caps:` is a small extension to the existing infrastructure:

```tinct
--- caps: [%nc: @NetCap  %data: @DirCap  %store: @DirCap]
[emit [str ...]]
```

The value is a tinct dict where keys are cap names and values are type annotations. Supports any number of caps and any mix of `@NetCap`, `@DirCap`, `@Handle`, etc. Error messages include the flag to use:
- missing `@NetCap` → `"%nc (NetCap) required — pass --cap-net nc=ENTRY"`
- missing `@DirCap` → `"%data (DirCap) required — pass --cap-fs data=PATH"`

Fits naturally with existing `--- %name@Type expects: Type` header syntax — same dispatch mechanism, same dict-of-annotations pattern.

- [ ] AST: add `caps: Option<Spanned<Vec<(String, Annotation)>>>` field to `Document` struct (alongside existing `name` and `expects`) (`src/ast.rs`)
- [ ] Parser: recognize `caps:` pragma key in `---` header dispatch (same pattern as `expects:` at `src/parser.rs:3053`); parse the value as a dict where each entry is `%name: @Type` — collect into `Vec<(cap_name, annotation)>` (`src/parser.rs`)
- [ ] Type checker: when `caps:` is present, extend TypeEnv with each `(cap_name, type)` pair before type-checking the document body; handle both `DirCap` and `NetCap` type names (`src/typecheck.rs`, `src/imports.rs`)
- [ ] Runtime: on document eval, validate each declared cap is present in root env with the declared type; emit a structured error per missing cap with: (a) which variable is missing, (b) its declared type, (c) the exact CLI flag to fix it — derive the flag from the type and strip the `%` prefix for the name:
  ```
  error: %nc@NetCap is required but not provided
    inject it with:  tinct run --cap-net nc=HOST:PORT ...
    or unrestricted: tinct run --cap-net nc=any ...
  ```
  ```
  error: %data@DirCap is required but not provided
    inject it with:  tinct run --cap-fs data=PATH ...
  ```
  Special-case auto-injected caps (`%pwd`, `%libdir`, `%stdin`): if these are declared but missing, suggest the suppression flag they may have hit:
  ```
  error: %pwd@DirCap is required but not provided
    note: %pwd is injected automatically — did you pass --no-pwd?
  ```
  (`src/eval.rs` or `src/main.rs`)
- [ ] Update `samples/versions.llt`: add `--- caps: [%nc: @NetCap]` as the document header; remove the manual comment block listing cap names (`samples/versions.llt`)
- [ ] Corpus tests: single NetCap declared + present; single DirCap declared + present; mixed NetCap + DirCap declared + both present; cap missing → helpful error with correct CLI flag suggestion; multiple caps missing → one error per missing cap (`tests/corpus/eval/caps/`)

