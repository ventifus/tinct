# What If: Native AST Value Types

**State:** Superseded — 2026-05-19 — absorbed into [`runtime-v2.md`](runtime-v2.md)

**Refines:** [`include-decomposition.md`](include-decomposition.md) — replaces the serialized-Dict representation for `load`/`expand`/`eval` with native `Value::AstFile` / `Value::AstDoc` / `Value::AstExpr` variants; the self-hosted pipeline structure is unchanged. Supersedes the representation sections of `include-decomp-eval-primitives`.

**Prerequisite:** [`include-decomposition`](include-decomposition.md) complete (all three sprints: primitives, prelude, review).

## Problem

After include-decomp, the pipeline operates across three parallel representations of the same data:

1. **Rust structs** — `File`, `Document`, `Spanned<Expr>` — used by parser, expander, evaluator, and typechecker
2. **Serialized Dict schema** — produced by `ast_to_dict` / `ast_to_dict_expr`, consumed by `dict_to_file` / `dict_to_ast`; what `load` returns and what `expand` / `eval` receive
3. **`Value::Dict`** at runtime — what tinct code sees when iterating `doc.expressions`

These must stay in sync. Adding a new `Expr` variant requires changes in three places: the Rust enum, the serializer, and the deserializer. The schema uses string-typed `type:` discriminators (`type: "var"`, `type: "call"`) which the type checker cannot reason about — `doc.expressions[0].name` has type `Any`, and match arms over `[get "type" node]` are untyped. Round-trips occur on every include-cache miss:

- `load`: parse → `ast_to_dict` — serializes `File` to Dict
- `expand`: `dict_to_file` → `expand()` → `ast_to_dict` — full round-trip
- `eval`: `dict_to_ast` per expression — deserializes each expression

## The Proposal

Add three new `Value` variants — `Value::AstFile`, `Value::AstDoc`, `Value::AstExpr` — wrapping the existing Rust AST types directly. These become the canonical representation throughout the include pipeline. Declare corresponding tinct nominal types (`AstFile`, `AstDoc`, `AstExpr`) in prelude; the type checker can then reason about match arms and field access on AST values, giving tinct metaprogramming code the same static guarantees it gets from any other nominal type.

`load` returns `Value::AstFile`. `expand` takes and returns `Value::AstFile`. `eval` and `eval-types` take `[Seq AstExpr]`. `ast-of` returns `Value::AstExpr`. Tinct code pattern-matches on `AstExpr` variants using the same `match` form it uses for any nominal type.

`dict_to_file` is never written. `dict_to_ast` is deleted. `ast_to_dict_expr` is retained only for `deep-materialize` and JSON output.

## New Value Variants

```rust
Value::AstFile(Rc<File>)
Value::AstDoc(Rc<Spanned<Document>>)
Value::AstExpr(Rc<Spanned<Expr>>)
```

No `Value::AstSpan` — spans remain plain Dicts. Span allocation is bounded and spans are infrequently accessed in normal pipeline execution; a fourth variant is not worth the cost at this stage.

## Tinct Type Declarations

Added to prelude (not a separate module) because the primitive signatures of `load`, `expand`, `eval`, `eval-types`, and `ast-of` reference these types, and they must be resolvable everywhere those primitives are used without an explicit import.

### Supporting Types

```tinct
AstSpan: [type [AstSpan
  file:       String
  start-line: Int
  start-col:  Int
  end-line:   Int
  end-col:    Int]]

# Dot-access key: named field or integer index
AstDotKey: [type [Ident String] [Index Int]]

# Function parameter — variadic flag distinguishes ...rest from positional/named
AstParam: [type [AstParam
  name:       String
  annotation: AstAnnotation
  variadic:   Bool
  span:       AstSpan]]

AstNamedArg: [type [AstNamedArg
  name:  String
  value: AstExpr
  span:  AstSpan]]

# Dict entry — key is [] for auto-indexed (positional) entries
AstEntry: [type [AstEntry
  key:   AstExpr   # [] when auto-indexed
  value: AstExpr
  span:  AstSpan]]

AstMatchArm: [type [AstMatchArm
  pattern: AstPattern
  guard:   AstExpr    # [] when no guard
  body:    AstExpr
  span:    AstSpan]]

AstAnnotation: [type
  [Simple      value: String]
  [PropertyDict entries: [Seq AstAnnotationEntry]]
  [Annotated   name: String  inner: AstAnnotation]]

AstAnnotationEntry: [type [AstAnnotationEntry
  key:   String
  value: AstAnnotation]]
```

