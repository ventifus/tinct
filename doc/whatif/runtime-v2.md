# What If: Runtime v2 — AST Redesign, Native Value Types, and Async Parallel Evaluation

**State:** Draft — 2026-05-19

**Supersedes:**
- [`ast-value-types.md`](ast-value-types.md) — fully absorbed
- [`async-eval.md`](async-eval.md) — core async runtime absorbed here; serve/connect layers and networking content extracted to [`lib-net-v3.md`](lib-net-v3.md)

**Refines:** [`include-decomposition.md`](include-decomposition.md) — replaces the serialized-Dict representation for `load`/`expand`/`eval` with native AST value types; the self-hosted pipeline structure is unchanged.

**Prerequisite:** [`include-decomposition`](include-decomposition.md) complete (all three sprints).

---

## Problem

The runtime has three entangled problems that cannot be solved independently.

### Three Parallel AST Representations

After include-decomp, the pipeline operates across three parallel representations of the same data:

1. **Rust structs** — `File`, `Document`, `Spanned<Expr>` — used by parser, expander, evaluator, and typechecker
2. **Serialized Dict schema** — produced by `ast_to_dict` / `ast_to_dict_expr`, consumed by `dict_to_file` / `dict_to_ast`; what `load` returns and what `expand` / `eval` receive
3. **`Value::Dict`** at runtime — what tinct code sees when iterating `doc.expressions`

These must stay in sync. Adding a new `Expr` variant requires changes in three places: the enum, the serializer, and the deserializer. The schema uses string-typed `type:` discriminators (`type: "var"`, `type: "call"`) which the type checker cannot reason about — `doc.expressions[0].name` has type `Any`, and match arms over `[get "type" node]` are untyped. Round-trips occur on every include-cache miss.

### Mutation Embedded in AST Nodes

`VarRef.resolved: RefCell<Option<Option<(u32, u32)>>>` and `TypeAssert.resolved_type: RefCell<Option<Type>>` are write-once fields populated by later passes (resolver, typechecker) directly into AST nodes. This makes `Expr` non-`Send`, prevents sharing across threads, and means any `Value` wrapping an `Expr` is also non-`Send`. The current fix — "mutation is frozen before `load` returns" — is an implicit invariant enforced nowhere.

### Synchronous, Single-Threaded Evaluation

Every builtin that performs I/O calls `block_on` from `src/async_rt.rs` — a thread-local Tokio `current_thread` runtime — blocking the thread for the full duration of the operation. `Rc<T>` throughout makes the evaluator non-`Send` and non-`Sync`, preventing multi-core execution. There is no way to express concurrent work within one program.

---

## The Proposal

Three interlocking changes, implemented as one coherent rewrite:

1. **AST Redesign** — split `Expr` into `SurfaceExpression` (immutable, `Send + Sync`, no `RefCell`, exposed to tinct) and `CoreExpr` (de Bruijn indices as plain fields, evaluator-internal only). Move compile-time-only declaration forms out of the expression enum. Assign stable `NodeId` to every node; move typechecker annotations into side tables.

2. **Native AST Value Types** — `Value::Program(Arc<SurfaceProgram>)`, `Value::Document(Arc<SurfaceDocument>)`, `Value::Expression(Arc<SurfaceNode>)`. Declare `Expression`, `Document`, `Program` as nominal types in prelude; tinct code pattern-matches on `Expression` variants with static typing and lazy field binding. `load` returns `Program`. `expand` takes and returns `Program`. `eval` takes `[Seq Expression]`. `dict_to_file` is never written. `dict_to_ast` is deleted.

3. **Async Runtime** — `eval` and `materialize` become `async fn`. `Rc<T>` → `Arc<T>` throughout; `RefCell<T>` → `RwLock<T>` or `Mutex<T>`; `ThunkState` replaced by an `OnceLock`-based pair. Multi-thread Tokio with work-stealing distributes independent thunks across all cores. `task`/`await`/`channel`/`select` primitives. These are one refactor: every file is touched for the `async fn` contagion anyway; the `Rc`→`Arc` migration is mechanical on top.

These three parts are inseparable. The `Rc`→`Arc` migration (Part 3) requires all `Value`s to be `Send`; `Value::Expression` wrapping a type with `RefCell` fields is not `Send` — so AST redesign (Part 1) is a prerequisite. The native value types (Part 2) must use `Arc` to be compatible with the parallel runtime; doing them with `Rc` first and migrating later opens the same files twice. The thunk's internal `UnevaluatedState::Expr` stores `CoreExpr` (Part 1), not raw `Expr`. All three parts form one coherent implementation sprint.

**Designed for future distribution.** The design choices here — `Arc`-based thunks, `OnceLock` "evaluate exactly once" semantics, `CoreExpr` as a pure data tree satisfying the serializability invariant, and the capability model that already identifies I/O-free computations — create the conditions under which distributing evaluation across machines becomes tractable.

---

## Part 1: AST Redesign

### The Two Representations

The current `Expr` enum serves every phase of the pipeline — parser, macro expander, resolver, typechecker, evaluator — with RefCell fields mutated in place by different passes. The redesign splits this into two clean types with a lowering pass between them.

**The `Surface*` family** (`SurfaceExpression`, `SurfaceDocument`, `SurfaceProgram`, `SurfaceDeclaration`, `SurfaceNode`) exists for one purpose: to be the immutable, `Send + Sync`, pointer-stable Rust representation that backs the tinct-visible nominal types (`Expression`, `Document`, `Program`, `Declaration`). These types are what `load` and `expand` produce, what `Value::Program`/`Value::Document`/`Value::Expression` wrap, and what tinct metaprogramming code operates on via match dispatch and field access. They contain no `RefCell`, no interior mutation, and no evaluator-specific data. They are the source-of-truth representation for tooling (formatters, linters, docgen, macros).

**The `Core*` family** (`CoreExpr`) exists for one purpose: to be the evaluator's private, optimised, already-resolved internal representation. It embeds de Bruijn coordinates as plain fields, is never exposed to tinct code, and can change freely without affecting the tinct API.

**`SurfaceExpression`** — what the parser produces and what tinct code sees:
- Immutable; no `RefCell` fields
- `Arc`-wrapped at every recursive position
- Node identity derived from `Arc` pointer — stable for the Arc's lifetime, no counter required
- Represents the user-visible structure: names, annotations, source forms
- Maps 1:1 to the tinct `Expression` type declaration
- Wrapped in `Value::Expression` — the representation tinct metaprogramming operates on

**`CoreExpr`** — what the evaluator operates on:
- De Bruijn coordinates as plain `u32` fields (no `RefCell`, no `Option`)
- Produced by the lowering pass from `SurfaceExpression`
- Stored inside `UnevaluatedState::Expr` in each thunk
- Never exposed to tinct code
- Can be optimized freely (e.g., unboxed closures, direct field access) without affecting the tinct API

### Node Identity

`SurfaceNode` is the dedicated wrapper for expression nodes — it replaces `Spanned<SurfaceExpression>` everywhere expressions appear. `Spanned<T>` remains unchanged for non-expression types (params, entries, annotations, match arms).

```rust
pub struct SurfaceNode {
    pub expr: SurfaceExpression,
    pub span: Span,
    // No id field — identity is the Arc pointer itself
}
```

`NodeId` is derived from `Arc<SurfaceNode>` pointer identity — never stored in any node, never assigned by a counter:

```rust
pub struct NodeId(usize);  // raw pointer value

fn node_id(arc: &Arc<SurfaceNode>) -> NodeId {
    NodeId(Arc::as_ptr(arc) as usize)
}

// Produced by the resolver pass — replaces VarRef.resolved: RefCell<...>
pub struct ResolutionTable(HashMap<NodeId, (u32, u32)>);    // level, slot

// Produced by the typechecker — replaces TypeAssert.resolved_type: RefCell<...>
pub struct TypeAnnotationTable(HashMap<NodeId, Type>);
```

`Arc` allocation guarantees unique addresses for all live objects — no global counter, no thread coordination. Works for synthetic nodes from macro expansion. A `NodeId` is valid only while its `Arc<SurfaceNode>` is alive; tables are always paired with the `SurfaceProgram` that keeps all nodes alive.

