# What If: Runtime Reflection — Annotations as Value Metadata

**State:** Accepted — 2026-05-14

What would it take to make tinct values carry their full annotation metadata at runtime, enabling inspection, documentation generation, and round-trip source reconstruction?

## Design

### Core Principle

Every function value carries its complete annotation metadata alongside its body and closure. `@[doc: "..."]`, `@[return: Type]`, `@[constraint: ...]`, and parameter annotations are not discarded after type-checking — they are stored as a metadata dict on the function value and remain accessible at runtime.

```tinct
add: [fn@[doc: "Add two numbers" return: Int] [a@Int b@Int] [+ a b]]

[describe add]
# → [doc:    "Add two numbers"
#    return: "Int"
#    params: [[name: "a"  annotation: "Int"] [name: "b"  annotation: "Int"]]
#    sig:    "fn@Int [a@Int b@Int]"]
```

### `ast-of : Any → Dict`

The single Rust primitive. Returns the AST dict for any value:

- **`Value::Function`** (stores its source AST): the full `Expr::Fn` structure — annotation, params, and body — via `ast_to_dict`.
- **Other values**: a minimal structural description — `[type: "int"  value: 42]`, `[type: "builtin"  name: "map"]`, etc.

`ast-of` is the only Rust primitive this design requires. Everything else is tinct.

**Annotation representation:** `ast-of` uses the existing `ast_to_dict` schema from `src/ast_dict.rs` — the same schema already used by the formatters and quasiquoting. Annotations have two shapes:

```tinct
# Annotation::Simple — e.g. fn@Int, param@Str, @Bool
[type: "annotation"  kind: "simple"  value: "Int"]

# Annotation::PropertyDict — named entries, e.g. fn@[return: Int  doc: "..."]
[type: "annotation"  kind: "dict"  entries: [
  [type: "entry"  key: "return"  value: "Int"]
  [type: "entry"  key: "doc"     value: "Add two numbers"]]]

# Annotation::PropertyDict — positional entries (union annotation), e.g. @[Ok Str]
[type: "annotation"  kind: "dict"  entries: [
  [type: "entry"  key: []  value: "Ok"]
  [type: "entry"  key: []  value: "Str"]]]

# Annotation entry values that are compound expressions
# (e.g. @[constraint: [a: Numeric]] or @[return: Seq@Int])
# are recursively serialized as full AST dicts rather than debug strings.
# This is the one fix applied to annotation_to_thunk_id in ast_dict.rs:
#   before: _ => "<expr at N:M>"
#   after:  _ => ast_to_dict_expr(value_expr)
```

There is no `.src` field — formatters walk `kind`, `entries`, and `value` to produce their own string. The existing formatters already handle this schema.

```tinct
[f-ast: [ast-of add]]
# → [type:       "fn"
#    return-ann: [type: "annotation"  kind: "simple"  value: "Int"]
#    params:     [[name: "a"  annotation: [type: "annotation"  kind: "simple"  value: "Int"]]
#                 [name: "b"  annotation: [type: "annotation"  kind: "simple"  value: "Int"]]]
#    body:       [...ast dict...]]

# caller's choice of formatter — include and call .format:
[compact-fmt: [include %libdir "formatter/compact.llt"]]
[pretty-fmt:  [include %libdir "formatter/pretty.llt"]]
[compact: [compact-fmt.format f-ast]]
[pretty:  [pretty-fmt.format  f-ast]]
```

### `describe` — pure tinct in `stdlib/prelude.llt`

Because `ast-of` uses the existing `ast_to_dict` schema (with the recursive-value fix), `describe` and its helpers can be written entirely in tinct using only primitives already in prelude:

