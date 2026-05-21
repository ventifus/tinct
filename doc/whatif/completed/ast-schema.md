# What If: Canonical AST Dict Schema for tinct

**State:** Accepted — 2026-05-05

What would it take to define a stable, versioned mapping from tinct's `Expr` AST to tinct dicts — the shared data language for the formatter, quasiquoting, and macros?

## Current State

tinct's AST (`src/ast.rs`) is a Rust enum hierarchy (`Expr`, `Entry`, `Param`, `Annotation`, `Document`, `File`). Three separate proposals need to represent this AST as tinct data:

- **`doc/whatif/quasiquoting.md`** — `[quote expr]` produces an AST dict; partial schema sketched
- **`doc/whatif/macros.md`** — macros transform AST dict → AST dict; defers to quasiquoting for schema; names `ast_to_dict` / `dict_to_ast`
- **`doc/whatif/tinct-hosted-formatter.md`** — `tinct fmt` renders source from an AST dict

Each proposal referenced the schema independently. Without a canonical definition, they drift. This document is the single source of truth.

### What's Missing

1. A complete mapping for every `Expr` variant, including `DotAccess`, `Pipe`, `TypeAlias`, `TypeAssert`, `Annotated`, `Rest`
2. A schema for supporting types: `Entry`, `Param`, `NamedArg`, `Annotation`, `Document`, `File`
3. A policy for optional fields (absent vs null), spans, comments, and bare-word detection
4. Defined Rust functions `ast_to_dict` and `dict_to_ast` with their full signatures
5. A versioning strategy so schema changes don't silently break macros or formatters

## Why a Canonical Schema Matters

**No drift.** Three consumers share one definition. Adding a new `Expr` variant updates one file; all three features see the change.

**Formatter as validation.** The tinct-hosted formatter is the most demanding consumer — it must round-trip every AST node back to parseable source. If the schema can support the formatter, it supports everything.

**Macros are safe.** `dict_to_ast` validates that a macro's output is a well-formed AST node before the evaluator sees it. The schema is the contract.

**Quasiquoting is ergonomic.** `[quote [+ x 1]]` produces a dict matching this schema. Users can pattern-match on `node.type`, walk `node.args`, and build new nodes manually — all using ordinary tinct operations.

## Design

### Conventions

- **`type:` string discriminator** on every node — `[type: "call" ...]`, `[type: "var" ...]`. Follows the tagged-union convention used by `try` results.
- **`[]` for absent optionals** — whenever a field is optional and absent, its value is `[]` (null/empty dict). Never omit the key — consistent field presence simplifies macro code.
- **`span:` on every node** — `[start: [line: 1 col: 5 offset: 4] end: [line: 1 col: 12 offset: 11]]`. Macro-generated nodes carry the macro call-site span. The formatter uses spans for error attribution.
- **Comments embedded on entries and documents** — `leading-comments:` is a `Seq` of comment strings (without the `#` prefix) that appeared before the node in source. `trailing-comment:` is a single string for an inline comment on the same line. Both absent (not `[]`) when empty — the only exception to the no-omit rule, since every node would otherwise carry empty comment fields.
- **Blank lines embedded on entries** — `blank-before: true` when a blank line appeared before this entry in source; `blank-before: false` otherwise. This preserves intentional visual grouping of related entries through formatter round-trips.
- **`schema-version: 1`** on the root `File` node — bump on breaking changes.
- **Access-pipeline:** `BracketAccess` and `RangeAccess` are absent from this schema — they are removed by the accepted `access-pipeline` whatif (phase 2). `DotAccess` field is string OR int. `Pipe` is present (desugar pass converts it to a call before evaluation, but the formatter sees it).

### Literals

```tinct
[type: "literal"  kind: "int"    value: 42     span: ...]
[type: "literal"  kind: "float"  value: 3.14   span: ...]
[type: "literal"  kind: "bool"   value: true   span: ...]
[type: "literal"  kind: "str"    value: "hello"  bare: false  span: ...]
```

`bare: true` on string literals means the source was written as a bare word (`hello`, not `"hello"`). Populated by `ast_to_dict` when `include_source_info: true` (formatter mode). Defaults to `false` for macro-generated nodes — generated code always uses quoted strings.

### Variable References

```tinct
[type: "var"  name: "x"  span: ...]
```

### Access and Pipeline

