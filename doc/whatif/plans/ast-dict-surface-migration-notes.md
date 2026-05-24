# ast-dict Surface Migration Notes

Planning document for `rv2-rewrite-ast-dict` sprint.
Surveyed: `src/ast_dict.rs` (full file, ~2200 lines), `src/ast.rs` (SurfaceExpression, SurfaceDeclaration, SurfaceItem, SurfaceDocument).

The current implementation bridges through `Expr`/`File` via `ast_convert.rs`. This document maps
every `Expr::*` variant in `expr_to_thunk_id` to its `SurfaceExpression::*` equivalent, identifies
declarations that moved to `SurfaceDeclaration`, and calls out gaps in the reverse direction
(`dict_to_ast_from_dict`).

---

## Section 1: Expr → SurfaceExpression Mapping Table

Each row covers one Expr variant handled in `expr_to_thunk_id` (the ast→dict forward path).
The "Variant Tag" is the string stored as the Variant's tag in the emitted value.
The "Dict Keys" are the fields inside the payload dict (every node also gets `span`).

### Group A: Direct Mappings (Expr → SurfaceExpression, 1:1)

These Expr variants have exact SurfaceExpression equivalents with identical structure.
Porting is mechanical: swap field types (`Box<Spanned<Expr>>` → `Arc<SurfaceNode>`,
`Rc<Spanned<Expr>>` → `Arc<SurfaceNode>`).

| Expr variant | SurfaceExpression variant | Variant Tag | Dict Keys |
|---|---|---|---|
| `Expr::Int(i64)` | `SurfaceExpression::Int(i64)` | `"Literal"` | `kind: "int"`, `value: Int` |
| `Expr::Float(f64)` | `SurfaceExpression::Float(f64)` | `"Literal"` | `kind: "float"`, `value: Float` |
| `Expr::Bool(bool)` | `SurfaceExpression::Bool(bool)` | `"Literal"` | `kind: "bool"`, `value: Bool` |
| `Expr::Str(String)` | `SurfaceExpression::Str(String)` | `"Literal"` | `kind: "str"`, `value: Str`, `bare: Bool` |
| `Expr::VarRef { name, .. }` | `SurfaceExpression::VarRef { name, escaped }` | `"VarRef"` | `name: Str` |
| `Expr::DotAccess { expr, field }` | `SurfaceExpression::DotAccess { expr, field }` | `"DotAccess"` | `target: ExprDict`, `field: Str\|Int` |
| `Expr::Pipe { lhs, rhs }` | `SurfaceExpression::Pipe { lhs, rhs }` | `"Pipe"` | `lhs: ExprDict`, `rhs: ExprDict` |
| `Expr::Sequential(exprs)` | `SurfaceExpression::Sequential(Vec<Arc<SurfaceNode>>)` | `"Sequential"` | `exprs: List` |
| `Expr::Dict(entries)` | `SurfaceExpression::Dict(Vec<Spanned<SurfaceEntry>>)` | `"Dict"` | `entries: List` |
| `Expr::Call { func, args, named_args, implied }` | `SurfaceExpression::Call { func, args, named_args, implied }` | `"Call"` | `fn: ExprDict`, `args: List`, `named-args: List`, `implied: Bool` |
| `Expr::Fn { return_ann, params, body, desugared }` | `SurfaceExpression::Fn { return_ann, params, body, desugared }` | `"Fn"` | `params: List`, `return-ann: Ann\|[]`, `body: ExprDict`, `desugared: Bool` |
| `Expr::TypeAssert { annotation, expr, .. }` | `SurfaceExpression::TypeAssert { annotation, expr }` | `"TypeAssert"` | `annotation: AnnDict`, `expr: ExprDict` |
| `Expr::Annotated { name, annotation }` | `SurfaceExpression::Annotated { name, annotation }` | `"Annotated"` | `name: Str`, `annotation: AnnDict` |
| `Expr::Rest(Option<String>)` | `SurfaceExpression::Rest(Option<String>)` | `"Rest"` | `name: Str\|[]` |
| `Expr::Quote(inner)` | `SurfaceExpression::Quote(Arc<SurfaceNode>)` | `"Quote"` | `expr: ExprDict` |
| `Expr::Unquote(inner)` | `SurfaceExpression::Unquote(Arc<SurfaceNode>)` | `"Unquote"` | `expr: ExprDict` |
| `Expr::UnquoteSplice(inner)` | `SurfaceExpression::UnquoteSplice(Arc<SurfaceNode>)` | `"UnquoteSplice"` | `expr: ExprDict` |
| `Expr::PatternDecl { bindings }` | `SurfaceExpression::PatternDecl { bindings }` | `"PatternDecl"` | `bindings: List` |
| `Expr::LetDecl { bindings }` | `SurfaceExpression::LetDecl { bindings }` | `"LetDecl"` | `bindings: List` |
| `Expr::CaseArm { pattern, body }` | `SurfaceExpression::CaseArm { pattern, body }` | `"CaseArm"` | `pattern: ExprDict`, `body: ExprDict` |
| `Expr::TypeApp { func, arg }` | `SurfaceExpression::TypeApp { func, arg }` | `"TypeApp"` | `func: ExprDict`, `arg: ExprDict` |
| `Expr::Placeholder` | `SurfaceExpression::Placeholder` | `"Placeholder"` | (none) |
| `Expr::Error(Span)` | `SurfaceExpression::Error(Span)` | `"AstError"` | `span: SpanDict` |
| `Expr::Match { scrutinee, arms }` | `SurfaceExpression::Match { scrutinee, arms }` | `"Match"` | `scrutinee: ExprDict`, `arms: List` |