**ResolutionTable caching:** `IncludeCacheEntry::Cached` stores three things together: the result thunk, the `Arc<ResolutionTable>`, and the `Arc<TypeAnnotationTable>`. The include cache keeps `Arc<SurfaceProgram>` alive, so a cached file's nodes have stable pointers for the process lifetime — the same pointer-derived `NodeId`s apply on every access. Both tables are immutable once produced; re-running either pass on a cache hit is pure waste. The `TypeAnnotationTable` may be evicted after all thunks for a file complete lowering (since `resolved_type` is then embedded in `CoreExpr`), while `ResolutionTable` must be retained for the lifetime of the cached file (needed whenever a new `Surface` thunk for that file is created).

### `SurfaceExpression`

```rust
pub enum SurfaceExpression {
    // Literals
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // Variable reference — escaped: true = $name (pin in patterns), false = bare (bind)
    // No 'resolved' field — resolution lives in ResolutionTable
    VarRef { name: String, escaped: bool },

    // Access
    DotAccess { expr: Arc<SurfaceNode>, field: DotKey },
    // Pipe is surface-only — the lowering pass rewrites it to Call before evaluation.
    // Kept in SurfaceExpression (and Expression) so formatters and metaprogramming tools
    // can distinguish pipe-form from explicit call-form in user source.
    Pipe       { lhs: Arc<SurfaceNode>, rhs: Arc<SurfaceNode> },

    // Sequential let* scoping (multi-expr fn bodies, match arm bodies)
    Sequential(Vec<Arc<SurfaceNode>>),

    // Dict/list literal — entry key is None for auto-indexed (positional) entries
    Dict(Vec<Spanned<SurfaceEntry>>),

    // Function call — implied: true = [f x y], false = [call f x y]
    Call {
        func: Arc<SurfaceNode>,
        args: Vec<Arc<SurfaceNode>>,
        named_args: Vec<Spanned<SurfaceNamedArg>>,
        implied: bool,
    },

    // Function definition — desugared: true = synthesised by $_ desugaring
    // return_ann is surface-level user input, not a pass result — lives here, not in a side table
    Fn {
        return_ann: Option<Spanned<Annotation>>,
        params: Vec<Spanned<SurfaceParam>>,
        body: Arc<SurfaceNode>,
        desugared: bool,
    },

    // Type assertion — no resolved_type field; lives in TypeAnnotationTable keyed by NodeId
    TypeAssert {
        annotation: Spanned<Annotation>,
        expr: Arc<SurfaceNode>,
    },

    // Annotated bare word, e.g. Fn@Number
    Annotated { name: String, annotation: Spanned<Annotation> },

    // Row variable / open record marker — None = unnamed (...)
    Rest(Option<String>),

    // Pattern matching
    Match {
        scrutinee: Arc<SurfaceNode>,
        arms: Vec<SurfaceMatchArm>,
    },

    // Quasiquoting
    Quote(Arc<SurfaceNode>),
    Unquote(Arc<SurfaceNode>),
    UnquoteSplice(Arc<SurfaceNode>),

    // Binding and pattern forms. Structurally valid only in specific host positions:
    // PatternDecl — inside InstanceDecl.arms[n].0 only
    // CaseArm — inside Match.arms only
    // LetDecl — inside Fn.params, ClassDecl, TypeAlias params, and CaseArm patterns
    // The lowering pass raises an error if these appear in other expression positions.
    PatternDecl { bindings: Vec<Arc<SurfaceNode>> },
    LetDecl     { bindings: Vec<Arc<SurfaceNode>> },
    CaseArm     { pattern: Arc<SurfaceNode>, body: Arc<SurfaceNode> },
    TypeApp     { func: Arc<SurfaceNode>, arg: Arc<SurfaceNode> },

    // Placeholder `...` — evaluates to error when forced
    Placeholder,

    // Parse error node — span covers the unparseable region
    Error(Span),
}
```

### Declaration Separation

Compile-time-only forms — `TypeAlias`, `ClassDecl`, `InstanceDecl`, `DefMacro`, `MacroDecl`, `SyntaxClass`, `Splice` — are removed from `SurfaceExpression` and placed in a `SurfaceDeclaration` enum. They cannot produce runtime values and their presence in the expression enum forces every evaluator match arm to handle them (typically with `unreachable!()`).

```rust
pub enum SurfaceDeclaration {
    TypeAlias  { params: Vec<String>, body: Arc<SurfaceNode> },
    ClassDecl  { name: String, params: Vec<String>, superclasses: Vec<(String, String)>,
                 methods: Vec<Spanned<SurfaceEntry>>, determines: Vec<Arc<SurfaceNode>>,
                 resolver: Option<Arc<SurfaceNode>>, resolver_injective: bool },
    InstanceDecl { class_name: String, arms: Vec<(Arc<SurfaceNode>, Vec<Spanned<SurfaceEntry>>)> },
    DefMacro   { name: String, params: Arc<SurfaceNode>, body: Arc<SurfaceNode> },
    MacroDecl  { name: String, params: Arc<SurfaceNode>, body: Arc<SurfaceNode> },
    SyntaxClass { name: String, pattern: Arc<SurfaceNode>, message: Option<String> },
    Splice     (Vec<Arc<SurfaceNode>>),
}
```

`SurfaceDocument` expresses this split via `SurfaceItem`:

```rust
pub enum SurfaceItem {
    Expr(Arc<SurfaceNode>),
    Decl(Spanned<SurfaceDeclaration>),
}

pub struct SurfaceDocument {
    pub stage: Option<Stage>,
    pub name: Option<String>,
    pub items: Vec<SurfaceItem>,
    pub output_type: Option<Spanned<Annotation>>,
    pub expects: Option<Spanned<Annotation>>,
    pub caps: Option<Spanned<Vec<(String, Annotation)>>>,
}

pub struct SurfaceProgram {
    pub documents: Vec<Spanned<SurfaceDocument>>,
}
```

### `CoreExpr` and the Lowering Pass

`CoreExpr` is the evaluator's internal representation. It embeds de Bruijn coordinates directly, eliminating the `Option<Option<(u32, u32)>>` indirection:

```rust
pub enum CoreExpr {
    Int(i64),
    Float(f64),
    Bool(bool),
    Str(String),

    // VarRef with resolved coordinates — unresolvable refs use a sentinel
    Var { name: String, level: u32, slot: u32 },
    FreeVar(String),   // include-introduced bindings, unresolvable statically

    DotAccess { expr: Arc<Spanned<CoreExpr>>, field: DotKey },
    // No Pipe variant — the lowering pass rewrites SurfaceExpression::Pipe { lhs, rhs }
    // to CoreExpr::Call { func: rhs, args: [lhs], implied: true } before evaluation.
    // Pipe is syntactic sugar; Sequential is semantic (multi-expression let* scoping).
    Sequential(Vec<Arc<Spanned<CoreExpr>>>),
    Dict(Vec<Spanned<CoreEntry>>),
    Call { func: Arc<Spanned<CoreExpr>>, args: Vec<Arc<Spanned<CoreExpr>>>,
           named_args: Vec<Spanned<CoreNamedArg>>, implied: bool },
    Fn   { return_ann: Option<Spanned<Annotation>>, params: Vec<Spanned<CoreParam>>,
           body: Arc<Spanned<CoreExpr>>, desugared: bool },
    // Statically type-checked TypeAssert — resolved_type set from TypeAnnotationTable during lowering
    TypeAssert { annotation: Spanned<Annotation>, expr: Arc<Spanned<CoreExpr>>,
                 resolved_type: Type },
    // TypeAssert for nodes absent from TypeAnnotationTable (macro-synthesized, bypassed typechecking)
    // Falls back to default: if present, raises error otherwise. resolved_type: None is not valid.
    RuntimeTypeCheck { annotation: Spanned<Annotation>, expr: Arc<Spanned<CoreExpr>>,
                       default: Option<Arc<Spanned<CoreExpr>>> },
    Annotated  { name: String, annotation: Spanned<Annotation> },
    Rest(Option<String>),
    Match { scrutinee: Arc<Spanned<CoreExpr>>, arms: Vec<CoreMatchArm> },
    Quote(Arc<Spanned<CoreExpr>>),
    Unquote(Arc<Spanned<CoreExpr>>),
    UnquoteSplice(Arc<Spanned<CoreExpr>>),
    PatternDecl { bindings: Vec<Spanned<CoreExpr>> },
    LetDecl     { bindings: Vec<Spanned<CoreExpr>> },
    CaseArm     { pattern: Arc<Spanned<CoreExpr>>, body: Arc<Spanned<CoreExpr>> },
    TypeApp     { func: Arc<Spanned<CoreExpr>>, arg: Arc<Spanned<CoreExpr>> },
    Placeholder,
    Error(Span),
}
```

