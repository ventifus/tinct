# Canonical AST Dict Schema

## Overview

The AST dict schema is the stable, versioned mapping from tinct's `Expr` AST to tinct dicts. It is the shared data language for the formatter, quasiquoting, and macros.

Three consumers share one definition. Adding a new `Expr` variant updates one file; all three features see the change.

**Formatter as validation.** The tinct-hosted formatter is the most demanding consumer — it must round-trip every AST node back to parseable source. If the schema supports the formatter, it supports everything.

**Macros are safe.** `dict_to_ast` validates that a macro's output is a well-formed AST node before the evaluator sees it. The schema is the contract.

**Quasiquoting is ergonomic.** `[quote [+ x 1]]` produces a Variant matching this schema. Users can pattern-match on Variant tags, walk `node.args`, and build new nodes manually — all using ordinary tinct operations.

`src/ast_dict.rs` implements `ast_to_dict` in minimal mode, with source info and comment support via `ast-dict-source`, and `dict_to_ast` via the `dict-to-ast` sprint.

## Design

### Conventions

- **`Value::Variant` tag on Expr nodes** — `Variant("Call", {fn: ..., args: ...})`, `Variant("VarRef", {name: "x", span: ...})`. Tags are PascalCase and match the `Expr` enum variant names. Structural nodes (Entry, Annotation, Pattern, Document, File) remain plain dicts with a `type:` string discriminator.
- **`[]` for absent optionals** — whenever a field is optional and absent, its value is `[]` (null/empty dict). Never omit the key — consistent field presence simplifies macro code.
- **`span:` on every node** — `[start: [line: 1 col: 5 offset: 4] end: [line: 1 col: 12 offset: 11]]`. Macro-generated nodes carry the macro call-site span. The formatter uses spans for error attribution.
- **Comments embedded on entries and documents** — `leading-comments:` is a `Seq` of comment strings (without the `#` prefix) that appeared before the node in source. `trailing-comment:` is a single string for an inline comment on the same line. Both absent (not `[]`) when empty — the only exception to the no-omit rule, since every node would otherwise carry empty comment fields.
- **Blank lines embedded on entries** — `blank-before: true` when a blank line appeared before this entry in source; `blank-before: false` otherwise. This preserves intentional visual grouping of related entries through formatter round-trips.
- **`schema-version: 1`** on the root `File` node — bump on breaking changes.
- **Access-pipeline:** `BracketAccess` and `RangeAccess` are absent from this schema — they are removed by the `access-pipeline` whatif (phase 2). `DotAccess` field is string OR int. `Pipe` is present (desugar pass converts it to a call before evaluation, but the formatter sees it).

### Literals

```tinct
Variant("Literal"  {kind: "int"    value: 42     span: ...})
Variant("Literal"  {kind: "float"  value: 3.14   span: ...})
Variant("Literal"  {kind: "bool"   value: true   span: ...})
Variant("Literal"  {kind: "str"    value: "hello"  bare: false  span: ...})
```

`bare: true` on string literals means the source was written as a bare word (`hello`, not `"hello"`). Populated by `ast_to_dict` when `include_source_info: true` (formatter mode). Defaults to `false` for macro-generated nodes — generated code always uses quoted strings.

### Variable References

```tinct
Variant("VarRef"  {name: "x"  span: ...})
```

### Access and Pipeline

```tinct
# field is a String for identifier keys (config.host) or Int for integer keys (list.0)
# — access-pipeline accepted whatif; check [str? node.field] or [int? node.field]
Variant("DotAccess"  {target: ...node...  field: "name"  span: ...})
Variant("DotAccess"  {target: ...node...  field: 0       span: ...})

# Pipe — desugar-only operator; present in the formatter's AST but
# converted to a Call by the desugar pass before evaluation
Variant("Pipe"  {lhs: ...node...  rhs: ...node...  span: ...})
```

`BracketAccess` (`data["key"]`) and `RangeAccess` (`data[0..5]`) are not in this schema — they are removed by the `access-pipeline` whatif (phase 2). Use `[get key data]` and `slice` instead.

### Dict (Data Constructor)

```tinct
Variant("Dict"  {entries: [...]  span: ...})
```