```tinct
# field is a String for identifier keys (config.host) or Int for integer keys (list.0)
# — access-pipeline accepted whatif; check [str? node.field] or [int? node.field]
[type: "dot-access"  target: ...node...  field: "name"  span: ...]
[type: "dot-access"  target: ...node...  field: 0       span: ...]

# Pipe — desugar-only operator; present in the formatter's AST but
# converted to a Call by the desugar pass before evaluation
[type: "pipe"  lhs: ...node...  rhs: ...node...  span: ...]
```

`BracketAccess` (`data["key"]`) and `RangeAccess` (`data[0..5]`) are not in this schema — they are removed by the accepted `access-pipeline` whatif (phase 2). Use `[get key data]` and `slice` instead.

### Dict (Data Constructor)

```tinct
[type: "dict"  entries: [...]  span: ...]
```

Each entry in `entries` is itself a dict:

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
[type: "call"
 fn: ...node...
 args: [...]           # positional args, each a node
 named-args: [         # key: value pairs at the call site
   [name: "timeout"  value: ...node...]
   ...
 ]
 implied: true         # true if written [f x] (no `call` keyword); false if [call f x]
 span: ...]
```

### Function

```tinct
[type: "fn"
 params: [
   [name: "x"  annotation: ...ann-or-null...  variadic: false]
   ...
 ]
 return-ann: ...ann-or-null...
 body: ...node...
 desugared: false    # true if created by _ underscore desugaring
 span: ...]
```

### Type Forms

```tinct
[type: "type-alias"   expr: ...node...  span: ...]

[type: "type-assert"  annotation: ...ann...  expr: ...node...  span: ...]

[type: "annotated"    name: "Fn"  annotation: ...ann...  span: ...]

[type: "rest"  name: "r"  span: ...]   # ...r (named row variable)
[type: "rest"  name: []   span: ...]   # ... (anonymous rest)
```

### Annotations

Annotations appear as values of `annotation:` fields. They are NOT top-level nodes (they cannot appear as dict values in user code directly):

```tinct
# Simple annotation: @Number
[type: "annotation"  kind: "simple"  value: "Number"]

# Property dict annotation: @[type: Number default: 30]
[type: "annotation"  kind: "dict"  entries: [...]]
```

Entries in an annotation dict follow the same `[type: "entry" key: ... value: ...]` shape as regular dict entries.

### Document

```tinct
[type: "document"
 expressions: [...]   # Seq of nodes forming the scope chain
 name: "config"       # [] if anonymous
 output-type: ...ann-or-null...
 expects: ...ann-or-null...
 leading-comments: ["# File header comment"]  # absent when empty
 span: ...]