`AstPattern` is a separate union type mapping the `Pattern` enum; its full declaration is deferred to the implementation sprint (see Open Questions).

### `AstExpr`

Variants map 1:1 to `Expr` enum members. Internal fields (`VarRef.resolved`, `TypeAssert.resolved_type`) are not exposed — they are implementation details of the resolution and typecheck passes. Formatter-relevant flags (`VarRef.escaped`, `Call.implied`, `Fn.desugared`) are exposed.

```tinct
AstExpr: [type
  # Literals
  [IntLiteral   value: Int    span: AstSpan]
  [FloatLiteral value: Float  span: AstSpan]
  [BoolLiteral  value: Bool   span: AstSpan]
  [StrLiteral   value: String span: AstSpan]

  # Variables
  # escaped: true when written as $name (pin in patterns), false for bare name (bind)
  [Var  name: String  escaped: Bool  span: AstSpan]

  # Access
  [DotAccess  target: AstExpr  field: AstDotKey  span: AstSpan]

  # Sequencing (multi-expression fn bodies and match arm bodies)
  [Sequential  exprs: [Seq AstExpr]  span: AstSpan]

  # Pipe operator (desugared before evaluation)
  [Pipe  lhs: AstExpr  rhs: AstExpr  span: AstSpan]

  # Dict/list literal — auto-indexed entries have key: []
  [Dict  entries: [Seq AstEntry]  span: AstSpan]

  # Function call
  # implied: true = [f x y], false = [call f x y]
  [Call  fn: AstExpr  args: [Seq AstExpr]  named: [Seq AstNamedArg]  implied: Bool  span: AstSpan]

  # Function definition
  # desugared: true = synthesised by $_ desugaring, not written by user
  [Fn  params: [Seq AstParam]  body: AstExpr  return-ann: AstAnnotation  desugared: Bool  span: AstSpan]

  # Type alias declaration — name comes from the enclosing dict entry key
  [TypeAlias  params: [Seq String]  body: AstExpr  span: AstSpan]

  # Type assertion [@Type expr] — resolved-type not exposed (typechecker internal)
  [TypeAssert  annotation: AstAnnotation  expr: AstExpr  span: AstSpan]

  # Annotated bare word, e.g. Fn@Number
  [Annotated  name: String  annotation: AstAnnotation  span: AstSpan]

  # Row variable / open record marker: ... or ...rest. name: [] when unnamed.
  [Rest  name: String  span: AstSpan]

  # Pattern matching
  [Match  scrutinee: AstExpr  arms: [Seq AstMatchArm]  span: AstSpan]

  # Quasiquoting
  [Quote         expr: AstExpr  span: AstSpan]
  [Unquote       expr: AstExpr  span: AstSpan]
  [UnquoteSplice expr: AstExpr  span: AstSpan]

  # Macros
  [DefMacro   name: String  params: AstExpr  body: AstExpr  span: AstSpan]
  [MacroDecl  name: String  params: AstExpr  body: AstExpr  span: AstSpan]
  [Splice     forms: [Seq AstExpr]           span: AstSpan]
  [SyntaxClass name: String  pattern: AstExpr  message: String  span: AstSpan]

  # Type classes
  [ClassDecl     name: String  params: [Seq String]  superclasses: [Seq AstExpr]
                 methods: [Seq AstEntry]  determines: [Seq AstExpr]
                 resolver: AstExpr  resolver-injective: Bool  span: AstSpan]
  [InstanceDecl  class-name: String  arms: [Seq AstExpr]  span: AstSpan]

  # Higher-kinded type application in annotation positions
  [TypeApp  fn: AstExpr  arg: AstExpr  span: AstSpan]

  # Binding and pattern forms
  [PatternDecl  bindings: [Seq AstExpr]             span: AstSpan]
  [LetDecl      bindings: [Seq AstExpr]             span: AstSpan]
  [CaseArm      pattern: AstExpr  body: AstExpr     span: AstSpan]

  # Placeholder `...` — evaluates to error when forced
  [Placeholder  span: AstSpan]

  # Parse error node — span covers the unparseable region
  [Error  span: AstSpan]]
```