**The lowering pass** `lower(node: &Arc<SurfaceNode>, res: &ResolutionTable, types: &TypeAnnotationTable) -> Spanned<CoreExpr>` converts a single expression. It is called **per-thunk** — when a thunk containing a `SurfaceNode` is first forced via the `OnceLock` protocol, it lowers at that moment. Desugaring is part of lowering — `desugar.rs` is deleted; its one responsibility (`Pipe` → `Call`) is handled here:

- `SurfaceExpression::Pipe { lhs, rhs }` → `CoreExpr::Call { func: lower(rhs), args: [lower(lhs)], implied: true }` — pipe is syntactic sugar, eliminated before evaluation
- `SurfaceExpression::Sequential` → `CoreExpr::Sequential` — kept; it carries real let\* semantics for multi-expression fn bodies Lowering is a pure function of `(SurfaceNode, ResolutionTable, TypeAnnotationTable)`; it commutes with evaluation order. Lowering cost is only paid for expressions that are actually evaluated — dead code is never lowered.

**Phase-ordering invariant:** Both `ResolutionTable` and `TypeAnnotationTable` must be fully populated before any thunk is forced. This is enforced by phase ordering: `expand` → resolution → typecheck → evaluation. Macro expansion and `include` must not synthesize new `SurfaceNode` expressions after typechecking. The tables are immutable once produced; the lowering pass reads but never writes them.

**TypeAssert lowering:** The lowering pass checks `type_table.get(&node_id(&arc))`:
- **Present** → `CoreExpr::TypeAssert { resolved_type: *ty }` — statically verified
- **Absent** (macro-synthesized, bypassed typechecking) → `CoreExpr::RuntimeTypeCheck { annotation, default }` — dynamic check, falls back to `default:` if present, raises error otherwise

`resolved_type: Option<Type>` does not exist in `CoreExpr` — the two cases are always distinct variants. `None` is not a valid runtime state.

**`CoreExpr::RuntimeTypeCheck` evaluation protocol:** (1) `expr` is forced — this is a materialization point. (2) The materialized value is checked against `annotation` using the same structural validation as the existing `TypeAssert` guard path. (3) If the check passes: return the materialized value. (4) If the check fails and `default` is present: return `default` as an unevaluated thunk (laziness preserved). (5) If the check fails and no `default`: raise `EvalError`. (6) The result (value or error) is cached in the thunk's `OnceLock` — permanently failed thunks do not retry.

Lowering errors (malformed AST, impossible variant combinations) surface lazily when the thunk is forced — consistent with tinct's lazy semantics generally.

`CoreExpr` is never exposed to tinct code. It lives only in thunks. Unlike `SurfaceExpression` which uses the dedicated `SurfaceNode` wrapper, `CoreExpr` recursive positions use `Arc<Spanned<CoreExpr>>` directly — no `CoreNode` type, since `CoreExpr` has no need for pointer-derived identity (it is never matched via `NodeId`).

**Serializability invariant:** Every field in `CoreExpr` is a primitive, `String`, `u32`, `Box<Type>`, `Vec<T>`, or `Arc<Spanned<CoreExpr>>` — no opaque Rust handles, no `Arc<dyn ...>`, no trait objects. This makes `CoreExpr` a pure data tree, serializable by a simple recursive function. Any new `CoreExpr` variant must maintain this invariant.

---

## Part 2: Native AST Value Types

### New Value Variants

```rust
Value::Program(Arc<SurfaceProgram>)
Value::Document(Arc<SurfaceDocument>)
Value::Expression(Arc<SurfaceNode>)
```

`Arc` throughout — these variants are `Send + Sync`, compatible with the async parallel runtime from Part 3. No `Value::Span` — spans remain plain Dicts (infrequently accessed, allocation cost is fine). `dict?` returns `false` for all three; they are nominal types, not plain Dicts. Code guarding access should use `type-of` or `match`.

### Tinct Type Declarations

Added to prelude — the primitive signatures of `load`, `expand`, `eval`, `eval-types`, and `ast-of` reference these types, and they must be resolvable everywhere without an explicit import.

#### Supporting Types

```tinct
Span: [type [Span
  file:       String
  start-line: Int
  start-col:  Int
  end-line:   Int
  end-col:    Int]]

DotKey: [type [Ident String] [Index Int]]

Parameter: [type [Parameter
  name:       String
  annotation: Annotation
  variadic:   Bool
  span:       Span]]

NamedArg: [type [NamedArg
  name:  String
  value: Expression
  span:  Span]]

Entry: [type [Entry
  key:   Expression   # [] when auto-indexed
  value: Expression
  span:  Span]]

MatchArm: [type [MatchArm
  pattern: Pattern
  guard:   Expression    # [] when no guard
  body:    Expression
  span:    Span]]

Annotation: [type
  [Simple       value: String]
  [PropertyDict entries: [Seq AnnotationEntry]]
  [Annotated    name: String  inner: Annotation]]

AnnotationEntry: [type [AnnotationEntry
  key:   String
  value: Annotation]]
```

`Pattern` maps the `Pattern` enum (`Wildcard`, `Variable`, `Literal`, `TypeTag`, `Pin`, `Dict`, `Seq`, `Constructor`, `Or`); its full declaration is deferred to the implementation sprint — the same approach applies.

**Known limitation — recursive type precision:** `Expression` is self-referential (e.g., `[Call fn: Expression args: [Seq Expression] ...]`). The current type system handles recursive aliases via a recursion guard that substitutes a fresh `TypeVar` at the recursive position rather than a proper mu-type. Type inference on nested AST operations (e.g., accessing `node.fn.name` through two levels of `Expression`) degrades to `TypeVar`-based reasoning rather than full structural reasoning. This is a known limitation of the current HM implementation; equi-recursive alias expansion (mu-variable encoding) is tracked separately as a future type system improvement.

#### `Expression`

Variants map 1:1 to `SurfaceExpression` members. Internal implementation fields are not exposed. Formatter-relevant flags (`escaped`, `implied`, `desugared`) are exposed.

```tinct
Expression: [type
  # Literals
  [IntLiteral   value: Int    span: Span]
  [FloatLiteral value: Float  span: Span]
  [BoolLiteral  value: Bool   span: Span]
  [StrLiteral   value: String span: Span]

  # Variables — escaped: true = $name (pin in patterns), false = bare name (bind)
  [Var  name: String  escaped: Bool  span: Span]

  # Access
  [DotAccess  target: Expression  field: DotKey  span: Span]

  # Pipe operator (desugar-only)
  [Pipe  lhs: Expression  rhs: Expression  span: Span]

  # Sequential let* scoping (multi-expr fn bodies, match arm bodies)
  [Sequential  exprs: [Seq Expression]  span: Span]

  # Dict/list literal — key: [] for auto-indexed entries
  [Dict  entries: [Seq Entry]  span: Span]

  # Function call — implied: true = [f x y], false = [call f x y]
  [Call  fn: Expression  args: [Seq Expression]  named: [Seq NamedArg]  implied: Bool  span: Span]

  # Function definition — desugared: true = synthesised by $_ desugaring
  [Fn  params: [Seq Parameter]  body: Expression  return-ann: Annotation  desugared: Bool  span: Span]

  # Type assertion — no resolved-type field (typechecker internal, lives in CoreExpr)
  [TypeAssert  annotation: Annotation  expr: Expression  span: Span]

  # Annotated bare word, e.g. Fn@Number
  [Annotated  name: String  annotation: Annotation  span: Span]

  # Row variable / open record marker: ... or ...rest. name: [] when unnamed.
  [Rest  name: String  span: Span]

  # Pattern matching
  [Match  scrutinee: Expression  arms: [Seq MatchArm]  span: Span]

  # Quasiquoting
  [Quote         expr: Expression  span: Span]
  [Unquote       expr: Expression  span: Span]
  [UnquoteSplice expr: Expression  span: Span]

  # Higher-kinded type application in annotation positions
  [TypeApp  fn: Expression  arg: Expression  span: Span]

  # Binding and pattern forms (used inside fn params, match arms, instance arms)
  [PatternDecl  bindings: [Seq Expression]          span: Span]
  [LetDecl      bindings: [Seq Expression]          span: Span]
  [CaseArm      pattern: Expression  body: Expression  span: Span]

  # Placeholder `...` — evaluates to error when forced
  [Placeholder  span: Span]

  # Parse error node — span covers the unparseable region
  [Error  span: Span]]
```