```tinct
annotation-of:   [fn [val] [get "return-ann" [ast-of val]]]
source-of:       [fn [val] [get "body"        [ast-of val]]]

# Stringify an annotation entry's value.
# Simple values (Str, Int) are already plain tinct values.
# Compound values (e.g. [a: Numeric] as a nested dict) are full AST dicts —
# use llt-repr as a fallback; a caller needing pretty output passes it to a formatter.
annotation-value-str: [fn [v]
  [if [str? v] v [llt-repr v]]]

# Stringify a full annotation dict (kind: "simple" or "dict").
# Null-safe: returns "" for absent/null annotations.
annotation-to-str: [fn [ann]
  [if [null? ann]
    ""
    [match ann.kind
      "simple": ann.value
      "dict":
        [positional: [filter [fn [e] [null? e.key]] ann.entries]]
        [if [not [empty? positional]]
          # All positional entries → union annotation like @[Ok Str]
          [str "[" [join " " [map [fn [e] [annotation-value-str e.value]] positional]] "]"]
          # Named entries → metadata dict (return:, doc:, constraint:, ...)
          # Extract the return type value for display
          [ret: [find-first-or [fn [e] [= "return" e.key]] null ann.entries]
            [if [null? ret] "" [annotation-value-str ret.value]]]]
      _: ""]]]

# Build "fn@RetType [a@Int  b@Str]" from an ast-of result.
# Note: function metadata dicts use "return-ann" key, not "annotation".
sig-from-ast: [fn [ast]
  [ret:    [annotation-to-str [get-or ast "return-ann" null]]]
  [params: [join "  " [map [fn [p]
              [if [has? p "annotation"]
                [str p.name "@" [annotation-to-str p.annotation]]
                p.name]]
            ast.params]]]
  [if [= ret ""]
    [str "fn [" params "]"]
    [str "fn@" ret " [" params "]"]]]

describe: [fn [val]
  [ast: [ast-of val]]
  [if [= ast.type "fn"]
    [doc:       [ann: [get-or ast "return-ann" null]
                  [if [null? ann] ""
                    [if [= ann.kind "dict"]
                      [d: [find-first-or [fn [e] [= "doc" e.key]] null ann.entries]
                        [if [null? d] "" d.value]]
                      ""]]]
     return-ann: [get-or ast "return-ann" null]   # full annotation dict for introspection
     params:     ast.params
     sig:        [sig-from-ast ast]]
    [type: [type-of val]]]]
```

**What's needed for pure-tinct `describe`:**
1. `ast-of` Rust primitive — the only Rust piece required
2. The one-line fix in `ast_dict.rs` — annotation entry values now recursively serialize instead of falling back to `"<expr at N:M>"` (already applied)
3. Everything else (`match`, `get-or`, `join`, `map`, `str`, `has?`, `find-first-or`, `null?`) is already in prelude

### `ast-of` in `rust::meta`

`ast-of` is added to the `rust::meta` primitive group, imported by prelude via `[include %rust "meta"]`, and available to any stdlib file that needs it.

---

## What Would Change

### `src/value.rs` — `Value::Function` carries `FnAnnotation`

`Value::Function` already stores `params: Rc<Vec<Param>>` where each `Param` carries `annotation: Option<Spanned<Annotation>>` — these survive to runtime and `ast-of` reads them directly. `FnAnnotation` stores only the data that is NOT already on the params or body: the function-level annotation and definition location.

```rust
pub struct FnAnnotation {
    pub doc: Option<String>,              // extracted from fn@[doc: "..."] at eval_fn time
    pub return_ann: Option<Annotation>,   // the fn-level Annotation (for return type, constraints)
    pub constraints: Vec<Constraint>,     // Vec<Constraint> — reuses the existing enum from types.rs
                                          // covers Class { class, var } (single-var form) and HasField { label, dict_var, field_var }
                                          // Note: Constraint::Class is single-var until the MPTC sprint lands;
                                          // multi-parameter constraints are not yet representable here
    pub source_file: Option<PathBuf>,     // file path — Span alone carries no file identity
    pub source_span: Span,               // always available at eval_fn time; non-optional
}
```

Wrapped as `Option<Box<FnAnnotation>>` on `Value::Function` for zero overhead on unannotated functions:
```rust
pub annotation: Option<Box<FnAnnotation>>,
```