### `AstDoc` and `AstFile`

```tinct
# Document name: either a section name or anonymous
DocName: [type [Named String] [Unnamed]]

AstDoc: [type [AstDoc
  stage:       [type [Runtime] [Type]]
  name:        DocName
  expressions: [Seq AstExpr]
  output-type: AstAnnotation   # [] when absent
  expects:     AstAnnotation]] # [] when absent

AstFile: [type [AstFile
  documents: [Seq AstDoc]]]
```

## Match Dispatch for `Value::AstExpr`

`Value::AstExpr` participates in `match` using the same protocol as `Value::Variant`. When the `match` builtin encounters `Value::AstExpr(e)`, it calls `ast_expr_match_view(e) -> (&'static str, Value::Dict)` to extract the variant tag and a shallow payload dict of the immediate fields. Match arm binding proceeds exactly as for `Value::Variant { tag, payload }`.

The payload dict is materialized per arm — one small allocation containing only the immediate fields of the matched node. Recursive children remain `Value::AstExpr` until accessed. This bounds allocation to O(fields in arm) rather than O(tree size).

`Value::AstDoc` and `Value::AstFile` follow the same protocol: `ast_doc_match_view` and `ast_file_match_view` respectively.

## Field Access on `Value::AstExpr`

Direct field access (`node.name`, `[get "span" node]`) is supported via the same field dispatcher used to build the match payload. The type checker warns that field access on a union type may not be valid for all variants — which is accurate, and better than the `Any` return type the old Dict schema provided. Code requiring typed access should use `match`.

## Changed Primitive Signatures

```tinct
# include-decomp                                   # ast-value-types
load@[Fn [source@String  name: @String] Dict]      load@[Fn [source@String  name: @String] AstFile]
expand@[Fn [ast@Dict] Dict]                        expand@[Fn [ast@AstFile] AstFile]
eval@[Fn [exprs@Dict  %: @Any  env: @Dict] Any]    eval@[Fn [exprs@[Seq AstExpr]  %: @Any  env: @Dict] Any]
eval-types@[Fn [exprs@Dict] Any]                   eval-types@[Fn [exprs@[Seq AstExpr]] Any]
ast-of@[Fn [expr@Any] Dict]                        ast-of@[Fn [expr@Any] AstExpr]
```

## Updated Include-Decomp Tinct Code

The self-hosted pipeline from include-decomp updates to use typed representations. The structure is identical; the type annotations and a few field accesses change.

```tinct
eval-document-runtime: [fn@[return: Dict] [let state doc@AstDoc include-dir]
  [result: [eval
    doc.expressions           # [Seq AstExpr] — passed directly to eval builtin
    %:   state.prev
    env: [merge
           [if [dict? state.prev] state.prev []]
           state.named
           ["%include-dir": include-dir]]]]
  [prev: result
   named: [if [match doc.name [Named n]: true  Unnamed: false]
            [merge state.named [[str "%" [match doc.name [Named n]: n]]: result]]
            state.named]]]

eval-document-pipeline: [fn@[return: Any] [let initial docs@[Seq AstDoc] include-dir]
  [get "prev"
    [reduce
      [fn@[return: Dict] [let state doc@AstDoc]
        [match doc.stage
          Runtime: [eval-document-runtime state doc include-dir]
          Type:    state]]
      [prev: initial  named: []]
      docs]]]

eval-file: [fn@[return: Any] [let ast@AstFile initial include-dir]
  [eval-document-pipeline initial ast.documents include-dir]]

# include, include-evaluate-and-cache, cli-pipeline — structure unchanged,
# load/expand now return AstFile so no other changes needed
```