Compile-time-only declaration forms (`TypeAlias`, `ClassDecl`, `InstanceDecl`, `DefMacro`, `MacroDecl`, `SyntaxClass`, `Splice`) are NOT in `Expression`. They appear in `SurfaceDocument.items` as `SurfaceItem::Decl` nodes. Tinct code iterating `doc.expressions` sees only value-producing expressions; type-system and macro declarations are accessible via a separate `doc.declarations` field.

#### `Document` and `Program`

```tinct
DocumentName: [type [Named String] [Unnamed]]

Document: [type [Document
  stage:        [type [Runtime] [Type]]
  name:         DocumentName
  expressions:  [Seq Expression]    # value-producing expressions only
  declarations: [Seq Declaration]    # compile-time-only forms
  output-type:  Annotation    # [] when absent
  expects:      Annotation]]  # [] when absent

Program: [type [Program
  documents: [Seq Document]]]

Declaration: [type
  [TypeAlias   params: [Seq String]  body: Expression]
  [ClassDecl   name: String  params: [Seq String]  ...]
  [InstanceDecl class-name: String   arms: [Seq [AstInstanceArm pattern: Expression methods: [Seq Entry]]]]
  [DefMacro    name: String  params: Expression  body: Expression]
  [MacroDecl   name: String  params: Expression  body: Expression]
  [SyntaxClass name: String  pattern: Expression  message: String]
  [Splice      forms: [Seq Expression]]]
```

### Match Dispatch for `Value::Expression`

Match arm bindings on `Value::Expression` are **lazy** — consistent with tinct's overall evaluation model where materialization is on-demand. When the `match` builtin encounters `Value::Expression(node)`:

1. `surface_expr_tag(&node.expr) -> &'static str` — extract the variant tag (e.g. `"Var"`, `"Call"`). O(1), no allocation.
2. If the tag matches a pattern arm, create one `UnevaluatedState::AstNodeField` thunk per **pattern-bound** variable:

```rust
UnevaluatedState::AstNodeField {
    node:  Arc<SurfaceNode>,
    field: &'static str,   // "name", "args", "span", etc.
}
```

3. Each thunk evaluates lazily by calling `surface_node_get_field(&node, field)` when demanded in the arm body. Unused bindings are never *evaluated* — their `Arc<Thunk>` wrapper is allocated at dispatch time, but `surface_node_get_field` is never called for them. The benefit is for expensive field computations: `args: [Seq Expression]` on a `Call` node is never constructed if the arm body doesn't use `args`.

This is especially significant for heavy fields: `args: [Seq Expression]` in a `[Call ...]` arm or `entries: [Seq Entry]` in a `[Dict ...]` arm are never constructed if the arm body doesn't use them.

`Value::Document` and `Value::Program` follow the same protocol with their respective field extractors. `Value::Variant` match is already lazy (payload Dict contains ThunkIds); no change there.

**Performance notes:** For each pattern-bound variable, one `Arc<Thunk>` with `AstNodeField` state is allocated at match dispatch regardless of use — the laziness is in evaluation, not allocation. The OnceLock Mutex lock (~5–10ns uncontested) is paid on first force of each field; for cheap leaf fields (`Bool`, `String` flags) this overhead may exceed the field extraction cost. If profiling shows this is a hotspot in AST-traversal workloads, consider a `try_lock` fast path for `AstNodeField` evaluation or a dedicated `OnceLock<Value>` that bypasses the full thunk machinery for this variant.

`[deep-materialize ast-expr]` is a **no-op** — it returns the `Value::Expression` unchanged, like `Value::Handle` or `Value::DirCap`. `Expression` is a nominal opaque type; materializing its internals into a tree would allocate O(n) values for the full subtree, and can be triggered accidentally by any builtin that calls `deep-materialize` on a value containing an `Expression`. Inspection is done via `match` and field access; serialization to JSON is done by writing a tinct traversal function using match dispatch. JSON output of a program that returns an `Expression` value produces the opaque type marker, not the expanded tree.

### Field Access on `Value::Expression`

Direct field access (`node.name`, `[get "span" node]`) is supported via the same field dispatcher used to build the match payload. The type checker warns that field access on a union type may not be valid for all variants — accurate, and better than the `Any` return type the Dict schema provided. Code requiring typed access should use `match`.

### Changed Primitive Signatures

```tinct
# After include-decomp                              # After runtime-v2
load@[Fn [source@String  name: @String] Dict]       load@[Fn [source@String  name: @String] Program]
expand@[Fn [ast@Dict] Dict]                         expand@[Fn [ast@Program] Program]
eval@[Fn [exprs@Dict  %: @Any  env: @Dict] Any]     eval@[Fn [exprs@[Seq Expression]  %: @Any  env: @Dict] Any]
eval-types@[Fn [exprs@Dict] Any]                    eval-types@[Fn [exprs@[Seq Expression]] Any]
ast-of@[Fn [expr@Any] Dict]                         ast-of@[Fn [expr@Any] Expression]
```

### Updated Include-Decomp Tinct Code

Structure unchanged; type annotations and the `doc.name` field access update:

```tinct
eval-document-runtime: [fn@[return: Dict] [let state doc@Document include-dir]
  [result: [eval
    doc.expressions          # [Seq Expression] — passed directly to eval builtin
    %:   state.prev
    env: [merge
           [if [dict? state.prev] state.prev []]
           state.named
           ["%include-dir": include-dir]]]]
  [prev: result
   named: [if [match doc.name [Named n]: true  Unnamed: false]
            [merge state.named [[str "%" [match doc.name [Named n]: n]]: result]]
            state.named]]]

eval-document-pipeline: [fn@[return: Any] [let initial docs@[Seq Document] include-dir]
  [get "prev"
    [reduce
      [fn@[return: Dict] [let state doc@Document]
        [match doc.stage
          Runtime: [eval-document-runtime state doc include-dir]
          Type:    state]]
      [prev: initial  named: []]
      docs]]]

eval-file: [fn@[return: Any] [let ast@Program initial include-dir]
  [eval-document-pipeline initial ast.documents include-dir]]
```

`doc.name` becomes `DocumentName` (`[Named String] | Unnamed`) rather than `String | []` — making the optional-name pattern explicit and type-safe.

### Quasiquoting and `eval-ast`

`[quote expr]` returns `Value::Expression` (was `Value::Dict`). `eval-ast` is deleted — replaced by `[eval [seq some-ast-expr] %: [] env: []]`.

### `ast_to_dict_expr` / `dict_to_ast` Fate

- **`dict_to_ast`** — deleted entirely; no longer called anywhere
- **`dict_to_file`** — never written; this proposal supersedes that item
- **`ast_to_dict_expr`** — retained internally; called by `surface_node_get_field` to produce `Value::Variant` field values for `deep-materialize` and JSON output. Not a registered builtin.

---

## Part 3: Async Runtime

### The Rust/tinct Boundary

tinct is a language, not a Rust application. Rust owns the irreducible OS interface; everything above it is tinct. A function belongs in Rust only if it meets one of:

1. **Binary protocol or encoding** — TLS crypto, QUIC, HTTP/3 QPACK, binary frame formats
2. **Security-critical** — cryptographic primitives, TLS certificate verification
3. **Performance cliff** — `from-json`/`emit` (JSON parsed on every eval startup), core arithmetic
4. **OS system call** — file I/O, socket bind, signal delivery, clock, process control
5. **Bootstrap** — the parser, evaluator, and type checker themselves

Everything else belongs in tinct. The test: *is this possible to implement in tinct?* If yes, it is stdlib.

One concrete consequence: `hyper` and `reqwest` are removed. HTTP/1.1 is a text protocol handled by `stdlib/http1.llt`. HTTP/3 framing (QPACK) stays in Rust via the `h3` crate (Huffman tables and binary encoding are not interesting logic to express in tinct). Rust dep change: add `h3`, `tokio-util`; remove `hyper`, `reqwest`.

### The Execution Model

Two layers implemented in one pass:

**Layer 1 — Async:** `eval` and `materialize` become `async fn`. Every blocking I/O operation yields to the scheduler. Tokio interleaves independent evaluations cooperatively.