**VarRef note:** `Expr::VarRef` has a `resolved: RefCell<Option<Option<(u32, u32)>>>` field (inline
resolution cache). `SurfaceExpression::VarRef` has no such field — resolution lives in
`ResolutionTable` keyed by `NodeId`. The emitted dict only carries `name:` so this is invisible at
the schema level. No schema change needed.

**Arm structure note:** `Expr::Match` arms use `MatchArm { pattern: Spanned<Pattern>, guard: Option<Box<Spanned<Expr>>>, body: Box<Spanned<Expr>> }`. `SurfaceExpression::Match` uses `SurfaceMatchArm { pattern: Spanned<Pattern>, guard: Option<Arc<SurfaceNode>>, body: Arc<SurfaceNode> }`. Same schema — `pattern_to_thunk_id` is shared, `guard` field is optional, `body` recurses.

### Group B: Expr Variants Moved to SurfaceDeclaration (No Longer in SurfaceExpression)

In the Surface type system, compile-time-only forms were extracted from `SurfaceExpression` into the
separate `SurfaceDeclaration` enum. They appear as `SurfaceItem::Decl(...)` inside `SurfaceDocument::items`
rather than as expression nodes.

The current `ast_to_dict` handles these because `Expr` keeps them inline. The rewritten version must
handle them as `SurfaceDeclaration` when iterating `SurfaceDocument::items`.

| Expr variant | SurfaceDeclaration variant | Variant Tag | Dict Keys | Notes |
|---|---|---|---|---|
| `Expr::TypeAlias { params, body }` | `SurfaceDeclaration::TypeAlias { params, body }` | `"TypeAlias"` | optional `params: List`, `expr: ExprDict` | No longer in SurfaceExpression |
| `Expr::ClassDecl { name, params, superclasses, methods, determines, resolver, resolver_injective }` | `SurfaceDeclaration::ClassDecl { ... }` | `"ClassDecl"` | `name`, `params`, `methods`, optional `determines`, `resolver`, `injective` | `superclasses` silently dropped today (TODO in code) |
| `Expr::InstanceDecl { class_name, arms }` | `SurfaceDeclaration::InstanceDecl { class_name, arms }` | `"InstanceDecl"` | `class`, `arms` | |
| `Expr::DefMacro { name, params, body }` | `SurfaceDeclaration::DefMacro { name, params, body }` | `"DefMacro"` | `name`, `params`, `body` | |
| `Expr::MacroDecl { name, params, body }` | `SurfaceDeclaration::MacroDecl { name, params, body }` | `"MacroDecl"` | `name`, `params`, `body` | |
| `Expr::SyntaxClass { name, pattern, message }` | `SurfaceDeclaration::SyntaxClass { name, pattern, message }` | `"SyntaxClass"` | `name`, `pattern`, optional `message` | |
| `Expr::Splice(forms)` | `SurfaceDeclaration::Splice(Vec<Arc<SurfaceNode>>)` | `"Splice"` | `forms` | |

**Document iteration note:** The current `document_to_dict` iterates `doc.expressions` (a `Vec<Rc<Spanned<Expr>>>`). The rewritten version must iterate `doc.items` (a `Vec<SurfaceItem>`) and dispatch on `SurfaceItem::Expr(node)` vs `SurfaceItem::Decl(decl)`. The `SurfaceItem::Decl` path calls a new `decl_to_thunk_id` function handling the seven `SurfaceDeclaration` variants.