Note: `doc.name` changes from `String | []` to `DocName` (`[Named String] | Unnamed`), making the optional-name pattern explicit and type-safe.

## Quasiquoting and `eval-ast`

`[quote expr]` returns `Value::AstExpr` (was `Value::Dict`). `eval-ast` — which called `dict_to_ast` then evaluated — is deleted. Its role is now:

```tinct
# Old: [eval-ast some-ast-dict]
# New: [eval [seq some-ast-expr] %: [] env: []]
```

This is one fewer primitive with no expressiveness loss.

## `ast_to_dict_expr` / `dict_to_ast` Fate

- **`dict_to_ast`** — deleted. No longer called anywhere. Was used by `eval-ast`, the macro expander's round-trip, and `builtin_eval_ast` — all replaced.
- **`dict_to_file`** — never written. This whatif supersedes the include-decomp-eval-primitives item that would have introduced it.
- **`ast_to_dict_expr`** — retained, used only by `deep-materialize` on `Value::AstExpr` and the JSON serializer.
- **`ast_to_dict`** (file-level) — retained for JSON output of whole files if needed; review at implementation time whether any call site actually uses it.

## `deep-materialize` and JSON Output

`[deep-materialize ast-expr]` produces a `Value::Variant` tree — not a string-keyed Dict. The discriminator is the variant tag (`Var`, `Call`, etc.), not a `type:` string field. JSON output follows: `{"Var": {"name": "x", "span": {...}}}` rather than `{"type": "var", "name": "x", ...}`.

This is a breaking change to any external tooling consuming AST JSON from tinct. Acceptable given no released users.

## What Would Change

### Add

- `Value::AstFile(Rc<File>)`, `Value::AstDoc(Rc<Spanned<Document>>)`, `Value::AstExpr(Rc<Spanned<Expr>>)` to `src/value.rs`
- `ast_expr_match_view(expr: &Spanned<Expr>) -> (&'static str, Value)` in `src/ast_dict.rs` — called by match evaluator; returns variant tag + shallow payload Dict. Analogues for doc and file.
- `ast_expr_get_field(expr: &Spanned<Expr>, field: &str, ctx: &Rc<EvalContext>) -> Option<Value>` in `src/ast_dict.rs` — called by dot-access and `get`. Analogues for doc and file.
- Prelude tinct type declarations: `AstExpr`, `AstDoc`, `AstFile`, `AstSpan`, `AstDotKey`, `DocName`, `AstAnnotation`, `AstAnnotationEntry`, `AstParam`, `AstNamedArg`, `AstEntry`, `AstMatchArm`, `AstPattern` (and `AstPattern` variants)

### Change

- `load` builtin: parse → wrap in `Value::AstFile`; delete `ast_to_dict` call (`src/builtins_meta.rs`)
- `expand` builtin: unwrap `Value::AstFile` → `&File`; call `expand()`; wrap result in `Value::AstFile`. The `dict_to_file` + `ast_to_dict` round-trip is replaced by direct pointer wrapping. (`src/builtins_meta.rs`, `src/expand.rs`)
- `eval` builtin: iterate `[Seq AstExpr]`; unwrap each `Value::AstExpr` → `Rc<Spanned<Expr>>`; call `crate::eval::eval()`. Delete `dict_to_ast` deserialization. (`src/builtins_meta.rs`)
- `eval-types` builtin: same change as `eval` (`src/builtins_meta.rs`)
- `ast-of` builtin: return `Value::AstExpr` wrapping the argument thunk's `Expr`; was `ast_to_dict_expr` (`src/builtins_meta.rs`)
- Match evaluator: add `Value::AstExpr` arm → `ast_expr_match_view` (`src/eval.rs`)
- Dot-access and bracket-access evaluators: add arms for the three new variants (`src/eval_access.rs`)
- `materialize`: `Value::AstExpr` passes through unchanged (already evaluated)
- `deep-materialize`: `Value::AstExpr` → call `ast_expr_match_view` recursively, build `Value::Variant` tree (`src/eval_deep.rs`)
- JSON serializer: add arms for the three new variants → materialize via match view (`src/eval.rs` or json output module)
- `dict?` / `type-of` / `has?` / `keys` / `values`: add arms; `type-of` returns `"AstExpr"` / `"AstDoc"` / `"AstFile"`; `dict?` returns `false` (these are not plain Dicts)
- Prelude tinct code: `eval-document-pipeline`, `eval-file`, `eval-document-runtime` per §Updated Include-Decomp Tinct Code
- Typechecker: `AstExpr`, `AstDoc`, `AstFile` are declared types in prelude; resolved normally via type declaration. Match arm payload types inferred from the `AstExpr` type declaration.