`doc` is extracted from `return_ann` at function creation in `eval_fn`: if `return_ann` is `PropertyDict` and has a `"doc"` entry, copy its string value into `doc`. `source_span` is always `expr.span`. `source_file` is threaded from the include context.

`constraints` is extracted from `return_ann` similarly — walk the PropertyDict entries looking for a `"constraint"` key; its value is itself a PropertyDict like `[a: Numeric  b: Equatable]` which maps directly to `vec![Constraint::Class { class: "Numeric", var: "a" }, ...]`. This requires a small `annotation_to_constraints(ann: &Annotation) -> Vec<Constraint>` helper in `eval.rs` — no type-checker involvement; it's purely structural annotation parsing.

### `src/builtins_meta.rs` — `ast-of`

`ast-of` constructs the result dict using the existing `ast_to_dict` schema:

```
[type:       "fn"
 return-ann: annotation_to_thunk_id(fn.annotation.return_ann)   # existing schema
 params:     [[name: p.name  annotation: annotation_to_thunk_id(p.annotation)] ...]
 body:       ast_to_dict_expr(fn.body)                           # eagerly serialized
]
```

`body:` is serialized via `ast_to_dict_expr` when the thunk is Materialized. For Unevaluated thunks, `ast-of` returns the expression AST directly without forcing — the `body:` field comes from the stored `Expr`, not from evaluation. This makes `ast-of` non-materializing: it branches on thunk state (Materialized / Unevaluated / Pending) rather than forcing first.

**Design evolution:** The original design used eager serialization. The current implementation avoids this by branching on thunk state — Unevaluated thunks return their expression tree directly via `ast_to_dict_expr`, eliminating the complexity concern. No new thunk variants are required; the non-materializing design branches on existing thunk states (Materialized / Unevaluated / Pending).

For `Value::Builtin`: `ast-of` uses a shared static lookup table `builtin_type_for(name) → TypeScheme` extracted into a new module (e.g. `src/builtin_types.rs`). Both `standard_builtins()` and `TypeEnv::with_builtins()` currently register the same builtin names in parallel — the table de-duplicates this into a single source of truth. `ast-of` calls `builtin_type_for(def.name)` directly with no `EvalContext` change and no eval/typecheck boundary violation. The existing `TypeScheme.doc` field is already present and available for free.

For other values: `[type: type-of(val)]` with a minimal description.

---

## What This Unlocks

**Docgen without string parsing:**
```tinct
# scripts/docgen.llt — core logic, using only prelude primitives:
[process-file: [fn [path]
  [module: [include %libdir [after-first "stdlib/" path]]]
  [filter [fn [e] [not [= "" [get-or [describe e] "doc" ""]]]]
    [each-kv module]]]]
```

**REPL `:describe` command:**
```
tinct> :describe map
map — fn@[f b] [fn@b [a]  [f a]]
  sig:    "fn@[f b] [fn@b [a]  [f a]]"
  params: [[name: "f"  annotation: [kind: "simple"  value: "Fn"]]
            [name: "xs" annotation: [kind: "simple"  value: "Mappable"]]]
  doc:    "Apply f to each element of a Seq or Dict"
```

**LSP — full integration via `FnAnnotation`:**

`FnAnnotation.source_span` carries where the function was *defined* in its source file. When hovering at a *call site*, there are now two meaningful spans — the call site in user code and the definition in the library file. The LSP puts the definition span in `relatedInformation`, giving users a clickable "Defined at `stdlib/io.llt:45`" link in hover without re-running `find_definition` through the include chain.

This also resolves **cross-file go-to-definition** for included modules without extra analysis: the function value already knows where it lives via `source_span`, so the LSP reads it directly instead of re-parsing the included file to locate the binding by name.

`FnAnnotation.doc` provides the doc string for hover without consulting the `DocMap` (which requires a type-checker re-run). For values returned by `[include %libdir "net.llt"]`, the doc string travels with the value through include chains even when the DocMap for that file isn't in the current LSP session's cache.

Parameter names come from `Value::Function.params` (already carry `Param.annotation` at runtime). `ast-of` exposes them in the params array, giving LSP signature help exact source-level names (`cap@DirCap`, `path@Str`) rather than the generated TypeScheme names (`_t0`, `_t1`).