---

## Section 2: Gaps and Asymmetries

### 2.1: Reverse direction (`dict_to_ast_from_dict`) is incomplete

The following Expr variants are **emitted** (forward: ast→dict) but have **no deserialization case**
in `dict_to_ast_from_dict` (reverse: dict→ast). They were likely added to the emitter after the
reverse was written.

| Variant Tag | Missing from reverse direction |
|---|---|
| `"Match"` | No arm for `"match" \| "Match"` |
| `"ClassDecl"` | No arm for `"class-decl" \| "ClassDecl"` |
| `"InstanceDecl"` | No arm for `"instance-decl" \| "InstanceDecl"` |
| `"PatternDecl"` | No arm for `"pattern-decl" \| "PatternDecl"` |

These are gaps in the **old** `Expr`-based reverse path. When rewriting for Surface types, the new
`dict_to_surface_expr` and `dict_to_surface_decl` functions must handle all tags. The gaps should be
fixed during the rewrite rather than separately.

### 2.2: `SurfaceDeclaration` needs its own serialization path

`SurfaceDeclaration` variants are not `SurfaceExpression` variants, so the main `surface_expr_to_thunk_id`
function cannot handle them. A separate `surface_decl_to_thunk_id` function is needed. The reverse
direction needs a matching `dict_to_surface_decl`. The schema (Variant tags and dict keys) can remain
identical to what the old `Expr`-based emitter produces — no schema migration needed.

### 2.3: `SurfaceEntry` vs `Entry` — minor structural difference

`Entry.key` is `Option<Spanned<Expr>>`, `SurfaceEntry.key` is `Option<Arc<SurfaceNode>>`. The emitted
schema is the same (`key: ExprDict | []`, `value: ExprDict`). The `entry_to_thunk_id` function just
needs to call `surface_node_to_thunk_id` instead of `expr_to_thunk_id` for key and value.

### 2.4: `SurfaceParam` vs `Param` — identical structure

`Param.name: String`, `variadic: bool`, `annotation: Option<Spanned<Annotation>>`. `SurfaceParam` is
identical. The `param_to_thunk_id` function needs no schema changes.

### 2.5: `SurfaceNamedArg` vs `NamedArg` — identical structure

Both have `name: String` and a value node. No schema changes.

### 2.6: `SurfaceMatchArm` vs `MatchArm` — identical schema

Both have `pattern: Spanned<Pattern>`, optional `guard`, and `body`. `pattern_to_thunk_id` operates on
`Pattern` (unchanged between Expr and Surface), so it can be reused directly.

### 2.7: `ClassDecl.superclasses` silently dropped

The current emitter has a `TODO` comment at line 775:
`superclasses: _, // TODO (grammar-doc-polish): ClassDecl.superclasses silently dropped`

When porting to `SurfaceDeclaration::ClassDecl`, the `superclasses: Vec<(String, String)>` field must
still be handled. Decision point for Phase 2: add a `superclasses` key to the schema, or continue
silently dropping. This TODO must be tracked and resolved, not silently carried forward.

---

## Section 3: Recommended Implementation Order (Phase 2)

The rewrite should proceed bottom-up: leaf helpers first, then the expression dispatcher, then the
document and program level.

### Step 1: Rename and adapt leaf helpers (independent)

The following helper functions are entirely independent of `Expr` vs `SurfaceExpression`. They operate
on shared types (`Annotation`, `Pattern`, `Span`, `Param`, `NamedArg`) and need only parameter type changes:

- `span_to_thunk_id(span, ctx)` — no changes needed, `Span` is shared
- `annotation_to_thunk_id(ann, span, ctx)` — no changes needed, `Annotation` is shared
- `pattern_to_thunk_id(pattern, span, ctx)` — no changes needed, `Pattern` is shared
- `param_to_thunk_id(param, span, ctx)` — change `Param` → `SurfaceParam` (same fields)
- `named_arg_to_thunk_id(na, span, ctx)` — change `NamedArg` → `SurfaceNamedArg` (same fields)
- `entry_to_thunk_id(entry, span, opts, ctx)` — change `Entry` → `SurfaceEntry`, recurse via `surface_node_to_thunk_id`
- `list_to_thunk_id(items, span, ctx)` — no changes needed (operates on `ThunkId` iterators)