### Delete

- `dict_to_ast` from `src/ast_dict.rs` entirely
- `dict_to_file` — never written; item removed from `include-decomp-eval-primitives` sprint
- `builtin_eval_ast` from `src/builtins_meta.rs`
- `eval-ast` from `standard_builtins()`
- The `type:` string-keyed Dict schema as a pipeline output format — `deep-materialize` / JSON now emit `Value::Variant` trees

## Open Questions

**Q1 — Match payload allocation:** `ast_expr_match_view` materializes a shallow payload Dict per arm (simple, reuses existing match infrastructure). Alternative: teach the match binder to destructure `Value::AstExpr` fields directly, avoiding even that allocation. Defer; start with payload Dict, profile later.

**Q2 — `AstPattern` type declaration:** The `Pattern` enum (`Wildcard`, `Variable`, `Literal`, `TypeTag`, `Pin`, `Dict`, `Seq`, `Constructor`, `Or`) needs a full `AstPattern` tinct type declaration analogous to `AstExpr`. Deferred to implementation sprint; same approach applies.

**Q3 — `deep-materialize` / JSON output format:** Produce `Value::Variant` tree (nominal, type-checkable downstream) or `Value::Dict` with `type:` string keys (old schema)? Lean toward `Value::Variant` — the Dict schema is superseded and has no external users.

**Q4 — `ast_to_dict` (file-level) fate:** Retained or deleted? Depends on whether any call site needs to materialize a whole `AstFile` to Dict for external consumption. Audit at implementation time; likely delete.

**Q5 — `dict?` on `Value::AstExpr`:** Returns `false` (not a plain Dict) or `true` (structurally Dict-like)? Lean toward `false` — these are nominal types, not Dicts. Code that currently checks `dict?` before accessing AST nodes would use `type-of` or match instead.

**Q6 — Internal fields on `VarRef`:** `resolved: RefCell<Option<Option<(u32, u32)>>>` is write-once data from the variable resolution pass. Not exposed in `AstExpr`. Confirm this is the right call — the resolution pass mutates AST nodes in place, and exposing the resolved coordinates would leak implementation detail into the tinct API.

## Prerequisites

- `include-decomp-primitives` complete (`blake3`, `cap-identity`, `load`, `include-cache-get`, `include-cache-put` registered; groundwork done)
- `include-decomp-eval-primitives` complete (the `dict_to_file`-based `expand` / `eval` / `eval-types` and deletion of `builtin_include` serve as the interim implementation; this whatif supersedes the representation but not the builtin deletion work)
- `include-decomp-prelude` complete (self-hosted pipeline in prelude; this whatif refactors it)

## References

- Abelson, H. & Sussman, G.J. (1996). *Structure and Interpretation of Computer Programs*, 2nd ed. MIT Press. §4.1 "The Metacircular Evaluator." — homoiconic representation of code as data.
- Pombrio, J. & Krishnamurthi, S. (2014). "Hygienic Resugaring of Call-by-Value Evaluation Sequences." *ICFP 2014*. — origin tracking for desugared nodes (`Fn.desugared`).
- Cardelli, L. (1997). "Type Systems." *Handbook of Computer Science and Engineering*. — nominal vs structural types and the value of nominal typing for AST node dispatch.