**Completion item enrichment** — completion items for named function bindings currently carry no `detail` or `documentation`. With `describe`, the completion handler enriches each item with the doc string and signature without re-running the type checker.

**Annotated vs inferred type in hover** — `describe` returns what the user *wrote* (the annotation); the type map holds what the type checker *inferred*. For hover on an over-broad annotation, the LSP shows both: "Annotated `@Number` — inferred as `Int`". `describe` supplies the annotation side; the type map supplies the inferred side. This is the `unknown-diagnostics` signal applied to hover.

**Builtin introspection** — `describe` on a builtin currently returns only its name and type tag. Full type signature introspection for builtins requires resolving the open question of how `ast-of` accesses `TypeEnv::with_builtins()` (not yet in `EvalContext`):
```tinct
[describe open]
# → [type: "builtin"  name: "open"]   # current minimal form
# future (once TypeEnv access is resolved):
# → [type: "builtin"  name: "open"  module: "rust::io"
#    return-ann: [kind: "simple"  value: "Handle"]
#    params: [[name: "cap"  annotation: [kind: "simple"  value: "DirCap"]] ...]]
```

**Module hover** — when hovering over `io` in `[io: [include %libdir "io.llt"]]`, the LSP renders a module summary by mapping `describe` over the dict:

```
io — module (17 exports)
  read-file    fn@[Ok Str | Err Str] [DirCap Str]  "Read a file..."
  write-file   fn@Null [DirCap Str Str]              "Write content to a file..."
  ...
```

**Metaprogramming** — `source-of` returns the body AST dict (not a string); pass to a formatter for display:
```tinct
debug-fn: [fn [f args]
  [compact-fmt: [include %libdir "formatter/compact.llt"]]
  [emit [str "calling: " [compact-fmt.format [source-of f]] "\n"]]
  [apply f args]]
```

**Testing helpers:**
```tinct
assert-documented: [fn [f name]
  [if [= "" [get-or [describe f] "doc" ""]]
    [error [str name " is missing a @[doc: ...] annotation"]]
    true]]
```

**Round-trip — two paths:**
```tinct
# In-memory (no file): ast-of → eval-ast
# Works for functions that only close over stdlib names
[add2: [eval-ast [ast-of add]]]
[= [add 1 2] [add2 1 2]]   # → true for pure/stdlib-only functions

# File persistence: format → write → include
[pretty-fmt: [include %libdir "formatter/pretty.llt"]]
[write %doc "add.llt" [pretty-fmt.format [ast-of add]]]
[add3: [include %doc "add.llt"]]
```

---

## Interaction with the Tinct-Hosted Formatters

`ast-of` changes the *input pipeline* for the formatters without changing the formatters themselves. There are multiple formatters (`stdlib/formatter/compact.llt`, `stdlib/formatter/pretty.llt`) — this design doesn't bless any one as canonical. The caller picks whichever they want.

**Current pipeline:** `source text → parse → AST dict → formatter → formatted string`

**New pipeline:** `value (carries stored AST) → ast-of → AST dict → formatter → formatted string`

Since `Value::Function` stores its body AST and `FnAnnotation`, the formatters can format a function that was never written in a source file — one built programmatically by a macro or constructed via `apply fn`. Parsing is no longer a prerequisite for formatting.

**Literate source reconstruction:**
```tinct
[io: [include %libdir "io.llt"]]
[pretty-fmt: [include %libdir "formatter/pretty.llt"]]
[each-kv [fn [name val]
  [emit [str name ": " [pretty-fmt.format [ast-of val]] "\n\n"]]]
  io]
```

---

## `include` Return Type — Modules as Typed Records

The return type of `[include %libdir "io.llt"]` is currently `Unknown`. This should be a closed `Record` of io.llt's exported bindings:

```tinct
[io: [include %libdir "io.llt"]]
# io : Record([
#   read-file:   Fn@[Ok Str | Err Str] [DirCap Str]
#   write-file:  Fn@Null [DirCap Str Str]
#   write-lines: Fn@Null [WriteHandle Seq@Str]
#   ...
# ])
```