Each entry in `entries` is itself a plain dict (not a Variant):

```tinct
# Auto-indexed entry (no explicit key)
[type: "entry"
 key: []
 value: ...node...
 blank-before: false
 leading-comments: ["# first comment" "# second comment"]
 trailing-comment: "# inline comment after value"]

# Keyed entry — same fields, key is non-null
[type: "entry"
 key: ...node...
 value: ...node...
 blank-before: true       # there was a blank line before this entry in source
 leading-comments: []     # absent when empty — not [] — so the formatter
                          # doesn't emit a leading-comments field at all
 trailing-comment: []]    # same — absent when no trailing comment
```

**Comment fields on entries:**
- `leading-comments:` — a `Seq` of comment strings (without `#`) for comments that appeared on lines before this entry. **Absent** (field omitted) when there are no leading comments.
- `trailing-comment:` — a single string for an inline comment on the same line as the entry (e.g., `port: 8080  # default`). **Absent** when none.
- `blank-before: true` — a blank line appeared before this entry in source. The formatter uses this to preserve the user's intentional visual grouping. `false` when no blank line preceded the entry.

### Call

```tinct
Variant("Call"  {
 fn: ...node...
 args: [...]           # positional args, each a node
 named-args: [         # key: value pairs at the call site
   [name: "timeout"  value: ...node...]
   ...
 ]
 implied: true         # true if written [f x] (no `call` keyword); false if [call f x]
 span: ...})
```

### Function

```tinct
Variant("Fn"  {
 params: [
   [name: "x"  annotation: ...ann-or-null...  variadic: false]
   ...
 ]
 return-ann: ...ann-or-null...
 body: ...node...
 desugared: false    # true if created by _ underscore desugaring
 span: ...})
```

### Type Forms

```tinct
Variant("TypeAlias"   {expr: ...node...  span: ...})

Variant("TypeAssert"  {annotation: ...ann...  expr: ...node...  span: ...})

Variant("Annotated"   {name: "Fn"  annotation: ...ann...  span: ...})

Variant("Rest"  {name: "r"  span: ...})   # ...r (named row variable)
Variant("Rest"  {name: []   span: ...})   # ... (anonymous rest)
```

### Sequential

```tinct
Variant("Sequential"  {exprs: [...]  span: ...})
```

`exprs` is a `Seq` of Expr Variant nodes. Written as `expr1; expr2; expr3` in source — semicolons separate expressions in the same scope.

### Quoting and Splicing

```tinct
# [quote expr] — freeze an expression as data
Variant("Quote"          {expr: ...node...  span: ...})

# [unquote expr] — escape back to evaluation inside a quote
Variant("Unquote"        {expr: ...node...  span: ...})

# [unquote-splice expr] — splice a single node into a quasiquote position
Variant("UnquoteSplice"  {expr: ...node...  span: ...})

# [splice form1 form2 ...] — internal multi-form splice produced by macro expansion
Variant("Splice"  {forms: [...]  span: ...})
```

`forms` in `Splice` is a `Seq` of Expr Variant nodes. `Splice` is an internal AST node produced by macro expansion; it does not appear in user-written source.

### Macros

```tinct
# [defmacro name params body] — legacy macro definition
Variant("DefMacro"  {name: "mymacro"  params: ...node...  body: ...node...  span: ...})

# [macro name params body] — new-style macro definition
Variant("MacroDecl"  {name: "mymacro"  params: ...node...  body: ...node...  span: ...})

# [syntax-class Name pattern: pat] — declare a syntax class for macro argument validation
# message: is absent (not []) when no error message was specified
Variant("SyntaxClass"  {name: "MyClass"  pattern: ...node...  span: ...})
Variant("SyntaxClass"  {name: "MyClass"  pattern: ...node...  message: "expected X"  span: ...})
```

`params` in `DefMacro` and `MacroDecl` is an Expr node (the parameter pattern expression, typically a `LetDecl`).

### Pattern Matching