**Layer 2 — Parallel:** `Rc<T>` → `Arc<T>` throughout; `RefCell<T>` → `RwLock<T>` or `Mutex<T>`; `ThunkState` replaced by an `OnceLock`-based pair. Multi-thread Tokio with work-stealing distributes independent thunks across cores. Independent dict entries evaluate in parallel automatically.

These are one refactor, not two. Every file is touched for the `async fn` contagion; the `Rc`→`Arc` migration is mechanical on top. Because Part 1 already eliminated all `RefCell` from AST nodes, the migration is clean — no exceptions, no special cases.

### The `Rc` → `Arc` Migration

| Before | After | Notes |
|--------|-------|-------|
| `Rc<Thunk>` | `Arc<Thunk>` | Thunks safely cross thread boundaries |
| `Rc<RefCell<Environment>>` | `Arc<RwLock<Environment>>` | Write-rarely, read-often |
| `Rc<RefCell<ThunkState>>` | `OnceLock` pair — see below | |
| `Rc<EvalConfig>` | `Arc<EvalConfig>` | Already immutable; trivial |
| `Rc<RefCell<EvalState>>` | `Arc<Mutex<EvalState>>` | Include cache; infrequent access |
| `Rc<SurfaceProgram/Doc/Expr>` | `Arc<SurfaceProgram/Doc/Expr>` | New variants — Arc from day one |

`Arc` clone costs ~10–50ns vs ~1ns for `Rc`. Thunk evaluation costs microseconds to milliseconds — the overhead is negligible for thunk-forcing workloads. For traversal-heavy workloads (formatters, linters walking large AST trees) that create many `AstNodeField` thunks without forcing them, Arc clone frequency is higher; profile before declaring negligible in those cases.

### The `OnceLock` Thunk

The current `ThunkState` enum inside a `RefCell` is replaced with a write-once pair. `InProgress` is no longer a safe sentinel — a thread could yield while holding `InProgress`, and another task demanding the same thunk would spuriously raise a cycle error.

```rust
pub struct Thunk {
    // Taken by the task that wins the evaluation race; None afterwards
    unevaluated: Mutex<Option<UnevaluatedState>>,
    // Set exactly once; all waiters unblock automatically
    result: tokio::sync::OnceCell<Result<Value, Arc<EvalError>>>,
    pub span: Span,
}

enum UnevaluatedState {
    // Pre-lowering: holds a SurfaceNode to be lowered to CoreExpr on first force.
    // Created by the `eval` builtin for each expression in [Seq Expression]. The captured
    // env is constructed by eval (stdlib_env + env: entries + %: binding) — this is the
    // closure environment. The expression sees only this env when forced, regardless of
    // the caller's ambient environment. Capability safety follows directly from closure
    // semantics: %pwd and other capabilities are absent from stdlib_env and only
    // reachable if the caller explicitly forwards them via env:.
    Surface {
        node:  Arc<SurfaceNode>,
        res:   Arc<ResolutionTable>,
        types: Arc<TypeAnnotationTable>,
        env:   Arc<Environment>,
        ctx:   Arc<EvalContext>,
    },
    // Post-lowering: holds a CoreExpr ready for evaluation. Created by the evaluator
    // during CoreExpr reduction, and by the forcing protocol when a Surface thunk is
    // first forced (lower SurfaceNode → CoreExpr, then evaluate).
    Expr    { expr: Spanned<CoreExpr>, env: Arc<Environment>, ctx: Arc<EvalContext> },
    Builtin { func: BuiltinFn, args: Vec<Arc<Thunk>>, named: IndexMap<String, Arc<Thunk>>,
              depth: usize, call_span: Span, ctx: Arc<EvalContext> },
    Call    { func: Arc<Thunk>, args: Vec<Arc<Thunk>>, call_span: Span, ctx: Arc<EvalContext> },
    // Created by match dispatch on Value::Expression — evaluates a single field lazily
    AstNodeField { node: Arc<SurfaceNode>, field: &'static str },
}
```

`Surface` is the pre-lowering state. On first force, the protocol calls `lower(node, res, types)` → `CoreExpr`, then continues as if the thunk held `Expr`. Thunks created by the evaluator during `CoreExpr` reduction always start as `Expr` — `Surface` is only produced by the `eval` builtin for externally-provided `Expression` nodes. `AstNodeField` thunks evaluate via `surface_node_get_field` with no lowering step.

**Forcing protocol:**

```
materialize(thunk):
  1. result.get() → Some(v): return v.clone()      [lock-free after init; the hot path]
  2. lock unevaluated mutex; take Option
     - Some(state): this task won. Release lock. Evaluate → value_or_err.
       result.set(value_or_err).ok()               [waiters unblock automatically]
     - None: another task is evaluating. Release lock.
       result.get_or_init(|| unreachable!()).await  [suspends until winner sets]
  3. return result.get().unwrap().clone()
```

Every thunk evaluates exactly once regardless of how many tasks demand it simultaneously. The hot path is fully lock-free.

**Cycle detection — hybrid model:** Two layers handle the two distinct cycle classes.

*Intra-task cycles (fast path):* each async task maintains a task-local `HashSet<*const Thunk>` of thunks currently on its own evaluation stack. Demanding a thunk already in this set is an immediate cycle error — same task, no cross-thread coordination, zero overhead on the common path.

*Cross-task cycles (slow path):* two tasks can mutually block on each other's thunks without either appearing in the other's task-local set — both enter `result.get_or_init().await` and suspend forever. A process-global wait-for graph detects this. Before entering the await path, a task records itself in `WAIT_FOR: ConcurrentHashMap<TaskId, *const Thunk>` (which thunk it is blocked on). A DFS over this graph from the current task detects cycles in O(blocked tasks). On detection: raise `EvalError::Cycle` on the waiting task with the thunk's span; the blocked chain unwinds.

The wait-for graph is **process-local and never distributed.** Cross-node deadlock cycles are structurally impossible: `remote-task` requires fully materialized environments (the distributable thunk constraint from [`dist-eval.md`](dist-eval.md)) — a remote thunk cannot capture a reference to an in-flight local `Task` handle. The distributable constraint eliminates cross-process cycles at the type level; the process-local graph covers everything that remains. `cluster-local` workers run in the same process and are covered by the same graph.

`EvalError` is now `Arc<EvalError>` (was `Box`) for cheap cross-thread clone.

### When `task` Starts

`[task expr]` passes `expr` as a thunk to the `task` builtin. `spawn_local` fires **when the `task` expression itself is materialized** — not when `await` demands the handle. An undemanded task thunk is never spawned.

```tinct
# task is spawned when 'worker' is first demanded — not before
worker: [task [fetch cap "https://api.example.com/"]]
result: [await worker]   # spawning happens here

# NEVER spawned — t is never demanded
[t: [task [expensive]]  answer: [+ 1 2]]   # only answer is demanded
```

### Async Primitives

```tinct
# Spawn a concurrent computation
worker: [task [fetch cap "https://api.example.com/data"]]

# Await a single task
result: [await worker]

# Await all tasks in parallel
[a: a-result  b: b-result]: [await-all
  [task [fetch cap "https://api1.example.com/"]]
  [task [fetch cap "https://api2.example.com/"]]]

# Channels — typed queues for communication
ch: [channel 32]
[send ch "hello"]
msg: [recv ch]

# Select — wait for whichever channel fires first
[select
  [sig-ch  [fn [let _]   [exit 0]]]
  [req-ch  [fn [let req] [task [handle-request req]]]]
  [tick-ch [fn [let _]   [log "heartbeat"]]]]
```

### Automatic Parallel Dict Evaluation

With `Arc` throughout and `tokio::spawn`, independent dict entries evaluate in parallel automatically:

```tinct
# a, b, c have no data dependency — they evaluate on separate cores simultaneously
[
  a:      [fetch cap "https://api1.example.com/"]
  b:      [fetch cap "https://api2.example.com/"]
  c:      [db-query db "SELECT * FROM users"]
  result: [merge a [merge b c]]   # waits for all three via OnceLock
]
```

`eval_dict` submits each entry's thunk to the Tokio thread pool via `tokio::spawn`. Data dependencies enforce ordering naturally.

### `par` and `par-map`

```tinct
# Sequential: map forces one element at a time
[map [fn [let x] [expensive x]] big-list]

# Parallel: all elements submitted to the thread pool simultaneously
[par-map [fn [let x] [expensive x]] big-list]

# par: start evaluating now, return value when demanded
a: [par [expensive-computation input]]
```

### Event Sources