### Step 2: Write `surface_node_to_thunk_id` (depends on Step 1)

New function replacing `expr_to_thunk_id`. Signature:

```rust
fn surface_node_to_thunk_id(
    node: &Arc<SurfaceNode>,
    opts: &AstToDictOpts,
    ctx: &Arc<EvalContext>,
) -> EvalResult<ThunkId>
```

Match on `node.expr` for all `SurfaceExpression` variants (Group A from Section 1). Call adapted
leaf helpers. Use `node.span` wherever span is needed.

For the capacity pre-computation block: replicate the pattern from `expr_to_thunk_id`'s `capacity`
match, updated for `SurfaceExpression` variants.

### Step 3: Write `surface_decl_to_thunk_id` (depends on Step 1)

New function for `SurfaceDeclaration` variants (Group B from Section 1). Signature:

```rust
fn surface_decl_to_thunk_id(
    decl: &SurfaceDeclaration,
    span: Span,
    opts: &AstToDictOpts,
    ctx: &Arc<EvalContext>,
) -> EvalResult<ThunkId>
```

Handle all seven `SurfaceDeclaration` variants. The Variant tags and payload schemas are identical
to the old Expr-based emitter, so the schema is stable.

Resolve the `ClassDecl.superclasses` TODO: add a `superclasses` key emitting a list of
`[class-name: Str param: Str]` dicts, or document the decision to omit it permanently.

### Step 4: Rewrite `document_to_dict` (depends on Steps 2 and 3)

Change to accept `SurfaceDocument` instead of `Document`. Iterate `doc.items` instead of
`doc.expressions`. Dispatch on `SurfaceItem::Expr(node)` → `surface_node_to_thunk_id` and
`SurfaceItem::Decl(decl)` → `surface_decl_to_thunk_id`.

The `name`, `output_type`, `expects`, `stage`, comment fields in `SurfaceDocument` are identical
in structure to `Document`. No schema changes.

### Step 5: Rewrite `ast_to_dict` (depends on Step 4)

Change to accept `SurfaceProgram` instead of `File`. Iterate `program.documents` instead of
`file.documents`. Remove the `File`-specific span extraction (use `program.documents.first().map(|d| d.span)`).

### Step 6: Write reverse direction (dict→Surface) (depends on Steps 2 and 3)

Write `dict_to_surface_expr` and `dict_to_surface_decl` to replace `dict_to_ast` /
`dict_to_ast_from_dict`. Fix the four missing deserialization cases from Section 2.1:
`"Match"`, `"ClassDecl"`, `"InstanceDecl"`, `"PatternDecl"`.

The new functions return `Arc<SurfaceNode>` instead of `Spanned<Expr>`. They construct
`SurfaceNode { expr, span }` and wrap in `Arc::new(...)`.

### Step 7: Update public bridge functions (depends on Steps 5 and 6)

The existing public bridge functions in the "Surface AST Bridge Functions" section:
- `surface_node_to_dict` — replace with direct `surface_node_to_thunk_id` call, remove `ast_convert` bridge
- `surface_program_to_dict` — replace with direct `ast_to_dict(SurfaceProgram)` call
- `dict_to_surface_node` — replace with direct `dict_to_surface_expr` call
- `dict_to_surface_program` — replace with direct `dict_to_surface_program` call

Delete the old `Expr`-based `expr_to_thunk_id`, `dict_to_ast`, `dict_to_ast_from_dict` functions
once all callers are migrated.

### Step 8: Delete `ast_to_dict_expr` public entry point or update signature (depends on Step 5)

`ast_to_dict_expr` currently takes `&Spanned<Expr>`. Callers use it for quasiquoting. Either:
- Replace with `surface_node_to_dict` (if all callers are already Surface-based)
- Keep but rename to something clearly Surface-oriented

Check callers with `grep -r 'ast_to_dict_expr'` before deciding.

### Step 9: Delete `ast_to_dict` (File-based) once `surface_program_to_dict` covers all callers

---

## Appendix: Schema Quick Reference

All nodes produce `Value::Variant { tag, payload: Some(dict_id) }`. The payload dict always
includes a `span` key. Absent optional fields use empty dict `[]` as sentinel (not `null`).

Lists are encoded as integer-keyed dicts: `{0: item0, 1: item1, ...}`.

The schema is **unchanged** by this migration — only the Rust types being traversed change.
Existing tinct metaprogramming code (formatters, macros) that consumes the dict representation
will continue to work without modification.