```tinct
# [match scrutinee pat1: body1 pat2: body2 ...]
Variant("Match"  {
  scrutinee: ...node...
  arms: [
    [pattern: ...pattern-dict...  body: ...node...]
    [pattern: ...pattern-dict...  guard: ...node...  body: ...node...]  # guard is absent when none
    ...
  ]
  span: ...})

# [let pat1 pat2 ...] — parameter pattern declaration (used inside fn/macro)
Variant("LetDecl"  {bindings: [...]  span: ...})

# [pattern pat1 pat2 ...] — pattern bindings in instance/class arms
Variant("PatternDecl"  {bindings: [...]  span: ...})

# [pat: body] — a single case arm (used inside macro expansion)
Variant("CaseArm"  {pattern: ...node...  body: ...node...  span: ...})
```

Each `arms` element in `Match` is a plain dict (not Variant). `pattern` inside each arm is a plain pattern dict (see pattern dict format below). `guard` is absent when no guard is specified — not `[]`.

### Type Classes

```tinct
# [class [let ClassName params...] method: TypeExpr ...]
# determines: and resolver: are absent when not specified
Variant("ClassDecl"  {
  name: "MyClass"
  params: [...]              # Seq of string parameter names
  methods: [...]             # dict of method-name → type-expr node
  determines: [...]          # absent when empty
  resolver: ...node...       # absent when no resolver
  injective: true            # absent when false
  span: ...})

# [instance ClassName [pattern ...]: [method: val ...] ...]
Variant("InstanceDecl"  {
  class: "MyClass"
  arms: [
    [pattern: ...node...  methods: [...]]
    ...
  ]
  span: ...})

# @[Func Arg] — type application
Variant("TypeApp"  {func: ...node...  arg: ...node...  span: ...})
```

`methods` in `ClassDecl` is a plain dict (not a Seq) keyed by method name. `params` in `ClassDecl` is a Seq of string values.

### Placeholder and Error

```tinct
# _ — the wildcard/hole expression
Variant("Placeholder"  {span: ...})

# Parse error node — carries its own span
# Tag is "AstError" (NOT "Error") to avoid collision with Result.Error
# dict_to_ast accepts both "AstError" and "error" as the type discriminator
Variant("AstError"  {span: ...})
```

`Placeholder` has an empty payload dict — only `span:` when span tracking is enabled.

### Annotations

Annotations appear as values of `annotation:` fields. They are NOT top-level nodes (they cannot appear as dict values in user code directly). Annotations are emitted as plain dicts with a `type:` string discriminator (not Variant):

```tinct
# Simple annotation: @Number
[type: "annotation"  kind: "simple"  value: "Number"]

# Property dict annotation: @[type: Number default: 30]
[type: "annotation"  kind: "dict"  entries: [...]]
```

Entries in an annotation dict follow the same `[type: "entry" key: ... value: ...]` shape as regular dict entries.

### Document

Document nodes are plain dicts (not Variant):

```tinct
[type: "document"
 expressions: [...]   # Seq of Variant nodes forming the scope chain
 name: "config"       # [] if anonymous
 output-type: ...ann-or-null...
 expects: ...ann-or-null...
 leading-comments: ["# File header comment"]  # absent when empty
 span: ...]
```

`leading-comments:` on a document captures comments that appear at the top of the document section (before the first expression), such as file-level documentation headers.

### File (Root)

The root File node is a plain dict (not Variant):

```tinct
[type: "file"
 documents: [...]   # Seq of document plain dicts
 schema-version: 1
 span: ...]
```

### Span Representation

```tinct
[start: [line: 1  col: 5  offset: 4]
 end:   [line: 1  col: 12  offset: 11]]
```

All positions are 1-based for line and column, 0-based for byte offset. Macro-generated nodes carry the macro call-site span so errors point to the macro invocation, not synthetic zero spans (Pombrio & Krishnamurthi 2014 resugaring principle).

---

### The Two Rust Functions

#### `ast_to_dict`

```rust
pub fn ast_to_dict(
    file: &File,
    opts: &AstToDictOpts,
    ctx: &Rc<EvalContext>,
) -> EvalResult<Rc<Thunk>>

pub struct AstToDictOpts<'a> {
    /// Source text — enables `bare:` flag on string literals.
    /// None → bare is always false (safe default for generated code).
    pub source: Option<&'a str>,
    /// Comment maps from ParseOutput — enables leading-comments, trailing-comment,
    /// and blank-before fields on Entry and Document nodes.
    /// None → no comment fields emitted (compact formatter, quasiquoting).
    pub comments: Option<CommentMaps<'a>>,
}
```