With a typed `io`, `io.read-file` gets a precise function type, LSP hover works on it, and `[describe io.read-file]` returns full annotation metadata.

### Why `include` doesn't need to be a special case

`resolve_includes` in `src/imports.rs` already type-checks every included file and accumulates its bindings into the calling file's `TypeEnv`. The fix is a post-pass in `build_type_env`: after `resolve_includes` runs, walk the AST for include call expressions with known literal paths, construct `Record([name: type ...])` from the bindings contributed by that path, and store it as the inferred type of that expression in the type map. No change to `infer_expr`, no new inference rule.

**Implementation challenge:** `resolve_includes` currently merges all included bindings into a flat `TypeEnv` with no per-file attribution — there is no existing mechanism to know which bindings came from which specific include call. The post-pass needs `resolve_includes` to additionally return a `HashMap<Span, Vec<(String, Type)>>` mapping each include call's span to the bindings it contributed. This is new data but the information is available during resolution. Include calls with computed paths (non-literal) remain `Unknown`. Cycles are handled by the existing visited-set guard.

### Interaction with runtime reflection

Once `include` returns a typed Record, enumerating a module's documented exports is a one-liner:

```tinct
[filter [fn [e] [not [= "" [get-or [describe e] "doc" ""]]]] [each-kv io]]
```

---

## Interaction with `%rust` Virtual Modules

`describe` on a builtin returns its type registration:

```tinct
[describe open]
# → [sig: "fn@Handle [DirCap Str Str]"  type: "builtin"  module: "rust::io"]
```

---

## Stdlib Reorganization — CLI Pipelines vs Libraries

As part of this feature, the stdlib should be reorganized to cleanly separate **CLI pipeline entry points** (files run by `tinct run`, `tinct fmt`, `-i`/`-o` flags) from **importable libraries** (files included by user code via `[include %libdir "..."]`).

### Proposed layout

```
stdlib/
  cli/
    in/
      json.llt          # ← was stdlib/in/json.llt
      toml-lite.llt     # ← was stdlib/in/toml-lite.llt
    out/
      json.llt          # ← was stdlib/out/json.llt
      json-pretty.llt   # ← was stdlib/out/json-pretty.llt
      llt.llt           # ← was stdlib/out/llt.llt
      raw.llt           # ← was stdlib/out/raw.llt
      yaml.llt          # ← was stdlib/out/yaml.llt
      csv.llt           # ← was stdlib/out/csv.llt
      toml.llt          # ← was stdlib/out/toml.llt
      env.llt           # ← was stdlib/out/env.llt
    fmt/
      compact.llt       # new: thin pipeline wrapper → formatter/compact.llt
      pretty.llt        # new: thin pipeline wrapper → formatter/pretty.llt
  formatter/
    compact.llt         # importable library; remove [emit [format %]] pipeline
    pretty.llt          # importable library; remove [emit [format %]] pipeline
  prelude.llt
  io.llt
  net.llt
  math.llt
  ... (all other libraries unchanged)
```

### Pipeline thin wrappers (`stdlib/cli/fmt/*.llt`)

```tinct
# stdlib/cli/fmt/compact.llt
[fmt: [include %libdir "formatter/compact.llt"]]
[emit [fmt.format %]]
```

### Library usage from tinct code

```tinct
# User code includes from formatter/ (the library), not cli/
[compact-fmt: [include %libdir "formatter/compact.llt"]]
[compact-fmt.format [ast-of f]]
```

### What changes in Rust

- `src/main.rs:886` — `libdir_path.join("in")` → `libdir_path.join("cli").join("in")`
- `src/main.rs:909` — `libdir_path.join("out")` → `libdir_path.join("cli").join("out")`
- `src/main.rs:2074` — `p.join("out").join("json.llt")` → `p.join("cli").join("out").join("json.llt")`
- `src/formatter.rs` — `include_str!("../stdlib/formatter/compact.llt")` stays (library); `tinct fmt` pipeline moves to `cli/fmt/`
- `src/lib.rs:890` comment updates