Event sources are Rust builtins returning a `Channel` written to by a background task:

```tinct
sig:     [signal-channel SIGTERM SIGINT]
tick:    [timer-channel %clock [seconds 5]]
changes: [watch-channel dir-cap "/etc/config"]
# http-channel is stdlib/http.llt — not a Rust builtin

[loop [fn [let]
  [select
    [sig     [fn [let name] [log [str "signal: " name]] [exit 0]]]
    [tick    [fn [let _]    [log "heartbeat"]]]
    [changes [fn [let _]    [reload-config]]]]]]
```

All event sources follow the same pattern: spawn a background task that writes to a channel; the channel is the user-visible value; `ChannelInner` holds an `AbortHandle` and calls `abort()` in `Drop` — cleanup is automatic when all channel references are dropped.

### Cancellation and Contexts

Every blocking operation needs a bound. A `Context` is a first-class tinct value backed by `tokio_util::sync::CancellationToken`. The runtime creates a root context for every program run.

```tinct
ctx: [context]
[child-ctx: child  cancel: cancel-fn]: [with-cancel ctx]
timed-ctx: [with-timeout ctx 5000]

# All blocking builtins (await, recv, send, select-once) respect the context
result: [timeout [seconds 5] [task [slow-fetch cap url]]]
```

All blocking builtins check the cancellation token via `tokio::select!`:

```rust
tokio::select! {
    result = the_operation.await  => Ok(result),
    _      = ctx.cancelled().await => Err(EvalError::Cancelled),
}
```

`EvalContext` gains a `cancel: CancellationToken` field alongside `type_stage_env`:

```rust
pub struct EvalConfig {
    pub base_dir:        cap_std::fs::Dir,
    pub stdlib_env:      Arc<RwLock<Environment>>,
    pub type_stage_env:  Arc<RwLock<Environment>>,  // from Part 2
    pub no_fs:           bool,
    pub require_integrity: bool,
}

pub struct EvalContext {
    pub config: Arc<EvalConfig>,
    pub state:  Arc<Mutex<EvalState>>,
    pub cancel: tokio_util::sync::CancellationToken,   // NEW
}
```

### Type System

```tinct
task-a@[Task Int]:  [task [+ 1 2]]
ch@[Channel Str]:   [channel 10]
result@Int:         [await task-a]

# await-all: homogeneous — [Seq [Task T]] → [Seq T], results in submission order
results@[Seq Int]:  [await-all [task [+ 1 2]] [task [* 3 4]]]
```

`Type::Task(Box<Type>)`, `Type::Channel(Box<Type>)`, `Type::Context` (opaque) — all new. `task` infers the inner type from the body expression. `await` unifies `Task@?T` → `?T`. Pattern is identical to existing parameterized types (`Seq@T`).


---

## Stdlib Module Map

```
stdlib/
  prelude.llt       — map, filter, reduce, result combinators; trimmed core
                      Expression/Document/Program/Parameter/Entry/... type declarations
  strings.llt       — trim, pad-left/right, starts-with?, ends-with?, str-contains?, str-replace,
                      str-split-lines, words, unwords
  seq.llt           — zip-with, enumerate, chunk, partition, group-by, sort-by, uniq-by,
                      flat-map, scan, window, interleave
                      # partition n seq: split into n roughly-equal parts (distinct from
                      # chunk which splits into chunks of size k)
  path.llt          — path-join, path-dirname, path-basename, path-ext, path-normalize
  result.llt        — and-then, map-ok, map-err, unwrap-or, unwrap, ok?, err?, collect-results
  cap.llt           — narrow, readable?, writable?, with-temp

  async.llt         — exit, graceful-exit, finally, loop-select, retry
                      cancel: [fn [c@CancelHandle] [c.cancel]]   # convenience wrapper
  datetime.llt      — Timestamp, Duration, formatting/parsing
  regex.llt         — Thompson NFA regex engine
  toml.llt          — complete TOML 1.0 parser
  sql.llt           — lazy SQL data sources
```

Network modules (`net.llt`, `http1.llt`, `http3.llt`, `serve.llt`, `http.llt`) are specified in [`lib-net-v3.md`](lib-net-v3.md) — they depend on this proposal's async foundation but are not part of the runtime itself.

The `Expression` and supporting type declarations live in `prelude.llt` (not a separate module) because `eval`, `load`, `expand` reference them in primitive signatures.

---

## What Would Change

### Add to `src/ast.rs`

- `SurfaceNode` struct — `{ expr: SurfaceExpression, span: Span }` — dedicated expression-node wrapper
- `NodeId(usize)` — pointer-derived identity; `fn node_id(arc: &Arc<SurfaceNode>) -> NodeId { NodeId(Arc::as_ptr(arc) as usize) }`; never stored in any node
- `SurfaceExpression` enum — immutable, `Arc<SurfaceNode>`-recursive, no `RefCell`
- `SurfaceDeclaration` enum — compile-time-only declaration forms
- `SurfaceItem` enum — `Expr(Arc<SurfaceNode>) | Decl(Spanned<SurfaceDeclaration>)` for doc items
- `SurfaceDocument`, `SurfaceProgram` structs — clean, no `RefCell`, no `id` field, `caps:` retained
- `CoreExpr` enum — de Bruijn as plain fields, evaluator-only, includes `RuntimeTypeCheck` variant
- `ResolutionTable(HashMap<NodeId, (u32, u32)>)` — replaces `VarRef.resolved: RefCell<...>`
- `TypeAnnotationTable(HashMap<NodeId, Type>)` — replaces `TypeAssert.resolved_type: RefCell<...>`
- Lowering pass `lower(node: &Arc<SurfaceNode>, res: &ResolutionTable, types: &TypeAnnotationTable) -> Spanned<CoreExpr>` in `src/lower.rs`

### Delete from `src/ast.rs`

- `Expr` enum — replaced by `SurfaceExpression` + `CoreExpr`
- `Document` struct — replaced by `SurfaceDocument`
- `File` struct — replaced by `SurfaceProgram`

### Add to `src/value.rs`

- `Value::Program(Arc<SurfaceProgram>)`
- `Value::Document(Arc<SurfaceDocument>)`
- `Value::Expression(Arc<SurfaceNode>)`
- `Value::Task(Arc<Mutex<TaskState>>)`
- `Value::Channel(Arc<ChannelInner>)`
- `Value::Context(tokio_util::sync::CancellationToken)`

### Change `src/value.rs`

- All `Rc<T>` → `Arc<T>` throughout; all `RefCell<T>` → `RwLock<T>` or `Mutex<T>`
- `ThunkState` enum replaced by `(Mutex<Option<UnevaluatedState>>, tokio::sync::OnceCell<...>)` pair

### Change `src/ast_dict.rs`

- **Add** `surface_expr_tag(expr: &SurfaceExpression) -> &'static str` — O(1) tag extraction; called by match evaluator for `Value::Expression`
- **Add** `surface_node_get_field(node: &Arc<SurfaceNode>, field: &str) -> Value` — field extraction for `AstNodeField` thunk evaluation and dot-access; calls `ast_to_dict_expr` internally for complex fields
- **Add** analogues `surface_doc_tag`, `surface_doc_get_field`, `surface_file_get_field` for `Value::Document` and `Value::Program`
- **Delete** `dict_to_ast` — removed entirely
- **Retain** `ast_to_dict_expr` — used only by `deep-materialize` and JSON output (now operates on `SurfaceExpression`)

### Change `src/eval.rs`

- All functions become `async fn`
- `eval` pattern-matches on `CoreExpr` (not `Expr`); all arms updated
- `eval_dict` fans out independent entries via `tokio::task::JoinSet` — automatic parallel evaluation
- **Add** `Value::Expression`, `Value::Document`, `Value::Program` arms to: match evaluator (calls `surface_expr_tag`, creates `AstNodeField` thunks per pattern-bound variable), dot-access evaluator (calls `surface_node_get_field`), `get`, `has?`, `deep-materialize` (no-op — returns value unchanged, like Handle/DirCap), JSON serializer (opaque type marker, not expanded tree), `type-of` (returns `"Expression"` / `"Document"` / `"Program"`), `dict?` (returns `false` — nominal types, not plain Dicts)
- `materialize` uses the `OnceLock` forcing protocol
- `EvalContext` gains `cancel: CancellationToken` field

### Change `src/builtins_meta.rs`

