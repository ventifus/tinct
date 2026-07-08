# Runtime Reflection

> **Supersedes:**
>
> - `doc/08-evaluation.md §Runtime Reflection` — spec-level description; this document is the user-facing guide
> - `doc/11a-builtins.md §Meta & Code Generation` — `ast-of` and `eval-ast` entries added there; `str` updated to note Castable dispatch

Tinct functions carry their full annotation metadata at runtime. The `ast-of` primitive returns a structured dict describing any value **without forcing it** — it peeks at the thunk's state and branches accordingly. The prelude provides helpers for inspection, signature display, and documentation generation.

## `ast-of` — Get the AST Dict for Any Value

`ast-of` returns a structured dict describing the value. The schema matches the AST dict format used by the tinct-hosted formatter (see `doc/15-ast.md §AST Dict Schema`).

**For functions:**

```tinct
add: [fn@[doc: "Add two numbers" return: Int] [a@Int b@Int] [+ a b]]

[ast-of add]
# → [type:       "fn"
#    return-ann: [type: "annotation"  kind: "simple"  value: "Int"]
#    params:     [[name: "a"  annotation: [type: "annotation"  kind: "simple"  value: "Int"]]
#                 [name: "b"  annotation: [type: "annotation"  kind: "simple"  value: "Int"]]]
#    body:       [type: "call"  ...]]
```

The `return-ann` dict has two shapes depending on the annotation form:

```tinct
# Simple annotation: fn@Int, param@Str, @Bool
[type: "annotation"  kind: "simple"  value: "Int"]

# Property dict annotation: fn@[return: Int  doc: "..."]
[type: "annotation"  kind: "dict"  entries: [
  [type: "entry"  key: "return"  value: "Int"]
  [type: "entry"  key: "doc"     value: "Add two numbers"]]]

# Union annotation: @[Ok Str]
[type: "annotation"  kind: "dict"  entries: [
  [type: "entry"  key: []  value: "Ok"]
  [type: "entry"  key: []  value: "Str"]]]
```

**For builtins:**

```tinct
[ast-of map]
# → [type: "builtin"  name: "map"  module: "rust::seq"
#    return-ann: [type: "annotation"  kind: "simple"  value: "Seq"]
#    params: [[name: "f"  annotation: ...]
#             [name: "xs" annotation: ...]]]
```

**For other values:**

```tinct
[ast-of 42]        # → [type: "int"]
[ast-of "hello"]   # → [type: "str"]
[ast-of [1 2 3]]   # → [type: "seq"]
```

**For unevaluated thunks** (expressions not yet forced):

```tinct
# A binding whose value hasn't been accessed yet
lazy-fn: [fn@[doc: "Compute something"] [x@Int] [+ x 1]]

[ast-of lazy-fn]
# → [type: "fn"  return-ann: ...  params: [...]  doc: "Compute something"]
# The function body is NOT called — ast-of reads the expression tree directly.
```

This is the key non-materializing property: `ast-of` inspects the thunk's state without forcing it. For an unevaluated thunk wrapping a `fn` expression, the result contains the annotation (including `doc:`) from the expression tree. No side effects are triggered.

**Note:** `ast-of` on an unevaluated thunk returns the expression AST without forcing. This enables safe introspection of module bindings that may trigger side effects (e.g., stdin reading, network calls).

**For pending thunks** (deferred calls like pipeline stage results):

```tinct
# [from-json [slurp %stdin]] is a PendingBuiltin — not yet evaluated
stage-result: [include %libdir "cli/in/json.llt"]

[ast-of stage-result]
# → [type: "pending"]
# %stdin is NOT read. The thunk is not forced.
```

This allows docgen and other introspection tools to detect pipeline stage files (non-module includes) without triggering their I/O.

`ast-of` is dynamically typed — its return type is `Unknown`. Field access on the result is not statically checked.

## `describe` — Structured Metadata for Any Value

`describe` returns a metadata dict for a function or a type tag for other values:

```tinct
add: [fn@[doc: "Add two numbers" return: Int] [a@Int b@Int] [+ a b]]

[describe add]
# → [doc:        "Add two numbers"
#    return-ann: [type: "annotation"  kind: "simple"  value: "Int"]
#    params:     [[name: "a"  annotation: [...]] [name: "b"  annotation: [...]]]
#    sig:        "fn@Int [a@Int  b@Int]"]

[describe 42]
# → [type: "int"]
```

The `sig` field is a human-readable signature string built from the annotation, matching what the user wrote.

## `sig-from-ast` — Signature String from AST Dict

`sig-from-ast` builds a `"fn@RetType [params]"` string from an `ast-of` result:

```tinct
[sig-from-ast [ast-of add]]   # → "fn@Int [a@Int  b@Int]"

[sig-from-ast [ast-of [fn [x y] [+ x y]]]]   # → "fn [x  y]"
```

## `annotation-of` and `source-of`

`annotation-of` returns the function-level annotation dict (the `return-ann` field):

```tinct
ann: [annotation-of add]   # → [type: "annotation"  kind: "simple"  value: "Int"]
```

`source-of` returns the body AST dict:

```tinct
body: [source-of add]   # → [type: "call"  fn: "+"  args: [...]]
```

For non-functions, both return `null`.

## Formatting AST Dicts

`ast-of` produces a structured dict, not a string. To render it, pass it to a formatter:

```tinct
[compact-fmt: [include %libdir "formatter/compact.llt"]]
[pretty-fmt:  [include %libdir "formatter/pretty.llt"]]

[compact-fmt.format [ast-of add]]   # → "fn@Int [a@Int b@Int] [+ a b]"
[pretty-fmt.format  [ast-of add]]   # → multi-line formatted source
```

## Round-Trip Evaluation

Functions defined in terms of stdlib names can be round-tripped through `eval-ast`:

```tinct
add2: [eval-ast [ast-of add]]
[= [add 1 2] [add2 1 2]]   # → true
```

`eval-ast` evaluates the AST dict in the stdlib environment. Functions that close over local bindings beyond the stdlib cannot be round-tripped this way — the free variables won't resolve.

For file persistence:

```tinct
[pretty-fmt: [include %libdir "formatter/pretty.llt"]]
[write %doc "add.llt" [pretty-fmt.format [ast-of add]]]
[add3: [include %doc "add.llt"]]
```

## Docgen

Generating documentation for a module is a one-liner with `describe` and `each-kv`:

```tinct
[io: [include %libdir "io.llt"]]

[documented: [filter [fn [e] [not [= "" [get-or "doc" "" [describe e]]]]]
                     [each-kv io]]]
# → all entries in io that have a @[doc: "..."] annotation
```

Building a full doc string per export:

```tinct
[format-entry: [fn [name val]
  [d: [describe val]]
  [if [= d.type "fn"]
    [str name "\n  " d.sig "\n  " d.doc "\n"]
    [str name "  (" [type-of val] ")\n"]]]]

[each-kv [fn [name val] [emit [format-entry name val]]] io]
```

## Testing with `assert-documented`

```tinct
assert-documented: [fn [f name]
  [if [= "" [get-or "doc" "" [describe f]]]
    [error [str name " is missing a @[doc: ...] annotation"]]
    true]]

[assert-documented add "add"]   # ✓ — has doc string
[assert-documented [fn [x] x] "anonymous"]  # ✗ — error
```

## LSP Integration

When hovering over a function call, the LSP shows:

- The type-inferred signature (from the type map)
- The annotated doc string (from `FnAnnotation.doc`, available without a type-checker re-run)
- A "Defined at `file.llt:line`" link (from `FnAnnotation.source_span`)
- Exact parameter names from the source (from `FnAnnotation` params, not generated `_t0` names)

Completion items for named function bindings include the doc string and signature.

## Reflection on Included Modules

`[include %libdir "io.llt"]` returns a typed `Record` of io.llt's exported bindings. Each exported function has a precise type and carries its full `FnAnnotation`. `describe` on any field works as expected:

```tinct
[io: [include %libdir "io.llt"]]
[describe io.read-file]
# → [sig:  "fn@[Ok Str | Err Str] [DirCap Str]"
#    doc:  "Read a file and return its contents, or an error string."
#    params: [[name: "cap"  annotation: ...]
#             [name: "path" annotation: ...]]]
```