### What changes in docs

- `doc/12-tooling.md` — update convention text and path examples throughout (`stdlib/in/` → `stdlib/cli/in/`, etc.)

### Runtime-reflection examples

All `[include %libdir "formatter/compact.llt"]` references in this whatif are correct for the proposed layout — `formatter/` remains the library path, unchanged.

## Prerequisites

- `Value::Function` AST body already stored (done)
- `Value::Function.params: Rc<Vec<Param>>` already carries param annotations (done) — `ast-of` reads them directly; no `param_annotations` field on `FnAnnotation`
- Formatters already handle all `Expr` variants (done)
- `annotation_to_thunk_id` recursive-value fix applied (done — `ast_dict.rs`)
- Needs: `FnAnnotation { doc, return_ann, constraints: Vec<Constraint>, source_file, source_span }` on `Value::Function`, `ast-of` Rust primitive, `describe`/`sig-from-ast`/`annotation-to-str`/`annotation-of`/`source-of` as tinct functions in prelude

**Note on dynamic typing:** `ast-of` returns `Unknown` from the type checker's perspective — field accesses like `ast.kind`, `ann.entries` are on an `Unknown`-typed value and cannot be statically verified. The reflection layer is inherently dynamically typed. This is intentional and consistent with how reflection works in other languages (Python `inspect`, Common Lisp `describe`). Tinct's gradual typing allows this: `@Unknown` opts out of checking for the reflection helpers.

**Open questions before implementation:**
- ~~Builtin introspection~~ **Resolved:** extract a shared `builtin_type_for(name)` static table into a new module; both `standard_builtins()` and `TypeEnv::with_builtins()` read from it; `ast-of` calls it directly — no EvalContext change, no duplication.
- ~~`source_file` threading~~ **Resolved:** add `current_file: Option<PathBuf>` to `EvalConfig`; repurpose the existing `with_base_dir_and_path` stub at `src/eval.rs:255` (currently ignores its `_base_dir_path` arg — verified) to set this field; change `builtin_include` at `src/builtins_meta.rs:1152` from `ctx.with_base_dir(included_dir)` to `ctx.with_base_dir_and_path(included_dir, Some(file_path))` — the call site must change, not just the stub; `eval_fn` reads `ctx.config.current_file.clone()`. This information exists in the include context but must be threaded into `EvalContext`.
- ~~Round-trip eval~~ **Resolved:** `eval-ast` already exists (`src/builtins_meta.rs:377`, registered at `src/builtins.rs:1093`); it takes an AST dict and evaluates it in `stdlib_env`. In-memory round-trip: `[eval-ast [ast-of f]]`. File persistence: `[format-pretty [ast-of f]]` written to a `DirCap` file, then `[include %doc "file.llt"]` to read back. No `eval-llt` string-eval primitive needed. Caveat: `eval-ast` evaluates in `stdlib_env`, so free variables beyond stdlib won't resolve — correct behavior, since functions worth serializing are pure/stdlib-only.

## References

- Clojure metadata system (`with-meta`, `meta`) — arbitrary metadata on any value; forms the basis of Clojure's docstring system, arglists metadata, and tooling integration (Rich Hickey, 2007)
- Common Lisp `describe` and `documentation` — built-in reflection on symbols, functions, classes; `(describe #'map)` prints type, args, docstring
- Elixir `Module.docs/2` — doc strings as first-class module attributes, accessible at runtime via `Code.fetch_docs/1`; powers `h(Function)` in IEx (interactive shell)
- Python `inspect` module — `inspect.signature(f)`, `inspect.getsource(f)`, `f.__doc__` — full runtime introspection of function annotations and source; `ast.unparse` for round-trip source reconstruction
- Sheard, T. & Peyton Jones, S. (2002). "Template Haskell." *Haskell Workshop*, pp. 1-16. — [staged metaprogramming in a typed functional language; `ast-of` + `eval-ast` is a single-stage runtime analogue of TH's splice/quote mechanism]