- `load`: parse → `SurfaceProgram` → wrap in `Value::Program`; `ast_to_dict` call deleted
- `expand`: unwrap `Program` → `&SurfaceProgram`; call `expand()`; wrap result. No round-trip.
- `eval`: iterate `[Seq Expression]`; unwrap each `Value::Expression` → `Arc<SurfaceNode>`; lower to `CoreExpr` per-thunk (lowering deferred to first force); return lazy thunks. Env chain: `ctx.config.stdlib_env` as root → child env with `env:` entries injected → child env with `"$"` bound to the `%:` thunk. **Capability safety:** eval'd expressions run in the closure captured at thunk-creation time — the `Surface` thunk's `env` field. This env is `stdlib_env + env: entries + %: binding`. Capabilities like `%pwd` are not in `stdlib_env` (they are injected by the CLI above stdlib_env) and are only accessible if the caller explicitly forwards them via `env:`. Capability safety follows from tinct's closure semantics: a function sees the environment where it was defined, not where it was invoked. No special capability-stripping is needed — the `Surface` thunk mechanically enforces the boundary.
- `eval-types`: same as `eval` but uses `ctx.config.type_stage_env`
- `ast-of`: receives argument thunk without forcing (`Strictness::Id`); returns `Value::Expression` wrapping the thunk's `SurfaceExpression`. For already-materialized Rust-backed values (`Value::Builtin`, `Value::Task`, `Value::Channel`, `Value::Context`) that have no source AST, returns `Value::Expression` wrapping `SurfaceExpression::Placeholder` — the tinct `...` form. This keeps the return type uniform and preserves round-trip validity; transformation code encountering a `Placeholder` knows the original was a Rust-backed value with no inspectable source.
- **Delete** `builtin_eval_ast` — replaced by `eval` on a single-element seq

### Change `src/expand.rs`

- Operates on `SurfaceExpression`/`SurfaceProgram` instead of `Expr`/`File`
- `surface_expr_tag` and `surface_node_get_field` replace the old `dict_to_ast` round-trip; no payload Dict is materialized during match dispatch
- The shadow guard was already deleted in `include-decomp-primitives`
- `SurfaceDeclaration::Splice` encountered in `SurfaceDocument.items` (produced by a macro returning a Splice) must be flattened inline into `SurfaceItem::Expr` entries; there is no persistent `SurfaceItem::Splice` form — Splice is a macro-expansion artifact only

### Change `src/resolve.rs`

- Resolver produces `ResolutionTable` instead of mutating `VarRef.resolved` in place
- Returns `(SurfaceProgram, ResolutionTable)` — the surface AST is unchanged by resolution

### Change `src/typecheck.rs`

- `TypeAnnotationTable` produced alongside type inference; no mutation of `TypeAssert.resolved_type`
- `Expression`, `Document`, `Program` are declared types in prelude; resolved normally via type declaration
- **Add** `Type::Task(Box<Type>)`, `Type::Channel(Box<Type>)`, `Type::Context` — inference rules for async primitives

### Change `src/builtins.rs`

- `BuiltinFn` becomes `async fn` pointer type returning `Pin<Box<dyn Future>>`
- All ~180 builtins gain async wrapper
- I/O builtins replace `block_on(fut)` with `fut.await`
- **Add** new async primitive builtins (see table below)

### New Types

Three types are opaque and Rust-backed — `Context` (wraps `tokio_util::sync::CancellationToken`), `Task@T` (spawned computation handle), and `Channel@T` (bounded async queue). They are registered by the runtime as primitive types; there is no tinct declaration for them.

`Signal` is a genuine tinct sum type — its variants are user-visible names, not implementation details:

```tinct
Signal: [type [SIGTERM] [SIGINT] [SIGHUP] [SIGUSR1] [SIGUSR2] [SIGPIPE] [SIGALRM]]
```

Three structural types are named so that signatures stay readable:

```tinct
# Zero-argument side-effecting function — runs for effect, returns nothing.
# Named after Moggi/Haskell's Action/IO distinction; avoids collision with
# the evaluator's own "thunk" concept.
Action: [Fn [] Null]

# Return value of with-cancel
CancelHandle: [type [CancelHandle
  child-ctx: Context
  cancel:    Action]]

# One source entry for select-once — a channel paired with its handler function.
# t: element type of the channel; r: return type of the handler.
SelectSource: [type [t r] [SelectSource
  ch:      [Channel t]
  handler: [Fn [t] r]]]
```

### New Builtins

| Builtin | Signature | Description |
|---------|-----------|-------------|
| `context` | `[Fn [] Context]` | Current evaluation's cancellation context |
| `with-cancel` | `[Fn [ctx@Context] CancelHandle]` | Child context + cancel function |
| `with-timeout` | `[Fn [ctx@Context  ms@Int] Context]` | Auto-cancels after duration |
| `with-deadline` | `[Fn [ctx@Context  ts@Timestamp] Context]` | Auto-cancels at absolute time |
| `cancelled?` | `[Fn [ctx@Context] Bool]` | True if context cancelled |
| `with-context` | `[Fn [ctx@Context  f@[Fn [] t]] t]` | Evaluates thunk under given context |
| `timeout` | `[Fn [dur@Duration  task@[Task t]] [Result t]]` | Awaits task with deadline |
| `cancel-task` | `[Fn [t@[Task t]] Null]` | Cancel a specific task; abort handle inside Task |
| `cancel-root` | `Action` | Cancel root token — signals all tasks to stop |
| `drain` | `Action` | Await until all in-flight tasks finish |
| `exit-now` | `[Fn [code@Int] Null]` | `process::exit` immediately |
| `task` | `[Fn [expr@Any] [Task t]]` | Spawn evaluation of expr |
| `await` | `[Fn [task@[Task t]] t]` | Suspend until task completes |
| `await-all` | `[Fn [tasks@[Seq [Task t]]] [Seq t]]` | Await all; results in submission order |
| `await-any` | `[Fn [tasks@[Seq [Task t]]] t]` | Return first completed; abort rest |
| `channel` | `[Fn [capacity@Int] [Channel t]]` | Bounded channel; capacity ≥ 1 |
| `send` | `[Fn [ch@[Channel t]  val@t] Null]` | Send; suspend if buffer full |
| `recv` | `[Fn [ch@[Channel t]] t]` | Receive; suspend until available |
| `select-once` | `[Fn [sources@[Seq [SelectSource t r]]] r]` | Wait for first ready channel |
| `par` | `[Fn [expr@Any] t]` | Spawn on thread pool immediately |
| `par-map` | `[Fn [f@[Fn [a] b]  seq@[Seq a]] [Seq b]]` | Parallel map; results in order |
| `par-filter` | `[Fn [f@[Fn [a] Bool]  seq@[Seq a]] [Seq a]]` | Parallel filter |
| `signal-channel` | `[Fn [signals@[Seq Signal]] [Channel Signal]]` | OS signal delivery channel |
| `timer-channel` | `[Fn [clock@ClockCap  interval@Duration] [Channel Timestamp]]` | Periodic timer channel |
| `watch-channel` | `[Fn [cap@DirCap  path@String] [Channel Null]]` | Filesystem watch channel |

The signatures in this table use lowercase single-letter names (`t`, `r`, `a`, `b`) as type variables — these are descriptive for human readers. In tinct source, the stdlib implementations carry no explicit type annotations; let-generalization at the definition site produces the correct polymorphic schemes. Functions close over the environment where they are defined, not where they are invoked — type variables are scoped to the definition, exactly as values are.

All builtins in this table are Rust primitives **except** `await-all`, `par-map`, and `par-filter`, which are tinct stdlib (`stdlib/async.llt`). Their implementations carry no explicit type annotations — inference handles polymorphism:

```tinct
# Fail-fast: routes results through a shared channel so completion order
# doesn't matter. First error cancels remaining tasks via shared CancelHandle.
# Results returned in submission order on full success.
await-all: [fn [tasks]
  [n:          [length [collect tasks]]]
  [cancel-h:   [with-cancel [context]]]
  [results-ch: [channel n]]
  [map-with-index [fn [i t]
    [with-context cancel-h.child-ctx [task [fn []
      [match [try [fn [] [await t]]]
        [Ok v]:   [send results-ch [Ok [i v]]]
        [Error e]: [cancel cancel-h]
                  [send results-ch [Error e]]]]]]]
  tasks]
  [recv-all results-ch n []]]

recv-all: [fn [ch remaining acc]
  [if [= remaining 0]
    [map [fn [p] p[1]] [sort-by [fn [p] p[0]] acc]]
    [match [recv ch]
      [Error e]:  [raise e]
      [Ok [i v]]: [recv-all ch [- remaining 1] [cons [i v] acc]]]]]

# collect forces the lazy map to spawn ALL tasks before any await blocks,
# ensuring true parallel execution.
par-map: [fn [f seq]
  [map await [collect [map [fn [x] [task [fn [] [f x]]]] seq]]]]

par-filter: [fn [f seq]
  [pairs: [collect [map [fn [x] [[x] [task [fn [] [f x]]]]] seq]]]
  [filter-map [fn [pair] [if [await pair[1]] pair[0] null]] pairs]]

# timeout cannot be expressed in tinct without a task→channel bridge —
# select-once takes Channel, not Task. Implemented in Rust via tokio::time::timeout.
```

### Change `src/parser.rs`

- Parser produces `SurfaceExpression` / `SurfaceDocument` / `SurfaceProgram` instead of `Expr` / `Document` / `File`
- No NodeId assignment — node identity is derived from `Arc::as_ptr()` by callers that need it
- `SurfaceDeclaration` nodes parsed separately from `SurfaceExpression` within document items

### Change `src/async_rt.rs`

- `run_program(fut)` — multi-thread runtime, work-stealing. Replaces `block_on`.
- `spawn_task(fut)` — `tokio::spawn` with `Arc`-based `TaskHandle`
- Thread-local `current_thread` runtime and `block_on` bridge removed

### Change `src/lsp/`

- Analysis functions become `async fn`
- LSP protocol handlers retain `block_on` at the outermost call site only during the async migration — not inside builtins, only at the LSP→analysis boundary
- Once the LSP event loop is itself async (tower-lsp or equivalent), `block_on` is removed entirely
- Add `Value::Expression`, `Value::Document`, `Value::Program` handling to any LSP code that inspects values

### Change test suite (`tests/`, `src/**/*_test.rs`)

- All tests become `#[tokio::test(flavor = "current_thread")]`
- Test helper `run_eval(source)` wraps evaluation in `run_program(...)`
- Individual test assertions unchanged

### Prelude tinct code

- `Expression`/`Document`/`Program`/`Parameter`/`Entry`/`MatchArm`/`Annotation`/`Declaration`/`DocumentName` type declarations added
- `Context`, `Task`, `Channel`, `Signal`, `Action`, `CancelHandle`, `SelectSource` type declarations added
- `eval-document-pipeline`, `eval-file`, `eval-document-runtime`, `include` updated per §Updated Include-Decomp Tinct Code
- `async.llt` added: `exit`, `graceful-exit`, `finally`, `loop-select`, `retry`

### Delete — Complete Dead-Code Inventory

A cleanup pass at the end of the sprint must verify every item below is gone. Nothing in this list should remain active.

**`src/ast.rs`**
- `Expr` enum — replaced by `SurfaceExpression` + `CoreExpr`
- `Document` struct — replaced by `SurfaceDocument`
- `File` struct — replaced by `SurfaceProgram`
- `VarRef.resolved: RefCell<...>` — replaced by `ResolutionTable`
- `TypeAssert.resolved_type: RefCell<Option<Type>>` — replaced by `TypeAnnotationTable` + distinct `CoreExpr` variants

**`src/ast_dict.rs`**
- `dict_to_ast` — removed entirely (no remaining callers)
- `dict_to_file` — never written; confirm it was never accidentally added
- `ast_to_dict` (file-level function) — removed; `load` now wraps `SurfaceProgram` directly
- `document_to_dict` — removed alongside `ast_to_dict`
- String-keyed `type:` Dict schema as the output format — superseded by `Value::Variant` trees

**`src/desugar.rs`** — deleted; `Pipe` → `Call` rewriting moves into the lowering pass in `src/lower.rs`

**`src/eval_pipeline.rs`** — entire file deleted
- `eval_file_with_input`
- `eval_document` (the let\* loop is extracted into `src/lower.rs` or inline in the evaluator)
- `eval_file`
- `run_eval`

**`src/builtins_meta.rs`**
- `builtin_eval_ast` — replaced by `eval` on a single-element seq
- `builtin_include` — deleted in `include-decomp-eval-primitives` (prerequisite); confirm gone

**`src/builtins.rs`**
- `eval-ast` registration in `standard_builtins()`
- `builtin_include` registration — from prerequisite sprint; confirm gone
- `rust_module()` dispatcher — from prerequisite sprint; confirm gone
- `builtin-*` alias registrations — from prerequisite sprint; confirm gone

**`src/value.rs`**
- `ThunkState` enum — replaced by `(Mutex<Option<UnevaluatedState>>, OnceCell<...>)` pair
- `Value::RustRegistry` — from prerequisite sprint; confirm gone
- All `Rc<T>` value types — replaced by `Arc<T>`; scan for any remaining `use std::rc::Rc` imports

**`src/eval.rs` / `src/eval_materialize.rs`**
- All `Rc<Thunk>` / `Rc<RefCell<Environment>>` usage — replaced by `Arc`
- `InProgress` / `PendingBuiltin` / `PendingCall` thunk states — replaced by `OnceLock` pair

**`src/eval_state.rs` or `src/eval.rs`**
- `EvalState::include_guard: HashSet<(u64, u64)>` — from prerequisite sprint; confirm gone
- `EvalState::include_cache` (old inode-keyed cache) — from prerequisite sprint; confirm gone

**`src/async_rt.rs`**
- Thread-local `current_thread` Tokio runtime — replaced by `run_program()`
- `block_on` bridge function — replaced by `.await`; confirm no remaining call sites except LSP boundary

**`src/main.rs`**
- `run_eval()` Rust call — replaced by tinct `cli-pipeline` (from prerequisite sprint); confirm gone

**Error types**
- `Box<EvalError>` at cross-thread boundaries — replaced by `Arc<EvalError>`; scan for remaining `Box<EvalError>` in async contexts

**Tinct stdlib**
- `[include %rust "..."]` patterns in `stdlib/prelude.llt` — from prerequisite sprint; confirm gone
- Any `dict?` guards around AST node access — `dict?` returns `false` for AST types; any such guard is dead code

---

## Prerequisites

- `include-decomp-primitives` complete (`blake3`, `cap-identity`, `load`, `include-cache-get`, `include-cache-put` registered)
- `include-decomp-eval-primitives` complete (interim `dict_to_file`-based `expand`/`eval`/`eval-types`; deletion of `builtin_include` and `eval_pipeline.rs` public functions; this proposal supersedes the representation but not the deletion work)
- `include-decomp-prelude` complete (self-hosted pipeline; this proposal refactors it)

---

## References

- Abelson, H. & Sussman, G.J. (1996). *Structure and Interpretation of Computer Programs*, 2nd ed. MIT Press. §4.1 "The Metacircular Evaluator." — homoiconic representation of code as data.
- Pombrio, J. & Krishnamurthi, S. (2014). "Hygienic Resugaring of Call-by-Value Evaluation Sequences." *ICFP 2014*. — origin tracking for desugared nodes (`Fn.desugared`, `SurfaceExpression` vs `CoreExpr` split motivation).
- Marlow, S. et al. (2009). "Runtime Support for Multicore Haskell." *ICFP '09*. — `par`/`seq` sparks and the GHC scheduler; the implicit-parallelism model for automatic parallel dict evaluation.
- Syme, D., Petricek, T. & Lomov, D. (2011). "The F# Asynchronous Programming Model." *PADL '11*. — Async workflows as first-class values; `task { }` computation expressions directly analogous to tinct's `[task ...]` builtin.
- Leijen, D., Schulte, W. & Burckhardt, S. (2009). "The Design of a Task Parallel Library." *OOPSLA '09*. — Structured task concurrency; `await-all`/`await-any` semantics.
- Go language specification. "Select statements." *go.dev/ref/spec*. — `select` over channels; tinct's `select-once` is directly analogous.
- Jones, S.P., Gordon, A. & Finne, S. (1996). "Concurrent Haskell." *POPL '96*. — MVars and the original "communicating lazy threads" model.
- Tokio documentation. "tokio::task::LocalSet." — The `!Send` cooperative execution model and multi-thread work-stealing runtime.
- Cardelli, L. (1997). "Type Systems." *Handbook of Computer Science and Engineering*. — nominal vs structural types and the value of nominal typing for AST node dispatch.