```

`leading-comments:` on a document captures comments that appear at the top of the document section (before the first expression), such as file-level documentation headers.

### File (Root)

```tinct
[type: "file"
 documents: [...]   # Seq of document nodes
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
pub fn ast_to_dict(file: &File, opts: AstToDictOpts) -> Value

pub struct AstToDictOpts<'a> {
    /// Source text — enables `bare:` flag on string literals.
    /// None → bare is always false (safe default for generated code).
    pub source: Option<&'a str>,
    /// Comment map from ParseOutput — enables leading-comments, trailing-comment,
    /// and blank-before fields on Entry and Document nodes.
    /// None → no comment fields emitted (compact formatter, quasiquoting).
    pub comments: Option<&'a HashMap<usize, Vec<String>>>,
}
```

Serializes the full AST to a tinct `Value::Dict` matching the schema above.

- `source: Some(src)` — enables `bare:` flag on `[type: "literal" kind: "str"]` nodes (compares token span first character against source to detect bare words)
- `comments: Some(map)` — embeds `leading-comments:`, `trailing-comment:`, and `blank-before:` on `Entry` and `Document` nodes by matching token offsets to AST spans
- Both `None` — minimal mode (quasiquoting, compact formatter): clean schema with no comment or source-info fields

#### `ast_to_dict_expr`

```rust
pub fn ast_to_dict_expr(expr: &Expr, opts: AstToDictOpts) -> Value
```

Serializes a **single expression** node to a tinct dict. Called by:

- The `[quote expr]` evaluator: quotes the subexpression without producing a `File` root node
- `ast_to_dict` internally: called per-expression while walking `Document.expressions`

`ast_to_dict` wraps this by walking the `File` → `Document` → `expressions` hierarchy and producing `[type: "file" ...]` and `[type: "document" ...]` wrappers. Quasiquoting only needs the inner expression dict, not the file wrapper.

#### `dict_to_ast`

```rust
pub fn dict_to_ast(v: &Value) -> Result<Expr, AstError>
```

Validates and converts a tinct dict back to an `Expr`. Used by:

- The macro evaluator: macro output `Value` → evaluatable `Expr`
- Future: `[eval-ast node]` builtin for runtime code generation

Validation rules:

- `type:` key must be a known string
- Required fields must be present and of the right shape
- `span:` is optional — absent nodes get a synthetic zero span
- Unknown fields are ignored (forward-compatible)

`dict_to_ast` is deliberately permissive on unknown fields so macros written against an older schema continue working after new fields are added.

---

## What Would Change

### `src/ast.rs` / new `src/ast_dict.rs`

**Current:** No serialization of `Expr` to `Value`.

**Proposed:** New file `src/ast_dict.rs` implementing `ast_to_dict` and `dict_to_ast`. No changes to the `Expr` enum itself.

**Impact:** Moderate — complete coverage of every `Expr` variant plus supporting types. No changes to existing code.

### `doc/whatif/quasiquoting.md`

**Current:** Contains a partial schema table and a declaration that spans should be included.

**Proposed:** Schema section replaced with: "See `doc/whatif/ast-schema.md` for the canonical AST dict schema." The quasiquoting proposal retains its `quote`/`unquote` mechanism design but defers schema to this document.

**Impact:** Minor — editorial update only.

### `doc/whatif/macros.md`

**Current:** References `ast_to_dict` / `dict_to_ast` by name; defers to quasiquoting for schema.

**Proposed:** Updated reference to this document. No design changes.

**Impact:** Minor — editorial update only.

### `doc/whatif/tinct-hosted-formatter.md`

**Current:** Proposal; references the schema.

**Proposed:** Uses `ast_to_dict(source: Some(...), comments: Some(...))` for the full formatter and `ast_to_dict(None, None)` for compact modes.

**Impact:** None — this document enables tinct-hosted-formatter rather than requiring changes to it.

## Phased Adoption

### Phase 1: Schema + `ast_to_dict` (minimal mode)

Implement `ast_to_dict(None, None)` — no source info, no comments. This unblocks:

- `tinct fmt --oneline` / `--nospaces` / `--minimize` (compact formatter in tinct)
- `[quote expr]` (quasiquoting Phase 1)

- `src/ast_dict.rs`: `ast_to_dict` covering all `Expr` variants, `Entry`, `Param`, `Annotation`, `Document`, `File`; `schema-version: 1` on root
- Tests: every `Expr` variant round-trips through `ast_to_dict`; schema version present; span included on every node

### Phase 2: Source Info + Comments

Add `source: Some(...)` and `comments: Some(...)` support to `ast_to_dict`. This unblocks:

- `tinct fmt` full formatter (width-based layout decisions in tinct, comment preservation)

- `src/ast_dict.rs`: `bare:` flag via source span comparison; `leading-comments:` embedding from `leading_comments` map
- Tests: `bare: true` for bare-word strings; comments round-trip through `ast_to_dict`

### Phase 3: `dict_to_ast`

Implement `dict_to_ast`. This unblocks:

- `[defmacro]` (macros Phase 1)
- `[eval-ast node]` builtin (runtime code generation)

- `src/ast_dict.rs`: `dict_to_ast` with validation; synthetic spans for absent `span:`; unknown fields ignored
- Tests: every known `type:` value round-trips; unknown fields preserved through round-trip; invalid schema produces `AstError` with field path

### Prerequisites

None. Phase 1 has no dependencies — `ast_to_dict` is a pure serialization function over the existing `Expr` enum.

### Trigger

Phase 1: When any of the following accepts: `tinct-hosted-formatter.md` (compact modes need the schema), `quasiquoting.md` (Phase 1 needs `ast_to_dict`).

Phase 3: When `macros.md` accepts.

## References

- tinct `src/ast.rs` — the `Expr` enum this schema maps; `Entry`, `Param`, `Annotation`, `Document`, `File`
- tinct `doc/whatif/quasiquoting.md` — partial schema origin; `quote`/`unquote` mechanism
- tinct `doc/whatif/macros.md` — `ast_to_dict`/`dict_to_ast` function names; macro use case
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