Serializes the full AST to a tinct value matching the schema above. Expr nodes are emitted as `Value::Variant`; structural nodes (File, Document, Entry, Annotation, Pattern) are emitted as plain `Value::Dict` with a `type:` string discriminator.

- `source: Some(src)` — enables `bare:` flag on `Variant("Literal", {kind: "str", ...})` nodes (compares token span first character against source to detect bare words)
- `comments: Some(map)` — embeds `leading-comments:`, `trailing-comment:`, and `blank-before:` on `Entry` and `Document` nodes by matching token offsets to AST spans
- Both `None` — minimal mode (quasiquoting, compact formatter): clean schema with no comment or source-info fields

#### `ast_to_dict_expr`

```rust
pub fn ast_to_dict_expr(
    expr: &Spanned<Expr>,
    opts: &AstToDictOpts,
    ctx: &Rc<EvalContext>,
) -> EvalResult<Rc<Thunk>>
```

Serializes a **single expression** node to a tinct dict. Called by:
- The `[quote expr]` evaluator: quotes the subexpression without producing a `File` root node
- `ast_to_dict` internally: called per-expression while walking `Document.expressions`

`ast_to_dict` wraps this by walking the `File` → `Document` → `expressions` hierarchy and producing `[type: "file" ...]` and `[type: "document" ...]` wrappers. Quasiquoting only needs the inner expression dict, not the file wrapper.

#### `dict_to_ast`

```rust
pub fn dict_to_ast(
    val: &Value,
    ctx: &Rc<EvalContext>,
) -> Result<Spanned<Expr>, AstError>
```

Validates and converts a tinct dict back to an `Expr`. Used by:
- The macro evaluator: macro output `Value` → evaluatable `Expr`
- `[eval-ast node]` builtin for runtime code generation

Validation rules:
- Accepts both `Value::Variant` (new format, e.g. `Variant("Call", {...})`) and legacy plain dicts with a `type:` string discriminator (e.g. `[type: "call" ...]`) for backward compatibility
- Required fields must be present and of the right shape
- `span:` is optional — absent nodes get a synthetic zero span
- Unknown fields are ignored (forward-compatible)

`dict_to_ast` is deliberately permissive on unknown fields so macros written against an older schema continue working after new fields are added.

---

## Implementation

### `src/ast_dict.rs`

Implements `ast_to_dict`, `ast_to_dict_expr`, and `dict_to_ast`. No changes to the `Expr` enum itself. `ast_to_dict` supports minimal mode (`None, None`) and full mode with source info and comments (via `ast-dict-source`). `dict_to_ast` is implemented via the `dict-to-ast` sprint.

### `doc/feature/macros.md`

References `ast_to_dict` / `dict_to_ast` by name; defers to this document for the canonical schema.

### `doc/whatif/tinct-hosted-formatter.md`

Uses `ast_to_dict(source: Some(...), comments: Some(...))` for the full formatter and `ast_to_dict(None, None)` for compact modes.

## References

- tinct `src/ast.rs` — the `Expr` enum this schema maps; `Entry`, `Param`, `Annotation`, `Document`, `File`
- tinct `doc/whatif/quasiquoting.md` — partial schema origin; `quote`/`unquote` mechanism
- tinct `doc/feature/macros.md` — `ast_to_dict`/`dict_to_ast` function names; macro use case
- tinct `doc/whatif/tinct-hosted-formatter.md` — formatter use case; requires both minimal and full modes
- Pombrio, J. & Krishnamurthi, S. (2014). "Resugaring: lifting
  evaluation sequences through syntactic sugar." In *PLDI '14*,
  pp. 361–371. ACM. — Establishes the principle that desugared code
  should be traceable back to surface syntax. Motivates span attribution
  on macro-generated nodes.
- Pombrio, J. & Krishnamurthi, S. (2015). "Hygienic resugaring of
  compositional desugaring." In *ICFP '15*, pp. 75–87. ACM. — Extends
  resugaring to handle nested/compositional desugaring. Macro-generated
  node spans should carry the expansion call-site span, not zero spans.
